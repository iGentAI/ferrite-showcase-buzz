//! Signed Nostr events, as the process cell needs them.
//!
//! A compact wire-shaped `Event`: hex strings on the wire (which is what
//! NIP-01 says and what the relay parses), bytes only where the arithmetic
//! needs them. The canonical form the id hashes is NIP-01's
//! `[0, pubkey, created_at, kind, tags, content]`, serialized by the same
//! JSON writer the relay uses — so an event this module signs is an event the
//! relay's `Event::verify` accepts, and `tests/event_vectors.rs` pins that
//! agreement with a fixed vector the relay's own suite asserts too.
//!
//! Signing is DETERMINISTIC (BIP-340 with a zero aux), for the reason the
//! relay documents: there is no entropy source on this platform, and a
//! re-driven effect must rebuild a byte-identical event whose id the relay's
//! admission marker already absorbs.

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json;

use crate::crypto::schnorr;
use crate::crypto::sha256::sha256;
use crate::crypto::to_hex;
use crate::error::{ProcessError, Result};

/// A NIP-01 event in its wire shape.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Event {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u32,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

impl Event {
    /// NIP-01's canonical array — what the id is the SHA-256 of.
    pub fn canonical_serialization(&self) -> Result<String> {
        serde_json::to_string(&(
            0u8,
            &self.pubkey,
            self.created_at,
            self.kind,
            &self.tags,
            &self.content,
        ))
        .map_err(|e| ProcessError::Internal(format!("canonical serialization: {e}")))
    }

    pub fn compute_id(&self) -> Result<[u8; 32]> {
        Ok(sha256(self.canonical_serialization()?.as_bytes()))
    }

    /// Sign a fresh event under `seckey`.
    pub fn sign(
        seckey: &[u8; 32],
        kind: u32,
        tags: Vec<Vec<String>>,
        content: &str,
        created_at: u64,
    ) -> Result<Self> {
        let pubkey = schnorr::xonly_pubkey(seckey)
            .ok_or_else(|| ProcessError::Internal("secret key is out of range".into()))?;
        let mut event = Self {
            id: String::new(),
            pubkey: to_hex(&pubkey),
            created_at,
            kind,
            tags,
            content: content.to_string(),
            sig: String::new(),
        };
        let id = event.compute_id()?;
        event.id = to_hex(&id);
        let sig = schnorr::sign(&id, seckey, &[0u8; 32])
            .ok_or_else(|| ProcessError::Internal("schnorr signing failed".into()))?;
        event.sig = to_hex(&sig);
        Ok(event)
    }

    /// Verify id then signature — the order matters, exactly as it does in the
    /// relay: a signature checked against a STATED id says nothing about the
    /// body until the id is confirmed to be that body's hash.
    pub fn verify(&self) -> Result<()> {
        let computed = self.compute_id()?;
        if to_hex(&computed) != self.id {
            return Err(ProcessError::Invalid(
                "event id does not match its fields".into(),
            ));
        }
        let (Some(pubkey), Some(sig)) = (
            crate::crypto::from_hex32(&self.pubkey),
            from_hex64(&self.sig),
        ) else {
            return Err(ProcessError::Invalid(
                "event pubkey or sig is not hex".into(),
            ));
        };
        if !schnorr::verify(&computed, &pubkey, &sig) {
            return Err(ProcessError::Invalid(
                "event signature does not verify".into(),
            ));
        }
        Ok(())
    }

    #[must_use]
    pub fn first_tag(&self, name: &str) -> Option<&str> {
        self.tags
            .iter()
            .find(|t| t.first().is_some_and(|k| k == name))
            .and_then(|t| t.get(1))
            .map(String::as_str)
    }

    /// The room this event names, if any.
    #[must_use]
    pub fn channel(&self) -> Option<&str> {
        self.first_tag("h")
    }
}

/// A 64-byte signature from hex.
#[must_use]
pub fn from_hex64(hex: &str) -> Option<[u8; 64]> {
    let bytes = crate::crypto::from_hex(hex)?;
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 64];
    out.copy_from_slice(&bytes);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SK: [u8; 32] = [7u8; 32];

    #[test]
    fn a_signed_event_verifies_and_is_deterministic() {
        let a = Event::sign(
            &SK,
            9,
            vec![vec!["h".into(), "room".into()]],
            "hello",
            1_700_000_000,
        )
        .expect("sign");
        a.verify().expect("verifies");
        let b = Event::sign(
            &SK,
            9,
            vec![vec!["h".into(), "room".into()]],
            "hello",
            1_700_000_000,
        )
        .expect("sign");
        assert_eq!(a, b, "signing must be byte-reproducible for saga replay");
    }

    #[test]
    fn a_tampered_event_fails_verification() {
        let mut e = Event::sign(&SK, 9, vec![], "hello", 1_700_000_000).expect("sign");
        e.content = "goodbye".into();
        assert!(e.verify().is_err(), "id must not match a changed body");
    }

    #[test]
    fn the_canonical_form_is_the_nip01_array() {
        let e = Event::sign(&SK, 1, vec![vec!["p".into(), "ab".into()]], "hi", 42).expect("sign");
        let canonical = e.canonical_serialization().expect("canonical");
        assert!(canonical.starts_with("[0,\""), "{canonical}");
        assert!(
            canonical.contains(",42,1,[[\"p\",\"ab\"]],\"hi\"]"),
            "{canonical}"
        );
    }

    #[test]
    fn tags_and_channel_read_back() {
        let e = Event::sign(
            &SK,
            9,
            vec![
                vec!["h".into(), "process-abc".into()],
                vec!["e".into(), "ff".into()],
            ],
            "",
            1,
        )
        .expect("sign");
        assert_eq!(e.channel(), Some("process-abc"));
        assert_eq!(e.first_tag("e"), Some("ff"));
        assert_eq!(e.first_tag("missing"), None);
    }
}
