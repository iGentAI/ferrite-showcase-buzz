//! Workflows: definitions clients write, and the engine that runs them.
//!
//! ## Why this module exists
//!
//! Kind **30620** (definition) and the whole **46001–46012** lifecycle were
//! declared in the registry, and the only reference to any of them outside it was
//! the feed's needs-action filter. A workflow could therefore be written, stored,
//! audited and delivered — and would never run. That is the accepted-but-inert
//! shape this codebase removes wherever it appears, and it is why every workflow
//! command in the browser client could only be refused.
//!
//! ## Built out of what already exists
//!
//! Three deliberate reuses, each of which keeps a rule in ONE place:
//!
//! * **An agent step posts a relay-signed message that MENTIONS the persona.** It
//!   does not reach into the turn queue. The mention→dispatch path already
//!   decides who may invoke an agent, rate-limits it, and audits it; routing
//!   through it means a workflow cannot become a second way to spend provider
//!   budget under different rules.
//! * **Every emitted event goes through [`crate::relay::ingest::admit`] +
//!   [`crate::relay::ingest::drive_relay_minted`]**, so it is stored, indexed,
//!   audited and delivered by exactly the code a client event runs.
//! * **Nothing is minted from entropy.** This app has none. A run id is a digest
//!   over `(workflow, trigger)`, an approval token is an HMAC under the relay
//!   identity, and lifecycle events are stamped with the trigger's ADMISSION
//!   instant — so a re-driven saga rebuilds byte-identical events whose ids are
//!   already admitted. Determinism IS the dedup; nothing here counts attempts.
//!
//! ## The two bounds that make this safe on a hot path
//!
//! 1. **A channel with no workflows costs one point read.** Admitting a 30620
//!    writes a `wfch/{c}/{channel}` marker; a message checks that marker and only
//!    scans when it is present. Without it, every message in the workspace would
//!    pay a scan for a feature most channels do not use.
//! 2. **A workflow cannot retrigger itself.** A run is capped at
//!    [`MAX_RUN_STEPS`], and a definition whose trigger would match its own
//!    emitted message is refused at VALIDATION rather than discovered at runtime
//!    — a loop that is only bounded is still a loop that runs.

use ferrite_app::client::{ApeClusterClient, StoreClusterClient, TopicLogClient};
use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json::{self, Value};

use crate::relay::crypto::hmac::hmac_sha256;
use crate::relay::crypto::sha256::sha256_concat;
use crate::relay::crypto::to_hex;
use crate::relay::error::{BuzzError, Result};
use crate::relay::event::{Event, PubKey};
use crate::relay::{keys, kind, store};

/// The most steps one run executes. A bound is required because a run emits
/// events, and events can trigger runs; this is the ceiling that makes the
/// depth argument finite even if a future trigger form slips past validation.
pub const MAX_RUN_STEPS: usize = 32;

/// The most workflows one channel may define. Each admitted message in a
/// workflow-bearing channel reads them all, so this bounds the per-message cost.
pub const MAX_CHANNEL_WORKFLOWS: usize = 32;

/// The most workflow steps ONE triggering message may start, summed across every
/// workflow it matches.
///
/// [`MAX_RUN_STEPS`] bounds one run and [`MAX_CHANNEL_WORKFLOWS`] bounds how many
/// definitions are read, but together they let one rate-limited message fan out
/// into 32 × 32 steps — about three thousand relay-signed events, none of them
/// subject to a per-author limit, all inside the triggering request. This caps
/// the product at one maximal workflow's worth. Workflows are considered in the
/// channel's stable definition order, and one whose steps would exceed what is
/// left is not started (and is logged), so every re-drive of the same trigger
/// makes the same choice.
pub const MAX_TRIGGER_STEPS: usize = MAX_RUN_STEPS;

/// The longest a step's posted text may be.
pub const MAX_STEP_TEXT: usize = 4096;

/// The longest a workflow name may be.
pub const MAX_WORKFLOW_NAME: usize = 128;

/// How long an approval gate stays open, in seconds. A gate that never expires
/// is a run that never ends, and a run that never ends holds its state forever.
pub const APPROVAL_TTL_SECS: u64 = 7 * 24 * 60 * 60;

/// What a workflow does when an event matches its trigger.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", tag = "type", rename_all = "snake_case")]
pub enum Step {
    /// Post a relay-signed message into the workflow's channel.
    Post { id: String, text: String },
    /// Post a message MENTIONING an agent, which the ordinary dispatch path then
    /// picks up. Deliberately not a direct turn enqueue — see the module header.
    Agent {
        id: String,
        agent: String,
        prompt: String,
    },
    /// Pause until an approval arrives for this step's token.
    Approval {
        id: String,
        /// Hex pubkey who may approve, or `"moderator"` for any moderator.
        approver: String,
    },
}

impl Step {
    /// The step's id — what a lifecycle event names and what an approval token
    /// is derived from.
    #[must_use]
    pub fn id(&self) -> &str {
        match self {
            Self::Post { id, .. } | Self::Agent { id, .. } | Self::Approval { id, .. } => id,
        }
    }

    /// The text this step would post, if it posts one. Used by validation to
    /// prove a workflow cannot trigger itself.
    #[must_use]
    pub fn posted_text(&self) -> Option<std::borrow::Cow<'_, str>> {
        match self {
            Self::Post { text, .. } => Some(std::borrow::Cow::Borrowed(text)),
            // Exactly what `execute` posts, so the self-trigger refusal sees the
            // `@agent` prefix a trigger could match.
            Self::Agent { agent, prompt, .. } => {
                Some(std::borrow::Cow::Owned(agent_step_text(agent, prompt)))
            }
            Self::Approval { .. } => None,
        }
    }
}

/// When a workflow runs.
///
/// One form today — a message in the workflow's channel, optionally containing a
/// phrase. Deliberately narrow: every trigger form is a way for one event to
/// cause others, so each needs its own self-trigger analysis, and shipping one
/// that is provably safe beats shipping four that are argued to be.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Trigger {
    /// Case-insensitive substring the message must contain. Empty = every
    /// message in the channel.
    #[serde(default)]
    pub contains: String,
}

impl Trigger {
    /// Whether an admitted message fires this trigger.
    #[must_use]
    pub fn matches(&self, content: &str) -> bool {
        if self.contains.is_empty() {
            return true;
        }
        content
            .to_lowercase()
            .contains(&self.contains.to_lowercase())
    }
}

/// A workflow definition, as carried in a kind-30620 event's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Definition {
    pub name: String,
    /// The trigger. Absent = the workflow only runs when triggered by hand.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub on: Option<Trigger>,
    pub steps: Vec<Step>,
    /// The YAML the author actually wrote, kept verbatim so an editor can show
    /// it back rather than a re-serialization of the parse.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub yaml: Option<String>,
    /// `active` or `paused`. A paused workflow is stored and does not fire.
    #[serde(default = "default_status")]
    pub status: String,
    /// Epoch seconds the definition was FIRST created, carried across updates so
    /// a replaceable event can report a creation time it would otherwise lose.
    #[serde(default)]
    pub created_at: u64,
}

fn default_status() -> String {
    "active".to_string()
}

impl Definition {
    /// Whether this workflow fires on admitted messages at all.
    #[must_use]
    pub fn is_active(&self) -> bool {
        self.status == "active" && self.on.is_some()
    }
}

