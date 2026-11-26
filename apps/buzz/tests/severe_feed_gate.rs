//! **M1.12 / W4 remainder — the home feed, decided by the WIRING.**
//!
//! `POST /buzz/api/feed` serves buzz's three categories with buzz's kind sets
//! and caps: mentions (stream messages + forum posts `p`-tagging the caller),
//! needs_action (approval requests + reminders `p`-tagging the caller), and
//! activity (recent readable-channel traffic). Every row is re-authorized
//! through the one access chokepoint, which is what realizes buzz's
//! `Some(empty) = global-only` convention: a caller with no readable channels
//! gets exactly the community-global rows, never everything. Assertions on
//! the route's replies and durable state, never logs.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::{Role, Visibility};
use buzz::relay::crypto::to_hex;
use buzz::relay::kind;
use common::{
    Actor, T0_SECS, body_json, gate_env, membership, post, seed_channel_with_owner, seed_membership,
    signed_at, submit, tag,
};
use ferrite_app::serde_json::{json, Value};
use common::SsoTestEnv as TestEnv;

const FEED_PATH: &str = "/buzz/api/feed";

async fn feed(env: &TestEnv, actor: &Actor, body: Value) -> (u16, Value) {
    let reply = env
        .handle(post(actor, FEED_PATH, body.to_string().into_bytes()))
        .await
        .expect("feed replies");
    (reply.status, body_json(&reply))
}

fn feed_ids(body: &Value) -> Vec<String> {
    body["events"]
        .as_array()
        .expect("events array")
        .iter()
        .map(|e| e["id"].as_str().expect("id").to_string())
        .collect()
}

/// Mentions are kind-filtered (a membership command `p`-tagging the caller is
/// NOT a mention), access-gated per event (a private channel the caller
/// cannot read contributes nothing), newest first, `since`-cut and
/// limit-capped.
#[test]
 fn severe_mentions_are_kind_filtered_and_access_gated() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let alice = Actor::new("alice");
    let open = "chan-open";
    let private_in = "chan-private-in";
    let private_out = "chan-private-out";
    seed_channel_with_owner(&env, open, Visibility::Open, &owner).await;
    seed_channel_with_owner(&env, private_in, Visibility::Private, &owner).await;
    seed_channel_with_owner(&env, private_out, Visibility::Private, &owner).await;
    seed_membership(&env, private_in, &alice, Role::Member).await;

    let mention = |channel: &str, text: &str, at: u64| {
        signed_at(
            &owner,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", channel), tag("p", &alice.pk_hex())],
            text,
            at,
        )
    };

    let old_open = mention(open, "old open mention", T0_SECS - 100);
    let in_private = mention(private_in, "private mention alice can read", T0_SECS - 50);
    let out_private = mention(
        private_out,
        "private mention alice canNOT read",
        T0_SECS - 40,
    );
    let new_open = mention(open, "new open mention", T0_SECS - 10);
    // A membership command p-tagging alice: NEVER a mention (kind filter).
    let put_user = signed_at(
        &owner,
        kind::KIND_NIP29_PUT_USER,
        vec![
            tag("h", open),
            tag("p", &alice.pk_hex()),
            tag("role", "member"),
        ],
        "",
        T0_SECS - 5,
    );
    for e in [&old_open, &in_private, &out_private, &new_open, &put_user] {
        assert_eq!(submit(&env, &owner, e).await.status, 200);
    }

    let (status, body) = feed(&env, &alice, json!({ "category": "mentions" })).await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(
        feed_ids(&body),
        vec![
            to_hex(&new_open.id),
            to_hex(&in_private.id),
            to_hex(&old_open.id)
        ],
        "kind-filtered, access-gated, newest first: {body}"
    );

    // `since` is a cutoff, not a filter: only rows at/after it come back.
    let (_, since_body) = feed(
        &env,
        &alice,
        json!({ "category": "mentions", "since": T0_SECS - 20 }),
    )
    .await;
    assert_eq!(feed_ids(&since_body), vec![to_hex(&new_open.id)]);

    // `limit` caps (and is itself capped at 100 — an absurd ask is clamped,
    // never honored).
    let (_, limited) = feed(&env, &alice, json!({ "category": "mentions", "limit": 1 })).await;
    assert_eq!(feed_ids(&limited).len(), 1);
    let (status, _) = feed(
        &env,
        &alice,
        json!({ "category": "mentions", "limit": 100000 }),
    )
    .await;
    assert_eq!(status, 200, "an oversized limit clamps rather than errors");

    // An unknown category is a 400, not a guess.
    let (status, _) = feed(&env, &alice, json!({ "category": "everything" })).await;
    assert_eq!(status, 400);

    });
}

/// needs_action and mentions are DISJOINT kind sets: a reminder `p`-tagging
/// the caller appears only under needs_action, a chat mention only under
/// mentions.
#[test]
 fn severe_needs_action_is_reminders_and_approvals_only() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let bob = Actor::new("bob");
    let channel = "chan-actions";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    let chat = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", channel), tag("p", &bob.pk_hex())],
        "bob, thoughts?",
        T0_SECS - 20,
    );
    let reminder = signed_at(
        &owner,
        kind::KIND_STREAM_REMINDER,
        vec![tag("h", channel), tag("p", &bob.pk_hex())],
        "review the rollout doc",
        T0_SECS - 10,
    );
    for e in [&chat, &reminder] {
        assert_eq!(submit(&env, &owner, e).await.status, 200);
    }

    let (_, actions) = feed(&env, &bob, json!({ "category": "needs_action" })).await;
    assert_eq!(
        feed_ids(&actions),
        vec![to_hex(&reminder.id)],
        "needs_action carries the reminder and not the chat mention: {actions}"
    );
    let (_, mentions) = feed(&env, &bob, json!({ "category": "mentions" })).await;
    assert_eq!(
        feed_ids(&mentions),
        vec![to_hex(&chat.id)],
        "mentions carries the chat and not the reminder: {mentions}"
    );

    });
}

/// Activity is scoped to what the caller can READ — and a caller with no
/// readable channels gets exactly the global rows (buzz's
/// `Some(empty) = global-only` convention), which for channel-scoped chat
/// means an empty feed, never a leak.
#[test]
 fn severe_activity_is_scoped_to_readable_channels() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let member = Actor::new("member");
    let stranger = Actor::new("stranger");
    let open = "chan-act-open";
    let private = "chan-act-private";
    seed_channel_with_owner(&env, open, Visibility::Open, &owner).await;
    seed_channel_with_owner(&env, private, Visibility::Private, &owner).await;
    seed_membership(&env, private, &member, Role::Member).await;

    let open_msg = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", open)],
        "public traffic",
        T0_SECS - 30,
    );
    let private_msg = signed_at(
        &owner,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", private)],
        "private planning",
        T0_SECS - 20,
    );
    for e in [&open_msg, &private_msg] {
        assert_eq!(submit(&env, &owner, e).await.status, 200);
    }

    // The member sees both, newest first.
    let (_, member_feed) = feed(&env, &member, json!({ "category": "activity" })).await;
    assert_eq!(
        feed_ids(&member_feed),
        vec![to_hex(&private_msg.id), to_hex(&open_msg.id)],
        "{member_feed}"
    );

    // The stranger sees only the open channel's row — the private one neither
    // appears nor is signalled.
    let (_, stranger_feed) = feed(&env, &stranger, json!({ "category": "activity" })).await;
    assert_eq!(
        feed_ids(&stranger_feed),
        vec![to_hex(&open_msg.id)],
        "{stranger_feed}"
    );

    });
}
