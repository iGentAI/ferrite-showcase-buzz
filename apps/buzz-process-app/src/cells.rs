//! The native cell's branch observation, as a pure state machine.
//!
//! Polling is not an audit stream and this module says so out loud: it
//! converges on CURRENT state and cannot see a transient move between two
//! observations. What it CAN do exactly is decide, from two snapshots, which
//! branches appeared, advanced, or retired — and keep an incarnation counter
//! so a branch name deleted and recreated is a DIFFERENT room rather than a
//! resurrection of the old one.
//!
//! Observations and their pending effects are persisted together before
//! publication, so restart replays the recorded observation rather than
//! deriving a different effect from a newer snapshot.

use std::collections::{BTreeMap, BTreeSet};

use ferrite_app::serde::{Deserialize, Serialize};

use crate::error::{ProcessError, Result};

/// Which provider profile this deployment of the cell runs.
///
/// One source package, two isolated deployments: the native cell holds the
/// platform token and drives RCS; the GitHub cell holds a webhook secret and a
/// provider token. Neither holds the other's credential, which is the whole
/// reason they are separate apps rather than one app with a flag.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Native,
    Github,
}

impl Profile {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Github => "github",
        }
    }

    /// Parse the configured profile. An unrecognized value is an ERROR rather
    /// than a default: a typo must not silently start the wrong cell with the
    /// wrong credential expectations.
    pub fn parse(s: &str) -> Result<Self> {
        match s.trim() {
            "native" => Ok(Self::Native),
            "github" => Ok(Self::Github),
            other => Err(ProcessError::Invalid(format!(
                "process profile {other:?} is not native|github"
            ))),
        }
    }
}

/// What changed between two observations of a repository's branches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub enum BranchChange {
    /// A branch this cell has not seen at this incarnation.
    Appeared {
        name: String,
        tip: String,
        /// 1 for a first sighting; higher when the NAME has been seen before
        /// and retired, which makes this a new lifetime.
        incarnation: u32,
    },
    /// A known branch moved.
    Advanced {
        name: String,
        from: String,
        to: String,
        incarnation: u32,
    },
    /// A known branch is gone.
    Retired {
        name: String,
        last_tip: String,
        incarnation: u32,
    },
}

impl BranchChange {
    #[must_use]
    pub fn name(&self) -> &str {
        match self {
            Self::Appeared { name, .. }
            | Self::Advanced { name, .. }
            | Self::Retired { name, .. } => name,
        }
    }

    #[must_use]
    pub const fn incarnation(&self) -> u32 {
        match self {
            Self::Appeared { incarnation, .. }
            | Self::Advanced { incarnation, .. }
            | Self::Retired { incarnation, .. } => *incarnation,
        }
    }

    /// The generation string the binding identity carries. Derived from the
    /// incarnation counter, so it is stable across restarts and changes
    /// exactly when the branch lifetime does.
    #[must_use]
    pub fn generation(&self) -> String {
        format!("g{}", self.incarnation())
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", default)]
pub struct BranchState {
    /// The last observed tip per live branch.
    pub tips: BTreeMap<String, String>,
    /// The incarnation counter per branch NAME, kept even after retirement —
    /// that is what makes a recreated name a new lifetime rather than the old
    /// room waking up with somebody else's commits in it.
    pub incarnations: BTreeMap<String, u32>,
    /// Command event ids already answered, so a redelivery answers once.
    pub answered: BTreeSet<String>,
    /// Rooms of retired branch lifetimes, still answered (with a refusal).
    pub retired: Vec<RetiredRoom>,
    /// Per room: the relay's cursor for the next older page of the room's
    /// timeline, so a walk through the room's commands spans cycles.
    pub command_walk: BTreeMap<String, Walk>,
    pub observation_sequence: u64,
    pub pending_observations: Vec<PendingObservation>,
    pub pending_delivery_plans: BTreeMap<String, crate::github::Plan>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct PendingObservation {
    pub sequence: u64,
    pub at_ms: u64,
    pub change: BranchChange,
}

/// Where a walk through one room's timeline has got to.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct Walk {
    /// The relay's cursor for the next page.
    pub cursor: String,
    /// Epoch milliseconds of the visit that read the walk's first page.
    pub started_ms: u64,
}

/// A retired branch lifetime whose room still gets answers.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct RetiredRoom {
    /// The branch name (native) or observation key (GitHub).
    pub name: String,
    /// The retired lifetime's generation (`g<n>`).
    pub generation: String,
    /// Epoch milliseconds of the retirement.
    pub at_ms: u64,
}

/// How long a retired room keeps getting answers: a command left there later
/// stays unanswered, as the room is history. The room is forgotten at the end
/// of the first walk of its timeline that began after this long and answered
/// every command it found, so every command left inside the window is reached.
pub const RETIRED_ROOM_ANSWER_MS: u64 = 7 * 24 * 60 * 60 * 1000;

/// The most answered-command ids kept. Bounded because this document is
/// rewritten on every cycle: the relay's own event log is the durable record,
/// and this is only the cell's short-term memory of what it has replied to.
pub const MAX_ANSWERED: usize = 512;

impl BranchState {
    /// Fold one observation into the state, returning what changed.
    ///
    /// `current` becomes the new tip set. Persist the returned changes with
    /// this state before publishing their effects.
    pub fn observe(&mut self, current: BTreeMap<String, String>) -> Vec<BranchChange> {
        let mut changes = Vec::new();

        for (name, tip) in &current {
            match self.tips.get(name) {
                Some(known) if known == tip => {}
                Some(known) => {
                    let incarnation = self.incarnations.get(name).copied().unwrap_or(1);
                    changes.push(BranchChange::Advanced {
                        name: name.clone(),
                        from: known.clone(),
                        to: tip.clone(),
                        incarnation,
                    });
                }
                None => {
                    // A name seen before and retired starts a NEW incarnation.
                    let incarnation = self
                        .incarnations
                        .get(name)
                        .map_or(1, |seen| seen.saturating_add(1));
                    self.incarnations.insert(name.clone(), incarnation);
                    changes.push(BranchChange::Appeared {
                        name: name.clone(),
                        tip: tip.clone(),
                        incarnation,
                    });
                }
            }
        }

        for (name, last_tip) in &self.tips {
            if !current.contains_key(name) {
                changes.push(BranchChange::Retired {
                    name: name.clone(),
                    last_tip: last_tip.clone(),
                    incarnation: self.incarnations.get(name).copied().unwrap_or(1),
                });
            }
        }

        self.tips = current;
        changes
    }

