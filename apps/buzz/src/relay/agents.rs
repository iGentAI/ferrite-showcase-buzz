//! The persona dispatch vocabulary: the registry, the dispatch gate, and the
//! trigger rendering.
//!
//! This module is deliberately pure — parsing and predicates only. The I/O
//! (config reads, the turn-queue publish) lives in [`crate::relay::ingress`]
//! beside the other serve loops, and the persona COMMAND surface (what an
//! agent may do in the workspace) lives in [`crate::relay::persona`]. The
//! split mirrors [`crate::relay::effects`]: decisions here, plumbing there.
//!
//! ## The seam, in one paragraph
//!
//! A registered persona is an `(agent id, pubkey)` pair: the agent id names an
//! agent definition in this app's own agent runtime (`crate::agents`), the
//! pubkey is that persona's workspace identity. When a stored channel message
//! `p`-tags a registered persona — and the persona could itself READ that
//! message — the relay publishes a durable [`crate::agents::turns::TurnRequest`]
//! onto the app's own `agent-turns` queue (a mention must survive a crash; the
//! turn pipeline's admission absorbs redeliveries). The turn's tool effects
//! come back through [`crate::relay::persona::execute`]: signed by the
//! persona's relay-held key and pushed through the same ingest saga as any
//! client event, so a persona enjoys no bypass of membership, moderation,
//! rate limits, or the audit chain. No seam tokens exist — both halves are
//! this one application.

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json;

use crate::relay::crypto::{from_hex32, to_hex};
use crate::relay::event::{Event, PubKey};
use crate::relay::kind;
use crate::relay::{BuzzError, Result};

/// The config key holding the persona registry: a JSON array of
/// `{ "agent": "<agent id>", "pubkey": "<64-hex>" }`.
pub const REGISTRY_CONFIG: &[u8] = b"buzz-agents";

/// A dispatched trigger message is clamped to this many bytes: the persona's
/// tools read the channel for context, so the trigger only has to orient the
/// turn, not carry the whole conversation.
pub const MAX_TRIGGER_BYTES: usize = 8 * 1024;

/// The harness a persona runs on when its registry entry does not say.
///
/// The in-platform runtime, because that is the one a deployment has by
/// construction: an entry that names nothing is asking for "this workspace's
/// own agent", not for a remote peer it never configured.
pub const DEFAULT_HARNESS: &str = "ferrite-cloud";

/// One registered persona.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Persona {
    /// The agent id in this app's agent runtime (its definition key).
    pub agent: String,
    /// The persona's workspace identity.
    pub pubkey: PubKey,
    /// Which backend runs this persona's turn — one of the ids the relay
    /// declares at `GET /buzz/api/agent-harnesses`. The relay does not run the
    /// turn itself and so does not interpret this beyond passing it on; the
    /// turn pipeline picks the backend from it.
    pub harness: String,
}

/// The operator-seeded persona registry.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Registry {
    pub personas: Vec<Persona>,
}

#[derive(Debug, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
struct RegistryEntry {
    agent: String,
    pubkey: String,
    /// Optional; absent means [`DEFAULT_HARNESS`].
    #[serde(default)]
    harness: Option<String>,
}

impl Registry {
    /// Parse the registry config document. Strict: a malformed document or a
    /// malformed entry is an error, not a silent skip — a registry the
    /// operator got wrong should fail loudly at dispatch time, not quietly
    /// route some personas and drop others.
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let entries: Vec<RegistryEntry> = serde_json::from_slice(bytes)
            .map_err(|e| BuzzError::Internal(format!("buzz-agents registry: {e}")))?;
        let mut personas: Vec<Persona> = Vec::with_capacity(entries.len());
        for entry in entries {
            if entry.agent.is_empty() || entry.agent.len() > 64 {
                return Err(BuzzError::Internal(format!(
                    "buzz-agents registry: agent id {:?} is empty or too long",
                    entry.agent
                )));
            }
            // The id names store prefixes and delegated scopes
            // (`{parent}.sub.{id}`), so it keeps the charset clean_agent_id
            // produces: no `.`, `/`, upper case or punctuation.
            if !entry
                .agent
                .bytes()
                .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
            {
                return Err(BuzzError::Internal(format!(
                    "buzz-agents registry: agent id {:?} must use only a-z, 0-9, - and _",
                    entry.agent
                )));
            }
            if personas.iter().any(|p| p.agent == entry.agent) {
                return Err(BuzzError::Internal(format!(
                    "buzz-agents registry: agent id {:?} is listed twice",
                    entry.agent
                )));
            }
            if from_hex32(&entry.agent).is_some() {
                return Err(BuzzError::Internal(format!(
                    "buzz-agents registry: agent id {:?} collides with 32-byte hex pubkeys",
                    entry.agent
                )));
            }
            let Some(pubkey) = from_hex32(&entry.pubkey) else {
                return Err(BuzzError::Internal(format!(
                    "buzz-agents registry: pubkey for {:?} is not 32-byte hex",
                    entry.agent
                )));
            };
            if personas.iter().any(|p| p.pubkey == pubkey) {
                return Err(BuzzError::Internal(format!(
                    "buzz-agents registry: the pubkey of {:?} is already registered",
                    entry.agent
                )));
            }
            let harness = entry.harness.unwrap_or_else(|| DEFAULT_HARNESS.to_string());
            if harness.is_empty() || harness.len() > 64 {
                return Err(BuzzError::Internal(format!(
                    "buzz-agents registry: harness for {:?} is empty or too long",
                    entry.agent
                )));
            }
            personas.push(Persona {
                agent: entry.agent,
                pubkey,
                harness,
            });
        }
        Ok(Self { personas })
    }

    /// Is this pubkey a registered persona? (The loop-prevention predicate:
    /// a persona's own message never triggers another persona.)
    #[must_use]
    pub fn is_persona(&self, pubkey: &PubKey) -> bool {
        self.personas.iter().any(|p| &p.pubkey == pubkey)
    }

    /// The persona an agent tool command names, if registered.
    #[must_use]
    pub fn by_agent(&self, agent: &str) -> Option<&Persona> {
        self.personas.iter().find(|p| p.agent == agent)
    }
}

