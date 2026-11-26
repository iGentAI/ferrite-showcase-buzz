//! # buzz-desktop-core — the shell's logic, without the shell
//!
//! Everything the desktop app decides lives here, and nothing here depends on
//! Tauri. That split is not tidiness: the decisions worth testing — which
//! commands the shell owns, whether a path is inside the repositories
//! directory, what `git status` actually said, whether an agent may run a tool
//! — are then testable with `cargo test` on any machine, with no webview, no
//! display and no OS keychain.
//!
//! The Tauri crate beside this one is wiring: it turns each function here into
//! a `#[tauri::command]`, injects the registry marker before the bundle loads,
//! and forwards permission requests to a dialog. If a bug is ever interesting,
//! it is in this crate.
//!
//! ## The modules
//!
//! | module | what it owns |
//! |---|---|
//! | [`registry`] | Contract C: which commands route native, and the marker document |
//! | [`paths`] | the repositories directory, and what a safe path under it is |
//! | [`git`] | running `git`, and parsing what it says |
//! | [`project`] | the wire shapes `projectGit.ts` invokes, built from git output |
//! | [`credentials`] | the credential store behind git pushes and harness secrets |
//! | [`acp`] | the ACP wire: framing, initialize, sessions, prompts |
//! | [`remote`] | the ACP HTTP binding: connection identity, calls, replies |
//! | [`nostr`] | an agent's own key, its signed events, and its NIP-98 header |
//! | [`bech32`] | the one place a key is shown to a person: NIP-19 `nsec` / `npub` |
//! | [`broker`] | the fail-closed permission broker |
//! | [`harness`] | local stdio harnesses and remote ACP endpoints |
//! | [`time`] | formatting an instant the way the client's wire shapes carry one |

#![forbid(unsafe_code)]

pub mod acp;
pub mod bech32;
pub mod broker;
pub mod credentials;
pub mod git;
pub mod harness;
pub mod nostr;
pub mod paths;
pub mod project;
pub mod registry;
pub mod remote;
pub mod time;

/// What went wrong. One type across the crate, because a Tauri command has one
/// error channel and a caller can only act on a few distinctions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// The request cannot be served as written. Final.
    Invalid(String),
    /// Well-formed and not permitted. Final — this is what a denied permission
    /// request and a refused credential both are.
    Refused(String),
    /// A subprocess (git, a harness) failed. Its own message is carried
    /// verbatim, because git's diagnostics are better than any paraphrase.
    Tool { program: String, detail: String },
    Internal(String),
}

impl Error {
    /// Whether retrying the identical request could ever answer differently.
    #[must_use]
    pub const fn is_final(&self) -> bool {
        matches!(self, Self::Invalid(_) | Self::Refused(_))
    }
}

impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(m) => write!(f, "invalid: {m}"),
            Self::Refused(m) => write!(f, "refused: {m}"),
            Self::Tool { program, detail } => write!(f, "{program}: {detail}"),
            Self::Internal(m) => write!(f, "error: {m}"),
        }
    }
}

impl std::error::Error for Error {}

pub type Result<T> = core::result::Result<T, Error>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finality_is_what_decides_a_retry() {
        assert!(Error::Invalid("x".into()).is_final());
        assert!(Error::Refused("x".into()).is_final());
        assert!(!Error::Tool {
            program: "git".into(),
            detail: "x".into()
        }
        .is_final());
        assert!(!Error::Internal("x".into()).is_final());
    }

    #[test]
    fn a_tool_error_carries_the_tools_own_words() {
        // git's diagnostics are better than any paraphrase, and a user who sees
        // one can act on it — so the message is passed through, not rewritten.
        let err = Error::Tool {
            program: "git".into(),
            detail: "fatal: not a git repository".into(),
        };
        assert_eq!(err.to_string(), "git: fatal: not a git repository");
    }
}
