//! The agent runtime: everything past `.run()`.
//!
//! ```no_run
//! # use buzz_sdk::{Agent, Identity, Result, Workspace};
//! # async fn demo() -> Result<()> {
//! let workspace = Workspace::builder(std::env::var("BUZZ_URL")?)
//!     .identity(Identity::from_env("BUZZ_AGENT_KEY")?)
//!     .connect()
//!     .await?;
//!
//! Agent::builder(workspace)
//!     .name("hello-bot")
//!     .instructions("You are a friendly teammate.")
//!     .model("claude-sonnet-5")
//!     .run()
//!     .await
//! # }
//! ```
//!
//! ## What `.run()` guarantees
//!
//! * **It answers mentions of its own key.** One subscription, filtered to `#p`
//!   = the agent's pubkey and to conversation kinds, so a reaction or a
//!   membership change does not wake a turn.
//! * **It never answers itself.** Its own events are dropped before anything
//!   else looks at them, which is what stops two agents in one channel from
//!   talking until the budget runs out.
//! * **One turn per channel at a time.** [`Dispatcher`] is a single-flight gate:
//!   mentions that arrive while a channel's turn is running are queued, and when
//!   that turn finishes they are answered together as one batch. Different
//!   channels run concurrently.
//! * **Nothing is answered twice.** Ids are deduplicated through the relay's own
//!   bounded [`buzz::relay::delivery::SeenIds`] ring, so an at-least-once
//!   redelivery is absorbed.
//! * **A restart resumes.** The stream cursor is persisted through a
//!   [`CursorStore`]; on a fresh start with no stored cursor the agent skips
//!   mentions older than [`AgentBuilder::max_backfill`] rather than replaying a
//!   community's whole history into the model.
//! * **It survives the stream dying.** End-of-stream or a transport error
//!   re-subscribes with exponential backoff; a `reset` frame re-reads recent
//!   mentions through the query path before re-subscribing, because the relay has
//!   just said the cursor cannot be honored.
//!
//! ## What it does not do
//!
//! It does not span processes: two copies of one agent identity will both answer
//! the same mention. The relay's admission marker makes the *events* idempotent,
//! not the model calls. Coordinating replicas needs a claim in shared state, which
//! belongs to the relay's agent dispatcher rather than to a client library.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use buzz::relay::crypto::to_hex;
use buzz::relay::delivery::SeenIds;
use buzz::relay::kind::{Kind, KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2};
use buzz::relay::{Event, PubKey};

use crate::backend::{Backend, DEFAULT_MAX_TOKENS, DEFAULT_MODEL};
use crate::cron::Cron;
use crate::error::{Result, SdkError};
use crate::memory::Memory;
use crate::tool::{Tool, ToolRegistry};
use crate::transport::{mention_filters, Frame, QueryRequest, SessionHandle};
use crate::turn::{Answer, Trigger, Turn};
use crate::workspace::Workspace;

/// Kinds that wake an agent by default: stream messages, old and new.
pub const DEFAULT_TRIGGER_KINDS: &[Kind] = &[KIND_STREAM_MESSAGE, KIND_STREAM_MESSAGE_V2];

/// How many mentions may queue behind a busy channel before the oldest are
/// dropped.
///
/// Bounded by construction: a channel that mentions the agent faster than it can
/// answer must not grow this process without limit. Drops are counted
/// ([`Dispatcher::dropped`]) rather than hidden.
pub const MAX_QUEUED_PER_CHANNEL: usize = 64;

/// How often the run loop checks schedules.
const SCHEDULE_TICK: Duration = Duration::from_secs(20);

/// Shortest wait before re-subscribing after the stream ends.
const RECONNECT_MIN: Duration = Duration::from_millis(250);

/// Longest wait between re-subscribe attempts.
const RECONNECT_MAX: Duration = Duration::from_secs(30);

/// How often the cursor is written while frames are flowing.
const CURSOR_FLUSH_MS: u64 = 2_000;

type Handler = Arc<dyn Fn(Turn) -> crate::transport::BoxFuture<'static, Result<()>> + Send + Sync>;

type ErrorSink = Arc<dyn Fn(&SdkError) + Send + Sync>;

/// Everything a turn needs about the agent it belongs to.
///
/// Shared behind an `Arc`: one of these backs every concurrent turn.
pub struct AgentCore {
    pub(crate) workspace: Workspace,
    pub(crate) name: String,
    pub(crate) instructions: String,
    pub(crate) model: String,
    pub(crate) backend: Arc<dyn Backend>,
    pub(crate) tools: ToolRegistry,
    pub(crate) memory: Option<Memory>,
    pub(crate) max_steps: usize,
    pub(crate) max_tokens: u32,
    pub(crate) context_limit: usize,
    pub(crate) memory_limit: usize,
}

impl core::fmt::Debug for AgentCore {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AgentCore")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("tools", &self.tools.names())
            .field("memory", &self.memory.is_some())
            .finish_non_exhaustive()
    }
}

impl AgentCore {
    /// The workspace the agent lives in.
    #[must_use]
    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    /// The agent's name, as it appears in its own system prompt.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The caller's instructions.
    #[must_use]
    pub fn instructions(&self) -> &str {
        &self.instructions
    }

    /// The model id every turn calls.
    #[must_use]
    pub fn model(&self) -> &str {
        &self.model
    }

    /// The registered tools.
    #[must_use]
    pub const fn tools(&self) -> &ToolRegistry {
        &self.tools
    }

    /// Durable memory, if `.with_memory()` was used.
    #[must_use]
    pub const fn memory(&self) -> Option<&Memory> {
        self.memory.as_ref()
    }

    /// Most model calls one turn may make before the tools are withdrawn.
    #[must_use]
    pub const fn max_steps(&self) -> usize {
        self.max_steps
    }

    /// Output-token bound per model call.
    #[must_use]
    pub const fn max_tokens(&self) -> u32 {
        self.max_tokens
    }

    /// How many recent channel events a turn reads for context.
    #[must_use]
    pub const fn context_limit(&self) -> usize {
        self.context_limit
    }
}

/// Why a delivered event did not become a turn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Skip {
    /// The agent authored it. Answering your own message is how a loop starts.
    Own,
    /// Not a conversation kind the agent listens for.
    WrongKind,
    /// The agent is not `p`-tagged in it.
    NotMentioned,
    /// `.respond_to_members()` is on and the event is not channel-scoped.
    NotChannelScoped,
    /// An allowlist is set and the author is not on it.
    NotAllowed,
    /// Older than the backfill window on a cold start.
    TooOld,
}

impl Skip {
    /// A short, log-shaped reason.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own event",
            Self::WrongKind => "kind not listened for",
            Self::NotMentioned => "not mentioned",
            Self::NotChannelScoped => "not channel-scoped",
            Self::NotAllowed => "author not allowed",
            Self::TooOld => "older than the backfill window",
        }
    }
}

/// Per-channel single-flight with batching.
///
/// The rule: **at most one turn per key at a time.** Anything that arrives while
/// a key is busy waits, and the whole waiting set is released as one batch when
/// that key's turn finishes. That is what stops an agent from posting three
/// half-answers to three messages that were really one thought, and what stops it
/// from running N concurrent model calls for one channel.
///
/// Pure state machine: no clock, no I/O, no tasks. The run loop owns one of these
/// and the tests drive it directly.
#[derive(Debug, Default)]
pub struct Dispatcher {
    busy: HashSet<String>,
    queued: HashMap<String, Vec<Event>>,
    dropped: u64,
}

