//! **The headline proof.** A real LLM-driven agent, built with this SDK, doing
//! real work on this repository, through the real relay, verified against the
//! relay's own durable state.
//!
//! Both gates in this file are `#[ignore]`d because they call a paid model. Run
//! them explicitly:
//!
//! ```text
//! cargo test --test live_agent -- --ignored --nocapture
//! ```
//!
//! ## What is real here, and what is not
//!
//! The fleet is `ferrite-app-test`'s in-process fleet — the real queue, store,
//! APE and topics engines — and the app under test is one live instance of
//! `buzz::relay::ingress::serve` consuming its own request queue. So every
//! request in this file pays for:
//!
//! * **real routing** — the app's own `route()`, by method and path;
//! * **real NIP-98 auth** — a kind-27235 event per request, Schnorr-verified,
//!   bound to method + path, and burned on first use by the relay's replay guard;
//! * **real channel authorization** — the channel is PRIVATE, so `submit_event`
//!   resolves the channel document off the APE plane and the membership row off
//!   the Store plane before it will accept a write, and `run_query` /
//!   the live pump re-authorize every event through `access::may_deliver`;
//! * **real ingest** — the guarded-saga admission marker, the Store writes and
//!   indexes, the audit chain, and the durable delivery feed;
//! * **the real stream face** — `POST /buzz/api/subscriptions` mints a session
//!   and a stream token, and the agent's frames come off the app's own SSE pump
//!   (`stream_loop` → `run_pump` → the frame queue), not off a poll loop.
//!
//! What it does **not** exercise: the HTTP hop and the gateway process. The
//! transport hands `IngressRequest`s to the app's request queue in the same
//! envelope shape the gateway publishes, which is the one seam this file
//! bypasses.
//!
//! ## Why the transport signs its own authorization events
//!
//! [`Identity::authorization_header`] builds the header from `(method, url,
//! created_at)` alone. Signing is deterministic, `created_at` is in whole
//! seconds, and the relay burns an authorization event's id on first use — so
//! two requests to the same route inside the same second mint the *same* event
//! and the second is refused as a replay. This transport therefore adds a
//! `nonce` tag, exactly as the relay's own fixtures do and for exactly the
//! same reason. See the note on [`nip98_header`].

// The live gates drive a real model through `AnthropicBackend`, which only the
// `http` feature builds, so the I/O-free build compiles this file out.
#![cfg(feature = "http")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::io;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use buzz::relay::channel::Role;
use buzz::relay::crypto::sha256::sha256;
use buzz::relay::crypto::to_hex;
use buzz::relay::delivery::Frame;
use buzz::relay::event::{Event, PubKey};
use buzz::relay::filter::Filter;
use buzz::relay::{b64, keys, kind, store, DEFAULT_COMMUNITY};

use buzz_sdk::error::{Result as SdkResult, SdkError};
use buzz_sdk::{
    tool, Agent, BoxFuture, Identity, QueryPage, QueryRequest, SessionHandle, Transport, Turn,
    Workspace,
};

use ferrite_app::client::StoreClusterClient;
use ferrite_app::{IngressReply, IngressRequest, StreamFrame};
use ferrite_app_test::{ingress, TestHarness, WsSession};

use serde::Deserialize;
use serde_json::{json, Value};
use tokio::sync::{mpsc, oneshot};

// ───────────────────────────────────────────────────────────── configuration

/// The model these gates drive. A current id — see the crate README for the
/// vocabulary; `claude-opus-4-8` also works and costs more.
const MODEL: &str = "claude-sonnet-4-6";

/// The relay serves only signed-in visitors: its request loop runs the Ferrite
/// SSO property gate before any route (`relay/ingress.rs`, `ferrite_sso::gate_reply`).
/// The fleet injects a disposable shared secret and every request carries a
/// session minted from it, exactly as `apps/buzz/tests/common` does. NIP-98 still
/// decides who is calling inside the relay.
const TEST_SSO_SECRET: &str = "buzz-sdk-disposable-sso-test-only";

fn signed_in(mut req: IngressRequest, now_ms: u64) -> IngressRequest {
    let session = buzz::ferrite_sso::session_cookie_pair(TEST_SSO_SECRET, "buzz", "buzz-sdk-live-gate", now_ms)
        .expect("fixture session");
    req.headers.push(("cookie".into(), session));
    req
}

/// The directory `read_source` is allowed to read, resolved from this crate's
/// manifest so the gate is not pinned to one checkout path.
const SOURCE_ROOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/../buzz/src/relay");

/// Per-request reply budget for the fleet. Sized off the platform's own retry
/// budget (50 attempts at 100 ms per plane op, ~10 ops per ingest), not picked:
/// a correct-but-starved fleet may legitimately answer slowly.
const REPLY_BUDGET: Duration = Duration::from_secs(120);

/// How long a gate waits for the agent to finish a turn. A model call plus a
/// tool call plus the publish round trip; generous because the model is real.
const TURN_BUDGET: Duration = Duration::from_secs(180);

/// Routes, spelled once.
const EVENTS_PATH: &str = "/buzz/api/events";
const QUERY_PATH: &str = "/buzz/api/query";
const SUBSCRIPTIONS_PATH: &str = "/buzz/api/subscriptions";
const CHANNELS_PATH: &str = "/buzz/api/channels";
/// The stream prefix the relay's `claim_session` parses a session id out of.
const STREAM_PREFIX: &str = "/buzz/sub/";

