//! **S16 — authorization abuse.** The NIP-98 request-authentication path, driven
//! through the routes with headers a real attacker would send.
//!
//! There is no socket to authenticate once here: every request carries its own
//! signed authorization event, so the three defences (binding to method+path,
//! freshness, single use) are the whole perimeter. Each is attacked below, and each
//! refusal is checked against observable state — the status, the wire reason, and
//! whether the single-use marker was burned. The final case is the one that catches
//! a whole class of bug: after every hostile header, the app must still be serving.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use buzz::relay::b64;
use buzz::relay::event::Event;
use buzz::relay::kind;
use common::{
    Actor, COMMUNITY, EVENTS_PATH, QUERY_PATH, SUBSCRIPTIONS_PATH, T0_SECS, admission,
    auth_event, auth_header, body_json, gate_env, error_text, fresh_auth, query, replay_marker,
    request_with_auth, returned, signed, stored_event_count, submit,
};
use ferrite_app::serde_json::{self, json};
use ferrite_app_test::ingress;

/// One authorization event authorizes exactly one ACTION. A second delivery of
/// the same header re-runs nothing: it is answered with what the first delivery
/// answered, and the marker that decides this is durable.
///
/// This used to assert a 401 on the replay. That was wrong in the field, not
/// merely strict: the request queue is at-least-once (a reply-publish failure
/// deliberately leaves the envelope unacked so the work is re-driven,
/// `ingress::api_loop`), so the "replay" is routinely the platform redelivering
/// a request that ALREADY SUCCEEDED — and answering it `already been used` told
/// users their completed action had failed. At-most-once execution is what
/// NIP-98 single use is for, and that is what is pinned below.
#[test]
 fn severe_a_replayed_authorization_header_is_answered_not_re_executed() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let (auth_event, header) = fresh_auth(&alice, "POST", QUERY_PATH);

    assert!(
        replay_marker(&env, &auth_event.id).await.is_none(),
        "nothing is burned before the first use"
    );

    let body = serde_json::to_vec(&json!({})).unwrap();
    let first = env
        .handle(request_with_auth(
            "POST",
            QUERY_PATH,
            Some(&header),
            body.clone(),
        ))
        .await
        .expect("a reply");
    assert_eq!(
        first.status,
        200,
        "{}",
        String::from_utf8_lossy(&first.body)
    );
    assert!(
        replay_marker(&env, &auth_event.id).await.is_some(),
        "a used authorization event must be burned"
    );

    // The identical header, replayed (a queue re-drive, a gateway hedge, a
    // captured request). This path is a READ, and reads are deliberately not
    // recorded — the record costs a whole new store client plus a write on the
    // busiest routes. So the guaranteed property here is the honest one: the
    // duplicate is told to RETRY and is never told its request "has already been
    // used". Being answered the first reply verbatim is pinned where it actually
    // matters, on a write, by
    // `severe_auth_idempotency_gate::severe_a_redelivered_write_is_answered_with_its_first_result`.
    for attempt in 0..3 {
        let again = env
            .handle(request_with_auth(
                "POST",
                QUERY_PATH,
                Some(&header),
                body.clone(),
            ))
            .await
            .expect("a reply");
        assert!(
            !error_text(&again).contains(buzz::relay::auth::REPLAYED_MESSAGE),
            "replay {attempt} was refused as a replay: {}",
            error_text(&again)
        );
        assert_eq!(
            again.status,
            503,
            "replay {attempt} must be retryable, got {}: {}",
            again.status,
            error_text(&again)
        );
    }
    // The burn is still there — the guard was consulted, not bypassed.
    assert!(
        replay_marker(&env, &auth_event.id).await.is_some(),
        "the replay record must survive being served"
    );

    // A fresh header for the same caller and path still works: the guard is per
    // authorization event, not a lockout.
    let page = query(&env, &alice, &json!({})).await;
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));

    });
}

