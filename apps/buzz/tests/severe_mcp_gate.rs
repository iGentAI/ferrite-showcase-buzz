//! The MCP 2026-07-28 surface, over a real fleet.
//!
//! The package's own tests cover the protocol and the authorization state machine
//! in isolation. What only a gate can show is that `ingress::route` actually
//! reaches them, and that the two ACCESS properties hold end to end: a tool call
//! resolves rooms against the TOKEN'S SUBJECT, and a private room a subject is
//! not in stays invisible even though its words have index rows exactly like
//! anybody else's.

use super::common;

use buzz::mcp::proto;
use buzz::relay::channel::Visibility;
use buzz::relay::kind;
use buzz::relay::{operator, store as buzz_store};
use common::{
    Actor, SsoTestEnv, body_json, gate_env, error_text, fresh_auth, post, query, request_with_auth,
    seed_channel_with_owner, signed, submit, tag,
};
use ferrite_app::serde_json::{self, json, Value};
use ferrite_app::IngressRequest;

/// The resource identifier this fixture's deployment publishes under.
const RESOURCE: &str = "https://gate.example/mcp";
/// Where a client — and the handler — derive its metadata document.
const METADATA_PATH: &str = "/.well-known/oauth-protected-resource/mcp";

/// Seed the two config rows the MCP surface is fail-closed without.
///
/// Through the config-store FALLBACK rather than secrets: `ferrite-app-test`
/// injects an empty secrets map, so the fallback is the only seam a gate has —
/// the same reason the operator gates use it.
async fn seed_mcp_config(env: &SsoTestEnv) {
    let ctx = env.context();
    let mut config = ctx
        .store(buzz::relay::store::CONFIG)
        .expect("the config binding");
    config
        .put(b"buzz-mcp-resource", RESOURCE.as_bytes())
        .await
        .expect("seed the resource identifier");
    config
        .put(b"buzz-mcp-key", b"a-gate-hmac-key-of-at-least-32-bytes")
        .await
        .expect("seed the hmac key");
}

/// A raw request with arbitrary headers — the MCP endpoint needs a `Bearer`
/// credential and a protocol-version header, neither of which the NIP-98
/// builders produce.
fn raw(method: &str, path: &str, headers: Vec<(String, String)>, body: Vec<u8>) -> IngressRequest {
    IngressRequest {
        stream_open: false,
        correlation_id: String::new(),
        idempotency_key: String::new(),
        route: path.to_string(),
        path: path.to_string(),
        method: method.to_string(),
        headers,
        body,
        deadline_ms: 0,
        reply_to: String::new(),
        published_ms: 0,
        ..IngressRequest::new("", "", Vec::new())
    }
}

/// A well-formed MCP request body.
fn rpc(id: u64, method: &str, params: Value) -> Vec<u8> {
    proto::request(
        &json!(id),
        method,
        params,
        Some(&proto::Identity::new("gate", "1")),
        &json!({}),
    )
    .to_string()
    .into_bytes()
}

/// `POST /mcp`, optionally bearing a token.
///
/// Carries the SEP-2243 mirrored headers, because a conformant server refuses a
/// request without them — and because a test that omitted them would be refused
/// `-32020`, which is the same code the version-mismatch test asserts on. It would
/// pass for the wrong reason.
fn mcp_request(id: u64, method: &str, params: Value, bearer: Option<&str>) -> IngressRequest {
    let tool = if method == proto::M_TOOLS_CALL {
        params.get("name").and_then(Value::as_str)
    } else {
        None
    };
    let mut headers = vec![
        (
            proto::HEADER_PROTOCOL_VERSION.to_string(),
            proto::PROTOCOL_VERSION.to_string(),
        ),
        (proto::HEADER_METHOD.to_string(), method.to_string()),
    ];
    if let Some(name) = tool {
        headers.push((proto::HEADER_NAME.to_string(), name.to_string()));
    }
    if let Some(token) = bearer {
        headers.push(("authorization".to_string(), format!("Bearer {token}")));
    }
    raw("POST", "/mcp", headers, rpc(id, method, params))
}

