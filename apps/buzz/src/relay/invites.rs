//! Stateless relay invite tokens and the join-policy acceptance receipt
//! (buzz's `invite_token`).
//!
//! An invite code is a compact, URL-safe, HMAC-signed blob minted by a
//! community authority and later presented by a joining pubkey. The relay
//! verifies the signature and expiry, then materializes the presenter's
//! community membership — no server-side invite storage.
//!
//! ## Format (buzz's, byte-compatible in shape)
//!
//! ```text
//! code = base64url_nopad(payload_json) + "." + base64url_nopad(hmac_sha256(key, payload_json))
//! payload_json = {"c":"<community>","r":"member","e":<unix secs>,"n":"<nonce>"}
//! ```
//!
//! ## Key derivation
//!
//! `key = sha256(key_material || "buzz-invite-v1")`. buzz derives the material
//! from the relay's signing secret; this port derives it from the dedicated
//! `buzz-invite-key` secret/config (the stream-token-key pattern — the port's
//! precedent for operator-provisioned key material), with the same
//! domain-separation label. Rotating the key invalidates every outstanding
//! invite — the intended blast-radius control for a leaked link.
//!
//! ## Security properties (and non-properties), unchanged from buzz
//!
//! - Codes are **multi-use until expiry**; default expiry is deliberately
//!   short ([`DEFAULT_INVITE_TTL_SECS`]).
//! - Codes are **community-scoped** and **role-capped at `member`** — both
//!   re-checked at verify, so a hand-crafted payload with an elevated role is
//!   rejected even under a valid MAC.
//! - Revocation is coarse: rotate the key, or remove the member after the
//!   fact.
//! - The verify errors are deliberately coarse at the HTTP layer so the
//!   endpoint is a poor oracle for forging codes.
//!
//! One deliberate divergence: buzz draws a random nonce; this platform has no
//! entropy source, so the nonce is derived (`sha256` over caller-supplied
//! seed material — minter, instant, correlation). The nonce only
//! distinguishes identically-parameterised invites, so determinism per seed
//! is harmless.

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json;

use crate::relay::crypto::hmac::hmac_sha256;
use crate::relay::crypto::sha256::sha256;
use crate::relay::crypto::{ct_eq, to_hex};
use crate::relay::{b64, BuzzError, Result};

/// Default invite lifetime: 72 hours.
pub const DEFAULT_INVITE_TTL_SECS: u64 = 72 * 60 * 60;
/// Maximum invite lifetime a mint request may ask for: 30 days.
pub const MAX_INVITE_TTL_SECS: u64 = 30 * 24 * 60 * 60;
/// Minimum lifetime — a code that expires as it is minted helps no one.
pub const MIN_INVITE_TTL_SECS: u64 = 60;
/// Maximum accepted code length (defense against absurd inputs before any
/// parsing work happens). A real code is ~200 bytes.
const MAX_CODE_LEN: usize = 1024;
/// Maximum accepted policy-receipt length.
const MAX_RECEIPT_LEN: usize = 2048;
/// Domain-separation label mixed into the HMAC key derivation (buzz's).
const KEY_DERIVATION_LABEL: &[u8] = b"buzz-invite-v1";
/// Policy acceptance receipts are short-lived: ten minutes to click through.
const POLICY_RECEIPT_TTL_SECS: u64 = 10 * 60;

/// The signed payload carried inside an invite code.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(crate = "ferrite_app::serde")]
pub struct InvitePayload {
    /// Community the invite admits into.
    pub c: String,
    /// Role granted on claim. Only `"member"` is valid in v1.
    pub r: String,
    /// Expiry as unix seconds.
    pub e: u64,
    /// Nonce so identically-parameterised invites differ.
    pub n: String,
}

