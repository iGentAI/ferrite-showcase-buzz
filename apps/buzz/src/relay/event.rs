//! The signed event — the only thing the workspace stores.
//!
//! Every message, reaction, membership change, workflow step, and agent turn is
//! one of these: a NIP-01 event whose `id` is the SHA-256 of a canonical
//! serialization and whose `sig` is a BIP-340 Schnorr signature over that id.
//! Buzz got this type from the `nostr` crate; the platform code gate admits
//! exactly one dependency, so the type, its wire form, its canonicalization and
//! its verification are implemented here from NIP-01.
//!
//! Three things are load-bearing and easy to get subtly wrong:
//!
//! * **Canonicalization is the identity.** `[0,pubkey,created_at,kind,tags,content]`
//!   serialized with *JSON's* escaping rules, hashed. One byte of difference in
//!   escaping is a different id, so [`Event::canonical_serialization`] builds the
//!   array through `serde_json` rather than by string concatenation — the escaping
//!   is then the same escaping every other Nostr implementation gets from its own
//!   JSON writer.
//! * **The size bound is on the wire form, not the content.** The gateway caps
//!   the serialized body, and JSON escaping inflates content by up to 2× (quotes,
//!   backslashes) or 6× (control characters). [`Event::serialized_len`] is
//!   therefore the authoritative measurement; a content-length check alone would
//!   admit a body the gateway will refuse.
//! * **Verification is two checks, in order.** Recompute the id first: a
//!   signature that verifies against a *stated* id proves nothing about the
//!   fields if the id was never checked against them.

use core::fmt;

use ferrite_app::serde::de::{self, IgnoredAny, MapAccess, Visitor};
use ferrite_app::serde::ser::SerializeStruct;
use ferrite_app::serde::{Deserialize, Deserializer, Serialize, Serializer};
use ferrite_app::serde_json;

use crate::relay::crypto::sha256::sha256;
use crate::relay::crypto::{ct_eq, from_hex, to_hex};
use crate::relay::kind::Kind;
use crate::relay::{BuzzError, Result};

/// A BIP-340 x-only public key: the 32-byte X coordinate of a secp256k1 point
/// whose Y coordinate is even. This is a Nostr identity.
pub type PubKey = [u8; 32];

/// A NIP-01 event id: SHA-256 over [`Event::canonical_serialization`].
pub type EventId = [u8; 32];

/// Authoritative upper bound on a serialized event, in bytes.
///
/// The gateway refuses request bodies over 256 KiB, so this leaves headroom for
/// the envelope the event travels inside. Checked against
/// [`Event::serialized_len`] — the wire form — because that is what the gateway
/// actually measures.
pub const MAX_SERIALIZED_EVENT_BYTES: usize = 240 * 1024;

/// Content size that is *guaranteed* postable, in bytes.
///
/// A content string of this length always fits under
/// [`MAX_SERIALIZED_EVENT_BYTES`] once escaped at the realistic worst case (2×,
/// every byte a quote or a backslash) plus the fixed cost of the hex id, pubkey
/// and signature. Content between this and the serialized bound succeeds when
/// its escaped form fits: text is nearly 1×, but a string of control characters
/// escapes to `\u00XX`, six bytes each, and will be refused well before the
/// serialized cap. Advertised alongside [`MAX_SERIALIZED_EVENT_BYTES`] so a
/// client can size its own buffers honestly.
pub const MAX_CONTENT_BYTES_GUARANTEED: usize = 96 * 1024;

/// A Schnorr-signed workspace event.
///
/// Field order in the struct is the field order on the wire, which is also the
/// order NIP-01 canonicalizes in.
///
/// Equality is *structural*, not identity-based: two `Event`s are equal when
/// every field matches, including `sig`. That is what the storage layer needs
/// (an idempotent rewrite must produce identical bytes) and it is deliberately
/// stricter than "same id".
#[derive(Clone, PartialEq, Eq)]
pub struct Event {
    /// SHA-256 of the canonical serialization. Never trusted as given —
    /// [`Event::verify`] recomputes it.
    pub id: EventId,
    /// The author's x-only public key.
    pub pubkey: PubKey,
    /// Author-claimed creation time, Unix seconds. Inside the signed material,
    /// so the relay can only accept or refuse it, never correct it.
    pub created_at: u64,
    /// NIP-01 kind, which selects the validator, the read gate and the
    /// replacement rule.
    pub kind: Kind,
    /// Tags: each is a non-empty list whose first element names the tag. Kept as
    /// raw strings because the canonical form must reproduce exactly what was
    /// signed, including tags this relay does not understand.
    pub tags: Vec<Vec<String>>,
    /// Application payload. May be plaintext or a NIP-44 ciphertext.
    pub content: String,
    /// BIP-340 signature over [`Event::id`].
    pub sig: [u8; 64],
}

/// Debug renders the hashes as hex and *omits* `content`, reporting only its
/// length. Event bodies include private messages and encrypted agent state;
/// a `tracing` span that formats an event must not put them in the log.
impl fmt::Debug for Event {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Event")
            .field("id", &to_hex(&self.id))
            .field("pubkey", &to_hex(&self.pubkey))
            .field("created_at", &self.created_at)
            .field("kind", &self.kind)
            .field("tags", &self.tags)
            .field("content_bytes", &self.content.len())
            .field("sig", &to_hex(&self.sig))
            .finish()
    }
}

