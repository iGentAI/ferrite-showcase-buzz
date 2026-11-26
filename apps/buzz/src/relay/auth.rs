//! Request authentication.
//!
//! Buzz authenticated a WebSocket once, with a challenge the relay had just
//! issued (NIP-42), and trusted the socket afterwards. There is no socket here,
//! so every request authenticates itself: the caller signs a short-lived
//! authorization event naming the method and path it is about to call, and sends
//! it in the `Authorization` header (NIP-98). That is strictly *more* work per
//! request and strictly *less* state to get wrong, and it is the only shape that
//! works when consecutive requests may land on different instances.
//!
//! Three defences, all required, because a signed token is a bearer token until
//! you bound it:
//!
//! * **Binding** — the signed event names the method and the absolute URL, so a
//!   token captured from a read cannot be replayed against a write, and one
//!   signed for another server (another Buzz workspace, any NIP-98 service the
//!   member uses with the same key) is not accepted here: NIP-98 has no server
//!   challenge, so the `u` tag's origin is the only thing tying a header to one
//!   server ([`origin_admits`]).
//! * **Freshness** — a ±60 second window, as in buzz.
//! * **Single use** — a create-if-absent TTL key per authorization event id. The
//!   store's conditional put *is* the guard; if it cannot be reached the request
//!   is refused, because a replay guard that fails open is not a replay guard.

use ferrite_app::client::StoreClusterClient;

use crate::relay::b64;
use crate::relay::event::{Event, PubKey};
use crate::relay::keys;
use crate::relay::kind;
use crate::relay::{BuzzError, Result};

/// Accepted clock skew for an authorization event, in seconds. Buzz's value.
pub const MAX_AUTH_SKEW_SECS: u64 = 60;

/// How long a used authorization event id is remembered. Comfortably longer than
/// the freshness window, so a token cannot outlive its own replay record.
pub const REPLAY_TTL_MS: u64 = 120 * 1000;

/// Largest reply body kept inside a replay record.
///
/// The record must be re-servable from one store row (16 MiB cap), but the real
/// bound is politeness: every authenticated request pays this write, so caching
/// a quarter-megabyte read would make the cure cost more than the disease. A
/// reply above the cap records its status and *no* body, and a replay of it is
/// answered as retryable — safe, because a reply that large is a read, and a
/// read re-run under a fresh authorization event returns the same thing.
pub const MAX_CACHED_REPLY_BYTES: usize = 64 * 1024;

/// The refusal a burned authorization event produces.
///
/// A constant because two places must agree on it byte for byte: the refusal
/// itself, and [`crate::relay::ingress`], which recognizes its own replay guard in a
/// finished reply so it can serve the first delivery's answer instead. Nothing
/// else in the relay may emit this sentence.
pub const REPLAYED_MESSAGE: &str = "authorization event has already been used";

/// The value byte marking a claim with no recorded answer yet.
const CLAIM_MARKER: u8 = b'1';
/// The value byte introducing a recorded `status + body` answer.
const OUTCOME_MARKER: u8 = b'R';

/// What the replay row for one authorization event holds.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Replay {
    /// Claimed, and the first attempt has not recorded an answer: it is still
    /// running, or it died. A duplicate may usefully wait a moment for it.
    InFlight,
    /// The first attempt finished, but its reply was over
    /// [`MAX_CACHED_REPLY_BYTES`] and was deliberately not stored.
    ///
    /// Distinct from [`Replay::InFlight`] on purpose, and the distinction is a
    /// security property rather than tidiness. Both end in "tell the caller to
    /// retry", but only `InFlight` is worth WAITING on. Conflating them let one
    /// cheap oversized read (any reply over the cap) be replayed to park a task
    /// for the full poll budget each time — with `ingress::MAX_IN_FLIGHT` slots
    /// and no signature check needed beyond re-sending a captured header, that
    /// is a remote capacity-exhaustion vector for the price of one request.
    /// A `NotKept` row is answered immediately, so it parks nothing.
    NotKept { status: u16 },
    /// The first attempt's answer, to be served again verbatim.
    Answered { status: u16, body: Vec<u8> },
}

/// Frame a recorded answer. `None` body means "answered, but not kept".
#[must_use]
fn encode_replay(status: u16, body: Option<&[u8]>) -> Vec<u8> {
    let payload = body.unwrap_or(&[]);
    let mut out = Vec::with_capacity(payload.len() + 3);
    out.push(OUTCOME_MARKER);
    out.extend_from_slice(&status.to_be_bytes());
    out.extend_from_slice(payload);
    out
}

