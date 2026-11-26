//! # buzz-process-app — the process cell
//!
//! Branch and pull-request PROCESS ROOMS, and the mediated RCS lifecycle verbs
//! a workspace member cannot perform from a desktop, as one constrained
//! Ferrite app deployed in two provider-isolated profiles.
//!
//! ## Why this is a separate app from `buzz`
//!
//! The native profile holds the platform's ACCOUNT OWNER TOKEN — the one
//! credential that can deploy, withdraw, and read every secret in the account.
//! The relay must not hold it: a relay compromise would then be an account
//! compromise. So the token lives in the smallest app that can do the job, with
//! two egress destinations and one durable store, and everything it does to the
//! workspace it does through the workspace's OWN public API as an enrolled
//! member — subject to membership, moderation, rate limits and the audit chain,
//! exactly like a person.
//!
//! Precision about what that buys: the cell narrows WHERE the credential lives,
//! not what it can do. An exfiltrated token is an account-wide compromise until
//! it is rotated. What the split removes is the blast radius of a relay bug.
//!
//! ## The two profiles
//!
//! | profile | credential | what it does |
//! |---|---|---|
//! | `native` | platform owner token | polls RCS branches, materializes process rooms, executes member-authorized lifecycle verbs |
//! | `github` | webhook secret | verifies webhooks, reconciles PR/review/check state, projects NIP-34 events into rooms |
//!
//! Neither profile holds the other's credential, which is why they are two
//! deployments of one source package rather than one deployment with a flag.
//!
//! ## The shape of every decision
//!
//! Pure logic in [`binding`], [`command`], [`cells`], [`github`] and [`rcs`];
//! platform I/O only in [`planes`]. A cell's security properties — which room a
//! branch names, whether a webhook is authentic, whether a member may land —
//! are therefore decided by functions a test can call directly.

#![forbid(unsafe_code)]

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::time::Duration;

use ferrite_app::tracing;
use ferrite_app::{rt, serde_json, AppContext, IngressReply, IngressRequest};

pub mod b64;
pub mod binding;
pub mod buzzapi;
pub mod cells;
pub mod command;
mod delivery;
pub mod crypto;
pub mod crypto_kernel;
pub mod error;
pub mod event;
pub mod github;
mod github_cell;
mod lifecycle;
mod mutation;
mod state;
pub mod planes;
pub mod rcs;

use crate::github_cell::webhook_loop;
#[cfg(test)]
use crate::github_cell::{github_command_sweep, handle_webhook, webhook_step, DeliveryClaim};
use crate::binding::{Binding, Identity, Lifecycle, Scope};
use crate::buzzapi::{BuzzApi, NonceSource};
use crate::cells::{BranchChange, BranchState, Profile};
use crate::command::{Command, CommandResult, Outcome, Role, Verb};
use crate::error::{ProcessError, Result};
use crate::event::Event;
use crate::github::short;

pub const APP_ID: &str = "buzz-process-app";
/// The community every cell serves (one deployment is one community, as in
/// `buzz` itself).
pub const DEFAULT_COMMUNITY: &str = "main";

// Secrets, not config-store rows: every one of these is a credential or names
// a destination the platform must already have granted, so they arrive by
// injection and the app refuses to run without the ones its profile needs.

/// `native` | `github`.
pub const SECRET_PROFILE: &str = "buzz-process-profile";
/// The workspace origin this cell acts on.
pub const SECRET_ORIGIN: &str = "buzz-process-origin";
/// The cell's own enrolled member key (64 hex).
pub const SECRET_CELL_KEY: &str = "buzz-process-cell-key";
/// The community owner pubkey a binding's scope must match (64 hex).
pub const SECRET_COMMUNITY_OWNER: &str = "buzz-process-community-owner";
/// The platform control-plane endpoint (native).
pub const SECRET_CONTROL_ENDPOINT: &str = "buzz-process-control-endpoint";
/// The platform owner token (native). The one credential that matters.
pub const SECRET_CONTROL_TOKEN: &str = "buzz-process-control-token";
/// The `{owner}/{app}` RCS repository this cell drives (native).
pub const SECRET_RCS_APP: &str = "buzz-process-rcs-app";
/// The GitHub webhook secret (github).
pub const SECRET_WEBHOOK: &str = "buzz-process-webhook-secret";
/// The GitHub provider instance name recorded in bindings (github).
pub const SECRET_GITHUB_INSTANCE: &str = "buzz-process-github-instance";

/// How long the native cell waits between observations.
///
/// Polling converges; it is not an audit stream. Thirty seconds is a
/// deliberate compromise: fast enough that a room appears while somebody is
/// still looking at their terminal, slow enough that a quiet workspace costs
/// two control-plane calls a minute.
const POLL_INTERVAL_MS: u64 = 30_000;
/// How long the GitHub cell's queue poll parks.
const WEBHOOK_POLL_MS: u64 = 1_000;
/// Most commands answered in one cycle, so one busy room cannot starve others.
const MAX_COMMANDS_PER_CYCLE: usize = 16;
/// Events read per page of a room's timeline. Two pages are read per room
/// visit; the relay may return fewer when the events are large.
const TIMELINE_PAGE: u32 = 100;
/// How often the GitHub cell answers outstanding commands in its rooms.
///
/// The native cell has a poll cadence to hang this off; the GitHub cell is
/// delivery-driven, so it keeps its own deadline — checked every loop iteration,
/// busy or idle, because a queue with traffic in it never reports idle and
/// command answering must not wait for a quiet repository.
const GITHUB_COMMAND_SWEEP_MS: u64 = 30_000;

/// The order rooms are visited in, rotated by `cycle`.
///
/// The per-cycle cap ([`MAX_COMMANDS_PER_CYCLE`]) is what stops one busy room
/// monopolising a cycle — but both command loops walk a SORTED map, so with a
/// fixed order that same cap would starve every room after the first busy one,
/// which is a worse failure than the one it prevents. Rotating the start makes the
/// cap a bound on LATENCY rather than a wall: a room waits at most as many cycles
/// as there are rooms ahead of it.
#[must_use]
fn rotated<T: Clone>(items: &[T], cycle: usize) -> Vec<T> {
    if items.is_empty() {
        return Vec::new();
    }
    let start = cycle % items.len();
    items[start..]
        .iter()
        .chain(items[..start].iter())
        .cloned()
        .collect()
}

/// The durable key for the native cell's observation state.
fn state_key(community: &str, app: &str) -> Vec<u8> {
    format!("branchstate/{community}/{app}").into_bytes()
}

/// The observation key for ONE ref of ONE repository.
///
/// The GitHub cell holds a single state document per provider instance, so a key
/// of the branch name alone is shared by every repository on that instance — and
/// every repository has a `main`. Deleting one repository's `main` would then bump
/// the incarnation counter of every other's, and the next push to a branch nobody
/// touched would be projected into a NEW room while its old one was archived for
/// nothing.
///
/// The head repository is part of the key when there is one, by the same
/// argument: a fork's `feature/x` is not the base repository's `feature/x`, which
/// is exactly what [`Identity::github`] already says about the binding.
#[must_use]
fn observation_key(stable_id: &str, head_stable_id: Option<&str>, git_ref: &str) -> String {
    match head_stable_id {
        Some(head) => format!("{stable_id}/{head}:{git_ref}"),
        None => format!("{stable_id}:{git_ref}"),
    }
}

/// Read an observation key back into the identity components it names.
///
/// The ref is split off at the FIRST colon, because a git ref may not contain one
/// (git's own `check-ref-format` forbids it) while a stable id is a provider's
/// opaque string — so the ref side is the unambiguous half. `None` for anything
/// that is not one of these keys, which is how a state document written by an
/// older build is skipped rather than misread.
#[must_use]
fn parse_observation_key(key: &str) -> Option<(String, Option<String>, String)> {
    let (repository, git_ref) = key.split_once(':')?;
    if repository.is_empty() || !git_ref.starts_with("refs/") {
        return None;
    }
    match repository.split_once('/') {
        Some((stable_id, head)) if !stable_id.is_empty() && !head.is_empty() => Some((
            stable_id.to_string(),
            Some(head.to_string()),
            git_ref.to_string(),
        )),
        Some(_) => None,
        None => Some((repository.to_string(), None, git_ref.to_string())),
    }
}

/// The GitHub cell's answer to one mediated verb.
///
/// PURE, so the whole answer table is testable, and deliberately EXHAUSTIVE: a
/// command nobody answers leaves a member watching a room where nothing ever
/// happens, which is the silence this design forbids everywhere else — a refusal
/// with a reason, never a silent skip.
///
/// The mutating verbs are refused rather than attempted because the GitHub WRITE
/// path is not implemented: it needs GitHub App authentication, which is an open
/// decision, and a cell that quietly did nothing would be
/// indistinguishable from one that had done the work.
#[must_use]
fn github_answer(command_id: &str, verb: Verb, binding_id: &str) -> CommandResult {
    match verb {
        // Truthful rather than a promise of work: this cell's state comes from
        // deliveries, so there is genuinely nothing to re-poll.
        Verb::Refresh => CommandResult::new(
            command_id,
            verb,
            binding_id,
            Outcome::Ok,
            "this room reconciles from GitHub webhook deliveries, so there is nothing \
             to re-poll",
        ),
        Verb::Branch | Verb::Commit | Verb::Merge | Verb::Land => CommandResult::new(
            command_id,
            verb,
            binding_id,
            Outcome::Refused,
            "this cell does not write to GitHub yet: that needs GitHub App \
             authentication, which is an open decision. Do it on GitHub, or from a \
             working copy in the desktop app",
        ),
        Verb::BindingCreate | Verb::BindingArchive => CommandResult::new(
            command_id,
            verb,
            binding_id,
            Outcome::Refused,
            "binding lifecycle is the native cell's: this cell materializes a room \
             from a delivery and does not retire one on request",
        ),
    }
}

pub async fn run(ctx: AppContext) -> io::Result<()> {
    let profile = Profile::parse(&planes::secret(&ctx, SECRET_PROFILE)?)?;
    tracing::info!(
        app = APP_ID,
        profile = profile.as_str(),
        "process cell starting"
    );
    match profile {
        Profile::Native => native_loop(ctx).await,
        Profile::Github => webhook_loop(ctx).await,
    }
}

/// Everything a cell needs to act on the workspace.
struct CellContext {
    api: BuzzApi,
    nonces: NonceSource,
    /// The origin the api was built with. Carried here because a binding's
    /// scope records it and `BuzzApi` keeps its own copy private.
    origin: String,
    community_owner: String,
    community: String,
}

impl CellContext {
    fn build(ctx: &AppContext, seed: &str) -> Result<Self> {
        let origin = buzzapi::validate_origin(&planes::secret(ctx, SECRET_ORIGIN)?)?;
        let api = BuzzApi::new(&origin, planes::secret_key(ctx, SECRET_CELL_KEY)?)?;
        let community_owner = planes::secret(ctx, SECRET_COMMUNITY_OWNER)?;
        if crypto::from_hex32(&community_owner).is_none() {
            return Err(ProcessError::Invalid(format!(
                "the {SECRET_COMMUNITY_OWNER} secret is not 64 hex characters"
            )));
        }
        Ok(Self {
            api,
            nonces: NonceSource::new(seed),
            origin,
            community_owner,
            community: DEFAULT_COMMUNITY.to_string(),
        })
    }

    fn scope(&self) -> Scope {
        Scope {
            buzz_origin: self.origin.clone(),
            community_owner_pubkey: self.community_owner.clone(),
            visibility: "public".to_string(),
        }
    }
}

async fn publish(ctx: &AppContext, cell: &CellContext, event: &Event) -> Result<u16> {
    let call = cell
        .api
        .publish(event, ctx.now_ms() / 1000, &cell.nonces.next())?;
    let (status, body) = planes::deliver(ctx, planes::BUZZ_EGRESS, &call).await?;
    if !(200..300).contains(&status) {
        return Err(ProcessError::Egress(format!(
            "the workspace refused an event with {status}: {}",
            String::from_utf8_lossy(&body)
                .chars()
                .take(200)
                .collect::<String>()
        )));
    }
    Ok(status)
}

/// One page of a room's timeline, newest first, and the relay's cursor for
/// the page after it (`None` past the room's oldest event). The cursor is the
/// relay's own index position, so a walk by it passes every event once,
/// however many share a second.
///
/// Only the page's process commands and results come back, each
/// [`verified`]: they are all the cell acts on, and checking a signature costs
/// milliseconds, so the room's chatter is dropped unread rather than verified.
async fn timeline_page(
    ctx: &AppContext,
    cell: &CellContext,
    room: &str,
    cursor: Option<&str>,
) -> Result<(Vec<Event>, Option<String>)> {
    let (events, next) = timeline_rows(ctx, cell, room, cursor).await?;
    let process = events
        .into_iter()
        .filter(|e| e.kind == command::KIND_PROCESS_COMMAND || e.kind == command::KIND_PROCESS_RESULT)
        .collect();
    Ok((verified(process), next))
}

/// One page of a room's timeline as the relay sent it, every kind and
/// UNVERIFIED: a caller keeps the rows it wants and passes those through
/// [`verified`] before weighing them.
async fn timeline_rows(
    ctx: &AppContext,
    cell: &CellContext,
    room: &str,
    cursor: Option<&str>,
) -> Result<(Vec<Event>, Option<String>)> {
    let call = cell.api.query(
        &command::timeline_query(room, cursor, TIMELINE_PAGE),
        ctx.now_ms() / 1000,
        &cell.nonces.next(),
    )?;
    let (status, body) = planes::deliver(ctx, planes::BUZZ_EGRESS, &call).await?;
    if !(200..300).contains(&status) {
        return Err(ProcessError::Egress(format!(
            "the workspace refused a timeline read with {status}"
        )));
    }
    #[derive(ferrite_app::serde::Deserialize)]
    #[serde(crate = "ferrite_app::serde")]
    struct Page {
        events: Vec<Event>,
        next_cursor: Option<String>,
        complete: bool,
    }
    let page: Page = serde_json::from_slice(&body)
        .map_err(|e| ProcessError::Egress(format!("invalid timeline reply: {e}")))?;
    if page.complete != page.next_cursor.is_none() {
        return Err(ProcessError::Egress("inconsistent timeline completion".into()));
    }
    Ok((page.events, page.next_cursor))
}

async fn filter_query(
    ctx: &AppContext,
    cell: &CellContext,
    filters: &[serde_json::Value],
) -> Result<Vec<Event>> {
    let call = cell
        .api
        .filter_query(filters, ctx.now_ms() / 1000, &cell.nonces.next())?;
    let (status, body) = planes::deliver(ctx, planes::BUZZ_EGRESS, &call).await?;
    if !(200..300).contains(&status) {
        return Err(ProcessError::Egress(format!(
            "the workspace refused a query with {status}"
        )));
    }
    let value: serde_json::Value = serde_json::from_slice(&body)
        .map_err(|e| ProcessError::Egress(format!("query reply is not JSON: {e}")))?;
    let serde_json::Value::Array(rows) = value else {
        return Err(ProcessError::Egress("query reply is not an array".into()));
    };
    Ok(decode_events(rows))
}

/// Decode reply rows. A malformed row is skipped rather than failing the
/// whole read: one unparseable row must not stop the cell answering the
/// others. So is a row that fails [`verified`].
fn decode_events(rows: Vec<serde_json::Value>) -> Vec<Event> {
    verified(
        rows.into_iter()
            .filter_map(|row| serde_json::from_value::<Event>(row).ok())
            .collect(),
    )
}

/// The events whose id is the hash of their fields and whose signature is
/// their stated author's ([`Event::verify`]); the rest are dropped.
///
/// Every read from the relay passes through here before anything weighs an
/// event's pubkey or tags. Authority (whose command this is), answer-once
/// (whether this cell already answered) and the binding's lifecycle are all
/// decided from the author an event NAMES, and a name is only worth what its
/// signature proves: the relay verifies on admission, but a cell that took
/// its word for it would execute a forged command with a member's authority,
/// or let a forged result silence a real one.
fn verified(events: Vec<Event>) -> Vec<Event> {
    events.into_iter().filter(|event| event.verify().is_ok()).collect()
}

enum AnswerWork {
    Execute(Event),
    Publish(Event, AnswerRecord),
}

impl AnswerWork {
    fn request(&self) -> &Event {
        match self {
            Self::Execute(request) | Self::Publish(request, _) => request,
        }
    }
}

struct Visit {
    owed: Vec<AnswerWork>,
    walk_owed: Vec<String>,
    next: Option<String>,
}

#[derive(ferrite_app::serde::Serialize, ferrite_app::serde::Deserialize)]
#[serde(crate = "ferrite_app::serde")]
enum AnswerIndex {
    Scanning { cursor: String },
    Complete,
}

async fn index_legacy_answers(ctx: &AppContext, cell: &CellContext, room: &str) -> Result<bool> {
    let key = format!("answer-index/v1/{}/{room}", cell.api.pubkey_hex()).into_bytes();
    let progress = planes::load::<AnswerIndex>(ctx, &key).await?;
    let cursor = match &progress {
        Some(AnswerIndex::Complete) => return Ok(true),
        Some(AnswerIndex::Scanning { cursor }) => Some(cursor.as_str()),
        None => None,
    };
    let (events, next) = timeline_page(ctx, cell, room, cursor).await?;
    for event in events {
        if event.kind != command::KIND_PROCESS_RESULT
            || !event.pubkey.eq_ignore_ascii_case(cell.api.pubkey_hex())
            || event.channel() != Some(room)
        {
            continue;
        }
        if let Some(id) = event.first_tag("e") {
            let key = answer_key(room, id);
            let record = AnswerRecord { event, summary: String::new(), published: true };
            let encoded = serde_json::to_vec(&record)
                .map_err(|e| ProcessError::Internal(format!("answer encode: {e}")))?;
            planes::claim_once(ctx, &key, &encoded).await?;
        }
    }
    let progress = match next {
        Some(cursor) => AnswerIndex::Scanning { cursor },
        None => AnswerIndex::Complete,
    };
    planes::save(ctx, &key, &progress).await?;
    Ok(matches!(progress, AnswerIndex::Complete))
}

