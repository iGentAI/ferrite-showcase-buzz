//! Contract D — the mediated RCS verbs: what a member may ask for, and what
//! the cell answers.
//!
//! A member's request is a signed `KIND_PROCESS_COMMAND` (47001) in the
//! process room; the cell's answer is a signed `KIND_PROCESS_RESULT` (47002)
//! naming the command it answers. Both are ordinary client-signed room events
//! — the relay gates the RESULT kind to operator-registered cell identities,
//! which is what makes "the platform did X" unforgeable without making the
//! relay the signer.
//!
//! Two rules carry the design:
//!
//! * **The command event id IS the idempotency key.** A re-delivered or
//!   re-driven command must produce byte-identical effects or a `duplicate`
//!   answer — determinism is the dedup, as everywhere else in this product.
//! * **Authority is weighed when the verb RUNS**, against the roster, not at
//!   the moment the command was written. A member who has since been demoted
//!   cannot land a branch because their request is older than the demotion.

use ferrite_app::serde::{Deserialize, Serialize};
use ferrite_app::serde_json::{self, Value};

use crate::error::{ProcessError, Result};

/// A member's request that the cell perform an RCS lifecycle verb.
pub const KIND_PROCESS_COMMAND: u32 = 47001;
/// The cell's answer. Authorship-gated at the relay.
pub const KIND_PROCESS_RESULT: u32 = 47002;
/// The envelope version both kinds carry.
pub const ENVELOPE_VERSION: u32 = 1;
/// Longest accepted argument string (branch names, messages, paths).
const MAX_ARG: usize = 512;
/// Most files one mediated commit may carry.
const MAX_COMMIT_FILES: usize = 256;

/// What a member can ask the cell to do.
///
/// Each variant carries its EXPLICIT contract wire name. `rename_all` is
/// deliberately absent: it would have encoded `Branch` as `"branch"` while
/// Contract D says `rcs_branch`, and a round-trip test written against the
/// same wrong spelling would never notice. The names here and
/// [`Verb::as_str`] are pinned against each other by
/// `every_verb_has_its_contract_wire_name`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub enum Verb {
    /// Re-poll the provider and reconcile the room. Changes nothing upstream.
    #[serde(rename = "refresh")]
    Refresh,
    /// Create a branch at a given start point.
    #[serde(rename = "rcs_branch")]
    Branch,
    /// Commit files on the member's behalf (the mediated write path that
    /// exists because member desktops hold no RCS credential).
    #[serde(rename = "rcs_commit")]
    Commit,
    /// Three-way merge into a non-trunk branch.
    #[serde(rename = "rcs_merge")]
    Merge,
    /// The GATED trunk land: fast-forward descent plus a green test verdict.
    #[serde(rename = "rcs_land")]
    Land,
    /// Publish a binding for a branch incarnation.
    #[serde(rename = "binding_create")]
    BindingCreate,
    /// Retire a binding and archive its room.
    #[serde(rename = "binding_archive")]
    BindingArchive,
}

