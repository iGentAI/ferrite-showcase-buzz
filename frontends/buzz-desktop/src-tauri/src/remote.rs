//! Driving a REMOTE ACP agent: the HTTP half of the harness surface.
//!
//! A local harness is a process this shell owns and speaks to over pipes; a
//! remote agent is a service it calls. [`buzz_desktop_core::remote`] builds the
//! calls and reads the replies — all of it pure and tested — so what lives here
//! is the I/O and one loop.
//!
//! The loop is the interesting part. The binding is POST-only, which means an
//! agent-initiated request ("may I run this tool?") arrives as part of the REPLY
//! to something we sent, and its answer has to ride a new POST. So a prompt is
//! not one exchange but a short conversation, and every turn of it goes through
//! the SAME fail-closed broker the local path uses — one implementation of
//! "nobody answered means no", not two.

use std::collections::VecDeque;
use std::io::Read;
use std::time::Duration;

use buzz_desktop_core::acp::{self, AnswerBuffer, Message, PermissionRequest};
use buzz_desktop_core::broker;
use buzz_desktop_core::credentials::{Key, Store};
use buzz_desktop_core::harness::{Harness, RemoteAuth};
use buzz_desktop_core::nostr::AnswerTarget;
use buzz_desktop_core::remote::{self, HttpCall, RemoteConnection};
use serde_json::{json, Value};
use tauri::{AppHandle, Emitter, Manager};

use crate::state::AppState;
use crate::supervisor::{self, Decided, EVENT_AGENT_UPDATE};

/// How long past the broker's own deadline this driver waits before refusing on
/// the agent's behalf.
///
/// A GRACE rather than a second deadline: the broker's sweep runs every couple of
/// seconds, so waiting a little longer lets IT resolve the request and keeps one
/// authority over the answer. Past this, the driver sweeps and refuses itself,
/// because a remote agent's refusal cannot be delivered any other way.
const PERMISSION_GRACE_MS: u64 = 3_000;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(15);
/// Generous because a prompt is a model call; bounded so a hung endpoint cannot
/// hold a thread forever.
const CALL_TIMEOUT: Duration = Duration::from_secs(300);

fn http() -> ureq::Agent {
    ureq::AgentBuilder::new()
        .timeout_connect(CONNECT_TIMEOUT)
        .timeout(CALL_TIMEOUT)
        // Never follow a redirect: ureq would replay custom credential headers
        // and the connection id to the new location, across origins and from
        // https to http, and a 307/308 re-sends the prompt body there too. A
        // 3xx is answered as a non-2xx reply (finding_0e1ab595684a13c8).
        .redirects(0)
        .build()
}

struct Reply {
    connection_id: Option<String>,
    messages: Vec<Message>,
}

/// POST one call and read its reply.
///
/// A non-2xx is NOT treated as a transport failure here: it is a reply with a
/// status, and `remote::parse_reply` is what decides whether it means "your
/// credential is wrong" (final) or "the host is unwell" (retryable). Collapsing
/// them would tell a user to check their token because a gateway restarted.
fn post(call: &HttpCall) -> Result<Reply, String> {
    let mut request = http().post(&call.url);
    for (name, value) in &call.headers {
        request = request.set(name, value);
    }
    let (status, response) = match request.send_bytes(&call.body) {
        Ok(response) => (response.status(), response),
        Err(ureq::Error::Status(status, response)) => (status, response),
        // A genuine transport failure. The URL is what the message quotes; it
        // never carries the credential (that rides a header), and redacting it
        // directly spares cloning a request body that can run to megabytes just
        // to read one field.
        Err(e) => {
            return Err(format!(
                "the agent endpoint could not be reached ({}): {e}",
                buzz_desktop_core::credentials::redact(&call.url)
            ))
        }
    };
    // Read the header BEFORE the body: `into_reader` consumes the response.
    let connection_id = response
        .header(remote::HEADER_CONNECTION_ID)
        .map(str::to_string);
    let mut body = Vec::new();
    response
        .into_reader()
        // One byte over the cap is enough to detect it; `parse_reply` refuses.
        .take(remote::MAX_BODY_BYTES as u64 + 1)
        .read_to_end(&mut body)
        .map_err(|e| format!("the agent's reply could not be read: {e}"))?;
    let messages = remote::parse_reply(status, &body).map_err(|e| e.to_string())?;
    Ok(Reply {
        connection_id,
        messages,
    })
}

