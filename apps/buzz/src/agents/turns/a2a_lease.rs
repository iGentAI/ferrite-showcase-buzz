//! Local and A2A turn drives, and the lease marker that makes each drive exclusive
//! and reclaimable.

use ferrite_app::tracing;
use std::sync::Arc;
use std::time::Duration;
use ferrite_app::rt;
use ferrite_app::serde_json;
use ferrite_app::AppContext;
use crate::agents::tools::CloudToolRegistry;
use super::{A2A_LEASE_RENEW_MS, A2A_MAX_LEASE_RENEWALS, A2A_TURN_LEASE_MS, A2A_TURN_MARKER_TTL_MS, A2A_TURN_MAX_ATTEMPTS, CLAIM_ROUNDS, FABRIC_A2A_EGRESS, FABRIC_A2A_TOKEN_SECRET, MEMORY_STORE, STAGE_BACKOFF_MS, STAGE_ROUNDS, TurnDrive, TurnRequest, WS_CONTENT, WS_INDEX, absorbed_drive, superseded_drive};
use super::llm_backend::{LlmTransport};
use super::execution::{DuplicatePolicy, TurnFence, clean_model_id, fabric_a2a_endpoint, run_fenced_turn, run_turn, turn_finalized};
use super::fabric_context::{run_fabric_context_turn};
use super::delivery::{ANSWER_POST_ATTEMPTS, DeliverError, clamp_channel_reply, deliver_answer};

/// How many lease renewals an in-process turn may make (an hour at the renewal
/// period): a local turn's loop can far outlast one A2A send.
pub(super) const LOCAL_MAX_LEASE_RENEWALS: u64 = 3_600_000 / A2A_LEASE_RENEW_MS;

