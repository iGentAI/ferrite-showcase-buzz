//! Human approval grants for gated tools.
//!
//! A persona definition can list tools under `approvals`. The turn pipeline
//! soft-parks every call to such a tool until the thread's session log holds a
//! live grant for it (`FilteredTools` in [`crate::agents::definition`], fed by
//! [`fold_grants`] over the log the turn reads at start). This module is the
//! only writer of grants: without one, a gated tool cannot run and every attempt
//! ends in `approval_required`.
//!
//! The grant has the shape the owner chose: ONE tool on ONE thread. It lasts
//! for the life of that thread until it is REVOKED (the owner's decision: no
//! time expiry, and a revoke action). It is never widened: it does not reach
//! another tool, another thread, another persona, or a delegated child (a child
//! turn runs on its own thread, which holds no grants, so it keeps escalating).
//! Who may grant or revoke is the relay's decision
//! ([`crate::relay::agent_approvals`]); this module only records the event once
//! that decision has been made.
//!
//! # Events and cycles
//!
//! A grant is an `approval_granted` event and a revoke an `approval_revoked`
//! event, both carrying the tool's name. Replayed in log order, a grant adds
//! the tool and a revoke removes it ([`fold_grants`]), so a revoked tool is
//! back to `approval_required` from the next turn on.
//!
//! The session engine applies each step id at most once, so the step ids carry
//! a CYCLE number: cycle `n` is closed by the step `approval-revoke:{tool}:{n}`,
//! and the current cycle is the first one without it. Approving twice within a
//! cycle is absorbed (same step id), revoking twice is a no-op, and an approval
//! after a revoke opens the next cycle rather than being swallowed by the old
//! one.

use std::collections::BTreeSet;

use ferrite_app::AppContext;

use crate::agents::definition::load_agent_definition;
use crate::agents::error::{EventDoc, Scope};
use crate::agents::session::{Advanced, ApeOps, SessionEngine};

/// The session event kind a grant is recorded under, with the tool's name as
/// the event data.
pub const APPROVAL_GRANTED_KIND: &str = "approval_granted";
/// The session event kind a revoke is recorded under, with the tool's name as
/// the event data.
pub const APPROVAL_REVOKED_KIND: &str = "approval_revoked";

/// How many approve/revoke cycles one tool may go through on one thread.
/// Finding the current cycle probes one step marker per past cycle, so this
/// bounds that work; a thread that reaches it refuses further grants.
pub const MAX_APPROVAL_CYCLES: u32 = 256;

/// How often a grant is re-tried when a concurrent revoke closes the cycle it
/// was recorded into.
const GRANT_ATTEMPTS: usize = 4;

/// The turn pipeline's fixed single-tenant identity and session binding: the
/// same values `crate::agents::turns` opens a turn's session under, so a grant
/// or revoke lands in the log the next turn reads.
const TENANT: &str = "cloud";
const SESSION: &str = "agent-sessions";
/// The store the persona's definition is loaded from.
const CONFIG: &str = "config";

/// Why a grant or revoke was not recorded.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApprovalError {
    /// The persona's definition does not gate this tool (or the persona has no
    /// definition), so there is nothing to approve. Refused rather than recorded,
    /// so a grant can never sit waiting for a gate added later.
    NotGated(String),
    /// The agent or thread is not a valid session scope.
    InvalidScope(String),
    /// The tool has been approved and revoked [`MAX_APPROVAL_CYCLES`] times on
    /// this thread.
    TooManyCycles(String),
    /// The session plane refused or failed.
    Plane(String),
}

impl std::fmt::Display for ApprovalError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotGated(m) | Self::InvalidScope(m) | Self::TooManyCycles(m) | Self::Plane(m) => {
                f.write_str(m)
            }
        }
    }
}

/// The step id of the grant in `cycle`. Cannot collide with the tool loop's own
/// step ids, which start with the turn's admission id.
fn grant_step(tool: &str, cycle: u32) -> String {
    format!("approval:{tool}:{cycle}")
}

/// The step id of the revoke that closes `cycle`.
fn revoke_step(tool: &str, cycle: u32) -> String {
    format!("approval-revoke:{tool}:{cycle}")
}

fn plane(what: &str, e: impl std::fmt::Display) -> ApprovalError {
    ApprovalError::Plane(format!("session {what}: {e}"))
}

/// The tools with a live grant after replaying `events` in order: each
/// `approval_granted` adds its tool and each `approval_revoked` removes it.
/// Events of any other kind, and names that are not UTF-8, are ignored.
pub fn fold_grants(events: &[EventDoc]) -> BTreeSet<String> {
    let mut live = BTreeSet::new();
    for ev in events {
        let Ok(tool) = std::str::from_utf8(&ev.data) else {
            continue;
        };
        if ev.kind == APPROVAL_GRANTED_KIND {
            live.insert(tool.to_string());
        } else if ev.kind == APPROVAL_REVOKED_KIND {
            live.remove(tool);
        }
    }
    live
}

