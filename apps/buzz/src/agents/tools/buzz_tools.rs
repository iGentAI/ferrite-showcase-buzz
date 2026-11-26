//! The tools that act on the relay: posting and reading through the loopback seam.

use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use crate::agents::error::Result;
use super::{CloudToolRegistry, PostRecord, post_record};

impl<'a> CloudToolRegistry<'a> {
    /// One buzz workspace command — a function call into the relay's persona
    /// surface, which runs the same gates as any client write. The
    /// correlation id is a hash of `(scope, msg_id, tool, args)`, so a
    /// redelivered driver or a model retry of the SAME call lands on the SAME
    /// durable command: the relay pins the event identity under it and
    /// nothing posts twice. `pub(crate)`: the turn pipeline answers INTO the
    /// channel through this same seam.
    pub(crate) async fn invoke_buzz(&self, tool: &str, args: &[u8]) -> Result<Vec<u8>> {
        let Some(buzz) = &self.buzz else {
            return Self::err("buzz workspace tools are not available: the seam is not wired");
        };
        let v: Value = serde_json::from_slice(args).unwrap_or(serde_json::json!({}));

        // Build the persona op from the tool args (the fixed op vocabulary —
        // the relay decides what kinds these may mint).
        let op = match tool {
            "buzz_post" => {
                let channel = v.get("channel").and_then(Value::as_str).unwrap_or("");
                let content = v.get("content").and_then(Value::as_str).unwrap_or("");
                if channel.is_empty() || content.trim().is_empty() {
                    return Self::err("buzz_post requires 'channel' and non-empty 'content'");
                }
                crate::relay::persona::PersonaOp::Post {
                    channel: channel.to_string(),
                    content: content.to_string(),
                    reply_to: v
                        .get("reply_to")
                        .and_then(Value::as_str)
                        .filter(|s| !s.is_empty())
                        .map(str::to_string),
                }
            }
            "buzz_react" => {
                let target = v.get("target").and_then(Value::as_str).unwrap_or("");
                let emoji = v.get("emoji").and_then(Value::as_str).unwrap_or("");
                if target.is_empty() || emoji.is_empty() {
                    return Self::err("buzz_react requires 'target' and 'emoji'");
                }
                crate::relay::persona::PersonaOp::React {
                    target: target.to_string(),
                    emoji: emoji.to_string(),
                }
            }
            "buzz_read" => {
                let channel = v.get("channel").and_then(Value::as_str).unwrap_or("");
                if channel.is_empty() {
                    return Self::err("buzz_read requires 'channel'");
                }
                crate::relay::persona::PersonaOp::Read {
                    channel: channel.to_string(),
                    cursor: v.get("cursor").and_then(Value::as_str).map(str::to_string),
                    limit: v
                        .get("limit")
                        .and_then(Value::as_u64)
                        .map(|n| usize::try_from(n).unwrap_or(usize::MAX)),
                }
            }
            other => return Self::err(format!("unknown buzz tool: {other}")),
        };

        // Deterministic per (scope, turn, tool, args) — see the method doc.
        let sep = [0x1fu8];
        let mut hash = 0xcbf29ce484222325u64;
        for b in self
            .scope
            .as_bytes()
            .iter()
            .chain(sep.iter())
            .chain(buzz.msg_id.as_bytes().iter())
            .chain(sep.iter())
            .chain(tool.as_bytes().iter())
            .chain(sep.iter())
            .chain(args.iter())
        {
            hash ^= *b as u64;
            hash = hash.wrapping_mul(0x100000001b3);
        }
        let correlation_id = format!("{}:buzz:{hash:016x}", buzz.agent);

        // Writes speak in the channel: only the turn's current lease holder may.
        if !matches!(op, crate::relay::persona::PersonaOp::Read { .. }) {
            if let Some(fence) = &buzz.fence {
                if !fence.holds(self.ctx).await {
                    return Err(crate::agents::error::AgentError::Plane {
                        message: "this turn's lease was lost to a newer driver; not writing".into(),
                    });
                }
            }
        }

        // The write itself: every refusal (unregistered persona, membership,
        // moderation, rate limit) is a soft tool result the model can read,
        // never a plane error that kills the turn.
        match crate::relay::persona::execute_for(
            self.ctx,
            &buzz.agent,
            self.requester.as_deref(),
            &correlation_id,
            op,
        )
        .await
        {
            Ok(result) => {
                if tool == "buzz_post" {
                    match post_record(&result) {
                        // An ACCEPTED post is what "the persona already answered
                        // into the channel" means to the turn loop's
                        // post-the-answer fallback.
                        PostRecord::Landed => {
                            buzz.posts_made.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                        }
                        // A success envelope that does not say the post landed (a
                        // retryable answer, an unexpected shape) is as uncertain
                        // as a storage error: never recorded, retried with the
                        // same correlation identity.
                        PostRecord::Unknown => {
                            return Err(crate::agents::error::AgentError::Plane {
                                message: format!(
                                    "the buzz_post write's outcome is unknown ({result}); it is retried with the same identity"
                                ),
                            });
                        }
                        PostRecord::NotPosted => {}
                    }
                } else if tool == "buzz_react" && post_record(&result) == PostRecord::Unknown {
                    // A reaction is a channel write too: a success envelope that
                    // does not settle its outcome is never recorded (a replay
                    // never repeats a recorded call), so it fails as a plane
                    // error and is retried under the same correlation identity.
                    return Err(crate::agents::error::AgentError::Plane {
                        message: format!(
                            "the buzz_react write's outcome is unknown ({result}); it is retried with the same identity"
                        ),
                    });
                }
                Self::ok(result)
            }
            // A storage or internal failure, or a lost race, may have committed the
            // write: its outcome is UNKNOWN. Recording it as a result would freeze
            // that uncertainty into the log (a replay never repeats a recorded
            // call), so the call FAILS instead: the turn stays unsettled, and its
            // replay repeats this identical, correlation-pinned write until the
            // relay gives a definite answer.
            Err(
                e @ (crate::relay::BuzzError::Storage(_)
                | crate::relay::BuzzError::Internal(_)
                | crate::relay::BuzzError::Conflict(_)),
            ) => Err(crate::agents::error::AgentError::Plane {
                message: format!("the {tool} write's outcome is unknown ({e}); it is retried with the same identity"),
            }),
            // A refusal: nothing was written, and the model can read why.
            Err(e) => Self::ok(serde_json::json!({
                "ok": false,
                "error": e.to_string(),
                "retryable": false,
            })),
        }
    }
}
