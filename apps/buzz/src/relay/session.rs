//! Subscription sessions and the stream tokens that open them.
//!
//! Buzz authenticated a socket once (NIP-42) and then trusted it for the
//! connection's life. There is no socket here: a client POSTs its filters to
//! `/buzz/api/subscriptions` with a signed request, gets back a session id, a
//! bearer *stream token*, and a cursor, then opens
//! `GET /buzz/sub/{session}?token=…&from=…` for the SSE feed.
//!
//! The token rides the query string because the gateway's stream face delivers
//! **no request headers** to the app (the platform gateway's ingress dispatch sends an
//! empty header vec), so there is nowhere else to put it. That makes three
//! properties mandatory, and this module owns all three:
//!
//! * **Unforgeable** — an HMAC over the session's own facts under a sealed
//!   secret, compared in constant time.
//! * **Expiring** — the expiry is inside the signed material, so a leaked URL
//!   stops working without any revocation list.
//! * **Single-flight** — the session document carries a `generation` that each
//!   stream open advances by compare-and-swap, so a replayed token supersedes
//!   the previous pump instead of running a second one beside it.
//!
//! Access is never frozen into the session. The filters are remembered, but
//! every record is re-authorized at delivery against live membership, so
//! removing someone from a channel stops their existing stream.

use ferrite_app::serde::{Deserialize, Serialize};

use crate::relay::crypto::{ct_eq, from_hex, hmac::hmac_sha256_concat, to_hex};
use crate::relay::event::PubKey;
use crate::relay::filter::Filter;
use crate::relay::{BuzzError, Result};

/// How long a freshly minted stream token is valid. Clients re-mint by POSTing
/// the subscription again; pumps exit when the token behind them expires, which
/// is what bounds a pump's lifetime.
pub const TOKEN_TTL_MS: u64 = 60 * 60 * 1000;

/// A session's client lease. A pump checks it every batch and exits when it has
/// lapsed, so a client that vanishes without a `STREAM_CLOSE` (a killed browser,
/// a dead gateway instance) cannot leave a pump polling forever.
pub const LEASE_TTL_MS: u64 = 5 * 60 * 1000;

/// How often a pump emits a heartbeat frame when no records are flowing. The
/// gateway keep-alives the socket on its own schedule and would happily hold a
/// healthy-looking connection to a *dead* app pump, so liveness has to be
/// visible at the application layer or clients cannot tell the difference.
pub const HEARTBEAT_INTERVAL_MS: u64 = 10 * 1000;

/// The durable record behind a subscription.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Session {
    /// Opaque session id, derived (not random — there is no entropy source) from
    /// the correlating request and the subscriber.
    pub id: String,
    /// Hex pubkey of the subscriber; every delivery is authorized against it.
    pub pubkey: String,
    /// NIP-01 filters this session subscribed with.
    pub filters: Vec<Filter>,
    /// Advanced by compare-and-swap on every stream open. The pump carries the
    /// value it started with and exits as soon as the stored value differs.
    pub generation: u64,
    /// Absolute expiry of the current stream token.
    pub token_expires_at_ms: u64,
    /// Absolute expiry of the client's lease; refreshed by the client.
    pub lease_expires_at_ms: u64,
    /// The feed generation this session is reading (topics restart offsets at 0
    /// per generation, so a cursor without its generation is ambiguous).
    pub cursor_generation: u32,
    /// Last offset delivered, so a reconnect resumes exactly.
    pub cursor_offset: u64,
    pub created_at_ms: u64,
}

impl Session {
    /// Open a session for `pubkey` with `filters`.
    ///
    /// `seed` must be unique per subscription request — the gateway's correlation
    /// id is exactly that, and it is what stands in for randomness on a platform
    /// that injects no RNG. The id is the HMAC of the seed under the same sealed
    /// secret that signs tokens, so ids are unguessable without being random.
    #[must_use]
    pub fn open(
        secret: &[u8],
        pubkey: &PubKey,
        filters: Vec<Filter>,
        seed: &str,
        cursor_generation: u32,
        cursor_offset: u64,
        now_ms: u64,
    ) -> Self {
        let id_mac = hmac_sha256_concat(secret, &[b"buzz/session-id", pubkey, seed.as_bytes()]);
        Self {
            id: to_hex(&id_mac[..16]),
            pubkey: to_hex(pubkey),
            filters,
            generation: 1,
            token_expires_at_ms: now_ms.saturating_add(TOKEN_TTL_MS),
            lease_expires_at_ms: now_ms.saturating_add(LEASE_TTL_MS),
            cursor_generation,
            cursor_offset,
            created_at_ms: now_ms,
        }
    }

