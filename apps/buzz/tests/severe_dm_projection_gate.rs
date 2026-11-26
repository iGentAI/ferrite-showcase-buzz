//! **DM projections (41010/41011 ⇒ 39000/39001/39002), decided on the client
//! SOCKET.**
//!
//! A DM room's id is the participant hash — there is no `h` tag on the open
//! command, so the face deliberately does not resolve its channel and
//! `effects::projection_channel` derives it from the event itself, AFTER the
//! effect has created the room. The room's metadata carries `t=dm` and
//! `hidden` (how a client tells a conversation from a room), and its cells are
//! as private as the conversation: a participant reads them, a non-participant
//! must not — stored AND live.
//!
//! Every case rides the WebSocket face, because that is the only face that
//! derives each event's channel from the event itself, and it is the face
//! buzz's real client builds its DM list from (`REQ {kinds:[39000,39002]}`).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::collections::BTreeSet;
use std::time::Duration;

use buzz::relay::crypto::to_hex;
use buzz::relay::effects::ReplaceableHead;
use buzz::relay::event::Event;
use buzz::relay::{keys, kind, store as buzz_store};
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, T0_MS, T0_SECS, gate_env, signed, signed_at, stored_event, tag,
    ws_path_with_ticket,
};
use ferrite_app::serde_json::{self, json, Value};
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{TestHarness, WsSession};

/// One ws fleet at a time in this binary.
static DM_WS_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

const WS_PATH: &str = "/buzz/ws/";
const RELAY_URL: &str = "wss://buzz.test/buzz/ws/";

/// The config key the relay's signing identity is seeded into.
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

/// One attempt's answer budget, the re-send count while the relay is silent
/// (the frozen-clock lease artifact `severe_ws_gate.rs` documents), and how
/// long a frame that must NOT arrive is watched for.
const ATTEMPT: Duration = Duration::from_secs(25);
const ATTEMPTS: usize = 3;
const NEVER: Duration = Duration::from_secs(6);

fn relay() -> Actor {
    Actor::new("the-relay-itself")
}

/// The severe_ws_gate fleet plus the relay identity, all seeded BEFORE the app
/// serves a request.
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
        buzz_store::put_raw(
            &mut config,
            RELAY_IDENTITY_CONFIG,
            to_hex(&relay().sk).as_bytes(),
        )
        .await
        .expect("relay identity seed");
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

/// Send `message` and return the relay's first `verb` answer, re-sending while
/// the relay stays silent (every verb driven here is idempotent by design).
async fn ask(session: &mut WsSession, message: &Value, verb: &str) -> Vec<Value> {
    for _ in 0..ATTEMPTS {
        send(session, message).await;
        if let Some(answer) = wait_for(session, verb, ATTEMPT).await {
            return answer;
        }
    }
    panic!("no {verb} answer to {message} after {ATTEMPTS} attempts");
}

/// Open the real socket, answer its NIP-42 challenge as `actor`.
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

/// Publish `event` over the socket and require acceptance.
async fn publish_ok(session: &mut WsSession, event: &Event) {
    let ok = ask(session, &json!(["EVENT", event]), "OK").await;
    assert_eq!(ok[1], json!(to_hex(&event.id)), "the OK names the event");
    assert_eq!(ok[2], json!(true), "the event must be accepted: {ok:?}");
}

