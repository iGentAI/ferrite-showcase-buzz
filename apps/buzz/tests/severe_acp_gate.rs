//! **The ACP v1 agent surface, over a real fleet.**
//!
//! The `packages/acp` unit tests cover the wire model and the v1 vocabulary in
//! isolation. What only a gate can show is that `ingress::route` reaches the
//! surface at all, and that the properties which are ABOUT DURABLE STATE hold end
//! to end — connection ownership, session ownership, and prompt idempotency.
//!
//! # The connection id, both ways
//!
//! ACP's HTTP binding names the connection in an `Acp-Connection-Id` RESPONSE
//! header. The surface could not set one while the reply tier carried no
//! headers, so the id travelled in `_meta`; it now travels in both, and
//! [`initialize`] asserts on every call that the two agree.
//!
//! # Why the model being unreachable is not a gap
//!
//! This harness binds no provider egress, so a turn fails at its first model step
//! and answers with the failure text. Every property under test is unaffected: the
//! session is still looked up, the ownership check still runs, the admission still
//! fires, the answer is still stored and still comes back as protocol content. What
//! the gate deliberately does NOT assert is anything about what a model said — the
//! agent gates own that, with a scripted transport.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    clippy::panic,
    clippy::indexing_slicing
)]

use super::common;

use std::time::Duration;

use buzz::acp::proto;
use buzz::acp::schema::PROTOCOL_VERSION_V1;
use buzz::acp::transport::HEADER_CONNECTION_ID;
use buzz::relay::acp_surface::{
    ACP_CONNECT_RATE_CLASS, ACP_CONNECT_RATE_LIMIT, ACP_PROMPT_RATE_CLASS, ACP_PROMPT_RATE_LIMIT,
    IDLE_TTL_MS,
};
use common::SsoTestEnv as TestEnv;
use common::{
    Actor, REPLY_BUDGET, T0_MS, auth_event, auth_header, body_json, error_text, post,
    request_with_auth,
};
use ferrite_app::serde_json::{json, Value};
use ferrite_app::IngressRequest;
use ferrite_app_test::TestHarness;

/// The resource this fixture publishes under. ACP reuses the MCP identity, so this
/// is the audience its bearer is minted for.
const RESOURCE: &str = "https://gate.example/mcp";

/// The relay plus the agent planes, so a prompt can reach the turn engine.
async fn acp_env() -> TestEnv {
    let env = TestEnv(
        TestHarness::new().with_store("nonces")
            .with_queue("requests")
            .with_queue("replies")
            .with_queue("stream-requests")
            .with_queue("agent-turns")
            .with_store("events")
            .with_store("config")
            .with_store("agent-workspace-index")
            .with_store("agent-memory")
            .with_store("gateway-config")
            .with_ape("guards")
            .with_ape("agent-sessions")
            .with_topics("feed")
            .with_blob("media")
            .with_blob("web-assets")
            .with_blob("agent-workspace-content")
            .request_reply("requests", "replies")
            .stream_requests("stream-requests")
            .manual_clock(T0_MS)
            .timeout(REPLY_BUDGET)
            .build()
            .await,
    );
    env.run_app(|ctx| async move { buzz::relay::ingress::serve(ctx).await });
    for _ in 0..40 {
        match env
            .handle(common::get("/buzz/api/healthz"))
            .await
        {
            Ok(reply) if reply.status == 200 => return env,
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    panic!("the app did not become healthy");
}

/// Seed the identity ACP authenticates against.
///
/// Through the config FALLBACK: `ferrite-app-test` injects an empty secrets map, so
/// this is the only seam a gate has — the same reason the MCP and operator gates use
/// it.
async fn seed_identity(env: &TestEnv) {
    let mut config = env.store("config");
    config
        .put(b"buzz-mcp-resource", RESOURCE.as_bytes())
        .await
        .expect("seed the resource identifier");
    config
        .put(b"buzz-mcp-key", b"a-gate-hmac-key-of-at-least-32-bytes")
        .await
        .expect("seed the hmac key");
}

/// Register one persona, so a session has somebody to answer as.
async fn seed_persona(env: &TestEnv, agent: &str, actor: &Actor) {
    let mut config = env.store("config");
    let entries = json!([{ "agent": agent, "pubkey": actor.pk_hex() }]);
    config
        .put(
            buzz::relay::agents::REGISTRY_CONFIG,
            entries.to_string().as_bytes(),
        )
        .await
        .expect("seed the registry");
}

/// Make `actor` the recorded creator of `agent`.
///
/// Driving a persona over ACP runs a turn with the persona's own access, so it
/// takes the persona's management authority (creator, moderator, or operator).
/// The creator row is the narrowest of the three and the one agent creation
/// writes, so it is what these gates seed for the editor they drive with.
async fn seed_owner(env: &TestEnv, agent: &str, actor: &Actor) {
    let mut config = env.store("config");
    config
        .put(
            &buzz::relay::keys::agent_owner(buzz::relay::DEFAULT_COMMUNITY, agent),
            actor.pk_hex().as_bytes(),
        )
        .await
        .expect("seed the owner");
}

/// A raw `POST /acp`, optionally bearing a token and naming a connection.
fn acp_post(body: Value, bearer: Option<&str>, connection: Option<&str>) -> IngressRequest {
    let mut headers = Vec::new();
    if let Some(token) = bearer {
        headers.push(("authorization".to_string(), format!("Bearer {token}")));
    }
    if let Some(id) = connection {
        headers.push((HEADER_CONNECTION_ID.to_string(), id.to_string()));
    }
    IngressRequest {
        stream_open: false,
        correlation_id: format!("acp-{}", body.to_string().len()),
        idempotency_key: String::new(),
        route: "/acp".to_string(),
        path: "/acp".to_string(),
        method: "POST".to_string(),
        headers,
        body: body.to_string().into_bytes(),
        deadline_ms: 0,
        reply_to: String::new(),
        published_ms: 0,
        ..IngressRequest::new("", "", Vec::new())
    }
}

fn rpc(id: u64, method: &str, params: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params })
}