impl Verb {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Refresh => "refresh",
            Self::Branch => "rcs_branch",
            Self::Commit => "rcs_commit",
            Self::Merge => "rcs_merge",
            Self::Land => "rcs_land",
            Self::BindingCreate => "binding_create",
            Self::BindingArchive => "binding_archive",
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "refresh" => Self::Refresh,
            "rcs_branch" => Self::Branch,
            "rcs_commit" => Self::Commit,
            "rcs_merge" => Self::Merge,
            "rcs_land" => Self::Land,
            "binding_create" => Self::BindingCreate,
            "binding_archive" => Self::BindingArchive,
            _ => return None,
        })
    }

    /// The LEAST authority that may run this verb.
    ///
    /// The split is the whole reason mediation is safe: everyday work is a
    /// member's to do, while the two verbs that move a protected trunk or
    /// retire a room need somebody the workspace has trusted with moderation.
    /// Land is a moderator's because it moves the branch everything else
    /// descends from; binding lifecycle is a moderator's because archiving a
    /// room takes it away from the people working in it. A commit or merge
    /// that writes the trunk itself is raised to a moderator's for the same
    /// reason as a land ([`Command::required_role_in`]).
    #[must_use]
    pub const fn required_role(self) -> Role {
        match self {
            Self::Refresh | Self::Branch | Self::Commit | Self::Merge => Role::Member,
            Self::Land | Self::BindingCreate | Self::BindingArchive => Role::Moderator,
        }
    }

    /// Every verb, for exhaustive tests and documentation.
    pub const ALL: &'static [Self] = &[
        Self::Refresh,
        Self::Branch,
        Self::Commit,
        Self::Merge,
        Self::Land,
        Self::BindingCreate,
        Self::BindingArchive,
    ];

    /// The argument names this verb USES. Anything else present in a command
    /// is refused rather than ignored: a `refresh` carrying files, or a
    /// `rcs_branch` carrying merge arguments, means the sender and this cell
    /// disagree about what was asked — and silently dropping the difference is
    /// how a caller comes to believe something happened that did not.
    #[must_use]
    pub const fn allowed_args(self) -> &'static [&'static str] {
        match self {
            Self::Refresh | Self::BindingCreate | Self::BindingArchive => &[],
            Self::Branch => &["branch", "from"],
            // A commit lands on its room's branch; a `branch` it would ignore
            // is refused instead.
            Self::Commit => &["files", "message"],
            Self::Merge | Self::Land => &["from", "into"],
        }
    }
}

/// What the workspace says about a requester, in rising order of authority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "lowercase")]
pub enum Role {
    Member,
    Moderator,
    Operator,
}

impl Role {
    /// Rank, so "at least a moderator" is one comparison rather than a match
    /// somebody can forget to extend.
    #[must_use]
    pub const fn rank(self) -> u8 {
        match self {
            Self::Member => 0,
            Self::Moderator => 1,
            Self::Operator => 2,
        }
    }

    #[must_use]
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s {
            "member" => Self::Member,
            "moderator" | "admin" => Self::Moderator,
            "operator" => Self::Operator,
            _ => return None,
        })
    }
}

/// Whether `role` may run `verb`. `None` — no role at all — is never allowed:
/// a non-member's command is refused, and an UNRESOLVABLE role is treated as
/// no role, because failing open on a roster read would hand authority to
/// whoever can make that read fail.
#[must_use]
pub fn authorized(verb: Verb, role: Option<Role>) -> bool {
    meets(role, verb.required_role())
}

/// Whether `role` is at least `required`; `None` never is (see [`authorized`]).
#[must_use]
pub fn meets(role: Option<Role>, required: Role) -> bool {
    role.is_some_and(|r| r.rank() >= required.rank())
}

/// The trunk: the branch a land moves, and the platform's default branch.
pub const TRUNK: &str = "main";

/// The arguments a verb needs. Flat and optional rather than per-verb variants
/// so an unknown field is refused once, centrally.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct Args {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub into: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// `path -> content`, for the mediated commit path.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files: Option<std::collections::BTreeMap<String, String>>,
}

impl Args {
    /// The argument names actually present in this command.
    #[must_use]
    pub fn present(&self) -> Vec<&'static str> {
        let mut out = Vec::new();
        if self.branch.is_some() {
            out.push("branch");
        }
        if self.from.is_some() {
            out.push("from");
        }
        if self.into.is_some() {
            out.push("into");
        }
        if self.message.is_some() {
            out.push("message");
        }
        if self.files.is_some() {
            out.push("files");
        }
        out
    }
}

/// A member's command, as it travels in the event's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", deny_unknown_fields)]
pub struct Command {
    pub v: u32,
    pub verb: Verb,
    pub binding_id: String,
    #[serde(default)]
    pub args: Args,
}

impl Command {
    /// The least authority that may run this command when it is asked in the
    /// room of `room_branch`: its verb's ([`Verb::required_role`]), raised to a
    /// moderator's when the command writes the trunk directly. A commit in the
    /// trunk's room, or a merge into the trunk, moves the branch everything else
    /// descends from without the verdict a land checks, so it needs at least the
    /// authority a land needs.
    #[must_use]
    pub fn required_role_in(&self, room_branch: &str) -> Role {
        // Why: creating the trunk (when it does not exist, or was deleted)
        // writes it as surely as a commit does.
        let writes_trunk = match self.verb {
            Verb::Commit => room_branch == TRUNK,
            Verb::Merge => self.args.into.as_deref().unwrap_or(TRUNK) == TRUNK,
            Verb::Branch => self.args.branch.as_deref() == Some(TRUNK),
            _ => false,
        };
        if writes_trunk {
            Role::Moderator
        } else {
            self.verb.required_role()
        }
    }

