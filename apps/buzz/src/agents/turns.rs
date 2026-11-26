//! The persona turn pipeline: consume durable [`TurnRequest`]s off the app's
//! own `agent-turns` queue and drive each to ONE completed agent turn whose
//! answer lands in the buzz channel where the mention happened.
//!
//! The model boundary is a direct call into [`crate::llm`], and a persona's
//! workspace writes go through [`crate::relay::persona::execute`]. The durability
//! contracts:
//!
//! * **The trigger queue stays.** A mention must survive a crash; the queue's
//!   at-least-once delivery plus the session engine's admission (msg id =
//!   trigger event id, [`DuplicatePolicy::Skip`]) is the exactly-one-turn
//!   contract.
//! * **Persona writes stay deterministic.** The `agentcmd/` created-at
//!   reservation keyed by the correlation hash pins each write's event
//!   identity, so model retries and turn redeliveries rebuild the SAME event
//!   and the APE admission absorbs it.
//! * **The A2A lease machinery** keeps its markers in the app's own
//!   `agent-memory` store.
//!
//! The LLM boundary is a trait ([`LlmTransport`]) so gates can script model
//! behavior; production uses [`DirectLlm`], which calls [`crate::llm`] in
//! process — no correlation ids, no reply cache, no cross-turn reply theft.

use ferrite_app::tracing;
use std::io;
use std::sync::Arc;
use std::time::Duration;

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::{rt, serde_json, AppContext};

use crate::agents::definition::PLATFORM_MAX_STEPS_CAP;

/// Per-turn step budget (tool calls + final).
pub(crate) const MAX_STEPS: usize = PLATFORM_MAX_STEPS_CAP;
/// Bindings (must match `ferrite.json`).
pub const TURNS: &str = "agent-turns";
const SESSION: &str = "agent-sessions";
const WS_CONTENT: &str = "agent-workspace-content";
const WS_INDEX: &str = "agent-workspace-index";
const MEMORY_STORE: &str = "agent-memory";
/// Agent definitions, skills, and the A2A endpoint override live in the
/// app's ONE config store (the relay's operator plane writes it).
const CONFIG: &str = "config";
/// The optional web-tool egress. NOT granted by the buzz manifest today; a
/// deployment that adds the grant lights the web tools up with no code
/// change (their schemas are added only when this handle exists).
const WEB_EGRESS: &str = "web-egress";
/// The MCP connector egress: the destination-scoped handles every
/// third-party MCP server in the catalog is reached through. Any present ⇒ the
/// connector tools are advertised AND wired; none ⇒ neither, which is the same
/// rule [`WEB_EGRESS`] follows and for the same reason — a model shown a tool the
/// egress plane refuses has been handed a trap.
///
/// There is more than one handle because a capability carries at most sixteen key
/// prefixes and the catalog needs more destinations than that, so the grants are
/// split by provider. The names come from the CATALOG rather than being spelled
/// here: two literals for one manifest binding is a typo away from a surface that
/// is advertised and then cannot dial.
fn mcp_egress_bound(ctx: &AppContext) -> bool {
    crate::mcp::catalog::EGRESS_HANDLES
        .iter()
        .any(|handle| ctx.payload().services.contains_key(*handle))
}
/// The Fabric A2A seam: egress to the ONE configured A2A peer plus the
/// per-app client token its caller gate requires. Both degrade silently when
/// absent, so a deployment without the seam keeps working unchanged. The
/// token is a SECRET (`ctx.secret`, seeded via MCP `set_secret`).
const FABRIC_A2A_EGRESS: &str = "fabric-a2a-egress";
const FABRIC_A2A_TOKEN_SECRET: &str = "fabric-a2a-client-token";
/// The default peer endpoint — the same prefix the manifest's egress
/// destination declares. An operator can repoint it with the
/// `buzz-fabric-a2a-endpoint` config document (operator-writable); the
/// manifest egress list still bounds what the gateway will deliver to.
const FABRIC_A2A_ENDPOINT: &str = "https://agents.example.com/a2a/";
const FABRIC_A2A_ENDPOINT_KEY: &[u8] = b"buzz-fabric-a2a-endpoint";
const LLM_DEFAULT_MODEL: &str = "claude-sonnet-5";
/// Fixed single-tenant identity for the deployed app.
const TENANT: &str = "cloud";

/// How many turns this instance drives at once. A turn is minutes of
/// wall-clock (model steps, tool calls), so the bound is about memory and
/// provider fairness, not CPU; the queue's visibility redelivery plus the
/// admission dedup make concurrent consumption safe.
const MAX_CONCURRENT_TURNS: usize = 4;

/// How long the turn loop parks waiting for a lease before cycling — short,
/// because this loop is also the only thing that acks finished turns.
const TURN_POLL_MS: u64 = 200;

/// How many times a wake that overtook its own row is re-read while the lease
/// is held, and the real-time pause between reads. `create` publishes the wake
/// BEFORE writing the row on purpose — an early wake is repairable, a lost one
/// is not — so a fast delivery's first read can find nothing. Redelivery would
/// repair that, but only after a full visibility window, and under a frozen
/// harness clock (the schedule gate) a lease never expires, so redelivery
/// never comes. A bounded re-read settles the race in-process: the row lands
/// within milliseconds of the publish it trails. Exhausting the budget leaves
/// the lease unacked, and redelivery repairs it.
const WAKE_ROW_RACE_REREADS: u32 = 10;
/// Real-time pause between those re-reads (the manual clock does not gate
/// [`rt::time::sleep`], which is what lets this work in the frozen-clock gate).
const WAKE_ROW_RACE_PAUSE_MS: u64 = 50;

/// How long a finished A2A turn's marker stays as a tombstone
/// (`a2a/turn/{agent}/{thread}/{msg_id}` in the agent-memory store), so a
/// late redelivery of an already-answered trigger is absorbed rather than
/// answered twice.
const A2A_TURN_MARKER_TTL_MS: u64 = 3_600_000;

/// How long ONE driver of an A2A turn owns the trigger without renewing —
/// the lease. Short and RENEWED (see [`LeaseRenewer`]) rather than sized to
/// cover a whole peer turn: every redelivery a trigger will ever get arrives
/// within a few visibility windows, and a lease that outlives them is a
/// tombstone wearing a lease's clothes. A live driver keeps the trigger; a
/// dead one lets go within one lease window.
const A2A_TURN_LEASE_MS: u64 = 45_000;

/// How often a working driver pushes its lease forward. Three renewals fit
/// inside one lease, so a missed tick cannot hand the trigger away.
const A2A_LEASE_RENEW_MS: u64 = 15_000;

/// Wall-clock ceiling on posting the answer. The post is now ONE in-process
/// ingest (a few plane round trips) plus its top-level fallback, so this is
/// a generous ceiling rather than a queue-polling budget.
const A2A_POST_BUDGET_MS: u64 = 30_000;

/// How many renewals one driver may perform — sized to the handler's own
/// ceiling so a leaked renewer stops on its own.
const A2A_MAX_LEASE_RENEWALS: u64 =
    (crate::agents::tools::FABRIC_A2A_WALL_MS + A2A_POST_BUDGET_MS) / A2A_LEASE_RENEW_MS + 2;

/// Compile-time guard on the lease window: a lease survives a missed
/// renewal, fits inside the marker TTL, and the renewer covers the whole
/// handler. Pure arithmetic over constants — a change that breaks one fails
/// the BUILD.
const _LEASE_WINDOW_IS_SANE: () = {
    assert!(A2A_TURN_LEASE_MS >= 3 * A2A_LEASE_RENEW_MS);
    assert!(A2A_TURN_LEASE_MS < A2A_TURN_MARKER_TTL_MS);
    assert!(
        A2A_MAX_LEASE_RENEWALS * A2A_LEASE_RENEW_MS
            > crate::agents::tools::FABRIC_A2A_WALL_MS + A2A_POST_BUDGET_MS
    );
};

