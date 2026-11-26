//! **The WebSocket face serves connections CONCURRENTLY, and one connection's
//! frames IN ORDER.**
//!
//! The ws face was strictly serial: `ws_loop` awaited the whole of
//! `handle_ws_message` between poll and ack, so an app instance served exactly
//! one inbound frame at a time no matter how many sockets it held. Measured on
//! the live deployment, 96 connections × 4 REQs produced **0 of 384 EOSE frames
//! and 0 EVENT frames in 210 s**, and an earlier 96 × 8 handshake took **60 060
//! ms** — while relay RSS *fell* and the app sat at 1-2% of its CPU budget. The
//! face the real client speaks collapsed on connection count, for no reason but
//! head-of-line blocking.
//!
//! The fix is a worker per connection: envelopes are dispatched to a task keyed
//! by `correlation_id` and the loop keeps leasing. That buys concurrency across
//! sockets and keeps order within one, and BOTH halves need defending —
//! concurrency alone (a flat worker pool) would let an `EVENT` be authorized
//! against a pre-`AUTH` snapshot of the same socket's connection document, and
//! let a `CLOSE` be overwritten by the `REQ` it was meant to cancel.
//!
//! So there are three gates here, in that order:
//!
//! * [`severe_one_connection_cannot_block_another_connections_frames`] — an
//!   ORDER assertion, not a timing threshold: six cheap frames published on six
//!   sockets *behind* four expensive ones on a seventh must still be answered
//!   first. Under a serial loop they are answered after all of them, always,
//!   because the loop cannot even take the next lease while one frame is in
//!   flight.
//! * [`severe_an_event_pipelined_behind_auth_on_one_socket_is_authorized`] — the
//!   per-connection ordering guarantee, asserted on DURABLE state rather than on
//!   a frame, so a lost frame cannot make it pass or fail by accident.
//! * [`severe_a_close_pipelined_behind_its_req_on_one_socket_wins`] — the same
//!   guarantee for the other order-sensitive pair, also on durable state.
//!
//! The clock is FROZEN at [`T0_MS`], as in every gate here. That has one
//! consequence this file must respect: the harness relays outbound frames with a
//! long-polling consumer whose lease can never expire against a stopped clock
//! (`work_queue.rs:251`), so a frame lost to that race is never redelivered. The
//! two ordering gates therefore assert on the store, and the concurrency gate
//! re-drives its whole burst rather than a single frame — re-driving a burst
//! preserves the property under test, where re-driving one frame would not.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;
use tokio::time::Instant;

use buzz::relay::channel::Visibility;
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{keys, kind, store as buzz_store, ws};
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, T0_MS, T0_SECS, gate_env, seed_channel_with_owner, signed_at,
    stored_event, tag, ws_path_with_ticket,
};
use ferrite_app::serde_json::{json, Value};
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{TestHarness, WsSession};

/// One fleet at a time in this binary: each gate stands up its own in-process
/// fleet, and `cargo test` runs a file's tests concurrently.
static WS_CONCURRENCY_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The route the ws face is published under.
const WS_PATH: &str = "/buzz/ws/";

/// The relay URL the operator configured, and therefore the one a NIP-42 `relay`
/// tag must name.
const RELAY_URL: &str = "wss://buzz.test/buzz/ws/";

/// How long ONE attempt waits for an answer that must arrive.
const ATTEMPT: Duration = Duration::from_secs(25);

/// How many times a client frame (or a burst) is re-driven while the relay stays
/// silent — the frozen-clock frame-loss race described in the module docs.
const ATTEMPTS: usize = 3;

/// How long a durable-state assertion polls before it gives up.
const SETTLE: Duration = Duration::from_secs(20);

// ------------------------------------------------------------------- the fleet