/// The credential for one auth configuration, resolved from the keychain.
///
/// A configured-but-ABSENT credential is an error rather than "send nothing":
/// sending nothing produces a 401 whose message is about authentication, when the
/// truth is that the token was never stored on this machine.
fn secret_for(state: &AppState, auth: &RemoteAuth) -> Result<Option<(String, String)>, String> {
    let reference = match auth {
        RemoteAuth::None => return Ok(None),
        RemoteAuth::Bearer { secret_ref } | RemoteAuth::Header { secret_ref, .. } => secret_ref,
    };
    let Some(secret) = state
        .keychain
        .get(&Key::harness(reference))
        .map_err(|e| e.to_string())?
    else {
        return Err(format!(
            "no credential is stored for {reference}; add the token before using this harness"
        ));
    };
    Ok(remote::auth_header(auth, &secret))
}

fn remote_parts(harness: &Harness) -> Result<(&str, &RemoteAuth), String> {
    match harness {
        Harness::RemoteAcp { endpoint, auth, .. } => Ok((endpoint, auth)),
        Harness::LocalStdio { .. } => Err(
            "that harness is a local process, not a remote endpoint; start it instead".to_string(),
        ),
    }
}

fn result_for(messages: &[Message], id: u64) -> Result<Value, String> {
    for message in messages {
        match message {
            Message::Response {
                id: answered,
                result,
            } if answered.as_u64() == Some(id) => return Ok(result.clone()),
            Message::Failure {
                id: answered,
                message,
                ..
            } if answered.as_u64() == Some(id) => {
                return Err(format!("the agent refused: {message}"))
            }
            _ => {}
        }
    }
    Err("the agent did not answer that request".to_string())
}

/// Contract B `_meta` from a binding the caller supplied, if it is complete.
///
/// All-or-nothing: a half-filled binding would name a room or a repository the
/// agent cannot resolve, which is worse than sending none — the extension is
/// optional by design.
fn binding_meta_from(binding: &Value) -> Option<Value> {
    let text = |key: &str| binding.get(key).and_then(Value::as_str);
    let repository = binding.get("repository").unwrap_or(binding);
    let repo_text = |key: &str| repository.get(key).and_then(Value::as_str);
    Some(acp::binding_meta(&acp::ProjectBinding {
        binding_id: text("bindingId")?,
        channel_id: text("channelId")?,
        provider: repo_text("provider").or_else(|| text("provider"))?,
        provider_instance: repo_text("providerInstance")
            .or_else(|| text("providerInstance"))?,
        stable_id: repo_text("stableId").or_else(|| text("stableId"))?,
        head_stable_id: repo_text("headStableId").or_else(|| text("headStableId")),
        git_ref: text("ref")?,
        generation: text("generation")?,
        scope: text("scope")?,
    }))
}

