//! The remote ACP binding: HTTP POST against a deployed workspace's `/acp`.
//!
//! A local harness is a process this shell owns; a remote agent is a service it
//! calls. The protocol is identical either way — [`crate::acp`] builds the same
//! messages — so what lives here is only what the TRANSPORT adds: where a request
//! goes, which credential it carries, and how a connection is identified across
//! calls.
//!
//! Transcribed from the port's own binding (`buzz/src/acp/transport.rs`), because
//! none of it follows from the protocol: `initialize` creates a connection and
//! answers with `Acp-Connection-Id`, every later call echoes that header, and a
//! POST-only host reads the session from `params.sessionId` rather than from a
//! header. Getting one of those wrong routes a message to the wrong reader
//! instead of failing loudly, which is why they are pinned by tests here.
//!
//! Pure: nothing in this module performs I/O. It builds a [`HttpCall`] and reads
//! a reply that somebody else fetched, so the whole binding is testable with no
//! network.

use serde_json::Value;

use crate::acp::{self, Message};
use crate::harness::{validate_endpoint, RemoteAuth};
use crate::{Error, Result};

pub const HEADER_CONNECTION_ID: &str = "acp-connection-id";
pub const JSON_MIME: &str = "application/json";
/// The binding's cap on one body, from its own documentation.
pub const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// How many permission rounds one prompt may interleave before this shell stops.
///
/// A bound rather than a loop: an agent that answered every permission with
/// another permission request would otherwise hold a thread and a dialog forever.
pub const MAX_PERMISSION_ROUNDS: usize = 32;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpCall {
    pub url: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    /// Recorded at construction rather than inferred at redaction time, because
    /// [`RemoteAuth::Header`] lets a harness name its header anything: a
    /// name-matching heuristic would redact `x-api-key` and leave `X-Auth` or
    /// `Cookie` in the clear.
    credential_header: Option<String>,
}

impl HttpCall {
    #[must_use]
    pub fn credential_header(&self) -> Option<&str> {
        self.credential_header.as_deref()
    }

    /// The same call with the credential replaced — what a log line gets.
    ///
    /// The redacted header is the one the call was BUILT with, plus
    /// `authorization` unconditionally: belt and braces for a call assembled by
    /// some future path that forgot to record it.
    #[must_use]
    pub fn redacted(&self) -> Self {
        Self {
            url: self.url.clone(),
            body: self.body.clone(),
            credential_header: self.credential_header.clone(),
            headers: self
                .headers
                .iter()
                .map(|(name, value)| {
                    let is_credential = name.eq_ignore_ascii_case("authorization")
                        || self
                            .credential_header
                            .as_deref()
                            .is_some_and(|carrier| name.eq_ignore_ascii_case(carrier));
                    if is_credential {
                        (name.clone(), "<redacted>".to_string())
                    } else {
                        (name.clone(), value.clone())
                    }
                })
                .collect(),
        }
    }
}

