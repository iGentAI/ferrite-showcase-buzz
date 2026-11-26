//! The front door: the serve loops that turn gateway envelopes into work.
//!
//! The app owns no listener. The gateway accepts HTTP and publishes an
//! [`IngressRequest`] onto a queue; this module leases those, routes them, and
//! publishes an [`IngressReply`] back. A second loop does the same for the SSE
//! stream face, where an open arrives as an envelope and the response is a series
//! of frames published to the queue the open names.
//!
//! Two disciplines that the platform's shape forces, and that the loops below
//! implement rather than assume:
//!
//! * **Ack after commit.** A request envelope is acked only once its work is
//!   durable, because redelivery is the only resume driver an unfinished saga
//!   has. The reply is published first, then the ack.
//! * **Frames follow the envelope.** Outbound stream frames are published to the
//!   queue named by the *open envelope's* `reply_to`, never to a statically bound
//!   frames queue. Each gateway instance competing-consumes its own frame group
//!   and permanently absorbs frames addressed to sessions it does not hold, so a
//!   statically bound queue would silently destroy most frames the moment a second
//!   gateway existed.

use ferrite_app::tracing;
use std::io;
use std::time::Duration;

use ferrite_app::serde_json;
use ferrite_app::{rt, AppContext, IngressRequest};

use crate::relay::auth;
use crate::relay::delivery;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::{BuzzError, Result, APP_ID, DEFAULT_COMMUNITY};

/// Store key holding the sealed key that signs stream tokens, when a secret is
/// not injected. Secrets are the preferred home; this is the operator-config
/// fallback so a dev deployment can run without a secrets round-trip.
const STREAM_KEY_CONFIG: &[u8] = b"buzz-stream-token-key";

/// The secret name checked first for the stream-token key.
const STREAM_KEY_SECRET: &str = "buzz-stream-token-key";

/// Store key holding the relay's own signing identity (64-hex secp256k1
/// secret), when a secret is not injected — the [`STREAM_KEY_CONFIG`] pattern.
/// This is the key the relay-signed NIP-29 group projections are signed with;
/// see [`relay_identity`].
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

/// The secret name checked first for the relay identity.
const RELAY_IDENTITY_SECRET: &str = "buzz-relay-identity";

/// Access-cache TTL. Buzz's value; the eager feed-driven invalidation is what
/// actually keeps removals prompt, and this is only the backstop.
const ACCESS_TTL_MS: u64 = 10_000;
/// Presence heartbeat lifetime: three heartbeat windows, the contract
/// upstream's client states outright (`presence.ts`: "keep the local
/// optimistic cache and relay expiry at three heartbeat windows"). Upstream's
/// heartbeat is 60 s (PR #3783), so three windows is 180 s; at a 90 s TTL, ONE
/// delayed beat flickers a present member offline. Expiry *is* the offline
/// signal; there is no sweeper.
const PRESENCE_TTL_MS: u64 = 180_000;
/// Per-author message rate: buzz's human tier, 60 messages per minute.
const MSG_RATE_LIMIT: i64 = 60;
const MSG_RATE_WINDOW_MS: u64 = 60_000;
/// Hard upper bound on rows returned by any feed category (buzz's
/// `FEED_MAX_LIMIT`): callers may request fewer, never more.
const FEED_MAX_LIMIT: usize = 100;
/// How many index pages a mention-driven feed read will walk before giving
/// up (each page is up to `FEED_MAX_LIMIT` rows): the kind filter can discard
/// most of a page, and an unbounded walk over a busy mention index would turn
/// one request into an unbounded scan.
const FEED_MAX_PAGES: usize = 5;
/// Config key holding the JSON array of community-moderator hex pubkeys. Only
/// these may issue a moderation command; absent ⇒ no one can (fail-closed).
const MODERATORS_CONFIG: &[u8] = b"buzz-community-moderators";

/// The invite HMAC key material: a secret when injected, else
/// the operator-config fallback — the stream-token-key pattern. Absent both,
/// minting and claiming are refused (a guessable key would make every invite
/// forgeable).
const INVITE_KEY_SECRET: &str = "buzz-invite-key";
const INVITE_KEY_CONFIG: &[u8] = b"buzz-invite-key";
/// The operator join-policy document: JSON `{ "version", "terms_markdown"?,
/// "privacy_markdown"?, "age_attestation_required"? }`. Present ⇒ a claim
/// must carry a receipt minted by the accept-policy route.
const JOIN_POLICY_CONFIG: &[u8] = b"buzz-join-policy";
/// `"true"` ⇒ the community is invite-only: event WRITES require the
/// membership an invite claim materializes. Absent/other ⇒ open (the default).
const COMMUNITY_CLOSED_CONFIG: &[u8] = b"buzz-community-closed";
/// The pubkeys allowed to author `KIND_PROCESS_RESULT`
/// — the operator-registered process cell identities. Written over the NIP-98
/// operator plane; validated by `operator::validate_process_cells` to the
/// same shape as the moderator set.
const PROCESS_CELLS_CONFIG: &[u8] = b"buzz-process-cells";
/// Fixed-window bound on claim attempts per pubkey (buzz's: 10 per minute) —
/// claims are idempotent and a real user performs exactly one, so this only
/// bounds brute-force probing.
const INVITE_CLAIM_RATE_LIMIT: i64 = 10;
const INVITE_CLAIM_RATE_WINDOW_MS: u64 = 60_000;

/// How many times a stream open retries its generation compare-and-swap before
/// giving up. Contention here means another open for the same session is landing
/// at the same moment; a couple of rounds settles it, and refusing after that is
/// better than looping while a client waits.
const CLAIM_RETRIES: u32 = 8;

/// The live feed generation: the one topic this binary appends to and serves
/// sessions from.
///
/// ROTATING IT IS A SOURCE CHANGE AND A REDEPLOY, not a runtime lever. A generation is compiled in on purpose:
/// every publish path would otherwise pay a config read to learn where to
/// append, and two instances that disagreed mid-rollout would split the feed.
/// Sessions carry the generation they read (`Cursor` is `(generation, offset)`),
/// so a bump is detected by `delivery::check_continuity` and reported as
/// `ResetReason::GenerationRotated` rather than silently confusing offsets.
///
/// What IS a runtime lever is disposal of what a rotation left behind:
/// `POST /buzz/api/operator/feed/retire` deletes generations strictly below
/// this one ([`crate::relay::ingest::retire_superseded_feeds`]). Run it after a
/// rollout has finished, not during: an instance still running the previous
/// generation would recreate its topic on the next append.
const FEED_GENERATION: u32 = 0;

mod boot;
mod ui_assets;
mod events;
mod agents;
mod members;
mod automation;
mod link_preview;
mod community;
mod reads;
mod channels;
mod observability;
mod media;
mod operator_plane;
mod stream;
mod ws_face;
mod identity;

pub use boot::{seed_ui_assets};
use boot::{SEED_UI_BLOB, ensure_feed_topics, reconcile_due_schedules, reconcile_group_projections, reconcile_persona_profiles};
#[cfg(test)]
use boot::{MAX_RECONCILED_CHANNELS, starter_rooms_first};
pub use ui_assets::{resolve_ui_manifest_file};
use ui_assets::{ui_asset};
pub(crate) use events::{IngestOutcome, ingest_event, mcp_search};
use events::{submit_event, upstream_count, upstream_query};
use agents::{agent_config, agent_harnesses, agent_memory, agent_models, agent_owner_route, caller_is_operator, create_agent, delete_project, list_agents, may_manage_agent, remove_agent, set_agent_active};
#[cfg(test)]
use agents::{agent_config_view};
use members::{members_add, members_remove, members_role};
pub(crate) use automation::{CreatorStanding, agent_paused, creator_standing, persona_schedule_context, persona_standing};
use automation::{schedules_cancel, schedules_create, schedules_list, workflow_runs};
use link_preview::{link_preview};
pub(crate) use community::enforce_read_standing;
use community::{accept_policy, archived_identities_route, claim_invite, enforce_community_membership, join_policy, me, mint_invite, nip05};
#[cfg(test)]
use community::{claim_status, closed_flag};
pub(crate) use reads::{window_page};
use reads::{channel_window, feed, members, presence, reactions, relay_info, run_query, search_route, subscribe};
use channels::{create_channel};
#[cfg(test)]
use channels::{CHANNEL_ID_RETRIES, assigned_channel_id, salted_channel_id, slugify_channel_name};
use observability::{metrics};
#[cfg(test)]
use observability::{requests_counts};
use media::{MEDIA_URL_PREFIX, media_begin, media_limits, media_object, media_stage_chunk};
use operator_plane::{OPERATOR_CONFIG_PREFIX, operator_assets_begin, operator_assets_chunk, operator_assets_finish, operator_config_list, operator_config_read, operator_config_write, operator_feed_retire};
use stream::{stream_loop};
use ws_face::{ws_loop, ws_ticket};
#[cfg(test)]
use ws_face::{WsDone, WsJob, ws_close_ack, ws_open_ack};
#[cfg(test)]
use crate::relay::keys;
#[cfg(test)]
use crate::relay::session::Cursor;
#[cfg(test)]
use agents::DefinitionRows;

