//! Reactions: one active reaction per member, emoji and target, and the tally.

use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde::Deserialize;
use ferrite_app::serde::Serialize;
use crate::relay::crypto::to_hex;
use crate::relay::error::BuzzError;
use crate::relay::error::Result;
use crate::relay::event::Event;
use crate::relay::event::PubKey;
use crate::relay::keys;
use crate::relay::store;
use super::{CAS_RETRIES};
use super::threads::{is_thread_target};

/// The longest reaction emoji, in Unicode scalar values (buzz's
/// `MAX_REACTION_EMOJI_CHARS`, counted with `chars()`, not bytes).
pub const MAX_REACTION_EMOJI_CHARS: usize = 64;

/// One materialized reaction, keyed by `(target, actor, emoji)`. Soft removal
/// (`removed_at`) matches buzz, so a re-add revives the same row.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ReactionState {
    /// Hex id of the kind-7 event that most recently set this reaction live.
    pub reaction_event_id: String,
    /// The emoji, verbatim (the key only holds a hash of it).
    pub emoji: String,
    pub created_at: u64,
    /// `Some` once removed; `None` = live (buzz's `removed_at`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub removed_at: Option<u64>,
}

impl ReactionState {
    #[must_use]
    pub const fn is_live(&self) -> bool {
        self.removed_at.is_none()
    }
}

/// The `(target_hex, emoji)` a reaction (kind 7) carries: its **last** `e` tag
/// with a 64-hex value, and its content (empty ⇒ `"+"`), rejecting an
/// over-length emoji. buzz's `ingest.rs` reaction parse.
///
/// Returns `None` when there is no valid target or the emoji is too long — the
/// caller refuses the event.
#[must_use]
pub fn reaction_target_and_emoji(event: &Event) -> Option<(String, String)> {
    let target = event
        .tag_values("e")
        .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
        .last()?
        .to_ascii_lowercase();
    let emoji = if event.content.is_empty() {
        "+".to_string()
    } else {
        event.content.clone()
    };
    if emoji.chars().count() > MAX_REACTION_EMOJI_CHARS {
        return None;
    }
    Some((target, emoji))
}

/// Whether a reaction would be an **active duplicate** — the state exists and is
/// live. buzz refuses to store the kind-7 event in that case (`ON CONFLICT DO
/// UPDATE ... WHERE removed_at IS NOT NULL` matches nothing), so this is checked
/// *before* admission. An absent or removed row returns `false` (the reaction is
/// new or a revival, both of which proceed).
pub async fn reaction_is_active_duplicate(
    store: &mut StoreClusterClient,
    community: &str,
    target_hex: &str,
    actor: &PubKey,
    emoji: &str,
) -> Result<bool> {
    let key = keys::reaction(community, target_hex, &to_hex(actor), emoji);
    Ok(store::get_doc::<ReactionState>(store, &key)
        .await?
        .is_some_and(|r| r.is_live()))
}

/// Materialize a reaction (kind 7): create the row, or revive a removed one, with
/// buzz's "reactivate only if removed" rule. An already-live row is left as-is
/// (its `reaction_event_id`/`created_at` unchanged), which is why the pre-storage
/// duplicate check exists — this is the concurrent-race backstop.
pub(super) async fn apply_reaction(
    store: &mut StoreClusterClient,
    community: &str,
    event: &Event,
    channel: Option<&str>,
    now_ms: u64,
) -> Result<()> {
    let Some((target, emoji)) = reaction_target_and_emoji(event) else {
        return Ok(());
    };
    if !is_thread_target(store, community, &target, channel).await? {
        return Ok(());
    }
    let key = keys::reaction(community, &target, &to_hex(&event.pubkey), &emoji);
    for _ in 0..CAS_RETRIES {
        let current = store::get_doc_versioned::<ReactionState>(store, &key).await?;
        let (doc, expected) = match current {
            Some((existing, version)) => {
                if existing.is_live() {
                    // Active duplicate: no change (buzz's WHERE removed_at IS NOT
                    // NULL matches nothing).
                    return Ok(());
                }
                if existing.reaction_event_id == crate::relay::crypto::to_hex(&event.id) {
                    // This very reaction was already applied and then retracted;
                    // re-applying it must not undo the retraction.
                    return Ok(());
                }
                // Revive: re-stamp created_at to now and adopt this event's id.
                (
                    ReactionState {
                        reaction_event_id: crate::relay::crypto::to_hex(&event.id),
                        emoji: emoji.clone(),
                        created_at: now_ms,
                        removed_at: None,
                    },
                    Some(version),
                )
            }
            None => (
                ReactionState {
                    reaction_event_id: crate::relay::crypto::to_hex(&event.id),
                    emoji: emoji.clone(),
                    created_at: now_ms,
                    removed_at: None,
                },
                None,
            ),
        };
        match store::put_doc_if_version(store, &key, &doc, expected).await? {
            Ok(_) => return Ok(()),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("reaction upsert contended".into()))
}

/// One emoji's tally on a target: the emoji, the count, and the reacting
/// pubkeys (hex), oldest reaction first.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ReactionTally {
    pub emoji: String,
    pub count: usize,
    pub pubkeys: Vec<String>,
}

/// The live reactions on `target`, grouped by emoji (buzz's `get_reactions`
/// shape). Removed rows are skipped. Groups are ordered by emoji; within a group,
/// reactors are ordered by when they reacted (`created_at`).
pub async fn read_reactions(
    store: &mut StoreClusterClient,
    community: &str,
    target_hex: &str,
) -> Result<Vec<ReactionTally>> {
    use std::collections::BTreeMap;
    let prefix = keys::reaction_prefix(community, target_hex);
    let mut groups: BTreeMap<String, Vec<(u64, String)>> = BTreeMap::new();
    let mut start_after: Option<Vec<u8>> = None;
    loop {
        let (rows, has_more) = store::scan(store, &prefix, start_after.as_deref(), 256).await?;
        if rows.is_empty() {
            break;
        }
        for (key, value, _version) in &rows {
            start_after = Some(key.clone());
            let Ok(state) = store::decode_doc::<ReactionState>(key, value) else {
                continue;
            };
            if !state.is_live() {
                continue;
            }
            // The reactor's hex pubkey is the `{actor}` component of the key
            // (`rxn/{c}/{target}/{actor}/{emoji_hash}`); the row value carries the
            // emoji and timing.
            groups
                .entry(state.emoji.clone())
                .or_default()
                .push((state.created_at, reactor_from_key(key)));
        }
        if !has_more {
            break;
        }
    }
    Ok(groups
        .into_iter()
        .map(|(emoji, mut reactors)| {
            reactors.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));
            ReactionTally {
                count: reactors.len(),
                pubkeys: reactors.into_iter().map(|(_, pk)| pk).collect(),
                emoji,
            }
        })
        .collect())
}

/// Recover the reactor's hex pubkey from a `rxn/{c}/{target}/{actor}/{emoji}`
/// key: it is the second-to-last `/`-separated component.
pub(super) fn reactor_from_key(key: &[u8]) -> String {
    let text = String::from_utf8_lossy(key);
    text.rsplit('/').nth(1).unwrap_or("").to_string()
}
