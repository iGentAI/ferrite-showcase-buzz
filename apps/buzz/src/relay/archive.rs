//! NIP-IA identity archiving: retiring a pubkey, and publishing the set.
//!
//! ## Why this module exists
//!
//! Every kind this needs was already declared — `9035` (archive request), `9036`
//! (unarchive request), `8002`/`8003` (the notices), `13535` (the list) — and
//! **none of them was referenced anywhere outside the registry**. So a request
//! was accepted, stored, audited and delivered while nothing archived anything,
//! and the client's `list_archived_identities` answered an empty set forever.
//! Accepted-but-inert, which is the shape this codebase removes wherever it
//! turns up.
//!
//! ## The shape, and why it mirrors the group projections
//!
//! Identity archiving has the same structure as NIP-29 group state, so it uses
//! the same machinery rather than a second one:
//!
//! | | NIP-29 | NIP-IA |
//! |---|---|---|
//! | a member sends | 9000/9001 | 9035/9036 |
//! | the relay materializes | `mem/` rows | `iarch/` rows |
//! | the relay publishes | 39002 roster | 13535 list |
//!
//! The list is minted with [`Event::sign_new`] and pushed through
//! [`crate::relay::ingest::admit`] + [`crate::relay::ingest::drive_relay_minted`],
//! so it is stored, indexed, audited, replaced and delivered by exactly the code
//! a client event runs. Nothing here writes storage directly.
//!
//! ## Three properties, each load-bearing
//!
//! 1. **Self-recognizing.** The list is derived from live rows and compared
//!    against the published version before anything is minted, so a re-driven
//!    saga costs one read instead of a second event.
//! 2. **The watermark is a FLOOR.** 13535 is replaceable, and
//!    [`crate::relay::effects::version_beats`] needs a strictly greater
//!    `created_at` — an equal one breaks to the lowest id, which is exactly where
//!    a stale list can win. So a second list within one clock second sits one
//!    second above the version it replaces.
//! 3. **It cannot loop.** 8002/8003/13535 trigger no archive effect, and they are
//!    driven through `drive_relay_minted`, which has no archive step. There is no
//!    path from an emission back to an emission.
//!
//! ## What an archive is
//!
//! An archive RETIRES an identity. The `iarch/` row is the fact, and the
//! 8002/8003 notices and the 13535 list announce it. The row is enforced where
//! it matters: [`crate::relay::ingress`]'s write gate refuses every write from an
//! archived pubkey (posts, reactions, uploads, persona turns, and the unarchive
//! request itself), and a schedule whose creator or persona is archived is
//! paused rather than spending on turns ingest would refuse. Reading and
//! delivery are unchanged, so an archived member's history stays readable.
//!
//! Only a community moderator lifts an archive ([`authorize_unarchive`]). An
//! identity that archived itself asks a moderator to bring it back, which is
//! what makes a moderator's archive a decision its subject cannot revert.

use ferrite_app::tracing;
use ferrite_app::client::{ApeClusterClient, StoreClusterClient, TopicLogClient};
use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json;

use crate::relay::crypto::to_hex;
use crate::relay::error::{BuzzError, Result};
use crate::relay::event::{Event, PubKey};
use crate::relay::{keys, kind, store};

/// The refusal an archived identity gets for any write (the error adds its own
/// `restricted: ` prefix).
pub const ARCHIVED_REFUSAL: &str =
    "this identity is archived; only a community moderator can unarchive it";

/// The longest reason an archive request may carry. Bounded because it is
/// republished inside the list every client reads.
pub const MAX_ARCHIVE_REASON: usize = 512;

/// The most identities one 13535 list carries.
///
/// The list is ONE event and an event has a hard serialized ceiling
/// ([`crate::relay::event::MAX_SERIALIZED_EVENT_BYTES`]), enforced at mint time —
/// past it the relay cannot mint the list AT ALL and the cell freezes on its last
/// version, so a bound must exist. One `["p","<64-hex>"]` tag costs at most 76
/// bytes on the wire, and the envelope is under 1 KiB, so this leaves ample slack
/// under the 240 KiB ceiling.
pub const MAX_ARCHIVED_LISTED: usize = 2048;