    #[must_use]
    pub fn new(verb: Verb, binding_id: &str) -> Self {
        Self {
            v: ENVELOPE_VERSION,
            verb,
            binding_id: binding_id.to_string(),
            args: Args::default(),
        }
    }

    pub fn to_content(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| ProcessError::Internal(format!("command encode: {e}")))
    }

    /// Parse strictly: an unknown version or an unknown field is refused, so a
    /// newer client's command is never half-understood by an older cell.
    pub fn parse(content: &str) -> Result<Self> {
        let command: Self = serde_json::from_str(content)
            .map_err(|e| ProcessError::Invalid(format!("process command: {e}")))?;
        if command.v != ENVELOPE_VERSION {
            return Err(ProcessError::Invalid(format!(
                "process command version {} is not supported (expected {ENVELOPE_VERSION})",
                command.v
            )));
        }
        if !command.binding_id.starts_with("sha256:") {
            return Err(ProcessError::Invalid(
                "process command binding_id is not a sha256: digest".into(),
            ));
        }
        command.check_args()?;
        Ok(command)
    }

    /// Every verb's own argument requirements, in ONE place — so a verb that
    /// reaches the control plane always has what it needs, a caller learns
    /// what is missing instead of getting a provider error back, and an
    /// argument the verb does not use is refused rather than dropped.
    pub fn check_args(&self) -> Result<()> {
        let verb = self.verb.as_str();
        // 1. Nothing irrelevant. An argument this verb never reads means the
        //    sender expected something else to happen.
        let allowed = self.verb.allowed_args();
        for present in self.args.present() {
            if !allowed.contains(&present) {
                return Err(ProcessError::Invalid(format!(
                    "{verb} does not take `{present}`"
                )));
            }
        }
        // 2. No blank strings: a present-but-empty branch name is not a
        //    request, it is a mistake with a plausible shape.
        let non_blank = |name: &str, value: &Option<String>| match value {
            Some(v) if v.trim().is_empty() => {
                Err(ProcessError::Invalid(format!("{verb}: `{name}` is empty")))
            }
            Some(v) if v.len() > MAX_ARG => Err(ProcessError::Invalid(format!(
                "{verb}: `{name}` is over {MAX_ARG} bytes"
            ))),
            _ => Ok(()),
        };
        non_blank("branch", &self.args.branch)?;
        non_blank("from", &self.args.from)?;
        non_blank("into", &self.args.into)?;
        // Branch names in one canonical spelling, so the trunk checks above
        // and the room lookups compare what the provider will act on: no
        // surrounding whitespace, no control characters, no `refs/` prefix.
        let canonical = |name: &str, value: &Option<String>| match value {
            Some(v)
                if v.trim() != v
                    || v.chars().any(char::is_control)
                    || v.starts_with("refs/") =>
            {
                Err(ProcessError::Invalid(format!(
                    "{verb}: `{name}` must be a bare branch name (no whitespace, control characters or refs/ prefix)"
                )))
            }
            _ => Ok(()),
        };
        canonical("branch", &self.args.branch)?;
        canonical("from", &self.args.from)?;
        canonical("into", &self.args.into)?;
        non_blank("message", &self.args.message)?;
        if let Some(files) = &self.args.files {
            if files.len() > MAX_COMMIT_FILES {
                return Err(ProcessError::Invalid(format!(
                    "{verb}: more than {MAX_COMMIT_FILES} files"
                )));
            }
            for path in files.keys() {
                if path.trim().is_empty() || path.len() > MAX_ARG {
                    return Err(ProcessError::Invalid(format!(
                        "{verb}: a file path is empty or over {MAX_ARG} bytes"
                    )));
                }
            }
        }
        // 3. Everything required.
        let need = |present: bool, what: &str| {
            if present {
                Ok(())
            } else {
                Err(ProcessError::Invalid(format!("{verb} needs {what}")))
            }
        };
        match self.verb {
            Verb::Refresh | Verb::BindingCreate | Verb::BindingArchive => Ok(()),
            Verb::Branch => need(self.args.branch.is_some(), "a branch name"),
            Verb::Commit => {
                need(
                    self.args.files.as_ref().is_some_and(|f| !f.is_empty()),
                    "at least one file",
                )?;
                need(self.args.message.is_some(), "a commit message")
            }
            Verb::Merge | Verb::Land => {
                need(self.args.from.is_some(), "a `from` branch")?;
                need(self.args.into.is_some(), "an `into` branch")
            }
        }
    }
}

