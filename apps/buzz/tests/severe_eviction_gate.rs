//! **M1.14 / §6.3 rule (b) — the per-batch revocation tombstone, decided by
//! the WIRING.**
//!
//! The feed-driven invalidation stops a removed member's delivery when the
//! revocation's feed record is READ. Rule (b) covers the window that
//! mechanism cannot: a saga that commits its state write and crashes before
//! its feed append. These gates construct exactly that torn state (membership
//! removed / ban recorded + tombstone bumped, NO feed record), then prove the
//! live pump refuses on its very next batch — under a FROZEN clock, so the
//! 10s cache TTL can never quietly save the old code, which makes each case
//! fail without the tombstone check.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;

use buzz::relay::channel::{Membership, Role, Visibility};
use buzz::relay::effects::Restriction;
use buzz::relay::{keys, kind, store as buzz_store};
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, T0_MS, T0_SECS, body_json, gate_env, membership, post,
    seed_channel_with_owner, seed_membership, signed_at, submit, tag,
};
use ferrite_app::serde_json::{self, json, Value};
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{TestHarness, WsSession};

/// One fleet at a time in this binary (the common permit is private to it).
static EVICTION_GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The common env plus the gateway frame queue the SSE pump publishes to.
async fn pump_env() -> TestEnv {
    let env = TestEnv(TestHarness::new().with_store("nonces")
        .with_queue("requests")
        .with_queue("replies")
        .with_queue("stream-requests")
        .with_queue("frames")
        .with_queue("work")
        .with_store("events")
        .with_store("config")
        .with_ape("guards")
        .with_topics("feed")
        .with_blob("media")
        .with_blob("web-assets")
        .request_reply("requests", "replies")
        .stream_requests("stream-requests")
        .frames("frames")
        .manual_clock(T0_MS)
        .timeout(REPLY_BUDGET)
        .build()
        .await);
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
    for attempt in 0..80 {
        match env
            .handle(common::get("/buzz/api/healthz"))
            .await
        {
            Ok(reply) if reply.status == 200 => break,
            _ if attempt == 79 => panic!("the app did not become healthy"),
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    let mut config = env.store("config");
    buzz_store::put_raw(&mut config, b"buzz-stream-token-key", &[0x5au8; 32])
        .await
        .expect("stream key seed");
    env
}

/// Mint a session over the real subscriptions route and open the real SSE
/// face for it.
async fn open_stream(env: &TestEnv, actor: &Actor) -> WsSession {
    let reply = env
        .handle(post(
            actor,
            "/buzz/api/subscriptions",
            json!([{ "kinds": [
                kind::KIND_STREAM_MESSAGE,
                kind::KIND_NIP29_REMOVE_USER,
                kind::KIND_MODERATION_BAN
            ] }])
            .to_string()
            .into_bytes(),
        ))
        .await
        .expect("subscribe replies");
    assert_eq!(
        reply.status,
        200,
        "subscription mints: {}",
        String::from_utf8_lossy(&reply.body)
    );
    let body = body_json(&reply);
    let session = body["session"].as_str().expect("a session id");
    let token = body["token"].as_str().expect("a stream token");
    env.sse_open(&format!("/buzz/sub/{session}?token={token}"))
        .await
        .expect("the SSE face opens")
}

/// The next `event` frame's event id within `budget`, skipping heartbeats;
/// `None` when the budget passes without one.
async fn next_event_frame(stream: &mut WsSession, budget: Duration) -> Option<(String, bool)> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = match tokio::time::timeout(remaining, stream.recv()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => return None,
            Err(_) => return None,
        };
        let decoded: Value = match serde_json::from_slice(&frame.data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        match decoded["type"].as_str() {
            Some("event") => {
                let id = decoded["event"]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string();
                return Some((id, false));
            }
            Some("reset") => {
                return Some((
                    decoded["reason"].as_str().unwrap_or_default().to_string(),
                    true,
                ))
            }
            _ => continue, // heartbeat
        }
    }
}

/// A removed member whose removal's FEED RECORD never landed (the crash
/// window) stops receiving the private channel on the very next batch — the
/// tombstone, not the feed record and not the (frozen, never-expiring) TTL,
/// is what enforces it.
#[test]
 fn severe_a_torn_removal_still_stops_private_delivery() {
    common::run_scenario(|| async move {
    let _serial = EVICTION_GATE_LOCK.lock().await;
    let env = pump_env().await;
    let owner = Actor::new("owner");
    let bob = Actor::new("bob");
    let channel = "chan-evict";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
    seed_membership(&env, channel, &bob, Role::Member).await;

    let mut stream = open_stream(&env, &bob).await;

    // Prove the pump serves bob the private channel (and warm its cache —
    // under the frozen clock this cache entry can never TTL out, which is
    // exactly what gives this gate teeth against the pre-tombstone code).
    let msg1 = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "before the removal",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &msg1).await.status, 200);
    let first = next_event_frame(&mut stream, Duration::from_secs(15)).await;
    assert_eq!(
        first,
        Some((buzz::relay::crypto::to_hex(&msg1.id), false)),
        "the member receives the private channel before the removal"
    );

    // THE CRASH WINDOW, constructed exactly: the removal's state write and
    // tombstone bump are durable, its feed append never happened.
    {
        let mut events = env.store("events");
        let key = keys::mem(COMMUNITY, channel, &bob.pk).expect("safe id");
        let mut doc: Membership = buzz_store::get_doc(&mut events, &key)
            .await
            .expect("read")
            .expect("bob's row");
        doc.removed_at = Some(T0_MS);
        buzz_store::put_doc(&mut events, &key, &doc)
            .await
            .expect("soft remove");
        buzz_store::put_raw(
            &mut events,
            &keys::revoked(COMMUNITY, &bob.pk),
            T0_MS.to_string().as_bytes(),
        )
        .await
        .expect("tombstone bump");
    }

    // The next private message DOES ride the feed — and must NOT reach bob.
    let msg2 = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "after the torn removal",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &msg2).await.status, 200);
    let leaked = next_event_frame(&mut stream, Duration::from_secs(6)).await;
    assert_eq!(
        leaked, None,
        "a torn removal must stop private delivery on the next batch"
    );

    });
}

