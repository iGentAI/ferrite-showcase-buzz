//! Channel authorization and creation.

use ferrite_app::tracing;
use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use ferrite_app::AppContext;
use ferrite_app::IngressRequest;
use crate::relay::event::Event;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{FEED_GENERATION, MODERATORS_CONFIG, authenticate_request, enforce_write_gate, storage};
use super::agents::{caller_is_operator};
use super::observability::{record_audit_or_alert};
use super::ws_face::{InheritedTargets};
use super::identity::{relay_identity};

/// Resolve the channel an event belongs to.
///
/// Buzz derives this rather than trusting the author: a reaction or a deletion
/// takes its target's channel, a global-only kind is always channel-less, and
/// everything else may name a channel with an `h` tag. Returning the *derived*
/// value is what stops a client filing a reaction into a room its target never
/// appeared in.
///
/// `prefetched` lets a caller deriving MANY channels at once pay one batched
/// read instead of one read per event ([`ws_inherited_targets`]). It is an
/// input, never a second implementation: there is exactly one place that decides
/// which room an event is in, because a second one would be a second opinion
/// about an access boundary. A caller with nothing pre-resolved passes `None` and
/// the target is read here, as it always was.
fn inherited_target_tag(event: &Event) -> Option<&str> {
    if event.kind == crate::relay::kind::KIND_REACTION {
        event
            .tag_values("e")
            .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
            .last()
            .or_else(|| event.tag_values("e").last())
    } else {
        event.tag_values("e").next()
    }
}

fn direct_event_channel(event: &Event) -> Option<String> {
    if crate::relay::kind::is_global_only_kind(event.kind) {
        return None;
    }
    if crate::relay::kind::is_group_projection(event.kind) {
        return event
            .channel_id()
            .or_else(|| event.d_tag())
            .map(str::to_string);
    }
    event.channel_id().map(str::to_string)
}

/// Resolve the target event's channel for `POST /buzz/api/reactions`, walking
/// chained reactions/deletions up to 8 hops. Returns `Ok(None)` when `event` is
/// a reaction/deletion whose target chain is broken or deleted, so callers fail
/// closed rather than treating an orphaned reaction as global.
pub(super) async fn reaction_target_channel(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    event: &Event,
) -> Result<Option<Option<String>>> {
    if !matches!(
        event.kind,
        crate::relay::kind::KIND_REACTION | crate::relay::kind::KIND_DELETION
    ) {
        return Ok(Some(direct_event_channel(event)));
    }
    let mut current: std::borrow::Cow<'_, Event> = std::borrow::Cow::Borrowed(event);
    for _ in 0..8 {
        let Some(target_hex) = inherited_target_tag(&current) else {
            return Ok(None);
        };
        let Some(target_id) = crate::relay::crypto::from_hex32(target_hex) else {
            return Ok(None);
        };
        let Some(target) = crate::relay::query::event_by_id(events, community, &target_id).await?
        else {
            return Ok(None);
        };
        if !matches!(
            target.kind,
            crate::relay::kind::KIND_REACTION | crate::relay::kind::KIND_DELETION
        ) {
            return Ok(Some(direct_event_channel(&target)));
        }
        current = std::borrow::Cow::Owned(target);
    }
    Ok(None)
}