/// The fleet, with the ws route's own queue as the driver's request queue, and
/// exactly ONE app instance.
///
/// One instance is deliberate and is what the ordering gates measure. Several
/// instances competing-consume one `ws-requests` queue, so two frames of one
/// connection can be leased by two instances and served at the same time — that
/// was true of the serial loop too, and the per-connection guarantee this file
/// defends is a PER-INSTANCE one (see `ingress::ws_loop`).
async fn ws_env() -> TestEnv {
    let env = TestEnv(TestHarness::new().with_store("nonces")
        .with_queue("requests")
        .with_queue("replies")
        .with_queue("stream-requests")
        .with_queue("ws-requests")
        .with_queue("frames")
        .with_queue("work")
        .with_store("events")
        .with_store("config")
        .with_ape("guards")
        .with_topics("feed")
        .with_blob("media")
        .with_blob("web-assets")
        .request_reply("ws-requests", "replies")
        .stream_requests("stream-requests")
        .frames("frames")
        .manual_clock(T0_MS)
        .timeout(REPLY_BUDGET)
        .build()
        .await);
    {
        let mut config = env.store("config");
        buzz_store::put_raw(&mut config, b"buzz-stream-token-key", &[0x5au8; 32])
            .await
            .expect("stream key seed");
        buzz_store::put_raw(&mut config, b"buzz-relay-url", RELAY_URL.as_bytes())
            .await
            .expect("relay url seed");
    }
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
    env
}

// ------------------------------------------------------------------ ws driving

async fn next_message(session: &mut WsSession, budget: Duration) -> Option<Vec<Value>> {
    let frame = tokio::time::timeout(budget, session.recv()).await.ok()??;
    ferrite_app::serde_json::from_slice::<Vec<Value>>(&frame.data).ok()
}

async fn wait_for(session: &mut WsSession, verb: &str, budget: Duration) -> Option<Vec<Value>> {
    let deadline = Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        if let Some(message) = next_message(session, left).await {
            if message.first() == Some(&json!(verb)) {
                return Some(message);
            }
        }
    }
}

async fn send(session: &mut WsSession, message: &Value) {
    session
        .send(message.to_string())
        .await
        .expect("the frame is published");
}

/// Open the real ws face and take the NIP-42 challenge it must send first.
async fn connect(env: &TestEnv) -> (WsSession, String) {
    for _ in 0..ATTEMPTS {
        let mut session = env
            .ws_open(&ws_path_with_ticket(WS_PATH, env.now_ms()))
            .await
            .expect("the ws face opens");
        if let Some(message) = wait_for(&mut session, "AUTH", ATTEMPT).await {
            let challenge = message[1].as_str().expect("a challenge").to_string();
            return (session, challenge);
        }
    }
    panic!("the ws face issued no challenge in {ATTEMPTS} connections");
}

fn auth_event_for(actor: &Actor, challenge: &str) -> Event {
    signed_at(
        actor,
        kind::KIND_AUTH,
        vec![tag("relay", RELAY_URL), tag("challenge", challenge)],
        "",
        T0_SECS,
    )
}

/// Open a connection and complete NIP-42 on it, waiting for the `OK`.
async fn connect_as(env: &TestEnv, actor: &Actor) -> WsSession {
    let (mut session, challenge) = connect(env).await;
    for _ in 0..ATTEMPTS {
        let event = auth_event_for(actor, &challenge);
        send(&mut session, &json!(["AUTH", event])).await;
        if let Some(ok) = wait_for(&mut session, "OK", ATTEMPT).await {
            assert_eq!(ok[2], json!(true), "NIP-42 must be accepted: {ok:?}");
            return session;
        }
    }
    panic!("the socket never authenticated");
}

/// The connection document the app durably holds for this socket.
async fn connection_doc(env: &TestEnv, session: &WsSession) -> Option<ws::Connection> {
    let mut config = env.store("config");
    buzz_store::get_doc::<ws::Connection>(
        &mut config,
        &keys::wsconn(COMMUNITY, session.correlation_id()),
    )
    .await
    .expect("config read")
}

// ---------------------------------------------------------- the two REQ shapes

/// How many filters one expensive REQ carries (`ws::MAX_FILTERS_PER_REQ`).
const HEAVY_FILTERS: usize = 8;

/// How many `#e` targets each of those filters lists.
///
/// Bounded by [`ws::MAX_SUBSCRIPTION_TABLE_BYTES`] rather than by
/// `ws::MAX_FILTER_VALUES`: the filters are stored in the durable connection
/// document, so eight filters × 512 targets × ~68 bytes is over the 256 KiB
/// table budget and the relay correctly answers `CLOSED` instead of serving it.
/// 256 keeps the table near 143 KiB.
const HEAVY_TARGETS: usize = 256;

