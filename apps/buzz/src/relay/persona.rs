//! The persona command surface: what an agent may DO in the workspace,
//! entered by a function call from the in-process agent runtime
//! (`crate::agents`).
//!
//! Every write here runs the SAME chain a client submission runs — registry
//! check, relay-held key check, then [`crate::relay::ingress::ingest_event`]
//! (restriction, membership, moderation authority, rate limits, channel
//! derivation, write authorization, admission, the guarded saga) — so a
//! persona enjoys no second, subtly different write path. There is no seam
//! token: the caller IS this application, and the authority a command needs
//! is decided by the registry + the persona's relay-held key.
//!
//! ## Determinism under retries
//!
//! `correlation_id` is the durable dedup identity: the event timestamp is
//! reserved under it (`agentcmd/{community}/{correlation}`) on first
//! execution, so a model retry of the same tool call — or a redelivered turn
//! that re-runs the whole loop — rebuilds the SAME signed event (id and all)
//! and the ingest admission absorbs it.

use ferrite_app::serde_json::{self, Value};
use ferrite_app::AppContext;

use crate::relay::agents::{CommandReservation, Persona, Registry};
use crate::relay::event::Event;
use crate::relay::ingress::{ingest_event, window_page, IngestOutcome};
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::{kind, BuzzError, Result, DEFAULT_COMMUNITY};

/// What a persona wants done. Fixed vocabulary: the kinds a persona can mint
/// are decided HERE, not by the model.
#[derive(Debug, Clone)]
pub enum PersonaOp {
    /// Post a chat message (kind 9) into a channel, optionally as a thread
    /// reply to `reply_to` (an event id the thread step resolves ancestry for).
    Post {
        channel: String,
        content: String,
        reply_to: Option<String>,
    },
    /// React (kind 7) to a target event.
    React { target: String, emoji: String },
    /// Read a channel's window as the persona (access-gated like any reader).
    Read {
        channel: String,
        cursor: Option<String>,
        limit: Option<usize>,
    },
}

/// Execute one persona command. Fail-closed at every step: the persona must
/// be registered, its key must be seeded AND match the registry pubkey — and
/// then the write goes through [`ingest_event`], the same path a client
/// submission takes, so a persona is subject to membership, moderation, rate
/// limits, and the audit chain like anyone else.
pub async fn execute(
    ctx: &AppContext,
    agent: &str,
    correlation_id: &str,
    op: PersonaOp,
) -> Result<Value> {
    execute_for(ctx, agent, None, correlation_id, op).await
}

