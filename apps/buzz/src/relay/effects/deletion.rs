//! Event deletion, by id and by address, and retiring a reaction.

use ferrite_app::client::ApeClusterClient;
use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde_json;
use crate::relay::crypto::to_hex;
use crate::relay::error::BuzzError;
use crate::relay::error::Result;
use crate::relay::event::Event;
use crate::relay::event::PubKey;
use crate::relay::kind::Kind;
use crate::relay::keys;
use crate::relay::kind;
use crate::relay::store;
use super::{CAS_RETRIES};
use super::reactions::{ReactionState, reaction_target_and_emoji};
use super::replacement::{IndexedChannel, ReplaceableHead, delete_event_rows_scoped, replacement_d_tag};
use super::projections::{author_kind_versions};

/// Apply a NIP-09 deletion request (kind 5).
///
/// **The rule, and every part of it is load-bearing.** A kind-5 event asks the
/// relay to stop serving events its author owns. It names them two ways, and
/// this relay honors both:
///
/// * an **`e` tag** names one event id. The stored target is erased — body and
///   every index row — and its admission document is tombstoned
///   ([`crate::relay::ingest::tombstone`]) so nothing can put it back: not a re-driven
///   torn saga (which would otherwise re-create the rows in `apply_storage`),
///   not the repair sweep, not a client re-publishing the same bytes.
/// * an **`a` tag** names an addressable cell, `kind:pubkey:d` (NIP-01's
///   coordinate; `d` is empty for a plain replaceable kind like the agent
///   profile 10100). NIP-09's semantics there are a *watermark*, not a row
///   delete: every version of that cell with `created_at <= the request's
///   created_at` is retired, including versions this relay has not been handed
///   yet. That is [`ReplaceableHead::deleted_through`], and it is why an `a`
///   deletion cannot be implemented as "delete what I currently hold".
///
/// **Authorization is per target and never inferred.** A target is only
/// deleted when its author IS the deleter: for `e`, that is read off the stored
/// body (an id whose body this relay does not hold is SKIPPED — it cannot be
/// authorized, and tombstoning it on a stranger's word would be a remote erase
/// primitive); for `a`, the coordinate carries the pubkey, so it is compared
/// directly and a foreign one is ignored.
///
/// The deletion request itself stays stored, audited, and queryable — that is
/// NIP-09's design, and it is what lets another relay or a rejoining client
/// learn the deletion happened.
///
/// The reaction tally is still soft-removed for a deleted kind-7 (buzz's
/// self-retract path), because a reaction's materialized count is a separate
/// document from the event rows.
///
/// **Known limitation, deliberately not fixed here.** Deleting a threaded REPLY
/// does not decrement its root's materialized thread counters
/// ([`ThreadMeta::reply_count`] / `descendant_count`), so a thread can advertise
/// one more reply than it can serve. The counter is an approximation maintained
/// forward-only by [`apply_thread`], and a decrement is not idempotent — this
/// step is re-run by every redelivery of the deletion request, so a naive
/// decrement would subtract once per re-drive and corrupt the very state it was
/// trying to correct. Making it exactly-once means deriving the signal from
/// "this driver is the one that removed the body", which is a bigger change to
/// the erase path than a cosmetic over-count justifies. The reaction tally is
/// handled because its removal is a CAS on `removed_at IS NULL` — idempotent by
/// construction.
///
/// `authorized_channel` is the channel ingress derived from the deletion's
/// FIRST `e` target and checked for writability and membership. Every other
/// `e` target is deleted only when it lives in that same channel: a deletion
/// naming targets in several rooms passed the gate for one of them only, so an
/// author expelled from a private room, or writing into an archived one, could
/// otherwise erase there by listing that room's message second.
pub(super) async fn apply_deletion(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    event: &Event,
    authorized_channel: Option<&str>,
    now_ms: u64,
) -> Result<()> {
    for target_hex in event.tag_values("e") {
        let Some(target_id) = crate::relay::crypto::from_hex32(target_hex) else {
            continue;
        };
        let Some(target) = crate::relay::query::event_by_id(store, community, &target_id).await?
        else {
            // Unknown here: no body means no provable author, so no deletion.
            continue;
        };
        if target.pubkey != event.pubkey {
            continue;
        }
        if target_channel(store, community, &target).await?.as_deref() != authorized_channel {
            continue;
        }
        // A retracted reaction also loses its materialized tally row.
        if target.kind == kind::KIND_REACTION {
            if let Some((rx_target, emoji)) = reaction_target_and_emoji(&target) {
                soft_remove_reaction(
                    store,
                    community,
                    &rx_target,
                    &target.pubkey,
                    &emoji,
                    &to_hex(&target.id),
                    now_ms,
                )
                .await?;
            }
        }
        // Order: tombstone FIRST, erase second. The reverse order has a window
        // where a concurrent re-drive re-creates the rows after the erase and
        // before the marker, and nothing would ever remove them again. The
        // tombstone also hands back the scope the rows were indexed under, which
        // for a reaction or a deletion is NOT re-derivable from its own tags.
        let indexed = crate::relay::ingest::tombstone(ape, community, &target_id, now_ms).await?;
        delete_event_rows_scoped(
            store,
            community,
            &to_hex(&target_id),
            IndexedChannel::Exact(indexed.as_deref()),
        )
        .await?;
    }

    for coordinate in event.tag_values("a") {
        let Some((kind_, author, d_tag)) = parse_address(coordinate) else {
            continue;
        };
        if author != event.pubkey {
            continue; // NIP-09: an author may only delete their own address
        }
        if !kind::is_replaceable(kind_) && !kind::is_parameterized_replaceable(kind_) {
            // A regular kind has no addressable cell; NIP-09 uses `e` for those.
            continue;
        }
        apply_address_deletion(
            store,
            ape,
            community,
            kind_,
            &author,
            &d_tag,
            event.created_at,
            now_ms,
        )
        .await?;
    }
    Ok(())
}