/// The config key the app reads its stream-token secret from
/// (`ingress::STREAM_KEY_CONFIG`). Without it, subscriptions are refused rather
/// than minted under a guessable key — so the gate seeds one.
const STREAM_KEY_CONFIG: &[u8] = b"buzz-stream-token-key";

/// Nonce source for authorization events: the relay burns an authorization
/// event's id on use, so every request needs a distinct one.
static NONCE: AtomicU64 = AtomicU64::new(0);

fn next_nonce() -> String {
    format!("n{}", NONCE.fetch_add(1, Ordering::Relaxed))
}

// ─────────────────────────────────────────────────────────────────── the fleet
//
// `TestEnv` is `Send` but **not `Sync`** — it owns the fleet's node guards as
// `Box<dyn Any + Send>` — while [`Transport`] is `Send + Sync`. A future holding
// `&TestEnv` across an await is therefore not `Send`, which rules out both
// `Arc<TestEnv>` and `Arc<Mutex<TestEnv>>` (a guard over a non-`Sync` value is
// itself not `Send`). So the env lives in ONE task pinned to the test's thread
// ([`gate`] runs each gate inside a `LocalSet`) and everything else talks to it
// over a channel.
//
// The driver serves jobs one at a time, which is fine: every job is a fast queue
// round trip, and the two slow things — the model call and waiting on a frame —
// do not go through it at all.

enum Job {
    /// Drive one request through the app and return its reply.
    Handle(IngressRequest, oneshot::Sender<io::Result<IngressReply>>),
    /// Open the app's SSE face for a session path.
    Stream(String, oneshot::Sender<io::Result<WsSession>>),
    /// Hand out a Store client for a state read or a state seed.
    Store(String, oneshot::Sender<StoreClusterClient>),
}

/// A handle to the in-process fleet, usable from any task.
struct Fleet {
    jobs: mpsc::Sender<Job>,
    driver: tokio::task::JoinHandle<()>,
}

impl Drop for Fleet {
    fn drop(&mut self) {
        self.driver.abort();
    }
}

impl Fleet {
    /// Stand the fleet up with the app's `ferrite.json` bindings, run ONE app
    /// instance on it, wait for it to be healthy, and seed the stream-token key.
    async fn start() -> Arc<Self> {
        let env = TestHarness::new()
            .with_store("nonces")
            // queues
            .with_queue("requests")
            .with_queue("replies")
            .with_queue("stream-requests")
            .with_queue("frames")
            // stores
            .with_store("events")
            .with_store("config")
            // guards + feed
            .with_ape("guards")
            .with_topics("feed")
            // blobs
            .with_blob("media")
            .with_blob("web-assets")
            .request_reply("requests", "replies")
            .stream_requests("stream-requests")
            // The gateway's frame queue: where `run_pump` publishes SSE frames.
            .frames("frames")
            .timeout(REPLY_BUDGET)
            .build()
            .await;
        env.run_app(|ctx| async move {
            let mut payload = ctx.payload().clone();
            payload
                .secrets
                .insert("sso_shared_secret".into(), TEST_SSO_SECRET.into());
            buzz::relay::ingress::serve(ferrite_app::AppContext::with_clock(payload, ctx.clock())).await
        });

        // Wait for the app to be serving, so the first real request is not also
        // paying the fleet's cold-start cost.
        let mut healthy = false;
        for _ in 0..80 {
            match env.handle(signed_in(ingress::get("/buzz/api/healthz"), env.now_ms())).await {
                Ok(reply) if reply.status == 200 => {
                    healthy = true;
                    break;
                }
                _ => tokio::time::sleep(Duration::from_millis(50)).await,
            }
        }
        assert!(healthy, "the app did not become healthy");

        // The stream-token secret. A deployment injects this as a sealed secret;
        // absent both a secret and this config value the app refuses every
        // subscription rather than minting forgeable tokens.
        let mut config = env.store("config");
        store::put_raw(&mut config, STREAM_KEY_CONFIG, &[0x5au8; 32])
            .await
            .expect("stream key seed");

        let (tx, mut rx) = mpsc::channel::<Job>(64);
        // `spawn_local`, not `spawn`: the driver's future borrows the non-`Sync`
        // env across awaits, so it is not `Send`. Pinning it to the gate's thread
        // is the whole reason [`gate`] wraps each test in a `LocalSet`.
        let driver = tokio::task::spawn_local(async move {
            // The env moves in here and never leaves; this task is the only
            // thing in the process that touches it.
            while let Some(job) = rx.recv().await {
                match job {
                    Job::Handle(req, reply) => {
                        let now_ms = env.now_ms();
                        let _ = reply.send(env.handle(signed_in(req, now_ms)).await);
                    }
                    Job::Stream(path, reply) => {
                        let _ = reply.send(env.sse_open(&path).await);
                    }
                    Job::Store(name, reply) => {
                        let _ = reply.send(env.store(&name));
                    }
                }
            }
        });

        Arc::new(Self { jobs: tx, driver })
    }