/// [`execute`] on behalf of `requester` (a member's 64-hex pubkey), the member
/// whose mention, ACP prompt or schedule started the turn.
///
/// A persona is a deputy, not an independent principal: a read returns only
/// what BOTH the persona and the requester may see, so a member cannot have a
/// persona that sits in a private room read it out to them. Writes keep the
/// persona's own gates (it speaks only where it is a member).
pub async fn execute_for(
    ctx: &AppContext,
    agent: &str,
    requester: Option<&str>,
    correlation_id: &str,
    op: PersonaOp,
) -> Result<Value> {
    if correlation_id.is_empty() || correlation_id.len() > 128 {
        return Err(BuzzError::InvalidEvent(
            "persona correlation id must be 1..=128 bytes".into(),
        ));
    }
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;

    // The persona must be registered; the registry maps its agent id to the
    // ONE pubkey it may act as.
    let registry_bytes = store::get_raw(&mut config, crate::relay::agents::REGISTRY_CONFIG)
        .await?
        .ok_or_else(|| BuzzError::Unauthorized("no personas are registered".into()))?;
    let registry = Registry::parse(&registry_bytes)?;
    let persona = registry
        .by_agent(agent)
        .ok_or_else(|| BuzzError::Unauthorized(format!("{agent:?} is not a registered persona")))?;

    match &op {
        PersonaOp::Read {
            channel,
            cursor,
            limit,
        } => {
            let limit = limit.unwrap_or(50).clamp(1, 200);
            let mut page = window_page(
                ctx,
                community,
                channel,
                &persona.pubkey,
                cursor.as_deref(),
                limit,
                now_ms,
            )
            .await?;
            if let Some(requester) = requester {
                let Some(member) = crate::relay::crypto::from_hex32(requester) else {
                    return Err(BuzzError::Unauthorized(
                        "the member this turn acts for is not a workspace identity".into(),
                    ));
                };
                if member != persona.pubkey {
                    let theirs = window_page(
                        ctx,
                        community,
                        channel,
                        &member,
                        cursor.as_deref(),
                        limit,
                        now_ms,
                    )
                    .await?;
                    page = intersect_windows(page, &theirs);
                }
            }
            Ok(serde_json::json!({ "ok": true, "window": page }))
        }
        PersonaOp::Post { .. } | PersonaOp::React { .. } => {
            let seckey = persona_seckey(ctx, &mut config, community, persona).await?;

            // Pin the event timestamp under the correlation id BEFORE building
            // the event: a redelivered turn or a model-retried call then
            // rebuilds a byte-identical event (deterministic signing) whose id
            // the admission marker absorbs — the no-double-post rule.
            let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
            let created_at =
                reserve_created_at(&mut events, community, correlation_id, now_ms / 1000).await?;

            let (kind_, tags, content) = match &op {
                PersonaOp::Post {
                    channel,
                    content,
                    reply_to,
                } => {
                    if content.trim().is_empty() {
                        return Err(BuzzError::InvalidEvent("post content is empty".into()));
                    }
                    let mut tags = vec![vec!["h".to_string(), channel.clone()]];
                    if let Some(parent) = reply_to {
                        tags.extend(reply_tags(&mut events, community, channel, parent).await?);
                    }
                    (kind::KIND_STREAM_MESSAGE, tags, content.clone())
                }
                PersonaOp::React { target, emoji } => (
                    kind::KIND_REACTION,
                    vec![vec!["e".to_string(), target.clone()]],
                    emoji.clone(),
                ),
                PersonaOp::Read { .. } => unreachable!("handled above"),
            };

            let event = Event::sign_new(&seckey, kind_, tags, &content, created_at)?;
            match ingest_event(ctx, &persona.pubkey, &event, now_ms).await? {
                IngestOutcome::Accepted {
                    event_id_hex,
                    duplicate,
                } => Ok(serde_json::json!({
                    "ok": true,
                    "event_id": event_id_hex,
                    "duplicate": duplicate,
                })),
                IngestOutcome::ReactionDuplicate { event_id_hex } => Ok(serde_json::json!({
                    "ok": true,
                    "event_id": event_id_hex,
                    "duplicate": true,
                })),
            }
        }
    }
}

/// Load and validate a persona's relay-held signing key.
///
/// Secret-first, config-fallback — the `relay_identity` pattern, because the
/// key IS a credential: on a live deployment the only owner-gated write
/// channel is `set_secret`, so `buzz-keyring-{agent}` is how an operator
/// seeds a persona key at all (the operator HTTP face refuses every
/// `/`-prefixed config key by design, `keyring/{c}/{agent}` included). The
/// config row remains for the gates, which inject an empty secrets map.
///
/// Either way the value is 64-hex and its derived pubkey MUST equal the
/// registry's — a mis-seeded key acting as the wrong identity is an
/// integrity failure, not a fallback.
pub(crate) async fn persona_seckey(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    persona: &Persona,
) -> Result<[u8; 32]> {
    let hex = match ctx.secret(&format!("buzz-keyring-{}", persona.agent)) {
        Some(injected) => injected,
        None => {
            let key = keys::keyring(community, &persona.agent);
            let bytes = store::get_raw(config, &key).await?.ok_or_else(|| {
                BuzzError::Unauthorized(format!("persona {:?} has no seeded key", persona.agent))
            })?;
            String::from_utf8_lossy(&bytes).into_owned()
        }
    };
    let seckey = crate::relay::crypto::from_hex32(hex.trim())
        .ok_or_else(|| BuzzError::Internal("persona key is not 32-byte hex".into()))?;
    let derived = crate::relay::crypto::schnorr::xonly_pubkey(&seckey)
        .ok_or_else(|| BuzzError::Internal("persona key is out of range".into()))?;
    if derived != persona.pubkey {
        return Err(BuzzError::Internal(
            "persona key does not match the registered pubkey".into(),
        ));
    }
    Ok(seckey)
}

