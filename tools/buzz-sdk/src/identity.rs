//! The agent's own keypair — the thing that makes it a member rather than a bot
//! account borrowed from a human.
//!
//! An [`Identity`] is a secp256k1 secret key you already hold, plus the two
//! things derived from it once at construction: the x-only public key and its
//! `npub`. Deriving the public key costs one scalar multiplication, so it is
//! done in the constructor and cached rather than recomputed per event.
//!
//! The SDK mints no keys. There is no `Identity::generate()`: key generation
//! belongs to whatever already owns your secrets (a password manager, a KMS, the
//! relay's keyring), and an SDK that invented keys would quietly become the
//! place they are stored.
//!
//! All signing goes through [`buzz::relay::event::Event::sign_new`], so an event
//! this SDK produces is byte-identical to one the relay would produce from the
//! same fields — there is no second implementation of canonicalization or
//! BIP-340 here.

use buzz::relay::crypto::{bech32, from_hex32, schnorr, to_hex};
use buzz::relay::event::{Event, PubKey};
use buzz::relay::kind::{Kind, KIND_HTTP_AUTH};
use buzz::relay::{b64, BuzzError};

use crate::error::{Result, SdkError};

/// An agent's signing identity.
///
/// Clone is cheap-ish (copies 32 secret bytes and clones the cached `npub`
/// string) and is how the identity reaches the transport, the workspace and the
/// agent runtime without any of them owning it exclusively.
#[derive(Clone)]
pub struct Identity {
    secret: [u8; 32],
    pubkey: PubKey,
    npub: String,
    /// Monotone per-request counter, shared across clones.
    ///
    /// Authorization events are signed deterministically (BIP-340 with a fixed
    /// aux) and `created_at` has one-second granularity, so an event built from
    /// `(method, url, created_at)` alone is *byte-identical* for two requests to
    /// the same route inside one second — same id, and the relay's replay guard
    /// correctly refuses the second as a replay. An agent posting twice in a
    /// second would have its second call rejected. This counter makes every
    /// header distinct.
    nonce: std::sync::Arc<std::sync::atomic::AtomicU64>,
}

/// Debug never renders the secret — an agent that logs its own struct must not
/// leak its key. Only the public identity is shown.
impl core::fmt::Debug for Identity {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Identity")
            .field("npub", &self.npub)
            .field("secret", &"<redacted>")
            .finish()
    }
}

/// Best-effort scrub of the secret when the last copy goes away.
///
/// Honest about what it is: without a `zeroize` dependency (the SDK adds none)
/// the compiler is permitted to elide these writes, and the key may have been
/// copied by the allocator or the stack before this runs. It costs nothing and
/// removes the easy case; it is not a guarantee.
impl Drop for Identity {
    fn drop(&mut self) {
        for byte in &mut self.secret {
            *byte = 0;
        }
    }
}

/// A starting point for an identity's request counter.
///
/// Seeded from the host clock's nanoseconds so two *processes* holding the same
/// key are overwhelmingly unlikely to mint the same header in the same second;
/// within one process the counter alone guarantees it. There is no RNG in this
/// crate's dependency set, and this does not need to be unpredictable — only
/// distinct, because the value's job is to change an event id, not to be a secret.
fn new_nonce_seed() -> std::sync::Arc<std::sync::atomic::AtomicU64> {
    let seed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::from(d.subsec_nanos()));
    std::sync::Arc::new(std::sync::atomic::AtomicU64::new(seed))
}

impl Identity {
    /// Build an identity from raw secret-key bytes.
    ///
    /// # Errors
    ///
    /// [`SdkError::Identity`] if the scalar is zero or at/above the secp256k1
    /// group order — such a key cannot produce a public key and every later
    /// signature would fail.
    pub fn from_secret(secret: [u8; 32]) -> Result<Self> {
        let pubkey = schnorr::xonly_pubkey(&secret).ok_or_else(|| {
            SdkError::Identity("secret key is not a valid secp256k1 scalar".into())
        })?;
        let npub = bech32::npub(&pubkey)
            .ok_or_else(|| SdkError::Identity("public key could not be encoded as npub".into()))?;
        Ok(Self {
            secret,
            pubkey,
            npub,
            nonce: new_nonce_seed(),
        })
    }

    /// Build an identity from a 64-character hex secret key.
    ///
    /// Surrounding whitespace is trimmed, because this value usually arrives
    /// from a shell variable or a file.
    ///
    /// # Errors
    ///
    /// [`SdkError::Identity`] if the string is not 32 bytes of hex, or if the
    /// key is out of range.
    pub fn from_hex(hex: &str) -> Result<Self> {
        let bytes = from_hex32(hex.trim())
            .ok_or_else(|| SdkError::Identity("secret key is not 32 bytes of hex".into()))?;
        Self::from_secret(bytes)
    }