    /// The generation string for a branch's CURRENT incarnation.
    #[must_use]
    pub fn generation(&self, name: &str) -> String {
        format!("g{}", self.incarnations.get(name).copied().unwrap_or(1))
    }

    /// Note that a branch EXISTS, from an event rather than a snapshot.
    ///
    /// The event-driven (GitHub) cell learns about one ref at a time, so it
    /// cannot use [`Self::observe`] — a single-ref snapshot would retire every
    /// other branch. This is the incremental half: it returns the branch's
    /// incarnation, bumping it when the name was previously RETIRED, which is
    /// what makes a deleted-and-recreated ref a new room rather than the old
    /// one adopting somebody else's commits.
    pub fn note_present(&mut self, name: &str, tip: &str) -> u32 {
        let incarnation = match self.tips.get(name) {
            // Already live: same lifetime, whatever the tip does.
            Some(_) => self.incarnations.get(name).copied().unwrap_or(1),
            // Not live. Seen before ANDretired → a new lifetime.
            None => self
                .incarnations
                .get(name)
                .map_or(1, |seen| seen.saturating_add(1)),
        };
        self.incarnations.insert(name.to_string(), incarnation);
        self.tips.insert(name.to_string(), tip.to_string());
        incarnation
    }

    /// Note that a branch is GONE, from an event.
    ///
    /// The incarnation counter is deliberately KEPT: it is the memory that
    /// makes the next `note_present` a new lifetime. Only the live tip is
    /// forgotten.
    pub fn note_absent(&mut self, name: &str) {
        self.tips.remove(name);
        // A branch that existed before the cell first saw it was in its
        // first lifetime: record that, so a recreation is lifetime 2 and
        // never reuses the retired room.
        self.incarnations.entry(name.to_string()).or_insert(1);
    }

    #[must_use]
    pub fn is_live(&self, name: &str) -> bool {
        self.tips.contains_key(name)
    }

    /// Record that `name`'s lifetime `generation` retired at `now_ms`, so its
    /// room keeps answering commands.
    pub fn note_retired(&mut self, name: &str, generation: &str, now_ms: u64) {
        self.retired
            .retain(|r| !(r.name == name && r.generation == generation));
        self.retired.push(RetiredRoom {
            name: name.to_string(),
            generation: generation.to_string(),
            at_ms: now_ms,
        });
    }

