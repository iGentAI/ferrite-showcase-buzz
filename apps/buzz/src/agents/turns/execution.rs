//! Running one turn: the fence, model selection, setup failures and the recorded
//! posts a finished turn leaves.

use ferrite_app::tracing;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use ferrite_app::serde_json;
use ferrite_app::AppContext;
use crate::agents::agent_prompt::base_instructions_for;
use crate::agents::compaction::compact_if_needed;
use crate::agents::definition::load_agent_definition;
use crate::agents::definition::AgentDefinition;
use crate::agents::definition::FilteredTools;
use crate::agents::error::Scope;
use crate::agents::model_client::SteeringModelClient;
use crate::agents::session::AdmitOutcome;
use crate::agents::session::SessionEngine;
use crate::agents::tools::buzz_tool_schemas;
use crate::agents::tools::coding_tool_schemas;
use crate::agents::tools::fabric_a2a_tool_schemas;
use crate::agents::tools::mcp_tool_schemas;
use crate::agents::tools::web_tool_schemas;
use crate::agents::tools::CloudToolRegistry;
use super::{A2A_TURN_MARKER_TTL_MS, A2A_TURN_MAX_ATTEMPTS, CONFIG, FABRIC_A2A_EGRESS, FABRIC_A2A_ENDPOINT, FABRIC_A2A_ENDPOINT_KEY, FABRIC_A2A_TOKEN_SECRET, LLM_DEFAULT_MODEL, MAX_STEPS, MEMORY_STORE, SESSION, TENANT, WEB_EGRESS, WS_CONTENT, WS_INDEX, mcp_egress_bound};
use super::llm_backend::{LlmBackend, LlmTransport};
use super::a2a_lease::{MarkerState, leased_marker};

/// The outcome of one driven turn.
#[derive(Debug, Clone)]
pub(crate) struct TurnResult {
    /// The turn's final answer bytes.
    pub(crate) answer: Vec<u8>,
    /// True when the admission was a duplicate and [`DuplicatePolicy::Skip`]
    /// ended the turn before the loop ran.
    pub(crate) duplicate: bool,
    /// How many `buzz_post` writes the turn's tools made. The turn loop
    /// posts the FINAL answer into the channel exactly when this is zero — a
    /// trigger is fire-and-forget, so an answer that was neither tool-posted
    /// nor loop-posted is silence.
    pub(crate) buzz_posts: usize,
    /// True when the pipeline must neither post a fallback answer nor ack:
    /// the driver lost its lease while the loop ran, or whether the turn's
    /// own tools already posted could not be established. The delivery is
    /// left for redelivery.
    pub(crate) retry: bool,
}

impl TurnResult {
    pub(super) fn text(text: impl Into<Vec<u8>>) -> Self {
        Self {
            answer: text.into(),
            duplicate: false,
            buzz_posts: 0,
            retry: false,
        }
    }

    /// Neither post nor ack: see [`TurnResult::retry`].
    pub(super) fn unsettled(reason: impl Into<String>) -> Self {
        let mut result = Self::text(reason.into());
        result.retry = true;
        result
    }

    /// The [`DuplicatePolicy::Skip`] early return: the msg id was already
    /// admitted, so this delivery is inert and no loop ran.
    pub(super) fn duplicate() -> Self {
        let mut result = Self::text("duplicate admission: this turn already ran".to_string());
        result.duplicate = true;
        result
    }
}

/// What a turn does when `admit_message` reports the msg id as already
/// admitted. The at-least-once trigger queue uses `Skip`, so a redelivery of
/// a turn that outlived the visibility window cannot run a second concurrent
/// driver; the `delegate` tool uses `RunAnyway` so a replayed parent gets a
/// fresh child answer (the durable child log stays single via admission
/// dedup).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum DuplicatePolicy {
    /// Run the loop even when the admission is a duplicate.
    RunAnyway,
    /// Return [`TurnResult::duplicate`] without running the loop.
    Skip,
}

/// The platform's "no preference" sentinel: resolved by [`crate::llm`] (like
/// an empty model) to its operator-configured default.
pub(super) const MODEL_SENTINEL_DEFAULT: &str = "agent-default";

