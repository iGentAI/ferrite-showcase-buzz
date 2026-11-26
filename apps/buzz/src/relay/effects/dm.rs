//! Direct message rooms: the channel id from the participants, open and hide.

use ferrite_app::client::ApeClusterClient;
use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde_json;
use crate::relay::channel::Channel;
use crate::relay::channel::Membership;
use crate::relay::channel::Role;
use crate::relay::channel::Visibility;
use crate::relay::error::BuzzError;
use crate::relay::error::Result;
use crate::relay::event::Event;
use crate::relay::event::PubKey;
use crate::relay::keys;
use crate::relay::store;
use super::{CAS_RETRIES};
use super::membership::{add_or_update_member};

/// The participant set a DM command names: the author plus every `p`-tag pubkey,
/// de-duplicated. `None` if a `p` tag is not a 32-byte hex pubkey.
#[must_use]
pub fn dm_participants(event: &Event) -> Option<Vec<PubKey>> {
    let mut set = vec![event.pubkey];
    for hex in event.tag_values("p") {
        set.push(crate::relay::crypto::from_hex32(hex)?);
    }
    Some(set)
}

/// The DM channel id derived from a participant set: the participant hash itself,
/// so opening a DM with the same people twice resolves to the same room without a
/// separate hash index (buzz uses a server UUID + a unique hash index; deriving
/// the id from the hash is the Ferrite-native equivalent).
#[must_use]
pub fn dm_channel_id(participants: &[PubKey]) -> String {
    crate::relay::channel::dm_participant_hash(participants)
}

fn is_valid_dm_channel(doc: &Channel, expected_hash: &str) -> bool {
    doc.channel_type == crate::relay::channel::ChannelType::Dm
        && doc.visibility == Visibility::Private
        && doc.owners.is_empty()
        && doc.participant_hash.as_deref() == Some(expected_hash)
        && doc.is_writable()
}

pub(crate) fn is_compatible_dm_guard(bytes: &[u8], expected_hash: &str) -> bool {
    serde_json::from_slice::<Channel>(bytes)
        .map(|doc| is_valid_dm_channel(&doc, expected_hash))
        .unwrap_or(false)
}

/// Open (or find) a DM (41010/41011): create the channel if absent with every
/// participant as a member, and clear the opener's own hide. Idempotent — the id
/// is the participant hash, so a re-open finds the same room. Out-of-range
/// participant counts are a no-op here (validated pre-storage).
pub(super) async fn apply_dm_open(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    event: &Event,
    now_ms: u64,
) -> Result<()> {
    let Some(participants) = dm_participants(event) else {
        return Ok(());
    };
    let count = crate::relay::channel::distinct_participant_count(&participants);
    if !(crate::relay::channel::DM_MIN_PARTICIPANTS..=crate::relay::channel::DM_MAX_PARTICIPANTS)
        .contains(&count)
    {
        return Ok(());
    }
    let hash = crate::relay::channel::dm_participant_hash(&participants);
    let channel_id = dm_channel_id(&participants);
    let guard_key = keys::guard_channel(community, &channel_id)?;

    // Create the DM channel document if it does not exist (create-if-absent CAS).
    // Never proceed to materialize membership rows unless the DM channel guard
    // exists on the APE plane.
    let mut doc = Channel::new(
        channel_id.clone(),
        format!("DM ({count})"),
        crate::relay::channel::ChannelType::Dm,
        Visibility::Private,
        &event.pubkey,
        now_ms,
    );
    // A DM has no owner (every participant is a plain member); clear the
    // creator-as-owner set that `Channel::new` seeds.
    doc.owners.clear();
    doc.add_policy = crate::relay::channel::AddPolicy::Nobody;
    doc.participant_hash = Some(hash.clone());
    let bytes = serde_json::to_vec(&doc)
        .map_err(|e| BuzzError::Internal(format!("dm channel encode: {e}")))?;

    let mut guard_ready = false;
    for _ in 0..CAS_RETRIES {
        let existing = ape
            .get(&guard_key)
            .await
            .map_err(|e| BuzzError::Storage(format!("dm channel read: {e}")))?;
        if let Some(existing_bytes) = existing {
            if !is_compatible_dm_guard(&existing_bytes, &hash) {
                ferrite_app::tracing::warn!(
                    "refusing to materialize DM into incompatible channel {channel_id}"
                );
                return Ok(());
            }
            guard_ready = true;
            break;
        }
        match ape
            .commit_n_if(
                &guard_key,
                None,
                &[(guard_key.as_slice(), bytes.as_slice())],
            )
            .await
            .map_err(|e| BuzzError::Storage(format!("dm channel create: {e}")))?
        {
            Ok(()) => {
                guard_ready = true;
                break;
            }
            Err(Some(existing_bytes)) => {
                if !is_compatible_dm_guard(&existing_bytes, &hash) {
                    ferrite_app::tracing::warn!(
                        "refusing to materialize DM into incompatible channel {channel_id}"
                    );
                    return Ok(());
                }
                guard_ready = true;
                break;
            }
            Err(None) => continue,
        }
    }
    if !guard_ready {
        return Err(BuzzError::Conflict("dm channel creation contended".into()));
    }

    // Every participant is a member (idempotent upsert); the opener is unhidden.
    for pk in &participants {
        add_or_update_member(
            store,
            community,
            &channel_id,
            pk,
            Role::Member,
            None,
            false,
            now_ms,
        )
        .await?;
    }
    set_member_hidden(store, community, &channel_id, &event.pubkey, None).await
}

/// Hide a DM for its author (41012): set `hidden_at` on the author's own
/// membership. A no-op if they are not a member.
pub(super) async fn apply_dm_hide(
    store: &mut StoreClusterClient,
    community: &str,
    event: &Event,
    now_ms: u64,
) -> Result<()> {
    let Some(channel) = event.channel_id() else {
        return Ok(());
    };
    let channel = channel.to_string();
    set_member_hidden(store, community, &channel, &event.pubkey, Some(now_ms)).await
}

/// Set or clear a member's `hidden_at` (DM per-viewer visibility). CAS.
pub(super) async fn set_member_hidden(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
    who: &PubKey,
    hidden_at: Option<u64>,
) -> Result<()> {
    let key = keys::mem(community, channel, who)?;
    for _ in 0..CAS_RETRIES {
        let Some((mut doc, version)) = store::get_doc_versioned::<Membership>(store, &key).await?
        else {
            return Ok(());
        };
        if doc.hidden_at == hidden_at {
            return Ok(());
        }
        doc.hidden_at = hidden_at;
        match store::put_doc_if_version(store, &key, &doc, Some(version)).await? {
            Ok(_) => return Ok(()),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("dm visibility contended".into()))
}