/// Parse and validate a definition, returning the canonical form to store.
///
/// The self-trigger check is the one that matters: a workflow whose trigger
/// would match text one of its own steps posts is refused HERE, at write time,
/// rather than discovered as a run that bounces off the step cap. A loop that is
/// merely bounded is still a loop that runs, emits, audits and delivers.
pub fn validate_definition(value: &Value) -> Result<Definition> {
    let refuse = |why: &str| BuzzError::InvalidEvent(format!("workflow: {why}"));
    let mut def: Definition = serde_json::from_value(value.clone())
        .map_err(|e| refuse(&format!("definition is not readable ({e})")))?;

    let name = def.name.trim().to_string();
    if name.is_empty() || name.chars().count() > MAX_WORKFLOW_NAME {
        return Err(refuse("name must be 1–128 characters"));
    }
    def.name = name;

    if def.status != "active" && def.status != "paused" {
        return Err(refuse("status must be active or paused"));
    }
    if def.steps.is_empty() {
        return Err(refuse("a workflow needs at least one step"));
    }
    if def.steps.len() > MAX_RUN_STEPS {
        return Err(refuse("a workflow may not declare more than 32 steps"));
    }

    let mut seen: Vec<&str> = Vec::with_capacity(def.steps.len());
    for step in &def.steps {
        let id = step.id();
        if id.is_empty() || id.len() > 64 {
            return Err(refuse("a step id must be 1–64 characters"));
        }
        if seen.contains(&id) {
            return Err(refuse("two steps share an id"));
        }
        seen.push(id);
        if let Some(text) = step.posted_text() {
            if text.is_empty() || text.len() > MAX_STEP_TEXT {
                return Err(refuse("a step's text must be 1–4096 bytes"));
            }
        }
        match step {
            Step::Agent { agent, .. } => {
                // The runtime's own agent-id rule, written HERE rather than
                // borrowed: `agents` exposes no validator, and inventing a
                // dependency on one that does not exist is how the previous
                // revision failed to compile.
                let ok = !agent.is_empty()
                    && agent.len() <= 64
                    && agent
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_'));
                if !ok {
                    return Err(refuse("a step names an agent id that cannot exist"));
                }
            }
            Step::Approval { approver, .. } => {
                if approver != "moderator" && crate::relay::crypto::from_hex32(approver).is_none() {
                    return Err(refuse(
                        "an approval's approver must be a 32-byte hex pubkey or \"moderator\"",
                    ));
                }
            }
            Step::Post { .. } => {}
        }
    }

    // The self-trigger refusal. Checked against EVERY posted text, because a run
    // that posts one matching message is a run that starts another run.
    if let Some(trigger) = &def.on {
        for step in &def.steps {
            if let Some(text) = step.posted_text() {
                if trigger.matches(&text) {
                    return Err(refuse(
                        "this workflow's trigger matches text one of its own steps posts, so \
                         running it would trigger it again",
                    ));
                }
            }
        }
    }
    Ok(def)
}

/// The definition carried by a kind-30620 event.
pub fn definition_of(event: &Event) -> Result<Definition> {
    let Some(channel) = event.channel_id() else {
        return Err(BuzzError::InvalidEvent(
            "workflow: definition must carry a non-empty h channel tag".into(),
        ));
    };
    if !keys::is_safe_component(channel) {
        return Err(BuzzError::InvalidEvent(
            "workflow: h channel tag must be a non-empty safe key component".into(),
        ));
    }
    let Some(id) = workflow_id(event) else {
        return Err(BuzzError::InvalidEvent(
            "workflow: definition must carry a non-empty d tag".into(),
        ));
    };
    if !keys::is_safe_component(id) {
        return Err(BuzzError::InvalidEvent(
            "workflow: d tag must be a non-empty safe key component (at most 96 bytes, no '/', NUL, control characters, or leading '!')".into(),
        ));
    }
    let value: Value = serde_json::from_str(&event.content)
        .map_err(|e| BuzzError::InvalidEvent(format!("workflow: content is not JSON ({e})")))?;
    validate_definition(&value)
}

/// The workflow id a 30620 addresses — its `d` tag, which is what makes the
/// definition replaceable per workflow.
#[must_use]
pub fn workflow_id(event: &Event) -> Option<&str> {
    event.tag_values("d").next()
}

/// Where a run has got to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "snake_case")]
pub enum RunStatus {
    Running,
    /// Halted at an approval gate, waiting for a decision.
    AwaitingApproval,
    Completed,
    Failed,
    Cancelled,
}

/// The workflows one triggering message admitted, as first decided.
///
/// [`MAX_TRIGGER_STEPS`] is a bound on what ONE message may start, so the
/// decision has to be made once. Recomputing it on every pass over the trigger
/// from the channel's CURRENT definitions would let a crash-recovery re-drive
/// after an edit (a workflow shortened, removed, or added) start a workflow the
/// first pass had refused, so the message starts more than the bound across its
/// passes, or skip one the first pass had started and strand its run. The record
/// is written create-if-absent before any run starts and every later pass
/// replays it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct TriggerAdmission {
    pub admitted: Vec<AdmittedWorkflow>,
}

/// One workflow a trigger admitted.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct AdmittedWorkflow {
    pub workflow_id: String,
    /// Hex id of the 30620 the admission was decided against, which the run
    /// pins; a re-drive runs this definition, never the one indexed now.
    pub definition_event: String,
    /// What it cost against [`MAX_TRIGGER_STEPS`].
    pub cost: usize,
}

/// The admission a trigger's first pass decided, deciding it now if this is
/// that pass.
///
/// An empty decision is not recorded: nothing ran, so a later pass has nothing
/// to stay consistent with, and recording it would put a write on every
/// non-matching message in a workflow-bearing channel.
async fn trigger_admission(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
    event: &Event,
    trigger_hex: &str,
) -> Result<TriggerAdmission> {
    let key = keys::workflow_trigger_admission(community, trigger_hex);
    if let Some(decided) = store::get_doc::<TriggerAdmission>(store, &key).await? {
        return Ok(decided);
    }
    let mut budget = MAX_TRIGGER_STEPS;
    let mut fresh = TriggerAdmission::default();
    for (id, def, def_event) in definitions_in(store, community, channel).await? {
        if !def.is_active() {
            continue;
        }
        let Some(trigger) = &def.on else { continue };
        if !trigger.matches(&event.content) {
            continue;
        }
        let cost = def.steps.len().min(MAX_RUN_STEPS);
        if cost > budget {
            ferrite_app::tracing::warn!(
                "workflow {id} not started: this message already started {} of its {MAX_TRIGGER_STEPS} steps",
                MAX_TRIGGER_STEPS - budget
            );
            continue;
        }
        budget -= cost;
        fresh.admitted.push(AdmittedWorkflow {
            workflow_id: id,
            definition_event: def_event,
            cost,
        });
    }
    if fresh.admitted.is_empty() {
        return Ok(fresh);
    }
    for _ in 0..8 {
        match store::put_doc_if_version(store, &key, &fresh, None).await? {
            Ok(_) => return Ok(fresh),
            // Another pass decided first (or a lost reply to our own write):
            // read back the committed decision; if the row is absent, retry
            // rather than running an uncommitted admission.
            Err(_) => {
                if let Some(decided) = store::get_doc::<TriggerAdmission>(store, &key).await? {
                    return Ok(decided);
                }
            }
        }
    }
    Err(BuzzError::Conflict(
        "workflow trigger admission contended".into(),
    ))
}

/// One execution of a workflow.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Run {
    pub id: String,
    pub workflow_id: String,
    pub channel: String,
    /// Hex id of the event that triggered this run.
    pub trigger: String,
    /// Hex id of the 30620 this run executes.
    ///
    /// PINNED at start rather than resolved per step: the approver spec IS the
    /// authorization for a gate, so reloading the current definition when a
    /// decision arrives would let an edit between the request and the decision
    /// change who may approve — a privilege escalation performed by editing.
    #[serde(default)]
    pub definition_event: String,
    pub status: RunStatus,
    /// Index of the step to execute next.
    pub next_step: usize,
    pub started_at: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Per-step outcome, in execution order.
    #[serde(default)]
    pub trace: Vec<TraceEntry>,
    /// `created_at` (unix seconds) of the decision that last resumed this run
    /// past an approval gate, written in the same CAS that flips it to Running.
    ///
    /// A crash-recovery re-drive stamps its emissions with THIS rather than with
    /// whatever decision event re-entered: the emitted ids are derived from the
    /// stamp, so reusing the original one makes a re-drive mint ids that are
    /// already admitted and absorbed, and a caller cannot pick the timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub resumed_at: Option<u64>,
}

/// An open approval gate, as stored under its token.
///
/// Carries the deadline because the expiry has to be ENFORCEABLE: the emitted
/// request advertises seven days, and a row that merely mentions it in a tag
/// leaves the gate open forever.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct OpenGate {
    pub run: String,
    /// Epoch SECONDS the gate closes at.
    pub expires_at: u64,
}

/// One step's outcome, shaped as the client's `RawTraceEntry`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct TraceEntry {
    pub step_id: String,
    pub status: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// The id of the run a `(workflow, trigger)` pair produces.
///
/// DERIVED, not minted: the same trigger re-driven by a redelivered saga
/// produces the same run id, so the run document's create-if-absent write is the
/// dedup and a workflow fires exactly once per triggering event.
#[must_use]
pub fn run_id(workflow_id: &str, trigger_hex: &str) -> String {
    let digest = sha256_concat(&[workflow_id.as_bytes(), b"\x00", trigger_hex.as_bytes()]);
    to_hex(&digest)[..32].to_string()
}

/// The approval token for one `(run, step)` gate.
///
/// HMAC'd under the relay identity rather than derived in the clear: the token is
/// what a 46011 presents to resume a run, so a value a client could COMPUTE would
/// let anyone approve a gate they were never offered. This app has no entropy
/// source, so a keyed derivation is the available way to make it unguessable.
#[must_use]
pub fn approval_token(seckey: &[u8; 32], run: &str, step_id: &str) -> String {
    let mac = hmac_sha256(
        seckey,
        &[run.as_bytes(), b"\x00", step_id.as_bytes()].concat(),
    );
    to_hex(&mac)[..32].to_string()
}

