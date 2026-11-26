//! Plan 031 WP-S1: the upstream wire-compat alias routes.
//!
//! Stock `block/buzz` tooling — `buzz-cli`, `sprig`, the `buzz-acp` harness —
//! builds its paths at the relay ROOT (`/events`, `/query`, `/count`, `/info`)
//! and puts the ABSOLUTE URL in its NIP-98 `u` tag. This gate holds both
//! halves of that compatibility claim: the paths are served, and a header a
//! stock client would build is accepted.
//!
//! The sharpest test here is the last one. An alias must not be a second read
//! path with its own access rules — if it were, every gate in this suite would
//! be testing the wrong door — so a private room's message is asked for
//! through the alias by a non-member and must not come back.

use super::common;

use buzz::relay::channel::Visibility;
use buzz::relay::crypto::to_hex;
use buzz::relay::kind;
use common::{
    Actor, SUBMIT_ATTEMPTS, auth_header, gate_env, error_text, query, request_with_auth,
    seed_channel_with_owner, signed, stored_event, submit, tag,
};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::{IngressReply, IngressRequest};
use common::SsoTestEnv as TestEnv;

/// The relay origin a stock client would have been configured with. Only its
/// PATH component is compared by the binding check, which is the property this
/// gate exercises.
const UPSTREAM_ORIGIN: &str = "https://relay.example.test";

/// A per-request counter, because the gate's clock is FROZEN.
///
/// Two requests to one path with the same nonce and the same `created_at` build
/// the identical authorization event — identical id — and the relay's
/// single-use burn correctly refuses the second as a replay. That is the
/// relay being right and the test being wrong, and it is the same uniqueness
/// trap `buzz_process_app::buzzapi::NonceSource` exists to close for the cell.
static NONCE: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

fn next_nonce() -> String {
    format!(
        "wire-{}",
        NONCE.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
    )
}

/// An authorization header in the shape stock upstream tooling builds: the
/// `u` tag carries the absolute URL rather than the bare path.
fn upstream_auth(actor: &Actor, method: &str, path: &str, nonce: &str) -> String {
    let event = signed(
        actor,
        kind::KIND_HTTP_AUTH,
        vec![
            tag("u", &format!("{UPSTREAM_ORIGIN}{path}")),
            tag("method", method),
            tag("nonce", nonce),
        ],
        "",
    );
    auth_header(&event)
}

/// Drive one aliased request, re-sending on a lost reply (the frozen-clock
/// discipline `common::query` documents).
async fn aliased(
    env: &TestEnv,
    actor: &Actor,
    method: &str,
    path: &str,
    body: Vec<u8>,
) -> IngressReply {
    for attempt in 1..=SUBMIT_ATTEMPTS {
        let header = upstream_auth(actor, method, path, &next_nonce());
        let mut request: IngressRequest = request_with_auth(method, path, Some(&header), body.clone());
        // Stock tooling signs the relay's own URL, so the gateway routes it on
        // that host.
        request.verified_host = Some("relay.example.test".into());
        match env.handle(request).await {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => {
                panic!("no reply for {method} {path} after {SUBMIT_ATTEMPTS}: {e}")
            }
            Err(_) => continue,
        }
    }
    unreachable!("the loop either returns a reply or panics")
}

/// Drive one UNAUTHENTICATED aliased request, with the same re-send discipline.
async fn public(env: &TestEnv, method: &str, path: &str, body: Vec<u8>) -> IngressReply {
    for attempt in 1..=SUBMIT_ATTEMPTS {
        let request = request_with_auth(method, path, None, body.clone());
        match env.handle(request).await {
            Ok(reply) => return reply,
            Err(e) if attempt == SUBMIT_ATTEMPTS => {
                panic!("no reply for {method} {path} after {SUBMIT_ATTEMPTS}: {e}")
            }
            Err(_) => continue,
        }
    }
    unreachable!("the loop either returns a reply or panics")
}