    async fn handle(&self, req: IngressRequest) -> io::Result<IngressReply> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Job::Handle(req, tx))
            .await
            .map_err(|_| io::Error::other("fleet driver is gone"))?;
        rx.await
            .map_err(|_| io::Error::other("fleet driver dropped the job"))?
    }

    async fn stream(&self, path: &str) -> io::Result<WsSession> {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Job::Stream(path.to_string(), tx))
            .await
            .map_err(|_| io::Error::other("fleet driver is gone"))?;
        rx.await
            .map_err(|_| io::Error::other("fleet driver dropped the job"))?
    }

    async fn store(&self, name: &str) -> StoreClusterClient {
        let (tx, rx) = oneshot::channel();
        self.jobs
            .send(Job::Store(name.to_string(), tx))
            .await
            .expect("fleet driver is alive");
        rx.await.expect("fleet driver answered")
    }
}

// ────────────────────────────────────────────────────────────────── requests

/// A kind-27235 authorization event bound to `method` + `path`, base64'd into an
/// `Authorization: Nostr …` header value.
///
/// The `nonce` tag is load-bearing: the relay burns an authorization event's id
/// on first use, and `Identity::authorization_header` — which mints no nonce and
/// stamps whole seconds — produces byte-identical events for two same-second
/// requests to one route. That is a real defect in the bundled HTTP transport,
/// not something this gate should paper over silently, so it is called out here
/// and in the module docs.
fn nip98_header(identity: &Identity, method: &str, path: &str, now_secs: u64) -> SdkResult<String> {
    let event = identity.sign(
        kind::KIND_HTTP_AUTH,
        vec![
            vec!["u".to_string(), format!("https://buzz.test{path}")],
            vec!["method".to_string(), method.to_ascii_uppercase()],
            vec!["nonce".to_string(), next_nonce()],
        ],
        "",
        now_secs,
    )?;
    let json = serde_json::to_vec(&event).map_err(|e| SdkError::encoding(e.to_string()))?;
    Ok(format!("Nostr {}", b64::encode(&json)))
}

/// The request envelope the gateway publishes for an API call.
fn api_request(
    method: &str,
    path: &str,
    authorization: &str,
    body: Vec<u8>,
    deadline_ms: u64,
) -> IngressRequest {
    IngressRequest {
        stream_open: false,
        correlation_id: String::new(),
        verified_host: None,
        idempotency_key: String::new(),
        route: "/buzz/api/".to_string(),
        path: path.to_string(),
        method: method.to_string(),
        headers: vec![("authorization".to_string(), authorization.to_string())],
        body,
        deadline_ms,
        reply_to: String::new(),
        published_ms: 0,
        auth_context: None,
        native_caller: None,
    }
}

/// The relay's reply body as JSON, refusing anything that is not a success.
fn success_json(reply: &IngressReply) -> SdkResult<Value> {
    let text = String::from_utf8_lossy(&reply.body).to_string();
    if reply.status < 200 || reply.status >= 300 {
        return Err(SdkError::http(reply.status, text));
    }
    let value: Value =
        serde_json::from_str(&text).map_err(|e| SdkError::encoding(format!("{e}: {text}")))?;
    if value.get("ok").and_then(Value::as_bool) == Some(false) {
        let reason = value
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("the relay refused the request");
        return Err(SdkError::http(reply.status, reason));
    }
    Ok(value)
}

// ───────────────────────────────────────────────────────── the transport

/// A [`Transport`] that drives the REAL app in process.
///
/// Every method builds the `IngressRequest` the gateway would publish — including
/// a freshly signed NIP-98 header for the caller's own identity — hands it to the
/// app's request queue, and decodes the `IngressReply` the app publishes back.
/// `subscribe` mints a real session through `POST /buzz/api/subscriptions`, and
/// `next_frame` reads the app's own SSE pump.
struct InProcessTransport {
    fleet: Arc<Fleet>,
    identity: Identity,
}

impl InProcessTransport {
    fn new(fleet: Arc<Fleet>, identity: Identity) -> Self {
        Self { fleet, identity }
    }

    /// Wall-clock seconds. Read from the host rather than from a workspace clock
    /// because a NIP-98 event must be fresh against the *relay's* clock.
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// POST a body to an authenticated route and return the decoded reply.
    async fn post_json(&self, path: &str, body: Vec<u8>) -> SdkResult<Value> {
        let authorization = nip98_header(&self.identity, "POST", path, Self::now_secs())?;
        let deadline_ms = Self::now_secs()
            .saturating_mul(1000)
            .saturating_add(REPLY_BUDGET.as_millis() as u64);
        let reply = self
            .fleet
            .handle(api_request("POST", path, &authorization, body, deadline_ms))
            .await
            .map_err(|e| SdkError::transport(format!("{path}: {e}")))?;
        success_json(&reply)
    }

