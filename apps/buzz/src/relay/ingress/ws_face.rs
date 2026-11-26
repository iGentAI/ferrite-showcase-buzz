//! The WebSocket face: envelope serving, per-connection workers, the NIP-01
//! messages and the pump behind a subscription.

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
use crate::relay::delivery::SeenIds;
use crate::relay::event::Event;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::session::Cursor;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{ACCESS_TTL_MS, CLAIM_RETRIES, FEED_GENERATION, query_param, storage};
#[cfg(not(feature = "platform-topic-read-wait"))]
use super::released_feed_page;
use super::events::{IngestOutcome, accept_event};
use super::channels::{derive_channel};
use super::identity::{check_subscriber_standing, relay_identity, resolve_access, stream_key};

/// How many index pages one REQ walks before it answers with what it has.
///
/// A filter can discard most of a page (a narrow kind set inside a busy
/// channel), so a walk needs more than one page to be useful — and an unbounded
/// one would turn a single REQ into an unbounded scan of the community.
pub(super) const WS_MAX_PAGES: usize = 4;

/// The relay's own public URL, when an operator configured it.
///
/// The gateway forwards no Host header, so this config value is the ONLY way the
/// app can know what a NIP-42 `relay` tag should say. Absent it, the tag must
/// still be a URL but its host cannot be checked — a deployment that wants the
/// full NIP-42 guarantee sets this.
pub(super) const RELAY_URL_CONFIG: &[u8] = b"buzz-relay-url";

/// How many WebSocket envelopes this instance holds at once — being served in a
/// per-connection worker, plus queued behind one.
///
/// Awaiting the whole of `handle_ws_message` INLINE between poll and ack makes
/// the face that the real client actually speaks serial across every connection
/// the deployment holds. Measured on the live deployment with an inline await: **96
/// connections × 4 REQs produced 0 of 384 EOSE frames and 0 EVENT frames in 210
/// s**, and an earlier 96 × 8 handshake took 60 060 ms. Relay RSS *fell* during
/// it — this is pure serialization, not memory and not CPU (the app sat at 1-2%
/// of its one-CPU budget).
///
/// The bound is a MEMORY bound, sized off the worst case rather than the typical
/// one: an inbound frame is up to [`crate::relay::ws::MAX_CLIENT_MESSAGE_BYTES`] (1 MiB,
/// the gateway's own ceiling) and a REQ being served holds a page of up to
/// [`crate::relay::ws::MAX_REQ_BYTES`] (512 KiB), so 32 is a bounded ~48 MiB against the
/// app's 512 MiB cgroup — the same shape of argument [`MAX_IN_FLIGHT`] makes for
/// the HTTP face, and the same number.
pub(super) const MAX_WS_IN_FLIGHT: usize = 32;

/// How long the ws loop parks waiting for a new lease before cycling.
///
/// The receive-side timeout binds especially hard here: an inbound frame may be a full
/// [`crate::relay::ws::MAX_CLIENT_MESSAGE_BYTES`] (1 MiB), so a budget that
/// cannot take delivery of one makes the app abandon its own frame mid-transfer
/// and wait for the queue to hand it back. 200 ms is short
/// enough to lose large frames. One second is long enough to receive a maximum
/// -size frame and still far below any lease TTL.
pub(super) const WS_ACK_POLL_MS: u64 = 1_000;

/// One leased WebSocket envelope, handed to its connection's worker.
///
/// The job owns its [`WsCompletion`] from the moment it is dispatched, so the
/// loop's slot comes back whether the job is served, or dropped while still
/// queued behind a worker that panicked or was dropped.
pub(super) struct WsJob {
    pub(super) req: IngressRequest,
    pub(super) token: u64,
    pub(super) completion: WsCompletion,
}

impl WsJob {
    pub(super) fn new(req: IngressRequest, token: u64, done: rt::mpsc::Sender<WsDone>) -> Self {
        let completion = WsCompletion {
            tx: done,
            cid: req.correlation_id.clone(),
            ack: None,
        };
        Self {
            req,
            token,
            completion,
        }
    }
}

/// The ack for a finished `WS_OPEN`: a transient failure (a lost claim race, a
/// plane failure) stays unacked so the queue re-drives the open; success and a
/// terminal refusal, which has already been answered on the socket, are acked.
pub(super) fn ws_open_ack(token: u64, opened: &Result<u64>) -> Option<u64> {
    match opened {
        Err(e) if crate::relay::ws::frame_should_redeliver(e) => None,
        _ => Some(token),
    }
}

/// The ack for a finished `WS_CLOSE`: only a completed cleanup is acked, so a
/// failed delete is re-driven instead of leaving an active subscription.
pub(super) fn ws_close_ack(token: u64, closed: &Result<()>) -> Option<u64> {
    closed.as_ref().ok().map(|()| token)
}

/// A finished envelope, reported back to the ws loop.
pub(super) struct WsDone {
    /// The connection the job belonged to — the key of the worker whose slot is
    /// being returned.
    pub(super) cid: String,
    /// `Some(token)` once the work is durable and its answer is published;
    /// `None` leaves the envelope unacked so the queue re-drives it.
    pub(super) ack: Option<u64>,
}

/// Reports a finished envelope to the ws loop EXACTLY once, even on unwind.
///
/// The same discipline [`Completion`] enforces for the HTTP face, and it is
/// load-bearing for the same reason: the loop decrements its counters only when
/// a completion arrives, so a worker that ended without reporting would leak a
/// slot, and after [`MAX_WS_IN_FLIGHT`] leaks the loop stops taking work
/// forever. Reporting from `Drop` makes the decrement unconditional — including
/// on the path where the whole worker task is dropped mid-frame, where the
/// `None` ack is exactly right (nothing was published, so redelivery is the
/// retry).
///
/// `try_send` cannot lose a report: the channel's capacity is
/// [`MAX_WS_IN_FLIGHT`], every dispatched job produces exactly one `WsDone`, and
/// the loop never dispatches more than `MAX_WS_IN_FLIGHT` jobs at a time.
pub(super) struct WsCompletion {
    pub(super) tx: rt::mpsc::Sender<WsDone>,
    pub(super) cid: String,
    pub(super) ack: Option<u64>,
}

impl Drop for WsCompletion {
    fn drop(&mut self) {
        let _ = self.tx.try_send(WsDone {
            cid: core::mem::take(&mut self.cid),
            ack: self.ack,
        });
    }
}

/// One connection's serial worker, as the loop sees it.
pub(super) struct WsWorker {
    pub(super) jobs: rt::mpsc::Sender<WsJob>,
    /// Envelopes dispatched to this worker that have not reported back. The
    /// entry is dropped (and with it the sender, which is how the worker is told
    /// to exit) the moment this reaches zero.
    pub(super) outstanding: usize,
}

