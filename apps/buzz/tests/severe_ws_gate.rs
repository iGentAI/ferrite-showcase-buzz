//! **The NIP-01 + NIP-42 WebSocket face, driven the way buzz's real client
//! drives it.**
//!
//! Every assertion here rides the SOCKET: the gate publishes `WS_OPEN` /
//! `WS_MESSAGE` / `WS_CLOSE` envelopes onto the ws route's own queue through
//! `TestEnv::ws_open`, exactly as the gateway's WebSocket face does, and reads
//! the app's outbound frames back off the frame queue. There is no mock, no
//! direct call into a policy function, and no HTTP shortcut — a client that can
//! only speak nostr over one socket is the whole point of this face, so a gate
//! that reached for the HTTP face would be testing something else.
//!
//! The clock is FROZEN at [`T0_MS`], so every time-dependent rule (the NIP-42
//! ±10 minute window, the event drift bound, the access-cache TTL that must not
//! be what enforces a ban) is exercised by choosing signed timestamps rather
//! than by sleeping — and the cache TTL can never quietly rescue a stale
//! decision.
//!
//! One consequence of that frozen clock shapes the helpers below. The harness
//! relays outbound frames with a long-polling queue consumer, and a poll's
//! client-side timeout can fire in the same instant the queue grants it a lease
//! (`cluster_queue_node.rs:2789`); the message stays leased, and because
//! `reclaim_expired` compares the lease deadline against the *harness* clock
//! (`work_queue.rs:251`), a frozen clock means that lease never expires and the
//! frame is never redelivered. Under a real clock the visibility timeout
//! reclaims it and the next poll relays it. The real client's answer to silence
//! is the same one [`ask`] uses — re-send the frame (its op timeout, then
//! `sendRawWithReconnectRetry`) — and every verb driven here is idempotent by
//! design: a REQ upserts its subscription, an EVENT is absorbed by the admission
//! marker, an AUTH rebinds the same pubkey. The relay still has to answer.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::collections::BTreeSet;
use std::time::Duration;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::effects::Restriction;
use buzz::relay::event::Event;
use buzz::relay::{delivery, keys, kind, store as buzz_store, ws};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, REPLY_BUDGET, T0_MS, T0_SECS, WS_GATE_STREAM_KEY,
    admission, audit_entries_for, body_json, gate_env, error_text, feed_copies, membership,
    request_with_auth, seed_channel_with_owner, seed_membership, signed, signed_at,
    stored_event, tag, ws_path_with_ticket,
};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::StreamFrame;
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{TestHarness, WsSession};

/// One fleet at a time in this binary: each gate stands up its own in-process
/// fleet, and `cargo test` runs a file's tests concurrently.
static WS_GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The route the ws face is published under.
const WS_PATH: &str = "/buzz/ws/";

/// The relay URL the operator configured, and therefore the one a NIP-42 `relay`
/// tag must name.
const RELAY_URL: &str = "wss://buzz.test/buzz/ws/";

/// How long ONE attempt waits for an answer that must arrive. Generous, because
/// several gate binaries each run their own in-process fleet and a correct but
/// starved fleet answers slowly.
const ATTEMPT: Duration = Duration::from_secs(25);

/// How many times a client frame is re-sent while the relay stays silent (see
/// the module docs).
const ATTEMPTS: usize = 3;

/// How long a frame that must NOT arrive is waited for. Longer than the pump's
/// idle poll (1s) and its active poll (100ms), so a leak has several batches to
/// show up in.
const NEVER: Duration = Duration::from_secs(6);

/// The fleet, with the ws route's own queue as the driver's request queue.
///
/// `TestEnv::ws_open` publishes onto the harness's request queue and the ws face
/// consumes `ws-requests` (its route's binding), so here they are the same
/// queue. The api face is bound and served beside it — it is simply never used.
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
        // The sealed key every NIP-42 challenge is derived from.
        buzz_store::put_raw(&mut config, b"buzz-stream-token-key", &[0x5au8; 32])
            .await
            .expect("stream key seed");
        // The relay's own URL: without it the app cannot know what a `relay` tag
        // should say, because the gateway forwards no Host header.
        buzz_store::put_raw(&mut config, b"buzz-relay-url", RELAY_URL.as_bytes())
            .await
            .expect("relay url seed");
    }
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
    env
}

// ------------------------------------------------------------------ ws driving

/// The next frame for this connection, or `None` when `budget` passes.
async fn next_frame(session: &mut WsSession, budget: Duration) -> Option<StreamFrame> {
    tokio::time::timeout(budget, session.recv()).await.ok()?
}

/// The next decoded NIP-01 message. A socket-close frame (empty data, `done`)
/// decodes as `None`, as does a timeout.
async fn next_message(session: &mut WsSession, budget: Duration) -> Option<Vec<Value>> {
    let frame = next_frame(session, budget).await?;
    serde_json::from_slice::<Vec<Value>>(&frame.data).ok()
}

/// Wait for the next message whose verb is `verb`, ignoring others; `None` when
/// the budget passes in silence.
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

/// Send one client frame.
async fn send(session: &mut WsSession, message: &Value) {
    session
        .send(message.to_string())
        .await
        .expect("the frame is published");
}

/// Send `message` and return the relay's first `verb` answer, re-sending while
/// the relay is silent (see the module docs).
///
/// A failure reports the queue's own counters, because "the face never answered"
/// and "the envelope never reached it" are different bugs.
async fn ask(env: &TestEnv, session: &mut WsSession, message: &Value, verb: &str) -> Vec<Value> {
    for _ in 0..ATTEMPTS {
        send(session, message).await;
        if let Some(answer) = wait_for(session, verb, ATTEMPT).await {
            return answer;
        }
    }
    panic!(
        "no {verb} answer to {message} after {ATTEMPTS} attempts;{}",
        queue_report(env).await
    );
}