/// Mint the bearer an editor authenticates with — the same route an MCP client uses.
async fn mint_token(env: &TestEnv, actor: &Actor) -> String {
    let reply = env
        .handle(post(actor, "/buzz/api/mcp/token", b"{}".to_vec()))
        .await
        .expect("the token route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    body_json(&reply)["access_token"]
        .as_str()
        .expect("a minted token")
        .to_string()
}

/// Open a connection, returning its id.
///
/// Both carriages are checked on every call, because every ACP gate in this file
/// goes through here: the `Acp-Connection-Id` RESPONSE header the HTTP binding
/// specifies, and the `_meta` copy kept beside it for one release. If they ever
/// disagree, a client reading one and a client reading the other are talking to
/// two different connections.
async fn initialize(env: &TestEnv, token: &str) -> String {
    let reply = env
        .handle(acp_post(
            rpc(
                1,
                "initialize",
                json!({ "protocolVersion": PROTOCOL_VERSION_V1 }),
            ),
            Some(token),
            None,
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let sent = reply
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(HEADER_CONNECTION_ID))
        .map(|(_, value)| value.clone())
        .expect("initialize answers with the connection-id header");
    let in_meta = body_json(&reply)["result"]["_meta"]["acp"]["connectionId"]
        .as_str()
        .expect("a connection id")
        .to_string();
    assert_eq!(sent, in_meta, "the header and the _meta copy are one id");
    in_meta
}

/// Open a session on `connection`, returning its id.
async fn new_session(env: &TestEnv, token: &str, connection: &str) -> String {
    let reply = env
        .handle(acp_post(
            rpc(2, "session/new", json!({ "cwd": "/w", "mcpServers": [] })),
            Some(token),
            Some(connection),
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let body = body_json(&reply);
    body["result"]["sessionId"]
        .as_str()
        .unwrap_or_else(|| panic!("a session id: {body}"))
        .to_string()
}

/// The messages a `session/prompt` answered with.
///
/// `request_id` is explicit because it is the half of a turn's identity the WIRE
/// controls: resending the same id is a retry, a fresh id is a fresh question. A
/// helper that hardcoded it could never exercise the second half.
async fn prompt(
    env: &TestEnv,
    token: &str,
    connection: &str,
    session: &str,
    request_id: u64,
    text: &str,
) -> Vec<Value> {
    let reply = env
        .handle(acp_post(
            rpc(
                request_id,
                "session/prompt",
                json!({
                    "sessionId": session,
                    "prompt": [{ "type": "text", "text": text }]
                }),
            ),
            Some(token),
            Some(connection),
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let body = body_json(&reply);
    match body {
        Value::Array(items) => items,
        other => vec![other],
    }
}

/// Whether this call was answered from a stored turn rather than by running one.
///
/// The observable that makes idempotency provable here, and it replaced a turn
/// counter for two reasons. Comparing ANSWERS cannot establish the property: with the
/// model unreachable, two independent turns produce the same deterministic failure
/// text, so matching text is consistent with the model having been reached twice. And
/// a counter is derived shared state, which two overlapping turns corrupt — this is a
/// fact about the calling turn's own row.
fn replayed(answers: &[Value]) -> bool {
    answers
        .iter()
        .find_map(|m| {
            m.get("result")?
                .get("_meta")?
                .get("buzz")?
                .get("replayed")?
                .as_bool()
        })
        .unwrap_or_else(|| {
            panic!("the response must say whether it reached the model: {answers:?}")
        })
}

/// **An unauthenticated editor learns nothing — not even whether a session exists.**
///
/// Identity is checked before the frame is parsed, so a caller with no credential
/// cannot use malformed-versus-well-formed answers to probe this deployment.
#[test]
 fn severe_an_unauthenticated_editor_learns_nothing() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;

    let reply = env
        .handle(acp_post(
            rpc(1, "initialize", json!({ "protocolVersion": 1 })),
            None,
            None,
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(reply.status, 401, "{}", error_text(&reply));

    // A garbage token is the same answer, so a probe cannot tell a wrong credential
    // from a missing one.
    let reply = env
        .handle(acp_post(
            rpc(1, "initialize", json!({ "protocolVersion": 1 })),
            Some("not.a.real.token"),
            None,
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(reply.status, 401, "{}", error_text(&reply));

    });
}

/// **`initialize` negotiates a version and hands back a connection.**
///
/// And the capabilities are the ones this agent can honour: claiming `loadSession`
/// would make an editor offer a resume that fails.
#[test]
 fn severe_initialize_negotiates_and_declares_only_what_it_honours() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let editor = Actor::new("acp-editor");
    let token = mint_token(&env, &editor).await;

    // A client offering a LATER version is answered at ours, never above it.
    let reply = env
        .handle(acp_post(
            rpc(1, "initialize", json!({ "protocolVersion": 99 })),
            Some(&token),
            None,
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let body = body_json(&reply);
    let result = &body["result"];
    assert_eq!(result["protocolVersion"], json!(PROTOCOL_VERSION_V1));
    assert_eq!(result["agentInfo"]["name"], json!("buzz"));
    assert!(
        result["_meta"]["acp"]["connectionId"].is_string(),
        "the connection id must be reachable: {body}"
    );

    // Honest capabilities, presence-encoded as the schema requires: `{}` means
    // supported and absence means not, so `true` here would be a conformance defect.
    let caps = &result["agentCapabilities"];
    assert_eq!(caps["loadSession"], json!(false));
    assert_eq!(caps["sessionCapabilities"]["list"], json!({}));
    assert!(
        caps["sessionCapabilities"].get("resume").is_none(),
        "an unsupported capability is ABSENT, not false: {body}"
    );

    });
}

/// **A session belongs to the connection that made it.**
///
/// The property that stops a session id being a bearer capability: two editors on
/// one deployment are two connections, and neither may prompt in the other's
/// session. Reported as ABSENT rather than forbidden — whether a stranger's session
/// exists is not this caller's to learn.
#[test]
 fn severe_a_session_belongs_to_the_connection_that_made_it() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let revka = Actor::new("revka");
    seed_persona(&env, "revka", &revka).await;

    let alice = Actor::new("acp-alice");
    seed_owner(&env, "revka", &alice).await;
    let alice_token = mint_token(&env, &alice).await;
    let alice_conn = initialize(&env, &alice_token).await;
    let alice_session = new_session(&env, &alice_token, &alice_conn).await;

    // A SECOND editor, with its own identity and its own connection.
    let bob = Actor::new("acp-bob");
    let bob_token = mint_token(&env, &bob).await;
    let bob_conn = initialize(&env, &bob_token).await;
    assert_ne!(alice_conn, bob_conn, "two editors are two connections");

    // Bob names Alice's session. Refused.
    let answers = prompt(
        &env,
        &bob_token,
        &bob_conn,
        &alice_session,
        3,
        "whose is this?",
    )
    .await;
    let error = &answers[answers.len() - 1]["error"];
    assert!(
        !error.is_null(),
        "prompting in another connection's session must be refused: {answers:?}"
    );
    assert_eq!(error["code"], json!(proto::INVALID_PARAMS));

    // And Bob cannot see it on his own roster either.
    let reply = env
        .handle(acp_post(
            rpc(9, "session/list", json!({})),
            Some(&bob_token),
            Some(&bob_conn),
        ))
        .await
        .expect("the acp route answers");
    let listed = body_json(&reply);
    let sessions = listed["result"]["sessions"].as_array().expect("a list");
    assert!(
        sessions.is_empty(),
        "another connection's session must not be listed: {listed}"
    );

    // Alice's OWN connection still reaches it, so the refusal is about ownership
    // rather than the session having failed to exist.
    let reply = env
        .handle(acp_post(
            rpc(9, "session/list", json!({})),
            Some(&alice_token),
            Some(&alice_conn),
        ))
        .await
        .expect("the acp route answers");
    let mine = body_json(&reply);
    assert_eq!(
        mine["result"]["sessions"][0]["sessionId"],
        json!(alice_session),
        "{mine}"
    );

    });
}

/// **A retried prompt returns the same answer, and does not run a second turn.**
///
/// A client retries precisely when it did not receive the answer, so "already ran"
/// would leave it no better off than the failure it is retrying.
///
/// The proof is the response's own `replayed` signal, not the answer text. Under this
/// harness the model is unreachable, so two independent turns answer with the same
/// deterministic failure text — matching text is therefore consistent with having run
/// twice, and only a statement about the path taken can tell the two apart.
#[test]
 fn severe_a_retried_prompt_is_answered_from_the_turn_it_already_ran() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let revka = Actor::new("revka");
    seed_persona(&env, "revka", &revka).await;
    let editor = Actor::new("acp-editor");
    seed_owner(&env, "revka", &editor).await;
    let token = mint_token(&env, &editor).await;
    let connection = initialize(&env, &token).await;
    let session = new_session(&env, &token, &connection).await;

    let first = prompt(&env, &token, &connection, &session, 7, "what is a queue?").await;
    assert!(
        !replayed(&first),
        "the first prompt must actually run: {first:?}"
    );

    // The SAME request id and the same text — a retry, which is what a client resends
    // when it did not receive the answer.
    let second = prompt(&env, &token, &connection, &session, 7, "what is a queue?").await;
    assert!(
        replayed(&second),
        "a retried prompt must not run a second turn: {second:?}"
    );

    // And the retry replays the same ANSWER — the update sequence and the stop
    // reason. Deliberately NOT the whole response: `replayed` differs between the two
    // by design, so comparing the arrays wholesale would require the very marker this
    // test is reading to be wrong.
    //
    // Structural equality of the parsed messages rather than of the bytes, because
    // JSON key order and whitespace carry no meaning and requiring byte identity
    // would pin a detail the protocol does not define.
    let updates = |answers: &[Value]| -> Vec<Value> {
        answers
            .iter()
            .filter(|m| m.get("method") == Some(&json!("session/update")))
            .cloned()
            .collect()
    };
    assert!(
        !updates(&first).is_empty(),
        "the answer must arrive as protocol content: {first:?}"
    );
    assert_eq!(
        updates(&first),
        updates(&second),
        "a retry must replay the original answer, message for message"
    );

    let stop_reason = |answers: &[Value]| -> Value {
        answers
            .iter()
            .find_map(|m| m.get("result").and_then(|r| r.get("stopReason")).cloned())
            .unwrap_or(Value::Null)
    };
    assert_eq!(stop_reason(&first), json!("end_turn"));
    assert_eq!(
        stop_reason(&first),
        stop_reason(&second),
        "a replay ends the turn the same way the original did"
    );

    let answered = first
        .iter()
        .find_map(|m| {
            m.get("params")
                .and_then(|p| p.get("update"))
                .and_then(|u| u.get("content"))
                .and_then(|c| c.get("text"))
                .and_then(Value::as_str)
        })
        .unwrap_or_else(|| panic!("an answer chunk: {first:?}"));
    assert!(
        !answered.contains("duplicate admission"),
        "a retry must never receive the engine's duplicate placeholder: {answered}"
    );

    // A DIFFERENT prompt IS a new turn, which is what makes the cache's keying
    // testable rather than assumed: a cache keyed on the session alone would answer
    // this from the first turn.
    let other = prompt(&env, &token, &connection, &session, 8, "and a topic?").await;
    assert!(
        !replayed(&other),
        "a different prompt is a new turn, not a cache hit: {other:?}"
    );

    // And the SAME text under a NEW request id RUNS. This is the half a text-keyed
    // cache fails — asking again is not retrying — and the half this gate could not
    // see while its prompt helper hardcoded one id.
    let again = prompt(&env, &token, &connection, &session, 9, "what is a queue?").await;
    assert!(
        !replayed(&again),
        "a deliberate repeat must run rather than echo: {again:?}"
    );

    });
}

