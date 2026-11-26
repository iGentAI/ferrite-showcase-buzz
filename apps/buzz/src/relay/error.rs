//! The one error type, and the client-facing vocabulary it speaks.
//!
//! Buzz answered every submission with a NIP-20 `OK` frame whose reason string
//! began with a fixed token — `invalid:`, `restricted:`, `blocked:`,
//! `rate-limited:`, `error:`, `duplicate:` — and clients branch on that token
//! (retry, re-auth, back off, or give up). The tokens are therefore *protocol*,
//! not log prose, so they live on the error type itself rather than being
//! reassembled by each caller: [`BuzzError::reason_prefix`] is the single place
//! the mapping is decided, and [`BuzzError`]'s `Display` is the wire reason.
//!
//! Three consequences shaped this module:
//!
//! * **No `thiserror`.** The platform code gate admits exactly one dependency,
//!   so `Display` and [`std::error::Error`] are hand-written.
//! * **`io::Error` conversion.** The app's serve loop returns
//!   [`std::io::Result`], so [`BuzzError`] converts into an
//!   [`std::io::Error`] that keeps the typed error as its `source()` — a caller
//!   can still downcast rather than parse a string.
//! * **Coarse on purpose.** A relay must not leak *why* a read was refused
//!   beyond the token; the detail string is written by the caller and is
//!   expected to be safe to hand back.

use core::fmt;

/// Every failure the relay can report.
///
/// Variants are grouped by the wire token they map to, not by the module that
/// raises them, because the token is what a client acts on. Adding a variant is
/// a protocol decision: it must pick one of the six existing tokens (see
/// [`BuzzError::reason_prefix`]) rather than introduce a seventh.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BuzzError {
    /// The event or request is malformed, out of spec, or references something
    /// that does not exist. Client error; retrying unchanged cannot help.
    InvalidEvent(String),
    /// BIP-340 Schnorr verification failed for the event's `sig` over its `id`.
    BadSignature,
    /// The event's `id` is not the SHA-256 of its canonical serialization, so
    /// some field was altered after signing.
    IdMismatch,
    /// The submitted body exceeds the serialized-event bound.
    TooLarge {
        /// Serialized size of the offending event, in bytes.
        got: usize,
        /// The bound that was exceeded, in bytes.
        max: usize,
    },
    /// `created_at` is too far from relay time. Signed, so it cannot be
    /// corrected by the relay; the client must fix its clock and re-sign.
    Drift {
        /// Signed skew in seconds: positive when the event is in the future.
        skew_secs: i64,
    },
    /// The request's CREDENTIAL is the problem: absent, malformed, forged, stale,
    /// already spent, or not committing to this body. HTTP 401 — re-authenticating
    /// with a fresh authorization event is exactly the right response.
    ///
    /// Split from [`BuzzError::Unauthorized`] because the two ask the caller for
    /// opposite things and the HTTP face has separate codes for them: this one
    /// means "authenticate", that one means "do not bother, you are not allowed".
    /// They share the `restricted: ` wire token on purpose — see
    /// [`BuzzError::reason_prefix`].
    Unauthenticated(String),
    /// The caller is authenticated but not permitted. HTTP 403. Distinct from
    /// [`BuzzError::Blocked`]: this is a scope or membership decision about one
    /// action, not a standing sanction on the identity.
    Unauthorized(String),
    /// The identity is under a standing sanction (ban, mute) in this community.
    Blocked(String),
    /// The addressed object does not exist, or the caller may not learn that it
    /// does. Reported with the `invalid:` token precisely so that a probe
    /// cannot distinguish "absent" from "hidden".
    NotFound(String),
    /// A guarded write lost its race beyond the retry budget, or state moved
    /// under the caller. Retryable at the transport layer.
    Conflict(String),
    /// The event was already admitted, and its effects are visible. This is a
    /// *success* for the client's intent, reported honestly as a duplicate.
    Duplicate(String),
    /// A quota was exhausted. Retryable after a back-off.
    RateLimited,
    /// A backing plane (Store, APE, topics, queue, blob) failed. Not the
    /// caller's fault and usually retryable.
    Storage(String),
    /// An invariant this crate owns was violated — an encode that cannot fail
    /// failing, a hex field that was validated on the way in. Never the
    /// caller's fault; always a bug worth an alert.
    Internal(String),
}