/// Put `actor` in the operator set, through the config fallback.
///
/// The same seam the operator gates use: `ferrite-app-test` injects an empty
/// secrets map, so the config row is the only way in from outside the face.
async fn seed_operator(env: &SsoTestEnv, actor: &Actor) {
    let mut config = env.store("config");
    let set = serde_json::to_vec(&json!([actor.pk_hex()])).expect("set encodes");
    buzz_store::put_raw(&mut config, operator::OPERATOR_SET_CONFIG, &set)
        .await
        .expect("operator seed");
}

/// Seed the OAuth client identifier, separately from [`seed_mcp_config`].
///
/// Separate because its ABSENCE is a distinct, testable state: a deployment that
/// has not been given one should be told so rather than told about egress.
async fn seed_mcp_client_id(env: &SsoTestEnv) {
    let ctx = env.context();
    let mut config = ctx
        .store(buzz::relay::store::CONFIG)
        .expect("the config binding");
    config
        .put(b"buzz-mcp-client-id", b"https://gate.example/mcp-client")
        .await
        .expect("seed the client id");
}

/// Read the connector catalog as `actor`.
async fn read_catalog(env: &SsoTestEnv, actor: &Actor) -> Value {
    let (_, header) = fresh_auth(actor, "GET", "/buzz/api/mcp/connectors");
    let reply = env
        .handle(request_with_auth(
            "GET",
            "/buzz/api/mcp/connectors",
            Some(&header),
            Vec::new(),
        ))
        .await
        .expect("the connectors route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    body_json(&reply)
}

/// Ask to connect a connector, as `actor`.
async fn attempt_authorize(
    env: &SsoTestEnv,
    actor: &Actor,
    connector: &str,
) -> ferrite_app::IngressReply {
    env.handle(post(
        actor,
        "/buzz/api/mcp/authorize",
        json!({ "connector": connector }).to_string().into_bytes(),
    ))
    .await
    .expect("the authorize route answers")
}

/// Mint a self-issued access token for `actor` through the NIP-98 route.
async fn mint_token(env: &SsoTestEnv, actor: &Actor) -> String {
    let reply = env
        .handle(post(actor, "/buzz/api/mcp/token", b"{}".to_vec()))
        .await
        .expect("the token route answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let body = body_json(&reply);
    assert_eq!(body["token_type"], json!("Bearer"));
    assert_eq!(body["resource"], json!(RESOURCE));
    body["access_token"]
        .as_str()
        .expect("a minted token")
        .to_string()
}

/// **Discovery is reachable without a credential; a real operation is not.**
///
/// A client must be able to learn HOW to authenticate before it can, so
/// `server/discover` requires no scope. Everything else answers 401 with the RFC
/// 9728 challenge — as a real `WWW-Authenticate` header now that the reply tier
/// carries headers, and, for one release, in the body as well, so a client
/// written against the header-less shape still finds the metadata URL.
#[test]
 fn severe_mcp_discovery_is_anonymous_and_a_tool_listing_is_not() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;

    let discovered = env
        .handle(mcp_request(1, proto::M_DISCOVER, json!({}), None))
        .await
        .expect("discover answers");
    assert_eq!(discovered.status, 200, "{}", error_text(&discovered));
    let result = &body_json(&discovered)["result"];
    assert_eq!(
        result["supportedVersions"],
        json!([proto::PROTOCOL_VERSION]),
        "a server advertises what it speaks"
    );
    assert_eq!(result["resultType"], json!(proto::RESULT_COMPLETE));
    assert!(result["capabilities"]["tools"].is_object());

    let refused = env
        .handle(mcp_request(2, proto::M_TOOLS_LIST, json!({}), None))
        .await
        .expect("tools/list answers");
    assert_eq!(refused.status, 401, "{}", error_text(&refused));
    let sent = refused
        .headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("www-authenticate"))
        .map(|(_, value)| value.clone())
        .expect("the challenge is a real response header");
    assert!(sent.contains("resource_metadata"), "{sent}");
    let body = body_json(&refused);
    let challenge = body["www_authenticate"]
        .as_str()
        .expect("the challenge also travels in the body for one release");
    assert_eq!(
        sent, challenge,
        "the two carriages must not be able to disagree"
    );
    assert_eq!(
        body["resource_metadata"],
        json!(format!("https://gate.example{METADATA_PATH}")),
        "discovery must start somewhere a client can reach"
    );
    // A PROTOCOL refusal is not an authorization problem, so it gets no
    // challenge — RFC 9110 §11. `discover` is anonymous, so its 200 has none
    // either.
    assert!(
        !discovered
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("www-authenticate")),
        "an answered call carries no challenge"
    );

    });
}