/// An in-process turn with the same durability as an A2A one.
///
/// * A LEASE marker (`local/turn/{agent}/{thread}/{msg_id}`) is claimed before
///   anything runs, so admission is not mistaken for completion: a
///   driver that crashes lets its lease lapse and a redelivery reclaims it.
///   The reclaiming attempt re-runs the loop with [`DuplicatePolicy::RunAnyway`]
///   (the message is admitted once; the loop resumes over the transcript).
///   EVERY attempt does, the first included: a finalized session entry proves
///   only that the answer was GENERATED, not that it was delivered (a crash
///   between finalizing and staging, or a marker that lapsed before
///   recovery, leaves it undelivered). Replaying a finalized turn returns its
///   recorded answer, and its delivery repeats the identical correlation-pinned
///   write, which the relay absorbs if it already posted.
/// * The final answer is STAGED under the marker before it is posted; a post
///   that fails leaves the delivery unacked, and the reclaiming driver posts the
///   staged text in its staged form instead of generating a new answer.
/// * A redelivery for a live lease, or a contended one, is not acked.
pub(super) async fn run_local_turn(
    ctx: &AppContext,
    llm: &Arc<dyn LlmTransport>,
    request: &TurnRequest,
    mut markers: ferrite_app::client::StoreClusterClient,
) -> TurnDrive {
    let marker_key = format!("local/turn/{}/{}/{}", request.agent, request.thread, request.msg_id);
    // Whether the model already finished this turn (its answer is recorded in
    // the session log). A finished turn is never abandoned for the generation
    // budget: a reclaim only replays the recorded answer.
    let generation_done =
        match turn_finalized(ctx, &request.agent, &request.thread, &request.msg_id).await {
            Ok(done) => done,
            Err(e) => {
                tracing::warn!("turn {marker_key} could not read its session log; left for redelivery: {e}");
                return TurnDrive::Retry;
            }
        };
    let (attempt, staged) =
        match claim_a2a_turn(&mut markers, &marker_key, ctx.now_ms(), generation_done).await {
        Ok(TurnClaim::Claimed { attempt, pending }) => (attempt, pending),
        // Abandonment is durable but NEVER acked for an in-process turn: the last
        // generation attempt's driver may still finish and record its answer at
        // any moment, and then crash before re-taking its marker. Leaving the
        // delivery unacked keeps a trigger that re-opens the turn once the
        // session log records an answer (`generation_done`); the queue's own
        // delivery budget bounds how long it is kept. No model call is spent.
        Ok(TurnClaim::Absorbed { reason: "abandoned" }) => {
            tracing::warn!("turn {marker_key} is abandoned; its delivery is kept until the queue's budget ends");
            return TurnDrive::Retry;
        }
        Ok(TurnClaim::Absorbed { reason }) => return absorbed_drive(reason),
        Err(e) => {
            tracing::warn!("turn {marker_key} could not claim its marker; left for redelivery: {e}");
            return TurnDrive::Retry;
        }
    };
    let lease = LeaseRenewer::start_with(ctx, marker_key.clone(), attempt, LOCAL_MAX_LEASE_RENEWALS);
    let channel = request.thread.strip_prefix("buzz-").filter(|c| !c.is_empty());
    let registry = CloudToolRegistry::new(
        ctx,
        WS_CONTENT,
        WS_INDEX,
        format!("{}:{}", request.agent, request.thread),
    )
    .with_buzz(&request.agent, &request.msg_id)
    .with_requester(request.requester.as_deref())
    // The delivery's own write is fenced too, checked at the moment of the write.
    .with_turn_fence(Some(TurnFence::new(marker_key.clone(), attempt)));

    let mut pending = match staged {
        // An earlier attempt generated this answer and did not confirm it.
        Some(pending) => pending,
        None => {
            // Never Skip on a duplicate admission: generation and delivery are
            // separate facts, and only the marker's answered state records
            // delivery. A finalized turn replays its recorded answer here.
            let policy = DuplicatePolicy::RunAnyway;
            let model = request.model.as_deref().and_then(clean_model_id);
            let result = run_fenced_turn(
                ctx,
                llm,
                &request.agent,
                &request.thread,
                &request.msg_id,
                model.as_deref(),
                request.message.as_bytes(),
                policy,
                TurnFence::new(marker_key.clone(), attempt),
                request.requester.as_deref(),
            )
            .await;
            // Lease lost mid-turn, or the tool posts are unknown: no post, no ack.
            if result.retry {
                drop(lease);
                tracing::warn!("turn {marker_key} attempt {attempt} left for redelivery: {}", String::from_utf8_lossy(&result.answer));
                return match markers.get(marker_key.as_bytes()).await {
                    Ok(raw) => superseded_drive(raw.as_deref()),
                    Err(_) => TurnDrive::Retry,
                };
            }
            // Nothing for this pipeline to post: a pre-marker admission, a turn
            // whose own tools already answered, or a thread with no channel.
            if result.duplicate || result.buzz_posts > 0 || channel.is_none() {
                drop(lease);
                return finished_drive(&mut markers, &marker_key, attempt, ctx.now_ms()).await;
            }
            let answer = String::from_utf8_lossy(&result.answer).trim().to_string();
            let text = if answer.is_empty() {
                "I ran the turn but produced no answer.".to_string()
            } else {
                answer
            };
            let pending = PendingAnswer { text: clamp_channel_reply(&text), top_level: false, uncertain: false };
            if !stage_pending(&mut markers, &marker_key, attempt, ctx.now_ms(), &pending).await {
                tracing::warn!("turn {marker_key} attempt {attempt} lost its lease before staging its answer");
                return match markers.get(marker_key.as_bytes()).await {
                    Ok(raw) => superseded_drive(raw.as_deref()),
                    Err(_) => TurnDrive::Retry,
                };
            }
            pending
        }
    };
    let Some(channel) = channel else {
        drop(lease);
        return finished_drive(&mut markers, &marker_key, attempt, ctx.now_ms()).await;
    };
    match deliver_answer(
        &registry,
        &mut markers,
        &marker_key,
        attempt,
        || ctx.now_ms(),
        channel,
        &request.msg_id,
        &mut pending,
    )
    .await
    {
        Ok(()) => {
            drop(lease);
            finished_drive(&mut markers, &marker_key, attempt, ctx.now_ms()).await
        }
        Err(DeliverError::Superseded) => {
            drop(lease);
            tracing::warn!("turn {marker_key} attempt {attempt} lost its lease before posting; staying silent");
            match markers.get(marker_key.as_bytes()).await {
                Ok(raw) => superseded_drive(raw.as_deref()),
                Err(_) => TurnDrive::Retry,
            }
        }
        Err(e) => {
            tracing::error!(
                "a turn's answer could not be delivered to the channel after \
                 {ANSWER_POST_ATTEMPTS} attempts; it stays staged and the delivery is left \
                 for redelivery (marker={marker_key}, attempt={attempt}): {e}"
            );
            TurnDrive::Retry
        }
    }
}