/// The channel a deletion target lives in, as ingress derives it: a reaction
/// or deletion inherits its target's channel (up to 8 hops); anything else
/// carries its own. A broken chain resolves to no channel.
async fn target_channel(
    store: &mut StoreClusterClient,
    community: &str,
    target: &Event,
) -> Result<Option<String>> {
    let mut current = target.clone();
    for _ in 0..8 {
        if !matches!(current.kind, kind::KIND_REACTION | kind::KIND_DELETION) {
            if kind::is_global_only_kind(current.kind) {
                return Ok(None);
            }
            if kind::is_group_projection(current.kind) {
                return Ok(current.channel_id().or_else(|| current.d_tag()).map(str::to_string));
            }
            return Ok(current.channel_id().map(str::to_string));
        }
        let next_hex = if current.kind == kind::KIND_REACTION {
            current
                .tag_values("e")
                .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                .last()
        } else {
            current.tag_values("e").next()
        };
        let Some(next_id) = next_hex.and_then(crate::relay::crypto::from_hex32) else {
            return Ok(None);
        };
        let Some(next) = crate::relay::query::event_by_id(store, community, &next_id).await? else {
            return Ok(None);
        };
        current = next;
    }
    Ok(None)
}

/// Parse a NIP-01 addressable coordinate, `kind:pubkey:d-tag`.
///
/// The `d` part may be empty (a plain replaceable kind's cell) and may itself
/// contain `:` — the split is therefore exactly two, from the left, and anything
/// that is not `u32:64-hex:…` is refused rather than guessed at.
#[must_use]
pub fn parse_address(coordinate: &str) -> Option<(Kind, PubKey, String)> {
    let mut parts = coordinate.splitn(3, ':');
    let kind_ = parts.next()?.parse::<Kind>().ok()?;
    let author = crate::relay::crypto::from_hex32(parts.next()?)?;
    let d_tag = parts.next().unwrap_or("").to_string();
    Some((kind_, author, d_tag))
}

/// Retire one addressable cell on behalf of an ALREADY AUTHORIZED caller — the
/// same watermark-then-sweep [`apply_address_deletion`] a NIP-09 `a` deletion
/// performs, for a route whose authority is not the author's own signature
/// (`POST /buzz/api/projects/delete`). Refuses a kind that has no addressable
/// cell rather than silently doing nothing.
#[allow(clippy::too_many_arguments)]
pub async fn retire_address(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    kind_: Kind,
    author: &PubKey,
    d_tag: &str,
    through_created_at: u64,
    now_ms: u64,
) -> Result<()> {
    if !kind::is_replaceable(kind_) && !kind::is_parameterized_replaceable(kind_) {
        return Err(BuzzError::InvalidEvent(format!(
            "kind {kind_} has no addressable cell to retire"
        )));
    }
    apply_address_deletion(
        store,
        ape,
        community,
        kind_,
        author,
        d_tag,
        through_created_at,
        now_ms,
    )
    .await
}

