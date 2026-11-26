//! The Fabric-backed turn: its context generation, retirement and exchange.

use std::time::Duration;
use ferrite_app::rt;
use ferrite_app::serde_json;
use ferrite_app::AppContext;
use crate::agents::tools::CloudToolRegistry;
use super::{MEMORY_STORE, STAGE_ROUNDS};

/// Run one A2A turn under this buzz thread's durable context, with the
/// MEASURED failed-task rule applied: a FAILED task usually means the turn
/// was superseded on the Fabric side, so the context generation
/// (`a2a/ctx/{agent}/{thread}` in the agent-memory store) is bumped and the
/// send retried ONCE on a fresh context, inside the same wall-clock deadline.
///
/// The outer `Err` means nothing was sent because the context generation
/// could not be established (the marker store or its record is unreadable):
/// the delivery is retried rather than answered, since falling back to the
/// first generation could redial a context already retired as poisoned.
pub(super) async fn run_fabric_context_turn(
    ctx: &AppContext,
    registry: &CloudToolRegistry<'_>,
    agent: &str,
    thread: &str,
    message: &str,
) -> std::result::Result<std::result::Result<String, String>, String> {
    let prompt = a2a_bridge_prompt(message);
    let mut markers = ctx
        .store(MEMORY_STORE)
        .map_err(|e| format!("marker store: {e}"))?;
    let generation_key = format!("a2a/ctx/{agent}/{thread}");
    let generation = read_context_generation(&mut markers, &generation_key).await?;
    // The remote context is per PERSONA and thread, like the local generation
    // record: two personas in one channel never share a Fabric conversation
    // (or retire each other's).
    let context_key = persona_context_key(agent, thread);
    Ok(fabric_context_exchange(ctx, registry, &mut markers, &generation_key, &context_key, thread, &prompt, generation).await)
}

/// The Fabric thread key of one persona's conversation in one Buzz thread.
/// Prefixed so it can never equal a model-chosen key (`tool~…`) or another
/// persona's.
pub(super) fn persona_context_key(agent: &str, thread: &str) -> String {
    format!("persona~{agent}~{thread}")
}

/// Parse a stored context generation (`{"generation": n}`, n >= 1).
pub(super) fn parse_context_generation(raw: &[u8]) -> Option<u64> {
    serde_json::from_slice::<serde_json::Value>(raw)
        .ok()?
        .get("generation")?
        .as_u64()
        .filter(|g| *g >= 1)
}

/// The current context generation: 1 only when NO record exists. A failed read
/// or an unreadable record is an error, never generation 1.
pub(super) async fn read_context_generation(
    markers: &mut ferrite_app::client::StoreClusterClient,
    key: &str,
) -> std::result::Result<u64, String> {
    match markers.get(key.as_bytes()).await {
        Ok(None) => Ok(1),
        Ok(Some(raw)) => parse_context_generation(&raw)
            .ok_or_else(|| format!("the context generation record {key} is unreadable")),
        Err(e) => Err(format!("the context generation {key} could not be read: {e}")),
    }
}

/// Retire generation `from` MONOTONICALLY: compare-and-swap the record to
/// `from + 1` unless it already holds a newer generation, re-reading after a
/// conflict, and never lowering it. Returns the generation now stored (the
/// context to dial next): a slower driver can never overwrite a newer
/// retirement and send later mentions back to a retired context.
pub(super) async fn retire_context_generation(
    markers: &mut ferrite_app::client::StoreClusterClient,
    key: &str,
    from: u64,
) -> std::result::Result<u64, String> {
    for _ in 0..STAGE_ROUNDS {
        let (current, version) = match markers.get_versioned(key.as_bytes()).await {
            Ok(Some((raw, version))) => match parse_context_generation(&raw) {
                Some(g) => (g, Some(version)),
                None => return Err(format!("the context generation record {key} is unreadable")),
            },
            Ok(None) => (1, None),
            Err(e) => return Err(format!("the context generation {key} could not be read: {e}")),
        };
        if current > from {
            return Ok(current);
        }
        // CHECKED: a saturated generation would "retire" to itself and the
        // retry would redial the very context it means to leave.
        let Some(next) = from.checked_add(1) else {
            return Err(format!("the context generation {key} is exhausted; it cannot be retired again"));
        };
        let doc = serde_json::json!({ "generation": next }).to_string();
        match markers.put_if_version(key.as_bytes(), doc.as_bytes(), version).await {
            Ok(Ok(_)) => return Ok(next),
            Ok(Err(_)) => continue,
            Err(e) => return Err(format!("the context generation {key} could not be recorded: {e}")),
        }
    }
    Err(format!("the context generation {key} kept changing; the retirement was not recorded"))
}

