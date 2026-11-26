//! Group metadata and roster events the relay signs, which are what a client builds
//! its workspace from.

use ferrite_app::tracing;
use ferrite_app::client::ApeClusterClient;
use ferrite_app::client::StoreClusterClient;
use ferrite_app::client::TopicLogClient;
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
use super::dm::{dm_channel_id, dm_participants};
use super::replacement::{ReplaceableHead};

//
// A real buzz client does not build its workspace from this relay's JSON
// routes. It builds it from kind **39000** (group metadata) and kind **39002**
// (group members) events that the RELAY signs with its own key, and treats
// those as the authority on which rooms exist and who is in them — buzz's
// `emit_group_discovery_events` mints them, and the desktop client's
// `channel_info_from_event` / `channel_members_from_event` are the only way its
// channel list and roster are ever populated. Declaring the kinds is not
// enough: with nothing producing them, a correct client opens onto an empty
// workspace however much state this relay holds.
//
// Four properties make publishing them from inside the ingest saga safe:
//
// 1. **One write path.** A projection is minted with [`Event::sign_new`] and
//    pushed through [`crate::relay::ingest::admit`] + the saga
//    ([`crate::relay::ingest::drive_relay_minted`]) like any other event, so it is
//    stored, indexed, audited, replaced, and delivered by exactly the code a
//    client event uses. Nothing here writes storage directly, and nothing here
//    re-implements a gate.
// 2. **The replacement engine retires the previous version.** 39xxx is
//    parameterized-replaceable and `d` is the channel, so `apply_replacement`
//    supersedes the old roster per `(kind, relay pubkey, channel)` — but only
//    if `created_at` strictly ADVANCES. Hence the timestamp is
//    `max(now, watermark + 1)`: with a coarse (or frozen) clock two versions
//    would tie at one second, and [`version_beats`] breaks a tie by lowest id,
//    where the STALE roster can win. The watermark is the floor precisely
//    because it is the same fact the replacement decision reads.
// 3. **It is self-recognizing, so re-entry is a no-op.** Each attempt derives
//    the projection from live state and compares it against the version the
//    watermark already names; identical means nothing is published. That is
//    what makes a re-driven saga (and a second instance driving the same
//    event) cost one read instead of a second event, and it is why the step is
//    NOT gated on the event being new: a saga that tore between storage and
//    completion must still get its roster out.
// 4. **It cannot loop.** A projection is driven by
//    [`crate::relay::ingest::drive_relay_minted`], which HAS no projection step, and
//    [`projections_for`] is empty for a projection kind anyway. There is no
//    path from an emission back to an emission.
//
// DM rooms ARE here, with one wrinkle: `apply_dm_open` creates a channel whose
// id comes from the participant hash rather than an `h` tag, so the face's
// `derive_channel` has a DM branch feeding this the same way an `h` tag does.
// A DM's metadata projection carries `t=dm` and `hidden` (`push_metadata_tags`),
// which is how a client tells a conversation from a room.

/// Every projection of one channel. A brand-new room needs all three at once:
/// it exists, and its creator is already its sole owner and member.
pub const ALL_GROUP_PROJECTIONS: &[Kind] = &[
    kind::KIND_NIP29_GROUP_METADATA,
    kind::KIND_NIP29_GROUP_ADMINS,
    kind::KIND_NIP29_GROUP_MEMBERS,
];

/// A metadata edit changes the room's identity, never its roster.
pub(super) const METADATA_PROJECTION: &[Kind] = &[kind::KIND_NIP29_GROUP_METADATA];

/// A membership change changes the roster, and may change the admin list with
/// it (an added owner, a demoted admin, a departed one). Both are attempted
/// rather than predicted: deciding which moved would mean re-deriving the roles
/// the command implies, and the emitter already publishes nothing when a
/// projection is unchanged.
pub(super) const ROSTER_PROJECTIONS: &[Kind] = &[
    kind::KIND_NIP29_GROUP_MEMBERS,
    kind::KIND_NIP29_GROUP_ADMINS,
];

