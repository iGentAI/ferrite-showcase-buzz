//! A steering-aware [`ModelClient`]: it assembles each turn's prompt (base
//! instructions + the ordered context-fragment bundle + the folded conversation)
//! and delegates the actual completion to a [`ChatBackend`].
//!
//! In this bundle the [`ChatBackend`] is a direct in-process call into
//! [`crate::llm`], which carries its own compiled model catalog.

use std::future::Future;
use std::sync::{Arc, Mutex};

use ferrite_app::serde::{Deserialize, Serialize};

use crate::agents::agent_loop::{ModelClient, ModelStep, StepToolCall};
use crate::agents::compaction::fold_session_events;
use crate::agents::conv::{ConvMessage, ConvRole};
use crate::agents::error::{EventDoc, Result};
use crate::agents::fragments::{FragmentRole, RenderedFragment};

/// A snapshot of cumulative model token usage.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct TokenUsage {
    pub input_tokens: i64,
    pub cached_input_tokens: i64,
    pub output_tokens: i64,
}

impl TokenUsage {
    /// Non-cached input: `(input - cached).max(0)`.
    pub fn non_cached_input(&self) -> i64 {
        (self.input_tokens - self.cached_input_tokens).max(0)
    }
}

/// The soft per-step charge: `non_cached_input + max(output,0)` (cached input
/// excluded; reasoning not double-counted).
pub fn goal_token_delta(usage: &TokenUsage) -> i64 {
    usage
        .non_cached_input()
        .saturating_add(usage.output_tokens.max(0))
}

/// A tool call the model requested.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCallRequest {
    pub id: String,
    pub tool: String,
    pub args: Vec<u8>,
}

/// The result of a single completion.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChatOutcome {
    /// The assistant's text (used when there is no tool call).
    pub text: String,
    /// Tool calls requested this turn (all calls in the batch are executed).
    pub tool_calls: Vec<ToolCallRequest>,
    /// The token usage this completion consumed.
    pub usage: TokenUsage,
}

/// A completion backend the [`SteeringModelClient`] delegates to.
pub trait ChatBackend {
    fn complete(
        &self,
        messages: &[ConvMessage],
    ) -> impl Future<Output = Result<ChatOutcome>> + Send;
}

fn role_to_conv(role: FragmentRole) -> ConvRole {
    match role {
        FragmentRole::Developer => ConvRole::System,
        FragmentRole::User => ConvRole::User,
    }
}

/// A `ModelClient` that performs steering-aware prompt assembly around a backend.
pub struct SteeringModelClient<B: ChatBackend> {
    backend: B,
    base_instructions: String,
    developer_fragments: Vec<RenderedFragment>,
    user_fragments: Vec<RenderedFragment>,
    last_usage: Arc<Mutex<TokenUsage>>,
}

impl<B: ChatBackend> SteeringModelClient<B> {
    pub fn new(
        backend: B,
        base_instructions: impl Into<String>,
        developer_fragments: Vec<RenderedFragment>,
        user_fragments: Vec<RenderedFragment>,
    ) -> Self {
        Self {
            backend,
            base_instructions: base_instructions.into(),
            developer_fragments,
            user_fragments,
            last_usage: Arc::new(Mutex::new(TokenUsage::default())),
        }
    }

    /// A shared handle to the most recent turn's usage.
    pub fn usage_handle(&self) -> Arc<Mutex<TokenUsage>> {
        Arc::clone(&self.last_usage)
    }

    /// The reinjected context bundle: base instructions, then developer
    /// fragments, then user fragments — the prefix the conversation folds onto.
    fn reinjected(&self) -> Vec<ConvMessage> {
        let mut v = vec![ConvMessage::system(self.base_instructions.clone())];
        for f in self
            .developer_fragments
            .iter()
            .chain(self.user_fragments.iter())
        {
            v.push(ConvMessage::with_role(role_to_conv(f.role), f.text.clone()));
        }
        v
    }

    /// The messages this client would send for `history` (exposed for tests).
    pub fn assemble(&self, history: &[EventDoc]) -> Vec<ConvMessage> {
        fold_session_events(history, &self.reinjected())
    }
}