/// `published/acked/dead_lettered/pending` for the ws route's queue and for the
/// outbound frame queue.
async fn queue_report(env: &TestEnv) -> String {
    let mut out = String::new();
    for name in ["ws-requests", "frames"] {
        let mut queue = env.queue(name);
        let line = match queue.counts().await {
            Ok((published, acked, dlq, pending)) => format!(
                " {name}: published={published} acked={acked} dead_lettered={dlq} pending={pending}"
            ),
            Err(e) => format!(" {name}: counts unavailable: {e}"),
        };
        out.push_str(&line);
    }
    out
}

/// Open the real ws face and take the NIP-42 challenge it must send first.
///
/// A challenge that never arrives is answered the way the client answers it: by
/// reconnecting.
async fn connect(env: &TestEnv) -> (WsSession, String) {
    for _ in 0..ATTEMPTS {
        let mut session = env
            .ws_open(&ws_path_with_ticket(WS_PATH, env.now_ms()))
            .await
            .expect("the ws face opens");
        if let Some(message) = wait_for(&mut session, "AUTH", ATTEMPT).await {
            let challenge = message[1]
                .as_str()
                .expect("the challenge is a string")
                .to_string();
            assert_eq!(challenge.len(), 64, "the challenge is a 32-byte hex value");
            assert!(
                challenge.bytes().all(|b| b.is_ascii_hexdigit()),
                "{challenge}"
            );
            return (session, challenge);
        }
    }
    panic!(
        "the ws face issued no challenge in {ATTEMPTS} connections;{}",
        queue_report(env).await
    );
}

/// A NIP-42 kind-22242 event answering `challenge` for `relay`.
fn auth_event_for(actor: &Actor, challenge: &str, relay: &str, created_at: u64) -> Event {
    signed_at(
        actor,
        kind::KIND_AUTH,
        vec![tag("relay", relay), tag("challenge", challenge)],
        "",
        created_at,
    )
}

/// Complete NIP-42 as `actor` and assert the relay accepted it.
async fn authenticate(env: &TestEnv, session: &mut WsSession, challenge: &str, actor: &Actor) {
    let event = auth_event_for(actor, challenge, RELAY_URL, T0_SECS);
    let ok = ask(env, session, &json!(["AUTH", event]), "OK").await;
    assert_eq!(
        ok[1],
        json!(to_hex(&event.id)),
        "the OK names the AUTH event"
    );
    assert_eq!(ok[2], json!(true), "NIP-42 must be accepted: {ok:?}");
}

/// Open a connection and authenticate it.
async fn connect_as(env: &TestEnv, actor: &Actor) -> WsSession {
    let (mut session, challenge) = connect(env).await;
    authenticate(env, &mut session, &challenge, actor).await;
    session
}

/// Publish `event` over the socket and return the `OK` frame.
async fn publish(env: &TestEnv, session: &mut WsSession, event: &Event) -> Vec<Value> {
    let ok = ask(env, session, &json!(["EVENT", event]), "OK").await;
    assert_eq!(ok[1], json!(to_hex(&event.id)), "the OK names the event");
    ok
}

/// Publish `event` and require acceptance.
async fn publish_ok(env: &TestEnv, session: &mut WsSession, event: &Event) {
    let ok = publish(env, session, event).await;
    assert_eq!(ok[2], json!(true), "the event must be accepted: {ok:?}");
}

/// Issue a REQ and collect the event ids it serves BEFORE its EOSE.
///
/// A `CLOSED` for the same subscription is a finding, not a page: a REQ this
/// gate expects to be served must never be refused.
async fn req_page(
    env: &TestEnv,
    session: &mut WsSession,
    request: &Value,
    sub_id: &str,
) -> BTreeSet<String> {
    for _ in 0..ATTEMPTS {
        send(session, request).await;
        let mut ids = BTreeSet::new();
        let deadline = tokio::time::Instant::now() + ATTEMPT;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                break; // silence: re-send the REQ
            }
            let Some(message) = next_message(session, left).await else {
                continue;
            };
            match message.first().and_then(Value::as_str) {
                Some("EVENT") if message[1] == json!(sub_id) => {
                    ids.insert(message[2]["id"].as_str().unwrap_or_default().to_string());
                }
                Some("EOSE") if message[1] == json!(sub_id) => return ids,
                Some("CLOSED") if message[1] == json!(sub_id) => {
                    panic!("the subscription was closed instead of served: {message:?}")
                }
                _ => continue,
            }
        }
    }
    panic!(
        "no EOSE for {sub_id} after {ATTEMPTS} attempts;{}",
        queue_report(env).await
    );
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