/// How many times one projection re-derives and re-publishes before it gives
/// up and logs.
///
/// More than one is needed because a lost tie is normal: two membership events
/// for one channel can drive at once, each reads the roster, and only one of
/// their versions takes the cell. The loser re-derives from live state — which
/// by then includes the winner's change — and republishes above the new
/// watermark, so the cell converges on the CURRENT roster rather than on
/// whichever event arrived last. A handful of rounds settles that; giving up
/// after them logs instead of spinning, because the next membership or metadata
/// change re-converges the cell anyway.
pub(super) const PROJECTION_ATTEMPTS: usize = 4;

/// Rows asked for per page of one channel's membership walk.
///
/// The store plane charges a scan the page it ASKS for, not the rows it returns
/// (upstream `ferrite-store` service.rs: "a scan is charged the page it ASKS
/// for"), so a 1024-row page cost 1024 request units even for a room of three.
/// Every roster and admin projection walks the roster, and a boot reconciles
/// every room, so that price was paid several times per room and emptied the
/// events store's quota within seconds of a boot. The walk pages, so a smaller
/// page changes only the cost: about `rows + 128` units instead of 1024 minimum.
pub(super) const ROSTER_SCAN_PAGE: u32 = 128;

/// Rows asked for per page when walking the stored versions of one
/// `(author, kind)` address. Such an address holds one or two versions between
/// sweeps, so a small page is the honest request (see [`ROSTER_SCAN_PAGE`] for
/// why the size is a cost).
pub(super) const VERSION_SCAN_PAGE: u32 = 32;

/// The most versions one sweep walks: the single 1024-row page the sweeps read
/// before they paged, kept as the bound.
pub(super) const MAX_SWEPT_VERSIONS: usize = 1024;

/// The index keys under one `(author, kind)` prefix, paged at
/// [`VERSION_SCAN_PAGE`] and bounded by [`MAX_SWEPT_VERSIONS`]. Collected before
/// the caller deletes anything, as the single-page read it replaces did.
pub(super) async fn author_kind_versions(
    store: &mut StoreClusterClient,
    prefix: &[u8],
    what: &str,
) -> Result<Vec<Vec<u8>>> {
    let mut keys: Vec<Vec<u8>> = Vec::new();
    let mut start_after: Option<Vec<u8>> = None;
    while keys.len() < MAX_SWEPT_VERSIONS {
        let (rows, more) = store::scan(store, prefix, start_after.as_deref(), VERSION_SCAN_PAGE)
            .await
            .map_err(|e| BuzzError::Storage(format!("{what}: {e}")))?;
        let Some((last, _, _)) = rows.last() else {
            break;
        };
        start_after = Some(last.clone());
        keys.extend(rows.into_iter().map(|(key, _value, _version)| key));
        if !more {
            break;
        }
    }
    keys.truncate(MAX_SWEPT_VERSIONS);
    Ok(keys)
}

/// The most `p` tags one roster projection carries.
///
/// A roster is ONE event and an event has a hard serialized ceiling
/// ([`crate::relay::event::MAX_SERIALIZED_EVENT_BYTES`] = 240 KiB), enforced at mint
/// time by [`Event::sign_new`] — past it the relay cannot mint the roster AT
/// ALL, the cell freezes on its last published version, and a client reads a
/// stale room forever. So a bound must exist, and it is derived from that
/// ceiling rather than from a scan page (the scan pages; [`live_members`]
/// walks every page):
///
/// * one member tag `,["p","<64-hex>","","member"]` costs at most **85 bytes**
///   on the wire (66 for the quoted pubkey, `member` the longest role);
/// * everything else is under **1 KiB**: the envelope's hex `id`/`pubkey`/
///   `sig` plus `created_at`/`kind`/`content:""` (356 bytes), the `d`+`h`
///   tags at the 96-byte channel-id cap ([`keys::MAX_COMPONENT_BYTES`]), and
///   the [`ROSTER_TRUNCATED_TAG`] marker (together ≤ 247 bytes).
///
/// 2816 × 85 + 1024 = 240,384 ≤ 245,760, with ~5 KiB of slack for tags this
/// projection may grow later (the absolute ceiling is 2,884 members).
/// `a_roster_at_the_member_cap_still_signs` in this module's tests pins the
/// arithmetic against drift.
///
/// A channel with more live members than this publishes the FIRST 2816 in
/// pubkey order plus a `["truncated","<total live>"]` tag, so incompleteness
/// is visible ON THE WIRE instead of only in a relay log line
/// ([`push_truncation_tag`]). buzz needs none of this because a Postgres
/// result set has no size ceiling.
pub const MAX_PROJECTION_MEMBERS: usize = 2816;