/// The property that makes the lease a lease rather than a tombstone in
/// disguise: it must lapse while the trigger still HAS redeliveries.
const _LEASE_IS_RECLAIMABLE_IN_TIME: () = {
    assert!(A2A_TURN_LEASE_MS < crate::agents::tools::FABRIC_A2A_WALL_MS);
};

/// How many drivers may take the lease for one trigger before it is
/// abandoned, so a turn that kills its driver deterministically cannot bill
/// the peer forever. GENERATION attempts only: a turn whose answer is already
/// staged under its marker is never abandoned for this budget — reclaiming it
/// only re-posts the staged text (no model or peer call), and the queue's own
/// redelivery bound is what stops a post that can never succeed.
const A2A_TURN_MAX_ATTEMPTS: u32 = 3;
/// Cap on the answer text posted into a channel — the same bound the relay
/// puts on the trigger it sends here (`MAX_TRIGGER_BYTES`).
const A2A_REPLY_MAX_BYTES: usize = 8 * 1024;
/// How many times a claim re-reads after losing its compare-and-swap.
const CLAIM_ROUNDS: usize = 3;
/// How many times staging an answer re-reads after a conflict or a transient
/// store error. Generous on purpose: losing a staging race to this attempt's
/// own lease renewer must not throw away an answer the attempt still owns.
const STAGE_ROUNDS: usize = 16;
/// Backoff between staging rounds that failed on a store error.
const STAGE_BACKOFF_MS: u64 = 100;

mod llm_backend;
mod execution;
mod a2a_lease;
mod fabric_context;
mod delivery;

pub use llm_backend::{DirectLlm, LlmTransport};
pub(crate) use execution::{DuplicatePolicy, TurnFence, clean_model_id, run_delegated_turn, run_turn};
use a2a_lease::{MarkerState, run_a2a_turn, run_local_turn};
pub(crate) use a2a_lease::{drive_prompt, PromptDrive};
#[cfg(test)]
use a2a_lease::{claimed_marker, decide_claim};
#[cfg(test)]
use crate::agents::tools::buzz_tool_schemas;
#[cfg(test)]
use crate::agents::tools::coding_tool_schemas;
#[cfg(test)]
use crate::agents::tools::mcp_tool_schemas;
#[cfg(test)]
use crate::agents::tools::CloudToolRegistry;
#[cfg(test)]
use llm_backend::parse_agent_json_completion;
#[cfg(test)]
use llm_backend::render_tools;
#[cfg(test)]
use execution::MODEL_SENTINEL_DEFAULT;
#[cfg(test)]
use execution::final_answer_read_failure;
#[cfg(test)]
use execution::resolve_model;
#[cfg(test)]
use execution::setup_answer_unless_final;
#[cfg(test)]
use execution::setup_failure;
#[cfg(test)]
use a2a_lease::ClaimDecision;
#[cfg(test)]
use a2a_lease::PendingAnswer;
#[cfg(test)]
use a2a_lease::abandoned_marker;
#[cfg(test)]
use a2a_lease::decide_claim_with;
#[cfg(test)]
use a2a_lease::finish_owned;
#[cfg(test)]
use a2a_lease::leased_marker;
#[cfg(test)]
use a2a_lease::marker_is_owned_by;
#[cfg(test)]
use a2a_lease::marker_pending;
#[cfg(test)]
use a2a_lease::staged_marker;
#[cfg(test)]
use fabric_context::a2a_bridge_prompt;
#[cfg(test)]
use fabric_context::persona_context_key;
#[cfg(test)]
use fabric_context::read_context_generation;
#[cfg(test)]
use fabric_context::retire_before_ceiling;
#[cfg(test)]
use fabric_context::retire_context_generation;
#[cfg(test)]
use delivery::DeliveryStep;
#[cfg(test)]
use delivery::PostOutcome;
#[cfg(test)]
use delivery::clamp_channel_reply;
#[cfg(test)]
use delivery::classify_post_result;
#[cfg(test)]
use delivery::next_delivery_step;

/// Which scheduled occurrence a [`TurnRequest`] is, when it came from a
/// schedule rather than a mention.
///
/// Carried so the consumer can consult the schedule's ROW before running: the
/// queue plane cannot recall a delayed message, so the row is what makes
/// cancellation real rather than decorative. See [`crate::relay::schedules`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct ScheduledWake {
    /// The schedule this wake belongs to.
    pub id: String,
    /// Which occurrence, counting from zero. Half of the admission id.
    pub occurrence: u32,
    /// The schedule revision this wake was published for. A re-created schedule
    /// raises it, so a wake left over from the cancelled revision cannot run the
    /// new one. A wake queued before this field existed reads as 0, its true value.
    #[serde(default)]
    pub revision: u32,
}

/// The durable thread a channel's agent turns live in.
///
/// Delegates to the mention path's own [`crate::relay::agents::dispatch_thread`]
/// rather than repeating `buzz-{channel}`, because the two MUST agree and the
/// failure if they drift is quiet and strange: a scheduled turn would land in a
/// thread holding none of the channel's mentions, so the persona would answer as
/// though it had never spoken there.
#[must_use]
pub fn thread_for_channel(channel: &str) -> String {
    crate::relay::agents::dispatch_thread(channel)
}

/// One durable persona-turn trigger, published by the relay's dispatch hook
/// (`crate::relay::ingress::maybe_dispatch`) onto the `agent-turns` queue.
/// `msg_id` is the triggering EVENT id — the admission dedup key that
/// absorbs queue redeliveries into exactly one turn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct TurnRequest {
    /// The persona (registry agent id) that was mentioned.
    pub agent: String,
    /// Which backend runs the turn — the registry's `harness` field. Empty
    /// or `ferrite-cloud` is the in-process loop; `a2a` / `fabric` drives
    /// the configured A2A peer.
    #[serde(default)]
    pub harness: String,
    /// The trigger event id (64-hex): the turn's admission dedup key.
    pub msg_id: String,
    /// The durable agent thread (the relay's `buzz-{channel}` convention).
    pub thread: String,
    /// The rendered trigger message.
    pub message: String,
    /// Optional explicit model override (none today; the registry does not
    /// carry one).
    #[serde(default)]
    pub model: Option<String>,
    /// Set when this turn came from a schedule rather than a mention.
    ///
    /// `skip_serializing_if` so a mention's envelope stays byte-identical to what
    /// it was before schedules existed: an in-flight queue message written by an
    /// older build decodes here, and one written here decodes there.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schedule: Option<ScheduledWake>,
    /// The member whose message (or schedule) started this turn, as 64-hex.
    ///
    /// A persona acts for whoever mentioned it, so the authority a turn's tools
    /// exercise is intersected with that member's: a read of a channel the
    /// member cannot read comes back empty, and a connector call with the
    /// workspace credential needs the member to be an operator, exactly as on
    /// the member's own routes. `None` (a persona-created schedule, an envelope
    /// from an older build) carries no member, so connector calls are refused
    /// and reads are the persona's own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requester: Option<String>,
}

/// Reports a finished turn to the loop EXACTLY once, even on unwind — the
/// relay api-loop discipline: the slot count only moves on a completion, so
/// an unreported exit would leak a slot forever.
struct Completion {
    tx: rt::mpsc::Sender<Option<u64>>,
    /// `Some(token)` once the turn completed and the lease may be acked;
    /// `None` leaves the envelope unacked so redelivery re-drives it.
    ack: Option<u64>,
}

impl Drop for Completion {
    fn drop(&mut self) {
        let _ = self.tx.try_send(self.ack);
    }
}

