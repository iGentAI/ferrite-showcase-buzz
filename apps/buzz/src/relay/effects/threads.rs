//! Thread ancestry, reply depth and the per-thread summary.

use ferrite_app::tracing;
use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde::Deserialize;
use ferrite_app::serde::Serialize;
use crate::relay::error::BuzzError;
use crate::relay::error::Result;
use crate::relay::event::Event;
use crate::relay::keys;
use crate::relay::kind;
use crate::relay::store;
use super::{CAS_RETRIES, materializes_thread};
use super::reactions::{reaction_target_and_emoji};

/// The deepest a reply may sit (buzz's `depth > 100` rejection).
pub const MAX_THREAD_DEPTH: u32 = 100;

/// Per-event thread metadata — the port of buzz's `thread_metadata` row.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ThreadMeta {
    /// Hex id of the thread root.
    pub root: String,
    /// Hex id of the direct parent, if this event is a reply.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<String>,
    /// 0 for a root, `parent.depth + 1` for a reply.
    pub depth: u32,
    /// Direct children of this event.
    #[serde(default)]
    pub reply_count: u32,
    /// Whole subtree under this event — maintained on the root's own row.
    #[serde(default)]
    pub descendant_count: u32,
    /// A depth-1 broadcast reply surfaces at the top level of the window.
    #[serde(default)]
    pub broadcast: bool,
    /// Unix-ms of the most recent direct reply.
    #[serde(default)]
    pub last_reply_at: u64,
    /// Tagged ids (`{reply}:r`, `{reply}:d`) of the most recent replies counted
    /// into this row, so a re-driven effect adds each increment once.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub counted: Vec<String>,
    /// Set on rows written by the recoverable counting effect. A reply row
    /// without it was written by the earlier effect, whose existence meant the
    /// counters were already applied.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub tracked: bool,
}

/// How many counted reply ids a thread row remembers for deduplication.
pub(super) const THREAD_COUNTED_WINDOW: usize = 64;

/// NIP-10 ancestry from an event's `e` tags with a 4th-field marker
/// (`["e", id, relay, "root"|"reply"]`), last-wins per marker (buzz's
/// `resolve_nip10_thread_meta`). Returns `(root_hex, parent_hex)`:
/// `(root,reply)` ⇒ parent = reply; `(None,reply)` ⇒ root = parent = reply;
/// `(root,None)` and neither ⇒ `None` (not a reply).
#[must_use]
pub fn thread_ancestry(event: &Event) -> Option<(String, String)> {
    let mut root = None;
    let mut reply = None;
    for t in &event.tags {
        if t.first().map(String::as_str) != Some("e") {
            continue;
        }
        let (Some(id), Some(marker)) = (t.get(1), t.get(3)) else {
            continue;
        };
        if id.len() != 64 || !id.bytes().all(|b| b.is_ascii_hexdigit()) {
            continue;
        }
        match marker.as_str() {
            "root" => root = Some(id.to_ascii_lowercase()),
            "reply" => reply = Some(id.to_ascii_lowercase()),
            _ => {}
        }
    }
    match (root, reply) {
        (Some(root), Some(reply)) => Some((root, reply)),
        (None, Some(reply)) => Some((reply.clone(), reply)),
        _ => None,
    }
}

/// Whether `event` is a broadcast reply (`["broadcast","1"]`).
pub(super) fn is_broadcast(event: &Event) -> bool {
    event.tags.iter().any(|t| {
        t.first().map(String::as_str) == Some("broadcast")
            && t.get(1).map(String::as_str) == Some("1")
    })
}

/// A reply's `(root, parent, depth)`, or `None` for an event that is not one.
///
/// The parent's depth decides this event's. If the parent has metadata, use
/// `parent.depth + 1`; otherwise it is a root (`depth 1`) or a reply to a reply
/// we never materialized (`depth 2`), mirroring buzz's fallback.
pub(super) async fn reply_depth(
    store: &mut StoreClusterClient,
    community: &str,
    event: &Event,
) -> Result<Option<(String, String, u32)>> {
    let Some((root, parent)) = thread_ancestry(event) else {
        return Ok(None);
    };
    let parent_key = keys::thread_meta(community, &parent);
    let parent_meta = store::get_doc::<ThreadMeta>(store, &parent_key).await?;
    let depth = match &parent_meta {
        Some(m) => m.depth.saturating_add(1),
        None if parent == root => 1,
        None => 2,
    };
    Ok(Some((root, parent, depth)))
}