async fn visit_room(
    ctx: &AppContext,
    cell: &CellContext,
    room: &str,
    state: &BranchState,
) -> Result<Option<Visit>> {
    if !index_legacy_answers(ctx, cell, room).await? {
        return Ok(None);
    }
    let (newest, newest_next) = timeline_page(ctx, cell, room, None).await?;
    let (walked, next) = match state.command_walk.get(room) {
        Some(walk) => timeline_page(ctx, cell, room, Some(&walk.cursor)).await?,
        None => (Vec::new(), newest_next),
    };
    let walk_page = if state.command_walk.contains_key(room) {
        &walked
    } else {
        &newest
    };
    let my_pubkey = cell.api.pubkey_hex();
    let results: Vec<Event> = newest
        .iter()
        .chain(walked.iter())
        .filter(|e| {
            e.kind == command::KIND_PROCESS_RESULT
                && e.pubkey.eq_ignore_ascii_case(my_pubkey)
                && e.channel() == Some(room)
        })
        .cloned()
        .collect();
    let mut commands: Vec<Event> = Vec::new();
    for event in newest.iter().chain(walked.iter()) {
        if event.kind == command::KIND_PROCESS_COMMAND
            && event.channel() == Some(room)
            && !commands.iter().any(|known| known.id == event.id)
        {
            commands.push(event.clone());
        }
    }
    let mut unrecorded = Vec::new();
    let mut owed = Vec::new();
    for command in commands {
        let key = answer_key(room, &command.id);
        match planes::load::<AnswerRecord>(ctx, &key).await? {
            Some(record) if !record.published => owed.push(AnswerWork::Publish(command, record)),
            Some(_) => {}
            None if !state.has_answered(&command.id) => unrecorded.push(command),
            None => {}
        }
    }
    owed.extend(cells::unanswered(unrecorded, &results).into_iter().map(AnswerWork::Execute));
    owed.sort_by(|a, b| {
        let (a, b) = (a.request(), b.request());
        a.created_at.cmp(&b.created_at).then_with(|| a.id.cmp(&b.id))
    });
    let walk_owed = owed
        .iter()
        .map(AnswerWork::request)
        .filter(|c| walk_page.iter().any(|e| e.id == c.id))
        .map(|c| c.id.clone())
        .collect();
    Ok(Some(Visit { owed, walk_owed, next }))
}

fn finish_visit(
    state: &mut BranchState,
    room: &str,
    visit: Visit,
    done: &BTreeSet<String>,
    now_ms: u64,
    retired: Option<(&str, &str)>,
) {
    let drained = visit.walk_owed.iter().all(|id| done.contains(id));
    let nothing_owed = visit.owed.iter().all(|work| done.contains(&work.request().id));
    if let Some(started_ms) = state.advance_walk(room, visit.next, drained, now_ms) {
        if let (Some((name, generation)), true) = (retired, nothing_owed) {
            state.close_retired(name, generation, started_ms);
        }
    }
}

/// How many roster pages a role read follows before giving up.
const ROSTER_PAGE_LIMIT: usize = 256;

async fn roster(ctx: &AppContext, cell: &CellContext, cursor: Option<&str>) -> Result<serde_json::Value> {
    let call = cell.api.members(cursor, ctx.now_ms() / 1000, &cell.nonces.next())?;
    // The members read is a GET, and its NIP-98 authorization signs that
    // method: sending it as a POST fails every roster read.
    let (status, body) = planes::fetch(ctx, planes::BUZZ_EGRESS, &call).await?;
    if !(200..300).contains(&status) {
        return Err(ProcessError::Egress(format!(
            "the workspace refused a roster read with {status}"
        )));
    }
    serde_json::from_slice(&body).map_err(|e| ProcessError::Egress(format!("roster is not JSON: {e}")))
}

/// The requester's role, read fresh from the roster.
///
/// Fresh on purpose: authority is weighed when the verb RUNS, not when the
/// command was written, so a member demoted since asking cannot land.
async fn role_of(ctx: &AppContext, cell: &CellContext, pubkey_hex: &str) -> Result<Option<Role>> {
    // The relay pages the roster: a member past the first page still counts.
    let mut cursor: Option<String> = None;
    for _ in 0..ROSTER_PAGE_LIMIT {
        let page = roster(ctx, cell, cursor.as_deref()).await?;
        if let Some(role) = buzzapi::role_in_roster(&page, pubkey_hex) {
            return Ok(Some(role));
        }
        match buzzapi::next_roster_cursor(&page) {
            Some(next) => cursor = Some(next),
            None => return Ok(None),
        }
    }
    Err(ProcessError::Egress("the roster has more pages than a role read follows".into()))
}

/// Materialize a process room for a branch incarnation: create the channel,
/// publish its binding, and post the line that says what happened.
///
/// Convergent rather than exclusive: the room id is derived from the binding
/// identity, so two cells (or two cycles) racing produce one room and one
/// conflict, and the conflict is not an error.
///
/// `created_at` is passed IN rather than read from the clock, because a retried
/// projection must rebuild byte-identical events: their ids are then the ones
/// the relay has already admitted, and its own dedup absorbs the repeat. A
/// fresh instant per attempt would publish the same message twice under two
/// ids — determinism is the dedup here, exactly as it is for workflows.
async fn materialize_room(
    ctx: &AppContext,
    cell: &CellContext,
    identity: &Identity,
    display: &str,
    clone_url: Option<&str>,
    summary: &str,
    created_at: u64,
    operation: &str,
) -> Result<String> {
    let room = identity.room_id()?;
    create_room(ctx, cell, &room, display, created_at).await?;

    let (_, binding_at) = publish_binding(ctx, cell, identity, display, clone_url,
        Lifecycle::Active, created_at, operation).await?;
    let seckey = planes::secret_key(ctx, SECRET_CELL_KEY)?;
    let note = Event::sign(
        &seckey,
        github::KIND_STREAM_MESSAGE,
        vec![vec!["h".to_string(), room.clone()]],
        summary,
        binding_at,
    )?;
    publish(ctx, cell, &note).await?;
    Ok(room)
}

/// Post one line into a room at a given instant (see [`materialize_room`] for
/// why a retried effect must not take a fresh one).
async fn say_at(
    ctx: &AppContext,
    cell: &CellContext,
    room: &str,
    text: &str,
    created_at: u64,
) -> Result<()> {
    let seckey = planes::secret_key(ctx, SECRET_CELL_KEY)?;
    let event = Event::sign(
        &seckey,
        github::KIND_STREAM_MESSAGE,
        vec![vec!["h".to_string(), room.to_string()]],
        text,
        created_at,
    )?;
    publish(ctx, cell, &event).await.map(|_| ())
}

async fn create_room(ctx: &AppContext, cell: &CellContext, room: &str, display: &str, created_at: u64) -> Result<()> {
    // Open: a private room enrolls only the cell, which locks every
    // workspace member out of the room meant for them.
    let call = cell.api.create_channel(room, display, false, created_at, &cell.nonces.next())?;
    let (status, _) = planes::deliver(ctx, planes::BUZZ_EGRESS, &call).await?;
    if !(200..300).contains(&status) && status != 409 {
        return Err(ProcessError::Egress(format!("the workspace refused a room create with {status}")));
    }
    Ok(())
}

async fn reserve_binding_event(
    ctx: &AppContext,
    cell: &CellContext,
    identity: &Identity,
    display: &str,
    clone_url: Option<&str>,
    lifecycle: Lifecycle,
    created_at: u64,
    operation: &str,
) -> Result<Event> {
    let mut binding = Binding::new(identity, display, None, clone_url, None, cell.scope())?;
    binding.lifecycle = lifecycle;
    let seckey = planes::secret_key(ctx, SECRET_CELL_KEY)?;
    let event = Event::sign(
        &seckey,
        github::KIND_STREAM_MESSAGE,
        binding.tags(),
        &binding.to_json()?,
        created_at,
    )?;
    lifecycle::reserve(ctx, cell, identity, operation, event).await
}

async fn publish_binding(
    ctx: &AppContext,
    cell: &CellContext,
    identity: &Identity,
    display: &str,
    clone_url: Option<&str>,
    lifecycle: Lifecycle,
    created_at: u64,
    operation: &str,
) -> Result<(String, u64)> {
    let event = reserve_binding_event(ctx, cell, identity, display, clone_url, lifecycle, created_at, operation).await?;
    publish(ctx, cell, &event).await?;
    Ok((identity.room_id()?, event.created_at))
}

/// The native cell: observe RCS, materialize rooms, answer commands.
async fn native_loop(ctx: AppContext) -> io::Result<()> {
    let app = planes::secret(&ctx, SECRET_RCS_APP)?;
    let control = rcs::ControlPlane::new(
        &planes::secret(&ctx, SECRET_CONTROL_ENDPOINT)?,
        &planes::secret(&ctx, SECRET_CONTROL_TOKEN)?,
        &app,
    )?;
    // The nonce seed must be unique per instance-lifetime: the app id plus the
    // boot instant is, and it is stable for the life of this process.
    let seed = format!("{APP_ID}/native/{app}/{}", ctx.now_ms());
    let cell = CellContext::build(&ctx, &seed)?;
    let key = state_key(&cell.community, &app);
    // Which cycle this is, so the room order can rotate (see `rotated`).
    let mut cycle = 0usize;

    loop {
        match native_cycle(&ctx, &cell, &control, &key, cycle).await {
            Ok(changes) if changes > 0 => {
                tracing::info!("reconciled {changes} branch change(s)");
            }
            Ok(_) => {}
            Err(e) => tracing::warn!("native cycle: {e}"),
        }
        cycle = cycle.wrapping_add(1);
        rt::time::sleep(Duration::from_millis(POLL_INTERVAL_MS)).await;
    }
}

/// One observation and its effects. Returns how many changes were reconciled.
async fn native_cycle(
    ctx: &AppContext,
    cell: &CellContext,
    control: &rcs::ControlPlane,
    key: &[u8],
    cycle: usize,
) -> Result<usize> {
    let stored = planes::load_versioned::<BranchState>(ctx, key).await?;
    let call = control.branches();
    let (status, body) = planes::deliver(ctx, planes::CONTROL_EGRESS, &call).await?;
    let reply = rcs::parse_reply(status, &body)?;
    let rcs::RcsReply::Branches(rows) = reply else {
        return Err(ProcessError::Egress(
            "the control plane answered something other than a branch list".into(),
        ));
    };
    let current: BTreeMap<String, String> = rows.into_iter().collect();

    let (mut state, version) = stored.map_or((BranchState::default(), None), |(state, version)| (state, Some(version)));
    let changes = state.observe(current);
    let now_ms = ctx.now_ms();
    for change in &changes {
        if let BranchChange::Retired { name, .. } = change {
            state.note_retired(name, &change.generation(), now_ms);
        }
        state.observation_sequence = state.observation_sequence.checked_add(1)
            .ok_or_else(|| ProcessError::Storage("observation sequence exhausted".into()))?;
        state.pending_observations.push(cells::PendingObservation {
            sequence: state.observation_sequence,
            at_ms: now_ms,
            change: change.clone(),
        });
    }
    if !planes::save_if_version(ctx, key, &state, version).await? {
        return Err(ProcessError::Storage("branch state changed during observation; resample before publishing".into()));
    }
    let account = control
        .app()
        .split('/')
        .next()
        .unwrap_or("account")
        .to_string();

    let mut completed = 0;
    while let Some(pending) = state.pending_observations.first().cloned() {
        let change = &pending.change;
        let git_ref = format!("refs/heads/{}", change.name());
        let identity = Identity::rcs(&account, control.app(), &git_ref, &change.generation());
        match change {
            BranchChange::Appeared { name, tip, .. } => {
                let room = materialize_room(
                    ctx,
                    cell,
                    &identity,
                    &format!("{} · {name}", control.app()),
                    None,
                    &format!(
                        "Branch {name} appeared at {} (observed on RCS).",
                        short(tip)
                    ),
                    pending.at_ms / 1000,
                    &format!("observation/{}", pending.sequence),
                )
                .await?;
                tracing::info!("process room {room} materialized for {name}");
            }
            BranchChange::Advanced { name, from, to, .. } => {
                let room = identity.room_id()?;
                publish_observation_note(
                    ctx,
                    cell,
                    &room,
                    &format!(
                        "Branch {name} moved {} → {} (observed on RCS; polling converges on \
                         current state and may not have seen every intermediate move).",
                        short(from),
                        short(to)
                    ),
                    &pending,
                )
                .await?;
            }
            BranchChange::Retired { name, last_tip, .. } => {
                let room = identity.room_id()?;
                publish_observation_note(
                    ctx,
                    cell,
                    &room,
                    &format!(
                        "Branch {name} is gone (last seen at {}). This room is now history.",
                        short(last_tip)
                    ),
                    &pending,
                )
                .await?;
            }
        }
        state = state::update(ctx, key, |current| {
            current.pending_observations.retain(|observation| observation.sequence != pending.sequence);
            Ok(())
        }).await?;
        completed += 1;
    }

    // Live rooms get their commands run; the rooms of retired lifetimes get a
    // refusal, so nobody waits in a room that is history. The order ROTATES
    // per cycle — see `rotated` for why a fixed order would turn the cap below
    // into starvation.
    let before_commands = state.clone();
    let rooms = rooms_to_answer(&state);
    let mut answered = 0usize;
    for (name, generation, live) in rotated(&rooms, cycle) {
        let budget = MAX_COMMANDS_PER_CYCLE.saturating_sub(answered);
        if budget == 0 {
            break;
        }
        let git_ref = format!("refs/heads/{name}");
        let identity = Identity::rcs(&account, control.app(), &git_ref, &generation);
        answered += if live {
            answer_commands(ctx, cell, control, &identity, &mut state, budget).await?
        } else {
            answer_retired(ctx, cell, control, &identity, &name, &mut state, budget).await?
        };
    }

    state::save_commands(ctx, key, &before_commands, &state).await?;
    Ok(completed + answered)
}

async fn publish_observation_note(
    ctx: &AppContext,
    cell: &CellContext,
    room: &str,
    text: &str,
    pending: &cells::PendingObservation,
) -> Result<()> {
    let event = Event::sign(
        &planes::secret_key(ctx, SECRET_CELL_KEY)?,
        github::KIND_STREAM_MESSAGE,
        vec![vec!["h".into(), room.into()], vec!["process-observation".into(), pending.sequence.to_string()]],
        text,
        pending.at_ms / 1000,
    )?;
    publish(ctx, cell, &event).await.map(|_| ())
}

/// The rooms a cell answers commands in, as `(name, generation, live)`: each
/// live ref's current lifetime, then each retired lifetime still kept.
fn rooms_to_answer(state: &BranchState) -> Vec<(String, String, bool)> {
    let mut rooms: Vec<(String, String, bool)> = state
        .tips
        .keys()
        .map(|name| (name.clone(), state.generation(name), true))
        .collect();
    rooms.extend(
        state
            .retired
            .iter()
            .map(|r| (r.name.clone(), r.generation.clone(), false)),
    );
    rooms
}

/// The commit message a MEDIATED commit carries.
///
/// The commit itself is made by the platform — its owner token is the only
/// credential that can write to RCS — so without a trailer the git history would
/// attribute a member's work to the cell, and nothing in the REPOSITORY would say
/// whose it was. The room records the signed request either way, but somebody
/// reading `rcs_log` months later is not reading the room.
///
/// The trailer is CHECKABLE rather than decorative: `Buzz-Command` names the
/// command event, which is signed by `Requested-by` and whose id is the
/// idempotency key the result event carries. Anyone can fetch it and verify the
/// claim.
///
/// Deterministic, because a re-driven effect must rebuild byte-identical bytes:
/// the same request always produces the same message.
#[must_use]
fn commit_message(message: &str, requester_pubkey: &str, command_id: &str) -> String {
    let body = message.trim_end();
    let attribution = format!("Requested-by: {requester_pubkey}\nBuzz-Command: {command_id}");
    if body.is_empty() {
        // A commit still needs a subject line; the verb is the honest one.
        return format!("rcs_commit (mediated)\n\n{attribution}\n");
    }
    // A blank line before the trailer block, which is what makes it a trailer
    // rather than the last line of the message.
    format!("{body}\n\n{attribution}\n")
}

/// The verb an unparseable command APPEARS to name — best effort, so a refusal
/// record still says what was attempted. Falls back to `Refresh` (the harmless
/// read-only verb) when even that cannot be read, and the detail line says so.
fn attempted_verb(content: &str) -> Verb {
    serde_json::from_str::<serde_json::Value>(content)
        .ok()
        .and_then(|v| {
            v.get("verb")
                .and_then(|verb| verb.as_str())
                .and_then(Verb::parse)
        })
        .unwrap_or(Verb::Refresh)
}