/// What a scheduled wake may do, decided when it FIRES rather than when it was
/// created.
///
/// Three outcomes, because two cannot express the difference between a sanction
/// that lapses and one that does not. Collapsing them would either destroy a
/// schedule its owner expects to resume, or skip an occurrence without queueing
/// another — which kills the schedule by accident, and the accident looks exactly
/// like the feature working.
enum WakeDecision {
    /// Run the turn, then settle.
    Run(crate::relay::schedules::Schedule),
    /// Do not run, but settle: consume this occurrence and queue the next. For a
    /// paused agent or a timed-out creator, both of which end.
    Skip(crate::relay::schedules::Schedule),
    /// Do not run and do not continue.
    Stop,
    /// Do not run, and do not decide: the row is not there yet, so this wake raced
    /// its own creation. Leave the lease unacked and let redelivery repair it — see
    /// [`crate::relay::schedules::Claim::Absent`] for the live measurement that
    /// made this its own answer rather than a `Stop`.
    Retry,
}

/// Consume the `agent-turns` queue with the production transport.
pub async fn turn_loop(ctx: AppContext) -> io::Result<()> {
    turn_loop_with(ctx, Arc::new(DirectLlm)).await
}

/// Consume the `agent-turns` queue, driving each trigger to one completed
/// turn over `llm` — bounded-concurrent (the relay api-loop pattern), acked
/// after completion, poison envelopes acked away.
pub async fn turn_loop_with(ctx: AppContext, llm: Arc<dyn LlmTransport>) -> io::Result<()> {
    eprintln!("[diag] turn loop starting");
    let mut turns = ctx.queue(TURNS)?;
    let (done_tx, mut done_rx) = rt::mpsc::channel::<Option<u64>>(MAX_CONCURRENT_TURNS);
    let mut in_flight = 0usize;

    loop {
        while let Ok(token) = done_rx.try_recv() {
            if let Some(token) = token {
                let _ = turns.ack(token).await;
            }
            in_flight = in_flight.saturating_sub(1);
        }
        if in_flight >= MAX_CONCURRENT_TURNS {
            match done_rx.recv().await {
                Some(token) => {
                    if let Some(token) = token {
                        let _ = turns.ack(token).await;
                    }
                    in_flight = in_flight.saturating_sub(1);
                }
                None => return Ok(()),
            }
            continue;
        }

        let lease = match turns.poll_wait(TURN_POLL_MS).await {
            Ok(Some(l)) => l,
            Ok(None) => continue,
            Err(e) => {
                tracing::warn!("turn poll failed: {e}");
                rt::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        let Ok(request) = serde_json::from_slice::<TurnRequest>(&lease.payload) else {
            let _ = turns.ack(lease.token).await; // poison rule
            continue;
        };
        if request.agent.is_empty() || request.msg_id.is_empty() || request.thread.is_empty() {
            let _ = turns.ack(lease.token).await; // unanswerable: no identity to run under
            continue;
        }

        let task_ctx = ctx.clone();
        let task_llm = Arc::clone(&llm);
        let task_done = done_tx.clone();
        let token = lease.token;
        in_flight = in_flight.saturating_add(1);
        rt::spawn(async move {
            let mut completion = Completion {
                tx: task_done,
                ack: None,
            };
            // A SCHEDULED wake consults its row and the CURRENT authority first.
            // The queue cannot recall a delayed message, so this is what makes
            // cancellation real — and it fails CLOSED: an unreadable row is not
            // permission to spend.
            let scheduled = match &request.schedule {
                None => None,
                Some(wake) => match wake_decision_settled(&task_ctx, wake).await {
                    Ok(WakeDecision::Run(row)) => Some(row),
                    // Not this time, but the schedule lives: settle it so the
                    // occurrence is consumed and the next one queued, then stop.
                    // Without the settle a pause would silently end a schedule.
                    Ok(WakeDecision::Skip(row)) => {
                        let now_ms = task_ctx.now_ms();
                        if let Err(e) =
                            settle_schedule(&task_ctx, &row, &request.harness, now_ms).await
                        {
                            tracing::warn!(
                                "a skipped occurrence did not advance, so it will be \
                                 redelivered: {e}"
                            );
                            return;
                        }
                        completion.ack = Some(token);
                        return;
                    }
                    // DEFINITIVE: cancelled, exhausted, a stale redelivery, or a
                    // creator whose authority is gone for good. Acked, because none
                    // of those change on a retry. A stale redelivery of a
                    // recurring schedule's PREVIOUS occurrence first republishes
                    // the committed next one: its settlement may have committed
                    // and then failed to publish, and this delivery is the only
                    // trigger left to repair that. If the repair cannot be done,
                    // the delivery stays unacked and is retried.
                    Ok(WakeDecision::Stop) => {
                        if let Err(e) = repair_advanced_schedule(&task_ctx, wake, &request.harness).await {
                            tracing::warn!(
                                "a stale wake could not confirm its schedule's next occurrence \
                                 is queued, so it will be redelivered: {e}"
                            );
                            return;
                        }
                        completion.ack = Some(token);
                        return;
                    }
                    // The row is not there YET — this wake overtook its own create.
                    // UNACKED so redelivery retries, by which time the row exists.
                    // Acking here destroyed the wake and left a row that was live,
                    // due, and had nothing coming for it (measured live).
                    Ok(WakeDecision::Retry) => {
                        tracing::info!(
                            "a wake arrived before its schedule row was written, so it will \
                             be redelivered rather than discarded"
                        );
                        return;
                    }
                    // TRANSIENT. The lease is deliberately left UNACKED so the
                    // queue redelivers and the claim is retried: acking here would
                    // drop a due occurrence because a store read blipped, and a
                    // lost occurrence cannot be recovered while a redelivery is
                    // cheap (the admission absorbs it).
                    Err(e) => {
                        tracing::warn!(
                            "a scheduled wake could not read its row, so it did not run and \
                             will be redelivered: {e}"
                        );
                        return;
                    }
                },
            };

            let drive = run_one_turn(&task_ctx, &task_llm, &request).await;
            if drive == TurnDrive::Retry {
                // UNACKED: the A2A driver could not reach a terminal state (a store
                // blip, a live lease another driver holds, an answer it could not
                // deliver). Acking would remove the only durable trigger; leaving
                // it lets redelivery reclaim once the lease expires.
                return;
            }

            // AFTER the turn, so a crash mid-turn redelivers into the same
            // admission and is absorbed rather than consuming an occurrence that
            // nothing performed.
            if let Some(row) = &scheduled {
                let now_ms = task_ctx.now_ms();
                if let Err(e) = settle_schedule(&task_ctx, row, &request.harness, now_ms).await {
                    // UNACKED, for the same reason and a sharper consequence: the
                    // turn ran but the next wake was never queued, so acking here
                    // stalls a recurring schedule permanently with nothing to
                    // notice it. On redelivery the turn is absorbed by the msg_id
                    // admission and only the settlement is retried.
                    tracing::warn!(
                        "a schedule did not advance, so its wake will be redelivered: {e}"
                    );
                    return;
                }
            }
            // Every completed drive is acked — errors were posted (or logged)
            // by the drive itself; only a crash leaves the lease to redeliver.
            completion.ack = Some(token);
        });
    }
}

/// [`wake_decision`], with the wake-before-row race settled in-process.
///
/// An absent row means this wake overtook its own `create` (which publishes
/// first, on purpose). Leaving the lease unacked for redelivery is correct, but
/// it pays a full visibility window live and never resolves under a frozen
/// harness clock, where a lease cannot expire. So an absent row is re-read a
/// bounded number of times while the lease is held; once the budget is
/// exhausted, the lease stays unacked for redelivery.
async fn wake_decision_settled(
    ctx: &AppContext,
    wake: &ScheduledWake,
) -> crate::relay::Result<WakeDecision> {
    let mut decision = wake_decision(ctx, wake).await;
    for _ in 0..WAKE_ROW_RACE_REREADS {
        if !matches!(decision, Ok(WakeDecision::Retry)) {
            break;
        }
        rt::time::sleep(Duration::from_millis(WAKE_ROW_RACE_PAUSE_MS)).await;
        decision = wake_decision(ctx, wake).await;
    }
    decision
}

/// What this wake may do: the row's own state, then the CURRENT authority.
///
/// Creation-time gating cannot bound a recurring schedule — it was authorized once
/// and then fires for weeks — so the two questions that decide future spend are
/// asked here, at each occurrence.
async fn wake_decision(
    ctx: &AppContext,
    wake: &ScheduledWake,
) -> crate::relay::Result<WakeDecision> {
    let mut config = crate::relay::planes::store(ctx, CONFIG)
        .map_err(|e| crate::relay::BuzzError::Storage(format!("config plane: {e}")))?;
    let community = crate::relay::DEFAULT_COMMUNITY;
    let now_ms = ctx.now_ms();
    let row = match crate::relay::schedules::claim(&mut config, community, wake, now_ms).await? {
        crate::relay::schedules::Claim::Run(row) => row,
        crate::relay::schedules::Claim::Stop => return Ok(WakeDecision::Stop),
        crate::relay::schedules::Claim::Absent | crate::relay::schedules::Claim::Pending => {
            return Ok(WakeDecision::Retry)
        }
    };

    // The CREATOR's standing FIRST, because revocation outranks a pause. Asked in
    // the other order, a paused agent whose creator had been banned was skipped
    // and requeued at every future occurrence forever — the temporary answer
    // applied to the permanent case. A pause is meant to be lifted; a ban is not,
    // so the answer that stops spending has to win.
    let creator = match crate::relay::schedules::parse_creator(&row.created_by) {
        crate::relay::schedules::ScheduleCreator::Member(creator) => {
            crate::relay::ingress::creator_standing(ctx, community, &creator, now_ms).await?
        }
        // Created by a PERSONA (its agent id, not a member pubkey), so there is no
        // member standing to check on that side — the persona's own, below, is the gate.
        crate::relay::schedules::ScheduleCreator::Agent(_) => {
            crate::relay::ingress::CreatorStanding::Allowed
        }
    };
    // And the PERSONA's own, which can change after the schedule was made: it may
    // have been retired from the registry, removed from a closed community, or
    // sanctioned. Without this a banned persona kept being woken hourly, each
    // occurrence spending a model call for an answer ingest then refused.
    let persona =
        crate::relay::ingress::persona_standing(ctx, community, &row.agent, now_ms).await?;
    let standing = creator.strictest(persona);
    if standing == crate::relay::ingress::CreatorStanding::Revoked {
        // PROPAGATED, not best-effort. Acking a revoked wake whose cancellation
        // failed strands the worst state available: a schedule that still reads as
        // live, with no wake queued and none coming. Leaving the lease unacked
        // means redelivery retries the cancellation instead.
        //
        // This is the one place the usual preference reverses — elsewhere a
        // duplicate beats a loss, but here the duplicate is another attempt to
        // STOP spending and the loss is a schedule nobody can see is broken.
        crate::relay::schedules::cancel(ctx, &mut config, community, &row.id, &row.created_by, true)
            .await?;
        tracing::info!(
            "schedule {} stopped: its creator or its persona is banned, retired, or no \
             longer a member",
            row.id
        );
        return Ok(WakeDecision::Stop);
    }

    // The persona's CURRENT pause state, and a timed-out creator, are the same
    // answer: not this occurrence, but the schedule lives. A mention honours the
    // pause flag, so a scheduled wake has to as well — otherwise the pause route's
    // promise is only true of mentions.
    if crate::relay::ingress::agent_paused(ctx, community, &row.agent).await?
        || standing == crate::relay::ingress::CreatorStanding::Suspended
    {
        return Ok(WakeDecision::Skip(row));
    }
    Ok(WakeDecision::Run(row))
}

/// For a wake answered Stop: republish the committed next occurrence when the
/// row shows this wake's settlement advanced it (see
/// [`crate::relay::schedules::repair_advanced`]).
async fn repair_advanced_schedule(
    ctx: &AppContext,
    wake: &ScheduledWake,
    harness: &str,
) -> crate::relay::Result<()> {
    let mut config = crate::relay::planes::store(ctx, CONFIG)
        .map_err(|e| crate::relay::BuzzError::Storage(format!("config plane: {e}")))?;
    crate::relay::schedules::repair_advanced(
        ctx,
        &mut config,
        crate::relay::DEFAULT_COMMUNITY,
        wake,
        harness,
    )
    .await
}

/// Advance a schedule and queue its next occurrence.
async fn settle_schedule(
    ctx: &AppContext,
    schedule: &crate::relay::schedules::Schedule,
    harness: &str,
    now_ms: u64,
) -> crate::relay::Result<()> {
    let mut config = crate::relay::planes::store(ctx, CONFIG)
        .map_err(|e| crate::relay::BuzzError::Storage(format!("config plane: {e}")))?;
    crate::relay::schedules::settle(
        ctx,
        &mut config,
        crate::relay::DEFAULT_COMMUNITY,
        schedule,
        harness,
        now_ms,
    )
    .await
}

/// Drive one trigger to completion: the in-process loop for the
/// `ferrite-cloud` harness, the A2A path for `a2a`/`fabric` — and for an
/// UNKNOWN harness too, whose legible refusal must surface in the channel
/// where the mention happened (a trigger is fire-and-forget; a swallowed
/// misconfiguration is silence).
/// Whether a drive reached a state the queue may forget.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnDrive {
    /// Answered, refused legibly, or definitively not ours: ack.
    Settled,
    /// Not terminal: leave the delivery unacked so redelivery can reclaim it.
    Retry,
}

/// What an absorbed A2A claim means for the delivery. Only a marker that
/// POSITIVELY says the turn is over (answered, or abandoned after its attempt
/// budget) lets the delivery be forgotten. A live lease, a contended claim or
/// an unreadable marker is somebody's work in progress or an unknown: acking
/// would leave nothing to reclaim the trigger if that driver then crashed.
fn absorbed_drive(reason: &str) -> TurnDrive {
    match reason {
        // ABANDONED is not acked: the last driver may still be finishing (it can
        // re-take its own abandoned marker) and crash before staging its answer;
        // the kept delivery is then what recovers it, bounded by the queue's
        // own delivery budget, exactly as the local path does.
        "answered" => TurnDrive::Settled,
        _ => TurnDrive::Retry,
    }
}

/// After this driver lost ownership, what the marker says about the delivery.
/// Settled only when a fresh read shows a terminal state; otherwise the newer
/// driver may still crash, and this delivery is what would reclaim the turn.
fn superseded_drive(raw: Option<&[u8]>) -> TurnDrive {
    match raw.map(MarkerState::parse) {
        // Only an ANSWERED marker is terminal for a superseded driver: an
        // abandoned one may still hide a finished, undelivered answer (the last
        // attempt can finalize as the redelivery abandons it), so its delivery is
        // kept and the queue's own budget bounds it.
        Some(MarkerState::Answered) => TurnDrive::Settled,
        _ => TurnDrive::Retry,
    }
}

async fn run_one_turn(ctx: &AppContext, llm: &Arc<dyn LlmTransport>, request: &TurnRequest) -> TurnDrive {
    let harness = request.harness.trim();
    if !(harness.is_empty() || harness == "ferrite-cloud") {
        return run_a2a_turn(ctx, request).await;
    }
    match ctx.store(MEMORY_STORE) {
        Ok(markers) => run_local_turn(ctx, llm, request, markers).await,
        // NOT run unprotected: without the marker store a crash or a failed post
        // would lose the answer. The deployment manifest binds this store, so its
        // absence is a fault to retry, not a mode to run in.
        Err(e) => {
            tracing::warn!("no {MEMORY_STORE} store is bound; the turn is left for redelivery: {e}");
            TurnDrive::Retry
        }
    }
}

#[cfg(test)]
mod turn_request_tests {
    use super::*;

    #[test]
    fn the_mcp_egress_handles_are_the_ones_the_relay_declares() {
        // ONE set of manifest bindings, named in two modules. A divergence here
        // would advertise the connector tools on a deployment that cannot dial
        // them, so the catalog is the single source and both sides read it.
        assert_eq!(
            crate::mcp::catalog::EGRESS_HANDLES,
            &[
                crate::relay::store::MCP_EGRESS_CLOUDFLARE,
                crate::relay::store::MCP_EGRESS_PARTNERS
            ]
        );
        // And every connector rides one of them — a connector on an undeclared
        // handle is unreachable with no diagnostic until its first call.
        for connector in crate::mcp::catalog::CONNECTORS {
            assert!(
                crate::mcp::catalog::EGRESS_HANDLES.contains(&connector.egress),
                "{} rides an undeclared handle",
                connector.id
            );
        }
    }

    #[test]
    fn the_mcp_tools_are_not_in_the_unconditional_surface() {
        // They join only where the egress handle exists. Asserted on the surface
        // that IS unconditional (coding + buzz), because a tool that slipped in
        // there would be offered to every model on every deployment.
        let mut always = coding_tool_schemas();
        always.extend(buzz_tool_schemas());
        let always: Vec<String> = always.into_iter().map(|t| t.name).collect();
        for tool in mcp_tool_schemas() {
            assert!(
                !always.contains(&tool.name),
                "{} must be egress-gated, not unconditional",
                tool.name
            );
        }
        // And the gated set is non-empty, so the assertion above is not vacuous.
        assert_eq!(mcp_tool_schemas().len(), 3);
    }

    #[test]
    fn turn_requests_round_trip_with_and_without_harness() {
        let full: TurnRequest = serde_json::from_slice(
            br#"{"agent":"revka","harness":"a2a","msg_id":"m1","thread":"buzz-general","message":"hi"}"#,
        )
        .expect("harnessed request parses");
        assert_eq!(full.harness, "a2a");
        assert!(full.model.is_none());
        let bare: TurnRequest = serde_json::from_slice(
            br#"{"agent":"revka","msg_id":"m1","thread":"buzz-general","message":"hi"}"#,
        )
        .expect("bare request parses");
        assert!(bare.harness.is_empty(), "absent harness is the cloud loop");
        let bytes = serde_json::to_vec(&full).expect("encodes");
        let back: TurnRequest = serde_json::from_slice(&bytes).expect("round trips");
        assert_eq!(back.msg_id, "m1");
    }

    #[test]
    fn runtime_turn_budget_stays_at_or_above_the_node_tier_default() {
        assert!(
            MAX_STEPS >= 24,
            "the app turn loop must not clamp the node-tier default"
        );
    }

    #[test]
    fn a2a_bridge_prompt_explains_reply_delivery_and_context_limits() {
        let prompt = a2a_bridge_prompt("channel: qa\n\nRun the browser check");

        assert!(prompt.contains("Buzz will post your final answer"));
        assert!(prompt.contains("do not require or search for a channel-posting tool"));
        assert!(prompt.contains("cannot independently inspect unquoted channel history"));
        assert!(prompt.contains("<buzz_mention>\nchannel: qa"));
        assert!(prompt.ends_with("Run the browser check\n</buzz_mention>"));
    }

    #[test]
    fn model_resolution_precedence_is_explicit_then_definition_then_default() {
        let def = crate::agents::definition::parse_agent_definition(
            br#"{"version":1,"agent_id":"agent","default_model":"claude-sonnet-5"}"#,
            "agent",
            MAX_STEPS,
        )
        .expect("valid definition");
        assert_eq!(resolve_model(None, None), LLM_DEFAULT_MODEL);
        assert_eq!(resolve_model(None, Some(&def)), "claude-sonnet-5");
        assert_eq!(resolve_model(Some("gpt-6"), Some(&def)), "gpt-6");
        let bare = crate::agents::definition::parse_agent_definition(
            br#"{"version":1,"agent_id":"agent"}"#,
            "agent",
            MAX_STEPS,
        )
        .expect("valid definition");
        assert_eq!(resolve_model(None, Some(&bare)), LLM_DEFAULT_MODEL);
        assert_eq!(
            resolve_model(Some(MODEL_SENTINEL_DEFAULT), Some(&def)),
            "claude-sonnet-5"
        );
        assert_eq!(
            resolve_model(Some(MODEL_SENTINEL_DEFAULT), None),
            MODEL_SENTINEL_DEFAULT
        );
        assert_eq!(
            resolve_model(Some(MODEL_SENTINEL_DEFAULT), Some(&bare)),
            MODEL_SENTINEL_DEFAULT
        );
    }

    #[test]
    fn definition_allowlist_narrows_rendered_tools() {
        let def = crate::agents::definition::parse_agent_definition(
            br#"{"version":1,"agent_id":"agent","tool_allow":["read_file"]}"#,
            "agent",
            MAX_STEPS,
        )
        .expect("valid definition");
        let rendered = render_tools(&def.filter_schemas(coding_tool_schemas()));
        assert!(rendered.contains("read_file"));
        assert!(rendered.contains("end_turn"), "protected tool stays");
        assert!(!rendered.contains("write_file"));
    }

    #[test]
    fn embedded_tool_call_json_is_extracted() {
        let parsed = parse_agent_json_completion(
            "I will remember that.\n{\"tool_calls\":[{\"id\":\"m1\",\"tool\":\"remember_memory\",\"args\":{\"text\":\"x\"}}]}",
        )
        .unwrap()
        .expect("embedded agent JSON is parsed");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].tool, "remember_memory");
    }

    #[test]
    fn bare_tool_calls_array_is_extracted() {
        let parsed = parse_agent_json_completion(
            "[{\"id\":\"m1\",\"tool\":\"recall_memory\",\"args\":{\"query\":\"x\"}}]",
        )
        .unwrap()
        .expect("bare tool_calls array is parsed");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].tool, "recall_memory");
    }

    #[test]
    fn plain_text_without_agent_json_is_plain_final_text() {
        assert!(parse_agent_json_completion("plain model final answer")
            .unwrap()
            .is_none());
    }

    #[test]
    fn answer_mentioning_tool_calls_substring_is_not_hijacked() {
        // A clean final object whose PROSE contains the literal `tool_calls:`
        // (and even a bracketed non-JSON example) must be returned as the
        // final answer, not reinterpreted as tool calls and not failed as a
        // parse error. Before the guard, the wrapper matched the substring
        // anywhere and errored on the `[ {..} ]` slice.
        let parsed = parse_agent_json_completion(
            "{\"final\":\"To call a tool, emit tool_calls: [ {..} ] as JSON.\"}",
        )
        .unwrap()
        .expect("clean final object parses");
        assert!(
            parsed.tool_calls.is_empty(),
            "prose mention is not a tool call"
        );
        assert!(parsed
            .final_text
            .expect("final text present")
            .contains("tool_calls:"));
    }

    #[test]
    fn tool_calls_wrapper_still_parses_when_completion_starts_with_the_token() {
        // The intended non-native protocol: the completion begins with the
        // `tool_calls:` token followed by the array. This path must still work.
        let parsed = parse_agent_json_completion(
            "tool_calls: [{\"id\":\"m1\",\"tool\":\"recall_memory\",\"args\":{\"query\":\"x\"}}]",
        )
        .unwrap()
        .expect("wrapper protocol still parses");
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].tool, "recall_memory");
    }

    #[test]
    fn malformed_json_shaped_completion_still_fails() {
        let err = parse_agent_json_completion("{\"final\":").unwrap_err();
        assert!(
            err.to_string()
                .contains("model completion was not valid agent JSON"),
            "{err}"
        );
    }
}