/// **A minted token lists the server's tools.**
#[test]
 fn severe_a_minted_token_reaches_the_tool_surface() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let ana = Actor::new("mcp-ana");
    let token = mint_token(&env, &ana).await;

    let listed = env
        .handle(mcp_request(1, proto::M_TOOLS_LIST, json!({}), Some(&token)))
        .await
        .expect("tools/list answers");
    assert_eq!(listed.status, 200, "{}", error_text(&listed));
    let tools = body_json(&listed)["result"]["tools"]
        .as_array()
        .expect("a tools array")
        .clone();
    let names: Vec<&str> = tools.iter().filter_map(|t| t["name"].as_str()).collect();
    assert!(names.contains(&"buzz_search"), "{names:?}");
    assert!(names.contains(&"buzz_read_channel"), "{names:?}");
    // READ-ONLY by construction: the relay holds no member's signing key, so a
    // write could not be attributed to anybody. Asserted so a future write tool
    // cannot arrive without somebody deciding whose voice it speaks in.
    for tool in &tools {
        let name = tool["name"].as_str().unwrap_or_default();
        assert!(
            name.starts_with("buzz_search") || name.starts_with("buzz_read"),
            "{name} is not a read tool; who would a write be attributed to?"
        );
    }

    });
}

/// **A tool call resolves rooms against the TOKEN'S SUBJECT.**
///
/// The property the whole design rests on: an MCP caller sees exactly the rooms
/// that identity may see. Ana finds her own message; Bo, holding his own token,
/// does not find it in her private room.
#[test]
 fn severe_an_mcp_search_sees_only_the_subjects_own_rooms() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let ana = Actor::new("mcp-owner");
    let bo = Actor::new("mcp-outsider");

    // A PRIVATE room only Ana is in.
    seed_channel_with_owner(&env, "secrets", Visibility::Private, &ana).await;
    let secret = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "secrets")],
        "the zarquon protocol ships friday",
    );
    assert_eq!(submit(&env, &ana, &secret).await.status, 200);

    // The tool returns its payload as JSON inside a text part, so the assertion is
    // on the EVENTS. A substring check would pass vacuously: the reply echoes the
    // terms it parsed, so `contains("zarquon")` is true even for a caller who
    // matched nothing — which made the member's half of this test unfailable too.
    let search_ids = |token: String| {
        let env = &env;
        async move {
            let reply = env
                .handle(mcp_request(
                    1,
                    proto::M_TOOLS_CALL,
                    json!({ "name": "buzz_search", "arguments": { "query": "zarquon" } }),
                    Some(&token),
                ))
                .await
                .expect("tools/call answers");
            assert_eq!(reply.status, 200, "{}", error_text(&reply));
            let result = body_json(&reply)["result"].clone();
            assert_eq!(result["isError"], json!(false), "{result}");
            let text = result["content"][0]["text"]
                .as_str()
                .expect("tool text")
                .to_string();
            let payload: Value =
                ferrite_app::serde_json::from_str(&text).expect("the tool text is JSON");
            payload["events"]
                .as_array()
                .expect("an events array")
                .iter()
                .filter_map(|e| e["id"].as_str().map(str::to_string))
                .collect::<Vec<String>>()
        }
    };

    let secret_id = buzz::relay::crypto::to_hex(&secret.id);

    let ana_token = mint_token(&env, &ana).await;
    let mine = search_ids(ana_token).await;
    assert!(
        mine.contains(&secret_id),
        "a member must find their own room's message: {mine:?}"
    );

    let bo_token = mint_token(&env, &bo).await;
    let theirs = search_ids(bo_token).await;
    assert!(
        !theirs.contains(&secret_id),
        "a private room's message must not surface to a non-member — the index row \
         exists either way, so only the per-hit gate stops this: {theirs:?}"
    );

    });
}

