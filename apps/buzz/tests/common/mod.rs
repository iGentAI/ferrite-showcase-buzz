//! Shared fixtures for the severe gates.
//!
//! Every gate in this directory drives the **real app** (`ingress::serve`) over a
//! **real in-process fleet** stood up by `ferrite-app-test`: the app's own
//! bindings, the real queue/store/APE/topics/blob engines, and the gateway's own
//! request/reply envelope shape. Nothing here mocks a plane, and nothing here
//! calls a policy function directly — assertions read the route's reply and the
//! durable state the route left behind.
//!
//! Three fixture groups:
//!
//! * **The fleet** ([`gate_env`], [`env_with`]) — bindings that match `ferrite.json`
//!   handle-for-handle, on a FROZEN virtual clock so every time-dependent gate
//!   (auth freshness, event drift) is exercised by choosing `created_at`, never by
//!   sleeping.
//! * **Callers** ([`Actor`], [`post`], [`submit`], [`query`]) — a NIP-98
//!   authorization event per request, signed by the caller and bound to the
//!   method and path. The nonce tag exists because the app's replay guard burns an
//!   authorization event id on first use: two requests cannot share a header.
//! * **State readers** ([`stored_event`], [`audit_entries`], [`feed_records`], …)
//!   — the app's own key builders against the same planes, so a gate asserts on
//!   what the app actually durably wrote.

#![allow(dead_code)]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use buzz::relay::audit::{AuditEntry, AuditHead};
use buzz::relay::channel::{Channel, ChannelType, Membership, Role, Visibility};
use buzz::relay::crypto::sha256::sha256;
use buzz::relay::crypto::{schnorr, to_hex};
use buzz::relay::delivery::FeedRecord;
use buzz::relay::event::{Event, EventId, PubKey};
use buzz::relay::ingest::Admission;
use buzz::relay::{b64, keys, kind, store, DEFAULT_COMMUNITY};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::{IngressReply, IngressRequest};
use ferrite_app_test::{ingress, TestEnv, TestHarness};

/// Property SSO for protocol fixtures. NIP-98 and ACP credentials remain
/// independent and are still checked by the real relay after this session.
const TEST_SSO_SECRET: &str = "buzz-disposable-sso-test-only";
pub struct SsoTestEnv(pub TestEnv);
impl std::ops::Deref for SsoTestEnv {
    type Target = TestEnv;
    fn deref(&self) -> &TestEnv {
        &self.0
    }
}
fn property_request(mut req: IngressRequest, now_ms: u64) -> IngressRequest {
    req.route = matched_route(&req.path).into();
    // The host the gateway routes on, which every NIP-98 `u` tag here names
    // (`auth_event`): the relay refuses a header signed for another origin.
    if req.verified_host.is_none() {
        req.verified_host = Some("buzz.test".into());
    }
    let session = buzz::ferrite_sso::SessionService::for_client(TEST_SSO_SECRET, "buzz", 3_600_000)
        .issue("local-protocol-owner", 0, now_ms)
        .expect("fixture session");
    req.headers.push((
        "cookie".into(),
        format!("__Host-ferrite_app_session={}", session.token),
    ));
    req
}
impl SsoTestEnv {
    pub async fn handle(&self, req: IngressRequest) -> std::io::Result<IngressReply> {
        self.0.handle(property_request(req, self.now_ms())).await
    }
    pub async fn handle_all<I>(&self, requests: I) -> Vec<std::io::Result<IngressReply>>
    where
        I: IntoIterator<Item = IngressRequest>,
    {
        self.0.handle_all(requests.into_iter().map(|req| property_request(req, self.now_ms()))).await
    }
    pub fn run_app<F, Fut>(&self, f: F)
    where
        F: FnOnce(ferrite_app::AppContext) -> Fut + Send + 'static,
        Fut: std::future::Future<Output = std::io::Result<()>> + Send + 'static,
    {
        self.0.run_app(move |ctx| async move {
            let mut payload = ctx.payload().clone();
            payload
                .secrets
                .insert("sso_shared_secret".into(), TEST_SSO_SECRET.into());
            f(ferrite_app::AppContext::with_clock(payload, ctx.clock())).await
        });
    }
}

/// The community every gate runs in (the app serves exactly one).
pub const COMMUNITY: &str = DEFAULT_COMMUNITY;