#[cfg(test)]
mod a2a_turn_lease_tests {
    use super::*;

    const T0: u64 = 1_700_000_000_000;

    fn claimed(attempt: u32, at: u64) -> Vec<u8> {
        claimed_marker(attempt, at)
    }

    fn answered(attempt: u32, at: u64) -> Vec<u8> {
        serde_json::json!({ "state": "answered", "attempt": attempt, "at_ms": at })
            .to_string()
            .into_bytes()
    }

    #[test]
    fn an_unclaimed_trigger_is_the_first_attempt() {
        assert_eq!(decide_claim(None, T0), ClaimDecision::Create { attempt: 1 });
    }

    #[test]
    fn a_live_lease_absorbs_a_redelivery() {
        let held = claimed(1, T0);
        for at in [T0, T0 + 1, T0 + A2A_LEASE_RENEW_MS, T0 + A2A_TURN_LEASE_MS - 1] {
            assert_eq!(
                decide_claim(Some((&held, 7)), at),
                ClaimDecision::Absorb {
                    reason: "in_flight"
                },
                "a driver inside its own budget must not be displaced at {at}"
            );
        }
    }

    #[test]
    fn an_expired_lease_is_reclaimed_by_the_next_driver() {
        let dead = claimed(1, T0);
        assert_eq!(
            decide_claim(Some((&dead, 42)), T0 + A2A_TURN_LEASE_MS),
            ClaimDecision::Reclaim {
                attempt: 2,
                version: 42
            },
        );
    }