/// **A private channel read answers empty rather than refusing.**
///
/// The same rule the HTTP window route follows: a reader who cannot see a channel
/// gets an empty window, not a signal that it exists.
#[test]
 fn severe_an_mcp_channel_read_reveals_no_room_it_may_not_see() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let ana = Actor::new("mcp-read-owner");
    let bo = Actor::new("mcp-read-outsider");
    seed_channel_with_owner(&env, "backroom", Visibility::Private, &ana).await;
    let message = signed(
        &ana,
        kind::KIND_STREAM_MESSAGE,
        vec![tag("h", "backroom")],
        "only members see this",
    );
    assert_eq!(submit(&env, &ana, &message).await.status, 200);

    let token = mint_token(&env, &bo).await;
    let reply = env
        .handle(mcp_request(
            1,
            proto::M_TOOLS_CALL,
            json!({ "name": "buzz_read_channel", "arguments": { "channel": "backroom" } }),
            Some(&token),
        ))
        .await
        .expect("tools/call answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let text = body_json(&reply)["result"]["content"][0]["text"]
        .as_str()
        .expect("tool text")
        .to_string();
    assert!(
        !text.contains("only members see this"),
        "a non-member must not read a private room: {text}"
    );
    assert!(
        text.contains("\"events\":[]"),
        "an unreadable room is an EMPTY window, not a refusal that confirms it \
         exists: {text}"
    );

    });
}

/// **A header that disagrees with the body is a protocol error, not an
/// authorization one.**
///
/// 400 with a JSON-RPC `-32020` and no challenge: pointing a client at the
/// authorization server would send it to fix the wrong thing.
#[test]
 fn severe_a_protocol_version_mismatch_is_a_400_with_no_challenge() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;

    let mut request = mcp_request(1, proto::M_DISCOVER, json!({}), None);
    // ONLY the version disagrees: the mirrored method header is still correct, so
    // the -32020 can only be attributed to the version. Replacing the whole header
    // set would also drop `Mcp-Method`, and an absent mirrored header produces the
    // same code — the assertion would hold while testing something else.
    request.headers = vec![
        (
            proto::HEADER_PROTOCOL_VERSION.to_string(),
            proto::VERSION_2025_06_18.to_string(),
        ),
        (
            proto::HEADER_METHOD.to_string(),
            proto::M_DISCOVER.to_string(),
        ),
    ];
    let reply = env.handle(request).await.expect("the endpoint answers");
    assert_eq!(reply.status, 400, "{}", error_text(&reply));
    let body = body_json(&reply);
    assert_eq!(body["error"]["code"], json!(proto::HEADER_MISMATCH));
    assert!(
        body.get("www_authenticate").is_none(),
        "a malformed request is not an authorization problem: {body}"
    );
    assert!(
        !reply
            .headers
            .iter()
            .any(|(name, _)| name.eq_ignore_ascii_case("www-authenticate")),
        "and it carries no challenge header either"
    );

    });
}