/// How a command ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde", rename_all = "snake_case")]
pub enum Outcome {
    /// The verb ran and the provider accepted it.
    Ok,
    /// Refused before anything ran (authority, scope, malformed).
    Refused,
    /// The provider refused: the branch is behind its trunk.
    Stale,
    /// The provider refused: paths diverged.
    Conflict,
    /// The provider refused for its own stated reason (e.g. a failing verdict).
    Rejected,
    /// This command had already been answered.
    Duplicate,
    /// A transient fault; the command may be retried.
    Failed,
}

/// The cell's answer, as it travels in the result event's content.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(crate = "ferrite_app::serde")]
pub struct CommandResult {
    pub v: u32,
    pub verb: Verb,
    pub binding_id: String,
    pub outcome: Outcome,
    /// The command event id this answers — also the idempotency key.
    pub command: String,
    pub detail: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub commit: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// The commit-bound test verdict a land consulted, quoted verbatim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub verdict: Option<String>,
}

impl CommandResult {
    #[must_use]
    pub fn new(
        command_id: &str,
        verb: Verb,
        binding_id: &str,
        outcome: Outcome,
        detail: &str,
    ) -> Self {
        Self {
            v: ENVELOPE_VERSION,
            verb,
            binding_id: binding_id.to_string(),
            outcome,
            command: command_id.to_string(),
            detail: detail.to_string(),
            commit: None,
            conflicts: Vec::new(),
            verdict: None,
        }
    }

    #[must_use]
    pub fn with_commit(mut self, commit: &str) -> Self {
        self.commit = Some(commit.to_string());
        self
    }

    #[must_use]
    pub fn with_conflicts(mut self, conflicts: Vec<String>) -> Self {
        self.conflicts = conflicts;
        self
    }

    #[must_use]
    pub fn with_verdict(mut self, verdict: &str) -> Self {
        self.verdict = Some(verdict.to_string());
        self
    }

    pub fn to_content(&self) -> Result<String> {
        serde_json::to_string(self)
            .map_err(|e| ProcessError::Internal(format!("result encode: {e}")))
    }

    /// The result event's tags: the room, and the command it answers.
    #[must_use]
    pub fn tags(&self, channel_id: &str) -> Vec<Vec<String>> {
        vec![
            vec!["h".to_string(), channel_id.to_string()],
            vec!["e".to_string(), self.command.clone()],
        ]
    }

    /// A one-line human summary for the room, so a person reading the channel
    /// sees what happened without decoding JSON.
    #[must_use]
    pub fn summary(&self) -> String {
        let verb = self.verb.as_str();
        match self.outcome {
            Outcome::Ok => match &self.commit {
                Some(c) => format!("{verb}: done — {c}"),
                None => format!("{verb}: done"),
            },
            Outcome::Stale => format!("{verb}: refused — the branch is behind its target"),
            Outcome::Conflict => format!(
                "{verb}: refused — {} path(s) conflict: {}",
                self.conflicts.len(),
                self.conflicts.join(", ")
            ),
            Outcome::Rejected => format!("{verb}: refused — {}", self.detail),
            Outcome::Refused => format!("{verb}: not permitted — {}", self.detail),
            Outcome::Duplicate => format!("{verb}: already answered"),
            Outcome::Failed => format!("{verb}: failed — {}", self.detail),
        }
    }
}