/// Connect: `initialize`, then `session/new`.
///
/// Both happen here rather than lazily at the first prompt, because "connected"
/// should mean the endpoint answered and a session exists — a connect that only
/// checked the URL would fail at the first message, where the reason is much
/// harder to see.
pub fn connect(
    state: &AppState,
    harness: &Harness,
    binding: Option<&Value>,
) -> Result<Value, String> {
    let generation = state.begin_remote_connect_for(harness)?;
    let (endpoint, auth) = remote_parts(harness)?;
    let secret = secret_for(state, auth)?;
    let mut connection = RemoteConnection::new(endpoint).map_err(|e| e.to_string())?;

    let id = connection.next_request_id();
    let call = connection
        .call(&acp::initialize_request(id), secret.clone())
        .map_err(|e| e.to_string())?;
    let reply = post(&call)?;
    let result = result_for(&reply.messages, id)?;
    connection.absorb_initialize(reply.connection_id.as_deref(), &result);

    // The session opens at the repositories directory, as the local path does:
    // it is the one place this app knows the user's code is.
    let cwd = state.workspace.repos_dir().to_string_lossy().into_owned();
    // Contract B travels only when the agent echoed the capability AND the caller
    // gave a binding — the extension is negotiated, not assumed.
    let meta = binding
        .filter(|_| connection.binding_meta())
        .and_then(binding_meta_from);
    let id = connection.next_request_id();
    let call = connection
        .call(
            &acp::session_new_request(id, &cwd, meta.as_ref()),
            secret.clone(),
        )
        .map_err(|e| e.to_string())?;
    let reply = post(&call)?;
    let result = result_for(&reply.messages, id)?;
    connection.absorb_session(&result).map_err(|e| e.to_string())?;

    let answer = json!({
        "id": harness.id(),
        "endpoint": endpoint,
        "connected": true,
        "session_id": connection.session_id(),
        "connection_id": connection.connection_id(),
        "binding_meta_negotiated": connection.binding_meta(),
        "binding_meta_sent": meta.is_some(),
        "credential_present": secret.is_some(),
    });
    state.finish_remote_connect_for(harness, generation, connection)?;
    Ok(answer)
}

pub fn is_connected(state: &AppState, id: &str) -> bool {
    state
        .remotes
        .lock()
        .map(|remotes| {
            remotes
                .get(id)
                .is_some_and(|connection| connection.session_id().is_some())
        })
        .unwrap_or(false)
}

pub fn disconnect(state: &AppState, id: &str) {
    state.disconnect_remote(id);
}

/// Prompt a connected remote agent. Returns the request id immediately; the
/// conversation continues on its own thread and reports through
/// [`EVENT_AGENT_UPDATE`].
///
/// `target` is where the answer goes — the room, and the thread when the prompt
/// came from one — posted as the AGENT when the turn ends, exactly as the local
/// path does.
pub fn prompt(
    app: &AppHandle,
    state: &AppState,
    harness: &Harness,
    text: &str,
    target: Option<AnswerTarget>,
) -> Result<u64, String> {
    let (_endpoint, auth) = remote_parts(harness)?;
    let secret = secret_for(state, auth)?;
    let harness_id = harness.id().to_string();

    // One turn at a time, for the same reason the local path enforces it: the
    // answer arrives as updates carrying the SESSION but not the request, so two
    // overlapping turns on one session cannot be told apart. Claiming the slot
    // and reserving `next_request_id` under the SAME lock order (`prompting` ->
    // `remotes`) ensures neither `connect` nor `disconnect` can race between slot
    // claim and connection clone, and a rejected concurrent prompt never advances
    // the connection's request counter.
    let (guard, connection, request_id, session) = {
        let mut prompting = state
            .prompting
            .lock()
            .map_err(|_| "the in-flight prompt table is unavailable".to_string())?;
        if prompting.contains(harness.id()) {
            return Err(format!(
                "{} is still answering the previous message; one turn at a time",
                harness.label()
            ));
        }
        let mut remotes = state
            .remotes
            .lock()
            .map_err(|_| "the remote connection table is unavailable".to_string())?;
        let Some(connection) = remotes.get_mut(harness.id()) else {
            return Err(format!(
                "{} is not connected yet — connect it before prompting",
                harness.label()
            ));
        };
        let Some(session) = connection.session_id().map(str::to_string) else {
            return Err(format!(
                "{} has no session, so there is nothing to prompt",
                harness.label()
            ));
        };
        prompting.insert(harness_id.clone());
        let guard = PromptGuard {
            prompting: std::sync::Arc::clone(&state.prompting),
            disconnecting: std::sync::Arc::clone(&state.disconnecting),
            remotes: std::sync::Arc::clone(&state.remotes),
            id: harness_id.clone(),
        };
        let id = connection.next_request_id();
        (guard, connection.clone(), id, session)
    };

    let first = connection
        .call(
            &acp::prompt_request(request_id, &session, text),
            secret.clone(),
        )
        .map_err(|e| e.to_string())?;

    let app = app.clone();
    std::thread::spawn(move || {
        let _guard = guard;
        drive(app, harness_id, connection, secret, first, request_id, target);
    });
    Ok(request_id)
}