/// One trigger whose persona runs on an A2A harness. The shape mirrors the
/// in-process arm's guarantees with the pieces an external peer forces us to
/// relocate:
/// * REDELIVERY: no session admission runs here, so the absorber is a
///   create-if-absent LEASE marker in the app's `agent-memory` store — a
///   redelivered trigger for a turn still in flight, or already answered,
///   ends as a duplicate, exactly like [`DuplicatePolicy::Skip`].
/// * ANSWERING: an A2A peer has no buzz tools, so THIS pipeline posts the
///   final text — or a legible refusal — through the persona surface, signed
///   by the relay-held persona key and subject to its gates like any client
///   write. Once the marker is claimed, silence is never an outcome.
/// * BOUNDS: the peer turn carries one absolute wall-clock deadline
///   ([`crate::agents::tools::FABRIC_A2A_WALL_MS`], send retries and the
///   single fresh-context retry included) and the posted reply is capped at
///   [`A2A_REPLY_MAX_BYTES`].
pub(super) async fn run_a2a_turn(ctx: &AppContext, request: &TurnRequest) -> TurnDrive {
    let agent = request.agent.as_str();
    let harness = request.harness.trim();
    let thread = request.thread.as_str();
    let msg_id = request.msg_id.as_str();
    let mut markers = match ctx.store(MEMORY_STORE) {
        Ok(store) => store,
        Err(e) => {
            tracing::warn!("a2a turn has no marker store; left for redelivery: {e}");
            return TurnDrive::Retry;
        }
    };
    // The redelivery absorber, claimed BEFORE any work. It is a LEASE, not a
    // tombstone — see `A2A_TURN_LEASE_MS` for why, and what a reclaim costs.
    let marker_key = format!("a2a/turn/{agent}/{thread}/{msg_id}");
    let (attempt, staged) = match claim_a2a_turn(&mut markers, &marker_key, ctx.now_ms(), false).await {
        Ok(TurnClaim::Claimed { attempt, pending }) => (attempt, pending),
        Ok(TurnClaim::Absorbed { reason }) => {
            tracing::info!("a2a turn {marker_key} absorbed: {reason}");
            return absorbed_drive(reason);
        }
        Err(e) => {
            tracing::warn!("a2a turn {marker_key} could not claim its marker; left for redelivery: {e}");
            return TurnDrive::Retry;
        }
    };
    if attempt > 1 {
        tracing::warn!(
            "a2a turn {marker_key} is attempt {attempt}: a previous driver stopped renewing \
             its lease without answering, so this redelivery reclaimed the trigger"
        );
    }
    // Hold the lease for as long as this driver is actually working. Dropped
    // on every return path below.
    let _lease = LeaseRenewer::start(ctx, marker_key.clone(), attempt);
    // The channel is recovered from the thread convention (`buzz-{channel}`).
    // A thread outside it names no channel to answer into — the one refusal
    // that CANNOT be made legible in a channel.
    let Some(channel) = thread.strip_prefix("buzz-").filter(|c| !c.is_empty()) else {
        tracing::warn!(
            "a2a turn {marker_key} names no buzz-{{channel}} thread; dropped"
        );
        // Acked only once the marker durably says this turn is over.
        drop(_lease);
        return finished_drive(&mut markers, &marker_key, attempt, ctx.now_ms()).await;
    };
    let ws_scope = format!("{agent}:{thread}");
    let mut registry = CloudToolRegistry::new(ctx, WS_CONTENT, WS_INDEX, ws_scope)
        .with_buzz(agent, msg_id)
        .with_requester(request.requester.as_deref())
        .with_turn_fence(Some(TurnFence::new(marker_key.clone(), attempt)));
    let fabric_token = ctx
        .secret(FABRIC_A2A_TOKEN_SECRET)
        .map(|token| token.trim().to_string())
        .filter(|token| !token.is_empty());
    let fabric_bound = ctx.payload().services.contains_key(FABRIC_A2A_EGRESS);
    let fabric_ready = fabric_bound && fabric_token.is_some();
    if let Some(token) = fabric_token {
        if fabric_bound {
            registry =
                registry.with_fabric_a2a(FABRIC_A2A_EGRESS, fabric_a2a_endpoint(ctx).await, token);
        }
    }
    // Run the peer turn — or, when this deployment cannot, say why. Every arm
    // funnels into ONE posted message so the channel never gets silence. A
    // reclaimed turn with a STAGED answer does not run the peer again: it posts
    // exactly what the earlier attempt generated, in the form it settled on.
    let (answered, mut pending) = if let Some(pending) = staged {
        (true, pending)
    } else {
        let turn: std::result::Result<String, String> = if !matches!(harness, "a2a" | "fabric") {
            Err(format!(
                "persona '{agent}' names harness '{harness}', which this deployment does not run (known: ferrite-cloud, a2a, fabric)"
            ))
        } else if !fabric_ready {
            Err("no A2A backend is configured here (the fabric-a2a-egress binding and the fabric-a2a-client-token secret are both required)".to_string())
        } else {
            match run_fabric_context_turn(ctx, &registry, agent, thread, &request.message).await {
                Ok(turn) => turn,
                // Nothing was sent: the context generation is unknown. The lease
                // lapses and a redelivery retries, instead of a passing store
                // fault being posted as the answer or redialing a retired context.
                Err(reason) => {
                    tracing::warn!("a2a turn {marker_key} attempt {attempt} left for redelivery: {reason}");
                    return TurnDrive::Retry;
                }
            }
        };
        let (answered, reply_text) = match turn {
            Ok(text) => (true, text),
            Err(reason) => (
                false,
                format!("I could not answer through the A2A backend: {reason}"),
            ),
        };
        let pending = PendingAnswer { text: clamp_channel_reply(&reply_text), top_level: false, uncertain: false };
        // A peer call can outlive the lease: if nobody newer took the turn (it
        // only lapsed, or a redelivery abandoned it), this attempt re-takes its
        // own marker before staging rather than discarding a finished answer.
        let _ = TurnFence::new(marker_key.clone(), attempt).holds(ctx).await;
        // Staged under the marker, and only while this attempt still owns it:
        // the same read that keeps a reclaimed lease from becoming a second voice.
        if !stage_pending(&mut markers, &marker_key, attempt, ctx.now_ms(), &pending).await {
            tracing::warn!(
                "a2a turn {marker_key} attempt {attempt} was superseded before it could post; \
                 staying silent so the current driver owns the answer"
            );
            // Acked only if the marker now positively says answered; a live or
            // unreadable marker keeps this delivery as the turn's recovery trigger.
            return match markers.get(marker_key.as_bytes()).await {
                Ok(raw) => superseded_drive(raw.as_deref()),
                Err(_) => TurnDrive::Retry,
            };
        }
        (answered, pending)
    };

    match deliver_answer(
        &registry,
        &mut markers,
        &marker_key,
        attempt,
        || ctx.now_ms(),
        channel,
        msg_id,
        &mut pending,
    )
    .await
    {
        Ok(()) => {
            // The channel has the answer (or the legible refusal): stop
            // renewing, then convert the lease into the tombstone a late
            // redelivery reads. AFTER the post, deliberately — the reverse
            // order trades a rare duplicate for a silence.
            drop(_lease);
            if !answered {
                tracing::warn!(
                    "a2a turn {marker_key} answered with a refusal (posted into the channel)"
                );
            }
            finished_drive(&mut markers, &marker_key, attempt, ctx.now_ms()).await
        }
        Err(DeliverError::Superseded) => {
            drop(_lease);
            tracing::warn!(
                "a2a turn {marker_key} attempt {attempt} lost its lease before posting; \
                 staying silent so the current driver owns the answer"
            );
            match markers.get(marker_key.as_bytes()).await {
                Ok(raw) => superseded_drive(raw.as_deref()),
                Err(_) => TurnDrive::Retry,
            }
        }
        Err(e) => {
            // NOT acked: the lease lapses (its renewer stops here) and a
            // redelivery reclaims the trigger and tries again, bounded by
            // A2A_TURN_MAX_ATTEMPTS. Acking made a relay outage a lost answer.
            tracing::error!(
                "an a2a turn produced an answer that could not be delivered to the channel \
                 after {ANSWER_POST_ATTEMPTS} attempts; leaving it for redelivery \
                 (marker={marker_key}, attempt={attempt}): {e}"
            );
            TurnDrive::Retry
        }
    }
}