impl BuzzError {
    /// The wire token this error is reported under, trailing separator
    /// included, in buzz's NIP-20 vocabulary.
    ///
    /// Exactly six tokens exist and every variant maps onto one of them:
    ///
    /// | token | meaning to a client |
    /// |---|---|
    /// | `invalid: ` | the request is wrong; do not retry it unchanged |
    /// | `restricted: ` | authenticate differently, or ask for access |
    /// | `blocked: ` | a standing sanction; stop submitting |
    /// | `rate-limited: ` | back off and retry |
    /// | `error: ` | the relay failed; retry is reasonable |
    /// | `duplicate: ` | already accepted; treat as success |
    ///
    /// [`BuzzError::Unauthenticated`] and [`BuzzError::Unauthorized`] deliberately
    /// share `restricted: `. NIP-42 defines an `auth-required:` token that would
    /// fit the first more precisely, but introducing it would change what the
    /// pinned client has to understand — a protocol change, where the defect being
    /// fixed is an HTTP status. The token's own documented meaning already covers
    /// both: "authenticate differently, or ask for access".
    #[must_use]
    pub const fn reason_prefix(&self) -> &'static str {
        match self {
            Self::InvalidEvent(_)
            | Self::BadSignature
            | Self::IdMismatch
            | Self::TooLarge { .. }
            | Self::Drift { .. }
            | Self::NotFound(_) => "invalid: ",
            Self::Unauthenticated(_) | Self::Unauthorized(_) => "restricted: ",
            Self::Blocked(_) => "blocked: ",
            Self::RateLimited => "rate-limited: ",
            Self::Duplicate(_) => "duplicate: ",
            Self::Conflict(_) | Self::Storage(_) | Self::Internal(_) => "error: ",
        }
    }

    /// Whether the client may usefully retry the identical request.
    ///
    /// Signed material cannot be repaired by the relay, so every `invalid:`
    /// case is terminal for that exact event; transport-level failures are not.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        matches!(
            self,
            Self::Conflict(_) | Self::Storage(_) | Self::RateLimited
        )
    }

    /// Whether this outcome means the client's intent was already satisfied.
    #[must_use]
    pub const fn is_duplicate(&self) -> bool {
        matches!(self, Self::Duplicate(_))
    }

    /// The [`std::io::ErrorKind`] this maps onto for the serve loop.
    ///
    /// Kept narrow and stable: the gateway turns the loop's `io::Error` into a
    /// status code, so the kind is part of the HTTP contract.
    #[must_use]
    pub const fn io_kind(&self) -> std::io::ErrorKind {
        use std::io::ErrorKind as K;
        match self {
            Self::InvalidEvent(_)
            | Self::BadSignature
            | Self::IdMismatch
            | Self::TooLarge { .. }
            | Self::Drift { .. } => K::InvalidData,
            Self::NotFound(_) => K::NotFound,
            Self::Unauthenticated(_) | Self::Unauthorized(_) | Self::Blocked(_) => {
                K::PermissionDenied
            }
            Self::Conflict(_) | Self::Duplicate(_) => K::AlreadyExists,
            // No stable `ResourceBusy`; `WouldBlock` is the retry-after-me kind.
            Self::RateLimited => K::WouldBlock,
            Self::Storage(_) | Self::Internal(_) => K::Other,
        }
    }
}

/// The rendered wire reason: token, then detail. Matches buzz's `OK` frames,
/// where the token was part of the message string.
impl fmt::Display for BuzzError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.reason_prefix())?;
        match self {
            Self::InvalidEvent(m)
            | Self::Unauthenticated(m)
            | Self::Unauthorized(m)
            | Self::Blocked(m)
            | Self::Conflict(m)
            | Self::Duplicate(m)
            | Self::Internal(m) => f.write_str(m),
            Self::BadSignature => f.write_str("signature verification failed"),
            Self::IdMismatch => f.write_str("event id does not match its canonical hash"),
            Self::TooLarge { got, max } => {
                write!(f, "event is {got} bytes, over the {max} byte limit")
            }
            Self::Drift { skew_secs } => {
                write!(f, "created_at is {skew_secs}s from relay time")
            }
            Self::NotFound(m) => write!(f, "not found: {m}"),
            Self::RateLimited => f.write_str("quota exceeded"),
            Self::Storage(m) => write!(f, "storage: {m}"),
        }
    }
}

