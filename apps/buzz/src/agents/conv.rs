//! The conversation message types shared by compaction, prompt assembly, and
//! the model client.

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json;

/// The conversational role of a folded message.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConvRole {
    System,
    User,
    Assistant,
    /// A tool result, paired (by `tool_call_id`) with a preceding assistant
    /// tool-use. Required so providers like Anthropic see a complete, structured
    /// tool exchange (without it the model re-issues the same call in a loop).
    Tool,
}

/// One tool-use request the assistant issued (carried on an assistant
/// [`ConvMessage`] so the fold can replay the structured exchange).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ToolUse {
    pub id: String,
    pub name: String,
    /// JSON-encoded arguments, as the model produced them.
    pub args: String,
}

/// A minimal conversation message used by the compaction fold. Beyond plain
/// text it carries the structured tool exchange: an assistant message may hold
/// `tool_calls`; a `Tool`-role message carries the `tool_call_id` it answers.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConvMessage {
    pub role: ConvRole,
    pub text: String,
    /// Tool-use requests on an assistant message (empty otherwise).
    pub tool_calls: Vec<ToolUse>,
    /// The tool-use id a `Tool`-role message answers (`None` otherwise).
    pub tool_call_id: Option<String>,
}

impl ConvMessage {
    pub fn user(text: impl Into<String>) -> Self {
        Self {
            role: ConvRole::User,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn assistant(text: impl Into<String>) -> Self {
        Self {
            role: ConvRole::Assistant,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    pub fn system(text: impl Into<String>) -> Self {
        Self {
            role: ConvRole::System,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    /// A plain (text-only) message with a dynamic role.
    pub fn with_role(role: ConvRole, text: impl Into<String>) -> Self {
        Self {
            role,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: None,
        }
    }
    /// An assistant message that issued tool calls (text may be empty).
    pub fn assistant_tool_use(text: impl Into<String>, tool_calls: Vec<ToolUse>) -> Self {
        Self {
            role: ConvRole::Assistant,
            text: text.into(),
            tool_calls,
            tool_call_id: None,
        }
    }
    /// A tool-result message answering the tool-use `id`.
    pub fn tool(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            role: ConvRole::Tool,
            text: text.into(),
            tool_calls: Vec::new(),
            tool_call_id: Some(id.into()),
        }
    }
}

/// The event kind under which the agent loop records a tool-use REQUEST
/// (`{id,name,args}`) before invoking the tool — so the durable log holds the
/// full exchange and the fold can reconstruct the structured transcript.
pub const TOOL_CALL_EVENT_KIND: &str = "tool_call";

/// The durable payload of a [`TOOL_CALL_EVENT_KIND`] event.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ToolCallRecord {
    pub id: String,
    pub name: String,
    pub args: String,
}

impl ToolCallRecord {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("ToolCallRecord serializes")
    }
    pub fn from_bytes(b: &[u8]) -> Option<ToolCallRecord> {
        serde_json::from_slice(b).ok()
    }
}

/// The durable payload of a [`TOOL_CALL_EVENT_KIND`] event: the
/// WHOLE batch of tool calls one assistant step issued, plus any commentary
/// text the model emitted alongside them. Mirrors the canonical runtime's
/// `tool_call` event (`packages/agent-toolkit/src/session.rs` `kind::TOOL_CALL`).
///
/// Older events hold a bare [`ToolCallRecord`] (`{id,name,args}`), which fails
/// to parse here (no `calls` field) and is handled by the single-call fallback
/// in `compaction::replay_live`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ToolCallBatch {
    /// Assistant commentary emitted alongside the calls (may be empty).
    #[serde(default)]
    pub text: String,
    /// The calls, in the order the model issued them.
    pub calls: Vec<ToolUse>,
    /// The loop's step id for this batch (`{turn_id}:batch:{n}`). Each
    /// result the loop records names it (see [`ToolResultTag`]), so the fold
    /// pairs a result with its call by identity rather than by log position.
    /// Empty on batches recorded before results carried a tag.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub step: String,
}

impl ToolCallBatch {
    pub fn to_bytes(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("ToolCallBatch serializes")
    }
    pub fn from_bytes(b: &[u8]) -> Option<ToolCallBatch> {
        serde_json::from_slice(b).ok()
    }
}

/// The event kind under which the agent loop records one tool's result.
pub const TOOL_RESULT_EVENT_KIND: &str = "tool_result";

/// Which call a recorded tool result answers: the batch's step id, the call's
/// position in that batch, and the call id the transcript shows.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ToolResultTag {
    pub batch: String,
    pub index: usize,
    pub id: String,
}

/// First byte of a tagged result. A result from the registry is JSON or text
/// and never starts with NUL, so an untagged result written before tags
/// existed is never mistaken for a tagged one.
const TOOL_RESULT_TAG_MARK: u8 = 0;

/// The durable payload of a [`TOOL_RESULT_EVENT_KIND`] event: the tag as one
/// line of JSON behind a NUL mark, then the tool's raw result bytes.
pub fn encode_tool_result(tag: &ToolResultTag, payload: &[u8]) -> Vec<u8> {
    let header = serde_json::to_vec(tag).expect("ToolResultTag serializes");
    let mut out = Vec::with_capacity(header.len() + payload.len() + 2);
    out.push(TOOL_RESULT_TAG_MARK);
    out.extend_from_slice(&header);
    out.push(b'\n');
    out.extend_from_slice(payload);
    out
}

/// Split a [`TOOL_RESULT_EVENT_KIND`] payload into its tag (absent on an
/// older, untagged result) and the tool's raw result bytes.
pub fn decode_tool_result(data: &[u8]) -> (Option<ToolResultTag>, &[u8]) {
    if data.first() == Some(&TOOL_RESULT_TAG_MARK) {
        if let Some(nl) = data.iter().position(|b| *b == b'\n') {
            if let Ok(tag) = serde_json::from_slice::<ToolResultTag>(&data[1..nl]) {
                return (Some(tag), &data[nl + 1..]);
            }
        }
    }
    (None, data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_tagged_result_round_trips_and_an_untagged_one_reads_as_legacy() {
        let tag = ToolResultTag {
            batch: "t1:batch:0".into(),
            index: 2,
            id: "call_a".into(),
        };
        let bytes = encode_tool_result(&tag, b"{\"ok\":true}\nline two");
        let (decoded, payload) = decode_tool_result(&bytes);
        assert_eq!(decoded, Some(tag));
        assert_eq!(payload, b"{\"ok\":true}\nline two");

        let (decoded, payload) = decode_tool_result(b"{\"ok\":true}");
        assert_eq!(decoded, None);
        assert_eq!(payload, b"{\"ok\":true}");
    }

    #[test]
    fn a_batch_without_a_step_still_parses() {
        let legacy = br#"{"text":"","calls":[{"id":"a","name":"read_file","args":"{}"}]}"#;
        let batch = ToolCallBatch::from_bytes(legacy).expect("legacy batch parses");
        assert!(batch.step.is_empty());
        assert!(!String::from_utf8(batch.to_bytes()).unwrap().contains("step"));
    }
}
