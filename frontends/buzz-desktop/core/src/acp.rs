//! The ACP wire, as this shell speaks it.
//!
//! Agent Client Protocol is JSON-RPC 2.0 over a transport: newline-delimited
//! JSON on a child process's stdio for a local harness, HTTP POST for a remote
//! one. This module is the wire and nothing else — framing, the requests this
//! shell sends, the responses it must answer — so the protocol is testable
//! without a subprocess, a network or an agent.
//!
//! The one rule that matters here: a `session/request_permission` is a REQUEST
//! the agent makes of us, and it must be answered exactly once. What decides the
//! answer is [`crate::broker`]; this module only builds and reads the frames.

use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{Error, Result};

pub const PROTOCOL_VERSION: u32 = 1;
pub const META_PROJECT_BINDING: &str = "buzz.chat/projectBinding";
pub const BINDING_META_VERSION: u32 = 1;

/// Largest single frame this shell will accept from a harness.
///
/// A harness is a local process, not a hostile network peer — but it is a
/// process whose output can run away (a tool that dumps a file, a crash loop
/// writing a stack trace per line), and an unbounded read of a pipe is an
/// unbounded allocation in this process.
pub const MAX_FRAME_BYTES: usize = 8 * 1024 * 1024;

/// One JSON-RPC message, classified by what it obliges us to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    /// The agent is asking US something and waits for an answer.
    Request {
        id: Value,
        method: String,
        params: Value,
    },
    /// An answer to something WE asked.
    Response { id: Value, result: Value },
    /// An error answer to something we asked.
    Failure {
        id: Value,
        code: i64,
        message: String,
    },
    /// Fire and forget — streamed output, status updates.
    Notification { method: String, params: Value },
}

impl Message {
    #[must_use]
    pub fn method(&self) -> Option<&str> {
        match self {
            Self::Request { method, .. } | Self::Notification { method, .. } => Some(method),
            Self::Response { .. } | Self::Failure { .. } => None,
        }
    }
}

/// Frame one message as a transport line.
///
/// Newline-delimited, and the JSON is checked for an embedded newline it cannot
/// contain: `serde_json` escapes them, so this is an assertion rather than a
/// sanitizer — if it ever fired, the framing would be broken and silently
/// splitting one message into two.
pub fn frame(message: &Value) -> Result<String> {
    let line = serde_json::to_string(message)
        .map_err(|e| Error::Internal(format!("could not encode an ACP message: {e}")))?;
    if line.contains('\n') {
        return Err(Error::Internal(
            "an encoded ACP message contains a newline, which would split the frame".into(),
        ));
    }
    Ok(format!("{line}\n"))
}

