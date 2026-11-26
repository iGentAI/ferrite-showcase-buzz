//! Membership notices: the `#p`-gated events that tell a person they joined a room.

use ferrite_app::tracing;
use ferrite_app::client::ApeClusterClient;
use ferrite_app::client::StoreClusterClient;
use ferrite_app::client::TopicLogClient;
use crate::relay::channel::Membership;
use crate::relay::crypto::to_hex;
use crate::relay::error::Result;
use crate::relay::event::Event;
use crate::relay::event::PubKey;
use crate::relay::kind::Kind;
use crate::relay::keys;
use crate::relay::kind;
use crate::relay::store;
use super::projections::{projection_channel, projections_for};

//
// **The two-minute DM.** A participant who is ADDED to a room — the recipient of
// a fresh DM, a member an admin puts in a private channel — had no way to learn
// it promptly. The relay published the room's 39000/39002 inside the same saga,
// but a client's live subscriptions are per KNOWN channel (`{"#h":[id]}`), and a
// room you have never heard of matches none of them. So discovery fell back to
// the channel-list poll, and the recipient side of a DM materialized in ~1–2
// minutes (measured live).
//
// The client already has the mechanism for the fast path: one always-on
// subscription for kinds 44100/44101 filtered `{"#p":[me]}`, whose handler
// invalidates the channel list (`buzz-web`'s `useMembershipNotifications`). Those
// kinds were DECLARED here — `#p`-gated, membership-changing — and nothing minted
// them. This is what mints them.
//
// Four properties, each mirroring the projection emitter's:
//
// 1. **One write path.** A notice is signed with the relay's key and pushed
//    through `admit` + [`crate::relay::ingest::drive_relay_minted`], so it is stored,
//    indexed, audited, and delivered by exactly the code a client event uses —
//    and it is `#p`-gated, so only the subject (and the relay) can ever read it.
// 2. **Exactly one notice per (trigger, subject).** The notice's `created_at` is
//    the TRIGGER's admission instant, read from the trigger's own admission
//    document, and its tags name the trigger's id — so every re-drive of the same
//    trigger mints a byte-identical event whose id is already admitted. A
//    deterministic id is the dedup; nothing here counts attempts.
// 3. **It states what the state SAYS, not what the command asked for.** A notice
//    is minted only when the subject's `mem/` row agrees (live for an add, gone
//    for a remove), so a refused or superseded command mints nothing.
// 4. **It cannot loop.** 44100/44101 invalidate no projection and produce no
//    notice of their own, and they are minted through `drive_relay_minted`, which
//    has neither step.

/// The most notices one trigger may mint. A membership command carries a `p`
/// list; without a bound, one command could mint an unbounded number of signed
/// events. DM participants are already capped much lower than this
/// ([`crate::relay::channel::DM_MAX_PARTICIPANTS`]).
pub const MAX_MEMBERSHIP_NOTICES: usize = 32;

/// The notice one admitted event asks for: which kind, and about whom.
///
/// `None` for everything else, including the notice kinds themselves. The
/// SUBJECT is never the trigger's own author: a notice exists to tell somebody
/// something they did not do, and the actor's client already knows. That is what
/// leaves join/leave requests (whose subject IS the author) out.
///
/// A role-changing 9000 mints a fresh 44100 for its subject, DELIBERATELY: a
/// notice is not prose ("you were added") but an invalidation signal — the
/// client's handler refetches the channel list, detail and members — and a role
/// change is exactly a membership fact the subject should learn promptly. Each
/// distinct trigger mints exactly one deterministic notice (property 2), so
/// there is no unbounded re-minting to bound here.
#[must_use]
pub fn membership_notice_for(event: &Event) -> Option<(Kind, Vec<PubKey>)> {
    let notice_kind = match event.kind {
        kind::KIND_DM_OPEN | kind::KIND_DM_ADD_MEMBER | kind::KIND_NIP29_PUT_USER => {
            kind::KIND_MEMBER_ADDED_NOTIFICATION
        }
        kind::KIND_NIP29_REMOVE_USER => kind::KIND_MEMBER_REMOVED_NOTIFICATION,
        _ => return None,
    };
    let mut subjects: Vec<PubKey> = Vec::new();
    for hex in event.tag_values("p") {
        let Some(pk) = crate::relay::crypto::from_hex32(hex) else {
            continue;
        };
        if pk == event.pubkey || subjects.contains(&pk) {
            continue;
        }
        subjects.push(pk);
    }
    if subjects.is_empty() {
        return None;
    }
    if subjects.len() > MAX_MEMBERSHIP_NOTICES {
        tracing::warn!(
            "a kind:{} command names {} subjects; only the first {MAX_MEMBERSHIP_NOTICES} are \
             notified (the rest learn on their next channel-list read)",
            event.kind,
            subjects.len()
        );
        subjects.truncate(MAX_MEMBERSHIP_NOTICES);
    }
    Some((notice_kind, subjects))
}