/// Whether an approver spec admits this pubkey.
#[must_use]
pub fn approver_admits(spec: &str, who: &PubKey, is_moderator: bool) -> bool {
    if spec == "moderator" {
        return is_moderator;
    }
    crate::relay::crypto::from_hex32(spec).is_some_and(|pk| pk == *who)
}

/// Read one run.
pub async fn read_run(
    store: &mut StoreClusterClient,
    community: &str,
    run: &str,
) -> Result<Option<Run>> {
    store::get_doc::<Run>(store, &keys::workflow_run(community, run)).await
}

/// Every run of one workflow, newest first, bounded.
pub async fn runs_of(
    store: &mut StoreClusterClient,
    community: &str,
    workflow_id: &str,
    limit: usize,
) -> Result<Vec<Run>> {
    // Asked for nothing, so nothing is read. Tested BEFORE the walk: a check
    // after a push would make `limit: 0` return one run.
    if limit == 0 {
        return Ok(Vec::new());
    }
    let prefix = keys::workflow_run_index_prefix(community, workflow_id);
    let (rows, _more) = store::scan(store, &prefix, None, store::MAX_SCAN_PAGE).await?;
    let mut out = Vec::new();
    for (key, _value, _version) in rows {
        // The index key's tail names the run; the document lives under its own key.
        let Some(run) = key.rsplit(|b| *b == b'/').next() else {
            continue;
        };
        let Ok(run) = core::str::from_utf8(run) else {
            continue;
        };
        if let Some(doc) = read_run(store, community, run).await? {
            out.push(doc);
        }
        if out.len() >= limit {
            break;
        }
    }
    Ok(out)
}

/// One page of a workflow's runs, newest first, from the community-wide run index, with the
/// index key to continue after (`None` when the index is exhausted).
///
/// The index is keyed by community and workflow id, not channel, and two channels may define a
/// workflow of the same id. A caller that filters runs (by channel, or by what the reader may
/// see) pages with this until it has enough, instead of cutting the first page at its limit
/// and letting runs it will discard shadow the ones it would keep.
pub async fn runs_page(
    store: &mut StoreClusterClient,
    community: &str,
    workflow_id: &str,
    start_after: Option<&[u8]>,
) -> Result<(Vec<Run>, Option<Vec<u8>>)> {
    let prefix = keys::workflow_run_index_prefix(community, workflow_id);
    let (rows, more) = store::scan(store, &prefix, start_after, store::MAX_SCAN_PAGE).await?;
    let last = rows.last().map(|(key, _, _)| key.clone());
    let mut out = Vec::new();
    for (key, _value, _version) in rows {
        let Some(run) = key.rsplit(|b| *b == b'/').next() else {
            continue;
        };
        let Ok(run) = core::str::from_utf8(run) else {
            continue;
        };
        if let Some(doc) = read_run(store, community, run).await? {
            out.push(doc);
        }
    }
    Ok((out, if more { last } else { None }))
}

/// Whether this channel has any workflow at all.
///
/// The whole point of the marker: a message in a channel with no workflows costs
/// ONE point read rather than a scan, which is what makes running this on the
/// admit path affordable.
pub async fn channel_has_workflows(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
) -> Result<bool> {
    Ok(
        store::get_raw(store, &keys::workflow_channel_marker(community, channel))
            .await?
            .is_some(),
    )
}

/// Parse a workflow definition index value (`"{event_hex}:{author_hex}:{created_at}"`,
/// or, on older rows, a bare 64-char `event_hex`).
pub(crate) fn parse_workflow_index(raw: &[u8]) -> Option<(&str, Option<PubKey>, Option<u64>)> {
    let s = core::str::from_utf8(raw).ok()?;
    let mut parts = s.split(':');
    let event_hex = parts.next()?;
    crate::relay::crypto::from_hex32(event_hex)?;
    match (parts.next(), parts.next(), parts.next()) {
        (None, None, None) => Some((event_hex, None, None)),
        (Some(author_hex), Some(ts_str), None) => {
            let author = crate::relay::crypto::from_hex32(author_hex)?;
            let created_at = ts_str.parse::<u64>().ok()?;
            Some((event_hex, Some(author), Some(created_at)))
        }
        _ => None,
    }
}

/// The author an index row names: inline when the row carries one, else read from
/// the indexed event.
pub(crate) async fn indexed_workflow_author(
    store: &mut StoreClusterClient,
    community: &str,
    raw: &[u8],
) -> Result<Option<PubKey>> {
    let Some((event_hex, inline_author, created_at)) = parse_workflow_index(raw) else {
        return Ok(None);
    };
    if let (Some(author), Some(_)) = (inline_author, created_at) {
        return Ok(Some(author));
    }
    let Some(event_id) = crate::relay::crypto::from_hex32(event_hex) else {
        return Ok(None);
    };
    Ok(store::get_doc::<Event>(store, &keys::evt(community, &event_id))
        .await?
        .map(|e| e.pubkey))
}

/// The channel and workflow id a definition event names, each a safe key component.
pub(crate) fn definition_coordinates<'a>(
    channel: Option<&'a str>,
    event: &'a Event,
) -> Result<(&'a str, &'a str)> {
    let ch = channel
        .or_else(|| event.channel_id())
        .filter(|c| keys::is_safe_component(c))
        .ok_or_else(|| {
            BuzzError::InvalidEvent(
                "workflow: definition must carry a safe h channel tag".into(),
            )
        })?;
    let wf_id = workflow_id(event)
        .filter(|id| keys::is_safe_component(id))
        .ok_or_else(|| {
            BuzzError::InvalidEvent("workflow: definition must carry a safe d tag".into())
        })?;
    Ok((ch, wf_id))
}

/// Verify that any pre-existing `wfdef/{c}/{ch}/{wf}` index row belongs to
/// `caller_pubkey`. When the index row names another verified owner and the
/// `wfown` APE guard is still absent, best-effort backfill `wfown` to that
/// owner before refusing with `Unauthorized`.
pub async fn verify_existing_index_owner(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    ch: &str,
    wf_id: &str,
    caller_pubkey: &PubKey,
) -> Result<()> {
    let owner_guard = keys::workflow_owner(community, ch, wf_id);
    let wf_key = keys::workflow_definition_index(community, ch, wf_id);
    if let Some(raw) = store::get_raw(store, &wf_key).await? {
        let existing_author = indexed_workflow_author(store, community, &raw).await?;
        match existing_author {
            Some(author) if &author == caller_pubkey => {}
            Some(owner) => {
                let owner_hex = to_hex(&owner);
                let _ = ape
                    .commit_n_if(
                        &owner_guard,
                        None,
                        &[(owner_guard.as_slice(), owner_hex.as_bytes())],
                    )
                    .await;
                return Err(BuzzError::Unauthorized(
                    "workflow id in this channel is already owned by another author".into(),
                ));
            }
            None => {
                return Err(BuzzError::Unauthorized(
                    "workflow definition owner cannot be verified for replacement".into(),
                ));
            }
        }
    }
    Ok(())
}

/// Claim or verify exclusive ownership of `(community, channel, workflow_id)` on
/// the `guards` APE plane and against any pre-existing `wfdef/` index row.
///
/// Every admission, resumed pre-upgrade saga, and workflow index path calls
/// this helper so a workflow definition never proceeds unowned.
pub async fn claim_or_verify_owner(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    channel: Option<&str>,
    event: &Event,
) -> Result<(String, String)> {
    let (ch, wf_id) = definition_coordinates(channel, event)?;
    let owner_guard = keys::workflow_owner(community, ch, wf_id);
    let author_hex = to_hex(&event.pubkey);
    for _ in 0..crate::relay::ingest::GUARD_RETRIES {
        verify_existing_index_owner(store, ape, community, ch, wf_id, &event.pubkey).await?;
        let current_owner = ape
            .get(&owner_guard)
            .await
            .map_err(|e| BuzzError::Storage(format!("workflow owner read: {e}")))?;
        if let Some(owner_bytes) = current_owner {
            if owner_bytes.as_slice() != author_hex.as_bytes() {
                return Err(BuzzError::Unauthorized(
                    "workflow id in this channel is already owned by another author".into(),
                ));
            }
            return Ok((ch.to_string(), wf_id.to_string()));
        }
        match ape
            .commit_n_if(
                &owner_guard,
                None,
                &[(owner_guard.as_slice(), author_hex.as_bytes())],
            )
            .await
            .map_err(|e| BuzzError::Storage(format!("workflow owner claim: {e}")))?
        {
            Ok(()) => return Ok((ch.to_string(), wf_id.to_string())),
            Err(_) => continue,
        }
    }
    Err(BuzzError::Conflict(
        "workflow ownership claim contended beyond the retry budget".into(),
    ))
}