/// Read a replay row back. Anything unrecognizable reads as [`Replay::InFlight`]
/// — the conservative answer, because it makes the caller retry rather than
/// serve a body it could not parse.
#[must_use]
pub fn decode_replay(raw: &[u8]) -> Replay {
    let Some((marker, rest)) = raw.split_first() else {
        return Replay::InFlight;
    };
    if *marker != OUTCOME_MARKER {
        return Replay::InFlight;
    }
    let (Some(hi), Some(lo)) = (rest.first(), rest.get(1)) else {
        return Replay::InFlight;
    };
    let status = u16::from_be_bytes([*hi, *lo]);
    match rest.get(2..) {
        Some(body) if !body.is_empty() => Replay::Answered {
            status,
            body: body.to_vec(),
        },
        // A recorded status with an empty body: the first attempt DID finish,
        // its reply was just too large to keep. Nothing to serve, and nothing
        // to wait for either.
        _ => Replay::NotKept { status },
    }
}

/// An authenticated caller.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Caller {
    pub pubkey: PubKey,
}

/// Parse and cryptographically verify an `Authorization: Nostr <base64>` header.
///
/// Checks, in order: the scheme, that the payload decodes, that it is an
/// authorization event, that the event's id and signature are sound, that it
/// names this method and path, and that it is fresh. Returns the author.
///
/// This function performs **no I/O** and therefore cannot enforce single use —
/// [`consume_once`] does that, and a caller that skips it has an authenticated
/// but replayable request.
pub fn verify_header(
    header: &str,
    method: &str,
    path: &str,
    now_ms: u64,
) -> Result<(Caller, Event)> {
    let (caller, event) = verify_binding(header, method, path)?;
    let now_secs = now_ms / 1000;
    let skew = now_secs.abs_diff(event.created_at);
    if skew > MAX_AUTH_SKEW_SECS {
        return Err(BuzzError::Unauthenticated(format!(
            "authorization event is {skew}s from now"
        )));
    }
    Ok((caller, event))
}

/// Identify the request an `Authorization` header names, cheaply.
///
/// Returns the authorization event's id and `created_at` — enough to find what a
/// previous delivery of this same request answered, and to know whether this one
/// is fresh enough to spend its authorization.
///
/// **Deliberately does not verify the signature.** [`verify_binding`] does, and
/// so does every route, via [`authenticate`]; doing it here as well would verify
/// the same schnorr signature twice on every authenticated request, which is the
/// most expensive thing in the path and buys nothing. What IS checked is that the
/// id is the hash of the event's own fields, which is the only property the id
/// needs to be usable as an idempotency key: nobody can present a *different*
/// event carrying someone else's id without breaking SHA-256, so a cache entry
/// cannot be aimed at another request. Presenting the *same* header is precisely
/// the duplicate case this exists to serve.
///
/// Not an admission decision. Nothing may authenticate a caller from this.
#[must_use]
pub fn identify(
    header: &str,
    method: &str,
    path: &str,
) -> Option<(crate::relay::event::EventId, u64)> {
    let payload = header
        .strip_prefix("Nostr ")
        .or_else(|| header.strip_prefix("nostr "))?;
    let raw = b64::decode(payload.trim())?;
    let event: Event = ferrite_app::serde_json::from_slice(&raw).ok()?;
    if event.kind != kind::KIND_HTTP_AUTH {
        return None;
    }
    // The id must be the hash of the fields, or it is not this event's id.
    if event.compute_id().ok()? != event.id {
        return None;
    }
    if !event.first_tag("method")?.eq_ignore_ascii_case(method) {
        return None;
    }
    if !url_covers_path(event.first_tag("u")?, path) {
        return None;
    }
    Some((event.id, event.created_at))
}

