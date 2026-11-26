//! **`ws::MAX_SUBSCRIPTIONS`, decided on the client socket.**
//!
//! The cap was raised from 32 to 256 because it is not a tuning knob: buzz's
//! real client opens one REQ per view and PEAKS AT 34 on a workspace with only
//! the two starter channels, and it classifies the overflow `CLOSED` as
//! TERMINAL (`relayClosedPolicy.ts`) — the subscription is never retried and
//! the feature behind it (reactions, mentions) silently never renders. So the
//! property has two halves and both are load-bearing:
//!
//! * a connection MAY hold far more than the old cap, every one of them
//!   served and durably registered;
//! * the cap still EXISTS, and overflow is refused with the terminal message
//!   (NIP-01 asks the relay to refuse, not to silently serve nothing) —
//!   without evicting anything the connection already holds.
//!
//! Driven over the real socket like every ws gate, with assertions on the
//! durable connection document beside every frame.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;

use buzz::relay::channel::Visibility;
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{keys, kind, store as buzz_store, ws};
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, T0_MS, T0_SECS, body_json, gate_env, seed_channel_with_owner,
    signed_at, tag, ws_path_with_ticket,
};
use ferrite_app::serde_json::{self, json, Value};
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{ingress, TestHarness, WsSession};

/// One ws fleet at a time in this binary.
static CAP_GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const WS_PATH: &str = "/buzz/ws/";
const RELAY_URL: &str = "wss://buzz.test/buzz/ws/";

/// How many concurrent subscriptions the gate holds open and SERVED before it
/// even looks at the cap: the real client's measured peak (34) with room for
/// the workspaces it grows into. Anyone lowering the cap back below this —
/// where the old 32 broke the real client — fails here first, legibly.
const REAL_CLIENT_FAN: usize = 40;

const ATTEMPT: Duration = Duration::from_secs(25);
const ATTEMPTS: usize = 3;

/// The severe_ws_gate fleet.
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
    serde_json::from_slice::<Vec<Value>>(&frame.data).ok()
}

async fn wait_for(session: &mut WsSession, verb: &str, budget: Duration) -> Option<Vec<Value>> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
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

/// Send and await the first `verb` answer, re-sending while the relay stays
/// silent (the frozen-clock lease artifact `severe_ws_gate.rs` documents;
/// every verb here is idempotent).
async fn ask(session: &mut WsSession, message: &Value, verb: &str) -> Vec<Value> {
    for _ in 0..ATTEMPTS {
        send(session, message).await;
        if let Some(answer) = wait_for(session, verb, ATTEMPT).await {
            return answer;
        }
    }
    panic!("no {verb} answer to {message} after {ATTEMPTS} attempts");
}

async fn connect_as(env: &TestEnv, actor: &Actor) -> WsSession {
    for _ in 0..ATTEMPTS {
        let mut session = env
            .ws_open(&ws_path_with_ticket(WS_PATH, env.now_ms()))
            .await
            .expect("the ws face opens");
        let Some(challenge) = wait_for(&mut session, "AUTH", ATTEMPT).await else {
            continue;
        };
        let challenge = challenge[1].as_str().expect("a challenge").to_string();
        let auth = signed_at(
            actor,
            kind::KIND_AUTH,
            vec![tag("relay", RELAY_URL), tag("challenge", &challenge)],
            "",
            T0_SECS,
        );
        let ok = ask(&mut session, &json!(["AUTH", auth]), "OK").await;
        assert_eq!(ok[2], json!(true), "NIP-42 must be accepted: {ok:?}");
        return session;
    }
    panic!("the ws face issued no challenge in {ATTEMPTS} connections");
}

async fn publish_ok(session: &mut WsSession, event: &Event) {
    let ok = ask(session, &json!(["EVENT", event]), "OK").await;
    assert_eq!(ok[1], json!(to_hex(&event.id)), "the OK names the event");
    assert_eq!(ok[2], json!(true), "the event must be accepted: {ok:?}");
}