/// The NIP-01 + NIP-42 WebSocket face: lease, dispatch to the connection's own
/// worker, ack what comes back.
///
/// # The ordering guarantee
///
/// **Per connection, strictly serial in queue-delivery order; across
/// connections, concurrent up to [`MAX_WS_IN_FLIGHT`].**
///
/// Every envelope is dispatched to a worker keyed by its `correlation_id` — the
/// gateway's per-socket handle — and a worker takes its next job only after the
/// previous one has finished. So a socket's `WS_OPEN`, its frames, and its
/// `WS_CLOSE` are handled in the order the queue handed them over; a *different*
/// socket's frames do not wait behind them.
///
/// That is a stronger guarantee than a flat worker pool, and the strength is
/// needed. Three orderings on one socket are not interchangeable:
///
/// * **NIP-42 `AUTH` before an `EVENT` from the same socket.** A flat pool could
///   serve the `EVENT` against a pre-`AUTH` snapshot of the connection document.
///   The failure is fail-CLOSED (`auth-required`, the client re-publishes) rather
///   than a bypass — `bind` only ever adds authority, and the pubkey it binds is
///   the one the challenge proved — but a client that pipelines would see its
///   first message spuriously refused.
/// * **`REQ` and `CLOSE` for the same `subId`.** These are a read-modify-write on
///   the same subscription table. Reordered, a `CLOSE` can be overwritten by a
///   `REQ` that read the table before it, leaving a subscription the client
///   believes it closed still matching on the pump.
/// * **Two `REQ`s on one socket.** Both are compare-and-swaps against one
///   document. Concurrently they are *correct* — `update_ws_conn` re-applies
///   against the winner's value — but they are also contention, and
///   [`CLAIM_RETRIES`] is finite: enough concurrent REQs on one connection turn a
///   correct-but-slow write into a `Conflict`, which is retryable, so the frame
///   redelivers and burns its attempt budget. Serializing them removes the
///   contention rather than surviving it.
///
/// What this guarantee does NOT cover, stated plainly: several app instances
/// competing-consume one `ws-requests` queue, so two frames of the same
/// connection can be leased by two instances and handled at the same time. The
/// connection document's CAS is what makes it safe, and the three orderings above degrade
/// to the fail-closed / lost-CLOSE / contention behaviour described. A
/// cross-instance ordering guarantee would need the gateway to shard frames by
/// correlation id onto per-connection queues; it does not.
///
/// # The two ack disciplines
///
/// Both are forced by the queue's at-least-once contract:
///
/// * `WS_OPEN` is acked as soon as the connection is claimed, never when its
///   pump ends — an open held for the life of a long pump would time out its
///   visibility, be redelivered (superseding a healthy pump every timeout), and
///   eventually dead-letter the connection.
/// * a `WS_MESSAGE` is acked only once its work is durable and its answer is
///   published, because redelivery is the only retry an unfinished frame has.
///
/// The ack itself comes back to THIS task rather than happening in the worker,
/// because a lease's fencing token must be acked by the client that took the
/// lease. Everything else — the store work and, critically, the frame publishes
/// — happens in the worker, so a finished answer never waits on this loop's poll
/// cycle. (That last point is not an optimisation. With the reply publish on the
/// loop, an HTTP-face caller at concurrency 1 measured ~522 ms instead of ~17 ms,
/// woken only by the gateway's 500 ms hedge.)
pub(super) async fn ws_loop(ctx: AppContext) -> io::Result<()> {
    let mut requests = ctx.queue(store::WS_REQUESTS)?;
    let (done_tx, mut done_rx) = rt::mpsc::channel::<WsDone>(MAX_WS_IN_FLIGHT);
    let mut workers: std::collections::HashMap<String, WsWorker> = std::collections::HashMap::new();
    let mut in_flight = 0usize;

    loop {
        // Ack everything that finished while we were away.
        while let Ok(done) = done_rx.try_recv() {
            retire_ws_job(&mut requests, &mut workers, &mut in_flight, done).await;
        }
        // At the ceiling the only way to make progress is to wait for a
        // completion — never to take another lease we cannot serve.
        if in_flight >= MAX_WS_IN_FLIGHT {
            match done_rx.recv().await {
                Some(done) => {
                    retire_ws_job(&mut requests, &mut workers, &mut in_flight, done).await;
                }
                // The channel cannot close while this task holds `done_tx`.
                None => return Ok(()),
            }
            continue;
        }

        let lease = match requests.poll_wait(WS_ACK_POLL_MS).await {
            Ok(Some(l)) => l,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("ws poll failed: {e}");
                rt::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let Ok(req) = serde_json::from_slice::<IngressRequest>(&lease.payload) else {
            // Not an ingress envelope: nothing can ever handle it (the poison
            // rule the api loop applies).
            let _ = requests.ack(lease.token).await;
            continue;
        };
        // An envelope this face has no verb for carries no connection ordering,
        // so it is answered here rather than spun up a worker for.
        if !matches!(req.method.as_str(), "WS_OPEN" | "WS_MESSAGE" | "WS_CLOSE") {
            tracing::warn!("the ws face ignored a {} envelope", req.method);
            let _ = requests.ack(lease.token).await;
            continue;
        }

        let cid = req.correlation_id.clone();
        let token = lease.token;
        let dispatched = {
            let worker = workers.entry(cid.clone()).or_insert_with(|| {
                let (jobs_tx, jobs_rx) = rt::mpsc::channel::<WsJob>(MAX_WS_IN_FLIGHT);
                let worker_ctx = ctx.clone();
                rt::spawn(async move {
                    ws_connection_worker(worker_ctx, jobs_rx).await;
                });
                WsWorker {
                    jobs: jobs_tx,
                    outstanding: 0,
                }
            });
            worker.outstanding = worker.outstanding.saturating_add(1);
            // Cannot fail: the channel holds `MAX_WS_IN_FLIGHT` and this
            // connection's share of the global budget is at most that. If it
            // does, the refused job is dropped here and its completion reports
            // the slot back, so the count below stays honest either way.
            worker
                .jobs
                .try_send(WsJob::new(req, token, done_tx.clone()))
                .is_ok()
        };
        in_flight = in_flight.saturating_add(1);
        if !dispatched {
            tracing::error!("ws worker queue refused a job; leaving it unacked");
        }
    }
}

/// Ack a finished envelope and return its connection's slot.
///
/// Dropping the map entry drops the worker's job sender, which is how the task
/// is told to exit. A later frame for the same connection spawns a fresh worker,
/// and ordering survives that: `outstanding == 0` means every earlier frame on
/// this connection has already reported completion, so there is nothing left for
/// the new worker to race.
pub(super) async fn retire_ws_job(
    requests: &mut ferrite_app::client::QueueClusterClient,
    workers: &mut std::collections::HashMap<String, WsWorker>,
    in_flight: &mut usize,
    done: WsDone,
) {
    if let Some(token) = done.ack {
        let _ = requests.ack(token).await;
    }
    *in_flight = in_flight.saturating_sub(1);
    release_ws_slot(workers, &done.cid);
}

/// Decrement a connection's outstanding count, retiring its worker at zero.
pub(super) fn release_ws_slot(workers: &mut std::collections::HashMap<String, WsWorker>, cid: &str) {
    let drained = match workers.get_mut(cid) {
        Some(worker) => {
            worker.outstanding = worker.outstanding.saturating_sub(1);
            worker.outstanding == 0
        }
        None => false,
    };
    if drained {
        workers.remove(cid);
    }
}

/// Serve one connection's envelopes, in order, until the loop retires it.
///
/// The worker holds its plane clients for the life of the burst. That is not
/// tidiness: `ctx.store` and `ctx.reply_to_queue` MINT a client — and with it a
/// fresh connection — per call, which would be two per inbound
/// frame. The frame queue is the worse of the two, because `reply_to_queue` only
/// has a fast path for a group that is an INJECTED service, and the group the
/// gateway names is its own: this app's manifest declares no `frames` binding,
/// so unless the deployment's materializer injects one, every frame would pay a
/// conductor connect plus a `group_members` lookup before it could answer.
pub(super) async fn ws_connection_worker(ctx: AppContext, mut jobs: rt::mpsc::Receiver<WsJob>) {
    let mut config: Option<planes::Pooled<ferrite_app::client::StoreClusterClient>> = None;
    let mut frames: Option<(String, Box<dyn FrameQueue>)> = None;

    while let Some(mut job) = jobs.recv().await {
        // The job carries its completion from dispatch, so whatever happens from
        // here — including this task being dropped mid-frame — the loop hears
        // about it exactly once and the slot comes back.
        let ack = serve_ws_envelope(&ctx, &mut config, &mut frames, &job).await;
        job.completion.ack = ack;
    }
}

/// The frame queue a connection worker keeps for the life of its burst.
///
/// `AppContext::reply_to_queue` answers a bare `QueueClusterClient` on the
/// released platform builder and a pooled `ReplyQueue` (which derefs to one) in
/// the packaged SDK; the released client has no `into_inner`. Holding the
/// handle as it was returned, behind this adapter, compiles against both and
/// keeps the pooled handle's return-to-pool on drop.
pub(super) trait FrameQueue: Send {
    /// The queue client frames are published on.
    fn queue(&mut self) -> &mut ferrite_app::client::QueueClusterClient;
}

/// A reply handle of whatever type the SDK returned, with its accessor.
pub(super) struct HeldFrameQueue<H> {
    pub(super) handle: H,
    pub(super) get: fn(&mut H) -> &mut ferrite_app::client::QueueClusterClient,
}

impl<H: Send> FrameQueue for HeldFrameQueue<H> {
    fn queue(&mut self) -> &mut ferrite_app::client::QueueClusterClient {
        (self.get)(&mut self.handle)
    }
}

/// Hold a reply handle; `get` is `|h| h`, whose return coerces through `Deref`
/// when the handle is a pooled wrapper and is the identity when it is the client.
pub(super) fn hold_frame_queue<H: Send + 'static>(
    handle: H,
    get: fn(&mut H) -> &mut ferrite_app::client::QueueClusterClient,
) -> Box<dyn FrameQueue> {
    Box::new(HeldFrameQueue { handle, get })
}

