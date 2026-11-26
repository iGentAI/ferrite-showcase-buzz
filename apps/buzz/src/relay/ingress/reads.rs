//! The read routes: relay info, presence, channel windows, reactions, queries,
//! feeds, member lists, search and subscriptions.

use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use ferrite_app::AppContext;
use ferrite_app::IngressRequest;
use crate::relay::access::ChannelAccess;
use crate::relay::event::Event;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::session::Cursor;
use crate::relay::session::Session;
use crate::relay::session::HEARTBEAT_INTERVAL_MS;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{FEED_GENERATION, FEED_MAX_LIMIT, FEED_MAX_PAGES, MODERATORS_CONFIG, authenticate_request, query_param, storage};
use super::community::{community_is_closed, enforce_read_standing};
use super::ws_face::{feed_head};
use super::identity::{relay_identity, resolve_access, stream_key};

/// `GET /buzz/api/relay-info` — NIP-11 relay metadata (public, unauthenticated):
/// name, description, the kinds the workspace speaks, and the size limits a
/// client must respect. A client reads it once to configure itself.
///
/// Every advertised limit is read from the constant the WebSocket face actually
/// enforces ([`crate::relay::ws`]), so a client that obeys NIP-11 is never refused for
/// a bound it could not have known about.
///
/// `pubkey` is the relay's signing identity (NIP-11's own field), present
/// exactly when the identity is configured and OMITTED when it is not — which
/// makes this route the public probe for the silent-identity-loss failure the
/// metrics `identity` block reports privately. buzz-web's `get_relay_self`
/// already parses exactly this field and, before this, always got null.
pub(super) async fn relay_info(ctx: &AppContext) -> Vec<u8> {
    let mut body = serde_json::json!({
        "name": "buzz",
        "description": "A Ferrite-native buzz workspace relay",
        "software": "buzz",
        // A literal, not env!("CARGO_PKG_VERSION"): the F-20 code gate denies
        // build-time environment reads in a deployed app.
        "version": "0.1.0",
        "supported_nips": [1, 9, 10, 11, 16, 29, 33, 42, 50, 98],
        "limitation": {
            "max_message_length": crate::relay::event::MAX_SERIALIZED_EVENT_BYTES,
            "max_subscriptions": crate::relay::ws::MAX_SUBSCRIPTIONS,
            "max_filters": crate::relay::ws::MAX_FILTERS_PER_REQ,
            "max_limit": crate::relay::ws::MAX_REQ_LIMIT,
            "max_subid_length": crate::relay::ws::MAX_SUB_ID_CHARS,
            "auth_required": true,
            "restricted_writes": true,
        },
    });
    // Best-effort: a config plane hiccup serves the static document rather
    // than failing a public, unauthenticated read.
    if let Ok(mut config) = planes::store(ctx, store::CONFIG) {
        if let Some(seckey) = relay_identity(ctx, &mut config).await {
            if let Some(pk) = crate::relay::crypto::schnorr::xonly_pubkey(&seckey) {
                if let Some(obj) = body.as_object_mut() {
                    obj.insert(
                        "pubkey".to_string(),
                        Value::String(crate::relay::crypto::to_hex(&pk)),
                    );
                }
            }
        }
    }
    serde_json::to_vec(&body).unwrap_or_else(|_| b"{}".to_vec())
}