impl Dispatcher {
    /// An idle dispatcher.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Offer an event for `key`.
    ///
    /// Returns `Some(batch)` when the caller should start a turn now (the key was
    /// idle and is now marked busy), or `None` when the event was queued behind a
    /// running turn.
    pub fn enqueue(&mut self, key: impl Into<String>, event: Event) -> Option<Vec<Event>> {
        let key = key.into();
        if self.busy.contains(&key) {
            let queue = self.queued.entry(key).or_default();
            queue.push(event);
            if queue.len() > MAX_QUEUED_PER_CHANNEL {
                // Shed the oldest: the newest mention is the one a person is
                // still waiting on.
                queue.remove(0);
                self.dropped = self.dropped.saturating_add(1);
            }
            return None;
        }
        self.busy.insert(key);
        Some(vec![event])
    }

    /// Report that `key`'s turn has finished.
    ///
    /// Returns `Some(batch)` if mentions queued up meanwhile — the key stays busy
    /// and the caller starts the next turn — or `None` if the key is now idle.
    pub fn release(&mut self, key: &str) -> Option<Vec<Event>> {
        match self.queued.remove(key) {
            Some(batch) if !batch.is_empty() => {
                // Still busy: the batch we just handed back is about to run.
                Some(batch)
            }
            _ => {
                self.busy.remove(key);
                None
            }
        }
    }

    /// Whether a turn is running for `key`.
    #[must_use]
    pub fn is_busy(&self, key: &str) -> bool {
        self.busy.contains(key)
    }

    /// How many mentions are waiting for `key`.
    #[must_use]
    pub fn queued(&self, key: &str) -> usize {
        self.queued.get(key).map_or(0, Vec::len)
    }

    /// How many keys have a turn running.
    #[must_use]
    pub fn busy_keys(&self) -> usize {
        self.busy.len()
    }

    /// How many mentions were shed because a queue was full.
    #[must_use]
    pub const fn dropped(&self) -> u64 {
        self.dropped
    }
}

/// Where an agent remembers how far it has read.
///
/// Implement this to put the cursor wherever your deployment keeps state.
pub trait CursorStore: Send + Sync {
    /// The stored cursor, or `None` if there is none.
    ///
    /// # Errors
    ///
    /// Implementation-defined; a missing cursor is `Ok(None)`, not an error.
    fn load(&self) -> Result<Option<String>>;

    /// Store a cursor.
    ///
    /// # Errors
    ///
    /// Implementation-defined.
    fn save(&self, cursor: &str) -> Result<()>;
}

/// A cursor that lives only as long as the process.
///
/// The default. A restart re-subscribes from the live edge and skips anything
/// older than the backfill window — which is right for an agent you run in the
/// foreground, and wrong for one that must not miss a mention while it is down.
/// For that, use [`FileCursor`].
#[derive(Debug, Default)]
pub struct MemoryCursor {
    inner: std::sync::Mutex<Option<String>>,
}

impl CursorStore for MemoryCursor {
    fn load(&self) -> Result<Option<String>> {
        Ok(self.inner.lock().ok().and_then(|c| c.clone()))
    }

    fn save(&self, cursor: &str) -> Result<()> {
        if let Ok(mut slot) = self.inner.lock() {
            *slot = Some(cursor.to_string());
        }
        Ok(())
    }
}

/// A cursor in a file, so a restart resumes where the process left off.
///
/// Writes are atomic-ish: the value goes to `<path>.tmp` and is renamed over the
/// target, so a crash mid-write leaves the previous cursor intact rather than a
/// truncated one.
///
/// This is the only part of the SDK that touches the filesystem, and it is opt-in.
#[derive(Debug, Clone)]
pub struct FileCursor {
    path: std::path::PathBuf,
}

impl FileCursor {
    /// A cursor stored at `path`.
    #[must_use]
    pub fn new(path: impl Into<std::path::PathBuf>) -> Self {
        Self { path: path.into() }
    }

    /// A cursor stored under `dir`, named for the agent's own key.
    ///
    /// Two agents sharing a state directory therefore do not share a cursor.
    #[must_use]
    pub fn in_dir(dir: impl Into<std::path::PathBuf>, npub: &str) -> Self {
        let mut path = dir.into();
        let safe: String = npub
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
            .take(72)
            .collect();
        path.push(format!("{safe}.cursor"));
        Self { path }
    }

    /// The file being written.
    #[must_use]
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }
}

impl CursorStore for FileCursor {
    fn load(&self) -> Result<Option<String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(text) => {
                let trimmed = text.trim();
                Ok(if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_string())
                })
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e.into()),
        }
    }

    fn save(&self, cursor: &str) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            if !parent.as_os_str().is_empty() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut temp = self.path.clone();
        temp.set_extension("tmp");
        std::fs::write(&temp, cursor)?;
        std::fs::rename(&temp, &self.path)?;
        Ok(())
    }
}

struct CursorWriter {
    store: Arc<dyn CursorStore>,
    last_written: Option<String>,
    /// When the last write happened. `None` means "never", which is why the very
    /// first cursor is written immediately rather than being throttled: a crash
    /// right after the first frame should still resume from it.
    last_at_ms: Option<u64>,
}

impl CursorWriter {
    fn new(store: Arc<dyn CursorStore>) -> Self {
        Self {
            store,
            last_written: None,
            last_at_ms: None,
        }
    }

    /// Persist `cursor` if it changed and either this is the first write, the
    /// flush interval elapsed, or the caller forced it (which the run loop does
    /// whenever a turn completes).
    fn maybe_save(&mut self, cursor: &str, now_ms: u64, force: bool) -> Result<()> {
        if self.last_written.as_deref() == Some(cursor) {
            return Ok(());
        }
        let throttled = self
            .last_at_ms
            .is_some_and(|last| now_ms.saturating_sub(last) < CURSOR_FLUSH_MS);
        if !force && throttled {
            return Ok(());
        }
        self.store.save(cursor)?;
        self.last_written = Some(cursor.to_string());
        self.last_at_ms = Some(now_ms);
        Ok(())
    }
}

/// One registered schedule.
#[derive(Debug, Clone)]
struct Schedule {
    cron: Cron,
    prompt: String,
    channel: Option<String>,
    last_minute: Option<u64>,
}

/// A built agent.
pub struct Agent {
    core: Arc<AgentCore>,
    handler: Option<Handler>,
    on_error: ErrorSink,
    respond_to_members: bool,
    allowlist: Vec<PubKey>,
    kinds: Vec<Kind>,
    schedules: std::sync::Mutex<Vec<Schedule>>,
    cursor: Arc<dyn CursorStore>,
    max_backfill_secs: u64,
    typing: bool,
}

impl core::fmt::Debug for Agent {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Agent")
            .field("core", &self.core)
            .field("custom_handler", &self.handler.is_some())
            .field("respond_to_members", &self.respond_to_members)
            .field("allowlist", &self.allowlist.len())
            .finish_non_exhaustive()
    }
}