/// Run the app: the api loop in the foreground, the stream loop beside it.
pub async fn serve(ctx: AppContext) -> io::Result<()> {
    tracing::info!(app = APP_ID, "relay serve loops starting");

    // The plane pool belongs to THIS serving instance: discard anything a
    // previous one left behind, whose connections point at a fleet this
    // instance cannot reach (see `planes::reset`).
    planes::reset();

    // The web client (buzz's `web/`, ported) is embedded in this binary and
    // seeded into the `web-assets` blob at boot, exactly as `agents-cloud`
    // seeds its own SPA: the gateway's content route then serves the bytes
    // with real MIME types and the relay never proxies them. Idempotent (an
    // unchanged object is left alone), and non-fatal — a deployment whose
    // blob is briefly unreachable still serves its API.
    // Bootstrap must never sit in front of the request consumer. Blob/store
    // maintenance can be slow (or temporarily unavailable) while the relay's
    // cheap API routes are otherwise healthy; awaiting it here made every
    // request time out because `api_loop` had not started polling yet.
    //
    // Not on upstream Core: there the api `/` catch-all shadows the `/buzz/ui/` content route, so the router
    // answers the UI itself (`ui_asset`): from the embedded table until an operator has pushed a bundle, and
    // from the pushed bundle's blob objects after. Nothing reads a blob copy of the EMBEDDED bundle, so seeding
    // it copied ~27 MB per boot through the web-assets binding for nothing. (Each binding's token carries its
    // own quota bucket, so this copy did NOT cause the events store's "capability quota exceeded" refusals;
    // those came from the store's declared workload, see `ferrite.json`.)
    // The operator's asset-push routes still work; flip `SEED_UI_BLOB` for a Core that serves content first.
    if SEED_UI_BLOB {
        let asset_ctx = ctx.clone();
        rt::spawn(async move {
            seed_ui_assets(&asset_ctx).await;
        });
    }

    // The delivery topics are the app's own to create (a topics binding carries
    // `LogAdmin` for exactly this), and creating them at BOOT keeps the manifest
    // round trip off the request path: the first message a deployment ever accepts
    // would otherwise pay it, and pay it again on the first message after a feed
    // rotation. A failure here is not fatal — `ingest::publish_feed` still creates
    // a topic it finds absent — so a topics service that is briefly unreachable
    // delays the first append instead of refusing to start.
    let topic_ctx = ctx.clone();
    rt::spawn(async move {
        ensure_feed_topics(&topic_ctx).await;
    });

    // Rooms that predate the relay's signing identity carry no NIP-29
    // projection, and nothing on the write path ever revisits them: a client
    // builds its channel list from kind 39000, so such a room is invisible
    // forever while `create_channel` keeps answering 409. Converge at boot.
    let reconcile_ctx = ctx.clone();
    rt::spawn(async move {
        reconcile_group_projections(&reconcile_ctx).await;
    });

    // The same class of problem for PERSONAS: a persona registered through the
    // operator's `buzz-agents` document has no profile, because only the
    // create-agent route publishes one — and a persona without a profile cannot
    // be mentioned at all (see `reconcile_persona_profiles`). Converge at boot.
    let profiles_ctx = ctx.clone();
    rt::spawn(async move {
        reconcile_persona_profiles(&profiles_ctx).await;
    });

    // The same class of problem for SCHEDULES: a row can be live with its due time
    // in the past and no wake in existence, and nothing else ever revisits one. Its
    // due time is part of the id's digest and `build` normalises a past one to now,
    // so re-creating the same definition makes a DIFFERENT schedule rather than
    // healing this one — which is why the repair has to live here. Converge at boot.
    let schedules_ctx = ctx.clone();
    rt::spawn(async move {
        reconcile_due_schedules(&schedules_ctx).await;
    });

    let stream_ctx = ctx.clone();
    rt::spawn(async move {
        if let Err(e) = stream_loop(stream_ctx).await {
            tracing::error!("stream loop exited: {e}");
        }
    });

    // The NIP-01/NIP-42 WebSocket face, on its own route queue. Spawned only
    // where that queue is bound: a deployment (or a gate) that wires the HTTP
    // and SSE faces alone keeps working, and the socket face is simply absent
    // rather than a loop logging a missing binding forever.
    if ctx.queue(store::WS_REQUESTS).is_ok() {
        let ws_ctx = ctx.clone();
        rt::spawn(async move {
            if let Err(e) = ws_loop(ws_ctx).await {
                tracing::error!("ws loop exited: {e}");
            }
        });
    } else {
        tracing::warn!("no ws-requests binding; the WebSocket face is not served");
    }

    api_loop(ctx).await
}

/// How many requests this app will have in flight at once.
///
/// Awaiting `route()` INLINE between poll and ack holds exactly one request in
/// flight for the whole app, so latency grows linearly with concurrency while
/// throughput stays pinned. Measured on the live deployment with an inline await:
/// `healthz` p50 10.2 → 96 → 365 ms at 1/10/50 concurrent, `query` 63 → 392 →
/// 2198 ms, throughput flat at ~116 rps for a no-op and 18-34 rps for real
/// routes, all while the relay used 1-2% of its one-CPU budget. That is pure
/// serialization, not a CPU or store limit.
///
/// The saga is safe to run concurrently — admission is an APE reservation keyed
/// by event id, the NIP-98 replay burn is a create-if-absent CAS, and the rate
/// limiter is a CAS counter — so the only thing a bound buys is memory: a page
/// read can hold a quarter-megabyte reply, and 32 of those is a bounded ~8 MiB
/// against the app's 512 MiB cgroup.
const MAX_IN_FLIGHT: usize = 32;

/// Idle backoff for cancellation-safe request polling. The named-queue long
/// poll can deliver immediately after its caller-side deadline, consuming a
/// lease that this app never observes. A nonblocking poll avoids that race;
/// this small backoff keeps an empty queue cheap without adding visible
/// request latency.
const REQUEST_IDLE_POLL_MS: u64 = 25;

/// The `content-type` response header name.
const HEADER_CONTENT_TYPE: &str = "content-type";
/// The RFC 9728 challenge header name. Not on the gateway's refused list
/// (the platform gateway's `is_forbidden_reply_header`), so it reaches a
/// client verbatim.
pub(crate) const HEADER_WWW_AUTHENTICATE: &str = "www-authenticate";
/// The RFC 6585 §4 back-off header, set on every 429 this relay produces.
const HEADER_RETRY_AFTER: &str = "retry-after";

/// `location`: the redirect target of the root landing reply.
const HEADER_LOCATION: &str = "location";
/// What a JSON reply declares itself to be.
const MIME_JSON: &str = "application/json";
/// What the liveness replies declare themselves to be.
const MIME_TEXT: &str = "text/plain; charset=utf-8";
/// How long a rate-limited caller is told to wait, in seconds.
///
/// The relay's own limiter windows are a minute wide ([`MSG_RATE_WINDOW_MS`],
/// [`OPERATOR_RATE_WINDOW_MS`], [`INVITE_CLAIM_RATE_WINDOW_MS`]), and the
/// coarser hourly windows still refill against the same clock, so one minute
/// is the honest floor to advertise: never longer than the shortest window,
/// never zero.
const RETRY_AFTER_SECS: &str = "60";

/// One routed answer: the HTTP status, the response headers, and the body.
///
/// The pinned SDK's [`ferrite_app::IngressReply`] declares
/// `headers: Vec<(String, String)>` with repeated names preserved, and the
/// gateway validates the list and writes it (the platform gateway's
/// `validate_reply_headers`). So the status and the headers travel together from
/// the handler that decides them to the envelope that carries them.
///
/// Body-side carriage of the same values stays beside the headers (the
/// `www_authenticate` body field, the ACP `_meta.acp.connectionId`): a client
/// written against the header-less shape must keep working.
pub(crate) struct Reply {
    /// The HTTP status to serve.
    pub(crate) status: u16,
    /// Application response headers, in emission order.
    pub(crate) headers: Vec<(String, String)>,
    /// The HTTP response body.
    pub(crate) body: Vec<u8>,
}

impl Reply {
    /// A JSON answer. Every JSON reply this relay produces goes through here,
    /// so `content-type` is set once rather than per route.
    pub(crate) fn json(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![(HEADER_CONTENT_TYPE.to_string(), MIME_JSON.to_string())],
            body,
        }
    }

    /// A `text/plain` answer: the liveness probes, which answer `ok` and not
    /// JSON.
    fn text(status: u16, body: Vec<u8>) -> Self {
        Self {
            status,
            headers: vec![(HEADER_CONTENT_TYPE.to_string(), MIME_TEXT.to_string())],
            body,
        }
    }

    /// An answer with no body and therefore no media type — the ACP 202 for a
    /// frame of nothing but notifications.
    pub(crate) fn bodyless(status: u16) -> Self {
        Self {
            status,
            headers: Vec::new(),
            body: Vec::new(),
        }
    }

    /// Append one response header, dropping a value the gateway would refuse.
    ///
    /// The gateway rejects the WHOLE reply with a 502 when any header value
    /// carries a control byte (`validate_reply_headers`), so an
    /// operator-configured resource identifier with a stray newline in it would
    /// turn a correct 401 into a gateway error. Names here are compile-time
    /// constants and all pass `is_forbidden_reply_header`; values are not, so
    /// they are checked. A dropped header is logged and the body-side carriage
    /// still delivers the value.
    pub(crate) fn with_header(mut self, name: &str, value: impl Into<String>) -> Self {
        let value = value.into();
        if value.bytes().any(|b| b < 0x20 || b == 0x7f) {
            tracing::warn!(
                "response header {name} dropped: its value carries a control byte"
            );
            return self;
        }
        self.headers.push((name.to_string(), value));
        self
    }
}

/// Reports a finished request to the api loop EXACTLY once, even on unwind.
///
/// The loop's `in_flight` counter is decremented only when a completion
/// arrives, so a task that ends without reporting would leak a slot — and after
/// [`MAX_IN_FLIGHT`] such leaks the loop stops taking work forever, which
/// presents as "no reply" on every subsequent request. Reporting from `Drop`
/// makes the decrement unconditional: `route()` is not supposed to panic (the
/// crate denies panics outside tests), but "not supposed to" is not a bound.
///
/// `try_send` is sound here rather than a lost report: the channel's capacity is
/// `MAX_IN_FLIGHT` and the loop never allows more than `MAX_IN_FLIGHT`
/// outstanding tasks, so there is always room for this one.
struct Completion {
    tx: rt::mpsc::Sender<Option<u64>>,
    /// `Some(token)` once the reply is published and the lease may be acked;
    /// `None` leaves the envelope unacked so the work is re-driven.
    ack: Option<u64>,
}

impl Drop for Completion {
    fn drop(&mut self) {
        let _ = self.tx.try_send(self.ack);
    }
}