pub fn parse_line(line: &str) -> Result<Message> {
    if line.len() > MAX_FRAME_BYTES {
        return Err(Error::Invalid(format!(
            "an ACP frame is over {MAX_FRAME_BYTES} bytes"
        )));
    }
    let value: Value = serde_json::from_str(line.trim())
        .map_err(|e| Error::Invalid(format!("an ACP frame is not JSON: {e}")))?;
    parse_value(&value)
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BoundedLine {
    Eof,
    /// A line within the bound, without its `\n` or `\r\n`; invalid UTF-8 is
    /// replaced rather than ending the stream.
    Line(String),
    /// A line over the bound. Only `max_bytes + 1` of it was ever held; the
    /// rest was read and discarded up to its newline. Carries its length.
    Oversized(usize),
}

/// Read one newline-delimited line, holding at most `max_bytes + 1` bytes of it.
///
/// `BufRead::lines` buffers a whole line before returning it, so a harness
/// that writes without a newline is an unbounded allocation in this process
/// before [`parse_line`] can refuse the frame. Here an over-long line is
/// drained in the reader's own buffer-sized chunks and reported as
/// [`BoundedLine::Oversized`], and the next line is read normally.
pub fn read_line_bounded(
    reader: &mut impl std::io::BufRead,
    max_bytes: usize,
) -> std::io::Result<BoundedLine> {
    use std::io::{BufRead, Read};
    let mut buf = Vec::new();
    let read = reader
        .by_ref()
        .take((max_bytes as u64).saturating_add(1))
        .read_until(b'\n', &mut buf)?;
    if read == 0 {
        return Ok(BoundedLine::Eof);
    }
    let ended = buf.last() == Some(&b'\n');
    if ended {
        buf.pop();
        if buf.last() == Some(&b'\r') {
            buf.pop();
        }
    }
    if buf.len() > max_bytes {
        let mut total = buf.len();
        if !ended {
            loop {
                let available = reader.fill_buf()?;
                if available.is_empty() {
                    break;
                }
                if let Some(at) = available.iter().position(|&b| b == b'\n') {
                    total = total.saturating_add(at);
                    reader.consume(at + 1);
                    break;
                }
                let len = available.len();
                total = total.saturating_add(len);
                reader.consume(len);
            }
        }
        return Ok(BoundedLine::Oversized(total));
    }
    Ok(BoundedLine::Line(String::from_utf8_lossy(&buf).into_owned()))
}

/// Classify one already-parsed message.
///
/// Shared with the HTTP binding ([`crate::remote`]), which receives a JSON
/// document rather than a line — so both transports agree about what counts as a
/// request the agent is WAITING on, which is the distinction that decides whether
/// this shell owes an answer.
pub fn parse_value(value: &Value) -> Result<Message> {
    let method = value.get("method").and_then(Value::as_str);
    let id = value.get("id").cloned();
    match (method, id) {
        // A request carries BOTH a method and an id: it is a question that
        // expects an answer, and answering the wrong shape leaves the agent
        // waiting forever.
        (Some(method), Some(id)) if !id.is_null() => Ok(Message::Request {
            id,
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        }),
        (Some(method), _) => Ok(Message::Notification {
            method: method.to_string(),
            params: value.get("params").cloned().unwrap_or(Value::Null),
        }),
        (None, Some(id)) => {
            if let Some(error) = value.get("error") {
                return Ok(Message::Failure {
                    id,
                    code: error.get("code").and_then(Value::as_i64).unwrap_or(0),
                    message: error
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or("the agent reported an error")
                        .to_string(),
                });
            }
            Ok(Message::Response {
                id,
                result: value.get("result").cloned().unwrap_or(Value::Null),
            })
        }
        (None, None) => Err(Error::Invalid(
            "an ACP frame has neither a method nor an id".into(),
        )),
    }
}

/// `initialize` — the first thing this shell sends.
///
/// `clientCapabilities` advertises the Contract B binding `_meta` so the other
/// side knows it may be sent; a harness that does not echo the capability is
/// never sent one, which is what keeps the extension optional rather than a
/// compatibility break.
#[must_use]
pub fn initialize_request(id: u64) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "initialize",
        "params": {
            "protocolVersion": PROTOCOL_VERSION,
            "clientCapabilities": {
                "fs": { "readTextFile": false, "writeTextFile": false },
                "terminal": false,
                "_meta": { META_PROJECT_BINDING: { "version": BINDING_META_VERSION } }
            }
        }
    })
}

/// Whether the other side advertised the Contract B binding extension.
#[must_use]
pub fn negotiated_binding_meta(initialize_result: &Value) -> bool {
    initialize_result
        .get("agentCapabilities")
        .or_else(|| initialize_result.get("capabilities"))
        .and_then(|caps| caps.get("_meta"))
        .and_then(|meta| meta.get(META_PROJECT_BINDING))
        .and_then(|binding| binding.get("version"))
        .and_then(Value::as_u64)
        .is_some_and(|version| version == u64::from(BINDING_META_VERSION))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ProjectBinding<'a> {
    pub binding_id: &'a str,
    pub channel_id: &'a str,
    pub provider: &'a str,
    pub provider_instance: &'a str,
    pub stable_id: &'a str,
    pub head_stable_id: Option<&'a str>,
    pub git_ref: &'a str,
    pub generation: &'a str,
    pub scope: &'a str,
}

/// Contract B: the `_meta` a session carries when a project room is bound.
///
/// Built from Contract A's identity fields, and deliberately carrying NO secret,
/// no token and no local path — it crosses to a remote agent, and a local path
/// would be both useless there and a disclosure about the user's machine.
#[must_use]
pub fn binding_meta(binding: &ProjectBinding<'_>) -> Value {
    let mut repository = json!({
        "provider": binding.provider,
        "providerInstance": binding.provider_instance,
        "stableId": binding.stable_id,
    });
    if let (Some(head), Some(map)) = (binding.head_stable_id, repository.as_object_mut()) {
        map.insert("headStableId".to_string(), Value::String(head.to_string()));
    }
    json!({
        META_PROJECT_BINDING: {
            "version": BINDING_META_VERSION,
            "bindingId": binding.binding_id,
            "channelId": binding.channel_id,
            "repository": repository,
            "ref": binding.git_ref,
            "generation": binding.generation,
            "scope": binding.scope,
        }
    })
}

