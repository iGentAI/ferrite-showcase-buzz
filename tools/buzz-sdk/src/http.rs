//! The bundled HTTP transport: NIP-98 signed requests and an SSE tail.
//!
//! This is the only module that opens a socket to the workspace, and it is
//! feature-gated (`http`, on by default) so the agent runtime can also be driven
//! from inside a constrained app where no HTTP client exists.
//!
//! ## Every request authenticates itself
//!
//! There is no login and no session cookie. Each call signs a fresh kind-27235
//! authorization event naming the method and the full URL, base64s it, and sends
//! it as `X-Buzz-Authorization: Nostr …` (NIP-98). Ferrite managed auth owns
//! the standard `Authorization` header; the app manifest forwards this scoped
//! inner credential to the relay. The relay verifies the signature,
//! checks the method/path binding and a ±60 s freshness window, and burns the
//! event id so the same header cannot be replayed.
//!
//! That costs one signature per request — deliberately. It is the only shape that
//! works when consecutive requests land on different instances, and it means a
//! captured header cannot be used for anything but the exact call it was minted
//! for.
//!
//! ## The stream, and why the pump starts late
//!
//! `POST /buzz/api/subscriptions` mints a session and a stream token;
//! `GET /buzz/sub/{session}?token=…&from=…` is the SSE tail. The tail is what
//! resumes from a cursor, so [`HttpTransport::subscribe`] does **not** start it:
//! it returns the handle, and the pump starts on the first
//! [`Transport::next_frame`]. That is what lets the agent runtime restore a saved
//! cursor onto the handle between the two calls without the [`Transport`] trait
//! having to know about cursors.
//!
//! One background task per session reads bytes, frames them with
//! [`crate::sse::SseDecoder`], and forwards frames over a bounded channel
//! ([`crate::transport::FRAME_BUFFER`]) — so a slow agent applies backpressure to
//! the socket instead of growing the process.
//!
//! ## `reset`
//!
//! A `reset` frame means the relay refused the cursor. The transport surfaces it
//! and clears the handle's cursor ([`crate::SessionHandle::observe`]); re-reading
//! what was missed is the agent runtime's job, and it does it through the query
//! path before re-subscribing.

use std::time::Duration;

use futures_util::StreamExt;
use serde_json::Value;

use buzz::relay::filter::Filter;
use buzz::relay::Event;

use crate::error::{Result, SdkError};
use crate::identity::Identity;
use crate::sse::SseDecoder;
use crate::transport::{
    BoxFuture, Frame, FrameSink, QueryPage, QueryRequest, SessionHandle, Transport,
};

const EVENTS_PATH: &str = "/buzz/api/events";
const QUERY_PATH: &str = "/buzz/api/query";
const SUBSCRIPTIONS_PATH: &str = "/buzz/api/subscriptions";
const HEALTH_PATH: &str = "/buzz/api/healthz";
/// App-scoped credential header forwarded through Ferrite managed auth.
const AUTHORIZATION_HEADER: &str = "x-buzz-authorization";
const STREAM_PREFIX: &str = "/buzz/sub/";

/// Default per-request timeout for the API routes.
///
/// Not applied to the stream, which is long-lived by design.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(30);

/// A [`Transport`] over the relay's HTTP surface.
#[derive(Debug, Clone)]
pub struct HttpTransport {
    client: reqwest::Client,
    base: String,
    identity: Identity,
    timeout: Duration,
}

impl HttpTransport {
    /// Build a transport for a relay origin.
    ///
    /// `base` is an origin (`https://buzz.example.com`), with or without a
    /// trailing slash. A trailing path is kept, so a relay served under a prefix
    /// works too.
    ///
    /// # Errors
    ///
    /// [`SdkError::Config`] if `base` is empty or does not start with `http://`
    /// or `https://` — failing here beats a confusing 404 later.
    pub fn new(base: impl Into<String>, identity: Identity) -> Result<Self> {
        let base = base.into().trim().trim_end_matches('/').to_string();
        if base.is_empty() {
            return Err(SdkError::config("a relay URL is required"));
        }
        if !base.starts_with("http://") && !base.starts_with("https://") {
            return Err(SdkError::config(format!(
                "relay URL must start with http:// or https://, got {base:?}"
            )));
        }
        Ok(Self {
            client: reqwest::Client::new(),
            base,
            identity,
            timeout: DEFAULT_TIMEOUT,
        })
    }

    /// Reuse a caller's `reqwest` client — its connection pool, proxy and TLS
    /// configuration.
    ///
    /// The client must **not** carry a global request timeout: the SSE tail is a
    /// long-lived response and a client-level timeout would sever it. Per-request
    /// timeouts are applied to the API routes by this transport.
    #[must_use]
    pub fn with_client(mut self, client: reqwest::Client) -> Self {
        self.client = client;
        self
    }

