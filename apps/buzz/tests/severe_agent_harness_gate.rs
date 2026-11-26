//! **`GET /buzz/api/agent-harnesses`, decided by the WIRING.**
//!
//! buzz's desktop client probes `PATH` for CLI harnesses; a browser tab can
//! probe nothing, so THIS RELAY is what says which agent backends the
//! deployment has. Two sources, in order: the operator's
//! `buzz-agent-harnesses` document when set, otherwise a default derived from
//! what is actually WIRED — Ferrite Cloud is `available` exactly when the
//! turn pipeline's queue (`agent-turns`, the same condition the composition
//! root uses to spawn the pipeline) is bound.
//!
//! Both branches are gated on real fleets whose bindings differ, and the
//! failure mode that matters most is gated explicitly: a malformed operator
//! document must FALL BACK to the wired truth, never serve garbage and never
//! 500 — a client renders whatever rows it gets.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;

use common::{REPLY_BUDGET, T0_MS, body_json, gate_env};
use ferrite_app::serde_json::{json, Value};
use common::SsoTestEnv as TestEnv;
use ferrite_app_test::{ingress, TestHarness};

/// One extra fleet at a time in this binary (the wired fixture stands beside
/// the common permit).
static HARNESS_GATE_LOCK: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

/// The operator config key the route reads (`ingress::AGENT_HARNESSES_CONFIG`).
const AGENT_HARNESSES_CONFIG: &[u8] = b"buzz-agent-harnesses";

/// The common fleet PLUS the turn queue — the wiring that makes Ferrite
/// Cloud real (`severe_agent_gate.rs` proves the pipeline behind it; this
/// gate proves the declaration).
async fn wired_env() -> TestEnv {
    let env = TestEnv(TestHarness::new().with_store("nonces")
        .with_queue("requests")
        .with_queue("replies")
        .with_queue("stream-requests")
        .with_queue("agent-turns")
        .with_store("events")
        .with_store("config")
        .with_ape("guards")
        .with_topics("feed")
        .with_blob("media")
        .with_blob("web-assets")
        .request_reply("requests", "replies")
        .stream_requests("stream-requests")
        .manual_clock(T0_MS)
        .timeout(REPLY_BUDGET)
        .build()
        .await);
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
    for _ in 0..40 {
        match env.handle(common::get("/buzz/api/healthz")).await {
            Ok(reply) if reply.status == 200 => return env,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("the app did not become healthy");
}

/// Ask the REAL route (public, unauthenticated — a client asks before it has
/// any identity) and return its parsed body.
async fn harnesses(env: &TestEnv) -> Value {
    let reply = env
        .handle(common::get("/buzz/api/agent-harnesses"))
        .await
        .expect("the app replied");
    assert_eq!(
        reply.status,
        200,
        "the declaration route must never fail a client: {}",
        String::from_utf8_lossy(&reply.body)
    );
    body_json(&reply)
}

/// The row for `id`, insisted on.
fn row<'a>(body: &'a Value, id: &str) -> &'a Value {
    body["harnesses"]
        .as_array()
        .unwrap_or_else(|| panic!("harnesses must be an array: {body}"))
        .iter()
        .find(|h| h["id"] == json!(id))
        .unwrap_or_else(|| panic!("no {id} row: {body}"))
}

async fn seed_operator_doc(env: &TestEnv, bytes: &[u8]) {
    let mut config = env.store("config");
    config
        .put(AGENT_HARNESSES_CONFIG, bytes)
        .await
        .expect("config seed");
}

