//! The event path: upstream reads, submission, the ingest pipeline and the agent
//! and workflow dispatch an accepted event starts.

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
use super::{FEED_GENERATION, MSG_RATE_LIMIT, MSG_RATE_WINDOW_MS, PRESENCE_TTL_MS, authenticate_request, enforce_write_gate, storage};
use super::agents::agent_is_paused;
use super::community::{enforce_process_cell_authorship};
use super::reads::feed_access;
use super::channels::{authorize_channel_delete, authorize_channel_edit, authorize_channel_write, authorize_membership, create_nip29_group, derive_channel, is_community_moderator};
use super::ws_face::{ws_stored_events};
use super::identity::{relay_identity, resolve_access};

/// `POST /query` in the UPSTREAM wire shape: a JSON array of NIP-01 filters in,
/// a JSON array of events out.
///
/// An ADAPTER, not a second read path: every filter is served by the same
/// [`ws_stored_events`] the WebSocket face uses, so the index planning, the
/// per-filter re-check and the per-event authorization are identical — there
/// is no way for this route to see something a `REQ` would not. Results are
/// deduplicated across filters, because a client batching two overlapping
/// filters must not receive one event twice.
pub(super) async fn upstream_query(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let events = upstream_filter_read(ctx, req).await?;
    serde_json::to_vec(&events).map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /count` in the upstream wire shape (NIP-45): the same read, counted.
///
/// Deliberately the same walk rather than a cheaper index count: a count that
/// included events the caller may not read would leak their existence, which
/// is precisely what the per-event gate exists to prevent.
pub(super) async fn upstream_count(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let events = upstream_filter_read(ctx, req).await?;
    serde_json::to_vec(&serde_json::json!({ "count": events.len() }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The shared body of the two filter-array routes.
pub(super) async fn upstream_filter_read(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<Event>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    super::community::enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let filters: Vec<crate::relay::filter::Filter> = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("filters: {e}")))?;
    if filters.is_empty() {
        return Ok(Vec::new());
    }
    // The SAME bound the socket face advertises in NIP-11, so a client that
    // obeys the advertised limit is never refused here for a different one.
    if filters.len() > crate::relay::ws::MAX_FILTERS_PER_REQ {
        return Err(BuzzError::InvalidEvent(format!(
            "at most {} filters per request",
            crate::relay::ws::MAX_FILTERS_PER_REQ
        )));
    }
    // And the socket face's per-filter value bound: each `#e`/`#p`/`ids`
    // value can cost its own index walk, so an unbounded list is an unbounded
    // read for the price of one request.
    for filter in &filters {
        crate::relay::ws::check_filter_bounds(filter).map_err(BuzzError::InvalidEvent)?;
    }

    // Preserve the socket face's FILTER boundary, not only its per-event
    // delivery boundary. Without this precheck, a foreign or unconstrained
    // author-only query scans storage and answers `[]`, which hides a permission
    // refusal as empty state and gives an authenticated caller a broad-scan
    // work primitive over data they may not request.
    for filter in &filters {
        if !crate::relay::access::filter_authorized(
            filter.kinds.as_deref(),
            filter.authors.as_deref(),
            filter.tag_filter("#p"),
            &caller.pubkey,
        ) {
            return Err(BuzzError::Unauthorized(
                "this filter asks for events you may not read".into(),
            ));
        }
    }

    let mut events_store = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut out: Vec<Event> = Vec::new();
    let mut bytes = 0usize;
    for filter in &filters {
        for event in ws_stored_events(
            ctx,
            &mut events_store,
            community,
            &caller.pubkey,
            filter,
            now_ms,
        )
        .await?
        {
            if !seen.insert(event.id) {
                continue;
            }
            // The reply rides one ingress envelope, so it carries the socket
            // face's byte budget too rather than an unbounded page.
            let size = event.serialized_len();
            if !out.is_empty() && bytes.saturating_add(size) > crate::relay::ws::MAX_REQ_BYTES {
                return Ok(out);
            }
            bytes = bytes.saturating_add(size);
            out.push(event);
        }
    }
    Ok(out)
}

/// `POST /buzz/api/events` — the ingest path.
pub(super) async fn submit_event(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;

    let event: Event = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("event body: {e}")))?;

    match accept_event(ctx, &caller.pubkey, &event, now_ms).await? {
        IngestOutcome::ReactionDuplicate { event_id_hex } => serde_json::to_vec(&serde_json::json!({
            "ok": true,
            "event_id": event_id_hex,
            "accepted": false,
            "message": "duplicate: reaction already exists",
        }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}"))),
        IngestOutcome::Accepted {
            event_id_hex,
            duplicate,
            ..
        } => {
            let message = if duplicate { "duplicate:" } else { "" };
            serde_json::to_vec(&serde_json::json!({
                "ok": true,
                "event_id": event_id_hex,
                "accepted": true,
                "message": message,
            }))
            .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
        }
    }
}