    #[test]
    fn a_reclaim_loop_is_bounded_so_a_poison_turn_stops_charging_the_peer() {
        let last = claimed(A2A_TURN_MAX_ATTEMPTS, T0);
        assert_eq!(
            decide_claim(Some((&last, 1)), T0 + A2A_TURN_LEASE_MS * 4),
            ClaimDecision::Abandon { attempt: A2A_TURN_MAX_ATTEMPTS, version: 1 },
            "the budget's end is a durable transition, not a silent ack"
        );
        // Once abandoned, a redelivery is absorbed (acked on a terminal marker),
        // unless the session log records a finished answer, which is delivered.
        let gone = abandoned_marker(A2A_TURN_MAX_ATTEMPTS, T0);
        assert_eq!(
            decide_claim(Some((&gone, 2)), T0 + A2A_TURN_LEASE_MS * 5),
            ClaimDecision::Absorb { reason: "abandoned" }
        );
        assert!(matches!(
            decide_claim_with(Some((&gone, 2)), T0 + A2A_TURN_LEASE_MS * 5, true),
            ClaimDecision::Reclaim { .. }
        ));
        assert_eq!(superseded_drive(Some(&gone)), TurnDrive::Retry, "an abandoned marker may hide a finished answer");
        let penultimate = claimed(A2A_TURN_MAX_ATTEMPTS - 1, T0);
        assert!(matches!(
            decide_claim(Some((&penultimate, 1)), T0 + A2A_TURN_LEASE_MS),
            ClaimDecision::Reclaim { .. }
        ));
        // A turn whose answer is STAGED is out of generation attempts, not out
        // of delivery: it is reclaimed (to post the staged text, no model call)
        // however many leases it has burned, never abandoned and acked.
        let staged = staged_marker(
            &claimed(A2A_TURN_MAX_ATTEMPTS, T0),
            &PendingAnswer { text: "generated".into(), top_level: false, uncertain: false },
        );
        assert_eq!(
            decide_claim(Some((&staged, 7)), T0 + A2A_TURN_LEASE_MS * 4),
            ClaimDecision::Reclaim { attempt: A2A_TURN_MAX_ATTEMPTS + 1, version: 7 }
        );
        let much_later = staged_marker(
            &claimed(A2A_TURN_MAX_ATTEMPTS + 5, T0),
            &PendingAnswer { text: "generated".into(), top_level: true, uncertain: false },
        );
        assert!(matches!(
            decide_claim(Some((&much_later, 8)), T0 + A2A_TURN_LEASE_MS * 4),
            ClaimDecision::Reclaim { .. }
        ));
        // A local turn whose answer the session log already records is out of
        // generation attempts too, but not abandoned: the reclaim replays it.
        assert!(matches!(
            decide_claim_with(Some((&last, 9)), T0 + A2A_TURN_LEASE_MS * 4, true),
            ClaimDecision::Reclaim { .. }
        ));
    }