/// Reserve the event timestamp for one command, exactly once: a create-if-
/// absent on `agentcmd/{c}/{correlation}`; the loser of a race (or a
/// redelivered turn) reads the winner's value and rebuilds the same event.
async fn reserve_created_at(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    correlation: &str,
    now_secs: u64,
) -> Result<u64> {
    let key = keys::agent_cmd(community, correlation);
    let reservation = CommandReservation {
        created_at: now_secs,
    };
    let bytes = store::encode_doc(&reservation)?;
    match events.put_if_version(&key, &bytes, None).await {
        Ok(Ok(_)) => Ok(now_secs),
        // Already reserved (an earlier execution, or a lost response to our
        // own create): read the pinned value.
        Ok(Err(_)) | Err(_) => {
            let existing: Option<CommandReservation> = store::get_doc(events, &key).await?;
            existing
                .map(|r| r.created_at)
                .ok_or_else(|| BuzzError::Conflict("command reservation raced and vanished".into()))
        }
    }
}

/// NIP-10 tags for a reply to `parent`: resolve the parent's thread root so
/// the materialization step files the reply under the right thread. A parent
/// with no thread row IS a root; a direct reply to a root carries the single
/// `reply` marker (root = parent = reply, the shape `thread_ancestry` decodes).
///
/// The parent must live in the channel the reply is posted to. Any other parent
/// (another room, a DM, a global event) answers the same "not found" as a
/// missing one: otherwise the reply is an existence oracle across rooms, and a
/// private thread's root id would be stamped onto a post in an open room
/// (finding_61967f1402c981bc).
async fn reply_tags(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    channel: &str,
    parent_hex: &str,
) -> Result<Vec<Vec<String>>> {
    let Some(parent_id) = crate::relay::crypto::from_hex32(parent_hex) else {
        return Err(BuzzError::InvalidEvent(
            "reply_to is not a 32-byte event id".into(),
        ));
    };
    let in_channel = crate::relay::query::event_by_id(events, community, &parent_id)
        .await?
        .is_some_and(|parent| {
            !crate::relay::kind::is_global_only_kind(parent.kind)
                && parent.channel_id() == Some(channel)
        });
    if !in_channel {
        return Err(BuzzError::InvalidEvent("reply_to event not found".into()));
    }
    let meta = crate::relay::effects::thread_meta_of(events, community, parent_hex).await?;
    let root = meta
        .map(|m| m.root)
        .filter(|root| !root.eq_ignore_ascii_case(parent_hex));
    Ok(match root {
        Some(root) => vec![
            vec!["e".to_string(), root, String::new(), "root".to_string()],
            vec![
                "e".to_string(),
                parent_hex.to_string(),
                String::new(),
                "reply".to_string(),
            ],
        ],
        None => vec![vec![
            "e".to_string(),
            parent_hex.to_string(),
            String::new(),
            "reply".to_string(),
        ]],
    })
}

fn storage(e: std::io::Error) -> BuzzError {
    BuzzError::Storage(e.to_string())
}

/// Keep only the rows of `ours` whose event also appears in `theirs`. Both
/// pages read the same timeline with the same cursor and limit, so the bounds
/// stay ours: paging on continues through rows neither reader loses.
fn intersect_windows(mut ours: Value, theirs: &Value) -> Value {
    let event_id = |row: &Value| row.get("event").and_then(|event| event.get("id")).cloned();
    let visible: Vec<Value> = theirs
        .get("events")
        .and_then(Value::as_array)
        .map(|rows| rows.iter().filter_map(event_id).collect())
        .unwrap_or_default();
    if let Some(rows) = ours.get_mut("events").and_then(Value::as_array_mut) {
        rows.retain(|row| event_id(row).is_some_and(|id| visible.contains(&id)));
    }
    ours
}

#[cfg(test)]
mod requester_tests {
    use super::intersect_windows;
    use ferrite_app::serde_json::json;

    #[test]
    fn a_persona_read_keeps_only_rows_the_requester_also_sees() {
        let persona = json!({"ok": true, "events": [
            {"event": {"id": "aa"}, "thread": null},
            {"event": {"id": "bb"}, "thread": null}
        ], "bounds": {"has_more": true, "next_cursor": "c"}});
        let member_sees_one = json!({"ok": true, "events": [{"event": {"id": "bb"}}], "bounds": {}});
        let kept = intersect_windows(persona.clone(), &member_sees_one);
        assert_eq!(kept["events"], json!([{"event": {"id": "bb"}, "thread": null}]));
        assert_eq!(kept["bounds"]["next_cursor"], "c");
        let member_sees_none = json!({"ok": true, "events": [], "bounds": {"has_more": false}});
        assert_eq!(intersect_windows(persona, &member_sees_none)["events"], json!([]));
    }
}