/// Lease → route (concurrently, bounded) → reply → ack.
///
/// The route AND its reply publish both run in the spawned task; only the ack
/// comes back to this task, because a lease's fencing token must be acked by the
/// client that took the lease.
///
/// Publishing from the task is not an optimisation, it is the whole point. The
/// first version of this kept the publish here, and a caller at concurrency 1
/// then waited ~522 ms instead of ~17 ms: with no further traffic the loop was
/// parked in `poll_wait`, so a finished reply sat unpublished until the NEXT
/// request woke the loop — and what woke it was the gateway's own 500 ms hedge
/// re-issuing the request. There is no `select!` available over a queue poll and
/// a channel receive (`ferrite_app::rt` re-exports `spawn`/`mpsc`/`time`, not the
/// macro), so the answer is to take the caller's reply off the loop's critical
/// path entirely and keep the loop's poll short enough that acks stay prompt.
async fn api_loop(ctx: AppContext) -> io::Result<()> {
    let mut requests = ctx.queue(store::REQUESTS)?;
    // A finished route reports the lease token to ack. `None` means its reply
    // could not be published, so the envelope is deliberately left unacked and
    // the work is re-driven rather than lost.
    let (done_tx, mut done_rx) = rt::mpsc::channel::<Option<u64>>(MAX_IN_FLIGHT);
    let mut in_flight = 0usize;

    loop {
        // Ack everything that finished while we were away.
        while let Ok(token) = done_rx.try_recv() {
            if let Some(token) = token {
                let _ = requests.ack(token).await;
            }
            in_flight = in_flight.saturating_sub(1);
        }
        // At the ceiling the only way to make progress is to wait for a
        // completion — never to take another lease we cannot serve.
        if in_flight >= MAX_IN_FLIGHT {
            match done_rx.recv().await {
                Some(token) => {
                    if let Some(token) = token {
                        let _ = requests.ack(token).await;
                    }
                    in_flight = in_flight.saturating_sub(1);
                }
                // The channel cannot close while this task holds `done_tx`.
                None => return Ok(()),
            }
            continue;
        }

        // Never use a caller-deadlined long poll here. The queue may assign a
        // lease just after that deadline and the cancelled response then
        // disappears from this consumer while the gateway waits forever.
        // Nonblocking poll + a bounded empty-queue backoff is cancellation-safe
        // and keeps the loop responsive enough to ack completed work promptly.
        let lease = match requests.poll().await {
            Ok(Some(l)) => l,
            Ok(None) => {
                rt::time::sleep(Duration::from_millis(REQUEST_IDLE_POLL_MS)).await;
                continue;
            }
            Err(e) => {
                tracing::warn!("request poll failed: {e}");
                rt::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };

        let Ok(req) = serde_json::from_slice::<IngressRequest>(&lease.payload) else {
            // Not an ingress envelope: nothing can ever handle it, so drop it
            // rather than let it redeliver forever into the dead-letter queue.
            let _ = requests.ack(lease.token).await;
            continue;
        };
        // Buzz is a public demonstration (2026-10-21): anyone with the link
        // reaches it without signing in. The open gate still answers the SSO
        // callback, sign-out and the anonymous `/healthz` probe itself and
        // publishes those replies; every other request, signed in or not,
        // carries on into the relay.
        //
        // No guest identity is minted here because Buzz never took its user
        // from the SSO session. A workspace identity is the caller's own
        // Schnorr key, proven per request by NIP-98 (HTTP) or NIP-42 (socket);
        // the browser client generates one on first visit and keeps it in
        // `localStorage`, so an anonymous visitor is already a stable guest.
        // Channel membership, private rooms, DMs, moderation and the operator
        // set are all decided on that key, exactly as for a signed-in user.
        let budget_reply = crate::http_route_budget::guard(&ctx, &req, "config", ROUTE_BUDGETS).await;
        if budget_reply.is_none() {
            let gated = crate::ferrite_sso::open_gate_reply(&ctx, &req, "buzz").await;
            if gated.handled() {
                if gated.ack() {
                    let _ = requests.ack(lease.token).await;
                }
                continue;
            }
        }

        let task_ctx = ctx.clone();
        let task_done = done_tx.clone();
        let token = lease.token;
        in_flight = in_flight.saturating_add(1);
        rt::spawn(async move {
            // Armed before any work: whatever happens from here, the loop hears
            // about it exactly once and the slot is returned.
            let mut completion = Completion {
                tx: task_done,
                ack: None,
            };
            let reply = if let Some(refusal) = budget_reply {
                refusal
            } else {
            let routed = route_idempotent(&task_ctx, &req).await;
            // The SDK's OWN envelope, not a hand-built literal: `IngressReply`
            // carries the reply header list, and the wire shape is unchanged
            // for a header-less reply.
            let mut reply =
                ferrite_app::IngressReply::new(&req.correlation_id, routed.status, routed.body);
            reply.headers.extend(routed.headers);
            reply
            };
            // Publish from HERE so the caller's answer never waits on the
            // loop's poll cycle. Each task takes its own replies client; the
            // stream and ws faces already own one per spawned pump.
            //
            // NOT pooled, and that is a measured decision. A queue client
            // carries lease and long-poll state, and
            // reusing ONE instance between this publish and the loop's own
            // consumer desynchronises it: pooling exactly this call makes a
            // 250 KiB submission's reply never arrive — three re-sends, six
            // consecutive reply-budget timeouts, with the app idle and nothing
            // logged (the `planes` module docs carry the measurement). A fresh
            // client per reply costs one dial; a lost reply costs the caller
            // its answer. The gateway supplies the exact ephemeral reply group
            // in the request. Publishing to a fixed `replies` binding leaves the
            // gateway waiting on a different queue.
            let published = match (
                serde_json::to_vec(&reply),
                task_ctx.reply_to_queue(&req.reply_to).await,
            ) {
                (Ok(payload), Ok(mut replies)) => match replies.publish(&payload).await {
                    Ok(_) => true,
                    Err(e) => {
                        // The caller will time out and retry; leaving the
                        // envelope unacked means the work is re-driven.
                        tracing::warn!("reply publish failed: {e}");
                        false
                    }
                },
                (Err(e), _) => {
                    tracing::error!("reply encode failed: {e}");
                    // Nothing to publish and nothing a redelivery would fix, so
                    // ack it away rather than loop on a poison envelope.
                    true
                }
                (_, Err(e)) => {
                    tracing::warn!("replies binding unavailable: {e}");
                    false
                }
            };
            completion.ack = published.then_some(token);
        });
    }
}

/// How long a duplicate delivery waits for the first attempt's answer before it
/// gives up and tells the caller to retry. Upstream's gateway HEDGES a request
/// unanswered after its hedge delay (up to three copies, first reply wins), so a
/// copy that gives up early answers 503 before a slow original can: the wait is
/// therefore most of the ingress deadline (8 s at the gateway), not a fraction of it.
const REPLAY_WAIT_MS: u64 = 6_000;

/// Poll step while waiting for the first attempt to record its answer.
const REPLAY_POLL_MS: u64 = 50;

/// [`route`], made safe to deliver **twice**.
///
/// The request queue is at-least-once, and this app leans on that: when a reply
/// cannot be published, [`api_loop`] deliberately leaves the envelope unacked so
/// the work is re-driven. That is right for a read and wrong for a write, and
/// the wrongness is not subtle. The redelivery re-enters [`route`], reaches
/// [`auth::consume_once`], finds the authorization event id already burned, and
/// answers `restricted: authorization event has already been used` — which
/// publishes fine and becomes the caller's answer. **The write succeeded and the
/// user is told it failed.** Observed live: a channel present in the sidebar
/// while its own creation dialog showed that refusal.
///
/// NIP-98 single use is supposed to mean *the action happens at most once*, not
/// *a second delivery errors*. So the authorization event id doubles as an
/// idempotency key: the first delivery records what it answered
/// ([`auth::record_outcome`]) and a duplicate is served that same answer. The
/// re-drive then delivers the original success, which is what leaving the
/// envelope unacked was trying to achieve in the first place.
///
/// Three paths, and the ordering keeps the common one free:
///
/// * **First delivery** — dispatch, then record the answer. One extra store
///   write; no extra read.
/// * **Duplicate whose first attempt finished** — the recorded answer, verbatim.
/// * **Duplicate that raced the first attempt** (a gateway hedge re-publishes
///   the identical envelope, in the platform gateway's request dispatch) — wait briefly
///   for the answer, then 503. Never a 4xx: a spurious auth failure is exactly
///   what misleads a client into reporting a completed action as rejected.
///
/// The replay record is consulted only when the dispatch already returned the
/// replay guard's own refusal, so an ordinary 401 (`not a member`, a stale
/// token) is passed straight through and costs nothing.
///
/// Security: a genuine replay of a captured token now receives the answer the
/// first delivery already produced, instead of a refusal. It gains nothing by
/// it — the same bytes, no second state change, and at-most-once execution is
/// unchanged. What is removed is only the misleading error.
async fn route_idempotent(ctx: &AppContext, req: &IngressRequest) -> Reply {
    let now_ms = ctx.now_ms();
    // Identify the request without burning anything: `verify_binding` does no
    // I/O and no clock check, so a redelivery that arrives after the freshness
    // window can still find its recorded answer. A route with no authorization
    // (healthz, relay-info) has no id and no side effect to protect.
    // `identify` does NOT re-verify the signature: the route does that, and
    // doing it twice per request is the single most expensive thing this wrapper
    // could add. It checks the id against the event's own fields, which is all
    // an idempotency key needs.
    let Some((auth_id, created_at)) =
        header(req, "authorization").and_then(|h| auth::identify(h, &req.method, &req.path))
    else {
        return route(ctx, req).await;
    };
    // Only a FRESH event ever reaches `consume_once`, so only a fresh one will
    // spend its authorization. A stale token is refused before the burn and must
    // leave no replay record behind — writing one would make a token that was
    // never used look used.
    let spends_authorization = (now_ms / 1000).abs_diff(created_at) <= auth::MAX_AUTH_SKEW_SECS;

    let reply = route(ctx, req).await;

    // Two 401s mean "this request may already have been answered": the replay
    // guard's own refusal, and a token now too old to admit — which is what a
    // re-drive delayed past the freshness window looks like. Both consult the
    // record, and both fall back to the refusal they were handed if there is
    // nothing recorded.
    //
    // The replay guard's refusal is only ever produced AFTER the route verified
    // the signature and bindings (`authenticate` checks the signature before
    // `consume_once`). A stale refusal is produced BEFORE that, so here the
    // header is still unverified: `identify` skips the signature and an event's
    // id does not cover `sig`, so a captured header with a forged signature
    // names the genuine id. Serving the record on that would hand anyone holding
    // a captured header the victim's answer without the victim's key. So the
    // stale path verifies signature and bindings first, waiving only freshness.
    let stale_but_authentic = !spends_authorization
        && header(req, "authorization")
            .is_some_and(|h| auth::verify_binding(h, &req.method, &req.path).is_ok());
    if reply.status == 401 && (is_replay_refusal(&reply.body) || stale_but_authentic) {
        let read_only =
            is_read_only_route(&req.method, crate::http_query::path_only(&req.path));
        return replayed_answer(ctx, &auth_id, reply, read_only).await;
    }
    // **Never record against a 401.** `identify` deliberately skips the
    // signature check, and an event's id does not cover its `sig` — so a header
    // with a FORGED signature still resolves to the genuine id. Recording its
    // refusal would write a replay row for a token that was never burned, which
    // is not merely untidy: it lets anyone who captured a header pre-empt the
    // victim's own use of it, because their real request then finds the id taken
    // and is served the planted refusal. A 401 always means the request failed
    // to authenticate or authorize, so there is no outcome worth keeping.
    //
    // The cost is that a post-burn 401 (`not a member`) is not cached either, so
    // a re-drive of one takes the wait-then-503 path instead of being answered
    // directly. That is a retryable answer rather than a false one, which is the
    // trade worth making.
    if reply.status == 401 || !spends_authorization || !is_terminal_outcome(reply.status) {
        return reply;
    }
    // Reads are not recorded, and this is a cost decision with a known cost.
    //
    // Every recorded outcome means a `planes::store(ctx, )` — which builds a WHOLE NEW
    // client, not a handle (`AppContext::store` → `fc()`, `ferrite-app`) — plus a
    // write, on a route that had already built its own. Skipping the reads takes
    // that off `/query`, `/window` and `/feed`, which are the bulk of all traffic.
    //
    // What it costs: a re-driven READ is not answered from the record. It
    // is NOT "harmlessly re-executed" either — the re-drive still meets the burnt
    // authorization id, so it reaches `replayed_answer`, finds the bare claim,
    // and answers the retryable 503. So the trade is "a read re-drive returns 503
    // instead of the answer", not "a read re-drive returns the answer twice".
    // That is acceptable because a read retry is cheap and the client's retry
    // mints a fresh authorization event, and it is emphatically NOT acceptable
    // for a write, where the effect is already durable and 503 would be the lie
    // this whole mechanism exists to prevent.
    //
    // The list is of READ-ONLY routes, so the default is to record. A route added
    // later is protected until someone deliberately declares it side-effect-free.
    if is_read_only_route(&req.method, crate::http_query::path_only(&req.path)) {
        return reply;
    }

    // First delivery: remember the answer so a re-drive serves THIS rather than
    // a refusal. Best effort by contract — the request already happened, and
    // failing it now because bookkeeping failed would invent the very problem
    // this exists to remove.
    //
    // The record keeps the status and the body, not the headers, and that costs
    // nothing to reconstruct: only NIP-98 routes reach this wrapper
    // (`auth::identify` accepts the `Nostr` scheme alone), every one of them
    // answers JSON, and [`replayed_answer`] rebuilds the reply through
    // [`Reply::json`]. The two routes that set a header of their own — `/mcp`
    // and `/acp` — authenticate with bearer tokens and are never recorded here.
    match planes::store(ctx, store::CONFIG) {
        Ok(mut config) => {
            let expires_at = now_ms.saturating_add(auth::REPLAY_TTL_MS);
            if let Err(e) = auth::record_outcome(
                &mut config,
                DEFAULT_COMMUNITY,
                &auth_id,
                reply.status,
                &reply.body,
                expires_at,
            )
            .await
            {
                tracing::warn!("replay outcome not recorded: {e}");
            }
        }
        Err(e) => tracing::warn!("replay outcome not recorded: {e}"),
    }
    reply
}

/// Does this request change nothing, so that losing its recorded outcome costs
/// only a retry?
///
/// Every `GET` here is a read by HTTP convention. The named `POST`s are reads
/// that use a body to carry a filter — buzz's protocol posts its queries — and
/// each was checked against its handler for writes: `run_query`, `channel_window`
/// and `feed` build a page out of index reads, and `reactions` reads tallies.
/// Everything else is treated as state-changing, INCLUDING routes that only
/// *sometimes* write (`presence`, `subscriptions`), because the cost of being
/// wrong in that direction is one extra store write, and the cost of being wrong
/// in the other is telling a caller their durable change failed.
fn is_read_only_route(method: &str, path: &str) -> bool {
    if method.eq_ignore_ascii_case("GET") {
        return matches!(
            path,
            "/buzz/api/healthz"
                | "/health"
                | "/_liveness"
                | "/_readiness"
                | "/info"
                | "/buzz/info"
                | "/buzz/api/relay-info"
                | "/buzz/api/nip05"
                | "/buzz/api/metrics"
                | "/buzz/api/join-policy"
                | "/buzz/api/me"
                | "/buzz/api/members"
                | "/buzz/api/agent-harnesses"
                | "/buzz/api/agent-models"
                | "/buzz/api/agents"
                | "/buzz/api/archived-identities"
                | "/mcp"
                | "/buzz/api/mcp/connectors"
                | "/buzz/api/media/limits"
                | "/buzz/api/operator/config"
        );
    }
    matches!(
        path,
        "/buzz/api/query"
            | "/buzz/api/window"
            | "/buzz/api/feed"
            | "/buzz/api/search"
            | "/buzz/api/reactions"
            // The upstream wire-compat aliases that only read.
            | "/query"
            | "/count"
            // Listing a connector's tools changes nothing. `/mcp` as a whole is
            // NOT listed: `tools/call` can reach a tool that does, and this
            // table's default-to-recording is what protects a route added later.
            | "/buzz/api/mcp/tools"
    )
}

/// Whether a reply is the request's settled answer, worth recording for a
/// duplicate delivery. A 5xx means the work may be unfinished, so it is not
/// recorded: a duplicate then waits and gets the retryable 503 instead of being
/// handed the failure for the whole replay TTL.
fn is_terminal_outcome(status: u16) -> bool {
    status < 500
}

/// Is this reply the replay guard's own refusal?
///
/// Matched on [`auth::REPLAYED_MESSAGE`], the one sentence only that guard
/// emits, so an ordinary 401 is never mistaken for a duplicate delivery.
fn is_replay_refusal(body: &[u8]) -> bool {
    core::str::from_utf8(body).is_ok_and(|text| text.contains(auth::REPLAYED_MESSAGE))
}

/// Serve what the first delivery of this authorization event answered.
///
/// Falls back to the refusal it was handed whenever the record cannot say
/// otherwise — an absent record means the 401 was not this guard after all.
///
/// The recorded answer is rebuilt as a JSON reply: the record keeps status and
/// body, and every route this wrapper protects answers JSON (see the note in
/// [`route_idempotent`]).
///
/// `read_only` is [`is_read_only_route`] for the request. Such a route never
/// records an outcome, so its claim stays a bare `InFlight` for the whole replay
/// TTL and waiting for it can only end in the same 503. Parking on it anyway let
/// one signed read, replayed ~33 times at once, hold every `MAX_IN_FLIGHT` slot
/// for the full poll budget and stall the whole request face; so a read's
/// duplicate is answered at once, exactly as a `NotKept` record is.
async fn replayed_answer(
    ctx: &AppContext,
    auth_id: &crate::relay::event::EventId,
    refusal: Reply,
    read_only: bool,
) -> Reply {
    // A client is built here rather than shared: this path needs a READ, it is
    // reached only for a duplicate or a stale token, and it must not contend with
    // the recorder's single writer.
    let Ok(mut config) = planes::store(ctx, store::CONFIG) else {
        return refusal;
    };
    let rounds = REPLAY_WAIT_MS / REPLAY_POLL_MS;
    for round in 0..=rounds {
        match auth::replay_state(&mut config, DEFAULT_COMMUNITY, auth_id).await {
            Ok(Some(auth::Replay::Answered {
                status: was,
                body: answer,
            })) => return Reply::json(was, answer),
            // Claimed but unanswered: the first attempt is still running or
            // died. Worth giving it a moment — unless the route never records
            // an answer, in which case there is nothing to wait for.
            Ok(Some(auth::Replay::InFlight)) if read_only => break,
            Ok(Some(auth::Replay::InFlight)) => {}
            // The first attempt FINISHED; its reply was just too large to keep.
            // There is nothing to wait for, so do not park — waiting here on a
            // replayable oversized read is a remote way to consume every
            // in-flight slot for the poll budget.
            Ok(Some(auth::Replay::NotKept { .. })) => break,
            // No record at all, so the refusal did not come from this guard.
            Ok(None) => return refusal,
            Err(e) => {
                tracing::warn!("replay record unreadable: {e}");
                return refusal;
            }
        }
        if round < rounds {
            rt::time::sleep(Duration::from_millis(REPLAY_POLL_MS)).await;
        }
    }
    tracing::warn!("duplicate delivery found no recorded answer; asking for a retry");
    Reply::json(
        503,
        br#"{"error":"unavailable: this request is still being processed; retry it"}"#.to_vec(),
    )
}

/// Route one request. Every arm returns a [`Reply`] — a status, its response
/// headers and a body; errors become the protocol's own reason-prefixed JSON so
/// a client sees the same vocabulary buzz used.
async fn route(ctx: &AppContext, req: &IngressRequest) -> Reply {
    let path = crate::http_query::path_only(&req.path);
    let method = req.method.as_str();

    let outcome = match (method, path) {
        ("GET", "/buzz/api/healthz") => Ok(Reply::text(200, b"ok".to_vec())),
        // The ORIGIN. The deploy spec's `/` api route is the longest-prefix
        // catch-all, so a signed-in browser that lands on the bare origin —
        // straight from the auth round trip — reaches this arm instead of the
        // gateway's 404. The app's web UI is the content route at `/buzz/ui/`;
        // send the browser there. Anonymous browsers never get this far: the
        // auth policy on `/` forces the sign-in redirect at the gateway.
        ("GET" | "HEAD", "/") => {
            Ok(Reply::bodyless(303).with_header(HEADER_LOCATION, "/buzz/ui/"))
        }
        // Browsers probe this beside every navigation; answer without a body
        // instead of the JSON "no route" fallthrough.
        ("GET" | "HEAD", "/favicon.ico") => Ok(Reply::bodyless(404)),
        // The web client. Upstream's gateway consults a `content` route only when no `api` or `stream`
        // route matched (docs/building-on-upstream-core.md, "Route precedence"), and the `/` catch-all
        // above is an api route, so the `/buzz/ui/` content route never answers there. The bundle is
        // served from here instead: the pushed one once committed, the embedded one until then.
        ("GET" | "HEAD", p) if p.starts_with("/buzz/ui/") => {
            Ok(ui_asset(ctx, p, method == "HEAD").await)
        }
        // Uploaded media, for the same reason: the `/buzz/blob/` content route is
        // shadowed by the `/` api catch-all, so without this arm every URL the
        // media routes hand out fell through to "no route".
        ("GET" | "HEAD", p) if p.starts_with(MEDIA_URL_PREFIX) => {
            media_object(ctx, p, method == "HEAD").await
        }
        // Upstream wire-compat aliases.
        //
        // Stock block/buzz tooling — `buzz-cli`, `sprig`, the `buzz-acp`
        // harness — builds these paths at the relay ROOT, not under
        // `/buzz/api/`. They are thin adapters over the same handlers this
        // relay already serves (never a second ingest path), so a deployment
        // of this port is reachable by the upstream agent ecosystem
        // unmodified. The NIP-98 binding needs no relaxation for it:
        // `auth::url_covers_path` already accepts the absolute URL those
        // clients put in the `u` tag.
        ("GET", "/health" | "/_liveness" | "/_readiness") => Ok(Reply::text(200, b"ok".to_vec())),
        ("GET", "/info") => Ok(Reply::json(200, relay_info(ctx).await)),
        // The relay-served browser client derives its HTTP base from its
        // WebSocket URL (`wss://host/buzz/` → `https://host/buzz`) and asks for
        // upstream's NIP-11 path under it (buzz-web `features/gifs/relay.ts`, the
        // GIF picker's and the MCP connectors' capability probe). That path falls
        // under the deploy spec's `/buzz/` prefix, not the root `/info` one, so it
        // needs its own arm with the SAME document.
        ("GET", "/buzz/info") => Ok(Reply::json(200, relay_info(ctx).await)),
        ("POST", "/events") => submit_event(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/query") => upstream_query(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/count") => upstream_count(ctx, req).await.map(|v| Reply::json(200, v)),
        ("GET", "/buzz/api/relay-info") => Ok(Reply::json(200, relay_info(ctx).await)),
        ("GET", "/buzz/api/nip05") => nip05(ctx, req).await.map(|v| Reply::json(200, v)),
        ("GET", "/buzz/api/metrics") => metrics(ctx, req).await.map(|v| Reply::json(200, v)),
        ("GET", "/buzz/api/join-policy") => join_policy(ctx).await.map(|v| Reply::json(200, v)),
        ("GET", "/buzz/api/me") => me(ctx, req).await.map(|v| Reply::json(200, v)),
        ("GET", "/buzz/api/members") => members(ctx, req).await.map(|v| Reply::json(200, v)),
        ("GET", "/buzz/api/agent-harnesses") => agent_harnesses(ctx).await.map(|v| Reply::json(200, v)),
        // The CLOUD-AGENT surface. A browser cannot start a local harness
        // process, but it does not need to: this deployment runs the persona
        // turn pipeline itself, so creating a cloud teammate is a
        // relay write, not a desktop capability.
        ("GET", "/buzz/api/agent-models") => agent_models(ctx).await.map(|v| Reply::json(200, v)),
        ("GET", "/buzz/api/agents") => list_agents(ctx).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/agents") => create_agent(ctx, req).await.map(|v| Reply::json(201, v)),
        ("POST", "/buzz/api/agents/remove") => remove_agent(ctx, req).await.map(|v| Reply::json(200, v)),
        // Retire a project (NIP-MP kind 30621). Its creator can already do this
        // with a NIP-09 `a` deletion; this route adds the community moderators
        // and operators, because a project whose creator is gone could
        // otherwise never be removed.
        ("POST", "/buzz/api/projects/delete") => delete_project(ctx, req).await.map(|v| Reply::json(200, v)),
        // Pause or resume a persona without retiring it, read its memory, and
        // read or write the definition a cloud agent is configured by — there is
        // no local harness process to hold a "session", so the definition IS the
        // configuration surface.
        ("POST", "/buzz/api/agents/active") => set_agent_active(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/agents/memory") => agent_memory(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/agents/config") => agent_config(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/agents/owner") => agent_owner_route(ctx, req).await.map(|v| Reply::json(200, v)),
        // Approve one gated tool on one thread, or revoke that approval (the
        // persona's owner or an operator).
        ("POST", "/buzz/api/agents/approve") => crate::relay::agent_approvals::approve_tool(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/agents/revoke") => crate::relay::agent_approvals::revoke_tool(ctx, req).await.map(|v| Reply::json(200, v)),
        // Community membership WRITES. The authority is split: a moderator adds
        // and removes plain members, and only an operator grants or revokes
        // moderator status — see `members_role` for why that split is the whole
        // reason these can exist at all.
        ("POST", "/buzz/api/members/add") => members_add(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/members/remove") => members_remove(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/members/role") => members_role(ctx, req).await.map(|v| Reply::json(200, v)),
        // SCHEDULED agent turns. A wake is a delayed publish of the same
        // `TurnRequest` a mention makes, so these three routes are the whole
        // surface: declare one, see what is pending, stop one.
        ("POST", "/buzz/api/schedules") => schedules_create(ctx, req).await.map(|v| Reply::json(201, v)),
        ("POST", "/buzz/api/schedules/list") => schedules_list(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/schedules/cancel") => {
            schedules_cancel(ctx, req).await.map(|v| Reply::json(200, v))
        }
        // Workflow RUNS and gates are documents, not events, so the ordinary
        // read path cannot reach them.
        ("POST", "/buzz/api/workflows/runs") => workflow_runs(ctx, req).await.map(|v| Reply::json(200, v)),
        ("GET", "/buzz/api/archived-identities") => {
            archived_identities_route(ctx, req).await.map(|v| Reply::json(200, v))
        }
        ("POST", "/buzz/api/link-preview") => link_preview(ctx, req).await.map(|v| Reply::json(200, v)),
        // MCP 2026-07-28.
        //
        // `/mcp` and `/.well-known/` sit OUTSIDE `/buzz/api/` because both are
        // addresses a third-party MCP client and its authorization server expect
        // at fixed locations: the resource identifier IS the endpoint URL, and
        // RFC 9728 puts the metadata document at the origin's well-known path.
        // The platform's route validator forbids only malformed and duplicate
        // prefixes, so both are legal app routes.
        ("POST", "/mcp") => {
            crate::relay::mcp_surface::serve_mcp(ctx, req, header(req, "authorization")).await
        }
        // A GET on the endpoint is a client at the right address with the wrong
        // method (the transport is POST-only). 405 with a JSON-RPC body, not the
        // 404 fallthrough that says the endpoint does not exist.
        ("GET", "/mcp") => crate::relay::mcp_surface::method_not_allowed(),
        // ACP v1.
        //
        // The perpendicular protocol: `/mcp` connects an agent to TOOLS, `/acp`
        // connects an EDITOR to this deployment's personas. Outside `/buzz/api/`
        // for the same reason — an editor is configured with the endpoint URL.
        ("POST", "/acp") => {
            crate::relay::acp_surface::serve_acp(ctx, req, header(req, "authorization")).await
        }
        ("GET", p)
            if p == "/.well-known/oauth-protected-resource"
                || p.starts_with("/.well-known/oauth-protected-resource/") =>
        {
            // A cheap two-shape filter. The EXACT path is decided in the handler,
            // which derives it from the configured resource identifier — the same
            // derivation a client performs — so a suffix like `…/evil` reaches a
            // 404 rather than the document.
            crate::relay::mcp_surface::protected_resource_metadata(ctx, p)
                .await
                .map(|v| Reply::json(200, v))
        }
        ("GET", "/buzz/api/mcp/connectors") => crate::relay::mcp_surface::connectors(ctx, req)
            .await
            .map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/mcp/authorize") => crate::relay::mcp_surface::authorize(ctx, req)
            .await
            .map(|v| Reply::json(200, v)),
        ("GET", "/buzz/api/mcp/callback") => crate::relay::mcp_surface::callback(ctx, req)
            .await
            .map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/mcp/disconnect") => crate::relay::mcp_surface::disconnect(ctx, req)
            .await
            .map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/mcp/tools") => crate::relay::mcp_surface::connector_tools(ctx, req)
            .await
            .map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/mcp/call") => crate::relay::mcp_surface::connector_call(ctx, req)
            .await
            .map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/mcp/token") => crate::relay::mcp_surface::mint_token(ctx, req)
            .await
            .map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/invites") => mint_invite(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/invites/claim") => claim_invite(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/invites/accept-policy") => {
            accept_policy(ctx, req).await.map(|v| Reply::json(200, v))
        }
        ("POST", "/buzz/api/events") => submit_event(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/filter-query") => upstream_query(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/query") => run_query(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/feed") => feed(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/search") => search_route(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/channels") => create_channel(ctx, req).await.map(|v| Reply::json(201, v)),
        ("POST", "/buzz/api/window") => channel_window(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/presence") => presence(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/reactions") => reactions(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/subscriptions") => subscribe(ctx, req).await.map(|v| Reply::json(200, v)),
        // The admission ticket for the WebSocket face. It proves the socket
        // was opened by a caller of this HTTP face, which the socket itself
        // cannot be checked for; with the open gate that caller may be
        // anonymous, and NIP-42 on the socket proves the workspace identity.
        ("POST", "/buzz/api/ws-ticket") => ws_ticket(ctx).await.map(|v| Reply::json(200, v)),
        // The MEDIA surface. The read half is `GET /buzz/blob/{sha}.{ext}`, served
        // above from the `media` binding (the manifest's content route for it is
        // shadowed by the `/` api catch-all); these are the write half.
        ("GET", "/buzz/api/media/limits") => media_limits().map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/media/begin") => media_begin(ctx, req).await.map(|v| Reply::json(200, v)),
        ("POST", "/buzz/api/media/chunk") => media_stage_chunk(ctx, req).await.map(|v| Reply::json(200, v)),
        // The operator plane: GET/POST only, because the gateway 405s
        // PUT/DELETE/PATCH before the app ever sees them (verified live).
        // NIP-98's exact-path binding is doing real work in this family: a
        // token minted for writing key A can never write key B, because the
        // key IS the path.
        ("GET", "/buzz/api/operator/config") => {
            operator_config_list(ctx, req).await.map(|v| Reply::json(200, v))
        }
        ("GET", p) if p.starts_with(OPERATOR_CONFIG_PREFIX) => {
            operator_config_read(ctx, req, p).await.map(|v| Reply::json(200, v))
        }
        ("POST", p) if p.starts_with(OPERATOR_CONFIG_PREFIX) => {
            operator_config_write(ctx, req, p).await.map(|v| Reply::json(200, v))
        }
        ("POST", "/buzz/api/operator/feed/retire") => {
            operator_feed_retire(ctx, req).await.map(|v| Reply::json(200, v))
        }
        ("POST", "/buzz/api/operator/assets/begin") => {
            operator_assets_begin(ctx, req).await.map(|v| Reply::json(200, v))
        }
        ("POST", "/buzz/api/operator/assets/chunk") => {
            operator_assets_chunk(ctx, req).await.map(|v| Reply::json(200, v))
        }
        ("POST", "/buzz/api/operator/assets/finish") => {
            operator_assets_finish(ctx, req).await.map(|v| Reply::json(200, v))
        }
        _ => Err(BuzzError::NotFound(format!("no route for {method} {path}"))),
    };

    match outcome {
        Ok(reply) => reply,
        Err(e) => {
            let status = http_status(&e);
            // `Display` already begins with the wire token (see
            // `BuzzError::reason_prefix`), so the reason IS the rendered error.
            // Prefixing again put `invalid: invalid: …` on the wire, which is not
            // a token any client branches on.
            let body = serde_json::to_vec(&serde_json::json!({
                "ok": false,
                "error": e.to_string(),
            }))
            .unwrap_or_else(|_| b"{\"ok\":false}".to_vec());
            let reply = Reply::json(status, body);
            // RFC 6585 §4: a 429 that names no wait leaves a client to guess,
            // and the guesses that matter are "immediately" and "never". Every
            // 429 this relay produces comes through `http_status`, so the
            // header is set in the one place that decides the status.
            if status == 429 {
                reply.with_header(HEADER_RETRY_AFTER, RETRY_AFTER_SECS)
            } else {
                reply
            }
        }
    }
}

/// Map a domain error onto an HTTP status.
const fn http_status(e: &BuzzError) -> u16 {
    match e {
        // The credential is the problem: authenticating again can fix it.
        BuzzError::Unauthenticated(_) => 401,
        // Authenticated and refused, or under a standing sanction: authenticating
        // again cannot help, and a 401 would make a standard client try. Falling
        // through to 400 would report a ban as a malformed request.
        BuzzError::Unauthorized(_) | BuzzError::Blocked(_) => 403,
        BuzzError::NotFound(_) => 404,
        BuzzError::Conflict(_) => 409,
        BuzzError::RateLimited => 429,
        BuzzError::Storage(_) | BuzzError::Internal(_) => 500,
        _ => 400,
    }
}

/// May this pubkey hand the deployment durable state, RIGHT NOW?
///
/// The two checks every write path runs, in the order that matters:
///
/// 1. **The closed-community gate.** An invite-only community refuses writes from
///    a pubkey no claim ever admitted. First, so an outsider and a banned member
///    read the same from outside — both are refusals before any work.
/// 2. **The standing sanction.** A banned or timed-out member cannot write, and
///    the refusal SENTENCE comes from the restriction state rather than being
///    written at the call site, because it is a client contract:
///    `buzz-web`'s composer banner arms only on
///    `restricted: you are timed out until <epoch-seconds>` and renders the
///    countdown it reads out of that number.
/// 3. **The identity archive.** An archived identity is retired: it cannot
///    post, react, upload or send another request, including the unarchive,
///    until a moderator lifts the archive ([`crate::relay::archive`]).
///
/// ONE function rather than a copy per route, and that is load-bearing rather
/// than tidy: the media upload path was written with these checks at `begin` and
/// not on the chunks, which made upload permission a capability frozen at the
/// start of an hour-long session instead of a live gate — precisely the window a
/// moderator's ban is supposed to close.
///
/// Fail-closed on a restriction read, by construction: the error propagates.
async fn enforce_write_gate(
    ctx: &AppContext,
    community: &str,
    actor: &crate::relay::event::PubKey,
    now_ms: u64,
) -> Result<()> {
    enforce_community_membership(ctx, community, actor).await?;
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    if let Some(restriction) =
        crate::relay::effects::restriction_of(&mut events, community, actor).await?
    {
        if let Some(block) = restriction.write_block(now_ms) {
            return Err(BuzzError::Unauthorized(block.refusal_detail()));
        }
    }
    if crate::relay::archive::is_archived(&mut events, community, actor).await? {
        return Err(BuzzError::Unauthorized(
            crate::relay::archive::ARCHIVED_REFUSAL.into(),
        ));
    }
    Ok(())
}

/// [`enforce_write_gate`], for a module outside this one.
///
/// The MCP surface reaches an external system on the workspace's behalf, which
/// spends this deployment's egress budget as the relay — a write-shaped act, so
/// it runs a write's gate. Re-exported rather than reimplemented: "may this
/// pubkey hand us durable state" must have exactly one answer.
pub(crate) async fn write_gate(
    ctx: &AppContext,
    community: &str,
    actor: &crate::relay::event::PubKey,
    now_ms: u64,
) -> Result<()> {
    enforce_write_gate(ctx, community, actor, now_ms).await
}

/// [`caller_is_operator`], for a module outside this one.
pub(crate) async fn is_operator(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    who: &crate::relay::event::PubKey,
) -> Result<bool> {
    caller_is_operator(ctx, config, who).await
}

/// [`may_manage_agent`], for a module outside this one.
///
/// The ACP surface runs a persona turn with the persona's own access and
/// returns what it produced to the caller, which reveals as much as reading the
/// agent's memory does, so it asks the same question that route asks.
pub(crate) async fn may_manage(
    ctx: &AppContext,
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    agent: &str,
    who: &crate::relay::event::PubKey,
) -> Result<bool> {
    may_manage_agent(ctx, config, community, agent, who).await
}

/// [`header`], for a module outside this one.
pub(crate) fn request_header<'a>(req: &'a IngressRequest, name: &str) -> Option<&'a str> {
    header(req, name)
}

/// [`auth::authenticate`] over the request's own authorization header, method and path.
pub(crate) async fn authenticate_request(
    config: &mut ferrite_app::client::StoreClusterClient,
    community: &str,
    req: &IngressRequest,
    now_ms: u64,
) -> Result<auth::Caller> {
    auth::authenticate(
        config,
        community,
        header(req, "authorization"),
        &req.method,
        &req.path,
        req.verified_host(),
        now_ms,
    )
    .await
}

/// [`storage`], for a module outside this one.
pub(crate) fn storage_error(e: io::Error) -> BuzzError {
    storage(e)
}

/// Read the next feed page, waiting up to [`delivery::WAIT_MS`] when there is
/// nothing new: the records and the topic's retained base, for
/// [`delivery::check_wait_continuity`]. `Ok(None)` means the page cannot be
/// delivered within its byte bound, and the caller resynchronizes.
///
/// With the `platform-topic-read-wait` feature they call the platform's parked
/// `read_wait`, which also reports the retained base. The released platform
/// builder has no such method (recorded platform gap 04, topic read wait), so the
/// default build reads with `read_from` and passes the result here: this sleeps
/// on an empty page and reports the requested offset as the base, so loss is
/// detected by the first returned offset running ahead of the cursor
/// ([`delivery::check_continuity`]), because `read_from` clamps a
/// below-retention cursor forward.
///
/// The record type is never named: the released builder SDK does not export
/// `ferrite_app::client::WireRecord` (the released builder refuses the path with
/// E0425), so this is generic over whatever the read returns.
#[cfg_attr(feature = "platform-topic-read-wait", allow(dead_code))]
async fn released_feed_page<R>(
    read: io::Result<Vec<R>>,
    from: u64,
    size: impl Fn(&R) -> usize,
) -> io::Result<Option<(Vec<R>, u64)>> {
    let records = read?;
    if records.is_empty() {
        ferrite_app::sleep_ms(delivery::WAIT_MS).await;
    }
    Ok(bound_feed_page(records, size).map(|records| (records, from)))
}

/// Keep the longest prefix within [`delivery::BATCH_BYTES`] of payload; the rest
/// is read next time from the advanced cursor. A single record larger than the
/// bound can never be delivered, which the caller answers by resynchronizing (the
/// same outcome `read_wait` reports as an oversized record).
#[cfg_attr(feature = "platform-topic-read-wait", allow(dead_code))]
fn bound_feed_page<R>(mut records: Vec<R>, size: impl Fn(&R) -> usize) -> Option<Vec<R>> {
    let mut bytes: u64 = 0;
    let mut keep = 0;
    for record in &records {
        let record_bytes = u64::try_from(size(record)).unwrap_or(u64::MAX);
        if bytes.saturating_add(record_bytes) > delivery::BATCH_BYTES {
            break;
        }
        bytes = bytes.saturating_add(record_bytes);
        keep += 1;
    }
    if keep == 0 && !records.is_empty() {
        return None;
    }
    records.truncate(keep);
    Some(records)
}

/// A forwarded header, lowercased by the gateway.
fn header<'a>(req: &'a IngressRequest, name: &str) -> Option<&'a str> {
    let direct = req
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str());
    if direct.is_some() || !name.eq_ignore_ascii_case("authorization") {
        return direct;
    }
    // Managed-auth routes deliberately refuse to forward the browser's
    // standard Authorization header into an app. The Buzz edge copies its
    // inner NIP-98/Bearer credential to this app-specific header so it cannot
    // collide with Ferrite's outer session authority.
    req.headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case("x-buzz-authorization"))
        .map(|(_, v)| v.as_str())
}