/// The header one auth configuration puts a resolved secret in.
///
/// `None` for [`RemoteAuth::None`] — a loopback development endpoint with no
/// credential is a legitimate configuration, and inventing an empty bearer for it
/// would send `Authorization: Bearer ` and be refused for the wrong reason.
#[must_use]
pub fn auth_header(auth: &RemoteAuth, secret: &str) -> Option<(String, String)> {
    match auth {
        RemoteAuth::None => None,
        RemoteAuth::Bearer { .. } => {
            Some(("authorization".to_string(), format!("Bearer {secret}")))
        }
        RemoteAuth::Header { name, .. } => Some((name.to_ascii_lowercase(), secret.to_string())),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteConnection {
    endpoint: String,
    connection_id: Option<String>,
    session_id: Option<String>,
    binding_meta: bool,
    next_id: u64,
}

impl RemoteConnection {
    pub fn new(endpoint: &str) -> Result<Self> {
        validate_endpoint(endpoint)?;
        Ok(Self {
            endpoint: endpoint.trim_end_matches('/').to_string(),
            connection_id: None,
            session_id: None,
            binding_meta: false,
            next_id: 1,
        })
    }

    #[must_use]
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    #[must_use]
    pub fn connection_id(&self) -> Option<&str> {
        self.connection_id.as_deref()
    }

    #[must_use]
    pub fn session_id(&self) -> Option<&str> {
        self.session_id.as_deref()
    }

    #[must_use]
    pub const fn binding_meta(&self) -> bool {
        self.binding_meta
    }

    pub fn next_request_id(&mut self) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }

    /// Build one call. `secret` is already resolved — this module never reads a
    /// credential store, so a test can exercise the whole binding.
    pub fn call(&self, message: &Value, secret: Option<(String, String)>) -> Result<HttpCall> {
        let body = serde_json::to_vec(message)
            .map_err(|e| Error::Internal(format!("could not encode an ACP message: {e}")))?;
        if body.len() > MAX_BODY_BYTES {
            return Err(Error::Invalid(format!(
                "that ACP request is over the binding's {MAX_BODY_BYTES}-byte limit"
            )));
        }
        let mut headers = vec![("content-type".to_string(), JSON_MIME.to_string())];
        let mut credential_header = None;
        if let Some((name, value)) = secret {
            credential_header = Some(name.clone());
            headers.push((name, value));
        }
        // Echoed on every call AFTER initialize, which is what ties a session to
        // the connection that created it.
        if let Some(connection) = &self.connection_id {
            headers.push((HEADER_CONNECTION_ID.to_string(), connection.clone()));
        }
        Ok(HttpCall {
            url: self.endpoint.clone(),
            headers,
            body,
            credential_header,
        })
    }

    /// Learn the connection id from an `initialize` answer.
    ///
    /// The RESPONSE HEADER is authoritative when the host sent one; the result's
    /// `_meta` is the documented fallback. Both are accepted because the port's
    /// own binding describes the header while the design records the `_meta` form,
    /// and a client that insisted on one would fail against a host using the
    /// other.
    pub fn absorb_initialize(&mut self, header: Option<&str>, result: &Value) {
        if let Some(id) = header.map(str::trim).filter(|id| !id.is_empty()) {
            self.connection_id = Some(id.to_string());
        } else if let Some(id) = result
            .get("_meta")
            .and_then(|meta| meta.get("connectionId"))
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
        {
            self.connection_id = Some(id.to_string());
        }
        self.binding_meta = acp::negotiated_binding_meta(result);
    }

    pub fn absorb_session(&mut self, result: &Value) -> Result<()> {
        let Some(session) = result.get("sessionId").and_then(Value::as_str) else {
            return Err(Error::Invalid(
                "the agent's session answer carries no sessionId".into(),
            ));
        };
        self.session_id = Some(session.to_string());
        Ok(())
    }
}

/// Read one HTTP reply into the messages it carries.
///
/// A status is classified rather than flattened, because the three cases ask the
/// user for different things: 401/403 is a CREDENTIAL problem (final — retrying
/// the same token cannot help), any other non-2xx is a transport fault (worth a
/// retry), and a 2xx body is the protocol's own answer, which may itself be an
/// error response.
///
/// A batch (JSON array) is accepted because the binding allows one: a prompt
/// whose answer interleaves a permission request arrives that way.
pub fn parse_reply(status: u16, body: &[u8]) -> Result<Vec<Message>> {
    if status == 401 || status == 403 {
        return Err(Error::Refused(format!(
            "the agent endpoint refused this shell's credential ({status}). Check the token \
             stored for this harness."
        )));
    }
    if !(200..300).contains(&status) {
        return Err(Error::Tool {
            program: "acp".to_string(),
            detail: format!("the agent endpoint answered {status}"),
        });
    }
    if body.len() > MAX_BODY_BYTES {
        return Err(Error::Invalid(format!(
            "the agent's reply is over the binding's {MAX_BODY_BYTES}-byte limit"
        )));
    }
    // An EMPTY 2xx is a legitimate answer to a notification, and it is not a
    // parse failure: a notification has no response.
    if body.iter().all(u8::is_ascii_whitespace) {
        return Ok(Vec::new());
    }
    let value: Value = serde_json::from_slice(body)
        .map_err(|e| Error::Invalid(format!("the agent's reply is not JSON: {e}")))?;
    match value {
        Value::Array(rows) => rows.iter().map(acp::parse_value).collect(),
        other => Ok(vec![acp::parse_value(&other)?]),
    }
}

#[must_use]
pub fn permission_answer(id: &Value, option_id: Option<&str>) -> Value {
    acp::permission_response(id, option_id)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owed {
    pub id: Value,
    pub method: String,
    pub params: Value,
}

/// What one reply asks the driver to do next.
///
/// A reply can carry several agent requests, and answering only the first
/// leaves the agent waiting on the rest forever.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Plan {
    pub owed: Vec<Owed>,
    pub answered: bool,
    pub updates: Vec<Value>,
    pub failure: Option<String>,
}

#[must_use]
pub fn plan_reply(messages: &[Message], request_id: u64) -> Plan {
    let mut plan = Plan::default();
    for message in messages {
        match message {
            Message::Response { id, result } => {
                plan.updates.push(result.clone());
                if id.as_u64() == Some(request_id) {
                    plan.answered = true;
                }
            }
            Message::Failure { id, message, .. } => {
                if id.as_u64() == Some(request_id) {
                    plan.answered = true;
                    plan.failure = Some(message.clone());
                } else {
                    // A failure answering something ELSE (one of our permission
                    // answers, say) is worth showing and is not the end of the
                    // turn.
                    plan.updates.push(Value::String(message.clone()));
                }
            }
            Message::Notification { method, params } => {
                plan.updates.push(
                    serde_json::json!({ "method": method, "params": params }),
                );
            }
            Message::Request { id, method, params } => plan.owed.push(Owed {
                id: id.clone(),
                method: method.clone(),
                params: params.clone(),
            }),
        }
    }
    plan
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn connection() -> RemoteConnection {
        RemoteConnection::new("https://buzz.example/acp").expect("endpoint")
    }

    #[test]
    fn an_endpoint_is_validated_by_the_same_rule_the_config_uses() {
        assert!(RemoteConnection::new("https://buzz.example/acp").is_ok());
        assert!(RemoteConnection::new("http://127.0.0.1:9000/acp").is_ok());
        // The WebSocket refusal, reached through this constructor too.
        let refused = RemoteConnection::new("wss://buzz.example/acp").expect_err("refused");
        assert!(refused.to_string().contains("WebSocket binding is not available"), "{refused}");
        assert!(RemoteConnection::new("http://buzz.example/acp").is_err());
    }

    #[test]
    fn initialize_carries_no_connection_id_and_later_calls_do() {
        let mut live = connection();
        let id = live.next_request_id();
        let call = live.call(&acp::initialize_request(id), None).expect("call");
        assert_eq!(call.url, "https://buzz.example/acp");
        assert!(
            !call.headers.iter().any(|(name, _)| name == HEADER_CONNECTION_ID),
            "there is no connection to name until initialize answers"
        );

        live.absorb_initialize(Some("conn-7"), &json!({}));
        assert_eq!(live.connection_id(), Some("conn-7"));
        let next = live.next_request_id();
        let call = live
            .call(&acp::session_new_request(next, "/tmp/repo", None), None)
            .expect("call");
        assert_eq!(
            call.headers
                .iter()
                .find(|(name, _)| name == HEADER_CONNECTION_ID)
                .map(|(_, value)| value.as_str()),
            Some("conn-7")
        );
    }

    #[test]
    fn a_connection_id_falls_back_to_the_results_meta() {
        // The header is authoritative, but a host that answers only in `_meta`
        // must still be usable — and a client that insisted on one form would
        // fail against the other.
        let mut from_meta = connection();
        from_meta.absorb_initialize(None, &json!({ "_meta": { "connectionId": "from-meta" } }));
        assert_eq!(from_meta.connection_id(), Some("from-meta"));

        let mut from_header = connection();
        from_header.absorb_initialize(
            Some("from-header"),
            &json!({ "_meta": { "connectionId": "from-meta" } }),
        );
        assert_eq!(from_header.connection_id(), Some("from-header"));

        // Neither: the connection is simply unidentified, which is not fatal for
        // a host that does not use them.
        let mut neither = connection();
        neither.absorb_initialize(Some("   "), &json!({}));
        assert_eq!(neither.connection_id(), None);
    }

    #[test]
    fn the_binding_extension_is_learned_from_initialize() {
        let mut negotiated = connection();
        assert!(!negotiated.binding_meta());
        negotiated.absorb_initialize(
            None,
            &json!({
                "agentCapabilities": {
                    "_meta": { acp::META_PROJECT_BINDING: { "version": 1 } }
                }
            }),
        );
        assert!(negotiated.binding_meta());
    }

    #[test]
    fn a_session_id_is_learned_or_refused() {
        let mut live = connection();
        live.absorb_session(&json!({ "sessionId": "sess-1" }))
            .expect("session");
        assert_eq!(live.session_id(), Some("sess-1"));
        assert!(live.absorb_session(&json!({})).is_err());
    }

    #[test]
    fn credentials_ride_in_a_header_and_never_in_the_url() {
        let connection = connection();
        let bearer = auth_header(
            &RemoteAuth::Bearer {
                secret_ref: "cloud".into(),
            },
            "tok-abc",
        )
        .expect("header");
        assert_eq!(bearer.0, "authorization");
        assert_eq!(bearer.1, "Bearer tok-abc");

        let custom = auth_header(
            &RemoteAuth::Header {
                name: "X-Api-Key".into(),
                secret_ref: "cloud".into(),
            },
            "tok-abc",
        )
        .expect("header");
        assert_eq!(custom.0, "x-api-key", "header names are lowercased");
        assert_eq!(custom.1, "tok-abc");

        // No credential is a legitimate loopback configuration — and must not
        // become an empty bearer, which would be refused for the wrong reason.
        assert!(auth_header(&RemoteAuth::None, "").is_none());

        let call = connection
            .call(&acp::initialize_request(1), Some(bearer))
            .expect("call");
        assert!(!call.url.contains("tok-abc"));
        assert!(!String::from_utf8_lossy(&call.body).contains("tok-abc"));
    }

    #[test]
    fn redaction_hides_the_credential_however_it_is_named() {
        let connection = connection();
        // The conventional name.
        let call = connection
            .call(
                &acp::initialize_request(1),
                Some(("x-api-key".to_string(), "tok-abc".to_string())),
            )
            .expect("call")
            .redacted();
        assert!(!format!("{call:?}").contains("tok-abc"), "{call:?}");

        // The names a heuristic would MISS — `RemoteAuth::Header` accepts any
        // header, so redaction cannot rest on what it is called.
        for name in ["x-auth", "cookie", "proxy-authorization", "x-buzz"] {
            let call = connection
                .call(
                    &acp::initialize_request(1),
                    Some((name.to_string(), "tok-abc".to_string())),
                )
                .expect("call");
            assert_eq!(call.credential_header(), Some(name));
            let redacted = call.redacted();
            assert!(
                !format!("{redacted:?}").contains("tok-abc"),
                "{name} leaked the credential: {redacted:?}"
            );
            assert!(
                redacted
                    .headers
                    .iter()
                    .any(|(header, value)| header == name && value == "<redacted>"),
                "{name} should be replaced"
            );
        }

        // A content type is not a secret and stays readable.
        let call = connection
            .call(&acp::initialize_request(1), None)
            .expect("call")
            .redacted();
        assert!(call
            .headers
            .iter()
            .any(|(name, value)| name == "content-type" && value == JSON_MIME));
        assert_eq!(call.credential_header(), None);
    }

    #[test]
    fn a_credential_failure_is_told_apart_from_a_transport_fault() {
        for status in [401u16, 403] {
            let err = parse_reply(status, b"{}").expect_err("refused");
            assert!(matches!(err, Error::Refused(_)), "{status}: {err:?}");
            assert!(err.is_final(), "a wrong token cannot be fixed by retrying");
            assert!(err.to_string().contains("token"), "{err}");
        }
        for status in [500u16, 502, 429] {
            let err = parse_reply(status, b"{}").expect_err("transport");
            assert!(matches!(err, Error::Tool { .. }), "{status}: {err:?}");
            assert!(!err.is_final(), "{status} is worth retrying");
        }
    }

    #[test]
    fn a_reply_carries_one_message_or_a_batch() {
        let one = parse_reply(200, br#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#)
            .expect("parse");
        assert_eq!(one.len(), 1);
        assert!(matches!(one[0], Message::Response { .. }));

        // The case this exists for: a prompt whose answer interleaves a
        // permission REQUEST from the agent.
        let batch = parse_reply(
            200,
            br#"[{"jsonrpc":"2.0","method":"session/update","params":{}},
                 {"jsonrpc":"2.0","id":9,"method":"session/request_permission","params":{}}]"#,
        )
        .expect("parse");
        assert_eq!(batch.len(), 2);
        assert!(matches!(batch[0], Message::Notification { .. }));
        assert!(matches!(batch[1], Message::Request { .. }));

        // An empty 2xx answers a notification and is not a parse failure.
        assert!(parse_reply(200, b"").expect("empty").is_empty());
        assert!(parse_reply(200, b"   \n").expect("blank").is_empty());

        assert!(parse_reply(200, b"not json").is_err());
    }

    #[test]
    fn request_ids_are_monotonic_within_a_connection() {
        let mut live = connection();
        let ids: Vec<u64> = (0..5).map(|_| live.next_request_id()).collect();
        assert_eq!(ids, vec![1, 2, 3, 4, 5]);
    }

    #[test]
    fn an_oversized_request_is_refused_before_it_is_sent() {
        let connection = connection();
        let huge = json!({ "text": "x".repeat(MAX_BODY_BYTES + 1) });
        assert!(connection.call(&huge, None).is_err());
    }

    #[test]
    fn every_request_in_one_reply_is_owed_an_answer() {
        // The bug this pins: a reply can carry SEVERAL agent requests, and
        // answering only the first leaves the agent waiting on the rest — which
        // on a POST-only binding means waiting forever.
        let messages = vec![
            Message::Request {
                id: json!(7),
                method: "session/request_permission".into(),
                params: json!({ "sessionId": "s1" }),
            },
            Message::Notification {
                method: "session/update".into(),
                params: json!({ "text": "working" }),
            },
            Message::Request {
                id: json!(8),
                method: "fs/read_text_file".into(),
                params: json!({ "path": "/tmp/x" }),
            },
        ];
        let plan = plan_reply(&messages, 1);
        assert_eq!(plan.owed.len(), 2, "both requests are owed answers");
        assert_eq!(plan.owed[0].id, json!(7));
        assert_eq!(plan.owed[1].method, "fs/read_text_file");
        assert_eq!(plan.updates.len(), 1);
        assert!(!plan.answered);
    }

    #[test]
    fn a_plan_knows_when_our_own_prompt_was_answered() {
        let ours = plan_reply(
            &[Message::Response {
                id: json!(4),
                result: json!({ "stopReason": "end_turn" }),
            }],
            4,
        );
        assert!(ours.answered);
        assert!(ours.failure.is_none());
        assert_eq!(ours.updates.len(), 1);

        // Somebody ELSE's answer is not ours.
        let theirs = plan_reply(
            &[Message::Response {
                id: json!(9),
                result: json!({}),
            }],
            4,
        );
        assert!(!theirs.answered);

        // An error answering OUR request ends the turn and carries its reason.
        let failed = plan_reply(
            &[Message::Failure {
                id: json!(4),
                code: -32000,
                message: "no model configured".into(),
            }],
            4,
        );
        assert!(failed.answered);
        assert_eq!(failed.failure.as_deref(), Some("no model configured"));

        // An error answering something else does NOT end it.
        let other = plan_reply(
            &[Message::Failure {
                id: json!(11),
                code: -32000,
                message: "that permission answer was late".into(),
            }],
            4,
        );
        assert!(!other.answered);
        assert_eq!(other.updates.len(), 1);
    }
}