/// Handle one envelope and return the lease token to ack, if it may be acked.
pub(super) async fn serve_ws_envelope(
    ctx: &AppContext,
    config: &mut Option<planes::Pooled<ferrite_app::client::StoreClusterClient>>,
    frames: &mut Option<(String, Box<dyn FrameQueue>)>,
    job: &WsJob,
) -> Option<u64> {
    let req = &job.req;
    // The frame client is keyed by the envelope's `reply_to`: a redelivered open
    // may be relayed by a DIFFERENT gateway instance, whose frame group is its
    // own, and a cached client would publish this connection's frames where
    // nobody is reading them.
    let stale = match frames.as_ref() {
        Some((group, _)) => group != &req.reply_to,
        None => true,
    };
    if stale {
        match ctx.reply_to_queue(&req.reply_to).await {
            Ok(client) => *frames = Some((req.reply_to.clone(), hold_frame_queue(client, |h| h))),
            Err(e) => {
                // Nothing can be said on a socket we cannot reach; redelivery
                // is the retry.
                tracing::warn!("ws frame queue unavailable: {e}");
                return None;
            }
        }
    }
    if config.is_none() {
        match planes::store(ctx, store::CONFIG) {
            Ok(client) => *config = Some(client),
            Err(e) => {
                tracing::warn!("ws config binding unavailable: {e}");
                return None;
            }
        }
    }
    let (Some((_, frames)), Some(config)) = (frames.as_mut(), config.as_mut()) else {
        // Unreachable: both were just filled in above.
        return None;
    };
    let frames = frames.queue();

    match req.method.as_str() {
        "WS_OPEN" => {
            let opened = open_ws_connection(ctx, config, frames, req).await;
            match &opened {
                Ok(generation) => {
                    let generation = *generation;
                    let pump_ctx = ctx.clone();
                    let pump_req = req.clone();
                    rt::spawn(async move {
                        if let Err(e) = run_ws_pump(pump_ctx, pump_req, generation).await {
                            tracing::info!("ws pump ended: {e}");
                        }
                    });
                }
                Err(e) => tracing::warn!("ws open refused: {e}"),
            }
            // Acked once the connection is CLAIMED — never held for the pump.
            ws_open_ack(job.token, &opened)
        }
        "WS_MESSAGE" => match handle_ws_message(ctx, config, frames, req).await {
            Ok(()) => Some(job.token),
            // A TRANSIENT failure (a lost race, a plane hiccup, a quota) is
            // deliberately UNACKED: the frame's work is not durable, so
            // redelivery is its retry, bounded by the queue's own attempt
            // budget rather than by a loop here.
            Err(e) if crate::relay::ws::frame_should_redeliver(&e) => {
                tracing::warn!("ws frame failed, will redeliver: {e}");
                None
            }
            // A TERMINAL failure cannot succeed on a retry, so redelivering it
            // only burns the envelope's attempt budget and leaves the client
            // waiting for an answer that will never come. Say so on the socket
            // and acknowledge (F2's second half — see
            // `ws::frame_should_redeliver` for the reachability argument).
            Err(e) => {
                tracing::warn!("ws frame failed terminally: {e}");
                let notice = crate::relay::ws::RelayMessage::Notice {
                    message: e.to_string(),
                };
                let _ = send_ws(frames, &req.correlation_id, &notice).await;
                Some(job.token)
            }
        },
        "WS_CLOSE" => {
            let closed = close_ws_connection(config, req).await;
            if let Err(e) = &closed {
                tracing::warn!("ws close cleanup failed, will redeliver: {e}");
            }
            ws_close_ack(job.token, &closed)
        }
        // The loop dispatches only the three verbs above; this is the arm that
        // keeps that true if one of them is ever renamed in one place only.
        other => {
            tracing::warn!("the ws face ignored a {other} envelope");
            Some(job.token)
        }
    }
}

/// `POST /buzz/api/ws-ticket` — a short-lived ticket that admits one socket open.
///
/// Nothing stands in front of this since the demonstration opened: the ticket
/// proves only that the socket's opener came through the HTTP face, and NIP-42
/// on the socket still proves the workspace identity.
pub(super) async fn ws_ticket(ctx: &AppContext) -> Result<Vec<u8>> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let secret = stream_key(ctx, &mut config).await?;
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "ticket": crate::relay::ws::mint_ticket(&secret, ctx.now_ms()),
        "expires_in_ms": crate::relay::ws::TICKET_TTL_MS,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// Claim a connection and issue its NIP-42 challenge.
///
/// The generation bump is the single-flight mechanism, exactly as
/// [`claim_session`] does it for the SSE face: a redelivered open supersedes the
/// previous pump instead of running a second one beside it.
///
/// # The ticket boundary
///
/// Every HTTP envelope passes the Ferrite SSO gate in `api_loop`, which is now
/// the open gate: it admits an anonymous visitor. This face receives no headers
/// from the gateway, so it cannot run that gate itself. Instead a NEW
/// connection must present a live `ticket` query parameter, which only the HTTP
/// face mints ([`ws_ticket`]). Without one the socket is told
/// why and hung up before any connection document exists, so every later frame
/// on it meets "this connection is not open" and nothing is read or written. A
/// redelivered open for a connection already admitted is not asked again: its
/// document exists only because an earlier open passed this check, and the
/// ticket may have expired while the envelope waited.
pub(super) async fn open_ws_connection(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    frames: &mut ferrite_app::client::QueueClusterClient,
    req: &IngressRequest,
) -> Result<u64> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;

    // Without the sealed key there is no unforgeable challenge, so the socket is
    // hung up rather than served unauthenticated.
    let secret = match stream_key(ctx, config).await {
        Ok(secret) => secret,
        Err(e) => {
            let notice = crate::relay::ws::RelayMessage::Notice {
                message: e.to_string(),
            };
            send_ws(frames, &req.correlation_id, &notice).await?;
            close_ws_socket(frames, &req.correlation_id).await?;
            return Err(e);
        }
    };

    let key = keys::wsconn(community, &req.correlation_id);
    let ticketed = query_param(&req.path, "ticket")
        .is_some_and(|ticket| crate::relay::ws::verify_ticket(&secret, &ticket, now_ms));
    if !ticketed
        && store::get_doc_versioned::<crate::relay::ws::Connection>(config, &key)
            .await?
            .is_none()
    {
        let refusal = BuzzError::Unauthorized(
            "this socket needs a ticket: POST /buzz/api/ws-ticket and connect with ?ticket=<ticket>"
                .into(),
        );
        let notice = crate::relay::ws::RelayMessage::Notice {
            message: refusal.to_string(),
        };
        send_ws(frames, &req.correlation_id, &notice).await?;
        close_ws_socket(frames, &req.correlation_id).await?;
        return Err(refusal);
    }
    let opened =
        crate::relay::ws::Connection::open(&secret, &req.correlation_id, &req.reply_to, now_ms);
    let mut conn = None;
    for _ in 0..CLAIM_RETRIES {
        let existing =
            store::get_doc_versioned::<crate::relay::ws::Connection>(config, &key).await?;
        let (next, version) = match existing {
            None => (opened.clone(), None),
            Some((mut held, version)) => {
                held.generation = held.generation.saturating_add(1);
                // A redelivered open may be relayed by a DIFFERENT gateway
                // instance, whose frame queue group is its own; the newest open
                // names where this connection's frames must now go.
                held.reply_to = req.reply_to.clone();
                held.touch(now_ms);
                (held, Some(version))
            }
        };
        let expires_at = next.expires_at_ms;
        match store::put_expiring_doc_if_version(config, &key, &next, version, expires_at).await? {
            Ok(_) => {
                conn = Some(next);
                break;
            }
            // Another open moved the document between our read and our write:
            // retry against the value it left.
            Err(_) => continue,
        }
    }
    let Some(conn) = conn else {
        return Err(BuzzError::Conflict(
            "ws connection claim contended beyond the retry budget".into(),
        ));
    };

    let challenge = crate::relay::ws::RelayMessage::Auth {
        challenge: conn.challenge.clone(),
    };
    send_ws(frames, &req.correlation_id, &challenge).await?;
    Ok(conn.generation)
}

/// The client vanished: drop the connection document so its pump exits and a
/// replayed frame cannot resume the subscription table.
pub(super) async fn close_ws_connection(
    config: &mut ferrite_app::client::StoreClusterClient,
    req: &IngressRequest,
) -> Result<()> {
    let community = DEFAULT_COMMUNITY;
    let _ = store::delete_key(config, &keys::wsconn_cursor(community, &req.correlation_id)).await;
    store::delete_key(config, &keys::wsconn(community, &req.correlation_id))
        .await
        .map(|_| ())
}

/// Handle one inbound frame.
///
/// The connection document is read ONCE here and its version travels with it
/// into [`update_ws_conn`] as a CAS seed. Reading it again inside the
/// compare-and-swap would be a whole store round trip per `AUTH`, `REQ` and
/// `CLOSE` frame for a value this function is already holding. A seed cannot be
/// wrong in a way the CAS does not catch: if the document moved between the read
/// and the write, the version check fails and the retry re-reads.
pub(super) async fn handle_ws_message(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    frames: &mut ferrite_app::client::QueueClusterClient,
    req: &IngressRequest,
) -> Result<()> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let cid = req.correlation_id.as_str();

    // Parse FIRST: a protocol error needs no connection state, and answering it
    // is what stops a confused client from retrying the same bad frame forever.
    let message = match crate::relay::ws::ClientMessage::parse(&req.body) {
        Ok(message) => message,
        Err(e) => {
            let notice = crate::relay::ws::RelayMessage::Notice {
                message: e.to_string(),
            };
            return send_ws(frames, cid, &notice).await;
        }
    };

    let key = keys::wsconn(community, cid);
    let Some((conn, version)) =
        store::get_doc_versioned::<crate::relay::ws::Connection>(config, &key).await?
    else {
        // A frame for a connection this deployment never opened, or one it has
        // already closed. Say so instead of dropping it in silence.
        let notice = crate::relay::ws::RelayMessage::Notice {
            message: "error: this connection is not open; reconnect".into(),
        };
        return send_ws(frames, cid, &notice).await;
    };
    if !conn.is_live(now_ms) {
        let notice = crate::relay::ws::RelayMessage::Notice {
            message: "error: this connection expired; reconnect".into(),
        };
        send_ws(frames, cid, &notice).await?;
        return close_ws_socket(frames, cid).await;
    }
    // Read off the pre-frame snapshot, because the handlers below CONSUME it as
    // the CAS seed.
    let needs_touch = conn.needs_touch(now_ms);

    let outcome = match message {
        crate::relay::ws::ClientMessage::Auth(event) => {
            ws_auth(ctx, config, frames, cid, &key, conn, version, &event, now_ms).await
        }
        crate::relay::ws::ClientMessage::Req { sub_id, filters } => {
            ws_req(
                ctx, config, frames, cid, &key, conn, version, &sub_id, &filters, now_ms,
            )
            .await
        }
        crate::relay::ws::ClientMessage::Close { sub_id } => {
            ws_close_subscription(config, &key, conn, version, &sub_id, now_ms).await
        }
        crate::relay::ws::ClientMessage::Event(event) => {
            ws_event(ctx, frames, cid, &conn, &event, now_ms).await
        }
        crate::relay::ws::ClientMessage::RefusedReq { sub_id, reason } => {
            // A NAMED refusal. The client learns which subscription died and
            // why, instead of a NOTICE it cannot attribute to anything. Nothing
            // was opened on this path, so there is no document write to undo and
            // no subscription slot held.
            send_ws(
                frames,
                cid,
                &crate::relay::ws::RelayMessage::closed(&sub_id, reason),
            )
            .await
        }
    };

    // Keep a busy-but-quiet connection's document alive without paying a store
    // write per frame (a REQ/CLOSE/AUTH already wrote and refreshed it). No
    // seed: whatever the handler above did, the snapshot is now stale.
    if needs_touch {
        let _ = update_ws_conn(config, &key, now_ms, None, |_| true).await;
    }
    outcome
}

