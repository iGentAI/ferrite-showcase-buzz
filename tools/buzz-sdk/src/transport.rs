//! The seam between agent logic and the wire.
//!
//! Everything above this module — the mention loop, batching, prompt assembly,
//! tools, memory — is pure logic over four operations: publish an event, read a
//! page of history, open a live session, take the next frame. [`Transport`] is
//! those four operations and nothing else, which is what lets the same agent run
//! beside Ferrite over HTTP or *inside* it over injected queue handles.
//!
//! ## Why the futures are boxed
//!
//! The trait is object-safe (`Arc<dyn Transport>`) because a [`crate::Workspace`]
//! must be able to hold any transport, and the SDK may not add an `async-trait`
//! dependency. Of the two dependency-free options — return-position `impl
//! Future` in traits, or `Pin<Box<dyn Future>>` — only the second is object-safe
//! today, so every method returns [`BoxFuture`].
//!
//! The cost is honest and small: **one heap allocation per call**. A publish
//! already costs a signature and a round trip, so the box is noise; a caller
//! polling [`Transport::next_frame`] in a tight loop pays one allocation per
//! frame. Implementors write `Box::pin(async move { … })` and are otherwise
//! ordinary async code.
//!
//! ## The contract implementors must honor
//!
//! * **`next_frame` must be cancel-safe.** The agent runtime selects over it
//!   alongside timers and turn-completion signals, so its future is dropped
//!   mid-poll routinely. Dropping it must not lose a frame that had already been
//!   handed over — build it out of primitives that guarantee that (a `tokio` mpsc
//!   `recv`, for instance), not out of a hand-rolled state machine that consumes
//!   before it yields.
//! * **`query` applies the request's filters.** The relay's read route pages a
//!   timeline; filters narrow it. A transport may push filters to the server, do
//!   the work locally with [`QueryRequest::keep`], or both — but the page it
//!   returns must not contain events the request excluded.
//! * **Nothing here authorizes anything.** A transport signs and sends; the relay
//!   decides. A page that comes back has already been permission-filtered by the
//!   relay for the agent's own key.

use core::future::Future;
use core::pin::Pin;

use buzz::relay::filter::{filters_match, Filter};
use buzz::relay::{Event, Kind};
use serde::{Deserialize, Serialize};

use crate::error::Result;

/// A boxed, `Send` future — the return type of every [`Transport`] method.
///
/// `'a` borrows the transport (and any argument), so an implementation may hold
/// references across the await without cloning.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// A live delivery frame, re-exported from the relay.
///
/// Deliberately *not* redefined here: the frame is protocol, and a second
/// definition in the client is exactly the drift this crate exists to avoid.
pub use buzz::relay::delivery::Frame;

/// The cursor wire type, re-exported for callers that resume a stream.
pub use buzz::relay::session::Cursor;

/// The four operations an agent needs from a workspace.
///
/// `Send + Sync` because the agent runtime shares one transport across the frame
/// loop and every in-flight turn.
pub trait Transport: Send + Sync {
    /// Publish one signed event.
    ///
    /// Must be idempotent from the caller's point of view: the relay elects a
    /// single writer per event id, so a retry of the same bytes is absorbed as a
    /// duplicate rather than delivered twice. That is what makes a retry after a
    /// timeout safe.
    fn publish_event<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, Result<()>>;

    /// Read one page of history.
    ///
    /// Pages are bounded by the relay (byte budget and event count, whichever
    /// binds first), so a caller wanting more must follow
    /// [`QueryPage::next_cursor`].
    fn query<'a>(&'a self, request: &'a QueryRequest) -> BoxFuture<'a, Result<QueryPage>>;

    /// Open a live session for `filters` and return its handle.
    ///
    /// Cost: one authenticated round trip, plus whatever the implementation
    /// spends keeping the stream alive (the bundled HTTP transport starts one
    /// background task per session).
    fn subscribe<'a>(&'a self, filters: &'a [Filter]) -> BoxFuture<'a, Result<SessionHandle>>;

    /// Take the next frame, or `Ok(None)` when the stream has ended.
    ///
    /// Must be cancel-safe (see the module docs). Implementations are expected to
    /// call [`SessionHandle::observe`] so the handle's cursor tracks what has
    /// actually been delivered.
    fn next_frame<'a>(
        &'a self,
        session: &'a mut SessionHandle,
    ) -> BoxFuture<'a, Result<Option<Frame>>>;

    /// Cheap liveness check, called by [`crate::WorkspaceBuilder::connect`].
    ///
    /// Default: succeed without doing anything, which is right for an in-process
    /// transport. The HTTP transport overrides it with one unauthenticated
    /// `GET /buzz/api/healthz` so a typo in the URL fails at `connect()` instead
    /// of at the first mention.
    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async { Ok(()) })
    }
}