/// Retire every version of one addressable cell at or below `through_created_at`
/// (NIP-09 for an `a` tag).
///
/// Two writes, in this order, and the order is the crash contract:
///
/// 1. **Raise the watermark** on the cell head under a CAS. Once that lands, no
///    covered version can ever be installed as the winner again
///    ([`apply_replacement`] checks it before anything else), so a crash right
///    here leaves the cell deleted-but-not-yet-swept rather than resurrectable.
/// 2. **Sweep what is stored**: walk the author+kind index, and for every
///    version of this cell at or below the watermark, tombstone its admission
///    and erase its rows.
///
/// Absent head ⇒ the cell has no winner yet, and the watermark is still written:
/// the deletion has to be able to precede the events it covers.
#[allow(clippy::too_many_arguments)]
pub(super) async fn apply_address_deletion(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    kind_: Kind,
    author: &PubKey,
    d_tag: &str,
    through_created_at: u64,
    now_ms: u64,
) -> Result<()> {
    let head_key = keys::dtag_head(community, kind_, author, d_tag);
    // The watermark MUST land before the sweep is worth anything: a sweep without
    // it deletes what is stored today and lets a covered version arrive tomorrow.
    // So contention beyond the budget is an error, not a silent partial deletion —
    // the deletion request's own saga is then re-driven and this runs again.
    let mut watermark_raised = false;
    for _ in 0..CAS_RETRIES {
        let current = ape
            .get(&head_key)
            .await
            .map_err(|e| BuzzError::Storage(format!("deletion head read: {e}")))?;
        let previous: Option<ReplaceableHead> = match current.as_deref() {
            None => None,
            Some(bytes) => Some(
                serde_json::from_slice(bytes)
                    .map_err(|e| BuzzError::Internal(format!("deletion head decode: {e}")))?,
            ),
        };
        if previous
            .as_ref()
            .is_some_and(|p| p.deleted_through >= through_created_at)
        {
            watermark_raised = true; // already retired at least this far
            break;
        }
        let next = match &previous {
            Some(previous) => ReplaceableHead {
                deleted_through: through_created_at,
                ..previous.clone()
            },
            // No version has taken the cell yet. The head then exists ONLY to
            // carry the watermark, so it names no id and cannot beat anything:
            // `created_at` 0 with an empty id loses every `version_beats` call.
            None => ReplaceableHead {
                id: String::new(),
                created_at: 0,
                channel: None,
                deleted_through: through_created_at,
            },
        };
        let next_bytes = serde_json::to_vec(&next)
            .map_err(|e| BuzzError::Internal(format!("deletion head encode: {e}")))?;
        match ape
            .commit_n_if(
                &head_key,
                current.as_deref(),
                &[(head_key.as_slice(), next_bytes.as_slice())],
            )
            .await
            .map_err(|e| BuzzError::Storage(format!("deletion head commit: {e}")))?
        {
            Ok(()) => {
                watermark_raised = true;
                break;
            }
            Err(_) => continue, // a version raced us; re-read and re-apply
        }
    }
    if !watermark_raised {
        return Err(BuzzError::Conflict(
            "the deletion watermark for this address was contended beyond the retry budget".into(),
        ));
    }

    // Sweep the versions this relay actually holds.
    let prefix = keys::author_prefix(community, author, Some(kind_));
    for key in author_kind_versions(store, &prefix, "deletion sweep scan").await? {
        let Some((_, id)) = keys::parse_index_tail(&key) else {
            continue;
        };
        let Some(body) = store
            .get(&keys::evt(community, &id))
            .await
            .map_err(|e| BuzzError::Storage(format!("deletion sweep body read: {e}")))?
        else {
            continue; // an index row whose body is gone is already unreadable
        };
        let stored: Event = serde_json::from_slice(&body)
            .map_err(|e| BuzzError::Internal(format!("deletion sweep body decode: {e}")))?;
        if replacement_d_tag(&stored).as_deref() != Some(d_tag) {
            continue; // same author+kind, different cell
        }
        if stored.created_at > through_created_at {
            continue; // NIP-09 covers versions up to the request only
        }
        let indexed = crate::relay::ingest::tombstone(ape, community, &id, now_ms).await?;
        delete_event_rows_scoped(
            store,
            community,
            &to_hex(&id),
            IndexedChannel::Exact(indexed.as_deref()),
        )
        .await?;
    }
    Ok(())
}

/// Soft-remove a reaction row (buzz's `SET removed_at = NOW() … WHERE removed_at
/// IS NULL`), only while the row still belongs to `reaction_event_id`: a later
/// reaction may have revived the row, and a late retraction of the earlier one
/// must leave it live. Absent or already-removed is a no-op.
pub(super) async fn soft_remove_reaction(
    store: &mut StoreClusterClient,
    community: &str,
    target_hex: &str,
    actor: &PubKey,
    emoji: &str,
    reaction_event_id: &str,
    now_ms: u64,
) -> Result<()> {
    let key = keys::reaction(community, target_hex, &to_hex(actor), emoji);
    for _ in 0..CAS_RETRIES {
        let Some((mut doc, version)) =
            store::get_doc_versioned::<ReactionState>(store, &key).await?
        else {
            return Ok(());
        };
        if !doc.is_live() || doc.reaction_event_id != reaction_event_id {
            return Ok(());
        }
        doc.removed_at = Some(now_ms);
        match store::put_doc_if_version(store, &key, &doc, Some(version)).await? {
            Ok(_) => return Ok(()),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("reaction removal contended".into()))
}