/// The personas a stored event should wake: chat messages only (buzz's kind 9;
/// commands, reactions, ephemera, and relay-signed kinds never wake anyone),
/// channel-scoped only, `p`-tag mentions only, and never for an author who is
/// itself a registered persona — a persona mentioning a persona would
/// otherwise ping-pong turns forever. The read gate (may the persona SEE the
/// triggering message?) is applied by the caller, which owns channel access
/// resolution; this predicate is everything decidable from the event alone.
#[must_use]
pub fn dispatch_targets<'a>(
    registry: &'a Registry,
    event: &Event,
    channel: Option<&str>,
) -> Vec<&'a Persona> {
    if event.kind != kind::KIND_STREAM_MESSAGE || channel.is_none() {
        return Vec::new();
    }
    if registry.is_persona(&event.pubkey) {
        return Vec::new();
    }
    registry
        .personas
        .iter()
        .filter(|persona| {
            let hex = to_hex(&persona.pubkey);
            event.tag_values("p").any(|v| v.eq_ignore_ascii_case(&hex))
        })
        .collect()
}

/// Render the turn's trigger message: enough to orient the persona (who,
/// where, what), clamped to [`MAX_TRIGGER_BYTES`] on a char boundary.
#[must_use]
pub fn render_trigger(event: &Event, channel: &str) -> String {
    let mut content = event.content.clone();
    if content.len() > MAX_TRIGGER_BYTES {
        let mut cut = MAX_TRIGGER_BYTES;
        while !content.is_char_boundary(cut) {
            cut -= 1;
        }
        content.truncate(cut);
        content.push_str("\n[truncated]");
    }
    format!(
        "You were mentioned in the buzz workspace.\n\
         channel: {channel}\n\
         author: {author}\n\
         event: {id}\n\
         created_at_unix_seconds: {created_at}\n\n\
         {content}",
        author = to_hex(&event.pubkey),
        id = to_hex(&event.id),
        created_at = event.created_at,
    )
}

/// The thread every dispatch for one buzz channel lands on: one durable agent
/// session per channel, so the persona keeps channel context across turns.
/// (Channel ids are validated key components on this relay, so the mapping is
/// collision-free after the agents app's own `[A-Za-z0-9-_.]` clamp.)
#[must_use]
pub fn dispatch_thread(channel: &str) -> String {
    format!("buzz-{channel}")
}