/// A header bound to `/buzz/api/query` does not authorize `/buzz/api/events` —
/// and the refusal does NOT burn the token, so the client's correctly-bound retry
/// still works.
#[test]
 fn severe_a_header_bound_to_query_does_not_authorize_events() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], "smuggled");
    let (auth_event, header) = fresh_auth(&alice, "POST", QUERY_PATH);

    let refused = env
        .handle(request_with_auth(
            "POST",
            EVENTS_PATH,
            Some(&header),
            serde_json::to_vec(&event).unwrap(),
        ))
        .await
        .expect("a reply");
    assert_eq!(
        refused.status,
        401,
        "{}",
        String::from_utf8_lossy(&refused.body)
    );
    assert_eq!(
        error_text(&refused),
        "restricted: authorization event is for a different path"
    );
    // Nothing was written, and the token was not spent on the refusal.
    assert_eq!(stored_event_count(&env).await, 0);
    assert!(admission(&env, &event.id).await.is_none());
    assert!(
        replay_marker(&env, &auth_event.id).await.is_none(),
        "a refused request must not burn the caller's token"
    );

    // The same token, used where it was bound, is accepted.
    let page = env
        .handle(request_with_auth(
            "POST",
            QUERY_PATH,
            Some(&header),
            serde_json::to_vec(&json!({})).unwrap(),
        ))
        .await
        .expect("a reply");
    assert_eq!(page.status, 200, "{}", String::from_utf8_lossy(&page.body));

    // A path that merely EXTENDS the signed one is refused too (no prefix match).
    let (_, header) = fresh_auth(&alice, "POST", QUERY_PATH);
    let nested = env
        .handle(request_with_auth(
            "POST",
            "/buzz/api/query/../events",
            Some(&header),
            serde_json::to_vec(&json!({})).unwrap(),
        ))
        .await
        .expect("a reply");
    assert!(
        nested.status == 401 || nested.status == 404,
        "a nested path was authorized by a parent token: {}",
        String::from_utf8_lossy(&nested.body)
    );

    // And a header signed for GET does not authorize a POST.
    let (_, get_header) = fresh_auth(&alice, "GET", QUERY_PATH);
    let wrong_method = env
        .handle(request_with_auth(
            "POST",
            QUERY_PATH,
            Some(&get_header),
            serde_json::to_vec(&json!({})).unwrap(),
        ))
        .await
        .expect("a reply");
    assert_eq!(wrong_method.status, 401);
    assert_eq!(
        error_text(&wrong_method),
        "restricted: authorization event is for a different method"
    );

    });
}

/// The freshness window is ±60 seconds, exactly.
#[test]
 fn severe_an_expired_authorization_header_is_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let body = serde_json::to_vec(&json!({})).unwrap();

    let expect_refused = |secs_offset: i64| {
        let created_at = u64::try_from(i64::try_from(T0_SECS).unwrap() + secs_offset).unwrap();
        auth_event(&alice, "POST", QUERY_PATH, created_at, "stale")
    };

    // 61 seconds old.
    let stale = expect_refused(-61);
    let reply = env
        .handle(request_with_auth(
            "POST",
            QUERY_PATH,
            Some(&auth_header(&stale)),
            body.clone(),
        ))
        .await
        .expect("a reply");
    assert_eq!(
        reply.status,
        401,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert_eq!(
        error_text(&reply),
        "restricted: authorization event is 61s from now"
    );
    assert!(
        replay_marker(&env, &stale.id).await.is_none(),
        "an expired token must not be burned"
    );

    // 61 seconds in the FUTURE is equally refused (a clock-ahead client cannot
    // pre-mint tokens).
    let ahead = auth_event(&alice, "POST", QUERY_PATH, T0_SECS + 61, "ahead");
    let reply = env
        .handle(request_with_auth(
            "POST",
            QUERY_PATH,
            Some(&auth_header(&ahead)),
            body.clone(),
        ))
        .await
        .expect("a reply");
    assert_eq!(reply.status, 401);
    assert_eq!(
        error_text(&reply),
        "restricted: authorization event is 61s from now"
    );

    // The boundary second itself is inside the window, in both directions.
    for (offset, nonce) in [(-60i64, "edge-past"), (60, "edge-future")] {
        let created_at = u64::try_from(i64::try_from(T0_SECS).unwrap() + offset).unwrap();
        let edge = auth_event(&alice, "POST", QUERY_PATH, created_at, nonce);
        let reply = env
            .handle(request_with_auth(
                "POST",
                QUERY_PATH,
                Some(&auth_header(&edge)),
                body.clone(),
            ))
            .await
            .expect("a reply");
        assert_eq!(
            reply.status,
            200,
            "the {offset}s boundary was refused: {}",
            String::from_utf8_lossy(&reply.body)
        );
    }

    });
}