    #[test]
    fn an_answered_trigger_is_never_re_run() {
        let done = answered(1, T0);
        for at in [T0, T0 + A2A_TURN_LEASE_MS * 10] {
            assert_eq!(
                decide_claim(Some((&done, 3)), at),
                ClaimDecision::Absorb { reason: "answered" }
            );
        }
    }

    #[test]
    fn an_unreadable_marker_is_left_alone() {
        for legacy in [&b"claimed"[..], b"", b"{}", b"{\"state\":\"weird\"}"] {
            assert_eq!(
                decide_claim(Some((legacy, 1)), T0 + A2A_TURN_LEASE_MS * 10),
                ClaimDecision::Absorb { reason: "claimed" },
                "an unreadable marker must never be stolen: {legacy:?}"
            );
        }
    }

    #[test]
    fn a_driver_only_posts_while_it_still_holds_its_own_lease() {
        let mine = claimed(1, T0);
        assert!(marker_is_owned_by(Some(&mine), 1, T0 + 5));
        let theirs = claimed(2, T0 + A2A_TURN_LEASE_MS);
        assert!(!marker_is_owned_by(
            Some(&theirs),
            1,
            T0 + A2A_TURN_LEASE_MS + 5
        ));
        assert!(!marker_is_owned_by(Some(&mine), 1, T0 + A2A_TURN_LEASE_MS));
        assert!(!marker_is_owned_by(Some(&answered(1, T0)), 1, T0));
        assert!(!marker_is_owned_by(None, 1, T0));
        assert!(!marker_is_owned_by(Some(b"claimed"), 1, T0));
    }

    #[test]
    fn the_claimed_marker_carries_the_lease_deadline_it_promises() {
        let raw = claimed(1, T0);
        let doc: serde_json::Value = serde_json::from_slice(&raw).expect("json");
        assert_eq!(doc["lease_until_ms"], serde_json::json!(T0 + A2A_TURN_LEASE_MS));
        assert_eq!(doc["attempt"], serde_json::json!(1));
    }