/// The assertions the WIRED default must satisfy — shared by the "no
/// document" and "malformed document" cases, because falling back means
/// serving exactly this.
fn assert_wired_default(body: &Value, cloud_available: bool) {
    let rows = body["harnesses"]
        .as_array()
        .unwrap_or_else(|| panic!("the default must be an array of harness rows: {body}"));
    assert_eq!(rows.len(), 3, "cloud + fabric + a2a: {body}");

    let cloud = row(body, "ferrite-cloud");
    if cloud_available {
        assert_eq!(
            cloud["availability"], "available",
            "the seam IS bound, so Ferrite Cloud must be available: {cloud}"
        );
        assert_eq!(
            cloud["install_hint"], "",
            "an available backend needs no hint"
        );
    } else {
        assert_eq!(
            cloud["availability"], "not_installed",
            "no seam bound, so Ferrite Cloud must NOT be declared: {cloud}"
        );
        let hint = cloud["install_hint"].as_str().unwrap_or_default();
        assert!(
            hint.contains("agent-turns"),
            "the hint must name the missing wiring: {hint:?}"
        );
    }

    // The endpoint-needing backends are reported not_installed with the way
    // in — never available-but-broken.
    for id in ["fabric", "a2a"] {
        let r = row(body, id);
        assert_eq!(r["availability"], "not_installed", "{r}");
        assert!(
            r["install_hint"]
                .as_str()
                .unwrap_or_default()
                .contains("buzz-agent-harnesses"),
            "{id} must say how an operator declares it: {r}"
        );
    }
}

// ------------------------------------------------------------------ the gates

/// **Unwired: the default tells the truth, and a broken document falls back to
/// it.** On a fleet with NO agents seam, Ferrite Cloud is `not_installed` with
/// the missing binding named; a malformed operator document serves that same
/// default (not garbage, not a 500); a valid document replaces it verbatim.
#[test]
 fn severe_harnesses_default_reports_the_unwired_seam_and_survives_a_malformed_document() {
    common::run_scenario(|| async move {
    let env = gate_env().await; // the common fleet: no agent-commands, no agent-results

    // 1. No operator document: the wired truth.
    assert_wired_default(&harnesses(&env).await, false);

    // 2. A document that is not JSON (a bad paste, a torn write): the route
    //    must fall back to the derived default rather than serve garbage.
    seed_operator_doc(&env, b"{ this is not json").await;
    assert_wired_default(&harnesses(&env).await, false);

    // 3. A valid document is served VERBATIM: declaring a backend is an
    //    operator write, not a client release.
    let declared = json!([{
        "id": "fabric",
        "label": "Fabric",
        "kind": "remote",
        "availability": "available",
        "auth_status": { "status": "logged_in" },
        "endpoint": "https://fabric.example/api",
        "install_hint": "",
    }]);
    seed_operator_doc(&env, declared.to_string().as_bytes()).await;
    let body = harnesses(&env).await;
    assert_eq!(
        body["harnesses"], declared,
        "the operator's declaration must be served exactly as written"
    );

    });
}

/// **Wired: Ferrite Cloud is available exactly because the turn queue is
/// bound.** Same route, a fleet whose only difference is the `agent-turns`
/// queue — so together with the unwired gate this pins "available ⇔ the turn
/// pipeline can actually run". A malformed document falls back to the WIRED
/// default, and a declared document beats detection.
#[test]
 fn severe_harnesses_declare_ferrite_cloud_exactly_when_the_agents_seam_is_bound() {
    common::run_scenario(|| async move {
    let _serial = HARNESS_GATE_LOCK.lock().await;
    let env = wired_env().await;

    // 1. No operator document: the seam is bound, so the cloud is declared.
    assert_wired_default(&harnesses(&env).await, true);

    // 2. Malformed document: fall back to the wired truth, still available.
    seed_operator_doc(&env, b"[ not json either").await;
    assert_wired_default(&harnesses(&env).await, true);

    // 3. The operator's word beats detection: a document that omits the cloud
    //    is served as-is, even though the seam is bound.
    let declared = json!([{
        "id": "a2a",
        "label": "A2A agent",
        "kind": "a2a",
        "availability": "available",
        "auth_status": { "status": "not_applicable" },
        "endpoint": "https://cards.example/agent.json",
        "install_hint": "",
    }]);
    seed_operator_doc(&env, declared.to_string().as_bytes()).await;
    assert_eq!(harnesses(&env).await["harnesses"], declared);

    });
}