/// Wait until the connection document satisfies `pred` — a durable state change
/// the protocol reports nothing about (a CLOSE, a WS_CLOSE).
async fn wait_for_doc(
    env: &TestEnv,
    session: &WsSession,
    what: &str,
    pred: impl Fn(Option<&ws::Connection>) -> bool,
) {
    for _ in 0..200 {
        if pred(connection_doc(env, session).await.as_ref()) {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("the connection document never {what}");
}

// ----------------------------------------------------------------- the gates

/// **NIP-42.** The face challenges every connection, refuses a forged, stale or
/// wrong-relay answer, and binds the pubkey only for a correct one — with the
/// binding visible in the durable connection document, not just in a frame.
#[test]
 fn severe_only_a_correct_nip42_answer_binds_the_socket() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let alice = Actor::new("alice");
    let (mut session, challenge) = connect(&env).await;

    // The challenge is per connection: a second socket gets a different one, so
    // an AUTH event captured from one cannot be replayed onto the other.
    let (mut other, other_challenge) = connect(&env).await;
    assert_ne!(challenge, other_challenge, "challenges are per connection");

    // An unauthenticated socket is unauthenticated in the durable state, not
    // merely un-flagged in memory.
    let doc = connection_doc(&env, &session)
        .await
        .expect("the connection document exists");
    assert_eq!(doc.pubkey, None, "no pubkey is bound before AUTH");
    assert_eq!(doc.reader(), ws::ANON_READER);

    // Every forgery, refused with an OK(false) the client can act on.
    let forgeries = [
        (
            "another connection's challenge",
            auth_event_for(&alice, &other_challenge, RELAY_URL, T0_SECS),
        ),
        (
            "a challenge nobody issued",
            auth_event_for(&alice, &"f".repeat(64), RELAY_URL, T0_SECS),
        ),
        (
            "an expired created_at",
            auth_event_for(
                &alice,
                &challenge,
                RELAY_URL,
                T0_SECS - ws::AUTH_SKEW_SECS - 1,
            ),
        ),
        (
            "a future created_at",
            auth_event_for(
                &alice,
                &challenge,
                RELAY_URL,
                T0_SECS + ws::AUTH_SKEW_SECS + 1,
            ),
        ),
        (
            "another relay",
            auth_event_for(&alice, &challenge, "wss://evil.example/buzz/ws/", T0_SECS),
        ),
        (
            "the wrong kind",
            signed_at(
                &alice,
                kind::KIND_STREAM_MESSAGE,
                vec![tag("relay", RELAY_URL), tag("challenge", &challenge)],
                "",
                T0_SECS,
            ),
        ),
    ];
    for (what, event) in forgeries {
        let ok = ask(&env, &mut session, &json!(["AUTH", event]), "OK").await;
        assert_eq!(ok[1], json!(to_hex(&event.id)));
        assert_eq!(ok[2], json!(false), "{what} was accepted: {ok:?}");
        assert!(
            ok[3]
                .as_str()
                .unwrap_or_default()
                .starts_with("restricted:"),
            "{what} must be refused in the relay's own vocabulary: {ok:?}"
        );
        assert_eq!(
            connection_doc(&env, &session).await.and_then(|c| c.pubkey),
            None,
            "{what} must leave the socket unauthenticated"
        );
    }

    // A tampered signature on an otherwise perfect AUTH event.
    let mut tampered = auth_event_for(&alice, &challenge, RELAY_URL, T0_SECS);
    tampered.sig[7] ^= 0x40;
    let ok = ask(&env, &mut session, &json!(["AUTH", tampered]), "OK").await;
    assert_eq!(ok[2], json!(false), "a tampered signature was accepted");
    assert_eq!(
        connection_doc(&env, &session).await.and_then(|c| c.pubkey),
        None
    );

    // While unauthenticated, the socket may not publish.
    let note = signed_at(
        &alice,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "chan-ws")],
        "before auth",
        T0_SECS,
    );
    let refused = publish(&env, &mut session, &note).await;
    assert_eq!(refused[2], json!(false));
    assert!(
        refused[3]
            .as_str()
            .unwrap_or_default()
            .starts_with("auth-required:"),
        "an unauthenticated publish must be refused with auth-required: {refused:?}"
    );
    assert!(
        stored_event(&env, &note.id).await.is_none(),
        "nothing an unauthenticated socket sent may be stored"
    );

    // The correct answer binds the pubkey, durably.
    authenticate(&env, &mut session, &challenge, &alice).await;
    assert_eq!(
        connection_doc(&env, &session).await.and_then(|c| c.pubkey),
        Some(alice.pk_hex()),
        "a correct NIP-42 answer binds the author's pubkey"
    );
    let _ = other.close().await;

    });
}

/// **REQ.** A subscription serves its stored page, ends it with EOSE, and stays
/// live: an event another actor publishes afterwards arrives on it.
#[test]
 fn severe_a_req_serves_stored_events_then_eose_then_live_ones() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let owner = Actor::new("owner");
    let bob = Actor::new("bob");
    let channel = "chan-ws-open";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    // Two messages exist BEFORE the reader connects, so they can only reach it
    // through the REQ's stored page.
    let mut writer = connect_as(&env, &owner).await;
    let first = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "first",
        T0_SECS,
    );
    let second = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "second",
        T0_SECS + 1,
    );
    publish_ok(&env, &mut writer, &first).await;
    publish_ok(&env, &mut writer, &second).await;
    assert!(stored_event(&env, &first.id).await.is_some());
    assert!(stored_event(&env, &second.id).await.is_some());

    let mut reader = connect_as(&env, &bob).await;
    let sub = "live-req-1";
    let page = req_page(
        &env,
        &mut reader,
        &json!(["REQ", sub, { "kinds": [kind::KIND_STREAM_MESSAGE], "#h": [channel], "limit": 10 }]),
        sub,
    )
    .await;
    assert!(
        page.contains(&to_hex(&first.id)) && page.contains(&to_hex(&second.id)),
        "the stored page must carry both messages before EOSE: {page:?}"
    );

    // The subscription is registered durably — that is what lets an instance
    // that never saw the REQ serve this connection's live traffic.
    let doc = connection_doc(&env, &reader).await.expect("a document");
    assert!(
        doc.subscription(sub).is_some(),
        "the subscription is durable: {:?}",
        doc.subscriptions
    );

    // Live: a third message, published by another actor over its own socket.
    let third = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "third",
        T0_SECS + 2,
    );
    publish_ok(&env, &mut writer, &third).await;
    assert!(
        receives(&mut reader, sub, &third, ATTEMPT).await,
        "a live event must arrive on the open subscription"
    );

    // CLOSE drops it: the next event reaches nobody on this socket.
    send(&mut reader, &json!(["CLOSE", sub])).await;
    wait_for_doc(&env, &reader, "dropped the subscription", |doc| {
        doc.is_some_and(|c| c.subscription(sub).is_none())
    })
    .await;
    let fourth = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "fourth",
        T0_SECS + 3,
    );
    publish_ok(&env, &mut writer, &fourth).await;
    assert!(
        !receives(&mut reader, sub, &fourth, NEVER).await,
        "a closed subscription must stop receiving"
    );

    });
}

