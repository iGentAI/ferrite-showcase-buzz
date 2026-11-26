//! The one lever buzz has over feed size, and the thing it must never touch.
//!
//! The log plane carries no per-topic retention, no prefix trim and no
//! per-record erasure (a known platform gap in the log lifecycle), so a
//! delivery feed grows for as long as the generation lives. Retiring a
//! SUPERSEDED generation — a whole-topic delete — is the only disposal the
//! platform admits, and `POST /buzz/api/operator/feed/retire` is where it lives.
//!
//! The route deletes durable history, so this gate is written around the
//! refusals rather than the deletion:
//!
//! * the LIVE generation is never retired, which on a deployment that has never
//!   rotated means the route retires nothing at all and says so;
//! * the changefeed, which is never rotated, is never a candidate;
//! * an unconfirmed call plans and deletes nothing;
//! * a member with an impeccable NIP-98 header is not an operator.
//!
//! Which generations a rotated deployment WOULD retire is decided by
//! `ingest::superseded_feed_generations`, a pure function with its own unit
//! tests; a gate cannot rotate a compiled-in generation.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::sync::atomic::{AtomicU64, Ordering};

use buzz::relay::crypto::sha256::sha256;
use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{keys, kind, operator, store as buzz_store};
use common::{
    Actor, COMMUNITY, FEED_GENERATION, T0_SECS, auth_header, body_json, gate_env, feed_copies,
    request_with_auth, signed, submit,
};
use ferrite_app::serde_json::{self, json};
use ferrite_app::IngressRequest;
use common::SsoTestEnv as TestEnv;

const RETIRE_PATH: &str = "/buzz/api/operator/feed/retire";

static RET_NONCE: AtomicU64 = AtomicU64::new(0);

fn nonce() -> String {
    format!("ret{}", RET_NONCE.fetch_add(1, Ordering::Relaxed))
}

/// A POST whose NIP-98 header commits to the body, as every operator write does.
fn operator_post(actor: &Actor, path: &str, body: Vec<u8>) -> IngressRequest {
    let event = Event::sign_new(
        &actor.sk,
        kind::KIND_HTTP_AUTH,
        vec![
            vec!["u".into(), format!("https://buzz.test{path}")],
            vec!["method".into(), "POST".into()],
            vec!["payload".into(), to_hex(&sha256(&body))],
            vec!["nonce".into(), nonce()],
        ],
        "",
        T0_SECS,
    )
    .expect("authorization events sign");
    request_with_auth("POST", path, Some(&auth_header(&event)), body)
}

async fn seed_operator(env: &TestEnv, op: &Actor) {
    let mut config = env.store("config");
    let set = serde_json::to_vec(&json!([op.pk_hex()])).expect("set encodes");
    buzz_store::put_raw(&mut config, operator::OPERATOR_SET_CONFIG, &set)
        .await
        .expect("operator seed");
}

async fn topic_names(env: &TestEnv) -> Vec<String> {
    let mut topics = env.topics("feed");
    topics
        .list_topics()
        .await
        .expect("topic list")
        .into_iter()
        .map(|(name, _, _)| name)
        .collect()
}

#[test]
 fn severe_retirement_never_touches_the_live_feed_or_the_changefeed() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let op = Actor::new("the-operator");
    let member = Actor::new("an-ordinary-member");
    seed_operator(&env, &op).await;

    // A real event, so the live feed holds something worth not deleting.
    let author = Actor::new("an-author");
    let event = signed(&author, kind::KIND_TEXT_NOTE, vec![], "hello");
    let accepted = submit(&env, &author, &event).await;
    assert_eq!(accepted.status, 200, "{}", body_json(&accepted));
    assert_eq!(
        feed_copies(&env, FEED_GENERATION, &event.id).await,
        1,
        "the live feed must hold the event this gate refuses to lose"
    );
    let before = topic_names(&env).await;
    assert!(
        before.contains(&keys::feed_topic(COMMUNITY, FEED_GENERATION)),
        "the live feed topic must exist: {before:?}"
    );

    // 1. A plan on a deployment that has never rotated is empty.
    let planned = env
        .handle(operator_post(&op, RETIRE_PATH, b"{}".to_vec()))
        .await
        .expect("a reply");
    assert_eq!(planned.status, 200, "{}", body_json(&planned));
    let p = body_json(&planned);
    assert_eq!(p["ok"], json!(true));
    assert_eq!(p["confirmed"], json!(false));
    assert_eq!(p["live_generation"], json!(FEED_GENERATION));
    assert_eq!(
        p["topics"],
        json!([]),
        "generation zero has superseded nothing: {p}"
    );

    // 2. A CONFIRMED call retires nothing either, and both topics survive it.
    //    This is the property that matters: the sweep is bounded from below by
    //    the live generation, not by the operator remembering to be careful.
    let confirmed = env
        .handle(operator_post(
            &op,
            RETIRE_PATH,
            json!({ "confirm": true }).to_string().into_bytes(),
        ))
        .await
        .expect("a reply");
    assert_eq!(confirmed.status, 200, "{}", body_json(&confirmed));
    let c = body_json(&confirmed);
    assert_eq!(c["confirmed"], json!(true));
    assert_eq!(c["retired"], json!(0), "the live generation was retired: {c}");

    let after = topic_names(&env).await;
    assert!(
        after.contains(&keys::feed_topic(COMMUNITY, FEED_GENERATION)),
        "the live feed topic was deleted: {after:?}"
    );
    assert!(
        after.contains(&keys::changefeed_topic(COMMUNITY)),
        "the changefeed was deleted: {after:?}"
    );
    assert_eq!(
        feed_copies(&env, FEED_GENERATION, &event.id).await,
        1,
        "the live feed lost its records"
    );

    // 3. A member's impeccable header authorizes nothing here.
    let refused = env
        .handle(operator_post(
            &member,
            RETIRE_PATH,
            json!({ "confirm": true }).to_string().into_bytes(),
        ))
        .await
        .expect("a reply");
    assert_eq!(
        refused.status, 403,
        "a non-operator reached the retirement route: {}",
        body_json(&refused)
    );

    // 4. An unsigned request is refused before any planning happens.
    let anonymous = env
        .handle(request_with_auth(
            "POST",
            RETIRE_PATH,
            None,
            json!({ "confirm": true }).to_string().into_bytes(),
        ))
        .await
        .expect("a reply");
    assert_eq!(anonymous.status, 401, "{}", body_json(&anonymous));

    });
}
