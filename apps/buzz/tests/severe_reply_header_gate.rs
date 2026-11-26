//! **Response headers, over a real fleet.**
//!
//! The relay's reply tier used to be a bare `(status, body)` pair and the api
//! loop published a JSON literal with `"headers": []` hard-coded, because
//! `IngressReply` once carried no header list. It carries one now, so the tier
//! threads headers from the handler that decides them to the envelope that
//! carries them, and this gate is what defends that end to end: the assertions
//! read `IngressReply::headers` off a reply that made a real round trip through
//! the request queue, not a return value from a function call.
//!
//! Three properties, each of which the header-less tier could not have:
//!
//! * every reply says what its body IS, so nothing downstream has to sniff;
//! * a 429 says how long to wait (RFC 6585 §4);
//! * every name and value this relay emits would survive the gateway's own
//!   reply-header boundary, which refuses the WHOLE response when one pair is
//!   malformed — a 401 that turns into a 502 is worse than the 401.
//!
//! The MCP `WWW-Authenticate` challenge and the ACP `Acp-Connection-Id` are
//! proven in `severe_mcp_gate.rs` and `severe_acp_gate.rs`, beside the protocol
//! behaviour they belong to.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use common::{Actor, body_json, gate_env, error_text, post};
use ferrite_app::serde_json::json;
use ferrite_app::{IngressReply, IngressRequest};
use ferrite_app_test::ingress;

/// The one header value a reply of this relay's may carry more than once is
/// none of them today, so a lookup returning the first match is exact.
fn header<'a>(reply: &'a IngressReply, name: &str) -> Option<&'a str> {
    reply
        .headers
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v.as_str())
}

/// The gateway's own reply-header boundary, applied to what this app emits.
///
/// A mirror of the platform gateway's `validate_reply_headers` at the point
/// the app can still do something about it. The gateway answers **502** and
/// discards the app's whole response when any pair fails, so a header this app
/// emits that would fail there is not a cosmetic defect: it destroys the answer.
/// The refused names are `is_forbidden_reply_header`'s list verbatim.
fn assert_gateway_would_serve(reply: &IngressReply, what: &str) {
    const FORBIDDEN: [&str; 10] = [
        "content-length",
        "connection",
        "transfer-encoding",
        "keep-alive",
        "upgrade",
        "proxy-connection",
        "te",
        "trailer",
        "x-correlation-id",
        "access-control-allow-origin",
    ];
    // `MAX_HEADERS`, and the 16 KiB budget `MAX_REPLY_HEADER_BYTES` bounds.
    assert!(
        reply.headers.len() <= 64,
        "{what}: {} headers passes the gateway's cap of 64",
        reply.headers.len()
    );
    let mut total = 0usize;
    for (name, value) in &reply.headers {
        let lower = name.to_ascii_lowercase();
        assert!(
            !lower.is_empty()
                && lower.bytes().all(|b| matches!(b,
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9'
                    | b'!' | b'#' | b'$' | b'%' | b'&' | b'\''
                    | b'*' | b'+' | b'-' | b'.' | b'^' | b'_'
                    | b'`' | b'|' | b'~')),
            "{what}: {name:?} is not an HTTP field name"
        );
        assert!(
            !FORBIDDEN.contains(&lower.as_str()),
            "{what}: the gateway refuses the whole reply over {name:?}"
        );
        assert!(
            !value.bytes().any(|b| b < 0x20 || b == 0x7f),
            "{what}: {name:?} carries a control byte, which 502s the reply"
        );
        total = total.saturating_add(lower.len() + value.len() + 4);
    }
    assert!(
        total <= 16 * 1024,
        "{what}: {total} header bytes passes the gateway's 16 KiB budget"
    );
}

/// **Every reply names its own media type, and no reply is header-less.**
///
/// Before the tier carried headers the gateway wrote no `content-type` at all —
/// it adds none of its own (`write_response` emits exactly what it is handed) —
/// so every one of these answers reached a client untyped and was sniffed.
#[test]
 fn severe_every_reply_declares_what_its_body_is() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let ana = Actor::new("header-ana");

    // The liveness probes answer `ok`, which is text and not JSON. Calling it
    // JSON would be the same defect in the other direction.
    for path in [
        "/buzz/api/healthz",
        "/health",
        "/_liveness",
        "/_readiness",
    ] {
        let reply = env.handle(common::get(path)).await.expect("answers");
        assert_eq!(reply.status, 200, "{path}");
        assert_eq!(
            header(&reply, "content-type"),
            Some("text/plain; charset=utf-8"),
            "{path} answers text, and must say so"
        );
        assert_gateway_would_serve(&reply, path);
    }

    // A JSON read, a JSON write, and a JSON refusal: the three shapes the tier
    // produces, one per construction site (`Reply::json` on an arm, on the
    // error tail, and on a handler that chose its own status).
    let relay_info = env
        .handle(common::get("/buzz/api/relay-info"))
        .await
        .expect("relay-info answers");
    assert_eq!(relay_info.status, 200);
    assert_eq!(
        header(&relay_info, "content-type"),
        Some("application/json")
    );
    assert_gateway_would_serve(&relay_info, "/buzz/api/relay-info");

    let created = env
        .handle(post(
            &ana,
            "/buzz/api/channels",
            json!({ "id": "hdr-room", "name": "hdr", "visibility": "open" })
                .to_string()
                .into_bytes(),
        ))
        .await
        .expect("create answers");
    assert_eq!(created.status, 201, "{}", error_text(&created));
    assert_eq!(header(&created, "content-type"), Some("application/json"));
    assert_gateway_would_serve(&created, "POST /buzz/api/channels");

    // The 404 fallthrough: an error body is JSON too, and a client that
    // branches on the reason token has to be able to parse it.
    let missing = env
        .handle(common::get("/buzz/api/there-is-no-such-route"))
        .await
        .expect("the fallthrough answers");
    assert_eq!(missing.status, 404);
    assert_eq!(header(&missing, "content-type"), Some("application/json"));
    assert!(
        header(&missing, "retry-after").is_none(),
        "only a 429 tells a caller to wait"
    );
    assert_gateway_would_serve(&missing, "the 404 fallthrough");

    });
}