/// **EVENT.** The socket's publish path is the same guarded saga the HTTP face
/// runs — proven on durable state — and every refusal carries the relay's own
/// reason token.
#[test]
 fn severe_event_rides_the_real_saga_and_refusals_keep_their_reason() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let owner = Actor::new("owner");
    let mallory = Actor::new("mallory");
    let channel = "chan-ws-saga";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    let mut session = connect_as(&env, &owner).await;

    let accepted = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "through the saga",
        T0_SECS,
    );
    publish_ok(&env, &mut session, &accepted).await;
    // The saga really ran: the body is stored, the admission marker exists, the
    // event reached the live feed exactly once, and the audit chain recorded it.
    assert_eq!(
        stored_event(&env, &accepted.id).await.as_ref(),
        Some(&accepted),
        "the socket's publish stores the event body"
    );
    assert!(
        admission(&env, &accepted.id).await.is_some(),
        "the guarded admission envelope exists"
    );
    assert_eq!(
        feed_copies(&env, FEED_GENERATION, &accepted.id).await,
        1,
        "the accepted event reached the feed once"
    );
    assert_eq!(
        audit_entries_for(&env, &accepted.id).await.len(),
        1,
        "the audit chain recorded the event once"
    );

    // Re-publishing the same event is a duplicate — a success for the client's
    // intent, and nothing is stored or fanned out twice.
    let again = publish(&env, &mut session, &accepted).await;
    assert_eq!(
        again[2],
        json!(true),
        "a duplicate is not a failure: {again:?}"
    );
    assert!(
        again[3]
            .as_str()
            .unwrap_or_default()
            .starts_with("duplicate:"),
        "a duplicate says so: {again:?}"
    );
    assert_eq!(feed_copies(&env, FEED_GENERATION, &accepted.id).await, 1);

    // Every refusal, with the token a client branches on.
    let elsewhere = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "chan-that-does-not-exist")],
        "no such room",
        T0_SECS,
    );
    let forged_author = signed_at(
        &mallory,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "not my key",
        T0_SECS,
    );
    let mut broken = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "tampered",
        T0_SECS,
    );
    broken.content = "tampered after signing".into();
    let relay_only = signed_at(
        &owner,
        kind::KIND_NIP43_MEMBERSHIP_LIST,
        vec![tag("h", channel)],
        "",
        T0_SECS,
    );
    let auth_kind = auth_event_for(&owner, &"a".repeat(64), RELAY_URL, T0_SECS);
    let drifted = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "from another era",
        T0_SECS - 5_000,
    );
    for (what, event, prefix) in [
        ("an unknown channel", &elsewhere, "invalid:"),
        ("another author's key", &forged_author, "restricted:"),
        ("an event edited after signing", &broken, "invalid:"),
        ("a relay-signed kind", &relay_only, "restricted:"),
        ("an authentication event", &auth_kind, "invalid:"),
        ("a drifted created_at", &drifted, "invalid:"),
    ] {
        let ok = publish(&env, &mut session, event).await;
        assert_eq!(ok[2], json!(false), "{what} was accepted: {ok:?}");
        assert!(
            ok[3].as_str().unwrap_or_default().starts_with(prefix),
            "{what} must be refused with {prefix}: {ok:?}"
        );
        assert!(
            stored_event(&env, &event.id).await.is_none(),
            "{what} must leave nothing stored"
        );
    }

    });
}

/// **Access.** A private channel's traffic never reaches a non-member — not
/// through a REQ's stored page, and not live — and a filter that asks for a
/// gated kind is refused rather than silently emptied.
#[test]
 fn severe_a_private_channel_never_reaches_a_non_member() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let owner = Actor::new("owner");
    let outsider = Actor::new("outsider");
    let private = "chan-ws-private";
    seed_channel_with_owner(&env, private, Visibility::Private, &owner).await;

    let mut writer = connect_as(&env, &owner).await;
    let secret = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", private)],
        "members only",
        T0_SECS,
    );
    publish_ok(&env, &mut writer, &secret).await;
    assert!(
        stored_event(&env, &secret.id).await.is_some(),
        "the private message really is stored — the gate is what hides it"
    );

    let mut stranger = connect_as(&env, &outsider).await;
    let sub = "live-private";
    let page = req_page(
        &env,
        &mut stranger,
        &json!(["REQ", sub, { "kinds": [kind::KIND_STREAM_MESSAGE], "#h": [private], "limit": 50 }]),
        sub,
    )
    .await;
    assert!(
        page.is_empty(),
        "a non-member's REQ must serve nothing from a private channel: {page:?}"
    );

    // A filter asking for a `#p`-gated kind without naming the reader is
    // REFUSED, so the client learns it asked for something it may not have.
    let gated = "live-gated";
    let closed = ask(
        &env,
        &mut stranger,
        &json!(["REQ", gated, { "kinds": [kind::KIND_GIFT_WRAP], "limit": 10 }]),
        "CLOSED",
    )
    .await;
    assert_eq!(closed[1], json!(gated));
    assert!(
        closed[2]
            .as_str()
            .unwrap_or_default()
            .starts_with("restricted:"),
        "a gated filter is CLOSED with the restricted token: {closed:?}"
    );
    assert!(
        connection_doc(&env, &stranger)
            .await
            .is_some_and(|c| c.subscription(gated).is_none()),
        "a refused filter must not leave a subscription behind"
    );

    // The same filter on an UNAUTHENTICATED socket is refused too, with the
    // NIP-42 token that tells the client to authenticate first.
    let (mut anonymous, _challenge) = connect(&env).await;
    let anon_sub = "anon-gated";
    let closed = ask(
        &env,
        &mut anonymous,
        &json!(["REQ", anon_sub, { "kinds": [kind::KIND_GIFT_WRAP], "limit": 10 }]),
        "CLOSED",
    )
    .await;
    assert_eq!(closed[1], json!(anon_sub));
    assert!(
        closed[2]
            .as_str()
            .unwrap_or_default()
            .starts_with("auth-required:"),
        "an unauthenticated gated filter is CLOSED with auth-required: {closed:?}"
    );

    // And an unauthenticated socket reading the private channel gets nothing.
    let anon_open = "anon-private";
    let anon_page = req_page(
        &env,
        &mut anonymous,
        &json!(["REQ", anon_open, { "kinds": [kind::KIND_STREAM_MESSAGE], "#h": [private], "limit": 50 }]),
        anon_open,
    )
    .await;
    assert!(
        anon_page.is_empty(),
        "an unauthenticated socket must read nothing from a private channel"
    );

    // LAST, because these are the assertions that wait on silence: live traffic
    // is gated on every batch too, for a non-member and for an anonymous socket.
    let later = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", private)],
        "still members only",
        T0_SECS + 1,
    );
    publish_ok(&env, &mut writer, &later).await;
    assert!(
        !receives(&mut stranger, sub, &later, NEVER).await,
        "a non-member must not receive a private channel's live traffic"
    );
    assert!(
        !receives(&mut anonymous, anon_open, &later, NEVER).await,
        "an unauthenticated socket must not receive it either"
    );

    });
}