/// Read one query parameter out of a path that still carries its query string,
/// percent-decoded.
///
/// The grammar and the decode belong to [`crate::http_query`], the canonical
/// decoder; this is the spelling the relay's call
/// sites use. Four behaviours are load-bearing here:
///
/// * **Values are percent-decoded.** The stream face takes a cursor as
///   `from={generation}:{offset}`, and any standards-compliant client
///   (`URLSearchParams`, `encodeURIComponent`) sends the `:` as `%3A`. The raw
///   text makes [`Cursor::parse`] fail, and its caller treats a failed
///   parse as *no cursor supplied* — so a reconnecting client is silently
///   resumed at the session's recorded offset instead of the one it asked for,
///   replaying or skipping events with no error anywhere. Silent wrong-offset
///   resumption is the worst shape for this bug, which is why the decode lives
///   here rather than at one call site.
/// * **A valueless pair is skipped, not fatal.** `?debug&token=x` has a first
///   pair with no `=`; abandoning the search there would return `None`, so a
///   token that IS present would go unseen and the stream open would be refused.
///   One odd parameter must not break authentication.
/// * **The NAME is decoded too.** Comparing the raw name means `?to%6Ben=x`
///   never matches a lookup for `token` while `?token=x` does — two spellings of
///   one parameter with two answers.
/// * **A valueless pair yields `("name", "")`, not nothing.** Skipping it would
///   read `?debug` as absent rather than as present and empty, and a flag could
///   not be sent without a value.
#[must_use]
pub fn query_param(path: &str, name: &str) -> Option<String> {
    crate::http_query::target_param(path, name)
}