    /// Per-request timeout for the API routes. Default [`DEFAULT_TIMEOUT`].
    #[must_use]
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// The relay origin.
    #[must_use]
    pub fn base(&self) -> &str {
        &self.base
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// Current time in Unix seconds.
    ///
    /// The transport reads the host clock rather than the workspace's, because a
    /// NIP-98 event must be fresh against the *relay's* clock: a test clock frozen
    /// in the past would mint headers the relay refuses.
    fn now_secs() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_secs())
    }

    /// POST a body to an authenticated route and return the decoded JSON.
    async fn post_json(&self, path: &str, body: Vec<u8>) -> Result<Value> {
        let url = self.url(path);
        let authorization = self
            .identity
            .authorization_header("POST", &url, Self::now_secs())?;
        let response = self
            .client
            .post(&url)
            .header(AUTHORIZATION_HEADER, authorization)
            .header("content-type", "application/json")
            .timeout(self.timeout)
            .body(body)
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            // The relay's error body carries its own reason token
            // (`auth-required:`, `invalid:`, `rate-limited:` …); keep it verbatim
            // so a caller can branch on the protocol's vocabulary.
            return Err(SdkError::http(status.as_u16(), text));
        }
        let value: Value = serde_json::from_str(&text)?;
        if value.get("ok").and_then(Value::as_bool) == Some(false) {
            let reason = value
                .get("error")
                .and_then(Value::as_str)
                .unwrap_or("the relay refused the request");
            return Err(SdkError::http(status.as_u16(), reason));
        }
        Ok(value)
    }

    /// Start the SSE pump for a session.
    ///
    /// Called on the first [`Transport::next_frame`], so a resumed cursor set on
    /// the handle after `subscribe` is honored.
    fn start_pump(&self, session: &mut SessionHandle) -> Result<()> {
        let mut url = format!(
            "{}{STREAM_PREFIX}{}?token={}",
            self.base,
            safe_component(&session.session, "session id")?,
            safe_component(&session.token, "stream token")?
        );
        if let Some(cursor) = session.cursor.as_deref() {
            url.push_str("&from=");
            url.push_str(safe_component(cursor, "cursor")?);
        }
        let sink = session.pump();
        let client = self.client.clone();
        tokio::spawn(async move {
            let outcome = pump(client, url, &sink).await;
            if let Err(e) = outcome {
                // Best effort: if the agent has already gone away there is nobody
                // to tell, and the channel closing is the signal it needs.
                let _ = sink.send(Err(e)).await;
            }
        });
        Ok(())
    }
}

/// Read one SSE stream to its end, forwarding frames.
///
/// Returns `Err` on a transport or framing failure; `Ok(())` when the stream ended
/// cleanly or the agent dropped its receiver.
async fn pump(client: reqwest::Client, url: String, sink: &FrameSink) -> Result<()> {
    let response = client
        .get(&url)
        .header("accept", "text/event-stream")
        .send()
        .await?;
    let status = response.status();
    if !status.is_success() {
        let body = response.text().await.unwrap_or_default();
        return Err(SdkError::http(status.as_u16(), body));
    }

    let mut decoder = SseDecoder::new();
    let mut bytes = response.bytes_stream();
    while let Some(chunk) = bytes.next().await {
        let chunk = chunk?;
        for event in decoder.push(&chunk)? {
            if let Some(frame) = event.to_frame()? {
                if sink.send(Ok(frame)).await.is_err() {
                    // The agent stopped reading: end the pump rather than hold the
                    // socket open forever.
                    return Ok(());
                }
            }
        }
    }
    Ok(())
}

/// Validate a value destined for a query-string component.
///
/// The relay reads query parameters **without** percent-decoding
/// ([`buzz::relay::ingress::query_param`]), so a value must be safe to send
/// literally: percent-encoding a cursor's `:` would arrive as the three characters
/// `%3A` and fail to parse. Rather than encode, refuse anything outside the
/// characters these values are actually made of.
fn safe_component<'a>(value: &'a str, what: &str) -> Result<&'a str> {
    let ok = !value.is_empty()
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | '~' | ':'));
    if ok {
        Ok(value)
    } else {
        Err(SdkError::transport(format!(
            "{what} contains characters that cannot ride a query string literally"
        )))
    }
}

impl Transport for HttpTransport {
    fn publish_event<'a>(&'a self, event: &'a Event) -> BoxFuture<'a, Result<()>> {
        Box::pin(async move {
            let body = serde_json::to_vec(event)?;
            self.post_json(EVENTS_PATH, body).await?;
            Ok(())
        })
    }

    fn query<'a>(&'a self, request: &'a QueryRequest) -> BoxFuture<'a, Result<QueryPage>> {
        Box::pin(async move {
            let body = serde_json::to_vec(request)?;
            let value = self.post_json(QUERY_PATH, body).await?;
            let mut page: QueryPage = serde_json::from_value(value)?;
            // The relay's read route pages a timeline; the request's filters are
            // applied here so `query` honors them whatever the relay evaluated.
            page.events.retain(|event| request.keep(event));
            Ok(page)
        })
    }