/// **The aliased ingest and read paths work, with an absolute-URL header.**
#[test]
 fn severe_stock_upstream_paths_serve_a_publish_and_a_read() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("wire-ana");
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;

    // POST /events — the upstream ingest path.
    let message = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "general")],
        "posted through the upstream alias",
    );
    let body = serde_json::to_vec(&message).expect("encode");
    let reply = aliased(&env, &ana, "POST", "/events", body).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    assert!(
        stored_event(&env, &message.id).await.is_some(),
        "the aliased ingest must reach the same storage the /buzz/api path does"
    );

    // POST /query — a NIP-01 filter ARRAY in, an event ARRAY out.
    let filters = json!([{ "kinds": [kind::KIND_STREAM_MESSAGE], "#h": ["general"], "limit": 10 }]);
    let reply = aliased(
        &env,
        &ana,
        "POST",
        "/query",
        serde_json::to_vec(&filters).expect("encode"),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let events: Value = serde_json::from_slice(&reply.body).expect("array reply");
    let rows = events
        .as_array()
        .expect("the upstream shape is a bare array");
    assert!(
        rows.iter()
            .any(|e| e.get("id").and_then(Value::as_str) == Some(to_hex(&message.id).as_str())),
        "the message must come back through the alias: {events}"
    );

    // POST /count — NIP-45.
    let reply = aliased(
        &env,
        &ana,
        "POST",
        "/count",
        serde_json::to_vec(&filters).expect("encode"),
    )
    .await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let counted: Value = serde_json::from_slice(&reply.body).expect("count reply");
    assert_eq!(
        counted.get("count").and_then(Value::as_u64),
        Some(rows.len() as u64),
        "count and query must agree: {counted}"
    );

    });
}

/// **The unauthenticated aliases answer**, so a stock client can probe a
/// deployment before it has an identity.
#[test]
 fn severe_the_unauthenticated_aliases_answer() {
    common::run_scenario(|| async move {
    let env = gate_env().await;

    let info = public(&env, "GET", "/info", Vec::new()).await;
    assert_eq!(info.status, 200);
    let doc: Value = serde_json::from_slice(&info.body).expect("NIP-11 JSON");
    assert_eq!(doc.get("software").and_then(Value::as_str), Some("buzz"));
    assert!(
        doc.get("supported_nips").is_some(),
        "NIP-11 must name the NIPs a client branches on: {doc}"
    );

    // The relay-served client computes its NIP-11 URL from its WebSocket URL,
    // `wss://host/buzz/`, so it asks for `/buzz/info`. Both spellings must
    // answer the same document, byte for byte.
    let served = public(&env, "GET", "/buzz/info", Vec::new()).await;
    assert_eq!(served.status, 200, "{}", error_text(&served));
    assert_eq!(
        served.body, info.body,
        "`/buzz/info` and `/info` must serve the identical NIP-11 document"
    );

    for path in ["/health", "/_liveness", "/_readiness"] {
        let reply = public(&env, "GET", path, Vec::new()).await;
        assert_eq!(reply.status, 200, "{path}");
        assert_eq!(reply.body, b"ok".to_vec(), "{path}");
    }

    });
}

/// **A malformed or oversized filter set is refused by NAME**, not answered
/// with an empty array a client would read as "nothing was said".
#[test]
 fn severe_the_alias_refuses_a_bad_filter_set_by_name() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("wire-ana");

    let reply = aliased(&env, &ana, "POST", "/query", b"not json".to_vec()).await;
    assert_eq!(reply.status, 400, "{}", error_text(&reply));
    assert!(
        error_text(&reply).contains("filters"),
        "the refusal should name what was wrong: {}",
        error_text(&reply)
    );

    // The advertised NIP-11 filter ceiling is the one enforced here, so a
    // client obeying the document is never refused for a different bound.
    let too_many: Vec<Value> = (0..=buzz::relay::ws::MAX_FILTERS_PER_REQ)
        .map(|i| json!({ "kinds": [9], "limit": 1, "since": i }))
        .collect();
    let reply = aliased(
        &env,
        &ana,
        "POST",
        "/query",
        serde_json::to_vec(&too_many).expect("encode"),
    )
    .await;
    assert_eq!(reply.status, 400, "{}", error_text(&reply));

    // An EMPTY array is a legitimate request for nothing.
    let reply = aliased(&env, &ana, "POST", "/query", b"[]".to_vec()).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    assert_eq!(reply.body, b"[]".to_vec());

    });
}

