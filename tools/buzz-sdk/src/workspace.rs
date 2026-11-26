//! The workspace: an identity plus a transport, and the small vocabulary an agent
//! needs on top of them.
//!
//! A [`Workspace`] is what `Workspace::builder(url).identity(id).connect()`
//! returns, and it is deliberately thin — sign, publish, read, subscribe. It owns
//! no agent logic and no model access, so the same workspace can back an agent, a
//! CLI, or a test.
//!
//! Cloning is an `Arc` bump: the agent runtime hands a clone to every turn, and
//! they all share one transport and one connection pool.
//!
//! ## Time
//!
//! Every event carries an author-stated `created_at` inside its signature, so the
//! SDK reads a clock in exactly one place: the [`Clock`] this workspace holds.
//! That indirection is what makes an agent testable — a [`FixedClock`] produces
//! byte-identical events on every run — and it is why no other module in the SDK
//! calls `SystemTime::now`.

use std::sync::Arc;

use buzz::relay::filter::Filter;
use buzz::relay::kind::{Kind, KIND_STREAM_MESSAGE};
use buzz::relay::{Event, PubKey};

use crate::error::{Result, SdkError};
use crate::identity::Identity;
use crate::transport::{Frame, QueryPage, QueryRequest, SessionHandle, Transport};

/// A source of wall-clock time, in Unix milliseconds.
///
/// `Send + Sync` because a workspace is shared across tasks.
pub trait Clock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;

    /// Seconds since the Unix epoch — what an event's `created_at` wants.
    fn now_secs(&self) -> u64 {
        self.now_ms() / 1_000
    }
}

/// The host clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            // A clock before 1970 is a misconfigured host, not a reason to
            // panic: treat it as the epoch and let the relay's drift check
            // refuse the events.
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// A clock that does not move, for tests.
///
/// Deterministic events mean a test can assert on an event id.
#[derive(Debug, Clone)]
pub struct FixedClock {
    now_ms: Arc<std::sync::atomic::AtomicU64>,
}

impl FixedClock {
    /// A clock stopped at `now_ms`.
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self {
            now_ms: Arc::new(std::sync::atomic::AtomicU64::new(now_ms)),
        }
    }

    /// Move the clock forward.
    pub fn advance_ms(&self, delta: u64) {
        self.now_ms
            .fetch_add(delta, std::sync::atomic::Ordering::SeqCst);
    }

    /// Set the clock.
    pub fn set_ms(&self, now_ms: u64) {
        self.now_ms
            .store(now_ms, std::sync::atomic::Ordering::SeqCst);
    }
}

impl Clock for FixedClock {
    fn now_ms(&self) -> u64 {
        self.now_ms.load(std::sync::atomic::Ordering::SeqCst)
    }
}

/// Shared workspace state.
struct Inner {
    transport: Arc<dyn Transport>,
    identity: Identity,
    clock: Arc<dyn Clock>,
    url: String,
    default_channel: Option<String>,
}

/// A connected workspace.
#[derive(Clone)]
pub struct Workspace {
    inner: Arc<Inner>,
}

impl core::fmt::Debug for Workspace {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Workspace")
            .field("url", &self.inner.url)
            .field("as", &self.inner.identity.npub())
            .field("default_channel", &self.inner.default_channel)
            .finish_non_exhaustive()
    }
}

impl Workspace {
    /// Start building a workspace for a relay base URL.
    ///
    /// The URL is the origin the relay is served from — `https://buzz.example.com`
    /// — not a path inside it.
    #[must_use]
    pub fn builder(url: impl Into<String>) -> WorkspaceBuilder {
        WorkspaceBuilder::new(url)
    }

    /// Build a workspace over a transport you supply, skipping the builder.
    ///
    /// This is the constructor an in-process host uses (a constrained app that
    /// already holds queue handles) and the one tests use with
    /// [`crate::testing::MemoryTransport`]. No probe is performed.
    #[must_use]
    pub fn with_transport(transport: Arc<dyn Transport>, identity: Identity) -> Self {
        Self {
            inner: Arc::new(Inner {
                transport,
                identity,
                clock: Arc::new(SystemClock),
                url: String::new(),
                default_channel: None,
            }),
        }
    }

    /// The agent's own identity.
    #[must_use]
    pub fn identity(&self) -> &Identity {
        &self.inner.identity
    }