    fn subscribe<'a>(&'a self, filters: &'a [Filter]) -> BoxFuture<'a, Result<SessionHandle>> {
        Box::pin(async move {
            let body = serde_json::to_vec(filters)?;
            let value = self.post_json(SUBSCRIPTIONS_PATH, body).await?;
            let session = value
                .get("session")
                .and_then(Value::as_str)
                .ok_or_else(|| SdkError::encoding("subscription reply has no session id"))?;
            let token = value
                .get("token")
                .and_then(Value::as_str)
                .ok_or_else(|| SdkError::encoding("subscription reply has no stream token"))?;
            let heartbeat_ms = value
                .get("heartbeat_ms")
                .and_then(Value::as_u64)
                .unwrap_or(10_000);
            Ok(SessionHandle::new(
                session,
                token,
                // The relay's starting cursor. The agent overwrites this with a
                // stored one before the first frame if it has one.
                value
                    .get("cursor")
                    .and_then(Value::as_str)
                    .map(str::to_string),
                heartbeat_ms,
                filters.to_vec(),
            ))
        })
    }

    fn next_frame<'a>(
        &'a self,
        session: &'a mut SessionHandle,
    ) -> BoxFuture<'a, Result<Option<Frame>>> {
        Box::pin(async move {
            if !session.is_pumped() {
                self.start_pump(session)?;
            }
            session.next_observed_frame().await
        })
    }

    fn probe(&self) -> BoxFuture<'_, Result<()>> {
        Box::pin(async move {
            let response = self
                .client
                .get(self.url(HEALTH_PATH))
                .timeout(self.timeout)
                .send()
                .await?;
            let status = response.status();
            if status.is_success() {
                Ok(())
            } else {
                Err(SdkError::http(
                    status.as_u16(),
                    response.text().await.unwrap_or_default(),
                ))
            }
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    const SK: [u8; 32] = [
        0x21, 0x43, 0x65, 0x87, 0xa9, 0xcb, 0xed, 0x0f, 0x21, 0x43, 0x65, 0x87, 0xa9, 0xcb, 0xed,
        0x0f, 0x21, 0x43, 0x65, 0x87, 0xa9, 0xcb, 0xed, 0x0f, 0x21, 0x43, 0x65, 0x87, 0xa9, 0xcb,
        0xed, 0x11,
    ];

    fn transport() -> HttpTransport {
        HttpTransport::new(
            "https://buzz.example.com/",
            Identity::from_secret(SK).unwrap(),
        )
        .unwrap()
    }

    #[test]
    fn urls_are_built_from_the_origin_without_a_double_slash() {
        let t = transport();
        assert_eq!(t.base(), "https://buzz.example.com");
        assert_eq!(
            t.url(EVENTS_PATH),
            "https://buzz.example.com/buzz/api/events"
        );
    }

    #[test]
    fn a_url_without_a_scheme_is_refused_at_construction() {
        let identity = Identity::from_secret(SK).unwrap();
        assert!(HttpTransport::new("buzz.example.com", identity.clone()).is_err());
        assert!(HttpTransport::new("", identity.clone()).is_err());
        assert!(HttpTransport::new("http://localhost:8787", identity).is_ok());
    }

    /// The header this transport puts on a publish must satisfy the relay's own
    /// verifier for that exact method and path.
    #[test]
    fn publish_headers_bind_to_the_events_route() {
        let t = transport();
        let now = HttpTransport::now_secs();
        let header = t
            .identity
            .authorization_header("POST", &t.url(EVENTS_PATH), now)
            .unwrap();
        let (caller, _event) =
            buzz::relay::auth::verify_header(&header, "POST", EVENTS_PATH, now * 1_000).unwrap();
        assert_eq!(caller.pubkey, t.identity.pubkey());
        // The same header must not open the query route.
        assert!(
            buzz::relay::auth::verify_header(&header, "POST", QUERY_PATH, now * 1_000).is_err()
        );
    }

    #[test]
    fn query_components_are_validated_rather_than_encoded() {
        // What the relay actually mints: `{expiry}.{hex}` tokens and `gen:offset`
        // cursors — both safe to send literally.
        assert!(safe_component("1700000000.deadbeef", "token").is_ok());
        assert!(safe_component("2:1234", "cursor").is_ok());
        assert!(safe_component("memory-session-1", "session id").is_ok());

        // Anything that would break the relay's parameter split is refused.
        for bad in ["", "a b", "a&b=c", "a?b", "a#b", "a%3Ab", "a=b"] {
            assert!(safe_component(bad, "token").is_err(), "accepted {bad:?}");
        }
    }
}