/// The signed payload inside a policy acceptance receipt: the invite it is
/// bound to (as a hash — the receipt must not be a second copy of the code),
/// the policy version accepted, and a short expiry.
#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(crate = "ferrite_app::serde")]
pub struct PolicyAcceptancePayload {
    /// `hex(sha256(code))` — binds the receipt to ONE invite code.
    pub c: String,
    /// The policy version the user accepted.
    pub v: String,
    /// Expiry as unix seconds.
    pub e: u64,
}

/// Why a code failed verification. Deliberately coarse; the HTTP layer maps
/// all but `Expired` to one generic rejection.
#[derive(Debug, PartialEq, Eq)]
pub enum InviteError {
    /// Structurally invalid (bad base64, bad JSON, wrong shape, too long).
    Malformed,
    /// MAC did not verify.
    BadSignature,
    /// Signature fine, but the expiry has passed.
    Expired,
    /// Signature fine, but minted for a different community.
    WrongCommunity,
    /// Signature fine, but the role is not one this relay grants via invite.
    InvalidRole,
}

impl InviteError {
    /// The coarse client-facing refusal (`Expired` is post-MAC, so revealing
    /// it helps the UX without helping a forger).
    #[must_use]
    pub fn refusal(&self) -> BuzzError {
        match self {
            Self::Expired => BuzzError::Unauthorized("invite_expired".into()),
            _ => BuzzError::Unauthorized("invite_invalid".into()),
        }
    }
}

/// Derive the invite HMAC key from operator key material.
#[must_use]
pub fn derive_invite_key(material: &[u8]) -> [u8; 32] {
    let mut input = Vec::with_capacity(material.len() + KEY_DERIVATION_LABEL.len());
    input.extend_from_slice(material);
    input.extend_from_slice(KEY_DERIVATION_LABEL);
    sha256(&input)
}

/// Mint an invite code for `community`, expiring `ttl_secs` from `now_secs`.
///
/// The role is fixed to `"member"` — elevated roles are granted post-join,
/// never via a bearer link. The nonce derives from `nonce_seed` (see the
/// module doc's divergence note).
///
/// # Errors
///
/// Never fails today; `Result` so a future payload bound has a lane.
pub fn mint_invite(
    key: &[u8; 32],
    community: &str,
    ttl_secs: u64,
    now_secs: u64,
    nonce_seed: &[u8],
) -> Result<(String, u64)> {
    let ttl = ttl_secs.clamp(MIN_INVITE_TTL_SECS, MAX_INVITE_TTL_SECS);
    let expires_at = now_secs.saturating_add(ttl);
    let nonce_bytes = sha256(nonce_seed);
    let payload = InvitePayload {
        c: community.to_string(),
        r: "member".to_string(),
        e: expires_at,
        n: encode_url_nopad(nonce_bytes.get(..16).unwrap_or(&nonce_bytes)),
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| BuzzError::Internal(format!("invite payload encode: {e}")))?;
    let mac = hmac_sha256(key, &bytes);
    Ok((
        format!("{}.{}", encode_url_nopad(&bytes), encode_url_nopad(&mac)),
        expires_at,
    ))
}

/// Verify an invite code presented to `community` at `now_secs`.
///
/// Order matters and is buzz's: the MAC is checked (constant-time) before any
/// claim inside the payload is trusted (expiry / community / role).
pub fn verify_invite(
    key: &[u8; 32],
    community: &str,
    code: &str,
    now_secs: u64,
) -> core::result::Result<InvitePayload, InviteError> {
    if code.len() > MAX_CODE_LEN {
        return Err(InviteError::Malformed);
    }
    let (payload_b64, mac_b64) = code.split_once('.').ok_or(InviteError::Malformed)?;
    let payload_bytes = decode_url_nopad(payload_b64).ok_or(InviteError::Malformed)?;
    let mac_bytes = decode_url_nopad(mac_b64).ok_or(InviteError::Malformed)?;
    let expected = hmac_sha256(key, &payload_bytes);
    if !ct_eq(&expected, &mac_bytes) {
        return Err(InviteError::BadSignature);
    }
    let payload: InvitePayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| InviteError::Malformed)?;
    if payload.e < now_secs {
        return Err(InviteError::Expired);
    }
    if payload.c != community {
        return Err(InviteError::WrongCommunity);
    }
    if payload.r != "member" {
        return Err(InviteError::InvalidRole);
    }
    Ok(payload)
}