/// One page of a room's timeline (`/buzz/api/query`), newest first: the
/// newest page, or with `cursor` the page after the one that returned it.
#[must_use]
pub fn timeline_query(channel_id: &str, cursor: Option<&str>, limit: u32) -> Value {
    let mut query = serde_json::json!({
        "channel": channel_id,
        "limit": limit,
    });
    if let Some(cursor) = cursor {
        query["cursor"] = serde_json::json!(cursor);
    }
    query
}

#[cfg(test)]
mod tests {

    #[test]
    fn branch_names_must_be_canonical_and_creating_the_trunk_needs_a_moderator() {
        for bad in [" main", "main ", "refs/heads/main", "ma\u{7}in"] {
            let mut c = Command::new(Verb::Merge, "sha256:x");
            c.args.from = Some("feature/x".into());
            c.args.into = Some(bad.into());
            assert!(c.check_args().is_err(), "{bad:?} was accepted as a branch name");
        }
        let mut c = Command::new(Verb::Branch, "sha256:x");
        c.args.branch = Some(TRUNK.into());
        assert_eq!(c.required_role_in("feature/x"), Role::Moderator);
        c.args.branch = Some("feature/y".into());
        assert_eq!(c.required_role_in("feature/x"), Verb::Branch.required_role());
    }
    use super::*;

    #[test]
    fn the_authorization_matrix_is_exactly_the_documented_split() {
        // Member work.
        for verb in [Verb::Refresh, Verb::Branch, Verb::Commit, Verb::Merge] {
            assert!(authorized(verb, Some(Role::Member)), "{verb:?}");
            assert!(authorized(verb, Some(Role::Moderator)), "{verb:?}");
            assert!(authorized(verb, Some(Role::Operator)), "{verb:?}");
        }
        // Moderator work: a plain member may NOT land or move a binding.
        for verb in [Verb::Land, Verb::BindingCreate, Verb::BindingArchive] {
            assert!(!authorized(verb, Some(Role::Member)), "{verb:?}");
            assert!(authorized(verb, Some(Role::Moderator)), "{verb:?}");
            assert!(authorized(verb, Some(Role::Operator)), "{verb:?}");
        }
        // No role at all is never allowed, for any verb — including the
        // read-only one, because a non-member has no business in the room.
        for verb in Verb::ALL {
            assert!(!authorized(*verb, None), "{verb:?} with no role");
        }
    }

    #[test]
    fn writing_the_trunk_needs_a_moderator() {
        let mut commit = Command::new(Verb::Commit, "b");
        assert_eq!(commit.required_role_in("feature/x"), Role::Member);
        assert_eq!(commit.required_role_in(TRUNK), Role::Moderator);
        commit.args.branch = Some("feature/x".into());
        assert_eq!(commit.required_role_in(TRUNK), Role::Moderator, "a commit lands in its room's branch");
        let mut merge = Command::new(Verb::Merge, "b");
        merge.args.into = Some("feature/y".into());
        assert_eq!(merge.required_role_in(TRUNK), Role::Member, "integrating the trunk into a branch is a member's");
        merge.args.into = Some(TRUNK.into());
        assert_eq!(merge.required_role_in("feature/x"), Role::Moderator);
        merge.args.into = None;
        assert_eq!(merge.required_role_in("feature/x"), Role::Moderator, "an absent `into` means the trunk");
        for verb in [Verb::Refresh, Verb::Branch, Verb::Land, Verb::BindingCreate, Verb::BindingArchive] {
            assert_eq!(Command::new(verb, "b").required_role_in(TRUNK), verb.required_role(), "{verb:?}");
        }
    }

    #[test]
    fn roles_rank_and_admin_reads_as_moderator() {
        assert!(Role::Operator.rank() > Role::Moderator.rank());
        assert!(Role::Moderator.rank() > Role::Member.rank());
        // The relay's roster says "admin" for a configured moderator.
        assert_eq!(Role::parse("admin"), Some(Role::Moderator));
        assert_eq!(Role::parse("member"), Some(Role::Member));
        assert_eq!(Role::parse("owner"), None, "this product has no owner role");
    }