/// Refuse, BEFORE storage, a reply deeper than [`MAX_THREAD_DEPTH`].
///
/// This is validation, so it belongs on the pre-storage path: raised from the
/// effect step instead, the refusal came after the body and indexes were
/// written, leaving an event that reads served but the audit chain, the feed and
/// the saga's completion never saw.
pub async fn refuse_over_deep_reply(
    store: &mut StoreClusterClient,
    community: &str,
    event: &Event,
) -> Result<()> {
    if event.kind == kind::KIND_WORKFLOW_DEF {
        if let (Some(ch), Some(wf_id)) = (
            event.channel_id(),
            crate::relay::workflows::workflow_id(event),
        ) {
            if keys::is_safe_component(ch) && keys::is_safe_component(wf_id) {
                let wf_key = keys::workflow_definition_index(community, ch, wf_id);
                if let Some(raw) = store::get_raw(store, &wf_key).await? {
                    let existing_author =
                        crate::relay::workflows::indexed_workflow_author(store, community, &raw)
                            .await?;
                    match existing_author {
                        Some(author) if author == event.pubkey => {}
                        Some(_) => {
                            return Err(BuzzError::Unauthorized(
                                "workflow definition is owned by another author".into(),
                            ));
                        }
                        None => {
                            return Err(BuzzError::Unauthorized(
                                "workflow definition owner cannot be verified for replacement"
                                    .into(),
                            ));
                        }
                    }
                }
            }
        }
        return Ok(());
    }
    if event.kind == kind::KIND_REACTION {
        if let (Some(first_hex), Some((last_hex, _))) =
            (event.tag_values("e").next(), reaction_target_and_emoji(event))
        {
            if !first_hex.eq_ignore_ascii_case(&last_hex) {
                let Some(first_id) = crate::relay::crypto::from_hex32(first_hex) else {
                    return Err(BuzzError::InvalidEvent(
                        "reaction e tag is not a 32-byte id".into(),
                    ));
                };
                let Some(last_id) = crate::relay::crypto::from_hex32(&last_hex) else {
                    return Err(BuzzError::InvalidEvent(
                        "reaction e tag is not a 32-byte id".into(),
                    ));
                };
                let first_ch = effective_target_channel(store, community, &first_id).await?;
                let last_ch = effective_target_channel(store, community, &last_id).await?;
                if first_ch != last_ch {
                    return Err(BuzzError::InvalidEvent(
                        "reaction e tags must reference the same channel".into(),
                    ));
                }
            }
        }
        return Ok(());
    }
    if !materializes_thread(event.kind) {
        return Ok(());
    }
    match reply_depth(store, community, event).await? {
        Some((_, _, depth)) if depth > MAX_THREAD_DEPTH => Err(BuzzError::InvalidEvent(
            "thread depth limit exceeded".into(),
        )),
        _ => Ok(()),
    }
}