/// Clears a harness's in-flight mark, whatever happens to the turn, and completes
/// any deferred `disconnect` that arrived while the turn was running.
///
/// From `Drop`, because a turn can end at a dozen points and one forgotten path
/// would leave the harness permanently "busy" — refusing every later message with
/// no way to recover but a restart.
struct PromptGuard {
    prompting: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    disconnecting: std::sync::Arc<std::sync::Mutex<std::collections::BTreeSet<String>>>,
    remotes: std::sync::Arc<
        std::sync::Mutex<
            std::collections::BTreeMap<String, buzz_desktop_core::remote::RemoteConnection>,
        >,
    >,
    id: String,
}

impl Drop for PromptGuard {
    fn drop(&mut self) {
        if let Ok(mut prompting) = self.prompting.lock() {
            prompting.remove(&self.id);
            let should_remove = self
                .disconnecting
                .lock()
                .map(|mut d| d.remove(&self.id))
                .unwrap_or(false);
            if should_remove {
                if let Ok(mut remotes) = self.remotes.lock() {
                    remotes.remove(&self.id);
                }
            }
        }
    }
}

/// Send whatever answers are still queued, ignoring their replies.
///
/// Called on EVERY early exit. Those calls are answers to requests the agent is
/// waiting on, so dropping one would wedge it; their own replies no longer matter
/// because this conversation is over. That asymmetry — send but do not read — is
/// the whole reason this is a separate function rather than another loop turn.
fn flush(outbox: &mut VecDeque<HttpCall>) {
    while let Some(call) = outbox.pop_front() {
        let _ = post(&call);
    }
}

fn drive(
    app: AppHandle,
    harness_id: String,
    connection: RemoteConnection,
    secret: Option<(String, String)>,
    first: HttpCall,
    request_id: u64,
    target: Option<AnswerTarget>,
) {
    let emit = |payload: Value| {
        let _ = app.emit(EVENT_AGENT_UPDATE, payload);
    };
    let mut answer = AnswerBuffer::new();
    // A reply can carry several agent requests, and on a POST-only binding
    // dropping any of them leaves the agent waiting forever.
    let mut outbox: VecDeque<HttpCall> = VecDeque::new();
    outbox.push_back(first);
    let mut answered = false;
    // Past `MAX_PERMISSION_ROUNDS`, requests are still answered with an explicit
    // refusal so the agent never wedges.
    let mut decisions_left = remote::MAX_PERMISSION_ROUNDS;
    let post_budget = remote::MAX_PERMISSION_ROUNDS * 4 + 2;
    let mut posts = 0usize;

    while let Some(call) = outbox.pop_front() {
        posts = posts.saturating_add(1);
        if posts > post_budget {
            // Re-queue `call` so `flush` sends this answer before stopping.
            outbox.push_front(call);
            flush(&mut outbox);
            emit(json!({
                "id": harness_id,
                "error": "this prompt needed more exchanges than the shell will make, so it \
                          stopped — every request it had received was answered first",
            }));
            return;
        }
        let reply = match post(&call) {
            Ok(reply) => reply,
            Err(detail) => {
                flush(&mut outbox);
                emit(json!({ "id": harness_id, "error": detail }));
                return;
            }
        };
        let plan = remote::plan_reply(&reply.messages, request_id);
        for update in plan.updates {
            if update.get("method").and_then(Value::as_str) == Some("session/update") {
                if let Some(text) = update
                    .get("params")
                    .and_then(|params| acp::session_text_chunk(params, connection.session_id()))
                {
                    answer.push(&text);
                }
            }
            emit(json!({ "id": harness_id, "update": update }));
        }
        // Owed requests are answered BEFORE any early return: one reply can carry
        // both our prompt's failure and a request the agent is waiting on, and
        // returning first would leave that request hanging.
        for owed in plan.owed {
            let answer = if decisions_left > 0 {
                decisions_left = decisions_left.saturating_sub(1);
                answer_for(
                    &app,
                    &harness_id,
                    connection.session_id(),
                    &owed.id,
                    &owed.method,
                    &owed.params,
                )
            } else {
                acp::error_response(
                    &owed.id,
                    -32603,
                    "this shell has answered the maximum number of requests for one prompt",
                )
            };
            match connection.call(&answer, secret.clone()) {
                Ok(next) => outbox.push_back(next),
                Err(e) => {
                    flush(&mut outbox);
                    emit(json!({ "id": harness_id, "error": e.to_string() }));
                    return;
                }
            }
        }
        if plan.answered {
            answered = true;
            emit(json!({ "id": harness_id, "answered": true }));
        }
        if let Some(failure) = plan.failure {
            flush(&mut outbox);
            // A failed turn posts NOTHING: half an answer attributed to the agent
            // would be worse than none.
            emit(json!({ "id": harness_id, "error": failure }));
            return;
        }
        // Our prompt is answered AND every answer we owe has been sent.
        if answered && outbox.is_empty() {
            post_answer(&app, &harness_id, target.as_ref(), answer.take());
            return;
        }
    }

    if !answered {
        // Said out loud rather than leaving a caller waiting for an event that
        // will never arrive.
        emit(json!({
            "id": harness_id,
            "error": "the agent ended the exchange without answering the prompt",
        }));
    }
}