/// Tag marking the published list as incomplete, exactly as the roster
/// projection's does: `["truncated","<total>"]` says the `p` list carries the
/// first [`MAX_ARCHIVED_LISTED`] of that many. Absent on a complete list, so a
/// list below the cap stays byte-identical and the self-recognition compare does
/// not churn the cell.
pub const TRUNCATED_TAG: &str = "truncated";

/// How many times the list re-derives and re-publishes before giving up.
/// A lost tie is normal when two archive requests drive at once; the loser
/// re-derives from live state (which by then includes the winner's row) and
/// republishes above the new watermark.
const LIST_ATTEMPTS: usize = 4;

/// One archived identity, as stored.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ArchivedIdentity {
    /// Hex pubkey of the archived identity.
    pub pubkey: String,
    /// Epoch-ms the archive took effect.
    pub archived_at: u64,
    /// Hex pubkey of whoever archived it (self or a moderator).
    pub by: String,
    /// Optional operator-supplied reason, bounded by [`MAX_ARCHIVE_REASON`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Whether a kind is an archive REQUEST — the two kinds this module acts on.
///
/// Public because the saga's identity-resolution predicate
/// ([`crate::relay::effects::needs_relay_identity`]) has to agree with it: a kind
/// this returns true for needs the relay's signing key resolved before `drive`
/// runs, or the publish half silently no-ops.
///
/// Deliberately NOT the notice or list kinds. Those are what this module EMITS,
/// and including them is how an emitter starts triggering itself.
#[must_use]
pub const fn is_archive_request(k: crate::relay::kind::Kind) -> bool {
    matches!(
        k,
        kind::KIND_IA_ARCHIVE_REQUEST | kind::KIND_IA_UNARCHIVE_REQUEST
    )
}

/// Who an archive request targets: its first `p` tag, defaulting to the author.
///
/// Defaulting to the author is what makes a bare 9035 a SELF-archive, which is
/// the common case and the one that needs no elevated authority.
#[must_use]
pub fn archive_target(event: &Event) -> Option<PubKey> {
    // No `p` tag at all means the author; exactly one with a value names its
    // target; a valueless `["p"]` or more than one `p` tag names nobody, so
    // pre-storage validation refuses the event instead of archiving the author.
    let mut p_tags = event.tags.iter().filter(|t| t.first().is_some_and(|k| k == "p"));
    match (p_tags.next(), p_tags.next()) {
        (None, _) => Some(event.pubkey),
        (Some(tag), None) => tag.get(1).and_then(|hex| crate::relay::crypto::from_hex32(hex)),
        (Some(_), Some(_)) => None,
    }
}

/// The reason an archive request carries, trimmed and bounded.
#[must_use]
pub fn archive_reason(event: &Event) -> Option<String> {
    let reason = event.content.trim();
    if reason.is_empty() {
        return None;
    }
    Some(reason.chars().take(MAX_ARCHIVE_REASON).collect())
}

/// Whether an archive request is authorized.
///
/// Pure, so it is exhaustively testable and so the caller
/// ([`crate::relay::ingress`]) reads moderator status from the planes and passes
/// it in — the same shape as `authorize_membership_command`.
///
/// An identity may archive ITSELF, and a community moderator may archive anyone.
/// Deliberately not "any member": archiving somebody else's identity is a
/// moderation act, and letting any member do it would be a griefing primitive
/// dressed as a feature.
pub fn authorize_archive(actor: &PubKey, target: &PubKey, actor_is_moderator: bool) -> Result<()> {
    if actor == target || actor_is_moderator {
        return Ok(());
    }
    Err(BuzzError::Unauthorized(
        "only the identity itself or a community moderator may archive it".into(),
    ))
}