/// Index an admitted definition (30620) and mark its channel.
///
/// Called from the ingest saga. Claims or verifies exclusive workflow ownership
/// on `ape` and against any existing `wfdef` index row before writing.
pub async fn on_definition(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    event: &Event,
    channel: Option<&str>,
) -> Result<()> {
    if event.kind != kind::KIND_WORKFLOW_DEF {
        return Ok(());
    }
    let (Some(channel), Some(id)) = (channel.or_else(|| event.channel_id()), workflow_id(event))
    else {
        return Ok(());
    };
    if !keys::is_safe_component(channel) || !keys::is_safe_component(id) {
        return Ok(());
    }
    // A malformed definition is stored (the relay accepted the event) but must
    // not claim ownership or be indexed as runnable — the trigger path would
    // then read a body it cannot parse on every message.
    if definition_of(event).is_err() {
        return Ok(());
    }
    claim_or_verify_owner(store, ape, community, Some(channel), event).await?;
    const WF_CAS_RETRIES: usize = 8;
    let idx_key = keys::workflow_definition_index(community, channel, id);
    let event_hex = to_hex(&event.id);
    let next_val = format!("{event_hex}:{}:{}", to_hex(&event.pubkey), event.created_at);
    let mut indexed = false;
    for _ in 0..WF_CAS_RETRIES {
        let current = store::get_raw_versioned(store, &idx_key).await?;
        if let Some((ref raw, _)) = current {
            let Some((existing_hex, inline_author, inline_created_at)) =
                parse_workflow_index(raw)
            else {
                // Corrupt or unrecognized index entry: refuse replacement unless
                // repaired/migrated explicitly.
                return Err(BuzzError::Unauthorized(
                    "workflow definition owner cannot be verified for replacement".into(),
                ));
            };
            let resolved = match (inline_author, inline_created_at) {
                (Some(author), Some(created_at)) => Some((author, created_at)),
                _ => {
                    if let Some(existing_id) = crate::relay::crypto::from_hex32(existing_hex) {
                        store::get_doc::<Event>(store, &keys::evt(community, &existing_id))
                            .await?
                            .map(|e| (e.pubkey, e.created_at))
                    } else {
                        None
                    }
                }
            };
            // When an index entry exists, its owner and version MUST be
            // determinable before replacement is permitted; an older entry
            // whose referenced event is missing cannot be hijacked by an
            // unverified replacement.
            let Some((existing_author, existing_created_at)) = resolved else {
                return Err(BuzzError::Unauthorized(
                    "workflow definition owner cannot be verified for replacement".into(),
                ));
            };
            // A workflow id in a channel belongs to its creator; another
            // author's event with the same `d` tag must never overwrite it,
            // and an older version from the same author must not regress it.
            if existing_author != event.pubkey {
                return Err(BuzzError::Unauthorized(
                    "workflow id in this channel is already owned by another author".into(),
                ));
            }
            if existing_hex == event_hex {
                indexed = true;
                break;
            }
            if !crate::relay::effects::version_beats(
                event.created_at,
                &event_hex,
                existing_created_at,
                existing_hex,
            ) {
                return Ok(());
            }
        }
        let expected_version = current.as_ref().map(|(_, v)| *v);
        match store
            .put_if_version(&idx_key, next_val.as_bytes(), expected_version)
            .await
            .map_err(|e| BuzzError::Storage(format!("workflow index cas: {e}")))?
        {
            Ok(_) => {
                indexed = true;
                break;
            }
            Err(_) => continue,
        }
    }
    if !indexed {
        return Err(BuzzError::Storage(
            "workflow definition index CAS contention".into(),
        ));
    }
    store::put_raw(
        store,
        &keys::workflow_channel_marker(community, channel),
        b"1",
    )
    .await?;
    Ok(())
}

/// The active definitions of one channel, bounded by [`MAX_CHANNEL_WORKFLOWS`].
///
/// Each entry carries the hex id of the event that defines it, so a run can PIN
/// the version it started under.
async fn definitions_in(
    store: &mut StoreClusterClient,
    community: &str,
    channel: &str,
) -> Result<Vec<(String, Definition, String)>> {
    let prefix = keys::workflow_definition_index_prefix(community, channel);
    let (rows, _more) = store::scan(store, &prefix, None, store::MAX_SCAN_PAGE).await?;
    let mut out = Vec::new();
    for (key, value, _version) in rows {
        if out.len() >= MAX_CHANNEL_WORKFLOWS {
            break;
        }
        let Some(id) = key.rsplit(|b| *b == b'/').next() else {
            continue;
        };
        let Ok(id) = core::str::from_utf8(id) else {
            continue;
        };
        let Some((hex, inline_author, _)) = parse_workflow_index(&value) else {
            continue;
        };
        let Some(event_id) = crate::relay::crypto::from_hex32(hex) else {
            continue;
        };
        // The definition may have been replaced or deleted since it was indexed;
        // an absent body simply means there is nothing to run.
        let Some(body) = store::get_doc::<Event>(store, &keys::evt(community, &event_id)).await?
        else {
            continue;
        };
        if let Some(expected_author) = inline_author {
            if body.pubkey != expected_author {
                continue;
            }
        }
        if let Ok(def) = definition_of(&body) {
            let canonical_id = workflow_id(&body).unwrap_or(id);
            if canonical_id != id {
                continue;
            }
            out.push((canonical_id.to_string(), def, hex.to_string()));
        }
    }
    Ok(out)
}

/// The definition a specific event carries — the PINNED read an approval uses.
async fn definition_by_event(
    store: &mut StoreClusterClient,
    community: &str,
    hex: &str,
) -> Result<Option<Definition>> {
    let Some(id) = crate::relay::crypto::from_hex32(hex) else {
        return Ok(None);
    };
    match store::get_doc::<Event>(store, &keys::evt(community, &id)).await? {
        Some(body) => Ok(definition_of(&body).ok()),
        // A newer version of the workflow replaced (and deleted) the event a
        // parked run pinned: the copy taken when it parked still answers, so an
        // edit or pause never strands an awaiting run (finding_6514a102b73fdda9).
        None => store::get_doc::<Definition>(store, &keys::workflow_pinned_definition(community, hex)).await,
    }
}

/// Start a run for every workflow in this channel whose trigger the message
/// fires — the entry point from the ingest saga.
///
/// A channel with no workflows costs one point read. This is the reason the
/// marker exists.
#[allow(clippy::too_many_arguments)]
pub async fn maybe_trigger(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    config: &mut StoreClusterClient,
    community: &str,
    generation: u32,
    event: &Event,
    channel: Option<&str>,
    seckey: Option<&[u8; 32]>,
    now_ms: u64,
) -> Result<()> {
    let mut mentions = Vec::new();
    maybe_trigger_collecting(
        store, ape, topics, config, community, generation, event, channel, seckey, now_ms,
        &mut mentions,
    )
    .await
}

