//! The one error type an SDK call can return.
//!
//! An agent's `main` is usually `-> buzz_sdk::Result<()>`, and the first two
//! lines of it are `std::env::var(...)?` and `Identity::from_env(...)?`. So
//! [`SdkError`] converts from the errors those calls produce ([`std::env::VarError`],
//! [`buzz::relay::BuzzError`], [`std::io::Error`], `serde_json::Error`) — the `?`
//! operator has to work across all of them in one function or the twenty-line
//! agent stops being twenty lines.
//!
//! Variants are grouped by *what the caller can do about it*, not by which
//! module raised them:
//!
//! * [`SdkError::Config`] / [`SdkError::Identity`] / [`SdkError::Env`] — a
//!   startup mistake. Retrying cannot help; fix the environment.
//! * [`SdkError::Protocol`] — the relay refused the event or read. The
//!   [`buzz::relay::BuzzError`] inside carries the protocol's own reason token.
//! * [`SdkError::Transport`] / [`SdkError::Http`] / [`SdkError::Io`] — the
//!   round trip failed. A retry may work.
//! * [`SdkError::Model`] / [`SdkError::Tool`] — the model or a tool failed.
//! * [`SdkError::Encoding`] — something on the wire did not parse.
//!
//! Cost: every variant owns its message (one small allocation on the error
//! path), so no error borrows from a buffer the caller may already have dropped.

use core::fmt;

/// Everything the SDK can fail with.
///
/// `#[non_exhaustive]`: matching on it must carry a `_` arm, so a later variant
/// is not a breaking change for downstream agents.
#[derive(Debug)]
#[non_exhaustive]
pub enum SdkError {
    /// A builder was asked to `connect()`/`run()` without something it needs
    /// (no identity, no transport, no backend), or a value it was given cannot
    /// be used (an unusable URL).
    Config(String),
    /// A key could not be read: bad bech32, wrong human-readable part, bad hex,
    /// or a scalar outside the secp256k1 group.
    ///
    /// The message never contains key material.
    Identity(String),
    /// An environment variable the caller named is missing or not UTF-8.
    Env {
        /// The variable that was read. `""` when the failure arrived through
        /// `From<std::env::VarError>`, which does not carry the name.
        var: String,
        /// Why it could not be used.
        message: String,
    },
    /// The relay refused, or a protocol operation (signing, verification,
    /// canonicalization) failed. Carries the relay's own typed error.
    Protocol(buzz::relay::BuzzError),
    /// The round trip did not complete: connection refused, timed out, stream
    /// ended, or the frame pump died.
    Transport(String),
    /// A request completed with a non-success status.
    Http {
        /// The HTTP status.
        status: u16,
        /// The response body, truncated to a bounded length so a large error
        /// page cannot be carried around whole.
        body: String,
    },
    /// The model call failed, or the model declined the request.
    Model(String),
    /// A tool could not be run: arguments did not deserialize into its
    /// argument type, or the tool returned an error.
    Tool {
        /// The tool's registered name.
        tool: String,
        /// What went wrong.
        message: String,
    },
    /// Something on the wire did not parse (JSON, SSE framing, base64).
    Encoding(String),
    /// A filesystem or process I/O failure — the durable cursor file is the
    /// only thing in the SDK that touches the filesystem.
    Io(String),
}

/// How much of an error body is kept. Long enough for a JSON error object,
/// short enough that an HTML error page is not carried through the call stack.
pub const MAX_BODY_CHARS: usize = 512;

impl SdkError {
    /// A configuration failure with a borrowed-or-owned message.
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    /// A transport failure with a borrowed-or-owned message.
    pub fn transport(message: impl Into<String>) -> Self {
        Self::Transport(message.into())
    }

    /// A model failure with a borrowed-or-owned message.
    pub fn model(message: impl Into<String>) -> Self {
        Self::Model(message.into())
    }

    /// An encoding failure with a borrowed-or-owned message.
    pub fn encoding(message: impl Into<String>) -> Self {
        Self::Encoding(message.into())
    }

    /// A tool failure naming the tool.
    pub fn tool(tool: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Tool {
            tool: tool.into(),
            message: message.into(),
        }
    }

    /// A non-success HTTP response, with the body clamped to
    /// [`MAX_BODY_CHARS`] characters.
    pub fn http(status: u16, body: impl AsRef<str>) -> Self {
        let body = body.as_ref();
        let clamped: String = body.chars().take(MAX_BODY_CHARS).collect();
        Self::Http {
            status,
            body: clamped,
        }
    }

    /// An environment-variable failure that knows which variable it was.
    pub fn env(var: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Env {
            var: var.into(),
            message: message.into(),
        }
    }