impl Agent {
    /// Start building an agent for a workspace.
    #[must_use]
    pub fn builder(workspace: Workspace) -> AgentBuilder {
        AgentBuilder::new(workspace)
    }

    /// The shared core every turn borrows.
    #[must_use]
    pub const fn core(&self) -> &Arc<AgentCore> {
        &self.core
    }

    /// The agent's name.
    #[must_use]
    pub fn name(&self) -> &str {
        &self.core.name
    }

    /// The agent's own `npub`.
    #[must_use]
    pub fn npub(&self) -> &str {
        self.core.workspace.whoami()
    }

    /// Whether this event should become a turn, and if not, why not.
    ///
    /// Pure over the agent's configuration — no clock read of its own, so a test
    /// can pin every rule. `cutoff` is the oldest `created_at` that will be
    /// answered, or `None` to accept any age (which is what a resumed cursor
    /// means: the relay is replaying events the agent genuinely missed).
    ///
    /// # Errors
    ///
    /// The [`Skip`] reason, when the event is not for this agent.
    pub fn admit(&self, event: &Event, cutoff: Option<u64>) -> core::result::Result<(), Skip> {
        if event.pubkey == self.core.workspace.pubkey() {
            return Err(Skip::Own);
        }
        if !self.kinds.contains(&event.kind) {
            return Err(Skip::WrongKind);
        }
        let me = self.core.workspace.identity().pubkey_hex();
        if !event.tag_values("p").any(|v| v.eq_ignore_ascii_case(&me)) {
            return Err(Skip::NotMentioned);
        }
        if self.respond_to_members && event.channel_id().is_none() {
            return Err(Skip::NotChannelScoped);
        }
        if !self.allowlist.is_empty() && !self.allowlist.contains(&event.pubkey) {
            return Err(Skip::NotAllowed);
        }
        if let Some(cutoff) = cutoff {
            if event.created_at < cutoff {
                return Err(Skip::TooOld);
            }
        }
        Ok(())
    }

    /// The single-flight key for an event: its channel, or its author for a
    /// mention that arrived outside one.
    #[must_use]
    pub fn key_for(event: &Event) -> String {
        match event.channel_id() {
            Some(channel) => format!("channel:{channel}"),
            None => format!("direct:{}", to_hex(&event.pubkey)),
        }
    }

    /// Build a turn for a batch of mentions.
    ///
    /// The channel comes from the newest mention in the batch.
    #[must_use]
    pub fn turn_for(&self, batch: Vec<Event>) -> Turn {
        let channel = batch
            .last()
            .and_then(|event| event.channel_id().map(str::to_string));
        Turn::new(Arc::clone(&self.core), channel, Trigger::Mention(batch))
    }

    /// Run one turn: the caller's handler, or the default think-then-reply.
    ///
    /// # Errors
    ///
    /// Whatever the handler returned.
    pub async fn run_turn(&self, turn: Turn) -> Result<()> {
        match self.handler.as_ref() {
            Some(handler) => handler(turn).await,
            None => Self::default_turn(turn).await,
        }
    }

    /// The default handler: show you are working, think, reply in-thread.
    async fn default_turn(turn: Turn) -> Result<()> {
        let answer: Answer = turn.think().await?;
        turn.reply(&answer).await?;
        Ok(())
    }

    /// Serve forever: subscribe, answer mentions, fire schedules.
    ///
    /// Returns only on an unrecoverable failure — a refused subscription that
    /// backoff cannot fix. Everything else (a dropped stream, a failed turn, a
    /// failed publish) is reported through the error sink and survived.
    ///
    /// # Errors
    ///
    /// [`SdkError`] from the initial subscription, or from cursor persistence
    /// failing repeatedly.
    pub async fn run(self) -> Result<()> {
        Arc::new(self).serve().await
    }

    /// The run loop, over a shared agent so turns can be spawned.
    ///
    /// # Errors
    ///
    /// As [`Agent::run`].
    pub async fn serve(self: Arc<Self>) -> Result<()> {
        let stored = self.cursor.load().unwrap_or(None);
        let resumed = stored.is_some();
        let cutoff = if resumed {
            // A stored cursor means these events were genuinely missed while the
            // agent was down: answer them however old they are.
            None
        } else {
            Some(
                self.core
                    .workspace
                    .now_secs()
                    .saturating_sub(self.max_backfill_secs),
            )
        };

        let filters = mention_filters(
            &self.core.workspace.identity().pubkey_hex(),
            self.kinds.clone(),
        );
        let mut session = self.open(&filters, stored).await?;

        let mut seen = SeenIds::new();
        let mut dispatcher = Dispatcher::new();
        let mut cursor_writer = CursorWriter::new(Arc::clone(&self.cursor));
        let (done_tx, mut done_rx) = tokio::sync::mpsc::channel::<String>(64);
        let mut ticker = tokio::time::interval(SCHEDULE_TICK);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        loop {
            tokio::select! {
                // Live frames.
                frame = self.core.workspace.next_frame(&mut session) => {
                    match frame {
                        Ok(Some(Frame::Event { offset, event })) => {
                            let _ = cursor_writer.maybe_save(&offset, self.core.workspace.now_ms(), false);
                            if !seen.insert(event.id) {
                                continue;
                            }
                            // A skip is not a failure: noise, other people's
                            // mentions and stale backfill are the common case, and
                            // reporting each one would drown the real errors.
                            if self.admit(&event, cutoff).is_err() {
                                continue;
                            }
                            let key = Self::key_for(&event);
                            if let Some(batch) = dispatcher.enqueue(key.clone(), event) {
                                self.spawn_turn(key, batch, done_tx.clone());
                            }
                        }
                        Ok(Some(Frame::Heartbeat { offset })) => {
                            let _ = cursor_writer.maybe_save(&offset, self.core.workspace.now_ms(), false);
                        }
                        Ok(Some(Frame::Reset { reason, from })) => {
                            (self.on_error)(&SdkError::transport(format!(
                                "the relay refused cursor {from} ({reason}); re-reading recent mentions"
                            )));
                            // The cursor is gone, so history is the only way to
                            // find what was missed. Dedup makes the overlap free.
                            self.backfill(&mut seen, &mut dispatcher, &done_tx, cutoff).await;
                            session = self.reopen(&filters).await?;
                        }
                        Ok(None) => {
                            session = self.reopen(&filters).await?;
                        }
                        Err(e) => {
                            (self.on_error)(&e);
                            session = self.reopen(&filters).await?;
                        }
                    }
                }

                // A turn finished: release its channel and start any batch that
                // queued up while it ran.
                Some(key) = done_rx.recv() => {
                    if let Some(cursor) = session.cursor.clone() {
                        let _ = cursor_writer.maybe_save(&cursor, self.core.workspace.now_ms(), true);
                    }
                    if let Some(batch) = dispatcher.release(&key) {
                        self.spawn_turn(key, batch, done_tx.clone());
                    }
                }

                // Schedules.
                _ = ticker.tick() => {
                    for (prompt, channel) in self.due_schedules() {
                        self.spawn_schedule(prompt, channel);
                    }
                }
            }
        }
    }

