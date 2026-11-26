//! The cell's client for the workspace's own public API.
//!
//! The cell acts on the workspace as an ENROLLED MEMBER — not as the relay,
//! not through a private seam — so everything it writes passes the same gates
//! a person's write passes: NIP-98 on the way in, membership and moderation
//! inside. That is the whole reason a compromised cell cannot do more to the
//! workspace than its own membership allows.
//!
//! One detail is easy to get wrong and expensive to debug: the NIP-98 `u` tag
//! must be the request PATH, because that is what the relay's binding check
//! compares it against (`auth::identify(header, &req.method, &req.path)`). A
//! full URL there authenticates nothing — it simply fails to match.

use ferrite_app::serde_json::{self, Value};

use crate::b64;
use crate::crypto::sha256::sha256;
use crate::crypto::to_hex;
use crate::error::{ProcessError, Result};
use crate::event::Event;
use crate::rcs::Call;

/// NIP-98's HTTP auth event kind.
pub const KIND_HTTP_AUTH: u32 = 27235;

/// A source of per-request nonces that are actually UNIQUE.
///
/// This matters more than it looks. The relay burns an authorization event's
/// id on first use, so two requests that build the SAME auth event are not
/// two requests — the second is refused as a replay. Deriving the nonce from
/// the request's own attributes and a second-resolution clock does not give
/// uniqueness: two identical calls in the same second collide, and the second
/// one fails for a reason that looks nothing like its cause.
///
/// So uniqueness comes from a monotonic counter plus a per-instance seed, and
/// the SEED is the caller's business because only the caller knows what is
/// unique in its world: the gateway's correlation id for a request-driven
/// call, or the app id plus boot instant for a polling loop. Two cells sharing
/// a seed AND a counter position would collide; nothing in this app does.
pub struct NonceSource {
    seed: String,
    counter: std::sync::atomic::AtomicU64,
}

