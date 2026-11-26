//! **M1.4 — the channel window (top-level timeline), decided by the WIRING.**
//!
//! `POST /buzz/api/window` returns a channel's roots (and depth-1 broadcast
//! replies), hides ordinary nested replies, attaches a thread summary to a row
//! that has replies, and reports exhaustion ONLY through its `bounds` object —
//! never by row count. Every case posts real events and reads the route.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::{crypto, kind};
use common::{
    Actor, T0_SECS, body_json, gate_env, error_text, seed_channel_with_owner, signed_at, submit, tag,
};
use ferrite_app::serde_json::{json, Value};
use common::SsoTestEnv as TestEnv;

async fn post(env: &TestEnv, actor: &Actor, tags: Vec<Vec<String>>, at: u64) -> String {
    let ev = signed_at(actor, kind::KIND_STREAM_MESSAGE, tags, "msg", at);
    let reply = submit(env, actor, &ev).await;
    assert_eq!(reply.status, 200, "post: {}", error_text(&reply));
    crypto::to_hex(&ev.id)
}

fn reply_tags(channel: &str, root: &str, parent: &str) -> Vec<Vec<String>> {
    vec![
        tag("h", channel),
        vec!["e".into(), root.into(), String::new(), "root".into()],
        vec!["e".into(), parent.into(), String::new(), "reply".into()],
    ]
}

async fn window(env: &TestEnv, reader: &Actor, channel: &str, limit: u64) -> Value {
    let body = json!({ "channel": channel, "limit": limit })
        .to_string()
        .into_bytes();
    let reply = env
        .handle(post_req(reader, "/buzz/api/window", body))
        .await
        .expect("window reply");
    assert_eq!(reply.status, 200, "window: {}", error_text(&reply));
    body_json(&reply)
}

// A local alias to the harness `post` request builder (the module `post` fn name
// is shadowed above by our event-posting helper).
fn post_req(actor: &Actor, path: &str, body: Vec<u8>) -> ferrite_app::IngressRequest {
    common::post(actor, path, body)
}

/// A root with a nested reply shows only the root at the top level, and the root
/// carries a thread summary; a broadcast depth-1 reply also surfaces.
#[test]
 fn severe_window_hides_nested_replies_and_summarizes_roots() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let channel = "chan-window";
    seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

    let root = post(&env, &owner, vec![tag("h", channel)], T0_SECS).await;
    let _nested = post(&env, &owner, reply_tags(channel, &root, &root), T0_SECS + 1).await;
    // A second standalone root.
    let root2 = post(&env, &owner, vec![tag("h", channel)], T0_SECS + 2).await;
    // A broadcast depth-1 reply to root2 surfaces at the top level.
    let mut btags = reply_tags(channel, &root2, &root2);
    btags.push(vec!["broadcast".into(), "1".into()]);
    let broadcast = post(&env, &owner, btags, T0_SECS + 3).await;

    let w = window(&env, &owner, channel, 50).await;
    let events = w["events"].as_array().unwrap();
    let ids: Vec<String> = events
        .iter()
        .map(|e| e["event"]["id"].as_str().unwrap().to_string())
        .collect();

    assert!(ids.contains(&root), "the root is top-level");
    assert!(ids.contains(&root2), "the second root is top-level");
    assert!(
        ids.contains(&broadcast),
        "a broadcast depth-1 reply surfaces"
    );
    assert!(
        !ids.contains(&_nested),
        "an ordinary nested reply is hidden"
    );

    // The root carries a thread summary (it has one reply).
    let root_row = events
        .iter()
        .find(|e| e["event"]["id"] == json!(root))
        .unwrap();
    assert_eq!(root_row["thread"]["reply_count"], json!(1));

    // Exhausted: bounds says no more.
    assert_eq!(w["bounds"]["has_more"], json!(false));
    assert_eq!(w["bounds"]["next_cursor"], Value::Null);

    });
}

/// A non-member sees an empty, complete window on a private channel — its
/// existence is not signalled.
#[test]
 fn severe_window_hides_a_private_channel_from_a_non_member() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("owner");
    let stranger = Actor::new("stranger");
    let channel = "chan-window-private";
    seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;
    let _root = post(&env, &owner, vec![tag("h", channel)], T0_SECS).await;

    let w = window(&env, &stranger, channel, 50).await;
    assert!(
        w["events"].as_array().unwrap().is_empty(),
        "non-member sees nothing"
    );
    assert_eq!(w["bounds"]["has_more"], json!(false));

    });
}