/// [`verify_header`] WITHOUT the freshness check: everything that makes the
/// header an authentic, correctly-bound authorization event, and nothing that
/// depends on the clock.
///
/// Split out for one reason: identifying a request is not the same as accepting
/// it. [`crate::relay::ingress`] needs the authorization event's **id** to look up what
/// a previous delivery of this same request answered, and a redelivery can
/// arrive after the freshness window has closed — at which point
/// [`verify_header`] rightly refuses, but the id is still exactly as
/// trustworthy, because it is the hash of signed fields. Using this for lookup
/// and [`verify_header`] for admission keeps a stale token unusable while still
/// letting its recorded answer be found.
///
/// Not an admission decision on its own. Nothing may authenticate a caller from
/// this without also checking freshness and single use.
pub fn verify_binding(header: &str, method: &str, path: &str) -> Result<(Caller, Event)> {
    let Some(payload) = header
        .strip_prefix("Nostr ")
        .or_else(|| header.strip_prefix("nostr "))
    else {
        return Err(BuzzError::Unauthenticated(
            "authorization scheme must be Nostr".into(),
        ));
    };
    let raw = b64::decode(payload.trim())
        .ok_or_else(|| BuzzError::Unauthenticated("authorization payload is not base64".into()))?;
    let event: Event = ferrite_app::serde_json::from_slice(&raw)
        .map_err(|e| BuzzError::Unauthenticated(format!("authorization payload: {e}")))?;

    if event.kind != kind::KIND_HTTP_AUTH {
        return Err(BuzzError::Unauthenticated(format!(
            "authorization event must be kind {}",
            kind::KIND_HTTP_AUTH
        )));
    }
    event
        .verify()
        .map_err(|e| BuzzError::Unauthenticated(format!("authorization event: {e}")))?;

    let Some(claimed_method) = event.first_tag("method") else {
        return Err(BuzzError::Unauthenticated(
            "authorization event lacks a method tag".into(),
        ));
    };
    if !claimed_method.eq_ignore_ascii_case(method) {
        return Err(BuzzError::Unauthenticated(
            "authorization event is for a different method".into(),
        ));
    }

    let Some(claimed_url) = event.first_tag("u") else {
        return Err(BuzzError::Unauthenticated(
            "authorization event lacks a u tag".into(),
        ));
    };
    if !url_covers_path(claimed_url, path) {
        return Err(BuzzError::Unauthenticated(
            "authorization event is for a different path".into(),
        ));
    }

    Ok((
        Caller {
            pubkey: event.pubkey,
        },
        event,
    ))
}

/// The operator-set config key holding this relay's public URL, stored as the
/// raw string (see `operator::validate_relay_url`). The NIP-42 socket path
/// reads the same key.
pub const RELAY_URL_KEY: &[u8] = b"buzz-relay-url";

/// Whether the `u` tag's origin is this relay.
///
/// The tag must be an absolute URL whose host is the host the gateway routed
/// the request on (`verified_host`, the caller's `Host`) or the host of the
/// operator-configured relay URL. Ports are ignored on both sides, as the NIP-42
/// check ignores them. A bare path names no server, and with neither host known
/// nothing can be compared: both are refused, because a relay that cannot tell
/// a header signed for it from one signed for any other server must not accept
/// either.
#[must_use]
pub fn origin_admits(claimed_url: &str, verified_host: Option<&str>, relay_url: Option<&str>) -> bool {
    if !claimed_url.contains("://") {
        return false;
    }
    let Some(claimed) = crate::relay::ws::relay_host(claimed_url) else {
        return false;
    };
    [verified_host, relay_url]
        .into_iter()
        .flatten()
        .filter_map(crate::relay::ws::relay_host)
        .any(|known| known.eq_ignore_ascii_case(&claimed))
}

/// Refuse a header whose `u` tag names another server ([`origin_admits`]).
///
/// The configured relay URL is read only when the routed host does not
/// already match, so the common case costs no store read.
async fn require_origin(
    store: &mut StoreClusterClient,
    auth_event: &Event,
    verified_host: Option<&str>,
) -> Result<()> {
    let claimed = auth_event.first_tag("u").unwrap_or_default();
    if origin_admits(claimed, verified_host, None) {
        return Ok(());
    }
    let relay_url = crate::relay::store::get_raw(store, RELAY_URL_KEY)
        .await?
        .map(|raw| String::from_utf8_lossy(&raw).trim().to_string())
        .filter(|url| !url.is_empty());
    if origin_admits(claimed, verified_host, relay_url.as_deref()) {
        return Ok(());
    }
    Err(BuzzError::Unauthenticated(
        "authorization event is for another server: its u tag must be this relay's absolute URL".into(),
    ))
}

/// Whether the `u` tag names the path being called. The PATH half of the
/// binding only: admission also requires the tag's origin to be this relay
/// ([`origin_admits`], checked in [`authenticate_event`]).
///
/// The tag holds a full URL and the app only ever sees a path (the gateway
/// forwards no Host), so the comparison is on the path component. The claimed
/// path must match exactly after the query string is dropped — a prefix match
/// would let a token minted for `/buzz/api/query` authorize
/// `/buzz/api/query/../events`.
#[must_use]
pub fn url_covers_path(claimed_url: &str, path: &str) -> bool {
    let claimed_path = strip_origin(claimed_url);
    let claimed_path = crate::http_query::path_only(claimed_path);
    let actual = crate::http_query::path_only(path);
    !claimed_path.is_empty() && claimed_path == actual
}