/// **The answer arrives as ACP CONTENT, not as private metadata.**
///
/// `PromptResponse` carries only a `stopReason`; a turn's content is delivered by
/// `session/update`. A surface that put the answer anywhere else would leave a
/// conformant editor showing a turn that ended with nothing in it.
#[test]
 fn severe_the_answer_arrives_as_protocol_content() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let revka = Actor::new("revka");
    seed_persona(&env, "revka", &revka).await;
    let editor = Actor::new("acp-editor");
    seed_owner(&env, "revka", &editor).await;
    let token = mint_token(&env, &editor).await;
    let connection = initialize(&env, &token).await;
    let session = new_session(&env, &token, &connection).await;

    let answers = prompt(&env, &token, &connection, &session, 3, "say something").await;
    assert!(
        answers.len() >= 2,
        "the content and the stop reason are separate messages: {answers:?}"
    );

    // The update comes FIRST, so a client reading in order sees the content before
    // the reason the turn ended.
    let update = &answers[0];
    assert_eq!(update["method"], json!("session/update"));
    assert_eq!(update["params"]["sessionId"], json!(session));
    assert_eq!(
        update["params"]["update"]["sessionUpdate"],
        json!("agent_message_chunk")
    );
    assert_eq!(update["params"]["update"]["content"]["type"], json!("text"));
    assert!(
        update["params"]["update"]["content"]["text"]
            .as_str()
            .is_some_and(|t| !t.is_empty()),
        "the chunk must carry the answer: {update}"
    );

    // A notification is never answered, so it carries no id.
    assert!(update.get("id").is_none(), "{update}");

    // And the response itself is the stop reason.
    let response = &answers[answers.len() - 1];
    assert_eq!(response["result"]["stopReason"], json!("end_turn"));

    });
}

