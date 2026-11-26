//! The `projectGit.ts` command surface, built out of real git.
//!
//! Every wire shape here is the one the client already reads — snake_case
//! fields, because `projectGit.ts` maps `Raw*` types with those names. Getting a
//! field name wrong is not a type error anywhere in this repository, so the
//! shapes are pinned by tests against the names that file uses.
//!
//! ## The access model this module implements
//!
//! Plan 031 §2.5: repository LIFECYCLE is server-mediated through the workspace,
//! and only working-copy BYTES take this direct git lane. That is why
//! [`sync_gate`] refuses more than a plain git client would:
//!
//! * a **GitHub** remote uses the person's own credential, so it behaves like
//!   any git client;
//! * a **Ferrite RCS** remote's git password IS the platform account owner
//!   bearer — account-wide authority — so a member's desktop must not push with
//!   it, and a non-trunk branch cannot even be fetched over that transport yet.
//!   Both are reported as reasons rather than as failures, so the UI
//!   can say what to do instead (the room's mediated verbs).
//!
//! The gate is a pure function over facts, which is what lets those rules be
//! tested without a server, a token or a network.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::git::{self, Commit, Git};
use crate::paths;
use crate::{Error, Result};

pub const SNAPSHOT_COMMITS: usize = 50;

/// How many files get per-file history and a preview.
///
/// Per-file detail is one `git log -1 -- <path>` per file, so an unbounded
/// snapshot of a large repository would be thousands of subprocesses for a view
/// that shows a screenful. The first files in tree order get detail; the rest
/// are listed with `latest_commit: null`, which the client renders as "not
/// loaded" rather than as "never changed".
pub const SNAPSHOT_FILE_DETAIL: usize = 40;
pub const PREVIEW_MAX_BYTES: usize = 64 * 1024;
pub const PATCH_MAX_LINES: usize = 2_000;

/// Which kind of host a clone URL points at.
///
/// The distinction is an AUTHORITY one, not a vendor one: it decides whose
/// credential a push would spend.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RemoteKind {
    /// A Ferrite RCS remote (`git.<domain>/<owner>/<app>.git`). Its git password
    /// is the platform ACCOUNT OWNER bearer.
    Rcs,
    /// GitHub, where the credential is the person's own.
    GitHub,
    /// Anything else: treated like GitHub — the user's own credential, their
    /// own risk — because that is what a plain git client would do.
    Other,
}

/// Classify a clone URL.
///
/// Matched on the HOST, never on a substring of the whole URL: a GitHub
/// repository could legitimately be named `git.example`, and matching anywhere
/// in the string would misclassify it and change which rules apply.
#[must_use]
pub fn remote_kind(clone_url: &str) -> RemoteKind {
    let host = host_of(clone_url).unwrap_or_default().to_ascii_lowercase();
    if host == "github.com" || host.ends_with(".github.com") {
        return RemoteKind::GitHub;
    }
    if host == "git" || host.starts_with("git.") {
        return RemoteKind::Rcs;
    }
    RemoteKind::Other
}

/// Validate a remote URL before handing it to `git`.
///
/// Refuses leading `-` (which `git` would read as an option such as
/// `--upload-pack=...`), control characters, whitespace, and non-network
/// transports (`ext::`, `file://`, bare local paths) so an untrusted Nostr
/// project tag or PR event cannot trigger arbitrary command execution or read
/// local repositories.
pub fn validate_clone_url(clone_url: &str) -> Result<&str> {
    let url = clone_url;
    if url.is_empty() {
        return Err(Error::Invalid("that project has no clone URL".into()));
    }
    if url.starts_with('-')
        || url
            .chars()
            .any(|c| c.is_control() || c.is_whitespace())
    {
        return Err(Error::Invalid(format!(
            "refusing unsafe git remote URL: {}",
            crate::credentials::redact(url)
        )));
    }
    let allowed_scheme = if let Some((scheme, _)) = url.split_once("://") {
        matches!(
            scheme.to_ascii_lowercase().as_str(),
            "https" | "http" | "ssh" | "git"
        )
    } else if let Some((user_host, path)) = url.split_once(':') {
        // SCP-like syntax `[user@]host:path`, never `ext::...` or `c:\...`
        is_scp_remote(user_host, path)
    } else {
        false
    };
    if !allowed_scheme {
        return Err(Error::Invalid(format!(
            "unsupported or unsafe git remote URL scheme: {}",
            crate::credentials::redact(url)
        )));
    }
    if host_of(url).is_none() {
        return Err(Error::Invalid(format!(
            "git remote URL has no valid host: {}",
            crate::credentials::redact(url)
        )));
    }
    Ok(url)
}

fn is_scp_remote(user_host: &str, path: &str) -> bool {
    if path.is_empty() || path.starts_with(':') || path.starts_with('\\') {
        return false;
    }
    if user_host.is_empty() || user_host.contains('/') || user_host.contains('\\') {
        return false;
    }
    !(user_host.len() == 1 && user_host.as_bytes()[0].is_ascii_alphabetic())
}

fn optional_git_ref(reference: Option<&str>) -> Result<Option<&str>> {
    reference
        .filter(|value| !value.is_empty())
        .map(validate_git_ref)
        .transpose()
}

/// Validate a branch or git reference name before handing it to `git`.
///
/// Refuses leading `-` (option injection such as `--upload-pack=...` or
/// `--output=...`), `..`, `@{`, control characters, whitespace, and characters
/// forbidden by `git-check-ref-format`.
pub fn validate_git_ref(reference: &str) -> Result<&str> {
    let r = reference;
    if r.is_empty()
        || r.starts_with('-')
        || r.starts_with('/')
        || r.ends_with('/')
        || r.ends_with('.')
        || r.ends_with(".lock")
        || r.contains("..")
        || r.contains("@{")
        || r.contains("//")
        || r.chars().any(|c| {
            c.is_control()
                || c.is_whitespace()
                || matches!(c, ':' | '?' | '*' | '[' | '\\')
        })
    {
        return Err(Error::Invalid(format!(
            "refusing invalid or unsafe git reference: {reference}"
        )));
    }
    Ok(r)
}

/// The host out of an https or scp-style git URL.
#[must_use]
pub fn host_of(clone_url: &str) -> Option<String> {
    let url = clone_url.trim();
    // scp-style: `git@host:owner/repo.git`
    if !url.contains("://") {
        let (userinfo_and_host, _) = url.split_once(':')?;
        let host = userinfo_and_host.rsplit('@').next()?;
        return (!host.is_empty()).then(|| host.to_string());
    }
    let after_scheme = url.split_once("://")?.1;
    let authority = after_scheme.split(['/', '?', '#']).next()?;
    // Strip any userinfo, then any port.
    let host = authority.rsplit('@').next()?;
    let host = host.split(':').next()?;
    (!host.is_empty()).then(|| host.to_string())
}

/// The facts a push/pull decision is made from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncFacts {
    pub remote: RemoteKind,
    /// The branch the working copy is on.
    pub branch: String,
    /// The remote's trunk, as the project declares it.
    pub trunk: String,
    pub ahead: u32,
    pub behind: u32,
    pub dirty: bool,
    pub untracked: bool,
    /// Whether a credential for this host is available to git.
    pub credential: bool,
    /// Whether the RCS transport can serve refs other than the trunk.
    ///
    /// `false` today: this is a platform gap. When it
    /// lands this becomes `true` and the branch-scoped refusal below disappears
    /// on its own — which is why it is a capability flag rather than a hard-coded
    /// branch comparison.
    pub rcs_multi_branch: bool,
}

/// What the client may offer, and why not when it may not.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncGate {
    pub can_push: bool,
    pub push_block_reason: Option<String>,
    pub can_pull: bool,
    pub pull_block_reason: Option<String>,
}

/// Decide whether this working copy may push and pull.
///
/// Pure, and ordered by how fundamental each obstacle is: an authority problem
/// is reported ahead of a state problem, because "commit your changes first" is
/// misleading advice when the push could never have been permitted anyway.
#[must_use]
pub fn sync_gate(facts: &SyncFacts) -> SyncGate {
    let push_block = push_block_reason(facts);
    let pull_block = pull_block_reason(facts);
    SyncGate {
        can_push: push_block.is_none(),
        push_block_reason: push_block,
        can_pull: pull_block.is_none(),
        pull_block_reason: pull_block,
    }
}

/// What a sync is FOR, which changes only the wording of an authority refusal —
/// the rule itself is the same for reading and writing.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Intent {
    Push,
    Fetch,
}

/// The AUTHORITY half of the decision: whose credential this would spend, and
/// whether the transport can carry this ref at all.
///
/// Separated from the state half because it must be answerable BEFORE touching
/// the network: a pull refreshes the remote refs, and refusing after that fetch
/// would mean spending a credential the caller was never permitted to use.
fn authority_block(facts: &SyncFacts, intent: Intent) -> Option<String> {
    if facts.remote == RemoteKind::Rcs {
        if !facts.credential {
            return Some(match intent {
                Intent::Push => "Pushing to a Ferrite repository needs the platform account \
                     credential, which this app does not hold: on Ferrite the git password IS \
                     the account owner token. Use the project room's process commands — they \
                     run the same branch, merge and land operations through the workspace, as \
                     you."
                    .to_string(),
                Intent::Fetch => "Reading from a Ferrite repository needs the platform account \
                     credential, which this app does not hold: on Ferrite the git password IS \
                     the account owner token. The project room shows the branch's commits \
                     without it."
                    .to_string(),
            });
        }
        if !facts.rcs_multi_branch && facts.branch != facts.trunk {
            return Some(format!(
                "Ferrite's git transport serves only {} today, so {} cannot be {} over it. \
                 The project room's process commands can branch and \
                 land without it.",
                facts.trunk,
                facts.branch,
                match intent {
                    Intent::Push => "pushed",
                    Intent::Fetch => "fetched",
                }
            ));
        }
        return None;
    }
    if !facts.credential {
        return Some(format!(
            "No credential is stored for this repository's host, so a {} would be refused. Add \
             one in Settings → Repositories, or use a terminal where your git credentials \
             already work.",
            match intent {
                Intent::Push => "push",
                Intent::Fetch => "fetch",
            }
        ));
    }
    None
}