impl<B: ChatBackend + Sync> ModelClient for SteeringModelClient<B> {
    async fn step(&self, history: &[EventDoc]) -> Result<ModelStep> {
        let messages = self.assemble(history);
        let outcome = self.backend.complete(&messages).await?;
        {
            let mut usage = self.last_usage.lock().expect("usage lock");
            usage.input_tokens = usage
                .input_tokens
                .saturating_add(outcome.usage.input_tokens);
            usage.cached_input_tokens = usage
                .cached_input_tokens
                .saturating_add(outcome.usage.cached_input_tokens);
            usage.output_tokens = usage
                .output_tokens
                .saturating_add(outcome.usage.output_tokens);
        }
        let calls: Vec<StepToolCall> = outcome
            .tool_calls
            .into_iter()
            .map(|tc| StepToolCall {
                id: tc.id,
                tool: tc.tool,
                args: tc.args,
            })
            .collect();
        if calls.is_empty() {
            Ok(ModelStep::Final { text: outcome.text })
        } else {
            Ok(ModelStep::ToolCalls {
                text: outcome.text,
                calls,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::fragments::{ContextFragment, EnvironmentContext};

    fn ev(seq: u64, kind: &str, data: &[u8]) -> EventDoc {
        EventDoc {
            seq,
            kind: kind.into(),
            data: data.to_vec(),
            ts_ms: 0,
        }
    }

    struct EchoBackend {
        usage: TokenUsage,
    }
    impl ChatBackend for EchoBackend {
        async fn complete(&self, messages: &[ConvMessage]) -> Result<ChatOutcome> {
            Ok(ChatOutcome {
                text: format!("saw {} messages", messages.len()),
                tool_calls: Vec::new(),
                usage: self.usage,
            })
        }
    }

    fn client() -> SteeringModelClient<EchoBackend> {
        let env = EnvironmentContext {
            cwd: Some("/r".into()),
            ..Default::default()
        }
        .rendered();
        SteeringModelClient::new(
            EchoBackend {
                usage: TokenUsage {
                    input_tokens: 900,
                    cached_input_tokens: 400,
                    output_tokens: 80,
                },
            },
            "BASE",
            vec![],
            vec![env],
        )
    }

    #[test]
    fn token_delta_excludes_cached() {
        let u = TokenUsage {
            input_tokens: 900,
            cached_input_tokens: 400,
            output_tokens: 80,
        };
        assert_eq!(goal_token_delta(&u), 580);
    }

    #[test]
    fn assembles_base_then_fragments_then_conversation() {
        let c = client();
        let history = vec![
            ev(1, "message_in", b"hello"),
            ev(2, "message_out", b"hi there"),
            ev(
                3,
                "message_in",
                "<goal_context>\nCONTINUE\n</goal_context>".as_bytes(),
            ),
        ];
        let msgs = c.assemble(&history);
        assert_eq!(msgs[0].text, "BASE");
        assert!(msgs[1].text.starts_with("<environment_context>"));
        assert_eq!(msgs[2], ConvMessage::user("hello"));
        assert_eq!(msgs[3], ConvMessage::assistant("hi there"));
        assert!(msgs[4].text.contains("CONTINUE"));
    }

    #[tokio::test]
    async fn step_records_usage_and_maps_final() {
        let c = client();
        let usage = c.usage_handle();
        let step = c.step(&[ev(1, "message_in", b"hi")]).await.unwrap();
        assert!(matches!(step, ModelStep::Final { .. }));
        let u = *usage.lock().unwrap();
        assert_eq!(
            u,
            TokenUsage {
                input_tokens: 900,
                cached_input_tokens: 400,
                output_tokens: 80,
            }
        );
        assert_eq!(goal_token_delta(&u), 580);
    }

    struct MultiBackend {
        text: String,
        calls: Vec<ToolCallRequest>,
    }
    impl ChatBackend for MultiBackend {
        async fn complete(&self, _messages: &[ConvMessage]) -> Result<ChatOutcome> {
            Ok(ChatOutcome {
                text: self.text.clone(),
                tool_calls: self.calls.clone(),
                usage: TokenUsage::default(),
            })
        }
    }

    #[tokio::test]
    async fn step_keeps_every_tool_call_and_the_text() {
        let backend = MultiBackend {
            text: "let me check two things".into(),
            calls: vec![
                ToolCallRequest {
                    id: "c1".into(),
                    tool: "read_file".into(),
                    args: b"{}".to_vec(),
                },
                ToolCallRequest {
                    id: "c2".into(),
                    tool: "search".into(),
                    args: b"{}".to_vec(),
                },
            ],
        };
        let c = SteeringModelClient::new(backend, "BASE", vec![], vec![]);
        let step = c.step(&[ev(1, "message_in", b"hi")]).await.unwrap();
        match step {
            ModelStep::ToolCalls { text, calls } => {
                assert_eq!(
                    text, "let me check two things",
                    "commentary text is preserved"
                );
                assert_eq!(calls.len(), 2, "both calls survive");
                assert_eq!(calls[0].id, "c1");
                assert_eq!(calls[1].id, "c2");
            }
            other => panic!("expected ToolCalls, got {other:?}"),
        }
    }
}