/// One historical read.
///
/// Serializes to the relay's `POST /buzz/api/query` body. `filters` is carried
/// alongside `channel`/`cursor`/`limit` and is *also* applied client-side by the
/// transport, so narrowing a query is honored whether or not the relay evaluates
/// filters itself.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryRequest {
    /// NIP-01 filters. Empty means "no narrowing".
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub filters: Vec<Filter>,
    /// The channel to page, or `None` for the community-global timeline.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub channel: Option<String>,
    /// An opaque cursor from a previous [`QueryPage::next_cursor`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<String>,
    /// How many events to ask for. The relay clamps this to its own page bound.
    pub limit: usize,
}

/// Default page size: large enough for a thread's worth of context, small enough
/// to stay well inside the relay's byte budget for ordinary messages.
pub const DEFAULT_PAGE_LIMIT: usize = 50;

impl Default for QueryRequest {
    fn default() -> Self {
        Self {
            filters: Vec::new(),
            channel: None,
            cursor: None,
            limit: DEFAULT_PAGE_LIMIT,
        }
    }
}

impl QueryRequest {
    /// An unnarrowed request for the community-global timeline.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Page one channel's timeline.
    #[must_use]
    pub fn channel(mut self, channel: impl Into<String>) -> Self {
        self.channel = Some(channel.into());
        self
    }

    /// Resume from a cursor.
    #[must_use]
    pub fn cursor(mut self, cursor: impl Into<String>) -> Self {
        self.cursor = Some(cursor.into());
        self
    }

    /// Ask for at most `limit` events.
    #[must_use]
    pub const fn limit(mut self, limit: usize) -> Self {
        self.limit = limit;
        self
    }

    /// Add one filter. Filters are OR-ed, as in NIP-01.
    #[must_use]
    pub fn filter(mut self, filter: Filter) -> Self {
        self.filters.push(filter);
        self
    }

    /// Narrow to a set of kinds, as a convenience over building a [`Filter`].
    #[must_use]
    pub fn kinds(self, kinds: Vec<Kind>) -> Self {
        self.filter(Filter {
            kinds: Some(kinds),
            ..Filter::default()
        })
    }

    /// Whether an event survives this request's filters.
    ///
    /// An empty filter list keeps everything (there is nothing to narrow by),
    /// which is not the same as NIP-01's "a subscription with no filters receives
    /// nothing" — a *query* with no filters is a plain timeline read.
    #[must_use]
    pub fn keep(&self, event: &Event) -> bool {
        self.filters.is_empty() || filters_match(&self.filters, event, self.channel.as_deref())
    }
}

/// One page of history, newest first.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryPage {
    /// The events, newest first, already permission-filtered by the relay.
    #[serde(default)]
    pub events: Vec<Event>,
    /// Cursor for the next page, or `None` at the end of the index.
    #[serde(default)]
    pub next_cursor: Option<String>,
    /// Whether the relay believes the read reached the end of the index.
    #[serde(default)]
    pub complete: bool,
}

impl QueryPage {
    /// The page's events, oldest first — the order a prompt wants.
    #[must_use]
    pub fn chronological(&self) -> Vec<Event> {
        let mut events = self.events.clone();
        events.sort_by(|a, b| {
            a.created_at
                .cmp(&b.created_at)
                // A stable tiebreak keeps two events in the same second from
                // reordering between calls, which would make prompts unstable.
                .then_with(|| a.id.cmp(&b.id))
        });
        events
    }