    /// Whether a retry of the same call could plausibly succeed.
    ///
    /// Transport failures, 5xx, 429 and I/O errors are retryable; a refused
    /// event, a bad key, or a 4xx is not. Advisory: the SDK never retries on a
    /// caller's behalf except where a doc comment says so.
    #[must_use]
    pub const fn is_retryable(&self) -> bool {
        match self {
            Self::Transport(_) | Self::Io(_) => true,
            Self::Http { status, .. } => *status == 429 || *status >= 500,
            Self::Protocol(
                buzz::relay::BuzzError::RateLimited | buzz::relay::BuzzError::Storage(_),
            ) => true,
            _ => false,
        }
    }
}

impl fmt::Display for SdkError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Config(m) => write!(f, "configuration: {m}"),
            Self::Identity(m) => write!(f, "identity: {m}"),
            Self::Env { var, message } if var.is_empty() => {
                write!(f, "environment: {message}")
            }
            Self::Env { var, message } => write!(f, "environment variable {var}: {message}"),
            Self::Protocol(e) => write!(f, "relay: {e}"),
            Self::Transport(m) => write!(f, "transport: {m}"),
            Self::Http { status, body } => write!(f, "http {status}: {body}"),
            Self::Model(m) => write!(f, "model: {m}"),
            Self::Tool { tool, message } => write!(f, "tool {tool}: {message}"),
            Self::Encoding(m) => write!(f, "encoding: {m}"),
            Self::Io(m) => write!(f, "io: {m}"),
        }
    }
}

impl std::error::Error for SdkError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Protocol(e) => Some(e),
            _ => None,
        }
    }
}

/// `std::env::VarError` carries no variable name, so the name is lost here.
/// [`crate::Identity::from_env`] and friends build [`SdkError::Env`] directly to
/// keep it.
impl From<std::env::VarError> for SdkError {
    fn from(e: std::env::VarError) -> Self {
        Self::Env {
            var: String::new(),
            message: e.to_string(),
        }
    }
}

impl From<buzz::relay::BuzzError> for SdkError {
    fn from(e: buzz::relay::BuzzError) -> Self {
        Self::Protocol(e)
    }
}

impl From<std::io::Error> for SdkError {
    fn from(e: std::io::Error) -> Self {
        Self::Io(e.to_string())
    }
}

impl From<serde_json::Error> for SdkError {
    fn from(e: serde_json::Error) -> Self {
        Self::Encoding(e.to_string())
    }
}

#[cfg(feature = "http")]
impl From<reqwest::Error> for SdkError {
    fn from(e: reqwest::Error) -> Self {
        Self::Transport(e.to_string())
    }
}

/// The SDK's result type. Every fallible public function returns this.
pub type Result<T> = core::result::Result<T, SdkError>;

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn env_var_error_converts_and_reads_sensibly() {
        let err: SdkError = std::env::var("BUZZ_SDK_DEFINITELY_UNSET_VARIABLE")
            .err()
            .unwrap()
            .into();
        assert!(matches!(err, SdkError::Env { .. }));
        assert!(err.to_string().starts_with("environment:"), "{err}");
    }

    #[test]
    fn relay_errors_convert_and_keep_their_reason_token() {
        let err: SdkError = buzz::relay::BuzzError::BadSignature.into();
        assert!(err.to_string().contains("invalid:"), "{err}");
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn http_bodies_are_clamped() {
        let err = SdkError::http(500, "x".repeat(MAX_BODY_CHARS * 4));
        let SdkError::Http { body, .. } = &err else {
            panic!("wrong variant");
        };
        assert_eq!(body.chars().count(), MAX_BODY_CHARS);
    }

    #[test]
    fn retryability_is_classified() {
        assert!(SdkError::transport("reset").is_retryable());
        assert!(SdkError::http(503, "").is_retryable());
        assert!(SdkError::http(429, "").is_retryable());
        assert!(!SdkError::http(401, "").is_retryable());
        assert!(!SdkError::Identity("bad key".into()).is_retryable());
        assert!(SdkError::Protocol(buzz::relay::BuzzError::RateLimited).is_retryable());
        assert!(!SdkError::Protocol(buzz::relay::BuzzError::BadSignature).is_retryable());
    }

    /// The error is carried in every `Result` the SDK returns, so it must stay
    /// small enough that clippy's `result_large_err` does not fire and that
    /// returning it is cheap.
    #[test]
    fn the_error_stays_small() {
        assert!(
            core::mem::size_of::<SdkError>() <= 128,
            "SdkError grew to {} bytes",
            core::mem::size_of::<SdkError>()
        );
    }
}