/// How many expensive REQs are stacked on the ONE heavy socket.
///
/// They all reuse a single `subId`, so NIP-01's replace rule keeps the
/// subscription table at one entry however many are sent — and because one
/// socket is served strictly in order, four of them are four times the work with
/// no extra concurrency. That is the amplifier: the signal has to clear the
/// harness's own overheads, measured at ~10 ms per test-side publish and ~25 ms
/// per relayed frame, which together set the floor the cheap answers can reach.
const HEAVY_REQS: usize = 4;

/// A REQ that is EXPENSIVE for the app and CHEAP on the wire.
///
/// `ws::plan_read` turns a filter carrying `#e` targets into `ReadPlan::Etags`,
/// and `merge_indexes` walks ONE PAGE PER TARGET — so eight filters of 256
/// targets is **2048 index reads in one REQ**, entirely within the bounds the
/// relay advertises. None of the targets exists, so the answer is zero `EVENT`
/// frames and one `EOSE`.
///
/// Both halves of that matter. The harness relays outbound frames through a
/// single serial pump, as the real gateway does, so a gate whose signal rode a
/// burst of frames would be measuring the pump. This REQ puts all of its cost
/// inside the app and emits exactly one frame.
fn heavy_req(sub_id: &str) -> Value {
    let mut message = vec![json!("REQ"), json!(sub_id)];
    for f in 0..HEAVY_FILTERS {
        // Well-formed event ids that no event has: 64 hex characters, distinct
        // per filter so the eight walks share no prefix.
        let targets: Vec<String> = (0..HEAVY_TARGETS)
            .map(|t| format!("{f:02x}{t:04x}{}", "e".repeat(58)))
            .collect();
        message.push(json!({ "#e": targets, "limit": 10 }));
    }
    Value::Array(message)
}

/// A REQ that is as cheap as the app can serve one: `limit: 0` is buzz's
/// live-only subscription, and `ws_stored_events` returns on it without touching
/// an index. One EOSE frame, two store operations.
fn light_req(sub_id: &str) -> Value {
    json!(["REQ", sub_id, { "kinds": [kind::KIND_STREAM_MESSAGE], "limit": 0 }])
}

// ------------------------------------------------------------------- the gates

/// How many cheap frames are published BEHIND the expensive one, each on its own
/// socket.
const LIGHT: usize = 6;

/// How many of them must overtake it. Zero is the serial score and `LIGHT` is
/// the concurrent one; one slot of slack absorbs scheduler jitter without coming
/// anywhere near the serial outcome.
const MUST_OVERTAKE: usize = LIGHT - 1;