/// The event ids one REQ serves before its EOSE; a CLOSED is a finding.
async fn req_page(session: &mut WsSession, request: &Value, sub_id: &str) -> BTreeSet<String> {
    for _ in 0..ATTEMPTS {
        send(session, request).await;
        let deadline = tokio::time::Instant::now() + ATTEMPT;
        let mut ids = BTreeSet::new();
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
    panic!("no EOSE for {sub_id} after {ATTEMPTS} attempts");
}

/// The REQ buzz's client builds its room list from.
fn cells_req(sub_id: &str) -> Value {
    json!(["REQ", sub_id, { "kinds": [
        kind::KIND_NIP29_GROUP_METADATA,
        kind::KIND_NIP29_GROUP_ADMINS,
        kind::KIND_NIP29_GROUP_MEMBERS
    ], "limit": 50 }])
}

/// Read live frames on `sub_id` until every id in `want` arrived (true) or the
/// budget passed (false).
async fn receives_all(
    session: &mut WsSession,
    sub_id: &str,
    want: &[String],
    budget: Duration,
) -> bool {
    let mut missing: BTreeSet<&String> = want.iter().collect();
    let deadline = tokio::time::Instant::now() + budget;
    while !missing.is_empty() {
        let left = deadline.saturating_duration_since(tokio::time::Instant::now());
        if left.is_zero() {
            return false;
        }
        let Some(message) = next_message(session, left).await else {
            continue;
        };
        if message.first() == Some(&json!("EVENT")) && message[1] == json!(sub_id) {
            let id = message[2]["id"].as_str().unwrap_or_default().to_string();
            missing.remove(&id);
        }
    }
    true
}

// -------------------------------------------------------------- durable state

async fn projection_head(
    env: &TestEnv,
    kind_: u32,
    relay: &Actor,
    channel: &str,
) -> Option<ReplaceableHead> {
    let mut guards = env.ape("guards");
    let raw = guards
        .get(&keys::dtag_head(COMMUNITY, kind_, &relay.pk, channel))
        .await
        .expect("ape read")?;
    Some(serde_json::from_slice(&raw).expect("head decodes"))
}

/// The published cell, insisted on: the whole gate exists because a missing
/// projection reads to a client as "this conversation does not exist".
async fn expect_projection(env: &TestEnv, kind_: u32, relay: &Actor, channel: &str) -> Event {
    let deadline = tokio::time::Instant::now() + REPLY_BUDGET;
    loop {
        if let Some(head) = projection_head(env, kind_, relay, channel).await {
            let id = buzz::relay::crypto::from_hex32(&head.id).expect("a 32-byte head id");
            if let Some(event) = stored_event(env, &id).await {
                return event;
            }
        }
        assert!(
            tokio::time::Instant::now() < deadline,
            "no kind:{kind_} projection for DM room {channel}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

fn has_marker(event: &Event, name: &str) -> bool {
    event
        .tags
        .iter()
        .any(|t| t.as_slice() == [name.to_string()])
}

/// The member pubkeys a 39002 carries (`["p", pk, relay_url, role]`).
fn member_pubkeys(event: &Event) -> BTreeSet<String> {
    event
        .tags
        .iter()
        .filter(|t| t.first().map(String::as_str) == Some("p"))
        .filter_map(|t| t.get(1).cloned())
        .collect()
}

// ------------------------------------------------------------------ the gates

/// **Opening a DM publishes all three cells for the participant-hash room, and
/// only its participants can read them.** The metadata says what the client
/// needs to file it as a conversation (`t=dm`, `hidden`, `private`), the
/// roster is exactly the participant set with no owner, and the cells reach a
/// participant's open subscription live while a non-participant's stays
/// silent — the stored page agrees on both.
#[test]
 fn severe_a_dm_open_projects_a_hidden_room_its_participants_alone_can_read() {
    common::run_scenario(|| async move {
    let _serial = DM_WS_LOCK.lock().await;
    let env = ws_env().await;
    let relay = relay();
    let alice = Actor::new("alice");
    let bob = Actor::new("bob");
    let mallory = Actor::new("mallory");
    let dm = buzz::relay::channel::dm_participant_hash(&[alice.pk, bob.pk]);

    // Both readers subscribe BEFORE the DM exists, so the live path is what
    // must deliver (or withhold) the cells.
    let mut bob_sock = connect_as(&env, &bob).await;
    let bob_sub = "dm-cells-bob";
    assert!(
        req_page(&mut bob_sock, &cells_req(bob_sub), bob_sub)
            .await
            .is_empty(),
        "no room exists yet"
    );
    let mut mallory_sock = connect_as(&env, &mallory).await;
    let mallory_sub = "dm-cells-mallory";
    assert!(
        req_page(&mut mallory_sock, &cells_req(mallory_sub), mallory_sub)
            .await
            .is_empty()
    );

    // Alice opens the DM over the socket — the 41010 carries no `h` tag; the
    // room's id must come from the participant hash the event itself implies.
    let mut alice_sock = connect_as(&env, &alice).await;
    let open = signed(
        &alice,
        kind::KIND_DM_OPEN,
        vec![tag("p", &bob.pk_hex())],
        "",
    );
    publish_ok(&mut alice_sock, &open).await;

    // All three cells exist for the hash-id room, relay-signed and valid.
    let meta = expect_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, &dm).await;
    let admins = expect_projection(&env, kind::KIND_NIP29_GROUP_ADMINS, &relay, &dm).await;
    let members = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, &dm).await;
    for cell in [&meta, &admins, &members] {
        assert_eq!(cell.pubkey, relay.pk, "the relay signs its projections");
        assert_eq!(cell.verify(), Ok(()));
        assert_eq!(
            cell.first_tag("d"),
            Some(dm.as_str()),
            "the cell's id IS the participant hash: {:?}",
            cell.tags
        );
        assert_eq!(
            cell.first_tag("h"),
            Some(dm.as_str()),
            "gated by its own room"
        );
    }

    // The metadata a client files the conversation by.
    assert_eq!(meta.first_tag("t"), Some("dm"), "{:?}", meta.tags);
    assert!(
        has_marker(&meta, "hidden"),
        "a DM stays out of public group lists: {:?}",
        meta.tags
    );
    assert!(has_marker(&meta, "private"), "{:?}", meta.tags);
    assert!(
        has_marker(&meta, "closed"),
        "a DM is never self-joinable: {:?}",
        meta.tags
    );
    assert_eq!(meta.first_tag("name"), Some("DM (2)"), "{:?}", meta.tags);

    // The roster is exactly the participants, and NOBODY owns a DM.
    assert_eq!(
        member_pubkeys(&members),
        BTreeSet::from([alice.pk_hex(), bob.pk_hex()]),
        "{:?}",
        members.tags
    );
    assert!(
        member_pubkeys(&admins).is_empty(),
        "a DM has no owner, so its 39001 names no one: {:?}",
        admins.tags
    );

    // LIVE: the participant's open subscription receives the cells…
    let want: Vec<String> = [&meta, &admins, &members]
        .iter()
        .map(|e| to_hex(&e.id))
        .collect();
    assert!(
        receives_all(&mut bob_sock, bob_sub, &want, REPLY_BUDGET).await,
        "a participant must receive the DM's cells on their open subscription"
    );

    // …and the STORED page agrees for both readers.
    let bob_page = req_page(&mut bob_sock, &cells_req("dm-bob-2"), "dm-bob-2").await;
    for id in &want {
        assert!(
            bob_page.contains(id),
            "a participant's REQ must serve {id}: {bob_page:?}"
        );
    }
    let mallory_page = req_page(
        &mut mallory_sock,
        &cells_req("dm-mallory-2"),
        "dm-mallory-2",
    )
    .await;
    for id in &want {
        assert!(
            !mallory_page.contains(id),
            "a NON-participant read a DM's cell {id}: {mallory_page:?}"
        );
    }

    // LAST (it waits on silence): the non-participant's live subscription
    // received nothing either.
    assert!(
        !receives_all(&mut mallory_sock, mallory_sub, &want, NEVER).await,
        "a non-participant RECEIVED a DM's cells live"
    );

    });
}

/// **41011 makes a NEW room, projected for its own participants.** The
/// participant set is immutable, so adding someone derives a different hash —
/// a different room with its own three cells — and the newcomer can read the
/// new room's cells while remaining unable to read the original
/// conversation's.
#[test]
 fn severe_adding_a_dm_member_projects_a_new_room_without_leaking_the_old_one() {
    common::run_scenario(|| async move {
    let _serial = DM_WS_LOCK.lock().await;
    let env = ws_env().await;
    let relay = relay();
    let alice = Actor::new("alice");
    let bob = Actor::new("bob");
    let carol = Actor::new("carol");
    let two = buzz::relay::channel::dm_participant_hash(&[alice.pk, bob.pk]);
    let three = buzz::relay::channel::dm_participant_hash(&[alice.pk, bob.pk, carol.pk]);
    assert_ne!(
        two, three,
        "a different participant set IS a different room"
    );

    let mut alice_sock = connect_as(&env, &alice).await;
    let open = signed(
        &alice,
        kind::KIND_DM_OPEN,
        vec![tag("p", &bob.pk_hex())],
        "",
    );
    publish_ok(&mut alice_sock, &open).await;
    let two_meta = expect_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, &two).await;
    let two_members = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, &two).await;

    // "Add carol": a 41011 naming the full new set.
    let add = signed_at(
        &alice,
        kind::KIND_DM_ADD_MEMBER,
        vec![tag("p", &bob.pk_hex()), tag("p", &carol.pk_hex())],
        "",
        T0_SECS + 1,
    );
    publish_ok(&mut alice_sock, &add).await;

    // The NEW room has all three cells, keyed by the new hash.
    let meta = expect_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, &three).await;
    let admins = expect_projection(&env, kind::KIND_NIP29_GROUP_ADMINS, &relay, &three).await;
    let members = expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, &three).await;
    assert_eq!(meta.first_tag("t"), Some("dm"));
    assert!(has_marker(&meta, "hidden"));
    assert_eq!(meta.first_tag("name"), Some("DM (3)"), "{:?}", meta.tags);
    assert_eq!(
        member_pubkeys(&members),
        BTreeSet::from([alice.pk_hex(), bob.pk_hex(), carol.pk_hex()]),
        "{:?}",
        members.tags
    );

    // The ORIGINAL room is untouched: same winning metadata, same roster.
    assert_eq!(
        expect_projection(&env, kind::KIND_NIP29_GROUP_METADATA, &relay, &two)
            .await
            .id,
        two_meta.id,
        "the two-party room must not be republished by the add"
    );
    assert_eq!(
        member_pubkeys(
            &expect_projection(&env, kind::KIND_NIP29_GROUP_MEMBERS, &relay, &two).await
        ),
        BTreeSet::from([alice.pk_hex(), bob.pk_hex()]),
        "carol must never join the two-party roster"
    );

    // Carol reads the new room's cells — and none of the old room's.
    let mut carol_sock = connect_as(&env, &carol).await;
    let page = req_page(&mut carol_sock, &cells_req("dm-carol"), "dm-carol").await;
    for cell in [&meta, &admins, &members] {
        assert!(
            page.contains(&to_hex(&cell.id)),
            "the newcomer must see the new room's kind:{}: {page:?}",
            cell.kind
        );
    }
    for cell in [&two_meta, &two_members] {
        assert!(
            !page.contains(&to_hex(&cell.id)),
            "the newcomer READ the original conversation's kind:{}: {page:?}",
            cell.kind
        );
    }

    });
}