/// `POST /buzz/api/presence` — the set of currently-online pubkeys.
///
/// Presence is TTL-key absence: a heartbeat (kind 20001) refreshes a key that
/// expires after [`PRESENCE_TTL_MS`], so this read is simply "which presence keys
/// still exist". No sweeper, no snapshot event to reconcile.
///
/// Members only, like the roster ([`members`]): who is online right now is a fact
/// about the workspace's membership, and in a closed community an outsider
/// polling it would build a presence timeline of people they were never invited
/// to see.
pub(super) async fn presence(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let mut store = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let prefix = keys::presence_prefix(community);
    let mut online = Vec::new();
    let mut start_after: Option<Vec<u8>> = None;
    loop {
        let (rows, has_more) =
            crate::relay::store::scan(&mut store, &prefix, start_after.as_deref(), 256).await?;
        if rows.is_empty() {
            break;
        }
        for (key, _value, _version) in &rows {
            start_after = Some(key.clone());
            // The pubkey is the key's last `/`-separated component.
            if let Some(pk) = String::from_utf8_lossy(key).rsplit('/').next() {
                online.push(pk.to_string());
            }
        }
        if !has_more {
            break;
        }
    }
    serde_json::to_vec(&serde_json::json!({ "ok": true, "online": online }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/window` — a channel's TOP-LEVEL timeline (buzz's window view).
///
/// Body: `{ "channel": "<id>", "cursor": "<opaque>"?, "limit": N? }`. Returns
/// `events` (roots, and depth-1 broadcast replies — ordinary nested replies are
/// hidden), each with a `thread` summary when it has replies, and a `bounds`
/// object `{ has_more, next_cursor }` that is the ONLY authority on exhaustion
/// (buzz's kind-39006 rule): a caller pages until `has_more` is false and never
/// infers "done" from a short page, since the top-level filter makes page size
/// variable. The underlying scan reuses the reviewed byte-budgeted timeline path,
/// so the reply stays inside the envelope.
pub(super) async fn channel_window(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("window body: {e}")))?;
    let channel = body
        .get("channel")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("window needs a channel".into()))?;
    let cursor = body.get("cursor").and_then(Value::as_str);
    let limit = body
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(50, |n| usize::try_from(n).unwrap_or(50))
        .clamp(1, 200);

    let page = window_page(
        ctx,
        community,
        channel,
        &caller.pubkey,
        cursor,
        limit,
        now_ms,
    )
    .await?;
    serde_json::to_vec(&page).map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The window query for one reader: the access-gated, top-level timeline with
/// per-row thread summaries and the 39006 exhaustion authority. Shared by the
/// HTTP route (the authenticated caller) and the persona surface (the
/// persona as reader) so both see the world through the SAME gate.
pub(crate) async fn window_page(
    ctx: &AppContext,
    community: &str,
    channel: &str,
    reader: &crate::relay::event::PubKey,
    cursor: Option<&str>,
    limit: usize,
    now_ms: u64,
) -> Result<Value> {
    // The channel must be readable by the reader, resolved on every request.
    let access = resolve_access(ctx, community, channel, reader, now_ms).await;
    if matches!(access, ChannelAccess::Unresolved)
        || matches!(
            access,
            ChannelAccess::Private {
                reader_is_member: false
            }
        )
    {
        // A reader who cannot see the channel gets an empty, complete window
        // rather than a signal that it exists.
        return Ok(serde_json::json!({
            "ok": true, "events": [], "bounds": { "has_more": false, "next_cursor": Value::Null }
        }));
    }

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let page =
        crate::relay::query::timeline(&mut events, community, Some(channel), cursor, limit).await?;

    // Keep only top-level rows: a root (no thread metadata, or depth 0) or a
    // depth-1 broadcast reply. Attach a summary to any row with replies.
    //
    // The thread metadata for the WHOLE page is fetched in ONE batched read.
    // A `get` per row would make opening a 50-message channel 50
    // sequential round trips for the summaries alone — the single largest cost
    // in the read that every client performs first. `multi_get_raw` chunks to
    // the platform's 256-key refusal bound, so a full page is one call.
    let visible: Vec<&Event> = page
        .events
        .iter()
        .filter(|event| crate::relay::access::may_deliver(event, reader, access, true).is_allowed())
        .collect();
    let meta_keys: Vec<Vec<u8>> = visible
        .iter()
        .map(|event| keys::thread_meta(community, &crate::relay::crypto::to_hex(&event.id)))
        .collect();
    let meta_rows = store::multi_get_raw(&mut events, &meta_keys).await?;

    let mut out = Vec::with_capacity(visible.len());
    for ((event, key), raw) in visible.iter().zip(meta_keys.iter()).zip(meta_rows.iter()) {
        let meta: Option<crate::relay::effects::ThreadMeta> = match raw {
            Some(bytes) => Some(store::decode_doc(key, bytes)?),
            None => None,
        };
        let is_top_level = match &meta {
            None => true,
            Some(m) => m.depth == 0 || (m.depth == 1 && m.broadcast),
        };
        if !is_top_level {
            continue;
        }
        let summary = meta.filter(|m| m.reply_count > 0).map(|m| {
            serde_json::json!({
                "reply_count": m.reply_count,
                "descendant_count": m.descendant_count,
                "last_reply_at": m.last_reply_at,
            })
        });
        out.push(serde_json::json!({ "event": event, "thread": summary }));
    }

    Ok(serde_json::json!({
        "ok": true,
        "events": out,
        // The 39006 exhaustion authority: page until has_more is false.
        "bounds": {
            "has_more": !page.complete,
            "next_cursor": page.next_cursor,
        },
    }))
}

/// `POST /buzz/api/reactions` — the live reaction tallies on a target event.
///
/// Body: `{ "target": "<64-hex event id>" }`. Authorized like a read; the target's
/// own channel gate is inherited (a reaction lives in its target's channel), and a
/// caller who cannot see the target simply gets an empty set because they cannot
/// name a target they never received.
pub(super) async fn reactions(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("reactions body: {e}")))?;
    let target = body
        .get("target")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("reactions needs a target".into()))?;
    let Some(target_id) = crate::relay::crypto::from_hex32(target) else {
        return Err(BuzzError::InvalidEvent("target is not a 32-byte id".into()));
    };

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    // Re-authorize against the target's channel, so reactions on a private
    // channel's message are visible only to a member — the same gate the query
    // path applies to the message itself.
    let target_event = crate::relay::query::event_by_id(&mut events, community, &target_id).await?;
    let visible = match target_event.as_ref() {
        None => false,
        Some(e) => match super::channels::reaction_target_channel(&mut events, community, e).await? {
            None => false,
            Some(None) => crate::relay::access::may_deliver(
                e,
                &caller.pubkey,
                ChannelAccess::Global,
                true,
            )
            .is_allowed(),
            Some(Some(ch)) => {
                let access = resolve_access(ctx, community, &ch, &caller.pubkey, now_ms).await;
                crate::relay::access::may_deliver(e, &caller.pubkey, access, true).is_allowed()
            }
        },
    };
    let tallies = if visible {
        crate::relay::effects::read_reactions(&mut events, community, target).await?
    } else {
        Vec::new()
    };

    serde_json::to_vec(&serde_json::json!({ "ok": true, "reactions": tallies }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/query` — historical reads.
pub(super) async fn run_query(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("query body: {e}")))?;
    let channel = body.get("channel").and_then(Value::as_str);
    let limit = body
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(100, |n| usize::try_from(n).unwrap_or(100));

    // The channel gate comes BEFORE the walk, as `/window` does it: a reader
    // who cannot see a room gets an empty, complete page, not a cursor that
    // says the room has messages (finding_7858c47f9d8ba53f).
    let access = match channel {
        None => ChannelAccess::Global,
        Some(ch) => resolve_access(ctx, community, ch, &caller.pubkey, now_ms).await,
    };
    if matches!(
        access,
        ChannelAccess::Unresolved | ChannelAccess::Private { reader_is_member: false }
    ) {
        return serde_json::to_vec(&serde_json::json!({
            "ok": true, "events": [], "next_cursor": Value::Null, "complete": true,
        }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")));
    }

    // The cursor is an index key: an event id and its exact timestamp. It is
    // sealed under the relay's stream key before it leaves, so a page whose hits
    // were all withheld does not hand the caller the ids and times it withheld,
    // and a caller cannot forge a position. Domain-separated from search's.
    // Without a configured key there is no cursor at all (as for search): a
    // plain one would leak exactly what sealing hides.
    let seal_key = stream_key(ctx, &mut config).await.ok();
    let cursor = match body.get("cursor").and_then(Value::as_str) {
        None => None,
        Some(sealed) => Some(
            seal_key
                .as_deref()
                .and_then(|key| crate::relay::search::open_cursor(key, sealed).ok())
                .and_then(|plain| plain.strip_prefix(QUERY_CURSOR_DOMAIN).map(str::to_string))
                .ok_or_else(|| {
                    BuzzError::InvalidEvent("query cursor is not one this relay issued".into())
                })?,
        ),
    };

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let page = crate::relay::query::timeline(&mut events, community, channel, cursor.as_deref(), limit)
        .await?;
    let next_cursor = page.next_cursor.as_deref().and_then(|plain| {
        seal_key.as_deref().map(|key| {
            crate::relay::search::seal_cursor(key, &format!("{QUERY_CURSOR_DOMAIN}{plain}"))
        })
    });

    // Reads are re-authorized per hit: an index is a routing structure, never an
    // access decision.
    let visible: Vec<&Event> = page
        .events
        .iter()
        .filter(|e| crate::relay::access::may_deliver(e, &caller.pubkey, access, true).is_allowed())
        .collect();

    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "events": visible,
        "next_cursor": next_cursor,
        "complete": page.complete,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The plaintext prefix of a sealed `/buzz/api/query` cursor, so a search
/// cursor (sealed under the same key) is never accepted here, or the reverse.
const QUERY_CURSOR_DOMAIN: &str = "query:";

/// `POST /buzz/api/feed` — the home feed (buzz's M1.12
/// remainder): `{ "category": "mentions"|"needs_action"|"activity",
/// "since"?: <unix secs>, "limit"?: <n ≤ 100> }`.
///
/// buzz's three categories, kind sets, and caps, transliterated
/// (`buzz-db/src/feed.rs`):
///
/// * **mentions** — events `p`-tagging the caller, kinds 9/40002 (stream
///   messages) + 45001/45003 (forum post/comment).
/// * **needs_action** — events `p`-tagging the caller, kinds 46010 (workflow
///   approval requested) + 40007 (reminder).
/// * **activity** — recent events across readable channels, kinds 9/40002 +
///   45001 + 43001/43003/43004 (agent jobs); workflow-execution kinds are
///   deliberately excluded as noise, exactly as buzz excludes them.
///
/// buzz filters by an accessible-channel id list with the
/// `Some(empty) = global-only` convention (an empty list means "channel-less
/// events only", never "everything"). This port reaches the same set through
/// its one access chokepoint instead: every candidate is re-authorized with
/// `resolve_access` + `may_deliver`, so a caller with NO readable channels
/// receives exactly the community-global rows — the convention's outcome,
/// decided per event rather than per SQL clause.
pub(super) async fn feed(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("feed body: {e}")))?;
    let category = body
        .get("category")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("feed needs a category".into()))?;
    let since = body.get("since").and_then(Value::as_u64);
    let limit = body
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(50, |n| usize::try_from(n).unwrap_or(50))
        .clamp(1, FEED_MAX_LIMIT);

    let events = match category {
        "mentions" => {
            let kinds: &[u32] = &[
                crate::relay::kind::KIND_STREAM_MESSAGE,
                crate::relay::kind::KIND_STREAM_MESSAGE_V2,
                crate::relay::kind::KIND_FORUM_POST,
                crate::relay::kind::KIND_FORUM_COMMENT,
            ];
            feed_mention_scan(ctx, community, &caller.pubkey, kinds, since, limit, now_ms).await?
        }
        "needs_action" => {
            let kinds: &[u32] = &[
                crate::relay::kind::KIND_WORKFLOW_APPROVAL_REQUESTED,
                crate::relay::kind::KIND_STREAM_REMINDER,
            ];
            feed_mention_scan(ctx, community, &caller.pubkey, kinds, since, limit, now_ms).await?
        }
        "activity" => {
            let kinds: &[u32] = &[
                crate::relay::kind::KIND_STREAM_MESSAGE,
                crate::relay::kind::KIND_STREAM_MESSAGE_V2,
                crate::relay::kind::KIND_FORUM_POST,
                crate::relay::kind::KIND_JOB_REQUEST,
                crate::relay::kind::KIND_JOB_PROGRESS,
                crate::relay::kind::KIND_JOB_RESULT,
            ];
            feed_activity_scan(ctx, community, &caller.pubkey, kinds, since, limit, now_ms).await?
        }
        other => {
            return Err(BuzzError::InvalidEvent(format!(
                "unknown feed category: {other:?} (mentions | needs_action | activity)"
            )))
        }
    };

    serde_json::to_vec(&serde_json::json!({ "ok": true, "category": category, "events": events }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// The most roster rows one read returns. A community is the one list here that
/// grows without anyone deciding to grow it, so the scan is bounded and the
/// caller pages.
pub(super) const MEMBERS_PAGE: u32 = 200;

/// `GET /buzz/api/members` — who is in this community, and who moderates it.
///
/// The read `list_relay_members` needs, and the one thing the community's own
/// state could not answer: membership rows have been WRITTEN since invites
/// existed (a claim materializes one, and the persona reconciler writes one per
/// registered agent) but nothing ever read them back as a set, so the roster was
/// invisible even to the people in it.
///
/// **Members only.** Not public, unlike `relay-info`: a roster is not secret to
/// the people in it, but publishing everyone in a workspace to the open internet
/// is a disclosure nobody asked for — and in a CLOSED community it is exactly the
/// list an attacker wants.
///
/// **An open community answers honestly rather than emptily.** With no membership
/// gate configured there are no claim rows at all, so `members` is empty and
/// `membership_required` is false. Those two together mean "this community does
/// not track membership", which is a different statement from "nobody is here" —
/// and without the flag a client cannot tell them apart.
///
/// `role` is the client's own vocabulary (`RelayMemberRole`), and `admin` comes
/// from the operator-provisioned moderator set and NOTHING else — that set is
/// what authorizes kinds 9040–9044, so honouring a membership row's claimed role
/// would draw moderation controls for a caller the relay then refuses. Never
/// `owner`: this relay has no owner concept, and reporting one would be a lie a
/// client could act on (the rule `/buzz/api/me` follows).
///
/// The roster is the deduplicated UNION of the scanned rows and that set,
/// because a provisioned moderator holds no claim row and would otherwise be
/// missing from the list entirely.
pub(super) async fn members(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    // A roster is for the people in the community. In a closed one that is the
    // gate itself; in an open one everyone who authenticated is in it.
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let moderators: Vec<String> = match config.get(MODERATORS_CONFIG).await {
        Ok(Some(bytes)) => serde_json::from_slice(&bytes).unwrap_or_default(),
        _ => Vec::new(),
    };

    let cursor = query_param(&req.path, "cursor");
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let prefix = keys::community_mem_prefix(community);
    let (rows, more) = store::scan(
        &mut events,
        &prefix,
        cursor.as_deref().map(str::as_bytes),
        MEMBERS_PAGE,
    )
    .await?;

    let mut out = Vec::with_capacity(rows.len());
    let mut seen: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut next_cursor: Option<String> = None;

    // The configured moderators, on the FIRST page only.
    //
    // They belong IN the roster rather than beside it: an operator-provisioned
    // moderator holds no invite-claim row — the operator plane writes the set,
    // not a `cmem` row — so reporting the scan alone omits the people with the
    // most authority in the community. The set is bounded (256) and small, and
    // folding it into the first page only keeps a cursored page a pure scan, so
    // paging still means what it did.
    if cursor.is_none() {
        for pubkey in &moderators {
            if crate::relay::crypto::from_hex32(pubkey).is_none() {
                continue;
            }
            if seen.insert(pubkey.clone()) {
                out.push(serde_json::json!({
                    "pubkey": pubkey,
                    "role": "admin",
                    "added_by": Value::Null,
                    "created_at": Value::Null,
                    "source": "moderator-set",
                }));
            }
        }
    }

    for (key, value, _version) in &rows {
        // `cmem/{community}/{pubkey}` — the pubkey is the last component.
        let Some(pubkey) = key
            .strip_prefix(prefix.as_slice())
            .and_then(|tail| core::str::from_utf8(tail).ok())
        else {
            continue;
        };
        if crate::relay::crypto::from_hex32(pubkey).is_none() {
            continue;
        }
        // The cursor advances over every row EXAMINED, including ones skipped
        // below, so a page cannot repeat itself.
        next_cursor = Some(String::from_utf8_lossy(key).into_owned());
        // A moderator is emitted on page ONE, always — so their membership row is
        // redundant on every page, including this one. Skipping it throughout is
        // what makes the union deduplicated ACROSS pages and not merely within
        // one: `seen` is rebuilt per request, so a moderator whose row happened to
        // fall on a later page would otherwise appear twice in an assembled
        // roster, as `admin` and then as `member`, with the second overwriting the
        // first in any pubkey-keyed map.
        if moderators.iter().any(|m| m == pubkey) {
            continue;
        }
        if !seen.insert(pubkey.to_string()) {
            continue;
        }
        let doc: Value = serde_json::from_slice(value).unwrap_or(Value::Null);
        // **The moderator set is the ONLY source of `admin`.** A membership row's
        // `role` is whatever an invite payload said, while what actually
        // authorizes kinds 9040–9044 is the operator-provisioned set — so honouring
        // a claimed `admin` here would draw moderation controls for a caller the
        // relay then refuses, and would disagree with `/buzz/api/me`, which reads
        // the set directly.
        out.push(serde_json::json!({
            "pubkey": pubkey,
            "role": "member",
            "added_by": Value::Null,
            "created_at": doc.get("claimed_at_ms").and_then(Value::as_u64).map(|ms| ms / 1000),
            "source": doc.get("source").and_then(Value::as_str),
        }));
    }

    let membership_required = community_is_closed(&mut config).await?;
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "membership_required": membership_required,
        "members": out,
        "moderators": moderators,
        "complete": !more,
        "next_cursor": if more { next_cursor } else { None },
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/search` — full-text search over stored messages.
///
/// Body: `{ "q": "<terms>", "channel": "<id>"?, "limit": N? }`. Answers
/// newest-first with the events whose content carries EVERY term, which is the
/// conjunction a person means by typing two words.
///
/// This is what the client's search box calls. It had no backend at all in a
/// browser build — `search_messages` was a desktop-bridge command — so searching
/// simply returned nothing, indistinguishable from a workspace where nothing had
/// ever been said.
///
/// Every hit is re-authorized through the ONE access chokepoint, memoized per
/// channel for the request. That is not belt-and-braces: the index is keyed by
/// term alone, so a word said in a private room has a row exactly like a word
/// said in the open, and only this gate keeps the first out of a non-member's
/// results.
pub(super) async fn search_route(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("search body: {e}")))?;
    let query = body.get("q").and_then(Value::as_str).unwrap_or_default();
    let channel = body.get("channel").and_then(Value::as_str);
    let limit = body
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(50, |n| usize::try_from(n).unwrap_or(50))
        .clamp(1, FEED_MAX_LIMIT);
    // The client's search operators (`from:` / `after:` / `before:`),
    // decomposed client-side into structured fields (upstream v0.5.x,
    // PR #2871). Validated rather than silently dropped: a search that
    // ignores its own `from:` returns messages the caller explicitly
    // excluded, and the answer LOOKS right. PRESENT is distinguished from
    // ABSENT throughout — a malformed field is refused by name, never
    // coerced into "no bound". JSON null reads as absent, because null is
    // how an optional-field client spells the bound it is not setting.
    let mut authors: Vec<crate::relay::event::PubKey> = Vec::new();
    match body.get("authors") {
        None | Some(Value::Null) => {}
        Some(Value::Array(list)) => {
            if list.len() > 16 {
                return Err(BuzzError::InvalidEvent(
                    "a search names at most 16 authors".into(),
                ));
            }
            for entry in list {
                let Some(pk) = entry.as_str().and_then(crate::relay::crypto::from_hex32) else {
                    return Err(BuzzError::InvalidEvent(
                        "a search author must be a 64-hex pubkey".into(),
                    ));
                };
                authors.push(pk);
            }
        }
        Some(_) => {
            return Err(BuzzError::InvalidEvent(
                "search authors must be an array of 64-hex pubkeys".into(),
            ))
        }
    }
    let time_bound = |name: &str| -> Result<Option<u64>> {
        match body.get(name) {
            None | Some(Value::Null) => Ok(None),
            Some(value) => value.as_u64().map(Some).ok_or_else(|| {
                BuzzError::InvalidEvent(format!(
                    "search {name} must be a non-negative integer of unix seconds"
                ))
            }),
        }
    };
    let scope = crate::relay::search::SearchScope {
        authors,
        since: time_bound("since")?,
        until: time_bound("until")?,
    };

    let terms = crate::relay::search::query_terms(query);
    if terms.is_empty() {
        // A query of pure punctuation has no terms. Answering with nothing is
        // right; answering with the whole community would be a surprising and
        // expensive way to say the same thing.
        return serde_json::to_vec(&serde_json::json!({
            "ok": true, "events": [], "terms": terms, "complete": true, "next_cursor": Value::Null
        }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")));
    }

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    // Cursors cross the wire SEALED (see `search::seal_cursor`): the plain form is
    // an index key naming an event id and timestamp the caller may not be allowed
    // to read. Without the relay's sealed key there is nothing to seal under, so
    // such a deployment answers first pages only rather than leak positions.
    let seal = stream_key(ctx, &mut config).await.ok();
    let cursor_in = match body.get("cursor").and_then(Value::as_str) {
        None => None,
        Some(sealed) => match &seal {
            Some(key) => Some(crate::relay::search::open_cursor(key, sealed)?),
            None => {
                return Err(BuzzError::InvalidEvent(
                    "search paging is unavailable: this relay has no sealed stream key".into(),
                ))
            }
        },
    };
    // Over-fetch, because the access gate and the channel filter below discard
    // hits: a page that came back short would be indistinguishable from
    // exhaustion to a caller.
    let hits = crate::relay::search::search_current(
        &mut events,
        community,
        &terms,
        limit.saturating_mul(2),
        cursor_in.as_deref(),
        &scope,
    )
    .await?;

    let mut memo = std::collections::BTreeMap::new();
    let mut out: Vec<Event> = Vec::with_capacity(limit);
    // Default to where the WALK stopped. `search` may return an empty page with
    // `complete: false` — a conjunction whose candidates all fail the AND probe
    // does precisely that — and dropping its cursor would tell a caller there is
    // more while giving them no way to reach it, leaving retry-the-same-walk as
    // their only move.
    let mut next_cursor = hits.next_cursor.clone();
    let mut complete = hits.complete;
    for hit in hits.events {
        let crate::relay::search::SearchHit { event, cursor } = hit;
        if let Some(want) = channel {
            if event.channel_id() != Some(want) {
                continue;
            }
        }
        if feed_access(ctx, community, &caller.pubkey, &event, &mut memo, now_ms).await {
            out.push(event);
            if out.len() >= limit {
                // The walk went further than this answer did, so resume from the
                // last hit actually RETURNED — anything between here and the
                // walk's end position would otherwise be skipped. And a page cut
                // short is never exhaustion, whatever the walk reported.
                next_cursor = Some(cursor);
                complete = false;
                break;
            }
        }
    }
    // The walk finishing IS exhaustion, and then there is nothing to resume from.
    if complete {
        next_cursor = None;
    }
    let next_cursor = next_cursor.and_then(|plain| {
        seal.as_deref()
            .map(|key| crate::relay::search::seal_cursor(key, &plain))
    });

    // `complete` is the exhaustion authority, exactly as it is for `/window`: the
    // walk is bounded, so an empty page can mean "no more matches" OR "the budget
    // ran out first", and only the relay can tell a caller which. A client pages
    // until `complete` is true and never infers it from a short page.
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "events": out,
        "terms": terms,
        "complete": complete,
        "next_cursor": next_cursor,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// Resolve one event's access for `reader`, memoizing per channel for the
/// request (the feed touches many events in few channels).
pub(super) async fn feed_access(
    ctx: &AppContext,
    community: &str,
    reader: &crate::relay::event::PubKey,
    event: &Event,
    memo: &mut std::collections::BTreeMap<String, ChannelAccess>,
    now_ms: u64,
) -> bool {
    let channel = if crate::relay::kind::is_global_only_kind(event.kind) {
        None
    } else {
        event.channel_id()
    };
    let access = match channel {
        None => ChannelAccess::Global,
        Some(ch) => match memo.get(ch) {
            Some(a) => *a,
            None => {
                let a = resolve_access(ctx, community, ch, reader, now_ms).await;
                memo.insert(ch.to_string(), a);
                a
            }
        },
    };
    crate::relay::access::may_deliver(event, reader, access, true).is_allowed()
}

/// The mention-driven categories: walk the caller's own mention index (newest
/// first by construction), keep the category's kinds, re-authorize per event,
/// stop at `since` (the index order makes the cutoff a stop, not a filter),
/// bounded by [`FEED_MAX_PAGES`].
pub(super) async fn feed_mention_scan(
    ctx: &AppContext,
    community: &str,
    reader: &crate::relay::event::PubKey,
    kinds: &[u32],
    since: Option<u64>,
    limit: usize,
    now_ms: u64,
) -> Result<Vec<Event>> {
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let reader_hex = crate::relay::crypto::to_hex(reader);
    let mut memo = std::collections::BTreeMap::new();
    let mut out: Vec<Event> = Vec::with_capacity(limit);
    let mut cursor: Option<String> = None;
    for _ in 0..FEED_MAX_PAGES {
        let page = crate::relay::query::by_mention(
            &mut events,
            community,
            &reader_hex,
            cursor.as_deref(),
            FEED_MAX_LIMIT,
        )
        .await?;
        for event in &page.events {
            if since.is_some_and(|s| event.created_at < s) {
                return Ok(out); // newest-first: past the cutoff means done
            }
            if !kinds.contains(&event.kind) {
                continue;
            }
            if feed_access(ctx, community, reader, event, &mut memo, now_ms).await {
                out.push(event.clone());
                if out.len() == limit {
                    return Ok(out);
                }
            }
        }
        match page.next_cursor {
            Some(next) if !page.complete => cursor = Some(next),
            _ => break,
        }
    }
    Ok(out)
}

/// The activity category: one page per kind index, merged newest-first with
/// buzz's `created_at DESC, id ASC` order, re-authorized per event.
pub(super) async fn feed_activity_scan(
    ctx: &AppContext,
    community: &str,
    reader: &crate::relay::event::PubKey,
    kinds: &[u32],
    since: Option<u64>,
    limit: usize,
    now_ms: u64,
) -> Result<Vec<Event>> {
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let mut memo = std::collections::BTreeMap::new();
    let mut merged: Vec<Event> = Vec::new();
    for kind_ in kinds {
        let page =
            crate::relay::query::by_kind(&mut events, community, *kind_, None, FEED_MAX_LIMIT)
                .await?;
        for event in page.events {
            if since.is_some_and(|s| event.created_at < s) {
                break; // each kind index is newest-first
            }
            if feed_access(ctx, community, reader, &event, &mut memo, now_ms).await {
                merged.push(event);
            }
        }
    }
    merged.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    merged.truncate(limit);
    Ok(merged)
}

/// `POST /buzz/api/subscriptions` — open a live session.
pub(super) async fn subscribe(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let filters: Vec<crate::relay::filter::Filter> = if req.body.is_empty() {
        Vec::new()
    } else {
        serde_json::from_slice(&req.body)
            .map_err(|e| BuzzError::InvalidEvent(format!("filters: {e}")))?
    };
    // The bounds the socket face enforces on a REQ: the filter count it
    // advertises in NIP-11, and the per-filter value limit, since every live
    // event is matched against every value of every stored filter.
    if filters.len() > crate::relay::ws::MAX_FILTERS_PER_REQ {
        return Err(BuzzError::InvalidEvent(format!(
            "at most {} filters per subscription",
            crate::relay::ws::MAX_FILTERS_PER_REQ
        )));
    }
    for filter in &filters {
        crate::relay::ws::check_filter_bounds(filter).map_err(BuzzError::InvalidEvent)?;
    }

    let secret = stream_key(ctx, &mut config).await?;
    // Start at the feed HEAD, not 0. Offset 0 made every new SSE session replay
    // the entire feed before it reached a live event — a cost that grows without
    // bound as the community accumulates history, and the wrong semantics
    // besides: a subscriber asks for what happens NEXT, while history belongs to
    // the paged `/window` and `/query` routes. The ws face already did this
    // (`run_ws_pump`); only this route did not, so the two faces disagreed about
    // what "subscribe" means.
    let mut topics = planes::topics(ctx, store::FEED).map_err(storage)?;
    let head = feed_head(&mut topics, DEFAULT_COMMUNITY, FEED_GENERATION).await;
    let session = Session::open(
        &secret,
        &caller.pubkey,
        filters,
        &req.correlation_id,
        FEED_GENERATION,
        head,
        now_ms,
    );
    let token = session.mint_token(&secret);

    store::put_doc(&mut config, &keys::sess(community, &session.id), &session).await?;

    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "session": session.id,
        "token": token,
        "cursor": Cursor::new(session.cursor_generation, session.cursor_offset).encode(),
        "heartbeat_ms": HEARTBEAT_INTERVAL_MS,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}