    /// The subscriber's pubkey as bytes.
    pub fn subscriber(&self) -> Result<PubKey> {
        let raw = from_hex(&self.pubkey)
            .ok_or_else(|| BuzzError::Internal("session pubkey is not hex".into()))?;
        let bytes: [u8; 32] = raw
            .try_into()
            .map_err(|_| BuzzError::Internal("session pubkey is not 32 bytes".into()))?;
        Ok(bytes)
    }

    /// Whether the client's lease is still live at `now_ms`.
    #[must_use]
    pub const fn lease_is_live(&self, now_ms: u64) -> bool {
        now_ms < self.lease_expires_at_ms
    }

    /// Whether the current token is still valid at `now_ms`.
    #[must_use]
    pub const fn token_is_live(&self, now_ms: u64) -> bool {
        now_ms < self.token_expires_at_ms
    }

    /// Refresh the client lease (called on any authenticated client touch).
    pub fn refresh_lease(&mut self, now_ms: u64) {
        self.lease_expires_at_ms = now_ms.saturating_add(LEASE_TTL_MS);
    }

    /// Mint the bearer token for the session's current expiry.
    ///
    /// Shape: `{expiry_ms}.{hex mac}`. The expiry is in the clear *and* inside
    /// the signed material, so a client cannot extend its own token by editing
    /// the visible half.
    #[must_use]
    pub fn mint_token(&self, secret: &[u8]) -> String {
        let expiry = self.token_expires_at_ms.to_string();
        let mac = hmac_sha256_concat(
            secret,
            &[
                b"buzz/stream-token",
                self.pubkey.as_bytes(),
                b"\0",
                self.id.as_bytes(),
                b"\0",
                expiry.as_bytes(),
            ],
        );
        let mut out = String::with_capacity(expiry.len() + 1 + 64);
        out.push_str(&expiry);
        out.push('.');
        out.push_str(&to_hex(&mac));
        out
    }

    /// Verify a presented token against this session.
    ///
    /// Refuses malformed shapes, a mismatched MAC (constant-time compare), and
    /// an expired token. A token minted for a *different* session or pubkey fails
    /// the MAC because both are in the signed material.
    #[must_use]
    pub fn verify_token(&self, secret: &[u8], presented: &str, now_ms: u64) -> bool {
        let Some((expiry_str, mac_hex)) = presented.split_once('.') else {
            return false;
        };
        let Ok(expiry) = expiry_str.parse::<u64>() else {
            return false;
        };
        if now_ms >= expiry {
            return false;
        }
        // The token's own expiry must be the one this session issued; otherwise a
        // token minted before a re-mint would keep working past its replacement.
        if expiry != self.token_expires_at_ms {
            return false;
        }
        let Some(presented_mac) = from_hex(mac_hex) else {
            return false;
        };
        let expected = hmac_sha256_concat(
            secret,
            &[
                b"buzz/stream-token",
                self.pubkey.as_bytes(),
                b"\0",
                self.id.as_bytes(),
                b"\0",
                expiry_str.as_bytes(),
            ],
        );
        ct_eq(&presented_mac, &expected)
    }
}

/// A resume cursor: the feed generation plus the dense offset inside it.
///
/// Both halves are required. Topic offsets are dense *per topic*, and a rotated
/// generation starts again at zero, so an offset alone is ambiguous — and the
/// topic front fails softly (a too-old cursor is silently clamped, a too-new one
/// silently returns nothing), which would strand a pump that could not tell the
/// difference.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Cursor {
    pub generation: u32,
    pub offset: u64,
}

impl Cursor {
    #[must_use]
    pub const fn new(generation: u32, offset: u64) -> Self {
        Self { generation, offset }
    }

    /// Wire form: `{generation}:{offset}`.
    #[must_use]
    pub fn encode(&self) -> String {
        let mut s = self.generation.to_string();
        s.push(':');
        s.push_str(&self.offset.to_string());
        s
    }

    /// Parse the wire form. A bare integer is accepted as generation 0 so an
    /// early client that only knew about offsets still resumes somewhere valid
    /// rather than being rejected.
    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        match s.split_once(':') {
            Some((g, o)) => Some(Self {
                generation: g.parse().ok()?,
                offset: o.parse().ok()?,
            }),
            None => Some(Self {
                generation: 0,
                offset: s.parse().ok()?,
            }),
        }
    }
}

/// Why a pump told its client to start over.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ResetReason {
    /// The requested generation is no longer the live one.
    GenerationRotated,
    /// The feed no longer holds the requested offset (retention clamped it).
    RetentionClamped,
    /// The generation's topic is gone.
    FeedRetired,
    /// The subscriber's access was revoked (a live ban): the pump ends rather
    /// than keep a session open for a member the community expelled.
    Revoked,
}