/// Exchange explicit policy acceptance for a short-lived, invite-bound
/// receipt.
///
/// # Errors
///
/// Only on a JSON-encode failure, which cannot happen for these fields.
pub fn mint_policy_acceptance(
    key: &[u8; 32],
    code: &str,
    version: &str,
    now_secs: u64,
) -> Result<String> {
    let payload = PolicyAcceptancePayload {
        c: to_hex(&sha256(code.as_bytes())),
        v: version.to_string(),
        e: now_secs.saturating_add(POLICY_RECEIPT_TTL_SECS),
    };
    let bytes = serde_json::to_vec(&payload)
        .map_err(|e| BuzzError::Internal(format!("policy receipt encode: {e}")))?;
    let mac = hmac_sha256(key, &bytes);
    Ok(format!(
        "{}.{}",
        encode_url_nopad(&bytes),
        encode_url_nopad(&mac)
    ))
}

/// Verify a policy receipt and bind it to the submitted invite code and the
/// CURRENT policy version.
pub fn verify_policy_acceptance(
    key: &[u8; 32],
    receipt: &str,
    code: &str,
    version: &str,
    now_secs: u64,
) -> core::result::Result<PolicyAcceptancePayload, InviteError> {
    if receipt.len() > MAX_RECEIPT_LEN {
        return Err(InviteError::Malformed);
    }
    let (payload_b64, mac_b64) = receipt.split_once('.').ok_or(InviteError::Malformed)?;
    let payload_bytes = decode_url_nopad(payload_b64).ok_or(InviteError::Malformed)?;
    let mac_bytes = decode_url_nopad(mac_b64).ok_or(InviteError::Malformed)?;
    let expected = hmac_sha256(key, &payload_bytes);
    if !ct_eq(&expected, &mac_bytes) {
        return Err(InviteError::BadSignature);
    }
    let payload: PolicyAcceptancePayload =
        serde_json::from_slice(&payload_bytes).map_err(|_| InviteError::Malformed)?;
    if payload.e < now_secs {
        return Err(InviteError::Expired);
    }
    if payload.c != to_hex(&sha256(code.as_bytes())) || payload.v != version {
        return Err(InviteError::BadSignature);
    }
    Ok(payload)
}

/// URL-safe, unpadded base64 over the vendored standard encoder: `+/` mapped
/// to `-_`, padding stripped (the invite wire format buzz uses).
fn encode_url_nopad(bytes: &[u8]) -> String {
    b64::encode(bytes)
        .replace('+', "-")
        .replace('/', "_")
        .trim_end_matches('=')
        .to_string()
}

