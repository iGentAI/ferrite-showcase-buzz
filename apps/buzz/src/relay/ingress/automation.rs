//! Workflow runs, schedules and the standing of the person or persona behind them.

use ferrite_app::tracing;
use ferrite_app::serde_json;
use ferrite_app::serde_json::Value;
use ferrite_app::AppContext;
use ferrite_app::IngressRequest;
use crate::relay::access::ChannelAccess;
use crate::relay::keys;
use crate::relay::planes;
use crate::relay::store;
use crate::relay::BuzzError;
use crate::relay::Result;
use crate::relay::DEFAULT_COMMUNITY;
use super::{FEED_MAX_LIMIT, authenticate_request, enforce_write_gate, storage};
use super::agents::{agent_is_paused, caller_is_operator, clean_agent_id, registry_strict};
use super::community::{enforce_community_membership, enforce_read_standing};
use super::channels::{authorize_channel_write, is_community_moderator};
use super::identity::{resolve_access};

/// `POST /buzz/api/workflows/runs` — a workflow's runs, newest first.
///
/// Runs are DOCUMENTS, not events, so the ordinary read path cannot reach them:
/// a client can query a workflow's definition (kind 30620) through `/query`, and
/// needs this for everything that happened when it ran.
pub(super) async fn workflow_runs(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("runs body: {e}")))?;
    let workflow = body
        .get("workflow")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("this needs a 'workflow' id".into()))?;
    let limit = body
        .get("limit")
        .and_then(Value::as_u64)
        .map_or(50, |n| usize::try_from(n).unwrap_or(50))
        .clamp(1, FEED_MAX_LIMIT);

    // Two channels may define a workflow of the same id, and the run index is per community:
    // an optional `channel` narrows the history to one of them.
    let channel = body.get("channel").and_then(Value::as_str);

    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    // A run happened in a CHANNEL, and its trace repeats what was posted there.
    // Gated per run through the one access chokepoint, so a workflow's history is
    // exactly as visible as the room it ran in. The filter runs BEFORE the limit,
    // paging the index, so runs the reader cannot see never crowd out ones they can.
    const MAX_RUN_PAGES: usize = 16;
    let mut visible = Vec::new();
    let mut cursor: Option<Vec<u8>> = None;
    for _ in 0..MAX_RUN_PAGES {
        let (runs, next) =
            crate::relay::workflows::runs_page(&mut events, community, workflow, cursor.as_deref()).await?;
        for run in runs {
            if channel.is_some_and(|c| c != run.channel) {
                continue;
            }
            let access = resolve_access(ctx, community, &run.channel, &caller.pubkey, now_ms).await;
            let readable = matches!(access, ChannelAccess::Open | ChannelAccess::Global)
                || matches!(
                    access,
                    ChannelAccess::Private {
                        reader_is_member: true
                    }
                );
            if readable {
                visible.push(run);
                if visible.len() >= limit {
                    break;
                }
            }
        }
        if visible.len() >= limit {
            break;
        }
        match next {
            Some(after) => cursor = Some(after),
            None => break,
        }
    }
    serde_json::to_vec(&serde_json::json!({ "ok": true, "runs": visible }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// How many schedules one pubkey may create per hour.
///
/// A schedule is future PROVIDER SPEND, and unlike a mention nobody is watching
/// when it fires. The per-agent cap bounds how many can be live; this bounds how
/// fast one caller can get there.
pub(super) const SCHEDULE_CREATE_RATE_LIMIT: i64 = 30;
pub(super) const SCHEDULE_CREATE_RATE_WINDOW_MS: u64 = 60 * 60 * 1000;

/// The harness a persona runs on, verified together with its right to post in
/// `channel`.
///
/// ONE call because the three questions are inseparable here. A schedule needs the
/// persona to be REGISTERED, or a wake cannot name a backend to run on. It needs
/// the HARNESS, because that is what the wake carries. And it needs the persona to
/// be able to POST in the room, because otherwise every occurrence spends a model
/// call to produce an answer the ingest gate then refuses — silent, recurring, and
/// billed.
///
/// That last check is about a DIFFERENT pubkey from the caller's, and in a private
/// room the two routinely disagree: a member can schedule a persona that is not in
/// the room. Checked before the row is written, because afterwards the cheapest
/// failure available is already an hourly one.
pub(crate) async fn persona_schedule_context(
    ctx: &AppContext,
    community: &str,
    agent: &str,
    channel: &str,
) -> Result<String> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    // STRICT here too. Not destructive, but refusing a create with "no agent named
    // X is registered" when the truth is "the registry does not parse" sends
    // whoever reads it to the wrong problem.
    let registry = registry_strict(&mut config).await?;
    let persona = registry
        .personas
        .iter()
        .find(|p| p.agent == agent)
        .ok_or_else(|| BuzzError::NotFound(format!("no agent named {agent:?} is registered")))?;
    let harness = persona.harness.clone();
    let pubkey = persona.pubkey;
    // Drop the registry borrow before the next plane call.
    drop(registry);
    // The FULL write gate, not just the channel. A persona removed from a closed
    // community, banned, or timed out passes the channel check in an open room and
    // then fails at ingest — so without this a schedule could be created whose every
    // occurrence spends a model call to produce an answer that is refused.
    enforce_write_gate(ctx, community, &pubkey, ctx.now_ms()).await?;
    authorize_channel_write(ctx, community, channel, &pubkey).await?;
    Ok(harness)
}

/// `POST /buzz/api/schedules` — wake a persona at a future instant.
///
/// Body: `{ agent, channel, prompt, first_due_ms, interval_ms?, occurrences? }`.
///
/// Gated as a WRITE, because that is what it is: a model call on this
/// deployment's own credential, made later, with nobody watching. So a banned or
/// timed-out member cannot leave one behind that keeps spending after they have
/// been silenced — and the channel must be one they may write to, since the
/// persona's answer will appear there under its own name.
pub(super) async fn schedules_create(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_write_gate(ctx, community, &caller.pubkey, now_ms).await?;
    match store::bump_expiring_counter(
        &mut config,
        &keys::rate(community, &caller.pubkey, "sched"),
        1,
        now_ms.saturating_add(SCHEDULE_CREATE_RATE_WINDOW_MS),
    )
    .await
    {
        Ok(count) if count > SCHEDULE_CREATE_RATE_LIMIT => return Err(BuzzError::RateLimited),
        Ok(_) => {}
        Err(e) => tracing::warn!("schedule limiter failed open: {e}"),
    }

    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("schedule body: {e}")))?;
    let agent = body
        .get("agent")
        .and_then(Value::as_str)
        .and_then(clean_agent_id)
        .ok_or_else(|| BuzzError::InvalidEvent("a schedule needs an 'agent' id".into()))?;
    let channel = body
        .get("channel")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    // The answer lands in this room under the persona's name, so the person
    // asking must be able to write there themselves — AND so must the persona,
    // which is a different pubkey and, in a private room, a different answer.
    authorize_channel_write(ctx, community, &channel, &caller.pubkey).await?;
    let harness = persona_schedule_context(ctx, community, &agent, &channel).await?;
    let request = crate::relay::schedules::ScheduleRequest {
        agent,
        channel,
        prompt: body
            .get("prompt")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        // Absent means "as soon as possible", which `build` normalises to now.
        first_due_ms: body
            .get("first_due_ms")
            .and_then(Value::as_u64)
            .unwrap_or(now_ms),
        interval_ms: body.get("interval_ms").and_then(Value::as_u64),
        occurrences: body
            .get("occurrences")
            .and_then(Value::as_u64)
            // SATURATE, never fall back to 1. `try_from().ok().unwrap_or(1)` turned
            // "run this forever" into a one-shot — the opposite of the request —
            // and the schedule module clamps to its own maximum anyway.
            .map_or(1, |n| u32::try_from(n).unwrap_or(u32::MAX)),
        created_by: crate::relay::schedules::member_creator(&caller.pubkey),
    };
    let schedule =
        crate::relay::schedules::create(ctx, &mut config, community, &request, &harness, now_ms)
            .await?;
    serde_json::to_vec(&serde_json::json!({ "ok": true, "schedule": schedule }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/schedules/list` — what is pending.
///
/// Members only, like the roster: what a workspace has told its agents to do on a
/// timer is a fact about the workspace, and in a closed community it is exactly
/// the list an outsider would like.
///
/// And per channel, like every other read: a schedule carries its prompt verbatim
/// and the channel it posts into, and creating one needed write access to that
/// channel, so a reader sees only schedules for channels they may read (plus any
/// they created themselves, so a creator can always find what to cancel). Before
/// this, a member outside a private room read the prompts written for it.
pub(super) async fn schedules_list(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    enforce_read_standing(ctx, community, &caller.pubkey, now_ms).await?;

    let (all, truncated) = crate::relay::schedules::list(&mut config, community).await?;
    // An optional agent filter, applied after the walk: the rows are keyed by
    // digest, so there is no per-agent index to walk instead.
    let body: Value = serde_json::from_slice(&req.body).unwrap_or(Value::Null);
    let wanted = body
        .get("agent")
        .and_then(Value::as_str)
        .and_then(clean_agent_id);
    let mut memo: std::collections::BTreeMap<String, ChannelAccess> =
        std::collections::BTreeMap::new();
    let mut rows = Vec::new();
    for schedule in all {
        if wanted.as_deref().is_some_and(|a| schedule.agent != a) {
            continue;
        }
        if !schedule.is_created_by_member(&caller.pubkey) {
            let access = match memo.get(&schedule.channel) {
                Some(a) => *a,
                None => {
                    let a =
                        resolve_access(ctx, community, &schedule.channel, &caller.pubkey, now_ms)
                            .await;
                    memo.insert(schedule.channel.clone(), a);
                    a
                }
            };
            let readable = matches!(
                access,
                ChannelAccess::Open
                    | ChannelAccess::Private {
                        reader_is_member: true
                    }
            );
            if !readable {
                continue;
            }
        }
        rows.push(schedule);
    }
    serde_json::to_vec(&serde_json::json!({
        "ok": true,
        "schedules": rows,
        // A truncated list must be able to say so rather than imply completeness.
        "complete": !truncated,
    }))
    .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// `POST /buzz/api/schedules/cancel` — stop a schedule.
///
/// Its creator, a community moderator, or an operator — the rule agent management
/// follows. The row is MARKED rather than deleted, because a wake already sitting
/// in the queue cannot be recalled and has to find something that says stop.
pub(super) async fn schedules_cancel(ctx: &AppContext, req: &IngressRequest) -> Result<Vec<u8>> {
    let now_ms = ctx.now_ms();
    let community = DEFAULT_COMMUNITY;
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    let caller = authenticate_request(&mut config, community, req, now_ms).await?;
    let body: Value = serde_json::from_slice(&req.body)
        .map_err(|e| BuzzError::InvalidEvent(format!("cancel body: {e}")))?;
    let id = body
        .get("schedule")
        .and_then(Value::as_str)
        .ok_or_else(|| BuzzError::InvalidEvent("this needs a 'schedule' id".into()))?;
    let operator = caller_is_operator(ctx, &mut config, &caller.pubkey).await?;
    let privileged =
        operator || is_community_moderator(&mut config, community, &caller.pubkey).await?;
    if !operator {
        super::write_gate(ctx, community, &caller.pubkey, now_ms).await?;
    }
    let cancelled = crate::relay::schedules::cancel(
        ctx,
        &mut config,
        community,
        id,
        &crate::relay::schedules::member_creator(&caller.pubkey),
        privileged,
    )
    .await?;
    serde_json::to_vec(&serde_json::json!({ "ok": true, "schedule": id, "cancelled": cancelled }))
        .map_err(|e| BuzzError::Internal(format!("reply encode: {e}")))
}

/// Whether a schedule's creator may still cause spend, and how permanent a
/// refusal is.
///
/// The distinction is the point. A schedule is authorized ONCE and then fires for
/// hours or weeks, so what matters at each wake is not "was this allowed" but
/// "is it still allowed, and if not, is that going to change".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum CreatorStanding {
    /// Still entitled to spend.
    Allowed,
    /// Temporarily not: a timeout, which lapses. Skip this occurrence and keep
    /// the schedule, exactly as a paused agent does.
    Suspended,
    /// Indefinitely not: banned, or removed from a closed community. The
    /// schedule should stop rather than skip forever.
    Revoked,
}

impl CreatorStanding {
    /// The stricter of two standings: `Revoked` beats `Suspended` beats `Allowed`.
    ///
    /// Explicit because the ORDERING is the rule. A wake weighs two independent
    /// standings — the creator's and the persona's — and a permanent refusal must
    /// not be softened into a temporary one by whichever check happened to run
    /// second.
    pub(crate) const fn strictest(self, other: Self) -> Self {
        match (self, other) {
            (Self::Revoked, _) | (_, Self::Revoked) => Self::Revoked,
            (Self::Suspended, _) | (_, Self::Suspended) => Self::Suspended,
            _ => Self::Allowed,
        }
    }
}

/// A schedule creator's CURRENT standing, asked at the moment a wake fires.
///
/// Deliberately not `enforce_write_gate`: that answers one boolean, and a wake
/// needs to tell a lapsing sanction from a permanent one. Built from the same two
/// sources in the same order, so it cannot disagree with what a write would do.
///
/// A storage failure PROPAGATES. Treating an unreadable restriction as "allowed"
/// would spend on a blip, and treating it as revoked would destroy a schedule
/// because a read failed; the caller leaves the wake unacked and retries.
pub(crate) async fn creator_standing(
    ctx: &AppContext,
    community: &str,
    who: &crate::relay::event::PubKey,
    now_ms: u64,
) -> Result<CreatorStanding> {
    match enforce_community_membership(ctx, community, who).await {
        Ok(()) => {}
        // Removed from (or never admitted to) a closed community.
        Err(BuzzError::Unauthorized(_)) => return Ok(CreatorStanding::Revoked),
        Err(e) => return Err(e),
    }
    let mut events = planes::store(ctx, store::EVENTS).map_err(storage)?;
    let restriction = crate::relay::effects::restriction_of(&mut events, community, who).await?;
    let standing = match restriction.and_then(|r| r.write_block(now_ms)) {
        None => CreatorStanding::Allowed,
        Some(crate::relay::effects::WriteBlock::Banned { .. }) => CreatorStanding::Revoked,
        // A timeout, which has an end.
        Some(_) => CreatorStanding::Suspended,
    };
    // An archived identity cannot write (see `enforce_write_gate`), so its
    // schedules must not spend on turns ingest would refuse. Suspended rather
    // than revoked: a moderator can lift an archive, and the schedule resumes.
    if standing == CreatorStanding::Allowed
        && crate::relay::archive::is_archived(&mut events, community, who).await?
    {
        return Ok(CreatorStanding::Suspended);
    }
    Ok(standing)
}

/// The PERSONA's own current standing, asked at the moment a wake fires.
///
/// A separate question from the creator's, and it can change after a schedule is
/// made: a persona can be retired from the registry, removed from a closed
/// community, or sanctioned like anybody else. Without this a banned persona keeps
/// being woken hourly, each occurrence spending a model call to produce an answer
/// the ingest gate then refuses.
///
/// An UNREGISTERED persona is `Revoked` rather than an error. Nothing can run a
/// wake for a persona that no longer exists, and skipping it forever is not the
/// same as saying so.
pub(crate) async fn persona_standing(
    ctx: &AppContext,
    community: &str,
    agent: &str,
    now_ms: u64,
) -> Result<CreatorStanding> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    // STRICT: a malformed registry must not read as "this persona was retired",
    // because the caller answers that by cancelling the schedule.
    let registry = registry_strict(&mut config).await?;
    let Some(persona) = registry.personas.iter().find(|p| p.agent == agent) else {
        return Ok(CreatorStanding::Revoked);
    };
    let pubkey = persona.pubkey;
    drop(registry);
    creator_standing(ctx, community, &pubkey, now_ms).await
}

/// [`agent_is_paused`], for the scheduled-wake path.
///
/// The pause route promises the persona stops answering, and a mention honours it
/// ([`maybe_dispatch`]). A scheduled wake has to honour the same flag or the
/// promise is only true of mentions — which is the shape of untruth this codebase
/// keeps removing.
pub(crate) async fn agent_paused(ctx: &AppContext, community: &str, agent: &str) -> Result<bool> {
    let mut config = planes::store(ctx, store::CONFIG).map_err(storage)?;
    Ok(agent_is_paused(&mut config, community, agent).await)
}