/// What claiming an A2A trigger produced.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum TurnClaim {
    /// This driver owns the trigger for [`A2A_TURN_LEASE_MS`]. `attempt` is
    /// 1 for the first driver and increments on every reclaim; `pending` is an
    /// answer an earlier attempt staged but did not confirm posting.
    Claimed { attempt: u32, pending: Option<PendingAnswer> },
    /// Somebody else owns it, it is already answered, or it was abandoned.
    Absorbed { reason: &'static str },
}

/// The marker document's state, as stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MarkerState {
    /// A driver holds the trigger until `lease_until_ms`.
    Claimed { attempt: u32, lease_until_ms: u64 },
    /// The channel already has this turn's answer.
    Answered,
    /// Durably given up after `attempt`, the last generation attempt.
    Abandoned { attempt: u32 },
    /// Present but unreadable. Treated as owned by somebody with an unknown
    /// deadline — a marker whose semantics we cannot see must not be stolen.
    Opaque,
}

impl MarkerState {
    pub(super) fn parse(raw: &[u8]) -> Self {
        let Ok(value) = serde_json::from_slice::<serde_json::Value>(raw) else {
            return Self::Opaque;
        };
        match value.get("state").and_then(serde_json::Value::as_str) {
            Some("answered") => Self::Answered,
            Some("abandoned") => Self::Abandoned {
                attempt: value
                    .get("attempt")
                    .and_then(serde_json::Value::as_u64)
                    .and_then(|a| u32::try_from(a).ok())
                    .unwrap_or(u32::MAX),
            },
            Some("claimed") => {
                let attempt = value
                    .get("attempt")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(1);
                let lease_until_ms = value
                    .get("lease_until_ms")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0);
                Self::Claimed {
                    attempt: u32::try_from(attempt).unwrap_or(u32::MAX),
                    lease_until_ms,
                }
            }
            _ => Self::Opaque,
        }
    }
}

#[cfg(test)]
pub(super) fn claimed_marker(attempt: u32, now_ms: u64) -> Vec<u8> {
    leased_marker(None, attempt, now_ms)
}

/// An answer staged under its turn marker BEFORE it is posted, with the form the
/// post has settled on. A driver that reclaims the turn posts THIS text in THIS
/// form rather than generating a new answer, so a crash between generating and
/// confirming a post can never yield a second, different answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PendingAnswer {
    pub(super) text: String,
    pub(super) top_level: bool,
    /// A threaded write of this answer had an UNKNOWN outcome: it may have
    /// committed. From then on the form never switches to top-level, even on a
    /// refusal, because a refused retry does not prove the earlier write failed.
    pub(super) uncertain: bool,
}

