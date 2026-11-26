//! **A ban that has already lapsed must not cost a member their live session.**
//!
//! `Restriction::banned_until` is new (it is what made "ban for 3 days" mean
//! three days instead of forever). The two live pumps that evict a banned
//! member's session were not updated with it: both read the raw `banned` flag,
//! which the stored document keeps set until some LATER moderation command
//! normalizes it away — so a member whose one-hour ban ended yesterday was still
//! "banned" to the pump.
//!
//! That is reachable without any moderator doing anything, because the tombstone
//! the pumps watch is bumped by an ordinary channel removal as well as by a
//! moderation command (`effects::bump_revocation`, called from both
//! `apply_moderation` and the membership-removal path). So: temp-ban a member,
//! let it lapse, remove them from any room — and their SSE session is ended with
//! a `revoked` reset (the WS face sends `restricted: your access to this
//! community was revoked` and closes the socket), while `ingest_event`'s
//! time-aware `blocks_write` was happily accepting their messages. The relay
//! contradicted itself about the same member in the same instant.
//!
//! **Why this gate can fail.** It constructs the torn state directly — a lapsed
//! temporary ban plus a bumped tombstone, no feed record — under the gates' FROZEN
//! clock, so `banned_until` is unambiguously in the past and the access cache's
//! 10 s TTL can never quietly rescue the old code. Against the raw-flag read this
//! asserts `event` and receives `reset(revoked)`.
//!
//! The pump harness here (`pump_env`, `open_stream`, `next_event_frame`) is the
//! one from `severe_eviction_gate.rs`, trimmed to what this case needs: those
//! helpers live in that gate rather than in `tests/common`, and this gate takes
//! its own copy rather than editing a file another author owns.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::effects::Restriction;
use buzz::relay::{keys, kind, store as buzz_store};
use common::{
    Actor, COMMUNITY, REPLY_BUDGET, T0_MS, T0_SECS, body_json, post, seed_channel_with_owner,
    seed_membership, signed_at, submit, tag,
};
use ferrite_app::serde_json::{self, json, Value};
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{TestHarness, WsSession};

/// One fleet at a time in this binary: this file builds its own env (it needs the
/// gateway frame queue), so it cannot use `common`'s private fleet permit.
static LAPSED_BAN_GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The common fleet plus the gateway frame queue the SSE pump publishes to.
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

