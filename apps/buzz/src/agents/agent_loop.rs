//! The durable, idempotent agent tool loop, with step-boundary interruption
//! (`conversation/turn.cancel`). Every step is recorded through the
//! [`SessionEngine`](crate::agents::session::SessionEngine) with a per-step
//! idempotency id, so a crashed/retried runner re-derives the SAME history.
//! Bounded by `max_steps` (a runaway loop fails closed with `StepBudget`).

use std::collections::HashSet;
use std::future::Future;

use crate::agents::conv::{
    encode_tool_result, ToolCallBatch, ToolResultTag, ToolUse, TOOL_CALL_EVENT_KIND,
    TOOL_RESULT_EVENT_KIND,
};
use crate::agents::error::{AgentError, EventDoc, Result, Scope};
use crate::agents::session::{Advanced, ApeOps, SessionEngine};

/// One tool call inside a model step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepToolCall {
    /// The model's id for this call. Transcript metadata only: the loop keys
    /// idempotency by position and renames an id that is empty, malformed, or
    /// already used in the turn.
    pub id: String,
    pub tool: String,
    pub args: Vec<u8>,
}

/// One step a model produces in the tool loop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ModelStep {
    /// Invoke every call in `calls`; each is recorded and invoked under its own
    /// positional `{turn_id}:batch:{n}:call:{i}` idempotency id. `text` is any commentary the model
    /// emitted alongside the calls (preserved so the fold can replay it).
    ToolCalls {
        text: String,
        calls: Vec<StepToolCall>,
    },
    /// The final assistant answer; the turn finalizes.
    Final { text: String },
}

/// A model the agent loop drives. `step` sees the turn's recorded history and
/// returns the next [`ModelStep`]. It MUST be a pure function of the history so
/// a replay after a crash re-derives the same decision.
pub trait ModelClient {
    fn step(&self, history: &[EventDoc]) -> impl Future<Output = Result<ModelStep>> + Send;
}

/// The tools the agent loop can invoke.
pub trait ToolRegistry {
    fn invoke(&self, tool: &str, args: &[u8]) -> impl Future<Output = Result<Vec<u8>>> + Send;

    /// Whether repeating an interrupted call of `tool` with the same arguments
    /// is safe: a read, or a write whose identity is pinned so a repeat is
    /// absorbed. Anything else (an edit, a patch, a connector call) is NOT
    /// repeated when a crash or a competing driver left its outcome unknown;
    /// it is answered "outcome unknown" instead. Defaults to not safe.
    fn replay_safe(&self, _tool: &str) -> bool {
        false
    }

    /// Told when a recorded call of `tool` is settled from the log instead of
    /// being invoked (its result was already recorded, or it is answered
    /// "outcome unknown"), so a registry that bounds calls per turn counts it
    /// exactly as an invocation would have. Defaults to nothing.
    fn note_recorded(&self, _tool: &str) {}
}

/// The event kind recording that one driver STARTED a non-replay-safe call
/// (`{call_step}:intent`). Invisible to the transcript fold.
pub const TOOL_INTENT_EVENT_KIND: &str = "tool_intent";

/// Run (or settle) one recorded call at `{batch_step}:call:{i}`.
///
/// A replay-safe tool is simply invoked. Any other tool first records an
/// intent step only one writer can create: the creator invokes it; a driver
/// that finds the intent already recorded without a result (a crash after the
/// call started, or another driver running it) does NOT repeat it and records
/// an explicit "outcome unknown, not repeated" result, so the model can check
/// the state instead of applying the effect twice.
///
/// Boundary, stated plainly: this prevents automatic repeats, not concurrent
/// execution. Drivers of one turn are serialized by the turn's lease (renewed
/// by the running process; see `turns::LeaseRenewer`), so a second driver
/// reaches an intent without a result only after the first driver's lease
/// lapsed unrenewed, i.e. its process died or was cut off. If it was only cut
/// off, its call may still complete after "unknown" is recorded; its late
/// result is then absorbed by the step id and the model has already been told
/// the outcome is unknown. Tools that need exactly-once effects must carry
/// their own idempotency key.
#[allow(clippy::too_many_arguments)]
async fn run_recorded_call<A, T>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    tools: &T,
    batch_step: &str,
    index: usize,
    call: &ToolUse,
    args: &[u8],
    // Whether a call settled from the log is reported through
    // `ToolRegistry::note_recorded`. Only the tool loop's single pass over the
    // batches reports; `finish_torn_batches` re-walks calls the loop already
    // counted (it runs again when a turn finalizes), so it never does.
    note: bool,
) -> Result<()>
where
    A: ApeOps,
    T: ToolRegistry,
{
    let call_step = format!("{batch_step}:call:{index}");
    if engine.step_seq(scope, &call_step).await?.is_some() {
        if note {
            tools.note_recorded(&call.name);
        }
        return Ok(());
    }
    let tag = ToolResultTag {
        batch: batch_step.to_string(),
        index,
        id: call.id.clone(),
    };
    if !tools.replay_safe(&call.name) {
        let intent_step = format!("{call_step}:intent");
        match engine
            .record_event_unless(scope, &intent_step, TOOL_INTENT_EVENT_KIND, b"", &call_step)
            .await?
        {
            Advanced::Applied(_) => {}
            Advanced::Blocked(_) => {
                if note {
                    tools.note_recorded(&call.name);
                }
                return Ok(());
            }
            Advanced::Existed(_) => {
                if note {
                    tools.note_recorded(&call.name);
                }
                let unknown = ferrite_app::serde_json::json!({
                    "ok": false,
                    "outcome_unknown": true,
                    "error": format!(
                        "the earlier '{}' call was interrupted before its result was recorded; it may or may not \
                         have taken effect, and it was NOT repeated. Check the current state before acting on it.",
                        call.name
                    ),
                });
                engine
                    .record_event(
                        scope,
                        &call_step,
                        TOOL_RESULT_EVENT_KIND,
                        &encode_tool_result(&tag, unknown.to_string().as_bytes()),
                    )
                    .await?;
                return Ok(());
            }
        }
    }
    let result = tools.invoke(&call.name, args).await?;
    engine
        .record_event(scope, &call_step, TOOL_RESULT_EVENT_KIND, &encode_tool_result(&tag, &result))
        .await?;
    Ok(())
}

/// A step-boundary control the loop consults before each model step.
/// `Some(reason)` stops the turn AT THAT BOUNDARY — a step already in flight
/// finishes and is recorded, so cancellation never truncates durable history.
pub trait TurnGuard {
    fn interrupt(&self) -> impl Future<Output = Option<String>> + Send;
}

/// The no-op guard: a turn nothing can interrupt.
pub struct Uninterrupted;

impl TurnGuard for Uninterrupted {
    async fn interrupt(&self) -> Option<String> {
        None
    }
}

/// The recorded answer of a turn stopped by its guard. Durable: a replay of
/// the same turn returns this text like any other finalized answer. Whether a
/// turn WAS cancelled is never inferred from this text (a model may say the
/// same words): it is the [`cancel_step`] marker committed with the final.
pub const CANCELLED_ANSWER: &str = "Turn cancelled.";

/// The step marker committed in the same envelope as a guard-stopped turn's
/// final answer, so it exists exactly when the cancellation is the recorded
/// outcome.
fn cancel_step(turn_id: &str) -> String {
    format!("{turn_id}:final:cancelled")
}