/// [`maybe_trigger`], collecting the persona mentions its agent steps posted, which
/// the caller must dispatch: they were admitted through `ingest::drive`, which
/// starts no turns.
#[allow(clippy::too_many_arguments)]
pub async fn maybe_trigger_collecting(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    config: &mut StoreClusterClient,
    community: &str,
    generation: u32,
    event: &Event,
    channel: Option<&str>,
    seckey: Option<&[u8; 32]>,
    now_ms: u64,
    mentions: &mut Vec<Event>,
) -> Result<()> {
    // Only ordinary channel messages trigger, and only when the relay can sign.
    // A relay-minted event never triggers: that is what stops a workflow's own
    // post from starting another run, on top of the validation-time refusal.
    if !matches!(
        event.kind,
        kind::KIND_STREAM_MESSAGE | kind::KIND_STREAM_MESSAGE_V2
    ) {
        return Ok(());
    }
    let (Some(channel), Some(seckey)) = (channel, seckey) else {
        return Ok(());
    };
    let relay_pk = crate::relay::crypto::schnorr::xonly_pubkey(seckey)
        .ok_or_else(|| BuzzError::Internal("relay identity is not a valid secret key".into()))?;
    if event.pubkey == relay_pk {
        return Ok(());
    }
    if !channel_has_workflows(store, community, channel).await? {
        return Ok(());
    }

    let trigger_hex = to_hex(&event.id);
    let admission = trigger_admission(store, community, channel, event, &trigger_hex).await?;
    for AdmittedWorkflow {
        workflow_id: id,
        definition_event: def_event,
        ..
    } in admission.admitted
    {
        // The definition the admission was decided against. Gone (deleted or
        // replaced and collected) means there is nothing left to run.
        let Some(def) = definition_by_event(store, community, &def_event).await? else {
            continue;
        };
        let run = Run {
            id: run_id(&id, &trigger_hex),
            workflow_id: id.clone(),
            channel: channel.to_string(),
            trigger: trigger_hex.clone(),
            definition_event: def_event,
            status: RunStatus::Running,
            next_step: 0,
            started_at: now_ms,
            completed_at: None,
            error: None,
            trace: Vec::new(),
            resumed_at: None,
        };
        // Create-if-absent starts the run exactly once. But "the document
        // exists" is NOT "the trigger finished": a driver that died after this
        // write and before the last step would strand a Running run that every
        // redelivery then skipped. So an existing NON-TERMINAL run is RESUMED.
        //
        // Safe because of the property this executor is built on: every emitted
        // event is deterministic, so re-executing a step mints an id that is
        // already admitted and the duplicate is absorbed rather than doubled.
        let key = keys::workflow_run(community, &run.id);
        let (run, effective) = match store::put_doc_if_version(store, &key, &run, None).await? {
            Ok(_) => {
                store::put_raw(
                    store,
                    &keys::workflow_run_index(community, &id, now_ms, &run.id),
                    b"1",
                )
                .await?;
                emit_lifecycle(
                    store,
                    ape,
                    topics,
                    community,
                    generation,
                    channel,
                    kind::KIND_WORKFLOW_TRIGGERED,
                    &run,
                    None,
                    seckey,
                    event.created_at,
                )
                .await?;
                (run, def)
            }
            Err(_) => match read_run(store, community, &run.id).await? {
                // Terminal, or parked at a gate waiting for a decision that is
                // not ours to make: nothing to do.
                Some(existing)
                    if matches!(
                        existing.status,
                        RunStatus::Completed
                            | RunStatus::Failed
                            | RunStatus::Cancelled
                            | RunStatus::AwaitingApproval
                    ) =>
                {
                    continue
                }
                Some(existing) => {
                    // Resume with the definition the run PINNED, not the one
                    // this iteration happens to hold — otherwise an edit after a
                    // crash silently changes the remaining steps and the
                    // approver, which is the drift the pin exists to prevent.
                    let pinned = if existing.definition_event.is_empty() {
                        // A run recorded before the field existed has nothing to
                        // pin to; the current definition is the only one there
                        // is, and saying so beats failing to resume.
                        def
                    } else {
                        match definition_by_event(store, community, &existing.definition_event)
                            .await?
                        {
                            Some(pinned) => pinned,
                            None => continue,
                        }
                    };
                    (existing, pinned)
                }
                None => continue,
            },
        };

        // A run resumed past a gate re-mints its later steps under the grant's
        // instant, not the trigger's, or a redelivered trigger would emit fresh
        // ids for steps the grant already ran.
        let stamp = run.resumed_at.unwrap_or(event.created_at);
        execute(
            store,
            ape,
            topics,
            config,
            community,
            generation,
            run,
            &effective,
            seckey,
            stamp,
            now_ms,
            mentions,
        )
        .await?;
    }
    Ok(())
}

/// Start the workflow a MANUAL trigger (46020) names, whatever its trigger
/// condition says.
///
/// A separate entry point rather than a synthetic message, for two reasons: "run
/// it now" means ignoring the condition, and manufacturing a message that happens
/// to match one would post text nobody wrote into the room.
///
/// Everything after the start is the ordinary executor — same deterministic run
/// id, same lifecycle events, same gates — so a manually started run is
/// indistinguishable from one a message started, which is what makes the run
/// history uniform.
#[allow(clippy::too_many_arguments)]
pub async fn trigger_now(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    config: &mut StoreClusterClient,
    community: &str,
    generation: u32,
    event: &Event,
    channel: Option<&str>,
    seckey: Option<&[u8; 32]>,
    now_ms: u64,
) -> Result<()> {
    let mut mentions = Vec::new();
    trigger_now_collecting(
        store, ape, topics, config, community, generation, event, channel, seckey, now_ms,
        &mut mentions,
    )
    .await
}

/// [`trigger_now`], collecting the persona mentions its agent steps posted, which
/// the caller must dispatch: they were admitted through `ingest::drive`, which
/// starts no turns.
#[allow(clippy::too_many_arguments)]
pub async fn trigger_now_collecting(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    config: &mut StoreClusterClient,
    community: &str,
    generation: u32,
    event: &Event,
    channel: Option<&str>,
    seckey: Option<&[u8; 32]>,
    now_ms: u64,
    mentions: &mut Vec<Event>,
) -> Result<()> {
    if event.kind != kind::KIND_WORKFLOW_TRIGGER {
        return Ok(());
    }
    let (Some(channel), Some(seckey), Some(wanted)) =
        (channel, seckey, event.first_tag("workflow"))
    else {
        return Ok(());
    };
    let Some((id, def, def_event)) = definitions_in(store, community, channel)
        .await?
        .into_iter()
        .find(|(id, _, _)| id == wanted)
    else {
        return Err(BuzzError::NotFound(format!(
            "no workflow {wanted} in this channel"
        )));
    };
    if def.status != "active" {
        return Err(BuzzError::InvalidEvent(
            "that workflow is paused; resume it before running it".into(),
        ));
    }

    let trigger_hex = to_hex(&event.id);
    let run = Run {
        id: run_id(&id, &trigger_hex),
        workflow_id: id.clone(),
        channel: channel.to_string(),
        trigger: trigger_hex,
        definition_event: def_event,
        status: RunStatus::Running,
        next_step: 0,
        started_at: now_ms,
        completed_at: None,
        error: None,
        trace: Vec::new(),
        resumed_at: None,
    };
    let key = keys::workflow_run(community, &run.id);
    // The same create-if-absent start and non-terminal resume the message path
    // uses — a redelivered trigger must not run the workflow twice, and a
    // half-finished run must not be stranded.
    let (run, effective) = match store::put_doc_if_version(store, &key, &run, None).await? {
        Ok(_) => {
            store::put_raw(
                store,
                &keys::workflow_run_index(community, &id, now_ms, &run.id),
                b"1",
            )
            .await?;
            emit_lifecycle(
                store,
                ape,
                topics,
                community,
                generation,
                channel,
                kind::KIND_WORKFLOW_TRIGGERED,
                &run,
                None,
                seckey,
                event.created_at,
            )
            .await?;
            (run, def)
        }
        Err(_) => match read_run(store, community, &run.id).await? {
            Some(existing)
                if matches!(
                    existing.status,
                    RunStatus::Completed
                        | RunStatus::Failed
                        | RunStatus::Cancelled
                        | RunStatus::AwaitingApproval
                ) =>
            {
                return Ok(())
            }
            Some(existing) => {
                // The definition the RUN pinned, never the one indexed now —
                // otherwise editing a workflow while a manual run is half-finished
                // silently changes its remaining steps and its approver, which is
                // the drift the pin exists to prevent.
                let pinned = if existing.definition_event.is_empty() {
                    def
                } else {
                    match definition_by_event(store, community, &existing.definition_event).await? {
                        Some(pinned) => pinned,
                        None => return Ok(()),
                    }
                };
                (existing, pinned)
            }
            None => return Ok(()),
        },
    };
    let stamp = run.resumed_at.unwrap_or(event.created_at);
    execute(
        store,
        ape,
        topics,
        config,
        community,
        generation,
        run,
        &effective,
        seckey,
        stamp,
        now_ms,
        mentions,
    )
    .await
}