/// **A mirrored header that disagrees with the body is refused.**
///
/// SEP-2243 puts the method and tool name in headers so a routing intermediary
/// can dispatch without parsing a body. That is exactly why a disagreement must
/// be refused rather than resolved in the body's favour: the intermediary already
/// routed on the header, so acting on the body means the request was handled
/// somewhere its own hop did not expect.
///
/// Found by the official conformance suite — the server answered 200 and served
/// the listing.
#[test]
 fn severe_a_mirrored_header_that_disagrees_with_the_body_is_refused() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let ana = Actor::new("mcp-headers");
    let token = mint_token(&env, &ana).await;

    // The header says prompts/list, the body says tools/list.
    let mut request = mcp_request(1, proto::M_TOOLS_LIST, json!({}), Some(&token));
    for header in &mut request.headers {
        if header.0 == proto::HEADER_METHOD {
            header.1 = proto::M_PROMPTS_LIST.to_string();
        }
    }
    let reply = env.handle(request).await.expect("the endpoint answers");
    assert_eq!(reply.status, 400, "{}", error_text(&reply));
    assert_eq!(
        body_json(&reply)["error"]["code"],
        json!(proto::HEADER_MISMATCH)
    );

    // A method name is case-SENSITIVE, so a folded value is a different route.
    let mut folded = mcp_request(2, proto::M_TOOLS_LIST, json!({}), Some(&token));
    for header in &mut folded.headers {
        if header.0 == proto::HEADER_METHOD {
            header.1 = "TOOLS/LIST".to_string();
        }
    }
    assert_eq!(
        env.handle(folded).await.expect("answers").status,
        400,
        "a folded method value must not be accepted"
    );

    // And a tools/call whose Mcp-Name names another tool.
    let mut wrong_name = mcp_request(
        3,
        proto::M_TOOLS_CALL,
        json!({ "name": "buzz_search", "arguments": { "query": "x" } }),
        Some(&token),
    );
    for header in &mut wrong_name.headers {
        if header.0 == proto::HEADER_NAME {
            header.1 = "buzz_read_channel".to_string();
        }
    }
    let reply = env.handle(wrong_name).await.expect("answers");
    assert_eq!(reply.status, 400, "{}", error_text(&reply));

    // The correct pair still works, so the rule is not simply refusing everything.
    let ok = env
        .handle(mcp_request(4, proto::M_TOOLS_LIST, json!({}), Some(&token)))
        .await
        .expect("answers");
    assert_eq!(ok.status, 200, "{}", error_text(&ok));

    });
}

/// **An unimplemented method is HTTP 404, not 200 with the error inside.**
///
/// SEP-2575 requires 404 together with `-32601`. It matters because an
/// intermediary reads the status and not the body, so the two are not
/// interchangeable however correct the JSON-RPC error is.
///
/// Found by the official conformance suite across six methods.
#[test]
 fn severe_an_unimplemented_method_is_a_404_carrying_method_not_found() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let ana = Actor::new("mcp-404");
    let token = mint_token(&env, &ana).await;

    // The methods this revision removed, plus one nobody ever defined.
    for method in [
        "initialize",
        "ping",
        "logging/setLevel",
        "resources/subscribe",
        "resources/unsubscribe",
        "unknown/method",
    ] {
        let reply = env
            .handle(mcp_request(1, method, json!({}), Some(&token)))
            .await
            .expect("the endpoint answers");
        assert_eq!(reply.status, 404, "{method}: {}", error_text(&reply));
        assert_eq!(
            body_json(&reply)["error"]["code"],
            json!(proto::METHOD_NOT_FOUND),
            "{method} must carry -32601 as well as the status"
        );
    }

    // And an unknown TOOL is a different thing: the method exists, its argument
    // does not, so it is invalid params — never `isError`, which would say a tool
    // ran and failed and invite a retry that cannot succeed.
    let reply = env
        .handle(mcp_request(
            2,
            proto::M_TOOLS_CALL,
            json!({ "name": "no_such_tool", "arguments": {} }),
            Some(&token),
        ))
        .await
        .expect("answers");
    assert_eq!(reply.status, 400, "{}", error_text(&reply));
    let body = body_json(&reply);
    assert_eq!(body["error"]["code"], json!(proto::INVALID_PARAMS));
    assert!(
        body.get("result").is_none(),
        "an unknown tool must not be reported as a tool result: {body}"
    );

    });
}