/// Reduce a URL to its path, tolerating a bare path.
fn strip_origin(url: &str) -> &str {
    let after_scheme = match url.find("://") {
        Some(i) => url.get(i + 3..).unwrap_or(""),
        None => return url,
    };
    match after_scheme.find('/') {
        Some(i) => after_scheme.get(i..).unwrap_or("/"),
        None => "/",
    }
}

/// Burn an authorization event's id so it cannot be used twice.
///
/// A create-if-absent write with a TTL: the first caller wins, a replay finds the
/// key present and is refused. Any storage error refuses the request — failing
/// open here would silently disable replay protection for as long as the store is
/// unreachable.
pub async fn consume_once(
    store: &mut StoreClusterClient,
    community: &str,
    auth_event: &Event,
    now_ms: u64,
) -> Result<()> {
    let key = keys::replay(community, &auth_event.id);
    let expires_at = now_ms.saturating_add(REPLAY_TTL_MS);
    match store
        .put_if_version_expiring(&key, &[CLAIM_MARKER], None, expires_at)
        .await
    {
        Ok(Ok(_)) => Ok(()),
        Ok(Err(_)) => Err(BuzzError::Unauthenticated(REPLAYED_MESSAGE.into())),
        Err(e) => Err(BuzzError::Storage(format!("replay guard unavailable: {e}"))),
    }
}

/// Read the replay row for `auth_event_id`, if there is one.
///
/// `None` means the authorization event has never been seen. `Some` means it
/// has, and [`Replay`] says whether its answer is available to serve again.
pub async fn replay_state(
    store: &mut StoreClusterClient,
    community: &str,
    auth_event_id: &crate::relay::event::EventId,
) -> Result<Option<Replay>> {
    let key = keys::replay(community, auth_event_id);
    Ok(crate::relay::store::get_raw(store, &key)
        .await?
        .map(|raw| decode_replay(&raw)))
}

/// Record what the first delivery answered, so a redelivery of the SAME request
/// can be served that answer instead of a refusal.
///
/// This is what makes single use mean *"the action happens at most once"*
/// rather than *"a second delivery errors"*. The two are not the same, and the
/// difference is a live bug: the request queue is at-least-once by design (a
/// reply-publish failure deliberately leaves the envelope unacked so the work is
/// re-driven, `ingress::api_loop`), so a redelivery re-enters the route, finds
/// the id burned, and publishes `authorization event has already been used` as
/// the caller's answer — **for a write that already succeeded**. The user is
/// told their action failed while its effects are durable, which is the worst
/// thing a write API can report.
///
/// `expires_at_ms` must be the deadline the CLAIM was written with, not a fresh
/// one: recording an outcome must not extend how long a token is remembered.
///
/// # What this discloses
///
/// Serving a recorded answer changes what a captured `Authorization` header is
/// worth. Before, replaying a burned token yielded a refusal and nothing else;
/// now it yields the reply that header's request produced. So a party who
/// captured only the REQUEST obtains a response they had not seen.
///
/// Stated rather than waved away, because it is a real cost. It is bounded twice
/// — to [`REPLAY_TTL_MS`] (two minutes) and to bodies at or under
/// [`MAX_CACHED_REPLY_BYTES`], since anything larger is recorded without its
/// body — and in practice whoever can capture the header can usually capture the
/// reply beside it. It also cannot be mitigated by binding the record to a
/// caller: replaying REQUIRES presenting the original signed kind-27235 event,
/// which verifies as that user, so there is nothing to distinguish a captured
/// replay from that user's own legitimate retry. The alternative — keeping the
/// refusal — is what tells users a completed write failed, which is worse.
///
/// Best-effort by contract. A failure here costs a future redelivery its cached
/// answer; it must never fail the request whose answer it is recording, because
/// that request has already happened.
pub async fn record_outcome(
    store: &mut StoreClusterClient,
    community: &str,
    auth_event_id: &crate::relay::event::EventId,
    status: u16,
    body: &[u8],
    expires_at_ms: u64,
) -> Result<()> {
    let keep = (body.len() <= MAX_CACHED_REPLY_BYTES).then_some(body);
    let key = keys::replay(community, auth_event_id);
    crate::relay::store::put_expiring_raw(store, &key, &encode_replay(status, keep), expires_at_ms)
        .await
        .map(|_| ())
}