/// `["AUTH", event]` — the NIP-42 response.
///
/// Both outcomes answer with an `OK` for the authentication event's own id,
/// because that is what the real client's connect promise resolves on; a failure
/// also carries a `NOTICE`, which is what shows up in an operator's log.
#[allow(clippy::too_many_arguments)]
pub(super) async fn ws_auth(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    frames: &mut ferrite_app::client::QueueClusterClient,
    cid: &str,
    key: &[u8],
    conn: crate::relay::ws::Connection,
    version: u64,
    event: &Event,
    now_ms: u64,
) -> Result<()> {
    let relay_url = configured_relay_url(config).await;
    let verified = match crate::relay::ws::verify_auth(&conn.challenge, event, relay_url.as_deref(), now_ms) {
        Ok(pubkey) => match super::community::enforce_read_standing(ctx, DEFAULT_COMMUNITY, &pubkey, now_ms).await {
            Ok(()) => Ok(pubkey),
            Err(e) => Err(e),
        },
        Err(e) => Err(e),
    };
    match verified {
        Ok(pubkey) => {
            update_ws_conn(config, key, now_ms, Some((conn, version)), |c| {
                c.bind(&pubkey);
                true
            })
            .await?;
            send_ws(
                frames,
                cid,
                &crate::relay::ws::RelayMessage::accepted(&event.id),
            )
            .await
        }
        Err(e) => {
            send_ws(
                frames,
                cid,
                &crate::relay::ws::RelayMessage::refused(&event.id, &e),
            )
            .await?;
            let notice = crate::relay::ws::RelayMessage::Notice {
                message: e.to_string(),
            };
            send_ws(frames, cid, &notice).await
        }
    }
}

/// `["REQ", subId, filter]` — authorize, register, serve the stored page, EOSE.
#[allow(clippy::too_many_arguments)]
pub(super) async fn ws_req(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    frames: &mut ferrite_app::client::QueueClusterClient,
    cid: &str,
    key: &[u8],
    conn: crate::relay::ws::Connection,
    version: u64,
    sub_id: &str,
    filters: &[crate::relay::filter::Filter],
    now_ms: u64,
) -> Result<()> {
    let community = DEFAULT_COMMUNITY;
    let reader = conn.reader();
    let authenticated = conn.authenticated().is_some();

    if let Some(pubkey) = conn.authenticated() {
        match super::community::enforce_read_standing(ctx, community, &pubkey, now_ms).await {
            Ok(()) => {}
            Err(e @ BuzzError::Unauthorized(_)) => {
                let closed = crate::relay::ws::RelayMessage::closed(sub_id, e.to_string());
                return send_ws(frames, cid, &closed).await;
            }
            Err(e) => return Err(e),
        }
    } else if super::community::community_is_closed(config).await? {
        let closed = crate::relay::ws::RelayMessage::closed(
            sub_id,
            "auth-required: authenticate before reading this invite-only community",
        );
        return send_ws(frames, cid, &closed).await;
    }

    // Authorize every filter BEFORE the subscription exists. buzz refuses the
    // subscription rather than silently returning nothing, so a client learns it
    // asked for something it may not have — and an empty EOSE cannot be mistaken
    // for an empty channel.
    for filter in filters {
        if !authenticated && crate::relay::ws::needs_authentication(filter) {
            let closed = crate::relay::ws::RelayMessage::closed(
                sub_id,
                "auth-required: authenticate before reading these kinds",
            );
            return send_ws(frames, cid, &closed).await;
        }
        if !crate::relay::access::filter_authorized(
            filter.kinds.as_deref(),
            filter.authors.as_deref(),
            filter.tag_filter("#p"),
            &reader,
        ) {
            let closed = crate::relay::ws::RelayMessage::closed(
                sub_id,
                "restricted: this filter asks for events you may not read",
            );
            return send_ws(frames, cid, &closed).await;
        }
    }

    // Register BEFORE serving history: a subscription that only came into
    // existence at EOSE would miss every event published while the stored page
    // was being read, and nothing would ever resend it.
    //
    // The table refuses on TWO bounds — the subscription count and the
    // serialized byte budget (`ws::MAX_SUBSCRIPTION_TABLE_BYTES`, which keeps
    // the durable connection document writable at all) — and the refusal
    // carries the bound it hit, because the client classifies the message.
    let mut refusal: Option<String> = None;
    let registered = update_ws_conn(config, key, now_ms, Some((conn, version)), |c| {
        match c.upsert_subscription(sub_id.to_string(), filters.to_vec()) {
            Ok(()) => true,
            Err(why) => {
                refusal = Some(why);
                false
            }
        }
    })
    .await?;
    if let Some(why) = refusal {
        let closed = crate::relay::ws::RelayMessage::closed(sub_id, why);
        return send_ws(frames, cid, &closed).await;
    }
    if registered.is_none() {
        return Ok(()); // the connection closed under us
    }

    // One page per filter, deduplicated across them: a client that batches two
    // overlapping filters into one REQ must not receive an event twice.
    //
    // ONE events client for the whole REQ. Minting it inside
    // `ws_stored_events`, which runs once per filter, would make a REQ carrying the
    // eight filters the relay advertises pay eight client mints, and eight
    // connections, to read eight pages.
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let mut sent = std::collections::HashSet::new();
    for filter in filters {
        for event in ws_stored_events(ctx, &mut events, community, &reader, filter, now_ms).await? {
            if !sent.insert(event.id) {
                continue;
            }
            let frame = crate::relay::ws::RelayMessage::Event {
                sub_id: sub_id.to_string(),
                event: Box::new(event),
            };
            send_ws(frames, cid, &frame).await?;
        }
    }
    let eose = crate::relay::ws::RelayMessage::Eose {
        sub_id: sub_id.to_string(),
    };
    send_ws(frames, cid, &eose).await
}

/// `["CLOSE", subId]` — drop the subscription. NIP-01 answers nothing, and
/// closing one that is already gone is not an error (the client closes
/// subscriptions the relay may itself have `CLOSED`).
pub(super) async fn ws_close_subscription(
    config: &mut ferrite_app::client::StoreClusterClient,
    key: &[u8],
    conn: crate::relay::ws::Connection,
    version: u64,
    sub_id: &str,
    now_ms: u64,
) -> Result<()> {
    update_ws_conn(config, key, now_ms, Some((conn, version)), |c| {
        c.remove_subscription(sub_id)
    })
    .await
    .map(|_| ())
}

/// `["EVENT", event]` — publish through the same chain a POST to
/// `/buzz/api/events` runs, and answer with the `OK` the client waits on.
pub(super) async fn ws_event(
    ctx: &AppContext,
    frames: &mut ferrite_app::client::QueueClusterClient,
    cid: &str,
    conn: &crate::relay::ws::Connection,
    event: &Event,
    now_ms: u64,
) -> Result<()> {
    let Some(caller) = conn.authenticated() else {
        // NIP-42's own answer: the client authenticates and republishes.
        let refusal = crate::relay::ws::RelayMessage::Ok {
            event_id: crate::relay::crypto::to_hex(&event.id),
            accepted: false,
            message: "auth-required: authenticate before publishing".into(),
        };
        return send_ws(frames, cid, &refusal).await;
    };
    let reply = match accept_event(ctx, &caller, event, now_ms).await {
        Ok(IngestOutcome::Accepted {
            duplicate: false, ..
        }) => crate::relay::ws::RelayMessage::accepted(&event.id),
        // A duplicate is a SUCCESS for the client's intent, reported honestly —
        // the same answer the HTTP face gives.
        Ok(IngestOutcome::Accepted { .. }) => crate::relay::ws::RelayMessage::Ok {
            event_id: crate::relay::crypto::to_hex(&event.id),
            accepted: true,
            message: "duplicate: this event was already accepted".into(),
        },
        Ok(IngestOutcome::ReactionDuplicate { .. }) => crate::relay::ws::RelayMessage::Ok {
            event_id: crate::relay::crypto::to_hex(&event.id),
            accepted: true,
            message: "duplicate: reaction already exists".into(),
        },
        // Every refusal reason is the relay's own wire vocabulary ("invalid:",
        // "restricted:", "rate-limited:", "error:"), which is exactly what the
        // client branches on.
        Err(e) => crate::relay::ws::RelayMessage::refused(&event.id, &e),
    };
    send_ws(frames, cid, &reply).await
}

