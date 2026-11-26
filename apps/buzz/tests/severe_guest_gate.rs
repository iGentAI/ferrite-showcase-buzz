//! **Guest access.** The demonstration is public: a visitor with no Ferrite
//! session reaches the relay and can do what the product is for, joining an
//! open room, posting in it and reading it back, under the key their own
//! client holds.
//!
//! Every request here is sent WITHOUT the property session cookie (the bare
//! harness, `env.0`), so nothing in these gates rides on a sign-in. The
//! workspace identity still comes from NIP-98 on every request, so the same
//! gates that bind a signed-in user bind a guest: a private room stays closed
//! to them, and a request with no authorization at all is still refused.

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
    Actor, QUERY_PATH, SUBMIT_ATTEMPTS, channel_query, gate_env, error_text, membership, post, query,
    request_with_auth, returned, seed_channel_with_owner, signed, submit_request, tag,
};
use ferrite_app::serde_json::{self, Value};
use ferrite_app::IngressReply;
use ferrite_app_test::{ingress, TestEnv};

/// Submit `event` as `actor` with NO session cookie, re-posting a lost reply
/// for the frozen-clock reason [`common::submit`] documents.
async fn submit_as_guest(env: &TestEnv, actor: &Actor, event: &buzz::relay::event::Event) -> IngressReply {
    for attempt in 1..=SUBMIT_ATTEMPTS {
        match env.handle(submit_request(actor, event)).await {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => panic!("no reply to a guest submission: {e}"),
            Err(_) => continue,
        }
    }
    unreachable!("the loop either returns a reply or panics")
}

/// `POST /buzz/api/query` as `actor` with NO session cookie.
async fn query_as_guest(env: &TestEnv, actor: &Actor, body: &Value) -> IngressReply {
    let bytes = serde_json::to_vec(body).expect("query body encodes");
    for attempt in 1..=SUBMIT_ATTEMPTS {
        match env.handle(post(actor, QUERY_PATH, bytes.clone())).await {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => panic!("no reply to a guest query: {e}"),
            Err(_) => continue,
        }
    }
    unreachable!("the loop either returns a reply or panics")
}

/// **A visitor with no session joins an open room, posts, and reads it back,
/// and a signed-in member sees the guest's message.**
#[test]
fn severe_an_anonymous_guest_joins_posts_and_reads_an_open_room() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let owner = Actor::new("guest-gate-owner");
        let guest = Actor::new("guest-gate-visitor");
        let channel = "chan-guest-lobby";
        seed_channel_with_owner(&env, channel, Visibility::Open, &owner).await;

        // Admitted: no sign-in redirect, no 401.
        let health = env
            .0
            .handle(common::get("/buzz/api/healthz"))
            .await
            .expect("the relay answers");
        assert_eq!(health.status, 200, "{}", error_text(&health));

        let join = signed(&guest, kind::KIND_NIP29_JOIN_REQUEST, vec![tag("h", channel)], "");
        let reply = submit_as_guest(&env.0, &guest, &join).await;
        assert_eq!(reply.status, 200, "guest join: {}", error_text(&reply));
        let row = membership(&env, channel, &guest).await.expect("the guest is a member");
        assert!(row.is_live());

        let hello = signed(
            &guest,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", channel)],
            "hello from a guest",
        );
        let reply = submit_as_guest(&env.0, &guest, &hello).await;
        assert_eq!(reply.status, 200, "guest post: {}", error_text(&reply));

        let page = query_as_guest(&env.0, &guest, &channel_query(channel)).await;
        assert_eq!(page.status, 200, "{}", error_text(&page));
        assert!(returned(&page, &hello), "the guest reads its own message back");

        // A signed-in caller shares the same workspace with the guest.
        let page = query(&env, &owner, &channel_query(channel)).await;
        assert_eq!(page.status, 200, "{}", error_text(&page));
        assert!(returned(&page, &hello), "a signed-in member sees the guest");
    });
}

/// **Opening the gate opened nothing behind it.** A guest is still refused a
/// private room, and a request with no NIP-98 authorization is still refused.
#[test]
fn severe_an_anonymous_guest_is_still_bound_by_the_workspace_gates() {
    common::run_scenario(|| async move {
        let env = gate_env().await;
        let owner = Actor::new("guest-gate-private-owner");
        let guest = Actor::new("guest-gate-intruder");
        let channel = "chan-guest-private";
        seed_channel_with_owner(&env, channel, Visibility::Private, &owner).await;

        let join = signed(&guest, kind::KIND_NIP29_JOIN_REQUEST, vec![tag("h", channel)], "");
        let reply = submit_as_guest(&env.0, &guest, &join).await;
        assert_eq!(reply.status, 403, "a guest cannot self-join a private room");
        assert!(membership(&env, channel, &guest).await.is_none());

        let post_in = signed(
            &guest,
            kind::KIND_STREAM_MESSAGE,
            vec![tag("h", channel)],
            "let me in",
        );
        let reply = submit_as_guest(&env.0, &guest, &post_in).await;
        assert!(
            reply.status == 401 || reply.status == 403,
            "a guest cannot write into a private room: {} {}",
            reply.status,
            error_text(&reply)
        );

        let unsigned = env
            .0
            .handle(request_with_auth(
                "POST",
                QUERY_PATH,
                None,
                serde_json::to_vec(&channel_query(channel)).expect("encodes"),
            ))
            .await
            .expect("the relay answers");
        assert_eq!(unsigned.status, 401, "NIP-98 is still required: {}", error_text(&unsigned));
    });
}