/// A ban whose feed record never landed ENDS the session on the next batch
/// (buzz evicts a banned member's live subs): the pump sends a `revoked`
/// reset and stops, rather than keeping a stream open for an expelled member.
#[test]
 fn severe_a_torn_ban_evicts_the_live_session() {
    common::run_scenario(|| async move {
    let _serial = EVICTION_GATE_LOCK.lock().await;
    let env = pump_env().await;
    let owner = Actor::new("owner");
    let carol = Actor::new("carol");
    let channel = "chan-ban";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    seed_membership(&env, channel, &carol, Role::Member).await;

    let mut stream = open_stream(&env, &carol).await;
    let msg1 = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "before the ban",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &msg1).await.status, 200);
    assert_eq!(
        next_event_frame(&mut stream, Duration::from_secs(15)).await,
        Some((buzz::relay::crypto::to_hex(&msg1.id), false)),
        "carol is live before the ban"
    );

    // The torn ban: restriction + tombstone durable, no feed record.
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

    // The next feed traffic wakes the pump; carol's session must END with a
    // `revoked` reset instead of delivering.
    let msg2 = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "after the torn ban",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &msg2).await.status, 200);
    assert_eq!(
        next_event_frame(&mut stream, Duration::from_secs(15)).await,
        Some(("revoked".to_string(), true)),
        "a banned member's live session ends with a revoked reset"
    );

    });
}

/// The whole-path case: the REAL 9001 removal through the real route (no torn
/// state) also stops delivery — the belt (feed-driven invalidation) and the
/// braces (tombstone) agree.
#[test]
 fn severe_a_real_removal_through_the_route_stops_delivery() {
    common::run_scenario(|| async move {
    let _serial = EVICTION_GATE_LOCK.lock().await;
    let env = pump_env().await;
    let owner = Actor::new("owner");
    let dave = Actor::new("dave");
    let channel = "chan-remove";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
    seed_membership(&env, channel, &dave, Role::Member).await;

    let mut stream = open_stream(&env, &dave).await;
    let msg1 = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "hello dave",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &msg1).await.status, 200);
    assert_eq!(
        next_event_frame(&mut stream, Duration::from_secs(15)).await,
        Some((buzz::relay::crypto::to_hex(&msg1.id), false)),
    );

    let removal = signed_at(
        &owner,
        kind::KIND_NIP29_REMOVE_USER,
        vec![tag("h", channel), tag("p", &dave.pk_hex())],
        "",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &removal).await.status, 200);

    let msg2 = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "dave is gone",
        T0_SECS + 2,
    );
    assert_eq!(submit(&env, &owner, &msg2).await.status, 200);

    // dave may still see the removal record itself (membership notices are
    // feed records); what he must NOT see is msg2.
    let mut saw_msg2 = false;
    while let Some((id, is_reset)) = next_event_frame(&mut stream, Duration::from_secs(6)).await {
        if is_reset {
            break;
        }
        if id == buzz::relay::crypto::to_hex(&msg2.id) {
            saw_msg2 = true;
            break;
        }
    }
    assert!(
        !saw_msg2,
        "a removed member must not receive later private traffic"
    );

    });
}