    /// Build an identity from a NIP-19 `nsec1…` string.
    ///
    /// The human-readable part must be exactly `nsec`: an `npub` or a `note` of
    /// the right shape is refused rather than silently accepted as a secret.
    ///
    /// # Errors
    ///
    /// [`SdkError::Identity`] if the bech32 checksum fails, the HRP is not
    /// `nsec`, or the key is out of range.
    pub fn from_nsec(nsec: &str) -> Result<Self> {
        let (hrp, bytes) = bech32::decode(nsec.trim())
            .ok_or_else(|| SdkError::Identity("nsec is not valid bech32".into()))?;
        if hrp != "nsec" {
            return Err(SdkError::Identity(format!(
                "expected an nsec key, got a '{hrp}' string"
            )));
        }
        Self::from_secret(bytes)
    }

    /// Read a key from the environment, accepting either encoding.
    ///
    /// A value starting with `nsec1` is decoded as bech32; anything else is
    /// tried as hex. Surrounding whitespace and a single pair of wrapping
    /// quotes are stripped, because `.env` files routinely add both.
    ///
    /// Cost: one `std::env::var` read. The value is *not* cached anywhere the
    /// caller cannot see — the returned [`Identity`] owns the only copy the SDK
    /// keeps.
    ///
    /// # Errors
    ///
    /// * [`SdkError::Env`] if the variable is unset or not UTF-8 — and unlike
    ///   the blanket [`From<std::env::VarError>`] conversion, this names the
    ///   variable.
    /// * [`SdkError::Identity`] if the value is not a usable key.
    pub fn from_env(var: &str) -> Result<Self> {
        let raw = std::env::var(var).map_err(|e| SdkError::env(var, e.to_string()))?;
        let trimmed = raw.trim();
        let unquoted = trimmed
            .strip_prefix('"')
            .and_then(|s| s.strip_suffix('"'))
            .or_else(|| {
                trimmed
                    .strip_prefix('\'')
                    .and_then(|s| s.strip_suffix('\''))
            })
            .unwrap_or(trimmed);
        if unquoted.is_empty() {
            return Err(SdkError::env(var, "is set but empty"));
        }
        if unquoted.starts_with("nsec1") {
            Self::from_nsec(unquoted)
        } else {
            Self::from_hex(unquoted)
        }
    }

    /// The agent's x-only public key — its workspace identity.
    #[must_use]
    pub const fn pubkey(&self) -> PubKey {
        self.pubkey
    }

    /// The public key as lowercase hex, which is how tags and filters spell it.
    #[must_use]
    pub fn pubkey_hex(&self) -> String {
        to_hex(&self.pubkey)
    }

    /// The public key as a NIP-19 `npub`, which is how humans read it.
    ///
    /// Computed once at construction; this is a borrow, not a re-encode.
    #[must_use]
    pub fn npub(&self) -> &str {
        &self.npub
    }

    /// Sign a new event.
    ///
    /// `created_at` is a parameter rather than a clock read because the SDK
    /// keeps time in one place ([`crate::workspace::Clock`]) — that is what lets
    /// a test drive an agent at a fixed instant and get byte-identical events.
    ///
    /// Signing is deterministic (see [`buzz::relay::event::Event::sign_new`]), so
    /// signing the same fields twice yields the same event id: a retried publish
    /// is a duplicate the relay absorbs, never a second message.
    ///
    /// Cost: one SHA-256 over the canonical form plus one scalar
    /// multiplication.
    ///
    /// # Errors
    ///
    /// [`SdkError::Protocol`] if the finished event would exceed the relay's
    /// serialized-size bound, or if signing fails.
    pub fn sign(
        &self,
        kind: Kind,
        tags: Vec<Vec<String>>,
        content: &str,
        created_at: u64,
    ) -> Result<Event> {
        Event::sign_new(&self.secret, kind, tags, content, created_at).map_err(SdkError::Protocol)
    }

