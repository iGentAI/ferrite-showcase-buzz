//! Scheduled agent turns: waking a persona at a future instant.
//!
//! # The mechanism is the platform's, not ours
//!
//! [`ferrite_app::client::QueueClusterClient`] — the type `ctx.queue()` returns —
//! implements `publish_doc_at(doc, not_before_ms)`, the queue plane's delayed
//! delivery gate. So a scheduled wake is the SAME [`crate::agents::turns::TurnRequest`]
//! a mention publishes, gated until its due instant.
//!
//! That matters more than it sounds. There is no timer to own, no pump to extend,
//! and no clock to consult — a constrained app has none of those. A scheduled turn
//! arrives through the same consumer, past the same session admission, subject to
//! the same pause check, as a message from a person. Everything already proven
//! about a mention's turn is therefore true of a scheduled one for free.
//!
//! # The row is the authority, not the message
//!
//! Cancellation marks the row stopped. Every wake — queued, leased, redelivered or
//! racing — checks the row ([`classify`]) before causing provider work, so a
//! cancelled or advanced schedule's wake is acked and ignored. That is the whole
//! stop, and it needs nothing from the queue plane.
//!
//! Queue-level RECALL of a persisted wake id is an optimisation on top: it retires
//! the message instead of letting it arrive only to be ignored. It is compiled only
//! with the `platform-queue-cancel` feature, because the released platform builder
//! has no `QueueClusterClient::cancel` yet (recorded platform gap 03, queue message cancel);
//! without it the receipts are still recorded and retired, and the row does the work.
//! This also covers a process stopping between publishing a wake and saving its receipt.
//!
//! Recurrence works the same way round. `remaining` lives in the ROW and is
//! decremented per occurrence, so a persona rescheduling itself cannot run away —
//! the bound is not carried in a payload the persona influences.
//!
//! # Nothing is drawn from entropy
//!
//! This app has none. A schedule's id is a digest over its own definition, which
//! makes creating the same schedule twice idempotent rather than doubling the
//! spend. An occurrence's admission id is a digest over `(schedule, index)`, and
//! that is what lets the session admission absorb a redelivered wake into exactly
//! one turn — the property [`crate::agents::turns`] already relies on for mentions.

use ferrite_app::client::StoreClusterClient;
use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::tracing;
use ferrite_app::AppContext;

use crate::relay::crypto::sha256::sha256;
use crate::relay::crypto::to_hex;
use crate::relay::error::{BuzzError, Result};
use crate::relay::store;

/// The shortest interval a recurring schedule may use.
///
/// A minute. Anything faster is a spend loop wearing a schedule's clothes: each
/// occurrence is a model call, and a ten-second recurrence is 8,640 calls a day
/// that nobody asked for twice.
pub const MIN_INTERVAL_MS: u64 = 60_000;

/// The furthest ahead a schedule may be set.
///
/// A year. Not a technical limit — the queue would hold it — but a wake five years
/// out will fire into a workspace whose channel, persona and purpose are all gone,
/// and the honest answer to that request is to refuse it now rather than surprise
/// somebody later.
pub const MAX_HORIZON_MS: u64 = 365 * 24 * 60 * 60 * 1000;

/// The most occurrences one schedule may ever run.
pub const MAX_OCCURRENCES: u32 = 1000;

/// The most live schedules one persona may hold.
///
/// Bounded because pending schedules are future spend that no single row reveals:
/// thirty-two rows each recurring hourly is a bill nobody reads until it arrives.
pub const MAX_PER_AGENT: usize = 32;

/// The longest instruction a schedule may carry.
pub const MAX_PROMPT_BYTES: usize = 4096;

/// One scheduled wake, live until cancelled or exhausted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Schedule {
    /// Derived from the definition, so the same request twice is one schedule.
    pub id: String,
    /// The persona to wake.
    pub agent: String,
    /// Where its answer goes.
    pub channel: String,
    /// What it is asked to do.
    pub prompt: String,
    /// When the NEXT occurrence is due (epoch ms).
    pub next_due_ms: u64,
    /// `None` for a one-shot.
    pub interval_ms: Option<u64>,
    /// Occurrences left, including the one that is due. Decremented in the ROW.
    pub remaining: u32,
    /// Which occurrence the next wake is, counting from zero. Half of the
    /// admission id, so it must advance even when an occurrence is skipped.
    pub occurrence: u32,
    /// Who asked for it — a member's pubkey, or the persona's own id when an
    /// agent scheduled itself. Recorded so a cancel can be authorized against
    /// something.
    pub created_by: String,
    pub created_ms: u64,
    /// Set instead of deleting, because a wake already in the queue has to find
    /// a row that says stop. Deleting the row would make the wake look like a
    /// schedule that never existed, which is a different and less useful answer.
    #[serde(default)]
    pub cancelled: bool,
    /// Which incarnation of this id the row is: 0 as first created, raised by one
    /// each time a cancelled schedule is re-created. Wakes carry it and are fenced
    /// to it; rows and wakes written before it existed read as 0, which is exactly
    /// what they were.
    #[serde(default)]
    pub revision: u32,
}

/// Prefix distinguishing a persona creator (`agent:<id>`) from a human member
/// pubkey (`<64-hex>`) in [`Schedule::created_by`].
pub const AGENT_CREATOR_PREFIX: &str = "agent:";

/// The typed principal behind a [`Schedule::created_by`] string.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScheduleCreator<'a> {
    Member(crate::relay::event::PubKey),
    Agent(&'a str),
}

/// Encode a human member's pubkey for [`ScheduleRequest::created_by`].
#[must_use]
pub fn member_creator(pubkey: &crate::relay::event::PubKey) -> String {
    to_hex(pubkey)
}

/// Encode a persona's agent id for [`ScheduleRequest::created_by`].
#[must_use]
pub fn agent_creator(agent: &str) -> String {
    format!("{AGENT_CREATOR_PREFIX}{agent}")
}

/// Parse a stored or supplied creator principal without allowing persona IDs and
/// 64-hex member pubkeys to impersonate one another.
#[must_use]
pub fn parse_creator(raw: &str) -> ScheduleCreator<'_> {
    if let Some(agent) = raw.strip_prefix(AGENT_CREATOR_PREFIX) {
        ScheduleCreator::Agent(agent)
    } else if let Some(pk) = crate::relay::crypto::from_hex32(raw) {
        ScheduleCreator::Member(pk)
    } else {
        ScheduleCreator::Agent(raw)
    }
}

impl Schedule {
    /// Whether this schedule still has future occurrences to run.
    #[must_use]
    pub const fn is_live(&self) -> bool {
        !self.cancelled && self.remaining > 0
    }

    /// The typed principal that created this schedule.
    #[must_use]
    pub fn creator(&self) -> ScheduleCreator<'_> {
        parse_creator(&self.created_by)
    }

    /// Whether human member `pubkey` created this schedule.
    #[must_use]
    pub fn is_created_by_member(&self, pubkey: &crate::relay::event::PubKey) -> bool {
        self.creator() == ScheduleCreator::Member(*pubkey)
    }

    /// Whether persona `agent` created this schedule.
    #[must_use]
    pub fn is_created_by_agent(&self, agent: &str) -> bool {
        self.creator() == ScheduleCreator::Agent(agent)
    }
}

/// `agentsched/{c}/{id}` — one schedule. The id is a digest, so it carries no
/// caller text into the keyspace.
#[must_use]
pub fn schedule_key(community: &str, id: &str) -> String {
    format!("agentsched/{community}/{id}")
}

/// The prefix every one of a community's schedules lives under.
#[must_use]
pub fn schedule_prefix(community: &str) -> String {
    format!("agentsched/{community}/")
}

/// A schedule's id: a digest over what it IS, and who owns it.
///
/// Deterministic on purpose. Asking twice for "remind the channel at 9am" is one
/// schedule, not two, and idempotence here is what makes a retried request safe —
/// the alternative is a caller who cannot tell whether their first attempt landed
/// and doubles the spend finding out.
///
/// `occurrences` and `created_by` are part of the identity, and both for reasons
/// that bite otherwise. Without the count, "run this once" and "run this a hundred
/// times" are the same row, so one caller silently inherits the other's recurrence
/// bound. Without the creator, two people asking for the same reminder share a row
/// whose `created_by` decides who may cancel it — so the second writer would take
/// authority over the first one's schedule.
#[must_use]
#[expect(
    clippy::too_many_arguments,
    reason = "public deterministic hash preimage keeps every identity component explicit"
)]
pub fn schedule_id(
    community: &str,
    agent: &str,
    channel: &str,
    prompt: &str,
    created_by: &str,
    first_due_ms: u64,
    interval_ms: Option<u64>,
    occurrences: u32,
) -> String {
    // Length-prefixed rather than delimiter-joined: a channel named `a/b` and an
    // agent named `a` with channel `b` must not digest to the same schedule.
    let mut material = Vec::new();
    for part in [community, agent, channel, prompt, created_by] {
        material.extend_from_slice(&(part.len() as u64).to_be_bytes());
        material.extend_from_slice(part.as_bytes());
    }
    material.extend_from_slice(&first_due_ms.to_be_bytes());
    material.extend_from_slice(&interval_ms.unwrap_or(0).to_be_bytes());
    material.extend_from_slice(&occurrences.to_be_bytes());
    to_hex(&sha256(&material))
}

/// The admission id for one occurrence — the `msg_id` its `TurnRequest` carries.
///
/// The turn pipeline dedups on `msg_id` (for a mention it is the trigger event's
/// id), so this is what makes a REDELIVERED wake absorb into exactly one turn
/// rather than answering twice. Distinct per occurrence, because a recurring
/// schedule's second run is a different turn and must not be absorbed into the
/// first.
///
/// And distinct per REVISION: reviving a cancelled schedule restarts its
/// occurrences at zero, and without the revision the revived runs would reuse the
/// earlier runs' admission ids and be absorbed as already performed. Revision 0 keeps the
/// original format, so a wake queued before revisions existed and a wake
/// re-published for the same occurrence now still share one admission and cannot
/// both run.
#[must_use]
pub fn occurrence_admission(schedule_id: &str, revision: u32, occurrence: u32) -> String {
    let material = if revision == 0 {
        format!("buzz-schedule-occurrence:{schedule_id}:{occurrence}")
    } else {
        format!("buzz-schedule-occurrence:{schedule_id}:r{revision}:{occurrence}")
    };
    to_hex(&sha256(material.as_bytes()))
}

/// What a caller asked for, before it becomes a [`Schedule`].
#[derive(Debug, Clone)]
pub struct ScheduleRequest {
    pub agent: String,
    pub channel: String,
    pub prompt: String,
    /// When the first occurrence should run (epoch ms).
    pub first_due_ms: u64,
    pub interval_ms: Option<u64>,
    /// Occurrences to run. Clamped, and meaningless above 1 for a one-shot.
    pub occurrences: u32,
    pub created_by: String,
}

/// Validate a request and shape the row, or refuse by name.
///
/// Every refusal names the field and the bound, because the caller is often a
/// MODEL: "invalid schedule" gives it nothing to correct, while "interval_ms must
/// be at least 60000" gives it the next action.
pub fn build(request: &ScheduleRequest, community: &str, now_ms: u64) -> Result<Schedule> {
    if request.agent.is_empty() {
        return Err(BuzzError::InvalidEvent("a schedule needs an agent".into()));
    }
    if request.channel.is_empty() {
        return Err(BuzzError::InvalidEvent(
            "a schedule needs a channel to answer in".into(),
        ));
    }
    let prompt = request.prompt.trim();
    if prompt.is_empty() {
        return Err(BuzzError::InvalidEvent(
            "a schedule needs an instruction; waking an agent with nothing to do \
             spends a model call to produce silence"
                .into(),
        ));
    }
    if prompt.len() > MAX_PROMPT_BYTES {
        return Err(BuzzError::InvalidEvent(format!(
            "a schedule's instruction is at most {MAX_PROMPT_BYTES} bytes, got {}",
            prompt.len()
        )));
    }
    // In the PAST is accepted and normalised to now: a caller computing a due
    // time against a slightly different clock should not be refused, and "as soon
    // as possible" is a coherent request. Refusing it would push callers into
    // guessing a safe margin.
    let first_due_ms = request.first_due_ms.max(now_ms);
    if first_due_ms.saturating_sub(now_ms) > MAX_HORIZON_MS {
        return Err(BuzzError::InvalidEvent(format!(
            "a schedule may be set at most {} days ahead",
            MAX_HORIZON_MS / (24 * 60 * 60 * 1000)
        )));
    }
    if let Some(interval) = request.interval_ms {
        if interval < MIN_INTERVAL_MS {
            return Err(BuzzError::InvalidEvent(format!(
                "a recurring schedule's interval_ms must be at least \
                 {MIN_INTERVAL_MS} (one minute), got {interval}"
            )));
        }
    }
    // A one-shot runs once whatever was asked for; a recurrence is capped.
    let remaining = match request.interval_ms {
        None => 1,
        Some(_) => request.occurrences.clamp(1, MAX_OCCURRENCES),
    };

    let id = schedule_id(
        community,
        &request.agent,
        &request.channel,
        prompt,
        &request.created_by,
        // The REQUESTED due time, not the normalised one. Normalising first would
        // make a retry of an immediate or overdue request a different schedule every
        // time the clock moves, so a caller whose first answer was lost would double
        // the spend finding out — the exact case the deterministic id exists to prevent.
        request.first_due_ms,
        request.interval_ms,
        // The NORMALISED count, so a request for `u32::MAX` and one for the cap are
        // the same schedule — they describe the same thing after clamping, and two
        // rows for one behaviour would double the spend.
        remaining,
    );
    Ok(Schedule {
        id,
        agent: request.agent.clone(),
        channel: request.channel.clone(),
        prompt: prompt.to_string(),
        next_due_ms: first_due_ms,
        interval_ms: request.interval_ms,
        remaining,
        occurrence: 0,
        created_by: request.created_by.clone(),
        created_ms: now_ms,
        cancelled: false,
        revision: 0,
    })
}

