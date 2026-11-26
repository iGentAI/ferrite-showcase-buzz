//! Replaceable events: which version wins, retiring the old one and purging its rows.

use ferrite_app::client::ApeClusterClient;
use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde::Deserialize;
use ferrite_app::serde::Serialize;
use ferrite_app::serde_json;
use crate::relay::crypto::to_hex;
use crate::relay::error::BuzzError;
use crate::relay::error::Result;
use crate::relay::event::Event;
use crate::relay::keys;
use crate::relay::kind;
use crate::relay::store;
use super::{CAS_RETRIES};
use super::projections::{author_kind_versions};

/// The replaceable-event head (the "watermark"): which version currently wins
/// one `(kind, author[, d])` cell, and the channel its rows were indexed
/// under (so a superseded winner's index keys can be recomputed exactly).
/// Lives on the APE plane at [`keys::dtag_head`] — the guard that replaces
/// buzz's advisory lock around replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ReplaceableHead {
    /// Hex id of the winning version.
    pub id: String,
    pub created_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// NIP-09 address deletion watermark: no version of this cell whose
    /// `created_at` is at or below this may be served, whenever it arrives.
    ///
    /// A deletion has to survive on the *cell* and not only on the versions the
    /// relay happens to hold, because NIP-09 deletes an ADDRESS ("every version
    /// of `kind:pubkey:d` up to my `created_at`"), and the relay can be handed a
    /// covered version afterwards — by a client syncing history, or by its own
    /// re-drive of a saga that tore before it ever reached the head. The
    /// watermark is what makes both of those erase instead of resurrect.
    #[serde(default)]
    pub deleted_through: u64,
}

/// The replacement cell an event belongs to, if its kind replaces at all:
/// `Some("")` for a NIP-01 replaceable kind (keyed `(pubkey, kind)`), the
/// first `d` tag value (absent ⇒ `""`, the NIP-33 convention) for a
/// parameterized one, `None` for regular/ephemeral kinds.
#[must_use]
pub fn replacement_d_tag(event: &Event) -> Option<String> {
    if kind::is_replaceable(event.kind) {
        return Some(String::new());
    }
    if kind::is_parameterized_replaceable(event.kind) {
        return Some(
            event
                .tag_values("d")
                .next()
                .map(ToOwned::to_owned)
                .unwrap_or_default(),
        );
    }
    None
}

/// NIP-16's total order over versions of one cell: newest `created_at` wins,
/// and equal timestamps break to the LOWEST id (NIP-01's rule), so every
/// relay — and every racing instance of this one — converges on the same
/// winner without coordination.
#[must_use]
pub fn version_beats(a_created_at: u64, a_id: &str, b_created_at: u64, b_id: &str) -> bool {
    a_created_at > b_created_at || (a_created_at == b_created_at && a_id < b_id)
}