/// **A method this agent does not implement is named in its refusal.**
///
/// It is declared unsupported in the capabilities, so a conformant client never
/// sends one — but one that does gets `-32601` naming the method rather than
/// silence.
#[test]
 fn severe_an_unimplemented_method_is_named_in_its_refusal() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let editor = Actor::new("acp-editor");
    let token = mint_token(&env, &editor).await;
    let connection = initialize(&env, &token).await;

    for method in ["session/load", "session/resume", "session/set_mode"] {
        let reply = env
            .handle(acp_post(
                rpc(
                    7,
                    method,
                    json!({ "sessionId": "s1", "cwd": "/w", "mcpServers": [], "modeId": "m" }),
                ),
                Some(&token),
                Some(&connection),
            ))
            .await
            .expect("the acp route answers");
        let body = body_json(&reply);
        assert_eq!(
            body["error"]["code"],
            json!(proto::METHOD_NOT_FOUND),
            "{method}: {body}"
        );
        assert!(
            body["error"]["message"]
                .as_str()
                .is_some_and(|m| m.contains(method)),
            "the refusal must name the method: {body}"
        );
    }

    // A method nothing declares at all is the same answer.
    let reply = env
        .handle(acp_post(
            rpc(8, "session/teleport", json!({})),
            Some(&token),
            Some(&connection),
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(
        body_json(&reply)["error"]["code"],
        json!(proto::METHOD_NOT_FOUND)
    );

    });
}

/// **A call that needs a connection says so, and a notification is never answered.**
///
/// Two shapes of the same discipline. Answering a notification puts an unmatched
/// response on the wire the peer must discard; failing to answer a request leaves
/// its caller waiting out a deadline.
#[test]
 fn severe_a_call_without_a_connection_is_refused_and_a_notification_is_not_answered() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let editor = Actor::new("acp-editor");
    let token = mint_token(&env, &editor).await;

    // Authenticated, but no connection header: refused by name rather than by a
    // confusing "no such session".
    let reply = env
        .handle(acp_post(
            rpc(2, "session/new", json!({ "cwd": "/w", "mcpServers": [] })),
            Some(&token),
            None,
        ))
        .await
        .expect("the acp route answers");
    let body = body_json(&reply);
    assert_eq!(
        body["error"]["code"],
        json!(proto::INVALID_REQUEST),
        "{body}"
    );
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains(HEADER_CONNECTION_ID)),
        "the refusal must name the header: {body}"
    );

    // `session/cancel` is a NOTIFICATION — no id, so no reply at all.
    let connection = initialize(&env, &token).await;
    let reply = env
        .handle(acp_post(
            json!({
                "jsonrpc": "2.0",
                "method": "session/cancel",
                "params": { "sessionId": "whatever" }
            }),
            Some(&token),
            Some(&connection),
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(
        reply.status,
        202,
        "a frame of only notifications has nothing to answer: {}",
        error_text(&reply)
    );
    assert!(reply.body.is_empty(), "and carries no body");

    });
}

