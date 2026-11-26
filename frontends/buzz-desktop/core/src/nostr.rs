//! Signing as an AGENT: its key, its events, and its NIP-98 header.
//!
//! A local ACP adapter — `claude-code-acp`, `codex acp`, Goose — is not a buzz
//! client. It speaks ACP to whoever spawned it and has no relay connection, no
//! identity and no idea a workspace exists. So when it answers, somebody has to
//! put those words in the room, and the only honest signer is the AGENT ITSELF:
//!
//! * the user's key would misattribute the agent's words to the user, and it is
//!   not the shell's to use anyway — the identity lives in the client;
//! * the relay cannot sign for it either (it holds no member's key);
//! * so the shell holds a key PER HARNESS, and an agent posts as itself.
//!
//! That is the same trust shape a cloud persona has — its own pubkey, its own
//! membership, its own moderation history — and it is what makes an agent's
//! message attributable, mutable by moderators, and refusable by the relay's
//! ordinary gates rather than smuggled in under a person's name.
//!
//! What it does NOT give: a local agent's authority is its own, so adding it to a
//! room is a workspace action a moderator performs on its pubkey. The shell can
//! report that pubkey ([`AgentKey::pubkey_hex`]); it cannot grant itself access.

use base64::Engine;
use k256::schnorr::signature::hazmat::PrehashSigner;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::{Error, Result};

/// NIP-98's HTTP authorization event kind.
pub const KIND_HTTP_AUTH: u32 = 27235;
/// NIP-01 profile metadata.
pub const KIND_PROFILE: u32 = 0;
/// A channel message.
pub const KIND_STREAM_MESSAGE: u32 = 9;

/// One agent's signing key.
///
/// Held in the OS keychain under `agent:<harness id>`, never in a config file and
/// never in an event — [`Self::to_hex`] exists so the keychain can store it and
/// for nothing else.
pub struct AgentKey {
    signing: k256::schnorr::SigningKey,
}

impl AgentKey {
    /// A fresh key from the OS entropy source.
    ///
    /// The desktop has a real RNG, which is why an agent here can have a key of
    /// its own at all: the relay derives its personas' keys instead precisely
    /// because a constrained app has no entropy.
    #[must_use]
    pub fn generate() -> Self {
        Self {
            signing: k256::schnorr::SigningKey::random(&mut rand_core::OsRng),
        }
    }

    pub fn from_hex(hex: &str) -> Result<Self> {
        let bytes = decode_hex(hex.trim())
            .ok_or_else(|| Error::Invalid("an agent key is not hex".into()))?;
        if bytes.len() != 32 {
            return Err(Error::Invalid("an agent key is not 32 bytes".into()));
        }
        let signing = k256::schnorr::SigningKey::from_bytes(&bytes)
            .map_err(|_| Error::Invalid("an agent key is out of range".into()))?;
        Ok(Self { signing })
    }

    /// The 64-hex secret, for the keychain and nothing else.
    #[must_use]
    pub fn to_hex(&self) -> String {
        encode_hex(&self.signing.to_bytes())
    }

    /// The key as NIP-19's `nsec`.
    ///
    /// For the ONE place a person is shown it: the panel the client's creation
    /// dialog offers for backup. It matters because an agent's whole history hangs
    /// off this key — losing it means the agent can never speak as itself again —
    /// and it never leaves the machine: the shell keeps it in the OS keychain and
    /// posts only signatures.
    #[must_use]
    pub fn nsec(&self) -> Option<String> {
        let bytes = self.signing.to_bytes();
        let mut secret = [0u8; 32];
        secret.copy_from_slice(&bytes);
        crate::bech32::nsec(&secret)
    }

    /// The x-only public key, which IS this agent's workspace identity.
    #[must_use]
    pub fn pubkey_hex(&self) -> String {
        encode_hex(&self.signing.verifying_key().to_bytes())
    }