/// The row after an occurrence has been driven, or `None` when it is finished.
///
/// Computed from the ROW, never from the message that woke it — which is what
/// bounds a self-rescheduling persona. `occurrence` advances even for the final
/// run, so an admission id is never reused if the row is later revived.
///
/// A due time is advanced by whole intervals past `now_ms`, so a wake delivered
/// late (a redeployment, a long backlog) does not queue a burst of catch-up runs.
/// One occurrence is one wake, and a schedule that slept through three of them
/// resumes rather than trying to make them up.
#[must_use]
pub fn advance(schedule: &Schedule, now_ms: u64) -> Option<Schedule> {
    let remaining = schedule.remaining.saturating_sub(1);
    let interval = schedule.interval_ms?;
    if remaining == 0 {
        return None;
    }
    // CHECKED, and CONSTANT TIME. Adding one interval per missed occurrence is
    // bounded by overflow but not by work: a minute interval against a far-future
    // `now_ms` is trillions of iterations. The number of
    // whole intervals to skip is a division. An overflow answers "exhausted", the
    // honest reading of a due time that cannot be represented.
    let first = schedule.next_due_ms.checked_add(interval)?;
    let next = if first > now_ms {
        first
    } else {
        let behind = now_ms - first;
        let steps = behind / interval + 1;
        first.checked_add(steps.checked_mul(interval)?)?
    };
    Some(Schedule {
        next_due_ms: next,
        remaining,
        occurrence: schedule.occurrence.saturating_add(1),
        ..schedule.clone()
    })
}

/// May `who` cancel this schedule?
///
/// Its creator, or a moderator/operator — decided by the caller, which already
/// knows the community's role sets. A persona's own schedule is created under the
/// persona's id, so an agent can cancel what it set without being able to touch
/// a person's.
#[must_use]
pub fn may_cancel(schedule: &Schedule, who: &str, privileged: bool) -> bool {
    privileged || schedule.creator() == parse_creator(who)
}

//
// Every pair of writes below is ordered so the bad outcome is a DUPLICATE rather
// than a loss, because the two are not symmetric here: a duplicate wake is
// absorbed by the turn pipeline's `msg_id` admission and costs nothing, while a
// lost wake is a schedule that silently never runs and that nobody can tell from
// one still waiting its turn.

/// The trigger text one wake carries.
///
/// The prompt ALONE is not enough, and the gap is not theoretical: measured on a
/// live deployment, a persona woken with a bare prompt GUESSED a channel id out of
/// the prompt's own text, `buzz_post` refused it as "no such channel", and the turn
/// spent its answer explaining that failure instead of doing the work.
///
/// A mention's trigger names the channel it happened in
/// ([`crate::relay::agents::render_trigger`]), so the persona always knows where it
/// is. A scheduled turn has no conversation around it — nothing named the room and
/// nothing to infer it from — so without this the model either guesses or every
/// prompt author has to hardcode the id and remember to.
///
/// Not part of the admission id, which is derived from `(schedule, occurrence)`, so
/// changing this wording breaks no dedup property: a wake queued by an older build
/// is still absorbed into exactly one turn.
#[must_use]
pub fn render_wake(schedule: &Schedule) -> String {
    format!(
        "[scheduled wake] A schedule is waking you now; nobody is necessarily \
         watching. Answer in channel `{}` — pass exactly that channel id to \
         buzz_post. Your instruction follows.\n\n{}",
        schedule.channel, schedule.prompt
    )
}

/// The queue publish for `schedule`'s CURRENT occurrence, gated until it is due.
///
/// This is the whole mechanism. `publish_doc_at` is the queue plane's delayed
/// delivery gate, so the platform holds the message and the app owns no timer.
/// What lands is the same [`TurnRequest`] a mention publishes, which is why a
/// scheduled turn inherits every property already proven about a mention's.
async fn publish_wake(
    ctx: &AppContext,
    community: &str,
    schedule: &Schedule,
    harness: &str,
) -> Result<()> {
    use ferrite_app::client::QueueDocuments;

    let mut turns = ctx.queue(crate::agents::turns::TURNS).map_err(|e| {
        BuzzError::Internal(format!(
            "this deployment has no {} queue bound, so nothing can be scheduled: {e}",
            crate::agents::turns::TURNS
        ))
    })?;
    let request = crate::agents::turns::TurnRequest {
        agent: schedule.agent.clone(),
        harness: harness.to_string(),
        // The admission key. Derived per occurrence, so a redelivered wake is
        // absorbed into one turn and a later occurrence is not absorbed into an
        // earlier one.
        msg_id: occurrence_admission(&schedule.id, schedule.revision, schedule.occurrence),
        thread: crate::agents::turns::thread_for_channel(&schedule.channel),
        message: render_wake(schedule),
        model: None,
        schedule: Some(crate::agents::turns::ScheduledWake {
            id: schedule.id.clone(),
            occurrence: schedule.occurrence,
            revision: schedule.revision,
        }),
        // A member's schedule runs with that member's authority; a persona's
        // own schedule carries no member.
        requester: match schedule.creator() {
            ScheduleCreator::Member(pubkey) => Some(to_hex(&pubkey)),
            ScheduleCreator::Agent(_) => None,
        },
    };
    let (group, resource, generation) = wake_queue_binding(ctx).ok_or_else(|| {
        BuzzError::Storage("scheduling requires a generation-bound turns queue".into())
    })?;
    let message_id = turns
        .publish_doc_at(&request, schedule.next_due_ms)
        .await
        .map_err(|e| BuzzError::Storage(format!("scheduling a wake: {e}")))?;
    // A receipt binds an ID to the exact allocated queue generation. Old IDs
    // must never be replayed against a recreated queue where they can be reused.
    {
        let receipt = WakeReceipt {
            message_id,
            group,
            resource,
            generation,
            occurrence: schedule.occurrence,
        };
        let key = wake_receipt_key(community, schedule, message_id);
        let mut config = ctx
            .store(store::CONFIG)
            .map_err(|e| BuzzError::Storage(e.to_string()))?;
        if let Err(error) = store::put_doc(&mut config, key.as_bytes(), &receipt).await {
            // Preserve the error even if rollback is already too late. The row's
            // Stop gate remains authoritative for a concurrently leased wake.
            let _ = recall_queued_wake(&mut turns, message_id).await;
            return Err(error);
        }
        if get(&mut config, community, &schedule.id)
            .await?
            .as_ref()
            .is_some_and(|live| publication_was_stopped(schedule, live))
        {
            recall_queued_wake(&mut turns, message_id)
                .await
                .map_err(|e| BuzzError::Storage(e.to_string()))?;
            store::delete_key(&mut config, key.as_bytes()).await?;
        }
    }
    Ok(())
}

/// Retire an already published wake at the queue plane, when the platform can.
///
/// An OPTIMISATION, never the stop: a wake that is not recalled still arrives,
/// meets [`classify`], finds its row cancelled or its occurrence advanced, and is
/// acked without running a turn. Recall only saves that delivery.
///
/// With the `platform-queue-cancel` feature this is `QueueClusterClient::cancel`
/// (a `TooLate` outcome keeps the row fence authoritative for an in-flight lease).
/// Without it the released platform builder has no such method
/// (recorded platform gap 03, queue message cancel), so this answers `Ok` having done
/// nothing, and the callers' receipt bookkeeping proceeds unchanged.
#[cfg(feature = "platform-queue-cancel")]
async fn recall_queued_wake(
    turns: &mut ferrite_app::client::QueueClusterClient,
    message_id: u64,
) -> std::io::Result<()> {
    turns.cancel(message_id).await.map(|_| ())
}

/// See the feature-enabled twin above: without platform queue cancellation the
/// row fence is the only stop, which is sufficient by construction.
#[cfg(not(feature = "platform-queue-cancel"))]
async fn recall_queued_wake(
    _turns: &mut ferrite_app::client::QueueClusterClient,
    _message_id: u64,
) -> std::io::Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
struct WakeReceipt {
    message_id: u64,
    group: String,
    resource: String,
    generation: u64,
    occurrence: u32,
}

/// The exact queue coordinates a wake receipt is bound to, so a recall is never
/// replayed against a recreated queue whose message ids can be reused.
#[cfg(feature = "platform-queue-cancel")]
fn wake_queue_binding(ctx: &AppContext) -> Option<(String, String, u64)> {
    let queue = ctx.queue(crate::agents::turns::TURNS).ok()?;
    let (group, resource, generation) = queue.resource_identity()?;
    Some((group.to_string(), resource.to_string(), generation))
}

/// Without platform queue cancellation there is nothing to replay: recall is a
/// no-op, so receipts need no generation binding (the released builder has no
/// `resource_identity` either). This keeps the "a turns queue is bound"
/// requirement and binds every receipt to one fixed, unbound coordinate. A
/// receipt written by a feature-enabled build never matches it and is retired
/// without a recall, which is harmless for the same reason.
#[cfg(not(feature = "platform-queue-cancel"))]
fn wake_queue_binding(ctx: &AppContext) -> Option<(String, String, u64)> {
    ctx.queue(crate::agents::turns::TURNS).ok()?;
    Some((String::new(), String::new(), 0))
}

fn wake_receipt_prefix(community: &str, schedule: &Schedule) -> String {
    format!(
        "agentwake/{community}/{}/{:020}/",
        schedule.id, schedule.created_ms
    )
}

fn wake_receipt_key(community: &str, schedule: &Schedule, message_id: u64) -> String {
    format!(
        "{}{:020}/{message_id:020}",
        wake_receipt_prefix(community, schedule),
        schedule.occurrence
    )
}

fn publication_was_stopped(published: &Schedule, live: &Schedule) -> bool {
    live.created_ms > published.created_ms
        || (live.created_ms == published.created_ms
            && (live.cancelled || live.occurrence > published.occurrence || live.remaining == 0))
}

fn receipt_matches_binding(receipt: &WakeReceipt, binding: &(String, String, u64)) -> bool {
    receipt.group == binding.0 && receipt.resource == binding.1 && receipt.generation == binding.2
}

async fn recall_wake_receipts(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    community: &str,
    schedule: &Schedule,
    occurrence: Option<u32>,
) -> Result<()> {
    let mut prefix = wake_receipt_prefix(community, schedule);
    if let Some(occurrence) = occurrence {
        prefix.push_str(&format!("{occurrence:020}/"));
    }
    let binding = wake_queue_binding(ctx);
    let mut turns = ctx
        .queue(crate::agents::turns::TURNS)
        .map_err(|e| BuzzError::Storage(e.to_string()))?;
    // Each successful page deletes its receipts; retries resume from the prefix.
    for _ in 0..8 {
        let (rows, more) =
            store::scan(config, prefix.as_bytes(), None, store::MAX_SCAN_PAGE).await?;
        for (key, value, _) in rows {
            let receipt: WakeReceipt = store::decode_doc(&key, &value)?;
            let binding = binding.as_ref().ok_or_else(|| {
                BuzzError::Storage(
                    "exact queue generation is unavailable; schedule is stopped, recall must retry"
                        .into(),
                )
            })?;
            if receipt_matches_binding(&receipt, binding) {
                // TooLate retains the row Stop fence; cancellation never pretends
                // that an in-flight lease was withdrawn. Without platform queue
                // cancellation this is a no-op and the fence is the whole stop.
                recall_queued_wake(&mut turns, receipt.message_id)
                    .await
                    .map_err(|e| BuzzError::Storage(e.to_string()))?;
            }
            store::delete_key(config, &key).await?;
        }
        if !more {
            return Ok(());
        }
    }
    Err(BuzzError::Conflict(
        "schedule stopped; retry to finish recalling queued wakes".into(),
    ))
}