/// **A malformed batch entry is answered individually.**
///
/// JSON-RPC §6: the valid members of a batch are processed and each invalid one is
/// answered on its own. Rejecting the array would discard a client's good work
/// alongside its typo.
#[test]
 fn severe_one_bad_batch_entry_does_not_discard_the_good_one() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let editor = Actor::new("acp-editor");
    let token = mint_token(&env, &editor).await;

    let reply = env
        .handle(acp_post(
            json!([
                { "jsonrpc": "2.0", "id": 1, "method": "initialize",
                  "params": { "protocolVersion": 1 } },
                { "nope": true }
            ]),
            Some(&token),
            None,
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let body = body_json(&reply);
    let answers = body.as_array().expect("a batch reply");
    assert_eq!(answers.len(), 2, "both entries are answered: {body}");
    assert!(
        answers
            .iter()
            .any(|a| a["result"]["protocolVersion"] == json!(1)),
        "the good entry must still be served: {body}"
    );
    assert!(
        answers
            .iter()
            .any(|a| a["error"]["code"] == json!(proto::INVALID_REQUEST)),
        "the bad entry must be answered on its own: {body}"
    );

    });
}

/// Mint a bearer at the clock's CURRENT instant.
///
/// [`mint_token`] signs its NIP-98 event at the fixture's T0, which is right for
/// every test that never moves the clock — and wrong for the one that moves it past
/// the token's own TTL, where a T0-signed event is stale and a T0-minted bearer is
/// expired.
async fn mint_token_at_now(env: &TestEnv, actor: &Actor, nonce: &str) -> String {
    let event = auth_event(
        actor,
        "POST",
        "/buzz/api/mcp/token",
        env.now_ms() / 1000,
        nonce,
    );
    let header = auth_header(&event);
    let reply = env
        .handle(request_with_auth(
            "POST",
            "/buzz/api/mcp/token",
            Some(&header),
            b"{}".to_vec(),
        ))
        .await
        .expect("the token route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    body_json(&reply)["access_token"]
        .as_str()
        .expect("a minted token")
        .to_string()
}

/// **An idle row expires; a used one is kept.**
///
/// `initialize` and `session/new` used to write durable rows with a plain put — no
/// TTL, no teardown — so an editor that leaked connections grew the store forever.
/// The rows now carry a sliding idle expiry, and this drives the harness clock
/// through all three cases: half a window is not expiry, use refreshes the window,
/// and a full unused window is the end of the row.
#[test]
 fn severe_an_idle_row_expires_and_a_used_one_is_kept() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let revka = Actor::new("revka");
    seed_persona(&env, "revka", &revka).await;
    let editor = Actor::new("acp-editor");
    seed_owner(&env, "revka", &editor).await;
    let token = mint_token(&env, &editor).await;
    let connection = initialize(&env, &token).await;
    let session = new_session(&env, &token, &connection).await;

    // Half a window on, the connection answers and still lists its session — and
    // answering IS use, so this call slides the connection's own window forward.
    env.advance(IDLE_TTL_MS / 2);
    let token = mint_token_at_now(&env, &editor, "acp-expiry-nonce-1").await;
    let reply = env
        .handle(acp_post(
            rpc(4, "session/list", json!({})),
            Some(&token),
            Some(&connection),
        ))
        .await
        .expect("the acp route answers");
    let listed = body_json(&reply);
    assert_eq!(
        listed["result"]["sessions"][0]["sessionId"],
        json!(session),
        "half an idle window must not expire anything: {listed}"
    );

    // A full window after its creation the UNUSED session is gone, while the
    // connection — refreshed by the listing above — still answers. Listing does
    // not refresh sessions; only addressing one does.
    env.advance(IDLE_TTL_MS / 2 + 60_000);
    let token = mint_token_at_now(&env, &editor, "acp-expiry-nonce-2").await;
    let reply = env
        .handle(acp_post(
            rpc(5, "session/list", json!({})),
            Some(&token),
            Some(&connection),
        ))
        .await
        .expect("the acp route answers");
    let listed = body_json(&reply);
    let sessions = listed["result"]["sessions"].as_array().expect("a list");
    assert!(
        sessions.is_empty(),
        "an idle session must expire rather than accrete: {listed}"
    );

    // And a full window with no use at all takes the connection itself: the next
    // call finds nothing, exactly as if it had never initialized.
    env.advance(IDLE_TTL_MS + 120_000);
    let token = mint_token_at_now(&env, &editor, "acp-expiry-nonce-3").await;
    let reply = env
        .handle(acp_post(
            rpc(6, "session/new", json!({ "cwd": "/w", "mcpServers": [] })),
            Some(&token),
            Some(&connection),
        ))
        .await
        .expect("the acp route answers");
    let body = body_json(&reply);
    assert_eq!(
        body["error"]["code"],
        json!(proto::RESOURCE_NOT_FOUND),
        "an idle connection must expire rather than accrete: {body}"
    );

    });
}

