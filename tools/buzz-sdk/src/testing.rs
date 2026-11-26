//! In-process fakes, so an agent can be tested without a relay.
//!
//! [`MemoryTransport`] implements [`Transport`] over a `Vec` and a channel. It is
//! not a relay: it does not verify signatures, enforce access, resolve a
//! reaction's channel from its target, or bound a page by bytes. What it *does*
//! reproduce is the shape agent code depends on — publish then read back, filters
//! narrow both reads and live delivery, frames arrive with monotonic cursors —
//! which is enough to drive the mention loop, the single-flight discipline, tool
//! dispatch and memory in a unit test.
//!
//! It ships in the library rather than under `#[cfg(test)]` because an agent
//! author needs it more than the SDK does: it is how you test *your* handler.
//! It is I/O-free, so it compiles and runs with `--no-default-features`.

use std::sync::Mutex;

use buzz::relay::filter::{filters_match, Filter};
use buzz::relay::Event;

use crate::error::{Result, SdkError};
use crate::transport::{
    BoxFuture, Cursor, Frame, FrameSink, QueryPage, QueryRequest, SessionHandle, Transport,
};

struct Subscription {
    id: String,
    filters: Vec<Filter>,
    sink: FrameSink,
}

#[derive(Default)]
struct State {
    /// Every event ever seen, in arrival order — what `query` reads.
    history: Vec<Event>,
    /// Only the events published *through this transport*, i.e. what the agent
    /// itself said. The assertion surface for "did it reply?".
    published: Vec<Event>,
    subscriptions: Vec<Subscription>,
    /// Next feed offset. Monotonic, like the relay's topic offsets.
    offset: u64,
    opened: u64,
    /// Injected failure for the next publish, if any.
    fail_next_publish: Option<String>,
}

/// An in-process [`Transport`] backed by memory.
///
/// Cheap to construct, safe to share (`&self` everywhere, one mutex inside), and
/// deliberately synchronous underneath: no task is spawned, so a test controls
/// exactly when frames appear.
#[derive(Default)]
pub struct MemoryTransport {
    state: Mutex<State>,
}

impl core::fmt::Debug for MemoryTransport {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        let (history, published, subs) = match self.state.lock() {
            Ok(state) => (
                state.history.len(),
                state.published.len(),
                state.subscriptions.len(),
            ),
            Err(_) => (0, 0, 0),
        };
        f.debug_struct("MemoryTransport")
            .field("history", &history)
            .field("published", &published)
            .field("subscriptions", &subs)
            .finish()
    }
}

impl MemoryTransport {
    /// An empty workspace.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Add an event to history without delivering it to subscribers.
    ///
    /// Use this to give an agent a past to read: prior messages in a channel,
    /// prior memories, precedent for a search.
    pub fn seed(&self, event: Event) {
        if let Ok(mut state) = self.state.lock() {
            state.history.push(event);
        }
    }

    /// Deliver an event as if it had just arrived from another member.
    ///
    /// Adds it to history and pushes an `event` frame to every subscription whose
    /// filters match — the same two effects the relay's ingest has.
    ///
    /// Returns the cursor the frame was delivered at.
    pub fn deliver(&self, event: Event) -> String {
        let Ok(mut state) = self.state.lock() else {
            return String::new();
        };
        state.history.push(event.clone());
        Self::fan_out(&mut state, &event)
    }

    /// Push a frame verbatim to every subscription — including a `reset`, which
    /// is otherwise hard to provoke.
    pub fn deliver_frame(&self, frame: Frame) {
        if let Ok(state) = self.state.lock() {
            for sub in &state.subscriptions {
                let _ = sub.sink.try_send(Ok(frame.clone()));
            }
        }
    }