/// **A list result carries the caching hints a client needs (SEP-2549).**
///
/// Without them a client must choose between re-fetching the tool list on every
/// call and caching something it may not be entitled to keep. `public` is a claim
/// about THIS list: it is identical for every caller.
#[test]
 fn severe_the_tool_listing_says_how_long_it_may_be_cached() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let ana = Actor::new("mcp-caching");
    let token = mint_token(&env, &ana).await;

    let reply = env
        .handle(mcp_request(1, proto::M_TOOLS_LIST, json!({}), Some(&token)))
        .await
        .expect("answers");
    assert_eq!(reply.status, 200, "{}", error_text(&reply));
    let result = body_json(&reply)["result"].clone();
    let ttl = result["ttlMs"]
        .as_u64()
        .expect("a list result must say how long it stays valid");
    assert!(ttl > 0, "a zero ttl tells a client not to cache at all");
    assert_eq!(
        result["cacheScope"],
        json!("public"),
        "this tool list is the same for every caller, so it is shareable"
    );

    // Discovery carries them too, and always did.
    let discovered = env
        .handle(mcp_request(2, proto::M_DISCOVER, json!({}), None))
        .await
        .expect("answers");
    let d = body_json(&discovered)["result"].clone();
    assert!(d["ttlMs"].as_u64().is_some());
    assert_eq!(d["cacheScope"], json!("public"));

    });
}

/// **The metadata document is served at its derived path and nowhere else.**
#[test]
 fn severe_the_protected_resource_metadata_has_exactly_one_address() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;

    let served = env
        .handle(raw("GET", METADATA_PATH, Vec::new(), Vec::new()))
        .await
        .expect("the well-known route answers");
    assert_eq!(served.status, 200, "{}", error_text(&served));
    let doc = body_json(&served);
    assert_eq!(doc["resource"], json!(RESOURCE));
    assert_eq!(doc["authorization_servers"], json!([RESOURCE]));
    // No write scope is advertised, because no exposed tool writes.
    let scopes: Vec<&str> = doc["scopes_supported"]
        .as_array()
        .expect("scopes")
        .iter()
        .filter_map(Value::as_str)
        .collect();
    assert!(!scopes.contains(&"mcp:write"), "{scopes:?}");

    // Every lookalike is a 404. The route predicate lets some of these through,
    // so this is the handler's exact-path check being asserted.
    for hostile in [
        "/.well-known/oauth-protected-resource",
        "/.well-known/oauth-protected-resource/evil",
        "/.well-known/oauth-protected-resource/mcp/extra",
        "/.well-known/oauth-protected-resource-evil",
    ] {
        let reply = env
            .handle(raw("GET", hostile, Vec::new(), Vec::new()))
            .await
            .expect("the route answers");
        assert_eq!(
            reply.status,
            404,
            "{hostile} must not serve the metadata: {}",
            error_text(&reply)
        );
    }

    });
}

/// **Connecting a connector is an operator action.**
///
/// It grants every agent in the workspace access to an external account, so an
/// ordinary member is refused — 403, not 401: authenticating again cannot help.
#[test]
 fn severe_only_an_operator_may_connect_an_mcp_server() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let ana = Actor::new("mcp-member");

    let refused = env
        .handle(post(
            &ana,
            "/buzz/api/mcp/authorize",
            json!({ "connector": "linear" }).to_string().into_bytes(),
        ))
        .await
        .expect("the authorize route answers");
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("operator"),
        "the refusal should say whose action this is: {}",
        error_text(&refused)
    );

    // And an unauthenticated caller is 401.
    let anonymous = env
        .handle(raw(
            "POST",
            "/buzz/api/mcp/authorize",
            Vec::new(),
            json!({ "connector": "linear" }).to_string().into_bytes(),
        ))
        .await
        .expect("the authorize route answers");
    assert_eq!(anonymous.status, 401, "{}", error_text(&anonymous));

    });
}