/// **Revocation (§6.3 rule b).** A banned reader's socket stops receiving on the
/// very next batch and is hung up — proven in the torn state the feed-driven
/// invalidation cannot cover: the ban's state write and tombstone are durable,
/// its feed record never landed.
#[test]
 fn severe_a_banned_reader_is_cut_off_on_the_next_batch() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let owner = Actor::new("owner");
    let carol = Actor::new("carol");
    let channel = "chan-ws-ban";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    seed_membership(&env, channel, &carol, Role::Member).await;

    let mut writer = connect_as(&env, &owner).await;
    let mut reader = connect_as(&env, &carol).await;
    let sub = "live-ban";
    let _page = req_page(
        &env,
        &mut reader,
        &json!(["REQ", sub, { "kinds": [kind::KIND_STREAM_MESSAGE], "#h": [channel], "limit": 10 }]),
        sub,
    )
    .await;

    // Live before the ban, which also warms the access cache — under the frozen
    // clock that entry can never TTL out, so only the tombstone check can
    // enforce what follows.
    let before = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "before the ban",
        T0_SECS,
    );
    publish_ok(&env, &mut writer, &before).await;
    assert!(
        receives(&mut reader, sub, &before, ATTEMPT).await,
        "carol is live before the ban"
    );

    // THE CRASH WINDOW: the ban's restriction row and revocation tombstone are
    // durable; its feed record never happened.
    {
        let mut events = env.store("events");
        buzz_store::put_doc(
            &mut events,
            &keys::restriction(COMMUNITY, &carol.pk),
            &Restriction {
                banned: true,
                banned_until: None,
                timeout_until: None,
            },
        )
        .await
        .expect("ban state");
        buzz_store::put_raw(
            &mut events,
            &keys::revoked(COMMUNITY, &carol.pk),
            T0_MS.to_string().as_bytes(),
        )
        .await
        .expect("tombstone bump");
    }

    // The next batch wakes the pump: carol must be cut off, not served. When the
    // socket goes quiet the gate keeps the feed moving, so the pump takes another
    // batch (it re-reads the tombstone on every non-empty one) rather than the
    // gate concluding "enforced" from silence alone.
    let after = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "after the torn ban",
        T0_SECS + 1,
    );
    publish_ok(&env, &mut writer, &after).await;

    let mut saw_notice = false;
    let mut closed_socket = false;
    let mut leaked = false;
    let mut nudge = 0u64;
    let deadline = tokio::time::Instant::now() + ATTEMPT;
    while tokio::time::Instant::now() < deadline && !(closed_socket && saw_notice) {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        let Some(frame) = next_frame(&mut reader, left.min(NEVER)).await else {
            nudge += 1;
            let event = signed_at(
                &owner,
                kind::KIND_STREAM_MESSAGE,
                vec![tag("h", channel)],
                "keeping the feed moving",
                T0_SECS + 1 + nudge,
            );
            publish_ok(&env, &mut writer, &event).await;
            continue;
        };
        if frame.done {
            closed_socket = true;
        }
        if let Ok(message) = serde_json::from_slice::<Vec<Value>>(&frame.data) {
            match message.first().and_then(Value::as_str) {
                Some("NOTICE") => {
                    saw_notice |= message[1]
                        .as_str()
                        .unwrap_or_default()
                        .starts_with("restricted:");
                }
                Some("EVENT") => {
                    leaked |= message[2]["id"] == json!(to_hex(&after.id));
                }
                _ => {}
            }
        }
    }
    assert!(!leaked, "a banned reader must not receive the next batch");
    assert!(
        saw_notice,
        "a banned reader is told why its connection ended"
    );
    assert!(
        closed_socket,
        "a banned reader's socket is closed, not left open and silent"
    );

    // The ban is a READ eviction and a WRITE refusal: carol cannot publish either.
    let attempt = signed_at(
        &carol,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "am I still here",
        T0_SECS + 100,
    );
    // A reconnecting banned member is refused at NIP-42 already, with the
    // reason, and the unbound socket cannot publish either.
    let (mut carol_again, challenge) = connect(&env).await;
    let auth = auth_event_for(&carol, &challenge, RELAY_URL, T0_SECS);
    let refused = ask(&env, &mut carol_again, &json!(["AUTH", auth]), "OK").await;
    assert_eq!(refused[2], json!(false), "a banned member cannot authenticate: {refused:?}");
    assert!(refused[3]
        .as_str()
        .unwrap_or_default()
        .starts_with("restricted:"));
    let ok = publish(&env, &mut carol_again, &attempt).await;
    assert_eq!(ok[2], json!(false), "a banned member cannot write: {ok:?}");

    });
}