/// Verify a request's authorization and burn it, in one step.
///
/// `verified_host` is the gateway-attached host the request was routed on
/// (`IngressRequest::verified_host`); the `u` tag must name it or the
/// configured relay URL.
pub async fn authenticate(
    store: &mut StoreClusterClient,
    community: &str,
    header: Option<&str>,
    method: &str,
    path: &str,
    verified_host: Option<&str>,
    now_ms: u64,
) -> Result<Caller> {
    authenticate_event(store, community, header, method, path, verified_host, None, now_ms)
        .await
        .map(|(caller, _)| caller)
}

/// [`authenticate`], returning the authorization event alongside the caller.
///
/// The operator routes need the event to check its `payload` tag
/// ([`verify_payload_tag`]); every other route only wants the identity, so
/// [`authenticate`] stays the common entry point and this is the one seam that
/// hands the event back. Same binding, freshness, and single-use burn.
///
/// With `payload`, the header must also commit to those body bytes
/// ([`verify_payload_tag`]), and that check runs BEFORE the burn: a header
/// refused for its body leaves no in-flight replay claim behind, which a 401
/// never settles, so a bad body could not pre-burn a captured token or pin a
/// worker slot on a stuck claim.
#[allow(clippy::too_many_arguments)]
pub async fn authenticate_event(
    store: &mut StoreClusterClient,
    community: &str,
    header: Option<&str>,
    method: &str,
    path: &str,
    verified_host: Option<&str>,
    payload: Option<&[u8]>,
    now_ms: u64,
) -> Result<(Caller, Event)> {
    let header = header.ok_or_else(|| {
        BuzzError::Unauthenticated("this route requires an Authorization header".into())
    })?;
    let (caller, auth_event) = verify_header(header, method, path, now_ms)?;
    // Before the burn, like the payload check: a header signed for another
    // server leaves no replay claim behind here.
    require_origin(store, &auth_event, verified_host).await?;
    if let Some(body) = payload {
        verify_payload_tag(&auth_event, body)?;
    }
    consume_once(store, community, &auth_event, now_ms).await?;
    Ok((caller, auth_event))
}

