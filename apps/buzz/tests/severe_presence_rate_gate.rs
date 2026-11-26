//! **M1.10 / M1.11 — presence and rate limits, decided by the WIRING.**
//!
//! Presence is TTL-key absence: a heartbeat refreshes a key the read route lists
//! while it lives. The message rate limiter is a per-author fixed window that
//! refuses the over-limit write with 429 and fails OPEN on an ambiguous error.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::kind;
use common::{
    Actor, T0_SECS, body_json, gate_env, error_text, post, seed_channel_with_owner, seed_membership,
    signed, signed_at, submit, tag,
};
use common::SsoTestEnv as TestEnv;

async fn presence(env: &TestEnv, reader: &Actor) -> Vec<String> {
    let reply = env
        .handle(post(reader, "/buzz/api/presence", b"{}".to_vec()))
        .await
        .expect("presence reply");
    assert_eq!(reply.status, 200, "presence: {}", error_text(&reply));
    body_json(&reply)["online"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect()
}

/// A heartbeat (kind 20001) makes the author appear online; a member who never
/// beat does not.
#[test]
 fn severe_presence_heartbeat_appears_online() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let bob = Actor::new("bob");

    assert!(
        !presence(&env, &alice).await.contains(&alice.pk_hex()),
        "nobody online yet"
    );

    let beat = signed(&alice, kind::KIND_PRESENCE_UPDATE, vec![], "online");
    assert_eq!(submit(&env, &alice, &beat).await.status, 200);

    let online = presence(&env, &bob).await;
    assert!(
        online.contains(&alice.pk_hex()),
        "alice is online after her heartbeat"
    );
    assert!(!online.contains(&bob.pk_hex()), "bob never beat");

    });
}

/// The per-author message window refuses the over-limit write with 429, and the
/// count is per-author (a second author is unaffected).
#[test]
 fn severe_message_rate_limit_refuses_the_over_limit_write() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let spammer = Actor::new("spammer");
    let calm = Actor::new("calm");
    let channel = "chan-rate";
    seed_channel_with_owner(&env, channel, Visibility::Open, &spammer).await;
    seed_membership(&env, channel, &calm, buzz::relay::channel::Role::Member).await;

    // 60 messages are accepted; the 61st is refused.
    let mut refused_at = None;
    for i in 0..61u64 {
        let msg = signed_at(
            &spammer,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", channel)],
            &format!("m{i}"),
            T0_SECS + i,
        );
        let status = submit(&env, &spammer, &msg).await.status;
        if status == 429 {
            refused_at = Some(i);
            break;
        }
        assert_eq!(status, 200, "message {i} accepted");
    }
    assert_eq!(
        refused_at,
        Some(60),
        "the 61st message trips the 60/min limit"
    );

    // A different author is unaffected by the spammer's window.
    let ok = signed_at(
        &calm,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "hi",
        T0_SECS,
    );
    assert_eq!(
        submit(&env, &calm, &ok).await.status,
        200,
        "the limit is per-author"
    );

    });
}

/// A presence heartbeat does not count against the message tier (ephemeral kinds
/// are exempt), so an agent's heartbeats cannot starve its own message budget.
#[test]
 fn severe_heartbeats_do_not_consume_the_message_budget() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let agent = Actor::new("agent");
    let channel = "chan-rate-hb";
    seed_channel_with_owner(&env, channel, Visibility::Open, &agent).await;

    // 100 heartbeats: all accepted (ephemeral, not rate-limited).
    for i in 0..100u64 {
        let beat = signed_at(
            &agent,
            kind::KIND_PRESENCE_UPDATE,
            vec![],
            "online",
            T0_SECS + i,
        );
        assert_eq!(
            submit(&env, &agent, &beat).await.status,
            200,
            "heartbeat {i}"
        );
    }
    // A message still goes through — the budget was untouched.
    let msg = signed(
        &agent,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel)],
        "still here",
    );
    assert_eq!(submit(&env, &agent, &msg).await.status, 200);

    });
}