/// The outcome recorded at `seq` (the turn's final step).
async fn recorded_outcome<A: ApeOps>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    turn_id: &str,
    seq: u64,
) -> Result<LoopOutcome> {
    // A recorded final answer that cannot be read is an error, never a reason
    // to generate again.
    let ev = engine.event_at(scope, seq).await?.ok_or_else(|| AgentError::RecordInvalid {
        reason: format!("{turn_id}:final names an event that is missing"),
    })?;
    let text = String::from_utf8_lossy(&ev.data).into_owned();
    let cancelled = engine.step_seq(scope, &cancel_step(turn_id)).await?.is_some();
    Ok(LoopOutcome { text, cancelled })
}

/// Return the recorded outcome of a finished turn. A final is only ever
/// committed behind the closing fence once every recorded call has a result,
/// so `finish_torn_batches` normally finds nothing; it still settles a log
/// written before the fence existed.
async fn settle_finished<A, T>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    tools: &T,
    turn_id: &str,
    seq: u64,
) -> Result<LoopOutcome>
where
    A: ApeOps,
    T: ToolRegistry,
{
    finish_torn_batches(engine, scope, tools, turn_id).await?;
    recorded_outcome(engine, scope, turn_id, seq).await
}

/// The calls of the batch recorded at `seq`, with invocation bytes taken from
/// the RECORD (never a driver's own bytes), so two drivers can never invoke a
/// call with different arguments.
async fn recorded_calls<A: ApeOps>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    batch_step: &str,
    seq: u64,
    used_ids: &mut HashSet<String>,
) -> Result<Vec<(ToolUse, Vec<u8>)>> {
    let recorded = engine
        .event_at(scope, seq)
        .await?
        .and_then(|ev| ToolCallBatch::from_bytes(&ev.data))
        .ok_or_else(|| AgentError::RecordInvalid {
            reason: format!("{batch_step} does not hold a tool-call batch"),
        })?;
    Ok(recorded
        .calls
        .into_iter()
        .map(|call| {
            used_ids.insert(call.id.clone());
            let args = call.args.clone().into_bytes();
            (call, args)
        })
        .collect())
}

/// What the guarded loop produced: the answer text, and whether the guard
/// (rather than the model) ended the turn.
pub struct LoopOutcome {
    pub text: String,
    pub cancelled: bool,
}

/// Run the agent tool loop for one already-admitted turn `turn_id`. Replay is
/// inert independent of model behavior: an already-finalized turn returns its
/// recorded answer and appends nothing; each tool step self-recognizes an
/// already-applied `{turn_id}:batch:{n}:call:{i}`.
pub async fn run_tool_loop<A, M, T>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    model: &M,
    tools: &T,
    turn_id: &str,
    max_steps: usize,
) -> Result<String>
where
    A: ApeOps,
    M: ModelClient,
    T: ToolRegistry,
{
    run_tool_loop_guarded(
        engine,
        scope,
        model,
        tools,
        turn_id,
        max_steps,
        &Uninterrupted,
    )
    .await
    .map(|outcome| outcome.text)
}