/// **Calling a connector's tools is an operator action too.**
///
/// A call runs with the workspace's credential for the external account, which
/// is the authority that makes connecting operator-only. A member used to reach
/// it through the write gate alone, which on an open community is any account
/// holder. Listing tools stays a member read; calling one needs an operator.
#[test]
 fn severe_only_an_operator_may_call_a_connectors_tools() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let ana = Actor::new("mcp-caller");
    let root = Actor::new("mcp-operator");
    seed_operator(&env, &root).await;
    let call = json!({ "connector": "linear", "tool": "create_issue", "arguments": {} })
        .to_string()
        .into_bytes();

    let refused = env
        .handle(post(&ana, "/buzz/api/mcp/call", call.clone()))
        .await
        .expect("the call route answers");
    assert_eq!(refused.status, 403, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("operator"),
        "the refusal should say whose action this is: {}",
        error_text(&refused)
    );

    // The operator gets past the authority check; this fixture grants no egress,
    // so the call then refuses for THAT reason instead.
    let operator = env
        .handle(post(&root, "/buzz/api/mcp/call", call))
        .await
        .expect("the call route answers");
    assert_ne!(operator.status, 403, "{}", error_text(&operator));
    assert!(
        error_text(&operator).contains("mcp-egress"),
        "the operator's call should reach the egress check: {}",
        error_text(&operator)
    );

    });
}

/// **The catalog reports whether the CALLER may act on it, and the route agrees.**
///
/// The settings surface disables Connect on this field rather than letting
/// somebody find out by pressing it. That makes the field a claim about what the
/// authorize route will do — so it is checked against the route, in both
/// directions. Asserting only that an operator sees `true` would pass equally well
/// if the field were hardcoded.
#[test]
 fn severe_the_connector_catalog_says_whether_the_caller_may_connect() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let op = Actor::new("mcp-operator");
    let member = Actor::new("mcp-plain-member");
    seed_operator(&env, &op).await;

    // A plain member is told no, and IS refused. 403 rather than 401: they
    // authenticated fine, and authenticating again cannot help.
    let theirs = read_catalog(&env, &member).await;
    assert_eq!(
        theirs["operator"],
        json!(false),
        "a member must not be offered an enabled control: {theirs}"
    );
    let refused = attempt_authorize(&env, &member, "linear").await;
    assert_eq!(refused.status, 403, "{}", error_text(&refused));

    // The operator is told yes, and their request gets PAST the authority check.
    // It cannot SUCCEED in this fixture, and what it fails on is itself a property
    // worth holding: configuration is resolved before anything is dialled, so an
    // operator whose deployment lacks a client id is told THAT — the cheaper,
    // entirely local problem — rather than being sent to edit an egress manifest
    // that was never the issue.
    let ours = read_catalog(&env, &op).await;
    assert_eq!(ours["operator"], json!(true), "{ours}");
    let unconfigured = attempt_authorize(&env, &op, "linear").await;
    assert_ne!(
        unconfigured.status,
        403,
        "an operator told `true` must not then be refused as unauthorized: {}",
        error_text(&unconfigured)
    );
    assert!(
        error_text(&unconfigured).contains("buzz-mcp-client-id"),
        "an unconfigured client id is the FIRST thing to report, and by name: {}",
        error_text(&unconfigured)
    );

    // Configured, the same request reaches the dial and names the missing GRANT.
    // Both refusals are ones only an operator can reach, so either proves the
    // correspondence; asserting the sequence also pins which is reported when.
    seed_mcp_client_id(&env).await;
    let attempted = attempt_authorize(&env, &op, "linear").await;
    assert_ne!(attempted.status, 403, "{}", error_text(&attempted));
    assert!(
        error_text(&attempted).contains("mcp-egress"),
        "once configured, the attempt should fail on the missing grant, naming it: {}",
        error_text(&attempted)
    );

    // The two fields that decide which instruction a row carries. Collapsing them
    // sends somebody to the wrong fix: not connected means press Connect, not
    // reachable means nobody declared that grant and no button here can help.
    for row in ours["connectors"].as_array().expect("rows") {
        assert!(
            row["egress"].as_str().is_some_and(|h| !h.is_empty()),
            "every row names the grant that covers it: {row}"
        );
        assert!(row["reachable"].is_boolean(), "{row}");
        assert!(row["connected"].is_boolean(), "{row}");
    }

    });
}