/// Mint a session over the real subscriptions route and open the real SSE face.
async fn open_stream(env: &TestEnv, actor: &Actor) -> WsSession {
    let reply = env
        .handle(post(
            actor,
            "/buzz/api/subscriptions",
            json!([{ "kinds": [kind::KIND_STREAM_MESSAGE] }])
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

/// The next `event` frame's id within `budget`, skipping heartbeats. A `reset`
/// returns `(reason, true)` so a gate can tell "delivered" from "evicted"
/// instead of just timing out.
async fn next_event_frame(stream: &mut WsSession, budget: Duration) -> Option<(String, bool)> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            return None;
        }
        let frame = match tokio::time::timeout(remaining, stream.recv()).await {
            Ok(Some(frame)) => frame,
            _ => return None,
        };
        let Ok(decoded) = serde_json::from_slice::<Value>(&frame.data) else {
            continue;
        };
        match decoded["type"].as_str() {
            Some("event") => {
                return Some((
                    decoded["event"]["id"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    false,
                ))
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

/// Seed `who`'s restriction document verbatim.
async fn seed_restriction(env: &TestEnv, who: &Actor, doc: &Restriction) {
    let mut events = env.store("events");
    buzz_store::put_doc(&mut events, &keys::restriction(COMMUNITY, &who.pk), doc)
        .await
        .expect("restriction seed");
}

/// Bump `who`'s revocation tombstone, which is what wakes the pump's check —
/// exactly what an ordinary channel removal does.
async fn bump_tombstone(env: &TestEnv, who: &Actor) {
    let mut events = env.store("events");
    buzz_store::put_raw(
        &mut events,
        &keys::revoked(COMMUNITY, &who.pk),
        T0_MS.to_string().as_bytes(),
    )
    .await
    .expect("tombstone bump");
}

/// A LAPSED temporary ban plus a bumped tombstone: delivery continues.
///
/// Bracketed by a delivery before and a delivery after, so "no reset arrived" is
/// not satisfied by a pump that simply stopped working.
#[test]
 fn severe_a_lapsed_temporary_ban_does_not_evict_a_live_session() {
    common::run_scenario(|| async move {
    let _serial = LAPSED_BAN_GATE_LOCK.lock().await;
    let env = pump_env().await;
    let owner = Actor::new("lapsed-ban-owner");
    let member = Actor::new("lapsed-ban-member");
    let channel = "chan-lapsed-ban";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    seed_membership(&env, channel, &member, Role::Member).await;

    let mut stream = open_stream(&env, &member).await;
    let before = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "before the lapsed ban",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &before).await.status, 200);
    assert_eq!(
        next_event_frame(&mut stream, Duration::from_secs(15)).await,
        Some((buzz::relay::crypto::to_hex(&before.id), false)),
        "the member is live before anything is seeded"
    );

    // The ban ended one millisecond ago on the frozen clock, and nothing has
    // rewritten the document since — which is the normal state of affairs, since
    // only a LATER moderation command normalizes it.
    seed_restriction(
        &env,
        &member,
        &Restriction {
            banned: true,
            banned_until: Some(T0_MS - 1),
            timeout_until: None,
        },
    )
    .await;
    bump_tombstone(&env, &member).await;

    let after = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "after the ban lapsed",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &after).await.status, 200);
    assert_eq!(
        next_event_frame(&mut stream, Duration::from_secs(15)).await,
        Some((buzz::relay::crypto::to_hex(&after.id), false)),
        "a member whose ban has LAPSED was evicted from their live session — the \
         write path admits them, so the relay is contradicting itself"
    );

    // The write path's agreement, asserted rather than assumed: this is the half
    // that was already time-aware, and it is what makes the eviction wrong.
    let mine = signed_at(
        &member,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "and I can still write",
        T0_SECS + 2,
    );
    assert_eq!(
        submit(&env, &member, &mine).await.status,
        200,
        "the lapsed ban blocked a write too, which would make the eviction correct"
    );

    });
}

/// The control: a ban that is still LIVE evicts exactly as before, so the fix
/// above narrowed the check rather than disabling it.
#[test]
 fn severe_a_live_temporary_ban_still_evicts() {
    common::run_scenario(|| async move {
    let _serial = LAPSED_BAN_GATE_LOCK.lock().await;
    let env = pump_env().await;
    let owner = Actor::new("live-ban-owner");
    let member = Actor::new("live-ban-member");
    let channel = "chan-live-ban";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;
    seed_membership(&env, channel, &member, Role::Member).await;

    let mut stream = open_stream(&env, &member).await;
    let before = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "before the live ban",
        T0_SECS,
    );
    assert_eq!(submit(&env, &owner, &before).await.status, 200);
    assert_eq!(
        next_event_frame(&mut stream, Duration::from_secs(15)).await,
        Some((buzz::relay::crypto::to_hex(&before.id), false)),
        "the member is live before the ban"
    );

    seed_restriction(
        &env,
        &member,
        &Restriction {
            banned: true,
            banned_until: Some(T0_MS + 3_600_000),
            timeout_until: None,
        },
    )
    .await;
    bump_tombstone(&env, &member).await;

    let after = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "after the live ban",
        T0_SECS + 1,
    );
    assert_eq!(submit(&env, &owner, &after).await.status, 200);
    assert_eq!(
        next_event_frame(&mut stream, Duration::from_secs(15)).await,
        Some(("revoked".to_string(), true)),
        "a LIVE ban must still end the session"
    );

    });
}