pub(super) async fn derive_channel(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    event: &Event,
    prefetched: Option<&InheritedTargets>,
) -> Result<Option<String>> {
    if crate::relay::kind::is_global_only_kind(event.kind) {
        return Ok(None);
    }
    if crate::relay::kind::is_group_projection(event.kind) {
        // A relay-signed group projection DESCRIBES a channel, and its NIP-29
        // identity is the `d` tag. This relay emits `h` as well, precisely so
        // the ordinary channel scope applies; the `d` fallback is what keeps the
        // whole class fail-CLOSED for a projection that reaches storage without
        // one — an `h`-less projection would otherwise resolve to no channel,
        // and a private room's member list would be gated as community-global.
        return Ok(event
            .channel_id()
            .or_else(|| event.d_tag())
            .map(str::to_string));
    }
    // NOTE: a DM open/add is deliberately NOT resolved to its room here. Its id
    // is the participant hash and the room may not exist yet, so resolving it
    // would send the first open through `authorize_channel_write`, which fails
    // closed on an unknown channel — nobody could ever start a conversation.
    // The projection emitter resolves it instead (`effects::projection_channel`).
    if matches!(
        event.kind,
        crate::relay::kind::KIND_REACTION | crate::relay::kind::KIND_DELETION
    ) {
        // Inherit from the target event, walking chained reactions/deletions up
        // to 8 hops so a reaction on a reaction still inherits the root
        // message's channel.
        let mut current: std::borrow::Cow<'_, Event> = std::borrow::Cow::Borrowed(event);
        for hop in 0..8 {
            let Some(target_hex) = inherited_target_tag(&current) else {
                return Ok(None);
            };
            let Some(target_id) = crate::relay::crypto::from_hex32(target_hex) else {
                return Err(BuzzError::InvalidEvent("e tag is not a 32-byte id".into()));
            };
            let target = if hop == 0 {
                if let Some(prefetched) = prefetched {
                    match prefetched.get(&target_id) {
                        Some(Some(target)) => Some(target.clone()),
                        Some(None) => {
                            return Err(BuzzError::Internal(format!(
                                "target {target_hex} is stored but cannot be read"
                            )))
                        }
                        None => None,
                    }
                } else {
                    crate::relay::query::event_by_id(events, community, &target_id).await?
                }
            } else {
                crate::relay::query::event_by_id(events, community, &target_id).await?
            };
            let Some(target) = target else {
                return Ok(None);
            };
            if !matches!(
                target.kind,
                crate::relay::kind::KIND_REACTION | crate::relay::kind::KIND_DELETION
            ) {
                return Ok(direct_event_channel(&target));
            }
            current = std::borrow::Cow::Owned(target);
        }
        return Ok(None);
    }
    Ok(event.channel_id().map(str::to_string))
}

/// Refuse a write into a channel the caller may not write to.
///
/// An unknown or unreadable channel fails closed, an archived channel refuses
/// writes while staying readable (history does not vanish), and a private channel
/// requires live membership. An open channel accepts any workspace member, which
/// is buzz's rule.
pub(super) async fn authorize_channel_write(
    ctx: &AppContext,
    community: &str,
    channel: &str,
    writer: &crate::relay::event::PubKey,
) -> Result<()> {
    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let guard_key = keys::guard_channel(community, channel)?;
    let raw = guards
        .get(&guard_key)
        .await
        .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?;
    let Some(bytes) = raw else {
        return Err(BuzzError::NotFound("no such channel".into()));
    };
    let doc: crate::relay::channel::Channel = serde_json::from_slice(&bytes)
        .map_err(|e| BuzzError::Internal(format!("channel decode: {e}")))?;
    if !doc.is_writable() {
        return Err(BuzzError::Unauthorized(
            "channel is archived or deleted".into(),
        ));
    }
    if !doc.visibility.is_private() {
        return Ok(());
    }
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let mem_key = keys::mem(community, channel, writer)?;
    match store::get_doc::<crate::relay::channel::Membership>(&mut events, &mem_key).await? {
        Some(m) if m.is_live() => Ok(()),
        _ => Err(BuzzError::Unauthorized(
            "not a member of this channel".into(),
        )),
    }
}