/// Put a finished turn's words in the room that asked for them.
///
/// Signed as the AGENT, for the reason `workspace::post_as_agent` gives: these
/// are not the user's words and the shell does not hold the user's key. A failure
/// is REPORTED — an answer that never arrived is precisely what somebody is
/// waiting for, and the relay's refusal usually says a moderator has not added
/// this agent to the room yet.
fn post_answer(
    app: &AppHandle,
    harness_id: &str,
    target: Option<&AnswerTarget>,
    answer: Option<String>,
) {
    let (Some(target), Some(answer)) = (target, answer) else {
        return;
    };
    let state = app.state::<AppState>();
    match crate::workspace::post_as_agent(&state, harness_id, target, &answer) {
        Ok(event_id) => {
            let _ = app.emit(
                EVENT_AGENT_UPDATE,
                json!({
                    "id": harness_id,
                    "posted": event_id,
                    "channel": target.channel,
                }),
            );
        }
        Err(detail) => {
            let _ = app.emit(
                EVENT_AGENT_UPDATE,
                json!({ "id": harness_id, "error": detail }),
            );
        }
    }
}

/// The message this shell sends back for one request the agent made of us.
///
/// EVERY request is answered, exactly as on the local path: a method this shell
/// does not implement gets a JSON-RPC "method not found" rather than silence,
/// because an unanswered request leaves the agent waiting.
///
/// The wait for a person is BOUNDED. The broker owns the deadline, and this
/// waits a little past it: if the dialog is still open when the deadline passes,
/// the request is swept and the agent is refused NOW — a remote agent's refusal
/// cannot be delivered by the broker's own sweep (there is no pipe to write to),
/// so the driver has to send it.
fn answer_for(
    app: &AppHandle,
    harness_id: &str,
    session_id: Option<&str>,
    id: &Value,
    method: &str,
    params: &Value,
) -> Value {
    if method != "session/request_permission" {
        return acp::error_response(
            id,
            -32601,
            &format!("{method} is not implemented by the buzz desktop shell"),
        );
    }
    let request = match PermissionRequest::parse(params) {
        Ok(request) => request,
        Err(e) => return acp::error_response(id, -32602, &format!("{e}")),
    };
    // Only the session this connection negotiated may ask: a request naming
    // another (or any, before one exists) is refused before a dialog opens,
    // so no person approves something this prompt never started.
    if let Err(detail) = permission_session_matches(session_id, &request.session_id) {
        return acp::error_response(id, -32602, &detail);
    }
    let waiting = Duration::from_millis(
        broker::DEFAULT_TIMEOUT_MS.saturating_add(PERMISSION_GRACE_MS),
    );
    let decided = match supervisor::ask_permission(app, harness_id, id, request)
        .recv_timeout(waiting)
    {
        Ok(decided) => decided,
        Err(_) => {
            // The deadline passed with the dialog still open. Sweep it so a late
            // answer cannot contradict this one, and refuse.
            supervisor::expire_permissions(app);
            Decided::AlreadyResolved
        }
    };
    match decided {
        Decided::Answered(outcome) => {
            remote::permission_answer(id, outcome.option_id.as_deref())
        }
        // Resolved by the broker rather than by a person — a timeout, or the
        // harness going away. Either is a REFUSAL, and that is what the agent is
        // told: the first answer wins, and fail-closed means it was no.
        Decided::AlreadyResolved => remote::permission_answer(id, None),
        Decided::CannotOpen(detail) => acp::error_response(id, -32603, &detail),
    }
}