/// Tag name marking a roster projection as incomplete: `["truncated","<M>"]`
/// says the `p` list carries the first [`MAX_PROJECTION_MEMBERS`] of `M` live
/// members. Absent on a complete roster — which also keeps every roster below
/// the cap byte-identical to what was published before the tag existed, so the
/// self-recognition compare never churns a cell over it.
pub const ROSTER_TRUNCATED_TAG: &str = "truncated";

/// The projections an admitted event invalidates.
///
/// Empty for everything else, including — load-bearingly — the projection
/// kinds themselves: that is what stops an emission from triggering an
/// emission.
#[must_use]
pub const fn projections_for(kind: Kind) -> &'static [Kind] {
    match kind {
        kind::KIND_NIP29_CREATE_GROUP | kind::KIND_NIP29_EDIT_METADATA => METADATA_PROJECTION,
        kind::KIND_NIP29_PUT_USER
        | kind::KIND_NIP29_REMOVE_USER
        | kind::KIND_NIP29_JOIN_REQUEST
        | kind::KIND_NIP29_LEAVE_REQUEST => ROSTER_PROJECTIONS,
        // Opening a DM (or adding someone to one) both CREATES the room and
        // sets its roster, so all three cells are new — the same case as
        // creating a channel, which the create route publishes by hand.
        kind::KIND_DM_OPEN | kind::KIND_DM_ADD_MEMBER => ALL_GROUP_PROJECTIONS,
        _ => &[],
    }
}

/// Publish whichever projections `event` invalidated — the saga step, called
/// from [`crate::relay::ingest::drive`].
///
/// `seckey` is the relay's own identity, resolved by the face
/// (`ingress::relay_identity`). `None` means the deployment has not configured
/// one, and then this is a no-op: the face has already warned once, and a
/// missing operator key must never fail a member's write.
#[allow(clippy::too_many_arguments)]
pub async fn emit_group_projections(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    event: &Event,
    channel: Option<&str>,
    seckey: Option<&[u8; 32]>,
    now_ms: u64,
) -> Result<()> {
    let kinds = projections_for(event.kind);
    if kinds.is_empty() {
        return Ok(());
    }
    let resolved = projection_channel(event, channel);
    // A trigger with no resolved channel describes no room. (A membership
    // command without one is a validation gap `apply_membership` already
    // declines to guess at.)
    let (Some(seckey), Some(channel)) = (seckey, resolved.as_deref()) else {
        return Ok(());
    };
    publish_group_projections(
        store, ape, topics, community, generation, channel, kinds, seckey, now_ms,
    )
    .await
}

/// The room a trigger describes: the channel the face resolved, except for a
/// DM, whose id is the participant hash carried by the event itself.
///
/// The face deliberately does NOT resolve a DM's channel (see
/// `ingress::derive_channel`): doing so would send the first open of a
/// conversation through the write gate, which fails closed on a room that does
/// not exist yet. Resolving it here — after the effect has already created the
/// room — is what lets a DM be projected at all.
pub(super) fn projection_channel(event: &Event, channel: Option<&str>) -> Option<String> {
    if matches!(event.kind, kind::KIND_DM_OPEN | kind::KIND_DM_ADD_MEMBER) {
        return dm_participants(event).map(|participants| dm_channel_id(&participants));
    }
    channel.map(str::to_string)
}