/// NIP-16/33 replacement (buzz's M1.6): converge one
/// `(kind, author[, d])` cell onto its winning version and remove every
/// superseded version's stored rows.
///
/// The head CAS is the advisory lock's replacement: every driver reads the
/// head, decides by [`version_beats`], and either installs its event as the
/// new head or recognizes its event as already superseded (then deletes the
/// event's OWN rows). Losing the CAS re-reads and re-decides, so two
/// concurrent versions converge in either arrival order.
///
/// A winner then SWEEPS its whole cell rather than trusting the head alone:
/// a version whose saga tore between storage and replacement was stored but
/// never head-registered, and the head cannot name it. The sweep walks the
/// author+kind index, keeps the winner and any version that would BEAT it
/// (that version's own drive installs it), and deletes everything the winner
/// supersedes — which also makes the cell self-healing on every later write.
///
/// Deliberately NOT gated on `was_new`: a re-driven saga re-creates its
/// event's rows in `apply_storage` (create-if-absent on keys a previous
/// replacement may have deleted), and this step is what re-converges them.
/// Every operation here is idempotent — the CAS no-ops once the head matches,
/// and deleting absent rows is a no-op.
///
/// The superseded event stays in the audit chain (append-only history) and
/// already rode the live feed; what replacement removes is its *queryable*
/// presence: the body and every index row.
pub async fn apply_replacement(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    event: &Event,
    channel: Option<&str>,
) -> Result<()> {
    let Some(d_tag) = replacement_d_tag(event) else {
        return Ok(());
    };
    let event_hex = to_hex(&event.id);
    let head_key = keys::dtag_head(community, event.kind, &event.pubkey, &d_tag);

    for _ in 0..CAS_RETRIES {
        let current = ape
            .get(&head_key)
            .await
            .map_err(|e| BuzzError::Storage(format!("replacement head read: {e}")))?;
        let previous: Option<ReplaceableHead> = match current.as_deref() {
            None => None,
            Some(bytes) => Some(
                serde_json::from_slice(bytes)
                    .map_err(|e| BuzzError::Internal(format!("replacement head decode: {e}")))?,
            ),
        };

        if let Some(previous) = &previous {
            // NIP-09 first: a version covered by an address deletion is not a
            // candidate for the cell at all, however it arrived and however new
            // the head is. Its own rows go and the head is left exactly as it
            // is — installing a deleted version as the winner would serve it.
            if event.created_at <= previous.deleted_through {
                return delete_event_rows(store, community, &event_hex).await;
            }
            if previous.id == event_hex {
                // Already the installed winner (a re-drive): re-sweep the cell
                // so a torn loser saga re-driven AFTER us still reconverges.
                return sweep_superseded(store, community, event, &d_tag).await;
            }
            if !version_beats(
                event.created_at,
                &event_hex,
                previous.created_at,
                &previous.id,
            ) {
                // The incoming version is the loser: its own rows go, the head
                // stands. (The relay ACCEPTED the event — NIP-01 lets a relay
                // absorb an older version rather than error a client that is
                // merely syncing history.)
                return delete_event_rows(store, community, &event_hex).await;
            }
        }

        let next = ReplaceableHead {
            id: event_hex.clone(),
            created_at: event.created_at,
            channel: channel.map(ToOwned::to_owned),
            // Carried forward, never reset: a newer version wins the cell, it
            // does NOT un-delete the versions a NIP-09 request retired.
            deleted_through: previous.as_ref().map_or(0, |p| p.deleted_through),
        };
        let next_bytes = serde_json::to_vec(&next)
            .map_err(|e| BuzzError::Internal(format!("replacement head encode: {e}")))?;
        match ape
            .commit_n_if(
                &head_key,
                current.as_deref(),
                &[(head_key.as_slice(), next_bytes.as_slice())],
            )
            .await
            .map_err(|e| BuzzError::Storage(format!("replacement head commit: {e}")))?
        {
            Ok(()) => return sweep_superseded(store, community, event, &d_tag).await,
            Err(_) => continue, // another version raced the head; re-decide
        }
    }
    Err(BuzzError::Conflict("replacement head contended".into()))
}

/// Delete every stored version of `winner`'s cell that `winner` supersedes.
/// A stored version that would BEAT the winner is left alone — its own saga
/// owns installing it at the head when it drives.
pub(super) async fn sweep_superseded(
    store: &mut StoreClusterClient,
    community: &str,
    winner: &Event,
    d_tag: &str,
) -> Result<()> {
    let winner_hex = to_hex(&winner.id);
    let prefix = keys::author_prefix(community, &winner.pubkey, Some(winner.kind));
    for key in author_kind_versions(store, &prefix, "replacement sweep scan").await? {
        let Some((_, id)) = keys::parse_index_tail(&key) else {
            continue;
        };
        let id_hex = to_hex(&id);
        if id_hex == winner_hex {
            continue;
        }
        let body = store
            .get(&keys::evt(community, &id))
            .await
            .map_err(|e| BuzzError::Storage(format!("sweep body read: {e}")))?;
        let Some(body) = body else {
            // An orphaned index row (its deletion tore between index and
            // body passes, in the other order): readers already skip it;
            // finish the job.
            store::delete_key(store, &key).await?;
            continue;
        };
        let other: Event = serde_json::from_slice(&body)
            .map_err(|e| BuzzError::Internal(format!("sweep body decode: {e}")))?;
        // Same author+kind by the index prefix; the cell also needs the d.
        if replacement_d_tag(&other).as_deref() != Some(d_tag) {
            continue;
        }
        if version_beats(winner.created_at, &winner_hex, other.created_at, &id_hex) {
            delete_event_rows(store, community, &id_hex).await?;
        }
    }
    Ok(())
}