/// Every check a CLIENT-SUBMITTED event must pass, whichever face carried it,
/// and then the guarded saga.
///
/// The HTTP route and the WebSocket `EVENT` verb call exactly this, in this
/// order, so the two faces cannot drift: a relay whose socket face admits one
/// event its request face refuses has two different security models, and only
/// one of them was reviewed. `caller` is the authenticated identity the face
/// established — a NIP-98 header for HTTP, a NIP-42 exchange for the socket.
///
/// Below this, [`ingest_event`] is the path a relay-minted persona event shares
/// too (restriction, moderation authority, rate limits, channel derivation,
/// write authorization, DM bounds, reaction dedup, admission, the saga); the
/// checks here are the ones that only make sense for something a CLIENT signed.
pub(super) async fn accept_event(
    ctx: &AppContext,
    caller: &crate::relay::event::PubKey,
    event: &Event,
    now_ms: u64,
) -> Result<IngestOutcome> {
    // Kinds a client may never submit, refused before anything else looks at the
    // body. AUTH events are a challenge/response artefact and are never stored;
    // relay-signed kinds (membership rosters, discovery metadata, presence
    // snapshots, window summaries) carry the relay's own authority, so accepting
    // one from a client would let anyone forge a membership list.
    if event.kind == crate::relay::kind::KIND_AUTH {
        return Err(BuzzError::InvalidEvent(
            "authentication events are never stored".into(),
        ));
    }
    if crate::relay::kind::is_relay_only_kind(event.kind) {
        return Err(BuzzError::Unauthorized(format!(
            "kind {} is relay-signed and cannot be submitted",
            event.kind
        )));
    }
    // Plan 031 Contract D: a process RESULT carries the process cell's
    // authority — accepting one from an arbitrary member would let anyone
    // forge an "rcs_land succeeded" record into a room. Client-signed (the
    // cell is a client, so the kind must not be relay-only), but
    // authorship-gated to the operator-registered cell identities, failing
    // closed when none is registered. The COMMAND (47001) stays an ordinary
    // member event: whether the ask is honoured is the cell's decision at
    // execution time, recorded in its result.
    if event.kind == crate::relay::kind::KIND_PROCESS_RESULT {
        enforce_process_cell_authorship(ctx, &event.pubkey).await?;
    }
    // A command this relay does not apply is refused, not stored. The NIP-43
    // relay-membership family (9030 add / 9031 remove / 9032 change-role) has no
    // applier and no projection anywhere in this crate: accepting one wrote a
    // signed event nothing ever read, so `addRelayMember` and
    // `changeRelayMemberRole` reported success while the roles never moved — and
    // the caller had no way to learn otherwise. Silently swallowing an authority
    // change is strictly worse than refusing it.
    //
    // Not implemented rather than refused-for-now on purpose: this relay has no
    // relay-member registry to apply them to. Its two authority surfaces are the
    // operator-provisioned community-moderator set (`buzz-community-moderators`,
    // written over the NIP-98 operator plane) and per-room roles (kinds 9000/9001,
    // which DO work). Mapping NIP-43's owner/admin/member vocabulary onto the
    // moderator set would make `addRelayMember(pk, "member")` grant community
    // moderation to a plain member — a privilege escalation dressed as a port.
    // `GET /buzz/api/me` is the read that tells a client which of those authorities
    // it actually holds.
    if matches!(
        event.kind,
        crate::relay::kind::RELAY_ADMIN_ADD_MEMBER
            | crate::relay::kind::RELAY_ADMIN_REMOVE_MEMBER
            | crate::relay::kind::RELAY_ADMIN_CHANGE_ROLE
    ) {
        return Err(BuzzError::InvalidEvent(format!(
            "kind {} (NIP-43 relay membership) is not implemented by this relay: \
             community moderators are provisioned by the operator, and room roles \
             are changed with kinds {} / {}",
            event.kind,
            crate::relay::kind::KIND_NIP29_PUT_USER,
            crate::relay::kind::KIND_NIP29_REMOVE_USER,
        )));
    }

    // Three more NIP-29 verbs this relay accepts and NOTHING applies. Found by
    // auditing every registered kind against the code that acts on one, and
    // refused for the reason the 9030–9032 block above gives: an accepted event
    // that changes nothing reports success to a caller who has no way to learn
    // otherwise, which is strictly worse than saying no.
    //
    // Each names the operation that DOES work, because "not implemented" without
    // an alternative is a dead end for whoever reads it.
    if event.kind == crate::relay::kind::KIND_NIP29_DELETE_EVENT {
        return Err(BuzzError::InvalidEvent(format!(
            "kind {} (NIP-29 delete-event) is not applied by this relay, so it would \
             delete nothing: use NIP-09 kind {}, which tombstones the id, purges its \
             rows and blocks resurrection",
            crate::relay::kind::KIND_NIP29_DELETE_EVENT,
            crate::relay::kind::KIND_DELETION,
        )));
    }
    if event.kind == crate::relay::kind::KIND_NIP29_CREATE_INVITE {
        return Err(BuzzError::InvalidEvent(format!(
            "kind {} (NIP-29 create-invite) is not applied by this relay: invites here are \
             HMAC codes minted at POST /buzz/api/invites and redeemed at \
             /buzz/api/invites/claim",
            crate::relay::kind::KIND_NIP29_CREATE_INVITE,
        )));
    }

    // The signature must be the caller's own. The ONLY exemption is the gift wrap
    // (NIP-59), which is signed by a per-message ephemeral key by construction —
    // exempting every `#p`-gated kind would let a caller forge membership notices,
    // observer frames and encrypted turn metrics under another pubkey.
    if &event.pubkey != caller && event.kind != crate::relay::kind::KIND_GIFT_WRAP {
        return Err(BuzzError::Unauthorized(
            "event pubkey does not match the authenticated caller".into(),
        ));
    }
    event.verify()?;
    if event.serialized_len() > crate::relay::event::MAX_SERIALIZED_EVENT_BYTES {
        return Err(BuzzError::TooLarge {
            got: event.serialized_len(),
            max: crate::relay::event::MAX_SERIALIZED_EVENT_BYTES,
        });
    }
    let skew = (now_ms / 1000).abs_diff(event.created_at);
    if skew > 900 {
        return Err(BuzzError::Drift {
            skew_secs: i64::try_from(skew).unwrap_or(i64::MAX),
        });
    }

    ingest_event(ctx, caller, event, now_ms).await
}

