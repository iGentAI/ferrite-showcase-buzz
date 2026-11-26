//! Errors the process cell can produce.
//!
//! Deliberately its own small type rather than a copy of the relay's
//! `BuzzError`: this app answers to a queue and a poll loop, not to an HTTP
//! status table, so the only distinctions that earn their keep are the ones a
//! caller acts on — a REFUSAL is final (say so in the room and stop), while
//! storage and egress faults are transient (leave the work undone and let
//! redelivery or the next poll retry it).

use core::fmt;

pub type Result<T> = core::result::Result<T, ProcessError>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProcessError {
    /// A document or request is malformed. Final: retrying identical bytes
    /// cannot help, so this becomes a refusal recorded in the room.
    Invalid(String),
    /// Well-formed and not permitted. Final, for the same reason.
    Refused(String),
    /// A plane call failed. Transient: the work is left undone.
    Storage(String),
    /// An egress call failed or was denied by the platform. Transient.
    Egress(String),
    /// A fault in this app's own logic.
    Internal(String),
    /// A guarded mutation whose outcome is durably unknown and whose room has
    /// been told so. Nothing will be submitted again and no result will be
    /// published, so the command is SETTLED for pagination and room cleanup:
    /// leaving it owed would hold the command walk and a retired room forever.
    Unsettled(String),
}

impl ProcessError {
    /// Whether the fault is FINAL — no retry can change the answer, so the
    /// cell records it and moves on rather than leaving work to redelivery.
    #[must_use]
    pub const fn is_final(&self) -> bool {
        matches!(self, Self::Invalid(_) | Self::Refused(_))
    }

    /// Whether this is a mutation whose unknown outcome is settled: told to
    /// the room, never retried, never answered with a result event.
    #[must_use]
    pub const fn is_settled_unknown(&self) -> bool {
        matches!(self, Self::Unsettled(_))
    }

    /// The wire token a room-facing message leads with, mirroring the relay's
    /// own reason-prefix vocabulary so a reader sees one language.
    #[must_use]
    pub const fn reason(&self) -> &'static str {
        match self {
            Self::Invalid(_) => "invalid: ",
            Self::Refused(_) => "restricted: ",
            Self::Storage(_) | Self::Egress(_) | Self::Unsettled(_) => "unavailable: ",
            Self::Internal(_) => "error: ",
        }
    }
}

impl fmt::Display for ProcessError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let detail = match self {
            Self::Invalid(m)
            | Self::Refused(m)
            | Self::Storage(m)
            | Self::Egress(m)
            | Self::Internal(m)
            | Self::Unsettled(m) => m,
        };
        write!(f, "{}{detail}", self.reason())
    }
}

impl std::error::Error for ProcessError {}

impl From<ProcessError> for std::io::Error {
    fn from(e: ProcessError) -> Self {
        Self::other(e.to_string())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finality_is_what_decides_retry() {
        assert!(ProcessError::Invalid("x".into()).is_final());
        assert!(ProcessError::Refused("x".into()).is_final());
        assert!(!ProcessError::Storage("x".into()).is_final());
        assert!(!ProcessError::Egress("x".into()).is_final());
        assert!(!ProcessError::Internal("x".into()).is_final());
        assert!(!ProcessError::Unsettled("x".into()).is_final());
        assert!(ProcessError::Unsettled("x".into()).is_settled_unknown());
        assert!(!ProcessError::Egress("x".into()).is_settled_unknown());
    }

    #[test]
    fn display_leads_with_the_relays_own_reason_token() {
        assert_eq!(
            ProcessError::Refused("not a moderator".into()).to_string(),
            "restricted: not a moderator"
        );
        assert_eq!(
            ProcessError::Invalid("bad json".into()).to_string(),
            "invalid: bad json"
        );
    }
}