/// Work that runs once the ceiling is already spent (retiring the wedged
/// context, the operator alert) is bounded by this grace each, so a slow store
/// or alert service can never hold the refusal back: the retirement is then
/// reported as not recorded, and the next mention's own retirement repairs it.
pub(super) const FABRIC_POST_CEILING_GRACE_MS: u64 = 2_000;

pub(super) async fn retire_within_grace(
    markers: &mut ferrite_app::client::StoreClusterClient,
    key: &str,
    from: u64,
) -> bool {
    matches!(
        rt::time::timeout(
            Duration::from_millis(FABRIC_POST_CEILING_GRACE_MS),
            retire_context_generation(markers, key, from)
        )
        .await,
        Ok(Ok(_))
    )
}

/// Retire `from` within `remaining_ms`, the time left before the exchange's
/// ceiling, so the fresh-context retry can never start after the ceiling.
/// With no time left nothing is attempted; the next mention's own exchange
/// retires the context then.
pub(super) async fn retire_before_ceiling(
    markers: &mut ferrite_app::client::StoreClusterClient,
    key: &str,
    from: u64,
    remaining_ms: u64,
) -> std::result::Result<u64, String> {
    if remaining_ms == 0 {
        return Err("the ceiling was already spent".to_string());
    }
    match rt::time::timeout(
        Duration::from_millis(remaining_ms),
        retire_context_generation(markers, key, from),
    )
    .await
    {
        Ok(result) => result,
        Err(_) => Err("the ceiling was reached before it was recorded".to_string()),
    }
}

pub(super) async fn alert_within_grace(ctx: &AppContext, code: &str, message: &str) {
    let _ = rt::time::timeout(
        Duration::from_millis(FABRIC_POST_CEILING_GRACE_MS),
        crate::relay::alerts::operational(ctx, "warn", code, message),
    )
    .await;
}