/// **An alias is not a second access model.**
///
/// The whole risk of adding a compatibility route: if it read events by a
/// different rule, every access gate in this suite would be guarding a door
/// nobody uses. A private room's message must be as invisible here as it is
/// through the socket and the native route.
#[test]
 fn severe_an_alias_never_serves_what_a_req_would_refuse() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("wire-ana");
    let outsider = Actor::new("wire-outsider");
    seed_channel_with_owner(&env, "private-room", Visibility::Private, &ana).await;

    let secret = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "private-room")],
        "members only",
    );
    assert_eq!(submit(&env, &ana, &secret).await.status, 200);

    let filters = json!([{ "kinds": [kind::KIND_STREAM_MESSAGE], "#h": ["private-room"] }]);
    let body = serde_json::to_vec(&filters).expect("encode");

    // The member sees it.
    let reply = aliased(&env, &ana, "POST", "/query", body.clone()).await;
    let rows: Value = serde_json::from_slice(&reply.body).expect("array");
    assert!(
        rows.as_array().is_some_and(|r| !r.is_empty()),
        "a member must still read their own private room through the alias"
    );

    // The outsider does not — and the COUNT does not leak it either.
    let reply = aliased(&env, &outsider, "POST", "/query", body.clone()).await;
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let rows: Value = serde_json::from_slice(&reply.body).expect("array");
    assert_eq!(
        rows.as_array().map(Vec::len),
        Some(0),
        "the alias must not serve a private room to a non-member: {rows}"
    );
    let reply = aliased(&env, &outsider, "POST", "/count", body).await;
    let counted: Value = serde_json::from_slice(&reply.body).expect("count");
    assert_eq!(
        counted.get("count").and_then(Value::as_u64),
        Some(0),
        "a count that included it would leak its existence: {counted}"
    );

    });
}

/// **Author-only filters are refused before storage is scanned.**
///
/// Per-event delivery still protects the bytes, but answering `[]` to an
/// unauthorized author-only filter makes a permission refusal look like empty
/// state and leaves a broad-scan work primitive on the HTTP edge. The adapter
/// promises the socket face's filter semantics, so it must reject the question
/// itself just as `REQ` does.
#[test]
 fn severe_http_filter_query_refuses_foreign_author_only_scans() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let owner = Actor::new("reminder-owner");
    let stranger = Actor::new("reminder-stranger");
    let reminder = signed(
        &owner,
        kind::KIND_EVENT_REMINDER,
        vec![tag("d", "reminder-1"), tag("not_before", "4102444800")],
        "opaque ciphertext",
    );
    assert_eq!(submit(&env, &owner, &reminder).await.status, 200);

    let mine = json!([{
        "kinds": [kind::KIND_EVENT_REMINDER],
        "authors": [to_hex(&owner.pk)],
        "limit": 50
    }]);
    let mine = aliased(
        &env,
        &owner,
        "POST",
        "/buzz/api/filter-query",
        serde_json::to_vec(&mine).expect("encode"),
    )
    .await;
    assert_eq!(mine.status, 200, "{}", error_text(&mine));
    let rows: Value = serde_json::from_slice(&mine.body).expect("array");
    assert!(
        rows.as_array()
            .is_some_and(|events| events.iter().any(|event| {
                event.get("id").and_then(Value::as_str) == Some(to_hex(&reminder.id).as_str())
            })),
        "the author must read their own reminder: {rows}"
    );

    let foreign = aliased(
        &env,
        &stranger,
        "POST",
        "/buzz/api/filter-query",
        serde_json::to_vec(&json!([{
            "kinds": [kind::KIND_EVENT_REMINDER],
            "authors": [to_hex(&owner.pk)],
            "limit": 50
        }]))
        .expect("encode"),
    )
    .await;
    assert_eq!(foreign.status, 403, "{}", error_text(&foreign));
    assert!(
        error_text(&foreign).contains("this filter asks for events you may not read"),
        "the refusal must name the filter boundary: {}",
        error_text(&foreign)
    );

    let unscoped = aliased(
        &env,
        &stranger,
        "POST",
        "/buzz/api/filter-query",
        serde_json::to_vec(&json!([{
            "kinds": [kind::KIND_EVENT_REMINDER],
            "limit": 50
        }]))
        .expect("encode"),
    )
    .await;
    assert_eq!(unscoped.status, 403, "{}", error_text(&unscoped));

    let anonymous = public(
        &env,
        "POST",
        "/buzz/api/filter-query",
        serde_json::to_vec(&json!([{
            "kinds": [kind::KIND_EVENT_REMINDER],
            "authors": [to_hex(&owner.pk)],
            "limit": 50
        }]))
        .expect("encode"),
    )
    .await;
    assert_eq!(anonymous.status, 401, "{}", error_text(&anonymous));

    });
}