/// Model resolution: explicit request → definition default → platform
/// default. An agent definition only fills silence, never overrides a
/// caller's choice — and the `agent-default` sentinel IS silence.
pub(super) fn resolve_model(explicit: Option<&str>, def: Option<&AgentDefinition>) -> String {
    if explicit == Some(MODEL_SENTINEL_DEFAULT) {
        return def
            .and_then(|d| d.default_model.clone())
            .unwrap_or_else(|| MODEL_SENTINEL_DEFAULT.to_string());
    }
    explicit
        .map(str::to_string)
        .or_else(|| def.and_then(|d| d.default_model.clone()))
        .unwrap_or_else(|| LLM_DEFAULT_MODEL.to_string())
}

/// A well-formed model id, or `None`.
pub(crate) fn clean_model_id(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed.len() > 128 {
        return None;
    }
    if trimmed
        .chars()
        .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '/' | ':'))
    {
        Some(trimmed.to_string())
    } else {
        None
    }
}

/// The set of tools with a live approval grant on this thread: the log's
/// grants and revokes replayed in order ([`crate::agents::approvals::fold_grants`]),
/// so a revoked tool parks again. Best effort: an unreadable session yields no
/// grants (fail closed — a required tool then parks rather than running
/// unapproved).
pub(super) async fn granted_approvals<A: crate::agents::session::ApeOps>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
) -> std::collections::BTreeSet<String> {
    let mut out = std::collections::BTreeSet::new();
    let Ok(Some(snap)) = engine.get(scope).await else {
        return out;
    };
    if let Ok(events) = engine.read_events(scope, 1, snap.seq).await {
        out = crate::agents::approvals::fold_grants(&events);
    }
    out
}

/// Load the agent's definition and run one turn. This is the ONLY entry for a
/// turn that speaks as a registered persona (mentions, schedules, ACP): the
/// persona's operator-authored definition — tool filter, approvals, step
/// budget, instructions — always applies, and a persona without a stored
/// definition runs the default surface because it has none, never because a
/// caller skipped loading it.
#[expect(
    clippy::too_many_arguments,
    reason = "internal boundary keeps persisted turn identity fields explicit at its single call site"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "internal boundary keeps persisted turn identity fields explicit"
)]
pub(crate) async fn run_turn(
    ctx: &AppContext,
    llm: &Arc<dyn LlmTransport>,
    agent: &str,
    thread: &str,
    msg_id: &str,
    explicit_model: Option<&str>,
    message: &[u8],
    duplicates: DuplicatePolicy,
    requester: Option<&str>,
) -> TurnResult {
    let def = match load_agent_definition(ctx, CONFIG, agent, MAX_STEPS).await {
        Ok(def) => def,
        Err(e) => return definition_failure(false, &e),
    };
    run_turn_with_def(
        ctx,
        llm,
        agent,
        thread,
        msg_id,
        explicit_model,
        message,
        def,
        duplicates,
        None,
        requester,
    )
    .await
}

/// A persona whose definition cannot be loaded never runs on the default
/// surface. An unreadable store leaves a fenced delivery for redelivery; an
/// unusable stored definition (or an unfenced caller) gets a visible refusal.
pub(super) fn definition_failure(
    fenced: bool,
    e: &crate::agents::bundle::DefinitionLoadError,
) -> TurnResult {
    match e {
        crate::agents::bundle::DefinitionLoadError::Unavailable(_) if fenced => {
            TurnResult::unsettled(format!("{e}; the delivery is retried"))
        }
        _ => TurnResult::text(format!("{e}; the turn did not run")),
    }
}

/// The lease a turn's driver holds on its marker. A turn run under a fence
/// checks it before every channel WRITE its tools make, and the pipeline checks
/// it again after the loop: a driver that lost its lease to a reclaim must not
/// speak in the channel the newer driver now owns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TurnFence {
    marker_key: String,
    attempt: u32,
}

impl TurnFence {
    pub(crate) fn new(marker_key: impl Into<String>, attempt: u32) -> Self {
        Self { marker_key: marker_key.into(), attempt }
    }