/// **Expensive frames on one socket must not delay cheap frames published behind
/// them on other sockets.**
///
/// The seven sockets are opened and authenticated FIRST, so the measured burst
/// carries nothing but `WS_MESSAGE` envelopes. Three expensive REQs are
/// published on one socket, and six cheap ones behind them on six others — so
/// under a serial loop every cheap answer necessarily comes after all three
/// expensive ones, every time: the loop takes one lease, serves it to
/// completion, and only then takes the next. Concurrent, six REQs that touch no
/// index at all should all overtake three that walk 2048 each.
///
/// The assertion is an ORDER, not a duration, so it needs no threshold
/// calibrated against machine speed and a fast machine cannot rescue a serial
/// loop — it scores exactly zero however fast it runs. What the machine speed
/// does change is the MARGIN, which is why the expensive work is stacked on ONE
/// socket: this harness runs the whole fleet inside the test's own runtime, so
/// it is CPU-bound where the real deployment is I/O-bound at 1-2% CPU, and
/// several expensive sockets at once would starve the cheap ones on the fleet
/// rather than on the ws loop — measuring the harness instead of the app.
#[test]
 fn severe_one_connection_cannot_block_another_connections_frames() {
    common::run_scenario(|| async move {
    let _serial = WS_CONCURRENCY_LOCK.lock().await;
    let env = ws_env().await;
    let reader = Actor::new("concurrency-reader");

    let mut heavy_session = connect_as(&env, &reader).await;
    let mut light_sessions = Vec::with_capacity(LIGHT);
    for _ in 0..LIGHT {
        light_sessions.push(connect_as(&env, &reader).await);
    }

    let mut report = String::new();
    for attempt in 1..=ATTEMPTS {
        // Dispatch from THIS task so the publish order is the queue's order: the
        // expensive REQs first, the cheap ones behind them.
        let started = Instant::now();
        for _ in 0..HEAVY_REQS {
            send(&mut heavy_session, &heavy_req(&format!("heavy-{attempt}"))).await;
        }
        for (i, session) in light_sessions.iter_mut().enumerate() {
            send(session, &light_req(&format!("light-{attempt}-{i}"))).await;
        }

        // Collect concurrently: each socket's answers are already queued in its
        // own channel, so moving the sessions into tasks now cannot lose one.
        let mut waiters = Vec::with_capacity(LIGHT + 1);
        waiters.push(tokio::spawn(async move {
            // The LAST of the three, so the reference point is the end of the
            // expensive socket's work rather than the first third of it.
            let mut got = true;
            for _ in 0..HEAVY_REQS {
                got &= wait_for(&mut heavy_session, "EOSE", ATTEMPT)
                    .await
                    .is_some();
            }
            ("heavy".to_string(), got, Instant::now(), heavy_session)
        }));
        for (i, mut session) in light_sessions.into_iter().enumerate() {
            waiters.push(tokio::spawn(async move {
                let got = wait_for(&mut session, "EOSE", ATTEMPT).await.is_some();
                (format!("light-{i}"), got, Instant::now(), session)
            }));
        }

        let mut answered: Vec<(String, bool, Instant)> = Vec::new();
        let mut sessions: Vec<WsSession> = Vec::new();
        for waiter in waiters {
            let (label, got, at, session) = waiter.await.expect("the waiter task ran");
            answered.push((label, got, at));
            sessions.push(session);
        }
        // Hand the sessions back for a possible re-drive.
        let mut sessions = sessions.into_iter();
        heavy_session = sessions.next().expect("the heavy session comes back first");
        light_sessions = sessions.collect();

        // A burst where anything went unanswered is the frozen-clock frame-loss
        // race, not a finding: re-drive the WHOLE burst, because re-driving one
        // frame would change the very interleaving under test.
        if answered.iter().any(|(_, got, _)| !got) {
            continue;
        }

        answered.sort_by_key(|(_, _, at)| *at);
        let heavy_at = answered
            .iter()
            .find(|(label, _, _)| label == "heavy")
            .map(|(_, _, at)| *at)
            .expect("the heavy answer is in the set");
        let overtook = answered
            .iter()
            .filter(|(label, _, at)| label != "heavy" && *at < heavy_at)
            .count();

        report = format!(
            "{overtook}/{LIGHT} cheap REQs published BEHIND the expensive one overtook it.\n  \
             order: {}\n  whole burst: {:?}",
            answered
                .iter()
                .map(|(label, _, at)| format!(
                    "{label}@{}ms",
                    at.saturating_duration_since(started).as_millis()
                ))
                .collect::<Vec<_>>()
                .join(" "),
            answered
                .last()
                .map(|(_, _, at)| at.saturating_duration_since(started))
                .unwrap_or_default(),
        );
        println!("[ws concurrency] {report}");

        assert!(
            overtook >= MUST_OVERTAKE,
            "the ws face is still serializing across connections: of {LIGHT} cheap REQs \
             published AFTER one expensive REQ — on {LIGHT} DIFFERENT sockets — only \
             {overtook} were answered before it (needs {MUST_OVERTAKE}). A serial loop \
             scores exactly 0, because it cannot take the next lease until the frame in \
             flight is finished.\n  {report}"
        );
        return;
    }
    panic!("the burst never completed in {ATTEMPTS} attempts; last report: {report}");

    });
}