/// The frozen virtual instant the fleet's clock starts at (and stays at, unless a
/// gate advances it): 2023-11-14T22:13:20Z.
pub const T0_MS: u64 = 1_700_000_000_000;

/// [`T0_MS`] in whole seconds — what a signed `created_at` is compared against.
pub const T0_SECS: u64 = T0_MS / 1000;

/// The feed generation the app publishes to (`ingress::FEED_GENERATION`).
pub const FEED_GENERATION: u32 = 0;

/// Route paths, spelled once.
pub const EVENTS_PATH: &str = "/buzz/api/events";
pub const QUERY_PATH: &str = "/buzz/api/query";
pub const SUBSCRIPTIONS_PATH: &str = "/buzz/api/subscriptions";

/// Nonce source for authorization events. The app's replay guard is keyed on the
/// authorization event's id, so every request needs a distinct one; a
/// process-global counter is unique across the concurrent tests in one binary.
static NONCE: AtomicU64 = AtomicU64::new(0);

fn next_nonce() -> String {
    format!("n{}", NONCE.fetch_add(1, Ordering::Relaxed))
}

// ------------------------------------------------------------------- the fleet

/// Per-request reply budget.
///
/// Sized off the PLATFORM's own retry budget rather than picked: every plane
/// client retries a request up to 50 times at 100 ms while a primary is
/// unresolvable (5 s per op), and one ingest touches ~10 ops across four planes.
/// So a correct-but-starved fleet — many gates each running their own in-process
/// fleet on a loaded machine — can legitimately answer slowly, and a shorter
/// budget would report that as a failure. A genuinely wedged app still fails.
pub const REPLY_BUDGET: Duration = Duration::from_secs(120);

/// Stand up the fleet with the app's `ferrite.json` bindings and run ONE app
/// instance on it.
pub async fn gate_env() -> Gate {
    let permit = fleet_permit().await;
    Gate {
        env: SsoTestEnv(env_with(1, REPLY_BUDGET).await),
        _permit: permit,
    }
}

/// How many gate fleets may exist at once in one test binary.
///
/// Each gate stands up its OWN in-process fleet — real queue, store, APE and
/// topics engines, each with their own tasks — and `cargo test` runs a file's
/// tests concurrently. Nine fleets on one machine starved each other badly enough
/// that a gate would sit at its reply deadline and fail, and *which* gate failed
/// changed from run to run: a false negative that looks exactly like a real
/// timeout bug. Bounding the fleets is the fix; inflating the deadline only makes
/// the false negative slower to arrive. Serially the whole file finishes in ~9s,
/// so the cap costs almost nothing.
/// Note this cap is per test BINARY: `cargo test` runs the gate files in parallel
/// too, so the whole suite can hold this many fleets times the number of gate
/// files. A `static` cannot see across binaries, which is why the README's
/// validation block bounds test threads as well.
///
/// **Lowered from 2 to 1.** Two concurrent fleets still lost replies: an app that
/// had demonstrably finished its work and published its answer (verified by
/// instrumenting the route and the publish — every request returned the right
/// status and every publish reported `Ok`) would nonetheless leave a caller
/// waiting out its full [`REPLY_BUDGET`], and *which* caller changed from run to
/// run. That is the same false negative this cap already existed to prevent, one
/// notch further down.
///
/// Two things make 1 the right answer rather than a bigger deadline. The gates
/// run on a MANUAL clock shared with the fleet engines, so a queue lease never
/// expires and a lost reply can never be re-driven — in a gate a missed reply is
/// permanent, not slow, and no deadline rescues it. And the cost is negligible:
/// the author's own note above measures a whole file at ~9s serially.
///
/// This bounds the harness; it does not explain where the reply went. The
/// remaining unknown is a reply that is published successfully and not delivered
/// under fleet concurrency, which lives in the harness pump or the in-process
/// queue engine rather than in this app. Production is not exposed to it: there
/// the clock is real, the lease expires, and the envelope is re-driven.
const MAX_CONCURRENT_FLEETS: usize = 1;

static FLEETS: tokio::sync::Semaphore = tokio::sync::Semaphore::const_new(MAX_CONCURRENT_FLEETS);