    /// Whether this attempt holds a live lease. A LAPSED lease that still names
    /// this attempt (nobody reclaimed it) is re-taken by compare-and-swap: a
    /// driver that outlived its lease, for instance on its last generation
    /// attempt while a redelivery decided to abandon the turn without writing,
    /// still owns the turn and must be able to deliver the answer it finished.
    /// An unreadable marker or a missing store is NOT proof of ownership.
    pub(crate) async fn holds(&self, ctx: &AppContext) -> bool {
        let Ok(mut markers) = ctx.store(MEMORY_STORE) else {
            return false;
        };
        let now = ctx.now_ms();
        let Ok(Some((raw, version))) = markers.get_versioned(self.marker_key.as_bytes()).await else {
            return false;
        };
        match MarkerState::parse(&raw) {
            // The turn's own attempt, lapsed and unclaimed, or abandoned for the
            // generation budget while this driver was still finishing: nobody else
            // owns it, so this driver re-takes it by CAS and delivers. A redelivery
            // that abandoned it acked only after writing that state, and this CAS
            // is ordered against any reclaim of it.
            MarkerState::Claimed { attempt, lease_until_ms } if attempt == self.attempt => {
                if now < lease_until_ms {
                    return true;
                }
                matches!(
                    markers
                        .put_if_version_expiring(
                            self.marker_key.as_bytes(),
                            &leased_marker(Some(&raw), self.attempt, now),
                            Some(version),
                            now.saturating_add(A2A_TURN_MARKER_TTL_MS),
                        )
                        .await,
                    Ok(Ok(_))
                )
            }
            MarkerState::Abandoned { attempt } if attempt == self.attempt => matches!(
                markers
                    .put_if_version_expiring(
                        self.marker_key.as_bytes(),
                        &leased_marker(Some(&raw), self.attempt, now),
                        Some(version),
                        now.saturating_add(A2A_TURN_MARKER_TTL_MS),
                    )
                    .await,
                Ok(Ok(_))
            ),
            _ => false,
        }
    }
}

/// [`run_turn`] under a [`TurnFence`]: the in-process mention pipeline.
#[expect(
    clippy::too_many_arguments,
    reason = "internal boundary keeps persisted turn identity fields explicit at its single call site"
)]
#[expect(
    clippy::too_many_arguments,
    reason = "internal boundary keeps persisted turn identity fields explicit"
)]
pub(super) async fn run_fenced_turn(
    ctx: &AppContext,
    llm: &Arc<dyn LlmTransport>,
    agent: &str,
    thread: &str,
    msg_id: &str,
    explicit_model: Option<&str>,
    message: &[u8],
    duplicates: DuplicatePolicy,
    fence: TurnFence,
    requester: Option<&str>,
) -> TurnResult {
    let def = match load_agent_definition(ctx, CONFIG, agent, MAX_STEPS).await {
        Ok(def) => def,
        Err(e) => return definition_failure(true, &e),
    };
    run_turn_with_def(
        ctx,
        llm,
        agent,
        thread,
        msg_id,
        explicit_model,
        message,
        def,
        duplicates,
        Some(fence),
        requester,
    )
    .await
}

/// Run one delegated child turn against the definition
/// [`crate::agents::definition::synth_child_def`] synthesized for it. Takes the
/// definition by value, so no caller can reach the pipeline with "no
/// definition" by omission.
#[expect(
    clippy::too_many_arguments,
    reason = "internal boundary keeps persisted turn identity fields explicit for delegated turns"
)]
pub(crate) async fn run_delegated_turn(
    ctx: &AppContext,
    llm: &Arc<dyn LlmTransport>,
    agent: &str,
    thread: &str,
    msg_id: &str,
    explicit_model: Option<&str>,
    message: &[u8],
    def: AgentDefinition,
    duplicates: DuplicatePolicy,
    requester: Option<&str>,
) -> TurnResult {
    run_turn_with_def(
        ctx,
        llm,
        agent,
        thread,
        msg_id,
        explicit_model,
        message,
        Some(def),
        duplicates,
        None,
        requester,
    )
    .await
}

/// The answer to a turn that failed before its loop could run. For a fenced
/// (mention) turn, a storage or plane fault (opening the session, admission,
/// model metadata, compaction) is TRANSIENT: the delivery stays unsettled and
/// is redelivered, instead of a passing fault being posted and acknowledged
/// as the turn's permanent answer. Each redelivery spends one of the turn's
/// generation attempts, so on the LAST attempt the failure is answered
/// legibly instead: an outage that outlasts the attempt budget (or a
/// persistent configuration fault such as an unknown model) ends in a visible
/// refusal, never in a turn abandoned in silence. A definitive error (an
/// invalid scope or record) is always answered.
pub(super) fn setup_failure(
    fenced: bool,
    last_attempt: bool,
    what: &str,
    e: &crate::agents::error::AgentError,
) -> TurnResult {
    use crate::agents::error::AgentError;
    let transient = matches!(
        e,
        AgentError::Plane { .. } | AgentError::Conflict | AgentError::SessionNotOpen
    );
    if fenced && transient && !last_attempt {
        TurnResult::unsettled(format!("{what} failed: {e}; the delivery is retried"))
    } else {
        TurnResult::text(format!("{what} error: {e}"))
    }
}

