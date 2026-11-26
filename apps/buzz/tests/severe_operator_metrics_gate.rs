//! **The metrics document is tiered** (adversarial review finding 10).
//!
//! `GET /buzz/api/metrics` stays public, because a liveness probe holds no key
//! and an app binds no private port. What it may SAY without one is the point of
//! this gate:
//!
//! * open: `ok`, `app`, `community`, `scope`, the identity fields NIP-11 already
//!   publishes on `/buzz/api/relay-info` (`configured`, `pubkey`), and
//!   `stream_key_configured` — the two `*_configured` booleans are the documented
//!   probe for the platform's secrets-do-not-survive-a-restart gap, and a check
//!   that needs a signed request is a check that stops being run;
//! * operator only: `identity.source` (secrets plane vs config fallback) and the
//!   `requests` queue counters — a volume disclosure and a live back-pressure
//!   oracle.
//!
//! The escalation cases matter as much as the happy one: a valid NIP-98 header
//! from a NON-operator, and an operator's header minted for a DIFFERENT path,
//! must both degrade to the public document rather than unlock the operator tier
//! or 401 the probe.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::sync::atomic::{AtomicU64, Ordering};

use buzz::relay::crypto::to_hex;
use buzz::relay::event::Event;
use buzz::relay::{kind, operator, store as buzz_store};
use common::{Actor, T0_SECS, auth_header, body_json, gate_env, request_with_auth};
use ferrite_app::serde_json::{self, json};
use ferrite_app::IngressRequest;
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{ingress};

const METRICS_PATH: &str = "/buzz/api/metrics";

/// The config key the relay's signing identity is seeded into
/// (`ingress::RELAY_IDENTITY_CONFIG`).
const RELAY_IDENTITY_CONFIG: &[u8] = b"buzz-relay-identity";

static MET_NONCE: AtomicU64 = AtomicU64::new(0);

fn nonce() -> String {
    format!("met{}", MET_NONCE.fetch_add(1, Ordering::Relaxed))
}

/// An authenticated GET of `path` (a GET carries no body, so no payload tag).
fn signed_get(actor: &Actor, path: &str) -> IngressRequest {
    let event = Event::sign_new(
        &actor.sk,
        kind::KIND_HTTP_AUTH,
        vec![
            vec!["u".into(), format!("https://buzz.test{path}")],
            vec!["method".into(), "GET".into()],
            vec!["nonce".into(), nonce()],
        ],
        "",
        T0_SECS,
    )
    .expect("authorization events sign");
    request_with_auth("GET", METRICS_PATH, Some(&auth_header(&event)), Vec::new())
}

/// Seed the operator set and a config-fallback relay identity from OUTSIDE the
/// face — the only seam a gate has (`ferrite-app-test` injects no secrets).
async fn seed(env: &TestEnv, op: &Actor, identity: &Actor) {
    let mut config = env.store("config");
    let set = serde_json::to_vec(&json!([op.pk_hex()])).expect("set encodes");
    buzz_store::put_raw(&mut config, operator::OPERATOR_SET_CONFIG, &set)
        .await
        .expect("operator seed");
    buzz_store::put_raw(
        &mut config,
        RELAY_IDENTITY_CONFIG,
        to_hex(&identity.sk).as_bytes(),
    )
    .await
    .expect("identity seed");
}

#[test]
 fn severe_metrics_serves_operators_the_detail_and_everyone_else_the_liveness_core() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let op = Actor::new("the-operator");
    let stranger = Actor::new("a-member");
    let identity = Actor::new("the-relay-itself");
    seed(&env, &op, &identity).await;

    // 1. The open document: liveness plus what NIP-11 already publishes.
    let public = env
        .handle(common::get(METRICS_PATH))
        .await
        .expect("a reply");
    assert_eq!(public.status, 200);
    let p = body_json(&public);
    assert_eq!(p["ok"], json!(true));
    assert_eq!(p["scope"], json!("public"));
    assert_eq!(p["identity"]["configured"], json!(true));
    assert_eq!(
        p["identity"]["pubkey"],
        json!(identity.pk_hex()),
        "the pubkey NIP-11 publishes stays public here too"
    );
    assert!(
        p["stream_key_configured"].is_boolean(),
        "the restart-gap probe stays open: {p}"
    );
    assert!(p["identity"]["source"].is_null(), "{p}");
    assert!(p["requests"].is_null(), "{p}");

    // 2. The operator document: every field, and it says so.
    let full = env
        .handle(signed_get(&op, METRICS_PATH))
        .await
        .expect("a reply");
    assert_eq!(full.status, 200, "{}", String::from_utf8_lossy(&full.body));
    let f = body_json(&full);
    assert_eq!(f["scope"], json!("operator"));
    assert_eq!(
        f["identity"]["source"],
        json!("config"),
        "the fallback seam is named"
    );
    assert!(f["stream_key_configured"].is_boolean(), "{f}");
    // A read the plane ANSWERED carries numbers and says so. The `readable`
    // flag is what separates that from a refused `Counts`, which now
    // answers `null` rather than a zero an operator would read as "drained".
    assert_eq!(f["requests"]["readable"], json!(true), "{f}");
    assert!(f["requests"]["published"].is_number(), "{f}");
    assert!(f["requests"]["pending"].is_number(), "{f}");
    assert!(f["requests"]["dead_lettered"].is_number(), "{f}");

    // 3. A member with a perfectly valid NIP-98 header is NOT an operator: the
    //    route answers, and answers the public document.
    let member = env
        .handle(signed_get(&stranger, METRICS_PATH))
        .await
        .expect("a reply");
    assert_eq!(member.status, 200, "the probe must never be 401'd");
    let m = body_json(&member);
    assert_eq!(m["scope"], json!("public"));
    assert!(
        m["requests"].is_null(),
        "a non-operator read the operator tier: {m}"
    );
    assert!(m["identity"]["source"].is_null(), "{m}");

    // 4. The operator's own token, minted for a DIFFERENT path, unlocks nothing.
    //    (NIP-98 binds method+path; this is the cross-path escalation attempt.)
    let cross = env
        .handle(signed_get(&op, "/buzz/api/operator/config"))
        .await
        .expect("a reply");
    assert_eq!(cross.status, 200);
    let c = body_json(&cross);
    assert_eq!(
        c["scope"],
        json!("public"),
        "a cross-path token unlocked the detail: {c}"
    );
    assert!(c["requests"].is_null(), "{c}");

    });
}