/// Open subscription `sub_id` and require it SERVED (its EOSE arrives, never a
/// CLOSED): the affirmative half of the cap's contract.
async fn subscribe_served(session: &mut WsSession, sub_id: &str, request: &Value) {
    for _ in 0..ATTEMPTS {
        send(session, request).await;
        let deadline = tokio::time::Instant::now() + ATTEMPT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break; // silence: re-send (a REQ upserts, so this is safe)
            }
            let Some(message) = next_message(session, left).await else {
                continue;
            };
            match message.first().and_then(Value::as_str) {
                Some("EOSE") if message[1] == json!(sub_id) => return,
                Some("CLOSED") if message[1] == json!(sub_id) => panic!(
                    "subscription {sub_id} was REFUSED under the cap — the raise \
                     from 32 exists because the real client holds this many: {message:?}"
                ),
                _ => continue,
            }
        }
    }
    panic!("no EOSE for {sub_id} after {ATTEMPTS} attempts");
}

/// A minimal live filter: `limit: 0` serves an empty page and keeps the
/// subscription open — the shape of the client's typing/presence REQs.
fn slot_req(sub_id: &str) -> Value {
    json!(["REQ", sub_id, { "kinds": [kind::KIND_STREAM_MESSAGE], "limit": 0 }])
}

/// Whether `event` arrives on `sub_id` within `budget`.
async fn receives(session: &mut WsSession, sub_id: &str, event: &Event, budget: Duration) -> bool {
    let want = to_hex(&event.id);
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        let Some(message) = next_message(session, left).await else {
            continue;
        };
        if message.first() == Some(&json!("EVENT"))
            && message[1] == json!(sub_id)
            && message[2]["id"] == json!(want)
        {
            return true;
        }
    }
}

/// The durable connection document — the registry every instance serves live
/// traffic from, and therefore where the cap is really held.
async fn connection_doc(env: &TestEnv, session: &WsSession) -> Option<ws::Connection> {
    let mut config = env.store("config");
    buzz_store::get_doc::<ws::Connection>(
        &mut config,
        &keys::wsconn(COMMUNITY, session.correlation_id()),
    )
    .await
    .expect("config read")
}

/// Send a CLOSE and wait for its slot to leave the durable document,
/// re-sending while nothing changes (a CLOSE has no reply frame to re-send on,
/// and it is idempotent).
async fn close_until_dropped(env: &TestEnv, session: &mut WsSession, sub_id: &str, expect: usize) {
    for _ in 0..ATTEMPTS {
        send(session, &json!(["CLOSE", sub_id])).await;
        for _ in 0..100 {
            if connection_doc(env, session)
                .await
                .is_some_and(|c| c.subscriptions.len() == expect)
            {
                return;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
    panic!("CLOSE {sub_id} never released its slot");
}

// ------------------------------------------------------------------ the gates

// The constant itself keeps the headroom the raise bought, checked at COMPILE
// time (the `kind.rs` house pattern): 34 is the real client's MEASURED peak on
// a two-channel workspace and the count grows with the channel list, so a cap
// without ~2x headroom puts a modest workspace right back at the terminal
// refusal that motivated the raise. The client treats the overflow CLOSED as
// terminal, so lowering this re-breaks reactions/mentions silently.
const _: () = assert!(
    ws::MAX_SUBSCRIPTIONS >= 2 * 34,
    "MAX_SUBSCRIPTIONS lost the headroom over the real client's measured peak \
     (34 concurrent REQs on a two-channel workspace)"
);
const _: () = assert!(
    REAL_CLIENT_FAN < ws::MAX_SUBSCRIPTIONS,
    "the gate's own fan must fit under the cap"
);

/// NIP-11 advertises the bound the socket enforces, from the same constant —
/// read back through the real public route, so a client that configures
/// itself from `/buzz/api/relay-info` is never refused for a limit it could
/// not have known.
#[test]
 fn severe_the_advertised_subscription_cap_is_the_enforced_one() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let reply = env
        .handle(common::get("/buzz/api/relay-info"))
        .await
        .expect("the app replied");
    assert_eq!(reply.status, 200);
    let body = body_json(&reply);
    assert_eq!(
        body["limitation"]["max_subscriptions"],
        json!(ws::MAX_SUBSCRIPTIONS),
        "NIP-11 must advertise the enforced cap: {body}"
    );

    });
}