/// One schedule, if it exists.
pub async fn get(
    config: &mut StoreClusterClient,
    community: &str,
    id: &str,
) -> Result<Option<Schedule>> {
    store::get_doc::<Schedule>(config, schedule_key(community, id).as_bytes()).await
}

/// Every schedule in the community, newest-created first.
///
/// Unbounded scans are the thing to avoid, so this walks at most
/// [`MAX_LISTED`] rows and reports whether it stopped early — a caller showing a
/// truncated list must be able to say so rather than imply it is complete.
pub async fn list(
    config: &mut StoreClusterClient,
    community: &str,
) -> Result<(Vec<Schedule>, bool)> {
    let prefix = schedule_prefix(community).into_bytes();
    let mut out = Vec::new();
    let mut start_after: Option<Vec<u8>> = None;
    let mut truncated = false;
    let mut exhausted = false;
    for _page in 0..COUNT_PAGE_BUDGET {
        let (rows, more) = store::scan(
            config,
            &prefix,
            start_after.as_deref(),
            store::MAX_SCAN_PAGE,
        )
        .await?;
        if rows.is_empty() {
            exhausted = true;
            break;
        }
        for (key, value, _version) in &rows {
            start_after = Some(key.clone());
            // One unreadable row must not cost the whole set, and dead
            // historical rows (cancelled or exhausted) must not crowd out live
            // schedules before sorting.
            match store::decode_doc::<Schedule>(key, value) {
                Ok(doc) if doc.is_live() => out.push(doc),
                Ok(_) => {}
                Err(e) => {
                    tracing::warn!("unreadable schedule row skipped: {e}");
                }
            }
        }
        if !more {
            exhausted = true;
            break;
        }
    }
    if !exhausted {
        truncated = true;
    }
    out.sort_by(|a, b| b.created_ms.cmp(&a.created_ms).then(a.id.cmp(&b.id)));
    if out.len() > MAX_LISTED {
        out.truncate(MAX_LISTED);
        truncated = true;
    }
    Ok((out, truncated))
}

/// The most schedules one list call will read.
pub const MAX_LISTED: usize = 256;

/// The most pages [`live_ids_for_agent`] will walk before it gives up.
///
/// Bounded work, and the give-up is a REFUSAL rather than an assumption: an
/// unprovable bound is not a satisfied bound.
const COUNT_PAGE_BUDGET: usize = 32;

/// How long a reservation whose schedule row never appeared still holds its slot.
///
/// A create reserves BEFORE it publishes and writes, so for a moment a reserved id
/// has no row. A create that then failed outright would hold its slot forever;
/// past this age an absent row is treated as abandoned.
pub const RESERVATION_GRACE_MS: u64 = 10 * 60 * 1000;

/// One reserved slot in an agent's [`AgentSlots`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Slot {
    pub id: String,
    pub at_ms: u64,
    /// The schedule revision this slot was reserved for. Release and the spend
    /// fence both match it, so a cancel of revision n cannot remove (or a wake of
    /// revision n run under) the slot a re-create took for revision n+1.
    #[serde(default)]
    pub revision: u32,
}

/// The ids holding an agent's live-schedule slots, written only by CAS.
///
/// The per-agent cap was a scan followed by an independent write, so concurrent
/// creates of different schedules each saw fewer than [`MAX_PER_AGENT`] and all
/// committed. One versioned document per agent makes the reservation atomic: two
/// creates contend on the same row, and the loser re-reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct AgentSlots {
    #[serde(default)]
    pub slots: Vec<Slot>,
}

/// `agentcap/{c}/{digest(agent)}` — outside [`schedule_prefix`], so the schedule
/// scans never see it, and digested so no caller text shapes the key.
#[must_use]
pub fn agent_slots_key(community: &str, agent: &str) -> String {
    format!("agentcap/{community}/{}", to_hex(&sha256(agent.as_bytes())))
}

/// What a slot's schedule row says about whether it still holds the slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SlotRow {
    Live,
    Finished,
    Absent,
}

/// The slots still held: live rows, and any other slot reserved within
/// [`RESERVATION_GRACE_MS`]. A fresh reservation whose row is absent is a create in
/// flight; one whose row is still cancelled is a REVIVAL in flight (a revival
/// refreshes its slot before it publishes). Pruning either let a concurrent create
/// take the capacity and both commit. Pure, so the rule is tested without a store.
#[must_use]
pub fn held_slots(slots: &[Slot], rows: &[SlotRow], now_ms: u64) -> Vec<Slot> {
    slots
        .iter()
        .zip(rows)
        .filter(|(slot, row)| {
            **row == SlotRow::Live || slot.at_ms.saturating_add(RESERVATION_GRACE_MS) > now_ms
        })
        .map(|(slot, _)| slot.clone())
        .collect()
}

/// The agent's live schedules from a scan, for seeding its first slot document,
/// as `(id, revision, created_ms)`. `None` when the scan cannot prove the set
/// (its page budget ran out). Rows that cannot be decoded are skipped: they can
/// never run.
async fn live_ids_for_agent(
    config: &mut StoreClusterClient,
    community: &str,
    agent: &str,
) -> Result<Option<Vec<(String, u32, u64)>>> {
    let prefix = schedule_prefix(community).into_bytes();
    let mut start_after: Option<Vec<u8>> = None;
    let mut ids = Vec::new();
    for _page in 0..COUNT_PAGE_BUDGET {
        let (rows, more) =
            store::scan(config, &prefix, start_after.as_deref(), store::MAX_SCAN_PAGE).await?;
        for (key, value, _version) in &rows {
            start_after = Some(key.clone());
            match store::decode_doc::<Schedule>(key, value) {
                Ok(doc) if doc.agent == agent && !doc.cancelled && doc.remaining > 0 => {
                    ids.push((doc.id, doc.revision, doc.created_ms));
                }
                Ok(_) => {}
                // A row that cannot be decoded can never be claimed (its wake reads
                // it the same way and fails), so it holds no slot: skipped, not a
                // reason to give up on everyone else's cap.
                Err(e) => {
                    tracing::warn!("an unreadable schedule row is skipped while seeding slots: {e}");
                }
            }
        }
        if rows.is_empty() || !more {
            return Ok(Some(ids));
        }
    }
    Ok(None)
}

/// The first slot record for an agent, from its pre-existing live schedules.
///
/// MIGRATION POLICY: at most [`MAX_PER_AGENT`] are admitted, oldest first. An
/// agent already over the cap (possible for schedules older than slot records)
/// keeps its oldest schedules; the excess holds no slot, so the spend fence stops
/// each of them, and marks it cancelled, the next time it wakes.
#[must_use]
pub fn seed_slots(mut live: Vec<(String, u32, u64)>, now_ms: u64) -> Vec<Slot> {
    live.sort_by(|a, b| a.2.cmp(&b.2).then_with(|| a.0.cmp(&b.0)));
    live.into_iter()
        .take(MAX_PER_AGENT)
        .map(|(id, revision, _)| Slot { id, at_ms: now_ms, revision })
        .collect()
}

/// Reserve one of `agent`'s [`MAX_PER_AGENT`] slots for schedule `id`.
///
/// `Ok(true)` when the slot is held (newly, or already by this id); `Ok(false)` when
/// the agent is at the cap with every slot genuinely held.
///
/// The document is the only admission to the cap. Its FIRST version is seeded with
/// the agent's existing live schedules and written conditionally on absence, so
/// schedules made before it existed count, and two creates cannot both seed it.
/// Every call RENEWS an existing slot's timestamp: a revival's slot is then not
/// pruned as finished while its row still reads cancelled, and a retried create's
/// old, prunable reservation is re-admitted rather than trusted.
async fn reserve_slot(
    config: &mut StoreClusterClient,
    community: &str,
    agent: &str,
    id: &str,
    revision: u32,
    now_ms: u64,
) -> Result<bool> {
    let key = agent_slots_key(community, agent);
    for _ in 0..8 {
        let (mut slots, version) =
            match store::get_doc_versioned::<AgentSlots>(config, key.as_bytes()).await? {
                Some((doc, version)) => (doc.slots, Some(version)),
                None => match live_ids_for_agent(config, community, agent).await? {
                    Some(live) => (seed_slots(live, now_ms), None),
                    // An unprovable bound is not a satisfied bound.
                    None => return Ok(false),
                },
            };
        if let Some(slot) = slots.iter_mut().find(|slot| slot.id == id) {
            // NEVER downgraded: a create delayed across a cancel and a newer
            // revival must not move the slot back to its older revision (the newer
            // revision's next wake would then fail the fence and be cancelled).
            // The stale create loses here, as it would at its row CAS anyway.
            if slot.revision > revision {
                return Err(BuzzError::Conflict(
                    "that schedule was re-created by another request; read it again".into(),
                ));
            }
            // Always RENEWED by CAS, never accepted as-is: an old reservation for an
            // id whose create failed may already be past its grace and prunable, and
            // accepting it without a write let a concurrent create take that
            // capacity while this one went on to commit. A re-create takes the slot
            // over for its new revision.
            slot.at_ms = now_ms;
            slot.revision = revision;
        } else {
            if slots.len() >= MAX_PER_AGENT {
                let mut rows = Vec::with_capacity(slots.len());
                for slot in &slots {
                    rows.push(match get(config, community, &slot.id).await? {
                        Some(row) if row.cancelled || row.remaining == 0 => SlotRow::Finished,
                        Some(_) => SlotRow::Live,
                        None => SlotRow::Absent,
                    });
                }
                slots = held_slots(&slots, &rows, now_ms);
                if slots.len() >= MAX_PER_AGENT {
                    return Ok(false);
                }
            }
            slots.push(Slot {
                id: id.to_string(),
                at_ms: now_ms,
                revision,
            });
        }
        // A lost race (or a lost reply to our own applied write) re-reads: the
        // next pass either finds this id already held or contends again.
        if store::put_doc_if_version(config, key.as_bytes(), &AgentSlots { slots }, version)
            .await?
            .is_ok()
        {
            return Ok(true);
        }
    }
    Err(BuzzError::Conflict(
        "too many schedules were being created for this agent at once; try again".into(),
    ))
}

fn at_cap_refusal(agent: &str) -> BuzzError {
    BuzzError::InvalidEvent(format!(
        "{agent} already holds the maximum of {MAX_PER_AGENT} live schedules; cancel one first"
    ))
}