/// [`run_tool_loop`] with a [`TurnGuard`] checked at every step boundary.
/// An interrupted turn is FINALIZED durably (answer [`CANCELLED_ANSWER`]), so
/// a redelivered or replayed turn stays inert instead of resuming.
pub async fn run_tool_loop_guarded<A, M, T, G>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    model: &M,
    tools: &T,
    turn_id: &str,
    max_steps: usize,
    guard: &G,
) -> Result<LoopOutcome>
where
    A: ApeOps,
    M: ModelClient,
    T: ToolRegistry,
    G: TurnGuard,
{
    let final_step = format!("{turn_id}:final");
    let cancel_marker = cancel_step(turn_id);
    // Ids already used by a tool call in this turn, so a repeat is renamed
    // rather than handed to the provider twice. Rebuilt on replay from the
    // batches the log holds, in the order they were recorded.
    let mut used_ids: HashSet<String> = HashSet::new();
    let id_prefix = call_id_prefix(turn_id);
    // An interruption, once observed, holds for the rest of this invocation: a
    // guard need not repeat a one-shot cancel, so a cancel that lost its fence
    // to a recorded batch is remembered and applied at the next boundary
    // instead of being consumed and forgotten.
    let mut interrupted = false;
    for batch_no in 0..max_steps {
        // Rechecked at EVERY boundary, not only on entry: another driver may
        // have finished the turn while this one ran a batch, and its recorded
        // answer is the outcome (never a model call on a finished turn).
        if let Some(seq) = engine.step_seq(scope, &final_step).await? {
            return settle_finished(engine, scope, tools, turn_id, seq).await;
        }
        // Keyed by the batch's POSITION in the turn, never by an id the model
        // chose: a model that repeats an id (or a provider that omits one, which
        // parses as "") would otherwise make a later call collide with an
        // earlier step and be skipped with no result recorded.
        let batch_step = format!("{turn_id}:batch:{batch_no}");
        let mut recorded_here = engine.step_seq(scope, &batch_step).await?;
        // A batch already recorded at this position is finished first (its calls
        // need results whatever happens next); the guard is consulted at the
        // first position with nothing recorded.
        if recorded_here.is_none() && (interrupted || guard.interrupt().await.is_some()) {
            interrupted = true;
            // The closing fence: finalize UNLESS another driver recorded a batch
            // here meanwhile. Every earlier batch already has all its results
            // (this driver ran them), so a final is never committed ahead of an
            // unfinished call, and the cancel marker commits with it.
            match engine
                .finalize_turn_unless(
                    scope,
                    &final_step,
                    CANCELLED_ANSWER.as_bytes(),
                    CANCELLED_ANSWER.as_bytes(),
                    &batch_step,
                    Some(&cancel_marker),
                )
                .await?
            {
                // Applied: this cancel is the outcome. Existed: another driver
                // finalized first; its answer (and marker, if any) is.
                Advanced::Applied(seq) | Advanced::Existed(seq) => {
                    return settle_finished(engine, scope, tools, turn_id, seq).await;
                }
                // A batch landed here first: finish it, then stop at the next
                // boundary.
                Advanced::Blocked(seq) => recorded_here = Some(seq),
            }
        }
        let calls: Vec<(ToolUse, Vec<u8>)> = match recorded_here {
            // A replay: this batch was already decided and recorded. Finish it
            // from the log instead of asking the model again, so a crash between
            // the record and the last result cannot strand a call without one.
            Some(seq) => recorded_calls(engine, scope, &batch_step, seq, &mut used_ids).await?,
            None => {
                let snap = engine.get(scope).await?.ok_or(AgentError::SessionNotOpen)?;
                let history = engine.read_events(scope, 1, snap.seq).await?;
                let step = match model.step(&history).await {
                    Ok(step) => step,
                    Err(err) => {
                        // A turn another driver finished while this model call
                        // ran has an answer; the model error is not the outcome.
                        if let Some(seq) = engine.step_seq(scope, &final_step).await? {
                            return settle_finished(engine, scope, tools, turn_id, seq).await;
                        }
                        return Err(err);
                    }
                };
                match step {
                    ModelStep::Final { text } => {
                        match engine
                            .finalize_turn_unless(
                                scope,
                                &final_step,
                                text.as_bytes(),
                                text.as_bytes(),
                                &batch_step,
                                None,
                            )
                            .await?
                        {
                            // Another driver of the same turn may have finalized
                            // it first (the step id is fixed, so this returns ITS
                            // seq): the recorded answer is THE answer.
                            Advanced::Applied(seq) | Advanced::Existed(seq) => {
                                return settle_finished(engine, scope, tools, turn_id, seq).await;
                            }
                            // Another driver recorded a batch at this position
                            // while this model thought: the log's decision wins,
                            // the turn is not over, and that batch runs now.
                            Advanced::Blocked(seq) => {
                                recorded_calls(engine, scope, &batch_step, seq, &mut used_ids).await?
                            }
                        }
                    }
                    ModelStep::ToolCalls { text, calls } => {
                        // The model's ids survive only as transcript metadata,
                        // and only when they are usable there; the fold pairs
                        // results with calls by position, so a renamed id stays
                        // consistent with its result.
                        let calls: Vec<(ToolUse, Vec<u8>)> = calls
                            .into_iter()
                            .enumerate()
                            .map(|(i, c)| {
                                // Arguments are recorded as text and every
                                // invocation reloads the record, so bytes that
                                // are not UTF-8 would be silently altered before
                                // even the first call: refused instead.
                                let args_text = String::from_utf8(c.args.clone()).map_err(|_| {
                                    AgentError::RecordInvalid {
                                        reason: format!("the arguments of tool call {i} are not UTF-8"),
                                    }
                                })?;
                                let id = if usable_call_id(&c.id) && !used_ids.contains(&c.id) {
                                    c.id
                                } else {
                                    // Deterministic, and never an id already in
                                    // use: a model may have SUPPLIED the name the
                                    // plain replacement would take.
                                    let base = format!("{id_prefix}_{batch_no}_{i}");
                                    let mut candidate = base.clone();
                                    let mut n = 0u32;
                                    while used_ids.contains(&candidate) {
                                        n += 1;
                                        candidate = format!("{base}_{n}");
                                    }
                                    candidate
                                };
                                used_ids.insert(id.clone());
                                let call = ToolUse {
                                    id,
                                    name: c.tool,
                                    args: args_text,
                                };
                                Ok((call, c.args))
                            })
                            .collect::<Result<Vec<_>>>()?;
                        // Record the batch as ONE assistant tool-use REQUEST
                        // before invoking, so the fold replays a single
                        // assistant(tool_use×N) message.
                        let batch = ToolCallBatch {
                            text,
                            calls: calls.iter().map(|(call, _)| call.clone()).collect(),
                            step: batch_step.clone(),
                        };
                        let seq = match engine
                            .record_event_unless(
                                scope,
                                &batch_step,
                                TOOL_CALL_EVENT_KIND,
                                &batch.to_bytes(),
                                &final_step,
                            )
                            .await?
                        {
                            Advanced::Applied(seq) | Advanced::Existed(seq) => seq,
                            // Another driver recorded the final answer while this
                            // one waited for its model: the turn is over, and a
                            // batch appended now could never be finished.
                            Advanced::Blocked(final_seq) => {
                                return settle_finished(engine, scope, tools, turn_id, final_seq).await;
                            }
                        };
                        // The step id is positional, so another driver of the same
                        // turn may have recorded this batch first (this call then
                        // returns ITS seq). The log's batch is THE batch: every
                        // driver runs the same calls with the same arguments, so a
                        // write two drivers both make is the same write (the relay
                        // derives its identity from the arguments), never two.
                        recorded_calls(engine, scope, &batch_step, seq, &mut used_ids).await?
                    }
                }
            }
        };
        // Invoke every call in order, each under its own positional step id,
        // recording all results before the next model step. Each result names
        // the batch and position it answers, so the fold pairs it with its
        // call even when the log interleaves other events or a result is
        // missing.
        for (i, (call, args)) in calls.iter().enumerate() {
            run_recorded_call(engine, scope, tools, &batch_step, i, call, args, true).await?;
        }
    }
    // Out of steps: still not an error if another driver finished the turn.
    if let Some(seq) = engine.step_seq(scope, &final_step).await? {
        return settle_finished(engine, scope, tools, turn_id, seq).await;
    }
    // A cancelled replay whose batches fill every position never reached a
    // guard poll (a recorded batch is finished first). Consult the guard once
    // more now that every batch has its results, and commit the cancel behind
    // the next position's fence, so a cancelled turn is recorded as one
    // rather than ending in a step-budget error.
    if interrupted || guard.interrupt().await.is_some() {
        let next = format!("{turn_id}:batch:{max_steps}");
        match engine
            .finalize_turn_unless(
                scope,
                &final_step,
                CANCELLED_ANSWER.as_bytes(),
                CANCELLED_ANSWER.as_bytes(),
                &next,
                Some(&cancel_marker),
            )
            .await?
        {
            Advanced::Applied(seq) | Advanced::Existed(seq) => {
                return settle_finished(engine, scope, tools, turn_id, seq).await;
            }
            Advanced::Blocked(_) => {}
        }
    }
    Err(AgentError::StepBudget { max: max_steps })
}

/// Run every recorded call of this turn that has no recorded result, under its
/// recorded batch and position, recording each result. Walks the batches in
/// step-id order until the first that does not exist, and returns that
/// position (the next batch the turn would record).
pub(crate) async fn finish_torn_batches<A, T>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    tools: &T,
    turn_id: &str,
) -> Result<usize>
where
    A: ApeOps,
    T: ToolRegistry,
{
    for batch_no in 0usize.. {
        let batch_step = format!("{turn_id}:batch:{batch_no}");
        let Some(seq) = engine.step_seq(scope, &batch_step).await? else {
            return Ok(batch_no);
        };
        let batch = engine
            .event_at(scope, seq)
            .await?
            .and_then(|ev| ToolCallBatch::from_bytes(&ev.data))
            .ok_or_else(|| AgentError::RecordInvalid {
                reason: format!("{batch_step} does not hold a tool-call batch"),
            })?;
        for (i, call) in batch.calls.iter().enumerate() {
            run_recorded_call(engine, scope, tools, &batch_step, i, call, call.args.as_bytes(), false).await?;
        }
    }
    unreachable!("the batch walk ends at the first missing position")
}

/// Finalize `turn_id` with `text` (a turn the caller ends itself, such as an
/// error answer) only after every recorded call has its result, behind the
/// closing fence on the next batch position. `Ok(Some(answer))` is the
/// recorded answer (another driver's, if it finalized first); `Ok(None)` means
/// another driver recorded a batch meanwhile, so the turn is not over and the
/// caller leaves it for replay.
pub(crate) async fn finalize_settled<A, T>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    tools: &T,
    turn_id: &str,
    text: &str,
) -> Result<Option<String>>
where
    A: ApeOps,
    T: ToolRegistry,
{
    let next = finish_torn_batches(engine, scope, tools, turn_id).await?;
    let final_step = format!("{turn_id}:final");
    let fence = format!("{turn_id}:batch:{next}");
    match engine
        .finalize_turn_unless(scope, &final_step, text.as_bytes(), text.as_bytes(), &fence, None)
        .await?
    {
        Advanced::Applied(seq) | Advanced::Existed(seq) => {
            Ok(Some(recorded_outcome(engine, scope, turn_id, seq).await?.text))
        }
        Advanced::Blocked(_) => Ok(None),
    }
}