/// A tampered authorization event is refused: a flipped signature byte, a
/// rewritten `u` tag with the original id, and a wrong-kind event.
#[test]
 fn severe_a_tampered_authorization_event_is_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let body = serde_json::to_vec(&json!({})).unwrap();

    // 1. Flipped signature byte.
    let mut forged = auth_event(&alice, "POST", QUERY_PATH, T0_SECS, "forged");
    forged.sig[0] ^= 0x01;
    let reply = env
        .handle(request_with_auth(
            "POST",
            QUERY_PATH,
            Some(&auth_header(&forged)),
            body.clone(),
        ))
        .await
        .expect("a reply");
    assert_eq!(
        reply.status,
        401,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(
        error_text(&reply).contains("signature verification failed"),
        "unexpected refusal: {}",
        error_text(&reply)
    );
    assert!(replay_marker(&env, &forged.id).await.is_none());

    // 2. The `u` tag rewritten to this path, keeping the id and signature that
    //    were computed over the ORIGINAL tags. The id check catches it.
    let original = auth_event(&alice, "POST", "/buzz/api/harmless", T0_SECS, "rewritten");
    let mut rewritten = original.clone();
    rewritten.tags = vec![
        vec!["u".into(), format!("https://buzz.test{QUERY_PATH}")],
        vec!["method".into(), "POST".into()],
        vec!["nonce".into(), "rewritten".into()],
    ];
    let reply = env
        .handle(request_with_auth(
            "POST",
            QUERY_PATH,
            Some(&auth_header(&rewritten)),
            body.clone(),
        ))
        .await
        .expect("a reply");
    assert_eq!(
        reply.status,
        401,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(
        error_text(&reply).contains("does not match its canonical hash"),
        "unexpected refusal: {}",
        error_text(&reply)
    );

    // 3. A validly signed event of the WRONG kind is not an authorization.
    let wrong_kind = Event::sign_new(
        &alice.sk,
        kind::KIND_TEXT_NOTE,
        vec![
            vec!["u".into(), format!("https://buzz.test{QUERY_PATH}")],
            vec!["method".into(), "POST".into()],
        ],
        "",
        T0_SECS,
    )
    .unwrap();
    let reply = env
        .handle(request_with_auth(
            "POST",
            QUERY_PATH,
            Some(&auth_header(&wrong_kind)),
            body.clone(),
        ))
        .await
        .expect("a reply");
    assert_eq!(reply.status, 401);
    assert!(error_text(&reply).contains("must be kind 27235"));

    // Nothing got through any of it.
    assert_eq!(stored_event_count(&env).await, 0);

    });
}

/// Every guarded route refuses a missing header; the two unguarded ones answer
/// without one.
#[test]
 fn severe_a_missing_authorization_header_is_refused_on_every_guarded_route() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], "unauthenticated");

    for (path, body) in [
        (EVENTS_PATH, serde_json::to_vec(&event).unwrap()),
        (QUERY_PATH, serde_json::to_vec(&json!({})).unwrap()),
        (SUBSCRIPTIONS_PATH, Vec::new()),
    ] {
        let reply = env
            .handle(request_with_auth("POST", path, None, body))
            .await
            .expect("a reply");
        assert_eq!(
            reply.status,
            401,
            "{path} served an unauthenticated caller: {}",
            String::from_utf8_lossy(&reply.body)
        );
        assert_eq!(
            error_text(&reply),
            "restricted: this route requires an Authorization header"
        );
    }
    assert_eq!(stored_event_count(&env).await, 0);

    // Health and metrics stay open — a liveness probe holds no key and this app
    // cannot bind a private port for one. What CHANGED (adversarial review
    // finding 10): the metrics payload is tiered, so the open document is a
    // liveness core and the operational detail needs an operator token. See
    // `ingress::metrics` for why each field sits where it does; the whole
    // operator tier is pinned by `severe_operator_metrics_gate.rs`.
    let health = env
        .handle(common::get("/buzz/api/healthz"))
        .await
        .expect("a reply");
    assert_eq!(health.status, 200);
    assert_eq!(health.body, b"ok");
    let metrics = env
        .handle(common::get("/buzz/api/metrics"))
        .await
        .expect("a reply");
    assert_eq!(
        metrics.status,
        200,
        "{}",
        String::from_utf8_lossy(&metrics.body)
    );
    let m = body_json(&metrics);
    assert_eq!(m["ok"], json!(true));
    assert_eq!(m["community"], json!(COMMUNITY));
    assert_eq!(
        m["scope"],
        json!("public"),
        "an unauthenticated read is the public tier"
    );

    // An unknown path under the route prefix is a 404, not a panic.
    let missing = env
        .handle(common::get("/buzz/api/nope"))
        .await
        .expect("a reply");
    assert_eq!(missing.status, 404);

    });
}