    /// Whether the page is empty.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

/// A live subscription's client-side state.
///
/// Holds the relay's session id and stream token, the resume cursor, and — for
/// transports that pump frames on a background task — the receiving end of that
/// pump. The pump half is private: a caller reads frames through
/// [`Transport::next_frame`] so a transport that has no background task (an
/// in-process one, say) is not forced to invent one.
#[derive(Debug)]
pub struct SessionHandle {
    /// The relay's session id, which appears in the stream path.
    pub session: String,
    /// The stream token minted by `POST /buzz/api/subscriptions`.
    pub token: String,
    /// Where to resume: `generation:offset`, or `None` to start from the feed's
    /// beginning.
    pub cursor: Option<String>,
    /// How often the relay promises an application-level heartbeat. Silence for
    /// materially longer than this means the pump died with the socket open.
    pub heartbeat_ms: u64,
    /// The filters this session was opened with, kept so a reconnect can
    /// reproduce it exactly.
    pub filters: Vec<Filter>,
    /// How many `reset` frames have arrived. A client that sees this increment
    /// has been told its cursor is unusable and must re-read history.
    pub resets: u32,
    /// How many frames of any kind have been observed.
    pub frames: u64,
    /// Frame source for transports that pump on a background task.
    inbox: Option<tokio::sync::mpsc::Receiver<Result<Frame>>>,
}

/// Sending half of a [`SessionHandle`]'s frame pump.
pub type FrameSink = tokio::sync::mpsc::Sender<Result<Frame>>;

/// How many frames a pump may run ahead of the agent before it applies
/// backpressure. Bounded on purpose: an unbounded channel turns a slow agent
/// into unbounded memory growth.
pub const FRAME_BUFFER: usize = 64;

impl SessionHandle {
    /// A handle with no frame source — for transports that produce frames
    /// on demand rather than pumping them.
    #[must_use]
    pub fn new(
        session: impl Into<String>,
        token: impl Into<String>,
        cursor: Option<String>,
        heartbeat_ms: u64,
        filters: Vec<Filter>,
    ) -> Self {
        Self {
            session: session.into(),
            token: token.into(),
            cursor,
            heartbeat_ms,
            filters,
            resets: 0,
            frames: 0,
            inbox: None,
        }
    }

    /// Attach the receiving half of a frame pump.
    #[must_use]
    pub fn with_inbox(mut self, inbox: tokio::sync::mpsc::Receiver<Result<Frame>>) -> Self {
        self.inbox = Some(inbox);
        self
    }

    /// Create a bounded pump channel and attach its receiver, handing back the
    /// sender for the pumping task.
    pub fn pump(&mut self) -> FrameSink {
        let (tx, rx) = tokio::sync::mpsc::channel(FRAME_BUFFER);
        self.inbox = Some(rx);
        tx
    }

    /// Take the next frame from the attached pump.
    ///
    /// Cancel-safe: `tokio`'s `recv` does not consume a message it has not
    /// returned, so dropping this future inside a `select!` cannot lose a frame.
    ///
    /// Returns `Ok(None)` when the pump has closed or when no pump is attached.
    ///
    /// # Errors
    ///
    /// Whatever the pump forwarded — a transport error or a framing error.
    pub async fn recv(&mut self) -> Result<Option<Frame>> {
        let Some(inbox) = self.inbox.as_mut() else {
            return Ok(None);
        };
        match inbox.recv().await {
            Some(Ok(frame)) => Ok(Some(frame)),
            Some(Err(e)) => Err(e),
            None => Ok(None),
        }
    }

    /// Record what a delivered frame implies about the session's position.
    ///
    /// * an `event` or `heartbeat` frame advances the cursor to its offset;
    /// * a `reset` frame drops the cursor (it is exactly the cursor the relay
    ///   just refused) and counts the reset, so the caller re-reads history
    ///   rather than resuming into a gap.
    ///
    /// Pure: no I/O, and the only state it touches is this handle.
    pub fn observe(&mut self, frame: &Frame) {
        self.frames = self.frames.saturating_add(1);
        match frame {
            Frame::Event { offset, .. } | Frame::Heartbeat { offset } => {
                self.cursor = Some(offset.clone());
            }
            Frame::Reset { .. } => {
                self.resets = self.resets.saturating_add(1);
                self.cursor = None;
            }
        }
    }

    /// Whether a pump is attached.
    #[must_use]
    pub const fn is_pumped(&self) -> bool {
        self.inbox.is_some()
    }