/// Authorize a membership command (9000/9001/9021/9022) before it is admitted.
///
/// Reads the channel document and the actor's live role, then defers the decision
/// to the pure [`crate::relay::effects::authorize_membership_command`] so the rule table
/// is testable without planes. An unknown channel fails closed.
pub(super) async fn authorize_membership(
    ctx: &AppContext,
    community: &str,
    channel: &str,
    event: &Event,
    actor: &crate::relay::event::PubKey,
) -> Result<()> {
    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let guard_key = keys::guard_channel(community, channel)?;
    let Some(bytes) = guards
        .get(&guard_key)
        .await
        .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
    else {
        return Err(BuzzError::NotFound("no such channel".into()));
    };
    let doc: crate::relay::channel::Channel = serde_json::from_slice(&bytes)
        .map_err(|e| BuzzError::Internal(format!("channel decode: {e}")))?;

    // A membership command may not mutate an archived/deleted room (buzz's
    // archived-channel gate); an unarchive is a 9002 edit, handled elsewhere.
    if !doc.is_writable() {
        return Err(BuzzError::Unauthorized(
            "channel is archived or deleted".into(),
        ));
    }

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let actor_role = live_role(&mut events, community, channel, actor).await?;
    let actor_is_owner = doc.owners.contains(&crate::relay::crypto::to_hex(actor));
    let target = crate::relay::effects::target_pubkey(event);
    let target_is_owner = target
        .as_ref()
        .is_some_and(|t| doc.owners.contains(&crate::relay::crypto::to_hex(t)));
    let requested_role = crate::relay::effects::requested_role(event);

    let target_role = match target.as_ref() {
        Some(t) => live_role(&mut events, community, channel, t).await?,
        None => None,
    };
    crate::relay::effects::authorize_target_role_change(
        event.kind,
        actor_role.is_some_and(crate::relay::channel::Role::is_moderator),
        target_role,
    )?;

    crate::relay::effects::authorize_membership_command(
        event.kind,
        &doc,
        actor,
        actor_role,
        actor_is_owner,
        target.as_ref(),
        target_is_owner,
        requested_role,
        doc.owners.len(),
    )
}

/// Authorize a channel-metadata edit (9002) before it is admitted. Reads the
/// channel document and the actor's live role, then defers to the pure
/// [`crate::relay::effects::authorize_channel_edit`].
pub(super) async fn authorize_channel_edit(
    ctx: &AppContext,
    community: &str,
    channel: &str,
    event: &Event,
    actor: &crate::relay::event::PubKey,
) -> Result<()> {
    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let guard_key = keys::guard_channel(community, channel)?;
    let Some(bytes) = guards
        .get(&guard_key)
        .await
        .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
    else {
        return Err(BuzzError::NotFound("no such channel".into()));
    };
    let doc: crate::relay::channel::Channel = serde_json::from_slice(&bytes)
        .map_err(|e| BuzzError::Internal(format!("channel decode: {e}")))?;
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let actor_role = live_role(&mut events, community, channel, actor).await?;
    crate::relay::effects::authorize_channel_edit(&doc, actor_role, event)
}

/// Authorize a channel deletion (9008) before it is stored.
pub(super) async fn authorize_channel_delete(
    ctx: &AppContext,
    community: &str,
    channel: &str,
    actor: &crate::relay::event::PubKey,
) -> Result<()> {
    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let guard_key = keys::guard_channel(community, channel)?;
    let Some(bytes) = guards
        .get(&guard_key)
        .await
        .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
    else {
        return Err(BuzzError::NotFound("no such channel".into()));
    };
    let doc: crate::relay::channel::Channel = serde_json::from_slice(&bytes)
        .map_err(|e| BuzzError::Internal(format!("channel decode: {e}")))?;
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let actor_role = live_role(&mut events, community, channel, actor).await?;
    // Only consulted when the room role does not already decide it, so an
    // owner's delete costs no config reads.
    let actor_is_community_authority = if actor_role.is_some_and(crate::relay::channel::Role::is_moderator) {
        false
    } else {
        let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
        is_community_moderator(&mut config, community, actor).await?
            || caller_is_operator(ctx, &mut config, actor).await?
    };
    crate::relay::effects::authorize_channel_delete(&doc, actor_role, actor_is_community_authority)
}

/// Whether `who` is a configured community moderator. Reads the
/// [`MODERATORS_CONFIG`] JSON array of hex pubkeys; absent, unreadable, or
/// malformed all read as "not a moderator" (fail-closed — no one can moderate
/// until an operator seeds the set).
pub(super) async fn is_community_moderator(
    config: &mut ferrite_app::client::StoreClusterClient,
    _community: &str,
    who: &crate::relay::event::PubKey,
) -> Result<bool> {
    let raw = config
        .get(MODERATORS_CONFIG)
        .await
        .map_err(|e| BuzzError::Storage(format!("moderator set read: {e}")))?;
    let Some(bytes) = raw else {
        return Ok(false);
    };
    let list: Vec<String> = serde_json::from_slice(&bytes).unwrap_or_default();
    let who_hex = crate::relay::crypto::to_hex(who);
    Ok(list.iter().any(|pk| pk == &who_hex))
}