/// Whether a permission request's `sessionId` is the connection's own.
fn permission_session_matches(connection: Option<&str>, requested: &str) -> Result<(), String> {
    match connection {
        Some(own) if own == requested => Ok(()),
        Some(_) => Err(format!(
            "session {requested} is not this connection's session; the permission request is refused"
        )),
        None => Err("this connection has no session yet; the permission request is refused".to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_permission_request_for_another_session_is_refused() {
        assert!(permission_session_matches(Some("s1"), "s1").is_ok());
        assert!(permission_session_matches(Some("s1"), "s2").is_err());
        assert!(permission_session_matches(None, "s1").is_err());
    }

    #[test]
    fn a_local_harness_is_refused_by_name() {
        let local = Harness::LocalStdio {
            id: "claude".into(),
            label: "Claude Code".into(),
            command: "claude-code-acp".into(),
            args: vec![],
            env: vec![],
        };
        let refused = remote_parts(&local).expect_err("refused");
        assert!(refused.contains("local process"), "{refused}");
    }

    #[test]
    fn a_result_is_matched_to_its_own_request() {
        let messages = vec![
            Message::Notification {
                method: "session/update".into(),
                params: json!({}),
            },
            Message::Response {
                id: json!(2),
                result: json!({ "other": true }),
            },
            Message::Response {
                id: json!(1),
                result: json!({ "ours": true }),
            },
        ];
        assert_eq!(
            result_for(&messages, 1).expect("ours"),
            json!({ "ours": true })
        );
        // An answer to a DIFFERENT request is not ours.
        assert!(result_for(&messages, 3).is_err());

        let failed = vec![Message::Failure {
            id: json!(1),
            code: -32000,
            message: "no model configured".into(),
        }];
        let err = result_for(&failed, 1).expect_err("the agent refused");
        assert!(err.contains("no model configured"), "{err}");
    }

    #[test]
    fn a_binding_meta_is_all_or_nothing() {
        // Complete, in the shape a client would send.
        let complete = json!({
            "bindingId": "sha256:abc",
            "channelId": "process-abc",
            "repository": {
                "provider": "github",
                "providerInstance": "github.com",
                "stableId": "12345",
                "headStableId": "999"
            },
            "ref": "refs/heads/feature/x",
            "generation": "g2",
            "scope": "https://buzz.example"
        });
        let meta = binding_meta_from(&complete).expect("meta");
        let body = &meta[acp::META_PROJECT_BINDING];
        assert_eq!(body["bindingId"], "sha256:abc");
        assert_eq!(body["repository"]["headStableId"], "999");

        // A flat shape is accepted too — the fields, not their nesting, are the
        // contract.
        let flat = json!({
            "bindingId": "sha256:abc",
            "channelId": "process-abc",
            "provider": "ferrite-rcs",
            "providerInstance": "acct-1",
            "stableId": "my-app",
            "ref": "refs/heads/x",
            "generation": "g1",
            "scope": "https://buzz.example"
        });
        assert!(binding_meta_from(&flat).is_some());

        // Missing ANY required field yields none: a half-filled binding would
        // name a room the agent cannot resolve, which is worse than sending
        // nothing at all.
        for missing in [
            "bindingId",
            "channelId",
            "provider",
            "providerInstance",
            "stableId",
            "ref",
            "generation",
            "scope",
        ] {
            let mut partial = flat.clone();
            partial
                .as_object_mut()
                .expect("object")
                .remove(missing);
            assert!(
                binding_meta_from(&partial).is_none(),
                "a binding without {missing} must not be sent"
            );
        }
    }
}