/// The durable reservation pinning a persona command's event timestamp (and
/// with it the event id) across in-turn retries: the model may re-invoke a
/// tool, and the turn queue may redeliver a turn that outlived its visibility
/// window — both rebuild the SAME event, and the admission marker absorbs it.
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct CommandReservation {
    pub created_at: u64,
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::relay::crypto::schnorr;
    use crate::relay::crypto::sha256::sha256;

    fn actor(label: &str) -> ([u8; 32], PubKey) {
        let sk = sha256(label.as_bytes());
        let pk = schnorr::xonly_pubkey(&sk).unwrap();
        (sk, pk)
    }

    fn registry_json(entries: &[(&str, &PubKey)]) -> Vec<u8> {
        let entries: Vec<_> = entries
            .iter()
            .map(|(agent, pk)| serde_json::json!({ "agent": agent, "pubkey": to_hex(*pk) }))
            .collect();
        serde_json::to_vec(&entries).unwrap()
    }

    fn message(author_sk: &[u8; 32], tags: Vec<Vec<String>>, kind_: u32) -> Event {
        Event::sign_new(author_sk, kind_, tags, "hello agents", 1_700_000_000).unwrap()
    }

    #[test]
    fn registry_parses_and_refuses_malformed_entries() {
        let (_, pk) = actor("revka");
        let reg = Registry::parse(&registry_json(&[("revka", &pk)])).unwrap();
        assert_eq!(reg.personas.len(), 1);
        assert_eq!(reg.by_agent("revka").unwrap().pubkey, pk);
        assert!(reg.by_agent("other").is_none());
        assert!(reg.is_persona(&pk));

        assert!(Registry::parse(b"not json").is_err());
        assert!(Registry::parse(br#"[{"agent":"x","pubkey":"zz"}]"#).is_err());
        assert!(Registry::parse(br#"[{"agent":"","pubkey":""}]"#).is_err());
        // An empty registry is valid and wakes no one.
        assert_eq!(Registry::parse(b"[]").unwrap(), Registry::default());
        // Ids that would escape a store prefix or collide with a delegated
        // scope, and duplicate ids or pubkeys, are refused.
        for bad in ["a.sub.b", "a/b", "Revka", "re vka"] {
            assert!(Registry::parse(&registry_json(&[(bad, &pk)])).is_err(), "{bad}");
        }
        assert!(Registry::parse(&registry_json(&[("revka", &pk), ("revka", &pk)])).is_err());
        assert!(Registry::parse(&registry_json(&[("revka", &pk), ("mira", &pk)])).is_err());
    }

    #[test]
    fn gate_dispatches_only_mentioned_personas_on_channel_messages() {
        let (human_sk, _) = actor("human");
        let (_, revka_pk) = actor("revka");
        let (_, mira_pk) = actor("mira");
        let reg =
            Registry::parse(&registry_json(&[("revka", &revka_pk), ("mira", &mira_pk)])).unwrap();

        let mention = |pk: &PubKey| vec![vec!["p".to_string(), to_hex(pk)]];

        // A mention of one persona wakes exactly that persona.
        let e = message(&human_sk, mention(&revka_pk), kind::KIND_STREAM_MESSAGE);
        let targets = dispatch_targets(&reg, &e, Some("chan"));
        assert_eq!(targets.len(), 1);
        assert_eq!(targets[0].agent, "revka");

        // No channel → no dispatch (global events wake no one).
        assert!(dispatch_targets(&reg, &e, None).is_empty());

        // A non-message kind never dispatches, mention or not.
        let cmd = message(&human_sk, mention(&revka_pk), kind::KIND_NIP29_PUT_USER);
        assert!(dispatch_targets(&reg, &cmd, Some("chan")).is_empty());

        // No mention → no dispatch.
        let plain = message(&human_sk, vec![], kind::KIND_STREAM_MESSAGE);
        assert!(dispatch_targets(&reg, &plain, Some("chan")).is_empty());
    }

    #[test]
    fn gate_never_lets_personas_wake_each_other() {
        let (revka_sk, revka_pk) = actor("revka");
        let (_, mira_pk) = actor("mira");
        let reg =
            Registry::parse(&registry_json(&[("revka", &revka_pk), ("mira", &mira_pk)])).unwrap();

        // revka (a persona) mentions mira (a persona): NO dispatch — this is
        // the loop-prevention rule, and it must hold even though mira is
        // genuinely mentioned.
        let e = message(
            &revka_sk,
            vec![vec!["p".to_string(), to_hex(&mira_pk)]],
            kind::KIND_STREAM_MESSAGE,
        );
        assert!(dispatch_targets(&reg, &e, Some("chan")).is_empty());
    }

    #[test]
    fn trigger_rendering_is_bounded_and_names_the_source() {
        let (human_sk, human_pk) = actor("human");
        let e = message(&human_sk, vec![], kind::KIND_STREAM_MESSAGE);
        let rendered = render_trigger(&e, "chan-7");
        assert!(rendered.contains("chan-7"));
        assert!(rendered.contains(&to_hex(&human_pk)));
        assert!(rendered.contains(&to_hex(&e.id)));
        assert!(rendered.contains("created_at_unix_seconds: 1700000000"));
        assert!(rendered.contains("hello agents"));

        // A huge content is clamped on a char boundary, marked truncated.
        let big = Event::sign_new(
            &human_sk,
            kind::KIND_STREAM_MESSAGE,
            vec![],
            &"é".repeat(MAX_TRIGGER_BYTES),
            1_700_000_000,
        )
        .unwrap();
        let rendered = render_trigger(&big, "chan");
        assert!(rendered.len() < MAX_TRIGGER_BYTES + 256);
        assert!(rendered.ends_with("[truncated]"));
    }
}