    /// Move `room`'s walk on after a visit: to `next`, the cursor of the page
    /// after the one visited (`None` past the room's oldest page), but only
    /// when the visit answered every command owed on its page (`drained`). A
    /// page is left behind only once nothing on it is owed, so a walk that
    /// reaches the end has answered every command it passed. Returns the
    /// start of the walk the visit finished, if it finished one.
    pub fn advance_walk(
        &mut self,
        room: &str,
        next: Option<String>,
        drained: bool,
        now_ms: u64,
    ) -> Option<u64> {
        let started_ms = self.command_walk.get(room).map_or(now_ms, |w| w.started_ms);
        if !drained {
            return None;
        }
        match next {
            Some(cursor) => {
                self.command_walk
                    .insert(room.to_string(), Walk { cursor, started_ms });
                None
            }
            None => {
                self.command_walk.remove(room);
                Some(started_ms)
            }
        }
    }

    /// Forget `name`'s retired lifetime `generation` when a walk of its room
    /// that began at `walk_started_ms`, at least [`RETIRED_ROOM_ANSWER_MS`]
    /// after the retirement, has answered everything.
    pub fn close_retired(&mut self, name: &str, generation: &str, walk_started_ms: u64) {
        self.retired.retain(|r| {
            !(r.name == name
                && r.generation == generation
                && walk_started_ms.saturating_sub(r.at_ms) >= RETIRED_ROOM_ANSWER_MS)
        });
    }

    /// Record that a command has been answered, keeping the set bounded.
    pub fn mark_answered(&mut self, command_id: &str) {
        self.answered.insert(command_id.to_string());
        while self.answered.len() > MAX_ANSWERED {
            // BTreeSet has no cheap "oldest", and command ids are hashes with
            // no order — so the eviction is arbitrary BY CONSTRUCTION. That is
            // acceptable precisely because this set is not the authority: the
            // durable record is the cell's own result events in the room, which
            // `unanswered` consults.
            let Some(first) = self.answered.iter().next().cloned() else {
                break;
            };
            self.answered.remove(&first);
        }
    }