/// **A feed record the pump cannot decode is consumed, not re-read forever.**
/// The cursor must advance past a malformed record even when nothing decodable
/// follows it; otherwise every tick re-reads the same page.
#[test]
 fn severe_a_malformed_trailing_feed_record_advances_the_pump_cursor() {
    common::run_scenario(|| async move {
    let _serial = EVICTION_GATE_LOCK.lock().await;
    let env = pump_env().await;
    let owner = Actor::new("owner");
    let erin = Actor::new("erin");
    let channel = "chan-malformed";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    seed_membership(&env, channel, &erin, Role::Member).await;

    let reply = env
        .handle(post(
            &erin,
            "/buzz/api/subscriptions",
            json!([{ "kinds": [kind::KIND_STREAM_MESSAGE] }]).to_string().into_bytes(),
        ))
        .await
        .expect("subscribe replies");
    assert_eq!(reply.status, 200, "{}", String::from_utf8_lossy(&reply.body));
    let body = body_json(&reply);
    let session_id = body["session"].as_str().expect("a session id").to_string();
    let token = body["token"].as_str().expect("a stream token");
    let mut stream = env
        .sse_open(&format!("/buzz/sub/{session_id}?token={token}"))
        .await
        .expect("the SSE face opens");

    let msg = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "a decodable record",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &msg).await.status, 200);
    assert_eq!(
        next_event_frame(&mut stream, Duration::from_secs(15)).await,
        Some((buzz::relay::crypto::to_hex(&msg.id), false)),
    );

    let mut topics = env.topics("feed");
    let bad = topics
        .append(
            &keys::feed_topic(COMMUNITY, common::FEED_GENERATION),
            b"bad",
            b"this is not a feed record",
        )
        .await
        .expect("the malformed record is appended");

    let mut config = env.store("config");
    let session: buzz::relay::session::Session =
        buzz_store::get_doc(&mut config, &keys::sess(COMMUNITY, &session_id))
            .await
            .expect("session read")
            .expect("the session row");
    let cursor_key = keys::sess_cursor(COMMUNITY, &session_id, session.generation);
    let mut offset = 0;
    for _ in 0..100 {
        let stored: Option<buzz::relay::session::Cursor> =
            buzz_store::get_doc(&mut config, &cursor_key).await.expect("cursor read");
        offset = stored.map_or(0, |c| c.offset);
        if offset > bad {
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        offset,
        bad + 1,
        "the pump must consume a malformed trailing record instead of re-reading it"
    );

    });
}

fn committed_post(actor: &Actor, path: &str, body: Vec<u8>) -> ferrite_app::IngressRequest {
    let event = buzz::relay::event::Event::sign_new(
        &actor.sk,
        kind::KIND_HTTP_AUTH,
        vec![
            vec!["u".into(), format!("https://buzz.test{path}")],
            vec!["method".into(), "POST".into()],
            vec![
                "payload".into(),
                buzz::relay::crypto::to_hex(&buzz::relay::crypto::sha256::sha256(&body)),
            ],
            vec!["nonce".into(), format!("evict-{}", body.len())],
        ],
        "",
        T0_SECS,
    )
    .expect("signs");
    common::request_with_auth("POST", path, Some(&common::auth_header(&event)), body)
}