/// Longest persona `d` tag accepted.
///
/// The same 64 the subscription id uses, and for the same reason: the value is
/// echoed, stored and rendered, so it is bounded where it enters rather than
/// wherever it is later used.
pub(crate) const MAX_PERSONA_D_TAG: usize = 64;

/// Refuse a persona event whose contract is not met.
///
/// buzz's own product contract for kind 30175, NOT a NIP rule — NIP-33 d-tags are
/// arbitrary text by specification, so nothing in the replaceable-event machinery
/// would ever catch any of this.
///
/// Both rules exist because the relay would otherwise accept a request and then
/// silently ignore it:
///
/// * **The `d` tag is a slug.** It is the persona's stable identity inside its
///   author's namespace: a client renders it, a URL carries it, an operator types
///   it. Non-empty, at most [`MAX_PERSONA_D_TAG`] characters, lowercase ASCII
///   letters, digits, `-` or `_`, and never a leading `_`.
/// * **A `shared` tag, when present, is exactly `["shared","true"]`.**
///   [`crate::relay::access::is_unshared_persona`] treats precisely that spelling as
///   "foreign readers may see this" and every other spelling as unshared — so
///   accepting `["shared","false"]` accepts a tag that reads as though it had been
///   understood, and a three-element form reads as shared while carrying an element
///   nothing looks at. Refused rather than ignored: the two-element `true` form is
///   the only spelling with a meaning.
pub(super) fn check_persona_contract(event: &Event) -> Result<()> {
    if !crate::relay::kind::is_persona(event.kind) {
        return Ok(());
    }
    let Some(d) = event.first_tag("d") else {
        return Err(BuzzError::InvalidEvent(
            "a persona event must carry a d tag".into(),
        ));
    };
    if d.is_empty() {
        return Err(BuzzError::InvalidEvent(
            "a persona d tag must not be empty".into(),
        ));
    }
    if d.chars().count() > MAX_PERSONA_D_TAG {
        return Err(BuzzError::InvalidEvent(format!(
            "a persona d tag is at most {MAX_PERSONA_D_TAG} characters"
        )));
    }
    if d.starts_with('_') {
        return Err(BuzzError::InvalidEvent(
            "a persona d tag must not begin with an underscore".into(),
        ));
    }
    if !d
        .bytes()
        .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_')
    {
        return Err(BuzzError::InvalidEvent(
            "a persona d tag must be lowercase letters, digits, - or _".into(),
        ));
    }
    for tag in &event.tags {
        if tag.first().map(String::as_str) != Some("shared") {
            continue;
        }
        if tag.len() != 2 || tag.get(1).map(String::as_str) != Some("true") {
            return Err(BuzzError::InvalidEvent(
                "a persona shared tag must be exactly [\"shared\",\"true\"]".into(),
            ));
        }
    }
    Ok(())
}

/// Refuse a non-canonical `shared` tag on any shared-gated kind.
///
/// [`crate::relay::access::is_unshared_gated`] treats exactly `["shared","true"]`
/// as shared and every other spelling as unshared, so accepting
/// `["shared","yes"]` on a team-catalog event (kind 30178, upstream's second
/// shared-gated kind) would store a tag that reads as though it had been
/// understood while the event stays private — the same
/// accepted-but-read-as-unshared trap [`check_persona_contract`] closes for
/// personas. Personas are refused by that contract FIRST, with the exact
/// sentence the live gates assert on; this covers the rest of the set.
pub(super) fn check_shared_tag_contract(event: &Event) -> Result<()> {
    if !crate::relay::kind::is_shared_gated(event.kind) {
        return Ok(());
    }
    for tag in &event.tags {
        if tag.first().map(String::as_str) != Some("shared") {
            continue;
        }
        if tag.len() != 2 || tag.get(1).map(String::as_str) != Some("true") {
            return Err(BuzzError::InvalidEvent(
                "a shared tag must be exactly [\"shared\",\"true\"]".into(),
            ));
        }
    }
    Ok(())
}

/// The outcome of the shared ingest tail.
pub(crate) enum IngestOutcome {
    /// Admitted (or found already admitted) and driven to completion.
    Accepted {
        event_id_hex: String,
        duplicate: bool,
    },
    /// A reaction already active for `(target, actor, emoji)`: nothing stored,
    /// buzz's sequential-path short-circuit.
    ReactionDuplicate { event_id_hex: String },
}