/// **A connection holds the real client's fan — and far past it, to the cap —
/// and the first REQ past the cap is refused with the terminal message,
/// evicting nothing.** After the refusal the connection is still fully alive:
/// its oldest subscription still delivers live traffic, and closing one slot
/// frees exactly one.
#[test]
 fn severe_many_subscriptions_are_served_and_the_cap_refuses_beyond_them() {
    common::run_scenario(|| async move {
    let _serial = CAP_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let owner = Actor::new("owner");
    let alice = Actor::new("alice");
    let channel = "chan-cap";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    let mut reader = connect_as(&env, &alice).await;

    // The subscription the client cares most about: the visible channel's
    // rows. It is opened FIRST so it is the oldest slot the overflow could
    // possibly evict.
    subscribe_served(
        &mut reader,
        "view-1",
        &json!(["REQ", "view-1", { "kinds": [kind::KIND_STREAM_MESSAGE], "#h": [channel], "limit": 10 }]),
    )
    .await;

    // The real client's fan: every one of these must be SERVED, not refused.
    // At the old cap of 32 this is where the relay said "too many
    // subscriptions" and the client silently lost features.
    for i in 2..=REAL_CLIENT_FAN {
        let sub = format!("view-{i}");
        subscribe_served(&mut reader, &sub, &slot_req(&sub)).await;
    }
    let doc = connection_doc(&env, &reader).await.expect("a document");
    assert_eq!(
        doc.subscriptions.len(),
        REAL_CLIENT_FAN,
        "all {REAL_CLIENT_FAN} concurrent subscriptions are durably registered"
    );

    // On to the advertised bound: the table accepts exactly MAX_SUBSCRIPTIONS.
    for i in (REAL_CLIENT_FAN + 1)..=ws::MAX_SUBSCRIPTIONS {
        let sub = format!("fill-{i}");
        subscribe_served(&mut reader, &sub, &slot_req(&sub)).await;
    }
    let doc = connection_doc(&env, &reader).await.expect("a document");
    assert_eq!(
        doc.subscriptions.len(),
        ws::MAX_SUBSCRIPTIONS,
        "the table is exactly full"
    );

    // One past the cap: REFUSED, with the message the client classifies —
    // never an EOSE over an empty page that silently serves nothing forever.
    let closed = ask(&mut reader, &slot_req("overflow"), "CLOSED").await;
    assert_eq!(
        closed[1],
        json!("overflow"),
        "the CLOSED names the refused REQ"
    );
    assert!(
        closed[2]
            .as_str()
            .unwrap_or_default()
            .starts_with("error: too many subscriptions"),
        "the refusal must carry the terminal token: {closed:?}"
    );

    // The refusal refused the REQ, not the connection: nothing was evicted,
    // nothing was registered…
    let doc = connection_doc(&env, &reader).await.expect("a document");
    assert_eq!(doc.subscriptions.len(), ws::MAX_SUBSCRIPTIONS);
    assert!(
        doc.subscription("overflow").is_none(),
        "a refused REQ must not hold a slot"
    );
    assert!(
        doc.subscription("view-1").is_some(),
        "the oldest subscription must survive the overflow"
    );

    // …and the full connection still DELIVERS: a live event reaches the very
    // first subscription.
    let mut writer = connect_as(&env, &owner).await;
    let live = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "still delivering at the cap",
        T0_SECS,
    );
    publish_ok(&mut writer, &live).await;
    assert!(
        receives(&mut reader, "view-1", &live, ATTEMPT).await,
        "a connection at the cap must still deliver on its live subscriptions"
    );

    // CLOSE frees exactly one slot: the next REQ is served again.
    let last = format!("fill-{}", ws::MAX_SUBSCRIPTIONS);
    close_until_dropped(&env, &mut reader, &last, ws::MAX_SUBSCRIPTIONS - 1).await;
    subscribe_served(&mut reader, "after-close", &slot_req("after-close")).await;
    let doc = connection_doc(&env, &reader).await.expect("a document");
    assert!(
        doc.subscription("after-close").is_some(),
        "the freed slot is usable"
    );
    assert_eq!(
        doc.subscriptions.len(),
        ws::MAX_SUBSCRIPTIONS,
        "full again, not leaking"
    );

    });
}
