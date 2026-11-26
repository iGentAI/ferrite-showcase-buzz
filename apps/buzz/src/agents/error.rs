//! Foundational agent types.
//!
//! * `thiserror` is a banned dependency, so [`AgentError`]'s `Display` is
//!   hand-rolled.
//! * The constrained surface offers no wall clock, and `EventDoc::ts_ms` is
//!   metadata no logic reads, so events carry `ts_ms: 0` until the platform
//!   stages virtual-clock injection (see the `ferrite-app` time-seam doc).

use std::fmt;
use std::io;

use ferrite_app::serde::{Deserialize, Serialize};

/// Bounded guard-retry rounds per transition (extreme contention surfaces as
/// [`AgentError::Conflict`] rather than livelocking).
pub const GUARD_RETRIES: usize = 64;
/// Maximum bytes of a scope field.
pub const MAX_SCOPE_FIELD_BYTES: usize = 128;
/// Maximum bytes of the session state blob.
pub const MAX_STATE_BYTES: usize = 256 * 1024;
/// Maximum bytes of one event's data.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;

const SEP: u8 = 0x1f;

/// The closed agent error vocabulary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub enum AgentError {
    /// A scope field failed validation.
    ScopeInvalid { reason: String },
    /// A transition was attempted on a session that was never opened.
    SessionNotOpen,
    /// A record field exceeded a bound.
    RecordInvalid { reason: String },
    /// Guard retries were exhausted under extreme contention.
    Conflict,
    /// The tool loop exceeded its step budget (a runaway loop fails closed).
    StepBudget { max: usize },
    /// A plane operation failed.
    Plane { message: String },
}

impl fmt::Display for AgentError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentError::ScopeInvalid { reason } => write!(f, "scope invalid: {reason}"),
            AgentError::SessionNotOpen => write!(f, "session not open"),
            AgentError::RecordInvalid { reason } => write!(f, "record invalid: {reason}"),
            AgentError::Conflict => write!(f, "conflict: guard retries exhausted"),
            AgentError::StepBudget { max } => write!(f, "tool loop exceeded {max} steps"),
            AgentError::Plane { message } => write!(f, "plane error: {message}"),
        }
    }
}

impl std::error::Error for AgentError {}

impl AgentError {
    /// The stable wire code.
    pub fn code(&self) -> &'static str {
        match self {
            AgentError::ScopeInvalid { .. } => "SCOPE_INVALID",
            AgentError::SessionNotOpen => "SESSION_NOT_OPEN",
            AgentError::RecordInvalid { .. } => "RECORD_INVALID",
            AgentError::Conflict => "CONFLICT",
            AgentError::StepBudget { .. } => "STEP_BUDGET",
            AgentError::Plane { .. } => "PLANE",
        }
    }
}

impl From<io::Error> for AgentError {
    fn from(e: io::Error) -> Self {
        AgentError::Plane {
            message: e.to_string(),
        }
    }
}

/// The crate-standard result.
pub type Result<T> = std::result::Result<T, AgentError>;

/// Encode a document to its canonical wire form (JSON). Local re-expression of
/// `ferrite-client`'s `encode_doc` over the re-exported `serde_json` — a 2-line
/// helper, not a system, kept local so the constrained crate depends only on
/// the guaranteed `ferrite_app::serde_json` surface.
pub fn encode_doc<T: Serialize>(doc: &T) -> io::Result<Vec<u8>> {
    ferrite_app::serde_json::to_vec(doc)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// Decode a document from its canonical wire form. A structurally invalid or
/// schema-mismatched payload is a loud `InvalidData` — never a silent parse.
pub fn decode_doc<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> io::Result<T> {
    ferrite_app::serde_json::from_slice(bytes)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e.to_string()))
}

/// A validated `(tenant, agent, thread)` session scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Scope {
    pub tenant: String,
    pub agent: String,
    /// The conversation thread id.
    pub thread: String,
}

impl Scope {
    /// Validate and build a scope.
    pub fn new(
        tenant: impl Into<String>,
        agent: impl Into<String>,
        thread: impl Into<String>,
    ) -> Result<Self> {
        let tenant = tenant.into();
        let agent = agent.into();
        let thread = thread.into();
        validate_field("tenant", &tenant)?;
        validate_field("agent", &agent)?;
        validate_field("thread", &thread)?;
        Ok(Scope {
            tenant,
            agent,
            thread,
        })
    }
}