    pub(crate) async fn next_observed_frame(&mut self) -> Result<Option<Frame>> {
        let frame = self.recv().await?;
        if let Some(frame) = frame.as_ref() {
            self.observe(frame);
        }
        Ok(frame)
    }
}

/// Filters that select messages addressed to `pubkey_hex`.
///
/// A mention is a `p` tag naming the agent, which is the workspace's convention
/// and the same thing the relay indexes. `kinds` narrows to the message kinds an
/// agent should answer, so a reaction or a membership change does not wake a
/// turn.
#[must_use]
pub fn mention_filters(pubkey_hex: &str, kinds: Vec<Kind>) -> Vec<Filter> {
    let mut tags = std::collections::BTreeMap::new();
    tags.insert("#p".to_string(), vec![pubkey_hex.to_string()]);
    vec![Filter {
        kinds: Some(kinds),
        tags,
        ..Filter::default()
    }]
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use buzz::relay::crypto::to_hex;
    use buzz::relay::kind::{KIND_REACTION, KIND_STREAM_MESSAGE};

    fn event(kind: Kind, tags: Vec<Vec<String>>, content: &str, at: u64) -> Event {
        Event::sign_new(&[3u8; 32], kind, tags, content, at).unwrap()
    }

    #[test]
    fn a_query_serializes_to_the_relays_body_shape() {
        let request = QueryRequest::new().channel("general").limit(10);
        let json = serde_json::to_value(&request).unwrap();
        assert_eq!(json["channel"], "general");
        assert_eq!(json["limit"], 10);
        assert!(json.get("cursor").is_none(), "absent, not null");
        assert!(json.get("filters").is_none(), "absent when unnarrowed");
    }

    #[test]
    fn filters_narrow_a_page_client_side() {
        let request = QueryRequest::new()
            .channel("general")
            .kinds(vec![KIND_STREAM_MESSAGE]);
        let message = event(
            KIND_STREAM_MESSAGE,
            vec![vec!["h".into(), "general".into()]],
            "hi",
            10,
        );
        let reaction = event(KIND_REACTION, vec![], "+", 11);
        assert!(request.keep(&message));
        assert!(!request.keep(&reaction));

        // No filters means no narrowing — a plain timeline read.
        assert!(QueryRequest::new().keep(&reaction));
    }

    #[test]
    fn a_page_can_be_read_oldest_first_deterministically() {
        let page = QueryPage {
            events: vec![event(1, vec![], "third", 30), event(1, vec![], "first", 10)],
            next_cursor: Some("abc".into()),
            complete: false,
        };
        let ordered = page.chronological();
        assert_eq!(ordered[0].content, "first");
        assert_eq!(ordered[1].content, "third");
        assert_eq!(page.chronological(), ordered, "ordering is stable");
    }

    #[test]
    fn mention_filters_ask_for_p_tagged_messages_only() {
        let hex = to_hex(&[9u8; 32]);
        let filters = mention_filters(&hex, vec![KIND_STREAM_MESSAGE]);
        assert_eq!(
            filters[0].tag_filter("#p").unwrap(),
            core::slice::from_ref(&hex)
        );

        let mine = event(
            KIND_STREAM_MESSAGE,
            vec![
                vec!["p".into(), hex.clone()],
                vec!["h".into(), "eng".into()],
            ],
            "@bot hello",
            5,
        );
        let someone_elses = event(
            KIND_STREAM_MESSAGE,
            vec![vec!["h".into(), "eng".into()]],
            "unrelated",
            5,
        );
        assert!(filters_match(&filters, &mine, Some("eng")));
        assert!(!filters_match(&filters, &someone_elses, Some("eng")));

        // A p-tagged event of an unwanted kind must not wake the agent.
        let reaction = event(KIND_REACTION, vec![vec!["p".into(), hex]], "+", 6);
        assert!(!filters_match(&filters, &reaction, Some("eng")));
    }

    #[test]
    fn observing_frames_tracks_the_cursor_and_resets() {
        let mut handle = SessionHandle::new("sess", "tok", None, 10_000, Vec::new());
        assert!(!handle.is_pumped());

        handle.observe(&Frame::heartbeat_at(Cursor::new(0, 4)));
        assert_eq!(handle.cursor.as_deref(), Some("0:4"));

        handle.observe(&Frame::event_at(
            Cursor::new(0, 5),
            event(KIND_STREAM_MESSAGE, vec![], "hi", 1),
        ));
        assert_eq!(handle.cursor.as_deref(), Some("0:5"));
        assert_eq!(handle.frames, 2);

        handle.observe(&Frame::reset(
            buzz::relay::session::ResetReason::GenerationRotated,
            Cursor::new(0, 5),
        ));
        assert_eq!(handle.resets, 1);
        assert_eq!(
            handle.cursor, None,
            "a refused cursor must not be resumed from"
        );
    }

    #[tokio::test]
    async fn a_pumped_handle_delivers_then_ends() {
        let mut handle = SessionHandle::new("sess", "tok", None, 10_000, Vec::new());
        let sink = handle.pump();
        let frame = Frame::heartbeat_at(Cursor::new(0, 1));
        sink.send(Ok(frame.clone())).await.unwrap();
        drop(sink);

        assert_eq!(handle.recv().await.unwrap(), Some(frame));
        assert_eq!(
            handle.recv().await.unwrap(),
            None,
            "closed pump ends cleanly"
        );
    }

    #[tokio::test]
    async fn an_unpumped_handle_reports_end_of_stream() {
        let mut handle = SessionHandle::new("sess", "tok", None, 10_000, Vec::new());
        assert_eq!(handle.recv().await.unwrap(), None);
    }
}