/// The actor's role if they hold a *live* membership in `channel`, else `None`.
pub(super) async fn live_role(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    channel: &str,
    who: &crate::relay::event::PubKey,
) -> Result<Option<crate::relay::channel::Role>> {
    let key = keys::mem(community, channel, who)?;
    match store::get_doc::<crate::relay::channel::Membership>(events, &key).await? {
        Some(m) if m.is_live() => Ok(Some(m.role)),
        _ => Ok(None),
    }
}

/// `POST /buzz/api/channels` — create a room.
///
/// The channel document and its creator's membership are written together, so a
/// channel can never exist without an owner. Creation is a guarded create-once on
/// the APE plane: two clients racing the same id produce one channel and one
/// conflict, not two half-built rooms.
/// The channel-id prefix process cells derive their rooms under.
pub(crate) const PROCESS_ROOM_PREFIX: &str = "process-";

/// Refuse a `process-` room to anyone but a registered process cell.
///
/// Why: a process room's id is derived from a public repository and ref, so a
/// member could create it first. The cell would then adopt a room another
/// member owns (who could lock its moderators out), or be refused writes in
/// it and stall every observation queued behind it.
pub(super) async fn refuse_reserved_channel_id(
    ctx: &AppContext,
    id: &str,
    creator: &crate::relay::event::PubKey,
) -> Result<()> {
    if !id.starts_with(PROCESS_ROOM_PREFIX) {
        return Ok(());
    }
    super::community::enforce_process_cell_authorship(ctx, creator)
        .await
        .map_err(|_| {
            BuzzError::Unauthorized(format!(
                "channel ids starting with `{PROCESS_ROOM_PREFIX}` are reserved for registered process cells"
            ))
        })
}