pub(super) fn marker_pending(raw: &[u8]) -> Option<PendingAnswer> {
    let value = serde_json::from_slice::<serde_json::Value>(raw).ok()?;
    let pending = value.get("pending")?;
    Some(PendingAnswer {
        text: pending.get("text")?.as_str()?.to_string(),
        top_level: pending
            .get("top_level")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
        uncertain: pending
            .get("uncertain")
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false),
    })
}

pub(super) fn pending_json(pending: &PendingAnswer) -> serde_json::Value {
    serde_json::json!({
        "text": pending.text,
        "top_level": pending.top_level,
        "uncertain": pending.uncertain,
    })
}

/// A claimed marker for `attempt`, carrying any staged answer forward from the
/// marker it replaces (a reclaim, or a lease renewal).
pub(super) fn leased_marker(prior: Option<&[u8]>, attempt: u32, now_ms: u64) -> Vec<u8> {
    let mut doc = serde_json::json!({
        "state": "claimed",
        "attempt": attempt,
        "lease_until_ms": now_ms.saturating_add(A2A_TURN_LEASE_MS),
    });
    if let (Some(pending), Some(map)) = (prior.and_then(marker_pending), doc.as_object_mut()) {
        map.insert("pending".into(), pending_json(&pending));
    }
    doc.to_string().into_bytes()
}

/// `raw` with `pending` staged on it, everything else unchanged.
pub(super) fn staged_marker(raw: &[u8], pending: &PendingAnswer) -> Vec<u8> {
    let mut doc = serde_json::from_slice::<serde_json::Value>(raw)
        .ok()
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    if let Some(map) = doc.as_object_mut() {
        map.insert("pending".into(), pending_json(pending));
    }
    doc.to_string().into_bytes()
}

/// Stage `pending` under the marker, by CAS and only while `attempt` owns it.
/// A conflict is re-read and retried: the lease renewer rewrites the same
/// marker, and losing to a renewal must not discard an answer this attempt
/// still owns, and a transient store error is retried after a short backoff.
/// Gives up when ownership is definitively lost (the marker is gone, belongs to
/// another attempt, or its lease lapsed) or after [`STAGE_ROUNDS`] rounds.
pub(super) async fn stage_pending(
    markers: &mut ferrite_app::client::StoreClusterClient,
    marker_key: &str,
    attempt: u32,
    now_ms: u64,
    pending: &PendingAnswer,
) -> bool {
    for round in 0..STAGE_ROUNDS {
        let (raw, version) = match markers.get_versioned(marker_key.as_bytes()).await {
            Ok(Some(found)) => found,
            Ok(None) => return false,
            Err(_) => {
                if round + 1 < STAGE_ROUNDS {
                    rt::time::sleep(Duration::from_millis(STAGE_BACKOFF_MS)).await;
                }
                continue;
            }
        };
        if !marker_is_owned_by(Some(&raw), attempt, now_ms) {
            return false;
        }
        match markers
            .put_if_version_expiring(
                marker_key.as_bytes(),
                &staged_marker(&raw, pending),
                Some(version),
                now_ms.saturating_add(A2A_TURN_MARKER_TTL_MS),
            )
            .await
        {
            Ok(Ok(_)) => return true,
            Ok(Err(_)) => continue, // a renewal (or a reclaim) moved it: re-read
            Err(_) => {
                // The write may or may not have landed; the next round re-reads.
                if round + 1 < STAGE_ROUNDS {
                    rt::time::sleep(Duration::from_millis(STAGE_BACKOFF_MS)).await;
                }
                continue;
            }
        }
    }
    false
}