impl Event {
    /// The NIP-01 canonical serialization:
    /// `[0,"<pubkey hex>",<created_at>,<kind>,<tags>,"<content>"]`.
    ///
    /// Built through `serde_json` so the string escaping is a JSON writer's
    /// escaping — `\n`, `\r`, `\t`, `\b`, `\f`, `\"`, `\\` in short form, other
    /// control characters as `\u00XX`, everything else verbatim UTF-8, no
    /// whitespace between tokens. That is precisely what NIP-01 requires and
    /// what every other implementation's JSON writer produces, which is why ids
    /// computed here match ids computed elsewhere.
    ///
    /// # Errors
    ///
    /// [`BuzzError::Internal`] if serialization fails. No input reachable
    /// through this type can cause that (there are no maps and no floats), so it
    /// is reported as an invariant violation rather than a client error.
    pub fn canonical_serialization(&self) -> Result<String> {
        serde_json::to_string(&Canonical(self))
            .map_err(|e| BuzzError::Internal(format!("canonical serialization: {e}")))
    }

    /// The event id this event's fields actually hash to.
    ///
    /// # Errors
    ///
    /// Propagates [`Event::canonical_serialization`].
    pub fn compute_id(&self) -> Result<EventId> {
        Ok(sha256(self.canonical_serialization()?.as_bytes()))
    }

    /// Verify the event end to end: id, then signature.
    ///
    /// Order matters. The signature is over the id, so verifying the signature
    /// against a *stated* id says nothing about `content`, `tags`, `kind` or
    /// `created_at` unless the id was first confirmed to be the hash of those
    /// fields. An attacker who could reorder these checks could take any signed
    /// event and swap its body.
    ///
    /// CPU-bound (one hash plus one scalar multiplication); callers on an async
    /// task should keep that in mind, as buzz did.
    ///
    /// # Errors
    ///
    /// * [`BuzzError::IdMismatch`] — the id is not the hash of the fields.
    /// * [`BuzzError::BadSignature`] — the id hashes correctly but `sig` does
    ///   not verify under `pubkey`, or `pubkey` is not a valid x-only point.
    pub fn verify(&self) -> Result<()> {
        let computed = self.compute_id()?;
        if !ct_eq(&computed, &self.id) {
            return Err(BuzzError::IdMismatch);
        }
        if !crate::relay::crypto::schnorr::verify(&self.id, &self.pubkey, &self.sig) {
            return Err(BuzzError::BadSignature);
        }
        Ok(())
    }

    /// Refuse an event whose wire form exceeds [`MAX_SERIALIZED_EVENT_BYTES`].
    ///
    /// The one place that bound is enforced, so ingest and any relay-authored
    /// path agree on it.
    ///
    /// # Errors
    ///
    /// [`BuzzError::TooLarge`] with the measured and permitted sizes.
    pub fn check_serialized_size(&self) -> Result<()> {
        let got = self.serialized_len();
        if got > MAX_SERIALIZED_EVENT_BYTES {
            return Err(BuzzError::TooLarge {
                got,
                max: MAX_SERIALIZED_EVENT_BYTES,
            });
        }
        Ok(())
    }