    /// The transport underneath, for callers that need an operation this type
    /// does not wrap.
    #[must_use]
    pub fn transport(&self) -> &Arc<dyn Transport> {
        &self.inner.transport
    }

    /// The relay base URL, or `""` for an in-process transport.
    #[must_use]
    pub fn url(&self) -> &str {
        &self.inner.url
    }

    /// Who this workspace acts as, as an `npub`.
    #[must_use]
    pub fn whoami(&self) -> &str {
        self.inner.identity.npub()
    }

    /// The agent's public key.
    #[must_use]
    pub fn pubkey(&self) -> PubKey {
        self.inner.identity.pubkey()
    }

    /// The channel used when an operation needs one and the caller did not name
    /// one (scheduled turns, most notably).
    #[must_use]
    pub fn default_channel(&self) -> Option<&str> {
        self.inner.default_channel.as_deref()
    }

    /// Current time in Unix milliseconds, from this workspace's clock.
    #[must_use]
    pub fn now_ms(&self) -> u64 {
        self.inner.clock.now_ms()
    }

    /// Current time in Unix seconds, from this workspace's clock.
    #[must_use]
    pub fn now_secs(&self) -> u64 {
        self.inner.clock.now_secs()
    }

    /// Sign an event as this workspace's identity, stamped with its clock.
    ///
    /// # Errors
    ///
    /// [`SdkError::Protocol`] if the event would exceed the relay's size bound.
    pub fn sign(&self, kind: Kind, tags: Vec<Vec<String>>, content: &str) -> Result<Event> {
        self.inner
            .identity
            .sign(kind, tags, content, self.now_secs())
    }

    /// Publish an already-signed event.
    ///
    /// # Errors
    ///
    /// Whatever the transport reports; [`SdkError::Protocol`] if the relay
    /// refused the event.
    pub async fn publish(&self, event: &Event) -> Result<()> {
        self.inner.transport.publish_event(event).await
    }

    /// Post a message into a channel.
    ///
    /// Builds a kind-9 stream message with the channel's `h` tag, signs it, and
    /// publishes it. Returns the event so the caller knows its id — an agent that
    /// cites what it said needs that.
    ///
    /// # Errors
    ///
    /// As [`Workspace::sign`] and [`Workspace::publish`].
    pub async fn post(&self, channel: &str, text: &str) -> Result<Event> {
        self.post_with_tags(channel, text, Vec::new()).await
    }

    /// Post a message into a channel with extra tags (thread markers, mentions).
    ///
    /// The `h` tag is added for you and placed first; a caller-supplied `h` tag is
    /// dropped, because two `h` tags on one event is a scope ambiguity the relay
    /// resolves by taking the first and a client should not create.
    ///
    /// # Errors
    ///
    /// As [`Workspace::sign`] and [`Workspace::publish`].
    pub async fn post_with_tags(
        &self,
        channel: &str,
        text: &str,
        tags: Vec<Vec<String>>,
    ) -> Result<Event> {
        if channel.trim().is_empty() {
            return Err(SdkError::config("cannot post without a channel"));
        }
        let mut all = Vec::with_capacity(tags.len() + 1);
        all.push(vec!["h".to_string(), channel.to_string()]);
        all.extend(
            tags.into_iter()
                .filter(|tag| tag.first().map(String::as_str) != Some("h")),
        );
        let event = self.sign(KIND_STREAM_MESSAGE, all, text)?;
        self.publish(&event).await?;
        Ok(event)
    }

    /// Read one page of history.
    ///
    /// # Errors
    ///
    /// Whatever the transport reports.
    pub async fn query(&self, request: &QueryRequest) -> Result<QueryPage> {
        self.inner.transport.query(request).await
    }

    /// Read a channel's most recent events, oldest first.
    ///
    /// `channel = None` reads the community-global timeline. One page: for more,
    /// use [`Workspace::query`] and follow the cursor.
    ///
    /// # Errors
    ///
    /// Whatever the transport reports.
    pub async fn timeline(&self, channel: Option<&str>, limit: usize) -> Result<Vec<Event>> {
        let mut request = QueryRequest::new().limit(limit);
        if let Some(channel) = channel {
            request = request.channel(channel);
        }
        Ok(self.query(&request).await?.chronological())
    }