/// The first cycle of `tool` on this thread that no revoke has closed.
async fn current_cycle<A: ApeOps>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    tool: &str,
) -> Result<u32, ApprovalError> {
    for cycle in 0..MAX_APPROVAL_CYCLES {
        let closed = engine
            .step_seq(scope, &revoke_step(tool, cycle))
            .await
            .map_err(|e| plane("read", e))?;
        if closed.is_none() {
            return Ok(cycle);
        }
    }
    Err(ApprovalError::TooManyCycles(format!(
        "{tool:?} has been approved and revoked {MAX_APPROVAL_CYCLES} times on this thread"
    )))
}

/// Record a grant for `tool` in the current cycle of an OPEN session. Returns
/// the sequence the grant sits at (the same sequence for a repeated approval
/// within one cycle).
async fn grant_on<A: ApeOps>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    tool: &str,
) -> Result<u64, ApprovalError> {
    for _ in 0..GRANT_ATTEMPTS {
        let cycle = current_cycle(engine, scope, tool).await?;
        // Recorded atomically UNLESS this cycle's revoke already exists: the
        // session document's seq guards the commit, and a revoke advances it.
        // A grant this call applied therefore precedes any revoke of its cycle,
        // so that revoke stands and the grant is reported where it sits; only a
        // grant blocked by an earlier revoke moves on to the next cycle. A
        // post-hoc re-read here used to re-grant in the next cycle after a
        // revoke that came AFTER this grant, silently undoing it.
        let advanced = engine
            .record_event_unless(
                scope,
                &grant_step(tool, cycle),
                APPROVAL_GRANTED_KIND,
                tool.as_bytes(),
                &revoke_step(tool, cycle),
            )
            .await
            .map_err(|e| plane("record", e))?;
        match advanced {
            Advanced::Applied(seq) => return Ok(seq),
            Advanced::Blocked(_) => continue,
            Advanced::Existed(seq) => {
                // A repeated approval within one cycle: it stands only while
                // that cycle is still open.
                let closed = engine
                    .step_seq(scope, &revoke_step(tool, cycle))
                    .await
                    .map_err(|e| plane("read", e))?;
                if closed.is_none() {
                    return Ok(seq);
                }
            }
        }
    }
    Err(ApprovalError::Plane(format!(
        "the approval of {tool:?} kept racing a revoke; try again"
    )))
}

/// Record a revoke closing the current cycle of `tool`, if that cycle holds a
/// grant. Returns the revoke's sequence, or `None` when there was no live grant
/// (never approved, or already revoked), so a repeated revoke changes nothing.
async fn revoke_on<A: ApeOps>(
    engine: &mut SessionEngine<A>,
    scope: &Scope,
    tool: &str,
) -> Result<Option<u64>, ApprovalError> {
    let cycle = current_cycle(engine, scope, tool).await?;
    let granted = engine
        .step_seq(scope, &grant_step(tool, cycle))
        .await
        .map_err(|e| plane("read", e))?;
    if granted.is_none() {
        return Ok(None);
    }
    engine
        .record_event(
            scope,
            &revoke_step(tool, cycle),
            APPROVAL_REVOKED_KIND,
            tool.as_bytes(),
        )
        .await
        .map(Some)
        .map_err(|e| plane("record", e))
}

fn session_scope(agent: &str, thread: &str) -> Result<Scope, ApprovalError> {
    Scope::new(TENANT, agent, thread).map_err(|e| ApprovalError::InvalidScope(e.to_string()))
}

fn session_engine(ctx: &AppContext) -> Result<SessionEngine, ApprovalError> {
    let ape = ctx.ape(SESSION).map_err(|e| plane("plane", e))?;
    Ok(SessionEngine::new(ape))
}

/// Record that `tool` may run on `agent`'s `thread` until it is revoked.
/// Returns the session sequence the grant sits at (the same sequence for a
/// repeated approval).
///
/// The caller must already have decided the approver may grant it.
pub async fn grant_tool_approval(
    ctx: &AppContext,
    agent: &str,
    thread: &str,
    tool: &str,
) -> Result<u64, ApprovalError> {
    let def = load_agent_definition(ctx, CONFIG, agent, crate::agents::turns::MAX_STEPS)
        .await
        .map_err(|e| plane("definition", e))?;
    if !def.as_ref().is_some_and(|d| d.requires_approval(tool)) {
        return Err(ApprovalError::NotGated(format!(
            "{agent:?} does not require approval for {tool:?}, so there is nothing to approve"
        )));
    }
    let scope = session_scope(agent, thread)?;
    let mut engine = session_engine(ctx)?;
    // A grant may precede the thread's first turn (an operator approving ahead
    // of time), so the session is opened if it does not exist yet.
    engine.open(&scope).await.map_err(|e| plane("open", e))?;
    grant_on(&mut engine, &scope, tool).await
}