    /// Values of every tag named `name` — the tag's *second* element.
    ///
    /// Tags shorter than two elements are skipped: `["h"]` names a tag with no
    /// value and yields nothing, rather than an empty string that could
    /// accidentally match an empty filter value.
    pub fn tag_values<'a>(&'a self, name: &'a str) -> impl Iterator<Item = &'a str> + 'a {
        self.tags.iter().filter_map(move |tag| {
            if tag.first().map(String::as_str) == Some(name) {
                tag.get(1).map(String::as_str)
            } else {
                None
            }
        })
    }

    /// The first value of the first tag named `name`, in event order.
    ///
    /// NIP-01 makes the first occurrence authoritative for single-valued tags
    /// (`d`, `h`), so a duplicate later in the list cannot override it.
    #[must_use]
    pub fn first_tag(&self, name: &str) -> Option<&str> {
        self.tags.iter().find_map(|tag| {
            if tag.first().map(String::as_str) == Some(name) {
                tag.get(1).map(String::as_str)
            } else {
                None
            }
        })
    }

    /// Whether the event carries a tag named `name` at all, with or without a
    /// value.
    ///
    /// Distinct from `first_tag(name).is_some()`: filter matching has to know
    /// that an author *asserted* a tag even when it is valueless, because an
    /// asserted `h` tag suppresses the channel fallback (see
    /// [`crate::relay::filter::Filter::matches`]).
    #[must_use]
    pub fn has_tag(&self, name: &str) -> bool {
        self.tags
            .iter()
            .any(|tag| tag.first().map(String::as_str) == Some(name))
    }

    /// The channel this event claims, from its `h` tag.
    ///
    /// Author-supplied and therefore *not* the resolved channel: reactions and
    /// deletions inherit their channel from the event they target, and ingest
    /// resolves that. Access decisions use the resolved value.
    #[must_use]
    pub fn channel_id(&self) -> Option<&str> {
        self.first_tag("h")
    }

    /// The `d` tag: the replaceable-event discriminator (NIP-33).
    #[must_use]
    pub fn d_tag(&self) -> Option<&str> {
        self.first_tag("d")
    }

    /// Length of the event's wire JSON in bytes — the authoritative size.
    ///
    /// Counted without materializing the JSON, so measuring a 240 KiB event
    /// costs no second copy of it.
    ///
    /// Fails closed: if the event cannot be serialized at all it reports
    /// [`usize::MAX`], so a size gate refuses it rather than waving through a
    /// body whose true size is unknown.
    #[must_use]
    pub fn serialized_len(&self) -> usize {
        let mut counter = ByteCounter(0);
        match serde_json::to_writer(&mut counter, self) {
            Ok(()) => counter.0,
            Err(_) => usize::MAX,
        }
    }

    /// Build and sign a new event.
    ///
    /// `created_at` is a parameter because the app has no clock: time arrives
    /// from the injected context and is passed down. The id is computed from the
    /// finished fields and the signature is taken over that id, so the returned
    /// event satisfies [`Event::verify`] by construction.
    ///
    /// # Determinism
    ///
    /// A pure function of its arguments: signing the same event twice yields
    /// identical bytes. That is a requirement, not a convenience — a replayed
    /// saga rewrites an event's bytes and those rewrites must be byte-identical
    /// — and it is why BIP-340's `aux_rand` is fixed rather than drawn (see the
    /// comment at the signing call, and [`crate::relay::crypto::schnorr::sign`]).
    ///
    /// # Errors
    ///
    /// * [`BuzzError::InvalidEvent`] — `seckey` is not a valid secp256k1 scalar
    ///   (zero, or at/above the group order).
    /// * [`BuzzError::TooLarge`] — the finished event exceeds
    ///   [`MAX_SERIALIZED_EVENT_BYTES`]. Checked here so this relay can never
    ///   mint an event it would itself refuse to ingest.
    /// * [`BuzzError::Internal`] — signing failed after the key was accepted.
    pub fn sign_new(
        seckey: &[u8; 32],
        kind: Kind,
        tags: Vec<Vec<String>>,
        content: &str,
        created_at: u64,
    ) -> Result<Self> {
        let pubkey = crate::relay::crypto::schnorr::xonly_pubkey(seckey)
            .ok_or_else(|| BuzzError::InvalidEvent("secret key is out of range".into()))?;
        let mut event = Self {
            id: [0u8; 32],
            pubkey,
            created_at,
            kind,
            tags,
            content: content.to_string(),
            sig: [0u8; 64],
        };
        event.id = event.compute_id()?;
        event.check_serialized_size()?;
        // Signing is DELIBERATELY DETERMINISTIC: BIP-340's `aux_rand` only adds
        // side-channel hardening, and the nonce already depends on the message,
        // so a zero aux still gives every distinct event a distinct nonce. What it
        // buys us is that re-signing the same event reproduces the same bytes —
        // which is exactly what the saga replay rules require, since a workflow
        // step or agent turn that re-runs must rebuild a byte-identical event
        // whose id the admission marker can absorb. There is also no entropy
        // source on this platform to draw a fresh aux from.
        event.sig = crate::relay::crypto::schnorr::sign(&event.id, seckey, &[0u8; 32])
            .ok_or_else(|| BuzzError::Internal("schnorr signing failed".into()))?;
        Ok(event)
    }
}

/// The NIP-01 canonical array, serialized by reference so a 240 KiB `content`
/// is never cloned to be hashed.
struct Canonical<'a>(&'a Event);

impl Serialize for Canonical<'_> {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        use ferrite_app::serde::ser::SerializeSeq;
        let e = self.0;
        let mut seq = serializer.serialize_seq(Some(6))?;
        // The leading 0 is NIP-01's version marker, not a field of the event.
        seq.serialize_element(&0u8)?;
        seq.serialize_element(&to_hex(&e.pubkey))?;
        seq.serialize_element(&e.created_at)?;
        seq.serialize_element(&e.kind)?;
        seq.serialize_element(&e.tags)?;
        seq.serialize_element(&e.content)?;
        seq.end()
    }
}

/// A sink that counts bytes and keeps none, for [`Event::serialized_len`].
struct ByteCounter(usize);

impl std::io::Write for ByteCounter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0 = self.0.saturating_add(buf.len());
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Wire field names, in NIP-01 order.
const FIELDS: &[&str] = &[
    "id",
    "pubkey",
    "created_at",
    "kind",
    "tags",
    "content",
    "sig",
];