    #[test]
    fn every_verb_has_its_contract_wire_name() {
        // The serde encoding is what travels; `as_str` is what the code reads.
        // They must agree, and BOTH must be the contract's spelling — this is
        // the assertion a `rename_all` would have quietly broken.
        let expected = [
            (Verb::Refresh, "refresh"),
            (Verb::Branch, "rcs_branch"),
            (Verb::Commit, "rcs_commit"),
            (Verb::Merge, "rcs_merge"),
            (Verb::Land, "rcs_land"),
            (Verb::BindingCreate, "binding_create"),
            (Verb::BindingArchive, "binding_archive"),
        ];
        assert_eq!(expected.len(), Verb::ALL.len(), "a verb was added");
        for (verb, wire) in expected {
            assert_eq!(verb.as_str(), wire, "as_str for {verb:?}");
            assert_eq!(
                serde_json::to_string(&verb).expect("encode"),
                format!("\"{wire}\""),
                "serde encoding for {verb:?}"
            );
            assert_eq!(
                serde_json::from_str::<Verb>(&format!("\"{wire}\"")).expect("decode"),
                verb
            );
            assert_eq!(Verb::parse(wire), Some(verb));
        }
        // And the spellings a `rename_all` would have produced are NOT accepted.
        for wrong in ["branch", "commit", "merge", "land"] {
            assert!(
                serde_json::from_str::<Verb>(&format!("\"{wrong}\"")).is_err(),
                "{wrong} must not decode"
            );
        }
    }

    #[test]
    fn a_command_round_trips_through_its_contract_encoding() {
        let mut command = Command::new(Verb::Land, &format!("sha256:{}", "ab".repeat(32)));
        command.args.from = Some("feature".into());
        command.args.into = Some("main".into());
        let content = command.to_content().expect("encode");
        assert!(content.contains("\"verb\":\"rcs_land\""), "{content}");
        assert_eq!(Command::parse(&content).expect("parse"), command);
    }

    #[test]
    fn command_parsing_is_strict() {
        let good = format!(
            "{{\"v\":1,\"verb\":\"refresh\",\"binding_id\":\"sha256:{}\"}}",
            "ab".repeat(32)
        );
        assert!(Command::parse(&good).is_ok());
        for bad in [
            // wrong version
            good.replace("\"v\":1", "\"v\":2"),
            // unknown verb
            good.replace("refresh", "rcs_nuke"),
            // binding id that is not a digest
            good.replace(&format!("sha256:{}", "ab".repeat(32)), "my-branch"),
            // unknown field
            good.replace("}", ",\"surprise\":true}"),
            "not json".to_string(),
        ] {
            assert!(Command::parse(&bad).is_err(), "{bad} must be refused");
        }
    }

    #[test]
    fn each_verb_states_what_it_needs() {
        let bid = format!("sha256:{}", "ab".repeat(32));
        // Missing arguments are refused by NAME.
        assert!(Command::new(Verb::Branch, &bid).check_args().is_err());
        assert!(Command::new(Verb::Merge, &bid).check_args().is_err());
        assert!(Command::new(Verb::Land, &bid).check_args().is_err());
        assert!(Command::new(Verb::Commit, &bid).check_args().is_err());
        // ...and the read-only and lifecycle verbs need nothing.
        assert!(Command::new(Verb::Refresh, &bid).check_args().is_ok());
        assert!(Command::new(Verb::BindingCreate, &bid).check_args().is_ok());

        let mut branch = Command::new(Verb::Branch, &bid);
        branch.args.branch = Some("feature".into());
        assert!(branch.check_args().is_ok());

        let mut commit = Command::new(Verb::Commit, &bid);
        commit.args.files = Some(std::collections::BTreeMap::new());
        commit.args.message = Some("msg".into());
        assert!(
            commit.check_args().is_err(),
            "an empty file set is not a commit"
        );
        commit
            .args
            .files
            .as_mut()
            .expect("files")
            .insert("a.txt".into(), "x".into());
        assert!(commit.check_args().is_ok());
    }

