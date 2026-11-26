//! The tools that create, list and cancel a persona's schedules.

use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use crate::agents::error::Result;
use super::{BUZZ_COMMUNITY, CloudToolRegistry, schedule_write_failure};

impl<'a> CloudToolRegistry<'a> {
    /// One scheduling tool: a persona arranging, listing, or cancelling its OWN
    /// future turn.
    ///
    /// The agent id comes from the binding, never from the arguments, so a persona
    /// cannot schedule another one — there is no cross-agent case to authorize.
    /// `created_by` is that same agent id, which is what lets it cancel its own
    /// schedules without being able to touch a person's.
    ///
    /// Every failure is a SOFT tool result, like the other seams: "you cannot post
    /// in that channel" is something the model should read and say, not something
    /// that should end the turn.
    pub(super) async fn invoke_schedule(&self, tool: &str, args: &[u8]) -> Result<Vec<u8>> {
        let Some(buzz) = &self.buzz else {
            return Self::err("scheduling is not available: the workspace seam is not wired");
        };
        // Creating or cancelling a schedule is a write with future cost: like a
        // channel write, only the turn's current lease holder may make one. A lost
        // lease is a plane error (nothing recorded), so the newer driver's replay
        // decides instead.
        if tool != "buzz_schedules" {
            if let Some(fence) = &buzz.fence {
                if !fence.holds(self.ctx).await {
                    return Err(crate::agents::error::AgentError::Plane {
                        message: "this turn's lease was lost to a newer driver; not scheduling".into(),
                    });
                }
            }
        }
        let community = BUZZ_COMMUNITY;
        let v: Value = serde_json::from_slice(args).unwrap_or(serde_json::json!({}));
        let mut config = match crate::relay::planes::store(self.ctx, crate::relay::store::CONFIG) {
            Ok(config) => config,
            Err(e) => return Self::err(format!("the config plane is unavailable: {e}")),
        };

        match tool {
            "buzz_schedule" => {
                let channel = v
                    .get("channel")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                let prompt = v.get("prompt").and_then(Value::as_str).unwrap_or("");
                if channel.is_empty() || prompt.trim().is_empty() {
                    return Self::err(
                        "buzz_schedule requires a 'channel' and a non-empty 'prompt' \
                         (the instruction your future self will be given)",
                    );
                }
                // The persona must be registered AND able to post in that room,
                // checked BEFORE the row is written. Skipping it would still be safe
                // — the eventual post is membership-gated — but the wake would have
                // spent a model call to produce an answer with nowhere to go.
                let harness = match crate::relay::ingress::persona_schedule_context(
                    self.ctx,
                    community,
                    &buzz.agent,
                    channel,
                )
                .await
                {
                    Ok(harness) => harness,
                    Err(e) => return Self::ok(serde_json::json!({ "ok": false, "error": e.to_string() })),
                };
                let now_ms = self.ctx.now_ms();
                let request = crate::relay::schedules::ScheduleRequest {
                    agent: buzz.agent.clone(),
                    channel: channel.to_string(),
                    prompt: prompt.to_string(),
                    // RELATIVE, because a model reasons in "an hour from now" and has
                    // no reliable notion of the wall clock. Absent means as soon as
                    // possible, which `build` normalises.
                    first_due_ms: now_ms
                        .saturating_add(v.get("in_ms").and_then(Value::as_u64).unwrap_or(0)),
                    interval_ms: v.get("interval_ms").and_then(Value::as_u64),
                    occurrences: v
                        .get("occurrences")
                        .and_then(Value::as_u64)
                        .map_or(1, |n| u32::try_from(n).unwrap_or(u32::MAX)),
                    // The persona's own prefixed principal, never a member pubkey.
                    created_by: crate::relay::schedules::agent_creator(&buzz.agent),
                };
                match crate::relay::schedules::create(
                    self.ctx,
                    &mut config,
                    community,
                    &request,
                    &harness,
                    now_ms,
                )
                .await
                {
                    Ok(schedule) => Self::ok(serde_json::json!({
                        "ok": true,
                        "schedule": schedule.id,
                        "next_due_ms": schedule.next_due_ms,
                        "remaining": schedule.remaining,
                        "note": "you will be woken with this prompt; nothing else will remind you",
                    })),
                    // Every refusal names its bound (see `schedules::build`), which is
                    // what makes it actionable rather than just a no. A plane error
                    // is uncertain, never a definite failure.
                    Err(e) => Self::ok(schedule_write_failure(tool, &e)),
                }
            }
            "buzz_schedules" => {
                match crate::relay::schedules::list(&mut config, community).await {
                    Ok((all, truncated)) => {
                        // `created_by`, not `agent`: what this tool lists must be what
                        // `buzz_unschedule` can cancel, and that is authorized against
                        // `created_by`. Filtering on `agent` listed wakes a PERSON had
                        // arranged for this persona — ids it could not act on.
                        //
                        // The `agent` check stays as a consistency guard. It should be
                        // implied, since a persona's own schedule names itself; if it
                        // ever is not, that is a corrupted row rather than something to
                        // show a model as its own work.
                        let mine: Vec<Value> = all
                            .iter()
                            .filter(|s| {
                                s.is_created_by_agent(&buzz.agent)
                                    && s.agent == buzz.agent
                                    && s.is_live()
                            })
                            .map(|s| {
                                serde_json::json!({
                                    "schedule": s.id,
                                    "channel": s.channel,
                                    "prompt": s.prompt,
                                    "next_due_ms": s.next_due_ms,
                                    "interval_ms": s.interval_ms,
                                    "remaining": s.remaining,
                                })
                            })
                            .collect();
                        Self::ok(serde_json::json!({
                            "ok": true,
                            "schedules": mine,
                            // A truncated list must say so rather than imply it is
                            // complete: "I have none" and "I could not see them all"
                            // lead to different next actions.
                            "complete": !truncated,
                        }))
                    }
                    Err(e) => Self::ok(serde_json::json!({ "ok": false, "error": e.to_string() })),
                }
            }
            "buzz_unschedule" => {
                let id = v
                    .get("schedule")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .trim();
                if id.is_empty() {
                    return Self::err(
                        "buzz_unschedule requires a 'schedule' id — call buzz_schedules first",
                    );
                }
                // `privileged: false`, so this can only cancel a row whose
                // `created_by` is this persona's own agent id.
                match crate::relay::schedules::cancel(
                    self.ctx,
                    &mut config,
                    community,
                    id,
                    &crate::relay::schedules::agent_creator(&buzz.agent),
                    false,
                )
                .await
                {
                    Ok(cancelled) => Self::ok(serde_json::json!({
                        "ok": true,
                        "schedule": id,
                        "cancelled": cancelled,
                        "note": if cancelled {
                            "future occurrences will not run"
                        } else {
                            "there was nothing live to cancel under that id"
                        },
                    })),
                    Err(e) => Self::ok(schedule_write_failure(tool, &e)),
                }
            }
            other => Self::err(format!("unknown schedule tool: {other}")),
        }
    }
}