/// `session/new`.
///
/// `mcpServers` is always empty: this shell does not hand an agent its own MCP
/// configuration. The workspace's connectors are the relay's business, and
/// passing local MCP config would give a harness capabilities the workspace
/// never granted it.
#[must_use]
pub fn session_new_request(id: u64, cwd: &str, meta: Option<&Value>) -> Value {
    let mut params = json!({ "cwd": cwd, "mcpServers": [] });
    if let (Some(meta), Some(map)) = (meta, params.as_object_mut()) {
        map.insert("_meta".to_string(), meta.clone());
    }
    json!({ "jsonrpc": "2.0", "id": id, "method": "session/new", "params": params })
}

#[must_use]
pub fn prompt_request(id: u64, session_id: &str, text: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "method": "session/prompt",
        "params": {
            "sessionId": session_id,
            "prompt": [{ "type": "text", "text": text }]
        }
    })
}

#[must_use]
pub fn cancel_notification(session_id: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "method": "session/cancel",
        "params": { "sessionId": session_id }
    })
}

/// The answer to a `session/request_permission`.
///
/// The shape is the protocol's: `{outcome: {outcome: "selected", optionId}}` for
/// a choice, `{outcome: {outcome: "cancelled"}}` for a refusal. A refusal is a
/// legitimate ANSWER rather than an error, which matters: an error response
/// leaves some agents retrying, while a cancellation is understood as no.
#[must_use]
pub fn permission_response(id: &Value, option_id: Option<&str>) -> Value {
    let outcome = match option_id {
        Some(option) => json!({ "outcome": "selected", "optionId": option }),
        None => json!({ "outcome": "cancelled" }),
    };
    json!({ "jsonrpc": "2.0", "id": id, "result": { "outcome": outcome } })
}

#[must_use]
pub fn error_response(id: &Value, code: i64, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": { "code": code, "message": message }
    })
}

/// The TEXT an agent spoke in one `session/update`, if it spoke any.
///
/// ACP splits an answer across notifications: `session/prompt` answers with only
/// a `stopReason`, and the words arrive as `agent_message_chunk` updates. So a
/// caller that wants the answer must accumulate these — reading the prompt's own
/// response would find nothing to say.
///
/// Only `agent_message_chunk` counts. A `tool_call`, a plan, or a THOUGHT chunk
/// is deliberately excluded: an agent's private reasoning is not its answer, and
/// posting it into a room would publish something the agent did not say to
/// anyone.
#[must_use]
pub fn agent_text_chunk(params: &Value) -> Option<String> {
    let update = params.get("update")?;
    let kind = update.get("sessionUpdate").and_then(Value::as_str)?;
    if kind != "agent_message_chunk" {
        return None;
    }
    let content = update.get("content")?;
    // The protocol's text block; anything else (an image, a resource) has no
    // text to add.
    if content.get("type").and_then(Value::as_str) != Some("text") {
        return None;
    }
    let text = content.get("text").and_then(Value::as_str)?;
    (!text.is_empty()).then(|| text.to_string())
}

#[must_use]
pub fn update_session_id(params: &Value) -> Option<String> {
    params
        .get("sessionId")
        .and_then(Value::as_str)
        .map(str::to_string)
}

/// Spoken text from a `session/update` notification, filtered to the active
/// `expected_session` so stray chunks from a superseded session are ignored.
#[must_use]
pub fn session_text_chunk(params: &Value, expected_session: Option<&str>) -> Option<String> {
    let text = agent_text_chunk(params)?;
    let matches_session =
        update_session_id(params).is_none_or(|sid| expected_session == Some(sid.as_str()));
    matches_session.then_some(text)
}