/// Walk a run's remaining steps until it completes, fails, or halts at a gate.
#[allow(clippy::too_many_arguments)]
async fn execute(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    config: &mut StoreClusterClient,
    community: &str,
    generation: u32,
    mut run: Run,
    def: &Definition,
    seckey: &[u8; 32],
    stamp_secs: u64,
    now_ms: u64,
    mentions: &mut Vec<Event>,
) -> Result<()> {
    let channel = run.channel.clone();
    // The step ceiling, enforced on the WALK as well as at validation: a
    // definition that slipped past the self-trigger refusal still cannot run
    // unbounded.
    let mut executed = 0usize;
    while run.next_step < def.steps.len() && executed < MAX_RUN_STEPS {
        executed += 1;
        let Some(step) = def.steps.get(run.next_step) else {
            break;
        };
        let step_id = step.id().to_string();
        emit_lifecycle(
            store,
            ape,
            topics,
            community,
            generation,
            &channel,
            kind::KIND_WORKFLOW_STEP_STARTED,
            &run,
            Some(&step_id),
            seckey,
            stamp_secs,
        )
        .await?;

        let outcome = match step {
            Step::Post { text, .. } => {
                // The run and step tags make each (run, step) post a distinct
                // event id, while a redelivery of the same step stays one.
                post_message(
                    store,
                    ape,
                    topics,
                    community,
                    generation,
                    &channel,
                    text,
                    &[
                        vec!["run".to_string(), run.id.clone()],
                        vec!["step".to_string(), step_id.clone()],
                    ],
                    seckey,
                    stamp_secs,
                )
                .await
            }
            Step::Agent { agent, prompt, .. } => {
                // A MENTION, not a turn enqueue: the dispatch path owns who may
                // invoke an agent, how often, and what is audited.
                let mention = match agent_pubkey(config, agent).await {
                    Some(pk) => vec![
                        vec!["p".to_string(), to_hex(&pk)],
                        vec!["run".to_string(), run.id.clone()],
                        vec!["step".to_string(), step_id.clone()],
                    ],
                    // An agent that is not registered cannot be mentioned into
                    // existence; the step fails loudly rather than posting text
                    // that will never be answered.
                    None => {
                        run.status = RunStatus::Failed;
                        run.error = Some(format!("step {step_id}: no agent named {agent}"));
                        break;
                    }
                };
                post_message_event(
                    store,
                    ape,
                    topics,
                    community,
                    generation,
                    &channel,
                    &agent_step_text(agent, prompt),
                    &mention,
                    seckey,
                    stamp_secs,
                )
                .await
                // The mention is admitted through `ingest::drive`, which does not
                // dispatch turns; ingress dispatches the collected mentions.
                .map(|event| mentions.push(event))
            }
            Step::Approval { approver, .. } => {
                // A gate is a PAUSE, not a poll: emit the request, record the
                // token WITH its deadline, and stop. A 46011/46012 carrying that
                // token resumes.
                let token = approval_token(seckey, &run.id, &step_id);
                let expires_at = stamp_secs.saturating_add(APPROVAL_TTL_SECS);
                store::put_doc(
                    store,
                    &keys::workflow_approval(community, &token),
                    &OpenGate {
                        run: run.id.clone(),
                        expires_at,
                    },
                )
                .await?;
                // Keep the pinned definition for as long as this gate can be
                // answered (plus a day), whatever later edits do to its event.
                if !run.definition_event.is_empty() {
                    store::put_expiring_doc(
                        store,
                        &keys::workflow_pinned_definition(community, &run.definition_event),
                        def,
                        expires_at.saturating_add(24 * 60 * 60).saturating_mul(1000),
                    )
                    .await?;
                }
                let mut tags = vec![
                    vec!["token".to_string(), token],
                    vec!["approver".to_string(), approver.clone()],
                    vec!["expiration".to_string(), expires_at.to_string()],
                ];
                if crate::relay::crypto::from_hex32(approver).is_some() {
                    tags.push(vec!["p".to_string(), approver.to_ascii_lowercase()]);
                }
                emit_lifecycle_tagged(
                    store,
                    ape,
                    topics,
                    community,
                    generation,
                    &channel,
                    kind::KIND_WORKFLOW_APPROVAL_REQUESTED,
                    &run,
                    Some(&step_id),
                    tags,
                    seckey,
                    stamp_secs,
                )
                .await?;
                run.status = RunStatus::AwaitingApproval;
                run.trace.push(TraceEntry {
                    step_id: step_id.clone(),
                    status: "awaiting_approval".to_string(),
                    started_at: Some(now_ms),
                    completed_at: None,
                    error: None,
                });
                run.trace.dedup_by(|a, b| a.step_id == b.step_id);
                store::put_doc(store, &keys::workflow_run(community, &run.id), &run).await?;
                return Ok(());
            }
        };

        match outcome {
            Ok(()) => {
                run.trace.push(TraceEntry {
                    step_id: step_id.clone(),
                    status: "completed".to_string(),
                    started_at: Some(now_ms),
                    completed_at: Some(now_ms),
                    error: None,
                });
                run.next_step = run.next_step.saturating_add(1);
                emit_lifecycle(
                    store,
                    ape,
                    topics,
                    community,
                    generation,
                    &channel,
                    kind::KIND_WORKFLOW_STEP_COMPLETED,
                    &run,
                    Some(&step_id),
                    seckey,
                    stamp_secs,
                )
                .await?;
            }
            Err(e) => {
                run.status = RunStatus::Failed;
                run.error = Some(format!("step {step_id}: {e}"));
                run.trace.push(TraceEntry {
                    step_id: step_id.clone(),
                    status: "failed".to_string(),
                    started_at: Some(now_ms),
                    completed_at: Some(now_ms),
                    error: Some(e.to_string()),
                });
                emit_lifecycle(
                    store,
                    ape,
                    topics,
                    community,
                    generation,
                    &channel,
                    kind::KIND_WORKFLOW_STEP_FAILED,
                    &run,
                    Some(&step_id),
                    seckey,
                    stamp_secs,
                )
                .await?;
                break;
            }
        }
    }

    let terminal = if run.status == RunStatus::Failed {
        kind::KIND_WORKFLOW_FAILED
    } else {
        run.status = RunStatus::Completed;
        kind::KIND_WORKFLOW_COMPLETED
    };
    run.completed_at = Some(now_ms);
    store::put_doc(store, &keys::workflow_run(community, &run.id), &run).await?;
    emit_lifecycle(
        store, ape, topics, community, generation, &channel, terminal, &run, None, seckey,
        stamp_secs,
    )
    .await
}

/// Resolve the target run's channel for an approval decision (`46011`/`46012`),
/// if its `token` names a stored gate and run.
pub async fn approval_gate_channel(
    store: &mut StoreClusterClient,
    community: &str,
    event: &Event,
) -> Result<Option<String>> {
    let Some(token) = event.first_tag("token") else {
        return Ok(None);
    };
    let Some(gate) =
        store::get_doc::<OpenGate>(store, &keys::workflow_approval(community, token)).await?
    else {
        return Ok(None);
    };
    let Some(run) = store::get_doc::<Run>(store, &keys::workflow_run(community, &gate.run)).await?
    else {
        return Ok(None);
    };
    Ok(Some(run.channel))
}

async fn authorize_run_channel_actor(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    community: &str,
    channel: &str,
    actor: &PubKey,
) -> Result<()> {
    let guard_key = keys::guard_channel(community, channel)?;
    let Some(bytes) = ape
        .get(&guard_key)
        .await
        .map_err(|e| BuzzError::Storage(format!("channel read: {e}")))?
    else {
        return Err(BuzzError::NotFound("no such channel".into()));
    };
    let doc: crate::relay::channel::Channel = serde_json::from_slice(&bytes)
        .map_err(|e| BuzzError::Internal(format!("channel decode: {e}")))?;
    if !doc.is_writable() {
        return Err(BuzzError::Unauthorized(
            "channel is archived or deleted".into(),
        ));
    }
    if doc.visibility.is_private() {
        let mem_key = keys::mem(community, channel, actor)?;
        match store::get_doc::<crate::relay::channel::Membership>(store, &mem_key).await? {
            Some(m) if m.is_live() => {}
            _ => {
                return Err(BuzzError::Unauthorized(
                    "not a member of this channel".into(),
                ))
            }
        }
    }
    Ok(())
}

/// Resume or cancel a run on an admitted approval decision (46011/46012).
#[allow(clippy::too_many_arguments)]
pub async fn on_approval(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    config: &mut StoreClusterClient,
    community: &str,
    generation: u32,
    event: &Event,
    actor_is_moderator: bool,
    seckey: Option<&[u8; 32]>,
    now_ms: u64,
) -> Result<()> {
    let mut mentions = Vec::new();
    on_approval_collecting(
        store, ape, topics, config, community, generation, event, actor_is_moderator, seckey, now_ms,
        &mut mentions,
    )
    .await
}

