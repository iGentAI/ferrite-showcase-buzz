//! The permission broker: fail-closed, by construction.
//!
//! When a local agent wants to run a tool, ACP sends a
//! `session/request_permission` and WAITS. Upstream's harness answers it by
//! selecting the `allow_once` option automatically, which is a reasonable
//! default for a CLI someone is watching and the wrong one for an agent running
//! in a chat room: the person who would be asked is not necessarily at the
//! keyboard, and the tool call can write files or spend money.
//!
//! So this broker exists, and it is built so that the SAFE answer is the one
//! that happens when anything goes wrong:
//!
//! * a request nobody answers in time is REJECTED (not left hanging, not
//!   allowed);
//! * a dialog the user dismisses is REJECTED;
//! * the app quitting rejects everything outstanding;
//! * an answer naming the wrong session is REFUSED and the request stays open,
//!   because a mismatch means somebody is answering a question that was not
//!   asked;
//! * a second answer to one request is REFUSED, so "exactly once" is a property
//!   of the type rather than of the caller's discipline.
//!
//! Everything here is pure state plus an injected clock, so all of that is
//! tested without a dialog, an agent or a timer.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

use crate::acp::PermissionRequest;
use crate::{Error, Result};

/// How long a permission request waits for a person.
///
/// Two minutes: long enough to read a dialog and think, short enough that an
/// agent is not wedged for an afternoon because somebody walked away. The
/// expiry is a REJECTION, so waiting longer never becomes permission.
pub const DEFAULT_TIMEOUT_MS: u64 = 120_000;