fn push_block_reason(facts: &SyncFacts) -> Option<String> {
    // Authority first: "commit your changes" is misleading advice when the push
    // could never have been permitted anyway.
    if let Some(reason) = authority_block(facts, Intent::Push) {
        return Some(reason);
    }
    // Behind before ahead: a push that would be rejected as non-fast-forward is
    // better explained as "pull first" than as git's own rejection.
    if facts.behind > 0 {
        return Some(format!(
            "This branch is {} commit(s) behind its remote, so a push would be rejected. Pull \
             first, then push.",
            facts.behind
        ));
    }
    if facts.ahead == 0 {
        return Some("Nothing to push — this branch matches its remote.".to_string());
    }
    None
}

fn pull_block_reason(facts: &SyncFacts) -> Option<String> {
    if let Some(reason) = authority_block(facts, Intent::Fetch) {
        return Some(reason);
    }
    if facts.dirty {
        return Some(
            "You have uncommitted changes, and pulling on top of them can leave the working \
             copy half-merged. Commit or stash them first."
                .to_string(),
        );
    }
    if facts.behind == 0 {
        return Some("Nothing to pull — this branch is up to date.".to_string());
    }
    // Untracked files do not block a pull: git refuses only when an incoming
    // file would overwrite one, and pre-refusing every pull would be wrong far
    // more often than it would be right.
    None
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoFile {
    pub path: String,
    pub kind: String,
    pub size: Option<u64>,
    pub preview_content: Option<String>,
    pub last_changed_at: Option<u64>,
    pub latest_commit: Option<Commit>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Contributor {
    pub name: String,
    pub email: String,
    pub commit_count: u32,
    pub last_commit_at: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoSnapshot {
    pub latest_commit: Option<Commit>,
    pub commits: Vec<Commit>,
    pub files: Vec<RepoFile>,
    pub contributors: Vec<Contributor>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LocalRepoSnapshot {
    pub path: String,
    pub snapshot: RepoSnapshot,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffFile {
    pub path: String,
    pub additions: u32,
    pub deletions: u32,
    pub patch: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoDiff {
    pub files: Vec<DiffFile>,
    pub additions: u32,
    pub deletions: u32,
    pub commit_body: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncStatus {
    pub local_path: Option<String>,
    pub local_branch: Option<String>,
    pub local_branches: Vec<String>,
    pub local_head: Option<String>,
    pub local_short_head: Option<String>,
    pub remote_branch: Option<String>,
    pub remote_head: Option<String>,
    pub remote_short_head: Option<String>,
    pub merge_base: Option<String>,
    pub ahead_count: u32,
    pub behind_count: u32,
    pub has_uncommitted_changes: bool,
    pub has_untracked_files: bool,
    pub can_push: bool,
    pub push_block_reason: Option<String>,
    pub can_pull: bool,
    pub pull_block_reason: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PushResult {
    pub pushed: bool,
    pub message: String,
    pub branch: String,
    pub commit: String,
    pub merge_base: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PullResult {
    pub pulled: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CloneResult {
    pub path: String,
    pub cloned: bool,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BranchResult {
    pub branch: String,
    pub commit: String,
    pub message: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalResult {
    pub path: String,
    pub cloned: bool,
}

/// camelCase because the client passes this shape through unmapped (see
/// `openProjectMergeRecoveryTerminal`), unlike its snake_case siblings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryTerminalResult {
    pub path: String,
    pub cloned: bool,
    #[serde(rename = "recoveryRef")]
    pub recovery_ref: String,
    #[serde(rename = "targetRef")]
    pub target_ref: String,
}

/// A merge failure the client can act on.
///
/// Serialized as the invoke error payload, in exactly the shape
/// `parseProjectPullRequestMergeError` reads: `{code, message, recovery}` with
/// `recovery.action == "open_terminal"`. Anything else is a generic failure to
/// that parser, so these names are a contract rather than a convenience.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeFailure {
    pub code: String,
    pub message: String,
    pub recovery: Option<MergeRecovery>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeRecovery {
    pub action: String,
    #[serde(rename = "targetBranch")]
    pub target_branch: String,
    #[serde(rename = "sourceBranch")]
    pub source_branch: String,
}

impl MergeFailure {
    fn failed(detail: impl Into<String>) -> Self {
        Self {
            code: "merge_failed".to_string(),
            message: detail.into(),
            recovery: None,
        }
    }

    #[must_use]
    pub fn conflict(target_branch: &str, source_branch: &str, detail: &str) -> Self {
        Self {
            code: "merge_conflict".to_string(),
            message: format!(
                "The merge stopped on conflicting changes, so nothing was pushed. {detail}"
            ),
            recovery: Some(MergeRecovery {
                action: "open_terminal".to_string(),
                target_branch: target_branch.to_string(),
                source_branch: source_branch.to_string(),
            }),
        }
    }

    #[must_use]
    pub fn stale(expected: &str, found: &str) -> Self {
        Self {
            code: "stale_source".to_string(),
            message: format!(
                "This pull request has moved since the page was loaded (expected {expected}, \
                 found {found}), so the merge was not attempted. Reload and try again."
            ),
            recovery: None,
        }
    }

    pub fn to_payload(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| {
            "{\"code\":\"internal\",\"message\":\"the merge failed\",\"recovery\":null}"
                .to_string()
        })
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MergeResult {
    pub message: String,
    pub merge_commit: String,
    /// The signed merged-status event, or empty when this shell did not sign one.
    pub status_event: String,
    pub status_publication_error: Option<String>,
}

/// Why this shell does not publish the merged-status event itself.
///
/// Stated as a constant because it is a DESIGN consequence rather than a bug:
/// the identity lives in the client (the same key the browser build uses, so one
/// person has one key across both), and a shell that does not hold a signing key
/// cannot sign an event. Upstream's desktop signs here because it owns the
/// keystore; this thin shell deliberately does not.
pub const STATUS_NOT_SIGNED: &str =
    "The merge is committed and pushed. The merged-status event was not published from here: \
     this desktop shell does not hold your signing key — your identity stays in the app itself, \
     the same key the browser build uses — so publish the status, or close the pull request, \
     from the pull request view.";

#[derive(Debug, Clone)]
pub struct Workspace {
    git: Git,
    repos_dir: PathBuf,
}

impl Workspace {
    pub fn new(git: Git, repos_dir: &Path) -> Result<Self> {
        Ok(Self {
            git,
            repos_dir: paths::validate_repos_dir(repos_dir)?,
        })
    }

    #[must_use]
    pub fn repos_dir(&self) -> &Path {
        &self.repos_dir
    }

    pub fn git_identity(&self) -> Result<serde_json::Value> {
        Ok(serde_json::json!({
            "name": self.git.try_stdout(None, &["config", "--get", "user.name"]),
            "email": self.git.try_stdout(None, &["config", "--get", "user.email"]),
        }))
    }

    /// `discover_git_bash_prerequisite` — is a usable git present?
    ///
    /// Named for Windows, where git ships with Git Bash and its absence is the
    /// usual reason nothing works; the answer is useful everywhere.
    pub fn git_prerequisite(&self) -> serde_json::Value {
        match self.git.version() {
            Ok(version) => serde_json::json!({
                "available": true,
                "version": version,
                "path": self.git.program(),
                "install_hint": "",
            }),
            Err(e) => serde_json::json!({
                "available": false,
                "version": null,
                "path": self.git.program(),
                "install_hint": format!(
                    "git is not available to this app ({e}). Install it (on Windows, Git for \
                     Windows includes Git Bash) and restart."
                ),
            }),
        }
    }

    pub fn local_repositories(&self) -> Result<Vec<serde_json::Value>> {
        Ok(paths::local_repositories(&self.repos_dir)?
            .into_iter()
            .map(|(name, path)| {
                serde_json::json!({ "name": name, "path": path.to_string_lossy() })
            })
            .collect())
    }

    pub fn repo_path(&self, project_dtag: &str) -> Result<PathBuf> {
        paths::repo_dir(&self.repos_dir, project_dtag)
    }

    pub fn is_cloned(&self, project_dtag: &str) -> bool {
        self.repo_path(project_dtag)
            .map(|path| path.join(".git").exists())
            .unwrap_or(false)
    }

    pub fn clone_repository(
        &self,
        project_dtag: &str,
        clone_url: &str,
        default_branch: Option<&str>,
    ) -> Result<CloneResult> {
        let clone_url = validate_clone_url(clone_url)?;
        let default_branch = optional_git_ref(default_branch)?;
        let path = self.repo_path(project_dtag)?;
        if path.join(".git").exists() {
            return Ok(CloneResult {
                path: path.to_string_lossy().into_owned(),
                cloned: false,
                message: "Already cloned.".to_string(),
            });
        }
        std::fs::create_dir_all(&self.repos_dir).map_err(|e| {
            Error::Internal(format!(
                "could not create {}: {e}",
                self.repos_dir.display()
            ))
        })?;
        let target = path.to_string_lossy().into_owned();
        self.run_clone(clone_url, default_branch, &target)?;
        Ok(CloneResult {
            path: target,
            cloned: true,
            message: "Cloned.".to_string(),
        })
    }

    fn run_clone(&self, clone_url: &str, default_branch: Option<&str>, target: &str) -> Result<()> {
        let mut args: Vec<&str> = vec!["clone", "--quiet"];
        if let Some(branch) = default_branch {
            args.push("--branch");
            args.push(branch);
        }
        args.push("--");
        args.push(clone_url);
        args.push(target);
        self.git.run(None, &args).map(|_| ())
    }

    pub fn local_snapshot(
        &self,
        project_dtag: &str,
        reference: Option<&str>,
    ) -> Result<Option<LocalRepoSnapshot>> {
        if !self.is_cloned(project_dtag) {
            return Ok(None);
        }
        let path = self.repo_path(project_dtag)?;
        let snapshot = self.snapshot_at(&path, reference.unwrap_or("HEAD"))?;
        Ok(Some(LocalRepoSnapshot {
            path: path.to_string_lossy().into_owned(),
            snapshot,
        }))
    }

    pub fn snapshot_at(&self, repo: &Path, reference: &str) -> Result<RepoSnapshot> {
        let reference = validate_git_ref(reference)?;
        let log = self.git.run(
            Some(repo),
            &[
                "log",
                &format!("--format={}", git::LOG_FORMAT),
                "-n",
                &SNAPSHOT_COMMITS.to_string(),
                "--end-of-options",
                reference,
                "--",
            ],
        )?;
        let commits = git::parse_log(&log);
        let files = self.tree_files(repo, reference)?;
        Ok(RepoSnapshot {
            latest_commit: commits.first().cloned(),
            contributors: contributors_from(&commits),
            commits,
            files,
        })
    }

    /// The tracked files at `reference`, with detail for the first few.
    fn tree_files(&self, repo: &Path, reference: &str) -> Result<Vec<RepoFile>> {
        let reference = validate_git_ref(reference)?;
        let listing = self
            .git
            .run(Some(repo), &["ls-tree", "-r", "-l", "--full-name", reference])?;
        let mut files = Vec::new();
        for (index, line) in listing.lines().enumerate() {
            let Some(entry) = parse_ls_tree_line(line) else {
                continue;
            };
            let detailed = index < SNAPSHOT_FILE_DETAIL;
            let latest_commit = if detailed {
                self.git
                    .output(
                        Some(repo),
                        &[
                            "log",
                            "-1",
                            &format!("--format={}", git::LOG_FORMAT),
                            reference,
                            "--",
                            &entry.path,
                        ],
                    )
                    .ok()
                    .filter(|out| out.ok)
                    .and_then(|out| git::parse_log(&out.stdout).into_iter().next())
            } else {
                None
            };
            let preview_content = if detailed
                && entry.size.is_some_and(|size| (size as usize) <= PREVIEW_MAX_BYTES)
            {
                self.git
                    .output(
                        Some(repo),
                        &["show", &format!("{reference}:{}", entry.path)],
                    )
                    .ok()
                    .filter(|out| out.ok)
                    // Only text: a binary blob's bytes are not a preview, and
                    // lossy-converting one would render as mojibake.
                    .filter(|out| !out.stdout.contains('\0'))
                    .map(|out| out.stdout)
            } else {
                None
            };
            files.push(RepoFile {
                path: entry.path,
                kind: "file".to_string(),
                size: entry.size,
                preview_content,
                last_changed_at: latest_commit.as_ref().map(|commit| commit.timestamp),
                latest_commit,
            });
        }
        Ok(files)
    }

    pub fn diff_between(
        &self,
        repo: &Path,
        base: &str,
        target: &str,
    ) -> Result<RepoDiff> {
        let base = validate_git_ref(base)?;
        let target = validate_git_ref(target)?;
        let range = format!("{base}..{target}");
        let numstat = self.git.run(Some(repo), &["diff", "--numstat", "--end-of-options", &range, "--"])?;
        let stats = git::parse_numstat(&numstat);
        let patch_text = self.git.run(Some(repo), &["diff", "--end-of-options", &range, "--"])?;
        let mut patches: std::collections::BTreeMap<String, String> =
            git::split_patches(&patch_text).into_iter().collect();

        let mut files = Vec::new();
        let mut additions = 0u32;
        let mut deletions = 0u32;
        for stat in &stats {
            additions = additions.saturating_add(stat.additions);
            deletions = deletions.saturating_add(stat.deletions);
            let (patch, truncated) = patches
                .remove(&stat.path)
                .map(|patch| git::truncate_patch(&patch, PATCH_MAX_LINES))
                .unwrap_or_default();
            files.push(DiffFile {
                path: stat.path.clone(),
                additions: stat.additions,
                deletions: stat.deletions,
                patch,
                truncated,
            });
        }
        let commit_body = self.git.try_stdout(
            Some(repo),
            &["log", "-1", "--format=%b", "--end-of-options", target, "--"],
        );
        Ok(RepoDiff {
            files,
            additions,
            deletions,
            commit_body,
        })
    }
}

/// Everything a sync decision needs about one repository.
///
/// Taken as a struct by [`Workspace::sync_status`], [`Workspace::push`] and
/// [`Workspace::pull`] alike, so the mutating commands CANNOT be called without
/// the facts the policy is decided from. That is the point: a gate that only
/// feeds the UI is a gate a direct `invoke` walks around, and the authority rule
/// here (a member's machine must not push with the platform account credential)
/// is not a presentation detail.
#[derive(Debug, Clone)]
pub struct SyncInputs<'a> {
    pub project_dtag: &'a str,
    pub clone_url: &'a str,
    /// The branch to act on; the checkout's current branch when absent.
    pub branch: Option<&'a str>,
    /// The project's declared trunk.
    pub trunk: &'a str,
    /// Whether a credential for this remote's host is available to git.
    pub credential: bool,
}

impl Workspace {
    pub fn sync_status(&self, inputs: &SyncInputs<'_>) -> Result<SyncStatus> {
        if !self.is_cloned(inputs.project_dtag) {
            // Not cloned is a STATE, not an error: the UI offers to clone.
            let reason = "This repository is not cloned on this machine yet.".to_string();
            return Ok(SyncStatus {
                local_path: None,
                local_branch: None,
                local_branches: Vec::new(),
                local_head: None,
                local_short_head: None,
                remote_branch: None,
                remote_head: None,
                remote_short_head: None,
                merge_base: None,
                ahead_count: 0,
                behind_count: 0,
                has_uncommitted_changes: false,
                has_untracked_files: false,
                can_push: false,
                push_block_reason: Some(reason.clone()),
                can_pull: false,
                pull_block_reason: Some(reason),
            });
        }
        let (facts, observed) = self.observe(inputs)?;
        let gate = sync_gate(&facts);
        Ok(SyncStatus {
            local_path: Some(observed.repo.to_string_lossy().into_owned()),
            local_branch: Some(facts.branch.clone()),
            local_branches: observed.branches,
            local_short_head: Some(short(&observed.local_head)),
            local_head: Some(observed.local_head),
            remote_branch: observed
                .remote_head
                .as_ref()
                .map(|_| format!("origin/{}", facts.branch)),
            remote_short_head: observed.remote_head.as_deref().map(short),
            remote_head: observed.remote_head,
            merge_base: observed.merge_base,
            ahead_count: facts.ahead,
            behind_count: facts.behind,
            has_uncommitted_changes: facts.dirty,
            has_untracked_files: facts.untracked,
            can_push: gate.can_push,
            push_block_reason: gate.push_block_reason,
            can_pull: gate.can_pull,
            pull_block_reason: gate.pull_block_reason,
        })
    }

    /// `push_project_local_repository` — gated.
    ///
    /// The gate is re-evaluated HERE, from the repository's current state, rather
    /// than trusted from whatever the UI last displayed: the status the user saw
    /// may be seconds old, and the authority rule must hold for a direct call
    /// too.
    pub fn push(&self, inputs: &SyncInputs<'_>) -> Result<PushResult> {
        if !self.is_cloned(inputs.project_dtag) {
            return Err(Error::Invalid(
                "This repository is not cloned on this machine yet.".into(),
            ));
        }
        let (facts, observed) = self.observe(inputs)?;
        if let Some(reason) = push_block_reason(&facts) {
            return Err(Error::Refused(reason));
        }
        let out = self.git.output(
            Some(&observed.repo),
            &["push", "origin", &format!("HEAD:{}", facts.branch)],
        )?;
        if !out.ok {
            return Ok(PushResult {
                pushed: false,
                // git's own words: a rejected push explains itself better than a
                // paraphrase, and the user often needs the detail verbatim — with
                // any credential in it redacted.
                message: crate::credentials::redact(out.stderr.trim()),
                branch: facts.branch,
                commit: observed.local_head,
                merge_base: observed.merge_base,
            });
        }
        Ok(PushResult {
            pushed: true,
            message: if out.stderr.trim().is_empty() {
                "Pushed.".to_string()
            } else {
                crate::credentials::redact(out.stderr.trim())
            },
            branch: facts.branch,
            commit: observed.local_head,
            merge_base: observed.merge_base,
        })
    }

    /// `pull_project_local_repository` — gated, and fast-forward only.
    ///
    /// A pull REFRESHES the remote refs first, and that is not an optimisation:
    /// [`Self::observe`] deliberately does not fetch (a status read must not wait
    /// on the network), so deciding "nothing to pull" from its cached
    /// `refs/remotes/origin/*` would mean a repository whose remote has moved is
    /// reported as up to date forever, and the button never works.
    ///
    /// The order matters: AUTHORITY is decided before the fetch, because a fetch
    /// spends the credential, and refusing afterwards would mean having used one
    /// the caller was never permitted to use.
    ///
    /// `--ff-only` deliberately: a merge commit created by a background pull is
    /// a commit nobody asked for, and a rebase would rewrite work the user has
    /// not reviewed. When it cannot fast-forward, saying so is the right answer.
    pub fn pull(&self, inputs: &SyncInputs<'_>) -> Result<PullResult> {
        if !self.is_cloned(inputs.project_dtag) {
            return Err(Error::Invalid(
                "This repository is not cloned on this machine yet.".into(),
            ));
        }
        let (facts, observed) = self.observe(inputs)?;
        if let Some(reason) = authority_block(&facts, Intent::Fetch) {
            return Err(Error::Refused(reason));
        }
        if facts.dirty {
            return Err(Error::Refused(
                "You have uncommitted changes, and pulling on top of them can leave the working \
                 copy half-merged. Commit or stash them first."
                    .into(),
            ));
        }
        let fetched = self
            .git
            .output(Some(&observed.repo), &["fetch", "--prune", "origin"])?;
        if !fetched.ok {
            return Ok(PullResult {
                pulled: false,
                message: crate::credentials::redact(fetched.stderr.trim()),
            });
        }
        // Re-read AFTER the fetch, so "up to date" is a fact about the remote
        // rather than about a cache.
        let (facts, observed) = self.observe(inputs)?;
        if facts.behind == 0 {
            return Ok(PullResult {
                pulled: false,
                message: "Nothing to pull — this branch is up to date.".to_string(),
            });
        }
        let out = self.git.output(
            Some(&observed.repo),
            &["pull", "--ff-only", "--", "origin", &facts.branch],
        )?;
        Ok(PullResult {
            pulled: out.ok,
            message: if out.ok {
                let text = out.stdout.trim();
                if text.is_empty() {
                    "Up to date.".to_string()
                } else {
                    text.to_string()
                }
            } else {
                crate::credentials::redact(out.stderr.trim())
            },
        })
    }
    /// Read the repository's current state — the facts a gate decides from.
    ///
    /// Deliberately does NOT fetch: this is what a status read calls, and a
    /// status read that reached the network would make opening a project panel
    /// wait on it. [`Self::pull`] fetches explicitly for that reason.
    fn observe(&self, inputs: &SyncInputs<'_>) -> Result<(SyncFacts, Observed)> {
        validate_clone_url(inputs.clone_url)?;
        let repo = self.repo_path(inputs.project_dtag)?;
        // The authority rules are judged from the remote that `git push origin`
        // and `git fetch origin` actually reach. A caller-supplied URL that is
        // not the checkout's origin would let one remote's rules (a GitHub
        // URL's) stand in for another's (the Ferrite RCS the checkout uses).
        let origin = self
            .git
            .try_stdout(Some(&repo), &["config", "--get", "remote.origin.url"])
            .map(|url| url.trim().to_string())
            .unwrap_or_default();
        if origin != inputs.clone_url {
            return Err(Error::Refused(format!(
                "this checkout's origin is {}, not the project's clone URL {}",
                crate::credentials::redact(&origin),
                crate::credentials::redact(inputs.clone_url)
            )));
        }
        let head_branch = self
            .git
            .run(Some(&repo), &["rev-parse", "--abbrev-ref", "HEAD"])?
            .trim()
            .to_string();
        let branch = match optional_git_ref(inputs.branch)? {
            Some(b) => b.to_string(),
            None => validate_git_ref(&head_branch)?.to_string(),
        };
        let local_head = self
            .git
            .run(Some(&repo), &["rev-parse", "HEAD"])?
            .trim()
            .to_string();
        let branches = git::parse_branch_list(&self.git.run(
            Some(&repo),
            &["branch", "--format=%(refname:short)"],
        )?);
        let worktree = git::parse_status(&self.git.run(Some(&repo), &["status", "--porcelain"])?);

        let remote_ref = format!("refs/remotes/origin/{branch}");
        let remote_head = self.git.try_stdout(Some(&repo), &["rev-parse", &remote_ref]);
        let (behind, ahead) = match &remote_head {
            Some(_) => {
                let counts = self.git.run(
                    Some(&repo),
                    &[
                        "rev-list",
                        "--left-right",
                        "--count",
                        &format!("{remote_ref}...HEAD"),
                    ],
                )?;
                git::parse_ahead_behind(&counts)
            }
            // No remote ref: everything local is ahead of nothing, which is what
            // a first push of a new branch is.
            None => (0, 1),
        };
        let merge_base = remote_head.as_ref().and_then(|_| {
            self.git
                .try_stdout(Some(&repo), &["merge-base", &remote_ref, "HEAD"])
        });

        Ok((
            SyncFacts {
                remote: remote_kind(inputs.clone_url),
                branch,
                trunk: inputs.trunk.to_string(),
                ahead,
                behind,
                dirty: worktree.is_dirty(),
                untracked: worktree.has_untracked(),
                credential: inputs.credential,
                // Multi-branch transport is not available; see `SyncFacts::rcs_multi_branch`.
                rcs_multi_branch: false,
            },
            Observed {
                repo,
                local_head,
                branches,
                remote_head,
                merge_base,
            },
        ))
    }
}

struct Observed {
    repo: PathBuf,
    local_head: String,
    branches: Vec<String>,
    remote_head: Option<String>,
    merge_base: Option<String>,
}

/// A stable directory name for a clone URL.
///
/// Hashed rather than slugified from the URL, because a URL contains characters
/// a path may not and two different URLs can slug to the same name. A short
/// readable prefix is kept so a person looking at the cache directory can tell
/// which repository is which; the hash is what makes it unique.
#[must_use]
pub fn cache_name(clone_url: &str) -> String {
    // FNV-1a, 64-bit. A cryptographic hash is not needed — this only has to be
    // stable and collision-resistant enough for a handful of directories — and
    // this crate deliberately has no hashing dependency.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in clone_url.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    let stem: String = clone_url
        .trim_end_matches(".git")
        .rsplit('/')
        .next()
        .unwrap_or("repo")
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(24)
        .collect();
    let stem = if stem.is_empty() {
        "repo".to_string()
    } else {
        stem
    };
    format!("{stem}-{hash:016x}")
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeRequest {
    pub target_clone_url: String,
    pub source_clone_url: String,
    pub target_branch: String,
    pub source_branch: String,
    /// The source tip the client believed it was merging.
    pub expected_commit: String,
}

impl Workspace {
    /// A checkout for a clone URL, when no project id is available.
    ///
    /// Under `.cache/` inside the repositories directory, which keeps it out of
    /// [`paths::local_repositories`] (that only reports directories holding a
    /// `.git`, and `.cache` does not) so it never shows up as one of the user's
    /// own checkouts.
    pub fn cache_checkout(
        &self,
        clone_url: &str,
        default_branch: Option<&str>,
    ) -> Result<PathBuf> {
        self.cache_checkout_with(clone_url, default_branch, false)
    }

    /// A cache checkout whose refs are KNOWN to be current.
    ///
    /// Every remote MUTATION uses this rather than [`Self::cache_checkout`], and
    /// the difference is a safety property, not a performance one: a read may
    /// answer from a stale cache (visibly stale beats unavailable), but a tip
    /// check against stale refs is worthless — a cached tip can still equal the
    /// commit the client saw while the remote has moved on, and a delete would
    /// then discard commits nobody has seen. A fetch that fails is therefore an
    /// error here.
    pub fn cache_checkout_fresh(
        &self,
        clone_url: &str,
        default_branch: Option<&str>,
    ) -> Result<PathBuf> {
        self.cache_checkout_with(clone_url, default_branch, true)
    }

    fn cache_checkout_with(
        &self,
        clone_url: &str,
        default_branch: Option<&str>,
        require_fresh: bool,
    ) -> Result<PathBuf> {
        let clone_url = validate_clone_url(clone_url)?;
        let default_branch = optional_git_ref(default_branch)?;
        let cache = self.repos_dir.join(".cache");
        std::fs::create_dir_all(&cache)
            .map_err(|e| Error::Internal(format!("could not create {}: {e}", cache.display())))?;
        let dir = paths::contained(&self.repos_dir, &cache.join(cache_name(clone_url)))?;
        if dir.join(".git").exists() {
            let fetched = self
                .git
                .output(Some(&dir), &["fetch", "--prune", "--tags", "origin"])?;
            if !fetched.ok && require_fresh {
                return Err(Error::Tool {
                    program: "git".to_string(),
                    detail: format!(
                        "the remote could not be refreshed, so this operation was not \
                         attempted — acting on stale refs could discard commits: {}",
                        crate::credentials::redact(fetched.stderr.trim())
                    ),
                });
            }
            // A read tolerates a failed refresh and answers from what is
            // already there: visibly stale beats unavailable.
            return Ok(dir);
        }
        let target = dir.to_string_lossy().into_owned();
        self.run_clone(clone_url, default_branch, &target)?;
        Ok(dir)
    }

    pub fn remote_snapshot(
        &self,
        clone_url: &str,
        default_branch: Option<&str>,
        reference: Option<&str>,
    ) -> Result<RepoSnapshot> {
        let dir = self.cache_checkout(clone_url, default_branch)?;
        // Read `origin/HEAD` rather than assuming `main` when no ref is given.
        let reference = match optional_git_ref(reference)? {
            Some(reference) => reference.to_string(),
            None => self.default_reference(&dir, default_branch),
        };
        self.snapshot_at(&dir, &reference)
    }

    pub fn remote_diff(
        &self,
        clone_url: &str,
        default_branch: Option<&str>,
        base: Option<&str>,
        target: Option<&str>,
    ) -> Result<RepoDiff> {
        let dir = self.cache_checkout(clone_url, default_branch)?;
        let base = match base.filter(|b| !b.is_empty()) {
            Some(base) => remote_ref(base),
            None => self.default_reference(&dir, default_branch),
        };
        let target = match target.filter(|t| !t.is_empty()) {
            Some(target) => remote_ref(target),
            None => "HEAD".to_string(),
        };
        self.diff_between(&dir, &base, &target)
    }

    fn default_reference(&self, dir: &Path, default_branch: Option<&str>) -> String {
        if let Some(branch) = default_branch.filter(|b| !b.is_empty()) {
            return remote_ref(branch);
        }
        self.git
            .try_stdout(
                Some(dir),
                &["symbolic-ref", "--short", "refs/remotes/origin/HEAD"],
            )
            .unwrap_or_else(|| "HEAD".to_string())
    }

    /// `create_project_remote_branch` — branch a remote at a known commit.
    ///
    /// `expected_commit` is verified against the source branch's ACTUAL tip
    /// first: the client read that commit some time ago, and branching from a tip
    /// that has moved would silently include work the person never saw.
    pub fn create_remote_branch(
        &self,
        clone_url: &str,
        source_branch: &str,
        expected_commit: &str,
        new_branch: &str,
    ) -> Result<BranchResult> {
        let source_branch = validate_git_ref(source_branch)?;
        let new_branch = validate_git_ref(new_branch)?;
        let dir = self.cache_checkout_fresh(clone_url, Some(source_branch))?;
        let tip = self
            .git
            .run(
                Some(&dir),
                &["rev-parse", &format!("refs/remotes/origin/{source_branch}")],
            )?
            .trim()
            .to_string();
        if !expected_commit.is_empty() && tip != expected_commit {
            return Err(Error::Refused(format!(
                "{source_branch} has moved since this page was loaded (expected \
                 {expected_commit}, found {tip}), so no branch was created. Reload and try \
                 again."
            )));
        }
        // `--force-with-lease=<ref>:` with an EMPTY expectation means "only if
        // this ref does not exist", which makes creation atomic: a branch
        // somebody else created in the meantime is not silently overwritten.
        self.git.run(
            Some(&dir),
            &[
                "push",
                &format!("--force-with-lease=refs/heads/{new_branch}:"),
                "origin",
                &format!("{tip}:refs/heads/{new_branch}"),
            ],
        )?;
        Ok(BranchResult {
            branch: new_branch.to_string(),
            commit: tip,
            message: format!("Created {new_branch} from {source_branch}."),
        })
    }

    /// `delete_project_remote_branch`.
    ///
    /// Deleting is the one operation here that can destroy work, so it is the
    /// most careful: the refs are REFRESHED (a stale cache would make the tip
    /// check meaningless), the tip is compared against what the client saw, and
    /// the delete itself carries `--force-with-lease` so the REMOTE re-checks the
    /// same expectation atomically. Without the lease there is still a window
    /// between the check and the push in which somebody could push a commit that
    /// this delete would then discard.
    pub fn delete_remote_branch(
        &self,
        clone_url: &str,
        branch: &str,
        expected_commit: &str,
    ) -> Result<BranchResult> {
        let branch = validate_git_ref(branch)?;
        let dir = self.cache_checkout_fresh(clone_url, None)?;
        let tip = self
            .git
            .run(Some(&dir), &["rev-parse", &format!("refs/remotes/origin/{branch}")])?
            .trim()
            .to_string();
        if !expected_commit.is_empty() && tip != expected_commit {
            return Err(Error::Refused(format!(
                "{branch} has moved since this page was loaded (expected {expected_commit}, \
                 found {tip}), so it was NOT deleted — deleting it now would discard commits \
                 nobody has seen. Reload and try again."
            )));
        }
        // The lease is the client's OWN expectation when it gave one: that is the
        // commit a person looked at, and it is what the remote should re-verify.
        let lease = if expected_commit.is_empty() {
            tip.clone()
        } else {
            expected_commit.to_string()
        };
        self.git.run(
            Some(&dir),
            &[
                "push",
                &format!("--force-with-lease=refs/heads/{branch}:{lease}"),
                "origin",
                &format!(":refs/heads/{branch}"),
            ],
        )?;
        Ok(BranchResult {
            branch: branch.to_string(),
            commit: tip,
            message: format!("Deleted {branch}."),
        })
    }

    /// `merge_project_pull_request` — a real git merge, then a push.
    ///
    /// The sequence is the one a person would run by hand, which is the point:
    /// fetch both sides, verify the source tip is what the client showed, merge
    /// with a merge commit, push. A conflict ABORTS the merge and answers with
    /// the structured failure the client's recovery dialog reads, so the working
    /// copy is never left half-merged.
    ///
    /// The merged-STATUS event is not published from here — see
    /// [`STATUS_NOT_SIGNED`] for why, and note that it is reported rather than
    /// silently skipped.
    pub fn merge_pull_request(
        &self,
        request: &MergeRequest,
    ) -> std::result::Result<MergeResult, MergeFailure> {
        let dir = self.prepare_merge_source(request)?;
        let merge_commit = self.execute_merge_and_push(&dir, request)?;
        Ok(MergeResult {
            message: format!(
                "Merged {} into {}.",
                request.source_branch, request.target_branch
            ),
            merge_commit,
            status_event: String::new(),
            status_publication_error: Some(STATUS_NOT_SIGNED.to_string()),
        })
    }

    fn prepare_merge_source(
        &self,
        request: &MergeRequest,
    ) -> std::result::Result<PathBuf, MergeFailure> {
        let map_err = |e: Error| MergeFailure::failed(e.to_string());
        let target_clone_url = validate_clone_url(&request.target_clone_url).map_err(map_err)?;
        let source_clone_url = validate_clone_url(&request.source_clone_url).map_err(map_err)?;
        let target_branch = validate_git_ref(&request.target_branch).map_err(map_err)?;
        let source_branch = validate_git_ref(&request.source_branch).map_err(map_err)?;
        let dir = self
            .cache_checkout_fresh(target_clone_url, Some(target_branch))
            .map_err(map_err)?;

        // The source may live in a fork, so fetch by URL into a dedicated ref.
        let fetch = self
            .git
            .output(
                Some(&dir),
                &[
                    "fetch",
                    "--no-tags",
                    "--",
                    source_clone_url,
                    &format!("+refs/heads/{source_branch}:refs/buzz/source"),
                ],
            )
            .map_err(map_err)?;
        if !fetch.ok {
            return Err(MergeFailure::failed(format!(
                "the source branch could not be fetched: {}",
                crate::credentials::redact(fetch.stderr.trim())
            )));
        }
        let tip = self
            .git
            .run(Some(&dir), &["rev-parse", "refs/buzz/source"])
            .map_err(map_err)?
            .trim()
            .to_string();
        if !request.expected_commit.is_empty() && tip != request.expected_commit {
            return Err(MergeFailure::stale(&request.expected_commit, &tip));
        }
        Ok(dir)
    }

    fn execute_merge_and_push(
        &self,
        dir: &Path,
        request: &MergeRequest,
    ) -> std::result::Result<String, MergeFailure> {
        let map_err = |e: Error| MergeFailure::failed(e.to_string());
        // Remember the target tip so the push lease verifies nothing landed concurrently.
        let target_before = self
            .git
            .run(
                Some(dir),
                &[
                    "rev-parse",
                    &format!("refs/remotes/origin/{}", request.target_branch),
                ],
            )
            .map_err(map_err)?
            .trim()
            .to_string();
        let checkout = self
            .git
            .output(
                Some(dir),
                &[
                    "checkout",
                    "--quiet",
                    "-B",
                    &request.target_branch,
                    &format!("refs/remotes/origin/{}", request.target_branch),
                ],
            )
            .map_err(map_err)?;
        if !checkout.ok {
            return Err(MergeFailure::failed(format!(
                "the target branch could not be checked out: {}",
                checkout.stderr.trim()
            )));
        }

        let merge = self
            .git
            .output(
                Some(dir),
                &[
                    "merge",
                    "--no-ff",
                    "-m",
                    &format!(
                        "Merge {} into {}",
                        request.source_branch, request.target_branch
                    ),
                    "refs/buzz/source",
                ],
            )
            .map_err(map_err)?;
        if !merge.ok {
            let conflicts = self
                .git
                .output(Some(dir), &["diff", "--name-only", "--diff-filter=U"])
                .ok()
                .map(|out| out.stdout.trim().to_string())
                .filter(|text| !text.is_empty())
                .unwrap_or_else(|| merge.stdout.trim().to_string());
            // Leave nothing half-merged: the recovery flow expects a clean tree.
            let _ = self.git.output(Some(dir), &["merge", "--abort"]);
            return Err(MergeFailure::conflict(
                &request.target_branch,
                &request.source_branch,
                &if conflicts.is_empty() {
                    "Finish it in a terminal.".to_string()
                } else {
                    format!("Conflicting: {}.", conflicts.replace('\n', ", "))
                },
            ));
        }

        let merge_commit = self
            .git
            .run(Some(dir), &["rev-parse", "HEAD"])
            .map_err(map_err)?
            .trim()
            .to_string();

        let push = self
            .git
            .output(
                Some(dir),
                &[
                    "push",
                    &format!(
                        "--force-with-lease=refs/heads/{}:{}",
                        request.target_branch, target_before
                    ),
                    "origin",
                    &format!("HEAD:refs/heads/{}", request.target_branch),
                ],
            )
            .map_err(map_err)?;
        if !push.ok {
            return Err(MergeFailure {
                code: "push_rejected".to_string(),
                message: format!(
                    "The merge was made locally but the push was refused: {}",
                    crate::credentials::redact(push.stderr.trim())
                ),
                recovery: Some(MergeRecovery {
                    action: "open_terminal".to_string(),
                    target_branch: request.target_branch.clone(),
                    source_branch: request.source_branch.clone(),
                }),
            });
        }
        Ok(merge_commit)
    }

    /// `open_project_merge_recovery_terminal` — a checkout with both sides
    /// fetched, and a terminal at it.
    ///
    /// The refs are prepared so the person can finish the merge by hand: this is
    /// what the conflict failure's recovery action leads to, and arriving at a
    /// directory where the source branch is not even fetched would make it
    /// useless.
    pub fn recovery_terminal(
        &self,
        request: &MergeRequest,
    ) -> Result<RecoveryTerminalResult> {
        let target_clone_url = validate_clone_url(&request.target_clone_url)?;
        let source_clone_url = validate_clone_url(&request.source_clone_url)?;
        let target_branch = validate_git_ref(&request.target_branch)?;
        let source_branch = validate_git_ref(&request.source_branch)?;
        let existed = self
            .cache_dir_exists(target_clone_url);
        let dir = self.cache_checkout(target_clone_url, Some(target_branch))?;
        let recovery_ref = format!("refs/buzz/recover/{source_branch}");
        self.git.run(
            Some(&dir),
            &[
                "fetch",
                "--no-tags",
                "--",
                source_clone_url,
                &format!("+refs/heads/{source_branch}:{recovery_ref}"),
            ],
        )?;
        let target_ref = format!("refs/remotes/origin/{target_branch}");
        self.spawn_terminal_at(&dir)?;
        Ok(RecoveryTerminalResult {
            path: dir.to_string_lossy().into_owned(),
            cloned: !existed,
            recovery_ref,
            target_ref,
        })
    }

    fn cache_dir_exists(&self, clone_url: &str) -> bool {
        self.repos_dir
            .join(".cache")
            .join(cache_name(clone_url))
            .join(".git")
            .exists()
    }
}

/// A branch name as a remote-tracking ref, leaving an explicit ref or a commit
/// id alone.
///
/// A cache checkout has no local branches but every remote one, so `main` must
/// be read as `origin/main` — while `refs/…`, `origin/…` and a raw sha are
/// already resolvable and must not be prefixed.
#[must_use]
pub fn remote_ref(reference: &str) -> String {
    let reference = reference.trim();
    if reference.starts_with("refs/")
        || reference.starts_with("origin/")
        || reference == "HEAD"
        || (reference.len() >= 7 && reference.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return reference.to_string();
    }
    format!("origin/{reference}")
}

struct TreeEntry {
    path: String,
    size: Option<u64>,
}

fn parse_ls_tree_line(line: &str) -> Option<TreeEntry> {
    let (meta, path) = line.split_once('\t')?;
    let mut fields = meta.split_whitespace();
    let _mode = fields.next()?;
    let kind = fields.next()?;
    let _sha = fields.next()?;
    // `-` for a tree or a submodule, where a byte size is meaningless.
    let size = fields.next().and_then(|value| value.parse::<u64>().ok());
    if kind != "blob" {
        return None;
    }
    Some(TreeEntry {
        path: git::unquote_path(path.trim()),
        size,
    })
}

/// Aggregate contributors out of the commits a snapshot read.
///
/// Derived from the SAME commits the snapshot shows, deliberately: a separate
/// `git shortlog` over all history would report contributors the commit list
/// does not contain, and the panel would disagree with itself.
#[must_use]
pub fn contributors_from(commits: &[Commit]) -> Vec<Contributor> {
    let mut by_email: std::collections::BTreeMap<String, Contributor> =
        std::collections::BTreeMap::new();
    for commit in commits {
        let key = commit.author_email.to_ascii_lowercase();
        let entry = by_email.entry(key).or_insert_with(|| Contributor {
            name: commit.author_name.clone(),
            email: commit.author_email.clone(),
            commit_count: 0,
            last_commit_at: 0,
        });
        entry.commit_count = entry.commit_count.saturating_add(1);
        entry.last_commit_at = entry.last_commit_at.max(commit.timestamp);
    }
    let mut out: Vec<Contributor> = by_email.into_values().collect();
    // Most commits first, then most recent — a stable order, so a re-read does
    // not reshuffle the list.
    out.sort_by(|a, b| {
        b.commit_count
            .cmp(&a.commit_count)
            .then(b.last_commit_at.cmp(&a.last_commit_at))
            .then(a.email.cmp(&b.email))
    });
    out
}

fn short(hash: &str) -> String {
    hash.chars().take(7).collect()
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalCommand {
    pub program: String,
    pub args: Vec<String>,
    /// Whether the path is passed as an ARGUMENT. When false, the spawner must
    /// set the child's working directory instead — see
    /// [`terminal_candidates`] for why that distinction is a security property
    /// rather than a style choice.
    pub path_as_arg: bool,
    /// Whether the child needs a console window of its own (Windows only).
    pub new_console: bool,
}

/// The ways to open a terminal at `path`, in preference order.
///
/// Candidates rather than one answer, because Linux has no single terminal: the
/// first that exists is used. Pure, so the table is testable without spawning.
///
/// **The untrusted path never reaches a command interpreter.** A repository
/// directory is named from a `d` tag, so it can contain `&`, `|`, `%VAR%` or a
/// quote. Two consequences:
///
/// * On Windows the path is NOT passed to `cmd.exe` at all. Everything after
///   `cmd /C` is command TEXT that cmd re-parses with its own metacharacter and
///   `%`-expansion rules, and no amount of argv quoting makes that inert. The
///   terminal is spawned with its working directory set instead, which never
///   goes through a parser.
/// * On Linux the same approach is used — every one of these terminals inherits
///   the spawner's working directory — so no flag needs to carry the path either.
///
/// macOS is the one case where the path IS an argument: `open` is `execve`'d
/// directly with an argv, no shell in between, and `Terminal.app` ignores an
/// inherited working directory.
#[must_use]
pub fn terminal_candidates(os: &str, path: &str) -> Vec<TerminalCommand> {
    match os {
        "macos" => vec![TerminalCommand {
            program: "open".to_string(),
            args: vec!["-a".into(), "Terminal".into(), path.to_string()],
            path_as_arg: true,
            new_console: false,
        }],
        "windows" => vec![
            // Windows Terminal when present, then the console host. Neither is
            // handed the path: `-d .` makes wt.exe open in the working
            // directory the spawner sets (without it, wt opens its profile's
            // default directory), and cmd.exe needs a console created for it
            // because a GUI parent has none to share.
            TerminalCommand {
                program: "wt.exe".to_string(),
                args: vec!["-d".into(), ".".into()],
                path_as_arg: false,
                new_console: false,
            },
            TerminalCommand {
                program: "cmd.exe".to_string(),
                args: Vec::new(),
                path_as_arg: false,
                new_console: true,
            },
        ],
        // Linux and the BSDs: the desktop-agnostic wrapper first, then the
        // terminals people actually have. All inherit the working directory.
        _ => ["x-terminal-emulator", "gnome-terminal", "konsole", "xterm"]
            .into_iter()
            .map(|program| TerminalCommand {
                program: program.to_string(),
                args: Vec::new(),
                path_as_arg: false,
                new_console: false,
            })
            .collect(),
    }
}

impl Workspace {
    /// Open a terminal at a project's checkout.
    ///
    /// The working directory is set on the child process, which is what makes a
    /// hostile directory name inert: it is passed as a path, never as text a
    /// shell or `cmd.exe` parses.
    pub fn open_terminal(&self, project_dtag: &str) -> Result<TerminalResult> {
        let repo = self.repo_path(project_dtag)?;
        if !repo.join(".git").exists() {
            return Err(Error::Invalid(
                "This repository is not cloned on this machine yet.".into(),
            ));
        }
        self.spawn_terminal_at(&repo)?;
        Ok(TerminalResult {
            path: repo.to_string_lossy().into_owned(),
            cloned: false,
        })
    }

    pub fn spawn_terminal_at(&self, dir: &Path) -> Result<()> {
        let path = dir.to_string_lossy().into_owned();
        launch_first(&terminal_candidates(std::env::consts::OS, &path), dir).map(|_| ())
    }
}

/// How long a launcher is watched for an immediate failure before it counts
/// as started. Launchers either hand off and exit 0 (`wt.exe`, `open`,
/// `gnome-terminal`) or keep running as the terminal; one that cannot open a
/// window (no display, no session bus) exits non-zero within this window.
const LAUNCH_CONFIRM: std::time::Duration = std::time::Duration::from_millis(500);

/// Start the first candidate that works in `dir`, returning its program name.
///
/// A candidate works when it starts and does not exit with a failure status
/// within [`LAUNCH_CONFIRM`]; a launcher that starts and then fails falls
/// through to the next candidate.
fn launch_first(candidates: &[TerminalCommand], dir: &Path) -> Result<String> {
    let mut last: Option<String> = None;
    for candidate in candidates {
        let mut command = std::process::Command::new(&candidate.program);
        command.args(&candidate.args);
        // ALWAYS set the working directory, including for the macOS case —
        // it is harmless there and means one code path cannot forget it.
        command.current_dir(dir);
        #[cfg(windows)]
        if candidate.new_console {
            use std::os::windows::process::CommandExt;
            const CREATE_NEW_CONSOLE: u32 = 0x0000_0010;
            command.creation_flags(CREATE_NEW_CONSOLE);
        }
        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(e) => {
                last = Some(format!("{}: {e}", candidate.program));
                continue;
            }
        };
        let deadline = std::time::Instant::now() + LAUNCH_CONFIRM;
        let failed = loop {
            match child.try_wait() {
                Ok(Some(status)) => break (!status.success()).then(|| status.to_string()),
                Ok(None) if std::time::Instant::now() >= deadline => {
                    // Still running: it is the terminal. Reap it when it ends
                    // so it never lingers as a zombie of this process.
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                    break None;
                }
                Ok(None) => std::thread::sleep(std::time::Duration::from_millis(20)),
                Err(e) => break Some(e.to_string()),
            }
        };
        match failed {
            None => return Ok(candidate.program.clone()),
            Some(why) => last = Some(format!("{}: {why}", candidate.program)),
        }
    }
    Err(Error::Tool {
        program: "terminal".to_string(),
        detail: format!(
            "no terminal could be started ({}). The repository is at {}.",
            last.unwrap_or_else(|| "no candidates".to_string()),
            dir.display()
        ),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn facts() -> SyncFacts {
        SyncFacts {
            remote: RemoteKind::GitHub,
            branch: "feature/x".into(),
            trunk: "main".into(),
            ahead: 2,
            behind: 0,
            dirty: false,
            untracked: false,
            credential: true,
            rcs_multi_branch: false,
        }
    }

    #[test]
    fn remote_kinds_are_decided_by_host_not_by_substring() {
        assert_eq!(
            remote_kind("https://github.com/acme/widgets.git"),
            RemoteKind::GitHub
        );
        assert_eq!(remote_kind("git@github.com:acme/widgets.git"), RemoteKind::GitHub);
        assert_eq!(
            remote_kind("https://git.example.com/acct/app.git"),
            RemoteKind::Rcs
        );
        assert_eq!(remote_kind("https://gitlab.com/acme/widgets.git"), RemoteKind::Other);
        // The trap: a repository NAMED like the other host must not be
        // misclassified, because the classification decides whose credential a
        // push would spend.
        assert_eq!(
            remote_kind("https://github.com/acme/git.example.git"),
            RemoteKind::GitHub
        );
        assert_eq!(
            remote_kind("https://example.com/git.something/repo.git"),
            RemoteKind::Other
        );
    }

    #[test]
    fn hosts_are_extracted_from_both_url_shapes() {
        assert_eq!(
            host_of("https://user:pass@github.com:443/a/b.git").as_deref(),
            Some("github.com")
        );
        assert_eq!(host_of("git@github.com:a/b.git").as_deref(), Some("github.com"));
        assert_eq!(host_of("ssh://git@git.example/a/b").as_deref(), Some("git.example"));
        assert_eq!(host_of("not a url"), None);
    }

    #[test]
    fn a_github_remote_behaves_like_any_git_client() {
        let gate = sync_gate(&facts());
        assert!(gate.can_push, "{:?}", gate.push_block_reason);
        assert!(!gate.can_pull, "nothing to pull");
        assert!(gate
            .pull_block_reason
            .expect("reason")
            .contains("up to date"));
    }

    #[test]
    fn an_rcs_remote_without_the_account_credential_names_the_mediated_path() {
        // The core of the dispatch design: a member's machine does not hold the
        // account owner token, and the refusal has to say what DOES work.
        let gate = sync_gate(&SyncFacts {
            remote: RemoteKind::Rcs,
            credential: false,
            branch: "main".into(),
            ..facts()
        });
        assert!(!gate.can_push);
        let reason = gate.push_block_reason.expect("a reason");
        assert!(reason.contains("account owner token"), "{reason}");
        assert!(
            reason.contains("process commands"),
            "the refusal must name the alternative: {reason}"
        );
    }

    #[test]
    fn an_rcs_branch_push_is_refused_until_multi_branch_lands() {
        // The expected-fail gate: today the transport serves
        // only the trunk, and flipping `rcs_multi_branch` is what makes this
        // refusal disappear — no other change.
        let operator = SyncFacts {
            remote: RemoteKind::Rcs,
            credential: true,
            branch: "feature/x".into(),
            ..facts()
        };
        let gate = sync_gate(&operator);
        assert!(!gate.can_push);
        let reason = gate.push_block_reason.expect("a reason");
        assert!(reason.contains("serves only"), "{reason}");
        assert!(reason.contains("main"), "{reason}");

        // The trunk itself pushes today…
        let trunk = SyncFacts {
            branch: "main".into(),
            ..operator.clone()
        };
        assert!(sync_gate(&trunk).can_push);
        // …and when the platform grows the capability, so does the branch.
        let after_cr = SyncFacts {
            rcs_multi_branch: true,
            ..operator
        };
        assert!(
            sync_gate(&after_cr).can_push,
            "the only change needed when multi-branch transport lands is this flag"
        );
    }

    #[test]
    fn being_behind_is_reported_before_being_dirty_for_a_push() {
        let gate = sync_gate(&SyncFacts {
            behind: 3,
            dirty: true,
            ..facts()
        });
        assert!(!gate.can_push);
        assert!(gate.push_block_reason.expect("reason").contains("behind"));
    }

    #[test]
    fn nothing_to_push_is_a_reason_not_a_failure() {
        let gate = sync_gate(&SyncFacts { ahead: 0, ..facts() });
        assert!(!gate.can_push);
        assert!(gate
            .push_block_reason
            .expect("reason")
            .contains("Nothing to push"));
    }

    #[test]
    fn a_dirty_tree_blocks_a_pull_but_untracked_files_do_not() {
        let dirty = sync_gate(&SyncFacts {
            dirty: true,
            behind: 1,
            ..facts()
        });
        assert!(!dirty.can_pull);
        assert!(dirty.pull_block_reason.expect("reason").contains("uncommitted"));

        // Untracked files do NOT block: git refuses only when an incoming file
        // would overwrite one, so pre-refusing would be wrong more often than
        // right.
        let untracked = sync_gate(&SyncFacts {
            untracked: true,
            behind: 1,
            ..facts()
        });
        assert!(untracked.can_pull, "{:?}", untracked.pull_block_reason);
    }
    #[test]
    fn a_cache_name_is_stable_readable_and_unique() {
        let a = cache_name("https://github.com/acme/widgets.git");
        let b = cache_name("https://github.com/other/widgets.git");
        assert_eq!(a, cache_name("https://github.com/acme/widgets.git"), "stable");
        assert_ne!(a, b, "two repositories that slug the same must not collide");
        assert!(a.starts_with("widgets-"), "{a} should be readable");
        assert!(paths::is_safe_segment(&a), "{a} must be a usable directory name");
        // A hostile URL still yields a safe single component.
        let hostile = cache_name("https://x/../../etc/passwd");
        assert!(paths::is_safe_segment(&hostile), "{hostile}");
    }

    #[test]
    fn a_branch_name_becomes_a_remote_ref_but_a_real_ref_is_left_alone() {
        // A cache checkout has no local branches, so `main` has to be read as
        // `origin/main` — while anything already resolvable must not be prefixed.
        assert_eq!(remote_ref("main"), "origin/main");
        assert_eq!(remote_ref("feature/x"), "origin/feature/x");
        assert_eq!(remote_ref("origin/main"), "origin/main");
        assert_eq!(remote_ref("refs/heads/main"), "refs/heads/main");
        assert_eq!(remote_ref("HEAD"), "HEAD");
        assert_eq!(remote_ref("a1b2c3d4e5f6"), "a1b2c3d4e5f6", "a sha resolves as itself");
        // A short hex-looking BRANCH name is still a branch: the length floor is
        // what keeps `abc` from being mistaken for a commit.
        assert_eq!(remote_ref("abc"), "origin/abc");
    }

    #[test]
    fn a_merge_failure_serializes_in_the_shape_the_client_parses() {
        // `parseProjectPullRequestMergeError` reads exactly these names, and
        // treats anything else as a generic failure — so the recovery dialog
        // depends on this serialization.
        let failure = MergeFailure::conflict("main", "feature/x", "2 files conflict.");
        let payload: serde_json::Value =
            serde_json::from_str(&failure.to_payload()).expect("json");
        assert_eq!(payload["code"], "merge_conflict");
        assert_eq!(payload["recovery"]["action"], "open_terminal");
        assert_eq!(payload["recovery"]["targetBranch"], "main");
        assert_eq!(payload["recovery"]["sourceBranch"], "feature/x");

        let stale = MergeFailure::stale("abc123", "def456");
        let payload: serde_json::Value = serde_json::from_str(&stale.to_payload()).expect("json");
        assert_eq!(payload["code"], "stale_source");
        assert!(payload["recovery"].is_null(), "a stale tip has no terminal recovery");
    }

    #[test]
    fn the_recovery_terminal_result_keeps_its_camel_case_keys() {
        // This one shape is passed through UNMAPPED by the client, so its keys
        // are the client's names rather than this crate's convention.
        let value = serde_json::to_value(RecoveryTerminalResult {
            path: "/tmp/repo".into(),
            cloned: false,
            recovery_ref: "refs/heads/recover".into(),
            target_ref: "refs/heads/main".into(),
        })
        .expect("json");
        assert_eq!(value["recoveryRef"], "refs/heads/recover");
        assert_eq!(value["targetRef"], "refs/heads/main");
    }

    #[test]
    fn the_wire_shapes_use_the_names_the_client_reads() {
        let status = serde_json::to_value(SyncStatus {
            local_path: None,
            local_branch: None,
            local_branches: vec![],
            local_head: None,
            local_short_head: None,
            remote_branch: None,
            remote_head: None,
            remote_short_head: None,
            merge_base: None,
            ahead_count: 0,
            behind_count: 0,
            has_uncommitted_changes: false,
            has_untracked_files: false,
            can_push: false,
            push_block_reason: None,
            can_pull: false,
            pull_block_reason: None,
        })
        .expect("json");
        for key in [
            "local_path",
            "local_branch",
            "local_branches",
            "local_head",
            "local_short_head",
            "remote_branch",
            "remote_head",
            "remote_short_head",
            "merge_base",
            "ahead_count",
            "behind_count",
            "has_uncommitted_changes",
            "has_untracked_files",
            "can_push",
            "push_block_reason",
            "can_pull",
            "pull_block_reason",
        ] {
            assert!(status.get(key).is_some(), "sync status is missing {key}");
        }

        let diff = serde_json::to_value(RepoDiff {
            files: vec![DiffFile {
                path: "a.rs".into(),
                additions: 1,
                deletions: 2,
                patch: "@@".into(),
                truncated: true,
            }],
            additions: 1,
            deletions: 2,
            commit_body: None,
        })
        .expect("json");
        assert!(diff.get("commit_body").is_some());
        assert_eq!(diff["files"][0]["truncated"], true);
    }

    #[test]
    fn ls_tree_rows_parse_and_non_blobs_are_skipped() {
        let blob = parse_ls_tree_line("100644 blob abc123    1234\tsrc/lib.rs").expect("blob");
        assert_eq!(blob.path, "src/lib.rs");
        assert_eq!(blob.size, Some(1234));
        // A submodule or tree is not a file to preview.
        assert!(parse_ls_tree_line("160000 commit abc123       -\tvendor/dep").is_none());
        assert!(parse_ls_tree_line("garbage").is_none());
        // A quoted non-ASCII path is unquoted, like everywhere else.
        let quoted =
            parse_ls_tree_line("100644 blob abc123    12\t\"caf\\303\\251.txt\"").expect("blob");
        assert_eq!(quoted.path, "caf\u{e9}.txt");
    }

    #[test]
    fn contributors_are_aggregated_from_the_commits_shown() {
        let commits = vec![
            Commit {
                hash: "a".into(),
                short_hash: "a".into(),
                author_name: "Ana".into(),
                author_email: "ana@example.com".into(),
                timestamp: 30,
                subject: "x".into(),
            },
            Commit {
                hash: "b".into(),
                short_hash: "b".into(),
                author_name: "Ana".into(),
                // Same person, different case — one contributor, not two.
                author_email: "ANA@example.com".into(),
                timestamp: 10,
                subject: "y".into(),
            },
            Commit {
                hash: "c".into(),
                short_hash: "c".into(),
                author_name: "Bo".into(),
                author_email: "bo@example.com".into(),
                timestamp: 20,
                subject: "z".into(),
            },
        ];
        let contributors = contributors_from(&commits);
        assert_eq!(contributors.len(), 2);
        assert_eq!(contributors[0].email, "ana@example.com");
        assert_eq!(contributors[0].commit_count, 2);
        assert_eq!(contributors[0].last_commit_at, 30, "the NEWEST commit wins");
        assert_eq!(contributors[1].name, "Bo");
    }

    #[test]
    fn terminal_candidates_never_hand_an_untrusted_path_to_a_parser() {
        // The finding: everything after `cmd /C` is command TEXT that cmd.exe
        // re-parses, so argv quoting does not make it inert. The answer is to
        // not pass the path at all — the spawner sets the working directory.
        let hostile = "C:\\repo & calc | %USERPROFILE% ; echo";
        for candidate in terminal_candidates("windows", hostile) {
            assert!(!candidate.path_as_arg);
            assert!(
                candidate.args.iter().all(|a| !a.contains("repo")),
                "{}: the path is never passed to a Windows program",
                candidate.program
            );
        }
        for candidate in terminal_candidates("linux", "/tmp/a & rm -rf ~") {
            assert!(!candidate.path_as_arg);
            assert!(candidate.args.is_empty(), "{} takes no path", candidate.program);
        }
        // macOS is the exception, and a safe one: `open` is execve'd with an
        // argv and Terminal.app ignores an inherited working directory.
        let mac = terminal_candidates("macos", "/tmp/a & b");
        assert_eq!(mac.len(), 1);
        assert!(mac[0].path_as_arg);
        assert_eq!(mac[0].args.last().map(String::as_str), Some("/tmp/a & b"));
    }

    #[test]
    fn windows_terminals_open_in_the_working_directory_with_a_window() {
        // wt.exe opens its first tab in its own default directory unless told
        // otherwise; `-d .` resolves against the working directory the spawner
        // sets, so the untrusted path still never reaches wt's `;` parser.
        // cmd.exe launched from a GUI process has no console unless one is
        // created for it.
        let windows = terminal_candidates("windows", "C:\\repo");
        assert_eq!(windows[0].program, "wt.exe");
        assert_eq!(windows[0].args, vec!["-d".to_string(), ".".to_string()]);
        assert!(!windows[0].new_console);
        assert_eq!(windows[1].program, "cmd.exe");
        assert!(windows[1].args.is_empty());
        assert!(windows[1].new_console);
        for os in ["linux", "macos"] {
            assert!(terminal_candidates(os, "/tmp/r").iter().all(|c| !c.new_console));
        }
    }

    #[test]
    #[cfg(unix)]
    fn a_launcher_that_exits_with_failure_falls_through_to_the_next() {
        let candidate = |program: &str| TerminalCommand {
            program: program.to_string(),
            args: Vec::new(),
            path_as_arg: false,
            new_console: false,
        };
        let dir = std::env::temp_dir();
        assert_eq!(
            launch_first(&[candidate("false"), candidate("true")], &dir).expect("true works"),
            "true",
            "a launcher that starts and then fails is not a terminal"
        );
        assert!(launch_first(&[candidate("false")], &dir).is_err());
        assert_eq!(
            launch_first(&[candidate("sleep")], &dir).ok(),
            None,
            "sleep without an operand exits with failure"
        );
    }

    #[test]
    fn url_refusals_never_echo_a_credential() {
        for bad in [
            "https://user:tok3n-secret@github.com/acme/widgets.git x",
            "ftp://user:tok3n-secret@github.com/acme/widgets.git",
            "https://user:tok3n-secret@/acme/widgets.git",
        ] {
            let err = validate_clone_url(bad).expect_err(bad);
            let shown = format!("{err} {err:?}");
            assert!(!shown.contains("tok3n-secret"), "{shown}");
            assert!(shown.contains("<redacted>"), "{shown}");
        }
    }

    #[test]
    fn linux_candidates_are_tried_in_preference_order() {
        let linux = terminal_candidates("linux", "/tmp/repo");
        let programs: Vec<&str> = linux.iter().map(|c| c.program.as_str()).collect();
        assert_eq!(
            programs,
            vec!["x-terminal-emulator", "gnome-terminal", "konsole", "xterm"]
        );
    }

    /// A REAL repository with one commit and an `origin` that is never contacted.
    ///
    /// The gate tests need a repository git accepts, or they prove only that a
    /// fake directory fails — which is not the property under test.
    fn scratch_repo(label: &str) -> (PathBuf, PathBuf) {
        let base = std::env::temp_dir().join(format!("buzz-{label}-{}", std::process::id()));
        let repos = base.join("repos");
        let repo = repos.join("app");
        std::fs::create_dir_all(&repo).expect("mkdir");
        let git = Git::new();
        let run = |args: &[&str]| {
            git.run(Some(&repo), args)
                .unwrap_or_else(|e| panic!("git {args:?}: {e}"));
        };
        run(&["init", "--quiet"]);
        run(&["config", "user.email", "test@example.com"]);
        run(&["config", "user.name", "Test"]);
        run(&["config", "commit.gpgsign", "false"]);
        std::fs::write(repo.join("README.md"), "hello\n").expect("write");
        run(&["add", "README.md"]);
        run(&["commit", "--quiet", "-m", "first"]);
        run(&["branch", "-M", "main"]);
        // A remote that exists in config and is never reached.
        run(&["remote", "add", "origin", "https://git.example/acct/app.git"]);
        (base, repos)
    }

    #[test]
    fn the_policy_refuses_a_real_repository_before_git_runs() {
        // The property: authority is enforced on the MUTATING path, from the
        // repository's real state — not merely reflected in a status the UI may
        // have read minutes ago.
        let (base, repos) = scratch_repo("policy");
        let workspace = Workspace::new(Git::new(), &repos).expect("workspace");

        let rcs = SyncInputs {
            project_dtag: "app",
            clone_url: "https://git.example/acct/app.git",
            branch: Some("main"),
            trunk: "main",
            credential: false,
        };
        match workspace.push(&rcs) {
            Err(Error::Refused(reason)) => {
                assert!(reason.contains("account owner token"), "{reason}");
            }
            other => panic!("a push without the account credential must be REFUSED: {other:?}"),
        }
        // A pull is refused for the same reason, and before any network call.
        match workspace.pull(&rcs) {
            Err(Error::Refused(reason)) => {
                assert!(reason.contains("account owner token"), "{reason}");
            }
            other => panic!("a fetch without the account credential must be REFUSED: {other:?}"),
        }

        // With the credential but on a non-trunk branch, the trunk-only transport is the reason.
        let git = Git::new();
        git.run(Some(&repos.join("app")), &["checkout", "--quiet", "-b", "feature/x"])
            .expect("branch");
        let branch = SyncInputs {
            branch: Some("feature/x"),
            credential: true,
            ..rcs.clone()
        };
        match workspace.push(&branch) {
            Err(Error::Refused(reason)) => assert!(reason.contains("serves only"), "{reason}"),
            other => panic!("a branch push over the RCS transport must be REFUSED: {other:?}"),
        }

        // And the status read agrees with the mutating path.
        let status = workspace.sync_status(&rcs).expect("status");
        assert!(!status.can_push);
        assert!(status.push_block_reason.is_some());
        assert_eq!(status.local_branch.as_deref(), Some("main"));
        assert!(status.local_branches.iter().any(|b| b == "main"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_clone_url_that_is_not_the_checkouts_origin_is_refused() {
        // The gap: the gate's remote kind and credential were judged from the
        // caller's URL while git pushed to the checkout's own origin, so a
        // GitHub URL lifted the RCS rules from an RCS checkout.
        let (base, repos) = scratch_repo("origin-bound");
        let workspace = Workspace::new(Git::new(), &repos).expect("workspace");
        let posing = SyncInputs {
            project_dtag: "app",
            clone_url: "ssh://github.com/x/y.git",
            branch: Some("main"),
            trunk: "main",
            credential: true,
        };
        for result in [
            workspace.push(&posing).map(|_| ()),
            workspace.pull(&posing).map(|_| ()),
            workspace.sync_status(&posing).map(|_| ()),
        ] {
            match result {
                Err(Error::Refused(reason)) => assert!(reason.contains("origin"), "{reason}"),
                other => panic!("a URL that is not the origin must be REFUSED: {other:?}"),
            }
        }
        // A URL that would inject credential-helper lines is refused outright.
        let injected = SyncInputs {
            clone_url: "https://none.invalid/x\nhost=github.com",
            ..posing.clone()
        };
        assert!(matches!(workspace.sync_status(&injected), Err(Error::Invalid(_))));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_snapshot_reads_a_real_repository() {
        let (base, repos) = scratch_repo("snapshot");
        let workspace = Workspace::new(Git::new(), &repos).expect("workspace");
        let snapshot = workspace
            .local_snapshot("app", None)
            .expect("snapshot")
            .expect("cloned");
        assert_eq!(snapshot.snapshot.commits.len(), 1);
        assert_eq!(snapshot.snapshot.commits[0].subject, "first");
        assert_eq!(
            snapshot.snapshot.files.iter().map(|f| f.path.as_str()).collect::<Vec<_>>(),
            vec!["README.md"]
        );
        // The first files get detail, so this one carries its history and text.
        let readme = &snapshot.snapshot.files[0];
        assert_eq!(readme.preview_content.as_deref(), Some("hello\n"));
        assert!(readme.latest_commit.is_some());
        assert_eq!(readme.size, Some(6));
        assert_eq!(snapshot.snapshot.contributors.len(), 1);
        assert_eq!(snapshot.snapshot.contributors[0].commit_count, 1);

        // An uncloned project is None rather than an error.
        assert!(workspace.local_snapshot("absent", None).expect("ok").is_none());
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_diff_between_two_commits_reads_real_patches() {
        let (base, repos) = scratch_repo("diff");
        let repo = repos.join("app");
        let git = Git::new();
        std::fs::write(repo.join("README.md"), "hello\nworld\n").expect("write");
        git.run(Some(&repo), &["add", "README.md"]).expect("add");
        git.run(Some(&repo), &["commit", "--quiet", "-m", "second"])
            .expect("commit");
        let workspace = Workspace::new(Git::new(), &repos).expect("workspace");
        let diff = workspace
            .diff_between(&repo, "HEAD~1", "HEAD")
            .expect("diff");
        assert_eq!(diff.files.len(), 1);
        assert_eq!(diff.files[0].path, "README.md");
        assert_eq!(diff.additions, 1);
        assert_eq!(diff.deletions, 0);
        assert!(diff.files[0].patch.contains("+world"));
        assert!(!diff.files[0].truncated);
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn an_rcs_pull_is_refused_for_the_same_reasons_as_a_push() {
        // The gap: authority rules were applied to pushes only, so an RCS branch
        // nobody could fetch was reported as pullable.
        let no_credential = SyncFacts {
            remote: RemoteKind::Rcs,
            credential: false,
            branch: "main".into(),
            behind: 3,
            ahead: 0,
            ..facts()
        };
        let gate = sync_gate(&no_credential);
        assert!(!gate.can_pull, "a fetch needs the same credential a push does");
        assert!(gate
            .pull_block_reason
            .expect("reason")
            .contains("account owner token"));

        let branch_gap = SyncFacts {
            remote: RemoteKind::Rcs,
            credential: true,
            branch: "feature/x".into(),
            behind: 2,
            ahead: 0,
            ..facts()
        };
        let gate = sync_gate(&branch_gap);
        assert!(!gate.can_pull);
        assert!(gate.pull_block_reason.expect("reason").contains("serves only"));

        // ...and the trunk with a credential pulls, as it should.
        let allowed = SyncFacts {
            branch: "main".into(),
            ..branch_gap
        };
        assert!(sync_gate(&allowed).can_pull, "{:?}", sync_gate(&allowed).pull_block_reason);
    }

    #[test]
    fn a_non_rcs_pull_still_needs_a_credential() {
        let gate = sync_gate(&SyncFacts {
            credential: false,
            behind: 1,
            ..facts()
        });
        assert!(!gate.can_pull);
        assert!(gate.pull_block_reason.expect("reason").contains("No credential"));
    }

    #[test]
    fn a_workspace_refuses_an_unusable_repositories_directory() {
        assert!(Workspace::new(Git::new(), Path::new("relative")).is_err());
        assert!(Workspace::new(Git::new(), Path::new("/")).is_err());
        let ok = Workspace::new(Git::new(), Path::new("/tmp/buzz-desktop-repos")).expect("ok");
        assert_eq!(ok.repos_dir(), Path::new("/tmp/buzz-desktop-repos"));
    }

    #[test]
    fn hostile_clone_urls_and_git_refs_are_refused_before_git_runs() {
        for ok_url in [
            "https://github.com/acme/widgets.git",
            "http://127.0.0.1:8080/acme/widgets.git",
            "ssh://git@github.com/acme/widgets.git",
            "git@github.com:acme/widgets.git",
            "github.com:acme/widgets.git",
        ] {
            assert!(validate_clone_url(ok_url).is_ok(), "{ok_url}");
        }
        for bad_url in [
            "--upload-pack=touch /tmp/pwned",
            "-oProxyCommand=calc",
            "ext::sh -c calc% ",
            "file:///etc/passwd",
            "/tmp/local-repo",
            "c:\\repos\\widgets",
            "C:/repos/widgets",
            "https://github.com/acme/widgets.git\n--upload-pack=calc",
            " https://github.com/acme/widgets.git",
            "https://github.com/acme/widgets.git\n",
            "https://github.com/acme/widgets.git\t",
            "",
        ] {
            assert!(validate_clone_url(bad_url).is_err(), "{bad_url:?}");
        }

        for ok_ref in ["main", "feature/x", "refs/heads/main", "origin/main", "a1b2c3d4"] {
            assert!(validate_git_ref(ok_ref).is_ok(), "{ok_ref}");
        }
        for bad_ref in [
            "--upload-pack=calc",
            "--output=/tmp/overwrite",
            "-f",
            "main..evil",
            "refs/heads/x.lock",
            "feature:evil",
            "feature\\evil",
            "feature@{0}",
            " main",
            "main\n",
            "main\t",
            "",
        ] {
            assert!(validate_git_ref(bad_ref).is_err(), "{bad_ref:?}");
        }
    }
}