/// [`on_approval`], collecting the persona mentions its agent steps posted, which
/// the caller must dispatch: they were admitted through `ingest::drive`, which
/// starts no turns.
#[allow(clippy::too_many_arguments)]
pub async fn on_approval_collecting(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    config: &mut StoreClusterClient,
    community: &str,
    generation: u32,
    event: &Event,
    actor_is_moderator: bool,
    seckey: Option<&[u8; 32]>,
    now_ms: u64,
    mentions: &mut Vec<Event>,
) -> Result<()> {
    let granted = match event.kind {
        kind::KIND_WORKFLOW_APPROVAL_GRANTED => true,
        kind::KIND_WORKFLOW_APPROVAL_DENIED => false,
        _ => return Ok(()),
    };
    let Some(seckey) = seckey else { return Ok(()) };
    let Some(token) = event.first_tag("token") else {
        return Ok(());
    };
    // The token resolves to its run. An unknown token names no gate — which is
    // also what a forged one looks like, since the token is HMAC'd under the
    // relay identity and cannot be computed by a client.
    let Some(gate) =
        store::get_doc::<OpenGate>(store, &keys::workflow_approval(community, token)).await?
    else {
        return Ok(());
    };
    let Some((mut run, version)) =
        store::get_doc_versioned::<Run>(store, &keys::workflow_run(community, &gate.run)).await?
    else {
        return Ok(());
    };
    if let Some(h) = event.channel_id() {
        if h != run.channel {
            return Err(BuzzError::InvalidEvent(
                "workflow approval channel tag does not match run channel".into(),
            ));
        }
    }
    authorize_run_channel_actor(store, ape, community, &run.channel, &event.pubkey).await?;
    // Already RESUMED by a driver that then crashed: re-execute rather than
    // return. That is what makes the window below recoverable instead of merely
    // small — the alternative strands the run with its gate already consumed.
    //
    // Authorized like the first grant: the token is broadcast to the whole room
    // in the approval request, so holding it proves nothing. Only a GRANT from
    // an approver the gate just passed (the step before `next_step`) may drive
    // the remaining steps, and they are stamped with the recorded grant's
    // instant, never this event's caller-chosen `created_at`, so the re-drive
    // re-mints the ids the original drive already admitted.
    if run.status == RunStatus::Running {
        if !granted {
            return Ok(());
        }
        let Some(def) = definition_by_event(store, community, &run.definition_event).await? else {
            return Ok(());
        };
        let passed_gate = run
            .next_step
            .checked_sub(1)
            .and_then(|i| def.steps.get(i));
        let Some(Step::Approval { approver, .. }) = passed_gate else {
            return Ok(());
        };
        if !approver_admits(approver, &event.pubkey, actor_is_moderator) {
            return Err(BuzzError::Unauthorized(
                "you are not the approver this workflow step names".into(),
            ));
        }
        let stamp = run.resumed_at.unwrap_or(event.created_at);
        return execute(
            store,
            ape,
            topics,
            config,
            community,
            generation,
            run,
            &def,
            seckey,
            stamp,
            now_ms,
            mentions,
        )
        .await;
    }
    if run.status != RunStatus::AwaitingApproval {
        // Terminal. A crash between a terminal CAS and its gate deletion leaves
        // the row behind, and a leaked gate is a token that still resolves — so
        // a redelivery that gets here finishes the cleanup.
        store::delete_key(store, &keys::workflow_approval(community, token)).await?;
        return Ok(());
    }

    // An EXPIRED gate is a run that is over. Judged by the relay's OWN admission
    // instant, never by `event.created_at`: the sender chooses that, so a
    // decision arriving a month late could carry a timestamp inside the window
    // and be honoured. (The event timestamp is still what DERIVES the emitted
    // ids — identical on every re-drive, which is exactly what it is for.)
    //
    // Persisted under the same CAS-first ordering as the decision paths below:
    // writing first and deleting second is what keeps a crash recoverable, and
    // guarding on the version is what stops a concurrent expiry handler
    // overwriting a run another driver has just resumed.
    if now_ms / 1000 > gate.expires_at {
        run.status = RunStatus::Cancelled;
        run.completed_at = Some(now_ms);
        run.error = Some("the approval gate expired before a decision arrived".to_string());
        if store::put_doc_if_version(
            store,
            &keys::workflow_run(community, &run.id),
            &run,
            Some(version),
        )
        .await?
        .is_err()
        {
            return Ok(()); // another driver moved this run; do not overwrite it
        }
        store::delete_key(store, &keys::workflow_approval(community, token)).await?;
        let channel = run.channel.clone();
        return emit_lifecycle(
            store,
            ape,
            topics,
            community,
            generation,
            &channel,
            kind::KIND_WORKFLOW_CANCELLED,
            &run,
            None,
            seckey,
            event.created_at,
        )
        .await;
    }

    // The PINNED definition — the version the approver was actually shown. Using
    // the current one would let an edit between the request and the decision
    // change who may approve, which is the authorization itself.
    let Some(def) = definition_by_event(store, community, &run.definition_event).await? else {
        return Ok(());
    };
    let Some(Step::Approval { approver, .. }) = def.steps.get(run.next_step) else {
        return Ok(());
    };
    if !approver_admits(approver, &event.pubkey, actor_is_moderator) {
        return Err(BuzzError::Unauthorized(
            "you are not the approver this workflow step names".into(),
        ));
    }

    // Order is the crash contract. The state change is persisted FIRST, under a
    // CAS on the version that proves nobody else moved this run; only then is
    // the gate consumed. The reverse order — delete, then execute, then persist
    // inside `execute` — has a window where the run is stored AwaitingApproval
    // with no token, so the approval cannot be redelivered and the trigger path
    // skips a parked run: permanently stranded.
    //
    // A crash between the two leaves a Running run and a spent gate, which the
    // Running branch above resumes.
    if !granted {
        run.status = RunStatus::Cancelled;
        run.completed_at = Some(now_ms);
        if store::put_doc_if_version(
            store,
            &keys::workflow_run(community, &run.id),
            &run,
            Some(version),
        )
        .await?
        .is_err()
        {
            return Ok(()); // another driver resolved it
        }
        store::delete_key(store, &keys::workflow_approval(community, token)).await?;
        let channel = run.channel.clone();
        return emit_lifecycle(
            store,
            ape,
            topics,
            community,
            generation,
            &channel,
            kind::KIND_WORKFLOW_CANCELLED,
            &run,
            None,
            seckey,
            event.created_at,
        )
        .await;
    }
    run.status = RunStatus::Running;
    run.next_step = run.next_step.saturating_add(1);
    run.resumed_at = Some(event.created_at);
    if store::put_doc_if_version(
        store,
        &keys::workflow_run(community, &run.id),
        &run,
        Some(version),
    )
    .await?
    .is_err()
    {
        return Ok(()); // another driver already resumed it
    }
    store::delete_key(store, &keys::workflow_approval(community, token)).await?;
    execute(
        store,
        ape,
        topics,
        config,
        community,
        generation,
        run,
        &def,
        seckey,
        event.created_at,
        now_ms,
        mentions,
    )
    .await
}

/// A registered persona's pubkey, read from the operator registry.
async fn agent_pubkey(config: &mut StoreClusterClient, agent: &str) -> Option<PubKey> {
    let bytes = store::get_raw(config, b"buzz-agents")
        .await
        .ok()
        .flatten()?;
    let registry = crate::relay::agents::Registry::parse(&bytes).ok()?;
    registry
        .personas
        .iter()
        .find(|p| p.agent == agent)
        .map(|p| p.pubkey)
}

/// Post one relay-signed message into a channel.
#[allow(clippy::too_many_arguments)]
/// The text a `Step::Agent` posts: a mention of the persona, then the prompt.
fn agent_step_text(agent: &str, prompt: &str) -> String {
    format!("@{agent} {prompt}")
}

#[allow(clippy::too_many_arguments)]
async fn post_message(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    channel: &str,
    text: &str,
    extra_tags: &[Vec<String>],
    seckey: &[u8; 32],
    created_at: u64,
) -> Result<()> {
    post_message_event(
        store, ape, topics, community, generation, channel, text, extra_tags, seckey, created_at,
    )
    .await
    .map(|_| ())
}

/// [`post_message`], returning the event it admitted.
#[allow(clippy::too_many_arguments)]
async fn post_message_event(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    channel: &str,
    text: &str,
    extra_tags: &[Vec<String>],
    seckey: &[u8; 32],
    created_at: u64,
) -> Result<Event> {
    let mut tags = vec![vec!["h".to_string(), channel.to_string()]];
    tags.extend_from_slice(extra_tags);
    publish_event(
        store,
        ape,
        topics,
        community,
        generation,
        channel,
        kind::KIND_STREAM_MESSAGE,
        tags,
        text,
        seckey,
        created_at,
    )
    .await
}

/// Emit one lifecycle event naming the run (and optionally the step).
#[allow(clippy::too_many_arguments)]
async fn emit_lifecycle(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    channel: &str,
    lifecycle: crate::relay::kind::Kind,
    run: &Run,
    step_id: Option<&str>,
    seckey: &[u8; 32],
    created_at: u64,
) -> Result<()> {
    emit_lifecycle_tagged(
        store,
        ape,
        topics,
        community,
        generation,
        channel,
        lifecycle,
        run,
        step_id,
        Vec::new(),
        seckey,
        created_at,
    )
    .await
}