/// Every check an event must pass REGARDLESS of who built it — a client
/// submission or a relay-minted persona event — then the guarded saga and the
/// dispatcher hook. This is deliberately ONE function so a persona enjoys no
/// second, subtly different write path: restriction, moderation authority,
/// rate limits, channel derivation, write authorization, DM bounds, reaction
/// dedup, admission, and the saga are byte-identical for both callers.
///
/// `actor` is the authenticated writer being restriction-checked and
/// rate-limited: the HTTP caller for a client submission, the persona for an
/// agent command. It differs from `event.pubkey` only for NIP-59 gift wraps
/// (an ephemeral signer), whose CALLER must not dodge a ban by wrapping.
pub(crate) async fn ingest_event(
    ctx: &AppContext,
    actor: &crate::relay::event::PubKey,
    event: &Event,
    now_ms: u64,
) -> Result<IngestOutcome> {
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;

    // The write gate: the opt-in closed-community membership check
    // and the standing-sanction check, in that order. Shared with the media
    // upload routes so "may this pubkey write" has exactly one answer — see
    // [`enforce_write_gate`] for why the order and the refusal sentence matter.
    enforce_write_gate(ctx, community, actor, now_ms).await?;

    // Only a community moderator may issue a moderation command (9040–9044).
    if crate::relay::kind::is_moderation_command_kind(event.kind)
        && !is_community_moderator(&mut config, community, actor).await?
    {
        return Err(BuzzError::Unauthorized(
            "only a community moderator may issue this command".into(),
        ));
    }

    // Per-author message rate limit (buzz's fixed-window human tier, minus Redis).
    // Ephemeral heartbeats/typing do not count against the message tier. The
    // limiter FAILS OPEN on an ambiguous increment error — matching buzz's own
    // Redis-INCR risk class — rather than refusing a legitimate write.
    if !crate::relay::kind::is_ephemeral(event.kind) {
        let rate_key = keys::rate(community, actor, "msg");
        match store::bump_expiring_counter(
            &mut config,
            &rate_key,
            1,
            now_ms.saturating_add(MSG_RATE_WINDOW_MS),
        )
        .await
        {
            Ok(count) if count > MSG_RATE_LIMIT => return Err(BuzzError::RateLimited),
            Ok(_) => {}
            Err(e) => tracing::warn!("rate limiter failed open: {e}"),
        }
    }

    let mut ape = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let mut topics = planes::topics(ctx, store::FEED).map_err(storage)?;

    // The content contract for kinds the relay would otherwise accept and then
    // ignore. Deliberately AFTER the restriction, moderation and rate checks
    // above: a content refusal that came first would answer differently for a
    // banned pubkey than for a permitted one, turning a malformed event into a way
    // to probe sanction state.
    check_persona_contract(event)?;
    check_shared_tag_contract(event)?;

    // A workflow definition the executor cannot RUN is refused rather than
    // stored. Without this the only validation was in the effect, which declines
    // to index what it cannot parse — so an invalid definition was answered 200,
    // appeared in the channel, and silently never fired. An author has no way to
    // learn that from a success, which is the whole shape this refuses.
    if event.kind == crate::relay::kind::KIND_WORKFLOW_DEF {
        crate::relay::workflows::definition_of(event)?;
    }

    // NIP-IA: an identity may archive ITSELF, and a community moderator may
    // archive anyone; only a moderator may unarchive. Enforced pre-storage like
    // every other command's own rule,
    // so an unauthorized request is refused rather than stored and then ignored
    // by the effect — which is the shape this whole family was fixed to remove.
    if crate::relay::archive::is_archive_request(event.kind) {
        let Some(target) = crate::relay::archive::archive_target(event) else {
            return Err(BuzzError::InvalidEvent(
                "an archive request's p tag is not a 32-byte pubkey".into(),
            ));
        };
        let actor_is_moderator = is_community_moderator(&mut config, community, actor).await?;
        if event.kind == crate::relay::kind::KIND_IA_UNARCHIVE_REQUEST {
            // Only a moderator lifts an archive. The archived identity itself
            // never reaches this line: the write gate above refuses it.
            crate::relay::archive::authorize_unarchive(actor_is_moderator)?;
        } else {
            crate::relay::archive::authorize_archive(actor, &target, actor_is_moderator)?;
        }
    }

    // Resolve which channel this event belongs to. The author's `h` tag is a
    // CLAIM, not the answer: reactions and deletions inherit their target's
    // channel (otherwise a reaction could be filed into a room the target is not
    // in), global-only kinds are forced channel-less however they are tagged, and
    // kinds that require a channel are refused without one.
    let channel = derive_channel(&mut events, community, event, None).await?;
    if crate::relay::kind::requires_h_channel_scope(event.kind) && channel.is_none() {
        return Err(BuzzError::InvalidEvent(format!(
            "kind {} must name a channel",
            event.kind
        )));
    }

    // Channel write authorization. Membership is the only gate in this product, so
    // it has to be checked on the WRITE path too: without this, any authenticated
    // pubkey could post into a private room it cannot even read.
    if let Some(ch) = channel.as_deref() {
        if crate::relay::kind::is_membership_change(event.kind) {
            // Membership commands carry their own authorization (who may add /
            // remove / the last-owner guard). A join request is the one write a
            // non-member may make, so it bypasses the generic membership gate;
            // every other membership command still requires the base gate first.
            if event.kind != crate::relay::kind::KIND_NIP29_JOIN_REQUEST {
                authorize_channel_write(ctx, community, ch, actor).await?;
            }
            authorize_membership(ctx, community, ch, event, actor).await?;
        } else if event.kind == crate::relay::kind::KIND_NIP29_CREATE_GROUP {
            // 9007 CREATES the room, so there is nothing yet to authorize a write
            // into: the generic gate would refuse it as "no such channel", which
            // is what made advertising NIP-29 a claim this relay did not honour.
            create_nip29_group(ctx, community, ch, event, actor, now_ms).await?;
        } else if event.kind == crate::relay::kind::KIND_NIP29_EDIT_METADATA {
            // A channel-metadata edit has its own per-tag authorization and its
            // own archived-channel rule (an unarchive is the one edit an archived
            // room accepts), so it does NOT go through the generic write gate,
            // which would refuse every write to an archived room.
            authorize_channel_edit(ctx, community, ch, event, actor).await?;
        } else if event.kind == crate::relay::kind::KIND_NIP29_DELETE_GROUP {
            // Deletion has its own owner/admin gate and is allowed while the
            // channel is archived. The generic write gate cannot express that:
            // it correctly refuses every ordinary write to an archived room.
            authorize_channel_delete(ctx, community, ch, actor).await?;
        } else {
            authorize_channel_write(ctx, community, ch, actor).await?;
        }
        if event.kind == crate::relay::kind::KIND_WORKFLOW_DEF {
            if let Some(wf_id) = crate::relay::workflows::workflow_id(event) {
                let wf_key = keys::workflow_definition_index(community, ch, wf_id);
                if let Some(raw) = store::get_raw(&mut events, &wf_key).await? {
                    let existing_author =
                        crate::relay::workflows::indexed_workflow_author(&mut events, community, &raw)
                            .await?;
                    match existing_author {
                        Some(author) if author == event.pubkey => {}
                        Some(_) => {
                            return Err(BuzzError::Unauthorized(
                                "workflow id in this channel is already owned by another author"
                                    .into(),
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
    }

    // A DM open/add names its participants; refuse an out-of-range set before
    // storage (buzz's 2–9 bound) so a malformed DM is a clean 400, not a silent
    // no-op in the effect.
    if matches!(
        event.kind,
        crate::relay::kind::KIND_DM_OPEN | crate::relay::kind::KIND_DM_ADD_MEMBER
    ) {
        let Some(participants) = crate::relay::effects::dm_participants(event) else {
            return Err(BuzzError::InvalidEvent(
                "a DM participant p tag is not a 32-byte pubkey".into(),
            ));
        };
        let count = crate::relay::channel::distinct_participant_count(&participants);
        if !(crate::relay::channel::DM_MIN_PARTICIPANTS
            ..=crate::relay::channel::DM_MAX_PARTICIPANTS)
            .contains(&count)
        {
            return Err(BuzzError::InvalidEvent(format!(
                "a DM must have {}–{} participants",
                crate::relay::channel::DM_MIN_PARTICIPANTS,
                crate::relay::channel::DM_MAX_PARTICIPANTS
            )));
        }
        let dm_id = crate::relay::effects::dm_channel_id(&participants);
        let guard_key = keys::guard_channel(community, &dm_id)?;
        if let Some(existing_bytes) = ape
            .get(&guard_key)
            .await
            .map_err(|e| BuzzError::Storage(format!("dm channel read: {e}")))?
        {
            if !crate::relay::effects::is_compatible_dm_guard(&existing_bytes, &dm_id) {
                return Err(BuzzError::Conflict(
                    "channel id is already occupied by an incompatible channel".into(),
                ));
            }
        }
    }

    if matches!(
        event.kind,
        crate::relay::kind::KIND_WORKFLOW_APPROVAL_GRANTED
            | crate::relay::kind::KIND_WORKFLOW_APPROVAL_DENIED
    ) {
        if let Some(run_ch) =
            crate::relay::workflows::approval_gate_channel(&mut events, community, event).await?
        {
            if let Some(h) = event.channel_id() {
                if h != run_ch {
                    return Err(BuzzError::InvalidEvent(
                        "workflow approval channel tag does not match run channel".into(),
                    ));
                }
            }
            authorize_channel_write(ctx, community, &run_ch, actor).await?;
        }
    }

    // Thread depth is VALIDATION, so it is decided here, before storage. Refusing
    // an over-deep reply in the effect step, after the body and indexes are
    // written, would leave a stored, served event with no audit entry, no feed
    // publish and a saga that never completes.
    crate::relay::effects::refuse_over_deep_reply(&mut events, community, event).await?;

    // Reactions are deduped and target-checked BEFORE storage: buzz stores no
    // kind-7 event for a reaction that is already active, and refuses one whose
    // target it does not hold. (The materialization step is the concurrent-race
    // backstop; this is the common sequential path and the "no target" refusal.)
    if event.kind == crate::relay::kind::KIND_REACTION {
        let Some((target, emoji)) = crate::relay::effects::reaction_target_and_emoji(event) else {
            return Err(BuzzError::InvalidEvent(
                "reaction must reference a target via an e tag, with an emoji of at most 64 characters".into(),
            ));
        };
        let Some(target_id) = crate::relay::crypto::from_hex32(&target) else {
            return Err(BuzzError::InvalidEvent(
                "reaction e tag is not a 32-byte id".into(),
            ));
        };
        if crate::relay::query::event_by_id(&mut events, community, &target_id)
            .await?
            .is_none()
        {
            return Err(BuzzError::InvalidEvent(
                "reaction target event not found".into(),
            ));
        }
        if crate::relay::effects::reaction_is_active_duplicate(
            &mut events,
            community,
            &target,
            &event.pubkey,
            &emoji,
        )
        .await?
        {
            return Ok(IngestOutcome::ReactionDuplicate {
                event_id_hex: crate::relay::crypto::to_hex(&event.id),
            });
        }
    }

    // The relay's own signing identity, for the relay-signed events the saga's
    // last steps publish: the NIP-29 projections and the membership notices.
    // Resolved on the write path rather than at boot so an operator can seed it
    // into a RUNNING deployment, and resolved only for the handful of kinds that
    // actually need it, so the hot chat path pays no extra config round trip.
    // The predicate lives with the emitters (`effects::needs_relay_identity`), so
    // adding a relay-signed step can never silently get `None` here.
    let relay_seckey = if crate::relay::effects::needs_relay_identity(event) {
        relay_identity(ctx, &mut config).await
    } else {
        None
    };

    // A presence heartbeat (20001) refreshes the author's TTL presence key, so
    // presence is observable absence (buzz's `SET EX 90`) with no sweeper.
    //
    // Written BEFORE the event is admitted and driven onto the feed, and a
    // failed write REFUSES the heartbeat (upstream #7532, sync 003). The order
    // is the contract snapshot readers depend on: a live 20001 a client
    // receives must follow a successful mutation of the state `POST
    // /buzz/api/presence` reads, or a client that confirms a live update
    // against the snapshot sees a member "online" in the stream and absent from
    // the roster. Running this after the drive with its error discarded would
    // report success and fan the event out while the state it announces was
    // never written. A storage refusal is a retryable
    // `error:` rather than `invalid:` — the client's 60 s heartbeat retries it.
    if event.kind == crate::relay::kind::KIND_PRESENCE_UPDATE {
        let mut store = planes::store(ctx, store::EVENTS).map_err(storage)?;
        let key = keys::presence(community, &event.pubkey);
        store::put_expiring_doc(
            &mut store,
            &key,
            &now_ms,
            now_ms.saturating_add(PRESENCE_TTL_MS),
        )
        .await
        .map_err(|e| BuzzError::Storage(format!("presence storage unavailable: {e}")))?;
    }

    let outcome = if event.kind == crate::relay::kind::KIND_WORKFLOW_DEF {
        crate::relay::ingest::admit_workflow_definition(
            &mut events,
            &mut ape,
            community,
            event,
            channel.clone(),
            now_ms,
        )
        .await?
    } else {
        crate::relay::ingest::admit(&mut ape, community, event, channel.clone(), now_ms).await?
    };
    // A duplicate DRIVES the saga it found rather than reporting success for work
    // that may still be unfinished.
    crate::relay::ingest::drive(
        &mut events,
        &mut ape,
        &mut topics,
        community,
        FEED_GENERATION,
        event,
        &outcome,
        now_ms,
        relay_seckey.as_ref(),
    )
    .await?;

    // The dispatcher hook: wake any mentioned persona. Retried by
    // the same drivers as the saga itself — a crash before the reply redelivers
    // the ingress envelope, and a resubmission of the same event re-runs this
    // hook; the agents app's admission (keyed by this event's id) absorbs every
    // re-publish, so at-least-once here is exactly-one-turn there.
    maybe_dispatch(ctx, community, event, channel.as_deref(), now_ms).await?;

    // The WORKFLOW hook, beside the dispatcher hook and driven by the same
    // redeliveries. Idempotent under them by construction: a run id is derived
    // from `(workflow, trigger event)`, so a re-driven trigger recomputes it and
    // either resumes its own run or finds it finished.
    maybe_run_workflows(ctx, community, event, channel.as_deref(), now_ms).await?;

    Ok(IngestOutcome::Accepted {
        event_id_hex: crate::relay::crypto::to_hex(&event.id),
        duplicate: outcome.was_duplicate(),
    })
}

/// The access-gated search behind the MCP server's `buzz_search` tool.
///
/// The SAME walk and the SAME per-hit gate [`search_route`] uses, with the
/// subject of the MCP token as the reader. An input to those, never a second
/// implementation: the search index is keyed by term alone, so a word said in a
/// private room has a row exactly like a word said in the open, and only this
/// gate keeps the first out of a non-member's results. A second copy of it would
/// be a second opinion about that boundary.
pub(crate) async fn mcp_search(
    ctx: &AppContext,
    community: &str,
    reader: &crate::relay::event::PubKey,
    query: &str,
    channel: Option<&str>,
    limit: usize,
    now_ms: u64,
) -> Result<Value> {
    super::community::enforce_read_standing(ctx, community, reader, now_ms).await?;
    let terms = crate::relay::search::query_terms(query);
    if terms.is_empty() {
        return Ok(serde_json::json!({ "events": [], "terms": terms, "complete": true }));
    }
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    // Over-fetch, because the gate below discards hits: a page that came back
    // short would be indistinguishable from exhaustion.
    let hits = crate::relay::search::search_current(
        &mut events,
        community,
        &terms,
        limit.saturating_mul(2),
        None,
        // The MCP tool exposes no operator parameters, so the scope is
        // explicitly unbounded rather than accidentally so.
        &crate::relay::search::SearchScope::default(),
    )
    .await?;
    let mut memo = std::collections::BTreeMap::new();
    let mut out: Vec<Event> = Vec::with_capacity(limit);
    let mut complete = hits.complete;
    for hit in hits.events {
        if let Some(want) = channel {
            if hit.event.channel_id() != Some(want) {
                continue;
            }
        }
        if feed_access(ctx, community, reader, &hit.event, &mut memo, now_ms).await {
            out.push(hit.event);
            if out.len() >= limit {
                // Cut short, so not exhaustion whatever the walk reported.
                complete = false;
                break;
            }
        }
    }
    Ok(serde_json::json!({ "events": out, "terms": terms, "complete": complete }))
}

/// Publish a durable [`crate::agents::turns::TurnRequest`] for every
/// registered persona this stored event `p`-mentions AND that may itself
/// read the event (a persona outside a private room must not have the room's
/// messages copied into its agent thread). No turn queue, no registry, or a
/// malformed registry ⇒ inert (warned, never blocking chat itself).
pub(super) async fn maybe_dispatch(
    ctx: &AppContext,
    community: &str,
    event: &Event,
    channel: Option<&str>,
    now_ms: u64,
) -> Result<()> {
    // Everything decidable without I/O first, so the common non-mention path
    // costs nothing.
    if event.kind != crate::relay::kind::KIND_STREAM_MESSAGE {
        return Ok(());
    }
    let Some(channel) = channel else {
        return Ok(());
    };
    if !event
        .tags
        .iter()
        .any(|t| t.first().is_some_and(|k| k == "p"))
    {
        return Ok(());
    }
    let Ok(mut turns) = ctx.queue(crate::agents::turns::TURNS) else {
        return Ok(()); // no turn pipeline on this deployment
    };
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let Some(registry_bytes) =
        store::get_raw(&mut config, crate::relay::agents::REGISTRY_CONFIG).await?
    else {
        return Ok(()); // no personas registered
    };
    // A malformed registry disables DISPATCH loudly but must not take the
    // workspace's message ingestion down with it: personas are optional,
    // chat is not.
    let registry = match crate::relay::agents::Registry::parse(&registry_bytes) {
        Ok(registry) => registry,
        Err(e) => {
            tracing::warn!("persona registry is malformed; no turn was started: {e}");
            // Chat is deliberately unaffected by this, which is exactly why it
            // needs saying out loud: the workspace looks healthy while every AI
            // teammate has gone deaf.
            crate::relay::alerts::operational(
                ctx,
                "error",
                crate::relay::alerts::PERSONA_REGISTRY,
                &format!(
                    "the buzz-agents registry does not parse, so no mention can start a \
                     turn and every persona is unreachable: {e}"
                ),
            )
            .await;
            return Ok(());
        }
    };
    let targets = crate::relay::agents::dispatch_targets(&registry, event, Some(channel));
    if targets.is_empty() {
        return Ok(());
    }

    // A PAUSED persona is registered and does not answer. Filtered here, at the
    // one place a mention becomes a turn, rather than trusted to be filtered by
    // whoever reads the roster — the pause route's whole promise is that the
    // agent stops answering, and a flag nothing enforces is a button that lies.
    //
    // A config read failure leaves the persona ACTIVE: an agent that goes silent
    // because a read blipped is worse than one that answers while a pause is
    // being applied, and this is a product preference, not a security boundary.
    let mut active_targets = Vec::with_capacity(targets.len());
    for persona in targets {
        if agent_is_paused(&mut config, community, &persona.agent).await {
            continue;
        }
        active_targets.push(persona);
    }
    let targets = active_targets;
    if targets.is_empty() {
        return Ok(());
    }
    let trigger = crate::relay::agents::render_trigger(event, channel);
    let thread = crate::relay::agents::dispatch_thread(channel);
    for persona in targets {
        // The read gate, resolved fresh (never from a session cache): may the
        // persona SEE the triggering message?
        let access = resolve_access(ctx, community, channel, &persona.pubkey, now_ms).await;
        if !crate::relay::access::may_deliver(event, &persona.pubkey, access, true).is_allowed() {
            continue;
        }
        // At-least-once by design: the trigger event id is the turn
        // pipeline's admission dedup key, so a redelivered or re-published
        // trigger is absorbed into exactly one turn.
        let turn = crate::agents::turns::TurnRequest {
            agent: persona.agent.clone(),
            harness: persona.harness.clone(),
            msg_id: crate::relay::crypto::to_hex(&event.id),
            thread: thread.clone(),
            message: trigger.clone(),
            model: None,
            // A mention, not a scheduled wake. `skip_serializing_if` keeps this
            // envelope byte-identical to what it was before schedules existed.
            schedule: None,
            // The persona acts for the member who mentioned it.
            requester: Some(crate::relay::crypto::to_hex(&event.pubkey)),
        };
        let payload = serde_json::to_vec(&turn)
            .map_err(|e| BuzzError::Internal(format!("turn encode: {e}")))?;
        turns
            .publish(&payload)
            .await
            .map_err(|e| BuzzError::Storage(format!("agent turn publish: {e}")))?;
    }
    Ok(())
}

/// Index a workflow definition, fire the workflows an admitted message triggers,
/// or resolve an approval gate.
///
/// **The cost order is the whole design.** Kind check first (free), then the
/// channel's `wfch/` marker (ONE point read), and only a channel that actually
/// has a workflow resolves a relay identity or scans a definition. Resolving the
/// signing key up front instead would put a config round trip on every message in
/// the workspace, which is why this is not a step inside `drive`: `drive` sees
/// every event and would have to be handed both the config store and the identity
/// for all of them.
pub(super) async fn maybe_run_workflows(
    ctx: &AppContext,
    community: &str,
    event: &Event,
    channel: Option<&str>,
    now_ms: u64,
) -> Result<()> {
    // A definition's own admission indexes it and marks its channel, which is
    // what every later message's cheap marker check reads.
    if event.kind == crate::relay::kind::KIND_WORKFLOW_DEF {
        let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
        let mut ape = planes::ape(ctx, store::GUARDS).map_err(storage)?;
        return crate::relay::workflows::on_definition(
            &mut events,
            &mut ape,
            community,
            event,
            channel,
        )
        .await;
    }

    let triggers = matches!(
        event.kind,
        crate::relay::kind::KIND_STREAM_MESSAGE | crate::relay::kind::KIND_STREAM_MESSAGE_V2
    );
    let manual = event.kind == crate::relay::kind::KIND_WORKFLOW_TRIGGER;
    let decides = matches!(
        event.kind,
        crate::relay::kind::KIND_WORKFLOW_APPROVAL_GRANTED
            | crate::relay::kind::KIND_WORKFLOW_APPROVAL_DENIED
    );
    if !triggers && !manual && !decides {
        return Ok(());
    }

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    if triggers {
        // The cheap gate: a channel with no workflows stops here, having paid one
        // point read for the whole feature existing. A MANUAL trigger skips it —
        // it names its workflow, so there is nothing to discover.
        let Some(ch) = channel else {
            return Ok(());
        };
        if !crate::relay::workflows::channel_has_workflows(&mut events, community, ch).await? {
            return Ok(());
        }
    }

    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    // Without an identity the relay cannot sign a step's message or a lifecycle
    // event, so there is no workflow to run — the same posture the projections
    // take, and already warned about on its own cadence.
    let Some(seckey) = relay_identity(ctx, &mut config).await else {
        return Ok(());
    };
    let mut ape = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let mut topics = planes::topics(ctx, store::FEED).map_err(storage)?;

    if triggers {
        let mut mentions = Vec::new();
        crate::relay::workflows::maybe_trigger_collecting(
            &mut events,
            &mut ape,
            &mut topics,
            &mut config,
            community,
            FEED_GENERATION,
            event,
            channel,
            Some(&seckey),
            now_ms,
            &mut mentions,
        )
        .await?;
        return dispatch_workflow_mentions(ctx, community, &mentions, now_ms).await;
    }

    // "Run it now", which ignores the workflow's trigger condition. Authorized
    // by the CHANNEL: a 46020 is a channel-scoped event, so it has already
    // passed the ordinary write gate for the room it names — somebody who cannot
    // write to a room cannot start a workflow in it.
    if manual {
        let mut mentions = Vec::new();
        crate::relay::workflows::trigger_now_collecting(
            &mut events,
            &mut ape,
            &mut topics,
            &mut config,
            community,
            FEED_GENERATION,
            event,
            channel,
            Some(&seckey),
            now_ms,
            &mut mentions,
        )
        .await?;
        return dispatch_workflow_mentions(ctx, community, &mentions, now_ms).await;
    }

    // An `approver: "moderator"` gate is authorized against the COMMUNITY
    // moderator set — the same set that authorizes every other moderation verb,
    // rather than a second notion of who is trusted.
    let actor_is_moderator = is_community_moderator(&mut config, community, &event.pubkey).await?;
    let mut mentions = Vec::new();
    crate::relay::workflows::on_approval_collecting(
        &mut events,
        &mut ape,
        &mut topics,
        &mut config,
        community,
        FEED_GENERATION,
        event,
        actor_is_moderator,
        Some(&seckey),
        now_ms,
        &mut mentions,
    )
    .await?;
    dispatch_workflow_mentions(ctx, community, &mentions, now_ms).await
}

/// Start the persona turns a workflow's agent steps asked for. Each step posts
/// a relay-signed mention through `ingest::drive`, which runs no ingress hooks,
/// so without this the mention sat in the room and no turn ever started
/// (finding_f47620e628f4b60e, finding_b963d9a036a21ae4). The mention's id is the
/// turn's admission key, so a re-driven workflow starts each turn once.
async fn dispatch_workflow_mentions(
    ctx: &AppContext,
    community: &str,
    mentions: &[Event],
    now_ms: u64,
) -> Result<()> {
    for mention in mentions {
        let channel = mention.channel_id().map(str::to_string);
        maybe_dispatch(ctx, community, mention, channel.as_deref(), now_ms).await?;
    }
    Ok(())
}