pub(super) async fn create_channel(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("channel body: {e}")))?;
    let requested_id = body.get("id").and_then(Value::as_str).map(str::to_string);
    if let Some(id) = requested_id.as_deref() {
        refuse_reserved_channel_id(ctx, id, &caller.pubkey).await?;
    }
    let name = body
        .get("name")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("channel needs a name".into()))?;
    let channel_type = body
        .get("type")
        .and_then(Value::as_str)
        .map_or(
            Some(crate::relay::channel::ChannelType::Stream),
            crate::relay::channel::ChannelType::parse,
        )
        .ok_or_else(|| BuzzError::InvalidEvent("unknown channel type".into()))?;
    let visibility = body
        .get("visibility")
        .and_then(Value::as_str)
        .map_or(
            Some(crate::relay::channel::Visibility::Open),
            crate::relay::channel::Visibility::parse,
        )
        .ok_or_else(|| BuzzError::InvalidEvent("unknown visibility".into()))?;
    let description = body
        .get("description")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    let ttl_seconds = match body.get("ttl_seconds") {
        None => None,
        Some(Value::String(value)) => Some(value.parse::<u64>().map_err(|_| {
            BuzzError::InvalidEvent("channel ttl_seconds must be a positive integer".into())
        })?),
        Some(Value::Number(value)) => Some(value.as_u64().ok_or_else(|| {
            BuzzError::InvalidEvent("channel ttl_seconds must be a positive integer".into())
        })?),
        Some(_) => {
            return Err(BuzzError::InvalidEvent(
                "channel ttl_seconds must be a positive integer".into(),
            ))
        }
    };
    if ttl_seconds == Some(0) {
        return Err(BuzzError::InvalidEvent(
            "channel ttl_seconds must be a positive integer".into(),
        ));
    }

    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let (id, doc) = match requested_id {
        // A client-supplied id keeps today's exact semantics: one attempt, and
        // a taken id is the caller's 409 — the starter rooms' create-or-join
        // race reads that conflict as "someone else won, join instead".
        Some(id) => {
            let created = try_create_channel(
                &mut guards,
                community,
                &id,
                name,
                channel_type,
                visibility,
                description,
                ttl_seconds,
                &caller.pubkey,
                now_ms,
            )
            .await?;
            let doc = match created {
                Some(doc) => doc,
                // The one exception to that 409: the caller's own room of this
                // name whose creator membership was never written (the create
                // committed the guard and then failed). The retry finishes the
                // creation instead of leaving an unusable room.
                None => {
                    let existing = guards
                        .get(&keys::guard_channel(community, &id)?)
                        .await
                        .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
                        .and_then(|bytes| {
                            serde_json::from_slice::<crate::relay::channel::Channel>(&bytes).ok()
                        });
                    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
                    let has_row = events
                        .get(&keys::mem(community, &id, &caller.pubkey)?)
                        .await
                        .map_err(|e| BuzzError::Storage(format!("membership read: {e}")))?
                        .is_some();
                    match existing {
                        Some(doc)
                            if !has_row
                                && doc.created_by == crate::relay::crypto::to_hex(&caller.pubkey)
                                && doc.name == name
                                && doc.is_writable() =>
                        {
                            doc
                        }
                        _ => return Err(BuzzError::Conflict("channel already exists".into())),
                    }
                }
            };
            (id, doc)
        }
        // No id: the relay assigns one. Client-side slugging made every user's
        // private "Welcome" collide on the one `welcome` id the first user
        // owns forever, so every later login ate a 409; a server assignment
        // retries a collision away with a caller-salted suffix instead. The
        // correlation id is the salt, so a REDELIVERED create (the api loop
        // re-drives when a reply publish fails) recomputes the same candidates
        // and lands on the room the first drive made instead of minting a
        // duplicate.
        None => {
            assign_and_create_channel(
                &mut guards,
                community,
                name,
                channel_type,
                visibility,
                description,
                ttl_seconds,
                &caller.pubkey,
                &req.correlation_id,
                now_ms,
            )
            .await?
        }
    };
    let id = id.as_str();
    let mem_key = keys::mem(community, id, &caller.pubkey)?;

    let membership = crate::relay::channel::Membership::new(
        &caller.pubkey,
        crate::relay::channel::Role::Owner,
        now_ms,
    );
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    // Create-if-absent: a creator who was since removed or demoted must not get
    // the Owner row back by creating the same room again.
    events
        .put_if_version(&mem_key, &store::encode_doc(&membership)?, None)
        .await
        .map_err(storage)?
        .ok();

    let audit = crate::relay::audit::AuditDraft::new(
        community,
        crate::relay::audit::AuditAction::ChannelCreated,
        now_ms,
    )
    .actor(&caller.pubkey)
    .object(id)
    .with("visibility", visibility.as_str())
    .with("type", channel_type.as_str());
    // The room exists and is owned. A contended chain is fought for, and only a
    // line that STILL cannot be written is raised as a fault — never a reason to
    // leave the caller without their channel.
    record_audit_or_alert(ctx, &mut guards, community, audit, "channel creation").await;

    // The relay-signed NIP-29 projections a real client reads its channel list
    // from. This route — not a 9007 event — is how a room comes into existence
    // in this port, so it is where the room's first projections are published;
    // every later change re-publishes through the ingest saga's own step. All
    // three at once, because a new room already has metadata, one owner, and one
    // member. Best-effort by the same rule as the audit line above: the caller's
    // channel exists either way.
    if let Some(seckey) = relay_identity(ctx, &mut config).await {
        let mut topics = planes::topics(ctx, store::FEED).map_err(storage)?;
        if let Err(e) = crate::relay::effects::publish_group_projections(
            &mut events,
            &mut guards,
            &mut topics,
            community,
            FEED_GENERATION,
            id,
            crate::relay::effects::ALL_GROUP_PROJECTIONS,
            &seckey,
            now_ms,
        )
        .await
        {
            tracing::warn!("channel {id} group projections failed: {e}");
        }
    }

    serde_json::to_vec(&serde_json::json!({ "ok": true, "channel": doc }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// Create the room a NIP-29 `9007` names, or accept that it is already there.
///
/// The two writes here are the SAME two `POST /buzz/api/channels` makes — a channel
/// document on the guards plane and the creator's owner membership row — so a room
/// created through either path is indistinguishable afterwards. The 39000/39001/39002
/// projections a client builds its channel list from are not published here: 9007 is
/// already classified as channel metadata, so the saga's own projection step emits
/// them, which also keeps them in the order a client consumes.
///
/// **A group id belongs to whoever created it first.** An id that already exists is
/// accepted only when this caller is one of its owners, and that single rule covers
/// both cases that matter: a re-driven saga finds its own room and proceeds (the
/// creation has to be idempotent, because the queue is at-least-once), while anybody
/// else is refused with a conflict. Without it, 9007 would be a way to take over a
/// room by naming it.
pub(super) async fn create_nip29_group(
    ctx: &AppContext,
    community: &str,
    id: &str,
    event: &Event,
    creator: &crate::relay::event::PubKey,
    now_ms: u64,
) -> Result<()> {
    refuse_reserved_channel_id(ctx, id, creator).await?;
    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    // NIP-29 carries the room's display name in a `name` tag. The id is the
    // fallback, so a bare `["h", id]` create still yields a legible room rather
    // than a nameless one.
    let name = event
        .first_tag("name")
        .map(str::to_string)
        .unwrap_or_else(|| id.to_string());
    // NIP-29 groups are public unless marked otherwise, and the marker is the
    // VALUELESS `["private"]`: `first_tag` returns the element at index 1, so it
    // would silently miss the standard spelling and produce a public room.
    let visibility = if event.has_tag("private") {
        crate::relay::channel::Visibility::Private
    } else {
        crate::relay::channel::Visibility::Open
    };

    // An unusable id is refused before anything is written: `try_create_channel`
    // validates BOTH keys the room needs up front.
    if try_create_channel(
        &mut guards,
        community,
        id,
        &name,
        crate::relay::channel::ChannelType::Stream,
        visibility,
        None,
        None,
        creator,
        now_ms,
    )
    .await?
    .is_none()
    {
        // Already there. Ours to FINISH, or somebody else's to refuse.
        let guard_key = keys::guard_channel(community, id)?;
        let existing = guards
            .get(&guard_key)
            .await
            .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
            .ok_or_else(|| BuzzError::Conflict("the group vanished mid-create".into()))?;
        let doc: crate::relay::channel::Channel = serde_json::from_slice(&existing)
            .map_err(|e| BuzzError::Internal(format!("channel decode: {e}")))?;
        if !doc.owners.contains(&crate::relay::crypto::to_hex(creator)) {
            return Err(BuzzError::Conflict("group already exists".into()));
        }
        // Deliberately NOT a return: a create is two writes, and one that
        // committed the document and died before the membership would otherwise
        // be "finished" by every retry while the room stayed ownerless — in a
        // private room, one its own creator cannot post into. Falling through is
        // what makes the pair idempotent rather than merely repeatable.
    }

    // The creator's owner membership, on BOTH paths, as an ATOMIC create-if-absent.
    // `expected = None` checks the absence and applies the write in one operation: a
    // read followed by a put would let a membership command land in between and be
    // overwritten with an Owner row — including a REMOVED one, which would make 9007
    // a way for an ex-owner to reinstate themselves with nobody granting it.
    //
    // A CAS miss is the outcome this WANTS: a row exists now, so leave it. That is
    // also why the miss needs no re-read, unlike a general compare-and-swap — the
    // goal is "ensure a row exists without changing one", and either side of the
    // race satisfies it.
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let mem_key = keys::mem(community, id, creator)?;
    let membership =
        crate::relay::channel::Membership::new(creator, crate::relay::channel::Role::Owner, now_ms);
    let _ = store::put_doc_if_version(&mut events, &mem_key, &membership, None).await?;
    Ok(())
}

/// One guarded create-once attempt for one candidate channel id.
///
/// `Ok(Some(doc))` means this call created the room; `Ok(None)` means the id
/// was already taken (the caller decides whether that is a 409 or a retry).
/// An unusable id is refused before anything is written — both keys the room
/// needs are validated up front, so the failure is a clean 400 rather than a
/// half-created room.
#[allow(clippy::too_many_arguments)]
pub(super) async fn try_create_channel(
    guards: &mut ferrite_app::client::ApeClusterClient,
    community: &str,
    id: &str,
    name: &str,
    channel_type: crate::relay::channel::ChannelType,
    visibility: crate::relay::channel::Visibility,
    description: Option<&str>,
    ttl_seconds: Option<u64>,
    creator: &crate::relay::event::PubKey,
    now_ms: u64,
) -> Result<Option<crate::relay::channel::Channel>> {
    // A 64-hex id is the shape of a DM's participant hash, and a DM is opened
    // only through 41010/41011 with an ownerless private document. A general
    // create of either would let a member pre-squat a future DM as its owner.
    if channel_type == crate::relay::channel::ChannelType::Dm
        || (id.len() == 64 && id.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(BuzzError::InvalidEvent(
            "direct message rooms are opened with a DM, not created as channels".into(),
        ));
    }
    let guard_key = keys::guard_channel(community, id)?;
    keys::mem(community, id, creator)?;
    let mut doc = crate::relay::channel::Channel::new(
        id.to_string(),
        name.to_string(),
        channel_type,
        visibility,
        creator,
        now_ms,
    );
    doc.description = description.map(str::to_string);
    doc.ttl_seconds = ttl_seconds;
    doc.ttl_deadline =
        ttl_seconds.map(|seconds| now_ms.saturating_add(seconds.saturating_mul(1000)));
    let doc_bytes = serde_json::to_vec(&doc)
        .map_err(|e| BuzzError::Internal(format!("channel encode: {e}")))?;
    match guards
        .commit_n_if(
            &guard_key,
            None,
            &[(guard_key.as_slice(), doc_bytes.as_slice())],
        )
        .await
        .map_err(|e| BuzzError::Storage(format!("channel create: {e}")))?
    {
        Ok(()) => Ok(Some(doc)),
        Err(_) => Ok(None),
    }
}

/// How many salted candidates a server-assigned id tries after its slug is
/// taken. Collisions beyond the first candidate need a DIFFERENT caller's
/// salted candidate to collide on 8 hex of a sha256, so the budget exists for
/// arithmetic honesty more than for expected traffic.
pub(super) const CHANNEL_ID_RETRIES: u8 = 4;

/// Create a room whose id the relay assigns: the plain slug first, then up to
/// [`CHANNEL_ID_RETRIES`] caller-salted candidates when a slug is taken.
///
/// The salt is a digest over the caller, the name, the REQUEST's correlation
/// id, and the attempt — deterministic (the platform offers no entropy
/// source) yet distinct per caller, so two users whose rooms both slug to
/// `welcome` stop colliding at their first retry instead of chasing each
/// other through the same candidate list.
///
/// Deriving the candidates from the request rather than from the clock is
/// what makes the route idempotent under queue REDELIVERY (the api loop
/// re-drives a route when its reply publish fails): the re-drive walks the
/// SAME candidate list, finds the room the first drive created — same
/// creator, same name, still live — and returns it instead of salting its
/// way to a duplicate. The same rule means one user re-creating a name they
/// already own is joined to their existing live room, which is exactly what
/// the starter-room create-or-join flow wants.
#[allow(clippy::too_many_arguments)]
pub(super) async fn assign_and_create_channel(
    guards: &mut ferrite_app::client::ApeClusterClient,
    community: &str,
    name: &str,
    channel_type: crate::relay::channel::ChannelType,
    visibility: crate::relay::channel::Visibility,
    description: Option<&str>,
    ttl_seconds: Option<u64>,
    creator: &crate::relay::event::PubKey,
    request_salt: &str,
    now_ms: u64,
) -> Result<(String, crate::relay::channel::Channel)> {
    let base = assigned_channel_id(name, creator, request_salt);
    for attempt in 0..=CHANNEL_ID_RETRIES {
        let id = if attempt == 0 {
            base.clone()
        } else {
            salted_channel_id(&base, name, creator, request_salt, attempt)
        };
        if let Some(doc) = try_create_channel(
            guards,
            community,
            &id,
            name,
            channel_type,
            visibility,
            description,
            ttl_seconds,
            creator,
            now_ms,
        )
        .await?
        {
            return Ok((id, doc));
        }
        // Taken. When it is taken by THIS creator's live room of the SAME
        // name, that room is the answer (the redelivery / re-create case
        // above); anything else — another creator, a renamed room, a deleted
        // one — falls through to the next salted candidate.
        let guard_key = keys::guard_channel(community, &id)?;
        let existing = guards
            .get(&guard_key)
            .await
            .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
            .and_then(|bytes| {
                serde_json::from_slice::<crate::relay::channel::Channel>(&bytes).ok()
            });
        if let Some(doc) = existing {
            if doc.created_by == crate::relay::crypto::to_hex(creator)
                && doc.name == name
                && doc.is_writable()
            {
                return Ok((id, doc));
            }
        }
    }
    Err(BuzzError::Conflict(
        "channel id assignment contended; retry".into(),
    ))
}

/// The base id for a create that omitted one: the CLIENT's own slug rule
/// (buzz-web `channels.ts`) — lowercase; runs of anything outside `[a-z0-9-_]`
/// become one `-`; trim `-`; cap 64 — so a server-assigned id is
/// indistinguishable from a client-assigned one. A name that slugs to nothing
/// usable (all emoji, all punctuation) falls back to a caller-salted `room-…`.
pub(super) fn assigned_channel_id(
    name: &str,
    creator: &crate::relay::event::PubKey,
    request_salt: &str,
) -> String {
    let slug = slugify_channel_name(name);
    let dm_shaped = slug.len() == 64 && slug.bytes().all(|b| b.is_ascii_hexdigit());
    if keys::is_safe_component(&slug) && !dm_shaped {
        return slug;
    }
    let digest = crate::relay::crypto::sha256::sha256_concat(&[
        creator.as_slice(),
        name.as_bytes(),
        request_salt.as_bytes(),
    ]);
    let mut hex = crate::relay::crypto::to_hex(&digest);
    hex.truncate(12);
    format!("room-{hex}")
}

/// The client's slug rule (buzz-web `channels.ts`), near enough that the two
/// agree on every REAL room name. One known divergence: on a `-` adjacent to
/// a disallowed character (`"Q3 - Billing"`) the client keeps the literal
/// dashes (`q3---billing`) while this loop collapses them (`q3--billing`),
/// because JS collapses only DISALLOWED runs. Harmless today — the client
/// always uses whichever id the relay returns — but do not lean on exact
/// equality.
pub(super) fn slugify_channel_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    let mut last_was_dash = false;
    for c in name.to_lowercase().chars() {
        if c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_' {
            out.push(c);
            last_was_dash = c == '-';
        } else if !last_was_dash {
            out.push('-');
            last_was_dash = true;
        }
    }
    let trimmed = out.trim_matches('-');
    trimmed.chars().take(64).collect()
}

/// A salted candidate for retry `attempt` after the base slug collided: the
/// slug truncated to 48 characters plus 8 hex of the caller-and-attempt
/// digest. Truncation keeps the whole id inside the 64-byte component bound
/// with the suffix attached.
pub(super) fn salted_channel_id(
    base: &str,
    name: &str,
    creator: &crate::relay::event::PubKey,
    request_salt: &str,
    attempt: u8,
) -> String {
    let digest = crate::relay::crypto::sha256::sha256_concat(&[
        creator.as_slice(),
        name.as_bytes(),
        request_salt.as_bytes(),
        &[attempt],
    ]);
    let mut suffix = crate::relay::crypto::to_hex(&digest);
    suffix.truncate(8);
    let stem: String = base.chars().take(48).collect();
    format!("{stem}-{suffix}")
}