impl NonceSource {
    /// `seed` must be unique per instance-lifetime (a correlation id, or an
    /// app id plus boot instant).
    #[must_use]
    pub fn new(seed: &str) -> Self {
        Self {
            seed: seed.to_string(),
            counter: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The next nonce. Monotonic within the instance, distinct across
    /// instances with distinct seeds.
    #[must_use]
    pub fn next(&self) -> String {
        let n = self
            .counter
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let digest = sha256(format!("{}|{n}", self.seed).as_bytes());
        let mut hex = to_hex(&digest);
        hex.truncate(32);
        hex
    }
}

/// Validate a workspace origin STRUCTURALLY.
///
/// A prefix test is not good enough here: `http://127.0.0.1.evil.example`
/// starts with the loopback string and is a completely different host, and
/// this origin is where the cell sends signed authorization headers. So the
/// value is parsed as exactly `scheme://host[:port]` and nothing else — an
/// origin with a path is not an origin, a bracketed IPv6 host may be followed
/// only by a port, and plaintext HTTP is admitted only when the host is
/// EXACTLY a loopback address.
pub fn validate_origin(origin: &str) -> Result<String> {
    let refuse = |why: &str| Err(ProcessError::Invalid(format!("workspace origin: {why}")));
    let trimmed = origin.trim_end_matches('/');
    if trimmed.is_empty() || trimmed.len() > 512 {
        return refuse("must be 1–512 bytes");
    }
    if trimmed.bytes().any(|b| !(0x21..=0x7e).contains(&b)) {
        return refuse("carries whitespace, control or non-ASCII bytes");
    }
    let (scheme, authority) = if let Some(rest) = trimmed.strip_prefix("https://") {
        ("https", rest)
    } else if let Some(rest) = trimmed.strip_prefix("http://") {
        ("http", rest)
    } else {
        return refuse("scheme must be https (or http on loopback for dev)");
    };
    if authority.is_empty() {
        return refuse("has no host");
    }
    // An ORIGIN is scheme + host + optional port. Anything else — a path, a
    // query, a fragment — would be silently concatenated with the request path
    // later and produce a URL nobody intended.
    if authority.contains('/') || authority.contains('?') || authority.contains('#') {
        return refuse("must be scheme://host[:port] with no path, query or fragment");
    }
    if authority.contains('@') {
        return refuse("must not carry userinfo (user:pass@host)");
    }

    // Split host and port. A bracketed IPv6 literal may be followed ONLY by
    // `:port` — trailing text after the bracket is how `[::1].evil.example`
    // sneaks past a lazy parser.
    let (host, port): (String, Option<&str>) = if let Some(after_open) = authority.strip_prefix('[')
    {
        let Some(close) = after_open.find(']') else {
            return refuse("unterminated IPv6 host");
        };
        let inner = after_open.get(..close).unwrap_or_default();
        let rest = after_open.get(close + 1..).unwrap_or_default();
        let port = if rest.is_empty() {
            None
        } else if let Some(p) = rest.strip_prefix(':') {
            Some(p)
        } else {
            return refuse("unexpected text after the IPv6 host");
        };
        if inner.is_empty() {
            return refuse("not an IPv6 literal");
        }
        // A charset check is not a parse: `[:]`, `[1:2:3]` and `[....]` all
        // pass one and are not addresses. The standard parser is the
        // authority, and its canonical form is what gets stored so two
        // spellings of one address cannot become two origins. The type comes
        // from `core`: it is a pure address VALUE with no socket behind it,
        // which is exactly the line the AICC ambient-authority gate draws.
        let Ok(parsed) = inner.parse::<core::net::Ipv6Addr>() else {
            return refuse("not an IPv6 literal");
        };
        (format!("[{parsed}]"), port)
    } else {
        let mut parts = authority.split(':');
        let host = parts.next().unwrap_or_default().to_ascii_lowercase();
        let port = parts.next();
        if parts.next().is_some() {
            return refuse("an IPv6 host must be bracketed");
        }
        if host.is_empty() {
            return refuse("has no host");
        }
        for label in host.split('.') {
            if label.is_empty() || label.len() > 63 {
                return refuse("a host label is empty or over 63 bytes");
            }
            if label.starts_with('-') || label.ends_with('-') {
                return refuse("a host label starts or ends with '-'");
            }
            if !label
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-')
            {
                return refuse("a host label carries invalid characters");
            }
        }
        (host, port)
    };
    // The port is kept as a NUMBER, not as the text it arrived in, and the
    // scheme's default port is dropped: `:0443`, `:443` and no port at all are
    // one origin, and leaving them as three strings would split every
    // origin-keyed scope three ways.
    let canonical_port = match port {
        None => None,
        Some(text) => {
            if text.is_empty() || text.len() > 5 || !text.bytes().all(|b| b.is_ascii_digit()) {
                return refuse("port must be 1–5 digits");
            }
            let value: u32 = text.parse().unwrap_or(0);
            if value == 0 || value > 65535 {
                return refuse("port is out of range");
            }
            let default = if scheme == "https" { 443 } else { 80 };
            if value == default {
                None
            } else {
                Some(value)
            }
        }
    };
    if scheme == "http" && !matches!(host.as_str(), "127.0.0.1" | "localhost" | "[::1]") {
        return refuse("plaintext http is accepted only for an exact loopback host");
    }
    // RECONSTRUCTED from the validated parts, not echoed back: the host is
    // lowercased, an IPv6 literal is in its canonical form, and the port is its
    // numeric value with the scheme's default omitted — so every spelling of one
    // origin becomes ONE stored origin. Anything not validated above cannot
    // survive into the result, which is the point of parsing at all.
    Ok(match canonical_port {
        Some(port) => format!("{scheme}://{host}:{port}"),
        None => format!("{scheme}://{host}"),
    })
}

#[derive(Clone)]
pub struct BuzzApi {
    origin: String,
    seckey: [u8; 32],
    pubkey_hex: String,
}

impl BuzzApi {
    /// `origin` is the deployment's HTTPS origin (a declared egress
    /// destination); `seckey` is the cell's own enrolled member key.
    pub fn new(origin: &str, seckey: [u8; 32]) -> Result<Self> {
        let origin = validate_origin(origin)?;
        let pubkey = crate::crypto::schnorr::xonly_pubkey(&seckey)
            .ok_or_else(|| ProcessError::Invalid("the cell's key is out of range".into()))?;
        Ok(Self {
            origin,
            seckey,
            pubkey_hex: to_hex(&pubkey),
        })
    }

    #[must_use]
    pub fn pubkey_hex(&self) -> &str {
        &self.pubkey_hex
    }

    /// Build the `Authorization: Nostr <base64>` header for one request.
    ///
    /// `nonce` must be unique per request — see [`NonceSource`] for why the
    /// caller supplies it rather than this function inventing one.
    ///
    /// The `payload` tag carries the body's SHA-256 whenever there is a body,
    /// because the operator plane's routes verify it and a header that omits
    /// it is refused there — including it always costs one hash and removes a
    /// class of "works on one route, fails on another" surprises.
    pub fn auth_header(
        &self,
        method: &str,
        path: &str,
        body: &[u8],
        created_at: u64,
        nonce: &str,
    ) -> Result<String> {
        if nonce.trim().is_empty() {
            return Err(ProcessError::Internal(
                "a NIP-98 nonce must be supplied and non-empty".into(),
            ));
        }
        // The ABSOLUTE URL: the relay requires the `u` tag's origin to be its
        // own, so a header signed for one workspace cannot be presented to
        // another (audit finding nip98-u-tag-origin-unbound).
        let mut tags = vec![
            vec!["u".to_string(), format!("{}{path}", self.origin)],
            vec!["method".to_string(), method.to_ascii_uppercase()],
            vec!["nonce".to_string(), nonce.to_string()],
        ];
        if !body.is_empty() {
            tags.push(vec!["payload".to_string(), to_hex(&sha256(body))]);
        }
        let event = Event::sign(&self.seckey, KIND_HTTP_AUTH, tags, "", created_at)?;
        let json = serde_json::to_vec(&event)
            .map_err(|e| ProcessError::Internal(format!("auth event encode: {e}")))?;
        Ok(format!("Nostr {}", b64::encode(&json)))
    }

    /// One authenticated request against the workspace.
    pub fn request(
        &self,
        method: &str,
        path: &str,
        body: Vec<u8>,
        created_at: u64,
        nonce: &str,
    ) -> Result<Call> {
        if !path.starts_with('/') {
            return Err(ProcessError::Internal(format!(
                "workspace path {path:?} must be absolute"
            )));
        }
        let header = self.auth_header(method, path, &body, created_at, nonce)?;
        let mut headers = vec![("authorization".to_string(), header)];
        if !body.is_empty() {
            headers.push(("content-type".to_string(), "application/json".to_string()));
        }
        Ok(Call {
            url: format!("{}{path}", self.origin),
            body,
            headers,
        })
    }

    pub fn publish(&self, event: &Event, created_at: u64, nonce: &str) -> Result<Call> {
        let body = serde_json::to_vec(event)
            .map_err(|e| ProcessError::Internal(format!("event encode: {e}")))?;
        self.request("POST", "/buzz/api/events", body, created_at, nonce)
    }

    pub fn query(&self, filter: &Value, created_at: u64, nonce: &str) -> Result<Call> {
        let body = serde_json::to_vec(filter)
            .map_err(|e| ProcessError::Internal(format!("query encode: {e}")))?;
        self.request("POST", "/buzz/api/query", body, created_at, nonce)
    }

    /// Read the events any of `filters` match.
    pub fn filter_query(&self, filters: &[Value], created_at: u64, nonce: &str) -> Result<Call> {
        let body = serde_json::to_vec(filters)
            .map_err(|e| ProcessError::Internal(format!("filter query encode: {e}")))?;
        self.request("POST", "/buzz/api/filter-query", body, created_at, nonce)
    }

    /// Create a room. Used when materializing a process room; a taken id
    /// answers 409, which the caller reads as "somebody else won the race".
    pub fn create_channel(
        &self,
        id: &str,
        name: &str,
        private: bool,
        created_at: u64,
        nonce: &str,
    ) -> Result<Call> {
        let body = serde_json::to_vec(&serde_json::json!({
            "id": id,
            "name": name,
            "type": "stream",
            "visibility": if private { "private" } else { "open" },
        }))
        .map_err(|e| ProcessError::Internal(format!("channel encode: {e}")))?;
        self.request("POST", "/buzz/api/channels", body, created_at, nonce)
    }

    /// One page of the community roster — how the cell resolves a
    /// requester's role. The relay pages it; `cursor` is the previous page's
    /// `next_cursor`.
    pub fn members(&self, cursor: Option<&str>, created_at: u64, nonce: &str) -> Result<Call> {
        let path = match cursor {
            Some(cursor) => format!("/buzz/api/members?cursor={}", percent_encode(cursor)),
            None => "/buzz/api/members".to_string(),
        };
        self.request("GET", &path, Vec::new(), created_at, nonce)
    }
}

fn percent_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// The next roster page's cursor, when the relay says the roster continues.
#[must_use]
pub fn next_roster_cursor(roster: &Value) -> Option<String> {
    if roster.get("complete").and_then(Value::as_bool) != Some(false) {
        return None;
    }
    roster.get("next_cursor").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string)
}

/// The role the roster reports for `pubkey_hex`, if any.
///
/// `None` means "not in the roster", which the authorization matrix treats as
/// no authority at all. The relay reports `admin` for a configured moderator
/// and `member` for everyone else; an OPERATOR is not visible in the roster,
/// so a cell never infers operator authority from it — the moderator tier is
/// the highest a roster read can establish, which is the conservative answer.
#[must_use]
pub fn role_in_roster(roster: &Value, pubkey_hex: &str) -> Option<crate::command::Role> {
    let members = roster.get("members")?.as_array()?;
    for row in members {
        let matches = row
            .get("pubkey")
            .and_then(Value::as_str)
            .is_some_and(|pk| pk.eq_ignore_ascii_case(pubkey_hex));
        if matches {
            let role = row.get("role").and_then(Value::as_str).unwrap_or("member");
            return crate::command::Role::parse(role).or(Some(crate::command::Role::Member));
        }
    }
    None
}

/// Whether the roster says this workspace gates membership at all.
#[must_use]
pub fn membership_required(roster: &Value) -> bool {
    roster
        .get("membership_required")
        .and_then(Value::as_bool)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SK: [u8; 32] = [3u8; 32];

    fn api() -> BuzzApi {
        BuzzApi::new("https://buzz.example/", SK).expect("api")
    }

    #[test]
    fn a_validated_origin_is_returned_in_canonical_form() {
        // Equality, not just acceptance: the stored origin is what every
        // request URL and every binding scope is built from, so one address
        // must have exactly one spelling.
        for (input, expected) in [
            ("https://buzz.example", "https://buzz.example"),
            ("https://buzz.example/", "https://buzz.example"),
            ("https://BUZZ.Example", "https://buzz.example"),
            ("https://buzz.example:8443", "https://buzz.example:8443"),
            ("http://127.0.0.1:8080", "http://127.0.0.1:8080"),
            ("http://LocalHost:3000", "http://localhost:3000"),
            // The canonicalization that matters: two spellings, one origin.
            ("http://[::1]:3000", "http://[::1]:3000"),
            ("http://[0:0:0:0:0:0:0:1]:3000", "http://[::1]:3000"),
            ("https://[2001:DB8::1]", "https://[2001:db8::1]"),
            // A leading-zero port and the scheme's default port converge too.
            ("https://buzz.example:0443", "https://buzz.example"),
            ("https://buzz.example:443", "https://buzz.example"),
            ("https://buzz.example:08443", "https://buzz.example:8443"),
            ("http://127.0.0.1:80", "http://127.0.0.1"),
        ] {
            assert_eq!(
                validate_origin(input).expect(input),
                expected,
                "canonical form of {input}"
            );
        }
    }

    #[test]
    fn an_origin_is_validated_structurally_not_by_prefix() {
        assert!(BuzzApi::new("https://buzz.example", SK).is_ok());
        assert!(BuzzApi::new("https://buzz.example:8443/", SK).is_ok());
        assert!(
            BuzzApi::new("http://127.0.0.1:8080", SK).is_ok(),
            "dev loopback"
        );
        assert!(
            BuzzApi::new("http://localhost:3000", SK).is_ok(),
            "dev loopback"
        );
        assert!(
            BuzzApi::new("http://[::1]:3000", SK).is_ok(),
            "dev loopback"
        );
        assert!(BuzzApi::new("https://[2001:db8::1]:443", SK).is_ok());
        // A non-canonical spelling of loopback is still loopback — the parser
        // canonicalizes before the host comparison.
        assert!(
            BuzzApi::new("http://[0:0:0:0:0:0:0:1]:3000", SK).is_ok(),
            "canonicalized loopback"
        );

        for hostile in [
            // A prefix test admits these as "loopback".
            "http://127.0.0.1.evil.example",
            "http://127.0.0.1evil.example",
            "http://localhost.evil.example",
            // A lazy bracket parser admits this one.
            "http://[::1].evil.example",
            "http://[::1]evil",
            "https://[2001:db8::1]x",
            "https://[2001:db8::1",
            // A charset check admits these; a real parser does not.
            "https://[:]",
            "https://[1:2:3]",
            "https://[....]",
            "https://[gggg::1]",
            "https://[::1::2]",
            "https://[]",
            // Plain http to a real host is never acceptable.
            "http://buzz.example",
            // Credentials smuggled into the authority.
            "https://user:pass@buzz.example",
            "http://x@127.0.0.1",
            // An origin is not a URL with a path, query or fragment: those
            // would be concatenated with the request path later.
            "https://buzz.example/api",
            "https://buzz.example?x=1",
            "https://buzz.example#f",
            // Malformed ports.
            "https://buzz.example:not-a-port",
            "https://buzz.example:",
            "https://buzz.example:0",
            "https://buzz.example:99999",
            "https://buzz.example:8080:9090",
            // Malformed hosts.
            "https://-buzz.example",
            "https://buzz-.example",
            "https://buzz..example",
            "https://buzz_example.com",
            // Not an origin at all.
            "buzz.example",
            "ftp://buzz.example",
            "https://",
            "https:// space.example",
            "",
        ] {
            assert!(
                BuzzApi::new(hostile, SK).is_err(),
                "{hostile} must be refused"
            );
        }
    }

    #[test]
    fn nonces_are_unique_per_request_and_per_instance() {
        let source = NonceSource::new("corr-1");
        let mut seen = std::collections::BTreeSet::new();
        for _ in 0..1_000 {
            assert!(seen.insert(source.next()), "a nonce repeated");
        }
        // A different seed gives a different sequence — two cells never collide.
        let other = NonceSource::new("corr-2");
        assert!(!seen.contains(&other.next()));
    }

    #[test]
    fn two_identical_requests_in_the_same_second_differ() {
        // This is what the relay's single-use burn requires: the SAME call made
        // twice must produce two distinct authorization events, or the second
        // is refused as a replay.
        let api = api();
        let source = NonceSource::new("corr-1");
        let a = api.members(None, 1_700_000_000, &source.next()).expect("call");
        let b = api.members(None, 1_700_000_000, &source.next()).expect("call");
        assert_ne!(
            a.headers, b.headers,
            "identical calls must not build identical auth events"
        );
    }

    #[test]
    fn an_empty_nonce_is_refused() {
        assert!(api()
            .auth_header("GET", "/buzz/api/members", b"", 1_700_000_000, "")
            .is_err());
        assert!(api()
            .auth_header("GET", "/buzz/api/members", b"", 1_700_000_000, "  ")
            .is_err());
    }

    #[test]
    fn the_auth_event_binds_the_absolute_url() {
        let header = api()
            .auth_header("POST", "/buzz/api/events", b"{}", 1_700_000_000, "n1")
            .expect("header");
        let encoded = header.strip_prefix("Nostr ").expect("prefix");
        let json = b64::decode(encoded).expect("base64");
        let event: Event = serde_json::from_slice(&json).expect("event");
        event.verify().expect("the auth event must verify");
        assert_eq!(event.kind, KIND_HTTP_AUTH);
        // The `u` tag is the absolute URL: the relay binds both its origin
        // and its path, and refuses a bare path.
        assert_eq!(event.first_tag("u"), Some("https://buzz.example/buzz/api/events"));
        assert_eq!(event.first_tag("method"), Some("POST"));
        assert_eq!(event.first_tag("nonce"), Some("n1"));
        assert_eq!(
            event.first_tag("payload"),
            Some(to_hex(&sha256(b"{}")).as_str()),
            "a body must be committed to by its digest"
        );
    }

    #[test]
    fn a_bodyless_request_carries_no_payload_tag() {
        let header = api()
            .auth_header("GET", "/buzz/api/members", b"", 1_700_000_000, "n1")
            .expect("header");
        let json = b64::decode(header.strip_prefix("Nostr ").expect("prefix")).expect("b64");
        let event: Event = serde_json::from_slice(&json).expect("event");
        assert_eq!(event.first_tag("payload"), None);
        assert_eq!(event.first_tag("method"), Some("GET"));
    }

    #[test]
    fn requests_target_the_origin_and_carry_authorization() {
        let call = api()
            .publish(
                &Event::sign(&SK, 9, vec![], "hi", 1_700_000_000).expect("sign"),
                1_700_000_000,
                "n1",
            )
            .expect("call");
        assert_eq!(call.url, "https://buzz.example/buzz/api/events");
        assert!(call.headers.iter().any(|(k, _)| k == "authorization"));
        assert!(call
            .headers
            .iter()
            .any(|(k, v)| k == "content-type" && v.contains("json")));

        let q = api()
            .query(&serde_json::json!({"kinds":[9],"limit":1}), 1_700_000_000, "n2")
            .expect("call");
        assert_eq!(q.url, "https://buzz.example/buzz/api/query");

        let f = api()
            .filter_query(&[serde_json::json!({"kinds":[9]})], 1_700_000_000, "n4")
            .expect("call");
        assert_eq!(f.url, "https://buzz.example/buzz/api/filter-query");
        assert_eq!(serde_json::from_slice::<Value>(&f.body).expect("json"), serde_json::json!([{"kinds":[9]}]));

        let members = api().members(None, 1_700_000_000, "n3").expect("call");
        assert_eq!(members.url, "https://buzz.example/buzz/api/members");
        assert!(members.body.is_empty());
    }

    #[test]
    fn a_channel_create_names_its_id_and_visibility() {
        let call = api()
            .create_channel("process-abc", "feature/x", true, 1_700_000_000, "n1")
            .expect("call");
        let body: Value = serde_json::from_slice(&call.body).expect("json");
        assert_eq!(body["id"], "process-abc");
        assert_eq!(body["visibility"], "private");
        assert_eq!(body["type"], "stream");
    }

    #[test]
    fn the_roster_resolves_roles_conservatively() {
        let roster = serde_json::json!({
            "membership_required": true,
            "members": [
                { "pubkey": "AA".repeat(32), "role": "admin" },
                { "pubkey": "bb".repeat(32), "role": "member" },
                { "pubkey": "cc".repeat(32) },
            ]
        });
        assert_eq!(
            role_in_roster(&roster, &"aa".repeat(32)),
            Some(crate::command::Role::Moderator),
            "admin reads as moderator, case-insensitively"
        );
        assert_eq!(
            role_in_roster(&roster, &"bb".repeat(32)),
            Some(crate::command::Role::Member)
        );
        assert_eq!(
            role_in_roster(&roster, &"cc".repeat(32)),
            Some(crate::command::Role::Member),
            "a row with no role is a plain member"
        );
        assert_eq!(
            role_in_roster(&roster, &"dd".repeat(32)),
            None,
            "absent means no authority at all"
        );
        assert!(membership_required(&roster));
        assert!(!membership_required(&serde_json::json!({})));
        for bad in [
            serde_json::json!({}),
            serde_json::json!({"members": "nope"}),
            serde_json::json!("nope"),
        ] {
            assert_eq!(role_in_roster(&bad, &"aa".repeat(32)), None, "{bad}");
        }
    }
}