impl std::error::Error for BuzzError {}

/// Converts for the serve loop, which returns [`std::io::Result`].
///
/// The typed error is preserved as the `io::Error`'s source, so a caller can
/// `downcast_ref::<BuzzError>()` and recover the variant instead of matching on
/// the rendered string.
impl From<BuzzError> for std::io::Error {
    fn from(err: BuzzError) -> Self {
        let kind = err.io_kind();
        Self::new(kind, err)
    }
}

/// This crate's result alias.
pub type Result<T> = core::result::Result<T, BuzzError>;

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::indexing_slicing)]
mod tests {
    use super::*;

    /// Every variant, so the mapping tests are exhaustive by construction.
    fn all() -> Vec<BuzzError> {
        vec![
            BuzzError::InvalidEvent("bad tag".into()),
            BuzzError::BadSignature,
            BuzzError::IdMismatch,
            BuzzError::TooLarge {
                got: 300_000,
                max: 245_760,
            },
            BuzzError::Drift { skew_secs: -1_800 },
            BuzzError::Unauthenticated("no authorization header".into()),
            BuzzError::Unauthorized("not a member".into()),
            BuzzError::Blocked("banned".into()),
            BuzzError::NotFound("channel c1".into()),
            BuzzError::Conflict("head contended".into()),
            BuzzError::Duplicate("already admitted".into()),
            BuzzError::RateLimited,
            BuzzError::Storage("store put".into()),
            BuzzError::Internal("encode".into()),
        ]
    }

    #[test]
    fn every_error_speaks_one_of_the_six_wire_tokens() {
        const TOKENS: [&str; 6] = [
            "invalid: ",
            "restricted: ",
            "blocked: ",
            "rate-limited: ",
            "error: ",
            "duplicate: ",
        ];
        let mut seen = Vec::new();
        for e in all() {
            assert!(
                TOKENS.contains(&e.reason_prefix()),
                "{e:?} invented the token {:?}",
                e.reason_prefix()
            );
            let rendered = e.to_string();
            assert!(
                rendered.starts_with(e.reason_prefix()),
                "{rendered:?} does not begin with {:?}",
                e.reason_prefix()
            );
            seen.push(e.reason_prefix());
        }
        seen.sort_unstable();
        seen.dedup();
        assert_eq!(seen.len(), 6, "unreachable token(s): {seen:?}");
    }

    #[test]
    fn display_is_the_wire_reason_prefix_then_detail() {
        assert_eq!(
            BuzzError::InvalidEvent("bad tag".into()).to_string(),
            "invalid: bad tag"
        );
        assert_eq!(
            BuzzError::BadSignature.to_string(),
            "invalid: signature verification failed"
        );
        assert_eq!(
            BuzzError::IdMismatch.to_string(),
            "invalid: event id does not match its canonical hash"
        );
        assert_eq!(
            BuzzError::TooLarge { got: 10, max: 5 }.to_string(),
            "invalid: event is 10 bytes, over the 5 byte limit"
        );
        assert_eq!(
            BuzzError::Drift { skew_secs: -930 }.to_string(),
            "invalid: created_at is -930s from relay time"
        );
        assert_eq!(
            BuzzError::Unauthorized("not a member".into()).to_string(),
            "restricted: not a member"
        );
        assert_eq!(
            BuzzError::Unauthenticated("no authorization header".into()).to_string(),
            "restricted: no authorization header"
        );
        assert_eq!(
            BuzzError::Blocked("banned".into()).to_string(),
            "blocked: banned"
        );
        assert_eq!(
            BuzzError::NotFound("channel c1".into()).to_string(),
            "invalid: not found: channel c1"
        );
        assert_eq!(
            BuzzError::RateLimited.to_string(),
            "rate-limited: quota exceeded"
        );
        assert_eq!(
            BuzzError::Storage("store put".into()).to_string(),
            "error: storage: store put"
        );
        assert_eq!(
            BuzzError::Duplicate("already admitted".into()).to_string(),
            "duplicate: already admitted"
        );
    }