    /// Open the app's SSE face for `session` and pump its frames into the
    /// handle's bounded inbox.
    ///
    /// Started on the first [`Transport::next_frame`] rather than in
    /// `subscribe`, so a resumed cursor set on the handle in between is honored —
    /// the same contract the bundled HTTP transport keeps.
    async fn start_pump(&self, session: &mut SessionHandle) -> SdkResult<()> {
        let mut path = format!("{STREAM_PREFIX}{}?token={}", session.session, session.token);
        if let Some(cursor) = session.cursor.as_deref() {
            path.push_str("&from=");
            path.push_str(cursor);
        }
        let mut sse = self
            .fleet
            .stream(&path)
            .await
            .map_err(|e| SdkError::transport(format!("stream open: {e}")))?;
        let sink = session.pump();
        tokio::spawn(async move {
            loop {
                // `recv` is bounded by the fleet's reply budget and yields None
                // when the stream is quiet for that long; the app heartbeats
                // every 10 s, so that means the pump really is gone.
                let Some(wire) = sse.recv().await else {
                    return;
                };
                let frame = decode_frame(&wire);
                let ended = frame.is_err();
                if sink.send(frame).await.is_err() || ended {
                    // Nobody is reading, or the frame did not decode: end the
                    // pump rather than hold the session open forever.
                    let _ = sse.close().await;
                    return;
                }
            }
        });
        Ok(())
    }
}

/// One wire frame as the protocol's [`Frame`].
///
/// The frame's own `type` field is authoritative, exactly as in the SSE decoder:
/// the `event:` name is a routing convenience for browser clients.
fn decode_frame(wire: &StreamFrame) -> SdkResult<Frame> {
    serde_json::from_slice::<Frame>(&wire.data).map_err(|e| {
        SdkError::encoding(format!(
            "stream frame{}: {e}",
            wire.event
                .as_deref()
                .map(|n| format!(" '{n}'"))
                .unwrap_or_default()
        ))
    })
}

impl Transport for InProcessTransport {
    fn publish_event<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, SdkResult<()>> {
        Box::pin(async move {
            let body = serde_json::to_vec(event).map_err(|e| SdkError::encoding(e.to_string()))?;
            self.post_json(EVENTS_PATH, body).await?;
            Ok(())
        })
    }

    fn query<'a>(&'a self, request: &'a QueryRequest) -> BoxFuture<'a, SdkResult<QueryPage>> {
        Box::pin(async move {
            let body =
                serde_json::to_vec(request).map_err(|e| SdkError::encoding(e.to_string()))?;
            let value = self.post_json(QUERY_PATH, body).await?;
            let mut page: QueryPage =
                serde_json::from_value(value).map_err(|e| SdkError::encoding(e.to_string()))?;
            page.events.retain(|event| request.keep(event));
            Ok(page)
        })
    }

    fn subscribe<'a>(&'a self, filters: &'a [Filter]) -> BoxFuture<'a, SdkResult<SessionHandle>> {
        Box::pin(async move {
            let body =
                serde_json::to_vec(filters).map_err(|e| SdkError::encoding(e.to_string()))?;
            let value = self.post_json(SUBSCRIPTIONS_PATH, body).await?;
            let session = value
                .get("session")
                .and_then(Value::as_str)
                .ok_or_else(|| SdkError::encoding("subscription reply has no session id"))?;
            let token = value
                .get("token")
                .and_then(Value::as_str)
                .ok_or_else(|| SdkError::encoding("subscription reply has no stream token"))?;
            Ok(SessionHandle::new(
                session,
                token,
                value
                    .get("cursor")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                value
                    .get("heartbeat_ms")
                    .and_then(Value::as_u64)
                    .unwrap_or(10_000),
                filters.to_vec(),
            ))
        })
    }

    fn next_frame<'a>(
        &'a self,
        session: &'a mut SessionHandle,
    ) -> BoxFuture<'a, SdkResult<Option<Frame>>> {
        Box::pin(async move {
            if !session.is_pumped() {
                self.start_pump(session).await?;
            }
            let frame = session.recv().await?;
            if let Some(frame) = frame.as_ref() {
                session.observe(frame);
            }
            Ok(frame)
        })
    }
}

// ──────────────────────────────────────────────────────────── the real tool

/// What the model passes to `read_source`.
#[derive(Debug, Deserialize)]
struct ReadSource {
    /// A file name directly under `buzz/src/relay/`, e.g. `access.rs`.
    path: String,
}

/// A genuine tool doing genuine work: read one of the relay's own source files
/// off disk. This is a TEST, so `std::fs` is the right answer — the constrained
/// app has no filesystem and this is not the constrained app.
///
/// The registry is a fixed allowlist by construction: only a bare file name
/// directly under [`SOURCE_ROOT`] is accepted, so nothing the model says can
/// walk out of that directory.
fn read_source_tool(calls: Arc<AtomicUsize>) -> buzz_sdk::Tool {
    tool(
        "read_source",
        "Read one of the buzz relay's own Rust source files. `path` is a bare \
         file name directly under buzz/src/relay/ — for example \"access.rs\", \
         \"ingress.rs\", \"ingest.rs\", \"auth.rs\", \"query.rs\", \"kind.rs\". \
         Returns the file's full text.",
        move |args: ReadSource| {
            let calls = Arc::clone(&calls);
            async move {
                calls.fetch_add(1, Ordering::Relaxed);
                let name = args.path.trim().trim_start_matches("src/").trim_start_matches('/');
                if name.is_empty()
                    || name.contains('/')
                    || name.contains('\\')
                    || name.contains("..")
                {
                    return Err(SdkError::tool(
                        "read_source",
                        format!(
                            "{:?} is not a bare file name directly under buzz/src/relay/",
                            args.path
                        ),
                    ));
                }
                let full = std::path::Path::new(SOURCE_ROOT).join(name);
                match std::fs::read_to_string(&full) {
                    Ok(text) => {
                        println!("  [tool] read_source({name}) -> {} bytes", text.len());
                        Ok(text)
                    }
                    Err(e) => Err(SdkError::tool(
                        "read_source",
                        format!("cannot read buzz/src/relay/{name}: {e}"),
                    )),
                }
            }
        },
    )
    .schema(json!({
        "type": "object",
        "properties": {
            "path": {
                "type": "string",
                "description": "A bare file name directly under buzz/src/relay/, e.g. \"access.rs\"."
            }
        },
        "required": ["path"],
        "additionalProperties": false
    }))
}