    /// Sign one event.
    ///
    /// The id is the SHA-256 of NIP-01's canonical array
    /// `[0, pubkey, created_at, kind, tags, content]`, and the signature is over
    /// that id — in that order, because a signature over a STATED id would say
    /// nothing about the body.
    pub fn sign(
        &self,
        kind: u32,
        tags: Vec<Vec<String>>,
        content: &str,
        created_at: u64,
    ) -> Result<Event> {
        let pubkey = self.pubkey_hex();
        let canonical = canonical_serialization(&pubkey, created_at, kind, &tags, content)?;
        let id = Sha256::digest(canonical.as_bytes());
        let signature: k256::schnorr::Signature = self
            .signing
            .sign_prehash(&id)
            .map_err(|e| Error::Internal(format!("signing failed: {e}")))?;
        Ok(Event {
            id: encode_hex(&id),
            pubkey,
            created_at,
            kind,
            tags,
            content: content.to_string(),
            sig: encode_hex(&signature.to_bytes()),
        })
    }
}

/// The kind-0 profile that makes an agent NAMEABLE.
///
/// Without one, a client renders the agent as a truncated pubkey and its mention
/// autocomplete — which is built from profiles — never offers it, so the agent
/// cannot be asked for anything. This is the same event, with the same `bot: true`
/// flag, that the relay publishes for its own personas and for exactly the same
/// reason.
pub fn profile_event(key: &AgentKey, name: &str, created_at: u64) -> Result<Event> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Invalid("an agent profile needs a name".into()));
    }
    let content = serde_json::to_string(&serde_json::json!({
        "name": name,
        "display_name": name,
        // What a client draws the bot badge from.
        "bot": true,
    }))
    .map_err(|e| Error::Internal(format!("profile encode: {e}")))?;
    key.sign(KIND_PROFILE, Vec::new(), &content, created_at)
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u32,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

/// NIP-01's canonical array — what the id is the hash of.
pub fn canonical_serialization(
    pubkey: &str,
    created_at: u64,
    kind: u32,
    tags: &[Vec<String>],
    content: &str,
) -> Result<String> {
    serde_json::to_string(&(0u8, pubkey, created_at, kind, tags, content))
        .map_err(|e| Error::Internal(format!("canonical serialization: {e}")))
}

/// The `Authorization` header for one request, as NIP-98 defines it.
///
/// The `u` tag carries the request PATH, because that is what the relay's binding
/// check compares it against — a full URL there authenticates nothing, it simply
/// fails to match. The `payload` tag carries the body's SHA-256 whenever there is
/// a body, since some routes verify it and a header without it is refused there.
///
/// `nonce` must be unique per request: the relay BURNS an authorization event's
/// id on first use, so two identical requests would build one event and the
/// second would be refused as a replay.
pub fn nip98_header(
    key: &AgentKey,
    method: &str,
    path: &str,
    body: &[u8],
    created_at: u64,
    nonce: &str,
) -> Result<String> {
    if nonce.trim().is_empty() {
        return Err(Error::Internal(
            "a NIP-98 nonce must be supplied and non-empty".into(),
        ));
    }
    let mut tags = vec![
        vec!["u".to_string(), path.to_string()],
        vec!["method".to_string(), method.to_ascii_uppercase()],
        vec!["nonce".to_string(), nonce.to_string()],
    ];
    if !body.is_empty() {
        tags.push(vec![
            "payload".to_string(),
            encode_hex(&Sha256::digest(body)),
        ]);
    }
    let event = key.sign(KIND_HTTP_AUTH, tags, "", created_at)?;
    let json = serde_json::to_vec(&event)
        .map_err(|e| Error::Internal(format!("auth event encode: {e}")))?;
    Ok(format!(
        "Nostr {}",
        base64::engine::general_purpose::STANDARD.encode(json)
    ))
}

/// Where an agent's answer goes: the room, and the message it answers.
///
/// The parent matters as much as the room. A mention made INSIDE a thread has to
/// be answered in that thread — otherwise the reply appears at the top of the
/// channel, detached from the question that prompted it, and the person who asked
/// sees an answer to nothing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerTarget {
    pub channel: String,
    /// The message being answered, when the prompt came from one.
    pub parent: Option<String>,
}