/// Whether a model-supplied call id can be kept in the transcript as given:
/// non-empty, at most 64 bytes, and only `[A-Za-z0-9_-]` (the shape providers
/// accept back). Anything else is renamed.
fn usable_call_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 64
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
}

/// The prefix of a renamed call id: derived from the turn, so ids renamed in
/// different turns of one session cannot collide with each other.
fn call_id_prefix(turn_id: &str) -> String {
    use std::fmt::Write as _;
    let digest = ferrite_app::crypto::sha256(turn_id.as_bytes());
    let mut out = String::from("call_");
    for b in digest.iter().take(6) {
        let _ = write!(out, "{b:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::session::testkit::MemApe;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    struct ScriptedModel {
        script: Vec<ModelStep>,
        cursor: AtomicUsize,
        stepped: AtomicBool,
    }

    impl ScriptedModel {
        fn new(script: Vec<ModelStep>) -> Self {
            Self {
                script,
                cursor: AtomicUsize::new(0),
                stepped: AtomicBool::new(false),
            }
        }
    }

    impl ModelClient for ScriptedModel {
        fn step(&self, _history: &[EventDoc]) -> impl Future<Output = Result<ModelStep>> + Send {
            self.stepped.store(true, Ordering::SeqCst);
            let i = self.cursor.fetch_add(1, Ordering::SeqCst);
            let idx = i.min(self.script.len() - 1);
            let step = self.script[idx].clone();
            async move { Ok(step) }
        }
    }

    #[derive(Default)]
    struct CountingTools {
        invocations: AtomicUsize,
    }

    impl ToolRegistry for CountingTools {
        fn invoke(
            &self,
            _tool: &str,
            _args: &[u8],
        ) -> impl Future<Output = Result<Vec<u8>>> + Send {
            self.invocations.fetch_add(1, Ordering::SeqCst);
            async move { Ok(b"ok".to_vec()) }
        }
    }

    fn scope() -> Scope {
        Scope::new("t", "a", "th").unwrap()
    }

    async fn admitted_engine(turn_id: &str) -> (SessionEngine<MemApe>, Scope) {
        let mut eng = SessionEngine::new(MemApe::default());
        let sc = scope();
        eng.open(&sc).await.unwrap();
        eng.admit_message(&sc, turn_id, b"hi").await.unwrap();
        (eng, sc)
    }

    #[tokio::test]
    async fn final_step_finalizes_and_returns() {
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = CountingTools::default();
        let model = ScriptedModel::new(vec![ModelStep::Final {
            text: "done".into(),
        }]);
        let out = run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8)
            .await
            .unwrap();
        assert_eq!(out, "done");

        let replay = ScriptedModel::new(vec![ModelStep::Final {
            text: "DIFFERENT".into(),
        }]);
        let out2 = run_tool_loop(&mut eng, &sc, &replay, &tools, "t1", 8)
            .await
            .unwrap();
        assert_eq!(out2, "done", "replay returns recorded answer, not new text");
        assert!(
            !replay.stepped.load(Ordering::SeqCst),
            "finalized turn must not step the model"
        );
    }

    #[tokio::test]
    async fn every_tool_call_in_a_batch_runs_once_and_replay_is_inert() {
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = CountingTools::default();
        let model = ScriptedModel::new(vec![
            ModelStep::ToolCalls {
                text: "working".into(),
                calls: vec![
                    StepToolCall {
                        id: "c1".into(),
                        tool: "a".into(),
                        args: b"1".to_vec(),
                    },
                    StepToolCall {
                        id: "c2".into(),
                        tool: "b".into(),
                        args: b"2".to_vec(),
                    },
                    StepToolCall {
                        id: "c3".into(),
                        tool: "c".into(),
                        args: b"3".to_vec(),
                    },
                ],
            },
            ModelStep::Final {
                text: "done".into(),
            },
        ]);
        let out = run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8)
            .await
            .unwrap();
        assert_eq!(out, "done");
        assert_eq!(
            tools.invocations.load(Ordering::SeqCst),
            3,
            "all three calls in the step must run"
        );
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        assert_eq!(
            events.iter().filter(|e| e.kind == "tool_result").count(),
            3,
            "one result recorded per call"
        );
        assert_eq!(
            events
                .iter()
                .filter(|e| e.kind == TOOL_CALL_EVENT_KIND)
                .count(),
            1,
            "the batch is one assistant tool-use request event"
        );

        // Replay the same turn: no call is re-invoked.
        let before = tools.invocations.load(Ordering::SeqCst);
        let replay = ScriptedModel::new(vec![
            ModelStep::ToolCalls {
                text: "working".into(),
                calls: vec![
                    StepToolCall {
                        id: "c1".into(),
                        tool: "a".into(),
                        args: b"1".to_vec(),
                    },
                    StepToolCall {
                        id: "c2".into(),
                        tool: "b".into(),
                        args: b"2".to_vec(),
                    },
                    StepToolCall {
                        id: "c3".into(),
                        tool: "c".into(),
                        args: b"3".to_vec(),
                    },
                ],
            },
            ModelStep::Final {
                text: "done".into(),
            },
        ]);
        let out2 = run_tool_loop(&mut eng, &sc, &replay, &tools, "t1", 8)
            .await
            .unwrap();
        assert_eq!(out2, "done");
        assert_eq!(
            tools.invocations.load(Ordering::SeqCst),
            before,
            "replay must not re-invoke any call"
        );
    }

    #[tokio::test]
    async fn replayed_tool_step_not_reinvoked() {
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = CountingTools::default();
        eng.record_event(&sc, "t1:batch:0:call:0", "tool_result", b"prev")
            .await
            .unwrap();
        let model = ScriptedModel::new(vec![
            ModelStep::ToolCalls {
                text: String::new(),
                calls: vec![StepToolCall {
                    id: "c1".into(),
                    tool: "echo".into(),
                    args: b"x".to_vec(),
                }],
            },
            ModelStep::Final {
                text: "done".into(),
            },
        ]);
        let out = run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8)
            .await
            .unwrap();
        assert_eq!(out, "done");
        assert_eq!(
            tools.invocations.load(Ordering::SeqCst),
            0,
            "already-applied tool step not re-invoked"
        );
    }

    /// Interrupts once `after` guard polls have happened.
    struct CancelAfter {
        polls: AtomicUsize,
        after: usize,
    }

    impl TurnGuard for CancelAfter {
        fn interrupt(&self) -> impl Future<Output = Option<String>> + Send {
            let n = self.polls.fetch_add(1, Ordering::SeqCst);
            let hit = n >= self.after;
            async move { hit.then(|| "cancelled".to_string()) }
        }
    }

    #[tokio::test]
    async fn a_cancelled_turn_finalizes_durably_and_stops_stepping() {
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = CountingTools::default();
        // The model would loop forever; the guard stops it at the second boundary.
        let model = ScriptedModel::new(vec![ModelStep::ToolCalls {
            text: String::new(),
            calls: vec![StepToolCall {
                id: "c1".into(),
                tool: "echo".into(),
                args: b"x".to_vec(),
            }],
        }]);
        let guard = CancelAfter {
            polls: AtomicUsize::new(0),
            after: 1,
        };
        let out = run_tool_loop_guarded(&mut eng, &sc, &model, &tools, "t1", 8, &guard)
            .await
            .unwrap();
        assert!(out.cancelled);
        assert_eq!(out.text, CANCELLED_ANSWER);
        assert_eq!(
            tools.invocations.load(Ordering::SeqCst),
            1,
            "the step in flight before the boundary still ran and was recorded"
        );

        // Replay: the finalized cancel is durable and inert — the model never steps.
        let replay = ScriptedModel::new(vec![ModelStep::Final {
            text: "DIFFERENT".into(),
        }]);
        let out2 = run_tool_loop_guarded(&mut eng, &sc, &replay, &tools, "t1", 8, &Uninterrupted)
            .await
            .unwrap();
        assert_eq!(out2.text, CANCELLED_ANSWER);
        assert!(out2.cancelled, "a replayed cancel is still reported as one");
        assert!(!replay.stepped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn a_cancel_before_the_first_step_never_calls_the_model() {
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = CountingTools::default();
        let model = ScriptedModel::new(vec![ModelStep::Final { text: "hi".into() }]);
        let guard = CancelAfter {
            polls: AtomicUsize::new(0),
            after: 0,
        };
        let out = run_tool_loop_guarded(&mut eng, &sc, &model, &tools, "t1", 8, &guard)
            .await
            .unwrap();
        assert!(out.cancelled);
        assert!(!model.stepped.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn step_budget_fails_closed() {
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = CountingTools::default();
        struct Runaway {
            n: AtomicUsize,
        }
        impl ModelClient for Runaway {
            fn step(
                &self,
                _history: &[EventDoc],
            ) -> impl Future<Output = Result<ModelStep>> + Send {
                let i = self.n.fetch_add(1, Ordering::SeqCst);
                async move {
                    Ok(ModelStep::ToolCalls {
                        text: String::new(),
                        calls: vec![StepToolCall {
                            id: format!("c{i}"),
                            tool: "echo".into(),
                            args: b"x".to_vec(),
                        }],
                    })
                }
            }
        }
        let runaway = Runaway {
            n: AtomicUsize::new(0),
        };
        let err = run_tool_loop(&mut eng, &sc, &runaway, &tools, "t1", 3)
            .await
            .unwrap_err();
        assert!(
            matches!(err, AgentError::StepBudget { max: 3 }),
            "got {err:?}"
        );

        // Review finding: a cancelled replay of a turn whose batches fill
        // every position never polled the guard and ended in StepBudget
        // instead of recording the cancellation.
        let replay = ScriptedModel::new(vec![ModelStep::Final { text: "late".into() }]);
        let guard = CancelAfter {
            polls: AtomicUsize::new(0),
            after: 0,
        };
        let out = run_tool_loop_guarded(&mut eng, &sc, &replay, &tools, "t1", 3, &guard)
            .await
            .expect("a cancel is an outcome, not a budget error");
        assert!(out.cancelled);
        assert_eq!(out.text, CANCELLED_ANSWER);
        assert!(!replay.stepped.load(Ordering::SeqCst), "the model never stepped");
        // And it is durable: an uninterrupted replay reports the same cancel.
        let again = run_tool_loop_guarded(&mut eng, &sc, &replay, &tools, "t1", 3, &Uninterrupted)
            .await
            .unwrap();
        assert!(again.cancelled);
    }

    /// Records every call it is asked to make, and can be told to fail once.
    #[derive(Default)]
    struct RecordingTools {
        calls: std::sync::Mutex<Vec<String>>,
        fail_on: std::sync::Mutex<Option<String>>,
        /// Model a side-effecting tool that must not be repeated blindly.
        unsafe_tools: bool,
        /// Calls settled from the log rather than invoked.
        noted: std::sync::Mutex<Vec<String>>,
    }

    impl ToolRegistry for RecordingTools {
        fn replay_safe(&self, _tool: &str) -> bool {
            !self.unsafe_tools
        }

        fn note_recorded(&self, tool: &str) {
            self.noted.lock().unwrap().push(tool.to_string());
        }

        fn invoke(&self, tool: &str, _args: &[u8]) -> impl Future<Output = Result<Vec<u8>>> + Send {
            let fail = {
                let mut fail_on = self.fail_on.lock().unwrap();
                if fail_on.as_deref() == Some(tool) {
                    fail_on.take();
                    true
                } else {
                    false
                }
            };
            if !fail {
                self.calls.lock().unwrap().push(tool.to_string());
            }
            async move {
                if fail {
                    Err(AgentError::Plane {
                        message: "tool plane down".into(),
                    })
                } else {
                    Ok(b"ok".to_vec())
                }
            }
        }
    }

    fn call(id: &str, tool: &str) -> StepToolCall {
        StepToolCall {
            id: id.into(),
            tool: tool.into(),
            args: b"{}".to_vec(),
        }
    }

    fn recorded_batches(events: &[EventDoc]) -> Vec<ToolCallBatch> {
        events
            .iter()
            .filter(|e| e.kind == TOOL_CALL_EVENT_KIND)
            .map(|e| ToolCallBatch::from_bytes(&e.data).unwrap())
            .collect()
    }

    #[tokio::test]
    async fn repeated_or_missing_call_ids_never_skip_a_call() {
        // Model-chosen ids used to BE the idempotency keys, so a repeated id
        // (the JSON fallback prompt literally shows "stable-id") or a provider
        // that omits ids (parsed as "") made the second call's step already
        // exist: it was skipped with no tool_result, stranding a tool_use.
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = RecordingTools::default();
        let model = ScriptedModel::new(vec![
            ModelStep::ToolCalls {
                text: String::new(),
                calls: vec![call("stable-id", "a"), call("stable-id", "b")],
            },
            ModelStep::ToolCalls {
                text: String::new(),
                calls: vec![call("stable-id", "c"), call("", "d"), call("", "e")],
            },
            ModelStep::Final {
                text: "done".into(),
            },
        ]);
        let out = run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8)
            .await
            .unwrap();
        assert_eq!(out, "done");
        assert_eq!(
            *tools.calls.lock().unwrap(),
            vec!["a", "b", "c", "d", "e"],
            "every call runs exactly once, in order"
        );

        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        assert_eq!(
            events.iter().filter(|e| e.kind == "tool_result").count(),
            5,
            "every tool_use keeps a matching result"
        );
        let batches = recorded_batches(&events);
        assert_eq!(batches.len(), 2);
        let ids: Vec<&str> = batches
            .iter()
            .flat_map(|b| b.calls.iter().map(|c| c.id.as_str()))
            .collect();
        assert_eq!(ids[0], "stable-id", "a usable first id is kept");
        let unique: HashSet<&str> = ids.iter().copied().collect();
        assert_eq!(unique.len(), ids.len(), "ids are unique in the turn: {ids:?}");
        assert!(ids.iter().all(|id| usable_call_id(id)), "{ids:?}");
    }

    #[tokio::test]
    async fn a_torn_batch_is_finished_from_the_log_on_replay() {
        // A batch recorded before all of its results were: the retry must run
        // the missing call under the recorded batch, not ask the model again
        // and leave the first batch's call without a result.
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = RecordingTools::default();
        *tools.fail_on.lock().unwrap() = Some("b".into());
        let model = ScriptedModel::new(vec![
            ModelStep::ToolCalls {
                text: String::new(),
                calls: vec![call("c1", "a"), call("c2", "b")],
            },
            ModelStep::Final {
                text: "done".into(),
            },
        ]);
        assert!(run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8)
            .await
            .is_err());
        assert_eq!(*tools.calls.lock().unwrap(), vec!["a"]);

        // The replay's model would decide something else entirely; the recorded
        // batch wins, and only the missing call runs.
        let replay = ScriptedModel::new(vec![ModelStep::Final {
            text: "done".into(),
        }]);
        let out = run_tool_loop(&mut eng, &sc, &replay, &tools, "t1", 8)
            .await
            .unwrap();
        assert_eq!(out, "done");
        assert_eq!(*tools.calls.lock().unwrap(), vec!["a", "b"]);
        // The call settled from the log is reported, so a per-turn bound (the
        // delegation fan-out cap) counts it after a driver restart.
        assert_eq!(*tools.noted.lock().unwrap(), vec!["a"]);
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        assert_eq!(recorded_batches(&events).len(), 1);
        assert_eq!(events.iter().filter(|e| e.kind == "tool_result").count(), 2);
    }

    /// One store, two engines: two drivers of the same turn.
    #[derive(Clone, Default)]
    struct SharedApe(std::sync::Arc<tokio::sync::Mutex<MemApe>>);

    impl ApeOps for SharedApe {
        async fn get(&mut self, key: &[u8]) -> std::io::Result<Option<Vec<u8>>> {
            self.0.lock().await.get(key).await
        }
        async fn multi_get(&mut self, keys: &[&[u8]]) -> std::io::Result<Vec<Option<Vec<u8>>>> {
            self.0.lock().await.multi_get(keys).await
        }
        async fn commit_n_if(
            &mut self,
            guard: &[u8],
            expected: Option<&[u8]>,
            entries: &[(&[u8], &[u8])],
        ) -> std::io::Result<std::result::Result<(), Option<Vec<u8>>>> {
            self.0.lock().await.commit_n_if(guard, expected, entries).await
        }
    }

    /// A model whose FIRST step races another driver: while it "thinks", the
    /// other driver records batch 0 with different arguments.
    struct RacedModel {
        other: SharedApe,
        raced: AtomicBool,
    }

    impl ModelClient for RacedModel {
        fn step(&self, _history: &[EventDoc]) -> impl Future<Output = Result<ModelStep>> + Send {
            let first = !self.raced.swap(true, Ordering::SeqCst);
            let other = self.other.clone();
            async move {
                if !first {
                    return Ok(ModelStep::Final { text: "mine".into() });
                }
                let mut theirs = SessionEngine::new(other);
                let batch = ToolCallBatch {
                    text: String::new(),
                    calls: vec![ToolUse { id: "theirs".into(), name: "post".into(), args: r#"{"text":"A"}"#.into() }],
                    step: "t1:batch:0".into(),
                };
                theirs
                    .record_event(&scope(), "t1:batch:0", TOOL_CALL_EVENT_KIND, &batch.to_bytes())
                    .await?;
                Ok(ModelStep::ToolCalls {
                    text: String::new(),
                    calls: vec![StepToolCall { id: "mine".into(), tool: "post".into(), args: br#"{"text":"B"}"#.to_vec() }],
                })
            }
        }
    }

    #[derive(Default)]
    struct ArgsTools {
        calls: std::sync::Mutex<Vec<String>>,
    }

    impl ToolRegistry for ArgsTools {
        fn invoke(&self, tool: &str, args: &[u8]) -> impl Future<Output = Result<Vec<u8>>> + Send {
            self.calls.lock().unwrap().push(format!("{tool} {}", String::from_utf8_lossy(args)));
            async move { Ok(b"ok".to_vec()) }
        }
    }

    #[tokio::test]
    async fn a_driver_that_loses_the_batch_race_runs_the_recorded_calls() {
        // Two drivers of one turn (a reclaim while the first still runs). The
        // other recorded batch 0 first; this driver's model chose different
        // arguments. Running its OWN calls made a write the relay sees as
        // different from the other driver's, so both could post.
        let shared = SharedApe::default();
        let mut eng = SessionEngine::new(shared.clone());
        let sc = scope();
        eng.open(&sc).await.unwrap();
        eng.admit_message(&sc, "t1", b"hi").await.unwrap();
        let tools = ArgsTools::default();
        let model = RacedModel { other: shared.clone(), raced: AtomicBool::new(false) };
        let out = run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8).await.unwrap();
        assert_eq!(out, "mine");
        assert_eq!(
            *tools.calls.lock().unwrap(),
            vec![r#"post {"text":"A"}"#.to_string()],
            "the recorded batch's call runs, not this driver's own"
        );
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        assert_eq!(recorded_batches(&events).len(), 1);
    }

    #[tokio::test]
    async fn a_batch_is_never_appended_after_another_driver_finalized() {
        // A stale driver still waiting for its model when another driver
        // finalized the turn: appending its batch after the final left calls no
        // replay would run once the turn was marked answered.
        let shared = SharedApe::default();
        let mut eng = SessionEngine::new(shared.clone());
        let sc = scope();
        eng.open(&sc).await.unwrap();
        eng.admit_message(&sc, "t1", b"hi").await.unwrap();
        struct FinalizedMeanwhile(SharedApe);
        impl ModelClient for FinalizedMeanwhile {
            fn step(&self, _h: &[EventDoc]) -> impl Future<Output = Result<ModelStep>> + Send {
                let other = self.0.clone();
                async move {
                    SessionEngine::new(other).finalize_turn(&scope(), "t1:final", b"theirs", b"theirs").await?;
                    Ok(ModelStep::ToolCalls { text: String::new(), calls: vec![call("c1", "post")] })
                }
            }
        }
        let tools = RecordingTools::default();
        let out = run_tool_loop(&mut eng, &sc, &FinalizedMeanwhile(shared), &tools, "t1", 8).await.unwrap();
        assert_eq!(out, "theirs");
        assert!(tools.calls.lock().unwrap().is_empty(), "no call runs after the final answer");
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        assert!(recorded_batches(&events).is_empty(), "and no batch is appended");
    }

    #[tokio::test]
    async fn a_final_answer_replay_first_finishes_a_torn_batch() {
        // Two drivers decided differently at one position: one recorded a batch
        // (and crashed before its call's result), the other finalized. Every
        // replay returned the final early, so the call never ran and the turn's
        // unknown write could never be resolved.
        let (mut eng, sc) = admitted_engine("t1").await;
        let batch = ToolCallBatch {
            text: String::new(),
            calls: vec![ToolUse { id: "c1".into(), name: "post".into(), args: "{}".into() }],
            step: "t1:batch:0".into(),
        };
        eng.record_event(&sc, "t1:batch:0", TOOL_CALL_EVENT_KIND, &batch.to_bytes()).await.unwrap();
        eng.finalize_turn(&sc, "t1:final", b"theirs", b"theirs").await.unwrap();
        let tools = RecordingTools::default();
        let model = ScriptedModel::new(vec![ModelStep::Final { text: "mine".into() }]);
        let out = run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8).await.unwrap();
        assert_eq!(out, "theirs");
        assert!(!model.stepped.load(Ordering::SeqCst), "no model call on a finished turn");
        assert_eq!(*tools.calls.lock().unwrap(), vec!["post"], "the torn call runs once");
        // Its result is recorded, so the next replay runs nothing.
        run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8).await.unwrap();
        assert_eq!(tools.calls.lock().unwrap().len(), 1);
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        assert_eq!(events.iter().filter(|e| e.kind == "tool_result").count(), 1);
    }

    #[tokio::test]
    async fn a_supplied_id_never_collides_with_a_generated_replacement() {
        // Call 0 supplies exactly the name call 1's replacement would take, and
        // call 1 supplies no id: the two must still be distinct.
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = RecordingTools::default();
        let taken = format!("{}_0_1", call_id_prefix("t1"));
        let model = ScriptedModel::new(vec![
            ModelStep::ToolCalls { text: String::new(), calls: vec![call(&taken, "a"), call("", "b")] },
            ModelStep::Final { text: "done".into() },
        ]);
        run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8).await.unwrap();
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        let ids: Vec<String> = recorded_batches(&events)[0].calls.iter().map(|c| c.id.clone()).collect();
        assert_eq!(ids[0], taken);
        assert_ne!(ids[0], ids[1], "{ids:?}");
        assert!(ids.iter().all(|id| usable_call_id(id)), "{ids:?}");
    }

    #[tokio::test]
    async fn a_cancelled_retry_finishes_a_recorded_batch_first() {
        // A batch was recorded and its call never got a result; the retry is
        // cancelled at its first step boundary. The recorded call still runs
        // and gets its result before the turn is finalized as cancelled.
        let (mut eng, sc) = admitted_engine("t1").await;
        let batch = ToolCallBatch {
            text: String::new(),
            calls: vec![ToolUse { id: "c1".into(), name: "post".into(), args: "{}".into() }],
            step: "t1:batch:0".into(),
        };
        eng.record_event(&sc, "t1:batch:0", TOOL_CALL_EVENT_KIND, &batch.to_bytes()).await.unwrap();
        let tools = RecordingTools::default();
        let model = ScriptedModel::new(vec![ModelStep::Final { text: "never".into() }]);
        let out = run_tool_loop_guarded(
            &mut eng,
            &sc,
            &model,
            &tools,
            "t1",
            8,
            &CancelAfter { polls: AtomicUsize::new(0), after: 0 },
        )
        .await
        .unwrap();
        assert!(out.cancelled);
        assert_eq!(*tools.calls.lock().unwrap(), vec!["post"], "the recorded call finished");
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        assert_eq!(events.iter().filter(|e| e.kind == "tool_result").count(), 1);
    }

    #[tokio::test]
    async fn an_interrupted_side_effecting_call_is_not_repeated() {
        // An edit that started (its intent is recorded) and never recorded a
        // result: the first attempt crashed mid-call, or another driver is still
        // running it. Replay must NOT apply it a second time; it answers
        // "outcome unknown" so the model checks the state instead.
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = RecordingTools { unsafe_tools: true, ..RecordingTools::default() };
        *tools.fail_on.lock().unwrap() = Some("edit".into()); // the crash: no result recorded
        let model = ScriptedModel::new(vec![
            ModelStep::ToolCalls { text: String::new(), calls: vec![call("c1", "edit")] },
            ModelStep::Final { text: "done".into() },
        ]);
        assert!(run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8).await.is_err());
        let replay = ScriptedModel::new(vec![ModelStep::Final { text: "checked".into() }]);
        let out = run_tool_loop(&mut eng, &sc, &replay, &tools, "t1", 8).await.unwrap();
        assert_eq!(out, "checked", "generation resumes after the call is settled");
        assert!(tools.calls.lock().unwrap().is_empty(), "the edit is never applied by the replay");
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        let results: Vec<String> = events
            .iter()
            .filter(|e| e.kind == "tool_result")
            .map(|e| String::from_utf8_lossy(crate::agents::conv::decode_tool_result(&e.data).1).into_owned())
            .collect();
        assert_eq!(results.len(), 1);
        assert!(results[0].contains("outcome_unknown"), "{}", results[0]);
        // The intent record never reaches the model's view of the transcript.
        let view = crate::agents::compaction::fold_session_events(&events, &[]);
        assert!(view.iter().all(|m| !m.text.contains("tool_intent")));
    }

    #[tokio::test]
    async fn a_side_effecting_call_started_by_another_driver_runs_only_there() {
        // Two drivers of one turn reach the same recorded call: only the driver
        // that creates the intent invokes it; the other records "outcome
        // unknown" (its own write would have been the second application).
        let shared = SharedApe::default();
        let mut a = SessionEngine::new(shared.clone());
        let mut b = SessionEngine::new(shared.clone());
        let sc = scope();
        a.open(&sc).await.unwrap();
        a.admit_message(&sc, "t1", b"hi").await.unwrap();
        let batch = ToolCallBatch {
            text: String::new(),
            calls: vec![ToolUse { id: "c1".into(), name: "edit".into(), args: "{}".into() }],
            step: "t1:batch:0".into(),
        };
        a.record_event(&sc, "t1:batch:0", TOOL_CALL_EVENT_KIND, &batch.to_bytes()).await.unwrap();
        // Driver A creates the intent (and is about to invoke).
        assert!(matches!(
            a.record_event_unless(&sc, "t1:batch:0:call:0:intent", TOOL_INTENT_EVENT_KIND, b"", "t1:batch:0:call:0")
                .await
                .unwrap(),
            Advanced::Applied(_)
        ));
        // Driver B finishes torn batches meanwhile.
        let tools_b = RecordingTools { unsafe_tools: true, ..RecordingTools::default() };
        finish_torn_batches(&mut b, &sc, &tools_b, "t1").await.unwrap();
        assert!(tools_b.calls.lock().unwrap().is_empty(), "B must not apply A's call");
    }

    #[tokio::test]
    async fn a_driver_that_loses_the_final_race_returns_the_recorded_answer() {
        let shared = SharedApe::default();
        let mut eng = SessionEngine::new(shared.clone());
        let sc = scope();
        eng.open(&sc).await.unwrap();
        eng.admit_message(&sc, "t1", b"hi").await.unwrap();
        // A model that, while it thinks, lets the other driver finalize first.
        struct LateFinal(SharedApe);
        impl ModelClient for LateFinal {
            fn step(&self, _h: &[EventDoc]) -> impl Future<Output = Result<ModelStep>> + Send {
                let other = self.0.clone();
                async move {
                    let mut theirs = SessionEngine::new(other);
                    theirs.finalize_turn(&scope(), "t1:final", b"theirs", b"theirs").await?;
                    Ok(ModelStep::Final { text: "mine".into() })
                }
            }
        }
        let tools = CountingTools::default();
        let out = run_tool_loop(&mut eng, &sc, &LateFinal(shared), &tools, "t1", 8).await.unwrap();
        assert_eq!(out, "theirs", "the recorded final answer is the answer");
    }

    fn seq_of(events: &[EventDoc], kind: &str) -> Vec<u64> {
        events.iter().filter(|e| e.kind == kind).map(|e| e.seq).collect()
    }

    #[tokio::test]
    async fn a_model_answer_that_reads_like_a_cancel_is_not_a_cancel() {
        // Cancellation used to be inferred from the answer text, so a model that
        // legitimately answered these words was reported as cancelled.
        let (mut eng, sc) = admitted_engine("t1").await;
        let tools = CountingTools::default();
        let model = ScriptedModel::new(vec![ModelStep::Final { text: CANCELLED_ANSWER.into() }]);
        let out = run_tool_loop_guarded(&mut eng, &sc, &model, &tools, "t1", 8, &Uninterrupted)
            .await
            .unwrap();
        assert_eq!(out.text, CANCELLED_ANSWER);
        assert!(!out.cancelled, "the guard never interrupted");
        let replay = run_tool_loop_guarded(&mut eng, &sc, &model, &tools, "t1", 8, &Uninterrupted)
            .await
            .unwrap();
        assert!(!replay.cancelled, "nor on replay");
    }

    /// Records batch 0 (one `post`) through another driver while this
    /// driver's model is thinking, then answers `Final`.
    struct BatchMeanwhile {
        other: SharedApe,
        raced: AtomicBool,
    }

    impl ModelClient for BatchMeanwhile {
        fn step(&self, _h: &[EventDoc]) -> impl Future<Output = Result<ModelStep>> + Send {
            let first = !self.raced.swap(true, Ordering::SeqCst);
            let other = self.other.clone();
            async move {
                if first {
                    let batch = ToolCallBatch {
                        text: String::new(),
                        calls: vec![ToolUse { id: "theirs".into(), name: "post".into(), args: "{}".into() }],
                        step: "t1:batch:0".into(),
                    };
                    SessionEngine::new(other)
                        .record_event(&scope(), "t1:batch:0", TOOL_CALL_EVENT_KIND, &batch.to_bytes())
                        .await?;
                    return Ok(ModelStep::Final { text: "mine".into() });
                }
                Ok(ModelStep::Final { text: "after".into() })
            }
        }
    }

    async fn shared_engine() -> (SharedApe, SessionEngine<SharedApe>, Scope) {
        let shared = SharedApe::default();
        let mut eng = SessionEngine::new(shared.clone());
        let sc = scope();
        eng.open(&sc).await.unwrap();
        eng.admit_message(&sc, "t1", b"hi").await.unwrap();
        (shared, eng, sc)
    }

    #[tokio::test]
    async fn a_final_answer_never_overtakes_a_batch_another_driver_recorded() {
        // The final used to be committed first and the other driver's call run
        // afterwards, so a finished, idle turn could still mutate state and a
        // failing call left a finalized turn with a missing result.
        let (shared, mut eng, sc) = shared_engine().await;
        let tools = RecordingTools::default();
        let model = BatchMeanwhile { other: shared, raced: AtomicBool::new(false) };
        let out = run_tool_loop(&mut eng, &sc, &model, &tools, "t1", 8).await.unwrap();
        assert_eq!(out, "after", "the recorded batch won the position; the turn went on");
        assert_eq!(*tools.calls.lock().unwrap(), vec!["post"]);
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        let results = seq_of(&events, "tool_result");
        let finals = seq_of(&events, "message_out");
        assert_eq!(finals.len(), 1);
        assert!(results.len() == 1 && results[0] < finals[0], "result {results:?} precedes final {finals:?}");
    }

    /// Interrupts on every poll; on the first, another driver records batch 0.
    struct CancelAsBatchLands {
        other: SharedApe,
        raced: AtomicBool,
    }

    impl TurnGuard for CancelAsBatchLands {
        fn interrupt(&self) -> impl Future<Output = Option<String>> + Send {
            let first = !self.raced.swap(true, Ordering::SeqCst);
            let other = self.other.clone();
            async move {
                if first {
                    let batch = ToolCallBatch {
                        text: String::new(),
                        calls: vec![ToolUse { id: "theirs".into(), name: "post".into(), args: "{}".into() }],
                        step: "t1:batch:0".into(),
                    };
                    SessionEngine::new(other)
                        .record_event(&scope(), "t1:batch:0", TOOL_CALL_EVENT_KIND, &batch.to_bytes())
                        .await
                        .unwrap();
                }
                Some("cancelled".to_string())
            }
        }
    }

    #[tokio::test]
    async fn a_cancel_never_overtakes_a_batch_another_driver_recorded() {
        let (shared, mut eng, sc) = shared_engine().await;
        let tools = RecordingTools::default();
        let model = ScriptedModel::new(vec![ModelStep::Final { text: "never".into() }]);
        let guard = CancelAsBatchLands { other: shared, raced: AtomicBool::new(false) };
        let out = run_tool_loop_guarded(&mut eng, &sc, &model, &tools, "t1", 8, &guard)
            .await
            .unwrap();
        assert!(out.cancelled);
        assert!(!model.stepped.load(Ordering::SeqCst));
        assert_eq!(*tools.calls.lock().unwrap(), vec!["post"], "the recorded call ran");
        let snap = eng.get(&sc).await.unwrap().unwrap();
        let events = eng.read_events(&sc, 1, snap.seq).await.unwrap();
        let results = seq_of(&events, "tool_result");
        let finals = seq_of(&events, "message_out");
        assert!(results.len() == 1 && finals.len() == 1 && results[0] < finals[0]);
    }

    #[tokio::test]
    async fn a_one_shot_cancel_that_lost_its_fence_is_latched_for_the_next_boundary() {
        // Interrupts ONCE only, as a one-shot guard may: the batch another
        // driver recorded wins that fence, and the cancel must still stop the
        // turn at the next boundary instead of being forgotten.
        struct OneShot {
            inner: CancelAsBatchLands,
        }
        impl TurnGuard for OneShot {
            fn interrupt(&self) -> impl Future<Output = Option<String>> + Send {
                let first = !self.inner.raced.load(Ordering::SeqCst);
                let fut = self.inner.interrupt();
                async move {
                    let answer = fut.await;
                    if first { answer } else { None }
                }
            }
        }
        let (shared, mut eng, sc) = shared_engine().await;
        let tools = RecordingTools::default();
        let model = ScriptedModel::new(vec![ModelStep::Final { text: "never".into() }]);
        let guard = OneShot { inner: CancelAsBatchLands { other: shared, raced: AtomicBool::new(false) } };
        let out = run_tool_loop_guarded(&mut eng, &sc, &model, &tools, "t1", 8, &guard)
            .await
            .unwrap();
        assert!(out.cancelled, "the observed cancel holds after the recorded batch finishes");
        assert!(!model.stepped.load(Ordering::SeqCst), "no model step after a cancel");
        assert_eq!(*tools.calls.lock().unwrap(), vec!["post"]);
    }

    #[tokio::test]
    async fn a_model_error_after_another_driver_finished_returns_its_answer() {
        // This driver's model call fails after another driver finalized: the
        // recorded answer is the outcome, not the model error.
        let (shared, mut eng, sc) = shared_engine().await;
        struct FinalThenFail(SharedApe);
        impl ModelClient for FinalThenFail {
            fn step(&self, _h: &[EventDoc]) -> impl Future<Output = Result<ModelStep>> + Send {
                let other = self.0.clone();
                async move {
                    SessionEngine::new(other).finalize_turn(&scope(), "t1:final", b"theirs", b"theirs").await?;
                    Err(AgentError::Plane { message: "model down".into() })
                }
            }
        }
        let tools = CountingTools::default();
        let out = run_tool_loop(&mut eng, &sc, &FinalThenFail(shared), &tools, "t1", 8).await.unwrap();
        assert_eq!(out, "theirs");
    }
}