/// [`emit_lifecycle`] with extra tags (the approval request's token).
#[allow(clippy::too_many_arguments)]
async fn emit_lifecycle_tagged(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    channel: &str,
    lifecycle: crate::relay::kind::Kind,
    run: &Run,
    step_id: Option<&str>,
    extra: Vec<Vec<String>>,
    seckey: &[u8; 32],
    created_at: u64,
) -> Result<()> {
    let mut tags = vec![
        vec!["h".to_string(), channel.to_string()],
        vec!["run".to_string(), run.id.clone()],
        vec!["workflow".to_string(), run.workflow_id.clone()],
        vec!["e".to_string(), run.trigger.clone()],
    ];
    if let Some(step_id) = step_id {
        tags.push(vec!["step".to_string(), step_id.to_string()]);
    }
    tags.extend(extra);
    publish(
        store, ape, topics, community, generation, channel, lifecycle, tags, "", seckey, created_at,
    )
    .await
}

/// Sign one relay-authored event and drive it through the ordinary saga.
///
/// Two paths, chosen by the kind, and the distinction is the one
/// `drive_relay_minted`'s own assert enforces:
///
/// * a LIFECYCLE event (46001–46007, 46010) is a relay-only kind, so it takes
///   the relay-minted path — which has no projection, notice or archive step,
///   making "an emission cannot trigger an emission" a property of the call
///   graph rather than of a flag;
/// * a STEP's post is an ordinary kind-9 message that merely happens to be
///   relay-signed, so it takes the ordinary `drive`. It cannot re-trigger a
///   workflow either, for a different reason: the trigger hook lives in
///   `ingress`, not in the saga, so nothing driven from inside this executor
///   ever reaches it.
#[allow(clippy::too_many_arguments)]
#[allow(clippy::too_many_arguments)]
async fn publish(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    channel: &str,
    event_kind: crate::relay::kind::Kind,
    tags: Vec<Vec<String>>,
    content: &str,
    seckey: &[u8; 32],
    created_at: u64,
) -> Result<()> {
    publish_event(
        store, ape, topics, community, generation, channel, event_kind, tags, content, seckey,
        created_at,
    )
    .await
    .map(|_| ())
}

/// [`publish`], returning the event it admitted.
#[allow(clippy::too_many_arguments)]
async fn publish_event(
    store: &mut StoreClusterClient,
    ape: &mut ApeClusterClient,
    topics: &mut TopicLogClient,
    community: &str,
    generation: u32,
    channel: &str,
    event_kind: crate::relay::kind::Kind,
    tags: Vec<Vec<String>>,
    content: &str,
    seckey: &[u8; 32],
    created_at: u64,
) -> Result<Event> {
    let event = Event::sign_new(seckey, event_kind, tags, content, created_at)?;
    let now_ms = created_at.saturating_mul(1000);
    let outcome =
        crate::relay::ingest::admit(ape, community, &event, Some(channel.to_string()), now_ms)
            .await?;
    let driven = if kind::is_relay_only_kind(event_kind) {
        crate::relay::ingest::drive_relay_minted(
            store, ape, topics, community, generation, &event, &outcome, now_ms,
        )
        .await
        .map(|_| ())
    } else {
        crate::relay::ingest::drive(
            store,
            ape,
            topics,
            community,
            generation,
            &event,
            &outcome,
            now_ms,
            Some(seckey),
        )
        .await
        .map(|_| ())
    };
    driven.map(|()| event)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use ferrite_app::serde_json;

    fn def(steps: Value, on: Option<Value>) -> Value {
        let mut v = serde_json::json!({ "name": "deploy", "steps": steps, "status": "active" });
        if let Some(on) = on {
            v["on"] = on;
        }
        v
    }

    #[test]
    fn a_valid_definition_round_trips() {
        let parsed = validate_definition(&def(
            serde_json::json!([{ "type": "post", "id": "say", "text": "starting the deploy" }]),
            Some(serde_json::json!({ "contains": "ship it" })),
        ))
        .unwrap();
        assert_eq!(parsed.name, "deploy");
        assert_eq!(parsed.steps.len(), 1);
        assert!(parsed.is_active());
    }

    #[test]
    fn a_workflow_that_would_trigger_itself_is_refused_at_write_time() {
        // THE loop guard. A run that posts text matching its own trigger starts
        // another run; bounding that at the step cap would still mean it runs,
        // emits, audits and delivers. So it is refused here instead.
        let err = validate_definition(&def(
            serde_json::json!([{ "type": "post", "id": "say", "text": "ship it now" }]),
            Some(serde_json::json!({ "contains": "ship it" })),
        ))
        .unwrap_err();
        assert!(format!("{err}").contains("trigger it again"), "{err}");

        // An agent step's PROMPT is posted too, so it is checked the same way.
        assert!(validate_definition(&def(
            serde_json::json!([{ "type": "agent", "id": "ask", "agent": "revka", "prompt": "ship it please" }]),
            Some(serde_json::json!({ "contains": "ship it" })),
        ))
        .is_err());

        // A workflow with NO trigger cannot self-trigger whatever it posts.
        assert!(validate_definition(&def(
            serde_json::json!([{ "type": "post", "id": "say", "text": "ship it now" }]),
            None,
        ))
        .is_ok());
    }

    #[test]
    fn malformed_definitions_are_refused() {
        // No steps.
        assert!(validate_definition(&def(serde_json::json!([]), None)).is_err());
        // Duplicate step ids — a trace keyed by step id could not tell them apart.
        assert!(validate_definition(&def(
            serde_json::json!([
                { "type": "post", "id": "a", "text": "x" },
                { "type": "post", "id": "a", "text": "y" },
            ]),
            None
        ))
        .is_err());
        // An empty name.
        let mut blank = def(serde_json::json!([{ "type": "post", "id": "a", "text": "x" }]), None);
        blank["name"] = serde_json::json!("   ");
        assert!(validate_definition(&blank).is_err());
        // An unknown status.
        let mut bad_status = def(serde_json::json!([{ "type": "post", "id": "a", "text": "x" }]), None);
        bad_status["status"] = serde_json::json!("running");
        assert!(validate_definition(&bad_status).is_err());
        // An approver that is neither a pubkey nor the moderator role.
        assert!(validate_definition(&def(
            serde_json::json!([{ "type": "approval", "id": "gate", "approver": "whoever" }]),
            None
        ))
        .is_err());
        // Over the step cap.
        let many: Vec<Value> = (0..=MAX_RUN_STEPS)
            .map(|i| serde_json::json!({ "type": "post", "id": format!("s{i}"), "text": "x" }))
            .collect();
        assert!(validate_definition(&def(serde_json::json!(many), None)).is_err());
    }

    #[test]
    fn a_trigger_matches_case_insensitively_and_an_empty_one_matches_everything() {
        let t = Trigger {
            contains: "Ship It".into(),
        };
        assert!(t.matches("please ship it now"));
        assert!(t.matches("SHIP IT"));
        assert!(!t.matches("nothing to do"));
        let all = Trigger {
            contains: String::new(),
        };
        assert!(all.matches("anything at all"));
    }

    #[test]
    fn a_run_id_is_derived_so_a_redrive_reuses_it() {
        // Determinism IS the dedup: the run document is written create-if-absent,
        // so a redelivered saga that recomputes the same id fires once.
        let a = run_id("wf-1", "ab".repeat(32).as_str());
        assert_eq!(a, run_id("wf-1", "ab".repeat(32).as_str()));
        assert_ne!(a, run_id("wf-2", "ab".repeat(32).as_str()));
        assert_ne!(a, run_id("wf-1", "cd".repeat(32).as_str()));
        assert_eq!(a.len(), 32);
    }

    #[test]
    fn an_approval_token_is_keyed_so_a_client_cannot_compute_it() {
        // A token a client could derive would let anyone resume a gate they were
        // never offered, so it is HMAC'd under the relay identity rather than
        // hashed in the clear.
        let key = [7u8; 32];
        let token = approval_token(&key, "run-1", "gate");
        assert_eq!(
            token,
            approval_token(&key, "run-1", "gate"),
            "deterministic"
        );
        assert_ne!(token, approval_token(&key, "run-2", "gate"));
        assert_ne!(token, approval_token(&key, "run-1", "other"));
        assert_ne!(
            token,
            approval_token(&[8u8; 32], "run-1", "gate"),
            "a different relay identity yields a different token"
        );
    }

    #[test]
    fn an_approver_spec_admits_exactly_who_it_names() {
        let alice = [1u8; 32];
        let bob = [2u8; 32];
        assert!(approver_admits(&to_hex(&alice), &alice, false));
        assert!(!approver_admits(&to_hex(&alice), &bob, false));
        // The role form admits any moderator and nobody else.
        assert!(approver_admits("moderator", &bob, true));
        assert!(!approver_admits("moderator", &bob, false));
        // Junk admits nobody rather than everybody.
        assert!(!approver_admits("anyone", &alice, true));
    }
}
