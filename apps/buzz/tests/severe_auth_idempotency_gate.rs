//! **A redelivered request must be answered, not refused.**
//!
//! The request queue is at-least-once, and this app relies on that: when a reply
//! cannot be published, `ingress::api_loop` deliberately leaves the envelope
//! unacked so the work is re-driven. Before this gate existed, the re-drive
//! re-entered `route`, hit `auth::consume_once`, found the authorization event id
//! already burned, and answered
//! `restricted: authorization event has already been used` — which published
//! fine and became the caller's answer.
//!
//! For a write that had already succeeded, that is the worst thing an API can
//! report: **the effect is durable and the user is told it failed.** It was
//! observed live — a channel sitting in the sidebar while its own creation
//! dialog displayed that refusal, whose modal overlay then swallowed every
//! subsequent click.
//!
//! So the property under test is NIP-98's real one — *the action happens at most
//! once* — not the weaker *a second delivery errors*. Each case drives the SAME
//! authorization event through the real ingress path twice and asserts on
//! durable state, not just on the reply.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::auth;
use common::{
    Actor, auth_event, auth_header, channel_doc, gate_env, error_text, fresh_auth, replay_marker,
    request_with_auth, stored_event_count,
};
use ferrite_app::serde_json::json;

/// A redelivered CREATE returns the original success, and creates nothing twice.
#[test]
 fn severe_a_redelivered_write_is_answered_with_its_first_result() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("dup-owner");
    let channel = "chan-dup";
    let body = json!({ "id": channel, "name": "Dup", "visibility": "open" })
        .to_string()
        .into_bytes();
    // Built ONCE: both deliveries carry the identical Authorization header, which
    // is exactly what a queue re-drive of one envelope looks like.
    let req = common::post(&owner, "/buzz/api/channels", body);

    let first = env.handle(req.clone()).await.expect("first delivery");
    assert_eq!(first.status, 201, "create: {}", error_text(&first));
    assert!(
        channel_doc(&env, channel).await.is_some(),
        "the write did not land, so the redelivery case is not being tested"
    );
    let events_after_first = stored_event_count(&env).await;

    // This used to be 401 + the burn message. The same applies to a captured
    // token sent again, so several deliveries are driven.
    for attempt in 0..3 {
        let again = env.handle(req.clone()).await.expect("redelivery");
        assert_eq!(
            (again.status, &again.body),
            (first.status, &first.body),
            "redelivery {attempt} must serve the first delivery's answer verbatim"
        );
        assert_eq!(
            stored_event_count(&env).await,
            events_after_first,
            "redelivery {attempt} performed the action a second time"
        );
    }

    });
}

/// A redelivered READ is not answered from the record — reads are deliberately
/// not recorded, because each recorded outcome costs a whole new store client
/// plus a write on the busiest routes. What it must STILL never do is lie: the
/// duplicate gets the retryable 503, never `already been used`.
///
/// This is the one place the write and read contracts differ, and the difference
/// is the point: a read retry is cheap, a write retry after a durable effect is
/// the failure this whole mechanism exists to prevent.
///
/// The duplicate must also be answered promptly. The wait used to run the whole
/// poll budget (~6 s) inside one of the request loop's 32 slots, so one signed
/// read replayed ~33 times at once stalled every route.
#[test]
 fn severe_a_redelivered_read_is_retryable_never_a_replay_refusal() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let reader = Actor::new("dup-reader");
    let req = common::post(
        &reader,
        "/buzz/api/query",
        json!({ "limit": 5 }).to_string().into_bytes(),
    );

    let first = env.handle(req.clone()).await.expect("first delivery");
    assert_eq!(first.status, 200, "query: {}", error_text(&first));

    let started = tokio::time::Instant::now();
    let second = env.handle(req.clone()).await.expect("second delivery");
    let waited = started.elapsed();
    assert!(
        !error_text(&second).contains(auth::REPLAYED_MESSAGE),
        "a redelivered read was refused as a replay: {}",
        error_text(&second)
    );
    assert_eq!(
        second.status,
        503,
        "a redelivered read must be retryable, got {}: {}",
        second.status,
        error_text(&second)
    );
    assert!(
        error_text(&second).contains("retry"),
        "the 503 must tell the caller to retry: {}",
        error_text(&second)
    );
    assert!(
        waited < std::time::Duration::from_secs(3),
        "a duplicate read parked for {waited:?} waiting on an answer a read never records"
    );

    });
}

/// The wrapper must not swallow ordinary auth failures. A token that is simply
/// too old is still refused **as stale** — turning every 401 into a retry would
/// trade one misleading answer for another.
#[test]
 fn severe_an_ordinary_auth_failure_is_still_reported_as_itself() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let actor = Actor::new("stale-caller");
    // Signed far outside the freshness window the app enforces.
    let stale = auth_event(&actor, "POST", "/buzz/api/query", 10, "stale-nonce");
    let req = request_with_auth(
        "POST",
        "/buzz/api/query",
        Some(&auth_header(&stale)),
        json!({ "limit": 5 }).to_string().into_bytes(),
    );

    let reply = env.handle(req).await.expect("the app replied");
    assert_eq!(reply.status, 401, "a stale token must still be refused");
    let text = error_text(&reply);
    assert!(
        text.contains("from now"),
        "a stale token must be refused AS STALE, got: {text}"
    );
    assert!(
        !text.contains(auth::REPLAYED_MESSAGE),
        "a stale token was misreported as a replay: {text}"
    );

    });
}

