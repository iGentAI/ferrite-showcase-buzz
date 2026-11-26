//! Membership changes under the per-channel lock, including the last-owner guard.

use ferrite_app::tracing;
use ferrite_app::client::ApeClusterClient;
use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde::Deserialize;
use ferrite_app::serde::Serialize;
use ferrite_app::serde_json;
use crate::relay::channel::Channel;
use crate::relay::channel::Membership;
use crate::relay::channel::Role;
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
use super::moderation::{bump_revocation};

/// The pubkey a membership command targets: its first `p` tag as 32 bytes.
#[must_use]
pub fn target_pubkey(event: &Event) -> Option<PubKey> {
    event
        .tag_values("p")
        .next()
        .and_then(crate::relay::crypto::from_hex32)
}

/// The role a `PUT_USER` grants (`role` tag), defaulting to [`Role::Member`].
/// An unrecognized value is `None` so a caller refuses rather than silently
/// downgrading — buzz parses the same tag and rejects an unknown role.
#[must_use]
pub fn requested_role(event: &Event) -> Option<Role> {
    match event.first_tag("role") {
        None => Some(Role::Member),
        Some(value) => Role::parse(value),
    }
}

/// How long a membership effect may hold its channel's lease. A holder that
/// dies leaves the lease to lapse, and its redelivery waits for that.
pub(super) const MEMBERSHIP_LOCK_TTL_MS: u64 = 30_000;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub(super) struct MembershipLock {
    pub(super) holder: String,
    pub(super) expires_ms: u64,
}