/// Publish `kinds` for `channel`, signed by the relay's own key.
///
/// Each projection is independent and **best-effort**: a failure is logged and
/// the next projection still runs. That is deliberate and is the same posture
/// as [`bump_revocation`] — by the time this runs, the state change that
/// triggered it is already committed, so failing the saga would refuse a
/// membership change that actually happened, and would keep refusing it until
/// the queue dead-lettered the envelope. A projection is a DERIVED view whose
/// source of truth is the `mem/` rows and the channel document; the cost of a
/// lost one is a client roster that is stale until the next change, and every
/// later change re-converges the cell because this step is self-recognizing.
#[allow(clippy::too_many_arguments)]
pub async fn publish_group_projections(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    channel: &str,
    kinds: &[Kind],
    seckey: &[u8; 32],
    now_ms: u64,
) -> Result<()> {
    for &kind_ in kinds {
        if let Err(e) = publish_projection(
            store, ape, topics, community, generation, channel, kind_, seckey, now_ms,
        )
        .await
        {
            tracing::warn!(
                "kind:{kind_} projection for channel {channel} was not published ({e}); \
                 a client's view of this room stays one change behind until the next one"
            );
        }
    }
    Ok(())
}

/// Converge one `(kind, relay, channel)` cell onto the current state.
#[allow(clippy::too_many_arguments)]
pub(super) async fn publish_projection(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    channel: &str,
    kind_: Kind,
    seckey: &[u8; 32],
    now_ms: u64,
) -> Result<()> {
    let relay_pk = crate::relay::crypto::schnorr::xonly_pubkey(seckey)
        .ok_or_else(|| BuzzError::Internal("relay identity is not a valid secret key".into()))?;

    for _ in 0..PROJECTION_ATTEMPTS {
        // Derived INSIDE the loop, from live state: a retry after a lost tie
        // must publish the roster as it is now, not the one this driver read
        // before the winner's change landed.
        let Some(tags) = projection_tags(store, ape, community, channel, kind_).await? else {
            // No channel document — nothing to describe. A deleted room keeps
            // its last projection rather than gaining an empty one.
            return Ok(());
        };
        let head = projection_head(ape, community, kind_, &relay_pk, channel).await?;
        if let Some(head) = &head {
            if let Some(published) = stored_projection(store, community, head).await? {
                if published.tags == tags && projection_is_indexed(store, community, &published).await? {
                    // Byte-identical to what is already published, and still
                    // readable: a re-driven saga, a duplicate submission, or a
                    // command that changed nothing. Publishing again would only
                    // churn the cell.
                    return Ok(());
                }
            }
        }

        // The watermark is a FLOOR, never a clamp. `version_beats` needs a
        // strictly greater `created_at` — an equal one is decided by lowest id,
        // which is precisely where a stale roster can win — so a second
        // projection within one clock second must sit one second above the
        // version it replaces. That can run the cell's timestamp ahead of the
        // clock, bounded by how fast changes arrive for ONE channel and given
        // back by the per-author rate limit; upstream buzz has the same property
        // (`existing.created_at + 1`). Clamping to the clock instead would
        // manufacture the tie this exists to avoid.
        let created_at =
            (now_ms / 1000).max(head.as_ref().map_or(0, |h| h.created_at.saturating_add(1)));
        let event = Event::sign_new(seckey, kind_, tags, "", created_at)?;
        let outcome =
            crate::relay::ingest::admit(ape, community, &event, Some(channel.to_string()), now_ms)
                .await?;
        // The SAME saga every client event runs — storage, materialization,
        // replacement, audit, feed, completion — through
        // [`crate::relay::ingest::drive_relay_minted`], which is `drive` without the
        // projection step. Calling that rather than `drive` is what makes "an
        // emission can never trigger an emission" a property of the call graph:
        // there is no path from here back to here.
        crate::relay::ingest::drive_relay_minted(
            store, ape, topics, community, generation, &event, &outcome, now_ms,
        )
        .await?;

        // Did this version take the cell? If it did, the projection is
        // published and this driver is done: the state it read was read after
        // the effect that called it, and any concurrent change runs this same
        // loop for itself. If it did NOT (a tie lost to a concurrent version,
        // whose rows `apply_replacement` has just deleted), re-derive and try
        // again above the new watermark.
        let landed = projection_head(ape, community, kind_, &relay_pk, channel)
            .await?
            .is_some_and(|h| h.id == to_hex(&event.id));
        if landed {
            return Ok(());
        }
    }
    Err(BuzzError::Conflict(format!(
        "the kind:{kind_} cell for channel {channel} was contended beyond the attempt budget"
    )))
}