/// The property gate is open (the demonstration is public): a request with no
/// session, or with a forged one, reaches the relay exactly as a signed-in one
/// does, while the gate still owns sign-out. The ACP bearer and connection
/// checks behind it are unchanged, which the rest of this file proves.
#[test]
 fn severe_property_gate_admits_anonymous_and_still_owns_sign_out() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    let req = common::get("/buzz/api/healthz");
    assert_eq!(
        env.0
            .handle(req.clone())
            .await
            .expect("anonymous reply")
            .status,
        200,
        "an anonymous visitor reaches the relay"
    );
    let mut forged = req.clone();
    forged
        .headers
        .push(("cookie".into(), "__Host-ferrite_app_session=forged".into()));
    assert_eq!(
        env.0.handle(forged).await.expect("forged reply").status,
        200,
        "a forged session is simply anonymous"
    );
    assert_eq!(env.handle(req).await.expect("signed reply").status, 200);

    let logout = env
        .0
        .handle(common::get("/_sso/logout"))
        .await
        .expect("logout reply");
    assert_eq!(logout.status, 303, "the gate still answers sign-out itself");

    });
}

/// The error a `session/new` call answered with, or `Null` when it succeeded.
async fn new_session_error(env: &TestEnv, token: &str, connection: &str, meta: Value) -> Value {
    let reply = env
        .handle(acp_post(
            rpc(
                2,
                "session/new",
                json!({ "cwd": "/w", "mcpServers": [], "_meta": meta }),
            ),
            Some(token),
            Some(connection),
        ))
        .await
        .expect("the acp route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    body_json(&reply)["error"].clone()
}

/// **A valid token is not authority over a persona.**
///
/// A prompt runs a full persona turn — its tools read and post AS the persona, and
/// the answer comes back to the caller — so a caller with no relation to the
/// persona could have it read a private room and print the result. The surface
/// now requires the persona's management authority and a passing write gate, at
/// `session/new` and again on every prompt, and every failure refuses.
#[test]
 fn severe_a_token_holder_without_authority_cannot_drive_a_persona() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let revka = Actor::new("revka");
    seed_persona(&env, "revka", &revka).await;
    let alice = Actor::new("acp-alice");
    seed_owner(&env, "revka", &alice).await;

    // A stranger: a self-minted identity with a token and a connection of its own.
    let mallory = Actor::new("acp-mallory");
    let mallory_token = mint_token(&env, &mallory).await;
    let mallory_conn = initialize(&env, &mallory_token).await;

    // Naming the persona, and taking the default one, are both refused.
    for meta in [json!({ "buzz": { "agent": "revka" } }), json!({})] {
        let error = new_session_error(&env, &mallory_token, &mallory_conn, meta).await;
        assert_eq!(error["code"], json!(proto::APPLICATION_ERROR), "{error}");
        assert!(
            error["message"]
                .as_str()
                .is_some_and(|m| m.contains("creator")),
            "the refusal must say whose authority is needed: {error}"
        );
    }

    // The owner is served, so the refusal above is about authority rather than a
    // broken session path.
    let alice_token = mint_token(&env, &alice).await;
    let alice_conn = initialize(&env, &alice_token).await;
    let session = new_session(&env, &alice_token, &alice_conn).await;
    let first = prompt(&env, &alice_token, &alice_conn, &session, 3, "hello").await;
    assert!(
        first.last().is_some_and(|m| m["error"].is_null()),
        "the owner's prompt must run: {first:?}"
    );

    // Authority is asked on EVERY prompt: once alice no longer owns the persona,
    // her open session stops answering — a fresh question and a retry alike,
    // since a replayed answer is persona output too.
    seed_owner(&env, "revka", &mallory).await;
    for (id, text) in [(4, "hello again"), (3, "hello")] {
        let answers = prompt(&env, &alice_token, &alice_conn, &session, id, text).await;
        let error = &answers[answers.len() - 1]["error"];
        assert_eq!(error["code"], json!(proto::APPLICATION_ERROR), "{answers:?}");
        assert!(
            answers
                .iter()
                .all(|m| m.get("method") != Some(&json!("session/update"))),
            "a refused prompt must carry no persona output: {answers:?}"
        );
    }

    // And the write gate runs first: in a closed community, even the persona's
    // owner is refused until a claim admits them.
    let mut config = env.store("config");
    config
        .put(b"buzz-community-closed", b"true")
        .await
        .expect("close the community");
    let mallory_conn = initialize(&env, &mallory_token).await;
    let error = new_session_error(
        &env,
        &mallory_token,
        &mallory_conn,
        json!({ "buzz": { "agent": "revka" } }),
    )
    .await;
    assert_eq!(error["code"], json!(proto::APPLICATION_ERROR), "{error}");
    assert!(
        error["message"]
            .as_str()
            .is_some_and(|m| m.contains("invite-only")),
        "a closed community refuses a non-member owner: {error}"
    );

    });
}

/// Fill `actor`'s window for one ACP rate class, as if `n` uses had been counted.
async fn fill_rate_window(env: &TestEnv, actor: &Actor, class: &str, n: i64) {
    let mut config = env.store("config");
    // add_expiring preserves a live counter's original deadline, so seed the
    // documented class window rather than depending on a later bump to reset it.
    let window_ms = match class {
        ACP_PROMPT_RATE_CLASS => 60 * 1000,
        ACP_CONNECT_RATE_CLASS => 60 * 60 * 1000,
        _ => panic!("unknown ACP fixture rate class: {class}"),
    };
    buzz::relay::store::bump_expiring_counter(
        &mut config,
        &buzz::relay::keys::rate(buzz::relay::DEFAULT_COMMUNITY, &actor.pk, class),
        n,
        env.now_ms().saturating_add(window_ms),
    )
    .await
    .expect("seed the rate window");
}

