//! The SSE stream face.

use ferrite_app::FramePublish;
use ferrite_app::tracing;
use std::io;
use std::time::Duration;
use ferrite_app::serde_json;
use ferrite_app::rt;
use ferrite_app::AppContext;
use ferrite_app::IngressRequest;
use crate::relay::access::ChannelAccess;
use crate::relay::delivery;
use crate::relay::delivery::AccessCache;
use crate::relay::delivery::Frame;
use crate::relay::delivery::SeenIds;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::session::Cursor;
use crate::relay::session::Session;
use crate::relay::session::HEARTBEAT_INTERVAL_MS;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{ACCESS_TTL_MS, CLAIM_RETRIES, FEED_GENERATION, query_param, storage};
#[cfg(not(feature = "platform-topic-read-wait"))]
use super::released_feed_page;
use super::identity::{check_subscriber_standing, resolve_access, stream_key};

/// The SSE face: an open envelope starts a pump, a close stops it.
pub(super) async fn stream_loop(ctx: AppContext) -> io::Result<()> {
    let mut opens = ctx.queue(store::STREAM_REQUESTS)?;

    loop {
        let lease = match opens.poll_wait(5_000).await {
            Ok(Some(l)) => l,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("stream poll failed: {e}");
                rt::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let Ok(req) = serde_json::from_slice::<IngressRequest>(&lease.payload) else {
            let _ = opens.ack(lease.token).await;
            continue;
        };

        // A close is informational: the pump notices through the session's
        // generation and lease, so there is nothing to do but acknowledge.
        if req.method == "STREAM_CLOSE" {
            let _ = opens.ack(lease.token).await;
            continue;
        }

        // Ack the open IMMEDIATELY after claiming the session, not when the pump
        // ends: the open rides an at-least-once queue with a visibility timeout,
        // so holding it for a long-lived pump would supersede a healthy pump every
        // timeout and dead-letter the session after the attempt budget.
        let pump_ctx = ctx.clone();
        let claimed = claim_session(&pump_ctx, &req).await;
        let _ = opens.ack(lease.token).await;

        match claimed {
            Ok(Some((session, generation))) => {
                rt::spawn(async move {
                    if let Err(e) = run_pump(pump_ctx, req, session, generation).await {
                        tracing::info!("pump ended: {e}");
                    }
                });
            }
            Ok(None) => {}
            Err(e) => tracing::warn!("stream open refused: {e}"),
        }
    }
}

/// Authenticate a stream open and take the session's live-connection generation.
///
/// The generation bump is the single-flight mechanism: a replayed token or a
/// redelivered open supersedes the previous pump instead of running beside it.
pub(super) async fn claim_session(ctx: &AppContext, req: &IngressRequest) -> Result<Option<(Session, u64)>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;

    // The query comes off FIRST, then the last segment. The other order —
    // `rsplit('/')` and then a `?` strip — read the wrong segment for a target
    // whose query carries a `/`, and the token this route reads lives in that
    // query, so `/buzz/sub/s1?token=a/b` resolved a session id of `b`.
    let session_id = crate::http_query::path_only(&req.path)
        .rsplit('/')
        .next()
        .unwrap_or_default()
        .to_string();
    let token = query_param(&req.path, "token")
        .ok_or_else(|| BuzzError::Unauthorized("stream open needs a token".into()))?;

    let key = keys::sess(community, &session_id);
    let secret = stream_key(ctx, &mut config).await?;

    // The generation bump is the single-flight mechanism, so it has to be an
    // actual compare-and-swap. A read-modify-write here lets two concurrent opens
    // (a replayed stream URL, a flapping client, or one at-least-once redelivery
    // of the open envelope) both read N, both write N+1, and both spawn a pump —
    // two pumps on one session, each clobbering the other. Only the CAS winner
    // gets to serve.
    for _ in 0..CLAIM_RETRIES {
        let Some((mut session, version)) =
            store::get_doc_versioned::<Session>(&mut config, &key).await?
        else {
            return Err(BuzzError::NotFound("unknown session".into()));
        };
        if !session.verify_token(&secret, &token, now_ms) {
            return Err(BuzzError::Unauthorized("stream token is not valid".into()));
        }
        // Standing (ban, closed-community membership) is checked by the pump
        // before its first delivery, which answers with a revoked reset the
        // client can act on rather than a refused open.

        session.generation = session.generation.saturating_add(1);
        session.refresh_lease(now_ms);
        if let Some(from) = query_param(&req.path, "from").and_then(|c| Cursor::parse(&c)) {
            session.cursor_generation = from.generation;
            session.cursor_offset = from.offset;
        }
        match store::put_doc_if_version(&mut config, &key, &session, Some(version)).await? {
            Ok(_) => {
                let generation = session.generation;
                return Ok(Some((session, generation)));
            }
            // Another open moved the document between our read and our write. Retry
            // against the value it left.
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict(
        "session claim contended beyond the retry budget".into(),
    ))
}

/// Tail the feed for one connection.
pub(super) async fn run_pump(
    ctx: AppContext,
    req: IngressRequest,
    mut session: Session,
    generation: u64,
) -> Result<()> {
    let community = DEFAULT_COMMUNITY;
    let subscriber = session.subscriber()?;
    let mut frames = ctx.reply_to_queue(&req.reply_to).await.map_err(storage)?;
    let mut topics = planes::topics(&ctx, store::FEED).map_err(storage)?;
    let mut config = planes::store(&ctx, store::CONFIG).map_err(storage)?;
    let mut events_store = planes::store(&ctx, store::EVENTS).map_err(storage)?;

    let mut seen = SeenIds::new();
    let mut access = AccessCache::new(ACCESS_TTL_MS);
    // §6.3 rule (b): the last revocation-tombstone value seen for this
    // subscriber. A change means a ban or a membership removal committed —
    // possibly WITHOUT its feed record (a saga that crashed before the
    // append), so the cache is dropped and a live ban ends the session. This
    // is the crash-window backstop the feed-driven invalidation cannot cover.
    // (Seeded by the start-up standing check below.)
    // Resume from this generation's own cursor if one exists (a redelivered open
    // for the same generation), else from what the claim recorded.
    let mut cursor = match store::get_doc::<Cursor>(
        &mut config,
        &keys::sess_cursor(community, &session.id, generation),
    )
    .await
    {
        Ok(Some(c)) => c,
        _ => Cursor::new(session.cursor_generation, session.cursor_offset),
    };
    let (initial_tombstone, initial_revoked) = check_subscriber_standing(
        &mut events_store,
        &mut config,
        community,
        &subscriber,
        ctx.now_ms(),
    )
    .await?;
    if initial_revoked {
        send_frame(
            &mut frames,
            &req.correlation_id,
            &Frame::reset(crate::relay::session::ResetReason::Revoked, cursor),
        )
        .await?;
        return Ok(());
    }
    let mut last_revocation: Option<Vec<u8>> = initial_tombstone;
    // What the cursor row already holds, so a tick that delivered nothing writes
    // nothing. Starts at the resumed value: that IS what is stored.
    let mut persisted = cursor;
    let mut last_beat = ctx.now_ms();

    loop {
        let now_ms = ctx.now_ms();

        // Someone else opened this session: that pump is authoritative now.
        let stored =
            store::get_doc::<Session>(&mut config, &keys::sess(community, &session.id)).await?;
        match stored {
            Some(s) if s.generation != generation => return Ok(()),
            Some(s) => {
                if !s.token_is_live(now_ms) || !s.lease_is_live(now_ms) {
                    return Ok(());
                }
                session = s;
            }
            None => return Ok(()),
        }

        let topic = keys::feed_topic(community, cursor.generation);
        // A batch is held in memory from its read until it has been delivered, and the
        // whole process may hold only `delivery::MAX_HELD_BATCHES` of them, so the per-batch
        // worst case is not multiplied by the number of connections. The slot is taken
        // BEFORE the read in both builds (see `delivery::read_holding_slot`) and given back
        // on an empty page, so an idle connection's sleep holds nothing.
        #[cfg(not(feature = "platform-topic-read-wait"))]
        let (_batch_slot, read) = delivery::read_holding_slot(
            topics.read_from(&topic, cursor.offset, delivery::BATCH_LIMIT),
            |read| !matches!(read, Ok(records) if records.is_empty()),
        )
        .await;
        #[cfg(feature = "platform-topic-read-wait")]
        let (_batch_slot, read) = delivery::read_holding_slot(
            topics.read_wait(
                &topic,
                cursor.offset,
                delivery::BATCH_LIMIT,
                delivery::BATCH_BYTES,
                delivery::WAIT_MS,
            ),
            |read| matches!(read, Ok(page) if !page.page.records.is_empty()),
        )
        .await;
        #[cfg(not(feature = "platform-topic-read-wait"))]
        let read = released_feed_page(read, cursor.offset, |record| record.value.len()).await;
        #[cfg(feature = "platform-topic-read-wait")]
        let read = read.map(|page| {
            page.page.oversized.is_none().then(|| (page.page.records, page.retained_base))
        });
        let page = match read {
            Ok(Some(page)) => page,
            _ => {
                // The feed is unavailable, trimmed, or exceeds the page bound: resynchronize
                // rather than sit on a cursor that can never advance.
                send_frame(
                    &mut frames,
                    &req.correlation_id,
                    &Frame::reset(crate::relay::session::ResetReason::FeedRetired, cursor),
                )
                .await?;
                return Ok(());
            }
        };

        let (batch, retained_base) = page;
        let now_ms = ctx.now_ms();
        // Someone else opened this session: that pump is authoritative now.
        let stored =
            store::get_doc::<Session>(&mut config, &keys::sess(community, &session.id)).await?;
        match stored {
            Some(s) if s.generation != generation => return Ok(()),
            Some(s) => {
                if !s.token_is_live(now_ms) || !s.lease_is_live(now_ms) {
                    return Ok(());
                }
                session = s;
            }
            None => return Ok(()),
        }

        if let Some(reason) =
            delivery::check_wait_continuity(cursor, FEED_GENERATION, retained_base, batch.first().map(|r| r.offset))
        {
            send_frame(
                &mut frames,
                &req.correlation_id,
                &Frame::reset(reason, cursor),
            )
            .await?;
            return Ok(());
        }

        let empty = batch.is_empty();

        // §6.3 rule (b): ONE tombstone read per non-empty batch, BEFORE any
        // record is delivered. A changed value means a ban or a membership
        // removal committed for this subscriber — with or without its feed
        // record — so every cached access decision is stale; a live ban ends
        // the session outright (buzz evicts a banned member's live subs).
        // Fail-CLOSED on an unreadable tombstone: delivering on a read error
        // would reopen exactly the window this check exists to close.
        if !empty {
            let (tombstone, revoked) = check_subscriber_standing(
                &mut events_store,
                &mut config,
                community,
                &subscriber,
                now_ms,
            )
            .await?;
            if tombstone != last_revocation {
                last_revocation = tombstone;
                access.invalidate_all();
            }
            if revoked {
                send_frame(
                    &mut frames,
                    &req.correlation_id,
                    &Frame::reset(crate::relay::session::ResetReason::Revoked, cursor),
                )
                .await?;
                return Ok(());
            }
        }

        for record in &batch {
            // Consumed before it is decoded: a record that cannot be decoded still
            // moves the cursor, so a page of malformed records is not re-read
            // forever.
            let at = Cursor::new(cursor.generation, record.offset.saturating_add(1));
            let Ok(decoded) = delivery::FeedRecord::decode(&record.value) else {
                cursor = at;
                continue;
            };
            // Invalidation rides the feed: a membership or moderation record drops
            // the affected cache entry BEFORE anything else is delivered.
            if decoded.invalidates_access() {
                match decoded.channel.as_deref() {
                    Some(ch) => access.invalidate(ch),
                    None => access.invalidate_all(),
                }
            }
            let resolved = match decoded.channel.as_deref() {
                None => ChannelAccess::Global,
                Some(ch) => match access.get(ch, now_ms) {
                    Some(a) => a,
                    None => {
                        let a = resolve_access(&ctx, community, ch, &subscriber, now_ms).await;
                        if let ChannelAccess::Open = a {
                            access.put(ch, crate::relay::channel::Visibility::Open, true, now_ms);
                        } else if let ChannelAccess::Private { reader_is_member } = a {
                            access.put(
                                ch,
                                crate::relay::channel::Visibility::Private,
                                reader_is_member,
                                now_ms,
                            );
                        }
                        a
                    }
                },
            };
            if let Some(frame) = delivery::evaluate(
                &decoded,
                community,
                &subscriber,
                &session.filters,
                resolved,
                at,
                &mut seen,
            ) {
                send_frame(&mut frames, &req.correlation_id, &frame).await?;
                last_beat = now_ms;
            }
            cursor = at;
        }

        // The gateway keep-alives the socket on its own, so a client cannot tell a
        // quiet stream from a dead pump without this.
        if now_ms.saturating_sub(last_beat) >= HEARTBEAT_INTERVAL_MS {
            send_frame(
                &mut frames,
                &req.correlation_id,
                &Frame::heartbeat_at(cursor),
            )
            .await?;
            last_beat = now_ms;
        }

        // Persist the cursor under a key this pump OWNS, never by writing the
        // session document back. The pump's copy of that document carries its own
        // (possibly superseded) generation and lease, so a blind write-back would
        // revert a newer open's bump — handing the session to a pump whose client
        // has already reconnected, while the live connection silently receives
        // nothing. Keyed by generation so a superseded pump's last write cannot be
        // mistaken for the live pump's progress.
        // Persisted only when it MOVED. Writing this row every tick would make
        // an idle connection perform a replicated store write once a second for a
        // cursor that has not changed. On failure the in-memory copy is left
        // behind, so the next tick retries rather than losing the progress.
        if cursor != persisted {
            let key = keys::sess_cursor(community, &session.id, generation);
            if store::put_doc(&mut config, &key, &cursor).await.is_ok() {
                persisted = cursor;
            }
        }
        let _ = &mut events_store;

        // The next read parks at the topic front; no polling sleep is needed.
    }
}

/// Publish one frame to the connection that opened this pump.
pub(super) async fn send_frame(
    frames: &mut ferrite_app::client::QueueClusterClient,
    correlation_id: &str,
    frame: &Frame,
) -> Result<()> {
    // A classic SSE stream: the route's `sse` mode fixes the framing, so the
    // dynamic first-frame overrides are never set.
    let wire = ferrite_app::StreamFrame {
        correlation_id: correlation_id.to_string(),
        event: Some(frame.event_name().to_string()),
        data: frame.to_json()?.into_bytes(),
        done: false,
    };
    frames
        .publish_frame(&wire)
        .await
        .map(|_| ())
        .map_err(|e| BuzzError::Storage(format!("frame publish: {e}")))
}