/// Serializes to the Nostr wire shape: hashes as lowercase hex strings,
/// `created_at`/`kind` as numbers, `tags` as arrays of string arrays.
impl Serialize for Event {
    fn serialize<S: Serializer>(&self, serializer: S) -> core::result::Result<S::Ok, S::Error> {
        let mut st = serializer.serialize_struct("Event", FIELDS.len())?;
        st.serialize_field("id", &to_hex(&self.id))?;
        st.serialize_field("pubkey", &to_hex(&self.pubkey))?;
        st.serialize_field("created_at", &self.created_at)?;
        st.serialize_field("kind", &self.kind)?;
        st.serialize_field("tags", &self.tags)?;
        st.serialize_field("content", &self.content)?;
        st.serialize_field("sig", &to_hex(&self.sig))?;
        st.end()
    }
}

/// Wire field identifiers. Unknown names deserialize to [`Field::Other`] and
/// their values are skipped, so a newer client's extra field is tolerated
/// instead of failing the whole submission.
enum Field {
    Id,
    Pubkey,
    CreatedAt,
    Kind,
    Tags,
    Content,
    Sig,
    Other,
}

impl<'de> Deserialize<'de> for Field {
    fn deserialize<D: Deserializer<'de>>(d: D) -> core::result::Result<Self, D::Error> {
        struct FieldVisitor;

        impl Visitor<'_> for FieldVisitor {
            type Value = Field;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a nostr event field name")
            }

            fn visit_str<E: de::Error>(self, value: &str) -> core::result::Result<Field, E> {
                Ok(match value {
                    "id" => Field::Id,
                    "pubkey" => Field::Pubkey,
                    "created_at" => Field::CreatedAt,
                    "kind" => Field::Kind,
                    "tags" => Field::Tags,
                    "content" => Field::Content,
                    "sig" => Field::Sig,
                    _ => Field::Other,
                })
            }
        }

        d.deserialize_identifier(FieldVisitor)
    }
}

/// Decode a fixed-width byte array from lowercase or uppercase hex, reporting
/// which field was wrong.
fn hex_field<const N: usize, E: de::Error>(
    field: &'static str,
    value: &str,
) -> core::result::Result<[u8; N], E> {
    let bytes = from_hex(value).ok_or_else(|| E::custom(format!("{field} is not hex")))?;
    if bytes.len() != N {
        return Err(E::custom(format!(
            "{field} must be {N} bytes of hex, got {}",
            bytes.len()
        )));
    }
    let mut out = [0u8; N];
    out.copy_from_slice(&bytes);
    Ok(out)
}

/// Deserializes the Nostr wire shape.
///
/// Duplicate fields are **rejected**, not last-wins. An event whose JSON object
/// carries `content` twice would otherwise be ambiguous: one parser could verify
/// the signature against the first value while another served the second.
impl<'de> Deserialize<'de> for Event {
    fn deserialize<D: Deserializer<'de>>(d: D) -> core::result::Result<Self, D::Error> {
        struct EventVisitor;

        impl<'de> Visitor<'de> for EventVisitor {
            type Value = Event;

            fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str("a nostr event object")
            }