/// Inverse of [`encode_url_nopad`]. The vendored decoder already accepts the
/// URL-safe alphabet; it insists on padding, so padding is restored first.
fn decode_url_nopad(input: &str) -> Option<Vec<u8>> {
    let mut padded = input.to_string();
    while !padded.len().is_multiple_of(4) {
        padded.push('=');
    }
    b64::decode(&padded)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const KEY: [u8; 32] = [7u8; 32];
    const NOW: u64 = 1_700_000_000;

    fn code() -> String {
        mint_invite(&KEY, "main", 3600, NOW, b"seed").unwrap().0
    }

    #[test]
    fn a_minted_code_round_trips_and_is_member_scoped() {
        let (code, expires_at) = mint_invite(&KEY, "main", 3600, NOW, b"seed").unwrap();
        assert_eq!(expires_at, NOW + 3600);
        let payload = verify_invite(&KEY, "main", &code, NOW).unwrap();
        assert_eq!(payload.r, "member");
        assert_eq!(payload.c, "main");
        assert_eq!(payload.e, expires_at);
    }

    #[test]
    fn ttl_is_clamped_both_ways() {
        let (_, e) = mint_invite(&KEY, "main", 0, NOW, b"s").unwrap();
        assert_eq!(e, NOW + MIN_INVITE_TTL_SECS);
        let (_, e) = mint_invite(&KEY, "main", u64::MAX, NOW, b"s").unwrap();
        assert_eq!(e, NOW + MAX_INVITE_TTL_SECS);
    }

    #[test]
    fn verification_refuses_every_forgery_class() {
        let code = code();
        // Tampered payload under the old MAC.
        let (payload_b64, mac_b64) = code.split_once('.').unwrap();
        let mut bytes = decode_url_nopad(payload_b64).unwrap();
        bytes[10] ^= 1;
        let tampered = format!("{}.{}", encode_url_nopad(&bytes), mac_b64);
        assert_eq!(
            verify_invite(&KEY, "main", &tampered, NOW),
            Err(InviteError::BadSignature)
        );
        // Wrong key.
        assert_eq!(
            verify_invite(&[9u8; 32], "main", &code, NOW),
            Err(InviteError::BadSignature)
        );
        // Wrong community, expired, and a hand-crafted elevated role — each
        // caught AFTER the MAC, in buzz's order.
        assert_eq!(
            verify_invite(&KEY, "other", &code, NOW),
            Err(InviteError::WrongCommunity)
        );
        assert_eq!(
            verify_invite(&KEY, "main", &code, NOW + 3601),
            Err(InviteError::Expired)
        );
        let owner_payload = InvitePayload {
            c: "main".into(),
            r: "owner".into(),
            e: NOW + 3600,
            n: "x".into(),
        };
        let bytes = serde_json::to_vec(&owner_payload).unwrap();
        let forged = format!(
            "{}.{}",
            encode_url_nopad(&bytes),
            encode_url_nopad(&hmac_sha256(&KEY, &bytes))
        );
        assert_eq!(
            verify_invite(&KEY, "main", &forged, NOW),
            Err(InviteError::InvalidRole)
        );
        // Structural garbage.
        for bad in ["", "nodot", "a.b", &"x".repeat(2000)] {
            assert!(verify_invite(&KEY, "main", bad, NOW).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn policy_receipts_bind_to_code_version_and_time() {
        let receipt = mint_policy_acceptance(&KEY, "invite-a", "v1", NOW).unwrap();
        let payload = verify_policy_acceptance(&KEY, &receipt, "invite-a", "v1", NOW).unwrap();
        assert_eq!(payload.v, "v1");
        assert!(verify_policy_acceptance(&KEY, &receipt, "invite-b", "v1", NOW).is_err());
        assert!(verify_policy_acceptance(&KEY, &receipt, "invite-a", "v2", NOW).is_err());
        assert!(verify_policy_acceptance(&[8u8; 32], &receipt, "invite-a", "v1", NOW).is_err());
        assert!(verify_policy_acceptance(
            &KEY,
            &receipt,
            "invite-a",
            "v1",
            NOW + POLICY_RECEIPT_TTL_SECS + 1
        )
        .is_err());
    }

    #[test]
    fn key_derivation_is_label_separated() {
        assert_ne!(derive_invite_key(b"material"), sha256(b"material"));
        assert_ne!(derive_invite_key(b"a"), derive_invite_key(b"b"));
    }

    #[test]
    fn url_nopad_round_trips_awkward_lengths() {
        for len in 0..10 {
            let bytes: Vec<u8> = (0..len).map(|i: u8| 0xf8u8.wrapping_add(i)).collect();
            let encoded = encode_url_nopad(&bytes);
            assert!(!encoded.contains(['+', '/', '=']), "{encoded}");
            assert_eq!(decode_url_nopad(&encoded).unwrap(), bytes);
        }
    }
}