async fn fleet_permit() -> tokio::sync::SemaphorePermit<'static> {
    FLEETS
        .acquire()
        .await
        .expect("the fleet semaphore is never closed")
}

/// A gate's fleet, holding its concurrency permit for as long as the fleet lives.
///
/// Derefs to [`TestEnv`], so a gate uses it exactly as it would use the env.
pub struct Gate {
    env: SsoTestEnv,
    _permit: tokio::sync::SemaphorePermit<'static>,
}

impl std::ops::Deref for Gate {
    type Target = SsoTestEnv;
    fn deref(&self) -> &SsoTestEnv {
        &self.env
    }
}

/// The same fleet with `instances` competing app instances (the real deployment
/// shape: several instances consuming one request queue), under the same fleet cap
/// as [`gate_env`].
pub async fn gate_with(instances: usize) -> Gate {
    let permit = fleet_permit().await;
    Gate {
        env: SsoTestEnv(env_with(instances, REPLY_BUDGET).await),
        _permit: permit,
    }
}

/// The same fleet with `instances` competing app instances and an explicit
/// per-request reply timeout, WITHOUT taking a fleet permit. Prefer
/// [`gate_with`]; this is the escape hatch for a gate that needs its own timeout.
pub async fn env_with(instances: usize, timeout: Duration) -> TestEnv {
    let env = SsoTestEnv(
        TestHarness::new().with_store("nonces")
            // queues
            .with_queue("requests")
            .with_queue("replies")
            .with_queue("stream-requests")
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
            // A FROZEN virtual clock: `ctx.now_ms()` is T0 for the app and for the
            // gates, so freshness and drift are decided by signed `created_at`
            // values rather than by wall-clock luck.
            .manual_clock(T0_MS)
            .timeout(timeout)
            .build()
            .await,
    );
    for _ in 0..instances {
        env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
    }
    // Wait for the app to be serving before the gate's first assertion. Without
    // this, the FIRST request also pays the app's boot cost (resolving a
    // freshly-elected primary on every plane, creating the delivery topics), which
    // on a loaded machine can be seconds — and a gate would then be timing the
    // fleet's cold start rather than the behaviour it is testing.
    for _ in 0..40 {
        match env.handle(get("/buzz/api/healthz")).await {
            Ok(reply) if reply.status == 200 => return env.0,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("the app did not become healthy");
}

// ---------------------------------------------------------------------- actors

/// A signing identity. Deterministic from its label, so a failing gate is
/// reproducible byte for byte.
#[derive(Debug, Clone, Copy)]
pub struct Actor {
    pub sk: [u8; 32],
    pub pk: PubKey,
}

impl Actor {
    /// The actor whose secret key is `sha256(label)` — always a valid secp256k1
    /// scalar in practice, and stable across runs.
    #[must_use]
    pub fn new(label: &str) -> Self {
        let sk = sha256(label.as_bytes());
        let pk = schnorr::xonly_pubkey(&sk).expect("a valid secret key");
        Self { sk, pk }
    }

    #[must_use]
    pub fn pk_hex(&self) -> String {
        to_hex(&self.pk)
    }
}

// ------------------------------------------------------------------ NIP-98 auth

/// A kind-27235 authorization event bound to `method` + `url`, signed by `actor`.
///
/// The `nonce` tag is what makes two otherwise-identical headers distinct events:
/// the app burns an authorization id on use, so a fixed nonce would make the
/// second request of a gate fail as a replay.
#[must_use]
pub fn auth_event(actor: &Actor, method: &str, path: &str, created_at: u64, nonce: &str) -> Event {
    Event::sign_new(
        &actor.sk,
        kind::KIND_HTTP_AUTH,
        vec![
            vec!["u".into(), format!("https://buzz.test{path}")],
            vec!["method".into(), method.to_string()],
            vec!["nonce".into(), nonce.to_string()],
        ],
        "",
        created_at,
    )
    .expect("authorization events are small and validly signed")
}

/// The `Authorization` header value carrying `event`.
#[must_use]
pub fn auth_header(event: &Event) -> String {
    format!(
        "Nostr {}",
        b64::encode(&serde_json::to_vec(event).expect("event encodes"))
    )
}

/// A fresh header for `method path`, plus the authorization event it carries (so a
/// gate can assert on the replay marker keyed by its id).
pub fn fresh_auth(actor: &Actor, method: &str, path: &str) -> (Event, String) {
    let event = auth_event(actor, method, path, T0_SECS, &next_nonce());
    let header = auth_header(&event);
    (event, header)
}

// -------------------------------------------------------------------- requests

/// A request with an explicit `Authorization` header value (the abuse cases:
/// replayed, mis-bound, expired, forged, absent).
#[must_use]
pub fn request_with_auth(
    method: &str,
    path: &str,
    header: Option<&str>,
    body: Vec<u8>,
) -> IngressRequest {
    IngressRequest {
        stream_open: false,
        correlation_id: String::new(),
        idempotency_key: String::new(),
        route: "/buzz/api/".to_string(),
        path: path.to_string(),
        method: method.to_string(),
        headers: header
            .map(|h| vec![("authorization".to_string(), h.to_string())])
            .unwrap_or_default(),
        body,
        deadline_ms: 0,
        reply_to: String::new(),
        published_ms: 0,
        // The host the gateway routed on, which `auth_event`'s `u` tags name.
        verified_host: Some("buzz.test".into()),
        ..IngressRequest::new("", "", Vec::new())
    }
}

/// A `POST` carrying a freshly signed, correctly bound header for `path`.
#[must_use]
pub fn post(actor: &Actor, path: &str, body: Vec<u8>) -> IngressRequest {
    let (_, header) = fresh_auth(actor, "POST", path);
    request_with_auth("POST", path, Some(&header), body)
}

/// Submit `event` to `POST /buzz/api/events` as `actor`, re-posting if the reply
/// is lost.
///
/// The retry is a HARNESS artifact, not slack in the app's contract. Under the
/// FROZEN clock these gates run on, a queue lease can be granted to a long-poll
/// whose client-side timeout fires in the same instant
/// (`ferrite-queue/src/cluster_queue_node.rs:2789`), and `reclaim_expired`
/// compares that lease's deadline against the same frozen clock
/// (`work_queue.rs:251`) — so the lease never expires and the ONE reply it holds
/// is never redelivered. A real client answers a lost reply by re-sending, and
/// so does this: each attempt carries a FRESH authorization header (the app burns
/// an authorization id on use, so replaying the header would be refused as a
/// replay), while the event id is unchanged, so the admission marker absorbs the
/// resubmission and the second reply reports `duplicate:` rather than doing the
/// work twice. A genuinely unanswering app still fails the gate.
pub async fn submit(env: &TestEnv, actor: &Actor, event: &Event) -> IngressReply {
    for attempt in 1..=SUBMIT_ATTEMPTS {
        match env
            .handle(property_request(submit_request(actor, event), env.now_ms()))
            .await
        {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => {
                panic!(
                    "the app never replied to {} submissions: {e}",
                    SUBMIT_ATTEMPTS
                )
            }
            Err(_) => continue,
        }
    }
    unreachable!("the loop either returns a reply or panics")
}

/// How many times [`submit`] re-posts an event while no reply comes back.
pub const SUBMIT_ATTEMPTS: usize = 3;

/// The submission request for `event` WITHOUT sending it (for concurrent drives).
#[must_use]
pub fn submit_request(actor: &Actor, event: &Event) -> IngressRequest {
    post(
        actor,
        EVENTS_PATH,
        serde_json::to_vec(event).expect("event encodes"),
    )
}

/// Drive several submissions CONCURRENTLY, re-posting any whose reply was lost.
///
/// The concurrency is the point: this is what puts distinct events through
/// competing instances at once, so the audit chain's head CAS is genuinely
/// contended rather than merely exercised. [`TestEnv::handle_all`] publishes
/// every request before collecting any reply, which is what makes that true.
///
/// The re-send is [`submit`]'s documented discipline reaching the one helper that
/// lacked it. Under the FROZEN clock these gates run on, a queue lease granted to
/// a long-poll whose client-side timeout fires in the same instant never expires,
/// so the ONE reply it holds is never redelivered — a missed reply here is
/// permanent, not slow. Observed directly: this gate failed a full-suite run with
/// `no reply for test-5` and passed three isolated runs at ~1.5s each.
///
/// It runs AFTER the concurrent drive, so the race under test has already
/// happened and only the ANSWER is recovered. Re-sending is safe on its own terms
/// for the reason `submit` gives: the event id is unchanged, so the admission
/// marker absorbs the resubmission and no effect runs twice — which the caller's
/// own "the chain has exactly N entries" assertion would catch if it were wrong.
pub async fn submit_all(env: &TestEnv, work: &[(Actor, Event)]) -> Vec<IngressReply> {
    let requests: Vec<IngressRequest> = work
        .iter()
        .map(|(actor, event)| submit_request(actor, event))
        .collect();
    let first = env.handle_all(requests.into_iter().map(|req| property_request(req, env.now_ms()))).await;
    let mut out = Vec::with_capacity(work.len());
    for (i, reply) in first.into_iter().enumerate() {
        match reply {
            Ok(reply) => out.push(reply),
            Err(_) => {
                let (actor, event) = work.get(i).expect("one reply per request");
                out.push(submit(env, actor, event).await);
            }
        }
    }
    out
}

/// Run `POST /buzz/api/query` with `body` as `actor`, re-posting if the reply
/// is lost.
///
/// This is [`submit`]'s documented re-send contract extended to the read
/// helper, for exactly the reason spelled out in `submit`'s comment: under the
/// gates' FROZEN clock, a queue lease granted to a long-poll whose client-side
/// timeout fires in the same instant never expires, so the ONE reply it holds
/// is never redelivered — in a gate a missed reply is permanent, not slow.
/// Measured on an otherwise idle machine (2026-07-29, five serialized runs of
/// `severe_auth_gate`): the loss hit ~40% of runs and wandered between tests,
/// always through a helper WITHOUT a re-send. This is a frozen-clock HARNESS
/// artifact, NOT a production exposure — on a real clock the lease expires and
/// the envelope is re-driven — so nothing in the app's `api_loop` should be
/// changed to chase it.
///
/// A real client answers a lost reply by re-sending, and so does this. Each
/// attempt mints a FRESH authorization header ([`post`] does that internally;
/// the replay guard burns an id on use, so re-sending the old header would be
/// answered as a duplicate, not as the query), and a query is a read, so asking
/// again changes nothing. The retry is BOUNDED so that a genuinely unanswering
/// app still fails the gate — loudly, and distinguishably from a logic failure.
pub async fn query(env: &TestEnv, actor: &Actor, body: &Value) -> IngressReply {
    let bytes = serde_json::to_vec(body).expect("query body encodes");
    for attempt in 1..=SUBMIT_ATTEMPTS {
        match env
            .handle(property_request(
                post(actor, QUERY_PATH, bytes.clone()),
                env.now_ms(),
            ))
            .await
        {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => panic!(
                "the app never replied to {SUBMIT_ATTEMPTS} query sends (each re-send \
                 exists only to absorb the frozen-clock lease race; spending all of \
                 them means the app genuinely did not answer): {e}"
            ),
            Err(_) => continue,
        }
    }
    unreachable!("the loop either returns a reply or panics")
}

// --------------------------------------------------------------------- replies

/// A reply body as JSON (panics if the app answered something else, which is
/// itself a finding).
#[must_use]
pub fn body_json(reply: &IngressReply) -> Value {
    serde_json::from_slice(&reply.body).unwrap_or_else(|e| {
        panic!(
            "reply body is not JSON ({e}): {}",
            String::from_utf8_lossy(&reply.body)
        )
    })
}

/// The `error` string of a refusal reply.
#[must_use]
pub fn error_text(reply: &IngressReply) -> String {
    body_json(reply)
        .get("error")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string()
}

/// The hex event ids a query reply returned, in order.
#[must_use]
pub fn returned_ids(reply: &IngressReply) -> Vec<String> {
    body_json(reply)
        .get("events")
        .and_then(Value::as_array)
        .map(|events| {
            events
                .iter()
                .filter_map(|e| e.get("id").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a query reply contains `event`.
#[must_use]
pub fn returned(reply: &IngressReply, event: &Event) -> bool {
    returned_ids(reply).contains(&to_hex(&event.id))
}

// ------------------------------------------------------------------- event mint

#[must_use]
pub fn tag(name: &str, value: &str) -> Vec<String> {
    vec![name.to_string(), value.to_string()]
}

/// A validly signed event at the fleet's current second.
#[must_use]
pub fn signed(actor: &Actor, kind: u32, tags: Vec<Vec<String>>, content: &str) -> Event {
    signed_at(actor, kind, tags, content, T0_SECS)
}

/// A validly signed event at an explicit `created_at` (the drift gates).
#[must_use]
pub fn signed_at(
    actor: &Actor,
    kind: u32,
    tags: Vec<Vec<String>>,
    content: &str,
    created_at: u64,
) -> Event {
    Event::sign_new(&actor.sk, kind, tags, content, created_at).expect("signs")
}

/// A validly signed event that is deliberately OVER the serialized bound.
///
/// [`Event::sign_new`] refuses to mint one (it checks the bound so the relay can
/// never author an event it would refuse), so the gate assembles the event and
/// signs it the same way `sign_new` does — id over the canonical serialization,
/// BIP-340 signature over the id. The result is cryptographically valid and
/// oversized, which is exactly the input the ingest bound exists to refuse.
#[must_use]
pub fn signed_oversized(actor: &Actor, kind: u32, tags: Vec<Vec<String>>, content: &str) -> Event {
    let mut event = Event {
        id: [0u8; 32],
        pubkey: actor.pk,
        created_at: T0_SECS,
        kind,
        tags,
        content: content.to_string(),
        sig: [0u8; 64],
    };
    event.id = event.compute_id().expect("canonicalizes");
    event.sig = schnorr::sign(&event.id, &actor.sk, &[0u8; 32]).expect("signs");
    assert!(
        event.verify().is_ok(),
        "the oversized fixture must still be a VALID event"
    );
    event
}

// -------------------------------------------------------------- state readers

/// The event body the app stored under `evt/{c}/{id}`, if any.
pub async fn stored_event(env: &TestEnv, id: &EventId) -> Option<Event> {
    let mut events = env.store("events");
    store::get_doc::<Event>(&mut events, &keys::evt(COMMUNITY, id))
        .await
        .expect("store read")
}

/// How many event bodies exist in the community at all.
pub async fn stored_event_count(env: &TestEnv) -> usize {
    let mut events = env.store("events");
    let (rows, _more) = store::scan(&mut events, &keys::evt_prefix(COMMUNITY), None, 1024)
        .await
        .expect("scan");
    rows.len()
}

/// Every index key under `prefix` (the read paths an event contributes to).
pub async fn index_keys(env: &TestEnv, prefix: &[u8]) -> Vec<Vec<u8>> {
    let mut events = env.store("events");
    let (rows, _more) = store::scan(&mut events, prefix, None, 1024)
        .await
        .expect("scan");
    rows.into_iter().map(|(k, _v, _ver)| k).collect()
}

/// The saga document for `id` on the guards (APE) plane.
pub async fn admission(env: &TestEnv, id: &EventId) -> Option<Admission> {
    let mut ape = env.ape("guards");
    let raw = ape
        .get(&keys::admit(COMMUNITY, id))
        .await
        .expect("ape read")?;
    Some(serde_json::from_slice(&raw).expect("admission decodes"))
}

/// The community's audit chain head.
pub async fn audit_head(env: &TestEnv) -> Option<AuditHead> {
    let mut ape = env.ape("guards");
    let raw = ape
        .get(&keys::audit_head(COMMUNITY))
        .await
        .expect("ape read")?;
    Some(serde_json::from_slice(&raw).expect("head decodes"))
}

/// The whole audit chain, in sequence order, read entry by entry from the APE
/// plane (there is no scan on that plane — the keys are derived from the head).
pub async fn audit_entries(env: &TestEnv) -> Vec<AuditEntry> {
    let Some(head) = audit_head(env).await else {
        return Vec::new();
    };
    let mut ape = env.ape("guards");
    let mut out = Vec::new();
    for seq in 1..=head.seq {
        if let Some(raw) = ape
            .get(&keys::audit_entry(COMMUNITY, seq))
            .await
            .expect("ape read")
        {
            out.push(serde_json::from_slice(&raw).expect("entry decodes"));
        }
    }
    out
}

/// The chain entries whose object is `id` — the "how many times was this event
/// audited" question every saga gate asks.
pub async fn audit_entries_for(env: &TestEnv, id: &EventId) -> Vec<AuditEntry> {
    let want = to_hex(id);
    audit_entries(env)
        .await
        .into_iter()
        .filter(|e| e.object.as_deref() == Some(want.as_str()))
        .collect()
}

/// The per-event audit marker (`auditref/{c}/{id}`): the sequence the event was
/// recorded at, if it was.
pub async fn audit_ref(env: &TestEnv, id: &EventId) -> Option<u64> {
    let mut ape = env.ape("guards");
    let raw = ape
        .get(&keys::auditref(COMMUNITY, id))
        .await
        .expect("ape read")?;
    core::str::from_utf8(&raw)
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
}

/// Every record on the live delivery feed for `generation`. A missing topic reads
/// as an empty feed rather than a panic, so a gate can distinguish "nothing was
/// published" from "the publish failed".
pub async fn feed_records(env: &TestEnv, generation: u32) -> Vec<FeedRecord> {
    let mut topics = env.topics("feed");
    let topic = keys::feed_topic(COMMUNITY, generation);
    match topics.read_from(&topic, 0, 1024).await {
        Ok(records) => records
            .iter()
            .filter_map(|r| FeedRecord::decode(&r.value).ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// Every record on the durable changefeed.
pub async fn changefeed_records(env: &TestEnv) -> Vec<FeedRecord> {
    let mut topics = env.topics("feed");
    match topics
        .read_from(&keys::changefeed_topic(COMMUNITY), 0, 1024)
        .await
    {
        Ok(records) => records
            .iter()
            .filter_map(|r| FeedRecord::decode(&r.value).ok())
            .collect(),
        Err(_) => Vec::new(),
    }
}

/// How many feed records carry `id` (the feed is at-least-once by design — a
/// consumer dedups by event id — so this counts copies, not distinct events).
pub async fn feed_copies(env: &TestEnv, generation: u32, id: &EventId) -> usize {
    feed_records(env, generation)
        .await
        .iter()
        .filter(|r| &r.event.id == id)
        .count()
}

/// The NIP-98 replay marker for an authorization event id.
pub async fn replay_marker(env: &TestEnv, auth_event_id: &EventId) -> Option<Vec<u8>> {
    let mut config = env.store("config");
    store::get_raw(&mut config, &keys::replay(COMMUNITY, auth_event_id))
        .await
        .expect("config read")
}

// -------------------------------------------------------------- state seeding

/// Seed a channel document on the guards (APE) plane — where `resolve_access`
/// reads it, under the app's own key builder.
///
/// The gates seed this rather than posting it because the access boundary is the
/// property under test and the creation route is in flux; the read path is
/// untouched by the seeding, since it still resolves visibility from the plane on
/// every single request.
pub async fn seed_channel(env: &TestEnv, id: &str, visibility: Visibility, owner: &Actor) {
    let doc = Channel::new(
        id.to_string(),
        format!("channel {id}"),
        ChannelType::Stream,
        visibility,
        &owner.pk,
        T0_MS,
    );
    let bytes = serde_json::to_vec(&doc).expect("channel encodes");
    let key = keys::guard_channel(COMMUNITY, id).expect("a safe channel id");
    let mut guards = env.ape("guards");
    guards
        .commit_n(&[(key.as_slice(), bytes.as_slice())])
        .await
        .expect("channel seed");
}

/// Seed a live membership row for `who` in `channel`.
///
/// Both the write path and the read path authorize on resolved facts — the channel
/// document plus a live membership row — so a gate that wants a successful
/// private-channel write has to provide both, exactly as `POST /buzz/api/channels`
/// does for a real creator.
pub async fn seed_membership(env: &TestEnv, channel: &str, who: &Actor, role: Role) {
    let doc = Membership::new(&who.pk, role, T0_MS);
    let key = keys::mem(COMMUNITY, channel, &who.pk).expect("a safe channel id");
    let mut events = env.store("events");
    store::put_doc(&mut events, &key, &doc)
        .await
        .expect("membership seed");
}

/// Read `who`'s membership row in `channel`, if any (`None` = never a member).
pub async fn membership(env: &TestEnv, channel: &str, who: &Actor) -> Option<Membership> {
    let mut events = env.store("events");
    let key = keys::mem(COMMUNITY, channel, &who.pk).expect("a safe channel id");
    store::get_doc::<Membership>(&mut events, &key)
        .await
        .expect("membership read")
}

/// The channel document's live-owner set (hex pubkeys).
pub async fn channel_owners(env: &TestEnv, id: &str) -> std::collections::BTreeSet<String> {
    let bytes = channel_doc(env, id).await.expect("channel present");
    let doc: Channel = ferrite_app::serde_json::from_slice(&bytes).expect("channel decodes");
    doc.owners
}

/// A channel plus its owner's membership: the state a real creation leaves behind.
pub async fn seed_channel_with_owner(
    env: &TestEnv,
    id: &str,
    visibility: Visibility,
    owner: &Actor,
) {
    seed_channel(env, id, visibility, owner).await;
    seed_membership(env, id, owner, Role::Owner).await;
}

/// Corrupt a channel document (a row that is present but cannot be read as a
/// channel: a bad migration, a truncated restore). The gate must treat it exactly
/// as it treats an absent one.
pub async fn corrupt_channel(env: &TestEnv, id: &str) {
    let key = keys::guard_channel(COMMUNITY, id).expect("a safe channel id");
    let mut guards = env.ape("guards");
    guards
        .commit_n(&[(key.as_slice(), b"{not-a-channel".as_slice())])
        .await
        .expect("channel corrupt");
}

/// The raw channel document bytes on the guards plane, if any.
pub async fn channel_doc(env: &TestEnv, id: &str) -> Option<Vec<u8>> {
    let key = keys::guard_channel(COMMUNITY, id).expect("a safe channel id");
    let mut guards = env.ape("guards");
    guards.get(&key).await.expect("ape read")
}

/// Seed a REMOVED membership row (a former member must read like a stranger).
pub async fn seed_removed_membership(env: &TestEnv, channel: &str, who: &Actor) {
    let mut doc = Membership::new(&who.pk, Role::Member, T0_MS);
    doc.removed_at = Some(T0_MS);
    let key = keys::mem(COMMUNITY, channel, &who.pk).expect("a safe channel id");
    let mut events = env.store("events");
    store::put_doc(&mut events, &key, &doc)
        .await
        .expect("membership seed");
}

/// A query body for one channel's timeline.
#[must_use]
pub fn channel_query(channel: &str) -> Value {
    json!({ "channel": channel })
}

/// The stream-token key every ws gate seeds (`buzz-stream-token-key`).
pub const WS_GATE_STREAM_KEY: [u8; 32] = [0x5a; 32];

/// `base` with the admission ticket the ws face requires on a new socket.
///
/// The ticket is what the gated `POST /buzz/api/ws-ticket` would hand a signed-in
/// browser, minted here under the key the gates seed because their harness binds
/// the request queue to the ws face rather than the api face.
pub fn ws_path_with_ticket(base: &str, now_ms: u64) -> String {
    format!(
        "{base}?ticket={}",
        buzz::relay::ws::mint_ticket(&WS_GATE_STREAM_KEY, now_ms)
    )
}

static SCENARIO_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

pub fn lock_scenario() -> std::sync::MutexGuard<'static, ()> {
    SCENARIO_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Run each scenario on the bounded worker that constructs its debug future.
/// Hosted SDK futures require a larger stack than the lightweight local harness.
pub fn run_scenario<F, Fut>(scenario: F)
where
    F: FnOnce() -> Fut + Send + 'static,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let _guard = lock_scenario();
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(4)
        .thread_stack_size(8 * 1024 * 1024)
        .enable_all()
        .build()
        .expect("fixture test runtime");
    runtime.block_on(async move {
        tokio::spawn(async move { Box::pin(scenario()).await })
            .await
            .expect("fixture scenario completes without a panic");
    });
}

/// Model the native gateway's matched API prefix in direct queue fixtures.
pub fn matched_route(path: &str) -> &'static str {
    let path = path.split('?').next().unwrap_or(path);
    ["/buzz/api/", "/events", "/query", "/count", "/info", "/mcp", "/acp", "/.well-known/"]
        .into_iter().find(|prefix| path.starts_with(prefix)).unwrap_or("/")
}
pub fn get(path: &str) -> IngressRequest {
    let mut req = ingress::get(path);
    req.route = matched_route(path).into();
    req
}