/// Create a schedule and queue its first wake.
///
/// Idempotent by id, so a retried request is one schedule. The publish happens
/// EVEN when the row already exists, which looks redundant and is the
/// self-healing case: a schedule whose wake was lost to a crash between the two
/// writes would otherwise stay dead forever, and a duplicate wake is absorbed by
/// the admission.
pub async fn create(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    community: &str,
    request: &ScheduleRequest,
    harness: &str,
    now_ms: u64,
) -> Result<Schedule> {
    let mut schedule = build(request, community, now_ms)?;
    let key = schedule_key(community, &schedule.id);

    // Four cases. A cancelled row is `Some`, so a write conditional on absence never
    // revived it — the freshly queued wake was then skipped forever and the create
    // looked like it had worked; and an exhausted row is a retry, not a revival.
    let observed = store::get_doc_versioned::<Schedule>(config, key.as_bytes()).await?;
    let live = match observed {
        // Already live: nothing to write, and the wake below is the self-heal.
        Some((row, _version)) if !row.cancelled && row.remaining > 0 => row,
        // EXHAUSTED by its own runs, and not cancelled: the same request again is a
        // retry of something that already happened. The id is the request's own
        // identity, so reviving here would run a finished one-shot a second time
        // because its creator's first answer was lost.
        Some((row, _version)) if !row.cancelled => return Ok(row),
        // Cancelled: an explicit re-create, so revive it as a NEW incarnation, by CAS
        // so a concurrent change loses rather than being clobbered.
        Some((old_row, version)) => {
            schedule.created_ms = schedule
                .created_ms
                .max(old_row.created_ms.saturating_add(1));
            // CHECKED: a saturated revision would reuse the cancelled one's id space,
            // unfencing its old wakes and absorbing new runs as old ones.
            schedule.revision = old_row.revision.checked_add(1).ok_or_else(|| {
                BuzzError::Conflict("that schedule has been re-created too many times; create a new one".into())
            })?;
            // Recorded BEFORE anything is published, so a wake for this revision
            // can always learn how its revival ended (see [`revival_abandoned`]).
            mark_revival(config, community, &schedule.id, schedule.revision, now_ms, RevivalEvent::Started).await?;
            if !reserve_slot(config, community, &schedule.agent, &schedule.id, schedule.revision, now_ms).await? {
                mark_revival(config, community, &schedule.id, schedule.revision, now_ms, RevivalEvent::Failed).await?;
                return Err(at_cap_refusal(&schedule.agent));
            }
            publish_wake(ctx, community, &schedule, harness).await?;
            // Re-renewed before the commit, as on the create path below.
            if !reserve_slot(config, community, &schedule.agent, &schedule.id, schedule.revision, ctx.now_ms()).await? {
                mark_revival(config, community, &schedule.id, schedule.revision, ctx.now_ms(), RevivalEvent::Failed).await?;
                return Err(at_cap_refusal(&schedule.agent));
            }
            if store::put_doc_if_version(config, key.as_bytes(), &schedule, Some(version))
                .await?
                .is_err()
            {
                // Another identical revival may have committed this revision and
                // then been cancelled or run out, releasing its slot before this
                // delayed one re-reserved it: see [`reconcile_losing_write`].
                reconcile_losing_write(config, community, &schedule).await?;
                mark_revival(config, community, &schedule.id, schedule.revision, ctx.now_ms(), RevivalEvent::Failed).await?;
                return Err(BuzzError::Conflict(
                    "that schedule changed while being re-created; try again".into(),
                ));
            }
            // Published AGAIN after the commit. The wake published above may
            // already have been answered Stop: a revival stalled past the grace
            // window looks abandoned to [`claim`], which acks that wake. A wake
            // published after the row commits always finds it, so a revival that
            // commits late is never left without a delivery; when the first wake
            // still runs, the occurrence's admission absorbs this duplicate. A
            // failure here is returned so the caller retries, and a retry of the
            // same request takes the already-live path, which publishes again.
            publish_wake(ctx, community, &schedule, harness).await?;
            return Ok(schedule);
        }
        None => {
            if !reserve_slot(config, community, &schedule.agent, &schedule.id, schedule.revision, now_ms).await? {
                return Err(at_cap_refusal(&schedule.agent));
            }
            // PUBLISH FIRST. A crash before the row write leaves a wake that finds
            // no row; the reverse order leaves a row with no wake, which is
            // permanently dead and looks exactly like one still waiting.
            //
            // The wake finding no row is a REAL race, not just a crash window: a
            // wake due now is deliverable the instant it is published, so the
            // consumer can claim it before the row lands. That is why [`claim`]
            // reports it as pending and the consumer retries rather than acking.
            publish_wake(ctx, community, &schedule, harness).await?;
            // RE-RENEW the reservation just before committing. A create delayed past
            // the grace window (a slow publish, a stalled process) may have had its
            // slot reclaimed; renewing re-admits it against the cap or refuses, and
            // a fresh slot cannot be pruned in the moment until the insert below.
            if !reserve_slot(config, community, &schedule.agent, &schedule.id, schedule.revision, ctx.now_ms()).await? {
                return Err(at_cap_refusal(&schedule.agent));
            }
            // CONDITIONAL on absence. Two identical creates can both have seen no
            // row; an unconditional write by the slower one could land after the
            // faster one's row was cancelled or advanced, and undo either.
            return match store::put_doc_if_version(config, key.as_bytes(), &schedule, None)
                .await?
            {
                // Published AGAIN after the insert, as on the revival path: a create
                // that stalled long enough for its pre-commit wake to be retried away
                // (absence is Pending, and the queue's attempt budget is finite)
                // would otherwise commit a live row with no wake left. The
                // occurrence's admission absorbs the duplicate when the first wake
                // still runs; a failure is returned so the caller retries, and the
                // retry takes the already-live path, which publishes again.
                Ok(_) => {
                    publish_wake(ctx, community, &schedule, harness).await?;
                    Ok(schedule)
                }
                // Someone else's create won (or ours applied and its reply was
                // lost): the stored row is the answer, never overwritten. See
                // [`reconcile_losing_write`] for this create's own reservation.
                Err(_) => reconcile_losing_write(config, community, &schedule).await?.ok_or_else(|| {
                    BuzzError::Conflict("that schedule changed while being created; try again".into())
                }),
            };
        }
    };

    // The already-live path's self-heal: re-publishing costs nothing (the
    // admission absorbs a duplicate) and recovers a schedule whose only wake was
    // lost to a crash between the publish and the row write.
    publish_wake(ctx, community, &live, harness).await?;
    Ok(live)
}

/// Whether this row is live, due, and may therefore have no wake coming.
///
/// The state exists because a wake can be lost: the create publishes before writing
/// the row, so a wake due NOW could be claimed and — before [`claim`] learned to
/// report absence separately — acked away as "no such schedule". Nothing else ever
/// revisits a schedule, so such a row stays live, overdue, and inert forever.
///
/// Deliberately not narrowed to "overdue by more than N": a row due and about to be
/// delivered matches too, and that costs one duplicate the admission absorbs. Trying
/// to exclude it would mean guessing a margin, and guessing wrong in the other
/// direction leaves the row stranded, which is the failure worth avoiding.
#[must_use]
pub fn needs_republish(schedule: &Schedule, now_ms: u64) -> bool {
    !schedule.cancelled && schedule.remaining > 0 && schedule.next_due_ms <= now_ms
}

/// `agentsweep/{c}` — where the due-schedule sweep stopped last time.
///
/// Deliberately NOT under [`schedule_prefix`]: a cursor row inside the schedule
/// keyspace would be scanned as a schedule, fail to decode, and warn on every page —
/// a self-inflicted diagnostic that trains whoever reads the logs to ignore the one
/// warning that means something.
#[must_use]
fn sweep_cursor_key(community: &str) -> String {
    format!("agentsweep/{community}")
}

/// The most pages the due-schedule sweep will walk.
///
/// Generous, because the sweep runs once per boot and the alternative to reaching a
/// row is that the row never runs. At [`store::MAX_SCAN_PAGE`] rows a page this
/// covers far more schedules than the per-agent cap times a full persona roster.
const SWEEP_PAGE_BUDGET: usize = 64;

/// Re-publish the wake for every schedule that is live and due.
///
/// The self-heal for a stranded row, and the reason it has to exist HERE rather than
/// being an operator action: a schedule's due time is part of the digest its id is
/// derived from, and [`build`] normalises a past due time to now — so asking for the
/// same definition again computes a DIFFERENT id and creates a second schedule
/// instead of healing the first. Re-creating is genuinely the repair for a row whose
/// due time is still in the future, and no repair at all for one that is overdue,
/// which is precisely the state that needs it.
///
/// Its OWN walk rather than [`list`], which stops at [`MAX_LISTED`] rows in key
/// order. Republishing removes nothing and reorders nothing, so a `list`-based sweep
/// would select the same prefix on every boot and a row beyond it would be starved
/// forever — the exact failure this function exists to prevent.
///
/// The walk is bounded by a page budget AND RESUMES, and both halves are needed. A
/// budget alone is not an improvement on a fixed prefix: starting from the beginning
/// every time means a keyspace larger than the budget has the same rows walked
/// repeatedly and the remainder starved, just at a larger threshold. The cursor is
/// therefore durable — each pass starts where the last stopped, and reaching the end
/// clears it so the next pass begins again from the front. Work per boot stays
/// bounded and every row is reached within a bounded number of passes.
///
/// Safe to run on every boot. The occurrence's admission id is unchanged, so a wake
/// that IS still queued absorbs the duplicate into exactly one turn, and [`settle`]
/// is idempotent under its occurrence check. The ordinary case — a row due and about
/// to be delivered when the instance restarted — therefore costs one absorbed message.
///
/// A retired persona's rows are skipped rather than published: the wake would only be
/// refused on arrival, so publishing spends a delivery to reach a decision the
/// registry already implies.
///
/// Returns `(healed, complete)`. `complete` means this pass reached the END of the
/// keyspace, not that nothing was left stranded — a resumed pass that finishes covers
/// only the tail, and the front is covered by the pass after it.
pub async fn republish_due(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    community: &str,
    now_ms: u64,
) -> Result<(usize, bool)> {
    // The harness per agent, read BEFORE the walk so the registry lookup holds no
    // borrow across it. STRICT parse: a malformed registry must not read as "every
    // persona retired" and silently skip every row — the same distinction the wake
    // path makes, for the same reason.
    let Some(bytes) = store::get_raw(config, crate::relay::agents::REGISTRY_CONFIG).await? else {
        return Ok((0, true)); // no personas registered, so no wake could run
    };
    let harnesses: std::collections::BTreeMap<String, String> =
        crate::relay::agents::Registry::parse(&bytes)?
            .personas
            .into_iter()
            .map(|p| (p.agent, p.harness))
            .collect();

    let cursor_key = sweep_cursor_key(community);
    // Where the last pass stopped. Absent means start at the front, which is also
    // what a completed pass leaves behind.
    let mut start_after: Option<Vec<u8>> = store::get_raw(config, cursor_key.as_bytes()).await?;
    let prefix = schedule_prefix(community).into_bytes();
    let mut healed = 0usize;
    let mut complete = false;
    for _page in 0..SWEEP_PAGE_BUDGET {
        let (rows, more) = store::scan(
            config,
            &prefix,
            start_after.as_deref(),
            store::MAX_SCAN_PAGE,
        )
        .await?;
        if rows.is_empty() {
            complete = true;
            break;
        }
        let mut due: Vec<Schedule> = Vec::new();
        for (key, value, _version) in &rows {
            start_after = Some(key.clone());
            match store::decode_doc::<Schedule>(key, value) {
                Ok(row) if needs_republish(&row, now_ms) => due.push(row),
                Ok(_) => {}
                // One unreadable row must not cost every other row its repair.
                Err(e) => {
                    tracing::warn!("unreadable schedule row skipped by sweep: {e}");
                }
            }
        }
        // Published outside the row loop so nothing holds a borrow of `rows`.
        for row in due {
            let Some(harness) = harnesses.get(&row.agent) else {
                continue; // retired persona: the wake would only be refused
            };
            match publish_wake(ctx, community, &row, harness).await {
                Ok(()) => healed = healed.saturating_add(1),
                // The next pass retries this one; the rest still get their wake.
                Err(e) => tracing::warn!(
                    "schedule {} is due and its wake could not be re-published: {e}",
                    row.id
                ),
            }
        }
        if !more {
            complete = true;
            break;
        }
    }

    // Record where to resume, or clear it so the next pass starts from the front.
    // Best effort: losing the cursor costs a repeated pass over ground already
    // covered, which is wasted work rather than a wrong answer.
    if complete {
        let _ = store::delete_key(config, cursor_key.as_bytes()).await;
    } else if let Some(last) = start_after {
        let _ = store::put_raw(config, cursor_key.as_bytes(), &last).await;
    }
    Ok((healed, complete))
}

/// The durable record of the revivals attempted for one schedule revision:
/// `agentschedrev/{c}/{id}/{revision}`. Identical concurrent revivals share it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct RevivalRecord {
    /// Revivals that started (counted BEFORE their wake was published).
    pub attempts: u32,
    /// Revivals that ended without committing the row.
    pub failed: u32,
    /// When the record last changed.
    pub at_ms: u64,
}

#[must_use]
pub fn revival_key(community: &str, id: &str, revision: u32) -> String {
    format!("agentschedrev/{community}/{id}/{revision}")
}

/// What a revival records about itself.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RevivalEvent {
    Started,
    Failed,
}

async fn mark_revival(
    config: &mut StoreClusterClient,
    community: &str,
    id: &str,
    revision: u32,
    now_ms: u64,
    event: RevivalEvent,
) -> Result<()> {
    let key = revival_key(community, id, revision);
    for _ in 0..8 {
        let (mut doc, version) = match store::get_doc_versioned::<RevivalRecord>(config, key.as_bytes()).await? {
            Some((doc, version)) => (doc, Some(version)),
            None => (RevivalRecord::default(), None),
        };
        match event {
            RevivalEvent::Started => doc.attempts = doc.attempts.saturating_add(1),
            RevivalEvent::Failed => doc.failed = doc.failed.saturating_add(1),
        }
        doc.at_ms = now_ms;
        if store::put_doc_if_version(config, key.as_bytes(), &doc, version).await?.is_ok() {
            return Ok(());
        }
    }
    Err(BuzzError::Conflict("the schedule's revival record kept changing".into()))
}