/// **A deployment with no `mcp-egress` handle says so.**
///
/// This fixture binds no egress, which is exactly a deployment that has not been
/// granted one. The connector routes must answer with that fact rather than
/// panicking or reporting an empty catalog — an operator reading "no connectors"
/// would go looking for the wrong thing.
#[test]
 fn severe_without_an_egress_grant_the_connector_routes_refuse_honestly() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    seed_mcp_config(&env).await;
    let ana = Actor::new("mcp-nogress");

    // The catalog READ still works — a client needs it to render the settings
    // surface — and it reports the missing grant instead of implying a choice.
    let (_, header) = fresh_auth(&ana, "GET", "/buzz/api/mcp/connectors");
    let listed = env
        .handle(request_with_auth(
            "GET",
            "/buzz/api/mcp/connectors",
            Some(&header),
            Vec::new(),
        ))
        .await
        .expect("the connectors route answers");
    assert_eq!(listed.status, 200, "{}", error_text(&listed));
    let body = body_json(&listed);
    assert_eq!(
        body["egress_available"],
        json!(false),
        "this fixture grants no egress, and the reply must say so: {body}"
    );
    assert!(
        !body["connectors"].as_array().expect("rows").is_empty(),
        "the catalog is still described — an empty list would read as 'no such \
         thing' rather than 'not granted here'"
    );
    for row in body["connectors"].as_array().expect("rows") {
        assert_eq!(row["connected"], json!(false));
        let provenance = row["provenance"].as_str().unwrap_or_default();
        assert!(
            provenance == "documented" || provenance == "community",
            "an operator must be able to see which endpoints are verified: {row}"
        );
    }
    // And the providers that CANNOT work are named with their reason, rather than
    // being silently absent.
    let unavailable = body["unavailable"].as_array().expect("unavailable rows");
    assert!(
        unavailable.iter().any(|u| u["id"] == json!("figma")),
        "a provider with no reachable endpoint should say why: {unavailable:?}"
    );

    // A tool listing needs the handle, so it refuses — naming the spec, which is
    // where the fix is.
    let refused = env
        .handle(post(
            &ana,
            "/buzz/api/mcp/tools",
            json!({ "connector": "linear" }).to_string().into_bytes(),
        ))
        .await
        .expect("the tools route answers");
    assert_ne!(refused.status, 200, "{}", error_text(&refused));
    assert!(
        error_text(&refused).contains("mcp-egress"),
        "the refusal should name the missing binding: {}",
        error_text(&refused)
    );

    });
}

/// **An unconfigured deployment refuses rather than inventing an identity.**
///
/// The resource identifier is the audience of every token and half of every
/// credential key, so a deployment that has not chosen one must not have one
/// chosen for it.
#[test]
 fn severe_an_unconfigured_resource_identifier_fails_closed() {
    common::run_scenario(|| async move {
    let env = gate_env().await;
    // Deliberately NOT seeded.
    let reply = env
        .handle(mcp_request(1, proto::M_DISCOVER, json!({}), None))
        .await
        .expect("the endpoint answers");
    assert_ne!(
        reply.status,
        200,
        "an unconfigured MCP surface must not serve: {}",
        error_text(&reply)
    );
    assert!(
        error_text(&reply).contains("buzz-mcp-resource"),
        "the refusal should name the row an operator must set: {}",
        error_text(&reply)
    );

    });
}