/// A table of hostile headers, none of which may be accepted — and after all of
/// them the app is still serving, which is the only way to prove the loop did not
/// die on one.
#[test]
 fn severe_hostile_headers_are_refused_and_never_wedge_the_app() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let body = serde_json::to_vec(&json!({})).unwrap();

    // An authorization event missing the tags the binding needs.
    let no_method = Event::sign_new(
        &alice.sk,
        kind::KIND_HTTP_AUTH,
        vec![vec!["u".into(), format!("https://buzz.test{QUERY_PATH}")]],
        "",
        T0_SECS,
    )
    .unwrap();
    let no_url = Event::sign_new(
        &alice.sk,
        kind::KIND_HTTP_AUTH,
        vec![vec!["method".into(), "POST".into()]],
        "",
        T0_SECS,
    )
    .unwrap();

    let hostile: Vec<String> = vec![
        String::new(),
        " ".into(),
        "Bearer abc".into(),
        "Nostr".into(),
        "Nostr ".into(),
        "Nostr !!!!".into(),
        // Valid base64, not JSON.
        format!("Nostr {}", b64::encode(b"not json at all")),
        // Valid JSON, not an event.
        format!("Nostr {}", b64::encode(b"{\"hello\":\"world\"}")),
        // An event-shaped object with junk hex fields.
        format!(
            "Nostr {}",
            b64::encode(
                b"{\"id\":\"zz\",\"pubkey\":\"zz\",\"created_at\":0,\"kind\":27235,\"tags\":[],\"content\":\"\",\"sig\":\"zz\"}"
            )
        ),
        // Base64 whose padding is wrong (the strict decoder must refuse it).
        "Nostr YWJj=".into(),
        // Whitespace inside the payload.
        "Nostr YW Jj".into(),
        // A very long payload (bounded refusal, no allocation blowup).
        format!("Nostr {}", b64::encode(&vec![b'A'; 200_000])),
        // Well-formed events that fail a binding check.
        auth_header(&no_method),
        auth_header(&no_url),
        // The scheme lowercase but the payload junk.
        format!("nostr {}", b64::encode(b"\x00\x01\x02")),
    ];

    for header in &hostile {
        let reply = env
            .handle(request_with_auth(
                "POST",
                QUERY_PATH,
                Some(header),
                body.clone(),
            ))
            .await
            .expect("the app replied");
        assert_eq!(
            reply.status,
            401,
            "header {:?} was not refused: {}",
            header.chars().take(40).collect::<String>(),
            String::from_utf8_lossy(&reply.body)
        );
        assert!(
            error_text(&reply).starts_with("restricted: "),
            "unexpected reason for {:?}: {}",
            header.chars().take(40).collect::<String>(),
            error_text(&reply)
        );
    }

    // Still alive, still correct.
    let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], "after the storm");
    assert_eq!(submit(&env, &alice, &event).await.status, 200);
    assert!(returned(&query(&env, &alice, &json!({})).await, &event));

    });
}

/// The single-use guard runs BEFORE the body is looked at, so a request with a
/// good header and a bad body spends the token. That ordering is deliberate — a
/// replay must not be able to probe by sending garbage — and it is pinned here so
/// a refactor cannot quietly move the body parse first.
#[test]
 fn severe_a_rejected_body_still_spends_its_authorization() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let alice = Actor::new("alice");
    let (auth_event, header) = fresh_auth(&alice, "POST", EVENTS_PATH);

    let reply = env
        .handle(request_with_auth(
            "POST",
            EVENTS_PATH,
            Some(&header),
            b"{ not an event".to_vec(),
        ))
        .await
        .expect("a reply");
    assert_eq!(
        reply.status,
        400,
        "{}",
        String::from_utf8_lossy(&reply.body)
    );
    assert!(error_text(&reply).starts_with("invalid: event body"));
    assert!(
        replay_marker(&env, &auth_event.id).await.is_some(),
        "the token was checked but not burned"
    );

    // The same header cannot now be retried with a GOOD body. It is answered
    // with the ORIGINAL rejection, so a captured token cannot be re-aimed at a
    // different payload — the stronger statement, and the one that matters:
    // swapping the body changes nothing about what the relay does.
    let event = signed(&alice, kind::KIND_TEXT_NOTE, vec![], "second try");
    let retry = env
        .handle(request_with_auth(
            "POST",
            EVENTS_PATH,
            Some(&header),
            serde_json::to_vec(&event).unwrap(),
        ))
        .await
        .expect("a reply");
    assert_eq!(
        (retry.status, &retry.body),
        (reply.status, &reply.body),
        "a spent token must answer with its original result, whatever body rides with it"
    );
    assert!(error_text(&retry).starts_with("invalid: event body"));
    // The decisive assertion, unchanged: the good body never took effect.
    assert_eq!(stored_event_count(&env).await, 0);

    // With a fresh header it lands.
    assert_eq!(submit(&env, &alice, &event).await.status, 200);
    assert_eq!(stored_event_count(&env).await, 1);

    });
}