/// **An `EVENT` pipelined immediately behind its `AUTH`, on the SAME socket, is
/// still authorized.**
///
/// This is the ordering half of the guarantee, and it is the one a flat worker
/// pool breaks: the `EVENT` handler reads the connection document to find the
/// authenticated pubkey, and a pool would let it read the pre-`AUTH` snapshot
/// and answer `auth-required`. That refusal is TERMINAL — `ws_event` publishes
/// the `OK(false)` and the envelope is acked — so nothing retries it and the
/// message the user sent is simply gone.
///
/// Asserted on the STORE, not on the `OK` frame. A refused event is never
/// stored, and a stored event was necessarily authorized; a frame lost to the
/// frozen-clock race can therefore neither hide the bug nor invent one.
#[test]
 fn severe_an_event_pipelined_behind_auth_on_one_socket_is_authorized() {
    common::run_scenario(|| async move {
    let _serial = WS_CONCURRENCY_LOCK.lock().await;
    let env = ws_env().await;
    let owner = Actor::new("pipeline-owner");
    let channel = "chan-ws-pipeline";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    let (mut session, challenge) = connect(&env).await;
    let auth = auth_event_for(&owner, &challenge);
    let message = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "pipelined straight behind the AUTH",
        T0_SECS,
    );

    // No wait between them: the client is allowed to pipeline, and NIP-42 says
    // nothing that forbids it.
    send(&mut session, &json!(["AUTH", auth])).await;
    send(&mut session, &json!(["EVENT", message])).await;

    let stored = poll_until(SETTLE, || async {
        stored_event(&env, &message.id).await.is_some()
    })
    .await;
    assert!(
        stored,
        "the EVENT published immediately behind its AUTH on the SAME socket was never \
         stored, so the ws face authorized it against a pre-AUTH snapshot of the \
         connection document. Frames from one socket must be served in the order the \
         queue delivered them. (event {}, socket {})",
        to_hex(&message.id),
        session.correlation_id(),
    );

    });
}

/// **A `CLOSE` pipelined immediately behind its own `REQ`, on the SAME socket,
/// still closes the subscription.**
///
/// The other order-sensitive pair, and a quieter failure than the AUTH one: both
/// frames are a read-modify-write of the same subscription table, so a reordered
/// `CLOSE` is not lost, it is OVERWRITTEN. Run concurrently, the `CLOSE` reads a
/// table that does not hold the subscription yet, finds nothing to remove and
/// writes NOTHING; the `REQ` then commits the subscription behind it. The client
/// believes it closed the subscription; the pump keeps matching it, forever.
///
/// The `REQ` here is the EXPENSIVE one deliberately. With two equally cheap
/// frames the race is a coin flip — half the time the `REQ` commits first and
/// the `CLOSE` then removes it correctly, so the gate would pass against a
/// broken relay every other run. An expensive `REQ` makes the reordering
/// decisive: a concurrent `CLOSE` finishes long before it, always.
///
/// The `EOSE` is the sync point: it proves the `REQ` was served, so from there
/// the `CLOSE` has either landed or is about to, and the assertion is that it
/// eventually wins.
#[test]
 fn severe_a_close_pipelined_behind_its_req_on_one_socket_wins() {
    common::run_scenario(|| async move {
    let _serial = WS_CONCURRENCY_LOCK.lock().await;
    let env = ws_env().await;
    let reader = Actor::new("pipeline-reader");
    let mut session = connect_as(&env, &reader).await;
    let sub_id = "sub-open-then-close";

    for attempt in 1..=ATTEMPTS {
        send(&mut session, &heavy_req(sub_id)).await;
        send(&mut session, &json!(["CLOSE", sub_id])).await;

        // The REQ was served (frozen-clock frame loss re-drives the PAIR, which
        // keeps the pipelining under test intact).
        if wait_for(&mut session, "EOSE", ATTEMPT).await.is_none() {
            assert!(attempt < ATTEMPTS, "the REQ was never answered");
            continue;
        }

        let closed = poll_until(SETTLE, || async {
            connection_doc(&env, &session)
                .await
                .is_some_and(|conn| conn.subscription(sub_id).is_none())
        })
        .await;
        assert!(
            closed,
            "the CLOSE pipelined immediately behind its own REQ on the SAME socket lost: \
             subscription {sub_id} is still live in the connection document. Run \
             concurrently, the CLOSE reads a table the REQ has not written yet, finds \
             nothing to remove and writes nothing — and the REQ then commits a \
             subscription the client believes it cancelled. (socket {})",
            session.correlation_id(),
        );
        return;
    }

    });
}

/// Poll `check` until it holds or `budget` passes.
async fn poll_until<F, Fut>(budget: Duration, mut check: F) -> bool
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = Instant::now() + budget;
    loop {
        if check().await {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}