/// Take the lease on one A2A trigger, or report why this driver must stay
/// quiet. The whole decision is a compare-and-swap on one key, so two
/// drivers racing the same expired lease produce exactly one winner.
/// `generation_done` says the turn's answer already exists outside the marker
/// (a local turn's finalized session step), which exempts it from abandonment.
pub(super) async fn claim_a2a_turn(
    markers: &mut ferrite_app::client::StoreClusterClient,
    marker_key: &str,
    now_ms: u64,
    generation_done: bool,
) -> std::result::Result<TurnClaim, String> {
    let ttl_deadline = now_ms.saturating_add(A2A_TURN_MARKER_TTL_MS);
    for _ in 0..CLAIM_ROUNDS {
        let current = markers
            .get_versioned(marker_key.as_bytes())
            .await
            .map_err(|e| e.to_string())?;
        let (expected, attempt) = match decide_claim_with(
            current
                .as_ref()
                .map(|(raw, version)| (raw.as_slice(), *version)),
            now_ms,
            generation_done,
        ) {
            ClaimDecision::Absorb { reason } => return Ok(TurnClaim::Absorbed { reason }),
            // Out of generation attempts: the abandonment is made DURABLE before
            // the delivery is acked, by CAS on the lapsed claim it replaces.
            ClaimDecision::Abandon { attempt, version } => {
                match markers
                    .put_if_version_expiring(
                        marker_key.as_bytes(),
                        &abandoned_marker(attempt, now_ms),
                        Some(version),
                        ttl_deadline,
                    )
                    .await
                {
                    Ok(Ok(_)) => {
                        tracing::warn!(
                            "a2a turn {marker_key} has burned {A2A_TURN_MAX_ATTEMPTS} leases \
                             without an answer; abandoning it so the peer is not charged again \
                             (this mention goes unanswered)"
                        );
                        return Ok(TurnClaim::Absorbed { reason: "abandoned" });
                    }
                    Ok(Err(_)) => continue,
                    Err(e) => return Err(e.to_string()),
                }
            }
            ClaimDecision::Create { attempt } => (None, attempt),
            ClaimDecision::Reclaim { attempt, version } => (Some(version), attempt),
        };
        let prior = current.as_ref().map(|(raw, _)| raw.as_slice());
        let pending = prior.and_then(marker_pending);
        match markers
            .put_if_version_expiring(
                marker_key.as_bytes(),
                &leased_marker(prior, attempt, now_ms),
                expected,
                ttl_deadline,
            )
            .await
        {
            Ok(Ok(_)) => return Ok(TurnClaim::Claimed { attempt, pending }),
            Ok(Err(_)) => continue, // lost the race: re-read and re-decide
            Err(e) => return Err(e.to_string()),
        }
    }
    Ok(TurnClaim::Absorbed {
        reason: "contended",
    })
}

/// What the marker's current bytes say this driver may do — the whole lease
/// policy, as a pure function of the stored value and the clock.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ClaimDecision {
    /// Nothing there: create the lease with `expected = None`.
    Create { attempt: u32 },
    /// An expired lease: take it under the version it was observed at.
    Reclaim { attempt: u32, version: u64 },
    /// An expired lease on the last generation attempt with no answer anywhere:
    /// make it durably abandoned under the version it was observed at.
    Abandon { attempt: u32, version: u64 },
    /// Stay quiet, and why.
    Absorb { reason: &'static str },
}

/// The terminal marker for a turn abandoned after `attempt`.
pub(super) fn abandoned_marker(attempt: u32, now_ms: u64) -> Vec<u8> {
    serde_json::json!({ "state": "abandoned", "attempt": attempt, "at_ms": now_ms })
        .to_string()
        .into_bytes()
}

#[cfg(test)]
pub(super) fn decide_claim(current: Option<(&[u8], u64)>, now_ms: u64) -> ClaimDecision {
    decide_claim_with(current, now_ms, false)
}

pub(super) fn decide_claim_with(
    current: Option<(&[u8], u64)>,
    now_ms: u64,
    generation_done: bool,
) -> ClaimDecision {
    let Some((raw, version)) = current else {
        return ClaimDecision::Create { attempt: 1 };
    };
    match MarkerState::parse(raw) {
        MarkerState::Answered => ClaimDecision::Absorb { reason: "answered" },
        MarkerState::Opaque => ClaimDecision::Absorb { reason: "claimed" },
        // A finished answer the session log records is still delivered.
        MarkerState::Abandoned { attempt } if generation_done => ClaimDecision::Reclaim {
            attempt: attempt.saturating_add(1),
            version,
        },
        MarkerState::Abandoned { .. } => ClaimDecision::Absorb { reason: "abandoned" },
        MarkerState::Claimed {
            attempt,
            lease_until_ms,
        } => {
            if now_ms < lease_until_ms {
                ClaimDecision::Absorb {
                    reason: "in_flight",
                }
            } else if attempt >= A2A_TURN_MAX_ATTEMPTS
                && marker_pending(raw).is_none()
                && !generation_done
            {
                // Out of GENERATION attempts with no answer anywhere. A staged
                // answer, or one the session log already records, is never
                // abandoned here: acking it would drop an answer that exists
                // but was never delivered.
                ClaimDecision::Abandon { attempt, version }
            } else {
                ClaimDecision::Reclaim {
                    attempt: attempt.saturating_add(1),
                    version,
                }
            }
        }
    }
}

/// Keeps one driver's lease alive while it works. The renewal is a
/// compare-and-swap that only ever pushes `lease_until_ms` forward for the
/// SAME attempt; it stops on its own after [`A2A_MAX_LEASE_RENEWALS`], and
/// the handle aborts the task on drop.
pub(super) struct LeaseRenewer {
    pub(super) handle: rt::JoinHandle<()>,
}

impl LeaseRenewer {
    pub(super) fn start(ctx: &AppContext, marker_key: String, attempt: u32) -> Self {
        Self::start_with(ctx, marker_key, attempt, A2A_MAX_LEASE_RENEWALS)
    }