    /// Open a live session.
    ///
    /// An empty filter list subscribes to *nothing* (NIP-01 semantics), which is
    /// almost never what a caller means — pass at least one filter.
    ///
    /// # Errors
    ///
    /// Whatever the transport reports.
    pub async fn subscribe(&self, filters: Vec<Filter>) -> Result<SessionHandle> {
        self.inner.transport.subscribe(&filters).await
    }

    /// Take the next frame from a live session.
    ///
    /// # Errors
    ///
    /// Whatever the transport reports.
    pub async fn next_frame(&self, session: &mut SessionHandle) -> Result<Option<Frame>> {
        self.inner.transport.next_frame(session).await
    }
}

/// Builder for [`Workspace`].
///
/// Every method is chainable and total: nothing validates until
/// [`WorkspaceBuilder::connect`], so a caller can build the configuration in any
/// order.
#[derive(Default)]
pub struct WorkspaceBuilder {
    url: String,
    identity: Option<Identity>,
    transport: Option<Arc<dyn Transport>>,
    clock: Option<Arc<dyn Clock>>,
    default_channel: Option<String>,
}

impl core::fmt::Debug for WorkspaceBuilder {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("WorkspaceBuilder")
            .field("url", &self.url)
            .field("identity", &self.identity.is_some())
            .field("transport", &self.transport.is_some())
            .finish_non_exhaustive()
    }
}

impl WorkspaceBuilder {
    /// A builder for a relay base URL.
    #[must_use]
    pub fn new(url: impl Into<String>) -> Self {
        Self {
            url: url.into().trim_end_matches('/').to_string(),
            ..Self::default()
        }
    }

    /// The identity the agent acts as. Required.
    #[must_use]
    pub fn identity(mut self, identity: Identity) -> Self {
        self.identity = Some(identity);
        self
    }

    /// Use a transport of your own instead of the bundled HTTP one.
    ///
    /// This is how the same agent runs inside a constrained app: supply a
    /// transport built from the app's injected handles and nothing above this
    /// line changes.
    #[must_use]
    pub fn transport(mut self, transport: Arc<dyn Transport>) -> Self {
        self.transport = Some(transport);
        self
    }

    /// Use a clock of your own — [`FixedClock`] in tests.
    #[must_use]
    pub fn clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// The channel to use when an operation needs one and none was named.
    #[must_use]
    pub fn default_channel(mut self, channel: impl Into<String>) -> Self {
        self.default_channel = Some(channel.into());
        self
    }