/// Whether the revival a wake of `revision` belongs to can no longer commit,
/// so the wake (whose row is still at an OLDER revision) is a definitive stop
/// rather than a wait. Pure, over the durable record:
///
/// * every attempt recorded has failed: nothing will commit it;
/// * the record is older than [`RESERVATION_GRACE_MS`]: an attempt crashed
///   or stalled mid-way. Stopping its wake is safe even if the stalled attempt
///   later commits, because a revival publishes its wake again AFTER its row
///   commits (see [`create`]), and that wake finds the committed row;
/// * there is no record: no revival of this revision ever started under the
///   recording code (an older build's wake), and the heal pass covers it too.
///
/// Anything else is an in-flight revival, and its wake waits.
#[must_use]
pub fn revival_abandoned(record: Option<&RevivalRecord>, now_ms: u64) -> bool {
    match record {
        None => true,
        Some(doc) => doc.failed >= doc.attempts || now_ms >= doc.at_ms.saturating_add(RESERVATION_GRACE_MS),
    }
}

/// After a create or revival LOST its row CAS: settle the slot it re-reserved
/// just before, against the row that won, and return that row.
///
/// The winner may have committed this same revision and then been cancelled or
/// run out, releasing its slot BEFORE this delayed writer re-reserved it; that
/// re-added slot would hold capacity for a finished schedule until its grace
/// expired. A TERMINAL row at the attempted revision holds no slot, so it is
/// released again. A LIVE winner at that revision keeps its slot (it is the
/// winner's own, and releasing it would let the spend fence cancel it), and a
/// row at another revision is not this writer's to touch (release is
/// revision-fenced besides).
async fn reconcile_losing_write(
    config: &mut StoreClusterClient,
    community: &str,
    attempted: &Schedule,
) -> Result<Option<Schedule>> {
    let Some(row) = get(config, community, &attempted.id).await? else {
        return Ok(None);
    };
    if losing_write_releases(&row, attempted.revision) {
        if let Err(e) = release_slot(config, community, &row.agent, &row.id, row.revision).await {
            tracing::warn!("schedule {} kept a stale slot for now: {e}", row.id);
        }
    }
    Ok(Some(row))
}

/// [`reconcile_losing_write`]'s decision: only a terminal row at the attempted
/// revision gives back the slot the losing writer re-reserved.
fn losing_write_releases(row: &Schedule, attempted_revision: u32) -> bool {
    row.revision == attempted_revision && (row.cancelled || row.remaining == 0)
}

/// Mark a schedule cancelled. `Ok(false)` when there was nothing to cancel.
///
/// The row is marked, not deleted, because a wake already in the queue cannot be
/// recalled and has to find something that says stop.
pub async fn cancel(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    community: &str,
    id: &str,
    who: &str,
    privileged: bool,
) -> Result<bool> {
    let key = schedule_key(community, id);
    let Some((mut schedule, version)) =
        store::get_doc_versioned::<Schedule>(config, key.as_bytes()).await?
    else {
        return Ok(false);
    };
    if !may_cancel(&schedule, who, privileged) {
        return Err(BuzzError::Unauthorized(
            "only the person who created a schedule, a moderator, or an operator \
             may cancel it"
                .into(),
        ));
    }
    if schedule.cancelled {
        // A retried cancel still frees the capacity (revision-fenced, so a newer
        // re-create's slot is untouched) even if the first attempt's recall failed
        // before it got that far.
        if let Err(e) = release_slot(config, community, &schedule.agent, &schedule.id, schedule.revision).await {
            tracing::warn!("cancelled schedule {} kept its slot for now: {e}", schedule.id);
        }
        recall_wake_receipts(ctx, config, community, &schedule, None).await?;
        return Ok(false);
    }
    schedule.cancelled = true;
    // CAS so a concurrent advance cannot resurrect the row by writing over this.
    // A lost race means somebody else changed it; the caller retries and reads the
    // current state rather than clobbering it.
    if store::put_doc_if_version(config, key.as_bytes(), &schedule, Some(version))
        .await?
        .is_err()
    {
        return Err(BuzzError::Conflict(
            "that schedule changed while being cancelled; try again".into(),
        ));
    }
    // Capacity is released AT the cancel, BEFORE the receipt recall, so "cancel one
    // first" frees a slot now even if the recall fails and must be retried. Best
    // effort: a slot left behind is pruned later, and the spend fence never lets a
    // schedule run without one.
    if let Err(e) = release_slot(config, community, &schedule.agent, &schedule.id, schedule.revision).await {
        tracing::warn!("cancelled schedule {} kept its slot for now: {e}", schedule.id);
    }
    recall_wake_receipts(ctx, config, community, &schedule, None).await?;
    Ok(true)
}

/// Remove `id`'s slot from its agent's record, by CAS — only while it is still
/// held for `revision`. A cancel that finishes after a re-create took the slot
/// over for a newer revision leaves that slot alone.
///
/// With NO record yet, the release seeds one itself (from a scan that cannot
/// include this schedule) by create-if-absent: a first-record seed already in
/// flight, which may have scanned this schedule while it was still live, then
/// loses its CAS and re-reads instead of writing a slot back for it.
async fn release_slot(
    config: &mut StoreClusterClient,
    community: &str,
    agent: &str,
    id: &str,
    revision: u32,
) -> Result<()> {
    let key = agent_slots_key(community, agent);
    for _ in 0..8 {
        let Some((mut doc, version)) =
            store::get_doc_versioned::<AgentSlots>(config, key.as_bytes()).await?
        else {
            let Some(live) = live_ids_for_agent(config, community, agent).await? else {
                // Unprovable: no record can be written; a later seed scans the
                // row as finished and leaves it out.
                return Ok(());
            };
            let live = live.into_iter().filter(|(sid, rev, _)| !(sid == id && *rev == revision)).collect();
            let seeded = AgentSlots { slots: seed_slots(live, 0) };
            if store::put_doc_if_version(config, key.as_bytes(), &seeded, None).await?.is_ok() {
                return Ok(());
            }
            continue; // a concurrent seed won: release from its record
        };
        let before = doc.slots.len();
        doc.slots.retain(|slot| !(slot.id == id && slot.revision == revision));
        if doc.slots.len() == before {
            return Ok(());
        }
        if store::put_doc_if_version(config, key.as_bytes(), &doc, Some(version))
            .await?
            .is_ok()
        {
            return Ok(());
        }
    }
    Err(BuzzError::Conflict("the slot record kept changing".into()))
}

/// What a wake's own row says about it.
///
/// Four answers. A row that says stop and a row that is NOT THERE are different
/// facts with opposite correct responses, and collapsing them destroyed wakes —
/// see [`claim`]; and a row not yet at this wake's state (or a cap that cannot be
/// proven) is a retry, not a stop.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Claim {
    /// Drive this occurrence.
    Run(Schedule),
    /// Definitively not: the row exists and says cancelled, exhausted, or that this
    /// wake is stale because the row already advanced past it.
    Stop,
    /// The row is not there YET. A race with its own creation, not a deletion.
    Absent,
    /// The row exists but has not yet reached this wake's state: a revival (newer
    /// incarnation) or an advance (later occurrence) was published before its row
    /// write committed. Retried, never acked — acking it stranded the schedule.
    Pending,
}

/// Should a wake run? Four answers ([`Claim`]); [`Claim::Absent`] is why this exists.
///
/// The queue cannot recall a delayed message, so the ROW decides. Two of the
/// answers are definitive — the row exists and refuses — and they cover three real
/// situations: it was cancelled, it is exhausted, or this wake is stale because the
/// row already advanced past it (a redelivery arriving after the next occurrence was
/// queued).
///
/// The third answer, [`Claim::Absent`], is a MEASURED defect rather than a
/// theoretical case. [`create`] publishes the wake before writing the row on purpose
/// (a crash in the other order leaves a row with nothing coming for it), but a wake
/// due NOW is deliverable the instant it is published — so the consumer can claim it
/// before `put_doc` lands. Observed live: one schedule in five was created, answered
/// 201, and never fired; its row read `occurrence = 0, remaining = 1` with a due time
/// seventeen minutes past, while a mention published ten minutes later was answered
/// in three seconds. The wake had been acked away as "no such schedule" and the row
/// appeared a moment afterwards, live and due, with nothing left to drive it.
///
/// So absence is reported separately and the caller leaves the lease UNACKED, which
/// makes redelivery the repair. Nothing is spent while it waits — no turn runs, no
/// model is called — which is what makes the queue's attempt budget an affordable
/// bound. Rows are never deleted here (cancel marks, exhaustion marks), so an absent
/// row is nearly always this race; an operator's manual removal now redelivers a few
/// times before dead-lettering instead of stopping immediately, which costs nothing.
pub async fn claim(
    config: &mut StoreClusterClient,
    community: &str,
    wake: &crate::agents::turns::ScheduledWake,
    now_ms: u64,
) -> Result<Claim> {
    let row = get(config, community, &wake.id).await?;
    // A wake of a NEWER revision than the row: its revival is in flight, or it
    // failed or crashed and will never commit. Waiting forever on the latter
    // spent queue deliveries without end; the revival's durable record decides.
    if let Some(current) = &row {
        if current.revision < wake.revision {
            let record = store::get_doc::<RevivalRecord>(
                config,
                revival_key(community, &wake.id, wake.revision).as_bytes(),
            )
            .await?;
            if revival_abandoned(record.as_ref(), now_ms) {
                return Ok(Claim::Stop);
            }
        }
    }
    let claim = classify(row, wake.revision, wake.occurrence, now_ms);
    let Claim::Run(schedule) = claim else {
        return Ok(claim);
    };
    // THE CAP'S FENCE, at the point of spend. Reservations are renewed before a
    // commit, but a create that stalls long enough can still commit after its slot
    // was reclaimed; no ordering of independent writes prevents that. So a wake
    // runs only while its schedule holds a slot, and the slot record never holds
    // more than MAX_PER_AGENT. A schedule that committed without one is stopped and
    // marked cancelled rather than left looking live.
    let Some(slots) = agent_slots_seeded(config, community, &schedule.agent, now_ms).await? else {
        // The cap cannot be proven: the agent has no slot record and the seeding
        // scan ran out of its page budget. FAIL CLOSED: not this time, and not
        // acked; an unprovable bound is not permission to spend. This is a
        // capacity limit of the community's schedule table, raised to the operator.
        tracing::error!(
            "schedule {} of {} cannot run: the per-agent cap cannot be proven because \
             the community's schedule rows exceed the seeding scan budget",
            schedule.id,
            schedule.agent
        );
        return Ok(Claim::Pending);
    };
    if slot_admits(Some(&slots), &schedule.id, schedule.revision) {
        return Ok(Claim::Run(schedule));
    }
    tracing::warn!(
        "schedule {} holds no slot under {}'s cap (its reservation was reclaimed); \
         it is stopped and will not run",
        schedule.id,
        schedule.agent
    );
    let key = schedule_key(community, &schedule.id);
    if let Some((mut row, version)) =
        store::get_doc_versioned::<Schedule>(config, key.as_bytes()).await?
    {
        if stop_marks_cancelled(&row, schedule.revision) {
            row.cancelled = true;
            let _ = store::put_doc_if_version(config, key.as_bytes(), &row, Some(version)).await?;
        }
    }
    Ok(Claim::Stop)
}

/// Whether a wake stopped for holding no slot marks its row cancelled: only a row
/// still LIVE at that revision. An exhausted row (a concurrent consumer may have
/// run the last occurrence and released the slot in between) stays exhausted, so
/// an identical create later reads it as a finished schedule, not as one to revive.
fn stop_marks_cancelled(row: &Schedule, revision: u32) -> bool {
    row.revision == revision && !row.cancelled && row.remaining > 0
}

/// The agent's slot record, creating it on first use from its live schedules
/// (capped, see [`seed_slots`]) if no create has done so yet. Without this an
/// agent that never creates another schedule keeps every older schedule running,
/// over the cap, forever. `None` when the seeding scan cannot prove the set.
async fn agent_slots_seeded(
    config: &mut StoreClusterClient,
    community: &str,
    agent: &str,
    now_ms: u64,
) -> Result<Option<AgentSlots>> {
    let key = agent_slots_key(community, agent);
    if let Some(doc) = store::get_doc::<AgentSlots>(config, key.as_bytes()).await? {
        return Ok(Some(doc));
    }
    let Some(live) = live_ids_for_agent(config, community, agent).await? else {
        return Ok(None);
    };
    let seeded = AgentSlots { slots: seed_slots(live, now_ms) };
    // Conditional on absence; whoever lost the race reads the winner's record.
    let _ = store::put_doc_if_version(config, key.as_bytes(), &seeded, None).await?;
    store::get_doc::<AgentSlots>(config, key.as_bytes()).await
}