/// **A 429 says how long to wait.**
///
/// RFC 6585 §4. Without the header a client's two natural guesses are
/// "immediately" — which re-trips the same window and makes the limiter's job
/// harder — and "never". The invite-claim window is the cheapest one to reach:
/// ten attempts per pubkey per minute.
#[test]
 fn severe_a_rate_limited_reply_carries_retry_after() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let prober = Actor::new("header-prober");
    // The claim route is fail-closed without the HMAC key, and a refusal that
    // never reaches the limiter would never produce the 429 under test.
    env.store("config")
        .put(b"buzz-invite-key", &[0x42u8; 32])
        .await
        .expect("seed the invite key");

    let mut limited = None;
    for attempt in 0..12u32 {
        let reply = env
            .handle(post(
                &prober,
                "/buzz/api/invites/claim",
                json!({ "code": "not-a-real-code" }).to_string().into_bytes(),
            ))
            .await
            .expect("claim answers");
        if reply.status == 429 {
            limited = Some((attempt, reply));
            break;
        }
        assert_eq!(reply.status, 403, "attempt {attempt} is a plain refusal");
        assert!(
            header(&reply, "retry-after").is_none(),
            "a 403 is not a wait instruction"
        );
    }
    let (attempt, reply) = limited.expect("ten claims per minute is the window");
    assert_eq!(attempt, 10, "the eleventh attempt trips the window");
    assert_eq!(
        header(&reply, "retry-after"),
        Some("60"),
        "a 429 names a wait no longer than the window that produced it"
    );
    assert_eq!(header(&reply, "content-type"), Some("application/json"));
    assert_gateway_would_serve(&reply, "the rate-limited claim");

    });
}

/// **A header value the gateway would refuse is dropped, not sent.**
///
/// `Reply::with_header` checks values because the gateway's answer to one bad
/// pair is to discard the ENTIRE response and serve 502. The resource
/// identifier an operator configures is the value that reaches a header without
/// passing through this app's own validation — the RFC 9728 challenge is built
/// around it — so a newline in that config row must cost the challenge header
/// and nothing else. The body-side carriage still delivers the value, which is
/// why keeping it for one release is worth what it costs.
#[test]
 fn severe_a_poisoned_challenge_costs_the_header_and_not_the_reply() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    let mut config = env.store("config");
    config
        .put(
            b"buzz-mcp-resource",
            b"https://gate.example/mcp\r\nx-injected: yes",
        )
        .await
        .expect("seed a poisoned resource identifier");
    config
        .put(b"buzz-mcp-key", b"a-gate-hmac-key-of-at-least-32-bytes")
        .await
        .expect("seed the hmac key");

    let refused = env
        .handle(unauthenticated_tools_list())
        .await
        .expect("the mcp endpoint answers");
    assert_eq!(refused.status, 401, "{}", error_text(&refused));
    assert!(
        header(&refused, "www-authenticate").is_none(),
        "a challenge carrying CRLF is dropped rather than handed to the gateway"
    );
    assert!(
        body_json(&refused)["www_authenticate"].is_string(),
        "and the body-side carriage still delivers it"
    );
    assert_gateway_would_serve(&refused, "the poisoned challenge");

    });
}

/// An MCP `tools/list` with no credential: the cheapest reply that carries a
/// challenge.
fn unauthenticated_tools_list() -> IngressRequest {
    IngressRequest {
        stream_open: false,
        correlation_id: String::new(),
        idempotency_key: String::new(),
        route: "/mcp".to_string(),
        path: "/mcp".to_string(),
        method: "POST".to_string(),
        headers: vec![
            (
                buzz::mcp::proto::HEADER_PROTOCOL_VERSION.to_string(),
                buzz::mcp::proto::PROTOCOL_VERSION.to_string(),
            ),
            (
                buzz::mcp::proto::HEADER_METHOD.to_string(),
                buzz::mcp::proto::M_TOOLS_LIST.to_string(),
            ),
        ],
        body: buzz::mcp::proto::request(
            &json!(1),
            buzz::mcp::proto::M_TOOLS_LIST,
            json!({}),
            Some(&buzz::mcp::proto::Identity::new("gate", "1")),
            &json!({}),
        )
        .to_string()
        .into_bytes(),
        deadline_ms: 0,
        reply_to: String::new(),
        published_ms: 0,
        ..IngressRequest::new("", "", Vec::new())
    }
}