/// Does driving `event` need the relay's own signing key?
///
/// True when any relay-signed saga step applies to it — the NIP-29 projections or
/// the membership notices. The face resolves the identity only for these, so the
/// hot chat path pays no config round trip; keeping the predicate HERE, next to
/// the emitters, is what stops a newly added relay-signed step from silently
/// receiving `None`.
#[must_use]
pub fn needs_relay_identity(event: &Event) -> bool {
    !projections_for(event.kind).is_empty()
        || membership_notice_for(event).is_some()
        || crate::relay::archive::is_archive_request(event.kind)
}

/// Publish the relay-signed membership notices `event` calls for — the saga step,
/// called from [`crate::relay::ingest::drive`] after the group projections.
///
/// `notice_at_secs` must be the TRIGGER's admission instant in whole seconds, so
/// a re-driven saga mints the identical event id (property 2 above).
///
/// Best-effort inside, exactly like the projections: the membership change is
/// already committed by the time this runs, so a failed notice must not fail the
/// saga — the subject then learns on their next channel-list read, which is the
/// behaviour this whole step exists to make rare rather than to replace.
#[allow(clippy::too_many_arguments)]
pub async fn emit_membership_notices(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    event: &Event,
    channel: Option<&str>,
    seckey: Option<&[u8; 32]>,
    notice_at_secs: u64,
) -> Result<()> {
    let Some((notice_kind, subjects)) = membership_notice_for(event) else {
        return Ok(());
    };
    let resolved = projection_channel(event, channel);
    let (Some(seckey), Some(channel)) = (seckey, resolved.as_deref()) else {
        return Ok(());
    };
    let trigger_hex = to_hex(&event.id);
    for subject in subjects {
        let subject_hex = to_hex(&subject);
        // The state, not the request: `mem/` is the source of truth the roster
        // projection is built from, and a notice that disagrees with it would be
        // a lie a client acts on.
        let live = match keys::mem(community, channel, &subject) {
            Ok(key) => store::get_doc::<Membership>(store, &key)
                .await?
                .is_some_and(|m| m.is_live()),
            Err(_) => continue, // an unspellable channel keys no membership row
        };
        let wanted = notice_kind == kind::KIND_MEMBER_ADDED_NOTIFICATION;
        if live != wanted {
            continue;
        }
        // `h` is the channel scope every gate reads; `p` is who this is about and
        // what makes it readable ONLY by them; `e` names the trigger, which is
        // both provenance and what makes the id deterministic per trigger.
        let tags = vec![
            vec!["h".to_string(), channel.to_string()],
            vec!["p".to_string(), subject_hex],
            vec!["e".to_string(), trigger_hex.clone()],
        ];
        if let Err(e) = publish_membership_notice(
            store,
            ape,
            topics,
            community,
            generation,
            channel,
            notice_kind,
            tags,
            seckey,
            notice_at_secs,
        )
        .await
        {
            tracing::warn!(
                "kind:{notice_kind} notice for channel {channel} was not published ({e}); \
                 the subject learns on their next channel-list read"
            );
        }
    }
    Ok(())
}

/// Sign and drive one membership notice through the ordinary saga.
#[allow(clippy::too_many_arguments)]
pub(super) async fn publish_membership_notice(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    channel: &str,
    notice_kind: Kind,
    tags: Vec<Vec<String>>,
    seckey: &[u8; 32],
    created_at: u64,
) -> Result<()> {
    let event = Event::sign_new(seckey, notice_kind, tags, "", created_at)?;
    let outcome = crate::relay::ingest::admit(
        ape,
        community,
        &event,
        Some(channel.to_string()),
        created_at.saturating_mul(1000),
    )
    .await?;
    crate::relay::ingest::drive_relay_minted(
        store,
        ape,
        topics,
        community,
        generation,
        &event,
        &outcome,
        created_at.saturating_mul(1000),
    )
    .await
    .map(|_| ())
}