/// The app's route-fallthrough refusal. A served row must never produce it,
/// and a declined row must produce exactly it — which is a sharper test than a
/// bare 404, because a route that RAN and found no such channel also answers
/// 404 and would otherwise read as "no route".
fn is_route_fallthrough(reply: &IngressReply) -> bool {
    reply.status == 404 && error_text(reply).contains("no route for")
}

/// **Every row of the artifact is DRIVEN**, in both directions.
///
/// A metadata check would let an edit to the file pass while no route was
/// exercised, and — worse — would let an accidentally exposed `/upload` or
/// `/hooks/{id}` sit there unnoticed while the artifact claimed it was
/// declined. So each served row must be ANSWERED and each declined row must
/// 404, which is the only reading of "this is our wire surface" with teeth.
#[test]
 fn severe_every_artifact_path_is_driven_in_both_directions() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("wire-ana");
    // The `/events` probe posts a real message, so the room it names must
    // exist: otherwise the app answers 404 for a reason that has nothing to do
    // with whether the route is served.
    seed_channel_with_owner(&env, "general", Visibility::Open, &ana).await;

    let raw = include_str!("fixtures/upstream-wire-surface.json");
    let doc: Value = serde_json::from_str(raw).expect("the artifact must be JSON");
    let paths = doc
        .get("paths")
        .and_then(Value::as_array)
        .expect("paths array");
    assert!(paths.len() >= 10, "the enumeration looks truncated");
    assert_eq!(
        doc.get("upstream_pin").and_then(Value::as_str),
        Some("4cd82f513214aad11c2b742ce7cc7c681e8e32a0"),
        "the artifact must name the pin it was enumerated from"
    );

    let mut served = 0usize;
    let mut declined = 0usize;
    for row in paths {
        let path = row.get("path").and_then(Value::as_str).expect("path");
        let method = row.get("method").and_then(Value::as_str).expect("method");
        let is_served = row
            .get("served")
            .and_then(Value::as_bool)
            .unwrap_or_else(|| panic!("{path} does not say whether it is served"));
        // Templated rows are driven with a concrete representative value — a
        // template string would test a path no client ever sends.
        let concrete = path
            .replace("{owner}", "wire-owner")
            .replace("{repo}", "wire-repository")
            .replace("{sha256_ext}", &format!("{}.png", "ab".repeat(32)))
            .replace("{id}", "some-hook-id");
        // A body the row's shape accepts, so a served row's answer is about the
        // ROUTE existing rather than about a malformed request.
        let body = match (method, row.get("request").and_then(Value::as_str)) {
            ("POST", Some(request)) if request.contains("filters") => b"[]".to_vec(),
            ("POST", _) => serde_json::to_vec(&signed(
                &ana,
                kind::KIND_STREAM_MESSAGE,
                vec![tag("h", "general")],
                "artifact probe",
            ))
            .expect("encode"),
            _ => Vec::new(),
        };
        let needs_auth = row.get("auth").and_then(Value::as_str) != Some("none");
        let reply = if needs_auth {
            aliased(&env, &ana, method, &concrete, body).await
        } else {
            public(&env, method, &concrete, body).await
        };

        if is_served {
            served += 1;
            assert!(
                row.get("handler").and_then(Value::as_str).is_some(),
                "{path} claims to be served but names no handler"
            );
            assert!(
                !is_route_fallthrough(&reply),
                "{method} {concrete} is claimed served but the app has no route: {}",
                error_text(&reply)
            );
            assert_ne!(
                reply.status, 405,
                "{method} {concrete} is claimed served but the method is not accepted"
            );
        } else {
            declined += 1;
            // A declined face must say WHY — an unexplained omission reads as an
            // oversight, and the next reader cannot tell the difference.
            let reason = row
                .get("reason")
                .and_then(Value::as_str)
                .unwrap_or_else(|| panic!("{path} is declined with no reason"));
            assert!(reason.len() > 40, "{path}'s reason is too thin: {reason}");
            assert!(
                is_route_fallthrough(&reply),
                "{method} {concrete} is claimed DECLINED but the app answered it \
                 — either the artifact is stale or a second door opened: {} {}",
                reply.status,
                error_text(&reply)
            );
        }
    }
    assert_eq!(served, 7, "the served alias set changed size");
    assert_eq!(declined, 6, "the declined set changed size");

    });
}