    /// Finish the workspace.
    ///
    /// With the `http` feature and no explicit transport, this builds an
    /// [`crate::HttpTransport`] for the URL. Then it calls
    /// [`Transport::probe`] — for the HTTP transport, one unauthenticated
    /// `GET /buzz/api/healthz` — so a wrong URL or a relay that is down fails
    /// here, at the line the developer wrote `connect()`, instead of at the first
    /// mention hours later.
    ///
    /// Cost: one round trip (none for an in-process transport).
    ///
    /// # Errors
    ///
    /// * [`SdkError::Config`] if no identity was supplied, if the URL is empty
    ///   while the transport must be built from it, or if the crate was compiled
    ///   without `http` and no transport was supplied.
    /// * Whatever [`Transport::probe`] reports.
    pub async fn connect(self) -> Result<Workspace> {
        let identity = self
            .identity
            .ok_or_else(|| SdkError::config("a workspace needs an identity: .identity(...)"))?;

        let transport: Arc<dyn Transport> = match self.transport {
            Some(transport) => transport,
            None => {
                #[cfg(feature = "http")]
                {
                    if self.url.is_empty() {
                        return Err(SdkError::config(
                            "a workspace needs a relay URL, or an explicit .transport(...)",
                        ));
                    }
                    Arc::new(crate::http::HttpTransport::new(
                        self.url.clone(),
                        identity.clone(),
                    )?)
                }
                #[cfg(not(feature = "http"))]
                {
                    return Err(SdkError::config(
                        "this build has no bundled transport (the `http` feature is off); supply one with .transport(...)",
                    ));
                }
            }
        };

        transport.probe().await?;

        Ok(Workspace {
            inner: Arc::new(Inner {
                transport,
                identity,
                clock: self.clock.unwrap_or_else(|| Arc::new(SystemClock)),
                url: self.url,
                default_channel: self.default_channel,
            }),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::testing::MemoryTransport;

    const SK: [u8; 32] = [
        0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e, 0x8f,
        0x90, 0xa1, 0xb2, 0xc3, 0xd4, 0xe5, 0xf6, 0x07, 0x18, 0x29, 0x3a, 0x4b, 0x5c, 0x6d, 0x7e,
        0x8f, 0x91,
    ];

    fn workspace(transport: Arc<MemoryTransport>) -> Workspace {
        Workspace {
            inner: Arc::new(Inner {
                transport,
                identity: Identity::from_secret(SK).unwrap(),
                clock: Arc::new(FixedClock::new(1_700_000_000_000)),
                url: "https://buzz.test".into(),
                default_channel: Some("general".into()),
            }),
        }
    }

    #[tokio::test]
    async fn posting_builds_a_signed_channel_message() {
        let transport = Arc::new(MemoryTransport::new());
        let ws = workspace(Arc::clone(&transport));

        let event = ws.post("general", "hello team").await.unwrap();
        event.verify().unwrap();
        assert_eq!(event.kind, KIND_STREAM_MESSAGE);
        assert_eq!(event.channel_id(), Some("general"));
        assert_eq!(event.content, "hello team");
        assert_eq!(
            event.created_at, 1_700_000_000,
            "the workspace clock stamped it"
        );
        assert_eq!(event.pubkey, ws.pubkey());

        let published = transport.published();
        assert_eq!(published.len(), 1);
        assert_eq!(published[0].id, event.id);
    }

    #[tokio::test]
    async fn extra_tags_survive_but_a_second_h_tag_does_not() {
        let transport = Arc::new(MemoryTransport::new());
        let ws = workspace(transport);
        let event = ws
            .post_with_tags(
                "general",
                "in-thread",
                vec![
                    vec!["e".into(), "abc".into(), String::new(), "root".into()],
                    vec!["h".into(), "smuggled".into()],
                ],
            )
            .await
            .unwrap();
        assert_eq!(event.channel_id(), Some("general"));
        assert_eq!(event.tags[0][0], "h");
        assert_eq!(event.tags[1][0], "e");
        assert_eq!(
            event.tags.iter().filter(|t| t[0] == "h").count(),
            1,
            "exactly one channel scope"
        );
    }

    #[tokio::test]
    async fn posting_without_a_channel_is_refused_before_signing() {
        let transport = Arc::new(MemoryTransport::new());
        let ws = workspace(Arc::clone(&transport));
        assert!(ws.post("  ", "orphan").await.is_err());
        assert!(transport.published().is_empty());
    }

    #[tokio::test]
    async fn timelines_come_back_oldest_first() {
        let transport = Arc::new(MemoryTransport::new());
        let ws = workspace(Arc::clone(&transport));
        for (i, text) in ["one", "two", "three"].iter().enumerate() {
            let event = Event::sign_new(
                &[4u8; 32],
                KIND_STREAM_MESSAGE,
                vec![vec!["h".into(), "general".into()]],
                text,
                100 + i as u64,
            )
            .unwrap();
            transport.seed(event);
        }
        let history = ws.timeline(Some("general"), 10).await.unwrap();
        assert_eq!(
            history
                .iter()
                .map(|e| e.content.as_str())
                .collect::<Vec<_>>(),
            ["one", "two", "three"]
        );
    }

    #[test]
    fn whoami_is_the_npub_of_the_signing_key() {
        let ws = workspace(Arc::new(MemoryTransport::new()));
        assert_eq!(
            ws.whoami(),
            buzz::relay::crypto::bech32::npub(&ws.pubkey()).unwrap()
        );
        assert!(ws.whoami().starts_with("npub1"));
    }

    #[tokio::test]
    async fn connect_requires_an_identity() {
        let err = Workspace::builder("https://buzz.test")
            .transport(Arc::new(MemoryTransport::new()))
            .connect()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("identity"), "{err}");
    }

    #[cfg(not(feature = "http"))]
    #[tokio::test]
    async fn without_the_http_feature_a_transport_must_be_supplied() {
        let err = Workspace::builder("https://buzz.test")
            .identity(Identity::from_secret(SK).unwrap())
            .connect()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("http"), "{err}");
    }

    #[test]
    fn the_system_clock_is_sane() {
        // Not a precise assertion — just that it is after this code was written
        // and in milliseconds rather than seconds.
        let now = SystemClock.now_ms();
        assert!(now > 1_700_000_000_000, "{now}");
        assert_eq!(SystemClock.now_secs(), now / 1000);
    }
}