/// Whether a schedule may run under its agent's cap. No slot record yet means
/// every schedule of that agent predates the cap record (callers seed one first,
/// see [`agent_slots_seeded`]); once a record exists, only an id holding a slot
/// for THIS revision may run.
#[must_use]
pub fn slot_admits(slots: Option<&AgentSlots>, id: &str, revision: u32) -> bool {
    slots.is_none_or(|doc| doc.slots.iter().any(|slot| slot.id == id && slot.revision == revision))
}

/// [`claim`]'s decision, with the I/O removed.
///
/// Split out because the case that matters cannot be tested through the store: an
/// absent row is a RACE, and a test that had to win one deliberately would be a
/// coincidence with an assertion attached rather than a test. Here every branch is
/// reachable directly.
///
/// The wake's `revision` is checked FIRST: a wake from an older revision is stale
/// whatever its occurrence says (otherwise an old occurrence-zero wake ran a revived
/// schedule early), and one from a newer revision is a revival whose row has not
/// landed. A wake queued before revisions existed reads as revision 0, which is
/// what its row was.
///
/// A matching wake that arrives before the row's committed due time is held too:
/// two settles racing across an interval boundary can each publish a different next
/// due time, and only one commits. Held rather than stopped, because the committed
/// wake itself can look a moment early against a clock that is not the queue's.
#[must_use]
pub fn classify(row: Option<Schedule>, revision: u32, occurrence: u32, now_ms: u64) -> Claim {
    let Some(schedule) = row else {
        // NOT the same as a refusal: the create may still be in flight.
        return Claim::Absent;
    };
    if schedule.revision > revision {
        return Claim::Stop;
    }
    if schedule.revision < revision {
        return Claim::Pending;
    }
    if schedule.cancelled {
        return Claim::Stop;
    }
    // EXHAUSTED. The final occurrence marks `remaining = 0` rather than deleting the
    // row, so a redelivery of that last wake finds something that says stop.
    if schedule.remaining == 0 {
        return Claim::Stop;
    }
    // AHEAD of the row: the next occurrence was published before its advance
    // committed. Same revision, so it is ours and not yet due to be judged.
    if occurrence > schedule.occurrence {
        return Claim::Pending;
    }
    if schedule.occurrence != occurrence {
        return Claim::Stop;
    }
    if now_ms < schedule.next_due_ms {
        return Claim::Pending;
    }
    Claim::Run(schedule)
}

/// For a wake that [`claim`] answered Stop: if the row is THIS revision, live,
/// and exactly one occurrence past the wake, this wake's settlement committed
/// the next occurrence, and its post-commit publish may have failed. The next
/// occurrence's wake is published again (the occurrence admission absorbs a
/// duplicate). Any other row (cancelled, exhausted, another revision, further
/// ahead) needs nothing. An error leaves the stale delivery unacked so the
/// repair is retried.
pub async fn repair_advanced(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    community: &str,
    wake: &crate::agents::turns::ScheduledWake,
    harness: &str,
) -> Result<()> {
    let Some(row) = get(config, community, &wake.id).await? else {
        return Ok(());
    };
    if repair_republishes(&row, wake.revision, wake.occurrence) {
        publish_wake(ctx, community, &row, harness).await?;
    }
    Ok(())
}

/// [`repair_advanced`]'s decision, pure.
fn repair_republishes(row: &Schedule, revision: u32, occurrence: u32) -> bool {
    row.revision == revision
        && !row.cancelled
        && row.remaining > 0
        && row.occurrence == occurrence.saturating_add(1)
}