/// Require the authorization event's `payload` tag to commit to `body`.
///
/// [`verify_header`] binds only the method and the path (and deliberately drops
/// query strings). That is enough for the member routes, whose bodies are
/// self-signed events — tampering with the body breaks the event's own
/// signature. Operator bodies are UNSIGNED config documents, so without this a
/// captured operator header would authorize any body an interceptor liked for
/// its 60-second window. The `payload` tag is standard NIP-98 (buzz-web's
/// `bzcli.mjs` already emits it): the lowercase SHA-256 hex of the exact body
/// bytes, making one signed header authorize exactly one body. Existing routes
/// never call this, so their observable behavior is unchanged.
pub fn verify_payload_tag(auth_event: &Event, body: &[u8]) -> Result<()> {
    let refusal =
        || BuzzError::Unauthenticated("authorization event does not commit to this body".into());
    let Some(claimed) = auth_event.first_tag("payload") else {
        return Err(refusal());
    };
    let expected = crate::relay::crypto::to_hex(&crate::relay::crypto::sha256::sha256(body));
    if !crate::relay::crypto::ct_eq(claimed.as_bytes(), expected.as_bytes()) {
        return Err(refusal());
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;
    use crate::relay::b64;
    use crate::relay::event::Event;

    /// A fixed secret key; BIP-340 signing here is deterministic, so tests are too.
    const SK: [u8; 32] = [
        0x9f, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71, 0x82, 0x93, 0xa4, 0xb5, 0xc6, 0xd7, 0xe8,
        0xf9, 0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71, 0x82, 0x93, 0xa4, 0xb5, 0xc6, 0xd7,
        0xe8, 0xf9,
    ];

    fn auth_event(method: &str, url: &str, created_at: u64) -> Event {
        Event::sign_new(
            &SK,
            kind::KIND_HTTP_AUTH,
            vec![
                vec!["u".into(), url.into()],
                vec!["method".into(), method.into()],
            ],
            "",
            created_at,
        )
        .unwrap()
    }

    fn header_for(event: &Event) -> String {
        let json = ferrite_app::serde_json::to_vec(event).unwrap();
        format!("Nostr {}", b64::encode(&json))
    }

    #[test]
    fn a_well_formed_header_authenticates() {
        let e = auth_event("POST", "https://buzz.example/buzz/api/events", 1_000);
        let (caller, returned) =
            verify_header(&header_for(&e), "POST", "/buzz/api/events", 1_000_000).unwrap();
        assert_eq!(caller.pubkey, e.pubkey);
        assert_eq!(returned.id, e.id);
    }

    #[test]
    fn method_is_case_insensitive_but_must_match() {
        let e = auth_event("post", "https://buzz.example/buzz/api/events", 1_000);
        assert!(verify_header(&header_for(&e), "POST", "/buzz/api/events", 1_000_000).is_ok());
        assert!(verify_header(&header_for(&e), "GET", "/buzz/api/events", 1_000_000).is_err());
    }

    #[test]
    fn a_token_for_one_path_does_not_authorize_another() {
        let e = auth_event("POST", "https://buzz.example/buzz/api/query", 1_000);
        assert!(verify_header(&header_for(&e), "POST", "/buzz/api/events", 1_000_000).is_err());
        // ...including a path that merely starts with the signed one.
        assert!(
            verify_header(&header_for(&e), "POST", "/buzz/api/query/extra", 1_000_000).is_err()
        );
    }

    #[test]
    fn query_strings_do_not_affect_the_binding() {
        let e = auth_event(
            "GET",
            "https://buzz.example/buzz/api/threads?limit=5",
            1_000,
        );
        assert!(verify_header(
            &header_for(&e),
            "GET",
            "/buzz/api/threads?limit=9",
            1_000_000
        )
        .is_ok());
    }

    #[test]
    fn stale_and_future_tokens_are_refused() {
        let e = auth_event("POST", "https://buzz.example/buzz/api/events", 1_000);
        // 61 seconds late.
        assert!(verify_header(&header_for(&e), "POST", "/buzz/api/events", 1_061_000).is_err());
        // 61 seconds early.
        assert!(verify_header(&header_for(&e), "POST", "/buzz/api/events", 939_000).is_err());
        // Just inside the window, both directions.
        assert!(verify_header(&header_for(&e), "POST", "/buzz/api/events", 1_060_000).is_ok());
        assert!(verify_header(&header_for(&e), "POST", "/buzz/api/events", 940_000).is_ok());
    }

    #[test]
    fn a_tampered_event_body_fails_verification() {
        let e = auth_event("POST", "https://buzz.example/buzz/api/events", 1_000);
        let mut tampered = e.clone();
        tampered.tags = vec![
            vec!["u".into(), "https://buzz.example/buzz/api/events".into()],
            vec!["method".into(), "POST".into()],
            vec!["injected".into(), "value".into()],
        ];
        // The id no longer matches the fields.
        assert!(verify_header(
            &header_for(&tampered),
            "POST",
            "/buzz/api/events",
            1_000_000
        )
        .is_err());
    }

    #[test]
    fn a_forged_signature_fails_verification() {
        let e = auth_event("POST", "https://buzz.example/buzz/api/events", 1_000);
        let mut forged = e.clone();
        forged.sig[0] ^= 0x01;
        assert!(
            verify_header(&header_for(&forged), "POST", "/buzz/api/events", 1_000_000).is_err()
        );
    }

    #[test]
    fn wrong_kind_is_refused() {
        let e = Event::sign_new(
            &SK,
            kind::KIND_STREAM_MESSAGE,
            vec![
                vec!["u".into(), "https://buzz.example/buzz/api/events".into()],
                vec!["method".into(), "POST".into()],
            ],
            "",
            1_000,
        )
        .unwrap();
        assert!(verify_header(&header_for(&e), "POST", "/buzz/api/events", 1_000_000).is_err());
    }

    #[test]
    fn missing_tags_are_refused() {
        let no_method = Event::sign_new(
            &SK,
            kind::KIND_HTTP_AUTH,
            vec![vec!["u".into(), "https://x/buzz/api/events".into()]],
            "",
            1_000,
        )
        .unwrap();
        assert!(verify_header(
            &header_for(&no_method),
            "POST",
            "/buzz/api/events",
            1_000_000
        )
        .is_err());

        let no_url = Event::sign_new(
            &SK,
            kind::KIND_HTTP_AUTH,
            vec![vec!["method".into(), "POST".into()]],
            "",
            1_000,
        )
        .unwrap();
        assert!(
            verify_header(&header_for(&no_url), "POST", "/buzz/api/events", 1_000_000).is_err()
        );
    }

    #[test]
    fn malformed_headers_are_refused_without_panicking() {
        for bad in [
            "",
            "Bearer abc",
            "Nostr",
            "Nostr ",
            "Nostr !!!!",
            "Nostr YWJj", // valid base64, not an event
            "nostr Zm9vYmFy",
        ] {
            assert!(
                verify_header(bad, "POST", "/buzz/api/events", 1_000_000).is_err(),
                "accepted {bad:?}"
            );
        }
    }

    /// An authorization event that ALSO commits to a body, as the operator
    /// routes require and as `bzcli.mjs` mints.
    fn auth_event_with_payload(method: &str, url: &str, body: &[u8], created_at: u64) -> Event {
        Event::sign_new(
            &SK,
            kind::KIND_HTTP_AUTH,
            vec![
                vec!["u".into(), url.into()],
                vec!["method".into(), method.into()],
                vec![
                    "payload".into(),
                    crate::relay::crypto::to_hex(&crate::relay::crypto::sha256::sha256(body)),
                ],
            ],
            "",
            created_at,
        )
        .unwrap()
    }

    #[test]
    fn a_missing_payload_tag_is_refused() {
        // A header without the tag authorizes NO body at all on routes that
        // require the commitment — absence is not "any body".
        let e = auth_event("POST", "https://h/buzz/api/operator/config/k", 1_000);
        assert!(verify_payload_tag(&e, b"").is_err());
        assert!(verify_payload_tag(&e, b"anything").is_err());
    }

    #[test]
    fn a_tampered_body_under_a_valid_header_is_refused() {
        // The interception scenario the tag exists for: the header itself still
        // verifies (method + path + signature all sound), but the body it rode
        // in with has been swapped.
        let body = br#"{"value":"wss://real.example"}"#;
        let e =
            auth_event_with_payload("POST", "https://h/buzz/api/operator/config/k", body, 1_000);
        let (_caller, returned) = verify_header(
            &header_for(&e),
            "POST",
            "/buzz/api/operator/config/k",
            1_000_000,
        )
        .unwrap();
        assert!(verify_payload_tag(&returned, body).is_ok());
        assert!(verify_payload_tag(&returned, br#"{"value":"wss://evil.example"}"#).is_err());
    }

    #[test]
    fn a_recorded_answer_round_trips() {
        let raw = encode_replay(201, Some(br#"{"ok":true}"#));
        assert_eq!(
            decode_replay(&raw),
            Replay::Answered {
                status: 201,
                body: br#"{"ok":true}"#.to_vec(),
            }
        );
        // Every status a route can produce survives the two-byte framing.
        for status in [200u16, 201, 401, 404, 409, 429, 500, 503] {
            let raw = encode_replay(status, Some(b"x"));
            assert_eq!(
                decode_replay(&raw),
                Replay::Answered {
                    status,
                    body: b"x".to_vec()
                }
            );
        }
    }

    #[test]
    fn an_answer_too_large_to_keep_is_distinguishable_from_in_flight() {
        // Both mean "retry", but only IN FLIGHT is worth waiting on. Keeping
        // them apart is what stops a replayed oversized read from parking a
        // task for the whole poll budget, over and over.
        assert_eq!(
            decode_replay(&encode_replay(200, None)),
            Replay::NotKept { status: 200 }
        );
        assert_ne!(decode_replay(&encode_replay(200, None)), Replay::InFlight);
        assert_eq!(decode_replay(&[CLAIM_MARKER]), Replay::InFlight);
    }

    #[test]
    fn a_malformed_record_never_panics_and_never_invents_an_answer() {
        // Truncated framing, a foreign marker, and an empty row must all fail
        // CLOSED to "retry", never to a fabricated body.
        for bad in [
            [].as_slice(),
            b"R",
            b"R\x00",
            b"1",
            b"nonsense",
            &[OUTCOME_MARKER, 0],
        ] {
            assert_eq!(decode_replay(bad), Replay::InFlight, "{bad:?}");
        }
    }

    #[test]
    fn the_burn_refusal_uses_the_shared_sentence() {
        // `ingress::is_replay_refusal` recognizes the guard by this exact text,
        // so the refusal and the constant must not drift apart.
        let refusal = BuzzError::Unauthenticated(REPLAYED_MESSAGE.into()).to_string();
        assert!(refusal.contains(REPLAYED_MESSAGE), "{refusal}");
        // The token must stay `restricted: ` whichever variant carries it, because
        // `ingress::is_replay_refusal` matches the RENDERED sentence.
        assert!(refusal.starts_with("restricted: "), "{refusal}");
    }

    #[test]
    fn binding_is_verified_without_the_clock_but_admission_is_not() {
        // A token far outside the freshness window is still IDENTIFIABLE (its id
        // is the hash of signed fields), which is what lets a late redelivery
        // find its recorded answer — while remaining inadmissible.
        let e = auth_event("POST", "https://buzz.example/buzz/api/events", 1_000);
        let header = header_for(&e);
        let (_caller, bound) = verify_binding(&header, "POST", "/buzz/api/events").unwrap();
        assert_eq!(bound.id, e.id);
        // ...but far-future "now" still refuses it for admission.
        assert!(verify_header(&header, "POST", "/buzz/api/events", 9_000_000).is_err());
        // A forged or mis-bound header is refused by the binding check itself.
        let mut forged = e.clone();
        forged.sig[0] ^= 0x01;
        assert!(verify_binding(&header_for(&forged), "POST", "/buzz/api/events").is_err());
        assert!(verify_binding(&header, "GET", "/buzz/api/events").is_err());
        assert!(verify_binding(&header, "POST", "/buzz/api/other").is_err());
    }

    #[test]
    fn urls_reduce_to_paths_correctly() {
        // The PATH half only; the origin half is `origin_admits`, below.
        assert!(url_covers_path(
            "https://h/buzz/api/events",
            "/buzz/api/events"
        ));
        assert!(url_covers_path(
            "http://h:3000/buzz/api/events",
            "/buzz/api/events"
        ));
        assert!(url_covers_path("/buzz/api/events", "/buzz/api/events"));
        assert!(!url_covers_path("https://h", "/buzz/api/events"));
        assert!(!url_covers_path("", "/buzz/api/events"));
        assert!(!url_covers_path("https://h/other", "/buzz/api/events"));
    }

    /// The `u` tag's origin must be this relay (audit finding
    /// `apps/buzz/src/relay/auth.rs:url_covers_path:nip98-u-tag-origin-unbound`).
    /// The path comparison alone accepted `http://h:3000/...` and any other
    /// host, so a header a member signed for another NIP-98 server
    /// authenticated them here.
    #[test]
    fn the_u_tag_must_name_this_relays_host() {
        let routed = Some("buzz.example");
        assert!(origin_admits("https://buzz.example/buzz/api/events", routed, None));
        assert!(origin_admits("https://BUZZ.example:443/buzz/api/events", Some("buzz.example:443"), None));
        assert!(!origin_admits("http://h:3000/buzz/api/events", routed, None), "another host");
        assert!(!origin_admits("https://other-relay.example/buzz/api/mcp/token", routed, None));
        assert!(!origin_admits("/buzz/api/events", routed, None), "a bare path names no server");
        assert!(!origin_admits("https://buzz.example/buzz/api/events", None, None), "nothing to compare with");
        // The configured relay URL is the other accepted origin.
        assert!(origin_admits("https://buzz.example/x", None, Some("wss://buzz.example/")));
        assert!(origin_admits("https://buzz.example/x", Some("internal.local"), Some("wss://buzz.example")));
        assert!(!origin_admits("https://evil.example/x", Some("internal.local"), Some("wss://buzz.example")));
    }

    /// End to end over the store: a fresh, correctly signed header for the
    /// right path but another origin is refused, and is not burned here.
    #[tokio::test]
    async fn a_header_signed_for_another_server_does_not_authenticate() {
        let env = ferrite_app_test::TestHarness::new().with_store("config").build().await;
        let ctx = env.context();
        let mut store = ctx.store("config").unwrap();
        let now_ms = 1_000_000;
        let foreign = auth_event("POST", "https://other-relay.example/buzz/api/mcp/token", 1_000);
        let refused = authenticate(&mut store, "c", Some(&header_for(&foreign)), "POST", "/buzz/api/mcp/token", Some("buzz.example"), now_ms).await;
        assert!(refused.is_err(), "a header signed for another server authenticated its signer");
        assert!(replay_state(&mut store, "c", &foreign.id).await.unwrap().is_none(), "the refused header was burned");

        let own = auth_event("POST", "https://buzz.example/buzz/api/mcp/token", 1_000);
        let caller = authenticate(&mut store, "c", Some(&header_for(&own)), "POST", "/buzz/api/mcp/token", Some("buzz.example"), now_ms).await.unwrap();
        assert_eq!(caller.pubkey, own.pubkey);

        // With no routed host, the operator-configured relay URL decides.
        store.put(RELAY_URL_KEY, b"wss://buzz.example/").await.unwrap();
        let configured = auth_event("POST", "https://buzz.example/buzz/api/query", 1_001);
        assert!(authenticate(&mut store, "c", Some(&header_for(&configured)), "POST", "/buzz/api/query", None, now_ms).await.is_ok());
        let elsewhere = auth_event("POST", "https://other-relay.example/buzz/api/query", 1_001);
        assert!(authenticate(&mut store, "c", Some(&header_for(&elsewhere)), "POST", "/buzz/api/query", None, now_ms).await.is_err());
    }
}