/// The answer when the lookup of a turn's RECORDED final answer fails. Unlike
/// other setup faults this is never answered on the last attempt: an earlier
/// driver may already have recorded the real answer and crashed before
/// delivering it, and staging an error text here would deliver that error in
/// its place and let the delivery marker suppress the real answer for good.
/// A fenced turn therefore always stays unsettled, so a later delivery reads
/// the record once the store answers again; an exhausted attempt budget is
/// handled by abandonment, which keeps the recorded answer recoverable. An
/// unfenced turn has no redelivery and answers as before.
pub(super) fn final_answer_read_failure(
    fenced: bool,
    last_attempt: bool,
    e: &crate::agents::error::AgentError,
) -> TurnResult {
    if fenced {
        TurnResult::unsettled(format!(
            "final-answer read failed: {e}; the delivery is retried rather than answered in place of a recorded answer"
        ))
    } else {
        setup_failure(false, last_attempt, "final-answer read", e)
    }
}

/// A deliverable setup-error answer is only deliverable while no final answer
/// is recorded for the turn: an earlier driver may have recorded the real
/// answer and crashed before delivering it (or another driver may finalize
/// while this one fails its setup), and staging the error would deliver it in
/// the record's place and let the delivery marker suppress the real answer.
/// `final_absent` is whether a fresh lookup PROVED no final is recorded;
/// anything else (recorded, or the lookup failed) leaves the delivery
/// unsettled so the recorded answer stays recoverable.
pub(super) fn setup_answer_unless_final(
    answer: TurnResult,
    final_absent: bool,
    what: &str,
    e: &crate::agents::error::AgentError,
) -> TurnResult {
    if answer.retry || final_absent {
        answer
    } else {
        TurnResult::unsettled(format!(
            "{what} error: {e}; a recorded final answer exists or could not be ruled out, so the delivery is left for recovery"
        ))
    }
}

/// [`setup_failure`] for a fenced turn, rechecking the recorded final before
/// an error answer becomes deliverable (see [`setup_answer_unless_final`]).
pub(super) async fn fenced_setup_failure<A: crate::agents::session::ApeOps>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    msg_id: &str,
    fenced: bool,
    last_attempt: bool,
    what: &str,
    e: &crate::agents::error::AgentError,
) -> TurnResult {
    let answer = setup_failure(fenced, last_attempt, what, e);
    if !fenced || answer.retry {
        return answer;
    }
    let final_absent = matches!(
        engine.step_seq(scope, &format!("{msg_id}:final")).await,
        Ok(None)
    );
    setup_answer_unless_final(answer, final_absent, what, e)
}