    async fn open(
        &self,
        filters: &[buzz::relay::filter::Filter],
        stored: Option<String>,
    ) -> Result<SessionHandle> {
        let mut session = self.core.workspace.subscribe(filters.to_vec()).await?;
        // Set before the first `next_frame`: the bundled HTTP transport starts its
        // pump lazily, precisely so a resumed cursor can be applied here.
        if stored.is_some() {
            session.cursor = stored;
        }
        Ok(session)
    }

    /// Whether a failure means "stop", rather than "wait and try again".
    ///
    /// Deliberately narrow. A credential the relay rejects (401/403) or a
    /// configuration mistake will never fix itself, so retrying is a hot loop that
    /// hides the real problem. **Everything else keeps retrying** — including a
    /// 404, which during a relay redeploy means "the route is not registered
    /// *yet*", not "give up". Treating a 404 as fatal is the difference between an
    /// agent that rides out a deploy and one that quietly dies in the middle of it.
    #[must_use]
    pub fn is_fatal(error: &SdkError) -> bool {
        matches!(
            error,
            SdkError::Config(_)
                | SdkError::Identity(_)
                | SdkError::Env { .. }
                | SdkError::Http {
                    status: 401 | 403,
                    ..
                }
        )
    }

    async fn reopen(&self, filters: &[buzz::relay::filter::Filter]) -> Result<SessionHandle> {
        let mut wait = RECONNECT_MIN;
        loop {
            tokio::time::sleep(wait).await;
            let stored = self.cursor.load().unwrap_or(None);
            match self.open(filters, stored).await {
                Ok(session) => return Ok(session),
                Err(e) => {
                    (self.on_error)(&e);
                    if Self::is_fatal(&e) {
                        return Err(e);
                    }
                    wait = (wait * 2).min(RECONNECT_MAX);
                }
            }
        }
    }

    /// Re-read recent mentions from history, for when a cursor is refused.
    ///
    /// Bounded to one page. Duplicates are absorbed by `seen`, so overlapping with
    /// what was already answered costs nothing.
    async fn backfill(
        self: &Arc<Self>,
        seen: &mut SeenIds,
        dispatcher: &mut Dispatcher,
        done_tx: &tokio::sync::mpsc::Sender<String>,
        cutoff: Option<u64>,
    ) {
        let request = QueryRequest::new().limit(50).kinds(self.kinds.clone());
        let page = match self.core.workspace.query(&request).await {
            Ok(page) => page,
            Err(e) => {
                (self.on_error)(&e);
                return;
            }
        };
        for event in page.chronological() {
            if !seen.insert(event.id) {
                continue;
            }
            if self.admit(&event, cutoff).is_err() {
                continue;
            }
            let key = Self::key_for(&event);
            if let Some(batch) = dispatcher.enqueue(key.clone(), event) {
                self.spawn_turn(key, batch, done_tx.clone());
            }
        }
    }

    fn spawn_turn(
        self: &Arc<Self>,
        key: String,
        batch: Vec<Event>,
        done: tokio::sync::mpsc::Sender<String>,
    ) {
        let agent = Arc::clone(self);
        tokio::spawn(async move {
            let turn = agent.turn_for(batch);
            // Presence is the runtime's job only when the caller did not write a
            // handler; a custom handler owns its own `turn.typing()`.
            if agent.typing && agent.handler.is_none() {
                if let Err(e) = turn.typing().await {
                    (agent.on_error)(&e);
                }
            }
            if let Err(e) = agent.run_turn(turn).await {
                (agent.on_error)(&e);
            }
            // Always release, even when the turn failed: a channel that stays
            // busy forever is worse than a turn that failed once.
            let _ = done.send(key).await;
        });
    }

    /// Start a scheduled turn.
    ///
    /// Scheduled turns run outside the per-channel gate: they fire at most once
    /// per matching minute, so they cannot pile up on themselves.
    fn spawn_schedule(self: &Arc<Self>, prompt: String, channel: Option<String>) {
        let agent = Arc::clone(self);
        tokio::spawn(async move {
            let channel =
                channel.or_else(|| agent.core.workspace.default_channel().map(str::to_string));
            let turn = Turn::new(Arc::clone(&agent.core), channel, Trigger::Schedule(prompt));
            if let Err(e) = agent.run_turn(turn).await {
                (agent.on_error)(&e);
            }
        });
    }

    /// Schedules whose minute has arrived, marking them fired.
    ///
    /// Fire-once is keyed on the minute rather than on the poll, so a tick that
    /// lands twice inside one minute cannot double-fire.
    fn due_schedules(&self) -> Vec<(String, Option<String>)> {
        let now = self.core.workspace.now_secs();
        let minute = Cron::minute_of(now);
        let mut due = Vec::new();
        if let Ok(mut schedules) = self.schedules.lock() {
            for schedule in schedules.iter_mut() {
                if schedule.last_minute == Some(minute) {
                    continue;
                }
                if schedule.cron.matches(now) {
                    schedule.last_minute = Some(minute);
                    due.push((schedule.prompt.clone(), schedule.channel.clone()));
                }
            }
        }
        due
    }
}

/// Builder for [`Agent`].
///
/// Every method is chainable and total: none of them can fail, and none of them
/// panic. A value that cannot be validated in place — a cron expression, for
/// instance — is checked at [`AgentBuilder::build`], which is the one place that
/// returns a `Result`.
pub struct AgentBuilder {
    workspace: Workspace,
    name: String,
    instructions: String,
    model: Option<String>,
    backend: Option<Arc<dyn Backend>>,
    tools: ToolRegistry,
    memory: bool,
    memory_scope: Option<String>,
    memory_limit: usize,
    respond_to_members: bool,
    allowlist: Vec<PubKey>,
    kinds: Vec<Kind>,
    schedules: Vec<Schedule>,
    handler: Option<Handler>,
    on_error: Option<ErrorSink>,
    max_steps: usize,
    max_tokens: u32,
    context_limit: usize,
    cursor: Option<Arc<dyn CursorStore>>,
    max_backfill_secs: u64,
    typing: bool,
    require_tool_schemas: bool,
    deferred: Vec<SdkError>,
}

impl core::fmt::Debug for AgentBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("AgentBuilder")
            .field("name", &self.name)
            .field("model", &self.model)
            .field("tools", &self.tools.names())
            .field("schedules", &self.schedules.len())
            .field("deferred_errors", &self.deferred.len())
            .finish_non_exhaustive()
    }
}

impl AgentBuilder {
    /// A builder with the SDK's defaults.
    #[must_use]
    pub fn new(workspace: Workspace) -> Self {
        Self {
            workspace,
            name: "agent".to_string(),
            instructions: String::new(),
            model: None,
            backend: None,
            tools: ToolRegistry::new(),
            memory: false,
            memory_scope: None,
            memory_limit: 5,
            respond_to_members: false,
            allowlist: Vec::new(),
            kinds: DEFAULT_TRIGGER_KINDS.to_vec(),
            schedules: Vec::new(),
            handler: None,
            on_error: None,
            max_steps: 8,
            max_tokens: DEFAULT_MAX_TOKENS,
            context_limit: 30,
            cursor: None,
            max_backfill_secs: 3_600,
            typing: true,
            require_tool_schemas: false,
            deferred: Vec::new(),
        }
    }