/// Take the channel's membership lease for `event`, so the actor's role read and
/// the target and owner-set writes of one command happen as one ordered
/// operation with respect to every other membership command on the channel.
///
/// Refused with `Conflict` while any attempt, including another delivery of the
/// same event, holds an unexpired lease; the saga is redelivered and tries
/// again. The lease is time-bounded, so [`ensure_membership_lock`] re-checks it
/// before every write.
pub(super) async fn acquire_membership_lock(
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    event: &Event,
    now_ms: u64,
) -> Result<Vec<u8>> {
    let key = keys::membership_lock(community, channel)?;
    let holder = to_hex(&event.id);
    for _ in 0..CAS_RETRIES {
        let current = ape
            .get(&key)
            .await
            .map_err(|e| BuzzError::Storage(format!("membership lock read: {e}")))?;
        if let Some(bytes) = &current {
            let held: MembershipLock = serde_json::from_slice(bytes)
                .map_err(|e| BuzzError::Internal(format!("membership lock decode: {e}")))?;
            if held.expires_ms > now_ms {
                return Err(BuzzError::Conflict(
                    "another membership change is in progress for this channel".into(),
                ));
            }
        }
        let next = serde_json::to_vec(&MembershipLock {
            holder: holder.clone(),
            expires_ms: now_ms.saturating_add(MEMBERSHIP_LOCK_TTL_MS),
        })
        .map_err(|e| BuzzError::Internal(format!("membership lock encode: {e}")))?;
        match ape
            .commit_n_if(&key, current.as_deref(), &[(key.as_slice(), next.as_slice())])
            .await
            .map_err(|e| BuzzError::Storage(format!("membership lock commit: {e}")))?
        {
            Ok(()) => return Ok(next),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("membership lock contended".into()))
}

/// Release the lease taken by [`acquire_membership_lock`]. A failed release is
/// harmless: the lease lapses on its own.
pub(super) async fn release_membership_lock(
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    held: &[u8],
) {
    let Ok(key) = keys::membership_lock(community, channel) else {
        return;
    };
    let Ok(freed) = serde_json::to_vec(&MembershipLock {
        holder: String::new(),
        expires_ms: 0,
    }) else {
        return;
    };
    let _ = ape
        .commit_n_if(&key, Some(held), &[(key.as_slice(), freed.as_slice())])
        .await;
}

/// Refuse to write on a lease that is no longer ours. A successor can take the
/// lease only after it lapsed, which rewrites the lock bytes, so a stalled
/// holder finds them changed here and stops before it writes. A stall between
/// this check and the write that follows it is not covered.
pub(super) async fn ensure_membership_lock(
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    held: &[u8],
) -> Result<()> {
    let key = keys::membership_lock(community, channel)?;
    let current = ape
        .get(&key)
        .await
        .map_err(|e| BuzzError::Storage(format!("membership lock read: {e}")))?;
    if current.as_deref() == Some(held) {
        Ok(())
    } else {
        Err(BuzzError::Conflict(
            "the membership lease for this channel was lost".into(),
        ))
    }
}

/// Whether `target_hex` is an owner and no OTHER owner in the set still holds a
/// live Owner membership row. The owner set and the membership row are written
/// one after the other, so a removal or grant that failed between the two leaves a
/// set entry without a live Owner row; counting the set alone would let the next
/// removal or demotion take the last real owner.
pub(super) async fn is_last_live_owner(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    target_hex: &str,
) -> Result<bool> {
    let key = keys::guard_channel(community, channel)?;
    let Some(bytes) = ape
        .get(&key)
        .await
        .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
    else {
        return Ok(false);
    };
    let doc: Channel = serde_json::from_slice(&bytes)
        .map_err(|e| BuzzError::Internal(format!("channel decode: {e}")))?;
    if !doc.owners.contains(target_hex) {
        return Ok(false);
    }
    for other in doc.owners.iter().filter(|owner| owner.as_str() != target_hex) {
        let Some(pubkey) = crate::relay::crypto::from_hex32(other) else {
            continue;
        };
        let row = store::get_doc::<Membership>(store, &keys::mem(community, channel, &pubkey)?)
            .await?;
        if row.is_some_and(|m| m.is_live() && m.role == Role::Owner) {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Materialize a membership change.
///
/// The `mem/` row (full per-member state, read by the access path) lives on the
/// Store plane; the owner set lives on the channel's APE document. The two writes
/// are ordered so a crash between them fails **closed**:
///
/// * granting/keeping ownership: update the owner set first (a would-be owner not
///   yet in `mem/` is denied access — safe), then the `mem/` row;
/// * removal/demotion: write the `mem/` removal first (the member loses access at
///   once — safe), then drop them from the owner set.
///
/// Re-driving repeats both writes idempotently.
pub(super) async fn apply_membership(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    event: &Event,
    now_ms: u64,
    held: &[u8],
) -> Result<()> {
    let guard_key = keys::guard_channel(community, channel)?;
    let Some(chan_bytes) = ape
        .get(&guard_key)
        .await
        .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
    else {
        return Ok(());
    };
    let chan_doc: Channel = serde_json::from_slice(&chan_bytes)
        .map_err(|e| BuzzError::Internal(format!("channel decode: {e}")))?;
    if !chan_doc.is_writable()
        || chan_doc.channel_type == crate::relay::channel::ChannelType::Dm
    {
        tracing::warn!("ignoring membership effect on unwritable or DM channel {channel}");
        return Ok(());
    }

    let actor_role = store::get_doc::<Membership>(store, &keys::mem(community, channel, &event.pubkey)?)
        .await?
        .filter(Membership::is_live)
        .map(|m| m.role);
    let actor_is_owner = chan_doc.owners.contains(&to_hex(&event.pubkey));
    let target = target_pubkey(event);
    let target_role = match target.as_ref() {
        Some(t) => store::get_doc::<Membership>(store, &keys::mem(community, channel, t)?)
            .await?
            .filter(Membership::is_live)
            .map(|m| m.role),
        None => None,
    };
    let target_is_owner = target
        .as_ref()
        .is_some_and(|t| chan_doc.owners.contains(&to_hex(t)));
    let requested = requested_role(event);

    if let Err(e) = authorize_target_role_change(
        event.kind,
        actor_role.is_some_and(Role::is_moderator),
        target_role,
    ) {
        tracing::warn!(
            "ignoring membership effect after concurrent target role change in {channel}: {e}"
        );
        return Ok(());
    }
    if let Err(e) = super::channels::authorize_membership_command(
        event.kind,
        &chan_doc,
        &event.pubkey,
        actor_role,
        actor_is_owner,
        target.as_ref(),
        target_is_owner,
        requested,
        chan_doc.owners.len(),
    ) {
        tracing::warn!(
            "ignoring membership effect that lost authorization race in {channel}: {e}"
        );
        return Ok(());
    }

    match event.kind {
        kind::KIND_NIP29_JOIN_REQUEST => {
            ensure_membership_lock(ape, community, channel, held).await?;
            join_member(store, community, channel, &event.pubkey, now_ms).await
        }
        kind::KIND_NIP29_PUT_USER => {
            let Some(target) = target else {
                return Ok(());
            };
            let Some(role) = requested else {
                return Ok(());
            };
            let actor_is_moderator = actor_role.is_some_and(Role::is_moderator);
            if !actor_is_moderator {
                // A non-moderator may only add or re-add a plain member and never
                // touches the owner set or an existing moderator's row.
                if role.is_moderator() {
                    tracing::warn!("ignoring elevated PUT_USER from a non-moderator");
                    return Ok(());
                }
                ensure_membership_lock(ape, community, channel, held).await?;
                return add_or_update_member(
                    store,
                    community,
                    channel,
                    &target,
                    role,
                    Some(&event.pubkey),
                    true,
                    now_ms,
                )
                .await;
            }
            let effective_role = if role == Role::Owner {
                ensure_membership_lock(ape, community, channel, held).await?;
                // Grant ownership: owner set first (fail-closed), then the row.
                add_owner(ape, community, channel, &to_hex(&target)).await?;
                Role::Owner
            } else {
                // Demote: drop from the owner set under the last-owner guard, so
                // concurrent distinct demotions can never empty it (the TOCTOU
                // adversarial review found). If the guard refuses (this is the
                // last owner), the demotion does NOT take effect — the target
                // stays an owner in BOTH the owner set and the `mem/` row, rather
                // than leaving a "member" whom the owner set still protects.
                ensure_membership_lock(ape, community, channel, held).await?;
                if is_last_live_owner(store, ape, community, channel, &to_hex(&target)).await? {
                    tracing::warn!("refusing to demote the last live owner of channel {channel}");
                    return Ok(());
                }
                let now_non_owner =
                    remove_owner_guarded(ape, community, channel, &to_hex(&target), true).await?;
                if now_non_owner {
                    role
                } else {
                    Role::Owner
                }
            };
            ensure_membership_lock(ape, community, channel, held).await?;
            add_or_update_member(
                store,
                community,
                channel,
                &target,
                effective_role,
                Some(&event.pubkey),
                false,
                now_ms,
            )
            .await
        }
        kind::KIND_NIP29_REMOVE_USER | kind::KIND_NIP29_LEAVE_REQUEST => {
            // 9001 targets its `p` tag; 9022 (leave) is always self.
            let target = if event.kind == kind::KIND_NIP29_LEAVE_REQUEST {
                event.pubkey
            } else {
                match target_pubkey(event) {
                    Some(t) => t,
                    None => return Ok(()),
                }
            };
            if is_last_live_owner(store, ape, community, channel, &to_hex(&target)).await? {
                tracing::warn!("refusing to remove the last owner of channel {channel}");
                return Ok(());
            }
            ensure_membership_lock(ape, community, channel, held).await?;
            soft_remove_member(store, community, channel, &target, now_ms).await?;
            ensure_membership_lock(ape, community, channel, held).await?;
            remove_owner_guarded(ape, community, channel, &to_hex(&target), true).await?;
            Ok(())
        }
        _ => Ok(()),
    }
}

/// The membership row a join request writes: absent ⇒ a plain member; live ⇒
/// unchanged, so a join never lowers a held role; soft-removed ⇒ re-admitted as
/// a plain member. A removal also drops the member from the channel's owner set,
/// so a revived row that kept its old Owner/Admin role would be an elevated role
/// nobody granted (and an Owner row absent from the owner set); `PUT_USER` is the
/// only verb that restores one.
pub(super) fn join_membership_doc(current: Option<Membership>, target: &PubKey, now_ms: u64) -> Membership {
    match current {
        Some(mut doc) => {
            if !doc.is_live() {
                doc.role = Role::Member;
            }
            doc.removed_at = None;
            doc
        }
        None => Membership::new(target, Role::Member, now_ms),
    }
}

/// Refuse a membership change that would alter an existing moderator's role when
/// the actor is not a moderator. [`authorize_membership_command`] gates only the
/// role being granted, so without this an ordinary member could `PUT_USER` an
/// admin down to `member` on a channel whose add policy is `Anyone`.
pub fn authorize_target_role_change(
    kind: Kind,
    actor_is_moderator: bool,
    target_role: Option<Role>,
) -> Result<()> {
    if kind == kind::KIND_NIP29_PUT_USER
        && !actor_is_moderator
        && target_role.is_some_and(Role::is_moderator)
    {
        return Err(BuzzError::Unauthorized(
            "only owners/admins may change an admin's or owner's role".into(),
        ));
    }
    Ok(())
}

pub(super) async fn join_member(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
    target: &PubKey,
    now_ms: u64,
) -> Result<()> {
    let key = keys::mem(community, channel, target)?;
    for _ in 0..CAS_RETRIES {
        let current = store::get_doc_versioned::<Membership>(store, &key).await?;
        let (existing, expected) = match current {
            Some((doc, version)) => (Some(doc), Some(version)),
            None => (None, None),
        };
        let doc = join_membership_doc(existing, target, now_ms);
        match store::put_doc_if_version(store, &key, &doc, expected).await? {
            Ok(_) => return Ok(()),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("membership upsert contended".into()))
}

/// The row a `PUT_USER` writes, or `None` when `protect_moderators` is set and
/// the row already belongs to a live moderator. Evaluated against the versioned
/// row inside the CAS loop, so a concurrent promotion is seen on retry.
pub(super) fn put_user_doc(
    current: Option<Membership>,
    target: &PubKey,
    role: Role,
    invited_by: Option<&PubKey>,
    protect_moderators: bool,
    now_ms: u64,
) -> Option<Membership> {
    let mut doc = match current {
        Some(doc) => {
            if protect_moderators && doc.is_live() && doc.role.is_moderator() {
                return None;
            }
            doc
        }
        None => Membership::new(target, role, now_ms),
    };
    doc.role = role;
    doc.removed_at = None;
    if doc.invited_by.is_none() {
        doc.invited_by = invited_by.map(|pk| to_hex(pk));
    }
    Some(doc)
}

/// Upsert a `mem/` row: create it, or revive a removed one and set its role,
/// preserving `joined_at`/`hidden_at` (buzz's `ON CONFLICT DO UPDATE removed_at =
/// NULL, role = EXCLUDED.role`). CAS-guarded against a concurrent write to the
/// same row; with `protect_moderators` a live Admin/Owner row is left untouched.
#[allow(clippy::too_many_arguments)]
pub(super) async fn add_or_update_member(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
    target: &PubKey,
    role: Role,
    invited_by: Option<&PubKey>,
    protect_moderators: bool,
    now_ms: u64,
) -> Result<()> {
    let key = keys::mem(community, channel, target)?;
    for _ in 0..CAS_RETRIES {
        let current = store::get_doc_versioned::<Membership>(store, &key).await?;
        let (existing, expected) = match current {
            Some((doc, version)) => (Some(doc), Some(version)),
            None => (None, None),
        };
        let Some(doc) = put_user_doc(
            existing,
            target,
            role,
            invited_by,
            protect_moderators,
            now_ms,
        ) else {
            tracing::warn!("refusing to change a moderator's membership for a non-moderator");
            return Ok(());
        };
        match store::put_doc_if_version(store, &key, &doc, expected).await? {
            Ok(_) => return Ok(()),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("membership upsert contended".into()))
}

/// Soft-remove a `mem/` row (buzz never hard-deletes). Absent or already-removed
/// is a no-op — the target ends not-live either way.
pub(super) async fn soft_remove_member(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
    target: &PubKey,
    now_ms: u64,
) -> Result<()> {
    let key = keys::mem(community, channel, target)?;
    for _ in 0..CAS_RETRIES {
        let Some((mut doc, version)) = store::get_doc_versioned::<Membership>(store, &key).await?
        else {
            return Ok(());
        };
        if !doc.is_live() {
            return Ok(());
        }
        doc.removed_at = Some(now_ms);
        match store::put_doc_if_version(store, &key, &doc, Some(version)).await? {
            Ok(_) => {
                // §6.3 rule (b): a removed member's pump must stop serving the
                // channel on its next batch even if the feed record is lost.
                bump_revocation(store, community, target, now_ms).await;
                return Ok(());
            }
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("membership removal contended".into()))
}

/// Add `pubkey_hex` to the channel document's owner set (idempotent). CAS on the
/// channel document's bytes; a lost race retries against the winner's document.
pub(super) async fn add_owner(
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    pubkey_hex: &str,
) -> Result<()> {
    mutate_channel(ape, community, channel, |doc| {
        if doc.owners.contains(pubkey_hex) {
            return false; // already present — nothing to commit
        }
        doc.owners.insert(pubkey_hex.to_string());
        doc.seq = doc.seq.saturating_add(1);
        true
    })
    .await
}

/// Drop `pubkey_hex` from the owner set. When `enforce_last_owner` is set, a
/// removal that would empty the set is refused as a **logged no-op** (never an
/// error — the event is already stored and a wedged saga would be worse than a
/// channel that keeps its last owner, which is buzz's own outcome: its apply-time
/// re-check `warn!`s and leaves the owner in place).
///
/// Returns whether the target ended up a **non-owner**: `true` if it was removed
/// or was not in the set to begin with, `false` if the removal was refused
/// because it was the last owner. The demote caller uses this to decide whether
/// to actually lower the target's `mem/` role, so the owner set and the role can
/// never disagree.
pub(super) async fn remove_owner_guarded(
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    pubkey_hex: &str,
    enforce_last_owner: bool,
) -> Result<bool> {
    let mut ended_non_owner = true;
    mutate_channel(ape, community, channel, |doc| {
        if !doc.owners.contains(pubkey_hex) {
            ended_non_owner = true; // not an owner ⇒ already non-owner
            return false;
        }
        if enforce_last_owner && doc.owners.len() <= 1 {
            tracing::warn!(
                "refusing to remove the last owner of channel {channel}; owner left in place"
            );
            ended_non_owner = false; // refused ⇒ stays an owner
            return false;
        }
        doc.owners.remove(pubkey_hex);
        doc.seq = doc.seq.saturating_add(1);
        ended_non_owner = true;
        true
    })
    .await?;
    Ok(ended_non_owner)
}

/// Read-modify-write the channel document under a bytes-CAS retry loop. `mutate`
/// returns `true` when it changed the document (commit) and `false` when there is
/// nothing to do (return without a write). An absent channel is a no-op: a
/// membership effect for a room that does not exist has nothing to guard.
pub(super) async fn mutate_channel(
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    mut mutate: impl FnMut(&mut Channel) -> bool,
) -> Result<()> {
    let key = keys::guard_channel(community, channel)?;
    for _ in 0..CAS_RETRIES {
        let current = ape
            .get(&key)
            .await
            .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?;
        let Some(bytes) = current else {
            return Ok(());
        };
        let mut doc: Channel = serde_json::from_slice(&bytes)
            .map_err(|e| BuzzError::Internal(format!("channel decode: {e}")))?;
        if !mutate(&mut doc) {
            return Ok(());
        }
        let next = serde_json::to_vec(&doc)
            .map_err(|e| BuzzError::Internal(format!("channel encode: {e}")))?;
        match ape
            .commit_n_if(&key, Some(&bytes), &[(key.as_slice(), next.as_slice())])
            .await
            .map_err(|e| BuzzError::Storage(format!("channel commit: {e}")))?
        {
            Ok(()) => return Ok(()),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict("channel mutation contended".into()))
}