    #[must_use]
    pub fn has_answered(&self, command_id: &str) -> bool {
        self.answered.contains(command_id)
    }
}

/// The commands in `commands` that no result in `results` answers.
///
/// The authority is the ROOM, not the cell's memory: a result event names the
/// command it answers with an `e` tag, so a restarted cell re-derives exactly
/// what it still owes by reading its own past answers. That is what makes
/// "answer once" survive losing all local state.
#[must_use]
pub fn unanswered(
    mut commands: Vec<crate::event::Event>,
    results: &[crate::event::Event],
) -> Vec<crate::event::Event> {
    let answered: BTreeSet<&str> = results.iter().filter_map(|r| r.first_tag("e")).collect();
    // Filtered in place: a page of commands is not cloned to be filtered.
    commands.retain(|c| !answered.contains(c.id.as_str()));
    commands
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deleting_a_branch_seen_only_now_retires_lifetime_one() {
        let mut state = BranchState::default();
        assert_eq!(state.generation("feature/x"), "g1");
        state.note_absent("feature/x");
        assert_eq!(state.note_present("feature/x", "c2"), 2, "a recreation is a new lifetime");
    }
    use crate::event::Event;

    fn snapshot(rows: &[(&str, &str)]) -> BTreeMap<String, String> {
        rows.iter()
            .map(|(n, t)| ((*n).to_string(), (*t).to_string()))
            .collect()
    }

    #[test]
    fn profiles_parse_strictly() {
        assert_eq!(Profile::parse("native").expect("native"), Profile::Native);
        assert_eq!(Profile::parse(" github ").expect("github"), Profile::Github);
        assert!(Profile::parse("gitlab").is_err());
        assert!(
            Profile::parse("").is_err(),
            "a typo must not pick a default"
        );
    }

    #[test]
    fn a_first_observation_reports_every_branch_as_appeared() {
        let mut state = BranchState::default();
        let changes = state.observe(snapshot(&[("main", "c1"), ("feature/x", "c2")]));
        assert_eq!(changes.len(), 2);
        for change in &changes {
            assert!(matches!(
                change,
                BranchChange::Appeared { incarnation: 1, .. }
            ));
            assert_eq!(change.generation(), "g1");
        }
        // Idempotent: observing the same snapshot again changes nothing.
        assert!(state
            .observe(snapshot(&[("main", "c1"), ("feature/x", "c2")]))
            .is_empty());
    }

    #[test]
    fn a_moved_branch_is_advanced_not_reappeared() {
        let mut state = BranchState::default();
        state.observe(snapshot(&[("main", "c1")]));
        let changes = state.observe(snapshot(&[("main", "c2")]));
        assert_eq!(
            changes,
            vec![BranchChange::Advanced {
                name: "main".into(),
                from: "c1".into(),
                to: "c2".into(),
                incarnation: 1,
            }]
        );
    }

    #[test]
    fn a_deleted_branch_is_retired() {
        let mut state = BranchState::default();
        state.observe(snapshot(&[("main", "c1"), ("feature/x", "c2")]));
        let changes = state.observe(snapshot(&[("main", "c1")]));
        assert_eq!(
            changes,
            vec![BranchChange::Retired {
                name: "feature/x".into(),
                last_tip: "c2".into(),
                incarnation: 1,
            }]
        );
    }

    #[test]
    fn a_recreated_branch_name_is_a_new_incarnation() {
        // The property that stops an old room adopting a new branch's commits.
        let mut state = BranchState::default();
        state.observe(snapshot(&[("feature/x", "c1")]));
        state.observe(snapshot(&[]));
        let changes = state.observe(snapshot(&[("feature/x", "c9")]));
        assert_eq!(
            changes,
            vec![BranchChange::Appeared {
                name: "feature/x".into(),
                tip: "c9".into(),
                incarnation: 2,
            }]
        );
        assert_eq!(state.generation("feature/x"), "g2");
        // ...and a third lifetime is a third generation.
        state.observe(snapshot(&[]));
        let changes = state.observe(snapshot(&[("feature/x", "cf")]));
        assert_eq!(changes[0].incarnation(), 3);
    }

    #[test]
    fn observation_state_round_trips_through_its_document() {
        let mut state = BranchState::default();
        state.observe(snapshot(&[("main", "c1")]));
        state.mark_answered(&"ab".repeat(32));
        let json = ferrite_app::serde_json::to_string(&state).expect("encode");
        let parsed: BranchState = ferrite_app::serde_json::from_str(&json).expect("decode");
        assert_eq!(parsed, state);
        assert!(parsed.has_answered(&"ab".repeat(32)));
    }

    #[test]
    fn incremental_presence_advances_a_recreated_ref() {
        // The event-driven cell's path: one ref at a time, and a recreated ref
        // must be a new lifetime just as it is under snapshot observation.
        let mut state = BranchState::default();
        assert_eq!(state.note_present("feature/x", "c1"), 1);
        assert!(state.is_live("feature/x"));
        // A push to a live branch is the SAME lifetime.
        assert_eq!(state.note_present("feature/x", "c2"), 1);
        assert_eq!(state.generation("feature/x"), "g1");

        // Deleted, then recreated — a new generation.
        state.note_absent("feature/x");
        assert!(!state.is_live("feature/x"));
        assert_eq!(state.note_present("feature/x", "c9"), 2);
        assert_eq!(state.generation("feature/x"), "g2");

        // ...and again.
        state.note_absent("feature/x");
        assert_eq!(state.note_present("feature/x", "cf"), 3);

        // Another branch is untouched by all of it — the whole reason this is
        // not a snapshot fold.
        assert_eq!(state.note_present("main", "m1"), 1);
        assert_eq!(state.generation("main"), "g1");
        assert!(
            state.is_live("feature/x"),
            "one ref's event must not retire another"
        );
    }

    #[test]
    fn incremental_and_snapshot_paths_agree_on_generations() {
        // Same lifecycle, two observation styles, one answer.
        let mut incremental = BranchState::default();
        incremental.note_present("feature/x", "c1");
        incremental.note_absent("feature/x");
        incremental.note_present("feature/x", "c9");

        let mut snapshotted = BranchState::default();
        snapshotted.observe(snapshot(&[("feature/x", "c1")]));
        snapshotted.observe(snapshot(&[]));
        snapshotted.observe(snapshot(&[("feature/x", "c9")]));

        assert_eq!(
            incremental.generation("feature/x"),
            snapshotted.generation("feature/x")
        );
        assert_eq!(incremental.generation("feature/x"), "g2");
    }

    #[test]
    fn the_answered_set_stays_bounded() {
        let mut state = BranchState::default();
        for i in 0..(MAX_ANSWERED + 50) {
            state.mark_answered(&format!("{i:064x}"));
        }
        assert_eq!(state.answered.len(), MAX_ANSWERED);
    }

    #[test]
    fn unanswered_reads_the_room_not_local_memory() {
        let sk = [5u8; 32];
        let a = Event::sign(&sk, 47001, vec![], "a", 1).expect("sign");
        let b = Event::sign(&sk, 47001, vec![], "b", 2).expect("sign");
        // One result answers `a` by naming it in an `e` tag.
        let result =
            Event::sign(&sk, 47002, vec![vec!["e".into(), a.id.clone()]], "", 3).expect("sign");
        let owed = unanswered(vec![a.clone(), b.clone()], &[result]);
        assert_eq!(owed.len(), 1);
        assert_eq!(owed[0].id, b.id);
        // With no results, everything is owed.
        assert_eq!(unanswered(vec![a, b], &[]).len(), 2);
    }
}