    /// Make the next [`Transport::publish_event`] fail, once.
    ///
    /// The agent runtime must survive a failed publish (a typing indicator that
    /// does not land is not a reason to stop answering), and this is how a test
    /// proves it.
    pub fn fail_next_publish(&self, message: impl Into<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.fail_next_publish = Some(message.into());
        }
    }

    /// Everything the agent published, in order.
    #[must_use]
    pub fn published(&self) -> Vec<Event> {
        self.state
            .lock()
            .map(|state| state.published.clone())
            .unwrap_or_default()
    }

    /// Everything the agent published of one kind, in order.
    #[must_use]
    pub fn published_kind(&self, kind: buzz::relay::Kind) -> Vec<Event> {
        self.published()
            .into_iter()
            .filter(|e| e.kind == kind)
            .collect()
    }

    /// Every event in history, in arrival order.
    #[must_use]
    pub fn history(&self) -> Vec<Event> {
        self.state
            .lock()
            .map(|state| state.history.clone())
            .unwrap_or_default()
    }

    /// How many live subscriptions exist.
    #[must_use]
    pub fn subscriptions(&self) -> usize {
        self.state
            .lock()
            .map(|state| state.subscriptions.len())
            .unwrap_or(0)
    }

    /// How many fanned-out frames are still waiting in subscription channels.
    ///
    /// Zero means every delivered frame has reached the agent's run loop. That is
    /// what makes a test of the batching path deterministic instead of a sleep:
    /// deliver, wait for this to reach zero, and the loop has definitely seen the
    /// frames.
    #[must_use]
    pub fn undelivered(&self) -> usize {
        self.state
            .lock()
            .map(|state| {
                state
                    .subscriptions
                    .iter()
                    .map(|sub| sub.sink.max_capacity().saturating_sub(sub.sink.capacity()))
                    .sum()
            })
            .unwrap_or(0)
    }

    /// The id of the nth live subscription, if it exists.
    #[must_use]
    pub fn subscription_id(&self, index: usize) -> Option<String> {
        self.state
            .lock()
            .ok()
            .and_then(|state| state.subscriptions.get(index).map(|s| s.id.clone()))
    }

    /// Fan one event out to matching subscriptions, returning its cursor.
    fn fan_out(state: &mut State, event: &Event) -> String {
        state.offset = state.offset.saturating_add(1);
        let cursor = Cursor::new(0, state.offset);
        let channel = event.channel_id().map(str::to_string);
        let frame = Frame::event_at(cursor, event.clone());
        // Drop subscriptions whose receiver is gone, the way a pump exits when
        // its client disconnects.
        state.subscriptions.retain(|sub| !sub.sink.is_closed());
        for sub in &state.subscriptions {
            let wanted =
                sub.filters.is_empty() || filters_match(&sub.filters, event, channel.as_deref());
            if wanted {
                // A full buffer drops the frame rather than blocking, which is
                // the fake's one deliberate difference from the relay: tests
                // should never deadlock on backpressure.
                let _ = sub.sink.try_send(Ok(frame.clone()));
            }
        }
        cursor.encode()
    }
}