/// An agent's answer as it accumulates, BOUNDED.
///
/// The words arrive as a stream of `agent_message_chunk` updates from a process
/// (or an endpoint) this shell does not control, so an unbounded buffer is
/// unbounded memory in the desktop app — the same reason a harness's log is capped
/// at its line count. One implementation, shared by both transports, because two
/// copies of a resource bound would be two chances to forget one.
#[derive(Debug, Default)]
pub struct AnswerBuffer {
    text: String,
    truncated: bool,
}

impl AnswerBuffer {
    /// Most an answer may accumulate before the rest is dropped.
    ///
    /// Far more than any chat message, and comfortably inside the relay's own
    /// serialized-event bound — so an answer this shell accepts is one the relay
    /// can actually store, rather than one it will refuse after the fact.
    pub const MAX_BYTES: usize = 64 * 1024;

    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add what the agent just said, up to the bound.
    ///
    /// Past the bound the rest is DROPPED rather than the turn abandoned: the
    /// beginning of a long answer is worth having, and [`Self::take`] says out
    /// loud that it was cut.
    pub fn push(&mut self, chunk: &str) {
        if self.truncated {
            return;
        }
        let remaining = Self::MAX_BYTES.saturating_sub(self.text.len());
        if chunk.len() <= remaining {
            self.text.push_str(chunk);
            return;
        }
        // Cut on a CHARACTER boundary, or the text would not be valid UTF-8 and
        // the event carrying it could not be encoded at all.
        let cut = chunk
            .char_indices()
            .take_while(|(at, c)| at + c.len_utf8() <= remaining)
            .last()
            .map_or(0, |(at, c)| at + c.len_utf8());
        self.text.push_str(&chunk[..cut]);
        self.truncated = true;
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.text.trim().is_empty()
    }

    #[must_use]
    pub const fn truncated(&self) -> bool {
        self.truncated
    }