/// Whether an unarchive request is authorized.
///
/// Only a community moderator may lift an archive, whoever imposed it. The
/// archived identity cannot even submit the request (the write gate refuses
/// it), and this rule is what stops anybody else from lifting it on its behalf.
pub fn authorize_unarchive(actor_is_moderator: bool) -> Result<()> {
    if actor_is_moderator {
        return Ok(());
    }
    Err(BuzzError::Unauthorized(
        "only a community moderator may unarchive an identity".into(),
    ))
}

/// The archive row for `who`, if they are archived.
pub async fn archived_row(
    store: &mut StoreClusterClient,
    community: &str,
    who: &PubKey,
) -> Result<Option<ArchivedIdentity>> {
    let key = keys::archived_identity(community, who);
    store::get_doc::<ArchivedIdentity>(store, &key).await
}

/// Whether `who` is archived right now.
///
/// The question the write gate and the schedule standing check ask.
pub async fn is_archived(
    store: &mut StoreClusterClient,
    community: &str,
    who: &PubKey,
) -> Result<bool> {
    Ok(archived_row(store, community, who).await?.is_some())
}

/// Every archived identity, in pubkey order, with the true total.
///
/// Pages the scan: the prefix is bounded only by how many identities have been
/// archived, while one scan page is clamped, so a single page is NOT the set.
pub async fn archived_identities(
    store: &mut StoreClusterClient,
    community: &str,
) -> Result<(Vec<ArchivedIdentity>, usize)> {
    let prefix = keys::archived_identity_prefix(community);
    let mut out: Vec<ArchivedIdentity> = Vec::new();
    let mut total = 0usize;
    let mut start_after: Option<Vec<u8>> = None;
    loop {
        let (rows, more) =
            store::scan(store, &prefix, start_after.as_deref(), store::MAX_SCAN_PAGE).await?;
        if rows.is_empty() {
            break;
        }
        for (key, value, _version) in &rows {
            start_after = Some(key.clone());
            // One unreadable row must not cost the whole set.
            match store::decode_doc::<ArchivedIdentity>(key, value) {
                Ok(doc) => {
                    total += 1;
                    if out.len() < MAX_ARCHIVED_LISTED {
                        out.push(doc);
                    }
                }
                Err(e) => {
                    tracing::warn!("archived list skipped an unreadable row: {e}");
                }
            }
        }
        if !more {
            break;
        }
    }
    Ok((out, total))
}