/// Run one turn against an ALREADY-RESOLVED definition. A `None` def is the
/// no-definition path: the default tool surface, the platform default model.
/// Private on purpose: callers go through [`run_turn`] (which loads the
/// persona's definition) or [`run_delegated_turn`], so a route addressing a
/// registered persona cannot fail open by passing `None`.
#[expect(
    clippy::too_many_arguments,
    reason = "internal boundary keeps persisted turn identity fields explicit for normal and delegated turns"
)]
pub(super) async fn run_turn_with_def(
    ctx: &AppContext,
    llm: &Arc<dyn LlmTransport>,
    agent: &str,
    thread: &str,
    msg_id: &str,
    explicit_model: Option<&str>,
    message: &[u8],
    def: Option<AgentDefinition>,
    duplicates: DuplicatePolicy,
    fence: Option<TurnFence>,
    requester: Option<&str>,
) -> TurnResult {
    let model: String = resolve_model(explicit_model, def.as_ref());
    let model = model.as_str();
    let fenced = fence.is_some();
    // The last generation attempt the marker allows: setup faults are answered
    // on it rather than left for a redelivery that would abandon the turn.
    let last_attempt = fence
        .as_ref()
        .is_some_and(|f| f.attempt >= A2A_TURN_MAX_ATTEMPTS);
    let scope = match Scope::new(TENANT, agent, thread) {
        Ok(s) => s,
        Err(e) => return TurnResult::text(format!("scope error: {e}")),
    };
    let ws_scope = format!("{agent}:{thread}");

    let ape = match ctx.ape(SESSION) {
        Ok(a) => a,
        Err(e) => {
            return TurnResult::text(format!("session plane error: {e}"));
        }
    };
    let mut engine = SessionEngine::new(ape);
    if let Err(e) = engine.open(&scope).await {
        return fenced_setup_failure(&mut engine, &scope, msg_id, fenced, last_attempt, "open", &e)
            .await;
    }
    match engine.admit_message(&scope, msg_id, message).await {
        Ok(AdmitOutcome::Admitted { .. }) => {}
        Ok(AdmitOutcome::Duplicate) => {
            if duplicates == DuplicatePolicy::Skip {
                return TurnResult::duplicate();
            }
        }
        Err(e) => {
            return fenced_setup_failure(&mut engine, &scope, msg_id, fenced, last_attempt, "admit", &e)
                .await
        }
    }

    // The Fabric A2A seam needs BOTH halves — the egress handle and a
    // non-empty client token — before the tools are advertised or wired:
    // showing the model a tool the caller gate would refuse is a trap.
    let has_fabric_a2a = ctx.payload().services.contains_key(FABRIC_A2A_EGRESS)
        && ctx
            .secret(FABRIC_A2A_TOKEN_SECRET)
            .is_some_and(|t| !t.trim().is_empty());
    // Web tools follow the same rule: schemas only when the egress exists.
    let has_web = ctx.payload().services.contains_key(WEB_EGRESS);
    // And the MCP connector tools, likewise: a workspace that cannot reach any
    // connector must not be offered tools for reaching one.
    let has_mcp = mcp_egress_bound(ctx);

    // The tool surface. Buzz tools are ALWAYS present — the workspace is this
    // same application, so the seam cannot be un-wired. Definition-gated
    // tools (skills, ask_question, delegate) join when declared; the whole
    // set then passes the definition's own allow/deny filter.
    let mut schemas = coding_tool_schemas();
    schemas.extend(buzz_tool_schemas());
    // Beside the buzz tools and unconditional for the same reason: the mechanism
    // needs no binding this deployment might lack. A wake is a delayed publish
    // onto the queue this pipeline is already consuming, so a persona that can be
    // mentioned can be scheduled.
    schemas.extend(crate::agents::tools::schedule_tool_schemas());
    if has_fabric_a2a {
        schemas.extend(fabric_a2a_tool_schemas());
    }
    if has_web {
        schemas.extend(web_tool_schemas());
    }
    if has_mcp {
        schemas.extend(mcp_tool_schemas());
    }
    if let Some(def) = &def {
        if !def.skills.is_empty() {
            schemas.push(crate::agents::tools::load_skill_tool_schema(&def.skills));
        }
        if def.ask_question {
            schemas.push(crate::agents::tools::ask_question_tool_schema());
        }
        if !def.subagents.is_empty() {
            schemas.push(crate::agents::tools::delegate_tool_schema(&def.subagents));
        }
    }
    if let Some(def) = &def {
        schemas = def.filter_schemas(schemas);
    }
    // The compaction trigger must count what rides EVERY model call but never
    // appears in the folded view: the rendered system prompt and the
    // serialized tool schemas. Computed before the schemas move into the
    // backend.
    let fixed_overhead = crate::agents::compaction::fixed_overhead_tokens(
        base_instructions_for(def.as_ref().and_then(|d| d.instructions.as_deref())),
        &schemas,
    );
    let backend = LlmBackend::new(ctx, Arc::clone(llm), model).with_tool_schemas(schemas);
    // A turn whose final answer is already recorded needs no model setup: its
    // replay returns that answer. Running metadata and compaction first could
    // turn a setup fault into an error answer posted in place of the real one.
    let finalized = match engine.step_seq(&scope, &format!("{msg_id}:final")).await {
        Ok(seq) => seq.is_some(),
        Err(e) => return final_answer_read_failure(fenced, last_attempt, &e),
    };
    if !finalized {
        let metadata = match backend.model_metadata().await {
            Ok(metadata) => metadata,
            Err(e) => {
                return fenced_setup_failure(
                    &mut engine,
                    &scope,
                    msg_id,
                    fenced,
                    last_attempt,
                    "model metadata",
                    &e,
                )
                .await
            }
        };
        if let Err(e) = compact_if_needed(
            &mut engine,
            &scope,
            &backend,
            metadata.context_window,
            None,
            fixed_overhead,
        )
        .await
        {
            return fenced_setup_failure(&mut engine, &scope, msg_id, fenced, last_attempt, "compaction", &e)
                .await;
        }
    }
    let model_client = SteeringModelClient::new(
        backend,
        base_instructions_for(def.as_ref().and_then(|d| d.instructions.as_deref())),
        Vec::new(),
        Vec::new(),
    );
    let mut tools = CloudToolRegistry::new(ctx, WS_CONTENT, WS_INDEX, ws_scope)
        .with_local_memory(MEMORY_STORE, TENANT, agent, thread)
        .with_llm(Arc::clone(llm))
        .with_buzz(agent, msg_id)
        .with_requester(requester)
        .with_turn_fence(fence.clone())
        .with_web(WEB_EGRESS, CONFIG);
    if has_mcp {
        tools = tools.with_mcp();
    }
    if has_fabric_a2a {
        if let Some(token) = ctx.secret(FABRIC_A2A_TOKEN_SECRET) {
            let token = token.trim().to_string();
            if !token.is_empty() {
                tools =
                    tools.with_fabric_a2a(FABRIC_A2A_EGRESS, fabric_a2a_endpoint(ctx).await, token);
            }
        }
    }
    if let Some(def) = &def {
        if !def.skills.is_empty() {
            tools = tools.with_skills(CONFIG, agent, def.skills.clone());
        }
        if !def.subagents.is_empty() {
            tools = tools.with_subagents(
                def.subagents.clone(),
                agent,
                thread,
                msg_id,
                def.child_policy(),
            );
        }
    }
    // Durable approval grants recorded for this thread: a required tool runs
    // only if a structured approve action has recorded its grant.
    let granted = match def.as_ref() {
        Some(d) if !d.approvals.is_empty() => granted_approvals(&mut engine, &scope).await,
        _ => std::collections::BTreeSet::new(),
    };
    // Cloned BEFORE the registry is wrapped and borrowed for the loop: the
    // turn loop reads it off the TurnResult to decide whether the persona
    // already answered into the channel.
    let buzz_posts_handle = tools.buzz_posts_handle();
    let tools = FilteredTools::new(&tools, def.as_ref()).with_granted_approvals(granted);
    let max_steps = def
        .as_ref()
        .map_or(MAX_STEPS, |d| d.effective_max_steps(MAX_STEPS));
    let posts_made = || {
        buzz_posts_handle
            .as_ref()
            .map_or(0, |counter| counter.load(Ordering::SeqCst))
    };
    let outcome = crate::agents::agent_loop::run_tool_loop(
        &mut engine,
        &scope,
        &model_client,
        &tools,
        msg_id,
        max_steps,
    )
    .await;
    // A driver that lost its lease while the loop ran neither posts nor acks:
    // the newer driver owns the turn (its tool writes were refused by the fence
    // without being recorded, so the newer driver's replay makes them).
    if let Some(fence) = &fence {
        if !fence.holds(ctx).await {
            return TurnResult::unsettled("superseded: this turn's lease was lost");
        }
    }
    // A loop that FAILED. If its final answer is already recorded (reading it
    // back failed, say), redelivery reads it: posting an error in its place would
    // replace the real answer for good. Otherwise a fenced (mention) turn is
    // FINALIZED with its error BEFORE its records are read, and every batch
    // behind that final is drained: the session log then agrees the turn is over,
    // a superseded driver can no longer append a batch the answered marker would
    // strand, and a replay returns this same answer. If another driver finalized
    // first, its recorded answer is the answer.
    let mut error_answer: Option<String> = None;
    if let Err(e) = &outcome {
        let final_step = format!("{msg_id}:final");
        match engine.step_seq(&scope, &final_step).await {
            Ok(None) => {}
            Ok(Some(_)) => {
                return TurnResult::unsettled("the final answer is recorded but could not be read back");
            }
            Err(e) => {
                return TurnResult::unsettled(format!("whether the turn finished could not be read: {e}"));
            }
        }
        let text = format!("turn error: {e}");
        // An INTERRUPTION is not an outcome. A storage conflict, or a call left
        // without a recorded result (an unknown write, a crash mid-call), is
        // recovered by replay, which settles that call and resumes generation;
        // finalizing here would turn a transient fault into the turn's answer.
        if fence.is_some() {
            if matches!(e, crate::agents::error::AgentError::Conflict) {
                return TurnResult::unsettled("a storage conflict interrupted the turn; it resumes from its log");
            }
            match recorded_buzz_posts(&mut engine, &scope, msg_id).await {
                Ok(records) if records.unrecorded => {
                    return TurnResult::unsettled(
                        "a call was interrupted before its result was recorded; the turn resumes from its log",
                    );
                }
                Ok(_) => {}
                Err(e) => {
                    return TurnResult::unsettled(format!("the turn's records could not be read: {e}"));
                }
            }
        }
        if fence.is_some() {
            // Every recorded call is settled first and the final is committed
            // behind the closing fence, so an error answer never overtakes a
            // call and a batch another driver records meanwhile keeps the turn
            // open for replay instead of being stranded behind it.
            match crate::agents::agent_loop::finalize_settled(&mut engine, &scope, &tools, msg_id, &text)
                .await
            {
                Ok(Some(answer)) => error_answer = Some(answer),
                Ok(None) => {
                    return TurnResult::unsettled(
                        "another driver recorded a step while this one failed; the turn resumes from its log",
                    );
                }
                Err(e) => {
                    return TurnResult::unsettled(format!("the turn's error could not be recorded as its final answer: {e}"));
                }
            }
        } else {
            // An unfenced turn (an ACP prompt, a delegation) has no replay driver, so its
            // error is its answer, and it is recorded as the turn's final: otherwise the
            // admitted turn stays open for good, and a retry of the same message meets an
            // admission with no final to answer from.
            match crate::agents::agent_loop::finalize_settled(&mut engine, &scope, &tools, msg_id, &text).await {
                Ok(Some(answer)) => error_answer = Some(answer),
                Ok(None) => error_answer = Some(text),
                Err(record_error) => {
                    tracing::warn!("turn {msg_id}: its error could not be recorded as its final answer: {record_error}");
                    error_answer = Some(text);
                }
            }
        }
    }
    // The turn's durable records decide what may be posted, not this run's
    // counters. Walked on EVERY turn: a replay does not re-run the tool calls its
    // log already records (so this run's counter misses an earlier driver's
    // posts), and a live post proves only that a write may have landed, not that
    // the earlier records are readable. A channel write started but never
    // recorded (a fence that could not be read, say) or an unreadable record
    // leaves the turn for replay instead of settling it.
    let live_posts = posts_made();
    let recorded = match recorded_buzz_posts(&mut engine, &scope, msg_id).await {
        Ok(recorded) if recorded.unrecorded => {
            return TurnResult::unsettled("a channel write was interrupted before its result was recorded");
        }
        Ok(recorded) => recorded,
        Err(e) => {
            tracing::warn!("turn {msg_id}: could not read its recorded tool posts: {e}");
            return TurnResult::unsettled(format!("recorded tool posts unreadable: {e}"));
        }
    };
    // A post whose RECORDED result does not establish its outcome is resolved by
    // repeating the identical write: the same tool, scope, turn and argument
    // bytes give the same correlation id, so the relay answers with the pinned
    // outcome instead of posting again. Its result step already exists, so a
    // replay would never repeat it; this is the path that settles it.
    let mut resolved_landed = 0usize;
    use crate::agents::agent_loop::ToolRegistry as _;
    for (tool, args) in &recorded.unknown {
        let result = match tools.invoke(tool, args.as_bytes()).await {
            Ok(bytes) => serde_json::from_slice::<serde_json::Value>(&bytes).ok(),
            Err(_) => None,
        };
        match result.as_ref().map(crate::agents::tools::post_record) {
            Some(crate::agents::tools::PostRecord::Landed) => resolved_landed += 1,
            // Only the RELAY's explicit refusal settles it; a park or an argument
            // check that stopped this retry before the relay proves nothing about
            // the earlier write.
            Some(crate::agents::tools::PostRecord::NotPosted)
                if result.as_ref().and_then(|v| v.get("ok")).is_some() => {}
            _ => {
                return TurnResult::unsettled("a recorded post's outcome is still unknown; it is retried");
            }
        }
    }
    let buzz_posts = live_posts.max(recorded.landed + resolved_landed);
    match outcome {
        Ok(answer) => TurnResult {
            answer: answer.into_bytes(),
            duplicate: false,
            buzz_posts,
            retry: false,
        },
        // A turn that FAILED after its tools already posted must still report
        // those posts: the answer-or-silence fallback keys on this count, and
        // reporting zero here made a persona that had already answered post a
        // second "turn error" message into the channel — the exact double
        // voice the rule exists to prevent (observed live, 2026-07-30: a
        // haiku answer followed by a `provider_shape` error under the same
        // trigger). The error text still becomes the turn's answer, so a turn
        // that failed BEFORE posting anything is still legible in the channel.
        Err(_) => TurnResult {
            answer: error_answer.unwrap_or_default().into_bytes(),
            duplicate: false,
            buzz_posts,
            retry: false,
        },
    }
}