    #[test]
    fn a_not_found_is_indistinguishable_from_a_refusal_at_the_token_level() {
        // Deliberate: a prober must not learn that an id exists but is hidden.
        assert_eq!(
            BuzzError::NotFound("x".into()).reason_prefix(),
            BuzzError::InvalidEvent("x".into()).reason_prefix()
        );
    }

    /// The credential failure and the permission failure share ONE wire token, and
    /// that is a deliberate protocol decision rather than an oversight: the six
    /// tokens are what the pinned client understands, and `restricted: ` already
    /// documents both readings ("authenticate differently, or ask for access").
    /// What separates them is the HTTP status, which is decided in `ingress`.
    #[test]
    fn a_credential_failure_and_a_permission_failure_share_the_restricted_token() {
        assert_eq!(
            BuzzError::Unauthenticated("stale".into()).reason_prefix(),
            BuzzError::Unauthorized("not a member".into()).reason_prefix()
        );
        // ...while remaining distinguishable to the relay itself, which is what
        // lets the HTTP face answer 401 for one and 403 for the other.
        assert_ne!(
            BuzzError::Unauthenticated("x".into()),
            BuzzError::Unauthorized("x".into())
        );
        // Neither is retryable unchanged: a fresh credential is a different
        // request, and a permission decision will not change on a retry.
        assert!(!BuzzError::Unauthenticated("x".into()).is_retryable());
        assert!(!BuzzError::Unauthorized("x".into()).is_retryable());
    }

    #[test]
    fn only_transport_failures_are_retryable() {
        assert!(BuzzError::RateLimited.is_retryable());
        assert!(BuzzError::Storage("s".into()).is_retryable());
        assert!(BuzzError::Conflict("c".into()).is_retryable());
        // Signed material cannot be repaired by a retry.
        assert!(!BuzzError::BadSignature.is_retryable());
        assert!(!BuzzError::IdMismatch.is_retryable());
        assert!(!BuzzError::Drift { skew_secs: 1 }.is_retryable());
        assert!(!BuzzError::TooLarge { got: 2, max: 1 }.is_retryable());
        assert!(!BuzzError::Internal("i".into()).is_retryable());
    }

    #[test]
    fn duplicate_is_the_only_already_satisfied_outcome() {
        assert!(BuzzError::Duplicate("d".into()).is_duplicate());
        for e in all()
            .into_iter()
            .filter(|e| !matches!(e, BuzzError::Duplicate(_)))
        {
            assert!(!e.is_duplicate(), "{e:?}");
        }
    }

    #[test]
    fn io_conversion_keeps_the_kind_and_the_typed_source() {
        let io: std::io::Error = BuzzError::Unauthorized("nope".into()).into();
        assert_eq!(io.kind(), std::io::ErrorKind::PermissionDenied);
        let source = io
            .into_inner()
            .expect("typed error is preserved as the io::Error payload");
        let typed = source
            .downcast_ref::<BuzzError>()
            .expect("payload downcasts back to BuzzError");
        assert_eq!(typed, &BuzzError::Unauthorized("nope".into()));
    }

    #[test]
    fn io_kinds_are_stable_per_class() {
        use std::io::ErrorKind as K;
        let expect = |e: BuzzError, k: K| {
            let io: std::io::Error = e.clone().into();
            assert_eq!(io.kind(), k, "{e:?}");
        };
        expect(BuzzError::InvalidEvent("m".into()), K::InvalidData);
        expect(BuzzError::BadSignature, K::InvalidData);
        expect(BuzzError::IdMismatch, K::InvalidData);
        expect(BuzzError::TooLarge { got: 2, max: 1 }, K::InvalidData);
        expect(BuzzError::Drift { skew_secs: 1 }, K::InvalidData);
        expect(BuzzError::NotFound("m".into()), K::NotFound);
        expect(BuzzError::Unauthenticated("m".into()), K::PermissionDenied);
        expect(BuzzError::Unauthorized("m".into()), K::PermissionDenied);
        expect(BuzzError::Blocked("m".into()), K::PermissionDenied);
        expect(BuzzError::Conflict("m".into()), K::AlreadyExists);
        expect(BuzzError::Duplicate("m".into()), K::AlreadyExists);
        expect(BuzzError::RateLimited, K::WouldBlock);
        expect(BuzzError::Storage("m".into()), K::Other);
        expect(BuzzError::Internal("m".into()), K::Other);
    }
}