/// How many permission requests one harness may hold open at once. Each is a
/// native modal dialog on its own thread, so a child that asks without
/// waiting must not stack them without bound; parallel tool calls
/// legitimately ask at once, so the cap is more than one.
pub const MAX_OPEN_PER_HARNESS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Decision {
    AllowOnce,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Resolution {
    Answered(Decision),
    TimedOut,
    Dismissed,
    Abandoned,
}

impl Resolution {
    /// Whether the tool call may proceed.
    ///
    /// Every route except an explicit `AllowOnce` answers NO — which is the
    /// whole design of this module in one function.
    #[must_use]
    pub const fn allows(self) -> bool {
        matches!(self, Self::Answered(Decision::AllowOnce))
    }

    /// What the user is told about a refusal, so a room message can say why the
    /// agent stopped rather than leaving it looking broken.
    #[must_use]
    pub const fn detail(self) -> &'static str {
        match self {
            Self::Answered(Decision::AllowOnce) => "allowed for this call",
            Self::Answered(Decision::Reject) => "refused",
            Self::TimedOut => "refused: nobody answered the permission request in time",
            Self::Dismissed => "refused: the permission dialog was dismissed",
            Self::Abandoned => "refused: the app closed before the request was answered",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    pub rpc_id: serde_json::Value,
    pub harness_id: String,
    pub request: PermissionRequest,
    pub opened_ms: u64,
    pub expires_ms: u64,
}

impl Pending {
    fn into_outcome(self, resolution: Resolution, option_id: Option<String>) -> Outcome {
        Outcome {
            rpc_id: self.rpc_id,
            harness_id: self.harness_id,
            session_id: self.request.session_id,
            resolution,
            option_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Outcome {
    pub rpc_id: serde_json::Value,
    pub harness_id: String,
    pub session_id: String,
    pub resolution: Resolution,
    /// The option to select, or `None` for a cancellation.
    pub option_id: Option<String>,
}

#[derive(Debug, Default)]
pub struct Broker {
    pending: BTreeMap<String, Pending>,
    timeout_ms: u64,
}

impl Broker {
    #[must_use]
    pub fn new() -> Self {
        Self {
            pending: BTreeMap::new(),
            timeout_ms: DEFAULT_TIMEOUT_MS,
        }
    }

    #[must_use]
    pub fn with_timeout(timeout_ms: u64) -> Self {
        Self {
            pending: BTreeMap::new(),
            timeout_ms,
        }
    }

    /// The dialog key for one request — unique across harnesses, because two
    /// harnesses can be asking at once and answering the wrong one would allow a
    /// call nobody looked at.
    #[must_use]
    pub fn key(harness_id: &str, rpc_id: &serde_json::Value) -> String {
        format!("{harness_id}#{rpc_id}")
    }

    /// Register a request and return its dialog key.
    ///
    /// A duplicate key is REFUSED rather than replacing the first: two live
    /// dialogs for one id would let one answer resolve the other. A harness
    /// that already holds [`MAX_OPEN_PER_HARNESS`] open requests is refused
    /// too, so a child cannot stack dialogs and their threads without bound.
    pub fn open(
        &mut self,
        harness_id: &str,
        rpc_id: serde_json::Value,
        request: PermissionRequest,
        now_ms: u64,
    ) -> Result<String> {
        let key = Self::key(harness_id, &rpc_id);
        if self.pending.contains_key(&key) {
            return Err(Error::Invalid(format!(
                "a permission request is already open for {key}"
            )));
        }
        let held = self
            .pending
            .values()
            .filter(|pending| pending.harness_id == harness_id)
            .count();
        if held >= MAX_OPEN_PER_HARNESS {
            return Err(Error::Invalid(format!(
                "{harness_id} already has {held} permission requests awaiting a decision"
            )));
        }
        self.pending.insert(
            key.clone(),
            Pending {
                rpc_id,
                harness_id: harness_id.to_string(),
                request,
                opened_ms: now_ms,
                expires_ms: now_ms.saturating_add(self.timeout_ms),
            },
        );
        Ok(key)
    }

    #[must_use]
    pub fn pending(&self) -> Vec<&Pending> {
        self.pending.values().collect()
    }

    #[must_use]
    pub fn is_pending(&self, key: &str) -> bool {
        self.pending.contains_key(key)
    }

    /// Resolve a request with a person's choice.
    ///
    /// `session_id` is checked against the request's own: a dialog answering a
    /// different session is refused and the request stays open.
    pub fn answer(
        &mut self,
        key: &str,
        session_id: &str,
        decision: Decision,
        now_ms: u64,
    ) -> Result<Outcome> {
        let Some(pending) = self.pending.get(key) else {
            return Err(Error::Invalid(format!(
                "no permission request is open for {key}; it may have expired"
            )));
        };
        if pending.request.session_id != session_id {
            return Err(Error::Refused(format!(
                "that answer names session {session_id}, but the request is for {}",
                pending.request.session_id
            )));
        }
        let resolved = self.pending.remove(key).expect("just looked it up");
        // An answer arriving after the deadline is a timeout, never an allowance.
        if now_ms >= resolved.expires_ms {
            return Ok(resolved.into_outcome(Resolution::TimedOut, None));
        }
        let option_id = match decision {
            Decision::AllowOnce => {
                // Only an option whose kind is `allow_once` counts; if the agent
                // offered none, refuse rather than selecting a broader grant.
                match resolved.request.allow_once_option().map(str::to_string) {
                    Some(option) => Some(option),
                    None => {
                        return Ok(resolved.into_outcome(Resolution::Answered(Decision::Reject), None))
                    }
                }
            }
            Decision::Reject => None,
        };
        Ok(resolved.into_outcome(Resolution::Answered(decision), option_id))
    }

    pub fn dismiss(&mut self, key: &str) -> Result<Outcome> {
        let Some(resolved) = self.pending.remove(key) else {
            return Err(Error::Invalid(format!(
                "no permission request is open for {key}"
            )));
        };
        Ok(resolved.into_outcome(Resolution::Dismissed, None))
    }

    pub fn expire(&mut self, now_ms: u64) -> Vec<Outcome> {
        self.drain_where(|pending| now_ms >= pending.expires_ms, Resolution::TimedOut)
    }

    /// Refuse everything outstanding when the app closes: an explicit
    /// cancellation lets the agent finish its turn rather than waiting on a pipe.
    pub fn abandon_all(&mut self) -> Vec<Outcome> {
        std::mem::take(&mut self.pending)
            .into_values()
            .map(|pending| pending.into_outcome(Resolution::Abandoned, None))
            .collect()
    }

    pub fn abandon_harness(&mut self, harness_id: &str) -> Vec<Outcome> {
        self.drain_where(|pending| pending.harness_id == harness_id, Resolution::Abandoned)
    }

    fn drain_where(
        &mut self,
        predicate: impl Fn(&Pending) -> bool,
        resolution: Resolution,
    ) -> Vec<Outcome> {
        let keys: Vec<String> = self
            .pending
            .iter()
            .filter(|(_, pending)| predicate(pending))
            .map(|(key, _)| key.clone())
            .collect();
        keys.into_iter()
            .filter_map(|key| self.pending.remove(&key))
            .map(|pending| pending.into_outcome(resolution, None))
            .collect()
    }
}

#[must_use]
pub fn response_for(outcome: &Outcome) -> serde_json::Value {
    crate::acp::permission_response(&outcome.rpc_id, outcome.option_id.as_deref())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::acp::PermissionOption;

    fn request(session: &str) -> PermissionRequest {
        PermissionRequest {
            session_id: session.to_string(),
            tool_name: "Write src/lib.rs".to_string(),
            title: Some("Write src/lib.rs".to_string()),
            options: vec![
                PermissionOption {
                    id: "reject".into(),
                    name: "No".into(),
                    kind: "reject_once".into(),
                },
                PermissionOption {
                    id: "allow".into(),
                    name: "Yes".into(),
                    kind: "allow_once".into(),
                },
            ],
        }
    }

    #[test]
    fn only_an_explicit_allowance_allows() {
        // The whole module in one assertion.
        assert!(Resolution::Answered(Decision::AllowOnce).allows());
        for refused in [
            Resolution::Answered(Decision::Reject),
            Resolution::TimedOut,
            Resolution::Dismissed,
            Resolution::Abandoned,
        ] {
            assert!(!refused.allows(), "{refused:?} must not allow a tool call");
            assert!(refused.detail().contains("refused"));
        }
    }

    #[test]
    fn an_allowed_request_selects_the_agents_own_allow_option() {
        let mut broker = Broker::new();
        let key = broker
            .open("claude", serde_json::json!(1), request("s1"), 0)
            .expect("open");
        assert!(broker.is_pending(&key));
        let outcome = broker
            .answer(&key, "s1", Decision::AllowOnce, 1_000)
            .expect("answer");
        assert!(outcome.resolution.allows());
        assert_eq!(outcome.option_id.as_deref(), Some("allow"));
        assert!(!broker.is_pending(&key), "an answered request is no longer pending");

        let frame = response_for(&outcome);
        assert_eq!(frame["result"]["outcome"]["outcome"], "selected");
        assert_eq!(frame["result"]["outcome"]["optionId"], "allow");
    }

    #[test]
    fn a_refusal_cancels_rather_than_selecting_anything() {
        let mut broker = Broker::new();
        let key = broker
            .open("claude", serde_json::json!(2), request("s1"), 0)
            .expect("open");
        let outcome = broker
            .answer(&key, "s1", Decision::Reject, 10)
            .expect("answer");
        assert!(!outcome.resolution.allows());
        assert!(outcome.option_id.is_none());
        assert_eq!(
            response_for(&outcome)["result"]["outcome"]["outcome"],
            "cancelled"
        );
    }

    #[test]
    fn a_request_offering_no_allow_once_cannot_be_allowed() {
        // An agent that offers only a standing grant must not have one selected
        // on the user's behalf: "allow this once" and "allow always" are
        // different decisions, and only the person can make the second.
        let mut broker = Broker::new();
        let mut only_always = request("s1");
        only_always.options = vec![PermissionOption {
            id: "always".into(),
            name: "Always".into(),
            kind: "allow_always".into(),
        }];
        let key = broker
            .open("claude", serde_json::json!(3), only_always, 0)
            .expect("open");
        let outcome = broker
            .answer(&key, "s1", Decision::AllowOnce, 10)
            .expect("answer");
        assert!(!outcome.resolution.allows(), "fail closed");
        assert!(outcome.option_id.is_none());
    }

    #[test]
    fn a_timeout_is_a_refusal_and_a_late_answer_cannot_undo_it() {
        let mut broker = Broker::with_timeout(1_000);
        let key = broker
            .open("claude", serde_json::json!(4), request("s1"), 0)
            .expect("open");

        // Nothing expires early.
        assert!(broker.expire(999).is_empty());

        // A late ALLOW is still a timeout: the agent has already been told no,
        // and a second answer would contradict the first.
        let outcome = broker
            .answer(&key, "s1", Decision::AllowOnce, 5_000)
            .expect("answer");
        assert_eq!(outcome.resolution, Resolution::TimedOut);
        assert!(!outcome.resolution.allows());
        assert!(!broker.is_pending(&key));
    }

    #[test]
    fn expiry_sweeps_every_overdue_request() {
        let mut broker = Broker::with_timeout(100);
        broker
            .open("claude", serde_json::json!(5), request("s1"), 0)
            .expect("open");
        broker
            .open("codex", serde_json::json!(5), request("s2"), 50)
            .expect("open");
        let expired = broker.expire(1_000);
        assert_eq!(expired.len(), 2);
        assert!(expired.iter().all(|o| o.resolution == Resolution::TimedOut));
        assert!(broker.pending().is_empty());
    }

    #[test]
    fn two_harnesses_asking_at_once_get_separate_dialogs() {
        // The same JSON-RPC id from two harnesses is two different questions;
        // one key for both would let an answer resolve the wrong one.
        let mut broker = Broker::new();
        let a = broker
            .open("claude", serde_json::json!(1), request("s1"), 0)
            .expect("open");
        let b = broker
            .open("codex", serde_json::json!(1), request("s2"), 0)
            .expect("open");
        assert_ne!(a, b);
        broker.answer(&a, "s1", Decision::AllowOnce, 1).expect("answer");
        assert!(broker.is_pending(&b), "the other harness is still waiting");
    }

    #[test]
    fn a_duplicate_open_is_refused() {
        let mut broker = Broker::new();
        broker
            .open("claude", serde_json::json!(1), request("s1"), 0)
            .expect("open");
        assert!(broker
            .open("claude", serde_json::json!(1), request("s1"), 0)
            .is_err());
    }

    #[test]
    fn one_harness_cannot_hold_more_than_its_share_of_open_requests() {
        // Each open request is a native modal dialog on its own thread, so a
        // child that asks without waiting must not stack them without bound.
        // Parallel tool calls legitimately ask at once, so the cap is not one.
        let mut broker = Broker::new();
        for id in 0..MAX_OPEN_PER_HARNESS {
            broker
                .open("claude", serde_json::json!(id), request("s1"), 0)
                .expect("within the cap");
        }
        let over = broker.open("claude", serde_json::json!("over"), request("s1"), 0);
        assert!(matches!(over, Err(Error::Invalid(_))), "{over:?}");
        broker
            .open("codex", serde_json::json!(0), request("s2"), 0)
            .expect("another harness has its own share");
        let first = Broker::key("claude", &serde_json::json!(0));
        broker.answer(&first, "s1", Decision::AllowOnce, 1).expect("answer");
        broker
            .open("claude", serde_json::json!("over"), request("s1"), 1)
            .expect("an answered request frees its place");
    }

    #[test]
    fn an_answer_for_the_wrong_session_is_refused_and_leaves_it_open() {
        let mut broker = Broker::new();
        let key = broker
            .open("claude", serde_json::json!(6), request("s1"), 0)
            .expect("open");
        let refused = broker.answer(&key, "s2", Decision::AllowOnce, 1);
        assert!(matches!(refused, Err(Error::Refused(_))));
        assert!(
            broker.is_pending(&key),
            "a mismatched answer must not resolve the request"
        );
    }

    #[test]
    fn answering_twice_is_refused() {
        let mut broker = Broker::new();
        let key = broker
            .open("claude", serde_json::json!(7), request("s1"), 0)
            .expect("open");
        broker.answer(&key, "s1", Decision::AllowOnce, 1).expect("first");
        assert!(
            broker.answer(&key, "s1", Decision::AllowOnce, 2).is_err(),
            "exactly one response per request"
        );
    }

    #[test]
    fn dismissal_and_shutdown_both_refuse() {
        let mut broker = Broker::new();
        let key = broker
            .open("claude", serde_json::json!(8), request("s1"), 0)
            .expect("open");
        let dismissed = broker.dismiss(&key).expect("dismiss");
        assert_eq!(dismissed.resolution, Resolution::Dismissed);
        assert!(!dismissed.resolution.allows());

        broker
            .open("claude", serde_json::json!(9), request("s1"), 0)
            .expect("open");
        broker
            .open("codex", serde_json::json!(9), request("s2"), 0)
            .expect("open");
        let abandoned = broker.abandon_all();
        assert_eq!(abandoned.len(), 2);
        assert!(abandoned.iter().all(|o| !o.resolution.allows()));
        assert!(broker.pending().is_empty());
    }

    #[test]
    fn a_harness_exiting_refuses_only_its_own_requests() {
        let mut broker = Broker::new();
        let mine = broker
            .open("claude", serde_json::json!(1), request("s1"), 0)
            .expect("open");
        let theirs = broker
            .open("codex", serde_json::json!(1), request("s2"), 0)
            .expect("open");
        let refused = broker.abandon_harness("claude");
        assert_eq!(refused.len(), 1);
        assert_eq!(refused[0].harness_id, "claude");
        assert!(!broker.is_pending(&mine));
        assert!(broker.is_pending(&theirs));
    }
}