/// Whether the session log records this turn's final answer. A scope that
/// cannot be built, or a deployment without the session plane, cannot hold
/// one (the turn itself cannot run there either): `Ok(false)`. Only a failed
/// READ is an error, and leaves the delivery for redelivery.
pub(super) async fn turn_finalized(
    ctx: &AppContext,
    agent: &str,
    thread: &str,
    msg_id: &str,
) -> std::result::Result<bool, String> {
    let Ok(scope) = Scope::new(TENANT, agent, thread) else {
        return Ok(false);
    };
    let Ok(ape) = ctx.ape(SESSION) else {
        return Ok(false);
    };
    let mut engine = SessionEngine::new(ape);
    engine
        .step_seq(&scope, &format!("{msg_id}:final"))
        .await
        .map(|seq| seq.is_some())
        .map_err(|e| e.to_string())
}

/// What a turn's log says about its calls.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RecordedPosts {
    /// `buzz_post` calls whose recorded result is accepted.
    pub(super) landed: usize,
    /// `buzz_post` calls whose recorded result does not establish the outcome,
    /// as `(tool, recorded argument string)`: resolved by repeating the write.
    pub(super) unknown: Vec<(String, String)>,
    /// A call (of ANY tool) the log records WITHOUT a result: started, fenced
    /// off, or stranded by another driver's final answer. Its outcome is not in
    /// the log and its transcript entry has no result, so the turn is not done.
    pub(super) unrecorded: bool,
}