pub(crate) fn validate_field(name: &str, f: &str) -> Result<()> {
    if f.is_empty() {
        return Err(AgentError::ScopeInvalid {
            reason: format!("{name} empty"),
        });
    }
    if f.len() > MAX_SCOPE_FIELD_BYTES {
        return Err(AgentError::ScopeInvalid {
            reason: format!("{name} too long"),
        });
    }
    for &b in f.as_bytes() {
        let ok = b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b':' | b'/');
        if !ok {
            return Err(AgentError::ScopeInvalid {
                reason: format!("{name} charset"),
            });
        }
    }
    Ok(())
}

/// The session snapshot: opaque app state, a monotone transition `seq`, the
/// number of completed turns, and a status line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct SessionDoc {
    /// Opaque agent state (e.g. folded JSON).
    pub state: Vec<u8>,
    /// Monotone transition sequence (== number of committed events).
    pub seq: u64,
    /// Number of admitted messages (turns).
    pub turns: u64,
    /// Status line (`open` | `running` | `idle`).
    pub status: String,
}

/// One turn event in the session's history.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct EventDoc {
    /// The transition seq this event was committed at.
    pub seq: u64,
    /// The event kind (`message_in` | `tool_call` | `tool_result` | `message_out` | …).
    pub kind: String,
    pub data: Vec<u8>,
    /// Commit time (epoch ms). Always `0` on the constrained surface — no wall
    /// clock is available in-sandbox; this field is metadata only (never read
    /// for logic).
    pub ts_ms: u64,
}

fn scoped(prefix: &[u8], scope: &Scope) -> Vec<u8> {
    let mut k = Vec::new();
    k.extend_from_slice(prefix);
    k.push(SEP);
    k.extend_from_slice(scope.tenant.as_bytes());
    k.push(SEP);
    k.extend_from_slice(scope.agent.as_bytes());
    k.push(SEP);
    k.extend_from_slice(scope.thread.as_bytes());
    k
}

pub(crate) fn sess_key(scope: &Scope) -> Vec<u8> {
    scoped(b"sess", scope)
}

pub(crate) fn msgmark_key(scope: &Scope, msg_id: &str) -> Vec<u8> {
    let mut k = scoped(b"msgmark", scope);
    k.push(SEP);
    k.extend_from_slice(msg_id.as_bytes());
    k
}

pub(crate) fn turn_key(scope: &Scope, seq: u64) -> Vec<u8> {
    let mut k = scoped(b"turn", scope);
    k.push(SEP);
    k.extend_from_slice(format!("{seq:020}").as_bytes());
    k
}

pub(crate) fn step_key(scope: &Scope, step_id: &str) -> Vec<u8> {
    let mut k = scoped(b"step", scope);
    k.push(SEP);
    k.extend_from_slice(step_id.as_bytes());
    k
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_validation_and_isolation() {
        assert!(Scope::new("t", "agentA", "thr:1").is_ok());
        assert!(matches!(
            Scope::new("t", "a\u{1f}x", "thr"),
            Err(AgentError::ScopeInvalid { .. })
        ));
        let a = Scope::new("t", "a", "thr").unwrap();
        let b = Scope::new("t", "ab", "thr").unwrap();
        assert_ne!(sess_key(&a), sess_key(&b));
    }

    #[test]
    fn turn_keys_sort_by_seq() {
        let s = Scope::new("t", "a", "thr").unwrap();
        assert!(turn_key(&s, 2) > turn_key(&s, 1));
        assert!(turn_key(&s, 11) > turn_key(&s, 2)); // zero-padded ordering
    }

    #[test]
    fn error_display_and_code_match_original() {
        assert_eq!(
            AgentError::Conflict.to_string(),
            "conflict: guard retries exhausted"
        );
        assert_eq!(
            AgentError::StepBudget { max: 24 }.to_string(),
            "tool loop exceeded 24 steps"
        );
        assert_eq!(AgentError::SessionNotOpen.code(), "SESSION_NOT_OPEN");
        assert_eq!(
            AgentError::Plane {
                message: "x".into()
            }
            .code(),
            "PLANE"
        );
    }
}