            fn visit_map<A: MapAccess<'de>>(
                self,
                mut map: A,
            ) -> core::result::Result<Event, A::Error> {
                let mut id: Option<EventId> = None;
                let mut pubkey: Option<PubKey> = None;
                let mut created_at: Option<u64> = None;
                let mut kind: Option<Kind> = None;
                let mut tags: Option<Vec<Vec<String>>> = None;
                let mut content: Option<String> = None;
                let mut sig: Option<[u8; 64]> = None;

                while let Some(field) = map.next_key::<Field>()? {
                    match field {
                        Field::Id => {
                            if id.is_some() {
                                return Err(de::Error::duplicate_field("id"));
                            }
                            id = Some(hex_field("id", &map.next_value::<String>()?)?);
                        }
                        Field::Pubkey => {
                            if pubkey.is_some() {
                                return Err(de::Error::duplicate_field("pubkey"));
                            }
                            pubkey = Some(hex_field("pubkey", &map.next_value::<String>()?)?);
                        }
                        Field::CreatedAt => {
                            if created_at.is_some() {
                                return Err(de::Error::duplicate_field("created_at"));
                            }
                            created_at = Some(map.next_value()?);
                        }
                        Field::Kind => {
                            if kind.is_some() {
                                return Err(de::Error::duplicate_field("kind"));
                            }
                            kind = Some(map.next_value()?);
                        }
                        Field::Tags => {
                            if tags.is_some() {
                                return Err(de::Error::duplicate_field("tags"));
                            }
                            tags = Some(map.next_value()?);
                        }
                        Field::Content => {
                            if content.is_some() {
                                return Err(de::Error::duplicate_field("content"));
                            }
                            content = Some(map.next_value()?);
                        }
                        Field::Sig => {
                            if sig.is_some() {
                                return Err(de::Error::duplicate_field("sig"));
                            }
                            sig = Some(hex_field("sig", &map.next_value::<String>()?)?);
                        }
                        Field::Other => {
                            map.next_value::<IgnoredAny>()?;
                        }
                    }
                }

                Ok(Event {
                    id: id.ok_or_else(|| de::Error::missing_field("id"))?,
                    pubkey: pubkey.ok_or_else(|| de::Error::missing_field("pubkey"))?,
                    created_at: created_at.ok_or_else(|| de::Error::missing_field("created_at"))?,
                    kind: kind.ok_or_else(|| de::Error::missing_field("kind"))?,
                    tags: tags.ok_or_else(|| de::Error::missing_field("tags"))?,
                    content: content.ok_or_else(|| de::Error::missing_field("content"))?,
                    sig: sig.ok_or_else(|| de::Error::missing_field("sig"))?,
                })
            }
        }

        d.deserialize_struct("Event", FIELDS, EventVisitor)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// A secret key that is a valid secp256k1 scalar (small, non-zero).
    fn seckey(b: u8) -> [u8; 32] {
        let mut k = [0u8; 32];
        k[31] = b;
        k[0] = 0x01;
        k
    }

    fn fixture() -> Event {
        Event {
            id: [0u8; 32],
            pubkey: [0x02u8; 32],
            created_at: 1_700_000_000,
            kind: 1,
            tags: vec![
                vec!["e".into(), "a".repeat(64)],
                vec!["p".into(), "b".repeat(64)],
                vec!["h".into(), "chan-1".into()],
            ],
            content: "he said \"hi\"\\path\nline\ttab \u{1F680}".into(),
            sig: [0x07u8; 64],
        }
    }

    #[test]
    fn canonical_serialization_escapes_exactly_like_a_json_writer() {
        let e = fixture();
        let canon = e.canonical_serialization().unwrap();
        assert_eq!(
            canon,
            concat!(
                "[0,\"0202020202020202020202020202020202020202020202020202020202020202\",",
                "1700000000,1,",
                "[[\"e\",\"aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa\"],",
                "[\"p\",\"bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb\"],",
                "[\"h\",\"chan-1\"]],",
                "\"he said \\\"hi\\\"\\\\path\\nline\\ttab \u{1F680}\"]"
            )
        );
        // No whitespace between tokens; the version marker leads.
        assert!(canon.starts_with("[0,\""));
        assert!(!canon.contains(", "));
    }

    #[test]
    fn event_id_is_stable_for_content_with_quotes_backslashes_newlines_tabs_and_a_non_bmp_emoji() {
        // Pinned vector: recomputing it requires reproducing NIP-01 escaping
        // byte for byte, so this test fails if canonicalization ever drifts.
        //
        // Provenance — this is not self-referential. Both this vector and the
        // one in the next test were cross-checked against `nostr` 0.44's
        // `EventId::new` (the crate buzz itself uses) for the identical fields,
        // and against an independent Python implementation of NIP-01. All three
        // agree, so the id space of this port is the id space of the network.
        let e = fixture();
        assert_eq!(
            to_hex(&e.compute_id().unwrap()),
            "53241e85943cd0e3a2a3f460564c2e5f0d85ffd8ab23345c82d1e58113dedf96"
        );
    }

    #[test]
    fn control_characters_escape_as_short_forms_or_u00xx_and_slashes_stay_bare() {
        let e = Event {
            id: [0u8; 32],
            pubkey: [0x02u8; 32],
            created_at: 0,
            kind: 30023,
            tags: vec![],
            content: "a\u{1}b/c\u{e9}d\u{8}\u{c}\r".into(),
            sig: [0u8; 64],
        };
        let canon = e.canonical_serialization().unwrap();
        assert!(
            canon.ends_with("\"a\\u0001b/c\u{e9}d\\b\\f\\r\"]"),
            "{canon}"
        );
        assert_eq!(
            to_hex(&e.compute_id().unwrap()),
            "d55dc28cba75d485ea1137760449291ef19c13749c48a7b002b0ea50e0ebe219"
        );
    }

    #[test]
    fn the_all_zero_empty_event_has_a_pinned_id() {
        let e = Event {
            id: [0u8; 32],
            pubkey: [0u8; 32],
            created_at: 0,
            kind: 0,
            tags: vec![],
            content: String::new(),
            sig: [0u8; 64],
        };
        assert_eq!(
            e.canonical_serialization().unwrap(),
            "[0,\"0000000000000000000000000000000000000000000000000000000000000000\",0,0,[],\"\"]"
        );
        assert_eq!(
            to_hex(&e.compute_id().unwrap()),
            "d23368f79cfcdd3ade5259fed1b761a9810fbbf55bc4a2d3dbbecd1d19a86b81"
        );
    }

    #[test]
    fn the_id_does_not_cover_the_id_or_the_signature() {
        // Canonicalization takes six elements; changing id or sig must not move
        // the computed id, or verification would be circular.
        let base = fixture();
        let mut other = base.clone();
        other.id = [0xff; 32];
        other.sig = [0xff; 64];
        assert_eq!(base.compute_id().unwrap(), other.compute_id().unwrap());
    }

    #[test]
    fn every_signed_field_changes_the_id() {
        let base = fixture();
        let base_id = base.compute_id().unwrap();

        let mut a = base.clone();
        a.pubkey = [0x03; 32];
        let mut b = base.clone();
        b.created_at += 1;
        let mut c = base.clone();
        c.kind += 1;
        let mut d = base.clone();
        d.tags.push(vec!["x".into()]);
        let mut e = base.clone();
        e.content.push('!');
        // Tag order is part of the signed material.
        let mut f = base.clone();
        f.tags.reverse();

        for variant in [a, b, c, d, e, f] {
            assert_ne!(variant.compute_id().unwrap(), base_id, "{variant:?}");
        }
    }

    #[test]
    fn serde_round_trips_through_the_nostr_wire_shape() {
        let e = fixture();
        let json = serde_json::to_string(&e).unwrap();
        assert!(json.starts_with(
            "{\"id\":\"0000000000000000000000000000000000000000000000000000000000000000\",\"pubkey\":\""
        ));
        assert!(json.contains("\"created_at\":1700000000"));
        assert!(json.contains("\"kind\":1"));
        assert!(json.contains(&format!("\"sig\":\"{}\"", "07".repeat(64))));
        assert_eq!(serde_json::from_str::<Event>(&json).unwrap(), e);
    }

    #[test]
    fn hashes_are_lowercase_hex_of_the_right_width() {
        let json = serde_json::to_value(fixture()).unwrap();
        assert_eq!(json["id"].as_str().unwrap().len(), 64);
        assert_eq!(json["pubkey"].as_str().unwrap().len(), 64);
        assert_eq!(json["sig"].as_str().unwrap().len(), 128);
        assert_eq!(json["pubkey"].as_str().unwrap(), "02".repeat(32));
        assert!(json["tags"].is_array());
        assert!(json["created_at"].is_u64());
        assert!(json["kind"].is_u64());
    }

    #[test]
    fn uppercase_hex_is_accepted_on_the_way_in() {
        // Clients that upcase hex still round-trip to the same bytes; the
        // canonical form is always re-derived from bytes, never echoed.
        let e = fixture();
        let mut json: serde_json::Value = serde_json::to_value(&e).unwrap();
        json["pubkey"] = serde_json::Value::String("02".repeat(32).to_uppercase());
        let parsed: Event = serde_json::from_value(json).unwrap();
        assert_eq!(parsed.pubkey, e.pubkey);
    }

    #[test]
    fn malformed_wire_events_are_refused_rather_than_truncated() {
        let good = serde_json::to_value(fixture()).unwrap();

        let mut short_id = good.clone();
        short_id["id"] = serde_json::Value::String("aa".into());
        assert!(serde_json::from_value::<Event>(short_id).is_err());

        let mut non_hex_sig = good.clone();
        non_hex_sig["sig"] = serde_json::Value::String("z".repeat(128));
        assert!(serde_json::from_value::<Event>(non_hex_sig).is_err());

        let mut long_sig = good.clone();
        long_sig["sig"] = serde_json::Value::String("aa".repeat(65));
        assert!(serde_json::from_value::<Event>(long_sig).is_err());

        let mut odd_hex = good.clone();
        odd_hex["id"] = serde_json::Value::String("a".repeat(63));
        assert!(serde_json::from_value::<Event>(odd_hex).is_err());

        let mut negative_kind = good.clone();
        negative_kind["kind"] = serde_json::Value::from(-1i64);
        assert!(serde_json::from_value::<Event>(negative_kind).is_err());

        let mut huge_kind = good.clone();
        huge_kind["kind"] = serde_json::Value::from(u64::from(u32::MAX) + 1);
        assert!(serde_json::from_value::<Event>(huge_kind).is_err());

        let mut bad_tags = good;
        bad_tags["tags"] = serde_json::Value::from(vec![1, 2, 3]);
        assert!(serde_json::from_value::<Event>(bad_tags).is_err());
    }

    #[test]
    fn every_field_is_required() {
        for missing in FIELDS {
            let mut json = serde_json::to_value(fixture()).unwrap();
            json.as_object_mut().unwrap().remove(*missing);
            assert!(
                serde_json::from_value::<Event>(json).is_err(),
                "accepted an event without {missing}"
            );
        }
    }

    #[test]
    fn a_duplicated_field_is_rejected_not_silently_resolved() {
        // Two `content` values would let one reader verify against the first
        // and another serve the second. Ambiguity is refused.
        let json = concat!(
            "{\"id\":\"",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "\",\"pubkey\":\"",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "\",\"created_at\":1,\"kind\":1,\"tags\":[],",
            "\"content\":\"first\",\"content\":\"second\",\"sig\":\"",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "0000000000000000000000000000000000000000000000000000000000000000",
            "\"}"
        );
        let err = serde_json::from_str::<Event>(json).unwrap_err();
        assert!(err.to_string().contains("content"), "{err}");
    }

    #[test]
    fn unknown_fields_are_tolerated() {
        let mut json = serde_json::to_value(fixture()).unwrap();
        json["relay_hint"] = serde_json::Value::String("wss://example".into());
        json["future"] = serde_json::json!({"nested": [1, 2, 3]});
        assert_eq!(serde_json::from_value::<Event>(json).unwrap(), fixture());
    }

    #[test]
    fn debug_reports_hex_hashes_and_never_the_content() {
        let mut e = fixture();
        e.content = "top secret plaintext".into();
        let rendered = format!("{e:?}");
        assert!(!rendered.contains("top secret"), "{rendered}");
        assert!(rendered.contains("content_bytes: 20"), "{rendered}");
        assert!(rendered.contains(&"02".repeat(32)), "{rendered}");
    }

    #[test]
    fn tag_values_yields_the_second_element_of_every_matching_tag() {
        let e = Event {
            tags: vec![
                vec!["p".into(), "one".into()],
                vec!["e".into(), "skipped".into()],
                vec!["p".into(), "two".into(), "extra".into()],
                // Valueless and empty tags contribute nothing.
                vec!["p".into()],
                vec![],
            ],
            ..fixture()
        };
        assert_eq!(e.tag_values("p").collect::<Vec<_>>(), vec!["one", "two"]);
        assert_eq!(e.tag_values("nope").count(), 0);
        assert_eq!(e.first_tag("p"), Some("one"));
        assert_eq!(e.first_tag("nope"), None);
    }

    #[test]
    fn has_tag_sees_a_valueless_tag_that_tag_values_skips() {
        let e = Event {
            tags: vec![vec!["h".into()]],
            ..fixture()
        };
        assert!(e.has_tag("h"));
        assert_eq!(e.first_tag("h"), None);
        assert_eq!(e.tag_values("h").count(), 0);
        assert!(!e.has_tag("d"));
    }

    #[test]
    fn tag_lookup_is_case_sensitive_and_exact() {
        let e = Event {
            tags: vec![
                vec!["H".into(), "upper".into()],
                vec!["hh".into(), "x".into()],
            ],
            ..fixture()
        };
        assert_eq!(e.first_tag("h"), None);
        assert_eq!(e.first_tag("H"), Some("upper"));
    }

    #[test]
    fn channel_id_and_d_tag_read_their_tags() {
        let e = Event {
            tags: vec![
                vec!["h".into(), "chan-1".into()],
                vec!["d".into(), "owner-key".into()],
                // A later duplicate must not win.
                vec!["h".into(), "chan-2".into()],
            ],
            ..fixture()
        };
        assert_eq!(e.channel_id(), Some("chan-1"));
        assert_eq!(e.d_tag(), Some("owner-key"));

        let bare = Event {
            tags: vec![],
            ..fixture()
        };
        assert_eq!(bare.channel_id(), None);
        assert_eq!(bare.d_tag(), None);
    }

    #[test]
    fn serialized_len_matches_the_wire_json_length() {
        let e = fixture();
        assert_eq!(e.serialized_len(), serde_json::to_string(&e).unwrap().len());
    }

    #[test]
    fn serialized_len_measures_escaped_bytes_not_content_bytes() {
        // One backslash is one byte of content and two bytes on the wire; the
        // whole point of measuring the wire form.
        let plain = Event {
            content: "a".repeat(64),
            ..fixture()
        };
        let escaped = Event {
            content: "\\".repeat(64),
            ..fixture()
        };
        assert_eq!(plain.content.len(), escaped.content.len());
        assert_eq!(escaped.serialized_len(), plain.serialized_len() + 64);

        // Control characters are six wire bytes each.
        let control = Event {
            content: "\u{1}".repeat(64),
            ..fixture()
        };
        assert_eq!(control.serialized_len(), plain.serialized_len() + 64 * 5);
    }

    #[test]
    fn the_guaranteed_content_size_always_fits_the_serialized_bound() {
        // The advertised guarantee, asserted at its realistic worst case: every
        // byte a quote, which escapes to two.
        let worst = Event {
            tags: vec![],
            content: "\"".repeat(MAX_CONTENT_BYTES_GUARANTEED),
            ..fixture()
        };
        assert!(worst.serialized_len() <= MAX_SERIALIZED_EVENT_BYTES);
        assert!(worst.check_serialized_size().is_ok());
    }

    #[test]
    fn an_oversized_event_is_refused_with_both_numbers() {
        let big = Event {
            content: "a".repeat(MAX_SERIALIZED_EVENT_BYTES + 1),
            ..fixture()
        };
        match big.check_serialized_size() {
            Err(BuzzError::TooLarge { got, max }) => {
                assert_eq!(max, MAX_SERIALIZED_EVENT_BYTES);
                assert_eq!(got, big.serialized_len());
                assert!(got > max);
            }
            other => panic!("expected TooLarge, got {other:?}"),
        }
    }

    #[test]
    fn control_character_content_is_refused_below_the_content_cap() {
        // Documented degradation: 96 KiB is guaranteed for 2× escaping, and a
        // control-character payload escapes 6×, so it is bounded by the
        // serialized cap instead. Asserting it so the advertised numbers stay
        // honest.
        let e = Event {
            content: "\u{1}".repeat(MAX_SERIALIZED_EVENT_BYTES / 5),
            ..fixture()
        };
        assert!(
            e.content.len() < MAX_CONTENT_BYTES_GUARANTEED,
            "the point of the test is that this is *under* the advertised cap"
        );
        assert!(matches!(
            e.check_serialized_size(),
            Err(BuzzError::TooLarge { .. })
        ));
    }

    #[test]
    fn sign_new_produces_an_event_that_verifies() {
        let sk = seckey(9);
        let e = Event::sign_new(
            &sk,
            1,
            vec![vec!["h".into(), "chan-1".into()]],
            "hello buzz",
            1_700_000_000,
        )
        .unwrap();
        assert_eq!(e.verify(), Ok(()));
        assert_eq!(e.id, e.compute_id().unwrap());
        assert_eq!(e.kind, 1);
        assert_eq!(e.channel_id(), Some("chan-1"));
        assert_eq!(e.content, "hello buzz");
        assert_eq!(e.created_at, 1_700_000_000);
        assert_ne!(e.sig, [0u8; 64]);
    }

    #[test]
    fn sign_new_round_trips_through_the_wire_and_still_verifies() {
        let e = Event::sign_new(&seckey(3), 30023, vec![], "long form \u{1F680}", 42).unwrap();
        let json = serde_json::to_string(&e).unwrap();
        let parsed: Event = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed, e);
        assert_eq!(parsed.verify(), Ok(()));
    }

    #[test]
    fn tampering_with_content_fails_the_id_check_not_the_signature_check() {
        let mut e = Event::sign_new(&seckey(5), 1, vec![], "original", 100).unwrap();
        e.content = "tampered".into();
        assert_eq!(e.verify(), Err(BuzzError::IdMismatch));
    }

    #[test]
    fn tampering_with_any_signed_field_fails_verification() {
        let base = Event::sign_new(
            &seckey(6),
            1,
            vec![vec!["h".into(), "chan-1".into()]],
            "body",
            100,
        )
        .unwrap();

        let mut created = base.clone();
        created.created_at += 1;
        let mut kind = base.clone();
        kind.kind = 7;
        let mut tags = base.clone();
        tags.tags = vec![vec!["h".into(), "chan-2".into()]];
        let mut author = base.clone();
        author.pubkey = [0x02; 32];

        for variant in [created, kind, tags, author] {
            assert!(variant.verify().is_err(), "{variant:?} verified");
        }
    }

    #[test]
    fn a_tampered_signature_fails_while_the_id_still_checks_out() {
        let mut e = Event::sign_new(&seckey(7), 1, vec![], "body", 100).unwrap();
        // Flip one bit of the signature: the id is untouched, so the id check
        // passes and only the Schnorr check can catch this.
        e.sig[63] ^= 0x01;
        assert_eq!(e.compute_id().unwrap(), e.id);
        assert_eq!(e.verify(), Err(BuzzError::BadSignature));

        let mut zeroed = Event::sign_new(&seckey(7), 1, vec![], "body", 100).unwrap();
        zeroed.sig = [0u8; 64];
        assert_eq!(zeroed.verify(), Err(BuzzError::BadSignature));
    }

    #[test]
    fn a_signature_from_another_key_does_not_verify() {
        let mine = Event::sign_new(&seckey(11), 1, vec![], "body", 100).unwrap();
        let theirs = Event::sign_new(&seckey(12), 1, vec![], "body", 100).unwrap();
        assert_ne!(mine.pubkey, theirs.pubkey);

        // Same id (same fields except pubkey → different canonical form), so
        // splice the signature onto the other author's event and confirm the
        // graft is refused.
        let mut spliced = mine.clone();
        spliced.sig = theirs.sig;
        assert_eq!(spliced.verify(), Err(BuzzError::BadSignature));
    }

    #[test]
    fn an_id_that_is_merely_close_is_still_refused() {
        let mut e = Event::sign_new(&seckey(13), 1, vec![], "body", 100).unwrap();
        e.id[0] ^= 0x01;
        assert_eq!(e.verify(), Err(BuzzError::IdMismatch));
    }

    #[test]
    fn signing_refuses_an_out_of_range_secret_key() {
        // Zero is not a scalar; neither is the group order itself.
        assert!(matches!(
            Event::sign_new(&[0u8; 32], 1, vec![], "x", 1),
            Err(BuzzError::InvalidEvent(_))
        ));
        let order = [
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xfe, 0xba, 0xae, 0xdc, 0xe6, 0xaf, 0x48, 0xa0, 0x3b, 0xbf, 0xd2, 0x5e, 0x8c,
            0xd0, 0x36, 0x41, 0x41,
        ];
        assert!(matches!(
            Event::sign_new(&order, 1, vec![], "x", 1),
            Err(BuzzError::InvalidEvent(_))
        ));
    }

    #[test]
    fn signing_refuses_to_mint_an_event_this_relay_would_reject() {
        let too_big = "a".repeat(MAX_SERIALIZED_EVENT_BYTES + 1);
        assert!(matches!(
            Event::sign_new(&seckey(17), 1, vec![], &too_big, 1),
            Err(BuzzError::TooLarge { .. })
        ));
    }

    #[test]
    fn signing_is_deterministic_for_the_same_inputs() {
        // No entropy source exists on this platform, so signing must be a pure
        // function of key and message — two signings of one event agree, and a
        // stored signature stays verifiable after a rewrite.
        let a = Event::sign_new(&seckey(19), 1, vec![], "body", 100).unwrap();
        let b = Event::sign_new(&seckey(19), 1, vec![], "body", 100).unwrap();
        assert_eq!(a, b);
    }
}