    #[test]
    fn a_staged_answer_survives_reclaims_and_renewals_with_its_form() {
        // A driver that crashed after generating its answer, or whose post went
        // unconfirmed, must leave the SAME answer for the next driver: a re-run
        // would generate different text, and different text is a second answer.
        let staged = PendingAnswer { text: "the answer".into(), top_level: false, uncertain: false };
        let raw = staged_marker(&claimed(1, T0), &staged);
        assert_eq!(marker_pending(&raw), Some(staged.clone()));
        // A reclaim by attempt 2 and a later renewal both carry it forward.
        let reclaimed = leased_marker(Some(&raw), 2, T0 + A2A_TURN_LEASE_MS + 1);
        assert_eq!(marker_pending(&reclaimed), Some(staged.clone()));
        assert!(matches!(MarkerState::parse(&reclaimed), MarkerState::Claimed { attempt: 2, .. }));
        let renewed = leased_marker(Some(&reclaimed), 2, T0 + A2A_TURN_LEASE_MS + 5);
        assert_eq!(marker_pending(&renewed), Some(staged));
        // The form switch (after an explicit refusal) is what the next driver uses.
        let flipped = staged_marker(&renewed, &PendingAnswer { text: "the answer".into(), top_level: true, uncertain: false });
        assert_eq!(marker_pending(&flipped).map(|p| p.top_level), Some(true));
        // The uncertainty lock survives a reclaim, so a later driver keeps the form.
        let locked = staged_marker(&renewed, &PendingAnswer { text: "the answer".into(), top_level: false, uncertain: true });
        let reclaimed_again = leased_marker(Some(&locked), 3, T0 + A2A_TURN_LEASE_MS * 3);
        assert_eq!(marker_pending(&reclaimed_again).map(|p| (p.top_level, p.uncertain)), Some((false, true)));
        // A marker staged before the lock existed reads as unlocked.
        let legacy = br#"{"state":"claimed","attempt":1,"lease_until_ms":0,"pending":{"text":"t","top_level":false}}"#;
        assert_eq!(marker_pending(legacy).map(|p| p.uncertain), Some(false));
        // A fresh claim has nothing staged.
        assert_eq!(marker_pending(&claimed(1, T0)), None);
    }

    #[test]
    fn an_uncertain_threaded_post_locks_the_form_against_a_later_refusal() {
        // `deliver_answer` stages uncertain=true BEFORE each threaded write; the
        // pure rule sees the lock that existed before that (prior_uncertain).
        let locked = || PendingAnswer { text: "a".into(), top_level: false, uncertain: true };
        // First threaded write ever, definitively refused: switch, lock cleared.
        let mut p = locked();
        assert!(matches!(next_delivery_step(&mut p, false, PostOutcome::Refused("no".into())), DeliveryStep::Restage(_)));
        assert!(p.top_level && !p.uncertain);
        // An earlier threaded write may have committed (a crashed driver's, or
        // this driver's uncertain one): a refusal must NOT switch.
        let mut p = locked();
        assert!(matches!(next_delivery_step(&mut p, true, PostOutcome::Refused("parent gone".into())), DeliveryStep::Again(_)));
        assert!(!p.top_level && p.uncertain, "the threaded form stays, still locked");
        // An uncertain threaded write keeps the lock and the form.
        let mut p = locked();
        assert!(matches!(next_delivery_step(&mut p, false, PostOutcome::Uncertain("timeout".into())), DeliveryStep::Again(_)));
        assert!(!p.top_level && p.uncertain);
        // Top-level outcomes never change the form back.
        let mut p = PendingAnswer { text: "a".into(), top_level: true, uncertain: false };
        assert!(matches!(next_delivery_step(&mut p, false, PostOutcome::Refused("no".into())), DeliveryStep::Again(_)));
        assert!(matches!(next_delivery_step(&mut p, true, PostOutcome::Uncertain("x".into())), DeliveryStep::Again(_)));
        assert!(p.top_level);
    }

    #[test]
    fn channel_reply_clamp_is_char_safe_and_marked() {
        assert_eq!(clamp_channel_reply("short answer"), "short answer");
        let big = "\u{e9}".repeat(A2A_REPLY_MAX_BYTES);
        let clamped = clamp_channel_reply(&big);
        assert!(clamped.ends_with("\n[truncated]"));
        assert!(clamped.len() <= A2A_REPLY_MAX_BYTES, "the whole reply, marker included, fits the cap");
        assert!(clamped.is_char_boundary(clamped.len() - "\n[truncated]".len()));
        let ascii = clamp_channel_reply(&"x".repeat(A2A_REPLY_MAX_BYTES + 1));
        assert_eq!(ascii.len(), A2A_REPLY_MAX_BYTES);
    }