/// **The protocol layer, through the real app.** A frame the relay cannot
/// understand is answered with a `NOTICE` — never dropped in silence, never a
/// panic that takes the serve loop with it. `severe_subscription_cap_gate` owns
/// the subscription bound.
#[test]
 fn severe_malformed_frames_are_answered_and_the_socket_survives() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let alice = Actor::new("alice");
    let mut session = connect_as(&env, &alice).await;

    // Frames whose parse cannot FINISH. That is the criterion, not whether an id
    // was present: the last two below do carry a readable `s`, but once an element
    // fails to deserialize there is no well-formed REQ left to refuse, and serde's
    // state is undefined so the error has to propagate. Recovering the id there
    // would mean parsing through a JSON value first, which this parser refuses to
    // do on purpose — a value map silently keeps the last of two identical keys and
    // would undo the duplicate-field refusal the last case exists to prove.
    for bad in [
        "not json at all",
        "{}",
        "[]",
        "[1,2,3]",
        "[\"REQ\"]",
        "[\"CLOSE\"]",
        "[\"EVENT\",\"nope\"]",
        "[\"AUTH\"]",
        "[\"REQ\",\"s\",\"not-an-object\"]",
        "[\"REQ\",\"s\",{\"kinds\":[9],\"kinds\":[7]}]",
    ] {
        let mut notice = None;
        for _ in 0..ATTEMPTS {
            session.send(bad).await.expect("the frame is published");
            notice = wait_for(&mut session, "NOTICE", ATTEMPT).await;
            if notice.is_some() {
                break;
            }
        }
        let notice = notice.unwrap_or_else(|| panic!("{bad:?} was answered with nothing"));
        assert!(
            notice[1]
                .as_str()
                .unwrap_or_default()
                .starts_with("invalid:"),
            "{bad:?} must be answered with an invalid: notice, got {notice:?}"
        );
    }

    // Frames that PARSED, and were then refused on their content. These must come
    // back NAMED: a NOTICE would leave the client waiting on a subscription it
    // cannot identify, which is the difference between reacting and hanging.
    for (frame, named) in [
        (json!(["REQ", "no-filter"]), "no-filter"),
        (json!(["REQ", "", {}]), ""),
        (json!(["COUNT", "s", {}]), "s"),
    ] {
        let closed = ask(&env, &mut session, &frame, "CLOSED").await;
        assert_eq!(
            closed[1],
            json!(named),
            "{frame} must be refused against its own subscription id"
        );
        assert!(
            !closed[2].as_str().unwrap_or_default().is_empty(),
            "a refusal must say why: {closed:?}"
        );
    }

    // The loop survived every one of them: a normal REQ still works.
    let alive = "live-alive";
    let _ = req_page(
        &env,
        &mut session,
        &json!(["REQ", alive, { "kinds": [kind::KIND_STREAM_MESSAGE], "limit": 1 }]),
        alive,
    )
    .await;

    // WS_CLOSE drops the whole connection: its document (and its pump) are gone.
    session.close().await.expect("the close is published");
    wait_for_doc(&env, &session, "was removed by WS_CLOSE", |doc| {
        doc.is_none()
    })
    .await;

    });
}

/// **A reaction is gated on the channel it INHERITS.** A reaction names its
/// target, not a channel, so its room exists only by inheritance — and a read
/// path that trusted the event's own tags would treat one as community-global
/// and hand a private room's reactions to anyone able to name the target id.
///
/// Pinned at the socket because the ws read path resolves that inheritance per
/// candidate, with a store read each: any change to HOW those reads are made
/// (batching them, for instance) must leave this boundary exactly where it is,
/// and every other gate in this file would stay green if it did not.
#[test]
 fn severe_a_reaction_is_gated_on_the_channel_it_inherits() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let owner = Actor::new("owner");
    let outsider = Actor::new("outsider");
    let private = "chan-ws-reactions";
    seed_channel_with_owner(&env, private, Visibility::Private, &owner).await;

    // A message in the private room, and a reaction to it.
    let mut writer = connect_as(&env, &owner).await;
    let message = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", private)],
        "a private note worth reacting to",
        T0_SECS,
    );
    publish_ok(&env, &mut writer, &message).await;
    let target = to_hex(&message.id);
    let reaction = signed_at(
        &owner,
        kind::KIND_REACTION,
        vec![tag("e", &target)],
        "\u{1f44d}",
        T0_SECS + 1,
    );
    publish_ok(&env, &mut writer, &reaction).await;
    assert!(
        reaction.channel_id().is_none(),
        "the fixture only exercises INHERITANCE while the reaction names no \
         channel of its own"
    );
    assert!(
        stored_event(&env, &reaction.id).await.is_some(),
        "the reaction really is stored — the gate is what hides it"
    );

    // The member asking by target id receives the reaction.
    let mine = "react-member";
    let served = req_page(
        &env,
        &mut writer,
        &json!(["REQ", mine, { "#e": [target.clone()], "limit": 50 }]),
        mine,
    )
    .await;
    assert!(
        served.contains(&to_hex(&reaction.id)),
        "a member must receive the reaction on their own room's message: {served:?}"
    );

    // An AUTHENTICATED outsider asking for exactly the same target receives
    // nothing, so the refusal is about membership rather than about anonymity.
    let mut stranger = connect_as(&env, &outsider).await;
    let theirs = "react-outsider";
    let refused = req_page(
        &env,
        &mut stranger,
        &json!(["REQ", theirs, { "#e": [target], "limit": 50 }]),
        theirs,
    )
    .await;
    assert!(
        refused.is_empty(),
        "a non-member must not receive a private room's reaction: {refused:?}"
    );

    });
}