/// Walk this turn's recorded batches, in the loop's own step-id order, until
/// the first batch that does not exist. NOT bounded by the agent's current step
/// limit: a limit lowered since the turn ran must not hide a later batch. A
/// batch or result that exists but cannot be read or decoded is an ERROR, never
/// "no post": it does not establish that nothing was posted.
pub(super) async fn recorded_buzz_posts(
    engine: &mut SessionEngine,
    scope: &Scope,
    turn_id: &str,
) -> crate::agents::error::Result<RecordedPosts> {
    let unreadable = |reason: String| crate::agents::error::AgentError::RecordInvalid { reason };
    let mut recorded = RecordedPosts { landed: 0, unknown: Vec::new(), unrecorded: false };
    for batch_no in 0usize.. {
        let batch_step = format!("{turn_id}:batch:{batch_no}");
        let Some(seq) = engine.step_seq(scope, &batch_step).await? else {
            break;
        };
        let batch = engine
            .event_at(scope, seq)
            .await?
            .and_then(|ev| crate::agents::conv::ToolCallBatch::from_bytes(&ev.data))
            .ok_or_else(|| unreadable(format!("{batch_step} is missing or not a tool-call batch")))?;
        for (i, call) in batch.calls.iter().enumerate() {
            let call_step = format!("{batch_step}:call:{i}");
            let Some(seq) = engine.step_seq(scope, &call_step).await? else {
                recorded.unrecorded = true;
                continue;
            };
            // Every recorded result must be readable, whatever the tool: a step
            // marker naming a missing event is not a finished call.
            let ev = engine
                .event_at(scope, seq)
                .await?
                .ok_or_else(|| unreadable(format!("{call_step} names an event that is missing")))?;
            if call.name != "buzz_post" {
                continue;
            }
            let (_, body) = crate::agents::conv::decode_tool_result(&ev.data);
            let value = serde_json::from_slice::<serde_json::Value>(body)
                .map_err(|e| unreadable(format!("{call_step} result is not JSON: {e}")))?;
            match crate::agents::tools::post_record(&value) {
                crate::agents::tools::PostRecord::Landed => recorded.landed += 1,
                crate::agents::tools::PostRecord::NotPosted => {}
                // A write of unknown outcome: resolved by repeating it.
                crate::agents::tools::PostRecord::Unknown => {
                    recorded.unknown.push((call.name.clone(), call.args.clone()));
                }
            }
        }
    }
    Ok(recorded)
}

/// The Fabric A2A endpoint: the operator's `buzz-fabric-a2a-endpoint` config
/// document when present, else the compiled default. Either way the
/// manifest's egress destinations remain the outer bound.
pub(super) async fn fabric_a2a_endpoint(ctx: &AppContext) -> String {
    if let Ok(mut store) = ctx.store(CONFIG) {
        if let Ok(Some(raw)) = store.get(FABRIC_A2A_ENDPOINT_KEY).await {
            let value = String::from_utf8_lossy(&raw).trim().to_string();
            if !value.is_empty() {
                return value;
            }
        }
    }
    FABRIC_A2A_ENDPOINT.to_string()
}