    /// The agent's name, used in its system prompt and its logs.
    #[must_use]
    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = name.into();
        self
    }

    /// What the agent is for. This leads the system prompt.
    #[must_use]
    pub fn instructions(mut self, instructions: impl Into<String>) -> Self {
        self.instructions = instructions.into();
        self
    }

    /// The model id to call. Defaults to [`DEFAULT_MODEL`].
    #[must_use]
    pub fn model(mut self, model: impl Into<String>) -> Self {
        self.model = Some(model.into());
        self
    }

    /// Use a specific [`Backend`] instead of building one from the environment.
    #[must_use]
    pub fn backend(mut self, backend: impl Backend + 'static) -> Self {
        self.backend = Some(Arc::new(backend));
        self
    }

    /// Supply a model backend that is already shared.
    ///
    /// Use this when one backend serves several agents; [`Self::backend`] is the
    /// common case and does the wrapping for you.
    #[must_use]
    pub fn shared_backend(mut self, backend: Arc<dyn Backend>) -> Self {
        self.backend = Some(backend);
        self
    }

    /// Register a tool. Re-registering a name replaces the earlier tool.
    #[must_use]
    pub fn tool(mut self, tool: Tool) -> Self {
        self.tools.insert(tool);
        self
    }

    /// Register several tools.
    #[must_use]
    pub fn tools(mut self, tools: impl IntoIterator<Item = Tool>) -> Self {
        for tool in tools {
            self.tools.insert(tool);
        }
        self
    }

    /// Give the agent durable memory in the workspace.
    ///
    /// Read [`crate::memory`] before using it in a community that is not yours:
    /// engrams are durable and access-controlled by the relay, but they are **not**
    /// encrypted or `#p`-gated, so memory content is readable by the community.
    #[must_use]
    pub const fn with_memory(mut self) -> Self {
        self.memory = true;
        self
    }

    /// Give the agent memory in a named scope.
    #[must_use]
    pub fn with_memory_scope(mut self, scope: impl Into<String>) -> Self {
        self.memory = true;
        self.memory_scope = Some(scope.into());
        self
    }

    /// How many memories a turn recalls into its prompt. Default 5.
    #[must_use]
    pub const fn memory_limit(mut self, limit: usize) -> Self {
        self.memory_limit = limit;
        self
    }

    /// Answer only mentions that arrive inside a channel.
    ///
    /// What this actually enforces, precisely: the triggering event must carry an
    /// `h` tag. That is a real trust boundary, because the relay only delivers a
    /// channel's events to a reader it authorized, and a private channel refuses
    /// writes from non-members at ingest — so a channel-scoped mention the agent
    /// receives came from someone who shares that channel with it.
    ///
    /// What it does **not** do: verify community membership in an *open* channel,
    /// where anyone in the community may post. For a hard list, use
    /// [`AgentBuilder::respond_to`].
    #[must_use]
    pub const fn respond_to_members(mut self) -> Self {
        self.respond_to_members = true;
        self
    }

    /// Answer only these pubkeys.
    ///
    /// Accepts an `npub` or 64-character hex. An unparseable value is recorded and
    /// reported by [`AgentBuilder::build`] rather than silently ignored — a
    /// mistyped allowlist entry that quietly widened the gate would be a security
    /// bug.
    #[must_use]
    pub fn respond_to(mut self, who: &str) -> Self {
        let parsed = if who.starts_with("npub1") {
            buzz::relay::crypto::bech32::decode_npub(who)
        } else {
            buzz::relay::crypto::from_hex32(who)
        };
        match parsed {
            Some(pubkey) => self.allowlist.push(pubkey),
            None => self.deferred.push(SdkError::config(format!(
                "respond_to: {who:?} is not an npub or a 64-character hex pubkey"
            ))),
        }
        self
    }

    /// Which event kinds wake the agent. Defaults to
    /// [`DEFAULT_TRIGGER_KINDS`].
    #[must_use]
    pub fn trigger_kinds(mut self, kinds: Vec<Kind>) -> Self {
        self.kinds = kinds;
        self
    }

    /// Run `prompt` on a cron schedule, in the workspace's default channel.
    ///
    /// UTC, minute granularity, five fields — see [`crate::cron`] for exactly
    /// what is supported. A schedule missed while the process was down is **not**
    /// backfilled.
    ///
    /// This drives the agent's own run loop, so it is for an out-of-process
    /// node-tier agent. A constrained persona hosted by the relay instead
    /// declares its schedules in its bundle, where the platform fires them
    /// durably — see [`crate::cron`] for the tier boundary.
    ///
    /// A malformed expression is reported by [`AgentBuilder::build`].
    #[must_use]
    pub fn schedule(self, cron: &str, prompt: impl Into<String>) -> Self {
        self.push_schedule(cron, prompt, None)
    }

    /// Run `prompt` on a cron schedule, in a named channel.
    #[must_use]
    pub fn schedule_in(
        self,
        channel: impl Into<String>,
        cron: &str,
        prompt: impl Into<String>,
    ) -> Self {
        let channel = channel.into();
        self.push_schedule(cron, prompt, Some(channel))
    }

    fn push_schedule(
        mut self,
        cron: &str,
        prompt: impl Into<String>,
        channel: Option<String>,
    ) -> Self {
        match Cron::parse(cron) {
            Ok(parsed) => self.schedules.push(Schedule {
                cron: parsed,
                prompt: prompt.into(),
                channel,
                last_minute: None,
            }),
            Err(e) => self.deferred.push(e),
        }
        self
    }

    /// Handle each turn yourself.
    ///
    /// Omit this and the agent does the sensible default: think, then reply
    /// in-thread. Install one and you own the turn — including whether to show a
    /// typing indicator, which the runtime then leaves to you.
    #[must_use]
    pub fn on_mention<F, Fut>(mut self, handler: F) -> Self
    where
        F: Fn(Turn) -> Fut + Send + Sync + 'static,
        Fut: core::future::Future<Output = Result<()>> + Send + 'static,
    {
        self.handler = Some(Arc::new(move |turn| Box::pin(handler(turn))));
        self
    }

    /// Where to report failures the agent survived.
    ///
    /// Default: one line per failure on stderr. A library should not choose your
    /// logging framework, and an agent that swallows errors silently is worse than
    /// one that is briefly noisy — so this is the seam.
    #[must_use]
    pub fn on_error<F>(mut self, sink: F) -> Self
    where
        F: Fn(&SdkError) + Send + Sync + 'static,
    {
        self.on_error = Some(Arc::new(sink));
        self
    }

    /// Require every registered tool to carry an explicit argument schema.
    ///
    /// A tool advertises the permissive `{"type":"object"}` by default — a
    /// deliberate, documented choice (see [`crate::tool`]): the SDK will not
    /// reflect over your argument type, and a bad argument is handed back to the
    /// model as a correctable tool error rather than a wrong-but-plausible schema.
    /// That default keeps the one-liner ergonomic, but it costs the model a step
    /// or two of self-correction when arguments are non-obvious.
    ///
    /// Turn this on and [`AgentBuilder::build`] refuses — naming each offender —
    /// unless every tool has an explicit schema (via [`crate::Tool::schema`] or
    /// [`crate::tool_with_schema`]). Use it when the discipline is worth more than
    /// the brevity, or for a persona you expect to run unattended.
    #[must_use]
    pub const fn require_tool_schemas(mut self, require: bool) -> Self {
        self.require_tool_schemas = require;
        self
    }

    /// Most model calls one turn may make before its tools are withdrawn.
    /// Default 8.
    #[must_use]
    pub const fn max_steps(mut self, steps: usize) -> Self {
        self.max_steps = steps;
        self
    }

    /// Output-token bound per model call.
    #[must_use]
    pub const fn max_tokens(mut self, max_tokens: u32) -> Self {
        self.max_tokens = max_tokens;
        self
    }

    /// How many recent channel events a turn reads for context. Default 30; `0`
    /// disables the read entirely.
    #[must_use]
    pub const fn context_limit(mut self, limit: usize) -> Self {
        self.context_limit = limit;
        self
    }

    /// Whether the runtime posts a typing indicator before the default handler
    /// runs. Default on.
    #[must_use]
    pub const fn typing(mut self, typing: bool) -> Self {
        self.typing = typing;
        self
    }

    /// Where to persist the stream cursor. Defaults to [`MemoryCursor`].
    #[must_use]
    pub fn cursor_store(mut self, store: Arc<dyn CursorStore>) -> Self {
        self.cursor = Some(store);
        self
    }

    /// Persist the cursor in `dir`, so a restart resumes where it left off.
    ///
    /// The file is named for the agent's own key. This is the only filesystem
    /// access the SDK performs, and it happens only if you ask for it.
    #[must_use]
    pub fn state_dir(mut self, dir: impl Into<std::path::PathBuf>) -> Self {
        let npub = self.workspace.whoami().to_string();
        self.cursor = Some(Arc::new(FileCursor::in_dir(dir, &npub)));
        self
    }

    /// On a cold start (no stored cursor), how far back to answer mentions.
    /// Default one hour; zero answers nothing older than now.
    #[must_use]
    pub const fn max_backfill(mut self, window: Duration) -> Self {
        self.max_backfill_secs = window.as_secs();
        self
    }

    /// Finish the agent.
    ///
    /// # Errors
    ///
    /// * Any error deferred by a builder method (a bad cron expression, a bad
    ///   allowlist entry) — all of them, in the order they happened.
    /// * [`SdkError::Config`] if no backend was supplied and none can be built:
    ///   without the `http` feature there is no default backend at all, and with
    ///   it, `ANTHROPIC_API_KEY` must be set.
    pub fn build(self) -> Result<Agent> {
        if let Some(first) = self.deferred.into_iter().next() {
            return Err(first);
        }

        if self.require_tool_schemas {
            let undocumented = self.tools.undocumented();
            if !undocumented.is_empty() {
                return Err(SdkError::config(format!(
                    "require_tool_schemas is on, but these tools have no explicit schema: {}. \
                     Attach one with Tool::schema(...) or tool_with_schema(...).",
                    undocumented.join(", ")
                )));
            }
        }

        // There is NO default backend, on purpose.
        //
        // A fallback to an Anthropic client built from an ambient
        // `ANTHROPIC_API_KEY` would quietly re-open the boundary the platform's
        // AI gateway exists to close: on this platform no component but the gateway
        // holds a provider key or egresses to a provider
        // (`docs/the-ai-gateway.md`). A silent default is the worst
        // shape for that — an agent composed without thinking about model access would
        // get a direct provider connection and an ambient credential. Choosing the model
        // seam is the composing binary's explicit decision.
        let backend: Arc<dyn Backend> = self.backend.ok_or_else(|| {
            SdkError::config(
                "no model backend: supply one with .backend(...). Prefer a gateway-backed \
                 backend so the provider key and egress stay with the platform's AI gateway; \
                 `backend::AnthropicBackend::from_env()` is available for local development \
                 and holds a provider key in this process.",
            )
        })?;

        let memory = self.memory.then(|| match self.memory_scope {
            Some(scope) => Memory::with_scope(self.workspace.clone(), scope),
            None => Memory::new(self.workspace.clone()),
        });

        let core = Arc::new(AgentCore {
            workspace: self.workspace,
            name: self.name,
            instructions: self.instructions,
            model: self.model.unwrap_or_else(|| DEFAULT_MODEL.to_string()),
            backend,
            tools: self.tools,
            memory,
            max_steps: self.max_steps,
            max_tokens: self.max_tokens,
            context_limit: self.context_limit,
            memory_limit: self.memory_limit,
        });

        Ok(Agent {
            core,
            handler: self.handler,
            on_error: self
                .on_error
                .unwrap_or_else(|| Arc::new(|e: &SdkError| eprintln!("buzz-sdk: {e}"))),
            respond_to_members: self.respond_to_members,
            allowlist: self.allowlist,
            kinds: self.kinds,
            schedules: std::sync::Mutex::new(self.schedules),
            cursor: self
                .cursor
                .unwrap_or_else(|| Arc::new(MemoryCursor::default())),
            max_backfill_secs: self.max_backfill_secs,
            typing: self.typing,
        })
    }

    /// Build and serve.
    ///
    /// The last line of an agent's `main`.
    ///
    /// # Errors
    ///
    /// As [`AgentBuilder::build`] and [`Agent::run`].
    pub async fn run(self) -> Result<()> {
        self.build()?.run().await
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::identity::Identity;
    use crate::testing::MemoryTransport;
    use crate::workspace::FixedClock;

    const SK: [u8; 32] = [
        0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00, 0x11,
        0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff, 0x00,
        0x11, 0x23,
    ];
    /// Another member's key.
    const THEIRS: [u8; 32] = [
        0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xff, 0xee, 0xdd, 0xcc, 0xbb,
        0xaa, 0x99, 0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11, 0x00, 0xff, 0xee, 0xdd, 0xcc,
        0xbb, 0xab,
    ];

    fn event(sk: &[u8; 32], kind: Kind, tags: Vec<Vec<String>>, content: &str, at: u64) -> Event {
        Event::sign_new(sk, kind, tags, content, at).unwrap()
    }

    fn mention(channel: Option<&str>, content: &str, at: u64) -> Event {
        let me = Identity::from_secret(SK).unwrap().pubkey_hex();
        let mut tags = vec![vec!["p".to_string(), me]];
        if let Some(channel) = channel {
            tags.push(vec!["h".to_string(), channel.to_string()]);
        }
        event(&THEIRS, KIND_STREAM_MESSAGE, tags, content, at)
    }

    async fn agent_with(
        transport: Arc<MemoryTransport>,
        configure: impl FnOnce(AgentBuilder) -> AgentBuilder,
    ) -> Agent {
        let workspace = Workspace::builder("")
            .identity(Identity::from_secret(SK).unwrap())
            .transport(transport as Arc<dyn crate::Transport>)
            .clock(Arc::new(FixedClock::new(1_700_000_000_000)))
            .default_channel("general")
            .connect()
            .await
            .unwrap();
        configure(
            Agent::builder(workspace)
                .name("test-bot")
                .backend(Arc::new(crate::backend::MockBackend::answering("ok"))),
        )
        .build()
        .unwrap()
    }

    // ── single-flight state machine ──────────────────────────────────────────

    #[test]
    fn mentions_queue_while_busy_and_batch_on_release() {
        let mut dispatcher = Dispatcher::new();
        let first = dispatcher
            .enqueue("channel:eng", mention(Some("eng"), "first", 1))
            .expect("an idle key runs at once");
        assert_eq!(first.len(), 1);
        assert!(dispatcher.is_busy("channel:eng"));

        // Two more arrive while the turn runs: neither starts a turn.
        assert!(dispatcher
            .enqueue("channel:eng", mention(Some("eng"), "second", 2))
            .is_none());
        assert!(dispatcher
            .enqueue("channel:eng", mention(Some("eng"), "third", 3))
            .is_none());
        assert_eq!(dispatcher.queued("channel:eng"), 2);

        // Release hands both back as ONE batch, and the key stays busy.
        let batch = dispatcher
            .release("channel:eng")
            .expect("a batch was waiting");
        assert_eq!(
            batch.iter().map(|e| e.content.as_str()).collect::<Vec<_>>(),
            ["second", "third"],
            "batched in arrival order"
        );
        assert!(dispatcher.is_busy("channel:eng"), "still running the batch");
        assert_eq!(dispatcher.queued("channel:eng"), 0);

        // Releasing again with nothing queued goes idle.
        assert!(dispatcher.release("channel:eng").is_none());
        assert!(!dispatcher.is_busy("channel:eng"));
    }

    #[test]
    fn channels_do_not_block_each_other() {
        let mut dispatcher = Dispatcher::new();
        assert!(dispatcher
            .enqueue("channel:eng", mention(Some("eng"), "a", 1))
            .is_some());
        assert!(
            dispatcher
                .enqueue("channel:design", mention(Some("design"), "b", 2))
                .is_some(),
            "a different channel runs concurrently"
        );
        assert_eq!(dispatcher.busy_keys(), 2);
    }

    #[test]
    fn a_queue_is_bounded_and_sheds_the_oldest() {
        let mut dispatcher = Dispatcher::new();
        dispatcher.enqueue("k", mention(Some("eng"), "running", 0));
        for i in 0..(MAX_QUEUED_PER_CHANNEL as u64 + 5) {
            dispatcher.enqueue("k", mention(Some("eng"), "queued", 100 + i));
        }
        assert_eq!(dispatcher.queued("k"), MAX_QUEUED_PER_CHANNEL);
        assert_eq!(dispatcher.dropped(), 5);
        let batch = dispatcher.release("k").unwrap();
        assert_eq!(batch.len(), MAX_QUEUED_PER_CHANNEL);
        assert_eq!(
            batch[0].created_at, 105,
            "the newest mentions survived, not the oldest"
        );
    }

    #[test]
    fn only_credentials_and_configuration_stop_the_reconnect_loop() {
        // These never fix themselves.
        assert!(Agent::is_fatal(&SdkError::http(401, "auth-required:")));
        assert!(Agent::is_fatal(&SdkError::http(403, "restricted:")));
        assert!(Agent::is_fatal(&SdkError::config("no identity")));
        assert!(Agent::is_fatal(&SdkError::Identity("bad key".into())));

        // These are what a relay redeploy or a flaky network looks like: keep
        // trying. A 404 in particular means "not registered yet" mid-deploy.
        assert!(!Agent::is_fatal(&SdkError::http(404, "no route")));
        assert!(!Agent::is_fatal(&SdkError::http(502, "bad gateway")));
        assert!(!Agent::is_fatal(&SdkError::http(429, "rate-limited:")));
        assert!(!Agent::is_fatal(&SdkError::transport("connection reset")));
        assert!(!Agent::is_fatal(&SdkError::Io("broken pipe".into())));
    }

    #[test]
    fn releasing_an_unknown_key_is_harmless() {
        let mut dispatcher = Dispatcher::new();
        assert!(dispatcher.release("never-seen").is_none());
    }

    // ── admission ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn admission_answers_mentions_and_refuses_everything_else() {
        let agent = agent_with(Arc::new(MemoryTransport::new()), |b| b).await;

        assert!(agent
            .admit(&mention(Some("eng"), "@bot hi", 100), None)
            .is_ok());

        // Its own event, even one that mentions itself.
        let mine = event(
            &SK,
            KIND_STREAM_MESSAGE,
            vec![
                vec!["p".into(), Identity::from_secret(SK).unwrap().pubkey_hex()],
                vec!["h".into(), "eng".into()],
            ],
            "talking to myself",
            100,
        );
        assert_eq!(agent.admit(&mine, None), Err(Skip::Own));

        // Not mentioned.
        let other = event(
            &THEIRS,
            KIND_STREAM_MESSAGE,
            vec![vec!["h".into(), "eng".into()]],
            "unrelated",
            100,
        );
        assert_eq!(agent.admit(&other, None), Err(Skip::NotMentioned));

        // A kind the agent does not listen for.
        let reaction = event(
            &THEIRS,
            buzz::relay::kind::KIND_REACTION,
            vec![vec![
                "p".into(),
                Identity::from_secret(SK).unwrap().pubkey_hex(),
            ]],
            "+",
            100,
        );
        assert_eq!(agent.admit(&reaction, None), Err(Skip::WrongKind));

        // Older than the backfill cutoff.
        assert_eq!(
            agent.admit(&mention(Some("eng"), "old", 50), Some(100)),
            Err(Skip::TooOld)
        );
        assert!(Skip::Own.as_str().contains("own"));
    }

    #[tokio::test]
    async fn respond_to_members_requires_a_channel() {
        let agent = agent_with(
            Arc::new(MemoryTransport::new()),
            AgentBuilder::respond_to_members,
        )
        .await;
        assert!(agent.admit(&mention(Some("eng"), "hi", 100), None).is_ok());
        assert_eq!(
            agent.admit(&mention(None, "hi", 100), None),
            Err(Skip::NotChannelScoped)
        );
    }

    #[tokio::test]
    async fn an_allowlist_is_enforced_and_a_bad_entry_fails_the_build() {
        let their_npub =
            buzz::relay::crypto::bech32::npub(&Identity::from_secret(THEIRS).unwrap().pubkey())
                .unwrap();
        let agent = agent_with(Arc::new(MemoryTransport::new()), |b| {
            b.respond_to(&their_npub)
        })
        .await;
        assert!(agent.admit(&mention(Some("eng"), "hi", 100), None).is_ok());

        let stranger = Event::sign_new(
            &[0x42u8; 32],
            KIND_STREAM_MESSAGE,
            vec![
                vec!["p".into(), Identity::from_secret(SK).unwrap().pubkey_hex()],
                vec!["h".into(), "eng".into()],
            ],
            "hi",
            100,
        )
        .unwrap();
        assert_eq!(agent.admit(&stranger, None), Err(Skip::NotAllowed));

        // A typo must not silently widen the gate.
        let workspace = Workspace::builder("")
            .identity(Identity::from_secret(SK).unwrap())
            .transport(Arc::new(MemoryTransport::new()) as Arc<dyn crate::Transport>)
            .connect()
            .await
            .unwrap();
        let err = Agent::builder(workspace)
            .backend(Arc::new(crate::backend::MockBackend::answering("ok")))
            .respond_to("npub1-typo")
            .build()
            .unwrap_err();
        assert!(err.to_string().contains("respond_to"), "{err}");
    }

    #[tokio::test]
    async fn require_tool_schemas_refuses_a_permissive_default_and_names_it() {
        let workspace = Workspace::builder("")
            .identity(Identity::from_secret(SK).unwrap())
            .transport(Arc::new(MemoryTransport::new()) as Arc<dyn crate::Transport>)
            .connect()
            .await
            .unwrap();
        // A tool registered with no explicit schema (the permissive default).
        let bare = crate::tool::tool("do_thing", "does a thing", |_: serde_json::Value| async {
            Ok(String::new())
        });
        let err = Agent::builder(workspace)
            .backend(Arc::new(crate::backend::MockBackend::answering("ok")))
            .tool(bare)
            .require_tool_schemas(true)
            .build()
            .unwrap_err();
        assert!(
            err.to_string().contains("do_thing") && err.to_string().contains("explicit schema"),
            "the refusal names the offender: {err}"
        );

        // The same tool WITH a schema builds cleanly.
        let workspace = Workspace::builder("")
            .identity(Identity::from_secret(SK).unwrap())
            .transport(Arc::new(MemoryTransport::new()) as Arc<dyn crate::Transport>)
            .connect()
            .await
            .unwrap();
        let documented = crate::tool::tool(
            "do_thing",
            "does a thing",
            |_: serde_json::Value| async { Ok(String::new()) },
        )
        .schema(
            serde_json::json!({ "type": "object", "properties": { "x": { "type": "string" } } }),
        );
        assert!(Agent::builder(workspace)
            .backend(Arc::new(crate::backend::MockBackend::answering("ok")))
            .tool(documented)
            .require_tool_schemas(true)
            .build()
            .is_ok());
    }

    #[tokio::test]
    async fn a_bad_cron_expression_fails_the_build_not_the_call() {
        let workspace = Workspace::builder("")
            .identity(Identity::from_secret(SK).unwrap())
            .transport(Arc::new(MemoryTransport::new()) as Arc<dyn crate::Transport>)
            .connect()
            .await
            .unwrap();
        // The builder method itself is total: it returns Self, not Result.
        let builder = Agent::builder(workspace)
            .backend(Arc::new(crate::backend::MockBackend::answering("ok")))
            .schedule("not a cron", "digest");
        let err = builder.build().unwrap_err();
        assert!(err.to_string().contains("cron"), "{err}");
    }

    // ── keys ─────────────────────────────────────────────────────────────────

    #[test]
    fn single_flight_keys_separate_channels_from_direct_mentions() {
        let in_channel = mention(Some("eng"), "hi", 1);
        let outside = mention(None, "hi", 1);
        assert_eq!(Agent::key_for(&in_channel), "channel:eng");
        assert!(Agent::key_for(&outside).starts_with("direct:"));
        assert_ne!(Agent::key_for(&in_channel), Agent::key_for(&outside));
    }

    // ── cursor stores ────────────────────────────────────────────────────────

    #[test]
    fn a_file_cursor_round_trips_and_tolerates_a_missing_file() {
        let dir = std::env::temp_dir().join(format!("buzz-sdk-cursor-{}", std::process::id()));
        let store = FileCursor::in_dir(&dir, "npub1testtesttest");
        assert!(
            store.load().unwrap().is_none(),
            "a missing cursor file is not an error"
        );
        store.save("2:1234").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("2:1234"));
        store.save("2:1235").unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("2:1235"));
        assert!(store.path().starts_with(&dir));
        // Leave nothing behind.
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_cursor_writer_throttles_but_honors_a_forced_flush() {
        let store = Arc::new(MemoryCursor::default());
        let mut writer = CursorWriter::new(Arc::clone(&store) as Arc<dyn CursorStore>);

        // The first cursor is written straight away.
        writer.maybe_save("0:1", 0, false).unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("0:1"));

        // Too soon, and not forced: not written.
        writer.maybe_save("0:2", 100, false).unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("0:1"));

        // Forced: written.
        writer.maybe_save("0:2", 100, true).unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("0:2"));

        // After the interval: written.
        writer
            .maybe_save("0:3", 100 + CURSOR_FLUSH_MS, false)
            .unwrap();
        assert_eq!(store.load().unwrap().as_deref(), Some("0:3"));
    }

    // ── schedules ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn a_schedule_fires_once_per_matching_minute() {
        let transport = Arc::new(MemoryTransport::new());
        let clock = FixedClock::new(0);
        let workspace = Workspace::builder("")
            .identity(Identity::from_secret(SK).unwrap())
            .transport(Arc::clone(&transport) as Arc<dyn crate::Transport>)
            .clock(Arc::new(clock.clone()))
            .default_channel("general")
            .connect()
            .await
            .unwrap();
        let agent = Agent::builder(workspace)
            .backend(Arc::new(crate::backend::MockBackend::answering("ok")))
            // Every minute.
            .schedule("* * * * *", "post the digest")
            .build()
            .unwrap();

        clock.set_ms(1_700_000_000_000);
        assert_eq!(agent.due_schedules().len(), 1, "the minute matched");
        assert!(
            agent.due_schedules().is_empty(),
            "a second poll inside the same minute must not re-fire"
        );

        clock.advance_ms(60_000);
        assert_eq!(
            agent.due_schedules().len(),
            1,
            "the next minute fires again"
        );
    }

    #[tokio::test]
    async fn a_schedule_can_name_its_own_channel() {
        let agent = agent_with(Arc::new(MemoryTransport::new()), |b| {
            b.schedule_in("releases", "* * * * *", "digest")
        })
        .await;
        assert_eq!(
            agent.due_schedules(),
            [("digest".to_string(), Some("releases".to_string()))]
        );
    }

    // ── defaults ─────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn builder_defaults_are_the_documented_ones() {
        let agent = agent_with(Arc::new(MemoryTransport::new()), |b| b).await;
        assert_eq!(agent.core().model(), DEFAULT_MODEL);
        assert_eq!(agent.core().max_steps(), 8);
        assert_eq!(agent.core().context_limit(), 30);
        assert!(agent.core().memory().is_none());
        assert!(agent.core().tools().is_empty());
        assert_eq!(agent.kinds, DEFAULT_TRIGGER_KINDS.to_vec());
        assert!(agent.typing);
        assert_eq!(agent.max_backfill_secs, 3_600);
        assert_eq!(agent.name(), "test-bot");
        assert!(agent.npub().starts_with("npub1"));
    }

    #[tokio::test]
    async fn with_memory_attaches_memory() {
        let agent = agent_with(Arc::new(MemoryTransport::new()), |b| {
            b.with_memory_scope("reviewer").memory_limit(2)
        })
        .await;
        assert_eq!(agent.core().memory().unwrap().scope(), "reviewer");
        assert_eq!(agent.core().memory_limit, 2);
    }

    #[cfg(not(feature = "http"))]
    #[tokio::test]
    async fn without_the_http_feature_a_backend_must_be_supplied() {
        let workspace = Workspace::builder("")
            .identity(Identity::from_secret(SK).unwrap())
            .transport(Arc::new(MemoryTransport::new()) as Arc<dyn crate::Transport>)
            .connect()
            .await
            .unwrap();
        let err = Agent::builder(workspace).build().unwrap_err();
        assert!(err.to_string().contains("backend"), "{err}");
    }
}