/// One peer exchange on `generation`, with the single fresh-context retry.
#[allow(clippy::too_many_arguments)]
pub(super) async fn fabric_context_exchange(
    ctx: &AppContext,
    registry: &CloudToolRegistry<'_>,
    markers: &mut ferrite_app::client::StoreClusterClient,
    generation_key: &str,
    context_key: &str,
    thread: &str,
    prompt: &str,
    generation: u64,
) -> std::result::Result<String, String> {
    let prompt = prompt.to_string();
    // ONE deadline covers the send, the polling, and the single fresh-context
    // retry: the promise to the channel is "an answer or a refusal within the
    // ceiling", not "twice the ceiling when Fabric misbehaves twice".
    let deadline_ms = ctx
        .now_ms()
        .saturating_add(crate::agents::tools::FABRIC_A2A_WALL_MS);
    let context_id = crate::agents::tools::fabric_context_id(context_key, generation);
    use crate::agents::tools::FabricTurnOutcome;
    match registry
        .fabric_turn(&prompt, &context_id, deadline_ms)
        .await
    {
        Ok(FabricTurnOutcome {
            task_id,
            state,
            text,
        }) if state == "completed" => {
            text.ok_or_else(|| format!("the a2a task {task_id} completed without any text"))
        }
        // A wedged context (still non-terminal at the ceiling) is retired the
        // same way a failed one is — but WITHOUT an in-request retry, because
        // the ceiling is spent. The bump is what stops the next mention in
        // this channel queueing behind the same stuck task.
        Ok(FabricTurnOutcome { task_id, state, .. })
            if state == crate::agents::tools::BUDGET_EXHAUSTED =>
        {
            let recorded = retire_within_grace(markers, generation_key, generation).await;
            let which = if task_id.is_empty() {
                "the peer stayed busy".to_string()
            } else {
                format!("the a2a task {task_id} was still running")
            };
            // The context is retired automatically, so the channel recovers on
            // the next mention — but a peer that wedges repeatedly is an
            // operator's problem, and nothing else would report it.
            alert_within_grace(
                ctx,
                crate::relay::alerts::FABRIC_WEDGED,
                &format!(
                    "{which} at the {}s ceiling for thread {thread}; its Fabric context \
                     was retired so later mentions do not queue behind it",
                    crate::agents::tools::FABRIC_A2A_WALL_MS / 1_000
                ),
            )
            .await;
            Err(format!(
                "{which} at the {}s ceiling; this conversation's Fabric context \
                 has been retired{} — mention me again",
                crate::agents::tools::FABRIC_A2A_WALL_MS / 1_000,
                if recorded {
                    ""
                } else {
                    " (the marker could not be recorded)"
                }
            ))
        }
        Ok(FabricTurnOutcome { task_id, state, .. }) if state == "failed" => {
            // Record the retirement BEFORE retrying: even if the retry dies, the
            // poisoned context is never dialed again. The retry dials the
            // generation actually committed (a newer one, if another driver
            // retired this context first).
            let remaining_ms = deadline_ms.saturating_sub(ctx.now_ms());
            let next = retire_before_ceiling(markers, generation_key, generation, remaining_ms)
                .await
                .map_err(|e| {
                    format!("the a2a task {task_id} failed and the fresh-context marker could not be recorded: {e}")
                })?;
            let fresh = crate::agents::tools::fabric_context_id(context_key, next);
            match registry.fabric_turn(&prompt, &fresh, deadline_ms).await {
                Ok(FabricTurnOutcome {
                    task_id,
                    state,
                    text,
                }) if state == "completed" => {
                    text.ok_or_else(|| format!("the a2a task {task_id} completed without any text"))
                }
                Ok(FabricTurnOutcome { task_id, state, .. }) => {
                    // The fresh context failed or wedged too: retire IT as well,
                    // durably, so the next mention does not dial a poisoned or
                    // stuck context again. No third send inside this deadline.
                    let recorded = retire_within_grace(markers, generation_key, next).await;
                    if state == crate::agents::tools::BUDGET_EXHAUSTED {
                        alert_within_grace(
                            ctx,
                            crate::relay::alerts::FABRIC_WEDGED,
                            &format!(
                                "the a2a task {task_id} on a fresh context was still running at the \
                                 ceiling for thread {thread}; that context was retired too"
                            ),
                        )
                        .await;
                    }
                    Err(format!(
                        "the a2a task {task_id} ended '{state}' even on a fresh context; that context \
                         has been retired{} — mention me again",
                        if recorded { "" } else { " (the marker could not be recorded)" }
                    ))
                }
                Err(e) => Err(e),
            }
        }
        Ok(FabricTurnOutcome { task_id, state, .. }) => {
            Err(format!("the a2a task {task_id} ended '{state}'"))
        }
        Err(e) => Err(e),
    }
}

/// Orient a remote A2A peer to the part Buzz owns around its answer. The peer
/// produces reply text; Buzz posts it into the originating thread through the
/// persona surface after the A2A task completes. Without this small protocol
/// preamble, honest remote agents repeatedly report that they cannot reply
/// because they do not have a Buzz posting tool—even though the bridge posts
/// their result immediately afterward.
pub(super) fn a2a_bridge_prompt(trigger: &str) -> String {
    format!(
        "You are answering a mention through the Buzz A2A bridge.\n\
         Buzz will post your final answer into the originating Buzz thread on your behalf; \
         return the reply content and do not require or search for a channel-posting tool.\n\
         The triggering mention below is authoritative input. You cannot independently inspect \
         unquoted channel history or delivery receipts, so do not invent them; state precisely \
         when the supplied context is insufficient.\n\n\
         <buzz_mention>\n{trigger}\n</buzz_mention>"
    )
}