impl AnswerTarget {
    /// A target, ignoring a blank parent (which is how a client spells "no
    /// thread" — an empty `e` tag would name an event that cannot exist).
    #[must_use]
    pub fn new(channel: &str, parent: Option<&str>) -> Self {
        Self {
            channel: channel.to_string(),
            parent: parent
                .map(str::trim)
                .filter(|id| !id.is_empty())
                .map(str::to_string),
        }
    }

    #[must_use]
    pub fn is_usable(&self) -> bool {
        !self.channel.trim().is_empty()
    }
}

pub fn channel_message(
    key: &AgentKey,
    target: &AnswerTarget,
    text: &str,
    created_at: u64,
) -> Result<Event> {
    if !target.is_usable() {
        return Err(Error::Invalid(
            "an agent message needs the channel it belongs to".into(),
        ));
    }
    if text.trim().is_empty() {
        return Err(Error::Invalid("an agent message needs something to say".into()));
    }
    let mut tags = vec![vec!["h".to_string(), target.channel.clone()]];
    if let Some(parent) = &target.parent {
        // NIP-10's MARKED form. The relay resolves a reply's ancestry from this
        // fourth field and from nothing else, so a bare `["e", id]` would be
        // stored as a ROOT — a reply detached from its own question, which is
        // exactly what this tag exists to prevent.
        tags.push(vec![
            "e".to_string(),
            parent.clone(),
            String::new(),
            "reply".to_string(),
        ]);
    }
    key.sign(KIND_STREAM_MESSAGE, tags, text, created_at)
}

/// A per-request nonce source.
///
/// Monotonic within the process and distinct across processes with distinct
/// seeds. Derived rather than random so it is testable, and unique because the
/// counter is — see [`nip98_header`] for why uniqueness is not optional.
pub struct NonceSource {
    seed: String,
    counter: std::sync::atomic::AtomicU64,
}