/// **A prompt that reaches the model is counted against its caller.**
///
/// Authorization decides who may spend this deployment's provider budget; this
/// decides how fast. Before the limit, the manifest's 600/min route limit was the
/// only ceiling on model turns per caller. A retry answered from its stored turn
/// reaches no model, so it is still served once the window is full.
#[test]
 fn severe_prompts_that_reach_the_model_are_rate_limited_per_caller() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let revka = Actor::new("revka");
    seed_persona(&env, "revka", &revka).await;
    let editor = Actor::new("acp-rate-editor");
    seed_owner(&env, "revka", &editor).await;
    let token = mint_token(&env, &editor).await;
    let connection = initialize(&env, &token).await;
    let session = new_session(&env, &token, &connection).await;

    let first = prompt(&env, &token, &connection, &session, 11, "first question").await;
    assert!(!replayed(&first), "the first prompt must run: {first:?}");

    // The window is now full.
    fill_rate_window(&env, &editor, ACP_PROMPT_RATE_CLASS, ACP_PROMPT_RATE_LIMIT).await;

    let refused = prompt(&env, &token, &connection, &session, 12, "one too many").await;
    let error = &refused[refused.len() - 1]["error"];
    assert_eq!(error["code"], json!(proto::APPLICATION_ERROR), "{refused:?}");
    assert!(
        error["message"].as_str().is_some_and(|m| m.contains("rate-limited")),
        "the refusal must say it is a rate limit: {refused:?}"
    );
    assert!(
        refused
            .iter()
            .all(|m| m.get("method") != Some(&json!("session/update"))),
        "a rate-limited prompt must run no turn: {refused:?}"
    );

    // A retry of the turn that already ran costs no model call and is still served.
    let retry = prompt(&env, &token, &connection, &session, 11, "first question").await;
    assert!(replayed(&retry), "a stored turn must still replay: {retry:?}");

    // Another caller has a window of their own.
    let other = Actor::new("acp-rate-other");
    seed_owner(&env, "revka", &other).await;
    let other_token = mint_token(&env, &other).await;
    let other_conn = initialize(&env, &other_token).await;
    let other_session = new_session(&env, &other_token, &other_conn).await;
    let served = prompt(&env, &other_token, &other_conn, &other_session, 13, "hi").await;
    assert!(!replayed(&served), "another caller is not limited: {served:?}");

    });
}

/// **Opening connections is bounded per caller too.**
///
/// Connection rows expire, which bounds them by rate rather than by total; this
/// bounds the rate.
#[test]
 fn severe_initialize_is_rate_limited_per_caller() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let editor = Actor::new("acp-conn-editor");
    let token = mint_token(&env, &editor).await;
    initialize(&env, &token).await;
    fill_rate_window(&env, &editor, ACP_CONNECT_RATE_CLASS, ACP_CONNECT_RATE_LIMIT).await;

    let reply = env
        .handle(acp_post(
            rpc(1, "initialize", json!({ "protocolVersion": PROTOCOL_VERSION_V1 })),
            Some(&token),
            None,
        ))
        .await
        .expect("the acp route answers");
    let body = body_json(&reply);
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("rate-limited")),
        "a caller past the window must not open another connection: {body}"
    );

    });
}

/// One `initialize` as `token`, returning the JSON-RPC body.
async fn try_initialize(env: &TestEnv, token: &str) -> Value {
    let reply = env
        .handle(acp_post(
            rpc(1, "initialize", json!({ "protocolVersion": PROTOCOL_VERSION_V1 })),
            Some(token),
            None,
        ))
        .await
        .expect("the acp route answers");
    body_json(&reply)
}

fn rate_limited(body: &Value) -> bool {
    body["error"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("rate-limited"))
}

/// **The limits are exactly 20 turns a minute and 120 connections an hour, and
/// a window that has passed admits again.**
///
/// Filled to one short of each bound through the counter, then driven through
/// the route: the last admitted call is served and the next one is refused, so
/// an off-by-one in either direction fails here. The clock is then moved past
/// the window, which must reopen it rather than leaving a caller locked out.
#[test]
 fn severe_acp_limits_are_exact_and_their_windows_expire() {
    common::run_scenario(|| async move {
    assert_eq!(ACP_PROMPT_RATE_LIMIT, 20, "the documented turn limit");
    assert_eq!(ACP_CONNECT_RATE_LIMIT, 120, "the documented connection limit");

    let env = acp_env().await;
    seed_identity(&env).await;
    let revka = Actor::new("revka");
    seed_persona(&env, "revka", &revka).await;
    let editor = Actor::new("acp-exact-editor");
    seed_owner(&env, "revka", &editor).await;
    let token = mint_token(&env, &editor).await;

    // Connections: the setup's own initialize is use 1.
    let connection = initialize(&env, &token).await;
    fill_rate_window(&env, &editor, ACP_CONNECT_RATE_CLASS, ACP_CONNECT_RATE_LIMIT - 2).await;
    let last = try_initialize(&env, &token).await;
    assert!(last["result"].is_object(), "use {ACP_CONNECT_RATE_LIMIT} must be served: {last}");
    let over = try_initialize(&env, &token).await;
    assert!(rate_limited(&over), "use {} must be refused: {over}", ACP_CONNECT_RATE_LIMIT + 1);

    // Turns: nothing has been counted yet.
    let session = new_session(&env, &token, &connection).await;
    fill_rate_window(&env, &editor, ACP_PROMPT_RATE_CLASS, ACP_PROMPT_RATE_LIMIT - 1).await;
    let last = prompt(&env, &token, &connection, &session, 30, "the last one").await;
    assert!(!replayed(&last), "turn {ACP_PROMPT_RATE_LIMIT} must run: {last:?}");
    let over = prompt(&env, &token, &connection, &session, 31, "one too many").await;
    assert!(
        rate_limited(&over[over.len() - 1]),
        "turn {} must be refused: {over:?}",
        ACP_PROMPT_RATE_LIMIT + 1
    );

    // A full minute later the turn window has passed; the connection window
    // (an hour) has not.
    env.advance(60 * 1000 + 1);
    let token = mint_token_at_now(&env, &editor, "acp-exact-nonce-1").await;
    let again = prompt(&env, &token, &connection, &session, 32, "a new minute").await;
    assert!(!replayed(&again), "an expired turn window must admit again: {again:?}");
    assert!(
        rate_limited(&try_initialize(&env, &token).await),
        "the connection window has not passed yet"
    );

    env.advance(60 * 60 * 1000);
    let token = mint_token_at_now(&env, &editor, "acp-exact-nonce-2").await;
    let reopened = try_initialize(&env, &token).await;
    assert!(
        reopened["result"].is_object(),
        "an expired connection window must admit again: {reopened}"
    );

    });
}