/// Materialize thread metadata for a channel-scoped reply. A non-reply writes
/// nothing.
///
/// A reply deeper than [`MAX_THREAD_DEPTH`] is refused before storage by
/// [`refuse_over_deep_reply`]; one that reaches this step anyway (its parent's
/// row materialized between the check and the effect) gets no thread row and
/// is logged, never refused: an error here would come after the event is
/// stored and wedge the saga short of its audit entry and feed publish.
///
/// The root and parent ids are the AUTHOR's claim (their `e` tags), so they are
/// resolved before any row is touched: each must be a stored event in the same
/// channel as this reply. Otherwise a member of an open room could bump the
/// counters of a private room's message, graft their reply onto a foreign
/// thread, or mint `thm/` rows for ids that do not exist. Such a reply is stored
/// and served as an ordinary top-level message; it just joins no thread, which
/// is the same no-refusal-after-storage rule as the depth case.
pub(super) async fn apply_thread(
    store: &mut StoreClusterClient,
    community: &str,
    event: &Event,
    channel: Option<&str>,
    now_ms: u64,
) -> Result<()> {
    let Some((root, parent, depth)) = reply_depth(store, community, event).await? else {
        return Ok(());
    };
    let self_hex = crate::relay::crypto::to_hex(&event.id);
    if depth > MAX_THREAD_DEPTH {
        tracing::warn!(
            "reply {self_hex} derived depth {depth} past the limit after admission; no thread row"
        );
        return Ok(());
    }
    for target in [&parent, &root] {
        if !is_thread_target(store, community, target, channel).await? {
            tracing::warn!(
                "reply {self_hex} names {target}, which is not a stored event in its channel; no thread row"
            );
            return Ok(());
        }
        if parent == root {
            break;
        }
    }

    // This event's own row. A row a descendant minted before this effect ran is
    // a stub (no parent): upgrade it in place, keeping its counters. Creating or
    // upgrading the row is NOT what marks the counters as applied: every drive
    // attempts both increments, and each row remembers which reply ids it has
    // already counted, so a crash between the two increments is recovered by
    // the re-drive without counting anything twice.
    let self_key = keys::thread_meta(community, &self_hex);
    let mut own_row_ready = false;
    for _ in 0..CAS_RETRIES {
        let current = store::get_doc_versioned::<ThreadMeta>(store, &self_key).await?;
        let (doc, expected) = match current {
            None => (
                ThreadMeta {
                    root: root.clone(),
                    parent: Some(parent.clone()),
                    depth,
                    broadcast: is_broadcast(event),
                    tracked: true,
                    ..ThreadMeta::default()
                },
                None,
            ),
            Some((existing, _)) if existing.parent.is_some() && !existing.tracked => {
                return Ok(());
            }
            Some((existing, _)) if existing.parent.is_some() => {
                own_row_ready = true;
                break;
            }
            Some((mut stub, version)) => {
                stub.root = root.clone();
                stub.parent = Some(parent.clone());
                stub.depth = depth;
                stub.broadcast = is_broadcast(event);
                stub.tracked = true;
                (stub, Some(version))
            }
        };
        if store::put_doc_if_version(store, &self_key, &doc, expected)
            .await?
            .is_ok()
        {
            own_row_ready = true;
            break;
        }
    }
    if !own_row_ready {
        return Err(BuzzError::Conflict("thread row contended".into()));
    }

    // Bump the parent's direct reply_count + last_reply_at.
    bump_thread(store, community, &parent, &root, now_ms, true, &self_hex).await?;
    // Bump the root's descendant_count (unconditionally, even when root == parent
    // — reply_count is direct children, descendant_count is the whole subtree).
    bump_thread(store, community, &root, &root, now_ms, false, &self_hex).await?;
    Ok(())
}

/// Whether `id_hex` names a stored event in `channel`: the only event a reply
/// written into `channel` may thread under.
///
/// A storage error propagates, so a transient failure re-drives the saga rather
/// than silently dropping the thread row. Any other failure (an integrity
/// fault on the stored body) reads as "not a target": it is permanent, and a
/// permanent error raised after storage would wedge the saga.
async fn effective_target_channel(
    store: &mut StoreClusterClient,
    community: &str,
    id: &crate::relay::event::EventId,
) -> Result<Option<Option<String>>> {
    let mut next_id = *id;
    for _ in 0..8 {
        let Some(target) = crate::relay::query::event_by_id(store, community, &next_id).await?
        else {
            return Ok(None);
        };
        if !matches!(target.kind, kind::KIND_REACTION | kind::KIND_DELETION) {
            let ch = if kind::is_global_only_kind(target.kind) {
                None
            } else if kind::is_group_projection(target.kind) {
                target
                    .channel_id()
                    .or_else(|| target.d_tag())
                    .map(str::to_string)
            } else {
                target.channel_id().map(str::to_string)
            };
            return Ok(Some(ch));
        }
        let next_hex = if target.kind == kind::KIND_REACTION {
            target
                .tag_values("e")
                .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                .last()
                .or_else(|| target.tag_values("e").last())
        } else {
            target.tag_values("e").next()
        };
        let Some(parsed) = next_hex.and_then(crate::relay::crypto::from_hex32) else {
            return Ok(None);
        };
        next_id = parsed;
    }
    Ok(None)
}