/// Advance a schedule after its occurrence has been driven, queueing the next.
///
/// Called AFTER the turn, so a crash mid-turn redelivers into the same admission
/// and is absorbed rather than consuming an occurrence nothing performed.
///
/// Idempotent under redelivery by the occurrence check: a second call finds the
/// row already advanced and does nothing.
pub async fn settle(
    ctx: &AppContext,
    config: &mut StoreClusterClient,
    community: &str,
    schedule: &Schedule,
    harness: &str,
    now_ms: u64,
) -> Result<()> {
    let key = schedule_key(community, &schedule.id);
    let Some((current, version)) =
        store::get_doc_versioned::<Schedule>(config, key.as_bytes()).await?
    else {
        return Ok(());
    };
    // Already advanced by an earlier delivery of this same wake, or cancelled
    // while the turn was running — in which case there is nothing further to queue.
    if current.created_ms != schedule.created_ms
        || current.occurrence != schedule.occurrence
        || current.cancelled
    {
        // A retry that finds THIS revision already advanced past this occurrence
        // and still live may be retrying a settlement whose post-commit publish
        // failed: its next wake is published again (a duplicate is absorbed by
        // the occurrence admission), so that failure never strands it.
        if current.revision == schedule.revision
            && current.created_ms == schedule.created_ms
            && current.occurrence == schedule.occurrence.saturating_add(1)
            && !current.cancelled
            && current.remaining > 0
        {
            publish_wake(ctx, community, &current, harness).await?;
        }
        return recall_wake_receipts(ctx, config, community, schedule, Some(schedule.occurrence))
            .await;
    }
    match advance(&current, now_ms) {
        // Exhausted. Marked, not deleted, and by CAS: an unconditional delete
        // after a versioned read can remove a cancellation that landed in
        // between. Zero `remaining` is what [`claim`] refuses, so a redelivery of
        // this final wake finds a row that says stop rather than one whose
        // occurrence still matches.
        None => {
            let finished = Schedule {
                remaining: 0,
                occurrence: current.occurrence.saturating_add(1),
                ..current.clone()
            };
            if store::put_doc_if_version(config, key.as_bytes(), &finished, Some(version))
                .await?
                .is_ok()
            {
                // Exhausted: its capacity is free now.
                if let Err(e) = release_slot(config, community, &current.agent, &current.id, current.revision).await {
                    tracing::warn!("exhausted schedule {} kept its slot for now: {e}", current.id);
                }
            }
        }
        Some(next) => {
            // Publish before the CAS, for the reason `create` does: a duplicate
            // wake is absorbed, a missing one is permanent. And AGAIN after a
            // committed CAS: a settlement that stalled long enough for the first
            // wake to be retried away (it finds the row not yet advanced) would
            // otherwise commit the next occurrence with no wake left. A failure
            // is returned so the settlement is retried.
            publish_wake(ctx, community, &next, harness).await?;
            if store::put_doc_if_version(config, key.as_bytes(), &next, Some(version))
                .await?
                .is_ok()
            {
                publish_wake(ctx, community, &next, harness).await?;
            }
        }
    }
    // Once this occurrence has settled, recall any duplicate publications and
    // remove their receipts. The next occurrence has a separate prefix.
    recall_wake_receipts(ctx, config, community, schedule, Some(schedule.occurrence)).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stale_wake_republishes_only_a_live_next_occurrence_of_its_revision() {
        let mut row = build(&request(), "main", 0).expect("builds");
        row.revision = 2;
        row.occurrence = 4;
        row.remaining = 3;
        // The wake for occurrence 3 settled it to 4: the next wake may be missing.
        assert!(repair_republishes(&row, 2, 3));
        // Further ahead, another revision, cancelled or exhausted: nothing to do.
        assert!(!repair_republishes(&row, 2, 2));
        assert!(!repair_republishes(&row, 1, 3));
        let mut cancelled = row.clone();
        cancelled.cancelled = true;
        assert!(!repair_republishes(&cancelled, 2, 3));
        let mut exhausted = row.clone();
        exhausted.remaining = 0;
        assert!(!repair_republishes(&exhausted, 2, 3));
    }

    /// The losing-writer invariant, step by step in a fixed interleaving: the
    /// slot a delayed create or revival re-reserved is given back only when the
    /// row that won is TERMINAL at the attempted revision. A live winner keeps
    /// its slot, and another revision's slot is never touched.
    #[tokio::test]
    async fn a_losing_writer_gives_back_its_slot_only_for_a_terminal_winner() {
        let env = ferrite_app_test::TestHarness::new().with_store("config").build().await;
        let ctx = env.context();
        let mut config = ctx.store("config").expect("config store");
        let mut row = build(&request(), "main", 0).expect("builds");
        row.revision = 1;
        let row_key = schedule_key("main", &row.id);
        let slots_key = agent_slots_key("main", &row.agent);
        let held = |rev: u32| AgentSlots { slots: vec![Slot { id: row.id.clone(), at_ms: 0, revision: rev }] };
        let holds = |doc: &AgentSlots, rev: u32| doc.slots.iter().any(|s| s.id == row.id && s.revision == rev);

        // 1. The winner committed revision 1 and is LIVE; the loser's
        //    re-reservation is the same slot. It stays: it is the winner's.
        store::put_doc(&mut config, row_key.as_bytes(), &row).await.expect("row");
        store::put_doc(&mut config, slots_key.as_bytes(), &held(1)).await.expect("slots");
        let won = reconcile_losing_write(&mut config, "main", &row).await.expect("reconciles");
        assert_eq!(won.as_ref().map(|r| r.revision), Some(1));
        let doc: AgentSlots = store::get_doc(&mut config, slots_key.as_bytes()).await.expect("read").expect("doc");
        assert!(holds(&doc, 1), "a live winner keeps its slot");

        // 2. The winner is cancelled and releases its slot; THEN the delayed
        //    loser re-reserves it and loses its CAS. The slot is given back.
        let mut cancelled = row.clone();
        cancelled.cancelled = true;
        store::put_doc(&mut config, row_key.as_bytes(), &cancelled).await.expect("cancel");
        store::put_doc(&mut config, slots_key.as_bytes(), &AgentSlots { slots: vec![] }).await.expect("release");
        store::put_doc(&mut config, slots_key.as_bytes(), &held(1)).await.expect("late re-reservation");
        reconcile_losing_write(&mut config, "main", &row).await.expect("reconciles");
        let doc: AgentSlots = store::get_doc(&mut config, slots_key.as_bytes()).await.expect("read").expect("doc");
        assert!(!holds(&doc, 1), "a terminal winner's re-added slot is released");

        // 3. The same for an EXHAUSTED winner.
        let mut exhausted = row.clone();
        exhausted.remaining = 0;
        store::put_doc(&mut config, row_key.as_bytes(), &exhausted).await.expect("exhaust");
        store::put_doc(&mut config, slots_key.as_bytes(), &held(1)).await.expect("late re-reservation");
        reconcile_losing_write(&mut config, "main", &row).await.expect("reconciles");
        let doc: AgentSlots = store::get_doc(&mut config, slots_key.as_bytes()).await.expect("read").expect("doc");
        assert!(!holds(&doc, 1));

        // 4. A row already at ANOTHER revision is not this writer's: untouched.
        let mut newer = cancelled.clone();
        newer.revision = 2;
        store::put_doc(&mut config, row_key.as_bytes(), &newer).await.expect("newer");
        store::put_doc(&mut config, slots_key.as_bytes(), &held(1)).await.expect("slot");
        reconcile_losing_write(&mut config, "main", &row).await.expect("reconciles");
        let doc: AgentSlots = store::get_doc(&mut config, slots_key.as_bytes()).await.expect("read").expect("doc");
        assert!(holds(&doc, 1), "a different revision's state is left alone");
    }

    fn request() -> ScheduleRequest {
        ScheduleRequest {
            agent: "revka".into(),
            channel: "general".into(),
            prompt: "post the standup reminder".into(),
            first_due_ms: 10_000,
            interval_ms: None,
            occurrences: 1,
            created_by: "ana".into(),
        }
    }

    #[test]
    fn a_wake_tells_the_persona_which_channel_to_answer_in() {
        // Measured live: with the bare prompt, the model guessed a channel id out of
        // the prompt's text, buzz_post refused it, and the turn's answer was an
        // apology instead of the work. A mention's trigger names its channel; a
        // wake's has to as well, because a scheduled turn has no conversation
        // around it to infer one from.
        let s = build(&request(), "main", 0).expect("builds");
        let wake = render_wake(&s);
        assert!(
            wake.contains(&s.channel),
            "a wake must name its channel: {wake}"
        );
        assert!(
            wake.contains(&s.prompt),
            "and must still carry the instruction verbatim: {wake}"
        );
        // The instruction comes LAST, so a long prompt cannot push the context out
        // of the model's attention before it is read.
        let channel_at = wake.find(&s.channel).expect("present");
        let prompt_at = wake.find(&s.prompt).expect("present");
        assert!(
            channel_at < prompt_at,
            "the context precedes the instruction"
        );
    }

    #[test]
    fn only_a_live_overdue_row_is_republished() {
        let due = build(&request(), "main", 0).expect("builds");
        // next_due_ms is 10_000 (the request's first_due_ms).
        assert!(
            needs_republish(&due, 10_000),
            "a row due exactly now needs its wake"
        );
        assert!(
            needs_republish(&due, 99_999),
            "and one overdue certainly does"
        );
        assert!(
            !needs_republish(&due, 9_999),
            "a row not yet due has a wake waiting in the queue"
        );

        // A cancelled row must never be revived by the sweep — that would undo a
        // cancellation, which is the one thing the row exists to make real.
        let mut cancelled = due.clone();
        cancelled.cancelled = true;
        assert!(!needs_republish(&cancelled, 99_999));

        // Nor an exhausted one: `remaining == 0` is what a redelivered final wake
        // reads as stop, so publishing another would be asking to be refused.
        let mut finished = due;
        finished.remaining = 0;
        assert!(!needs_republish(&finished, 99_999));
    }

    #[test]
    fn a_wake_that_arrives_before_its_own_row_waits_rather_than_being_discarded() {
        // The defect this separates, measured on a deployment: `create` publishes the
        // wake before writing the row, so a wake due NOW can be claimed before the
        // row lands. Classified with cancelled and exhausted as definitive, it was
        // acked away — and the row then appeared, live and due, with nothing left to
        // drive it. One schedule in five died that way.
        assert_eq!(
            classify(None, 0, 0, 0),
            Claim::Absent,
            "an absent row is a race with the create, not a refusal"
        );

        let live = build(&request(), "main", 7).expect("builds");
        let due = live.next_due_ms;
        assert_eq!(classify(Some(live.clone()), 0, 0, due), Claim::Run(live.clone()));

        // The three rows that EXIST and say no keep their definitive answer, because
        // none of them changes on a retry.
        let mut cancelled = live.clone();
        cancelled.cancelled = true;
        assert_eq!(classify(Some(cancelled), 0, 0, due), Claim::Stop);

        let mut exhausted = live.clone();
        exhausted.remaining = 0;
        assert_eq!(classify(Some(exhausted), 0, 0, due), Claim::Stop);

        // An occurrence BEHIND the row is stale — not a reason to run.
        let mut advanced = live;
        advanced.occurrence = 3;
        assert_eq!(classify(Some(advanced), 0, 2, due), Claim::Stop);
    }

    #[test]
    fn a_wake_is_fenced_to_its_own_revision() {
        // A re-created schedule restarts at occurrence 0 with a higher revision.
        // Before the fence, the cancelled revision's occurrence-0 wake matched the
        // new row and ran it before its due time.
        let old = build(&request(), "main", 7).expect("builds");
        let due = old.next_due_ms;
        let mut revived = old.clone();
        revived.revision = 1;
        assert_eq!(
            classify(Some(revived.clone()), 0, 0, due),
            Claim::Stop,
            "a wake from an older revision must never run the revived schedule"
        );

        // The revival publishes BEFORE its CAS, so its wake can find the old
        // cancelled row. That is a revival in flight, not a stop: acking it left the
        // revived row live with no wake.
        let mut cancelled_old = old;
        cancelled_old.cancelled = true;
        assert_eq!(
            classify(Some(cancelled_old), 1, 0, due),
            Claim::Pending,
            "a newer revision's wake waits for its row"
        );
        assert_eq!(classify(Some(revived.clone()), 1, 0, due), Claim::Run(revived));
    }

    #[test]
    fn a_wake_before_the_committed_due_time_waits() {
        // Two settles racing across an interval boundary publish different next due
        // times; the earlier one's wake matched revision and occurrence and ran
        // before the due time that actually committed.
        let row = build(&request(), "main", 7).expect("builds");
        assert_eq!(classify(Some(row.clone()), 0, 0, row.next_due_ms - 1), Claim::Pending);
        assert_eq!(classify(Some(row.clone()), 0, 0, row.next_due_ms), Claim::Run(row));
    }

    #[test]
    fn a_next_occurrence_published_before_its_advance_waits() {
        // settle publishes occurrence n+1, then CASes the row. A wake for n+1 that
        // arrives first found occurrence n and was acked as stale — stranding the
        // recurrence, since nothing else ever queues that occurrence again.
        let mut every = request();
        every.interval_ms = Some(MIN_INTERVAL_MS);
        every.occurrences = 5;
        let row = build(&every, "main", 7).expect("builds");
        let due = row.next_due_ms;
        assert_eq!(classify(Some(row.clone()), 0, 1, due), Claim::Pending);
        // Once the row is cancelled, the same wake is a definitive stop.
        let mut cancelled = row;
        cancelled.cancelled = true;
        assert_eq!(classify(Some(cancelled), 0, 1, due), Claim::Stop);
    }

    #[test]
    fn a_retry_of_an_overdue_request_is_the_same_schedule() {
        // The id used to digest the NORMALISED due time, so the same overdue request
        // retried a moment later was a new schedule and a second model call.
        //
        // UPGRADE NOTE: an overdue row created under the old scheme was keyed by the
        // `now` of its original create, which no later request can reconstruct. A
        // retry of it after this change therefore makes a new row, exactly as every
        // retry of it did BEFORE this change; requests that were not overdue digest
        // the same time under both schemes and keep their identity. Matching legacy
        // rows by content instead would merge distinct schedules that share a prompt.
        let mut asap = request();
        asap.first_due_ms = 1_000;
        let first = build(&asap, "main", 5_000).expect("builds");
        let retry = build(&asap, "main", 6_000).expect("builds");
        assert_eq!(first.id, retry.id);
        assert_eq!(first.next_due_ms, 5_000, "delivery is still normalised to now");
        // A future request digests the same time it always did.
        let mut later = request();
        later.first_due_ms = 9_000;
        assert_eq!(build(&later, "main", 5_000).expect("builds").next_due_ms, 9_000);
    }

    #[test]
    fn a_slotless_stop_never_cancels_an_exhausted_row() {
        let mut every = request();
        every.occurrences = 2;
        let live = build(&every, "main", 7).expect("builds");
        assert!(stop_marks_cancelled(&live, live.revision), "a live row without a slot is stopped for good");
        let mut exhausted = live.clone();
        exhausted.remaining = 0;
        assert!(
            !stop_marks_cancelled(&exhausted, live.revision),
            "an exhausted row must stay exhausted, or an identical create would revive it"
        );
        let mut revived = live.clone();
        revived.revision += 1;
        assert!(!stop_marks_cancelled(&revived, live.revision), "a newer revision is not this wake's");
        let mut cancelled = live;
        cancelled.cancelled = true;
        assert!(!stop_marks_cancelled(&cancelled, 0));
    }

    #[test]
    fn a_newer_revision_wake_stops_only_when_its_revival_cannot_commit() {
        let now = 1_000_000;
        let rec = |attempts, failed, at_ms| RevivalRecord { attempts, failed, at_ms };
        // In flight: one attempt, not failed, recent — the wake waits.
        assert!(!revival_abandoned(Some(&rec(1, 0, now)), now));
        // Two identical revivals, one lost its CAS: the other may still commit.
        assert!(!revival_abandoned(Some(&rec(2, 1, now)), now), "a loser must not stop the winner's wake");
        // Every attempt failed: stop.
        assert!(revival_abandoned(Some(&rec(2, 2, now)), now));
        // An attempt that crashed mid-way: stop once past the grace.
        assert!(!revival_abandoned(Some(&rec(1, 0, now)), now + RESERVATION_GRACE_MS - 1));
        assert!(revival_abandoned(Some(&rec(1, 0, now)), now + RESERVATION_GRACE_MS));
        // No record: an older build's wake, or nothing ever started.
        assert!(revival_abandoned(None, now));
    }

    #[tokio::test]
    async fn a_wake_for_a_failed_revival_is_a_definitive_stop() {
        let env = ferrite_app_test::TestHarness::new().with_store("config").build().await;
        let ctx = env.context();
        let mut config = ctx.store("config").expect("config store");
        let mut row = build(&request(), "main", 0).expect("builds");
        row.cancelled = true; // revision 0, cancelled; a revival to 1 was published
        store::put_doc(&mut config, schedule_key("main", &row.id).as_bytes(), &row).await.expect("row");
        let wake = crate::agents::turns::ScheduledWake { id: row.id.clone(), occurrence: 0, revision: 1 };
        let now = 5_000;
        mark_revival(&mut config, "main", &row.id, 1, now, RevivalEvent::Started).await.expect("start");
        assert_eq!(claim(&mut config, "main", &wake, now).await.expect("claims"), Claim::Pending, "in flight");
        mark_revival(&mut config, "main", &row.id, 1, now, RevivalEvent::Failed).await.expect("fail");
        assert_eq!(claim(&mut config, "main", &wake, now).await.expect("claims"), Claim::Stop, "never commits");
    }

    #[test]
    fn a_schedule_runs_only_while_it_holds_a_slot() {
        let held = AgentSlots { slots: vec![Slot { id: "kept".into(), at_ms: 0, revision: 1 }] };
        assert!(slot_admits(Some(&held), "kept", 1));
        assert!(
            !slot_admits(Some(&held), "reclaimed", 0),
            "a create that committed after its reservation was reclaimed must not spend"
        );
        assert!(
            !slot_admits(Some(&held), "kept", 0),
            "a slot taken over by a re-create does not admit the old revision"
        );
        assert!(slot_admits(None, "legacy", 0), "no record yet: schedules predating it still run");
    }

    #[test]
    fn seeding_admits_at_most_the_cap_oldest_first() {
        // An agent already over the cap under the old admission keeps its oldest
        // schedules; the rest hold no slot and are stopped by the spend fence.
        let live: Vec<_> = (0..MAX_PER_AGENT as u64 + 5)
            .rev()
            .map(|n| (format!("s{n:03}"), 0, n))
            .collect();
        let seeded = seed_slots(live, 99);
        assert_eq!(seeded.len(), MAX_PER_AGENT);
        assert_eq!(seeded[0].id, "s000");
        assert_eq!(seeded[MAX_PER_AGENT - 1].id, format!("s{:03}", MAX_PER_AGENT - 1));
    }

    #[test]
    fn a_slot_is_held_by_a_live_row_or_a_fresh_reservation_only() {
        let slot = |id: &str, at_ms| Slot { id: id.into(), at_ms, revision: 0 };
        let now = RESERVATION_GRACE_MS + 500;
        let slots = [
            slot("live", 0),
            slot("done", 0),
            slot("fresh", 1_000),
            slot("stale", 0),
            // A revival refreshes its slot while its row still reads cancelled.
            slot("reviving", now),
        ];
        let rows = [
            SlotRow::Live,
            SlotRow::Finished,
            SlotRow::Absent,
            SlotRow::Absent,
            SlotRow::Finished,
        ];
        let held: Vec<_> = held_slots(&slots, &rows, now).into_iter().map(|s| s.id).collect();
        assert_eq!(held, vec!["live".to_string(), "fresh".to_string(), "reviving".to_string()]);
    }

    #[test]
    fn the_same_request_is_the_same_schedule() {
        // Idempotence is the point: a caller who cannot tell whether their first
        // attempt landed must not double the spend finding out.
        let a = build(&request(), "main", 0).expect("builds");
        let b = build(&request(), "main", 0).expect("builds");
        assert_eq!(a.id, b.id);

        // ...and a different definition is a different schedule.
        let mut other = request();
        other.prompt = "post something else".into();
        assert_ne!(build(&other, "main", 0).expect("builds").id, a.id);
        let mut later = request();
        later.first_due_ms = 20_000;
        assert_ne!(build(&later, "main", 0).expect("builds").id, a.id);
        // Another community's identical schedule is not this one.
        assert_ne!(build(&request(), "other", 0).expect("builds").id, a.id);
    }

    #[test]
    fn the_id_cannot_be_confused_by_a_component_containing_a_separator() {
        // Length-prefixed digest material. Joined by a delimiter, an agent named
        // `a` in channel `b/c` and an agent named `a/b` in channel `c` would be
        // the same schedule — two personas sharing one row.
        let mut first = request();
        first.agent = "a".into();
        first.channel = "b/c".into();
        let mut second = request();
        second.agent = "a/b".into();
        second.channel = "c".into();
        assert_ne!(
            build(&first, "main", 0).expect("builds").id,
            build(&second, "main", 0).expect("builds").id
        );
    }

    #[test]
    fn each_occurrence_admits_separately_and_a_redelivery_does_not() {
        // The first half of the turn pipeline's dedup contract: a redelivered wake
        // reconstructs the SAME admission id and is absorbed into one turn.
        let id = "abc";
        // A revision-0 admission keeps the original format, so a wake queued before
        // revisions existed and one re-published now share one admission.
        assert_eq!(
            occurrence_admission(id, 0, 3),
            to_hex(&sha256(b"buzz-schedule-occurrence:abc:3"))
        );
        assert_eq!(occurrence_admission(id, 9, 3), occurrence_admission(id, 9, 3));
        // The second half: a later occurrence is a different turn and must not be
        // absorbed into the earlier one.
        assert_ne!(occurrence_admission(id, 9, 3), occurrence_admission(id, 9, 4));
        assert_ne!(occurrence_admission("abd", 9, 3), occurrence_admission(id, 9, 3));
        // A revived schedule restarts at occurrence 0; its runs must not be absorbed
        // as the cancelled incarnation's.
        assert_ne!(occurrence_admission(id, 9, 0), occurrence_admission(id, 10, 0));
        // A 64-hex id, which is what `TurnRequest::msg_id` is elsewhere.
        let admission = occurrence_admission(id, 9, 0);
        assert_eq!(admission.len(), 64);
        assert!(admission.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn a_recurrence_faster_than_a_minute_is_refused_by_name() {
        let mut fast = request();
        fast.interval_ms = Some(1_000);
        let refused = build(&fast, "main", 0).expect_err("refuses");
        // The caller is often a MODEL, so the bound has to be in the sentence.
        assert!(
            refused.to_string().contains("60000"),
            "the refusal must name the bound: {refused}"
        );
        // Exactly a minute is allowed — the floor is inclusive.
        let mut ok = request();
        ok.interval_ms = Some(MIN_INTERVAL_MS);
        assert!(build(&ok, "main", 0).is_ok());
    }

    #[test]
    fn an_empty_instruction_is_refused_because_it_would_buy_silence() {
        let mut blank = request();
        blank.prompt = "   ".into();
        assert!(build(&blank, "main", 0).is_err());
        let mut huge = request();
        huge.prompt = "x".repeat(MAX_PROMPT_BYTES + 1);
        assert!(build(&huge, "main", 0).is_err());
        // And the prompt is stored TRIMMED, so two requests differing only in
        // surrounding whitespace are one schedule.
        let mut padded = request();
        padded.prompt = format!("  {}  ", request().prompt);
        assert_eq!(
            build(&padded, "main", 0).expect("builds").id,
            build(&request(), "main", 0).expect("builds").id
        );
    }

    #[test]
    fn a_due_time_in_the_past_becomes_now_rather_than_a_refusal() {
        // "As soon as possible" is a coherent request, and a caller computing a
        // due time against a slightly different clock should not be refused — that
        // only teaches callers to guess a safe margin.
        let mut past = request();
        past.first_due_ms = 5;
        let built = build(&past, "main", 9_000).expect("builds");
        assert_eq!(built.next_due_ms, 9_000);

        // A wake a decade out is refused, because it fires into a workspace whose
        // channel and persona are gone.
        let mut distant = request();
        distant.first_due_ms = MAX_HORIZON_MS * 10;
        assert!(build(&distant, "main", 0).is_err());
    }

    #[test]
    fn a_one_shot_does_not_advance_and_a_recurrence_is_bounded_by_the_row() {
        let once = build(&request(), "main", 0).expect("builds");
        assert_eq!(once.remaining, 1);
        assert!(advance(&once, 10_000).is_none(), "a one-shot runs once");

        let mut every = request();
        every.interval_ms = Some(MIN_INTERVAL_MS);
        every.occurrences = 3;
        let first = build(&every, "main", 0).expect("builds");
        assert_eq!(first.remaining, 3);
        assert_eq!(first.occurrence, 0);

        let second = advance(&first, first.next_due_ms).expect("a second run");
        assert_eq!(second.remaining, 2);
        assert_eq!(second.occurrence, 1);
        assert_eq!(second.next_due_ms, first.next_due_ms + MIN_INTERVAL_MS);

        let third = advance(&second, second.next_due_ms).expect("a third run");
        assert_eq!(third.remaining, 1);
        // Exhausted: the bound lives in the ROW, so a persona cannot extend it by
        // influencing the message that woke it.
        assert!(advance(&third, third.next_due_ms).is_none());
    }

    #[test]
    fn a_late_wake_resumes_instead_of_queueing_a_burst() {
        // A redeployment or a backlog can deliver a wake long after it was due.
        // Advancing by ONE interval would then queue every missed occurrence back
        // to back, each a model call, all answering a question about a moment that
        // has passed.
        let mut every = request();
        every.interval_ms = Some(MIN_INTERVAL_MS);
        every.occurrences = 100;
        let due = build(&every, "main", 0).expect("builds");
        let very_late = due.next_due_ms + MIN_INTERVAL_MS * 50;
        let next = advance(&due, very_late).expect("a next run");
        assert!(
            next.next_due_ms > very_late,
            "the next occurrence must be in the future, got {} against now {}",
            next.next_due_ms,
            very_late
        );
        // And exactly one occurrence was consumed, not fifty.
        assert_eq!(next.remaining, 99);
        assert_eq!(next.occurrence, 1);
    }

    #[test]
    fn occurrences_above_the_cap_are_clamped_rather_than_refused() {
        // A model asking for "every hour forever" gets the cap, which is a usable
        // answer; refusing would make it guess a number the relay accepts.
        let mut greedy = request();
        greedy.interval_ms = Some(MIN_INTERVAL_MS);
        greedy.occurrences = u32::MAX;
        assert_eq!(
            build(&greedy, "main", 0).expect("builds").remaining,
            MAX_OCCURRENCES
        );
        // Zero is not silence either — a schedule that runs no times is a row
        // nobody can tell from a bug.
        let mut none = request();
        none.interval_ms = Some(MIN_INTERVAL_MS);
        none.occurrences = 0;
        assert_eq!(build(&none, "main", 0).expect("builds").remaining, 1);
    }

    #[test]
    fn a_different_occurrence_count_or_creator_is_a_different_schedule() {
        // Both were missing from the digest, and both bite.
        let mut once = request();
        once.interval_ms = Some(MIN_INTERVAL_MS);
        once.occurrences = 1;
        let mut many = once.clone();
        many.occurrences = 100;
        assert_ne!(
            build(&once, "main", 0).expect("builds").id,
            build(&many, "main", 0).expect("builds").id,
            "one row for both would give a caller a recurrence bound they never asked for"
        );

        // The sharper half: `may_cancel` authorizes against the row's creator, so a
        // shared row would hand the second writer authority over the first's schedule.
        let mut bo = request();
        bo.created_by = "bo".into();
        let ana_row = build(&request(), "main", 0).expect("builds");
        let bo_row = build(&bo, "main", 0).expect("builds");
        assert_ne!(ana_row.id, bo_row.id);
        assert!(!may_cancel(&ana_row, "bo", false));
        assert!(!may_cancel(&bo_row, "ana", false));
    }

    #[test]
    fn a_clamped_count_still_yields_one_schedule() {
        // The id is derived from the NORMALISED count, so asking for u32::MAX and
        // asking for the cap describe one behaviour and get one row.
        let mut greedy = request();
        greedy.interval_ms = Some(MIN_INTERVAL_MS);
        greedy.occurrences = u32::MAX;
        let mut capped = greedy.clone();
        capped.occurrences = MAX_OCCURRENCES;
        assert_eq!(
            build(&greedy, "main", 0).expect("builds").id,
            build(&capped, "main", 0).expect("builds").id
        );
    }

    #[test]
    fn advancing_cannot_spin_on_an_unrepresentable_due_time() {
        // The catch-up loop used saturating addition, which pins at u64::MAX and
        // never exceeds a now_ms of u64::MAX — an infinite loop reachable from a
        // value the type permits.
        let mut every = request();
        every.interval_ms = Some(MIN_INTERVAL_MS);
        every.occurrences = 10;
        let mut s = build(&every, "main", 0).expect("builds");
        s.next_due_ms = u64::MAX - 1;
        assert!(
            advance(&s, u64::MAX).is_none(),
            "an unrepresentable next due time is exhausted, not a spin"
        );
        // And an interval that overflows from an ordinary due time is exhausted too.
        let mut wide = build(&every, "main", 0).expect("builds");
        wide.interval_ms = Some(u64::MAX);
        wide.next_due_ms = 10;
        assert!(advance(&wide, 0).is_none());
    }

    #[test]
    fn advancing_over_a_huge_gap_is_one_step_of_arithmetic() {
        // The catch-up was a loop of one interval per missed occurrence: bounded by
        // overflow, not by work. A minute interval against a clock this far ahead
        // was ~1.5e11 iterations; now it is a division, and the answer is exact.
        let mut every = request();
        every.interval_ms = Some(MIN_INTERVAL_MS);
        every.occurrences = 10;
        let s = build(&every, "main", 0).expect("builds");
        let now = u64::MAX / 2;
        let next = advance(&s, now).expect("a next run");
        assert!(next.next_due_ms > now);
        assert!(next.next_due_ms - now <= MIN_INTERVAL_MS);
        assert_eq!((next.next_due_ms - s.next_due_ms) % MIN_INTERVAL_MS, 0, "on the schedule's grid");
        // A wake delivered exactly on a later grid point moves one interval past it.
        let on_grid = s.next_due_ms + 3 * MIN_INTERVAL_MS;
        assert_eq!(advance(&s, on_grid).expect("next").next_due_ms, on_grid + MIN_INTERVAL_MS);
    }

    #[test]
    fn publication_and_cancellation_races_keep_stop_authority() {
        let published = build(&request(), "main", 10).expect("the schedule builds");
        let mut live = published.clone();
        assert!(!publication_was_stopped(&published, &live));
        live.cancelled = true;
        assert!(publication_was_stopped(&published, &live));
        assert_eq!(
            classify(Some(live.clone()), published.revision, published.occurrence, published.next_due_ms),
            Claim::Stop
        );
        let mut revival = published.clone();
        revival.created_ms += 1;
        assert!(
            !publication_was_stopped(&revival, &live),
            "publish-before-revive CAS is not old cancellation"
        );
        assert_ne!(
            wake_receipt_prefix("main", &revival),
            wake_receipt_prefix("main", &published)
        );
        assert!(
            publication_was_stopped(&published, &revival),
            "old incarnation is stale"
        );
        live.cancelled = false;
        live.occurrence += 1;
        assert!(publication_was_stopped(&published, &live));
    }

    #[test]
    fn persisted_wake_ids_cannot_cancel_a_recreated_or_foreign_queue() {
        let receipt = WakeReceipt {
            message_id: 7,
            group: "group".into(),
            resource: "a/jobs".into(),
            generation: 2,
            occurrence: 0,
        };
        assert!(receipt_matches_binding(
            &receipt,
            &("group".into(), "a/jobs".into(), 2)
        ));
        assert!(!receipt_matches_binding(
            &receipt,
            &("group".into(), "a/jobs".into(), 3)
        ));
        assert!(!receipt_matches_binding(
            &receipt,
            &("group".into(), "b/jobs".into(), 2)
        ));
        assert!(!receipt_matches_binding(
            &receipt,
            &("other-group".into(), "a/jobs".into(), 2)
        ));
        let bytes = ferrite_app::serde_json::to_vec(&receipt).expect("the receipt encodes");
        let recovered: WakeReceipt = ferrite_app::serde_json::from_slice(&bytes).expect("the receipt decodes");
        assert_eq!(recovered.message_id, 7);
        assert!(receipt_matches_binding(
            &recovered,
            &("group".into(), "a/jobs".into(), 2)
        ));
    }

    #[test]
    fn only_the_creator_or_a_moderator_may_cancel() {
        let mine = build(&request(), "main", 0).expect("builds");
        assert!(may_cancel(&mine, "ana", false), "the creator may cancel");
        assert!(!may_cancel(&mine, "bo", false), "a stranger may not");
        assert!(may_cancel(&mine, "bo", true), "a moderator may");
    }

    #[test]
    fn a_key_is_built_from_the_digest_so_no_caller_text_reaches_it() {
        let s = build(&request(), "main", 0).expect("builds");
        let key = schedule_key("main", &s.id);
        assert!(key.starts_with(&schedule_prefix("main")));
        // The id is hex, so a prompt containing a slash cannot shape the keyspace.
        assert!(s.id.chars().all(|c| c.is_ascii_hexdigit()));
        let mut slashed = request();
        slashed.prompt = "a/b/../../escape".into();
        let hostile = build(&slashed, "main", 0).expect("builds");
        assert!(hostile.id.chars().all(|c| c.is_ascii_hexdigit()));
        assert!(!schedule_key("main", &hostile.id).contains(".."));
    }
}