/// Answer up to `budget` of the outstanding commands in one live room.
async fn answer_commands(
    ctx: &AppContext,
    cell: &CellContext,
    control: &rcs::ControlPlane,
    identity: &Identity,
    state: &mut BranchState,
    budget: usize,
) -> Result<usize> {
    let room = identity.room_id()?;
    let binding_id = identity.binding_id()?;
    let Some(visit) = visit_room(ctx, cell, &room, state).await? else {
        return Ok(0);
    };
    let mut done = BTreeSet::new();
    for work in &visit.owed {
        if done.len() >= budget {
            break;
        }
        let request = work.request();
        if let AnswerWork::Publish(_, record) = work {
            publish_answer(ctx, cell, &room, &answer_key(&room, &request.id), record.clone()).await?;
            state.mark_answered(&request.id);
            done.insert(request.id.clone());
            continue;
        }
        let result = execute(ctx, cell, control, identity, state, &binding_id, request).await;
        let result = match result {
            Ok(result) => result,
            // An unknown mutation outcome the room has been told about: no
            // result is published, but nothing more is owed, so the walk and
            // retired-room cleanup move past it.
            Err(e) if e.is_settled_unknown() => {
                tracing::warn!("command {} settled with an unknown outcome: {e}", request.id);
                state.mark_answered(&request.id);
                done.insert(request.id.clone());
                continue;
            }
            // A TRANSIENT fault is left for the next cycle: answering "failed"
            // when the truth is "the network blipped" would tell a member their
            // land was refused.
            Err(e) if !e.is_final() => {
                tracing::warn!("command {} deferred: {e}", request.id);
                continue;
            }
            Err(e) => CommandResult::new(
                &request.id,
                attempted_verb(&request.content),
                &binding_id,
                Outcome::Refused,
                &e.to_string(),
            ),
        };
        record_answer(ctx, cell, &room, &result, state).await?;
        done.insert(request.id.clone());
    }
    finish_visit(state, &room, visit, &done, ctx.now_ms(), None);
    Ok(done.len())
}

/// Answer up to `budget` outstanding commands in the room of a RETIRED branch
/// lifetime (`name`'s, at `identity`'s generation): nothing is done there,
/// and the member is told so rather than left waiting.
async fn answer_retired(
    ctx: &AppContext,
    cell: &CellContext,
    control: &rcs::ControlPlane,
    identity: &Identity,
    name: &str,
    state: &mut BranchState,
    budget: usize,
) -> Result<usize> {
    let room = identity.room_id()?;
    let binding_id = identity.binding_id()?;
    let Some(visit) = visit_room(ctx, cell, &room, state).await? else {
        return Ok(0);
    };
    let mut done = BTreeSet::new();
    for work in &visit.owed {
        if done.len() >= budget {
            break;
        }
        let request = work.request();
        if let AnswerWork::Publish(_, record) = work {
            publish_answer(ctx, cell, &room, &answer_key(&room, &request.id), record.clone()).await?;
            state.mark_answered(&request.id);
            done.insert(request.id.clone());
            continue;
        }
        let existing = match Command::parse(&request.content) {
            Ok(command) if mutation::owns(command.verb) => {
                match mutation::resume(ctx, cell, control, &command, request).await {
                    Ok(existing) => existing,
                    Err(error) if error.is_settled_unknown() => {
                        tracing::warn!("retired command {} settled with an unknown outcome: {error}", request.id);
                        state.mark_answered(&request.id);
                        done.insert(request.id.clone());
                        continue;
                    }
                    Err(error) => {
                        tracing::warn!("retired command {} remains unresolved: {error}", request.id);
                        continue;
                    }
                }
            }
            _ => None,
        };
        let result = existing.unwrap_or_else(|| {
            retired_answer(&request.id, attempted_verb(&request.content), &binding_id)
        });
        record_answer(ctx, cell, &room, &result, state).await?;
        done.insert(request.id.clone());
    }
    let retired = Some((name, identity.generation.as_str()));
    finish_visit(state, &room, visit, &done, ctx.now_ms(), retired);
    Ok(done.len())
}

#[must_use]
fn retired_answer(command_id: &str, verb: Verb, binding_id: &str) -> CommandResult {
    CommandResult::new(
        command_id,
        verb,
        binding_id,
        Outcome::Refused,
        "this branch is gone, so this room is history and nothing is done here; ask in \
         the room of the branch's current lifetime",
    )
}

#[derive(
    Debug, Clone, PartialEq, Eq, ferrite_app::serde::Serialize, ferrite_app::serde::Deserialize,
)]
#[serde(crate = "ferrite_app::serde")]
struct AnswerRecord {
    event: Event,
    summary: String,
    published: bool,
}

fn answer_key(room: &str, command_id: &str) -> Vec<u8> {
    format!("answer/{room}/{command_id}").into_bytes()
}

/// A crash after the record is written republishes the same bytes, whose ids
/// the relay has already admitted.
async fn record_answer(
    ctx: &AppContext,
    cell: &CellContext,
    room: &str,
    result: &CommandResult,
    state: &mut BranchState,
) -> Result<()> {
    let seckey = planes::secret_key(ctx, SECRET_CELL_KEY)?;
    let event = Event::sign(
        &seckey,
        command::KIND_PROCESS_RESULT,
        result.tags(room),
        &result.to_content()?,
        ctx.now_ms() / 1000,
    )?;
    let record = AnswerRecord {
        event,
        summary: result.summary(),
        published: false,
    };
    let key = answer_key(room, &result.command);
    let encoded = serde_json::to_vec(&record)
        .map_err(|e| ProcessError::Internal(format!("answer encode: {e}")))?;
    let record = if planes::claim_once(ctx, &key, &encoded).await? {
        record
    } else {
        planes::load::<AnswerRecord>(ctx, &key)
            .await?
            .ok_or_else(|| ProcessError::Storage("an answer record vanished after its claim".into()))?
    };
    publish_answer(ctx, cell, room, &key, record).await?;
    state.mark_answered(&result.command);
    Ok(())
}

async fn publish_answer(
    ctx: &AppContext,
    cell: &CellContext,
    room: &str,
    key: &[u8],
    mut record: AnswerRecord,
) -> Result<()> {
    if record.published {
        return Ok(());
    }
    publish(ctx, cell, &record.event).await?;
    say_at(ctx, cell, room, &record.summary, record.event.created_at).await?;
    record.published = true;
    planes::save(ctx, key, &record).await
}

/// Why the room's binding refuses this verb, or `None` when it does not.
///
/// Publishing an archived document is only half of archival; without this the
/// room's own notice ("no further process commands will be honoured") would be
/// a promise nothing kept. The split is deliberate:
///
/// * mutating verbs are blocked — that IS the archive;
/// * `refresh` is allowed, because reading is what an archived room is still
///   for;
/// * `binding_create` is allowed and REACTIVATES, because somebody has to be
///   able to undo an archive, and that verb already requires a moderator;
/// * `binding_archive` is allowed and idempotent — archiving twice is not an
///   error worth reporting.
///
/// A room whose binding the cell published but the relay no longer shows
/// ([`RoomBinding::Missing`]) blocks the same verbs: the cell cannot show the
/// room what it is acting under, and a room that has never had a binding is
/// the only one in which an absent binding means nothing is in force.
#[must_use]
fn lifecycle_refusal(verb: Verb, binding: &RoomBinding) -> Option<&'static str> {
    let mutating = match verb {
        Verb::Branch | Verb::Commit | Verb::Merge | Verb::Land => true,
        Verb::Refresh | Verb::BindingCreate | Verb::BindingArchive => false,
    };
    if !mutating {
        return None;
    }
    match binding {
        RoomBinding::Never => None,
        RoomBinding::Published(binding) if binding.lifecycle == Lifecycle::Active => None,
        RoomBinding::Published(_) => {
            Some("this binding is archived; publish a new binding to reactivate it")
        }
        RoomBinding::Missing => Some(
            "this room's binding is not on the relay; publish the binding again before changing the branch",
        ),
    }
}

/// A room's binding as the cell decides a command against it.
#[derive(Debug, Clone, PartialEq, Eq)]
enum RoomBinding {
    /// The cell has never published a binding for this room.
    Never,
    /// The binding the cell last published, which the relay holds.
    Published(Box<Binding>),
    /// The cell has published a binding for this room, and the relay does not
    /// hold the one it published last.
    Missing,
}

/// The room's CURRENT binding.
///
/// Decided from the cell's own durable record ([`lifecycle::recorded`]), not
/// from a search of the room: the relay answers a tag query from the room's
/// timeline index and examines only its newest rows (eight rounds of twice
/// the limit), so enough ordinary chat after an archive, about 130 messages,
/// hid the archived binding and the room read as never bound. The relay is
/// then asked for exactly the recorded event by id, a point read that room
/// traffic cannot push out of reach, and a recorded binding the relay does not
/// return is [`RoomBinding::Missing`].
///
/// A room with no record was bound, if at all, before the cell kept one. Its
/// whole timeline is walked by cursor for the newest binding this cell
/// authored; only documents authored by THIS CELL count, because kind 9 is an
/// ordinary room message and any member could post a JSON blob shaped like a
/// binding. Every event weighed has passed [`verified`], so the author
/// compared is one its signature proves.
async fn current_binding(
    ctx: &AppContext,
    cell: &CellContext,
    identity: &Identity,
) -> Result<RoomBinding> {
    let room = identity.room_id()?;
    let binding_id = identity.binding_id()?;
    let author = cell.api.pubkey_hex();
    let Some(recorded) = lifecycle::recorded(ctx, cell, identity).await? else {
        return Ok(match newest_walked_binding(ctx, cell, &room, &binding_id).await? {
            Some(binding) => RoomBinding::Published(Box::new(binding)),
            None => RoomBinding::Never,
        });
    };
    let binding = Binding::parse(recorded.content.as_bytes())
        .ok()
        .filter(|binding| binding.binding_id == binding_id)
        .ok_or_else(|| ProcessError::Storage("the recorded binding is not this room's binding".into()))?;
    let held = filter_query(ctx, cell, &[binding::binding_event_query(&recorded.id, &room, author)])
        .await?
        .iter()
        .any(|event| event.id == recorded.id && event.pubkey == author);
    Ok(if held { RoomBinding::Published(Box::new(binding)) } else { RoomBinding::Missing })
}

/// The newest binding for `binding_id` this cell authored in `room`, found by
/// walking the room's whole timeline.
async fn newest_walked_binding(
    ctx: &AppContext,
    cell: &CellContext,
    room: &str,
    binding_id: &str,
) -> Result<Option<Binding>> {
    Ok(newest_binding(walked_bindings(ctx, cell, room, binding_id).await?))
}

/// Every binding for `binding_id` this cell authored in `room`, with its
/// `created_at` and event id, found by walking the room's whole timeline.
///
/// A binding is an ordinary room message (kind 9), not a process command or
/// result, so the walk reads [`timeline_rows`] rather than [`timeline_page`],
/// which keeps only the latter. Only events naming this cell's key are kept,
/// and each passes [`verified`] before it is parsed: any member can post a
/// JSON blob shaped like a binding, or one claiming the cell's key.
async fn walked_bindings(
    ctx: &AppContext,
    cell: &CellContext,
    room: &str,
    binding_id: &str,
) -> Result<Vec<(u64, String, Binding)>> {
    let author = cell.api.pubkey_hex();
    let mut candidates: Vec<(u64, String, Binding)> = Vec::new();
    let mut cursor: Option<String> = None;
    loop {
        let (rows, next) = timeline_rows(ctx, cell, room, cursor.as_deref()).await?;
        let mine = rows
            .into_iter()
            .filter(|event| event.kind == github::KIND_STREAM_MESSAGE && event.pubkey == author)
            .collect();
        for event in verified(mine) {
            let Ok(binding) = Binding::parse(event.content.as_bytes()) else {
                continue;
            };
            if binding.binding_id == binding_id {
                candidates.push((event.created_at, event.id.clone(), binding));
            }
        }
        match next {
            Some(next) => cursor = Some(next),
            None => return Ok(candidates),
        }
    }
}

fn newest_binding(mut candidates: Vec<(u64, String, Binding)>) -> Option<Binding> {
    candidates
        .sort_by(|(a_at, a_id, _), (b_at, b_id, _)| a_at.cmp(b_at).then_with(|| a_id.cmp(b_id)));
    candidates.pop().map(|(_, _, binding)| binding)
}

/// Authorize and run one command.
async fn execute(
    ctx: &AppContext,
    cell: &CellContext,
    control: &rcs::ControlPlane,
    identity: &Identity,
    state: &BranchState,
    binding_id: &str,
    request: &Event,
) -> Result<CommandResult> {
    let command = Command::parse(&request.content)?;
    if command.binding_id != binding_id {
        return Err(ProcessError::Refused(
            "this command names another binding".into(),
        ));
    }
    if mutation::owns(command.verb) {
        if let Some(result) = mutation::resume(ctx, cell, control, &command, request).await? {
            return Ok(result);
        }
    }
    let branch = identity
        .git_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(&identity.git_ref)
        .to_string();
    let role = role_of(ctx, cell, &request.pubkey).await?;
    let required = command.required_role_in(&branch);
    if !command::meets(role, required) {
        let onto_trunk = if required == command.verb.required_role() {
            ""
        } else {
            " to write the trunk directly; land it from a branch instead"
        };
        return Ok(CommandResult::new(
            &request.id,
            command.verb,
            binding_id,
            Outcome::Refused,
            &format!(
                "{} needs at least {required:?}{onto_trunk}; this requester is {role:?}",
                command.verb.as_str(),
            ),
        ));
    }

    // An ARCHIVED binding stops mutating work, which is what the archive
    // notice in the room promised. Read fresh, for the same reason authority is
    // read fresh: a binding archived since the command was written must still
    // block it.
    let binding = current_binding(ctx, cell, identity).await?;
    if let Some(reason) = lifecycle_refusal(command.verb, &binding) {
        return Ok(CommandResult::new(
            &request.id,
            command.verb,
            binding_id,
            Outcome::Refused,
            reason,
        ));
    }

    // Why: a merge or a land WRITES its `into` branch, which may have a room of
    // its own. That room's archive must stop the write too, wherever the
    // command was posted.
    if matches!(command.verb, Verb::Merge | Verb::Land) {
        let into = command.args.into.as_deref().unwrap_or(command::TRUNK);
        if into != branch {
            let mut target = identity.clone();
            target.git_ref = format!("refs/heads/{into}");
            target.generation = state.generation(into);
            let binding = current_binding(ctx, cell, &target).await?;
            if lifecycle_refusal(command.verb, &binding).is_some() {
                return Ok(CommandResult::new(
                    &request.id,
                    command.verb,
                    binding_id,
                    Outcome::Refused,
                    &format!(
                        "the binding of `{into}`, which this {} writes, is archived or missing; \
                         a moderator must publish a new binding for it first",
                        command.verb.as_str()
                    ),
                ));
            }
        }
    }

    let call = match command.verb {
        Verb::Refresh => {
            return Ok(CommandResult::new(
                &request.id,
                command.verb,
                binding_id,
                Outcome::Ok,
                "re-observed on the next cycle",
            ))
        }
        Verb::BindingCreate => {
            // A REAL effect, not a note: publish the binding document (and the
            // room, if this is the first time anybody has named it).
            let created_at = ctx.now_ms() / 1000;
            let room = materialize_room(
                ctx,
                cell,
                identity,
                &format!("{} · {branch}", control.app()),
                None,
                &format!("Binding published for {branch} by request."),
                created_at,
                &request.id,
            )
            .await?;
            return Ok(CommandResult::new(
                &request.id,
                command.verb,
                binding_id,
                Outcome::Ok,
                &format!("binding published for room {room}"),
            ));
        }
        Verb::BindingArchive => {
            let created_at = ctx.now_ms() / 1000;
            let (room, created_at) = publish_binding(
                ctx,
                cell,
                identity,
                &format!("{} · {branch}", control.app()),
                None,
                Lifecycle::Archived,
                created_at,
                &request.id,
            )
            .await?;
            say_at(
                ctx,
                cell,
                &room,
                "This binding is archived: the room stays readable, and branch, \
                 commit, merge and land requests are refused until a moderator \
                 publishes a new binding for it.",
                created_at,
            )
            .await?;
            return Ok(CommandResult::new(
                &request.id,
                command.verb,
                binding_id,
                Outcome::Ok,
                "binding archived",
            ));
        }
        Verb::Branch => control.branch(
            command.args.branch.as_deref().unwrap_or(&branch),
            command.args.from.as_deref(),
        ),
        Verb::Commit | Verb::Merge | Verb::Land => {
            return mutation::send(ctx, cell, control, identity, &command, request).await;
        }
    };

    let (status, body) = planes::deliver(ctx, planes::CONTROL_EGRESS, &call).await?;
    let reply = rcs::parse_reply(status, &body)?;
    Ok(result_for(&request.id, command.verb, binding_id, &reply))
}