// ──────────────────────────────────────────────────────── state seed + read

/// A deterministic identity from a label, so a failing gate is reproducible.
fn actor(label: &str) -> Identity {
    Identity::from_secret(sha256(label.as_bytes())).expect("a valid secret key")
}

/// Create a channel through the REAL route, as `who`.
async fn create_channel(fleet: &Arc<Fleet>, who: &Identity, id: &str, visibility: &str) {
    let transport = InProcessTransport::new(Arc::clone(fleet), who.clone());
    let body = serde_json::to_vec(&json!({
        "id": id,
        "name": format!("#{id}"),
        "type": "stream",
        "visibility": visibility,
    }))
    .expect("channel body encodes");
    let value = transport
        .post_json(CHANNELS_PATH, body)
        .await
        .expect("the channel was created");
    assert_eq!(value["channel"]["id"], id);
    assert_eq!(value["channel"]["visibility"], visibility);
}

/// Every event the relay durably holds for `channel`, oldest first, read through
/// the app's own timeline index.
async fn channel_timeline(fleet: &Arc<Fleet>, channel: &str) -> Vec<Event> {
    let mut events = fleet.store("events").await;
    let page =
        buzz::relay::query::timeline(&mut events, DEFAULT_COMMUNITY, Some(channel), None, 200)
            .await
            .expect("timeline read");
    let mut out = page.events;
    out.sort_by(|a, b| {
        a.created_at
            .cmp(&b.created_at)
            .then_with(|| a.id.cmp(&b.id))
    });
    out
}