/// Make the counter behind `class` unusable for `actor`: the store refuses to
/// add to a value that is not a number, which is a real counter error.
async fn break_rate_counter(env: &TestEnv, actor: &Actor, class: &str) {
    let mut config = env.store("config");
    config
        .put(
            &buzz::relay::keys::rate(buzz::relay::DEFAULT_COMMUNITY, &actor.pk, class),
            b"not-a-counter",
        )
        .await
        .expect("seed an unusable counter");
}

fn unconfirmed_and_retryable(body: &Value) -> bool {
    body["error"]["message"]
        .as_str()
        .is_some_and(|m| m.contains("could not be confirmed"))
        && body["error"]["data"]["retryable"] == json!(true)
}

/// **A limit that cannot be counted refuses, retryably, instead of admitting.**
///
/// The limiter used to fail OPEN on a counter error, so anybody able to make
/// the counter fail had no limit on model turns or connections at all. Each
/// class is broken through the store and driven through the route: the call is
/// refused with a retryable error and no turn runs.
#[test]
 fn severe_an_uncountable_acp_call_is_refused_retryably() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let revka = Actor::new("revka");
    seed_persona(&env, "revka", &revka).await;
    let editor = Actor::new("acp-broken-counter-editor");
    seed_owner(&env, "revka", &editor).await;
    let token = mint_token(&env, &editor).await;
    let connection = initialize(&env, &token).await;
    let session = new_session(&env, &token, &connection).await;

    break_rate_counter(&env, &editor, ACP_PROMPT_RATE_CLASS).await;
    let refused = prompt(&env, &token, &connection, &session, 40, "count me").await;
    let last = &refused[refused.len() - 1];
    assert!(
        unconfirmed_and_retryable(last),
        "an uncountable turn must be refused retryably: {refused:?}"
    );
    assert!(
        refused
            .iter()
            .all(|m| m.get("method") != Some(&json!("session/update"))),
        "an uncountable turn must run nothing: {refused:?}"
    );

    break_rate_counter(&env, &editor, ACP_CONNECT_RATE_CLASS).await;
    let refused = try_initialize(&env, &token).await;
    assert!(
        unconfirmed_and_retryable(&refused),
        "an uncountable connection must be refused retryably: {refused}"
    );

    });
}

/// **A token whose subject is not a workspace identity cannot open a connection.**
///
/// Such a subject has no rate window, and `initialize` runs before any
/// authorization check, so the limiter used to let it through uncounted —
/// unlimited connection rows for any holder of such a token.
#[test]
 fn severe_a_subject_without_a_rate_window_cannot_initialize() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let token = buzz::mcp::tokens::mint(
        b"a-gate-hmac-key-of-at-least-32-bytes",
        &buzz::mcp::tokens::Claims {
            iss: RESOURCE.into(),
            aud: RESOURCE.into(),
            sub: "service:not-a-pubkey".into(),
            scope: Vec::new(),
            iat: T0_MS,
            exp: T0_MS + 60 * 60 * 1000,
            jti: "acp-gate-non-pubkey".into(),
        },
    )
    .expect("mint");
    let body = try_initialize(&env, &token).await;
    assert!(
        body["error"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("cannot be counted")),
        "a subject with no rate window must not open a connection: {body}"
    );

    });
}

/// A prompt whose first driver ran the turn but never stored its answer row
/// (the write failed, or the driver died) is finished by the retry rather
/// than left waiting on a winner that will never answer.
#[test]
fn severe_a_retry_after_a_lost_answer_row_is_answered() {
    common::run_scenario(|| async move {
    let env = acp_env().await;
    seed_identity(&env).await;
    let revka = Actor::new("revka");
    seed_persona(&env, "revka", &revka).await;
    let editor = Actor::new("acp-editor");
    seed_owner(&env, "revka", &editor).await;
    let token = mint_token(&env, &editor).await;
    let connection = initialize(&env, &token).await;
    let session = new_session(&env, &token, &connection).await;

    let first = prompt(&env, &token, &connection, &session, 11, "what is a lease?").await;
    assert!(!replayed(&first), "the first prompt must run: {first:?}");

    // The answer row is gone, as if its write had failed after the turn ran.
    let mut config = env.store("config");
    let (rows, _) = config.scan_prefix(b"acpturn/", None, 100).await.unwrap();
    assert!(!rows.is_empty(), "the first prompt stored no answer row");
    for (key, _, _) in rows {
        config.delete(&key).await.unwrap();
    }

    let retry = tokio::time::timeout(
        Duration::from_secs(60),
        prompt(&env, &token, &connection, &session, 11, "what is a lease?"),
    )
    .await
    .expect("the retry waited on a winner that will never answer");
    assert!(
        retry.iter().any(|m| m.get("method") == Some(&json!("session/update"))),
        "the retry carried no answer: {retry:?}"
    );
    });
}