impl ResetReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::GenerationRotated => "generation_rotated",
            Self::RetentionClamped => "retention_clamped",
            Self::FeedRetired => "feed_retired",
            Self::Revoked => "revoked",
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const SECRET: &[u8] = b"a-sealed-stream-token-key-32-byte";

    fn pk(b: u8) -> PubKey {
        [b; 32]
    }

    fn session(now: u64) -> Session {
        Session::open(SECRET, &pk(1), vec![], "correlation-abc", 0, 0, now)
    }

    #[test]
    fn minted_token_verifies() {
        let s = session(1_000);
        let t = s.mint_token(SECRET);
        assert!(s.verify_token(SECRET, &t, 1_000));
        assert!(s.verify_token(SECRET, &t, s.token_expires_at_ms - 1));
    }

    #[test]
    fn token_expires() {
        let s = session(1_000);
        let t = s.mint_token(SECRET);
        assert!(!s.verify_token(SECRET, &t, s.token_expires_at_ms));
        assert!(!s.verify_token(SECRET, &t, s.token_expires_at_ms + 1));
    }

    #[test]
    fn token_does_not_verify_under_a_different_secret() {
        let s = session(1_000);
        let t = s.mint_token(SECRET);
        assert!(!s.verify_token(b"another-key-entirely-------------", &t, 1_000));
    }

    #[test]
    fn token_is_bound_to_its_session_and_pubkey() {
        let a = Session::open(SECRET, &pk(1), vec![], "seed-a", 0, 0, 1_000);
        let mut b = Session::open(SECRET, &pk(2), vec![], "seed-b", 0, 0, 1_000);
        let token_a = a.mint_token(SECRET);
        // Same expiry, different session/pubkey: the MAC must fail.
        b.token_expires_at_ms = a.token_expires_at_ms;
        assert!(!b.verify_token(SECRET, &token_a, 1_000));
    }

    #[test]
    fn editing_the_visible_expiry_does_not_extend_the_token() {
        let s = session(1_000);
        let t = s.mint_token(SECRET);
        let (_, mac) = t.split_once('.').unwrap();
        let forged = format!("{}.{}", u64::MAX, mac);
        assert!(!s.verify_token(SECRET, &forged, 1_000));
    }

    #[test]
    fn a_stale_token_stops_working_after_a_remint() {
        let mut s = session(1_000);
        let old = s.mint_token(SECRET);
        s.token_expires_at_ms = 9_999_999;
        assert!(!s.verify_token(SECRET, &old, 1_000));
        let fresh = s.mint_token(SECRET);
        assert!(s.verify_token(SECRET, &fresh, 1_000));
    }

    #[test]
    fn malformed_tokens_are_refused_without_panicking() {
        let s = session(1_000);
        for bad in [
            "",
            ".",
            "abc",
            "abc.def",
            "1000",
            "1000.",
            ".deadbeef",
            "99999999999999999999999999.aa",
            "1000.zz",
        ] {
            assert!(!s.verify_token(SECRET, bad, 1_000), "accepted {bad:?}");
        }
    }

    #[test]
    fn flipping_any_mac_bit_is_refused() {
        let s = session(1_000);
        let t = s.mint_token(SECRET);
        let (expiry, mac) = t.split_once('.').unwrap();
        let mut bytes = from_hex(mac).unwrap();
        for i in 0..bytes.len() {
            let original = bytes[i];
            bytes[i] ^= 0x01;
            let forged = format!("{}.{}", expiry, to_hex(&bytes));
            assert!(
                !s.verify_token(SECRET, &forged, 1_000),
                "accepted flip at {i}"
            );
            bytes[i] = original;
        }
    }

    #[test]
    fn session_ids_differ_per_seed_and_are_not_the_pubkey() {
        let a = Session::open(SECRET, &pk(1), vec![], "seed-a", 0, 0, 1);
        let b = Session::open(SECRET, &pk(1), vec![], "seed-b", 0, 0, 1);
        assert_ne!(a.id, b.id);
        assert!(!a.id.contains(&to_hex(&pk(1))));
        assert_eq!(a.id.len(), 32);
    }

    #[test]
    fn subscriber_round_trips() {
        let s = session(1);
        assert_eq!(s.subscriber().unwrap(), pk(1));
    }

    #[test]
    fn leases_expire_and_refresh() {
        let mut s = session(1_000);
        assert!(s.lease_is_live(1_000));
        assert!(!s.lease_is_live(1_000 + LEASE_TTL_MS));
        s.refresh_lease(1_000 + LEASE_TTL_MS);
        assert!(s.lease_is_live(1_000 + LEASE_TTL_MS));
    }

    #[test]
    fn cursor_round_trips_and_tolerates_bare_offsets() {
        let c = Cursor::new(3, 42);
        assert_eq!(c.encode(), "3:42");
        assert_eq!(Cursor::parse("3:42"), Some(c));
        assert_eq!(Cursor::parse("42"), Some(Cursor::new(0, 42)));
        assert_eq!(Cursor::parse("x:1"), None);
        assert_eq!(Cursor::parse("1:x"), None);
        assert_eq!(Cursor::parse(""), None);
    }
}