/// A header with a FORGED signature must leave no trace in the replay record.
///
/// The trap this pins: recognizing a duplicate does not verify the signature (it
/// would double the schnorr cost of every request), and an event's id does not
/// cover its `sig` — so a captured header with one bit flipped in `sig` still
/// resolves to the GENUINE authorization event id. If the refusal were recorded
/// against that id, anyone who captured a header could plant a refusal under it
/// and the victim's own later request would be served the plant instead of
/// working. Nothing may be recorded for a request that failed to authenticate.
#[test]
 fn severe_a_forged_header_records_nothing_and_cannot_pre_empt_the_real_one() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("forge-victim");
    let (genuine, header) = fresh_auth(&alice, "POST", "/buzz/api/query");

    // The captured header, with its signature tampered. Same event id.
    let mut forged: ferrite_app::serde_json::Value = ferrite_app::serde_json::from_slice(
        &buzz::relay::b64::decode(header.trim_start_matches("Nostr ").trim()).expect("base64"),
    )
    .expect("json");
    let sig = forged["sig"].as_str().expect("sig").to_string();
    let flipped = format!(
        "{}{}",
        if sig.starts_with('a') { 'b' } else { 'a' },
        &sig[1..]
    );
    forged["sig"] = ferrite_app::serde_json::Value::String(flipped);
    let forged_header = format!(
        "Nostr {}",
        buzz::relay::b64::encode(&ferrite_app::serde_json::to_vec(&forged).expect("encodes"))
    );

    let refused = env
        .handle(request_with_auth(
            "POST",
            "/buzz/api/query",
            Some(&forged_header),
            b"{}".to_vec(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(refused.status, 401, "a forged signature must be refused");
    assert!(
        replay_marker(&env, &genuine.id).await.is_none(),
        "a forged header planted a replay record for a token that was never burned"
    );

    // ...and the genuine header still works, unimpeded.
    let ok = env
        .handle(request_with_auth(
            "POST",
            "/buzz/api/query",
            Some(&header),
            b"{}".to_vec(),
        ))
        .await
        .expect("the app replied");
    assert_eq!(
        ok.status,
        200,
        "the genuine request was pre-empted: {}",
        error_text(&ok)
    );

    });
}

/// A FORGED signature on a STALE token must not be served the recorded answer.
///
/// The stale path consults the replay record so a re-drive delayed past the
/// freshness window still gets its first answer. `identify` does not verify the
/// signature, and an event's id does not cover `sig`, so a captured header with
/// a tampered signature resolves to the genuine id. Served from the record
/// without a signature check, anyone holding a captured header could read the
/// victim's recorded reply after the window closed, without the victim's key.
#[test]
fn severe_a_forged_signature_on_a_stale_token_is_not_served_the_recorded_answer() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("stale-forge-victim");
    let channel = "chan-stale-forge";
    let body = json!({ "id": channel, "name": "Stale forge", "visibility": "open" })
        .to_string()
        .into_bytes();
    let req = common::post(&owner, "/buzz/api/channels", body.clone());
    let first = env.handle(req.clone()).await.expect("first delivery");
    assert_eq!(first.status, 201, "create: {}", error_text(&first));

    // Past the freshness window: from here the token is stale.
    env.advance((auth::MAX_AUTH_SKEW_SECS + 5) * 1_000);

    // The genuine header, re-driven late, is still answered from the record.
    let late = env.handle(req.clone()).await.expect("late redelivery");
    assert_eq!(
        (late.status, &late.body),
        (first.status, &first.body),
        "a genuine late redelivery lost its recorded answer"
    );

    // The same header with its signature tampered: same id, wrong key.
    let header = req
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
        .map(|(_, value)| value.clone())
        .expect("the request is signed");
    let mut forged: ferrite_app::serde_json::Value = ferrite_app::serde_json::from_slice(
        &buzz::relay::b64::decode(header.trim_start_matches("Nostr ").trim()).expect("base64"),
    )
    .expect("json");
    let sig = forged["sig"].as_str().expect("sig").to_string();
    forged["sig"] = ferrite_app::serde_json::Value::String(format!(
        "{}{}",
        if sig.starts_with('a') { 'b' } else { 'a' },
        &sig[1..]
    ));
    let forged_header = format!(
        "Nostr {}",
        buzz::relay::b64::encode(&ferrite_app::serde_json::to_vec(&forged).expect("encodes"))
    );
    let stolen = env
        .handle(request_with_auth("POST", "/buzz/api/channels", Some(&forged_header), body))
        .await
        .expect("the app replied");
    assert_eq!(stolen.status, 401, "a forged stale token must be refused");
    assert_ne!(
        stolen.body, first.body,
        "a forged signature was served the victim's recorded answer"
    );
    });
}