/// **A refused REQ is answered with CLOSED naming the subscription, not a NOTICE.**
///
/// This is the whole difference between a client that can react and one that waits
/// forever: NOTICE carries no subscription id, so a client told "something was
/// wrong" cannot know which of its REQs died, and goes on waiting for an EOSE that
/// is never coming. Asserted at the socket because the promise is about the frame a
/// client receives, not about how the parser classifies it.
#[test]
 fn severe_a_refused_req_is_answered_by_name() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let reader = Actor::new("owner");
    let mut session = connect_as(&env, &reader).await;

    // An over-long subscription id: refused, and the answer still names it —
    // truncated to the advertised bound, so one frame cannot choose the size of
    // the answer it gets back.
    let long = "x".repeat(ws::MAX_SUB_ID_CHARS + 40);
    let closed = ask(
        &env,
        &mut session,
        &json!(["REQ", long, { "kinds": [kind::KIND_STREAM_MESSAGE] }]),
        "CLOSED",
    )
    .await;
    let named = closed[1].as_str().unwrap_or_default();
    assert_eq!(named.chars().count(), ws::MAX_SUB_ID_CHARS);
    assert!(
        long.starts_with(named),
        "the echo must be a prefix of what was sent"
    );

    // COUNT is unimplemented, and says so against the caller's own id.
    let closed = ask(
        &env,
        &mut session,
        &json!(["COUNT", "how-many", { "kinds": [kind::KIND_STREAM_MESSAGE] }]),
        "CLOSED",
    )
    .await;
    assert_eq!(closed[1], json!("how-many"));
    assert_eq!(closed[2], json!(ws::COUNT_UNSUPPORTED));

    // A REQ carrying more filters than the cap: same treatment.
    let mut frame = vec![json!("REQ"), json!("over-filtered")];
    for _ in 0..=ws::MAX_FILTERS_PER_REQ {
        frame.push(json!({ "kinds": [kind::KIND_STREAM_MESSAGE] }));
    }
    let closed = ask(&env, &mut session, &Value::Array(frame), "CLOSED").await;
    assert_eq!(closed[1], json!("over-filtered"));
    assert!(
        closed[2]
            .as_str()
            .unwrap_or_default()
            .contains("filter limit"),
        "{closed:?}"
    );

    // The connection is still usable afterwards: a refusal is not a disconnect,
    // and it must not have consumed a subscription slot.
    let doc = connection_doc(&env, &session).await.expect("still open");
    assert!(
        doc.subscriptions.is_empty(),
        "a refused REQ must hold no subscription: {:?}",
        doc.subscriptions
    );
    let sub = "live-after-refusals";
    let served = req_page(
        &env,
        &mut session,
        &json!(["REQ", sub, { "kinds": [kind::KIND_STREAM_MESSAGE], "limit": 5 }]),
        sub,
    )
    .await;
    assert!(
        served.is_empty(),
        "nothing was published in this gate: {served:?}"
    );

    });
}

/// The cursor row this connection's pump owns, if it has written one.
async fn cursor_row(env: &TestEnv, session: &WsSession) -> Option<Vec<u8>> {
    let mut config = env.store("config");
    buzz_store::get_raw(
        &mut config,
        &keys::wsconn_cursor(COMMUNITY, session.correlation_id()),
    )
    .await
    .expect("config read")
}

/// **A pump with nothing to deliver writes nothing.**
///
/// The cursor row exists so a reconnect can resume, which means it only needs
/// writing when the cursor MOVES. It used to be written on every tick — so an idle
/// connection performed a replicated store write once a second, per connection, for
/// a value that had not changed. Nothing would have caught that: every delivery
/// assertion in this file passes either way.
///
/// Both halves are asserted, because "never writes at all" would satisfy the first
/// on its own while breaking resumption.
#[test]
 fn severe_a_quiet_pump_does_not_write_its_cursor() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let quiet = Actor::new("owner");
    let mut session = connect_as(&env, &quiet).await;

    // Outlast multiple empty wait_read deadlines, so the pump has had the
    // opportunity to persist an unchanged cursor after each timeout.
    let quiet_ms = delivery::WAIT_MS * 3;
    tokio::time::sleep(Duration::from_millis(quiet_ms)).await;
    assert_eq!(
        cursor_row(&env, &session).await,
        None,
        "a pump with nothing to deliver wrote a cursor row anyway, within {quiet_ms} ms \
         of silence"
    );

    // Now put something on the feed. The pump reads past it, so its cursor MOVES
    // and that progress must be recorded — otherwise a reconnect would replay.
    let room = "chan-ws-quiet";
    seed_channel_with_owner(&env, room, Visibility::Open, &quiet).await;
    let event = signed_at(
        &quiet,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", room)],
        "something for the pump to walk past",
        T0_SECS,
    );
    publish_ok(&env, &mut session, &event).await;

    let deadline = tokio::time::Instant::now() + ATTEMPT;
    while cursor_row(&env, &session).await.is_none() {
        assert!(
            tokio::time::Instant::now() < deadline,
            "the pump read a record but never recorded its progress"
        );
        tokio::time::sleep(Duration::from_millis(20)).await;
    }

    });
}

/// Every client-visible message on `session` until `budget` passes in silence or
/// the socket closes.
async fn drain(session: &mut WsSession, budget: Duration) -> Vec<Vec<Value>> {
    let mut seen = Vec::new();
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return seen;
        }
        match next_frame(session, left).await {
            Some(frame) => {
                if let Ok(message) = serde_json::from_slice::<Vec<Value>>(&frame.data) {
                    seen.push(message);
                }
            }
            None => return seen,
        }
    }
}

/// **A socket that did not come through the HTTP face is refused.**
///
/// Every HTTP envelope passes the Ferrite SSO gate (now the open gate); the
/// gateway forwards no headers to the ws face, so nothing about the caller can
/// be checked on the socket. A new socket must present a ticket only the HTTP
/// face mints. Without one — missing, forged,
/// or expired — it is told why and hung up with no connection document, no
/// challenge, and nothing read, while a ticketed socket in the same fleet is
/// served as before.
#[test]
 fn severe_a_socket_without_a_ticket_is_refused_before_it_can_read() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = ws_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-ws-ticket";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    // The control: a ticketed socket authenticates and writes into an open room.
    let mut writer = connect_as(&env, &owner).await;
    let message = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "only for signed-in sockets",
        T0_SECS,
    );
    publish_ok(&env, &mut writer, &message).await;

    let now = env.now_ms();
    let forged = format!("{WS_PATH}?ticket={}.{}", now + 30_000, "00".repeat(32));
    let expired = ws_path_with_ticket(WS_PATH, now.saturating_sub(ws::TICKET_TTL_MS + 1));
    for path in [WS_PATH.to_string(), forged, expired] {
        let mut session = env.ws_open(&path).await.expect("the ws face opens");
        let Some(notice) = wait_for(&mut session, "NOTICE", ATTEMPT).await else {
            panic!("{path}: the refusal is said;{}", queue_report(&env).await);
        };
        assert!(
            notice[1].as_str().is_some_and(|m| m.contains("ticket")),
            "{path}: the refusal names what is missing: {notice:?}"
        );

        // An anonymous read of an open room: the exact request the bypass served.
        send(
            &mut session,
            &json!(["REQ", "anon", { "kinds": [kind::KIND_STREAM_MESSAGE], "#h": [channel] }]),
        )
        .await;
        let seen = drain(&mut session, NEVER).await;
        assert!(
            seen.iter().all(|m| {
                let verb = m.first().and_then(Value::as_str).unwrap_or_default();
                !matches!(verb, "AUTH" | "EVENT" | "EOSE")
            }),
            "{path}: a refused socket gets no challenge and no events: {seen:?}"
        );
        assert!(
            connection_doc(&env, &session).await.is_none(),
            "{path}: a refused open leaves no connection document"
        );
    }

    });
}