/// Revoke the live grant of `tool` on `agent`'s `thread`, if there is one.
/// Returns the revoke's session sequence, or `None` when nothing was live.
///
/// Unlike a grant, a revoke does not require the tool to be gated today: a
/// grant left behind after its gate was removed would come back to life if the
/// gate were added again, so it must stay revocable.
///
/// The caller must already have decided the revoker may revoke it.
pub async fn revoke_tool_approval(
    ctx: &AppContext,
    agent: &str,
    thread: &str,
    tool: &str,
) -> Result<Option<u64>, ApprovalError> {
    let scope = session_scope(agent, thread)?;
    let mut engine = session_engine(ctx)?;
    if engine
        .get(&scope)
        .await
        .map_err(|e| plane("read", e))?
        .is_none()
    {
        return Ok(None);
    }
    revoke_on(&mut engine, &scope, tool).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::session::testkit::MemApe;

    fn scope() -> Scope {
        Scope::new("cloud", "revka", "buzz-chan").unwrap()
    }

    async fn open_engine() -> SessionEngine<MemApe> {
        let mut engine = SessionEngine::new(MemApe::default());
        engine.open(&scope()).await.unwrap();
        engine
    }

    async fn live(engine: &mut SessionEngine<MemApe>) -> BTreeSet<String> {
        let snap = engine.get(&scope()).await.unwrap().unwrap();
        let events = engine.read_events(&scope(), 1, snap.seq).await.unwrap();
        fold_grants(&events)
    }

    #[test]
    fn step_ids_are_per_tool_and_per_cycle() {
        assert_eq!(grant_step("buzz_post", 0), grant_step("buzz_post", 0));
        assert_ne!(grant_step("buzz_post", 0), grant_step("buzz_post", 1));
        assert_ne!(grant_step("buzz_post", 0), grant_step("buzz_react", 0));
        assert_ne!(grant_step("buzz_post", 0), revoke_step("buzz_post", 0));
        // Cannot collide with the tool loop's own step ids, which start with the
        // turn's admission id and carry `:batch:` / `:final`.
        assert!(grant_step("x", 0).starts_with("approval:"));
        assert!(revoke_step("x", 0).starts_with("approval-revoke:"));
    }

    #[test]
    fn the_fold_replays_grants_and_revokes_in_log_order() {
        let ev = |seq, kind: &str, tool: &str| EventDoc {
            seq,
            kind: kind.to_string(),
            data: tool.as_bytes().to_vec(),
            ts_ms: 0,
        };
        let events = vec![
            ev(1, APPROVAL_GRANTED_KIND, "buzz_post"),
            ev(2, APPROVAL_GRANTED_KIND, "run_code"),
            ev(3, "tool_result", "buzz_post"),
            ev(4, APPROVAL_REVOKED_KIND, "buzz_post"),
        ];
        assert_eq!(
            fold_grants(&events),
            BTreeSet::from(["run_code".to_string()])
        );
        let mut regranted = events.clone();
        regranted.push(ev(5, APPROVAL_GRANTED_KIND, "buzz_post"));
        assert_eq!(
            fold_grants(&regranted),
            BTreeSet::from(["buzz_post".to_string(), "run_code".to_string()])
        );
    }

    #[tokio::test]
    async fn a_grant_lasts_until_revoked_and_a_later_grant_opens_a_new_cycle() {
        let mut engine = open_engine().await;
        let s = scope();

        let first = grant_on(&mut engine, &s, "buzz_post").await.unwrap();
        // A repeat within the cycle is absorbed: same sequence, no new event.
        assert_eq!(grant_on(&mut engine, &s, "buzz_post").await.unwrap(), first);
        assert!(live(&mut engine).await.contains("buzz_post"));

        let revoked = revoke_on(&mut engine, &s, "buzz_post").await.unwrap();
        assert!(revoked.is_some_and(|seq| seq > first));
        assert!(
            !live(&mut engine).await.contains("buzz_post"),
            "a revoke must end the grant"
        );
        // A second revoke has nothing left to revoke and records nothing.
        let seq_before = engine.get(&s).await.unwrap().unwrap().seq;
        assert_eq!(revoke_on(&mut engine, &s, "buzz_post").await.unwrap(), None);
        assert_eq!(engine.get(&s).await.unwrap().unwrap().seq, seq_before);

        // Approving again is a new grant, not swallowed by the revoked cycle.
        let again = grant_on(&mut engine, &s, "buzz_post").await.unwrap();
        assert!(again > first);
        assert!(live(&mut engine).await.contains("buzz_post"));
    }

    #[tokio::test]
    async fn a_revoke_reaches_only_its_own_tool() {
        let mut engine = open_engine().await;
        let s = scope();
        grant_on(&mut engine, &s, "buzz_post").await.unwrap();
        grant_on(&mut engine, &s, "run_code").await.unwrap();
        assert_eq!(
            revoke_on(&mut engine, &s, "buzz_react").await.unwrap(),
            None
        );
        revoke_on(&mut engine, &s, "run_code").await.unwrap();
        assert_eq!(
            live(&mut engine).await,
            BTreeSet::from(["buzz_post".to_string()])
        );
    }
}