/// Wait until `want` holds over the channel's durable timeline, or give up.
async fn await_state(
    fleet: &Arc<Fleet>,
    channel: &str,
    budget: Duration,
    want: impl Fn(&[Event]) -> bool,
) -> Vec<Event> {
    let deadline = Instant::now() + budget;
    let mut last = Vec::new();
    while Instant::now() < deadline {
        last = channel_timeline(fleet, channel).await;
        if want(&last) {
            return last;
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    last
}

/// A short label for a stored event, for the printed transcript.
fn describe(event: &Event, who: &[(&str, String)]) -> String {
    let author = who
        .iter()
        .find(|(_, hex)| *hex == to_hex(&event.pubkey))
        .map(|(name, _)| (*name).to_string())
        .unwrap_or_else(|| to_hex(&event.pubkey).chars().take(8).collect());
    let id: String = to_hex(&event.id).chars().take(12).collect();
    format!("[{id}] kind {} {author}", event.kind)
}

/// Whether `event` threads to `root` through an `e` tag.
fn threads_to(event: &Event, root: &Event) -> bool {
    let want = to_hex(&root.id);
    event.tag_values("e").any(|v| v.eq_ignore_ascii_case(&want))
}

/// Whether `event` addresses `who` with a `p` tag.
fn addresses(event: &Event, who: &PubKey) -> bool {
    let want = to_hex(who);
    event.tag_values("p").any(|v| v.eq_ignore_ascii_case(&want))
}

/// Run one gate's body with a `LocalSet` in place, so [`Fleet`] can pin its
/// driver to this thread (see the fleet's module comment).
///
/// `#[tokio::test]` drives the test future with `Runtime::block_on`, which polls
/// it on the calling thread — so the `LocalSet` created here is polled on the one
/// thread it was created on, which is its contract.
async fn gate<F: core::future::Future<Output = ()>>(body: F) {
    tokio::task::LocalSet::new().run_until(body).await;
}

/// Skip a gate with a clear message when the key is absent.
fn api_key() -> Option<String> {
    match std::env::var("ANTHROPIC_API_KEY") {
        Ok(key) if !key.trim().is_empty() => Some(key),
        _ => {
            println!(
                "SKIPPED: this gate drives a real model. Set ANTHROPIC_API_KEY and re-run:\n  \
                 cargo test --test live_agent -- --ignored --nocapture"
            );
            None
        }
    }
}

/// Add `who` to `channel` as a member, through the REAL add-member route: a
/// kind-9000 (`PUT_USER`) event published by the channel `owner`. This exercises
/// `effects::apply_membership` end to end — the same path a human uses to invite
/// an agent — instead of writing the `mem/` row directly.
async fn add_member(fleet: &Arc<Fleet>, owner: &Identity, channel: &str, who: &PubKey, role: Role) {
    let event = owner
        .sign(
            kind::KIND_NIP29_PUT_USER,
            vec![
                vec!["h".to_string(), channel.to_string()],
                vec!["p".to_string(), to_hex(who)],
                vec!["role".to_string(), role.as_str().to_string()],
            ],
            "",
            InProcessTransport::now_secs(),
        )
        .expect("the add-member event signs");
    let transport = InProcessTransport::new(Arc::clone(fleet), owner.clone());
    transport
        .publish_event(&event)
        .await
        .expect("the relay accepted the add-member command");
}

/// Post a message as a human member, through the real ingest route.
async fn human_says(
    fleet: &Arc<Fleet>,
    who: &Identity,
    channel: &str,
    text: &str,
    mentions: &[PubKey],
) -> Event {
    let mut tags = vec![vec!["h".to_string(), channel.to_string()]];
    for mention in mentions {
        tags.push(vec!["p".to_string(), to_hex(mention)]);
    }
    let event = who
        .sign(
            kind::KIND_STREAM_MESSAGE,
            tags,
            text,
            InProcessTransport::now_secs(),
        )
        .expect("the message signs");
    let transport = InProcessTransport::new(Arc::clone(fleet), who.clone());
    transport
        .publish_event(&event)
        .await
        .expect("the relay accepted the human's message");
    event
}

// ═══════════════════════════════════════════════════════ gate 1: code review

/// **A real agent reviews real code.**
///
/// A human asks, in a private channel, a question that cannot be answered
/// without reading this repository's `buzz/src/relay/access.rs`. A real
/// Anthropic-backed agent — its own keypair, its own membership, its own
/// `read_source` tool — wakes on the mention off the relay's live feed, reads
/// the file, and answers in-thread. The gate then asserts on what the relay
/// durably stored, not on anything the agent printed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "drives a real model: run with --ignored and ANTHROPIC_API_KEY set"]
async fn live_a_real_agent_reviews_real_code() {
    gate(a_real_agent_reviews_real_code()).await;
}

async fn a_real_agent_reviews_real_code() {
    let Some(_key) = api_key() else { return };

    let channel = "code-review";
    let human = actor("live-human-maintainer");
    let agent_id = actor("live-agent-reviewer");
    let names = vec![
        ("human", human.pubkey_hex()),
        ("reviewer", agent_id.pubkey_hex()),
    ];

    let fleet = Fleet::start().await;
    println!("fleet up; source root = {SOURCE_ROOT}");

    // The human creates a PRIVATE channel (they become its owner and only
    // member), then the agent's membership is seeded so the write path will
    // accept its reply and the read path will deliver it the mention.
    create_channel(&fleet, &human, channel, "private").await;
    // The human (the channel's owner) adds the agent through the REAL 9000
    // add-member route — the same path a person uses to invite a bot.
    add_member(&fleet, &human, channel, &agent_id.pubkey(), Role::Bot).await;
    println!("channel #{channel} is private; reviewer seeded as a Bot member");

    // The agent: real model, real instructions, real tool.
    let tool_calls = Arc::new(AtomicUsize::new(0));
    let workspace = Workspace::builder("")
        .identity(agent_id.clone())
        .transport(Arc::new(InProcessTransport::new(
            Arc::clone(&fleet),
            agent_id.clone(),
        )))
        .default_channel(channel)
        .connect()
        .await
        .expect("the workspace connects over the in-process transport");

    let agent = Agent::builder(workspace)
        .name("reviewer")
        .model(MODEL)
        // Named explicitly: the SDK has no default backend, so nothing acquires a
        // provider connection by accident. A live gate is exactly the place that
        // should hold a real key, and it says so.
        .backend(buzz_sdk::backend::AnthropicBackend::from_env().expect("ANTHROPIC_API_KEY"))
        .instructions(
            "You are `reviewer`, a code reviewer for the buzz relay — a Rust crate whose \
             source you can read with the `read_source` tool.\n\n\
             Rules you always follow:\n\
             - NEVER answer a question about this code from memory or from what the name of a \
               function suggests. Call `read_source` on the relevant file first and answer only \
               from what you actually read.\n\
             - Name the file and the exact function you are talking about, and quote the \
               decisive line or match arm.\n\
             - Be direct: at most six sentences. Say yes or no first, then the evidence.",
        )
        .tool(read_source_tool(Arc::clone(&tool_calls)))
        .max_steps(5)
        .max_tokens(1200)
        .context_limit(20)
        .on_error(|e| eprintln!("  [agent error] {e}"))
        .build()
        .expect("the agent builds");
    println!("agent npub = {}", agent.npub());

    let serving = tokio::spawn(Arc::new(agent).serve());
    // Let the subscription and its pump come up before the mention lands. Not
    // load-bearing — the session's cursor starts at the feed's beginning, so a
    // mention posted first is still delivered — but it keeps the run tidy.
    tokio::time::sleep(Duration::from_millis(500)).await;

    let question = format!(
        "@reviewer does src/access.rs fail closed when a channel's visibility cannot be \
         resolved? Cite the function and the exact arm that decides it. (npub {})",
        agent_id.npub()
    );
    let asked = human_says(&fleet, &human, channel, &question, &[agent_id.pubkey()]).await;
    println!("human asked, event {}", to_hex(&asked.id));

    // Assert on DURABLE STATE: an event authored by the agent's own key, in this
    // channel, threaded to the human's message.
    let me = agent_id.pubkey();
    let timeline = await_state(&fleet, channel, TURN_BUDGET, |events| {
        events
            .iter()
            .any(|e| e.pubkey == me && e.kind == kind::KIND_STREAM_MESSAGE)
    })
    .await;
    serving.abort();

    println!("\n─── durable timeline for #{channel} ───");
    for event in &timeline {
        println!("{}", describe(event, &names));
    }

    let reply = timeline
        .iter()
        .find(|e| e.pubkey == me && e.kind == kind::KIND_STREAM_MESSAGE)
        .unwrap_or_else(|| {
            panic!(
                "the agent never posted a stream message; the relay holds {} event(s) for #{channel}",
                timeline.len()
            )
        });

    println!("\n─── the agent's reply, as the relay stored it ───");
    println!("id        {}", to_hex(&reply.id));
    println!("pubkey    {}", to_hex(&reply.pubkey));
    println!("kind      {}", reply.kind);
    println!("tags      {:?}", reply.tags);
    println!("content   {}\n", reply.content);

    // It is really the agent's event, really in this channel, really signed.
    reply
        .verify()
        .expect("the stored reply verifies as the agent's own signature");
    assert_eq!(reply.pubkey, me, "authored by the AGENT's key");
    assert_eq!(reply.channel_id(), Some(channel), "in the review channel");
    assert!(
        threads_to(reply, &asked),
        "threaded to the human's message: tags {:?}",
        reply.tags
    );
    assert!(
        addresses(reply, &human.pubkey()),
        "addressed back to the human: tags {:?}",
        reply.tags
    );
    assert!(!reply.content.trim().is_empty(), "the reply says something");

    // It read the file rather than bluffing.
    let lowered = reply.content.to_lowercase();
    assert!(
        lowered.contains("access.rs") || lowered.contains("may_deliver"),
        "the reply cites neither access.rs nor may_deliver, so nothing here shows the model \
         read the source: {}",
        reply.content
    );
    let calls = tool_calls.load(Ordering::Relaxed);
    assert!(
        calls > 0,
        "the model answered without ever calling read_source, so the citation is a bluff"
    );
    println!("read_source was called {calls} time(s)");

    // The event the agent wrote is the event the relay holds under its own id.
    let mut events = fleet.store("events").await;
    let stored = store::get_doc::<Event>(&mut events, &keys::evt(DEFAULT_COMMUNITY, &reply.id))
        .await
        .expect("store read")
        .expect("the reply has a stored body");
    assert_eq!(stored.id, reply.id);
    assert_eq!(stored.content, reply.content);
}

// ══════════════════════════════════════════════ gate 2: two agents converse

/// **Two agents hold a conversation.**
///
/// Two independently-keyed agents in one private channel, each `#p`-tagging the
/// other so the relay's own mention filter wakes it, bounded to two exchanges
/// apiece by the handler rather than by hope.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "drives a real model: run with --ignored and ANTHROPIC_API_KEY set"]
async fn live_two_agents_hold_a_conversation() {
    gate(two_agents_hold_a_conversation()).await;
}

async fn two_agents_hold_a_conversation() {
    let Some(_key) = api_key() else { return };

    /// How many peer-tagged turns each agent may take. The turn after that posts
    /// a sign-off with no `p` tag, which is what ends the conversation.
    const TAGGED_TURNS: usize = 2;

    let channel = "design-debate";
    let human = actor("live-human-chair");
    let ada = actor("live-agent-ada");
    let bee = actor("live-agent-bee");
    let names = vec![
        ("human", human.pubkey_hex()),
        ("ada", ada.pubkey_hex()),
        ("bee", bee.pubkey_hex()),
    ];

    let fleet = Fleet::start().await;
    create_channel(&fleet, &human, channel, "private").await;
    // Both agents are added through the REAL 9000 add-member route by the owner.
    add_member(&fleet, &human, channel, &ada.pubkey(), Role::Bot).await;
    add_member(&fleet, &human, channel, &bee.pubkey(), Role::Bot).await;

    let mut serving = Vec::new();
    for (name, me, peer, stance) in [
        (
            "ada",
            &ada,
            &bee,
            "You argue FOR failing closed everywhere, even when it costs availability.",
        ),
        (
            "bee",
            &bee,
            &ada,
            "You argue that failing closed is right for access decisions but wrong for \
             liveness, and you push back on over-application.",
        ),
    ] {
        let workspace = Workspace::builder("")
            .identity(me.clone())
            .transport(Arc::new(InProcessTransport::new(
                Arc::clone(&fleet),
                me.clone(),
            )))
            .default_channel(channel)
            .connect()
            .await
            .expect("the workspace connects");

        let peer_hex = peer.pubkey_hex();
        let turns = Arc::new(AtomicUsize::new(0));
        let agent = Agent::builder(workspace)
            .name(name)
            .model(MODEL)
            // Named explicitly: the SDK has no default backend, so nothing acquires a
            // provider connection by accident. A live gate is exactly the place that
            // should hold a real key, and it says so.
            .backend(buzz_sdk::backend::AnthropicBackend::from_env().expect("ANTHROPIC_API_KEY"))
            .instructions(format!(
                "You are one of two engineers debating a design question in a chat channel. \
                 {stance}\n\n\
                 Reply in at most three sentences. Address the other engineer's last point \
                 directly, then make exactly one new point. No lists, no headings, no preamble."
            ))
            .max_steps(1)
            .max_tokens(300)
            .context_limit(20)
            .typing(false)
            .on_error(|e| eprintln!("  [agent error] {e}"))
            .on_mention(move |turn: Turn| {
                let peer_hex = peer_hex.clone();
                let turns = Arc::clone(&turns);
                async move {
                    let taken = turns.fetch_add(1, Ordering::Relaxed);
                    let answer = turn.think().await?;
                    let Some(channel) = turn.channel().map(str::to_string) else {
                        return Ok(());
                    };
                    // Thread every post to the conversation's root.
                    let mut tags = Vec::new();
                    if let Some(root) = turn.thread_root() {
                        tags.push(vec![
                            "e".to_string(),
                            root,
                            String::new(),
                            "root".to_string(),
                        ]);
                    }
                    // Tag the peer only while under the bound: the `p` tag is
                    // what the relay's mention filter wakes them on, so dropping
                    // it is what ends the exchange.
                    if taken < TAGGED_TURNS {
                        tags.push(vec!["p".to_string(), peer_hex]);
                    }
                    let posted = turn
                        .workspace()
                        .post_with_tags(&channel, answer.as_str(), tags)
                        .await?;
                    let id: String = to_hex(&posted.id).chars().take(12).collect();
                    println!(
                        "  [{} turn {} -> event {id}] {}",
                        turn.agent_name(),
                        taken + 1,
                        posted.content.replace('\n', " ")
                    );
                    Ok(())
                }
            })
            .build()
            .expect("the agent builds");
        serving.push(tokio::spawn(Arc::new(agent).serve()));
    }

    tokio::time::sleep(Duration::from_millis(500)).await;
    let opened = human_says(
        &fleet,
        &human,
        channel,
        "@ada @bee settle this for me: should an access decision that cannot resolve a fact \
         always deny? Debate it with each other, not with me.",
        &[ada.pubkey()],
    )
    .await;

    // Two exchanges means: ada answers, bee answers ada, ada answers bee. Wait
    // for at least two posts from each side.
    let ada_pk = ada.pubkey();
    let bee_pk = bee.pubkey();
    let timeline = await_state(&fleet, channel, TURN_BUDGET, |events| {
        let count = |pk: &PubKey| {
            events
                .iter()
                .filter(|e| &e.pubkey == pk && e.kind == kind::KIND_STREAM_MESSAGE)
                .count()
        };
        count(&ada_pk) >= 2 && count(&bee_pk) >= 2
    })
    .await;
    for task in serving {
        task.abort();
    }

    println!("\n─── durable timeline for #{channel} ───");
    for event in &timeline {
        println!(
            "{}\n    {}",
            describe(event, &names),
            event.content.replace('\n', " ")
        );
    }

    let by = |pk: &PubKey| -> Vec<&Event> {
        timeline
            .iter()
            .filter(|e| &e.pubkey == pk && e.kind == kind::KIND_STREAM_MESSAGE)
            .collect()
    };
    let ada_posts = by(&ada_pk);
    let bee_posts = by(&bee_pk);
    assert!(
        ada_posts.len() >= 2,
        "ada posted {} time(s), so there was no second exchange",
        ada_posts.len()
    );
    assert!(
        bee_posts.len() >= 2,
        "bee posted {} time(s), so there was no second exchange",
        bee_posts.len()
    );

    // Each one really addressed the other through the relay's own mention tag,
    // and the exchange really is one thread.
    assert!(
        ada_posts.iter().any(|e| addresses(e, &bee_pk)),
        "no post of ada's `p`-tags bee, so bee was never woken by the relay"
    );
    assert!(
        bee_posts.iter().any(|e| addresses(e, &ada_pk)),
        "no post of bee's `p`-tags ada"
    );
    for event in ada_posts.iter().chain(bee_posts.iter()) {
        event.verify().expect("each stored post verifies");
        assert_eq!(event.channel_id(), Some(channel));
        assert!(!event.content.trim().is_empty());
        assert!(
            threads_to(event, &opened),
            "post {} left the thread: tags {:?}",
            to_hex(&event.id),
            event.tags
        );
    }
    // Bounded, and bounded where it matters: the `p` tag is the only thing that
    // wakes the peer, so the handler's cap has to show up as a cap on how many
    // stored posts carry one.
    let tagged =
        |posts: &[&Event], peer: &PubKey| posts.iter().filter(|e| addresses(e, peer)).count();
    assert!(
        tagged(&ada_posts, &bee_pk) <= TAGGED_TURNS,
        "ada woke bee {} time(s), past the bound of {TAGGED_TURNS}",
        tagged(&ada_posts, &bee_pk)
    );
    assert!(
        tagged(&bee_posts, &ada_pk) <= TAGGED_TURNS,
        "bee woke ada {} time(s), past the bound of {TAGGED_TURNS}",
        tagged(&bee_posts, &ada_pk)
    );
    assert!(
        ada_posts.len() <= TAGGED_TURNS + 1 && bee_posts.len() <= TAGGED_TURNS + 1,
        "the conversation was not bounded: ada {} / bee {}",
        ada_posts.len(),
        bee_posts.len()
    );
}