    #[test]
    fn an_argument_the_verb_does_not_use_is_refused() {
        let bid = format!("sha256:{}", "ab".repeat(32));
        // A refresh carrying a commit's arguments means the sender expected
        // something else to happen — refuse rather than silently ignore.
        let mut refresh = Command::new(Verb::Refresh, &bid);
        refresh.args.message = Some("do the thing".into());
        assert!(refresh.check_args().is_err());

        // A branch create carrying merge arguments, likewise.
        let mut branch = Command::new(Verb::Branch, &bid);
        branch.args.branch = Some("feature".into());
        branch.args.into = Some("main".into());
        assert!(branch.check_args().is_err());

        // ...while the arguments a verb DOES read are accepted.
        let mut branch = Command::new(Verb::Branch, &bid);
        branch.args.branch = Some("feature".into());
        branch.args.from = Some("main".into());
        assert!(branch.check_args().is_ok());
    }

    #[test]
    fn blank_and_oversized_arguments_are_refused() {
        let bid = format!("sha256:{}", "ab".repeat(32));
        for blank in ["", "   ", "\t"] {
            let mut branch = Command::new(Verb::Branch, &bid);
            branch.args.branch = Some(blank.to_string());
            assert!(
                branch.check_args().is_err(),
                "a present-but-blank branch name is not a request"
            );
        }
        let mut branch = Command::new(Verb::Branch, &bid);
        branch.args.branch = Some("x".repeat(MAX_ARG + 1));
        assert!(branch.check_args().is_err());

        let mut commit = Command::new(Verb::Commit, &bid);
        commit.args.message = Some("m".into());
        let mut files = std::collections::BTreeMap::new();
        files.insert(String::new(), "x".to_string());
        commit.args.files = Some(files);
        assert!(commit.check_args().is_err(), "an empty path is not a file");

        let mut commit = Command::new(Verb::Commit, &bid);
        commit.args.message = Some("m".into());
        let mut files = std::collections::BTreeMap::new();
        for i in 0..=MAX_COMMIT_FILES {
            files.insert(format!("f{i}.txt"), "x".to_string());
        }
        commit.args.files = Some(files);
        assert!(commit.check_args().is_err(), "the file cap must hold");
    }

    #[test]
    fn parse_applies_the_argument_rules_too() {
        // Strictness must not be reachable only through check_args: parsing a
        // command off the wire is where it matters.
        let bad = format!(
            "{{\"v\":1,\"verb\":\"refresh\",\"binding_id\":\"sha256:{}\",\
              \"args\":{{\"message\":\"x\"}}}}",
            "ab".repeat(32)
        );
        assert!(Command::parse(&bad).is_err());
    }

    #[test]
    fn a_result_carries_the_command_it_answers() {
        let result = CommandResult::new(
            &"cd".repeat(32),
            Verb::Land,
            &format!("sha256:{}", "ab".repeat(32)),
            Outcome::Ok,
            "landed",
        )
        .with_commit("c0ffee")
        .with_verdict("passed");
        let content = result.to_content().expect("encode");
        let parsed: CommandResult = serde_json::from_str(&content).expect("parse");
        assert_eq!(parsed, result);
        let tags = result.tags("process-abc");
        assert_eq!(tags[0], vec!["h".to_string(), "process-abc".to_string()]);
        assert_eq!(tags[1], vec!["e".to_string(), "cd".repeat(32)]);
        assert!(result.summary().contains("c0ffee"));
    }

    #[test]
    fn every_outcome_has_a_human_summary() {
        let bid = format!("sha256:{}", "ab".repeat(32));
        for outcome in [
            Outcome::Ok,
            Outcome::Refused,
            Outcome::Stale,
            Outcome::Conflict,
            Outcome::Rejected,
            Outcome::Duplicate,
            Outcome::Failed,
        ] {
            let r = CommandResult::new(&"cd".repeat(32), Verb::Merge, &bid, outcome, "why")
                .with_conflicts(vec!["a.txt".into()]);
            let summary = r.summary();
            assert!(!summary.is_empty(), "{outcome:?}");
            assert!(summary.contains("rcs_merge"), "{summary}");
        }
    }

    #[test]
    fn the_queries_name_their_kinds_and_room() {
        assert_eq!(
            timeline_query("process-abc", None, 20),
            serde_json::json!({ "channel": "process-abc", "limit": 20 })
        );
        assert_eq!(timeline_query("process-abc", Some("c9"), 20)["cursor"], "c9");
    }
}