fn storage(e: io::Error) -> BuzzError {
    BuzzError::Storage(e.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    #[test]
    fn an_unreadable_closed_flag_is_an_error_not_an_open_community() {
        assert!(matches!(
            closed_flag(Err(io::Error::other("config down"))),
            Err(BuzzError::Storage(_))
        ));
        assert!(closed_flag(Ok(Some(b" true\n".to_vec()))).unwrap());
        assert!(!closed_flag(Ok(Some(b"false".to_vec()))).unwrap());
        assert!(!closed_flag(Ok(None)).unwrap());
    }

    #[test]
    fn a_transient_ws_open_or_close_failure_is_left_unacked() {
        let transient = || BuzzError::Storage("store down".into());
        let terminal = BuzzError::Unauthorized("needs a ticket".into());
        assert_eq!(ws_open_ack(7, &Ok(1)), Some(7));
        assert_eq!(ws_open_ack(7, &Err(transient())), None);
        assert_eq!(ws_open_ack(7, &Err(terminal)), Some(7));
        assert_eq!(ws_close_ack(9, &Ok(())), Some(9));
        assert_eq!(ws_close_ack(9, &Err(transient())), None);
    }

    #[tokio::test]
    async fn a_queued_ws_job_returns_its_slot_when_it_is_dropped_unserved() {
        let (done_tx, mut done_rx) = rt::mpsc::channel::<WsDone>(4);
        let mut req = IngressRequest::new("WS_MESSAGE", "/ws", Vec::new());
        req.correlation_id = "conn-1".into();
        drop(WsJob::new(req, 7, done_tx));
        let done = tokio::time::timeout(Duration::from_secs(1), done_rx.recv())
            .await
            .expect("a dropped job must report its slot")
            .expect("the channel is open");
        assert_eq!(done.cid, "conn-1");
        assert_eq!(done.ack, None);
    }

    #[test]
    fn a_failed_membership_write_is_not_reported_as_already_member() {
        let down: io::Result<std::result::Result<(), ()>> = Err(io::Error::other("store down"));
        assert!(matches!(claim_status(down), Err(BuzzError::Storage(_))));
        assert_eq!(claim_status(io::Result::Ok(Ok::<(), ()>(()))).unwrap(), "joined");
        assert_eq!(
            claim_status(io::Result::Ok(Err::<(), ()>(()))).unwrap(),
            "already_member"
        );
    }

    /// A definition row that a concurrent editor rewrites after EVERY read:
    /// read `n` sees `{"instructions": "v<n>"}` at version `n`.
    struct EditedBetweenReads {
        version: u64,
    }

    impl DefinitionRows for EditedBetweenReads {
        async fn versioned(&mut self, _key: &[u8]) -> Result<Option<(Vec<u8>, u64)>> {
            self.version += 1;
            let doc = serde_json::json!({
                "instructions": format!("v{}", self.version),
                "default_model": format!("model-{}", self.version),
            });
            Ok(Some((serde_json::to_vec(&doc).unwrap(), self.version)))
        }
    }

    /// **The config reply's fields and generation come from the same stored
    /// version, even when the row changes between reads.**
    ///
    /// The reply used to read the fields and then, separately, the generation.
    /// An edit landing between the two handed a client the OLD instructions under
    /// the NEW generation, so its next `if_generation` write passed the
    /// compare-and-swap and silently overwrote the edit it never saw.
    #[tokio::test]
    async fn the_config_reply_reads_fields_and_generation_from_one_version() {
        let mut rows = EditedBetweenReads { version: 0 };
        let (instructions, model, generation) =
            agent_config_view(&mut rows, "nova").await.unwrap();
        assert_eq!(
            instructions.as_deref(),
            Some(format!("v{generation}").as_str()),
            "the instructions belong to a different version than the generation"
        );
        assert_eq!(model.as_deref(), Some(format!("model-{generation}").as_str()));
    }

    #[tokio::test]
    async fn a_missing_definition_answers_generation_zero_and_no_fields() {
        struct Missing;
        impl DefinitionRows for Missing {
            async fn versioned(&mut self, _key: &[u8]) -> Result<Option<(Vec<u8>, u64)>> {
                Ok(None)
            }
        }
        assert_eq!(
            agent_config_view(&mut Missing, "nova").await.unwrap(),
            (None, None, 0)
        );
    }

    #[test]
    fn the_boot_reconcile_always_reaches_the_starter_rooms() {
        // Hex room ids sort before `general`, so a truncated scan-order list
        // would never reach it.
        let mut scanned: Vec<String> = (0..600).map(|n| format!("{n:08x}-room")).collect();
        scanned.push("general".into());
        scanned.push("ops".into());
        scanned.push("welcome-everyone".into());
        let ordered = starter_rooms_first(scanned);
        assert_eq!(ordered.first().map(String::as_str), Some("general"));
        assert_eq!(ordered.get(1).map(String::as_str), Some("welcome-everyone"));
        assert_eq!(ordered.len(), 603, "nothing dropped, nothing duplicated");
        let truncated: Vec<&String> = ordered.iter().take(MAX_RECONCILED_CHANNELS).collect();
        assert!(truncated.iter().any(|room| room.as_str() == "general"));
        assert!(truncated.iter().any(|room| room.as_str() == "welcome-everyone"));
        // A community without starter rooms keeps its scan order untouched.
        assert_eq!(starter_rooms_first(vec!["b".into(), "a".into()]), vec!["b".to_string(), "a".to_string()]);
    }

    /// A record shape of its own: the helper is generic, and the test must not
    /// depend on the SDK exporting its record type.
    struct TestRecord {
        offset: u64,
        value: Vec<u8>,
    }

    fn feed_record(offset: u64, value_bytes: usize) -> TestRecord {
        TestRecord {
            offset,
            value: vec![0u8; value_bytes],
        }
    }

    fn size(record: &TestRecord) -> usize {
        record.value.len()
    }

    /// The released-platform read keeps the longest prefix inside the byte bound
    /// and leaves the rest for the next read from the advanced cursor.
    #[test]
    fn a_feed_page_is_cut_at_the_byte_bound_not_dropped() {
        let half = usize::try_from(delivery::BATCH_BYTES / 2).unwrap();
        let page = bound_feed_page(
            vec![feed_record(7, half), feed_record(8, half), feed_record(9, 1)],
            size,
        )
        .unwrap();
        assert_eq!(page.iter().map(|r| r.offset).collect::<Vec<_>>(), vec![7, 8]);
    }

    /// One record that can never fit is the oversized case: the pump must
    /// resynchronize rather than sit on a cursor that never advances.
    #[test]
    fn a_record_larger_than_the_bound_cannot_be_paged() {
        let over = usize::try_from(delivery::BATCH_BYTES).unwrap() + 1;
        assert!(bound_feed_page(vec![feed_record(3, over)], size).is_none());
        assert_eq!(bound_feed_page(Vec::new(), size).map(|p| p.len()), Some(0));
    }
    /// A drained queue and a refused read must not print the same document.
    /// Zeros for a refusal is the fold described above; these two cases are the
    /// before and after of removing it.
    #[test]
    fn a_refused_queue_count_reads_as_unknown_not_as_empty() {
        let drained = requests_counts(Some((7, 7, 0, 0)));
        assert_eq!(drained["readable"], serde_json::json!(true));
        assert_eq!(drained["published"], serde_json::json!(7));
        assert_eq!(drained["pending"], serde_json::json!(0));
        assert_eq!(drained["dead_lettered"], serde_json::json!(0));

        let refused = requests_counts(None);
        assert_eq!(refused["readable"], serde_json::json!(false));
        for field in ["published", "acked", "dead_lettered", "pending"] {
            assert!(
                refused[field].is_null(),
                "a refused count must be null, not zero: {refused}"
            );
            assert_ne!(refused[field], drained[field]);
        }
    }

    #[test]
    fn query_params_are_extracted() {
        let p = "/buzz/sub/abc123?token=1000.deadbeef&from=2:17";
        assert_eq!(query_param(p, "token").as_deref(), Some("1000.deadbeef"));
        assert_eq!(query_param(p, "from").as_deref(), Some("2:17"));
        assert_eq!(query_param(p, "missing"), None);
        assert_eq!(query_param("/buzz/sub/abc", "token"), None);
    }

    /// A percent-encoded cursor is what every standards-compliant client
    /// sends (`URLSearchParams` encodes `:` as `%3A`). Before the decode, the
    /// value came back raw, `Cursor::parse` failed, and the caller read that
    /// as "no cursor" — silently resuming the stream at the wrong offset.
    #[test]
    fn a_percent_encoded_cursor_survives_the_round_trip() {
        let p = "/buzz/sub/s1?token=1000.dead&from=2%3A17";
        assert_eq!(query_param(p, "from").as_deref(), Some("2:17"));
        let cursor = query_param(p, "from")
            .and_then(|c| Cursor::parse(&c))
            .expect("an encoded cursor still parses");
        assert_eq!(cursor.generation, 2);
        assert_eq!(cursor.offset, 17);
        // The unencoded spelling keeps working — clients may send either.
        assert_eq!(
            Cursor::parse(&query_param("/buzz/sub/s1?from=2:17", "from").expect("present"))
                .expect("parses")
                .offset,
            17
        );
    }

    /// One valueless parameter must not hide a token that IS present: the old
    /// `?` on the pair split abandoned the whole search and refused the open.
    #[test]
    fn a_valueless_query_pair_does_not_hide_later_params() {
        let p = "/buzz/sub/s1?debug&token=1000.dead&from=0%3A0";
        assert_eq!(query_param(p, "token").as_deref(), Some("1000.dead"));
        assert_eq!(query_param(p, "from").as_deref(), Some("0:0"));
        assert_eq!(query_param("/buzz/sub/s1?onlyflag", "token"), None);
    }

    /// The two divergences the local reader still carried after those two
    /// fixes, both closed by delegating to the canonical decoder.
    #[test]
    fn the_name_is_decoded_and_a_valueless_pair_is_present_not_absent() {
        // The name was compared raw, so two spellings of one parameter gave
        // two answers: `?token=x` matched and `?to%6Ben=x` did not.
        assert_eq!(
            query_param("/buzz/sub/s1?to%6Ben=1000.dead", "token").as_deref(),
            Some("1000.dead")
        );
        assert_eq!(
            query_param("/buzz/api/nip05?first+name=ada", "first name").as_deref(),
            Some("ada")
        );
        // A pair with no `=` was skipped entirely, so a flag read as absent
        // rather than as present and empty.
        assert_eq!(query_param("/buzz/sub/s1?debug", "debug").as_deref(), Some(""));
        assert_eq!(
            query_param("/buzz/sub/s1?debug&token=x", "debug").as_deref(),
            Some("")
        );
        // Present-and-empty and valueless are deliberately the same answer.
        assert_eq!(query_param("/buzz/sub/s1?debug=", "debug").as_deref(), Some(""));
        // A second `?` belongs to the query: the local reader's `split_once`
        // kept it, and the canonical decoder still does, so `b` is reachable.
        assert_eq!(query_param("/p?a=1?b=2", "a").as_deref(), Some("1?b=2"));
    }

    #[test]
    fn channel_name_slugging_matches_the_client_rule() {
        // lowercase; runs of [^a-z0-9-_] collapse to one '-'; trim '-'; cap 64.
        assert_eq!(slugify_channel_name("Welcome"), "welcome");
        assert_eq!(slugify_channel_name("Growth  Team!"), "growth-team");
        assert_eq!(slugify_channel_name("  Q3 / Billing  "), "q3-billing");
        assert_eq!(slugify_channel_name("snake_case-ok"), "snake_case-ok");
        assert_eq!(slugify_channel_name("---dash---"), "dash");
        assert_eq!(slugify_channel_name("émoji 🎉 name"), "moji-name");
        assert_eq!(slugify_channel_name(""), "");
        assert_eq!(slugify_channel_name("🎉🎉🎉"), "");
        assert_eq!(slugify_channel_name(&"A".repeat(100)).len(), 64);
    }

    #[test]
    fn assigned_channel_ids_are_always_safe_components() {
        let creator = [7u8; 32];
        // A sluggable name uses its slug verbatim...
        assert_eq!(
            assigned_channel_id("Welcome", &creator, "corr-1"),
            "welcome"
        );
        // ...and an unsluggable one falls back to a salted room id, never a 400.
        for hostile in ["", "🎉🎉", "///", "!!!", "   "] {
            let id = assigned_channel_id(hostile, &creator, "corr-1");
            assert!(id.starts_with("room-"), "{hostile:?} -> {id}");
            assert_eq!(id.len(), "room-".len() + 12);
            assert!(keys::is_safe_component(&id), "{hostile:?} -> {id}");
        }
        // The fallback is a function of the REQUEST, not the clock: the same
        // correlation id remints the same id (redelivery lands on the room the
        // first drive made), and a different request mints a different room.
        assert_eq!(
            assigned_channel_id("🎉🎉", &creator, "corr-1"),
            assigned_channel_id("🎉🎉", &creator, "corr-1"),
        );
        assert_ne!(
            assigned_channel_id("🎉🎉", &creator, "corr-1"),
            assigned_channel_id("🎉🎉", &creator, "corr-2"),
        );
    }

    #[test]
    fn salted_candidates_differ_per_caller_and_attempt() {
        let ana = [1u8; 32];
        let bo = [2u8; 32];
        // The collision the retry exists for: two users whose rooms slug to
        // the same id must diverge at their FIRST retry.
        let a1 = salted_channel_id("welcome", "Welcome", &ana, "corr-a", 1);
        let b1 = salted_channel_id("welcome", "Welcome", &bo, "corr-b", 1);
        assert_ne!(a1, b1);
        // ...and one user's successive attempts differ too.
        let a2 = salted_channel_id("welcome", "Welcome", &ana, "corr-a", 2);
        assert_ne!(a1, a2);
        // Every candidate stays a safe component within the 64-byte bound.
        let long_base = "l".repeat(64);
        for attempt in 1..=CHANNEL_ID_RETRIES {
            let id = salted_channel_id(&long_base, "Long", &ana, "corr-a", attempt);
            assert!(id.len() <= 64, "{id}");
            assert!(keys::is_safe_component(&id), "{id}");
        }
        // Deterministic: the salt is the request's correlation id, so a
        // redelivered envelope walks the SAME candidate list.
        assert_eq!(
            a1,
            salted_channel_id("welcome", "Welcome", &ana, "corr-a", 1)
        );
    }

    #[test]
    fn only_named_reads_are_read_only_and_the_mcp_callback_is_not() {
        assert!(is_read_only_route("GET", "/buzz/api/me"));
        assert!(is_read_only_route("GET", "/buzz/api/mcp/connectors"));
        assert!(is_read_only_route("POST", "/buzz/api/query"));
        assert!(
            !is_read_only_route("GET", "/buzz/api/mcp/callback"),
            "the authorization callback exchanges a code and stores credentials"
        );
        assert!(!is_read_only_route("GET", "/buzz/api/a-route-added-later"));
        assert!(!is_read_only_route("POST", "/buzz/api/channels"));
    }

    #[test]
    fn a_server_failure_is_not_recorded_as_the_terminal_answer() {
        assert!(is_terminal_outcome(200));
        assert!(is_terminal_outcome(409));
        assert!(!is_terminal_outcome(500));
        assert!(!is_terminal_outcome(503));
    }

    #[test]
    fn statuses_map_from_domain_errors() {
        assert_eq!(http_status(&BuzzError::NotFound("x".into())), 404);
        assert_eq!(http_status(&BuzzError::Conflict("x".into())), 409);
        // 401 asks the caller to authenticate; 403 tells them not to bother. The
        // relay returned 401 for both until the credential failure got its own
        // variant, so a permission refusal made a standard client retry auth.
        assert_eq!(http_status(&BuzzError::Unauthenticated("x".into())), 401);
        assert_eq!(http_status(&BuzzError::Unauthorized("x".into())), 403);
        // A standing sanction is a permission answer, not a malformed request.
        assert_eq!(http_status(&BuzzError::Blocked("banned".into())), 403);
        assert_eq!(http_status(&BuzzError::RateLimited), 429);
        assert_eq!(http_status(&BuzzError::Storage("x".into())), 500);
        assert_eq!(http_status(&BuzzError::Internal("x".into())), 500);
        assert_eq!(http_status(&BuzzError::BadSignature), 400);
    }
}

// Numeric policy retained from the app manifest; shared durable allowance.
const ROUTE_BUDGETS: &[crate::http_route_budget::Rule] = &[
    crate::http_route_budget::Rule {
        route: "/health",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/_liveness",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/_readiness",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/buzz/api/",
        per_min: 1200,
    },
    crate::http_route_budget::Rule {
        route: "/events",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/query",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/count",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/info",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/mcp",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/acp",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/.well-known/",
        per_min: 600,
    },
    crate::http_route_budget::Rule {
        route: "/",
        per_min: 600,
    },
];

#[cfg(test)]
mod route_budget_tests {
    use super::*;
    #[tokio::test]
    async fn http_route_budget_precedes_public_or_auth_dispatch() {
        let env = ferrite_app_test::TestHarness::new()
            .with_queue("requests")
            .with_queue("replies")
            .with_store("config")
            .manual_clock(1_700_000_000_000)
            .build()
            .await;
        let mut payload = env.payload().clone();
        payload.secrets.insert(
            "sso_shared_secret".into(),
            "test-secret-at-least-thirty-two-bytes".into(),
        );
        payload
            .secrets
            .insert("sso_broker_origin".into(), "https://login.example".into());
        let ctx = ferrite_app::AppContext::with_clock(payload, env.clock());
        env.store("config").put(b"http-route-budget/v1//", &ferrite_app::serde_json::to_vec(
            &ferrite_app::serde_json::json!({"per_min":600,"tokens":60000,"at_ms":env.now_ms()})
        ).unwrap()).await.unwrap();
        env.run_app(move |_| async move { api_loop(ctx).await });
        let request = || {
            let mut req = ferrite_app_test::ingress::get("/healthz");
            req.route = "/".into(); // Native gateway's matched prefix.
            req
        };
        assert_eq!(env.handle(request()).await.unwrap().status, 200);
        let refusal = env.handle(request()).await.unwrap();
        assert_eq!(refusal.status, 429);
        assert!(refusal.headers.iter().any(
            |(name, value)| name == "retry-after" && value.parse::<u64>().is_ok_and(|n| n > 0)
        ));
        assert_eq!(
            ferrite_app::serde_json::from_slice::<ferrite_app::serde_json::Value>(&refusal.body)
                .unwrap()["error"],
            "route_budget_exhausted"
        );
        env.advance(1000);
        assert_eq!(env.handle(request()).await.unwrap().status, 200);
    }
    #[tokio::test]
    async fn declared_health_routes_are_admitted() {
        let env = ferrite_app_test::TestHarness::new()
            .with_queue("requests").with_queue("replies").with_store("config")
            .manual_clock(1_700_000_000_000).build().await;
        let mut payload = env.payload().clone();
        payload.secrets.insert("sso_shared_secret".into(), "test-secret-at-least-thirty-two-bytes".into());
        payload.secrets.insert("sso_broker_origin".into(), "https://login.example".into());
        let ctx = ferrite_app::AppContext::with_clock(payload, env.clock());
        env.run_app(move |_| async move { api_loop(ctx).await });
        for path in ["/health", "/_liveness", "/_readiness"] {
            let mut req = ferrite_app_test::ingress::get(path);
            req.route = path.into();
            let reply = env.handle(req).await.unwrap();
            assert_eq!(reply.status, 200, "{path}: {}", String::from_utf8_lossy(&reply.body));
            assert_eq!(reply.body, b"ok");
        }
    }
}