/// Apply an archive/unarchive request: materialize the row, then publish the
/// notice and the refreshed list.
///
/// Authorization already happened pre-storage; this materializes the decision.
/// The publish half is best-effort for the same reason the projections are: the
/// state change is committed by the time it runs, so a failed publish must not
/// fail a saga — the next archive re-converges the cell, and the row is already
/// authoritative for [`is_archived`].
/// Raise the unarchive watermark to `admitted_at_ms`, never lowering it.
async fn raise_unarchive_watermark(
    store: &mut StoreClusterClient,
    key: &[u8],
    admitted_at_ms: u64,
) -> Result<()> {
    for _ in 0..8 {
        let current: Option<(u64, u64)> = store::get_doc_versioned(store, key).await?;
        if current.is_some_and(|(at, _)| at >= admitted_at_ms) {
            return Ok(());
        }
        let expected = current.map(|(_, version)| version);
        if store::put_doc_if_version(store, key, &admitted_at_ms, expected)
            .await?
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(crate::relay::error::BuzzError::Storage(
        "the unarchive watermark kept moving; the request is retried".into(),
    ))
}

#[allow(clippy::too_many_arguments)]
pub async fn apply_archive(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    event: &Event,
    seckey: Option<&[u8; 32]>,
    admitted_at_ms: u64,
    now_ms: u64,
) -> Result<()> {
    // The KIND decides first, and everything below is unreachable for anything
    // else. Ordering is load-bearing: `archive_target` falls back to the AUTHOR
    // when a request carries no `p` tag — correct for a bare self-archive, and
    // catastrophic one call earlier, where it would archive the sender of every
    // message the saga drives.
    if !is_archive_request(event.kind) {
        return Ok(());
    }
    let Some(target) = archive_target(event) else {
        return Ok(());
    };
    let key = keys::archived_identity(community, &target);
    let watermark_key = keys::unarchived_identity_watermark(community, &target);
    let archiving = event.kind == kind::KIND_IA_ARCHIVE_REQUEST;

    // Sagas run concurrently and are re-driven, so requests reach here out of
    // admission order. Each side therefore yields to a NEWER request of the
    // other kind: an archive admitted before the latest unarchive is stale,
    // and an unarchive admitted before the stored archive leaves it standing.
    if archiving {
        let unarchived_at: Option<u64> = store::get_doc(store, &watermark_key).await?;
        if unarchived_at.is_some_and(|at| at >= admitted_at_ms) {
            return Ok(());
        }
        let doc = ArchivedIdentity {
            pubkey: to_hex(&target),
            archived_at: admitted_at_ms,
            by: to_hex(&event.pubkey),
            reason: archive_reason(event),
        };
        // Create-if-absent: re-archiving an already-archived identity must not
        // restamp who did it or when, which would rewrite history on a re-drive.
        let _ = store::put_doc_if_version(store, &key, &doc, None).await?;
    } else {
        raise_unarchive_watermark(store, &watermark_key, admitted_at_ms).await?;
        let current: Option<ArchivedIdentity> = store::get_doc(store, &key).await?;
        match current {
            Some(doc) if doc.archived_at > admitted_at_ms => return Ok(()),
            Some(_) => {
                store::delete_key(store, &key).await?;
            }
            None => {}
        }
    }

    // The notice and the list are DERIVED views. A failure leaves the row — the
    // fact everything else reads — correct.
    let Some(seckey) = seckey else {
        return Ok(());
    };
    let notice_kind = if archiving {
        kind::KIND_IA_ARCHIVED
    } else {
        kind::KIND_IA_UNARCHIVED
    };
    // Stamped with the TRIGGER's admission instant, not live `now_ms`, so every
    // driver of this saga mints a byte-identical notice whose id is already
    // admitted — the same deterministic-id dedup the membership notices use.
    let notice_at = admitted_at_ms / 1000;
    if let Err(e) = publish_relay_event(
        store,
        ape,
        topics,
        community,
        generation,
        notice_kind,
        vec![
            vec!["p".to_string(), to_hex(&target)],
            vec!["e".to_string(), to_hex(&event.id)],
        ],
        seckey,
        notice_at,
    )
    .await
    {
        tracing::warn!(
            "kind:{notice_kind} archive notice was not published ({e}); the row still stands"
        );
    }
    if let Err(e) =
        publish_archived_list(store, ape, topics, community, generation, seckey, now_ms).await
    {
        tracing::warn!(
            "the archived-identity list was not republished ({e}); it refreshes on the next change"
        );
    }
    Ok(())
}

/// Converge the 13535 cell onto the current archived set.
///
/// Self-recognizing and watermark-floored, exactly like a group projection — see
/// this module's header for why both matter.
async fn publish_archived_list(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    seckey: &[u8; 32],
    now_ms: u64,
) -> Result<()> {
    let relay_pk = crate::relay::crypto::schnorr::xonly_pubkey(seckey)
        .ok_or_else(|| BuzzError::Internal("relay identity is not a valid secret key".into()))?;

    for _ in 0..LIST_ATTEMPTS {
        // Derived INSIDE the loop: a retry after a lost tie must publish the set
        // as it is NOW, not the one this driver read before the winner landed.
        let (archived, total) = archived_identities(store, community).await?;
        let mut tags: Vec<Vec<String>> = Vec::with_capacity(archived.len() + 1);
        for entry in &archived {
            tags.push(vec!["p".to_string(), entry.pubkey.clone()]);
        }
        if total > archived.len() {
            tags.push(vec![TRUNCATED_TAG.to_string(), total.to_string()]);
        }

        // 13535 is a plain replaceable kind, so its cell is keyed by the empty
        // d-tag — the same cell `replacement_d_tag` computes for it.
        let head_key = keys::dtag_head(community, kind::KIND_IA_ARCHIVED_LIST, &relay_pk, "");
        let head: Option<crate::relay::effects::ReplaceableHead> = ape
            .get(&head_key)
            .await
            .map_err(|e| BuzzError::Storage(format!("archived list head read: {e}")))?
            .and_then(|bytes| serde_json::from_slice(&bytes).ok());
        if let Some(head) = &head {
            if let Some(id) = crate::relay::crypto::from_hex32(&head.id) {
                if let Some(published) =
                    store::get_doc::<Event>(store, &keys::evt(community, &id)).await?
                {
                    if published.tags == tags {
                        return Ok(()); // already published; republishing would only churn
                    }
                }
            }
        }

        let created_at =
            (now_ms / 1000).max(head.as_ref().map_or(0, |h| h.created_at.saturating_add(1)));
        let event = Event::sign_new(seckey, kind::KIND_IA_ARCHIVED_LIST, tags, "", created_at)?;
        let outcome = crate::relay::ingest::admit(ape, community, &event, None, now_ms).await?;
        crate::relay::ingest::drive_relay_minted(
            store, ape, topics, community, generation, &event, &outcome, now_ms,
        )
        .await?;

        let landed = ape
            .get(&head_key)
            .await
            .map_err(|e| BuzzError::Storage(format!("archived list head read: {e}")))?
            .and_then(|bytes| {
                serde_json::from_slice::<crate::relay::effects::ReplaceableHead>(&bytes).ok()
            })
            .is_some_and(|h| h.id == to_hex(&event.id));
        if landed {
            return Ok(());
        }
    }
    Err(BuzzError::Conflict(
        "the archived-identity list cell was contended beyond the attempt budget".into(),
    ))
}

/// Sign one relay-authored event and drive it through the ordinary saga.
#[allow(clippy::too_many_arguments)]
async fn publish_relay_event(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    event_kind: crate::relay::kind::Kind,
    tags: Vec<Vec<String>>,
    seckey: &[u8; 32],
    created_at: u64,
) -> Result<()> {
    let event = Event::sign_new(seckey, event_kind, tags, "", created_at)?;
    let now_ms = created_at.saturating_mul(1000);
    let outcome = crate::relay::ingest::admit(ape, community, &event, None, now_ms).await?;
    crate::relay::ingest::drive_relay_minted(
        store, ape, topics, community, generation, &event, &outcome, now_ms,
    )
    .await
    .map(|_| ())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    fn pk(b: u8) -> PubKey {
        [b; 32]
    }

    fn request(
        kind_: crate::relay::kind::Kind,
        author: PubKey,
        tags: Vec<Vec<String>>,
        content: &str,
    ) -> Event {
        Event {
            id: [9u8; 32],
            pubkey: author,
            created_at: 1_700_000_000,
            kind: kind_,
            tags,
            content: content.to_string(),
            sig: [0u8; 64],
        }
    }

    #[test]
    fn only_the_request_kinds_are_acted_on() {
        // The guard that stops this module archiving the author of every event
        // the saga drives — `archive_target` falls back to the author, so the
        // kind check is the only thing between that fallback and every message.
        assert!(is_archive_request(kind::KIND_IA_ARCHIVE_REQUEST));
        assert!(is_archive_request(kind::KIND_IA_UNARCHIVE_REQUEST));
        for quiet in [
            kind::KIND_STREAM_MESSAGE,
            kind::KIND_REACTION,
            kind::KIND_PROFILE,
            // What this module EMITS must never trigger it: that is the
            // no-loop invariant, the same one the projections keep.
            kind::KIND_IA_ARCHIVED,
            kind::KIND_IA_UNARCHIVED,
            kind::KIND_IA_ARCHIVED_LIST,
        ] {
            assert!(
                !is_archive_request(quiet),
                "kind {quiet} would loop or misfire"
            );
        }
    }

    #[test]
    fn a_bare_request_archives_the_author_themselves() {
        // The common case, and the one that needs no elevated authority: a bare
        // 9035 with no `p` tag is a SELF-archive.
        let me = pk(1);
        let event = request(kind::KIND_IA_ARCHIVE_REQUEST, me, vec![], "");
        assert_eq!(archive_target(&event), Some(me));
        assert!(authorize_archive(&me, &me, false).is_ok());
    }

    #[test]
    fn archiving_someone_else_needs_moderator_authority() {
        let actor = pk(1);
        let victim = pk(2);
        assert!(
            authorize_archive(&actor, &victim, false).is_err(),
            "any member archiving anyone would be a griefing primitive"
        );
        assert!(authorize_archive(&actor, &victim, true).is_ok());
    }

    #[test]
    fn only_a_moderator_may_unarchive() {
        // Whoever imposed the archive, and whoever asks: a moderator or nobody.
        assert!(authorize_unarchive(true).is_ok());
        assert!(authorize_unarchive(false).is_err());
    }

    #[test]
    fn an_explicit_p_tag_names_the_target() {
        let actor = pk(1);
        let target = pk(7);
        let event = request(
            kind::KIND_IA_ARCHIVE_REQUEST,
            actor,
            vec![vec!["p".into(), to_hex(&target)]],
            "",
        );
        assert_eq!(archive_target(&event), Some(target));
        // A valueless p tag, or two p tags, name nobody too.
        for tags in [
            vec![vec!["p".into()]],
            vec![vec!["p".into()], vec!["p".into(), to_hex(&target)]],
            vec![vec!["p".into(), to_hex(&target)], vec!["p".into(), to_hex(&actor)]],
        ] {
            let event = request(kind::KIND_IA_ARCHIVE_REQUEST, actor, tags, "");
            assert_eq!(archive_target(&event), None);
        }
        // A malformed p tag names nobody rather than silently falling back to
        // the author — archiving the wrong identity is not a recoverable guess.
        let bad = request(
            kind::KIND_IA_ARCHIVE_REQUEST,
            actor,
            vec![vec!["p".into(), "nope".into()]],
            "",
        );
        assert_eq!(archive_target(&bad), None);
    }

    #[test]
    fn a_reason_is_trimmed_and_bounded() {
        let me = pk(1);
        assert_eq!(
            archive_reason(&request(
                kind::KIND_IA_ARCHIVE_REQUEST,
                me,
                vec![],
                "  moved on  "
            )),
            Some("moved on".to_string())
        );
        assert_eq!(
            archive_reason(&request(kind::KIND_IA_ARCHIVE_REQUEST, me, vec![], "   ")),
            None
        );
        let long = "x".repeat(MAX_ARCHIVE_REASON + 50);
        let bounded =
            archive_reason(&request(kind::KIND_IA_ARCHIVE_REQUEST, me, vec![], &long)).unwrap();
        assert_eq!(bounded.chars().count(), MAX_ARCHIVE_REASON);
    }

    #[test]
    fn a_list_at_the_cap_still_signs() {
        // The same arithmetic the roster projection pins: past the serialized
        // ceiling the relay cannot mint the list at all, and the cell freezes on
        // its last version.
        let mut tags: Vec<Vec<String>> = (0..MAX_ARCHIVED_LISTED)
            .map(|i| vec!["p".to_string(), format!("{i:064x}")])
            .collect();
        tags.push(vec![TRUNCATED_TAG.to_string(), usize::MAX.to_string()]);
        let event = Event::sign_new(&[1u8; 32], kind::KIND_IA_ARCHIVED_LIST, tags, "", u64::MAX)
            .expect("a cap-sized list must be mintable");
        assert!(event.serialized_len() <= crate::relay::event::MAX_SERIALIZED_EVENT_BYTES);
    }
}