    pub(super) fn start_with(ctx: &AppContext, marker_key: String, attempt: u32, max_renewals: u64) -> Self {
        let ctx = ctx.clone();
        let handle = rt::spawn(async move {
            let Ok(mut markers) = ctx.store(MEMORY_STORE) else {
                return;
            };
            for _ in 0..max_renewals {
                rt::time::sleep(core::time::Duration::from_millis(A2A_LEASE_RENEW_MS)).await;
                if !renew_a2a_lease(&mut markers, &marker_key, attempt, ctx.now_ms()).await {
                    return;
                }
            }
            tracing::warn!(
                "a2a turn {marker_key} attempt {attempt} outran its renewal budget; the \
                 trigger is now reclaimable while this driver may still be running"
            );
        });
        Self { handle }
    }
}

impl Drop for LeaseRenewer {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

/// Push one lease forward. Returns whether this driver still holds it. A CAS
/// conflict is NOT a loss: this driver's own staging write (or a re-take)
/// rewrites the same marker, so the marker is re-read and the renewal retried
/// while this attempt still owns it; only confirmed ownership loss stops it.
pub(super) async fn renew_a2a_lease(
    markers: &mut ferrite_app::client::StoreClusterClient,
    marker_key: &str,
    attempt: u32,
    now_ms: u64,
) -> bool {
    for _ in 0..STAGE_ROUNDS {
        let current = match markers.get_versioned(marker_key.as_bytes()).await {
            Ok(Some(current)) => current,
            Ok(None) => return false,
            // A transient read failure must not surrender a live lease — the
            // next tick tries again, and the lease is three ticks long for this
            // reason.
            Err(_) => return true,
        };
        let (raw, version) = current;
        if !marker_is_owned_by(Some(&raw), attempt, now_ms) {
            return false;
        }
        match markers
            .put_if_version_expiring(
                marker_key.as_bytes(),
                &leased_marker(Some(&raw), attempt, now_ms),
                Some(version),
                now_ms.saturating_add(A2A_TURN_MARKER_TTL_MS),
            )
            .await
        {
            Ok(Ok(_)) => return true,
            Ok(Err(_)) => continue, // moved under us: re-read and re-check ownership
            Err(_) => return true,  // transient, as above
        }
    }
    // Still ours at every read, but every write raced: keep renewing next tick.
    true
}

/// Does `attempt` still hold the lease the marker describes?
pub(super) fn marker_is_owned_by(raw: Option<&[u8]>, attempt: u32, now_ms: u64) -> bool {
    match raw.map(MarkerState::parse) {
        Some(MarkerState::Claimed {
            attempt: holder,
            lease_until_ms,
        }) => holder == attempt && now_ms < lease_until_ms,
        _ => false,
    }
}


/// Convert the lease into the answered tombstone, so a redelivery after the
/// answer is absorbed instead of re-run — by CAS, and only while this attempt
/// still owns the marker. An unconditional write let a driver whose post
/// outlived its lease overwrite a newer driver's lease with a tombstone.
/// Best-effort: the answer is already in the channel.
pub(super) async fn finish_a2a_turn(
    markers: &mut ferrite_app::client::StoreClusterClient,
    marker_key: &str,
    attempt: u32,
    now_ms: u64,
) -> bool {
    let doc = serde_json::json!({ "state": "answered", "attempt": attempt, "at_ms": now_ms })
        .to_string()
        .into_bytes();
    let current = match markers.get_versioned(marker_key.as_bytes()).await {
        Ok(current) => current,
        Err(e) => {
            tracing::warn!("a2a turn {marker_key} answered but its marker could not be read ({e})");
            return false;
        }
    };
    let Some((raw, version)) = current else {
        tracing::warn!("a2a turn {marker_key} answered but its marker had expired");
        return false;
    };
    if !finish_owned(&raw, attempt) {
        tracing::warn!(
            "a2a turn {marker_key} attempt {attempt} answered after another attempt took the \
             marker; leaving that attempt's marker alone"
        );
        return false;
    }
    match markers
        .put_if_version_expiring(
            marker_key.as_bytes(),
            &doc,
            Some(version),
            now_ms.saturating_add(A2A_TURN_MARKER_TTL_MS),
        )
        .await
    {
        Ok(Ok(_)) => true,
        Ok(Err(_)) => {
            tracing::warn!("a2a turn {marker_key} answered but its marker changed underneath; left as is");
            false
        }
        Err(e) => {
            tracing::warn!(
                "a2a turn {marker_key} answered but its marker could not be finalized ({e}); the \
                 delivery stays unacked and a reclaim re-posts the staged answer"
            );
            false
        }
    }
}

/// The delivery's fate once this driver posted: acked only when the marker is
/// positively answered (our CAS, or a fresh read). Otherwise the trigger stays,
/// and a reclaim re-posts the SAME staged answer, which the post identity absorbs.
pub(super) async fn finished_drive(
    markers: &mut ferrite_app::client::StoreClusterClient,
    marker_key: &str,
    attempt: u32,
    now_ms: u64,
) -> TurnDrive {
    if finish_a2a_turn(markers, marker_key, attempt, now_ms).await {
        return TurnDrive::Settled;
    }
    match markers.get(marker_key.as_bytes()).await {
        Ok(raw) => superseded_drive(raw.as_deref()),
        Err(_) => TurnDrive::Retry,
    }
}

/// Whether the stored marker is still this attempt's claim (lease expiry aside:
/// a post that ran long is still this attempt's answer if nobody reclaimed).
pub(super) fn finish_owned(raw: &[u8], attempt: u32) -> bool {
    matches!(MarkerState::parse(raw), MarkerState::Claimed { attempt: held, .. } if held == attempt)
}


/// What a leased ACP prompt drive came to.
pub(crate) enum PromptDrive {
    /// The turn's answer, generated now or replayed from the session log.
    /// The caller stores it, then settles the lease.
    Answered { answer: Vec<u8>, lease: PromptLease },
    /// Another live driver holds this prompt's lease, or it is already
    /// answered: its stored row is the answer to wait for.
    Busy,
    /// This attempt could not finish (lease lost, a failure, or the attempt
    /// budget spent); the reason is for the log.
    Retry(String),
}

/// A held prompt lease, kept renewed until it is settled or dropped.
pub(crate) struct PromptLease {
    held: Option<(String, u32, LeaseRenewer)>,
}

impl PromptLease {
    /// Mark the prompt answered, once its row is stored. Best effort: an
    /// unsettled lease lapses and the next retry replays the recorded answer.
    pub(crate) async fn settle(self, ctx: &AppContext) {
        let Some((marker_key, attempt, _renewer)) = self.held else {
            return;
        };
        if let Ok(mut markers) = ctx.store(MEMORY_STORE) {
            let _ = finish_a2a_turn(&mut markers, &marker_key, attempt, ctx.now_ms()).await;
        }
    }
}

/// Drive one ACP prompt under the same lease a local turn uses.
///
/// Why: the prompt's admission in the session engine is not its completion.
/// With `DuplicatePolicy::Skip` and no lease, a driver that crashed after
/// admission (or whose answer row failed to write) left every retry waiting
/// on a winner that would never answer. Here a retry waits only while a live
/// driver renews the lease; once it lapses, the retry reclaims it and runs
/// with `RunAnyway`, which resumes the transcript or replays the answer the
/// crashed attempt already recorded, without a second model turn.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn drive_prompt(
    ctx: &AppContext,
    llm: &Arc<dyn LlmTransport>,
    agent: &str,
    thread: &str,
    msg_id: &str,
    message: &[u8],
    requester: Option<&str>,
) -> PromptDrive {
    let marker_key = format!("acp/turn/{agent}/{thread}/{msg_id}");
    let mut markers = match ctx.store(MEMORY_STORE) {
        Ok(markers) => markers,
        Err(e) => return PromptDrive::Retry(format!("the turn marker store is unavailable: {e}")),
    };
    let generation_done = match turn_finalized(ctx, agent, thread, msg_id).await {
        Ok(done) => done,
        Err(e) => return PromptDrive::Retry(format!("the session log could not be read: {e}")),
    };
    let attempt = match claim_a2a_turn(&mut markers, &marker_key, ctx.now_ms(), generation_done).await {
        Ok(TurnClaim::Claimed { attempt, .. }) => attempt,
        Ok(TurnClaim::Absorbed { reason: "abandoned" }) => {
            return PromptDrive::Retry("the prompt spent its attempts without an answer".into())
        }
        // Answered, yet the caller found no row (it expired before the
        // marker, or its write failed after the settle): replay the answer
        // the session log recorded. No lease is needed to read it back.
        Ok(TurnClaim::Absorbed { reason: "answered" }) if generation_done => {
            let result = run_turn(ctx, llm, agent, thread, msg_id, None, message, DuplicatePolicy::RunAnyway, requester).await;
            if result.retry || result.duplicate {
                return PromptDrive::Retry(String::from_utf8_lossy(&result.answer).to_string());
            }
            return PromptDrive::Answered { answer: result.answer, lease: PromptLease { held: None } };
        }
        Ok(TurnClaim::Absorbed { .. }) => return PromptDrive::Busy,
        Err(e) => return PromptDrive::Retry(format!("the turn marker could not be claimed: {e}")),
    };
    let renewer = LeaseRenewer::start_with(ctx, marker_key.clone(), attempt, LOCAL_MAX_LEASE_RENEWALS);
    let result = run_fenced_turn(
        ctx,
        llm,
        agent,
        thread,
        msg_id,
        None,
        message,
        DuplicatePolicy::RunAnyway,
        TurnFence::new(marker_key.clone(), attempt),
        requester,
    )
    .await;
    if result.retry || result.duplicate {
        return PromptDrive::Retry(String::from_utf8_lossy(&result.answer).to_string());
    }
    PromptDrive::Answered {
        answer: result.answer,
        lease: PromptLease { held: Some((marker_key, attempt, renewer)) },
    }
}