/// **Banned and expelled community members lose read access across HTTP reads
/// and live streams, even when `keys::revoked` was never written.**
#[test]
fn severe_banned_and_expelled_members_lose_read_and_stream_access() {
    common::run_scenario(|| async move {
        let _serial = EVICTION_GATE_LOCK.lock().await;
        let env = pump_env().await;
        let owner = Actor::new("ban-read-owner");
        let moderator = Actor::new("ban-read-mod");
        let mallory = Actor::new("ban-read-mallory");
        let eve = Actor::new("expelled-read-eve");
        let priv_room = "chan-ban-priv";

        {
            let mut config = env.store("config");
            let mods = json!([moderator.pk_hex()]).to_string();
            buzz_store::put_raw(&mut config, b"buzz-community-moderators", mods.as_bytes())
                .await
                .expect("seed moderators");
        }
        seed_channel_with_owner(&env, priv_room, Visibility::Private, &owner).await;
        seed_membership(&env, priv_room, &mallory, Role::Member).await;
        seed_membership(&env, priv_room, &eve, Role::Member).await;

        let msg_before = signed_at(
            &owner,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", priv_room)],
            "before ban",
            T0_SECS,
        );
        assert_eq!(submit(&env, &owner, &msg_before).await.status, 200);
        let pre_q = env
            .handle(post(
                &mallory,
                "/buzz/api/query",
                json!({ "channel": priv_room }).to_string().into_bytes(),
            ))
            .await
            .expect("query");
        assert_eq!(pre_q.status, 200);

        // Open mallory's stream BEFORE a torn ban where `keys::revoked` is absent
        // (simulating a failed best-effort `bump_revocation`).
        let mut mallory_stream = open_stream(&env, &mallory).await;
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
            .expect("seed ban without tombstone");
        }

        let msg_after_ban = signed_at(
            &owner,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", priv_room)],
            "secret after ban",
            T0_SECS + 1,
        );
        assert_eq!(submit(&env, &owner, &msg_after_ban).await.status, 200);

        // 1. Torn ban with `keys::revoked == None` must still evict the live SSE pump.
        assert_eq!(
            next_event_frame(&mut mallory_stream, Duration::from_secs(15)).await,
            Some(("revoked".to_string(), true)),
            "banned subscriber must be evicted even when keys::revoked is None"
        );

        // 2. Banned member must be refused across HTTP read and subscription routes.
        for (path, payload) in [
            ("/buzz/api/query", json!({ "channel": priv_room })),
            ("/buzz/api/window", json!({ "channel": priv_room })),
            ("/buzz/api/feed", json!({ "category": "activity" })),
            ("/buzz/api/subscriptions", json!([{ "kinds": [kind::KIND_STREAM_MESSAGE] }])),
            ("/query", json!([{ "kinds": [kind::KIND_STREAM_MESSAGE], "#h": [priv_room] }])),
        ] {
            let reply = env
                .handle(post(&mallory, path, payload.to_string().into_bytes()))
                .await
                .expect("route answers");
            assert_eq!(
                reply.status,
                403,
                "banned member was not refused on {path}: {} {}",
                reply.status,
                String::from_utf8_lossy(&reply.body)
            );
        }

        // 3. Closed community: removing eve via `/buzz/api/members/remove` must
        //    cut off eve's live SSE stream AND block private-room HTTP reads even
        //    though eve's per-channel `mem/` row still exists.
        {
            let mut config = env.store("config");
            buzz_store::put_raw(&mut config, b"buzz-community-closed", b"true")
                .await
                .expect("close community");
            let mut events = env.store("events");
            for actor in [&owner, &moderator, &eve] {
                buzz_store::put_raw(&mut events, &keys::community_mem(COMMUNITY, &actor.pk), b"{}")
                    .await
                    .expect("seed cmem");
            }
        }
        let mut eve_stream = open_stream(&env, &eve).await;
        let msg_pre_expel = signed_at(
            &owner,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", priv_room)],
            "while eve is in community",
            T0_SECS + 2,
        );
        assert_eq!(submit(&env, &owner, &msg_pre_expel).await.status, 200);
        assert_eq!(
            next_event_frame(&mut eve_stream, Duration::from_secs(15)).await,
            Some((buzz::relay::crypto::to_hex(&msg_pre_expel.id), false)),
        );

        let rm_body = serde_json::to_vec(&json!({ "pubkey": eve.pk_hex() })).expect("encodes");
        let rm_reply = env
            .handle(committed_post(&moderator, "/buzz/api/members/remove", rm_body))
            .await
            .expect("remove answers");
        assert_eq!(rm_reply.status, 200);

        let msg_post_expel = signed_at(
            &owner,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", priv_room)],
            "after eve was expelled from community",
            T0_SECS + 3,
        );
        assert_eq!(submit(&env, &owner, &msg_post_expel).await.status, 200);
        assert_eq!(
            next_event_frame(&mut eve_stream, Duration::from_secs(15)).await,
            Some(("revoked".to_string(), true)),
            "expelled closed-community member's live stream must be revoked"
        );

        let eve_q = env
            .handle(post(
                &eve,
                "/buzz/api/query",
                json!({ "channel": priv_room }).to_string().into_bytes(),
            ))
            .await
            .expect("query");
        assert_eq!(eve_q.status, 403, "expelled member must be refused on /buzz/api/query");

        let eve_win = env
            .handle(post(
                &eve,
                "/buzz/api/window",
                json!({ "channel": priv_room }).to_string().into_bytes(),
            ))
            .await
            .expect("window");
        assert_eq!(eve_win.status, 403, "expelled member must be refused on /buzz/api/window");
    });
}