    /// The answer, leaving the buffer empty. `None` when nothing was said.
    ///
    /// A truncated answer carries a visible note, because a silently cut answer
    /// reads as the agent having stopped mid-sentence — which is a different and
    /// more alarming thing than a limit having been reached.
    pub fn take(&mut self) -> Option<String> {
        let truncated = self.truncated;
        self.truncated = false;
        let text = core::mem::take(&mut self.text);
        if text.trim().is_empty() {
            return None;
        }
        if truncated {
            return Some(format!(
                "{text}\n\n[This answer reached the {} KiB limit; the rest was not posted.]",
                Self::MAX_BYTES / 1024
            ));
        }
        Some(text)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionRequest {
    pub session_id: String,
    pub tool_name: String,
    pub title: Option<String>,
    pub options: Vec<PermissionOption>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

impl PermissionRequest {
    pub fn parse(params: &Value) -> Result<Self> {
        let session_id = params
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| Error::Invalid("a permission request has no sessionId".into()))?
            .to_string();
        let call = params.get("toolCall");
        let tool_name = call
            .and_then(|c| c.get("title").or_else(|| c.get("kind")))
            .and_then(Value::as_str)
            .unwrap_or("a tool")
            .to_string();
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .map(|rows| {
                rows.iter()
                    .filter_map(|row| {
                        Some(PermissionOption {
                            id: row.get("optionId")?.as_str()?.to_string(),
                            name: row
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                            kind: row
                                .get("kind")
                                .and_then(Value::as_str)
                                .unwrap_or("")
                                .to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            session_id,
            tool_name,
            title: call
                .and_then(|c| c.get("title"))
                .and_then(Value::as_str)
                .map(str::to_string),
            options,
        })
    }

    /// The option id for an "allow once" choice, when the agent offers one.
    ///
    /// Only an EXACT `allow_once` kind counts. Nothing here falls back to "the
    /// first option": an agent that offers `[reject, allow]` would then be
    /// answered with a refusal, and one that offers `[allow_always, …]` would be
    /// answered with a standing grant nobody chose.
    #[must_use]
    pub fn allow_once_option(&self) -> Option<&str> {
        self.options
            .iter()
            .find(|option| option.kind == "allow_once")
            .map(|option| option.id.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_line_over_the_bound_is_drained_not_held_and_the_next_line_reads() {
        let input = b"short\n0123456789abcdef\nafter\r\nno-newline-but-long-tail";
        // A one-byte buffer makes the drain walk chunk by chunk.
        let mut reader = std::io::BufReader::with_capacity(1, &input[..]);
        assert_eq!(read_line_bounded(&mut reader, 8).unwrap(), BoundedLine::Line("short".into()));
        assert_eq!(read_line_bounded(&mut reader, 8).unwrap(), BoundedLine::Oversized(16));
        assert_eq!(read_line_bounded(&mut reader, 8).unwrap(), BoundedLine::Line("after".into()));
        assert_eq!(read_line_bounded(&mut reader, 8).unwrap(), BoundedLine::Oversized(24));
        assert_eq!(read_line_bounded(&mut reader, 8).unwrap(), BoundedLine::Eof);
    }

    #[test]
    fn a_line_of_exactly_the_bound_is_kept_and_invalid_utf8_does_not_end_the_stream() {
        let input = b"12345678\n\xff\xfe\n";
        let mut reader = &input[..];
        assert_eq!(read_line_bounded(&mut reader, 8).unwrap(), BoundedLine::Line("12345678".into()));
        assert_eq!(
            read_line_bounded(&mut reader, 8).unwrap(),
            BoundedLine::Line("\u{fffd}\u{fffd}".into())
        );
        assert_eq!(read_line_bounded(&mut reader, 8).unwrap(), BoundedLine::Eof);
    }

    #[test]
    fn framing_is_one_line_per_message() {
        let line = frame(&json!({"jsonrpc":"2.0","id":1,"method":"x"})).expect("frame");
        assert!(line.ends_with('\n'));
        assert_eq!(line.matches('\n').count(), 1);
        // A newline inside a string is ESCAPED by the encoder, so the frame
        // stays one line — which is what makes newline-delimited framing safe.
        let line = frame(&json!({"text":"two\nlines"})).expect("frame");
        assert_eq!(line.matches('\n').count(), 1);
        assert!(line.contains("two\\nlines"));
    }

    #[test]
    fn a_request_is_told_apart_from_a_notification_and_a_response() {
        // The distinction decides whether the agent is WAITING for us.
        let request = parse_line(r#"{"jsonrpc":"2.0","id":7,"method":"session/request_permission","params":{}}"#)
            .expect("request");
        assert!(matches!(request, Message::Request { .. }));
        assert_eq!(request.method(), Some("session/request_permission"));

        let notification =
            parse_line(r#"{"jsonrpc":"2.0","method":"session/update","params":{}}"#)
                .expect("notification");
        assert!(matches!(notification, Message::Notification { .. }));

        let response = parse_line(r#"{"jsonrpc":"2.0","id":1,"result":{"ok":true}}"#)
            .expect("response");
        assert!(matches!(response, Message::Response { .. }));

        let failure =
            parse_line(r#"{"jsonrpc":"2.0","id":1,"error":{"code":-32601,"message":"nope"}}"#)
                .expect("failure");
        match failure {
            Message::Failure { code, message, .. } => {
                assert_eq!(code, -32601);
                assert_eq!(message, "nope");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_null_id_is_a_notification_not_a_request() {
        // JSON-RPC allows `id: null`, and treating it as a request would leave
        // this shell answering a question nobody asked.
        let message = parse_line(r#"{"jsonrpc":"2.0","id":null,"method":"x"}"#).expect("parse");
        assert!(matches!(message, Message::Notification { .. }));
    }

    #[test]
    fn malformed_frames_are_refused() {
        assert!(parse_line("not json").is_err());
        assert!(parse_line(r#"{"jsonrpc":"2.0"}"#).is_err(), "neither method nor id");
        let huge = "x".repeat(MAX_FRAME_BYTES + 1);
        assert!(parse_line(&huge).is_err());
    }

    #[test]
    fn initialize_advertises_the_binding_extension_and_no_filesystem() {
        let request = initialize_request(1);
        let caps = &request["params"]["clientCapabilities"];
        assert_eq!(caps["_meta"][META_PROJECT_BINDING]["version"], 1);
        // This shell does not offer the agent its filesystem or a terminal: a
        // capability advertised is a capability the agent will use.
        assert_eq!(caps["fs"]["readTextFile"], false);
        assert_eq!(caps["terminal"], false);
    }

    #[test]
    fn the_binding_extension_is_only_used_when_the_agent_echoes_it() {
        assert!(negotiated_binding_meta(&json!({
            "agentCapabilities": { "_meta": { META_PROJECT_BINDING: { "version": 1 } } }
        })));
        // A different version is NOT negotiated: sending a v1 body to a v2 peer
        // is how a contract silently half-works.
        assert!(!negotiated_binding_meta(&json!({
            "agentCapabilities": { "_meta": { META_PROJECT_BINDING: { "version": 2 } } }
        })));
        assert!(!negotiated_binding_meta(&json!({ "agentCapabilities": {} })));
        assert!(!negotiated_binding_meta(&json!({})));
    }

    #[test]
    fn the_binding_meta_carries_identity_and_nothing_private() {
        let meta = binding_meta(&ProjectBinding {
            binding_id: "sha256:abc",
            channel_id: "process-abc",
            provider: "github",
            provider_instance: "github.com",
            stable_id: "12345",
            head_stable_id: Some("999"),
            git_ref: "refs/heads/feature/x",
            generation: "g2",
            scope: "https://buzz.example",
        });
        let body = &meta[META_PROJECT_BINDING];
        assert_eq!(body["version"], 1);
        assert_eq!(body["bindingId"], "sha256:abc");
        assert_eq!(body["channelId"], "process-abc");
        assert_eq!(body["repository"]["headStableId"], "999");
        assert_eq!(body["ref"], "refs/heads/feature/x");

        // No secret, no token, no local path — this crosses to a remote agent.
        let encoded = serde_json::to_string(&meta).expect("json");
        for forbidden in ["token", "secret", "password", "/home/", "Users"] {
            assert!(
                !encoded.contains(forbidden),
                "{forbidden} must not appear in a binding _meta: {encoded}"
            );
        }

        // A same-repo binding omits the head repository entirely rather than
        // sending an empty string.
        let same_repo = binding_meta(&ProjectBinding {
            binding_id: "sha256:abc",
            channel_id: "process-abc",
            provider: "github",
            provider_instance: "github.com",
            stable_id: "12345",
            head_stable_id: None,
            git_ref: "refs/heads/x",
            generation: "g1",
            scope: "https://buzz.example",
        });
        assert!(same_repo[META_PROJECT_BINDING]["repository"]
            .get("headStableId")
            .is_none());
    }

    #[test]
    fn a_session_never_carries_mcp_configuration() {
        let request = session_new_request(2, "/tmp/repo", None);
        assert_eq!(request["params"]["cwd"], "/tmp/repo");
        assert_eq!(
            request["params"]["mcpServers"],
            json!([]),
            "handing an agent MCP config would grant capabilities the workspace did not"
        );
        assert!(request["params"].get("_meta").is_none());

        let meta = json!({ META_PROJECT_BINDING: { "version": 1 } });
        let bound = session_new_request(3, "/tmp/repo", Some(&meta));
        assert_eq!(bound["params"]["_meta"], meta);
    }

    #[test]
    fn a_prompt_is_one_text_block() {
        let request = prompt_request(4, "sess-1", "hello @agent");
        assert_eq!(request["params"]["sessionId"], "sess-1");
        assert_eq!(request["params"]["prompt"][0]["type"], "text");
        assert_eq!(request["params"]["prompt"][0]["text"], "hello @agent");
    }

    #[test]
    fn an_agents_words_are_accumulated_from_its_update_chunks() {
        // The answer is NOT in the prompt's response — that carries only a
        // stopReason — so a caller has to read these.
        let chunk = json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "agent_message_chunk",
                "content": { "type": "text", "text": "Hello" }
            }
        });
        assert_eq!(agent_text_chunk(&chunk).as_deref(), Some("Hello"));
        assert_eq!(update_session_id(&chunk).as_deref(), Some("s1"));

        // A THOUGHT is not an answer: posting an agent's private reasoning into a
        // room would publish something it never said to anybody.
        let thought = json!({
            "sessionId": "s1",
            "update": {
                "sessionUpdate": "agent_thought_chunk",
                "content": { "type": "text", "text": "thinking..." }
            }
        });
        assert_eq!(agent_text_chunk(&thought), None);

        // Neither is a tool call, a non-text block, or an empty string.
        for other in [
            json!({ "update": { "sessionUpdate": "tool_call", "content": { "type": "text", "text": "x" } } }),
            json!({ "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "image", "data": "" } } }),
            json!({ "update": { "sessionUpdate": "agent_message_chunk", "content": { "type": "text", "text": "" } } }),
            json!({ "update": {} }),
            json!({}),
        ] {
            assert_eq!(agent_text_chunk(&other), None, "{other}");
        }
    }

    #[test]
    fn an_answer_accumulates_and_taking_it_empties_it() {
        let mut answer = AnswerBuffer::new();
        assert!(answer.is_empty());
        assert!(answer.take().is_none(), "nothing said is nothing to post");

        answer.push("Hello, ");
        answer.push("world");
        assert!(!answer.is_empty());
        assert!(!answer.truncated());
        assert_eq!(answer.take().as_deref(), Some("Hello, world"));
        // Taking empties it, so a second turn cannot post the first turn's words.
        assert!(answer.take().is_none());
    }

    #[test]
    fn a_runaway_agent_cannot_grow_the_buffer_without_limit() {
        // The chunks come from a process this shell does not control, so the
        // bound is what stops a noisy or hostile harness consuming memory.
        let mut answer = AnswerBuffer::new();
        for _ in 0..1_000 {
            answer.push(&"x".repeat(1_024));
        }
        assert!(answer.truncated(), "the bound must engage");
        let posted = answer.take().expect("something was said");
        // The text itself never exceeds the bound, and the note is visible.
        assert!(posted.len() <= AnswerBuffer::MAX_BYTES + 100, "{}", posted.len());
        assert!(posted.contains("reached the 64 KiB limit"), "a cut answer must say so");
        // Reset afterwards, so the NEXT turn is not born truncated.
        assert!(!answer.truncated());
    }

    #[test]
    fn truncation_cuts_on_a_character_boundary() {
        // A cut mid-character would not be valid UTF-8, and the event carrying it
        // could not be encoded at all.
        let mut answer = AnswerBuffer::new();
        answer.push(&"a".repeat(AnswerBuffer::MAX_BYTES - 1));
        // A 3-byte character with only one byte of room left.
        answer.push("€€€");
        assert!(answer.truncated());
        let posted = answer.take().expect("text");
        // The character that does not fit is dropped whole.
        assert!(posted.starts_with(&"a".repeat(AnswerBuffer::MAX_BYTES - 1)));
        assert!(!posted.contains('€'), "{}", &posted[AnswerBuffer::MAX_BYTES - 1..]);
    }

    #[test]
    fn a_permission_answer_uses_cancellation_for_a_refusal() {
        let id = json!(9);
        let allowed = permission_response(&id, Some("opt-allow"));
        assert_eq!(allowed["result"]["outcome"]["outcome"], "selected");
        assert_eq!(allowed["result"]["outcome"]["optionId"], "opt-allow");

        // A refusal is an ANSWER, not an error: an error response leaves some
        // agents retrying, while a cancellation is understood as no.
        let refused = permission_response(&id, None);
        assert_eq!(refused["result"]["outcome"]["outcome"], "cancelled");
        assert!(refused.get("error").is_none());
    }

    #[test]
    fn a_permission_request_parses_and_never_guesses_its_allow_option() {
        let params = json!({
            "sessionId": "sess-1",
            "toolCall": { "title": "Write src/lib.rs", "kind": "edit" },
            "options": [
                { "optionId": "r", "name": "No", "kind": "reject_once" },
                { "optionId": "a", "name": "Yes", "kind": "allow_once" }
            ]
        });
        let request = PermissionRequest::parse(&params).expect("parse");
        assert_eq!(request.session_id, "sess-1");
        assert_eq!(request.tool_name, "Write src/lib.rs");
        assert_eq!(request.options.len(), 2);
        // The `allow_once` option BY KIND — not the first, which here is a
        // refusal, and answering with it would deny what the user allowed.
        assert_eq!(request.allow_once_option(), Some("a"));

        // No allow_once offered: nothing is invented.
        let only_always = json!({
            "sessionId": "s",
            "options": [{ "optionId": "x", "name": "Always", "kind": "allow_always" }]
        });
        assert_eq!(
            PermissionRequest::parse(&only_always)
                .expect("parse")
                .allow_once_option(),
            None,
            "a standing grant must not be selected on the user's behalf"
        );

        assert!(PermissionRequest::parse(&json!({})).is_err());
    }
}