pub(super) async fn is_thread_target(
    store: &mut StoreClusterClient,
    community: &str,
    id_hex: &str,
    channel: Option<&str>,
) -> Result<bool> {
    let Some(id) = crate::relay::crypto::from_hex32(id_hex) else {
        return Ok(false);
    };
    match effective_target_channel(store, community, &id).await {
        Ok(Some(target_ch)) => Ok(target_ch.as_deref() == channel),
        Ok(None) => Ok(false),
        Err(e @ BuzzError::Storage(_)) => Err(e),
        Err(e) => {
            tracing::warn!("thread target {id_hex} unreadable: {e}");
            Ok(false)
        }
    }
}

/// Read-modify-write a thread-metadata row: `reply` bumps `reply_count` +
/// `last_reply_at`, otherwise `descendant_count`. Creates a stub row for a node
/// that has none yet (a root gets its row on its first reply, like buzz). CAS.
///
/// Only reached for nodes [`is_thread_target`] resolved, so a stub is never
/// minted for an id that names no event in the reply's channel.
pub(super) async fn bump_thread(
    store: &mut StoreClusterClient,
    community: &str,
    node: &str,
    root: &str,
    now_ms: u64,
    reply: bool,
    reply_id: &str,
) -> Result<()> {
    let tag = format!("{reply_id}:{}", if reply { 'r' } else { 'd' });
    let key = keys::thread_meta(community, node);
    for _ in 0..CAS_RETRIES {
        let current = store::get_doc_versioned::<ThreadMeta>(store, &key).await?;
        let (mut doc, expected) = match current {
            Some((doc, version)) => (doc, Some(version)),
            None => (
                // A stub row for a node whose own event has no metadata yet — a
                // root gets its row on its first reply (buzz's stub is depth 0).
                ThreadMeta {
                    root: root.to_string(),
                    depth: 0,
                    ..ThreadMeta::default()
                },
                None,
            ),
        };
        if doc.counted.contains(&tag) {
            return Ok(());
        }
        doc.counted.push(tag.clone());
        if doc.counted.len() > THREAD_COUNTED_WINDOW {
            doc.counted.remove(0);
        }
        if reply {
            doc.reply_count = doc.reply_count.saturating_add(1);
            doc.last_reply_at = now_ms;
        } else {
            doc.descendant_count = doc.descendant_count.saturating_add(1);
        }
        match store::put_doc_if_version(store, &key, &doc, expected).await? {
            Ok(_) => return Ok(()),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("thread counter contended".into()))
}

/// The thread metadata for one event, if any (`None` = not part of a thread).
pub async fn thread_meta_of(
    store: &mut StoreClusterClient,
    community: &str,
    event_hex: &str,
) -> Result<Option<ThreadMeta>> {
    let key = keys::thread_meta(community, event_hex);
    store::get_doc::<ThreadMeta>(store, &key).await
}

/// The thread summary for `root` (buzz's kind-39005 content shape):
/// `(reply_count, descendant_count, last_reply_at)`. `None` when the root has no
/// thread metadata (no replies yet).
pub async fn thread_summary(
    store: &mut StoreClusterClient,
    community: &str,
    root_hex: &str,
) -> Result<Option<(u32, u32, u64)>> {
    let key = keys::thread_meta(community, root_hex);
    Ok(store::get_doc::<ThreadMeta>(store, &key)
        .await?
        .map(|m| (m.reply_count, m.descendant_count, m.last_reply_at)))
}