/// Which channel an event's index rows were actually written under — the one
/// fact a row deletion cannot get wrong, because the timeline key contains it.
#[derive(Debug, Clone, Copy)]
pub(super) enum IndexedChannel<'a> {
    /// Read off the saga's admission document, which records the value ingest
    /// indexed with (`None` = the global bucket). Exact for every kind.
    Exact(Option<&'a str>),
    /// Re-derive by the ingest rule (global-only kinds are channel-less,
    /// everything else scopes by its own `h` tag). Exact for every kind whose
    /// scope comes from its own tags — which is every replaceable kind — and an
    /// approximation only for the target-INHERITING kinds (a reaction, a
    /// deletion), whose scope came from an event they point at.
    Derived,
}

/// Erase one event's queryable rows, as a NIP-09 deletion or a replacement
/// does. Public because the ingest saga's tombstone path finishes an
/// interrupted erase ([`crate::relay::ingest::drive`]).
///
/// `indexed_channel` is the scope the admission document recorded (`None` = the
/// global bucket).
pub async fn purge_event_rows(
    store: &mut StoreClusterClient,
    community: &str,
    id_hex: &str,
    indexed_channel: Option<&str>,
) -> Result<()> {
    delete_event_rows_scoped(
        store,
        community,
        id_hex,
        IndexedChannel::Exact(indexed_channel),
    )
    .await
}

/// Remove one event's queryable rows under the ingest rule's scope.
pub(super) async fn delete_event_rows(
    store: &mut StoreClusterClient,
    community: &str,
    id_hex: &str,
) -> Result<()> {
    delete_event_rows_scoped(store, community, id_hex, IndexedChannel::Derived).await
}

/// Remove one event's queryable rows: every index key first (recomputed from
/// the still-present body), the body last. That order is what makes a crash
/// harmless in BOTH directions: readers skip an index entry whose body is
/// missing, and a re-driven deletion can always recompute the remaining index
/// keys while the body is still there. Absent body ⇒ already deleted ⇒ no-op.
pub(super) async fn delete_event_rows_scoped(
    store: &mut StoreClusterClient,
    community: &str,
    id_hex: &str,
    indexed_channel: IndexedChannel<'_>,
) -> Result<()> {
    let Some(id) = crate::relay::crypto::from_hex32(id_hex) else {
        return Err(BuzzError::Internal(
            "replacement head id is not 32-byte hex".into(),
        ));
    };
    let evt_key = keys::evt(community, &id);
    let body = store
        .get(&evt_key)
        .await
        .map_err(|e| BuzzError::Storage(format!("superseded body read: {e}")))?;
    let Some(body) = body else {
        return Ok(());
    };
    let event: Event = serde_json::from_slice(&body)
        .map_err(|e| BuzzError::Internal(format!("superseded body decode: {e}")))?;
    let channel = match indexed_channel {
        IndexedChannel::Exact(channel) => channel.map(ToOwned::to_owned),
        IndexedChannel::Derived => {
            if kind::is_global_only_kind(event.kind) {
                None
            } else {
                event.channel_id().map(ToOwned::to_owned)
            }
        }
    };
    for key in crate::relay::ingest::index_keys_for(community, &event, channel.as_deref()) {
        store::delete_key(store, &key).await?;
    }
    store::delete_key(store, &evt_key).await?;
    Ok(())
}