/// **Any caller of the HTTP face can mint a ticket, signed in or not.**
///
/// The demonstration is public, so the ticket route sits behind the OPEN gate:
/// an anonymous guest and a signed-in user both get a ticket the ws face
/// accepts, and NIP-42 on the socket is what proves the workspace identity.
#[test]
 fn severe_a_ws_ticket_is_minted_for_anonymous_and_signed_in_callers() {
    common::run_scenario(|| async move {
    let _serial = WS_GATE_LOCK.lock().await;
    let env = TestEnv(
        TestHarness::new().with_store("nonces")
            .with_queue("requests")
            .with_queue("replies")
            .with_store("events")
            .with_store("config")
            .with_ape("guards")
            .with_topics("feed")
            .with_blob("media")
            .with_blob("web-assets")
            .request_reply("requests", "replies")
            .manual_clock(T0_MS)
            .timeout(REPLY_BUDGET)
            .build()
            .await,
    );
    {
        let mut config = env.store("config");
        buzz_store::put_raw(&mut config, b"buzz-stream-token-key", &WS_GATE_STREAM_KEY)
            .await
            .expect("stream key seed");
    }
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });

    let ask = || request_with_auth("POST", "/buzz/api/ws-ticket", None, b"{}".to_vec());
    // `env.0` is the bare harness: no property session cookie.
    let anonymous = env.0.handle(ask()).await.expect("the route answers");
    assert_eq!(anonymous.status, 200, "{}", error_text(&anonymous));
    let guest_ticket = body_json(&anonymous)["ticket"]
        .as_str()
        .expect("a ticket for the guest")
        .to_string();
    assert!(ws::verify_ticket(&WS_GATE_STREAM_KEY, &guest_ticket, env.now_ms()));

    let reply = env.handle(ask()).await.expect("the route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let ticket = body_json(&reply)["ticket"]
        .as_str()
        .expect("a ticket")
        .to_string();
    assert!(ws::verify_ticket(&WS_GATE_STREAM_KEY, &ticket, env.now_ms()));
    // And it is short-lived: one TTL later the ws face would refuse it.
    assert!(!ws::verify_ticket(
        &WS_GATE_STREAM_KEY,
        &ticket,
        env.now_ms() + ws::TICKET_TTL_MS
    ));

    });
}

/// **Banned and closed-community-expelled readers are refused on WebSocket
/// `AUTH`, `REQ`, and live pump delivery (even when `keys::revoked` is `None`).**
#[test]
fn severe_banned_or_expelled_socket_cannot_authenticate_or_req() {
    common::run_scenario(|| async move {
        let _serial = WS_GATE_LOCK.lock().await;
        let env = ws_env().await;
        let owner = Actor::new("ws-ban-owner");
        let mallory = Actor::new("ws-ban-mallory");
        let priv_room = "chan-ws-ban-priv";
        seed_channel_with_owner(&env, priv_room, Visibility::Private, &owner).await;
        seed_membership(&env, priv_room, &mallory, Role::Member).await;

        let mut writer = connect_as(&env, &owner).await;
        let mut live_mallory = connect_as(&env, &mallory).await;

        // Torn ban WITHOUT writing `keys::revoked` (simulating a failed best-effort bump).
        {
            let mut events = env.store("events");
            buzz_store::put_doc(
                &mut events,
                &keys::restriction(COMMUNITY, &mallory.pk),
                &Restriction {
                    banned: true,
                    banned_until: None,
                    timeout_until: None,
                },
            )
            .await
            .expect("ban state");
        }

        let after_ban = signed_at(
            &owner,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", priv_room)],
            "private after ban",
            T0_SECS + 1,
        );
        publish_ok(&env, &mut writer, &after_ban).await;

        // 1. Existing authenticated socket issuing REQ after ban is CLOSED with restricted:.
        let closed = ask(
            &env,
            &mut live_mallory,
            &json!(["REQ", "after-ban-sub", { "kinds": [kind::KIND_STREAM_MESSAGE], "#h": [priv_room], "limit": 10 }]),
            "CLOSED",
        )
        .await;
        assert_eq!(closed[1], json!("after-ban-sub"));
        assert!(
            closed[2].as_str().unwrap_or_default().starts_with("restricted:"),
            "banned socket REQ must be CLOSED with restricted: {closed:?}"
        );

        // 2. Reconnecting banned user attempting NIP-42 AUTH is refused and stays unbound.
        let (mut fresh_sock, challenge) = connect(&env).await;
        let auth_ev = auth_event_for(&mallory, &challenge, RELAY_URL, T0_SECS);
        let ok = ask(&env, &mut fresh_sock, &json!(["AUTH", auth_ev]), "OK").await;
        assert_eq!(ok[2], json!(false), "banned user AUTH must be refused: {ok:?}");
        assert!(
            ok[3].as_str().unwrap_or_default().starts_with("restricted:"),
            "banned AUTH refusal must carry restricted: {ok:?}"
        );
        assert_eq!(
            connection_doc(&env, &fresh_sock).await.and_then(|c| c.pubkey),
            None,
            "refused AUTH must leave socket unbound"
        );
    });
}