/// The watermark for one projection cell, or `None` when nothing is published.
///
/// A head that cannot be decoded reads as absent: publishing a fresh version is
/// strictly better than refusing to publish because of one unreadable row, and
/// the winner's sweep retires the orphan.
pub(super) async fn projection_head(
    ape: &mut ApeClusterClient,
    community: &str,
    kind_: Kind,
    relay_pk: &PubKey,
    channel: &str,
) -> Result<Option<ReplaceableHead>> {
    let key = keys::dtag_head(community, kind_, relay_pk, channel);
    let Some(bytes) = ape
        .get(&key)
        .await
        .map_err(|e| BuzzError::Storage(format!("projection head read: {e}")))?
    else {
        return Ok(None);
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Whether the published version is still reachable by a filter read: its kind
/// index row exists. A body with no index rows is invisible to every read path
/// while the watermark still names it, so the cell would never be republished.
/// Measured on staging 2026-09-12: `general` and `welcome-everyone` served no
/// 39000/39001 while a redeploy's boot reconcile left them alone. Publishing a
/// fresh version (one second above the watermark) makes the room readable
/// again, whatever removed the rows.
pub(super) async fn projection_is_indexed(
    store: &mut StoreClusterClient,
    community: &str,
    published: &Event,
) -> Result<bool> {
    let key = keys::kind_index(community, published.kind, published.created_at, &published.id);
    Ok(store
        .get(&key)
        .await
        .map_err(|e| BuzzError::Storage(format!("projection index read: {e}")))?
        .is_some())
}

/// The stored body of the version `head` names, if it is still there (the
/// winner of a later race deletes a superseded body).
pub(super) async fn stored_projection(
    store: &mut StoreClusterClient,
    community: &str,
    head: &ReplaceableHead,
) -> Result<Option<Event>> {
    let Some(id) = crate::relay::crypto::from_hex32(&head.id) else {
        return Ok(None);
    };
    store::get_doc::<Event>(store, &keys::evt(community, &id)).await
}

/// The tag set one projection of `channel` must carry, or `None` when the room
/// has no channel document to describe.
///
/// Content is always `""` — every field of a NIP-29 group state event lives in
/// its tags, which is what makes the whole projection addressable and
/// diffable.
pub(super) async fn projection_tags(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    kind_: Kind,
) -> Result<Option<Vec<Vec<String>>>> {
    let Some(doc) = channel_document(ape, community, channel).await? else {
        return Ok(None);
    };
    // `d` is NIP-29's group identity — the tag a client keys its channel list
    // by — and `h` is this port's channel scope, which is what the access gate,
    // the timeline index, and the replacement deletion path all read
    // (`ingress::derive_channel`, `ingest::index_keys_for`,
    // `delete_event_rows`). Both name the same channel; a projection carrying
    // only `d` would be indexed and gated as community-GLOBAL, which is the
    // one shape that leaks a private room's roster.
    let mut tags = vec![
        vec!["d".to_string(), channel.to_string()],
        vec!["h".to_string(), channel.to_string()],
    ];
    match kind_ {
        kind::KIND_NIP29_GROUP_METADATA => push_metadata_tags(&doc, &mut tags),
        kind::KIND_NIP29_GROUP_MEMBERS => {
            let roster = live_members(store, community, channel).await?;
            let listed = roster.members.len();
            for member in roster.members {
                // NIP-29's member shape is `["p", pubkey, relay_url, role]`.
                // The relay url is empty because the canonical relay is the one
                // that SIGNED this event — and the gateway forwards no Host, so
                // this app cannot know its own URL without operator config.
                tags.push(vec![
                    "p".to_string(),
                    member.pubkey,
                    String::new(),
                    member.role.as_str().to_string(),
                ]);
            }
            push_truncation_tag(&mut tags, listed, roster.total_live);
        }
        kind::KIND_NIP29_GROUP_ADMINS => {
            let (moderators, total) = moderators(store, community, channel, &doc).await?;
            let listed = moderators.len();
            for (pubkey, role) in moderators {
                tags.push(vec!["p".to_string(), pubkey, role.as_str().to_string()]);
            }
            push_truncation_tag(&mut tags, listed, total);
        }
        _ => return Ok(None),
    }
    Ok(Some(tags))
}

/// The 39000 tag vocabulary, transliterated from buzz's
/// `emit_group_discovery_events`.
///
/// Two markers are DERIVED from the rules that actually decide, rather than
/// asserted: `public`/`private` mirrors the read gate
/// ([`crate::relay::access::may_deliver`]), and `open`/`closed` mirrors the join rule
/// ([`authorize_membership_command`], where a 9021 self-request is accepted for
/// an open channel and refused for a private one). buzz emits `closed`
/// unconditionally, which contradicts its own join rule; publishing the rule
/// this relay enforces is the point of the tag.
///
/// Two of NIP-29's tags are deliberately absent. **`picture`**: this port's
/// channel document has no avatar field, so there is no value to publish and
/// inventing one would put something on the wire no writer can set.
/// **`ttl_deadline`**: buzz publishes it as RFC-3339 and this port holds epoch
/// milliseconds; formatting a civil date is not something a one-dependency
/// crate should grow for a display hint, and `ttl` already carries the
/// lifetime.
pub(super) fn push_metadata_tags(doc: &Channel, tags: &mut Vec<Vec<String>>) {
    tags.push(vec!["name".to_string(), doc.name.clone()]);
    if let Some(about) = doc.description.as_deref().filter(|s| !s.is_empty()) {
        tags.push(vec!["about".to_string(), about.to_string()]);
    }
    if doc.visibility.is_private() {
        tags.push(vec!["private".to_string()]);
        tags.push(vec!["closed".to_string()]);
    } else {
        tags.push(vec!["public".to_string()]);
        tags.push(vec!["open".to_string()]);
    }
    tags.push(vec!["t".to_string(), doc.channel_type.as_str().to_string()]);
    if matches!(doc.channel_type, crate::relay::channel::ChannelType::Dm) {
        // NIP-29's hint that a conversation does not belong in a public group
        // list. A hint only: the boundary is the channel gate.
        tags.push(vec!["hidden".to_string()]);
    }
    if let Some(topic) = doc.topic.as_deref().filter(|s| !s.is_empty()) {
        tags.push(vec!["topic".to_string(), topic.to_string()]);
    }
    if let Some(purpose) = doc.purpose.as_deref().filter(|s| !s.is_empty()) {
        tags.push(vec!["purpose".to_string(), purpose.to_string()]);
    }
    if doc.archived_at.is_some() {
        tags.push(vec!["archived".to_string(), "true".to_string()]);
    }
    if let Some(ttl) = doc.ttl_seconds {
        tags.push(vec!["ttl".to_string(), ttl.to_string()]);
    }
}

/// The channel document, read-only. `None` = no such room (or a row that cannot
/// be read as one, which is the same answer every other reader gives it).
pub(super) async fn channel_document(
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
) -> Result<Option<Channel>> {
    let key = keys::guard_channel(community, channel)?;
    let Some(bytes) = ape
        .get(&key)
        .await
        .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
    else {
        return Ok(None);
    };
    Ok(serde_json::from_slice(&bytes).ok())
}

/// Say on the WIRE when a roster projection is incomplete.
///
/// `["truncated","<total>"]` after the member tags: the `p` list carries the
/// first `listed` of `total` live members, in pubkey order. A client that only
/// reads `p` tags keeps working — it sees a deterministic prefix of the roster
/// — while a client that reads the tag can tell "these are all the members"
/// from "these are the first N of M", which no relay-side log line can tell
/// it. Nothing is pushed for a complete roster.
pub(super) fn push_truncation_tag(tags: &mut Vec<Vec<String>>, listed: usize, total: usize) {
    if total > listed {
        tags.push(vec![ROSTER_TRUNCATED_TAG.to_string(), total.to_string()]);
    }
}

/// Walk every membership row of one channel in key (= pubkey) order, applying
/// `visit` to each LIVE one, and return how many live rows there were.
///
/// Pages through the scan: a `mem/` prefix is bounded only by community size,
/// while one scan page holds at most [`ROSTER_SCAN_PAGE`] rows — a single page
/// is NOT the roster. The platform's more-pages flag is over-approximate
/// (`true` does not promise another row), so the loop also ends on an empty
/// page.
pub(super) async fn for_each_live_member(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
    mut visit: impl FnMut(Membership),
) -> Result<usize> {
    let prefix = keys::mem_prefix(community, channel)?;
    let mut start_after: Option<Vec<u8>> = None;
    let mut live = 0usize;
    loop {
        let (rows, more) =
            store::scan(store, &prefix, start_after.as_deref(), ROSTER_SCAN_PAGE).await?;
        if rows.is_empty() {
            break;
        }
        for (key, value, _version) in &rows {
            start_after = Some(key.clone());
            // One unreadable row must not cost the whole roster: skip it, loudly.
            match store::decode_doc::<Membership>(key, value) {
                Ok(member) if member.is_live() => {
                    live += 1;
                    visit(member);
                }
                Ok(_) => {}
                Err(e) => tracing::warn!("roster skipped an unreadable row: {e}"),
            }
        }
        if !more {
            break;
        }
    }
    Ok(live)
}

/// One channel's roster as a projection can carry it.
pub(super) struct Roster {
    /// The first [`MAX_PROJECTION_MEMBERS`] live members, in pubkey order.
    ///
    /// Order comes from the key layout (`mem/{c}/{chan}/{pubkey}`), which
    /// matters twice: the tag list is then deterministic, so the
    /// self-recognition compare is a plain equality, and two instances
    /// projecting the same roster mint the same bytes.
    pub(super) members: Vec<Membership>,
    /// Every live member under the prefix — `> members.len()` means the list
    /// above is a prefix, and the projection must say so on the wire.
    pub(super) total_live: usize,
}

/// Every LIVE membership row of one channel, complete across scan pages,
/// collected up to [`MAX_PROJECTION_MEMBERS`] alongside the true live count.
pub(super) async fn live_members(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
) -> Result<Roster> {
    let mut members: Vec<Membership> = Vec::new();
    let total_live = for_each_live_member(store, community, channel, |member| {
        if members.len() < MAX_PROJECTION_MEMBERS {
            members.push(member);
        }
    })
    .await?;
    if total_live > members.len() {
        tracing::warn!(
            "channel {channel} has {total_live} live members; the published roster \
             carries the first {MAX_PROJECTION_MEMBERS} and a `truncated` tag"
        );
    }
    Ok(Roster {
        members,
        total_live,
    })
}

/// The channel's moderators in pubkey order — every live member holding an
/// elevated role, across ALL scan pages, plus every hex in the channel
/// document's `owners` set — with the total before the projection cap.
///
/// Both sources are read because they answer different questions and either can
/// be an instant ahead of the other by design (see [`apply_membership`]'s
/// ordering rules): the `mem/` row is the per-member record the access path
/// reads, while `owners` is the set the last-owner guard decides on. A pubkey
/// the guard still treats as an owner must appear in a client's admin list, so
/// the owner set wins where the two disagree.
///
/// The cap exists for the same event-size ceiling the roster has and is just
/// as theoretical here (it would take 2,816 distinct admins to reach);
/// [`push_truncation_tag`] makes even that case honest.
pub(super) async fn moderators(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
    doc: &Channel,
) -> Result<(Vec<(String, Role)>, usize)> {
    let mut out: std::collections::BTreeMap<String, Role> = std::collections::BTreeMap::new();
    for_each_live_member(store, community, channel, |member| {
        if member.role.is_moderator() {
            out.insert(member.pubkey, member.role);
        }
    })
    .await?;
    for owner in &doc.owners {
        out.insert(owner.clone(), Role::Owner);
    }
    let total = out.len();
    Ok((
        out.into_iter().take(MAX_PROJECTION_MEMBERS).collect(),
        total,
    ))
}