impl NonceSource {
    #[must_use]
    pub fn new(seed: &str) -> Self {
        Self {
            seed: seed.to_string(),
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    #[must_use]
    pub fn next(&self) -> String {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let digest = Sha256::digest(format!("{}|{n}", self.seed).as_bytes());
        encode_hex(&digest[..16])
    }
}

fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn decode_hex(hex: &str) -> Option<Vec<u8>> {
    if !hex.len().is_multiple_of(2) {
        return None;
    }
    (0..hex.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(hex.get(i..i + 2)?, 16).ok())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use k256::schnorr::signature::hazmat::PrehashVerifier;

    /// A fixed key, so the assertions below are about the SIGNING rather than
    /// about entropy.
    fn key() -> AgentKey {
        AgentKey::from_hex(&"07".repeat(32)).expect("a valid key")
    }

    #[test]
    fn a_key_round_trips_and_names_its_own_identity() {
        let key = key();
        assert_eq!(key.to_hex(), "07".repeat(32));
        let pubkey = key.pubkey_hex();
        assert_eq!(pubkey.len(), 64, "an x-only pubkey is 32 bytes");
        assert!(pubkey.bytes().all(|b| b.is_ascii_hexdigit()));
        // Reading the same secret back gives the same identity — which is what
        // makes a keychain entry a durable agent rather than a new agent per
        // launch.
        assert_eq!(AgentKey::from_hex(&key.to_hex()).expect("reread").pubkey_hex(), pubkey);

        // A generated key is usable and different.
        let fresh = AgentKey::generate();
        assert_ne!(fresh.pubkey_hex(), pubkey);
        assert_eq!(fresh.to_hex().len(), 64);

        for bad in ["", "nothex", &"aa".repeat(31), &"00".repeat(32)] {
            assert!(AgentKey::from_hex(bad).is_err(), "{bad:?} must be refused");
        }
    }

    #[test]
    fn an_events_id_is_the_hash_of_nip01s_canonical_array() {
        let key = key();
        let event = key
            .sign(
                KIND_STREAM_MESSAGE,
                vec![vec!["h".into(), "general".into()]],
                "hello",
                1_700_000_000,
            )
            .expect("sign");
        // Recomputed INDEPENDENTLY here rather than taken from the signer, so a
        // change to either would fail this.
        let canonical = format!(
            "[0,\"{}\",1700000000,9,[[\"h\",\"general\"]],\"hello\"]",
            key.pubkey_hex()
        );
        assert_eq!(
            event.id,
            encode_hex(&Sha256::digest(canonical.as_bytes())),
            "the id must be the canonical form's digest"
        );
        assert_eq!(event.pubkey, key.pubkey_hex());
        assert_eq!(event.kind, KIND_STREAM_MESSAGE);
    }

    #[test]
    fn a_signature_verifies_against_the_id_it_was_made_over() {
        let key = key();
        let event = key.sign(9, vec![], "verify me", 42).expect("sign");
        let id = decode_hex(&event.id).expect("hex id");
        let sig_bytes = decode_hex(&event.sig).expect("hex sig");
        let signature =
            k256::schnorr::Signature::try_from(sig_bytes.as_slice()).expect("a BIP-340 sig");
        key.signing
            .verifying_key()
            .verify_prehash(&id, &signature)
            .expect("the signature must verify over the event id");

        // ...and NOT over a different id, which is what makes the id
        // load-bearing rather than decorative.
        let other = key.sign(9, vec![], "something else", 42).expect("sign");
        let other_id = decode_hex(&other.id).expect("hex");
        assert!(key
            .signing
            .verifying_key()
            .verify_prehash(&other_id, &signature)
            .is_err());
    }

    #[test]
    fn a_channel_message_carries_its_room_and_refuses_to_be_empty() {
        let key = key();
        let target = AnswerTarget::new("general", None);
        let event = channel_message(&key, &target, "the answer", 7).expect("sign");
        assert_eq!(event.kind, KIND_STREAM_MESSAGE);
        assert_eq!(event.tags, vec![vec!["h".to_string(), "general".to_string()]]);
        assert_eq!(event.content, "the answer");
        // A message with no room could not be delivered, and one with no words is
        // not an answer.
        assert!(channel_message(&key, &AnswerTarget::new("", None), "x", 7).is_err());
        assert!(channel_message(&key, &target, "   ", 7).is_err());
    }

    #[test]
    fn an_answer_to_a_thread_replies_in_that_thread() {
        // The relay reads ancestry from NIP-10's MARKED form only, so the marker
        // is what keeps the reply attached to its question rather than appearing
        // at the top of the channel as a new conversation.
        let key = key();
        let parent = "ab".repeat(32);
        let target = AnswerTarget::new("general", Some(&parent));
        let event = channel_message(&key, &target, "answered", 7).expect("sign");
        assert_eq!(
            event.tags,
            vec![
                vec!["h".to_string(), "general".to_string()],
                vec!["e".to_string(), parent.clone(), String::new(), "reply".to_string()],
            ]
        );

        // A blank parent is how a client spells "no thread"; an empty `e` tag
        // would name an event that cannot exist.
        for blank in ["", "   "] {
            let target = AnswerTarget::new("general", Some(blank));
            assert!(target.parent.is_none(), "{blank:?}");
            let event = channel_message(&key, &target, "answered", 7).expect("sign");
            assert_eq!(event.tags.len(), 1, "only the room");
        }
    }

    #[test]
    fn the_auth_header_binds_the_path_the_method_and_the_body() {
        let key = key();
        let header = nip98_header(&key, "post", "/buzz/api/events", b"{}", 7, "n1")
            .expect("header");
        let encoded = header.strip_prefix("Nostr ").expect("the NIP-98 prefix");
        let json = base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .expect("base64");
        let event: Event = serde_json::from_slice(&json).expect("an event");
        assert_eq!(event.kind, KIND_HTTP_AUTH);
        let tag = |name: &str| {
            event
                .tags
                .iter()
                .find(|t| t.first().is_some_and(|k| k == name))
                .and_then(|t| t.get(1))
                .cloned()
        };
        // The PATH, not a URL: the relay compares this against the request path.
        assert_eq!(tag("u").as_deref(), Some("/buzz/api/events"));
        assert_eq!(tag("method").as_deref(), Some("POST"), "uppercased");
        assert_eq!(tag("nonce").as_deref(), Some("n1"));
        assert_eq!(
            tag("payload").as_deref(),
            Some(encode_hex(&Sha256::digest(b"{}")).as_str())
        );

        // A bodyless request carries no payload tag at all.
        let bodyless = nip98_header(&key, "GET", "/buzz/api/members", b"", 7, "n2")
            .expect("header");
        let json = base64::engine::general_purpose::STANDARD
            .decode(bodyless.strip_prefix("Nostr ").expect("prefix"))
            .expect("base64");
        let event: Event = serde_json::from_slice(&json).expect("event");
        assert!(!event
            .tags
            .iter()
            .any(|t| t.first().is_some_and(|k| k == "payload")));

        // An empty nonce is refused rather than sent: the relay burns these ids,
        // so a repeat is a replay.
        assert!(nip98_header(&key, "GET", "/x", b"", 7, "  ").is_err());
    }

    #[test]
    fn a_key_offers_itself_as_an_nsec_for_backup() {
        // The same bytes the keychain holds, in the form a person can paste into
        // another client — which is the whole point of showing it at all.
        let key = key();
        let nsec = key.nsec().expect("nsec");
        assert!(nsec.starts_with("nsec1"));
        assert_eq!(
            nsec,
            crate::bech32::nsec(&[7u8; 32]).expect("the same bytes"),
            "the nsec must encode the key the agent actually signs with"
        );
        // A different key is a different nsec.
        assert_ne!(nsec, AgentKey::generate().nsec().expect("nsec"));
    }

    #[test]
    fn a_profile_names_the_agent_and_flags_it_a_bot() {
        // Without this event a client renders a truncated pubkey and never offers
        // the agent in a mention picker — so it cannot be asked for anything.
        let key = key();
        let event = profile_event(&key, "Claude Code", 7).expect("sign");
        assert_eq!(event.kind, KIND_PROFILE);
        assert!(event.tags.is_empty(), "a profile is not scoped to a room");
        let content: serde_json::Value =
            serde_json::from_str(&event.content).expect("json content");
        assert_eq!(content["name"], "Claude Code");
        assert_eq!(content["display_name"], "Claude Code");
        assert_eq!(content["bot"], true, "what the badge is drawn from");
        // A nameless profile is not a profile.
        assert!(profile_event(&key, "   ", 7).is_err());
    }

    #[test]
    fn nonces_do_not_repeat() {
        let source = NonceSource::new("harness-claude/boot-1");
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..1_000 {
            assert!(seen.insert(source.next()), "a nonce repeated");
        }
        // Two harnesses with different seeds never collide.
        let other = NonceSource::new("harness-codex/boot-1");
        assert!(!seen.contains(&other.next()));
    }

    #[test]
    fn two_identical_requests_build_two_different_auth_events() {
        // The relay burns an authorization event's id, so the SAME call made
        // twice must not build the same event — the second would be refused.
        let key = key();
        let nonces = NonceSource::new("seed");
        let event_id = || {
            let header = nip98_header(&key, "POST", "/buzz/api/events", b"{}", 7, &nonces.next())
                .expect("header");
            let json = base64::engine::general_purpose::STANDARD
                .decode(header.strip_prefix("Nostr ").expect("prefix"))
                .expect("base64");
            serde_json::from_slice::<Event>(&json).expect("event").id
        };
        assert_ne!(event_id(), event_id());
    }
}