/// Turn a control-plane reply into the room's record of it.
fn result_for(
    command_id: &str,
    verb: Verb,
    binding_id: &str,
    reply: &rcs::RcsReply,
) -> CommandResult {
    match reply {
        rcs::RcsReply::Committed { commit, .. } => {
            CommandResult::new(command_id, verb, binding_id, Outcome::Ok, "committed")
                .with_commit(commit)
        }
        rcs::RcsReply::Landed { commit, .. } => {
            CommandResult::new(command_id, verb, binding_id, Outcome::Ok, "landed")
                .with_commit(commit)
        }
        rcs::RcsReply::Merged {
            commit,
            status,
            conflicts,
            ..
        } => match status {
            rcs::MergeStatus::Merged => {
                let result =
                    CommandResult::new(command_id, verb, binding_id, Outcome::Ok, "merged");
                match commit {
                    Some(c) => result.with_commit(c),
                    None => result,
                }
            }
            rcs::MergeStatus::Stale => CommandResult::new(
                command_id,
                verb,
                binding_id,
                Outcome::Stale,
                "the source branch is behind its target",
            ),
            rcs::MergeStatus::Conflict => CommandResult::new(
                command_id,
                verb,
                binding_id,
                Outcome::Conflict,
                "paths diverged; nothing was committed",
            )
            .with_conflicts(conflicts.clone()),
            rcs::MergeStatus::Unknown(other) => CommandResult::new(
                command_id,
                verb,
                binding_id,
                Outcome::Failed,
                &format!("the platform reported an unrecognized merge status: {other}"),
            ),
        },
        rcs::RcsReply::Rejected { reason } => {
            CommandResult::new(command_id, verb, binding_id, Outcome::Rejected, reason)
        }
        rcs::RcsReply::BuildStarted { operation } => CommandResult::new(
            command_id,
            verb,
            binding_id,
            Outcome::Ok,
            &format!("started ({operation})"),
        ),
        rcs::RcsReply::Branches(_) | rcs::RcsReply::Log(_) => CommandResult::new(
            command_id,
            verb,
            binding_id,
            Outcome::Failed,
            "the platform answered a read to a write request",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_room_order_rotates_so_a_busy_room_cannot_starve_the_rest() {
        // The cap on commands per cycle is what stops one room monopolising a
        // cycle; walking a SORTED map under that cap would starve every room
        // after the first busy one instead. Rotating turns the cap into a latency
        // bound.
        let rooms = vec!["a".to_string(), "b".to_string(), "c".to_string()];
        assert_eq!(rotated(&rooms, 0), vec!["a", "b", "c"]);
        assert_eq!(rotated(&rooms, 1), vec!["b", "c", "a"]);
        assert_eq!(rotated(&rooms, 2), vec!["c", "a", "b"]);
        assert_eq!(rotated(&rooms, 3), rotated(&rooms, 0));
        assert_eq!(rotated(&rooms, 3_000_002), rotated(&rooms, 2));

        for cycle in 0..7 {
            let order = rotated(&rooms, cycle);
            assert_eq!(order.len(), rooms.len(), "cycle {cycle}");
            let mut sorted = order.clone();
            sorted.sort();
            assert_eq!(sorted, rooms, "cycle {cycle}");
        }

        let leaders: std::collections::BTreeSet<String> = (0..rooms.len())
            .filter_map(|cycle| rotated(&rooms, cycle).first().cloned())
            .collect();
        assert_eq!(leaders.len(), rooms.len());

        assert!(rotated::<String>(&[], 5).is_empty());
    }

    #[test]
    fn a_state_key_is_scoped_to_its_community_and_app() {
        assert_eq!(
            state_key("main", "acct/app"),
            b"branchstate/main/acct/app".to_vec()
        );
        assert_ne!(state_key("main", "a"), state_key("other", "a"));
    }

    #[test]
    fn one_branch_name_in_two_repositories_is_two_observations() {
        // The defect this closes: the GitHub cell holds ONE state document per
        // provider instance, so keying by branch name alone made `main` in every
        // repository the same entry — and deleting one repository's `main` bumped
        // the incarnation of every other's, re-rooming branches nobody touched.
        let widgets = observation_key("12345", None, "refs/heads/main");
        let other = observation_key("999", None, "refs/heads/main");
        assert_ne!(widgets, other);

        let mut state = BranchState::default();
        assert_eq!(state.note_present(&widgets, "c1"), 1);
        assert_eq!(state.note_present(&other, "c2"), 1);
        // Retire and recreate ONE of them...
        state.note_absent(&widgets);
        assert_eq!(state.note_present(&widgets, "c9"), 2);
        // ...and the other repository's `main` is untouched.
        assert_eq!(state.generation(&other), "g1");
        assert!(state.is_live(&other));

        // A fork's branch is its own observation too, for the same reason its
        // binding is its own binding.
        let base = observation_key("12345", None, "refs/heads/feature/x");
        let fork = observation_key("12345", Some("999"), "refs/heads/feature/x");
        assert_ne!(base, fork);
    }

    #[test]
    fn an_observation_key_round_trips() {
        for (stable, head, git_ref) in [
            ("12345", None, "refs/heads/main"),
            ("12345", Some("999"), "refs/heads/feature/x"),
            ("my-app", None, "refs/tags/v1"),
        ] {
            let key = observation_key(stable, head, git_ref);
            let (parsed_stable, parsed_head, parsed_ref) = parse_observation_key(&key).expect(&key);
            assert_eq!(parsed_stable, stable);
            assert_eq!(parsed_head.as_deref(), head);
            assert_eq!(parsed_ref, git_ref);
        }
        // Anything that is not one of these keys is SKIPPED rather than misread —
        // which is how a document from an older build is tolerated.
        for junk in [
            "main",
            "",
            ":refs/heads/main",
            "12345:main",
            "12345:",
            "/999:refs/heads/x",
        ] {
            assert!(parse_observation_key(junk).is_none(), "{junk:?}");
        }
    }

    #[test]
    fn the_github_cell_answers_every_verb_rather_than_ignoring_it() {
        // A command nobody answers is a member watching a silent room, so the
        // table is exhaustive — and each answer is truthful about this cell.
        let bid = format!("sha256:{}", "ab".repeat(32));
        let cid = "cd".repeat(32);
        for verb in Verb::ALL {
            let result = github_answer(&cid, *verb, &bid);
            assert_eq!(result.command, cid);
            assert!(!result.detail.is_empty(), "{verb:?} answers with no reason");
            assert!(result.summary().contains(verb.as_str()));
        }
        // Refresh is genuinely answerable: this cell's state comes from
        // deliveries.
        assert_eq!(
            github_answer(&cid, Verb::Refresh, &bid).outcome,
            Outcome::Ok
        );
        // Every WRITE is refused, naming what to do instead — not attempted, and
        // not silently skipped.
        for verb in [Verb::Branch, Verb::Commit, Verb::Merge, Verb::Land] {
            let result = github_answer(&cid, verb, &bid);
            assert_eq!(result.outcome, Outcome::Refused, "{verb:?}");
            assert!(
                result.detail.contains("GitHub App"),
                "{verb:?} should name the blocker: {}",
                result.detail
            );
            assert!(result.detail.contains("desktop app"), "{verb:?}");
        }
        // Binding lifecycle belongs to the native cell, and says so.
        for verb in [Verb::BindingCreate, Verb::BindingArchive] {
            let result = github_answer(&cid, verb, &bid);
            assert_eq!(result.outcome, Outcome::Refused, "{verb:?}");
            assert!(result.detail.contains("native cell"), "{verb:?}");
        }
    }

    #[test]
    fn every_control_plane_reply_maps_to_a_room_record() {
        let bid = format!("sha256:{}", "ab".repeat(32));
        let cid = "cd".repeat(32);
        let cases: Vec<(rcs::RcsReply, Outcome)> = vec![
            (
                rcs::RcsReply::Landed {
                    into: "main".into(),
                    commit: "c1".into(),
                },
                Outcome::Ok,
            ),
            (
                rcs::RcsReply::Committed {
                    branch: "f".into(),
                    commit: "c2".into(),
                },
                Outcome::Ok,
            ),
            (
                rcs::RcsReply::Merged {
                    into: "d".into(),
                    commit: Some("c3".into()),
                    status: rcs::MergeStatus::Merged,
                    conflicts: vec![],
                },
                Outcome::Ok,
            ),
            (
                rcs::RcsReply::Merged {
                    into: "d".into(),
                    commit: None,
                    status: rcs::MergeStatus::Stale,
                    conflicts: vec![],
                },
                Outcome::Stale,
            ),
            (
                rcs::RcsReply::Merged {
                    into: "d".into(),
                    commit: None,
                    status: rcs::MergeStatus::Conflict,
                    conflicts: vec!["a.txt".into()],
                },
                Outcome::Conflict,
            ),
            (
                rcs::RcsReply::Merged {
                    into: "d".into(),
                    commit: None,
                    status: rcs::MergeStatus::Unknown("huh".into()),
                    conflicts: vec![],
                },
                Outcome::Failed,
            ),
            (
                rcs::RcsReply::Rejected {
                    reason: "tests failed".into(),
                },
                Outcome::Rejected,
            ),
            (rcs::RcsReply::Branches(vec![]), Outcome::Failed),
        ];
        for (reply, expected) in cases {
            let result = result_for(&cid, Verb::Land, &bid, &reply);
            assert_eq!(result.outcome, expected, "{reply:?}");
            assert_eq!(result.command, cid);
            assert!(!result.summary().is_empty());
        }

        let landed = result_for(
            &cid,
            Verb::Land,
            "sha256:x",
            &rcs::RcsReply::Landed {
                into: "main".into(),
                commit: "c0ffee".into(),
            },
        );
        assert_eq!(landed.commit.as_deref(), Some("c0ffee"));
        assert!(landed.summary().contains("c0ffee"));

        let conflict = result_for(
            &cid,
            Verb::Merge,
            "sha256:x",
            &rcs::RcsReply::Merged {
                into: "d".into(),
                commit: None,
                status: rcs::MergeStatus::Conflict,
                conflicts: vec!["a.txt".into(), "b.rs".into()],
            },
        );
        assert_eq!(conflict.conflicts.len(), 2);
        assert!(conflict.summary().contains("a.txt"));
    }

    #[test]
    fn short_ids_stay_short() {
        assert_eq!(short("abcdef1234567890"), "abcdef12");
        assert_eq!(short("abc"), "abc");
        assert_eq!(short(""), "");
    }

    #[test]
    fn a_mediated_commit_says_who_asked_for_it() {
        // The commit is made by the PLATFORM's token, so without this the git
        // history would attribute a member's work to the cell and the repository
        // itself would hold no record of whose it was.
        let pubkey = "aa".repeat(32);
        let command = "cd".repeat(32);
        let message = commit_message("Fix the parser", &pubkey, &command);
        assert!(message.starts_with("Fix the parser\n\n"), "{message}");
        assert!(message.contains(&format!("Requested-by: {pubkey}")));
        assert!(message.contains(&format!("Buzz-Command: {command}")));
        // A trailer block needs exactly one blank line before it, whatever
        // whitespace the requester's message ended with.
        for written in ["Fix it", "Fix it\n", "Fix it\n\n\n"] {
            let out = commit_message(written, &pubkey, &command);
            assert!(out.starts_with("Fix it\n\nRequested-by:"), "{out:?}");
        }
        // A multi-paragraph message keeps its shape.
        let long = commit_message("Subject\n\nBody here.", &pubkey, &command);
        assert!(
            long.starts_with("Subject\n\nBody here.\n\nRequested-by:"),
            "{long:?}"
        );
        // An empty message still yields a subject rather than a commit whose
        // first line is a trailer.
        let empty = commit_message("   ", &pubkey, &command);
        assert!(empty.starts_with("rcs_commit (mediated)\n\n"), "{empty:?}");
        // Deterministic, because a re-driven effect must rebuild identical bytes.
        assert_eq!(commit_message("Fix the parser", &pubkey, &command), message);
    }

    #[test]
    fn an_unparseable_command_still_records_what_was_attempted() {
        // Best effort: the verb is read even when the rest of the envelope is
        // unusable, so the refusal in the room says what was tried.
        assert_eq!(
            attempted_verb(r#"{"verb":"rcs_land","junk":true}"#),
            Verb::Land
        );
        assert_eq!(attempted_verb(r#"{"verb":"rcs_merge"}"#), Verb::Merge);
        // ...and falls back to the harmless read-only verb when it cannot.
        assert_eq!(attempted_verb("not json"), Verb::Refresh);
        assert_eq!(attempted_verb(r#"{"verb":"nonsense"}"#), Verb::Refresh);
        assert_eq!(attempted_verb("{}"), Verb::Refresh);
    }

    #[test]
    fn the_current_binding_is_chosen_by_a_total_order() {
        // Two lifecycle publications inside ONE second: the answer must not
        // depend on the order the relay returned them.
        let identity = Identity::rcs("acct", "acct/app", "refs/heads/x", "g1");
        let scope = Scope {
            buzz_origin: "https://buzz.example".into(),
            community_owner_pubkey: "aa".repeat(32),
            visibility: "private".into(),
        };
        let active = Binding::new(&identity, "d", None, None, None, scope.clone()).expect("active");
        let mut archived = active.clone();
        archived.lifecycle = Lifecycle::Archived;

        let at = 1_700_000_000u64;
        let low = "11".repeat(32);
        let high = "ff".repeat(32);
        // Same second, archived carrying the higher id — archived wins.
        let forward = vec![
            (at, low.clone(), active.clone()),
            (at, high.clone(), archived.clone()),
        ];
        let reversed = vec![
            (at, high.clone(), archived.clone()),
            (at, low.clone(), active.clone()),
        ];
        assert_eq!(
            newest_binding(forward).map(|b| b.lifecycle),
            Some(Lifecycle::Archived)
        );
        assert_eq!(
            newest_binding(reversed).map(|b| b.lifecycle),
            Some(Lifecycle::Archived),
            "the answer must not depend on query order"
        );

        // A later second always wins, whatever the ids say.
        let later = vec![
            (at + 1, low.clone(), active.clone()),
            (at, high.clone(), archived.clone()),
        ];
        assert_eq!(
            newest_binding(later).map(|b| b.lifecycle),
            Some(Lifecycle::Active)
        );

        assert!(newest_binding(vec![]).is_none());
    }

    #[test]
    fn an_archived_binding_blocks_mutating_work_and_nothing_else() {
        // The promise the archive notice makes, as a decision a test can call.
        let identity = Identity::rcs("acct", "acct/app", "refs/heads/x", "g1");
        let scope = Scope {
            buzz_origin: "https://buzz.example".into(),
            community_owner_pubkey: "aa".repeat(32),
            visibility: "private".into(),
        };
        let active = Binding::new(&identity, "d", None, None, None, scope).expect("active");
        let mut archived = active.clone();
        archived.lifecycle = Lifecycle::Archived;
        let (active, archived) = (RoomBinding::Published(Box::new(active)), RoomBinding::Published(Box::new(archived)));
        for verb in [Verb::Branch, Verb::Commit, Verb::Merge, Verb::Land] {
            assert!(
                lifecycle_refusal(verb, &archived).is_some(),
                "{verb:?} must be refused in an archived room"
            );
            // A binding the cell published and the relay no longer shows
            // refuses the same verbs.
            assert!(
                lifecycle_refusal(verb, &RoomBinding::Missing).is_some(),
                "{verb:?} must be refused when the room's binding is missing"
            );
        }
        // Reading still works; so does reactivating; archiving twice is fine.
        for verb in [Verb::Refresh, Verb::BindingCreate, Verb::BindingArchive] {
            assert!(lifecycle_refusal(verb, &archived).is_none(), "{verb:?} must still be allowed");
            assert!(lifecycle_refusal(verb, &RoomBinding::Missing).is_none(), "{verb:?} with a missing binding");
        }
        // A live binding blocks nothing...
        for verb in Verb::ALL {
            assert!(lifecycle_refusal(*verb, &active).is_none(), "{verb:?}");
            // ...and neither does a room that was NEVER bound: its binding has
            // not been published yet, and refusing there would make the first
            // `binding_create` impossible.
            assert!(lifecycle_refusal(*verb, &RoomBinding::Never).is_none(), "{verb:?} with no binding");
        }
    }

    #[test]
    fn a_retired_room_closes_only_after_a_walk_begun_past_its_window() {
        let mut state = BranchState::default();
        state.note_retired("feature/x", "g1", 1_000);
        state.note_retired("feature/x", "g1", 2_000);
        assert_eq!(state.retired.len(), 1, "one entry per lifetime");
        let window_end = 2_000 + cells::RETIRED_ROOM_ANSWER_MS;

        assert_eq!(state.advance_walk("r", Some("p2".into()), true, window_end - 1), None);
        assert_eq!(state.advance_walk("r", None, true, window_end + 5), Some(window_end - 1));
        state.close_retired("feature/x", "g1", window_end - 1);
        assert_eq!(state.retired.len(), 1, "that walk began inside the window");

        assert_eq!(state.advance_walk("r", Some("p2".into()), false, window_end), None);
        assert!(state.command_walk.is_empty(), "a page left owing holds the walk");
        assert_eq!(state.advance_walk("r", None, true, window_end), Some(window_end));
        state.close_retired("feature/x", "g1", window_end);
        assert!(state.retired.is_empty());
    }

    // ── Flows against a scripted platform ────────────────────────────────────
    //
    // The cell's real code runs against `planes::fake`: an in-memory store and
    // a scripted workspace relay and control plane. The fakes answer at once,
    // so `block_on` drives each future to completion on the test thread.

    use std::sync::{Arc, Mutex};

    use ferrite_app::serde_json::{json, Value};

    const T0: u64 = 1_700_000_000_000;
    const CELL_KEY: [u8; 32] = [1u8; 32];
    const MEMBER_KEY: [u8; 32] = [2u8; 32];

    fn block_on<F: std::future::Future>(future: F) -> F::Output {
        struct Noop;
        impl std::task::Wake for Noop {
            fn wake(self: std::sync::Arc<Self>) {}
        }
        let waker = std::task::Waker::from(std::sync::Arc::new(Noop));
        let mut cx = std::task::Context::from_waker(&waker);
        let mut future = Box::pin(future);
        loop {
            if let std::task::Poll::Ready(out) = future.as_mut().poll(&mut cx) {
                return out;
            }
        }
    }

    fn pubkey(key: &[u8; 32]) -> String {
        crypto::to_hex(&crypto::schnorr::xonly_pubkey(key).expect("a valid key"))
    }

    /// The workspace relay: stores events once each and answers the two read
    /// routes the way `apps/buzz` does: `/buzz/api/query` pages one room's
    /// timeline by index position (newest second first, then id), and
    /// `/buzz/api/filter-query` answers NIP-01 filters.
    fn commit_peer_state(ctx: &AppContext, peer: PeerStateWrite) {
        let mut state: BranchState = block_on(planes::load(ctx, &peer.key)).expect("read peer state").unwrap_or_default();
        if peer.recreate {
            let generation = state.generation(&peer.branch);
            state.note_absent(&peer.branch);
            state.note_retired(&peer.branch, &generation, ctx.now_ms());
        }
        state.note_present(&peer.branch, "peer-tip");
        state.mark_answered("peer-command");
        if let Some(plan) = peer.plan {
            state.pending_delivery_plans.insert("peer-delivery".into(), plan);
        }
        block_on(planes::save(ctx, &peer.key, &state)).expect("committed peer write");
    }

    struct PeerStateWrite {
        trigger: String,
        key: Vec<u8>,
        branch: String,
        recreate: bool,
        plan: Option<github::Plan>,
    }

    #[derive(Default)]
    struct Relay {
        peer_write: Option<PeerStateWrite>,
        events: Vec<Event>,
        members: Vec<(String, &'static str)>,
        fail_next_summary: bool,
        fail_next_channel: bool,
        channel_creates: usize,
        expire_during_channel: Option<ferrite_app::Clock>,
        fail_after_observation: bool,
        fail_next_timeline: bool,
        lose_next_note: Option<String>,
        fail_next_archive_note: bool,
        fail_next_result: bool,
        transient_binding_reads: usize,
    }

    /// An event's place in the relay's timeline index (`keys::ts_index`).
    fn position(event: &Event) -> String {
        format!("{:020}/{}", u64::MAX - event.created_at, event.id)
    }

    impl Relay {
        fn handle(&mut self, call: &rcs::Call) -> Result<(u16, Vec<u8>)> {
            let path = call.url.trim_start_matches("https://buzz.example");
            let reply = match path {
                "/buzz/api/events" => {
                    let event: Event = serde_json::from_slice(&call.body).expect("an event");
                    if self.fail_next_result && event.kind == command::KIND_PROCESS_RESULT {
                        self.fail_next_result = false;
                        return Ok((503, b"result unavailable".to_vec()));
                    }
                    if self.fail_next_archive_note && event.kind == 9 && event.content.starts_with("This binding is archived:") {
                        self.fail_next_archive_note = false;
                        return Ok((503, b"archive note unavailable".to_vec()));
                    }
                    if self.fail_next_summary && event.kind == 9 && event.content.starts_with("refresh:") {
                        self.fail_next_summary = false;
                        return Ok((503, b"summary unavailable".to_vec()));
                    }
                    let observation_note = event.content.starts_with("Branch ") && event.kind == 9;
                    let lose_reply = self.lose_next_note.as_ref().is_some_and(|text| event.content.contains(text));
                    if !self.events.iter().any(|e| e.id == event.id) {
                        self.events.push(event);
                    }
                    if lose_reply {
                        self.lose_next_note = None;
                        return Ok((503, b"reply lost after storing the note".to_vec()));
                    }
                    if self.fail_after_observation && observation_note {
                        self.fail_after_observation = false;
                        self.fail_next_timeline = true;
                    }
                    json!({})
                }
                "/buzz/api/channels" => {
                    self.channel_creates += 1;
                    if self.fail_next_channel {
                        self.fail_next_channel = false;
                        return Ok((503, b"room unavailable".to_vec()));
                    }
                    if let Some(clock) = self.expire_during_channel.take() {
                        clock.advance_ms(100_000);
                    }
                    json!({})
                },
                "/buzz/api/members" => {
                    // The read is signed for GET; the egress must send a GET.
                    assert_eq!(planes::fake::methods().last(), Some(&"GET"), "the roster read is a GET");
                    json!({
                    "members": self.members.iter().map(|(pk, role)| json!({"pubkey": pk, "role": role})).collect::<Vec<_>>(),
                })
                }
                "/buzz/api/query" => {
                    if self.fail_next_timeline {
                        self.fail_next_timeline = false;
                        return Ok((503, b"timeline unavailable".to_vec()));
                    }
                    let body: Value = serde_json::from_slice(&call.body).expect("a timeline read");
                    let room = body["channel"].as_str().expect("a room");
                    let limit = body["limit"].as_u64().unwrap_or(100).min(500) as usize;
                    let after = body["cursor"].as_str();
                    let mut rows: Vec<&Event> = self
                        .events
                        .iter()
                        .filter(|e| e.channel() == Some(room))
                        .filter(|e| after.is_none_or(|cursor| position(e).as_str() > cursor))
                        .collect();
                    rows.sort_by_key(|e| position(e));
                    let more = rows.len() > limit;
                    rows.truncate(limit);
                    let next = if more { rows.last().map(|e| position(e)) } else { None };
                    let complete = next.is_none();
                    json!({ "events": rows, "next_cursor": next, "complete": complete })
                }
                "/buzz/api/filter-query" => {
                    if self.transient_binding_reads > 0 {
                        self.transient_binding_reads -= 1;
                        return Ok((503, b"binding unavailable".to_vec()));
                    }
                    let filters: Vec<Value> = serde_json::from_slice(&call.body).expect("filters");
                    assert!(filters.len() <= 8, "the relay refuses more than eight filters");
                    let mut out: Vec<&Event> = Vec::new();
                    for filter in &filters {
                        let limit = filter["limit"].as_u64().unwrap_or(100) as usize;
                        // The relay picks candidates from one index (an `#e`
                        // target's, else one room's) and examines at most
                        // eight rounds of twice `limit` of them.
                        // An `ids` filter is a point read and comes first,
                        // as in `ws::plan_read`.
                        let indexed = |e: &&Event| match (filter["ids"].as_array(), filter["#e"].as_array(), filter["#h"].as_array()) {
                            (Some(ids), _, _) => ids.iter().any(|id| id.as_str() == Some(e.id.as_str())),
                            (None, Some(targets), _) => e.tags.iter().any(|t| {
                                t.first().map(String::as_str) == Some("e")
                                    && t.get(1).is_some_and(|v| targets.iter().any(|w| w.as_str() == Some(v)))
                            }),
                            (None, None, Some(rooms)) if rooms.len() == 1 => e.channel() == rooms[0].as_str(),
                            _ => true,
                        };
                        let mut candidates: Vec<&Event> = self.events.iter().filter(indexed).collect();
                        candidates.sort_by_key(|e| position(e));
                        candidates.truncate(16 * limit);
                        let hits = candidates.into_iter().filter(|e| matches(filter, e)).take(limit);
                        for hit in hits {
                            if !out.iter().any(|e| e.id == hit.id) {
                                out.push(hit);
                            }
                        }
                    }
                    json!(out)
                }
                other => panic!("unexpected workspace call {other}"),
            };
            Ok((200, serde_json::to_vec(&reply).expect("encode")))
        }
    }

    fn matches(filter: &Value, event: &Event) -> bool {
        let Some(fields) = filter.as_object() else {
            return false;
        };
        let any = |want: &Value, have: &str| {
            want.as_array()
                .is_some_and(|values| values.iter().any(|v| v.as_str() == Some(have)))
        };
        fields.iter().all(|(field, want)| match field.as_str() {
            "kinds" => want
                .as_array()
                .is_some_and(|k| k.iter().any(|k| k.as_u64() == Some(u64::from(event.kind)))),
            "authors" => any(want, &event.pubkey),
            "ids" => any(want, &event.id),
            "since" => want.as_u64().is_some_and(|s| event.created_at >= s),
            "until" => want.as_u64().is_some_and(|u| event.created_at <= u),
            "limit" => true,
            tag => tag.strip_prefix('#').is_some_and(|name| {
                event.tags.iter().any(|t| {
                    t.first().map(String::as_str) == Some(name) && t.get(1).is_some_and(|v| any(want, v))
                })
            }),
        })
    }

    fn results_for(relay: &Relay, command: &Event) -> Vec<CommandResult> {
        relay
            .events
            .iter()
            .filter(|e| e.kind == command::KIND_PROCESS_RESULT && e.first_tag("e") == Some(command.id.as_str()))
            .map(|e| serde_json::from_str(&e.content).expect("a result"))
            .collect()
    }

    /// A native cell with a scripted control plane whose branch list a test
    /// sets, and a relay whose roster is the given members.
    struct NativeRig {
        ctx: AppContext,
        clock: ferrite_app::Clock,
        relay: Arc<Mutex<Relay>>,
        branches: Arc<Mutex<Value>>,
        cell: CellContext,
        control: rcs::ControlPlane,
        identity: Identity,
        _platform: planes::fake::Installed,
    }

    impl NativeRig {
        fn new(members: &[[u8; 32]]) -> Self {
            let secrets = [
                (SECRET_CELL_KEY, crypto::to_hex(&CELL_KEY)),
                (SECRET_ORIGIN, "https://buzz.example".to_string()),
                (SECRET_COMMUNITY_OWNER, "aa".repeat(32)),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v))
            .collect();
            let payload = ferrite_app::InjectionPayload {
                zone: None,
                instance: None,
                conductor_addrs: Vec::new(),
                services: BTreeMap::new(),
                edge_roots: Vec::new(),
                depends: BTreeMap::new(),
                native_replies: BTreeMap::new(),
                secrets,
            };
            let clock = ferrite_app::Clock::manual(T0);
            let ctx = AppContext::with_clock(payload, clock.clone());
            let relay = Arc::new(Mutex::new(Relay {
                members: members.iter().map(|k| (pubkey(k), "member")).collect(),
                ..Relay::default()
            }));
            let branches = Arc::new(Mutex::new(json!([["feature/x", "c1"]])));
            let platform = {
                let (relay, branches) = (relay.clone(), branches.clone());
                let peer_ctx = ctx.clone();
                planes::fake::install(move |handle, call| {
                    if handle == planes::BUZZ_EGRESS {
                        let (reply, peer) = {
                            let mut relay = relay.lock().expect("lock");
                            let event = if call.url.ends_with("/buzz/api/events") {
                                serde_json::from_slice::<Event>(&call.body).ok()
                            } else { None };
                            let inject = relay.peer_write.as_ref().is_some_and(|peer| {
                                event.as_ref().is_some_and(|e| e.content.contains(&peer.trigger))
                            });
                            let peer = if inject { relay.peer_write.take() } else { None };
                            (relay.handle(call), peer)
                        };
                        if let Some(peer) = peer {
                            commit_peer_state(&peer_ctx, peer);
                        }
                        return reply;
                    }
                    let request: Value = serde_json::from_slice(&call.body).expect("a tools/call");
                    assert_eq!(request["params"]["name"], "rcs_branches", "no mutation is sent");
                    let reply = json!({"RcsBranches": {"branches": branches.lock().expect("lock").clone()}});
                    let peer = {
                        let mut relay = relay.lock().expect("lock");
                        if relay.peer_write.as_ref().is_some_and(|peer| peer.trigger == "control-snapshot") {
                            relay.peer_write.take()
                        } else { None }
                    };
                    if let Some(peer) = peer {
                        commit_peer_state(&peer_ctx, peer);
                    }
                    Ok((200, serde_json::to_vec(&reply).expect("encode")))
                })
            };
            Self {
                cell: CellContext::build(&ctx, "test").expect("cell"),
                control: rcs::ControlPlane::new("https://control.example/mcp", "token", "acct/app").expect("control"),
                identity: Identity::rcs("acct", "acct/app", "refs/heads/feature/x", "g1"),
                ctx,
                clock,
                relay,
                branches,
                _platform: platform,
            }
        }

        /// A refresh command from `author`, at unix second `at`.
        fn refresh(&self, author: &[u8; 32], nonce: usize, at: u64) -> Event {
            let command = Command::new(Verb::Refresh, &self.identity.binding_id().expect("binding"));
            let tags = vec![
                vec!["h".into(), self.identity.room_id().expect("room")],
                vec!["n".into(), nonce.to_string()],
            ];
            let event = Event::sign(author, command::KIND_PROCESS_COMMAND, tags, &command.to_content().expect("encode"), at)
                .expect("sign");
            self.relay.lock().expect("lock").events.push(event.clone());
            event
        }

        /// A result this cell published earlier for `command`.
        fn answered_earlier(&self, command: &Event) {
            self.answer(command, command.created_at + 1, 0);
        }

        /// A result this cell published for `command` at unix second `at`;
        /// `nonce` tells several answers to one command apart.
        fn answer(&self, command: &Event, at: u64, nonce: usize) {
            let tags = vec![
                vec!["h".into(), self.identity.room_id().expect("room")],
                vec!["e".into(), command.id.clone()],
                vec!["n".into(), nonce.to_string()],
            ];
            let content = CommandResult::new(&command.id, Verb::Refresh, "b", Outcome::Ok, "answered earlier")
                .to_content()
                .expect("encode");
            let result = Event::sign(&CELL_KEY, command::KIND_PROCESS_RESULT, tags, &content, at).expect("sign");
            self.relay.lock().expect("lock").events.push(result);
        }

        /// Run one native cycle and return how many results it published.
        fn cycle(&self, cycle: usize) -> usize {
            let before = self.relay.lock().expect("lock").events.len();
            block_on(native_cycle(&self.ctx, &self.cell, &self.control, &state_key("main", "acct/app"), cycle))
                .expect("a cycle");
            self.relay.lock().expect("lock").events[before..]
                .iter()
                .filter(|e| e.kind == command::KIND_PROCESS_RESULT)
                .count()
        }

        fn state(&self) -> BranchState {
            block_on(planes::load(&self.ctx, &state_key("main", "acct/app"))).expect("read").expect("saved")
        }
    }

    /// 150 commands from one member and 10 from an author no longer on the
    /// roster, all in one second. Every one is answered once, at most
    /// [`MAX_COMMANDS_PER_CYCLE`] a cycle.
    #[test]
    fn every_command_in_a_crowded_second_is_answered_once() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        let removed = [9u8; 32];
        let at = T0 / 1000 - 5;
        let mut commands: Vec<Event> = (0..150).map(|n| rig.refresh(&MEMBER_KEY, n, at)).collect();
        commands.extend((150..160).map(|n| rig.refresh(&removed, n, at)));
        for cycle in 0..40 {
            let answered = rig.cycle(cycle);
            assert!(answered <= MAX_COMMANDS_PER_CYCLE, "cycle {cycle} answered {answered}");
        }
        for command in &commands {
            assert_eq!(results_for(&rig.relay.lock().expect("lock"), command).len(), 1, "command {} answered once", command.id);
        }
    }

    /// Commands left in a retired branch's room are refused there, and the
    /// room is kept while its answering window is open.
    #[test]
    fn commands_left_in_a_retired_branchs_room_are_refused() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let commands: Vec<Event> = (0..3).map(|n| rig.refresh(&MEMBER_KEY, n, T0 / 1000 - 10)).collect();
        *rig.branches.lock().expect("lock") = json!([]);
        rig.cycle(1);
        rig.cycle(2);
        for command in &commands {
            let answers = results_for(&rig.relay.lock().expect("lock"), command);
            assert_eq!(answers.len(), 1, "answered once");
            assert_eq!(answers[0].outcome, Outcome::Refused);
            assert!(answers[0].detail.contains("branch is gone"), "{}", answers[0].detail);
        }
        let retired = rig.state().retired;
        assert_eq!(retired.len(), 1);
        assert_eq!((retired[0].name.as_str(), retired[0].generation.as_str()), ("feature/x", "g1"));
    }

    /// A retired room whose walks inside its window found nothing owed later
    /// holds a backlog spread over several pages, behind a page of answered
    /// commands, more than a cycle answers. Past the window it is kept until
    /// a walk has answered every one of them.
    #[test]
    fn a_retired_room_is_kept_until_a_walk_past_its_window_answers_everything() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        *rig.branches.lock().expect("lock") = json!([]);
        for cycle in 1..4 {
            rig.cycle(cycle);
        }
        assert_eq!(rig.state().retired.len(), 1, "walks inside the window keep the room");

        let base = T0 / 1000;
        let owed: Vec<Event> = (0..110).map(|n| rig.refresh(&MEMBER_KEY, n, base - 2000 + n as u64)).collect();
        for n in 0..100 {
            let newer = rig.refresh(&MEMBER_KEY, 1000 + n, base - 200 + n as u64);
            rig.answered_earlier(&newer);
        }
        rig.clock.advance_ms(cells::RETIRED_ROOM_ANSWER_MS + 1);

        for cycle in 4..60 {
            rig.cycle(cycle);
            if rig.state().retired.is_empty() {
                break;
            }
        }
        assert!(rig.state().retired.is_empty(), "the room is forgotten once everything is answered");
        for command in &owed {
            assert_eq!(results_for(&rig.relay.lock().expect("lock"), command).len(), 1, "answered before the room went");
        }
    }

    #[test]
    fn an_answer_buried_far_from_its_command_is_not_given_again() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let base = T0 / 1000;
        let room = rig.identity.room_id().expect("room");
        let command = rig.refresh(&MEMBER_KEY, 0, base - 100);
        assert_eq!(rig.cycle(1), 1, "answered once");

        rig.clock.advance_ms(600_000);
        for n in 0..150u64 {
            let between = Event::sign(&MEMBER_KEY, 9, vec![vec!["h".into(), room.clone()]], &format!("between {n}"), base - 99 + n / 2)
                .expect("sign");
            rig.relay.lock().expect("lock").events.push(between);
        }
        for n in 0..400u64 {
            let tags = vec![vec!["h".into(), room.clone()], vec!["e".into(), command.id.clone()]];
            let reaction = Event::sign(&MEMBER_KEY, 7, tags, &format!("+{n}"), base + 5).expect("sign");
            rig.relay.lock().expect("lock").events.push(reaction);
        }
        for n in 0..200u64 {
            let chatter = Event::sign(&MEMBER_KEY, 9, vec![vec!["h".into(), room.clone()]], &format!("chat {n}"), base + 10 + n)
                .expect("sign");
            rig.relay.lock().expect("lock").events.push(chatter);
        }
        let key = state_key("main", "acct/app");
        let mut state = rig.state();
        state.answered.clear();
        block_on(planes::save(&rig.ctx, &key, &state)).expect("save");

        for cycle in 2..14 {
            rig.cycle(cycle);
        }
        assert_eq!(results_for(&rig.relay.lock().expect("lock"), &command).len(), 1, "answered again");
    }

    #[test]
    fn an_answer_from_before_answer_records_is_found_and_recorded() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        let base = T0 / 1000;
        let room = rig.identity.room_id().expect("room");
        let command = rig.refresh(&MEMBER_KEY, 0, base - 400);
        let chat = |n: u64, at: u64| {
            Event::sign(&MEMBER_KEY, 9, vec![vec!["h".into(), room.clone()]], &format!("chat {n}"), at).expect("sign")
        };
        for n in 0..150u64 {
            rig.relay.lock().expect("lock").events.push(chat(n, base - 399 + n));
        }
        rig.answer(&command, base - 200, 0);
        for n in 150..300u64 {
            rig.relay.lock().expect("lock").events.push(chat(n, base - 340 + n));
        }
        for n in 0..400u64 {
            let tags = vec![vec!["h".into(), room.clone()], vec!["e".into(), command.id.clone()]];
            let reaction = Event::sign(&MEMBER_KEY, 7, tags, &format!("+{n}"), base - 40).expect("sign");
            rig.relay.lock().expect("lock").events.push(reaction);
        }
        let fresh = rig.refresh(&MEMBER_KEY, 1000, base - 410);
        rig.cycle(0);
        assert!(block_on(planes::claimed(&rig.ctx, &answer_key(&room, &fresh.id))).expect("read").is_none(),
            "unrecorded commands must wait for the complete legacy scan");
        for cycle in 1..40 {
            rig.cycle(cycle);
        }
        assert_eq!(results_for(&rig.relay.lock().expect("lock"), &command).len(), 1, "answered again");
        assert_eq!(results_for(&rig.relay.lock().expect("lock"), &fresh).len(), 1, "unanswered command was not reached");
    }

    /// `event` re-attributed to `claimed`: the id is recomputed over the new
    /// fields, so only the signature (made by the real author) is wrong — the
    /// forgery a relay that skipped verification would pass along.
    fn forge(mut event: Event, claimed: &[u8; 32]) -> Event {
        event.pubkey = pubkey(claimed);
        event.id = crypto::to_hex(&event.compute_id().expect("id"));
        event
    }

    /// A command whose signature is not its claimed author's, and a member's
    /// command whose body was changed after signing, are never executed.
    #[test]
    fn forged_and_tampered_commands_are_never_executed() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let at = T0 / 1000 - 5;
        let outsider = [9u8; 32];
        let forged = {
            let signed = rig.refresh(&outsider, 1, at);
            let mut relay = rig.relay.lock().expect("lock");
            relay.events.retain(|e| e.id != signed.id);
            let forged = forge(signed, &MEMBER_KEY);
            relay.events.push(forged.clone());
            forged
        };
        let tampered = {
            let signed = rig.refresh(&MEMBER_KEY, 2, at);
            let mut relay = rig.relay.lock().expect("lock");
            let stored = relay.events.iter_mut().find(|e| e.id == signed.id).expect("stored");
            stored.tags.push(vec!["x".into(), "changed after signing".into()]);
            stored.clone()
        };
        let genuine = rig.refresh(&MEMBER_KEY, 3, at);
        for cycle in 1..4 {
            rig.cycle(cycle);
        }
        let relay = rig.relay.lock().expect("lock");
        assert_eq!(results_for(&relay, &genuine).len(), 1, "the genuine command is answered");
        assert!(results_for(&relay, &forged).is_empty(), "a forged command was executed");
        assert!(results_for(&relay, &tampered).is_empty(), "a tampered command was executed");
    }

    /// A result claiming to be this cell's but not signed by it does not
    /// count as the cell's answer: the command it names is still answered.
    #[test]
    fn a_forged_result_is_not_counted_as_this_cells_answer() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        let command = rig.refresh(&MEMBER_KEY, 0, T0 / 1000 - 5);
        let tags = vec![
            vec!["h".into(), rig.identity.room_id().expect("room")],
            vec!["e".into(), command.id.clone()],
        ];
        let content = CommandResult::new(&command.id, Verb::Refresh, "b", Outcome::Ok, "forged answer")
            .to_content()
            .expect("encode");
        let signed = Event::sign(&MEMBER_KEY, command::KIND_PROCESS_RESULT, tags, &content, T0 / 1000 - 4)
            .expect("sign");
        let forged = forge(signed, &CELL_KEY);
        rig.relay.lock().expect("lock").events.push(forged.clone());
        for cycle in 0..4 {
            rig.cycle(cycle);
        }
        let relay = rig.relay.lock().expect("lock");
        let genuine: Vec<&Event> = relay
            .events
            .iter()
            .filter(|e| e.kind == command::KIND_PROCESS_RESULT && e.first_tag("e") == Some(command.id.as_str()))
            .filter(|e| e.id != forged.id)
            .collect();
        assert_eq!(genuine.len(), 1, "the forged result stood in for the cell's own answer");
        genuine[0].verify().expect("the cell's answer is genuinely signed");
    }

    /// A binding document claiming this cell's key but signed by someone
    /// else does not change the room's lifecycle.
    #[test]
    fn a_forged_binding_does_not_archive_the_room() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let before = published(&rig);
        assert_eq!(before.lifecycle, Lifecycle::Active);
        let mut binding = before.clone();
        binding.lifecycle = Lifecycle::Archived;
        let signed = Event::sign(&MEMBER_KEY, 9, binding.tags(), &binding.to_json().expect("json"), T0 / 1000 + 60)
            .expect("sign");
        rig.relay.lock().expect("lock").events.push(forge(signed, &CELL_KEY));
        let after = published(&rig);
        assert_eq!(after.lifecycle, Lifecycle::Active, "a forged binding archived the room");
    }

    /// A member who posts more messages carrying a binding's tags than the
    /// binding query's limit, each genuinely signed by the member, does not
    /// push the cell's own archived binding out of the query window: the room
    /// stays archived and a mutating command is still refused.
    #[test]
    fn crafted_binding_tags_from_a_member_do_not_unarchive_the_room() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").members[0].1 = "moderator";
        rig.cycle(0);
        let id = rig.identity.binding_id().expect("binding");
        let archive = lifecycle_request(&rig, Verb::BindingArchive, 1);
        let archived = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &archive)).expect("archive");
        assert_eq!(archived.outcome, Outcome::Ok);
        let mut decoy = Binding::new(&rig.identity, "acct/app · feature/x", None, None, None, rig.cell.scope()).expect("binding");
        decoy.lifecycle = Lifecycle::Active;
        for n in 0..9 {
            let crafted = Event::sign(&MEMBER_KEY, 9, decoy.tags(), &decoy.to_json().expect("json"), T0 / 1000 + 10 + n)
                .expect("sign");
            rig.relay.lock().expect("lock").events.push(crafted);
        }
        assert_eq!(published(&rig).lifecycle, Lifecycle::Archived, "member messages hid the cell's binding");
        let mut command = Command::new(Verb::Branch, &id);
        command.args.branch = Some("feature/y".into());
        let request = Event::sign(&MEMBER_KEY, command::KIND_PROCESS_COMMAND,
            vec![vec!["h".into(), rig.identity.room_id().expect("room")], vec!["n".into(), "2".into()]],
            &command.to_content().expect("encode"), T0 / 1000 + 20).expect("sign");
        let result = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &request)).expect("command");
        assert_eq!(result.outcome, Outcome::Refused, "a branch ran in an archived room");
    }

    /// A merge posted in another room cannot write a branch whose own room
    /// a moderator archived: the `into` branch's binding is checked too.
    #[test]
    fn a_merge_from_another_room_into_an_archived_branch_is_refused() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        archive_room(&rig);
        assert_eq!(published(&rig).lifecycle, Lifecycle::Archived);
        let into = rig.identity.git_ref.strip_prefix("refs/heads/").expect("a branch ref").to_string();
        assert_eq!(rig.identity.generation, BranchState::default().generation(&into));
        let mut scratch = rig.identity.clone();
        scratch.git_ref = "refs/heads/scratch".into();
        let id = scratch.binding_id().expect("binding");
        let mut command = Command::new(Verb::Merge, &id);
        command.args.from = Some("scratch".into());
        command.args.into = Some(into);
        let request = Event::sign(&MEMBER_KEY, command::KIND_PROCESS_COMMAND,
            vec![vec!["h".into(), scratch.room_id().expect("room")], vec!["n".into(), "9".into()]],
            &command.to_content().expect("encode"), T0 / 1000 + 30).expect("sign");
        let result = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &scratch, &BranchState::default(), &id, &request))
            .expect("command");
        assert_eq!(result.outcome, Outcome::Refused, "a merge wrote an archived branch from another room: {result:?}");
    }

    /// The rig room's binding, which must be published and held by the relay.
    fn published(rig: &NativeRig) -> Binding {
        match block_on(current_binding(&rig.ctx, &rig.cell, &rig.identity)).expect("read") {
            RoomBinding::Published(binding) => *binding,
            other => panic!("expected a published binding, found {other:?}"),
        }
    }

    /// A member's `rcs_branch` command in the rig's room, at unix second `at`.
    fn branch_request(rig: &NativeRig, nonce: usize, at: u64) -> Event {
        let mut command = Command::new(Verb::Branch, &rig.identity.binding_id().expect("binding"));
        command.args.branch = Some("feature/y".into());
        Event::sign(&MEMBER_KEY, command::KIND_PROCESS_COMMAND,
            vec![vec!["h".into(), rig.identity.room_id().expect("room")], vec!["n".into(), nonce.to_string()]],
            &command.to_content().expect("encode"), at).expect("sign")
    }

    /// Archive the rig's room through the cell, as a moderator would.
    fn archive_room(rig: &NativeRig) {
        rig.relay.lock().expect("lock").members[0].1 = "moderator";
        rig.cycle(0);
        let id = rig.identity.binding_id().expect("binding");
        let archive = lifecycle_request(rig, Verb::BindingArchive, 1);
        let archived = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &archive)).expect("archive");
        assert_eq!(archived.outcome, Outcome::Ok);
    }

    /// The real relay answers the binding's `#h` filter from the room's
    /// timeline index and examines at most eight rounds of twice the filter's
    /// limit of 8, the newest 128 room events (the double models the same
    /// window). 130 ordinary chat messages after an archive put the archived
    /// binding outside it; the room must still read as archived, and a
    /// mutating command must still be refused.
    #[test]
    fn room_volume_after_an_archive_does_not_unarchive_the_room() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        archive_room(&rig);
        let room = rig.identity.room_id().expect("room");
        for n in 0..130u64 {
            let chat = Event::sign(&MEMBER_KEY, 9, vec![vec!["h".into(), room.clone()]], &format!("message {n}"), T0 / 1000 + 10 + n)
                .expect("sign");
            rig.relay.lock().expect("lock").events.push(chat);
        }
        let id = rig.identity.binding_id().expect("binding");
        let request = branch_request(&rig, 2, T0 / 1000 + 200);
        let result = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &request)).expect("command");
        assert_eq!(result.outcome, Outcome::Refused, "a branch ran in an archived room once chat buried its binding");
    }

    /// The same volume, generated through the cell: `refresh` is allowed in
    /// an archived room, and each one the cell answers adds the member's
    /// command and the cell's own result to the room's timeline.
    #[test]
    fn refreshes_answered_in_an_archived_room_do_not_unarchive_it() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        archive_room(&rig);
        rig.clock.advance_ms(10_000);
        let refreshes: Vec<Event> = (0..72).map(|n| rig.refresh(&MEMBER_KEY, 10 + n, T0 / 1000 + 5)).collect();
        for cycle in 1..=6 {
            rig.cycle(cycle);
        }
        let room = rig.identity.room_id().expect("room");
        {
            let relay = rig.relay.lock().expect("lock");
            for command in &refreshes {
                assert_eq!(results_for(&relay, command).len(), 1, "every refresh is answered in an archived room");
            }
            let after_archive = relay.events.iter().filter(|e| e.channel() == Some(room.as_str()) && e.created_at > T0 / 1000 + 1).count();
            assert!(after_archive > 128, "the refreshes put {after_archive} events above the binding");
        }
        let id = rig.identity.binding_id().expect("binding");
        let request = branch_request(&rig, 2, T0 / 1000 + 20);
        let result = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &request)).expect("command");
        assert_eq!(result.outcome, Outcome::Refused, "a branch ran in an archived room once refreshes buried its binding");
    }

    /// A room whose binding the cell published, and which the relay no longer
    /// holds (a moderator deleted it, say), refuses mutating commands rather
    /// than reading as a room that was never bound; reading still works.
    #[test]
    fn a_published_binding_the_relay_no_longer_holds_refuses_mutations() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").members[0].1 = "moderator";
        rig.cycle(0);
        assert_eq!(published(&rig).lifecycle, Lifecycle::Active);
        let cell = rig.cell.api.pubkey_hex().to_string();
        rig.relay.lock().expect("lock").events.retain(|e| !(e.pubkey == cell && Binding::parse(e.content.as_bytes()).is_ok()));
        let binding = block_on(current_binding(&rig.ctx, &rig.cell, &rig.identity)).expect("read");
        assert_eq!(binding, RoomBinding::Missing);
        let id = rig.identity.binding_id().expect("binding");
        let request = branch_request(&rig, 2, T0 / 1000 + 20);
        let result = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &request)).expect("command");
        assert_eq!(result.outcome, Outcome::Refused, "a branch ran in a room whose binding is gone");
        let refresh = rig.refresh(&MEMBER_KEY, 3, T0 / 1000 + 21);
        let result = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &refresh)).expect("refresh");
        assert_eq!(result.outcome, Outcome::Ok, "reading must still work");
    }

    /// A room bound before the cell kept a durable record of its bindings has
    /// none to read, so its whole timeline is walked: 130 messages after an
    /// archived binding do not hide it.
    #[test]
    fn a_room_bound_before_the_record_is_read_by_walking_its_timeline() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").members[0].1 = "moderator";
        let room = rig.identity.room_id().expect("room");
        let mut binding = Binding::new(&rig.identity, "acct/app · feature/x", None, None, None, rig.cell.scope()).expect("binding");
        assert_eq!(block_on(current_binding(&rig.ctx, &rig.cell, &rig.identity)).expect("read"), RoomBinding::Never);
        binding.lifecycle = Lifecycle::Archived;
        let archived = Event::sign(&CELL_KEY, 9, binding.tags(), &binding.to_json().expect("json"), T0 / 1000 - 300).expect("sign");
        rig.relay.lock().expect("lock").events.push(archived);
        for n in 0..130u64 {
            let chat = Event::sign(&MEMBER_KEY, 9, vec![vec!["h".into(), room.clone()]], &format!("message {n}"), T0 / 1000 - 200 + n)
                .expect("sign");
            rig.relay.lock().expect("lock").events.push(chat);
        }
        assert_eq!(published(&rig).lifecycle, Lifecycle::Archived, "chat hid a binding published before the record");
        let id = rig.identity.binding_id().expect("binding");
        let request = branch_request(&rig, 2, T0 / 1000 - 10);
        let result = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &request)).expect("command");
        assert_eq!(result.outcome, Outcome::Refused, "a branch ran in an archived room bound before the record");
    }

    /// The first binding the cell reserves in a room bound before it kept a
    /// record is ordered after the binding it already published there, found
    /// on the room's timeline, even when that binding's clock is ahead of the
    /// cell's. Only a binding the cell genuinely signed counts: one a member
    /// posted, and one claiming the cell's key over a member's signature, are
    /// later still and must not move the order.
    #[test]
    fn the_first_recorded_binding_follows_a_binding_published_before_the_record() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").members[0].1 = "moderator";
        let room = rig.identity.room_id().expect("room");
        let binding = Binding::new(&rig.identity, "acct/app · feature/x", None, None, None, rig.cell.scope()).expect("binding");
        let content = binding.to_json().expect("json");
        let legacy = Event::sign(&CELL_KEY, 9, binding.tags(), &content, T0 / 1000 + 100).expect("sign");
        let member = Event::sign(&MEMBER_KEY, 9, binding.tags(), &content, T0 / 1000 + 800).expect("sign");
        let forged = forge(Event::sign(&MEMBER_KEY, 9, binding.tags(), &content, T0 / 1000 + 700).expect("sign"), &CELL_KEY);
        {
            let mut relay = rig.relay.lock().expect("lock");
            relay.events.extend([legacy.clone(), forged, member]);
            for n in 0..20u64 {
                let chat = Event::sign(&MEMBER_KEY, 9, vec![vec!["h".into(), room.clone()]], &format!("message {n}"), T0 / 1000 + 101 + n)
                    .expect("sign");
                relay.events.push(chat);
            }
        }
        assert_eq!(published(&rig).lifecycle, Lifecycle::Active);
        let id = rig.identity.binding_id().expect("binding");
        let archive = lifecycle_request(&rig, Verb::BindingArchive, 1);
        let archived = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &archive)).expect("archive");
        assert_eq!(archived.outcome, Outcome::Ok);
        let recorded = block_on(lifecycle::recorded(&rig.ctx, &rig.cell, &rig.identity)).expect("read").expect("recorded");
        assert_eq!(recorded.created_at, legacy.created_at + 1,
            "the archive is not ordered after the binding published before the record");
        let relay = rig.relay.lock().expect("lock");
        let newest = relay.events.iter()
            .filter(|e| e.pubkey == legacy.pubkey && e.verify().is_ok())
            .filter_map(|e| Binding::parse(e.content.as_bytes()).ok().map(|b| (e.created_at, b.lifecycle)))
            .max_by_key(|(at, _)| *at);
        assert_eq!(newest, Some((legacy.created_at + 1, Lifecycle::Archived)),
            "a reader ordering the cell's bindings by time must see the archive last");
    }

    fn assert_lost_mutation_not_resubmitted(verb: Verb, claimed_without_send: bool) {
            let rig = NativeRig::new(&[MEMBER_KEY]);
            rig.relay.lock().expect("lock").members[0].1 = "moderator";
            let writes = Arc::new(Mutex::new(Vec::<Value>::new()));
            let platform = {
                let relay = rig.relay.clone();
                let branches = rig.branches.clone();
                let writes = writes.clone();
                planes::fake::install(move |handle, call| {
                    if handle == planes::BUZZ_EGRESS {
                        return relay.lock().expect("lock").handle(call);
                    }
                    let request: Value = serde_json::from_slice(&call.body).expect("tools/call");
                    let name = request["params"]["name"].as_str().expect("name");
                    let reply = if name == "rcs_branches" {
                        json!({"RcsBranches": {"branches": branches.lock().expect("lock").clone()}})
                    } else if name == "rcs_log" {
                        let rows: Vec<Value> = writes.lock().expect("lock").iter().map(|args| {
                            json!({"commit":"c2", "message":args["message"].as_str().unwrap_or("merge"), "author":"acct", "timestamp_ms":T0})
                        }).collect();
                        json!({"RcsLog":{"commits": rows}})
                    } else {
                        assert_eq!(name, verb.as_str());
                        let mut writes = writes.lock().expect("lock");
                        writes.push(request["params"]["arguments"].clone());
                        if writes.len() == 1 {
                            return Err(ProcessError::Egress("reply lost after the mutation took effect".into()));
                        }
                        match verb {
                            Verb::Commit => json!({"RcsCommitted":{"branch":"feature/x", "commit":"c2"}}),
                            Verb::Merge => json!({"RcsMerged":{"into":"target", "commit":"c2", "status":"merged", "conflicts":[]}}),
                            Verb::Land => json!({"RcsLanded":{"into":"target", "commit":"c2"}}),
                            _ => unreachable!(),
                        }
                    };
                    Ok((200, serde_json::to_vec(&reply).expect("encode")))
                })
            };
            rig.cycle(0);
            let mut command = Command::new(verb, &rig.identity.binding_id().expect("binding"));
            if verb == Verb::Commit {
                command.args.message = Some("save work".into());
                command.args.files = Some(BTreeMap::from([("src/lib.rs".into(), "pub fn work() {}".into())]));
            } else {
                command.args.from = Some("feature/x".into());
                command.args.into = Some("target".into());
            }
            let event = Event::sign(&MEMBER_KEY, command::KIND_PROCESS_COMMAND,
                vec![vec!["h".into(), rig.identity.room_id().expect("room")]],
                &command.to_content().expect("encode"), T0 / 1000 - 1).expect("sign");
            rig.relay.lock().expect("lock").events.push(event.clone());
            let key = format!("mutation/{}/{}/{}", rig.control.app(), command.binding_id, event.id).into_bytes();
            if claimed_without_send {
                let record = json!({"state":"outcome_unknown", "branch":"feature/x", "message":"", "notice":"outcome unknown; dispatch was interrupted"});
                block_on(planes::save(&rig.ctx, &key, &record)).expect("seed interrupted claim");
            }
            for cycle in 1..4 {
                rig.cycle(cycle);
            }
            let expected_writes = if claimed_without_send { 0 } else { 1 };
            assert_eq!(writes.lock().expect("lock").len(), expected_writes,
                "{} was submitted twice or sent after an interrupted dispatch claim", verb.as_str());
            let durable: Value = block_on(planes::load(&rig.ctx, &key)).expect("read").expect("mutation record");
            if verb == Verb::Commit && !claimed_without_send {
                let results = results_for(&rig.relay.lock().expect("lock"), &event);
                assert_eq!(results.len(), 1);
                assert_eq!(results[0].commit.as_deref(), Some("c2"));
                assert_eq!(durable["state"], "completed");
            } else {
                assert_eq!(durable["state"], "outcome_unknown");
                assert!(results_for(&rig.relay.lock().expect("lock"), &event).is_empty(), "an unknown outcome is not reported as success or failure");
                assert!(rig.relay.lock().expect("lock").events.iter().any(|e| e.kind == 9 && e.content.contains("outcome unknown")),
                    "the room must be told that the outcome is unknown");
            }
            drop(platform);
    }

    fn lifecycle_request(rig: &NativeRig, verb: Verb, nonce: usize) -> Event {
        let command = Command::new(verb, &rig.identity.binding_id().expect("binding"));
        Event::sign(&MEMBER_KEY, command::KIND_PROCESS_COMMAND,
            vec![vec!["h".into(), rig.identity.room_id().expect("room")], vec!["n".into(), nonce.to_string()]],
            &command.to_content().expect("encode"), T0 / 1000).expect("sign")
    }

    fn webhook_request(uuid: &str, event: &str, tip: &str) -> IngressRequest {
        let payload = json!({"ref":"refs/heads/feature/x", "ref_type":"branch", "after":tip,
            "repository":{"id":123, "full_name":"example/repo", "clone_url":"https://example.com/repo.git"},
            "pusher":{"name":"test"}});
        let body = serde_json::to_vec(&payload).expect("encode");
        let signature = format!("sha256={}", crypto::to_hex(&crypto::hmac::hmac_sha256(b"test-secret", &body)));
        let mut request = IngressRequest::new("POST", "/webhook", body);
        request.headers = vec![
            (github::DELIVERY_HEADER.into(), uuid.into()),
            (github::EVENT_HEADER.into(), event.into()),
            (github::SIGNATURE_HEADER.into(), signature),
        ];
        request
    }

    #[test]
    fn a_pending_webhook_has_one_owner_until_lease_expiry() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        let request = webhook_request("pending-owner", "push", "c1");
        rig.relay.lock().expect("lock").fail_next_channel = true;
        assert!(block_on(handle_webhook(&rig.ctx, "test-secret", "github.com", &request, "first")).is_err());
        let calls = rig.relay.lock().expect("lock").channel_creates;
        assert!(block_on(handle_webhook(&rig.ctx, "test-secret", "github.com", &request, "competitor")).is_err(),
            "a pending unexpired delivery cannot be taken over");
        assert_eq!(rig.relay.lock().expect("lock").channel_creates, calls);
        rig.clock.advance_ms(100_000);
        block_on(handle_webhook(&rig.ctx, "test-secret", "github.com", &request, "recovery")).expect("expired-owner recovery");
        let key = b"ghdelivery/main/pending-owner";
        let claim: Value = block_on(planes::load(&rig.ctx, key)).expect("read").expect("claim");
        assert_eq!(claim["done"], true);
    }

    #[test]
    fn retrying_a_delivery_keeps_its_plan_after_branch_recreation() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        let call = |request: &IngressRequest| block_on(handle_webhook(&rig.ctx, "test-secret", "github.com", request, "call"));
        call(&webhook_request("base", "push", "c0")).expect("base");
        let interrupted = webhook_request("old-push", "push", "c1");
        rig.relay.lock().expect("lock").fail_next_channel = true;
        assert!(call(&interrupted).is_err());
        call(&webhook_request("deleted", "delete", "")).expect("delete");
        call(&webhook_request("recreated", "push", "c2")).expect("recreate");
        rig.clock.advance_ms(100_000);
        call(&interrupted).expect("resume old delivery");
        let state: BranchState = block_on(planes::load(&rig.ctx, b"ghstate/main/github.com")).expect("read").expect("state");
        let observed = observation_key("123", None, "refs/heads/feature/x");
        assert_eq!(state.tips.get(&observed).map(String::as_str), Some("c2"), "resuming an old plan must not rewrite current state");
        assert_eq!(state.generation(&observed), "g2");
        let original_room = Identity::github("github.com", "123", None, "refs/heads/feature/x", "g1").room_id().expect("room");
        let relay = rig.relay.lock().expect("lock");
        assert!(relay.events.iter().any(|e| e.channel() == Some(original_room.as_str()) && e.content.contains("c1")),
            "the interrupted delivery must finish in its original room");
    }

    #[test]
    fn a_webhook_stops_when_its_lease_expires_during_an_effect() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").expire_during_channel = Some(rig.clock.clone());
        let request = webhook_request("expired-effect", "push", "c1");
        assert!(block_on(handle_webhook(&rig.ctx, "test-secret", "github.com", &request, "owner")).is_err(),
            "the expired handler must stop before publishing the next effect");
        assert!(rig.relay.lock().expect("lock").events.is_empty());
        let claim: Value = block_on(planes::load(&rig.ctx, b"ghdelivery/main/expired-effect")).expect("read").expect("claim");
        assert_eq!(claim["done"], false);
        block_on(handle_webhook(&rig.ctx, "test-secret", "github.com", &request, "takeover")).expect("takeover");
        let claim: Value = block_on(planes::load(&rig.ctx, b"ghdelivery/main/expired-effect")).expect("read").expect("claim");
        assert_eq!(claim["done"], true);
    }

    #[test]
    fn an_interrupted_state_plan_survives_other_completed_deliveries() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        let request = webhook_request("state-plan-crash", "push", "c1");
        let (delivery, payload) = github::admit(b"test-secret", &request.headers, &request.body).expect("admit");
        let plan = github::plan(&delivery, &payload, "github.com", "g1").expect("plan");
        let observed = observation_key("123", None, "refs/heads/feature/x");
        let mut state = BranchState::default();
        state.note_present(&observed, "c0");
        state.note_absent(&observed);
        state.note_present(&observed, "c2");
        state.pending_delivery_plans.insert(delivery.uuid.clone(), plan);
        block_on(planes::save(&rig.ctx, b"ghstate/main/github.com", &state)).expect("state");
        let interrupted = DeliveryClaim { digest: delivery.body_sha256.clone(), at: T0 / 1000, epoch: 1, ..DeliveryClaim::default() };
        block_on(planes::save(&rig.ctx, &delivery.key("main"), &interrupted)).expect("claim");
        for n in 0..1025 {
            let ping = webhook_request(&format!("other-{n}"), "ping", "");
            block_on(handle_webhook(&rig.ctx, "test-secret", "github.com", &ping, "other")).expect("unrelated delivery");
        }
        let state: BranchState = block_on(planes::load(&rig.ctx, b"ghstate/main/github.com")).expect("read").expect("state");
        assert!(state.pending_delivery_plans.contains_key("state-plan-crash"));
        block_on(handle_webhook(&rig.ctx, "test-secret", "github.com", &request, "recover")).expect("recover");
        let state: BranchState = block_on(planes::load(&rig.ctx, b"ghstate/main/github.com")).expect("read").expect("state");
        assert_eq!(state.tips[&observed], "c2");
        assert_eq!(state.generation(&observed), "g2");
        assert!(!state.pending_delivery_plans.contains_key("state-plan-crash"));
        let claim: DeliveryClaim = block_on(planes::load(&rig.ctx, &delivery.key("main"))).expect("read").expect("claim");
        assert!(claim.done);
        assert_eq!(claim.plan.expect("durable plan").identity.expect("identity").generation, "g1");
    }

    #[test]
    fn failed_reply_publication_keeps_ingress_for_redelivery() {
        for signed in [true, false] {
            let rig = NativeRig::new(&[MEMBER_KEY]);
            let mut request = webhook_request(if signed { "reply-ok" } else { "reply-refused" }, "ping", "");
            request.correlation_id = "correlation-1".into();
            if !signed {
                request.headers.retain(|(name, _)| name != github::SIGNATURE_HEADER);
            }
            planes::fake::seed_webhook(serde_json::to_vec(&request).expect("encode"), 1);
            let _ = block_on(webhook_step(&rig.ctx, "test-secret", "github.com"));
            let (pending, replies, acked) = planes::fake::queue_snapshot();
            assert_eq!(pending, 1, "ingress must remain pending after its reply could not be published");
            assert!(replies.is_empty());
            assert!(acked.is_empty());
            block_on(webhook_step(&rig.ctx, "test-secret", "github.com")).expect("redelivery");
            block_on(webhook_step(&rig.ctx, "test-secret", "github.com")).expect("idle");
            let (pending, replies, acked) = planes::fake::queue_snapshot();
            assert_eq!(pending, 0);
            assert_eq!(acked.len(), 1);
            assert_eq!(replies.len(), 1);
            let reply: IngressReply = serde_json::from_slice(&replies[0]).expect("reply");
            assert_eq!(reply.correlation_id, "correlation-1");
            assert_eq!(reply.status, if signed { 202 } else { 401 });
            assert!(reply.headers.iter().any(|(name, value)| name == "content-type" && value == "text/plain; charset=utf-8"));
            if signed {
                assert_eq!(reply.body, b"accepted");
            } else {
                assert!(String::from_utf8_lossy(&reply.body).contains("webhook is missing"));
            }
        }
    }

    /// The reply goes to the group the request names in `reply_to` (the
    /// gateway names its own reply queue, one per gateway instance), and to the
    /// `replies` binding only when the request names none.
    #[test]
    fn a_webhook_reply_goes_to_the_callers_reply_group() {
        for (reply_to, expected) in [("gateway-replies-7", Some("gateway-replies-7")), ("", None)] {
            let rig = NativeRig::new(&[MEMBER_KEY]);
            let mut request = webhook_request(&format!("reply-to-{reply_to}"), "ping", "");
            request.correlation_id = "correlation-1".into();
            request.reply_to = reply_to.into();
            planes::fake::seed_webhook(serde_json::to_vec(&request).expect("encode"), 0);
            block_on(webhook_step(&rig.ctx, "test-secret", "github.com")).expect("step");
            let (pending, replies, _) = planes::fake::queue_snapshot();
            assert_eq!((pending, replies.len()), (0, 1));
            assert_eq!(planes::fake::reply_groups(), vec![expected.map(str::to_string)],
                "reply_to {reply_to:?} answered on the wrong queue");
        }
    }

    /// A commit in the trunk's room and a merge into the trunk move the branch
    /// every other descends from, as a land does, so they need the authority a
    /// land needs. A member's are refused before anything reaches the control
    /// plane; a moderator's run.
    #[test]
    fn a_member_cannot_commit_on_or_merge_into_the_trunk() {
        for (role, allowed) in [("member", false), ("moderator", true)] {
            let rig = NativeRig::new(&[MEMBER_KEY]);
            rig.relay.lock().expect("lock").members[0].1 = role;
            let writes = Arc::new(Mutex::new(Vec::<String>::new()));
            let platform = {
                let relay = rig.relay.clone();
                let writes = writes.clone();
                planes::fake::install(move |handle, call| {
                    if handle == planes::BUZZ_EGRESS {
                        return relay.lock().expect("lock").handle(call);
                    }
                    let request: Value = serde_json::from_slice(&call.body).expect("tools/call");
                    let name = request["params"]["name"].as_str().expect("name").to_string();
                    let reply = match name.as_str() {
                        "rcs_log" => json!({"RcsLog":{"commits": []}}),
                        "rcs_commit" => json!({"RcsCommitted":{"branch":"main", "commit":"c2"}}),
                        "rcs_merge" => json!({"RcsMerged":{"into":"main", "commit":"c2", "status":"merged", "conflicts":[]}}),
                        other => panic!("unexpected control call {other}"),
                    };
                    if name != "rcs_log" {
                        writes.lock().expect("lock").push(name);
                    }
                    Ok((200, serde_json::to_vec(&reply).expect("encode")))
                })
            };
            let trunk = Identity::rcs("acct", "acct/app", "refs/heads/main", "g1");
            let cases = [(trunk, Verb::Commit), (rig.identity.clone(), Verb::Merge)];
            for (n, (identity, verb)) in cases.into_iter().enumerate() {
                let binding_id = identity.binding_id().expect("binding");
                let mut command = Command::new(verb, &binding_id);
                if verb == Verb::Commit {
                    command.args.message = Some("straight onto the trunk".into());
                    command.args.files = Some(BTreeMap::from([("src/lib.rs".into(), "pub fn work() {}".into())]));
                } else {
                    command.args.from = Some("feature/x".into());
                    command.args.into = Some("main".into());
                }
                let request = Event::sign(&MEMBER_KEY, command::KIND_PROCESS_COMMAND,
                    vec![vec!["h".into(), identity.room_id().expect("room")], vec!["n".into(), n.to_string()]],
                    &command.to_content().expect("encode"), T0 / 1000).expect("sign");
                let result = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &identity, &BranchState::default(), &binding_id, &request))
                    .expect("answered");
                if allowed {
                    assert_eq!(result.outcome, Outcome::Ok, "{role} {verb:?}: {}", result.detail);
                } else {
                    assert_eq!(result.outcome, Outcome::Refused, "{role} {verb:?} reached the trunk: {}", result.detail);
                }
            }
            let sent = writes.lock().expect("lock").clone();
            let expected: Vec<String> = if allowed { vec!["rcs_commit".into(), "rcs_merge".into()] } else { Vec::new() };
            assert_eq!(sent, expected, "{role}");
            drop(platform);
        }
    }

    #[test]
    fn native_progress_does_not_erase_a_committed_peer_update() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").peer_write = Some(PeerStateWrite {
            trigger: "Branch feature/x appeared".into(),
            key: state_key("main", "acct/app"),
            branch: "peer-branch".into(),
            recreate: false,
            plan: None,
        });
        rig.cycle(0);
        let state = rig.state();
        assert_eq!(state.tips.get("peer-branch").map(String::as_str), Some("peer-tip"), "a stale native checkpoint erased the peer branch");
        assert_eq!(state.generation("peer-branch"), "g1");
        assert!(state.has_answered("peer-command"));
    }

    #[test]
    fn github_sweep_progress_preserves_a_peers_generation_and_plan() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        let identity = Identity::github("github.com", "123", None, "refs/heads/feature/x", "g1");
        let observed = observation_key("123", None, "refs/heads/feature/x");
        let mut state = BranchState::default();
        state.note_present(&observed, "c1");
        block_on(planes::save(&rig.ctx, b"ghstate/main/github.com", &state)).expect("seed");
        let request = Command::new(Verb::Refresh, &identity.binding_id().expect("binding"));
        let event = Event::sign(&MEMBER_KEY, command::KIND_PROCESS_COMMAND,
            vec![vec!["h".into(), identity.room_id().expect("room")]],
            &request.to_content().expect("encode"), T0 / 1000).expect("sign");
        rig.relay.lock().expect("lock").events.push(event.clone());
        rig.relay.lock().expect("lock").peer_write = Some(PeerStateWrite {
            trigger: "refresh:".into(),
            key: b"ghstate/main/github.com".to_vec(),
            branch: observed.clone(),
            recreate: true,
            plan: Some(github::Plan { identity: Some(Identity::github("github.com", "123", None, "refs/heads/feature/x", "g2")),
                summary: "peer delivery".into(), drafts: Vec::new() }),
        });
        assert_eq!(block_on(github_command_sweep(&rig.ctx, "github.com")).expect("sweep"), 1);
        let state: BranchState = block_on(planes::load(&rig.ctx, b"ghstate/main/github.com")).expect("read").expect("state");
        assert_eq!(state.tips.get(&observed).map(String::as_str), Some("peer-tip"), "a stale sweep erased the peer branch tip");
        assert_eq!(state.generation(&observed), "g2");
        assert!(state.pending_delivery_plans.contains_key("peer-delivery"));
        assert!(state.retired.iter().any(|room| room.name == observed && room.generation == "g1"));
        assert!(state.has_answered("peer-command"));
        assert!(state.has_answered(&event.id));
    }

    #[test]
    fn an_obsolete_snapshot_is_resampled_before_its_effects_are_published() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        let key = state_key("main", "acct/app");
        rig.relay.lock().expect("lock").peer_write = Some(PeerStateWrite {
            trigger: "control-snapshot".into(), key: key.clone(), branch: "peer-branch".into(), recreate: false, plan: None,
        });
        assert!(block_on(native_cycle(&rig.ctx, &rig.cell, &rig.control, &key, 0)).is_err());
        assert!(rig.relay.lock().expect("lock").events.is_empty(), "a stale snapshot must not publish any effects");
        assert_eq!(rig.state().tips.get("peer-branch").map(String::as_str), Some("peer-tip"));
        *rig.branches.lock().expect("lock") = json!([["feature/x", "c1"], ["peer-branch", "peer-tip"]]);
        rig.cycle(1);
        let state = rig.state();
        assert_eq!(state.tips.get("feature/x").map(String::as_str), Some("c1"));
        assert_eq!(state.generation("peer-branch"), "g1");
        assert!(state.has_answered("peer-command"));
    }

    #[test]
    fn stale_command_progress_keeps_a_newer_walk_and_refreshed_retirement() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        let key = b"progress-test";
        let mut before = BranchState::default();
        before.command_walk.insert("room".into(), cells::Walk { cursor: "page-1".into(), started_ms: 1 });
        before.note_retired("branch", "g1", 1);
        let mut after = before.clone();
        after.command_walk.remove("room");
        after.retired.clear();
        after.mark_answered("own-command");
        let mut current = before.clone();
        current.command_walk.insert("room".into(), cells::Walk { cursor: "page-2".into(), started_ms: 1 });
        current.note_retired("branch", "g1", 2);
        current.mark_answered("peer-command");
        block_on(planes::save(&rig.ctx, key, &current)).expect("peer checkpoint");
        block_on(state::save_commands(&rig.ctx, key, &before, &after)).expect("merge");
        let stored: BranchState = block_on(planes::load(&rig.ctx, key)).expect("read").expect("state");
        assert_eq!(stored.command_walk["room"].cursor, "page-2");
        assert_eq!(stored.retired.len(), 1);
        assert_eq!(stored.retired[0].at_ms, 2);
        assert!(stored.has_answered("own-command"));
        assert!(stored.has_answered("peer-command"));
    }

    #[test]
    fn an_interrupted_observation_keeps_its_first_snapshot() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").fail_after_observation = true;
        let key = state_key("main", "acct/app");
        assert!(block_on(native_cycle(&rig.ctx, &rig.cell, &rig.control, &key, 0)).is_err());
        *rig.branches.lock().expect("lock") = json!([["feature/x", "c2"]]);
        rig.clock.advance_ms(1_000);
        rig.cycle(1);
        let relay = rig.relay.lock().expect("lock");
        let appearances: Vec<&Event> = relay.events.iter().filter(|e| e.content.starts_with("Branch feature/x appeared")).collect();
        assert_eq!(appearances.len(), 1, "a partially published observation must not be rediscovered as a new appearance");
        assert!(relay.events.iter().any(|e| e.content.contains("moved c1 → c2")), "the next snapshot must advance from the durably observed tip");
    }

    #[test]
    fn lost_observation_replies_replay_the_original_event_identity() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let key = state_key("main", "acct/app");
        for (cycle, branches, needle) in [
            (1, json!([["feature/x", "c2"]]), "moved c1 → c2"),
            (3, json!([]), "is gone (last seen at c2)"),
        ] {
            *rig.branches.lock().expect("lock") = branches;
            rig.relay.lock().expect("lock").lose_next_note = Some(needle.into());
            assert!(block_on(native_cycle(&rig.ctx, &rig.cell, &rig.control, &key, cycle)).is_err());
            rig.clock.advance_ms(1_000);
            rig.cycle(cycle + 1);
            assert_eq!(rig.relay.lock().expect("lock").events.iter().filter(|e| e.content.contains(needle)).count(), 1,
                "a lost publication reply must not allocate another event");
        }
    }

    #[test]
    fn lifecycle_commands_in_one_second_follow_command_order() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").members[0].1 = "moderator";
        rig.cycle(0);
        for n in 0..8 {
            let (verb, expected) = if n % 2 == 0 {
                (Verb::BindingArchive, Lifecycle::Archived)
            } else {
                (Verb::BindingCreate, Lifecycle::Active)
            };
            let request = lifecycle_request(&rig, verb, n);
            let result = block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(),
                &rig.identity.binding_id().expect("binding"), &request)).expect("command");
            assert_eq!(result.outcome, Outcome::Ok);
            let binding = published(&rig);
            assert_eq!(binding.lifecycle, expected, "transition {n} must supersede its predecessor at the same clock");
        }
    }

    #[test]
    fn retrying_an_archive_keeps_its_order_before_a_later_reactivation() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").members[0].1 = "moderator";
        rig.cycle(0);
        let archive = lifecycle_request(&rig, Verb::BindingArchive, 1);
        let active = lifecycle_request(&rig, Verb::BindingCreate, 2);
        let id = rig.identity.binding_id().expect("binding");
        rig.relay.lock().expect("lock").fail_next_archive_note = true;
        assert!(block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &archive)).is_err());
        rig.clock.advance_ms(1_000);
        block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &active)).expect("reactivate");
        rig.clock.advance_ms(1_000);
        block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &archive)).expect("retry");
        let binding = published(&rig);
        assert_eq!(binding.lifecycle, Lifecycle::Active, "an older command retry must not archive a newer lifecycle");
        let relay = rig.relay.lock().expect("lock");
        let archives = relay.events.iter().filter(|e| Binding::parse(e.content.as_bytes()).is_ok_and(|b| b.lifecycle == Lifecycle::Archived)).count();
        assert_eq!(archives, 1, "retry must reuse the signed event");
    }

    #[test]
    fn a_reserved_lifecycle_head_is_checkpointed_before_a_later_change() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").members[0].1 = "moderator";
        rig.cycle(0);
        let archive = lifecycle_request(&rig, Verb::BindingArchive, 5);
        let active = lifecycle_request(&rig, Verb::BindingCreate, 6);
        let id = rig.identity.binding_id().expect("binding");
        let mut binding = Binding::new(&rig.identity, "acct/app · feature/x", None, None, None, rig.cell.scope()).expect("binding");
        binding.lifecycle = Lifecycle::Archived;
        let event = Event::sign(&CELL_KEY, 9, binding.tags(), &binding.to_json().expect("json"), T0 / 1000 + 1).expect("sign");
        let prefix = format!("binding-order/{}/{}", rig.cell.api.pubkey_hex(), rig.identity.room_id().expect("room"));
        block_on(planes::save(&rig.ctx, format!("{prefix}/head").as_bytes(),
            &json!({"operation":archive.id, "event":event}))).expect("interrupted reservation");
        block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &active)).expect("reactivate");
        block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &archive)).expect("resume archive");
        let binding = published(&rig);
        assert_eq!(binding.lifecycle, Lifecycle::Active);
        let recorded: Event = block_on(planes::load(&rig.ctx, format!("{prefix}/operation/{}", archive.id).as_bytes()))
            .expect("read").expect("receipt");
        assert_eq!(recorded.created_at, T0 / 1000 + 1);
    }

    #[test]
    fn lifecycle_order_defers_before_exceeding_the_relay_clock_window() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.relay.lock().expect("lock").members[0].1 = "moderator";
        rig.cycle(0);
        let request = lifecycle_request(&rig, Verb::BindingArchive, 10);
        let id = rig.identity.binding_id().expect("binding");
        let binding = Binding::new(&rig.identity, "d", None, None, None, rig.cell.scope()).expect("binding");
        let event = Event::sign(&CELL_KEY, 9, binding.tags(), &binding.to_json().expect("json"), T0 / 1000 + 900).expect("sign");
        let prefix = format!("binding-order/{}/{}", rig.cell.api.pubkey_hex(), rig.identity.room_id().expect("room"));
        block_on(planes::save(&rig.ctx, format!("{prefix}/head").as_bytes(),
            &json!({"operation":"earlier", "event":event}))).expect("head");
        assert!(block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &request)).is_err());
        let absent: Option<Event> = block_on(planes::load(&rig.ctx, format!("{prefix}/operation/{}", request.id).as_bytes())).expect("read");
        assert!(absent.is_none(), "a deferred operation must not reserve an inadmissible timestamp");
        rig.clock.advance_ms(1_000);
        block_on(execute(&rig.ctx, &rig.cell, &rig.control, &rig.identity, &BranchState::default(), &id, &request)).expect("clock caught up");
        let recorded: Event = block_on(planes::load(&rig.ctx, format!("{prefix}/operation/{}", request.id).as_bytes())).expect("read").expect("receipt");
        assert_eq!(recorded.created_at, T0 / 1000 + 901);
    }

    #[test]
    fn a_lost_commit_reply_is_reconciled_without_another_write() {
        assert_lost_mutation_not_resubmitted(Verb::Commit, false);
    }

    #[test]
    fn a_lost_merge_reply_stays_unknown_without_another_write() {
        assert_lost_mutation_not_resubmitted(Verb::Merge, false);
    }

    #[test]
    fn a_lost_land_reply_stays_unknown_without_another_write() {
        assert_lost_mutation_not_resubmitted(Verb::Land, false);
    }

    #[test]
    fn a_claim_interrupted_before_dispatch_stays_unknown_without_a_write() {
        assert_lost_mutation_not_resubmitted(Verb::Merge, true);
    }

    #[test]
    fn retiring_a_branch_preserves_an_unknown_mutation_outcome() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let mut command = Command::new(Verb::Merge, &rig.identity.binding_id().expect("binding"));
        command.args.from = Some("feature/x".into());
        command.args.into = Some("target".into());
        let event = Event::sign(&MEMBER_KEY, command::KIND_PROCESS_COMMAND,
            vec![vec!["h".into(), rig.identity.room_id().expect("room")]],
            &command.to_content().expect("encode"), T0 / 1000 - 1).expect("sign");
        let key = format!("mutation/{}/{}/{}", rig.control.app(), command.binding_id, event.id).into_bytes();
        let record = json!({"state":"outcome_unknown", "branch":"feature/x", "message":"", "notice":"outcome unknown; dispatch was interrupted"});
        block_on(planes::save(&rig.ctx, &key, &record)).expect("save");
        rig.relay.lock().expect("lock").events.push(event.clone());
        *rig.branches.lock().expect("lock") = json!([]);
        rig.cycle(1);
        rig.cycle(2);
        let relay = rig.relay.lock().expect("lock");
        assert!(results_for(&relay, &event).is_empty(), "retirement must not assert that an unknown mutation was refused");
        assert!(relay.events.iter().any(|e| e.kind == 9 && e.content.contains("outcome unknown")));
        drop(relay);
        let durable: Value = block_on(planes::load(&rig.ctx, &key)).expect("read").expect("record");
        assert_eq!(durable["state"], "outcome_unknown");
    }

    /// An unknown merge outcome is told to the room once and then settled: a
    /// retired room holding it is still forgotten past its window, instead of
    /// the unanswerable command holding the room and the walk forever.
    #[test]
    fn an_unknown_mutation_outcome_does_not_hold_a_retired_room_forever() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let mut command = Command::new(Verb::Merge, &rig.identity.binding_id().expect("binding"));
        command.args.from = Some("feature/x".into());
        command.args.into = Some("target".into());
        let event = Event::sign(&MEMBER_KEY, command::KIND_PROCESS_COMMAND,
            vec![vec!["h".into(), rig.identity.room_id().expect("room")]],
            &command.to_content().expect("encode"), T0 / 1000 - 1).expect("sign");
        let key = format!("mutation/{}/{}/{}", rig.control.app(), command.binding_id, event.id).into_bytes();
        let record = json!({"state":"outcome_unknown", "branch":"feature/x", "message":"", "notice":"outcome unknown; dispatch was interrupted"});
        block_on(planes::save(&rig.ctx, &key, &record)).expect("save");
        rig.relay.lock().expect("lock").events.push(event.clone());
        *rig.branches.lock().expect("lock") = json!([]);
        for cycle in 1..4 {
            rig.cycle(cycle);
        }
        rig.clock.advance_ms(cells::RETIRED_ROOM_ANSWER_MS + 1);
        for cycle in 4..20 {
            rig.cycle(cycle);
            if rig.state().retired.is_empty() {
                break;
            }
        }
        assert!(rig.state().retired.is_empty(), "an unknown outcome is settled, not owed forever");
        let relay = rig.relay.lock().expect("lock");
        assert!(results_for(&relay, &event).is_empty(), "still no false result");
        assert_eq!(relay.events.iter().filter(|e| e.kind == 9 && e.content.contains("outcome unknown")).count(), 1,
            "the room is told once");
        drop(relay);
        let durable: Value = block_on(planes::load(&rig.ctx, &key)).expect("read").expect("record");
        assert_eq!(durable["state"], "outcome_unknown");
    }

    #[test]
    fn transient_failures_do_not_starve_a_healthy_command_behind_them() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let deferred: Vec<Event> = (0..16).map(|n| rig.refresh(&MEMBER_KEY, n, T0 / 1000 - 20 + n as u64 / 2)).collect();
        let healthy = rig.refresh(&MEMBER_KEY, 100, T0 / 1000 - 5);
        rig.relay.lock().expect("lock").transient_binding_reads = 16;
        assert_eq!(rig.cycle(1), 1, "the healthy command is attempted after transient failures");
        assert_eq!(results_for(&rig.relay.lock().expect("lock"), &healthy).len(), 1);
        for command in &deferred {
            assert!(results_for(&rig.relay.lock().expect("lock"), command).is_empty());
        }
        assert_eq!(rig.cycle(2), 16);
        assert_eq!(rig.cycle(3), 0);
        for command in &deferred {
            assert_eq!(results_for(&rig.relay.lock().expect("lock"), command).len(), 1);
        }
    }

    #[test]
    fn pending_answers_share_the_command_cycle_budget() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let pending = rig.refresh(&MEMBER_KEY, 0, T0 / 1000 - 10);
        rig.relay.lock().expect("lock").fail_next_result = true;
        let key = state_key("main", "acct/app");
        assert!(block_on(native_cycle(&rig.ctx, &rig.cell, &rig.control, &key, 1)).is_err());
        assert!(results_for(&rig.relay.lock().expect("lock"), &pending).is_empty());
        let mut commands = vec![pending];
        commands.extend((1..=16).map(|n| rig.refresh(&MEMBER_KEY, n, T0 / 1000 - 5)));
        assert_eq!(block_on(native_cycle(&rig.ctx, &rig.cell, &rig.control, &key, 2)).expect("cycle"), 16,
            "completed pending answers are included in the returned count");
        assert_eq!(commands.iter().map(|command| results_for(&rig.relay.lock().expect("lock"), command).len()).sum::<usize>(), 16,
            "pending publication consumes one of the sixteen slots");
        assert_eq!(rig.cycle(3), 1);
        assert_eq!(rig.cycle(4), 0);
        for command in &commands {
            assert_eq!(results_for(&rig.relay.lock().expect("lock"), command).len(), 1);
        }
    }

    #[test]
    fn a_failed_summary_resumes_even_when_its_result_is_visible() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let command = rig.refresh(&MEMBER_KEY, 0, T0 / 1000 - 10);
        rig.relay.lock().expect("lock").fail_next_summary = true;
        let state_key = state_key("main", "acct/app");
        assert!(block_on(native_cycle(&rig.ctx, &rig.cell, &rig.control, &state_key, 1)).is_err());
        let room = rig.identity.room_id().expect("room");
        let key = answer_key(&room, &command.id);
        let pending: AnswerRecord = block_on(planes::load(&rig.ctx, &key)).expect("read").expect("record");
        assert!(!pending.published);
        assert_eq!(results_for(&rig.relay.lock().expect("lock"), &command).len(), 1);
        rig.cycle(2);
        rig.cycle(3);
        let finished: AnswerRecord = block_on(planes::load(&rig.ctx, &key)).expect("read").expect("record");
        assert!(finished.published);
        let relay = rig.relay.lock().expect("lock");
        assert_eq!(results_for(&relay, &command).len(), 1);
        assert_eq!(relay.events.iter().filter(|e| e.kind == 9 && e.content == finished.summary).count(), 1);
    }

    #[test]
    fn an_answer_recorded_before_a_crash_is_published_from_its_record() {
        let rig = NativeRig::new(&[MEMBER_KEY]);
        rig.cycle(0);
        let command = rig.refresh(&MEMBER_KEY, 0, T0 / 1000 - 10);
        let room = rig.identity.room_id().expect("room");
        let result = CommandResult::new(&command.id, Verb::Refresh, "b", Outcome::Ok, "recorded earlier");
        let event = Event::sign(&CELL_KEY, command::KIND_PROCESS_RESULT, result.tags(&room), &result.to_content().expect("encode"), T0 / 1000 - 5)
            .expect("sign");
        let record = AnswerRecord { event, summary: result.summary(), published: false };
        block_on(planes::save(&rig.ctx, &answer_key(&room, &command.id), &record)).expect("save");

        rig.cycle(1);
        rig.cycle(2);
        let answers = results_for(&rig.relay.lock().expect("lock"), &command);
        assert_eq!(answers.len(), 1, "published once");
        assert_eq!(answers[0].detail, "recorded earlier");
    }
}