    #[test]
    fn only_a_definitive_refusal_permits_the_top_level_fallback() {
        // A transport failure or an unreadable reply may hide a committed threaded
        // post; falling back on those produced a second, top-level answer.
        assert_eq!(classify_post_result(Ok(br#"{"ok":true}"#.to_vec())), PostOutcome::Posted);
        assert!(matches!(
            classify_post_result(Ok(br#"{"ok":false,"error":"no such parent","retryable":false}"#.to_vec())),
            PostOutcome::Refused(_)
        ));
        assert!(matches!(
            classify_post_result(Err("connection reset".into())),
            PostOutcome::Uncertain(_)
        ));
        assert!(matches!(classify_post_result(Ok(b"<html>".to_vec())), PostOutcome::Uncertain(_)));
        for shape in [
            &b"null"[..],
            b"{}",
            br#"{"ok":false}"#,
            br#"{"result":"posted"}"#,
            br#"{"ok":false,"error":"storage timeout","retryable":true}"#,
            br#"{"ok":false,"error":{}}"#,
            br#"{"ok":false,"error":""}"#,
            br#"{"ok":false,"error":true}"#,
        ] {
            assert!(
                matches!(classify_post_result(Ok(shape.to_vec())), PostOutcome::Uncertain(_)),
                "{} proves nothing about the write",
                String::from_utf8_lossy(shape)
            );
        }
    }

    #[test]
    fn a_live_lease_redelivery_is_not_acked_and_a_finish_is_fenced_to_its_attempt() {
        // Acking a redelivery absorbed by a LIVE lease removed the only trigger
        // that could reclaim it if the running driver then crashed.
        assert_eq!(absorbed_drive("in_flight"), TurnDrive::Retry);
        assert_eq!(absorbed_drive("contended"), TurnDrive::Retry);
        assert_eq!(absorbed_drive("claimed"), TurnDrive::Retry);
        assert_eq!(absorbed_drive("answered"), TurnDrive::Settled);
        assert_eq!(absorbed_drive("abandoned"), TurnDrive::Retry, "the last driver may still finish");
        // A superseded driver acks only on a positively answered marker.
        assert_eq!(superseded_drive(Some(&claimed(3, T0))), TurnDrive::Retry);
        assert_eq!(superseded_drive(None), TurnDrive::Retry);
        assert_eq!(
            superseded_drive(Some(br#"{"state":"answered","attempt":3}"#)),
            TurnDrive::Settled
        );
        // A slow driver finalizes only its own claim, never a newer attempt's.
        assert!(finish_owned(&claimed(2, T0), 2));
        assert!(!finish_owned(&claimed(3, T0), 2), "a reclaimed marker is not overwritten");
        assert!(!finish_owned(br#"{"state":"answered","attempt":2}"#, 2));
    }

    /// A driver that lost its turn cannot speak through its tools: a channel
    /// write behind a lost or lapsed lease fails BEFORE the relay sees it (a
    /// plane error, so the loop records no result and the new owner's replay
    /// makes the call). Reads are not fenced.
    #[tokio::test]
    async fn a_tool_write_behind_a_lost_lease_never_reaches_the_relay() {
        let env = ferrite_app_test::TestHarness::new()
            .with_store(MEMORY_STORE)
            .manual_clock(T0)
            .build()
            .await;
        let ctx = env.context();
        let key = "local/turn/revka/buzz-c/m1";
        let mut markers = ctx.store(MEMORY_STORE).expect("marker store");
        // Attempt 2 reclaimed the turn and holds a live lease.
        markers.put(key.as_bytes(), &claimed(2, T0)).await.expect("seed");
        let registry = |attempt| {
            CloudToolRegistry::new(&ctx, WS_CONTENT, WS_INDEX, "revka:buzz-c")
                .with_buzz("revka", "m1")
                .with_turn_fence(Some(TurnFence::new(key, attempt)))
        };
        let post = br#"{"channel":"c","content":"hello"}"#;
        // Whether a call was stopped BY THE FENCE. This harness wires no relay,
        // so a call that passes the fence still fails behind it; only the fence's
        // own refusal counts here.
        let fenced = |r: crate::agents::error::Result<Vec<u8>>| {
            matches!(r, Err(crate::agents::error::AgentError::Plane { message }) if message.contains("lease was lost"))
        };
        let stale = registry(1);
        assert!(fenced(stale.invoke_buzz("buzz_post", post).await), "a superseded driver's post");
        assert!(
            fenced(stale.invoke_buzz("buzz_react", br#"{"target":"e1","emoji":"+"}"#).await),
            "a superseded driver's reaction"
        );
        assert!(!fenced(stale.invoke_buzz("buzz_read", br#"{"channel":"c"}"#).await), "reads are not fenced");
        // The lease holder passes the fence (the relay's own gates then decide).
        assert!(!fenced(registry(2).invoke_buzz("buzz_post", post).await));
        // A LAPSED lease that nobody reclaimed is still this attempt's: it is
        // re-taken by CAS, and the marker shows a live lease again.
        markers
            .put(key.as_bytes(), &claimed(2, T0 - 2 * A2A_TURN_LEASE_MS))
            .await
            .expect("lapse");
        assert!(!fenced(registry(2).invoke_buzz("buzz_post", post).await), "an unclaimed lapsed lease is re-taken");
        let raw = markers.get(key.as_bytes()).await.expect("read").expect("marker");
        assert!(marker_is_owned_by(Some(&raw), 2, T0), "the re-take renewed the lease");
        // Once a newer attempt reclaims it, the old attempt is refused.
        markers.put(key.as_bytes(), &claimed(3, T0)).await.expect("reclaim");
        assert!(fenced(registry(2).invoke_buzz("buzz_post", post).await), "a reclaimed lease");
        // And an unreadable or missing marker is not ownership.
        markers.delete(key.as_bytes()).await.expect("delete");
        assert!(fenced(registry(2).invoke_buzz("buzz_post", post).await), "a missing marker");
    }

    #[tokio::test]
    async fn a_context_generation_only_moves_forward_and_never_falls_back() {
        let env = ferrite_app_test::TestHarness::new()
            .with_store(MEMORY_STORE)
            .manual_clock(T0)
            .build()
            .await;
        let ctx = env.context();
        let mut markers = ctx.store(MEMORY_STORE).expect("marker store");
        let key = "a2a/ctx/revka/buzz-c";
        // Absent: the first generation.
        assert_eq!(read_context_generation(&mut markers, key).await, Ok(1));
        // Retiring 1 commits 2.
        assert_eq!(retire_context_generation(&mut markers, key, 1).await, Ok(2));
        // A driver that retires further moves it to 3 ...
        assert_eq!(retire_context_generation(&mut markers, key, 2).await, Ok(3));
        // ... and a SLOWER driver still retiring generation 1 cannot lower it:
        // it gets the committed generation to dial instead.
        assert_eq!(retire_context_generation(&mut markers, key, 1).await, Ok(3));
        assert_eq!(read_context_generation(&mut markers, key).await, Ok(3));
        // An unreadable record is an error, never generation 1 (which may be a
        // context already retired as poisoned).
        markers.put(key.as_bytes(), b"not json").await.expect("corrupt");
        assert!(read_context_generation(&mut markers, key).await.is_err());
        assert!(retire_context_generation(&mut markers, key, 3).await.is_err());
        markers.put(key.as_bytes(), br#"{"generation":0}"#).await.expect("zero");
        assert!(read_context_generation(&mut markers, key).await.is_err());
    }

    /// Review finding: a failed task's retirement ran up to `STAGE_ROUNDS`
    /// store rounds with no deadline before the fresh-context retry, so the
    /// exchange could outrun its ceiling. With the ceiling spent nothing is
    /// written and no retry generation is handed out.
    #[tokio::test]
    async fn a_failed_task_retirement_is_bounded_by_the_time_left_before_the_ceiling() {
        let env = ferrite_app_test::TestHarness::new()
            .with_store(MEMORY_STORE)
            .manual_clock(T0)
            .build()
            .await;
        let ctx = env.context();
        let mut markers = ctx.store(MEMORY_STORE).expect("marker store");
        let key = "a2a/ctx/revka/buzz-d";
        assert!(retire_before_ceiling(&mut markers, key, 1, 0)
            .await
            .is_err());
        assert_eq!(read_context_generation(&mut markers, key).await, Ok(1));
        assert_eq!(
            retire_before_ceiling(&mut markers, key, 1, 1_000).await,
            Ok(2)
        );
        assert_eq!(read_context_generation(&mut markers, key).await, Ok(2));
    }

    #[test]
    fn a_setup_fault_is_retried_until_the_last_attempt_then_answered() {
        use crate::agents::error::AgentError;
        let plane = AgentError::Plane { message: "store down".into() };
        // A transient fault on a fenced turn: left for redelivery, nothing posted.
        let early = setup_failure(true, false, "open", &plane);
        assert!(early.retry);
        // On the last attempt it is answered, so the turn never ends in silence.
        let last = setup_failure(true, true, "open", &plane);
        assert!(!last.retry);
        assert!(String::from_utf8_lossy(&last.answer).contains("open error"));
        // A definitive error is answered at once; an unfenced turn has no redelivery.
        let bad = AgentError::RecordInvalid { reason: "too large".into() };
        assert!(!setup_failure(true, false, "admit", &bad).retry);
        assert!(!setup_failure(false, false, "open", &plane).retry);
    }

    /// Review finding: a failed read of the recorded final answer on the last
    /// attempt staged an error text, which was then delivered in place of the
    /// real answer an earlier driver had recorded.
    #[test]
    fn a_failed_final_answer_read_is_never_answered_in_place_of_the_record() {
        use crate::agents::error::AgentError;
        let plane = AgentError::Plane { message: "store down".into() };
        let bad = AgentError::RecordInvalid { reason: "unreadable".into() };
        for e in [&plane, &bad] {
            for last_attempt in [false, true] {
                let r = final_answer_read_failure(true, last_attempt, e);
                assert!(r.retry, "fenced, last_attempt={last_attempt}: left for redelivery");
            }
        }
        // An unfenced turn has no redelivery and still answers.
        assert!(!final_answer_read_failure(false, true, &plane).retry);
    }

    /// Review finding: on the last attempt an open, admission, metadata or
    /// compaction failure became a deliverable error answer without checking
    /// for a recorded final answer, so the error could replace it.
    #[test]
    fn a_last_attempt_setup_error_never_replaces_a_recorded_final() {
        use crate::agents::error::AgentError;
        let plane = AgentError::Plane { message: "store down".into() };
        let last = || setup_failure(true, true, "open", &plane);
        // A final recorded, or not ruled out: left for recovery.
        assert!(setup_answer_unless_final(last(), false, "open", &plane).retry);
        // Proven absent: the legible refusal is delivered as before.
        let answered = setup_answer_unless_final(last(), true, "open", &plane);
        assert!(!answered.retry);
        assert!(String::from_utf8_lossy(&answered.answer).contains("open error"));
        // A retry stays a retry.
        let early = setup_failure(true, false, "open", &plane);
        assert!(setup_answer_unless_final(early, false, "open", &plane).retry);
    }

    #[test]
    fn each_persona_has_its_own_remote_context_in_a_thread() {
        let a = crate::agents::tools::fabric_context_id(&persona_context_key("revka", "buzz-c"), 1);
        let b = crate::agents::tools::fabric_context_id(&persona_context_key("ostra", "buzz-c"), 1);
        assert_ne!(a, b, "two personas in one channel never share a Fabric conversation");
        assert_ne!(a, crate::agents::tools::fabric_context_id("buzz-c", 1));
        assert!(!persona_context_key("revka", "buzz-c").starts_with("tool~"), "never a model-chosen key");
    }
}