/// Read-modify-write one connection document under a compare-and-swap.
///
/// `apply` returns whether a write is needed, and is re-run on a lost CAS
/// against the value the winner left — never against a stale copy. `Ok(None)`
/// means the connection is gone (a `WS_CLOSE` landed mid-frame), which every
/// caller treats as "there is nobody to answer".
///
/// `seed` is a `(document, version)` pair the caller has ALREADY read, used for
/// the first attempt so the common path costs one store round trip instead of
/// two. It is an optimisation with no correctness surface of its own: a seed
/// that has gone stale loses its CAS exactly as a self-read one would, and the
/// retry re-reads. A seeded first attempt also cannot mistake a deleted document
/// for a live one — the version check fails, the retry reads `None`, and the
/// caller gets `Ok(None)`.
pub(super) async fn update_ws_conn(
    config: &mut ferrite_app::client::StoreClusterClient,
    key: &[u8],
    now_ms: u64,
    seed: Option<(crate::relay::ws::Connection, u64)>,
    mut apply: impl FnMut(&mut crate::relay::ws::Connection) -> bool,
) -> Result<Option<crate::relay::ws::Connection>> {
    let mut seed = seed;
    for _ in 0..CLAIM_RETRIES {
        let (mut conn, version) = match seed.take() {
            Some(pair) => pair,
            None => {
                match store::get_doc_versioned::<crate::relay::ws::Connection>(config, key).await? {
                    Some(pair) => pair,
                    None => return Ok(None),
                }
            }
        };
        if !apply(&mut conn) {
            return Ok(Some(conn));
        }
        conn.touch(now_ms);
        let expires_at = conn.expires_at_ms;
        match store::put_expiring_doc_if_version(config, key, &conn, Some(version), expires_at)
            .await?
        {
            Ok(_) => return Ok(Some(conn)),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict(
        "ws connection state contended beyond the retry budget".into(),
    ))
}

/// The operator-configured public relay URL, if any.
pub(super) async fn configured_relay_url(
    config: &mut ferrite_app::client::StoreClusterClient,
) -> Option<String> {
    match config.get(RELAY_URL_CONFIG).await {
        Ok(Some(bytes)) => {
            let url = String::from_utf8_lossy(&bytes).trim().to_string();
            (!url.is_empty()).then_some(url)
        }
        _ => None,
    }
}

/// Publish one NIP-01 frame to the connection that opened this pump.
///
/// `event: None` makes the gateway relay `data` as a WebSocket TEXT frame, which
/// is what a nostr client reads; frames go to the queue group the OPEN envelope
/// named, never to a statically bound one (see the module docs).
pub(super) async fn send_ws(
    frames: &mut ferrite_app::client::QueueClusterClient,
    correlation_id: &str,
    message: &crate::relay::ws::RelayMessage,
) -> Result<()> {
    // A WebSocket frame: HTTP framing fields do not apply.
    let wire = ferrite_app::StreamFrame {
        correlation_id: correlation_id.to_string(),
        event: None,
        data: message.to_bytes()?,
        done: false,
    };
    frames
        .publish_frame(&wire)
        .await
        .map(|_| ())
        .map_err(|e| BuzzError::Storage(format!("ws frame publish: {e}")))
}

/// Hang up: a `done` frame is what the gateway's write loop turns into a
/// WebSocket CLOSE. This is how the app ends a connection it will not serve — a
/// revoked reader, a rotated feed, an expired connection — instead of leaving a
/// socket open that receives nothing.
pub(super) async fn close_ws_socket(
    frames: &mut ferrite_app::client::QueueClusterClient,
    correlation_id: &str,
) -> Result<()> {
    // The socket CLOSE marker, not a buffered HTTP reply.
    let wire = ferrite_app::StreamFrame {
        correlation_id: correlation_id.to_string(),
        event: None,
        data: Vec::new(),
        done: true,
    };
    frames
        .publish_frame(&wire)
        .await
        .map(|_| ())
        .map_err(|e| BuzzError::Storage(format!("ws close publish: {e}")))
}

/// Target events a candidate may INHERIT its channel from, pre-resolved.
///
/// Three states, kept apart because [`derive_channel`] answers each differently
/// and a batched read must not blur them into one:
///
/// * `Some(Some(event))` — the target, decoded.
/// * `Some(None)` — the row is THERE but could not be decoded. The per-event
///   point read fails on this, so this must too: a corrupt target read as "no
///   channel" would publish a private room's reactions to everyone.
/// * absent — no such row. A reaction cannot be ingested without its target, but
///   a deletion can name anything, and this is the case that legitimately
///   derives no channel.
pub(super) type InheritedTargets = std::collections::BTreeMap<crate::relay::event::EventId, Option<Event>>;

/// Read every target this candidate set inherits a channel from, in one go.
///
/// Chunked to the platform's batched-read bound by [`store::multi_get_raw`], so
/// a full page is one or two calls rather than one per row. Only reaction and
/// deletion candidates inherit anything, and a malformed `e` tag is not read at
/// all — [`derive_channel`] refuses it without needing a row.
pub(super) async fn ws_inherited_targets(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    candidates: &[Event],
) -> Result<InheritedTargets> {
    let mut wanted: std::collections::BTreeSet<crate::relay::event::EventId> =
        std::collections::BTreeSet::new();
    for event in candidates {
        if !matches!(
            event.kind,
            crate::relay::kind::KIND_REACTION | crate::relay::kind::KIND_DELETION
        ) {
            continue;
        }
        let target_tag = if event.kind == crate::relay::kind::KIND_REACTION {
            event
                .tag_values("e")
                .filter(|v| v.len() == 64 && v.bytes().all(|b| b.is_ascii_hexdigit()))
                .last()
        } else {
            event.tag_values("e").next()
        };
        if let Some(id) = target_tag.and_then(crate::relay::crypto::from_hex32) {
            wanted.insert(id);
        }
    }
    let mut out = InheritedTargets::new();
    if wanted.is_empty() {
        return Ok(out);
    }
    let wanted: Vec<crate::relay::event::EventId> = wanted.into_iter().collect();
    let keys: Vec<Vec<u8>> = wanted.iter().map(|id| keys::evt(community, id)).collect();
    let rows = store::multi_get_raw(events, &keys).await?;
    for ((id, key), row) in wanted.iter().zip(keys.iter()).zip(rows.iter()) {
        // An absent row stays out of the map entirely, which is how a caller
        // tells "not stored" from "stored and unreadable".
        if let Some(bytes) = row {
            out.insert(*id, store::decode_doc::<Event>(key, bytes).ok());
        }
    }
    Ok(out)
}

/// The stored answer to one REQ filter: index-selected, re-filtered, re-gated.
///
/// Three properties, in this order, and none of them optional:
///
/// * the index is only a ROUTING choice ([`crate::relay::ws::plan_read`]) — every
///   candidate is re-checked against the filter and re-authorized by
///   `access::may_deliver`, because an index is not an access decision;
/// * the channel a candidate is gated under is DERIVED, never taken from the
///   author's `h` tag: a reaction inherits its target's channel, so a reaction
///   on a private room's message is refused to a non-member even when the
///   reaction itself carries no channel tag;
/// * the page is bounded by the filter's `limit`, [`crate::relay::ws::MAX_REQ_LIMIT`],
///   and [`crate::relay::ws::MAX_REQ_BYTES`] — one REQ can never dump the community
///   into the frame queue.
pub(super) async fn ws_stored_events(
    ctx: &AppContext,
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    reader: &crate::relay::event::PubKey,
    filter: &crate::relay::filter::Filter,
    now_ms: u64,
) -> Result<Vec<Event>> {
    let limit = crate::relay::ws::effective_limit(filter);
    // `limit: 0` is the client's live-only subscription (presence, user status):
    // it asked for no history, so it gets none.
    if limit == 0 {
        return Ok(Vec::new());
    }
    let candidates = match filter.search.as_deref() {
        // NIP-50: the term index chooses the candidates, and everything after
        // this line is unchanged — the same filter re-check, the same
        // per-event authorization, the same limit and byte budget. An index
        // only ever decides what to LOOK at.
        //
        // A search combined with other constraints is answered by intersecting
        // this result with them below, rather than by planning them together.
        // Correct, and occasionally more work than an ideal planner would do.
        Some(query) => {
            let terms = crate::relay::search::query_terms(query);
            if terms.is_empty() {
                return Ok(Vec::new());
            }
            // `until` is a deep-scrollback request, and on a newest-first index
            // it is a START POSITION rather than a post-filter. Without it the
            // walk begins at the newest row every time, so a client paging
            // backwards inspects the same capped window and is answered empty —
            // the same synthetic-cursor trick `walk_index` uses for every other
            // index here.
            let start = filter
                .until
                .and_then(|until| crate::relay::search::until_cursor(community, &terms, until));
            crate::relay::search::search(
                events,
                community,
                &terms,
                limit
                    .saturating_mul(2)
                    .min(crate::relay::query::MAX_PAGE_EVENTS),
                start.as_deref(),
                // `since` ends the walk early — pure saving, same result. The
                // OTHER bounds stay out of the scope on purpose: `until` is
                // already the synthetic start cursor above, and `authors` are
                // enforced by the `filter.matches` re-check below, which every
                // candidate passes through regardless of which index chose it.
                &crate::relay::search::SearchScope {
                    authors: Vec::new(),
                    since: filter.since,
                    until: None,
                },
            )
            .await?
            .events
            .into_iter()
            .map(|hit| hit.event)
            .collect()
        }
        None => {
            return ws_stored_events_walked(ctx, events, community, reader, filter, limit, now_ms).await;
        }
    };
    let mut out = Vec::new();
    ws_gate_candidates(ctx, events, community, reader, filter, limit, now_ms, candidates, &mut out).await?;
    Ok(out)
}

/// How many candidate pages one filter read walks before it answers with what it
/// has. Bounds the work a filter whose newest candidates are all unreadable can
/// cause, like [`WS_MAX_PAGES`] bounds one index walk.
pub(super) const WS_FILTER_ROUNDS: usize = 8;

/// A filter read that keeps walking OLDER candidates while the reader has been
/// served fewer than `limit` events.
///
/// Candidates are chosen by an index (newest `2·limit` rows) BEFORE the access
/// gate runs, so a page whose newest candidates the reader may not see answered
/// `[]` while readable events sat just below it. The answer carries no cursor,
/// so a client walking by `until` read `[]` as the end. Measured on staging
/// 2026-09-12: after an operator deleted 54 open rooms, the newest 500 39000
/// rows were all deleted or private rooms, and `{kinds:[39000]}` served no room
/// at all, `general` included, although every one of them was intact. The walk
/// continues from the oldest candidate seen (inclusive, deduplicated by id).
#[allow(clippy::too_many_arguments)]
pub(super) async fn ws_stored_events_walked(
    ctx: &AppContext,
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    reader: &crate::relay::event::PubKey,
    filter: &crate::relay::filter::Filter,
    limit: usize,
    now_ms: u64,
) -> Result<Vec<Event>> {
    let mut out: Vec<Event> = Vec::new();
    if let Some(candidates) = projection_point_candidates(ctx, community, filter).await? {
        ws_gate_candidates(ctx, events, community, reader, filter, limit, now_ms, candidates, &mut out).await?;
        return Ok(out);
    }
    let mut round_filter = filter.clone();
    let mut previous_oldest: Option<u64> = None;
    for _ in 0..WS_FILTER_ROUNDS {
        let candidates = ws_candidates(events, community, &round_filter, limit).await?;
        let Some(oldest) = candidates.iter().map(|event| event.created_at).min() else {
            break;
        };
        // A short candidate page is the end of what the index holds for this
        // filter: walking on would only re-read it. Only a FULL page of
        // candidates can hide readable events below it.
        let candidate_page = limit.saturating_mul(2).clamp(1, crate::relay::query::MAX_PAGE_EVENTS);
        let exhausted = candidates.len() < candidate_page;
        let remaining = limit.saturating_sub(out.len());
        ws_gate_candidates(
            ctx, events, community, reader, &round_filter, remaining, now_ms, candidates, &mut out,
        )
        .await?;
        // An id read is a point read: there is nothing older to walk to.
        if out.len() >= limit
            || exhausted
            || matches!(crate::relay::ws::plan_read(filter), crate::relay::ws::ReadPlan::Ids(_))
        {
            break;
        }
        // The same oldest second twice means one second holds more candidates
        // than a page: step below it rather than re-reading it forever.
        let next_until = if previous_oldest == Some(oldest) {
            oldest.saturating_sub(1)
        } else {
            oldest
        };
        if filter.since.is_some_and(|since| next_until < since) || next_until == 0 {
            break;
        }
        previous_oldest = Some(oldest);
        round_filter.until = Some(next_until);
    }
    Ok(out)
}

/// The most `#d` values one projection point read resolves.
pub(super) const MAX_PROJECTION_POINT_READS: usize = 256;

/// A room's projection by address: `{kinds: [39000|39001|39002], "#d": [room…]}`.
///
/// Relay-signed projections have exactly one author, the relay, so their `#d`
/// reads are point reads of the replaceable head rather than a walk of every
/// room's newest projection. Walking was bounded, so an old room such as
/// `general` was answered "not found" while newer rooms filled the candidate
/// pages (measured on staging 2026-09-12). `None` when the filter is not that
/// shape, and the caller walks the index as before.
pub(super) async fn projection_point_candidates(
    ctx: &AppContext,
    community: &str,
    filter: &crate::relay::filter::Filter,
) -> Result<Option<Vec<Event>>> {
    let (Some(kinds), Some(rooms)) = (filter.kinds.as_deref(), filter.tag_filter("#d")) else {
        return Ok(None);
    };
    if filter.ids.is_some()
        || filter.search.is_some()
        || kinds.is_empty()
        || rooms.is_empty()
        || rooms.len() > MAX_PROJECTION_POINT_READS
        || !kinds.iter().all(|kind_| crate::relay::kind::is_group_projection(*kind_))
    {
        return Ok(None);
    }
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let Some(seckey) = relay_identity(ctx, &mut config).await else {
        return Ok(None);
    };
    let Some(relay_pk) = crate::relay::crypto::schnorr::xonly_pubkey(&seckey) else {
        return Ok(None);
    };
    let relay_hex = crate::relay::crypto::to_hex(&relay_pk);
    if let Some(authors) = filter.authors.as_deref() {
        if !authors.iter().any(|author| relay_hex.starts_with(author.as_str())) {
            return Ok(Some(Vec::new()));
        }
    }
    let mut guards = planes::ape(ctx, store::GUARDS).map_err(storage)?;
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let mut out = Vec::new();
    for room in rooms {
        for kind_ in kinds {
            let key = keys::dtag_head(community, *kind_, &relay_pk, room);
            let Some(bytes) = guards
                .get(&key)
                .await
                .map_err(|e| BuzzError::Storage(format!("projection head read: {e}")))?
            else {
                continue;
            };
            let Ok(head) = serde_json::from_slice::<crate::relay::effects::ReplaceableHead>(&bytes) else {
                continue;
            };
            let Some(id) = crate::relay::crypto::from_hex32(&head.id) else {
                continue;
            };
            if let Some(event) = store::get_doc::<Event>(&mut events, &keys::evt(community, &id)).await? {
                out.push(event);
            }
        }
    }
    out.sort_by_key(|event| std::cmp::Reverse(event.created_at));
    Ok(Some(out))
}

/// Gate one candidate batch for `reader` and append what may be served to
/// `out`: at most `limit` new events, within the socket face's byte budget,
/// never an event `out` already holds.
#[allow(clippy::too_many_arguments)]
pub(super) async fn ws_gate_candidates(
    ctx: &AppContext,
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    reader: &crate::relay::event::PubKey,
    filter: &crate::relay::filter::Filter,
    limit: usize,
    now_ms: u64,
    candidates: Vec<Event>,
    out: &mut Vec<Event>,
) -> Result<()> {
    // ONE batched read for every channel this page INHERITS, before deriving any
    // of them: otherwise a page of reactions costs a store read per row here.
    let inherited = ws_inherited_targets(events, community, &candidates).await?;

    let mut memo: std::collections::BTreeMap<String, ChannelAccess> =
        std::collections::BTreeMap::new();
    let start = out.len();
    let mut bytes: usize = out.iter().map(Event::serialized_len).sum();
    for event in candidates {
        if out.iter().any(|served| served.id == event.id) {
            continue;
        }
        let channel = match derive_channel(events, community, &event, Some(&inherited)).await {
            Ok(channel) => channel,
            // A stored row whose reference cannot be read is skipped, never
            // served ungated and never allowed to fail the whole REQ.
            Err(_) => continue,
        };
        if !filter.matches(&event, channel.as_deref()) {
            continue;
        }
        let access = match channel.as_deref() {
            None => ChannelAccess::Global,
            Some(ch) => match memo.get(ch) {
                Some(access) => *access,
                None => {
                    let access = resolve_access(ctx, community, ch, reader, now_ms).await;
                    memo.insert(ch.to_string(), access);
                    access
                }
            },
        };
        if !crate::relay::access::may_deliver(&event, reader, access, true).is_allowed() {
            continue;
        }
        let size = event.serialized_len();
        if !out.is_empty() && bytes.saturating_add(size) > crate::relay::ws::MAX_REQ_BYTES {
            break;
        }
        bytes = bytes.saturating_add(size);
        out.push(event);
        if out.len() - start >= limit {
            break;
        }
    }
    Ok(())
}

/// One index read, named so the paging loop can be written once.
pub(super) enum IndexRead<'a> {
    Timeline(Option<&'a str>),
    Kind(u32),
    Author(&'a crate::relay::event::PubKey, Option<u32>),
    Mention(&'a str),
    Etag(&'a str),
}

impl IndexRead<'_> {
    /// The key prefix this read walks — what an `until` cursor is built against.
    pub(super) fn prefix(&self, community: &str) -> Vec<u8> {
        match self {
            Self::Timeline(channel) => keys::ts_prefix(community, *channel),
            Self::Kind(kind) => keys::kind_prefix(community, *kind),
            Self::Author(author, kind) => keys::author_prefix(community, author, *kind),
            Self::Mention(who) => keys::mention_prefix(community, who),
            Self::Etag(target) => keys::etag_prefix(community, target),
        }
    }

    /// Whether this index is newest-first in time. The author index without a
    /// kind is KIND-major (see `keys::author_prefix`), so a `since` bound there
    /// is a filter, never an early stop.
    pub(super) const fn is_time_ordered(&self) -> bool {
        !matches!(self, Self::Author(_, None))
    }

    pub(super) async fn page(
        &self,
        events: &mut ferrite_app::client::StoreClusterClient,
        community: &str,
        cursor: Option<&str>,
        limit: usize,
    ) -> Result<crate::relay::query::Page> {
        match self {
            Self::Timeline(channel) => {
                crate::relay::query::timeline(events, community, *channel, cursor, limit).await
            }
            Self::Kind(kind) => {
                crate::relay::query::by_kind(events, community, *kind, cursor, limit).await
            }
            Self::Author(author, kind) => {
                crate::relay::query::by_author(events, community, author, *kind, cursor, limit)
                    .await
            }
            Self::Mention(who) => {
                crate::relay::query::by_mention(events, community, who, cursor, limit).await
            }
            Self::Etag(target) => {
                crate::relay::query::by_etag(events, community, target, cursor, limit).await
            }
        }
    }
}

/// The candidate rows for one filter, from the index its plan selected.
///
/// Twice the wanted rows are gathered, because the access gate above may refuse
/// some of them: a REQ answers with at most `limit` rows and may answer with
/// fewer, which is the NIP-01 contract (a client pages with `until` and never
/// infers exhaustion from a short page).
pub(super) async fn ws_candidates(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    filter: &crate::relay::filter::Filter,
    limit: usize,
) -> Result<Vec<Event>> {
    let cap = limit
        .saturating_mul(2)
        .clamp(1, crate::relay::query::MAX_PAGE_EVENTS);
    match crate::relay::ws::plan_read(filter) {
        // Whole ids are point reads: no index walk, no ordering question.
        crate::relay::ws::ReadPlan::Ids(ids) => {
            crate::relay::query::events_by_ids(events, community, &ids).await
        }
        crate::relay::ws::ReadPlan::Channel(channel) => {
            walk_index(
                events,
                community,
                &IndexRead::Timeline(Some(&channel)),
                filter,
                cap,
                WS_MAX_PAGES,
            )
            .await
        }
        crate::relay::ws::ReadPlan::Channels(channels) => {
            let reads: Vec<IndexRead<'_>> =
                channels.iter().map(|c| IndexRead::Timeline(Some(c))).collect();
            merge_indexes(events, community, &reads, filter, cap).await
        }
        // Why: the `!g` timeline holds only channel-less events, so walking it
        // alone dropped every channel event. The kind index holds both.
        crate::relay::ws::ReadPlan::Global => {
            let prefix = format!("kind/{}/", keys::component(community)).into_bytes();
            let kinds = distinct_kinds(events, &prefix).await?;
            let reads: Vec<IndexRead<'_>> = kinds.iter().map(|k| IndexRead::Kind(*k)).collect();
            merge_indexes(events, community, &reads, filter, cap).await
        }
        crate::relay::ws::ReadPlan::Etags(targets) => {
            let reads: Vec<IndexRead<'_>> = targets.iter().map(|t| IndexRead::Etag(t)).collect();
            merge_indexes(events, community, &reads, filter, cap).await
        }
        crate::relay::ws::ReadPlan::Mentions(who) => {
            let reads: Vec<IndexRead<'_>> = who.iter().map(|w| IndexRead::Mention(w)).collect();
            merge_indexes(events, community, &reads, filter, cap).await
        }
        crate::relay::ws::ReadPlan::Kinds(kinds) => {
            let reads: Vec<IndexRead<'_>> = kinds.iter().map(|k| IndexRead::Kind(*k)).collect();
            merge_indexes(events, community, &reads, filter, cap).await
        }
        crate::relay::ws::ReadPlan::Authors(authors, kinds) => {
            let mut reads: Vec<IndexRead<'_>> = Vec::new();
            // Why: without kinds, the author index is kind-major, so one walk
            // reads in kind order and an `until` cursor sorts past every row.
            // Each kind the author wrote is a time-ordered run of its own.
            let mut discovered: Vec<(usize, Vec<u32>)> = Vec::new();
            if kinds.is_empty() {
                for (i, author) in authors.iter().enumerate() {
                    let prefix = keys::author_prefix(community, author, None);
                    discovered.push((i, distinct_kinds(events, &prefix).await?));
                }
            }
            for (i, author) in authors.iter().enumerate() {
                if kinds.is_empty() {
                    for (_, found) in discovered.iter().filter(|(j, _)| *j == i) {
                        for kind in found {
                            reads.push(IndexRead::Author(author, Some(*kind)));
                        }
                    }
                } else {
                    for kind in &kinds {
                        reads.push(IndexRead::Author(author, Some(*kind)));
                    }
                }
            }
            merge_indexes(events, community, &reads, filter, cap).await
        }
    }
}

/// How many distinct kinds one index's kind runs are read for. buzz writes a
/// few dozen kinds; the bound keeps a hostile kind spread from turning one REQ
/// into thousands of scans.
pub(super) const MAX_DISCOVERED_KINDS: usize = 64;

/// The distinct kinds under `prefix`, whose next key segment is a `{kind10}/`
/// run: one single-row scan per kind, each starting past the previous run.
pub(super) async fn distinct_kinds(
    events: &mut ferrite_app::client::StoreClusterClient,
    prefix: &[u8],
) -> Result<Vec<u32>> {
    let mut out: Vec<u32> = Vec::new();
    let mut after: Option<Vec<u8>> = None;
    while out.len() < MAX_DISCOVERED_KINDS {
        let (rows, _more) = store::scan(events, prefix, after.as_deref(), 1).await?;
        let Some((key, _, _)) = rows.into_iter().next() else {
            break;
        };
        let tail = &key[prefix.len().min(key.len())..];
        let Some(kind) = tail
            .get(..10)
            .filter(|_| tail.get(10) == Some(&b'/'))
            .and_then(|digits| std::str::from_utf8(digits).ok())
            .and_then(|digits| digits.parse::<u32>().ok())
        else {
            break;
        };
        out.push(kind);
        let mut next = prefix.to_vec();
        next.extend_from_slice(format!("{kind:010}/").as_bytes());
        next.push(0xff);
        after = Some(next);
    }
    Ok(out)
}

/// Walk one index newest-first, up to `pages` pages.
///
/// `until` becomes a synthetic START cursor rather than a filter: deep
/// scrollback on a busy channel would otherwise have to walk every newer page
/// first and would silently come back empty. `since` is an early STOP on a
/// time-ordered index, which is what keeps a narrow live subscription's backfill
/// from scanning the whole channel.
pub(super) async fn walk_index(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    read: &IndexRead<'_>,
    filter: &crate::relay::filter::Filter,
    cap: usize,
    pages: usize,
) -> Result<Vec<Event>> {
    let prefix = read.prefix(community);
    let mut cursor = filter
        .until
        .map(|until| crate::relay::ws::until_cursor(&prefix, until));
    let mut out: Vec<Event> = Vec::new();
    for _ in 0..pages.max(1) {
        let page = read.page(events, community, cursor.as_deref(), cap).await?;
        let mut done = page.complete;
        for event in page.events {
            if read.is_time_ordered() && filter.since.is_some_and(|since| event.created_at < since)
            {
                done = true;
                break;
            }
            out.push(event);
            if out.len() >= cap {
                done = true;
                break;
            }
        }
        if done {
            break;
        }
        match page.next_cursor {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    Ok(out)
}

/// Walk several indexes and merge them newest-first, in buzz's own order
/// (`created_at DESC, id ASC`), deduplicated by id — an event carrying two of
/// the filter's `#e` targets appears in two indexes and must be served once.
///
/// One page per index: several indexes times several pages each is how a single
/// REQ turns into a scan, and the merge only needs the newest rows of each.
pub(super) async fn merge_indexes(
    events: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    reads: &[IndexRead<'_>],
    filter: &crate::relay::filter::Filter,
    cap: usize,
) -> Result<Vec<Event>> {
    let pages = if reads.len() > 1 { 1 } else { WS_MAX_PAGES };
    let mut merged: Vec<Event> = Vec::new();
    for read in reads {
        merged.extend(walk_index(events, community, read, filter, cap, pages).await?);
    }
    merged.sort_by(|a, b| {
        b.created_at
            .cmp(&a.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    merged.dedup_by(|a, b| a.id == b.id);
    merged.truncate(cap);
    Ok(merged)
}

/// The feed's next offset: where a WebSocket connection starts reading.
///
/// A ws client asks for history with `REQ` and gets it from the indexes, so the
/// live pump starts at the HEAD of the log. Starting at zero (as an SSE session
/// legitimately may, because its client asked for a replay) would push the whole
/// community's history down every socket as if it were live, on every connect.
pub(super) async fn feed_head(
    topics: &mut ferrite_app::client::TopicLogClient,
    community: &str,
    generation: u32,
) -> u64 {
    let topic = keys::feed_topic(community, generation);
    match topics.list_topics().await {
        Ok(list) => list
            .iter()
            .find(|(name, _epoch, _len)| name == &topic)
            .map_or(0, |(_name, _epoch, len)| *len),
        Err(e) => {
            // An unreadable head means a replay rather than a silent gap: the
            // client dedups by event id, and missing live events would not be
            // recoverable.
            tracing::warn!("feed head unreadable; starting at 0: {e}");
            0
        }
    }
}

/// Tail the feed for one WebSocket connection.
///
/// The SAME log the SSE pump reads, with the same three defences, for the same
/// reasons: dedup by event id (a topic append is retried, so a failover alone can
/// duplicate a record), access-cache invalidation riding the feed (a membership
/// or moderation record drops the affected entry before anything later is
/// delivered), and ONE revocation-tombstone read per batch before any record in
/// it is served (§6.3 rule (b) — the crash window where a ban's state write
/// committed but its feed append never did).
pub(super) async fn run_ws_pump(ctx: AppContext, req: IngressRequest, generation: u64) -> Result<()> {
    let community = DEFAULT_COMMUNITY;
    let cid = req.correlation_id.clone();
    let mut frames = ctx.reply_to_queue(&req.reply_to).await.map_err(storage)?;
    let mut topics = planes::topics(&ctx, store::FEED).map_err(storage)?;
    let mut config = planes::store(&ctx, store::CONFIG).map_err(storage)?;
    let mut events_store = planes::store(&ctx, store::EVENTS).map_err(storage)?;
    let conn_key = keys::wsconn(community, &cid);
    let cursor_key = keys::wsconn_cursor(community, &cid);

    let mut seen = SeenIds::new();
    let mut access = AccessCache::new(ACCESS_TTL_MS);
    let mut last_revocation: Option<Vec<u8>> = None;
    let mut bound: Option<crate::relay::event::PubKey> = None;

    let mut cursor = match store::get_doc::<Cursor>(&mut config, &cursor_key).await {
        // A redelivered open resumes where the superseded pump stopped.
        Ok(Some(cursor)) => cursor,
        _ => Cursor::new(
            FEED_GENERATION,
            feed_head(&mut topics, community, FEED_GENERATION).await,
        ),
    };
    // As in the SSE pump: what the cursor row already holds, and how long this
    // connection has been quiet.
    let mut persisted = cursor;

    loop {
        let now_ms = ctx.now_ms();

        // The connection document is the liveness authority: gone means closed,
        // and a newer generation means another pump owns this socket now.
        let Some(conn) =
            store::get_doc::<crate::relay::ws::Connection>(&mut config, &conn_key).await?
        else {
            return Ok(());
        };
        if conn.generation != generation {
            // Do NOT close the socket: the pump that superseded us is serving it.
            return Ok(());
        }
        if !conn.is_live(now_ms) {
            close_ws_socket(&mut frames, &cid).await?;
            return Ok(());
        }

        // A NIP-42 exchange may bind (or rebind) the reader mid-connection, and
        // every cached decision belonged to the previous one.
        if conn.authenticated() != bound {
            bound = conn.authenticated();
            access.invalidate_all();
            last_revocation = None;
            if let Some(reader) = bound {
                let (tombstone, revoked) = check_subscriber_standing(
                    &mut events_store,
                    &mut config,
                    community,
                    &reader,
                    now_ms,
                )
                .await?;
                if revoked {
                    let notice = crate::relay::ws::RelayMessage::Notice {
                        message: "restricted: your access to this community was revoked".into(),
                    };
                    send_ws(&mut frames, &cid, &notice).await?;
                    close_ws_socket(&mut frames, &cid).await?;
                    return Ok(());
                }
                last_revocation = tombstone;
            }
        }

        let topic = keys::feed_topic(community, cursor.generation);
        // The same bounded read as the per-community stream above.
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
                // The feed cannot be resumed within its bounds. NIP-01 has no resume cursor, so
                // the honest answer is to say so and hang up: the client
                // reconnects and re-issues its REQs.
                let notice = crate::relay::ws::RelayMessage::Notice {
                    message: "error: the relay feed cannot be resumed; reconnect".into(),
                };
                send_ws(&mut frames, &cid, &notice).await?;
                close_ws_socket(&mut frames, &cid).await?;
                return Ok(());
            }
        };

        let (batch, retained_base) = page;
        let now_ms = ctx.now_ms();
        // The connection document is the liveness authority: gone means closed,
        // and a newer generation means another pump owns this socket now.
        let Some(conn) =
            store::get_doc::<crate::relay::ws::Connection>(&mut config, &conn_key).await?
        else {
            return Ok(());
        };
        if conn.generation != generation {
            // Do NOT close the socket: the pump that superseded us is serving it.
            return Ok(());
        }
        if !conn.is_live(now_ms) {
            close_ws_socket(&mut frames, &cid).await?;
            return Ok(());
        }

        // A NIP-42 exchange may bind (or rebind) the reader mid-connection, and
        // every cached decision belonged to the previous one.
        if conn.authenticated() != bound {
            bound = conn.authenticated();
            access.invalidate_all();
            last_revocation = None;
            if let Some(reader) = bound {
                let (tombstone, revoked) = check_subscriber_standing(
                    &mut events_store,
                    &mut config,
                    community,
                    &reader,
                    now_ms,
                )
                .await?;
                if revoked {
                    let notice = crate::relay::ws::RelayMessage::Notice {
                        message: "restricted: your access to this community was revoked".into(),
                    };
                    send_ws(&mut frames, &cid, &notice).await?;
                    close_ws_socket(&mut frames, &cid).await?;
                    return Ok(());
                }
                last_revocation = tombstone;
            }
        }
        let subscriber = conn.reader();

        if let Some(reason) = delivery::check_wait_continuity(
            cursor,
            FEED_GENERATION,
            retained_base,
            batch.first().map(|record| record.offset),
        ) {
            let notice = crate::relay::ws::RelayMessage::Notice {
                message: format!(
                    "error: the relay feed moved ({}); reconnect",
                    reason.as_str()
                ),
            };
            send_ws(&mut frames, &cid, &notice).await?;
            close_ws_socket(&mut frames, &cid).await?;
            return Ok(());
        }

        let empty = batch.is_empty();

        // §6.3 rule (b): ONE tombstone read per non-empty batch, BEFORE any
        // record in it is delivered. A changed value means a ban or a membership
        // removal committed for this subscriber — with or without its feed
        // record — so every cached access decision is stale, and a live ban ends
        // the connection outright. Fail-CLOSED on an unreadable tombstone.
        if !empty {
            if let Some(reader) = bound {
                let (tombstone, revoked) = check_subscriber_standing(
                    &mut events_store,
                    &mut config,
                    community,
                    &reader,
                    now_ms,
                )
                .await?;
                if tombstone != last_revocation {
                    last_revocation = tombstone;
                    access.invalidate_all();
                }
                if revoked {
                    let notice = crate::relay::ws::RelayMessage::Notice {
                        message: "restricted: your access to this community was revoked".into(),
                    };
                    send_ws(&mut frames, &cid, &notice).await?;
                    close_ws_socket(&mut frames, &cid).await?;
                    return Ok(());
                }
            } else if super::community::community_is_closed(&mut config).await? {
                if let Some(last) = batch.last() {
                    cursor = Cursor::new(cursor.generation, last.offset.saturating_add(1));
                }
                if cursor != persisted
                    && store::put_doc(&mut config, &cursor_key, &cursor)
                        .await
                        .is_ok()
                {
                    persisted = cursor;
                }
                continue;
            }
        }

        for record in &batch {
            let at = Cursor::new(cursor.generation, record.offset.saturating_add(1));
            cursor = at;
            let Ok(decoded) = delivery::FeedRecord::decode(&record.value) else {
                continue;
            };
            // Invalidation rides the feed: a membership, moderation or channel
            // record drops the affected cache entry BEFORE anything after it is
            // delivered.
            if decoded.invalidates_access() {
                match decoded.channel.as_deref() {
                    Some(ch) => access.invalidate(ch),
                    None => access.invalidate_all(),
                }
            }
            if decoded.community != community {
                continue;
            }
            // Which subscriptions asked for this record (filters only — asking
            // is not being allowed).
            let wanted: Vec<String> = conn
                .subscriptions
                .iter()
                .filter(|sub| {
                    crate::relay::filter::filters_match(
                        &sub.filters,
                        &decoded.event,
                        decoded.channel.as_deref(),
                    )
                })
                .map(|sub| sub.id.clone())
                .collect();
            if wanted.is_empty() {
                continue;
            }
            let resolved = match decoded.channel.as_deref() {
                None => ChannelAccess::Global,
                Some(ch) => match access.get(ch, now_ms) {
                    Some(cached) => cached,
                    None => {
                        let fresh = resolve_access(&ctx, community, ch, &subscriber, now_ms).await;
                        if let ChannelAccess::Open = fresh {
                            access.put(ch, crate::relay::channel::Visibility::Open, true, now_ms);
                        } else if let ChannelAccess::Private { reader_is_member } = fresh {
                            access.put(
                                ch,
                                crate::relay::channel::Visibility::Private,
                                reader_is_member,
                                now_ms,
                            );
                        }
                        fresh
                    }
                },
            };
            if !crate::relay::access::may_deliver(&decoded.event, &subscriber, resolved, true)
                .is_allowed()
            {
                continue;
            }
            // Dedup LAST, as the SSE pump does: an event the gate refused must
            // not consume a slot in the ring, or a later legitimate delivery of
            // the same id could be dropped.
            if !seen.insert(decoded.event.id) {
                continue;
            }
            for sub_id in wanted {
                let frame = crate::relay::ws::RelayMessage::Event {
                    sub_id,
                    event: Box::new(decoded.event.clone()),
                };
                send_ws(&mut frames, &cid, &frame).await?;
            }
        }

        // The cursor lives under a key this pump OWNS. Writing it back into the
        // connection document would let a pump revert a subscription a
        // concurrently-served REQ had just added. Written only when it MOVED, for
        // the reason the SSE pump gives: a tick that delivered nothing has nothing
        // to record.
        if cursor != persisted
            && store::put_doc(&mut config, &cursor_key, &cursor)
                .await
                .is_ok()
        {
            persisted = cursor;
        }

        // The next read parks at the topic front; no polling sleep is needed.
    }
}