    /// Build a NIP-98 `Authorization` header value for one request.
    ///
    /// The signed kind-27235 event names the method and the URL, so the header
    /// authorizes *that* call and nothing else: a token captured from a read
    /// cannot be replayed against a write, and the relay burns the event id
    /// after one use. `created_at` must be within the relay's ±60 s window
    /// ([`buzz::relay::auth::MAX_AUTH_SKEW_SECS`]).
    ///
    /// Cost: one signature and one base64 encode per request — the price of
    /// having no session state to get wrong.
    ///
    /// # Errors
    ///
    /// [`SdkError::Protocol`] if signing fails; [`SdkError::Encoding`] if the
    /// event cannot be serialized.
    pub fn authorization_header(&self, method: &str, url: &str, created_at: u64) -> Result<String> {
        // The nonce is what makes two same-second requests to one route distinct.
        // It rides inside the signed material, so it changes the event id — which
        // is exactly what the relay's single-use guard is keyed on. NIP-98 ignores
        // tags it does not know, so this stays wire-compatible.
        let nonce = self
            .nonce
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let event = self.sign(
            KIND_HTTP_AUTH,
            vec![
                vec!["u".to_string(), url.to_string()],
                vec!["method".to_string(), method.to_ascii_uppercase()],
                vec!["nonce".to_string(), nonce.to_string()],
            ],
            "",
            created_at,
        )?;
        let json = serde_json::to_vec(&event)
            .map_err(|e| SdkError::Protocol(BuzzError::Internal(format!("auth event: {e}"))))?;
        Ok(format!("Nostr {}", b64::encode(&json)))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Two headers for the SAME route in the SAME second must differ.
    ///
    /// Regression guard for a real defect: with deterministic signing and
    /// second-granularity `created_at`, a header built from `(method, url,
    /// created_at)` alone was byte-identical across two calls, so the relay's
    /// single-use replay guard refused the second — an agent posting twice in one
    /// second lost its second post.
    #[test]
    fn same_second_headers_for_one_route_are_distinct() {
        let id = Identity::from_secret(SK).unwrap();
        let a = id
            .authorization_header("POST", "https://h/buzz/api/events", 1_700_000_000)
            .unwrap();
        let b = id
            .authorization_header("POST", "https://h/buzz/api/events", 1_700_000_000)
            .unwrap();
        assert_ne!(a, b, "two same-second headers collided");

        // And both are still valid NIP-98 headers the relay accepts, with distinct
        // event ids (which is what the replay guard is keyed on).
        let now_ms = 1_700_000_000_000;
        let (_, ev_a) =
            buzz::relay::auth::verify_header(&a, "POST", "/buzz/api/events", now_ms).unwrap();
        let (_, ev_b) =
            buzz::relay::auth::verify_header(&b, "POST", "/buzz/api/events", now_ms).unwrap();
        assert_ne!(ev_a.id, ev_b.id);
    }

    /// Clones share one counter, so two handles to one identity cannot collide.
    #[test]
    fn cloned_identities_do_not_reissue_the_same_header() {
        let id = Identity::from_secret(SK).unwrap();
        let twin = id.clone();
        let a = id
            .authorization_header("GET", "https://h/buzz/api/query", 1_700_000_000)
            .unwrap();
        let b = twin
            .authorization_header("GET", "https://h/buzz/api/query", 1_700_000_000)
            .unwrap();
        assert_ne!(a, b, "a clone reissued an identical header");
    }

    /// A fixed, obviously-test secret. BIP-340 signing here is deterministic, so
    /// every assertion below is stable.
    const SK: [u8; 32] = [
        0x11, 0x22, 0x33, 0x44, 0x55, 0x66, 0x77, 0x88, 0x99, 0xaa, 0xbb, 0xcc, 0xdd, 0xee, 0xff,
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d, 0x0e, 0x0f,
        0x10, 0x20,
    ];

    fn nsec() -> String {
        bech32::encode("nsec", &SK).unwrap()
    }

    #[test]
    fn nsec_round_trips_to_pubkey_and_npub() {
        let from_bech = Identity::from_nsec(&nsec()).unwrap();
        let from_raw = Identity::from_secret(SK).unwrap();
        let from_hex = Identity::from_hex(&to_hex(&SK)).unwrap();

        // All three spellings of one key agree.
        assert_eq!(from_bech.pubkey(), from_raw.pubkey());
        assert_eq!(from_bech.pubkey(), from_hex.pubkey());
        assert_eq!(from_bech.npub(), from_raw.npub());

        // The npub is the pubkey, not something adjacent to it.
        assert!(from_bech.npub().starts_with("npub1"));
        assert_eq!(
            bech32::decode_npub(from_bech.npub()).unwrap(),
            from_bech.pubkey()
        );
        assert_eq!(from_bech.pubkey_hex(), to_hex(&from_bech.pubkey()));
    }

    #[test]
    fn a_key_of_the_wrong_species_is_refused() {
        let npub_string = bech32::npub(&Identity::from_secret(SK).unwrap().pubkey()).unwrap();
        // An npub is 32 well-formed bytes of bech32 — only the HRP check stops
        // it being loaded as a secret key.
        assert!(Identity::from_nsec(&npub_string).is_err());
        assert!(Identity::from_nsec("nsec1notvalidbech32").is_err());
        assert!(Identity::from_hex("deadbeef").is_err());
        assert!(Identity::from_secret([0u8; 32]).is_err());
    }

    #[test]
    fn hex_and_nsec_tolerate_surrounding_whitespace() {
        let padded = format!("  {}\n", to_hex(&SK));
        assert_eq!(
            Identity::from_hex(&padded).unwrap().pubkey(),
            Identity::from_secret(SK).unwrap().pubkey()
        );
        let padded_nsec = format!("\t{}\r\n", nsec());
        assert!(Identity::from_nsec(&padded_nsec).is_ok());
    }

    #[test]
    fn debug_never_prints_the_secret() {
        let id = Identity::from_secret(SK).unwrap();
        let rendered = format!("{id:?}");
        assert!(rendered.contains("redacted"));
        assert!(!rendered.contains(&to_hex(&SK)));
        assert!(!rendered.contains("1122"));
    }

    #[test]
    fn signed_events_verify_and_are_deterministic() {
        let id = Identity::from_secret(SK).unwrap();
        let a = id
            .sign(9, vec![vec!["h".into(), "general".into()]], "hello", 1_700)
            .unwrap();
        let b = id
            .sign(9, vec![vec!["h".into(), "general".into()]], "hello", 1_700)
            .unwrap();
        a.verify().unwrap();
        assert_eq!(a, b, "re-signing the same fields must be byte-identical");
        assert_eq!(a.pubkey, id.pubkey());
    }

    /// The header the HTTP transport puts on every request must satisfy the
    /// relay's own verifier — including its method and path binding.
    #[test]
    fn nip98_headers_verify_against_the_relay() {
        let id = Identity::from_secret(SK).unwrap();
        let header = id
            .authorization_header("post", "https://buzz.example.com/buzz/api/events", 1_000)
            .unwrap();

        let (caller, event) =
            buzz::relay::auth::verify_header(&header, "POST", "/buzz/api/events", 1_000_000)
                .unwrap();
        assert_eq!(caller.pubkey, id.pubkey());
        assert_eq!(event.kind, KIND_HTTP_AUTH);

        // Bound to the path it was minted for...
        assert!(
            buzz::relay::auth::verify_header(&header, "POST", "/buzz/api/query", 1_000_000)
                .is_err()
        );
        // ...and to the method.
        assert!(
            buzz::relay::auth::verify_header(&header, "GET", "/buzz/api/events", 1_000_000)
                .is_err()
        );
        // ...and fresh only inside the relay's skew window.
        assert!(
            buzz::relay::auth::verify_header(&header, "POST", "/buzz/api/events", 1_100_000)
                .is_err()
        );
    }

    #[test]
    fn nip98_headers_carry_a_query_string_without_breaking_the_binding() {
        let id = Identity::from_secret(SK).unwrap();
        let header = id
            .authorization_header("GET", "https://buzz.example.com/buzz/api/query?limit=5", 42)
            .unwrap();
        assert!(buzz::relay::auth::verify_header(
            &header,
            "GET",
            "/buzz/api/query?limit=9",
            42_000
        )
        .is_ok());
    }

    #[test]
    fn from_env_reads_both_encodings_and_names_a_missing_variable() {
        // Distinct variable names per assertion: env is process-global and the
        // test harness is threaded.
        std::env::set_var("BUZZ_SDK_TEST_KEY_NSEC", nsec());
        std::env::set_var("BUZZ_SDK_TEST_KEY_HEX", format!("\"{}\"", to_hex(&SK)));
        std::env::set_var("BUZZ_SDK_TEST_KEY_EMPTY", "   ");

        let expected = Identity::from_secret(SK).unwrap().pubkey();
        assert_eq!(
            Identity::from_env("BUZZ_SDK_TEST_KEY_NSEC")
                .unwrap()
                .pubkey(),
            expected
        );
        assert_eq!(
            Identity::from_env("BUZZ_SDK_TEST_KEY_HEX")
                .unwrap()
                .pubkey(),
            expected,
            "a quoted hex value from a .env file must still load"
        );

        let empty = Identity::from_env("BUZZ_SDK_TEST_KEY_EMPTY").unwrap_err();
        assert!(matches!(empty, SdkError::Env { .. }));

        let missing = Identity::from_env("BUZZ_SDK_TEST_KEY_ABSENT").unwrap_err();
        assert!(
            missing.to_string().contains("BUZZ_SDK_TEST_KEY_ABSENT"),
            "the error must name the variable: {missing}"
        );
    }
}