impl Transport for MemoryTransport {
    fn publish_event<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SdkError::transport("memory transport lock poisoned"))?;
            if let Some(message) = state.fail_next_publish.take() {
                return Err(SdkError::transport(message));
            }
            // Idempotent by event id, as the relay's admission marker makes it.
            if state.published.iter().any(|e| e.id == event.id) {
                return Ok(());
            }
            state.published.push(event.clone());
            state.history.push(event.clone());
            Self::fan_out(&mut state, event);
            Ok(())
        })
    }

    fn query<'a>(&'a self, request: &'a QueryRequest) -> BoxFuture<'a, Result<QueryPage>> {
        Box::pin(async move {
            let state = self
                .state
                .lock()
                .map_err(|_| SdkError::transport("memory transport lock poisoned"))?;
            let mut events: Vec<Event> = state
                .history
                .iter()
                .filter(|event| match request.channel.as_deref() {
                    // Channel scoping mirrors the relay's index: an event with no
                    // `h` tag is global and only appears in a global read.
                    Some(channel) => event.channel_id() == Some(channel),
                    None => event.channel_id().is_none(),
                })
                .filter(|event| request.keep(event))
                .cloned()
                .collect();
            // Newest first, as the relay's inverted-timestamp index returns.
            events.sort_by(|a, b| {
                b.created_at
                    .cmp(&a.created_at)
                    .then_with(|| b.id.cmp(&a.id))
            });
            let complete = events.len() <= request.limit;
            events.truncate(request.limit);
            Ok(QueryPage {
                events,
                next_cursor: None,
                complete,
            })
        })
    }

    fn subscribe<'a>(&'a self, filters: &'a [Filter]) -> BoxFuture<'a, Result<SessionHandle>> {
        Box::pin(async move {
            let mut state = self
                .state
                .lock()
                .map_err(|_| SdkError::transport("memory transport lock poisoned"))?;
            state.opened = state.opened.saturating_add(1);
            let id = format!("memory-session-{}", state.opened);
            let mut handle = SessionHandle::new(
                id.clone(),
                format!("token-{}", state.opened),
                None,
                10_000,
                filters.to_vec(),
            );
            let sink = handle.pump();
            state.subscriptions.push(Subscription {
                id,
                filters: filters.to_vec(),
                sink,
            });
            Ok(handle)
        })
    }

    fn next_frame<'a>(
        &'a self,
        session: &'a mut SessionHandle,
    ) -> BoxFuture<'a, Result<Option<Frame>>> {
        Box::pin(session.next_observed_frame())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use buzz::relay::kind::KIND_STREAM_MESSAGE;

    fn message(channel: Option<&str>, content: &str, at: u64) -> Event {
        let tags = channel
            .map(|c| vec![vec!["h".to_string(), c.to_string()]])
            .unwrap_or_default();
        Event::sign_new(&[5u8; 32], KIND_STREAM_MESSAGE, tags, content, at).unwrap()
    }

    #[tokio::test]
    async fn publishing_records_and_fans_out() {
        let transport = MemoryTransport::new();
        let mut session = transport.subscribe(&[]).await.unwrap();

        let event = message(Some("general"), "hello", 10);
        transport.publish_event(&event).await.unwrap();

        assert_eq!(transport.published().len(), 1);
        let frame = transport.next_frame(&mut session).await.unwrap().unwrap();
        assert_eq!(frame, Frame::event_at(Cursor::new(0, 1), event));
        assert_eq!(session.cursor.as_deref(), Some("0:1"));
    }

    #[tokio::test]
    async fn republishing_the_same_event_is_absorbed() {
        let transport = MemoryTransport::new();
        let event = message(Some("general"), "hello", 10);
        transport.publish_event(&event).await.unwrap();
        transport.publish_event(&event).await.unwrap();
        assert_eq!(transport.published().len(), 1);
    }

    #[tokio::test]
    async fn filters_gate_live_delivery() {
        let transport = MemoryTransport::new();
        let filters = crate::transport::mention_filters(
            &buzz::relay::crypto::to_hex(&[1u8; 32]),
            vec![KIND_STREAM_MESSAGE],
        );
        let mut session = transport.subscribe(&filters).await.unwrap();

        transport.deliver(message(Some("general"), "not for you", 10));
        let addressed = Event::sign_new(
            &[6u8; 32],
            KIND_STREAM_MESSAGE,
            vec![
                vec!["h".into(), "general".into()],
                vec!["p".into(), buzz::relay::crypto::to_hex(&[1u8; 32])],
            ],
            "for you",
            11,
        )
        .unwrap();
        transport.deliver(addressed.clone());

        let frame = transport.next_frame(&mut session).await.unwrap().unwrap();
        let Frame::Event { event, .. } = frame else {
            panic!("expected an event frame");
        };
        assert_eq!(event.id, addressed.id, "only the addressed event arrives");
    }

    #[tokio::test]
    async fn queries_are_channel_scoped_and_newest_first() {
        let transport = MemoryTransport::new();
        transport.seed(message(Some("general"), "older", 10));
        transport.seed(message(Some("general"), "newer", 20));
        transport.seed(message(Some("other"), "elsewhere", 30));
        transport.seed(message(None, "global", 40));

        let page = transport
            .query(&QueryRequest::new().channel("general").limit(10))
            .await
            .unwrap();
        assert_eq!(page.events.len(), 2);
        assert_eq!(page.events[0].content, "newer");
        assert!(page.complete);
        assert_eq!(page.chronological()[0].content, "older");

        let global = transport
            .query(&QueryRequest::new().limit(10))
            .await
            .unwrap();
        assert_eq!(global.events.len(), 1);
        assert_eq!(global.events[0].content, "global");
    }

    #[tokio::test]
    async fn a_page_reports_incompleteness_when_it_truncates() {
        let transport = MemoryTransport::new();
        for i in 0..5u64 {
            transport.seed(message(Some("general"), "m", 10 + i));
        }
        let page = transport
            .query(&QueryRequest::new().channel("general").limit(2))
            .await
            .unwrap();
        assert_eq!(page.events.len(), 2);
        assert!(!page.complete);
    }

    #[tokio::test]
    async fn injected_publish_failures_surface_once() {
        let transport = MemoryTransport::new();
        transport.fail_next_publish("relay unreachable");
        let event = message(Some("general"), "hi", 1);
        assert!(transport.publish_event(&event).await.is_err());
        assert!(transport.publish_event(&event).await.is_ok());
    }

    #[tokio::test]
    async fn resets_can_be_injected() {
        let transport = MemoryTransport::new();
        let mut session = transport.subscribe(&[]).await.unwrap();
        transport.deliver_frame(Frame::reset(
            buzz::relay::session::ResetReason::FeedRetired,
            Cursor::new(0, 3),
        ));
        let frame = transport.next_frame(&mut session).await.unwrap().unwrap();
        assert!(matches!(frame, Frame::Reset { .. }));
        assert_eq!(session.resets, 1);
    }

    #[tokio::test]
    async fn dropping_a_session_retires_its_subscription() {
        let transport = MemoryTransport::new();
        let session = transport.subscribe(&[]).await.unwrap();
        assert_eq!(transport.subscriptions(), 1);
        assert!(transport.subscription_id(0).is_some());
        drop(session);
        // The retire happens on the next fan-out, as a pump notices its client
        // only when it tries to write.
        transport.deliver(message(Some("general"), "hi", 1));
        transport.deliver(message(Some("general"), "hi again", 2));
        assert_eq!(transport.subscriptions(), 0);
    }
}
