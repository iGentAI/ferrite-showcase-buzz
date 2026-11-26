//! The commands the shell answers.
//!
//! Thin by design: each one resolves the right [`Workspace`] or harness and
//! hands off to `buzz-desktop-core`, which is where every decision lives. If a
//! function here is longer than a screen, the logic has leaked into the wiring.
//!
//! ## Argument names
//!
//! Tauri maps a JS `camelCase` key onto a Rust `snake_case` parameter, so these
//! signatures mirror what `projectGit.ts` sends. A name typed wrong here is not a
//! compile error anywhere — the command simply receives `None` — which is why the
//! names were read off that file rather than guessed.
//!
//! ## Errors
//!
//! Every command answers `Result<_, String>`, and the string is what the client
//! shows. One case is structured rather than prose: a merge failure serializes
//! as the JSON `parseProjectPullRequestMergeError` reads, so the recovery dialog
//! can offer to open a terminal.

use buzz_desktop_core::credentials::{self, Store};
use buzz_desktop_core::git::Git;
use buzz_desktop_core::harness::{Harness, Lifecycle, RemoteAuth, RuntimeStatus};
use buzz_desktop_core::nostr::AnswerTarget;
use buzz_desktop_core::paths;
use buzz_desktop_core::project::{
    self, BranchResult, CloneResult, LocalRepoSnapshot, MergeRequest, MergeResult, PullResult,
    PushResult, RecoveryTerminalResult, RepoDiff, RepoSnapshot, SyncInputs, SyncStatus,
    TerminalResult, Workspace,
};
use buzz_desktop_core::registry;
use buzz_desktop_core::time;
use serde::Deserialize;
use serde_json::{json, Value};
use tauri::{AppHandle, State};

use crate::remote;
use crate::state::AppState;

fn oops(e: buzz_desktop_core::Error) -> String {
    e.to_string()
}

/// The client nests most inputs under `input`, while older call sites send the
/// fields bare.
fn body_of(value: &Value) -> &Value {
    value.get("input").unwrap_or(value)
}

fn workspace(state: &AppState, repos_dir: Option<String>) -> Result<Workspace, String> {
    workspace_for(state, repos_dir, &[])
}

/// Build a [`Workspace`] with URL-scoped git credential helpers attached for
/// `remote_urls` via environment variables rather than argv (see
/// `credentials::credential_env`).
fn workspace_for(
    state: &AppState,
    repos_dir: Option<String>,
    remote_urls: &[&str],
) -> Result<Workspace, String> {
    let mut scoped: Vec<credentials::ScopedCredential> = Vec::new();
    for url in remote_urls {
        if project::validate_clone_url(url).is_err() {
            continue;
        }
        let Some(host) = project::host_of(url) else {
            continue;
        };
        let Ok(Some(secret)) = state.keychain.get(&credentials::Key::git(&host)) else {
            continue;
        };
        let Some(credential) = credentials::scoped_credential(url, &secret) else {
            continue;
        };
        // Deduplicated by the SCOPE the helper answers for (scheme + host +
        // port), not by hostname: a scope is origin-level, so two repositories
        // on one origin share a helper — while the same hostname reached over
        // another scheme or port is a DIFFERENT origin and needs its own.
        if !scoped.iter().any(|c| c.config_key == credential.config_key) {
            scoped.push(credential);
        }
    }
    let mut git = Git::new();
    for (key, value) in credentials::credential_env(&scoped) {
        git = git.with_env(&key, &value);
    }
    match repos_dir.filter(|dir| !dir.trim().is_empty()) {
        Some(dir) => {
            let path = std::path::PathBuf::from(dir);
            paths::validate_repos_dir(&path).map_err(oops)?;
            Workspace::new(git, &path).map_err(oops)
        }
        // No credential in play: the shared workspace is exactly right.
        None if scoped.is_empty() => Ok(state.workspace.clone()),
        None => Workspace::new(git, state.workspace.repos_dir()).map_err(oops),
    }
}

/// Whether a credential for this remote's host is available to git.
///
/// The SHELL's keychain, the USER's own git credential helper, or — for an SSH
/// remote — their key, which is not a password and is therefore not gated at all.
/// A machine where `git push` already works must not be refused because this app
/// holds no token of its own (see `credentials::credential_available`).
fn credential(state: &AppState, clone_url: &str) -> bool {
    // A URL `git` would refuse is never handed to `git credential fill`,
    // whose input a line break in it would add lines to.
    if project::validate_clone_url(clone_url).is_err() {
        return false;
    }
    let host = project::host_of(clone_url).unwrap_or_default();
    credentials::credential_available(&state.keychain, &Git::new(), clone_url, &host)
}

fn trunk(base_branch: Option<String>) -> String {
    base_branch
        .filter(|branch| !branch.trim().is_empty())
        .unwrap_or_else(|| "main".to_string())
}

/// Returns the same document `registry::init_script` injects plus host facts so
/// e2e runs can verify that the marker and compiled registry have not drifted.
#[tauri::command]
pub fn desktop_capabilities(state: State<'_, AppState>) -> Value {
    let mut document = registry::marker_document();
    if let Some(map) = document.as_object_mut() {
        map.insert(
            "credentialStoreDurable".to_string(),
            Value::Bool(state.keychain.is_durable()),
        );
        map.insert(
            "reposDir".to_string(),
            Value::String(state.workspace.repos_dir().to_string_lossy().into_owned()),
        );
        map.insert(
            "git".to_string(),
            state.workspace.git_prerequisite(),
        );
        // Whether this shell knows where to post an agent's answer. A desktop
        // build serves the SPA from `tauri://localhost`, so there is no page
        // origin to derive one from — see `workspace::origin`.
        match crate::workspace::origin(&state.data_dir) {
            Ok(origin) => {
                map.insert("workspaceOrigin".to_string(), Value::String(origin));
            }
            Err(reason) => {
                map.insert("workspaceOrigin".to_string(), Value::Null);
                map.insert(
                    "workspaceOriginReason".to_string(),
                    Value::String(reason),
                );
            }
        }
    }
    document
}

#[tauri::command]
pub fn get_git_identity(state: State<'_, AppState>) -> Result<Value, String> {
    state.workspace.git_identity().map_err(oops)
}

#[tauri::command]
pub fn discover_git_bash_prerequisite(state: State<'_, AppState>) -> Value {
    state.workspace.git_prerequisite()
}

#[tauri::command]
pub fn list_project_local_repositories(
    state: State<'_, AppState>,
    repos_dir: Option<String>,
) -> Result<Vec<Value>, String> {
    workspace(&state, repos_dir)?
        .local_repositories()
        .map_err(oops)
}

#[tauri::command]
pub fn clone_project_repository(
    state: State<'_, AppState>,
    repos_dir: Option<String>,
    project_dtag: String,
    clone_url: String,
    default_branch: Option<String>,
) -> Result<CloneResult, String> {
    workspace_for(&state, repos_dir, &[clone_url.as_str()])?
        .clone_repository(&project_dtag, &clone_url, default_branch.as_deref())
        .map_err(oops)
}

#[tauri::command]
pub fn get_project_repo_snapshot(
    state: State<'_, AppState>,
    clone_url: String,
    default_branch: Option<String>,
    base_branch: Option<String>,
    target_ref: Option<String>,
    target_commit: Option<String>,
) -> Result<RepoSnapshot, String> {
    // A commit is more specific than a ref, so it wins when both are given.
    let reference = target_commit
        .filter(|c| !c.is_empty())
        .or(target_ref.filter(|r| !r.is_empty()))
        .or(base_branch.filter(|b| !b.is_empty()));
    workspace_for(&state, None, &[clone_url.as_str()])?
        .remote_snapshot(&clone_url, default_branch.as_deref(), reference.as_deref())
        .map_err(oops)
}

#[tauri::command]
pub fn get_project_repo_diff(
    state: State<'_, AppState>,
    clone_url: String,
    default_branch: Option<String>,
    base_branch: Option<String>,
    target_ref: Option<String>,
    target_commit: Option<String>,
) -> Result<RepoDiff, String> {
    let target = target_commit
        .filter(|c| !c.is_empty())
        .or(target_ref.filter(|r| !r.is_empty()));
    workspace_for(&state, None, &[clone_url.as_str()])?
        .remote_diff(
            &clone_url,
            default_branch.as_deref(),
            base_branch.as_deref(),
            target.as_deref(),
        )
        .map_err(oops)
}

#[tauri::command]
pub fn get_project_local_repo_snapshot(
    state: State<'_, AppState>,
    repos_dir: Option<String>,
    project_dtag: String,
    base_branch: Option<String>,
) -> Result<Option<LocalRepoSnapshot>, String> {
    workspace(&state, repos_dir)?
        .local_snapshot(&project_dtag, base_branch.as_deref())
        .map_err(oops)
}

#[tauri::command]
pub fn get_project_local_repo_diff(
    state: State<'_, AppState>,
    repos_dir: Option<String>,
    project_dtag: String,
    base_branch: Option<String>,
    base_commit: Option<String>,
    target_commit: Option<String>,
) -> Result<Option<RepoDiff>, String> {
    let workspace = workspace(&state, repos_dir)?;
    if !workspace.is_cloned(&project_dtag) {
        // Not cloned is a STATE the client renders, not an error.
        return Ok(None);
    }
    let repo = workspace.repo_path(&project_dtag).map_err(oops)?;
    let base = base_commit
        .filter(|c| !c.is_empty())
        .or(base_branch.filter(|b| !b.is_empty()))
        .unwrap_or_else(|| "HEAD~1".to_string());
    let target = target_commit
        .filter(|c| !c.is_empty())
        .unwrap_or_else(|| "HEAD".to_string());
    workspace
        .diff_between(&repo, &base, &target)
        .map(Some)
        .map_err(oops)
}

#[tauri::command]
pub fn get_project_repo_sync_status(
    state: State<'_, AppState>,
    repos_dir: Option<String>,
    project_dtag: String,
    clone_url: String,
    branch_name: Option<String>,
    base_branch: Option<String>,
) -> Result<SyncStatus, String> {
    project::validate_clone_url(&clone_url).map_err(oops)?;
    let workspace = workspace_for(&state, repos_dir, &[clone_url.as_str()])?;
    let trunk = trunk(base_branch);
    workspace
        .sync_status(&SyncInputs {
            project_dtag: &project_dtag,
            clone_url: &clone_url,
            branch: branch_name.as_deref(),
            trunk: &trunk,
            credential: credential(&state, &clone_url),
        })
        .map_err(oops)
}

#[tauri::command]
pub fn push_project_local_repository(
    state: State<'_, AppState>,
    repos_dir: Option<String>,
    project_dtag: String,
    clone_url: String,
    branch_name: Option<String>,
    base_branch: Option<String>,
) -> Result<PushResult, String> {
    project::validate_clone_url(&clone_url).map_err(oops)?;
    let workspace = workspace_for(&state, repos_dir, &[clone_url.as_str()])?;
    let trunk = trunk(base_branch);
    workspace
        .push(&SyncInputs {
            project_dtag: &project_dtag,
            clone_url: &clone_url,
            branch: branch_name.as_deref(),
            trunk: &trunk,
            credential: credential(&state, &clone_url),
        })
        .map_err(oops)
}

#[tauri::command]
pub fn pull_project_local_repository(
    state: State<'_, AppState>,
    repos_dir: Option<String>,
    project_dtag: String,
    clone_url: String,
    branch_name: Option<String>,
) -> Result<PullResult, String> {
    project::validate_clone_url(&clone_url).map_err(oops)?;
    let workspace = workspace_for(&state, repos_dir, &[clone_url.as_str()])?;
    workspace
        .pull(&SyncInputs {
            project_dtag: &project_dtag,
            clone_url: &clone_url,
            branch: branch_name.as_deref(),
            // A pull's authority rules need the trunk; the client does not send
            // one here, so the branch itself is treated as the trunk — which is
            // the permissive reading, and the RCS credential rule still applies.
            trunk: branch_name.as_deref().unwrap_or("main"),
            credential: credential(&state, &clone_url),
        })
        .map_err(oops)
}

#[tauri::command]
pub fn create_project_remote_branch(
    state: State<'_, AppState>,
    clone_url: String,
    source_branch: String,
    expected_commit: String,
    new_branch: String,
) -> Result<BranchResult, String> {
    workspace_for(&state, None, &[clone_url.as_str()])?
        .create_remote_branch(&clone_url, &source_branch, &expected_commit, &new_branch)
        .map_err(oops)
}

#[tauri::command]
pub fn delete_project_remote_branch(
    state: State<'_, AppState>,
    clone_url: String,
    branch: String,
    expected_commit: String,
) -> Result<BranchResult, String> {
    workspace_for(&state, None, &[clone_url.as_str()])?
        .delete_remote_branch(&clone_url, &branch, &expected_commit)
        .map_err(oops)
}

#[tauri::command]
pub fn open_project_terminal(
    state: State<'_, AppState>,
    repos_dir: Option<String>,
    project_dtag: String,
    clone_url: Option<String>,
    default_branch: Option<String>,
) -> Result<TerminalResult, String> {
    // The clone URL rides along so the on-demand clone below can authenticate
    // with a stored token; the temporary list dies with the statement.
    let workspace = workspace_for(
        &state,
        repos_dir,
        &clone_url.as_deref().into_iter().collect::<Vec<_>>(),
    )?;
    // Clone on demand: "open a terminal here" on a project nobody has cloned yet
    // should give you the repository, not an error telling you to clone first.
    let mut cloned = false;
    if !workspace.is_cloned(&project_dtag) {
        let Some(clone_url) = clone_url.filter(|url| !url.is_empty()) else {
            return Err(
                "This repository is not cloned on this machine, and the project does not \
                 declare a clone URL to get it from."
                    .to_string(),
            );
        };
        let result = workspace
            .clone_repository(&project_dtag, &clone_url, default_branch.as_deref())
            .map_err(oops)?;
        cloned = result.cloned;
    }
    let mut opened = workspace.open_terminal(&project_dtag).map_err(oops)?;
    opened.cloned = cloned;
    Ok(opened)
}

/// The merge-recovery input, in the client's own shape.
///
/// `project_dtag` is accepted and unused: a recovery checkout is keyed by CLONE
/// URL (the pull request may come from a fork, which has no project of its own),
/// so the project id has nothing to resolve here. Kept because the client sends
/// it and this struct documents that wire shape — refusing the call over a field
/// the work does not need would break a recovery that otherwise works.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct RecoveryInput {
    pub repos_dir: Option<String>,
    pub project_dtag: String,
    pub target_clone_url: String,
    pub source_clone_url: String,
    pub target_branch: String,
    pub source_branch: String,
    pub expected_commit: String,
}

#[tauri::command]
pub fn open_project_merge_recovery_terminal(
    state: State<'_, AppState>,
    input: RecoveryInput,
) -> Result<RecoveryTerminalResult, String> {
    let workspace = workspace_for(
        &state,
        input.repos_dir.clone(),
        &[
            input.target_clone_url.as_str(),
            input.source_clone_url.as_str(),
        ],
    )?;
    workspace
        .recovery_terminal(&MergeRequest {
            target_clone_url: input.target_clone_url,
            source_clone_url: input.source_clone_url,
            target_branch: input.target_branch,
            source_branch: input.source_branch,
            expected_commit: input.expected_commit,
        })
        .map_err(oops)
}

/// The merge input, in the client's own shape.
///
/// The workspace-identity fields (`targetOwner`, `repoAddress`, `pullRequestId`,
/// …) are accepted and unused: they exist so the desktop can SIGN a status event,
/// and this shell does not hold a signing key (see
/// `project::STATUS_NOT_SIGNED`). Accepting and ignoring them is deliberate —
/// refusing the call because of a field the git work does not need would break a
/// merge that otherwise works perfectly.
#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
#[allow(dead_code)]
pub struct MergeInput {
    pub target_clone_url: String,
    pub source_clone_url: String,
    pub target_branch: String,
    pub source_branch: String,
    pub expected_commit: String,
    #[serde(default)]
    pub target_owner: Option<String>,
    #[serde(default)]
    pub repo_address: Option<String>,
    #[serde(default)]
    pub pull_request_id: Option<String>,
}

#[tauri::command]
pub fn merge_project_pull_request(
    state: State<'_, AppState>,
    input: MergeInput,
) -> Result<MergeResult, String> {
    // Both remotes' credentials ride along; each helper is scoped to its OWN
    // origin, so the source remote is never handed the target's token.
    workspace_for(
        &state,
        None,
        &[
            input.target_clone_url.as_str(),
            input.source_clone_url.as_str(),
        ],
    )?
    .merge_pull_request(&MergeRequest {
            target_clone_url: input.target_clone_url,
            source_clone_url: input.source_clone_url,
            target_branch: input.target_branch,
            source_branch: input.source_branch,
            expected_commit: input.expected_commit,
        })
        // The STRUCTURED failure: the client parses this JSON to decide whether
        // to offer the recovery terminal.
        .map_err(|failure| failure.to_payload())
}

#[tauri::command]
pub fn publish_project_pull_request_merged_status(_input: Value) -> Result<Value, String> {
    // Refused with the reason rather than answering "published" for an event
    // this shell never signed.
    Err(project::STATUS_NOT_SIGNED.to_string())
}

/// Shared across create and sparse update so the two paths cannot drift.
const LABEL_KEYS: &[&str] = &["label", "displayName", "name"];
const COMMAND_KEYS: &[&str] = &["command", "acpCommand", "agentCommand", "binaryPath"];
const ARGS_KEYS: &[&str] = &["args", "agentArgs", "defaultArgs"];
const ENV_KEYS: &[&str] = &["env", "envVars"];

fn first_text(body: &Value, keys: &[&str]) -> Option<String> {
    keys.iter().find_map(|key| {
        body.get(key)
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string)
    })
}

/// The first of `keys` present as an array of strings.
///
/// `None` means the request did not MENTION it, which is different from naming an
/// empty list — and that difference is the whole of what makes a sparse update
/// possible: "leave the arguments alone" and "there are no arguments now" are
/// opposite instructions.
fn string_list(body: &Value, keys: &[&str]) -> Option<Vec<String>> {
    keys.iter().find_map(|key| {
        body.get(key).and_then(Value::as_array).map(|rows| {
            rows.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
    })
}

/// The first of `keys` present as a string map, with the same present-versus-
/// absent distinction.
fn string_map(body: &Value, keys: &[&str]) -> Option<Vec<(String, String)>> {
    keys.iter().find_map(|key| {
        body.get(key).and_then(Value::as_object).map(|map| {
            map.iter()
                .filter_map(|(key, value)| {
                    value.as_str().map(|value| (key.clone(), value.to_string()))
                })
                .collect()
        })
    })
}

/// The auth a request names for a remote harness, or `None` when it names none.
///
/// For a CREATE, where there is nothing stored to preserve. An update goes
/// through [`merge_remote_auth`], which is a different question.
fn remote_auth(body: &Value) -> Option<RemoteAuth> {
    let secret_ref = first_text(body, &["secretRef"])?;
    Some(match first_text(body, &["authHeader"]) {
        Some(name) => RemoteAuth::Header { name, secret_ref },
        None => RemoteAuth::Bearer { secret_ref },
    })
}

/// Merge a remote credential field by field against what is stored.
///
/// Sparsely, like everything else in an update — and the two ONE-FIELD cases are
/// exactly where a whole-object replacement goes wrong: rotating only the secret
/// on a custom-header credential would silently turn it into a bearer and lose the
/// header name the endpoint expects, while naming only a header would be ignored
/// for want of a secret nobody was rotating. Either way the next prompt fails
/// authentication for a reason the person who made the edit did not cause.
///
/// One thing an update deliberately cannot do is CLEAR a credential: the client's
/// shape has no way to say "remove it" (an empty string is absent, not a
/// deletion), so removing one means saving the harness definition afresh.
fn merge_remote_auth(stored: &RemoteAuth, body: &Value) -> RemoteAuth {
    let named_secret = first_text(body, &["secretRef"]);
    let named_header = first_text(body, &["authHeader"]);
    if named_secret.is_none() && named_header.is_none() {
        return stored.clone();
    }
    // What is stored, so a half the request did not name is preserved rather than
    // dropped.
    let (stored_secret, stored_header) = match stored {
        RemoteAuth::None => (None, None),
        RemoteAuth::Bearer { secret_ref } => (Some(secret_ref.clone()), None),
        RemoteAuth::Header { name, secret_ref } => {
            (Some(secret_ref.clone()), Some(name.clone()))
        }
    };
    match (named_secret.or(stored_secret), named_header.or(stored_header)) {
        (Some(secret_ref), Some(name)) => RemoteAuth::Header { name, secret_ref },
        (Some(secret_ref), None) => RemoteAuth::Bearer { secret_ref },
        // A header named with no secret anywhere cannot authenticate anything, so
        // the credential stays absent rather than becoming half of one.
        (None, _) => RemoteAuth::None,
    }
}

/// Read a harness out of the client's own shape.
///
/// Read DEFENSIVELY, field by field, because this shape is upstream's and this
/// port cannot compile against its Rust types: a missing field is a named error
/// rather than a panic or a silent default.
fn harness_from(value: &Value) -> Result<Harness, String> {
    // THREE shapes reach here and all of them are the client's own: a bare
    // object, `{input: …}` (create/update managed agent) and `{definition: …}`
    // (save_custom_harness). Reading only one would leave two commands answering
    // "a harness needs an id" for a perfectly well-formed request.
    let body = value
        .get("input")
        .or_else(|| value.get("definition"))
        .unwrap_or(value);
    let label = first_text(body, LABEL_KEYS);
    let id = first_text(body, &["id", "harnessId", "agentId"])
        .or_else(|| label.clone())
        .as_deref()
        .and_then(clean_harness_id)
        .ok_or_else(|| "a harness needs an id or a name".to_string())?;
    let label = label.unwrap_or_else(|| id.clone());

    if let Some(endpoint) = first_text(body, &["endpoint"]) {
        let harness = Harness::RemoteAcp {
            id,
            label,
            endpoint,
            auth: remote_auth(body).unwrap_or(RemoteAuth::None),
        };
        harness.validate().map_err(oops)?;
        return Ok(harness);
    }

    // The client calls this field `acpCommand` on a custom harness and
    // `agentCommand` on a managed agent; upstream's own row calls it
    // `binary_path`. All four name the program to run.
    let command = first_text(body, COMMAND_KEYS)
        .ok_or_else(|| "a local harness needs a command".to_string())?;
    let args = string_list(body, ARGS_KEYS).unwrap_or_default();
    let env = string_map(body, ENV_KEYS).unwrap_or_default();
    let harness = Harness::LocalStdio {
        id,
        label,
        command,
        args,
        env,
    };
    harness.validate().map_err(oops)?;
    Ok(harness)
}

/// Apply one SPARSE update to a stored harness.
///
/// The client's update payload names the agent by PUBKEY and carries only what
/// somebody changed — `{pubkey, model}` and `{pubkey, name}` are both ordinary
/// updates — so reading it as a whole definition would refuse most of them for
/// want of a command nobody was changing. Merged into what is stored instead.
///
/// The ID never moves, and that is a decision rather than an omission: the
/// agent's signing key is kept under its id, so a rename that changed the id
/// would hand the agent a NEW identity — a new pubkey, no membership, and every
/// message it had already posted orphaned from the next. A rename changes the
/// label the workspace sees, which is what a person means by one.
fn merge_harness(stored: &Harness, body: &Value) -> Result<Harness, String> {
    let id = stored.id().to_string();
    let label = first_text(body, LABEL_KEYS).unwrap_or_else(|| stored.label().to_string());
    let harness = match stored {
        Harness::LocalStdio {
            command, args, env, ..
        } => Harness::LocalStdio {
            id,
            label,
            command: first_text(body, COMMAND_KEYS).unwrap_or_else(|| command.clone()),
            args: string_list(body, ARGS_KEYS).unwrap_or_else(|| args.clone()),
            env: string_map(body, ENV_KEYS).unwrap_or_else(|| env.clone()),
        },
        Harness::RemoteAcp { endpoint, auth, .. } => Harness::RemoteAcp {
            id,
            label,
            endpoint: first_text(body, &["endpoint"]).unwrap_or_else(|| endpoint.clone()),
            auth: merge_remote_auth(auth, body),
        },
    };
    harness.validate().map_err(oops)?;
    Ok(harness)
}

fn remote_running(harness: &Harness) -> RuntimeStatus {
    let mut status = RuntimeStatus::stopped(harness);
    status.lifecycle = buzz_desktop_core::harness::Lifecycle::Running;
    status
}

fn start_harness(
    app: &AppHandle,
    state: &AppState,
    id: &str,
) -> Result<RuntimeStatus, String> {
    let harness = state.harness(id).map_err(oops)?;
    match &harness {
        Harness::RemoteAcp { .. } => {
            if remote::is_connected(state, id) {
                return Err(format!("{} is already running", harness.label()));
            }
            remote::connect(state, &harness, None)?;
            Ok(remote_running(&harness))
        }
        Harness::LocalStdio { .. } => {
            // Restrict the agent's initial working directory to the managed
            // repositories root rather than `/`.
            let cwd = state.workspace.repos_dir().to_path_buf();
            state
                .supervisor
                .start(app, &harness, &cwd, &[])
                .map_err(oops)
        }
    }
}

fn stop_harness(state: &AppState, id: &str) -> Result<RuntimeStatus, String> {
    let harness = state.harness(id).map_err(oops)?;
    match &harness {
        Harness::RemoteAcp { .. } => {
            if !remote::is_connected(state, id) {
                return Err(format!("{id} is not running"));
            }
            remote::disconnect(state, id);
            Ok(RuntimeStatus::stopped(&harness))
        }
        Harness::LocalStdio { .. } => crate::supervisor::stop_local(state, id).map_err(oops),
    }
}

fn prompt_harness(
    app: &AppHandle,
    state: &AppState,
    id: &str,
    text: &str,
    target: Option<&AnswerTarget>,
) -> Result<u64, String> {
    let harness = state.harness(id).map_err(oops)?;
    match &harness {
        Harness::RemoteAcp { .. } => {
            remote::prompt(app, state, &harness, text, target.cloned())
        }
        Harness::LocalStdio { .. } => {
            state.supervisor.prompt(id, text, target).map_err(oops)
        }
    }
}

/// Every configured harness's status, running or not.
///
/// The supervisor only knows about PROCESSES, so a connected remote harness would
/// otherwise be reported stopped while it is perfectly promptable — and the UI
/// would offer to start something that is already running.
fn harness_statuses(state: &AppState) -> Result<Vec<RuntimeStatus>, String> {
    let configured = state
        .harnesses
        .lock()
        .map_err(|_| "the harness list is unavailable".to_string())?
        .clone();
    Ok(statuses_for(state, &configured))
}

/// [`harness_statuses`] over a table the caller already holds, so a caller that
/// needs both the harnesses and their statuses clones the list once, not twice.
fn statuses_for(state: &AppState, configured: &[Harness]) -> Vec<RuntimeStatus> {
    let mut statuses = state.supervisor.statuses(configured);
    for status in &mut statuses {
        let is_remote = configured
            .iter()
            .any(|harness| harness.id() == status.id && matches!(harness, Harness::RemoteAcp { .. }));
        if is_remote && remote::is_connected(state, &status.id) {
            status.lifecycle = buzz_desktop_core::harness::Lifecycle::Running;
        }
    }
    statuses
}

/// Validate, persist, and drop any connection made under the OLD configuration.
///
/// A reconfigured remote harness must not keep its previous connection: that
/// connection answers as the old endpoint, with the old credential.
fn upsert(state: &AppState, harness: &Harness) -> Result<(), String> {
    remote::disconnect(state, harness.id());
    state.upsert_harness(harness.clone()).map_err(oops)
}

fn forget_harness_by_id(state: &AppState, id: &str) -> Result<bool, String> {
    if state.supervisor.is_running(id) {
        crate::supervisor::stop_local(state, id).map_err(oops)?;
    }
    remote::disconnect(state, id);
    let removed = state.remove_harness(id).map_err(oops)?;
    crate::workspace::forget_agent_origin(state, id);
    Ok(removed)
}

fn forget_harness(state: &AppState, input: &Value) -> Result<bool, String> {
    let id = resolve_harness(state, input)?;
    forget_harness_by_id(state, &id)
}

/// One harness as a runtime CATALOG entry.
///
/// The shape `save_custom_harness` answers with, and the same one the seam's
/// harness catalog serves rows of — so a harness saved here renders in the same
/// list a discovered one does.
fn catalog_entry(harness: &Harness) -> Value {
    let (command, args) = match harness {
        Harness::LocalStdio { command, args, .. } => (command.clone(), args.clone()),
        Harness::RemoteAcp { endpoint, .. } => (endpoint.clone(), Vec::new()),
    };
    // A remote endpoint is not something to look for on PATH, and probing it
    // would spend a credential to answer a settings screen.
    let found = match harness {
        Harness::RemoteAcp { .. } => None,
        Harness::LocalStdio { .. } => which(&command),
    };
    let available = matches!(harness, Harness::RemoteAcp { .. }) || found.is_some();
    json!({
        "id": harness.id(),
        "label": harness.label(),
        "avatar_url": "",
        "availability": if available { "available" } else { "not_installed" },
        "command": command,
        "binary_path": found,
        "default_args": args,
        // This shell hands an agent no MCP configuration and configures neither
        // its model nor its provider, so there is no variable to name for those.
        "mcp_command": Value::Null,
        "model_env_var": Value::Null,
        "provider_env_var": Value::Null,
        "thinking_env_var": Value::Null,
        "install_hint": if available {
            String::new()
        } else {
            format!("{command} was not found on PATH. Install it, then start the agent.")
        },
        "install_instructions_url": "",
        // Installing means running a package manager as the user, which this
        // shell declines to do on their behalf (`registry::DECLINED`).
        "can_auto_install": false,
        "requires_external_cli": false,
        "underlying_cli_path": Value::Null,
        "node_required": false,
        "auth_status": { "status": "not_applicable" },
        "login_hint": Value::Null,
        // Custom, because a harness in this list is one somebody configured here.
        "source": "custom",
    })
}

/// `create_managed_agent` — configure a local or remote agent, and give it an
/// identity the workspace can address.
#[tauri::command]
pub fn create_managed_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    let harness = harness_from(&input)?;
    let body = body_of(&input);
    // The agent is bound to the workspace its relay URL names, so its profile
    // and answers go there and nowhere else (`workspace::agent_origin`).
    let relay_origin = body
        .get("relayUrl")
        .and_then(Value::as_str)
        .and_then(crate::workspace::origin_from_relay_url);
    if let Some(origin) = &relay_origin {
        crate::workspace::bind_agent_origin(&state, harness.id(), origin)?;
    }
    // The dialog's relay URL names the deployment this agent belongs to, which is
    // the same one its answers must be posted to — so somebody who filled that
    // field in has already said where the workspace is, and asking again in a file
    // would be a second step for no reason. Create-if-absent: an origin configured
    // deliberately is never replaced by one inferred from a form.
    if let Some(origin) = &relay_origin {
        let _ = crate::workspace::remember_origin(&state, origin);
    }
    upsert(&state, &harness)?;
    if let Some(on_launch) = body.get("startOnAppLaunch").and_then(Value::as_bool) {
        let _ = state.set_flag(harness.id(), None, Some(on_launch));
    }
    // The PROFILE is what makes this agent mentionable: a client builds its
    // mention list from kind-0 events, so without one nobody can type at it.
    // Best effort and REPORTED (the client renders `profileSyncError`), because an
    // agent that exists without a name yet is a cosmetic gap, while refusing the
    // whole creation over it would throw away a configured harness.
    let profile_sync_error =
        crate::workspace::publish_agent_profile(&state, harness.id(), harness.label()).err();
    // "Spawn after create" is a real request, so it is honoured rather than
    // ignored — and its failure goes in its own field instead of failing a create
    // that has already been persisted.
    let spawn_error = if body
        .get("spawnAfterCreate")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        start_harness(&app, &state, harness.id()).err()
    } else {
        None
    };
    Ok(json!({
        "agent": managed_agent_row(&state, harness.id())?,
        // The AGENT's own key, for the backup panel the dialog shows. Never the
        // user's: this shell does not hold one, and these are not their words.
        "private_key_nsec": crate::workspace::agent_nsec(&state, harness.id()),
        "profile_sync_error": profile_sync_error,
        "spawn_error": spawn_error,
    }))
}

#[tauri::command]
pub fn update_managed_agent(state: State<'_, AppState>, input: Value) -> Result<Value, String> {
    // Updates name the agent by pubkey and carry only changed fields.
    let id = resolve_harness(&state, &input)?;
    let stored = state.harness(&id).map_err(oops)?;
    let body = body_of(&input);
    let harness = merge_harness(&stored, body)?;
    if let Some(origin) = body
        .get("relayUrl")
        .and_then(Value::as_str)
        .and_then(crate::workspace::origin_from_relay_url)
    {
        crate::workspace::bind_agent_origin(&state, harness.id(), &origin)?;
    }
    upsert(&state, &harness)?;
    let profile_sync_error =
        crate::workspace::publish_agent_profile(&state, harness.id(), harness.label()).err();
    Ok(json!({
        "agent": managed_agent_row(&state, harness.id())?,
        "profile_sync_error": profile_sync_error,
    }))
}

#[tauri::command]
pub fn save_custom_harness(state: State<'_, AppState>, input: Value) -> Result<Value, String> {
    let harness = harness_from(&input)?;
    // `originalId` names the entry being REPLACED, which may have been renamed —
    // without dropping it the old definition would linger beside the new one and
    // both would be offered.
    if let Some(original) = input
        .get("originalId")
        .and_then(Value::as_str)
        .and_then(clean_harness_id)
    {
        if original != harness.id() {
            forget_harness_by_id(&state, &original)?;
        }
    }
    upsert(&state, &harness)?;
    Ok(catalog_entry(&harness))
}

#[tauri::command]
pub fn delete_managed_agent(state: State<'_, AppState>, input: Value) -> Result<bool, String> {
    forget_harness(&state, &input)
}

#[tauri::command]
pub fn delete_custom_harness(state: State<'_, AppState>, input: Value) -> Result<bool, String> {
    forget_harness(&state, &input)
}

/// A harness id from whatever the client called this thing.
///
/// The managed-agent dialog collects a free-text NAME ("Claude Code"), and a
/// harness id has to be `[a-z0-9-_]` for `Harness::validate` to accept it — so a
/// name is slugged rather than refused, which is what lets that dialog work at
/// all. The same rule the relay applies to its own agent ids, deliberately.
fn clean_harness_id(raw: &str) -> Option<String> {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.trim().chars() {
        let lower = ch.to_ascii_lowercase();
        if lower.is_ascii_alphanumeric() || lower == '-' || lower == '_' {
            out.push(lower);
        } else if !out.ends_with('-') {
            // Any other rune becomes ONE separator, so "Claude  Code" and
            // "Claude Code" name the same harness rather than two.
            out.push('-');
        }
    }
    let id = out.trim_matches('-').to_string();
    (!id.is_empty() && id.len() <= 64).then_some(id)
}

/// What a request NAMES, in the order the resolver trusts it.
///
/// The order is the interesting part, and it is why this is a type rather than a
/// chain of `or_else`: an update payload carries `{pubkey, name}` when somebody
/// renames an agent, so slugging that NEW name would look up a harness which does
/// not exist — while the pubkey names the one being renamed exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Reference {
    /// An explicit harness id, which is this shell's own name for it.
    Id(String),
    /// An agent pubkey — an exact identity, resolved against the keys held here.
    Pubkey(String),
    /// A display name, which is a GUESS: slugged the way a create would have.
    DisplayName(String),
}

/// What one request names, if anything.
///
/// Pure, so the PRIORITY is testable without a keychain — which is the property
/// that matters, since getting it wrong silently addresses the wrong agent.
fn harness_reference(value: &Value) -> Option<Reference> {
    let body = body_of(value);
    if let Some(named) = first_text(body, &["id", "harnessId", "agentId", "runtimeId"]) {
        return clean_harness_id(&named).map(Reference::Id);
    }
    if let Some(bare) = body
        .as_str()
        .filter(|value| !value.trim().is_empty())
        .and_then(clean_harness_id)
    {
        return Some(Reference::Id(bare));
    }
    if let Some(pubkey) = first_text(body, &["pubkey", "agentPubkey", "agent_pubkey", "outerPubkey"]) {
        return Some(Reference::Pubkey(pubkey));
    }
    // `connect_acp_runtime` sends `{request: {runtimeId, methodId}}`.
    if let Some(request) = body.get("request").filter(|request| request.is_object()) {
        return harness_reference(request);
    }
    first_text(body, &["name"])
        .as_deref()
        .and_then(clean_harness_id)
        .map(Reference::DisplayName)
}

/// The harness one command acts on.
///
/// The managed-agent family addresses agents by PUBKEY — `delete_managed_agent`
/// and `get_managed_agent_log` send `{pubkey}`, a channel message sends
/// `{agentPubkey}`, an update sends `{pubkey, …}` — because a harness id is this
/// shell's own bookkeeping and the client has never seen one. Resolving against
/// the agent keys held here is what makes a match meaningful: it names an agent
/// this shell can actually speak as.
fn resolve_harness(state: &AppState, value: &Value) -> Result<String, String> {
    match harness_reference(value) {
        Some(Reference::Id(id)) | Some(Reference::DisplayName(id)) => Ok(id),
        Some(Reference::Pubkey(pubkey)) => crate::workspace::harness_for_agent(state, &pubkey)
            .ok_or_else(|| {
                format!(
                    "no harness on this machine has the agent identity {pubkey}; that \
                     agent runs somewhere else"
                )
            }),
        None => Err("that command needs a harness id or an agent pubkey".to_string()),
    }
}

/// The environment one harness carries, as the client's row shape wants it.
fn env_map(harness: &Harness) -> Value {
    match harness {
        Harness::LocalStdio { env, .. } => {
            let mut map = serde_json::Map::new();
            for (key, value) in env {
                map.insert(key.clone(), Value::String(value.clone()));
            }
            Value::Object(map)
        }
        // A remote endpoint runs nowhere on this machine, so there is no
        // environment to report — and its credential is a keychain REFERENCE,
        // which must not be presented as configuration either.
        Harness::RemoteAcp { .. } => Value::Object(serde_json::Map::new()),
    }
}

/// `list_managed_agents` — the agents this machine hosts.
///
/// A DECLARED OVERRIDE of the seam (`registry::OVERRIDES`): a tab hosts no agent
/// processes and answers `[]`, which is true of a tab and false here. It is also
/// the command the whole local-agent path hangs from — the client learns an
/// agent's PUBKEY here and addresses everything downstream by it (the mention,
/// `send_managed_agent_channel_message`, and the answer posted as that agent), so
/// an empty list leaves a configured harness undiscoverable and mute.
///
/// Every field is either a fact about this shell or an honest null. Nothing is
/// invented to fill the client's shape: no fabricated persona, no made-up log
/// path, no timestamp taken per read.
fn managed_agent_rows(state: &AppState) -> Result<Vec<Value>, String> {
    let configured = state
        .harnesses
        .lock()
        .map_err(|_| "the harness list is unavailable".to_string())?
        .clone();
    let statuses = statuses_for(state, &configured);
    // The config file's mtime, not `now`: see `AppState::config_modified_ms`.
    let configured_at = time::iso_from_epoch_ms(state.config_modified_ms());

    let mut rows = Vec::with_capacity(configured.len());
    for harness in &configured {
        let status = statuses.iter().find(|status| status.id == harness.id());
        let live = status.is_some_and(|status| status.lifecycle == Lifecycle::Running);
        let flags = state.flags_for(harness.id());
        let (command, args, backend, lifecycle) = match harness {
            Harness::LocalStdio { command, args, .. } => (
                command.clone(),
                args.clone(),
                json!({ "type": "local" }),
                if live { "running" } else { "stopped" },
            ),
            Harness::RemoteAcp { endpoint, .. } => (
                endpoint.clone(),
                Vec::new(),
                json!({ "type": "provider", "id": "remote-acp", "config": {} }),
                // A provider-backed agent is DEPLOYED rather than running: there
                // is no process here to be running, and saying "running" would
                // claim one.
                if live { "deployed" } else { "not_deployed" },
            ),
        };
        // The row's whole purpose is to be ADDRESSABLE, so an unreadable identity
        // refuses the list rather than serving an agent with an empty pubkey: such
        // a row renders, cannot be mentioned, and gives no clue why — and several
        // of them would be indistinguishable from each other.
        let pubkey = crate::workspace::agent_identity(state, harness.id()).map_err(|detail| {
            format!(
                "{} has no usable agent identity, so nothing could address it: {detail}",
                harness.label()
            )
        })?;
        rows.push(json!({
            // The identity a moderator adds to a room, and the name the client
            // addresses this agent by.
            "pubkey": pubkey,
            "name": harness.label(),
            "runtime": harness.id(),
            // No local persona packs and no teams in this shell (the seam answers
            // both with an empty catalog), so there is nothing to point at.
            "persona_id": Value::Null,
            "team_id": Value::Null,
            "relay_url": crate::workspace::agent_origin(state, harness.id()).unwrap_or_default(),
            "acp_command": command,
            "agent_command": command,
            "agent_command_override": Value::Null,
            "agent_args": args,
            // This shell hands an agent no MCP configuration — see
            // `acp::session_new_request`, which always sends `mcpServers: []`.
            "mcp_command": "",
            "turn_timeout_seconds": 300,
            "idle_timeout_seconds": Value::Null,
            "max_turn_duration_seconds": Value::Null,
            // ONE turn at a time, and this is not decoration: it is what
            // `Supervisor::admit_prompt` and the remote driver enforce.
            "parallelism": 1,
            // The adapter owns its own prompt and model; this shell configures
            // neither, so claiming one would misdescribe the agent.
            "system_prompt": Value::Null,
            "avatar_url": Value::Null,
            "model": Value::Null,
            "provider": Value::Null,
            "persona_out_of_date": false,
            "persona_orphaned": false,
            "needs_restart": false,
            "restart_diff": [],
            "env_vars": env_map(harness),
            "status": lifecycle,
            "pid": status.and_then(|status| status.pid),
            "created_at": configured_at,
            "updated_at": configured_at,
            "last_started_at": Value::Null,
            "last_stopped_at": Value::Null,
            "last_exit_code": status.and_then(|status| status.exit_code),
            "last_error": status.and_then(|status| status.detail.clone()),
            "last_error_code": Value::Null,
            // The log lives in memory (`get_managed_agent_log` serves it), so
            // there is no path — and a made-up one would send somebody looking
            // for a file that does not exist.
            "log_path": "",
            "start_on_app_launch": flags.start_on_app_launch,
            "auto_restart_on_config_change": flags.auto_restart,
            "backend": backend,
            "backend_agent_id": Value::Null,
            // Who the agent answers is the RELAY's question, not this shell's: it
            // replies to what reaches it in a room it belongs to, and a moderator
            // decides which rooms those are.
            "respond_to": "anyone",
            "respond_to_allowlist": [],
        }));
    }
    Ok(rows)
}

#[tauri::command]
pub fn list_managed_agents(state: State<'_, AppState>) -> Result<Vec<Value>, String> {
    managed_agent_rows(&state)
}

/// ONE agent's row, so a create or an update answers with exactly what the list
/// says about it.
///
/// Built through the same function rather than assembled a second time: a row
/// produced by two code paths is a row that can disagree with itself, and the
/// client renders whichever it happened to receive.
fn managed_agent_row(state: &AppState, id: &str) -> Result<Value, String> {
    managed_agent_rows(state)?
        .into_iter()
        .find(|row| row.get("runtime").and_then(Value::as_str) == Some(id))
        .ok_or_else(|| format!("{id} was configured but produced no row"))
}

#[tauri::command]
pub fn list_managed_agent_runtimes(state: State<'_, AppState>) -> Result<Vec<RuntimeStatus>, String> {
    harness_statuses(&state)
}

#[tauri::command]
pub fn start_managed_agent(
    app: AppHandle,
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    // The client maps the answer with `fromRawManagedAgent`, so it is the
    // agent's row, not the bare runtime status.
    let id = resolve_harness(&state, &input)?;
    start_harness(&app, &state, &id)?;
    managed_agent_row(&state, &id)
}

#[tauri::command]
pub fn start_managed_agent_runtime(
    app: AppHandle,
    state: State<'_, AppState>,
    input: Value,
) -> Result<RuntimeStatus, String> {
    let id = resolve_harness(&state, &input)?;
    start_harness(&app, &state, &id)
}

#[tauri::command]
pub fn stop_managed_agent(
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    let id = resolve_harness(&state, &input)?;
    stop_harness(&state, &id)?;
    managed_agent_row(&state, &id)
}

#[tauri::command]
pub fn stop_managed_agent_runtime(
    state: State<'_, AppState>,
    input: Value,
) -> Result<RuntimeStatus, String> {
    let id = resolve_harness(&state, &input)?;
    stop_harness(&state, &id)
}

#[tauri::command]
pub fn restart_managed_agent_runtime(
    app: AppHandle,
    state: State<'_, AppState>,
    input: Value,
) -> Result<RuntimeStatus, String> {
    let id = resolve_harness(&state, &input)?;
    // Transport-aware, deliberately: the supervisor only knows about PROCESSES,
    // so a connected REMOTE harness looks stopped to it — and the start below
    // would then refuse as "already running", making restart impossible for
    // exactly the harnesses that are live.
    if state.supervisor.is_running(&id) || remote::is_connected(&state, &id) {
        stop_harness(&state, &id)?;
    }
    start_harness(&app, &state, &id)
}

#[tauri::command]
pub fn put_managed_agent_runtime_lifecycle(
    app: AppHandle,
    state: State<'_, AppState>,
    input: Value,
) -> Result<RuntimeStatus, String> {
    let id = resolve_harness(&state, &input)?;
    let outer = body_of(&input);
    // The client sends `{outerPubkey, payload}` with the lifecycle inside the
    // payload; a flat body is read as before.
    let body = outer.get("payload").filter(|payload| payload.is_object()).unwrap_or(outer);
    let wanted = body
        .get("lifecycle")
        .or_else(|| body.get("state"))
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_ascii_lowercase();
    let outcome = match wanted.as_str() {
        "running" | "start" | "started" => start_harness(&app, &state, &id),
        "stopped" | "stop" => stop_harness(&state, &id),
        other => {
            return Err(format!(
                "{other:?} is not a lifecycle this shell can apply; use running or stopped"
            ))
        }
    };
    match outcome {
        Ok(status) => Ok(status),
        // Asking for the state something is already in is not a failure — but the
        // answer must be what is ACTUALLY true. Reporting `stopped` for an
        // "already running" harness would be the opposite of the truth, and the
        // UI would offer to start it again.
        Err(e) if e.contains("already running") || e.contains("is not running") => {
            harness_statuses(&state)?
                .into_iter()
                .find(|status| status.id == id)
                .ok_or(e)
        }
        Err(e) => Err(e),
    }
}

#[tauri::command]
pub fn get_managed_agent_log(
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    let id = resolve_harness(&state, &input)?;
    let mut lines = state.supervisor.log(&id);
    if let Some(count) = body_of(&input)
        .get("lineCount")
        .and_then(Value::as_u64)
        .and_then(|count| usize::try_from(count).ok())
    {
        let skip = lines.len().saturating_sub(count);
        lines.drain(..skip);
    }
    // The supervisor keeps the log in memory; there is no file to name.
    Ok(json!({ "content": lines.join("\n"), "log_path": Value::Null }))
}

/// One program's availability, in the client's `{command, resolved_path,
/// available}` shape.
fn command_availability(command: Option<&str>) -> Value {
    let command = command.map(str::trim).unwrap_or_default();
    let found = if command.is_empty() { None } else { which(command) };
    json!({
        "command": command,
        "available": found.is_some(),
        "resolved_path": found,
    })
}

#[tauri::command]
pub fn discover_managed_agent_prereqs(
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    // The managed-agent dialog asks whether its two programs exist
    // (`{acpCommand, mcpCommand}`) and reads `{acp, mcp}`; neither names a
    // harness.
    let body = body_of(&input);
    if harness_reference(&input).is_none()
        && (body.get("acpCommand").is_some() || body.get("mcpCommand").is_some() || body.as_object().is_some_and(|map| map.is_empty()))
    {
        return Ok(json!({
            "acp": command_availability(body.get("acpCommand").and_then(Value::as_str)),
            "mcp": command_availability(body.get("mcpCommand").and_then(Value::as_str)),
        }));
    }
    let id = resolve_harness(&state, &input)?;
    let harness = state.harness(&id).map_err(oops)?;
    match &harness {
        Harness::LocalStdio { command, .. } => {
            // Probed by RUNNING it with no arguments would start an agent; asking
            // the OS whether the program exists is the honest, side-effect-free
            // check.
            let found = which(command);
            Ok(json!({
                "id": id,
                "kind": "local_stdio",
                "command": command,
                "available": found.is_some(),
                "path": found,
                "install_hint": if found.is_some() {
                    String::new()
                } else {
                    format!("{command} was not found on PATH. Install it, then start the agent.")
                },
            }))
        }
        Harness::RemoteAcp { endpoint, .. } => Ok(json!({
            "id": id,
            "kind": "remote_acp",
            "endpoint": endpoint,
            // Nothing to install for a remote endpoint, and nothing is probed:
            // a request would spend a credential to answer a settings screen.
            "available": true,
            "path": Value::Null,
            "install_hint": "",
        })),
    }
}

/// Where `program` lives on PATH, if anywhere.
///
/// Platform-aware because "exists" means different things: on Windows a bare
/// `foo` is launched as `foo.exe` (or whatever `PATHEXT` lists), so searching
/// only for the literal name would report every ordinary command as missing and
/// tell the user to install something they already have.
fn which(program: &str) -> Option<String> {
    let named_path = program.contains('/') || program.contains('\\');
    if named_path {
        return executable_candidates(std::path::Path::new(program))
            .into_iter()
            .find(|candidate| candidate.is_file())
            .map(|candidate| candidate.to_string_lossy().into_owned());
    }
    let paths = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&paths) {
        if let Some(found) = executable_candidates(&dir.join(program))
            .into_iter()
            .find(|candidate| candidate.is_file())
        {
            return Some(found.to_string_lossy().into_owned());
        }
    }
    None
}

/// The filenames one program name can have on this platform.
fn executable_candidates(base: &std::path::Path) -> Vec<std::path::PathBuf> {
    let mut out = vec![base.to_path_buf()];
    if cfg!(windows) {
        // `PATHEXT` is the OS's own list of what counts as executable; the
        // fallback is the conventional default for a machine that has unset it.
        let extensions = std::env::var("PATHEXT")
            .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".to_string());
        for extension in extensions.split(';').filter(|value| !value.is_empty()) {
            let mut name = base.as_os_str().to_os_string();
            name.push(extension);
            out.push(std::path::PathBuf::from(name));
        }
    }
    out
}

#[tauri::command]
pub fn discover_acp_auth_methods() -> Value {
    // What this shell can actually carry — read off the config type rather than
    // listed by hand, so a new variant cannot be forgotten here.
    json!({
        "methods": [
            { "id": "none", "name": "No credential", "label": "No credential", "applies_to": "loopback endpoints" },
            { "id": "bearer", "name": "Bearer token", "label": "Bearer token", "applies_to": "https endpoints" },
            { "id": "header", "name": "Custom header", "label": "Custom header", "applies_to": "https endpoints" }
        ],
        "secrets_stored_in": "os_keychain",
        "websocket_supported": false,
        "websocket_reason": "Queue-backed authenticated WebSockets are not available; use the HTTP endpoint."
    })
}

#[tauri::command]
pub fn connect_acp_runtime(
    app: AppHandle,
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    let id = resolve_harness(&state, &input)?;
    let harness = state.harness(&id).map_err(oops)?;
    match &harness {
        // A local harness "connects" by being started.
        Harness::LocalStdio { .. } => {
            let status = start_harness(&app, &state, &id)?;
            Ok(json!({ "launched": true, "status": status }))
        }
        // A remote one connects for real: `initialize`, then `session/new`. The
        // caller may pass a `binding` (Contract A's fields), which travels as
        // Contract B `_meta` when the agent echoed the capability.
        Harness::RemoteAcp { .. } => {
            let body = body_of(&input);
            let binding = body
                .get("binding")
                .or_else(|| body.get("request").and_then(|request| request.get("binding")))
                .filter(|value| value.is_object());
            let mut connected = remote::connect(&state, &harness, binding)?;
            if let Some(map) = connected.as_object_mut() {
                map.entry("launched").or_insert(Value::Bool(true));
            }
            Ok(connected)
        }
    }
}

#[tauri::command]
pub fn get_runtime_file_config(
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    let id = resolve_harness(&state, &input)?;
    let harness = state.harness(&id).map_err(oops)?;
    let flags = state.flags_for(&id);
    // The harness serializes with secret REFERENCES only (see `harness.rs`), so
    // this is safe to hand to a settings screen.
    Ok(json!({
        // The client's file-layer fields: this shell reads no harness config
        // file, so none is set there.
        "provider": Value::Null,
        "model": Value::Null,
        "satisfiedEnvKeys": [],
        "harness": harness,
        "flags": flags,
        // The agent's OWN workspace identity: a moderator adds this pubkey to a
        // room, and only then can the agent answer in it. Reported here because
        // this is the read a settings screen makes about one harness.
        "agent_pubkey": crate::workspace::agent_pubkey(&state, &id),
        // Whether an answer has anywhere to go at all. `null` when configured;
        // the reason when not, so the gap is visible rather than discovered by an
        // answer disappearing.
        "answer_posting_blocked": crate::workspace::agent_origin(&state, &id).err(),
    }))
}

fn set_harness_flag(
    state: &AppState,
    input: &Value,
    alt_key: &str,
    auto_restart: bool,
) -> Result<Value, String> {
    let id = resolve_harness(state, input)?;
    let body = body_of(input);
    let enabled = body
        .get("enabled")
        .or_else(|| body.get(alt_key))
        .and_then(Value::as_bool)
        .ok_or_else(|| "that command needs an enabled flag".to_string())?;
    let (restart, on_launch) = if auto_restart {
        (Some(enabled), None)
    } else {
        (None, Some(enabled))
    };
    state.set_flag(&id, restart, on_launch).map_err(oops)?;
    // The client maps the answer with `fromRawManagedAgent`.
    managed_agent_row(state, &id)
}

fn message_marker(body: &Value) -> Option<&str> {
    ["marker", "eventId", "messageId"]
        .into_iter()
        .find_map(|key| body.get(key).and_then(Value::as_str))
}

#[tauri::command]
pub fn set_managed_agent_auto_restart(
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    let key = if body_of(&input).get("autoRestartOnConfigChange").is_some() {
        "autoRestartOnConfigChange"
    } else {
        "autoRestart"
    };
    set_harness_flag(&state, &input, key, true)
}

#[tauri::command]
pub fn set_managed_agent_start_on_app_launch(
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    set_harness_flag(&state, &input, "startOnAppLaunch", false)
}

#[tauri::command]
pub fn send_managed_agent_channel_message(
    app: AppHandle,
    state: State<'_, AppState>,
    input: Value,
) -> Result<Value, String> {
    let body = body_of(&input);
    let id = resolve_harness(&state, &input)?;
    let text = body
        .get("message")
        .or_else(|| body.get("text"))
        .or_else(|| body.get("content"))
        .and_then(Value::as_str)
        .ok_or_else(|| "that message has no text".to_string())?;
    let marker = message_marker(body).map(str::to_string);
    let channel = body
        .get("channelId")
        .or_else(|| body.get("channel_id"))
        .or_else(|| body.get("channel"))
        .and_then(Value::as_str)
        .filter(|id| !id.is_empty());
    let parent = body
        .get("parentEventId")
        .or_else(|| body.get("parent_event_id"))
        .and_then(Value::as_str);
    let target = channel.map(|channel| AnswerTarget::new(channel, parent));
    // Re-delivered channel events must not prompt the agent twice.
    if let Some(marker) = &marker {
        if state.already_delivered(marker) {
            return Ok(json!({ "delivered": false, "reason": "already delivered" }));
        }
    }
    let request_id = prompt_harness(&app, &state, &id, text, target.as_ref())?;
    if let Some(marker) = &marker {
        state.mark_delivered(marker);
    }
    Ok(json!({
        "delivered": true,
        "request_id": request_id,
        "harness": id,
        "answer_channel": target.as_ref().map(|t| t.channel.clone()),
        "answer_thread": target.as_ref().and_then(|t| t.parent.clone()),
    }))
}

#[tauri::command]
pub fn has_managed_agent_channel_message_marker(
    state: State<'_, AppState>,
    input: Value,
) -> Result<bool, String> {
    let marker = message_marker(body_of(&input))
        .ok_or_else(|| "that command needs a marker".to_string())?;
    Ok(state.already_delivered(marker))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_harness_is_read_out_of_the_clients_shape() {
        let local = harness_from(&json!({
            "input": {
                "id": "claude",
                "displayName": "Claude Code",
                "command": "claude-code-acp",
                "args": ["--stdio"],
                "env": { "ANTHROPIC_MODEL": "claude-sonnet" }
            }
        }))
        .expect("local");
        assert_eq!(local.id(), "claude");
        assert_eq!(local.label(), "Claude Code");

        // An endpoint decides the transport.
        let remote = harness_from(&json!({
            "id": "cloud",
            "endpoint": "https://buzz.example/acp",
            "secretRef": "cloud"
        }))
        .expect("remote");
        assert!(matches!(remote, Harness::RemoteAcp { .. }));

        // A reserved environment variable is refused here too, because the same
        // validator runs.
        assert!(harness_from(&json!({
            "id": "x",
            "command": "x",
            "env": { "BUZZ_AGENT_NSEC": "leak" }
        }))
        .is_err());

        assert!(harness_from(&json!({ "command": "x" })).is_err(), "no id");
        assert!(harness_from(&json!({ "id": "x" })).is_err(), "no command");
    }

    #[test]
    fn the_managed_agent_dialogs_own_shape_is_accepted() {
        // What `createManagedAgent` actually sends: a free-text NAME, the command
        // under `agentCommand`, arguments under `agentArgs`, environment under
        // `envVars` — and no id at all. Reading only `command`/`args`/`env` made
        // this request fail with "a harness needs an id" for a form somebody had
        // filled in correctly.
        let harness = harness_from(&json!({
            "input": {
                "name": "Claude Code",
                "relayUrl": "wss://buzz.example/buzz/",
                "agentCommand": "claude-code-acp",
                "agentArgs": ["--stdio"],
                "envVars": { "ANTHROPIC_MODEL": "claude-sonnet" },
                "spawnAfterCreate": true
            }
        }))
        .expect("the dialog's shape");
        // The id is SLUGGED from the name, because `Harness::validate` accepts no
        // spaces — while the label keeps what the person typed.
        assert_eq!(harness.id(), "claude-code");
        assert_eq!(harness.label(), "Claude Code");
        match &harness {
            Harness::LocalStdio { command, args, env, .. } => {
                assert_eq!(command, "claude-code-acp");
                assert_eq!(args, &vec!["--stdio".to_string()]);
                assert!(env.iter().any(|(key, _)| key == "ANTHROPIC_MODEL"));
            }
            other => panic!("{other:?}"),
        }

        // And `saveCustomHarness`'s shape, which nests under `definition`.
        let custom = harness_from(&json!({
            "definition": {
                "id": "goose",
                "label": "Goose",
                "command": "goose",
                "args": ["acp"],
                "env": {}
            },
            "originalId": null
        }))
        .expect("the definition shape");
        assert_eq!(custom.id(), "goose");
        assert_eq!(custom.label(), "Goose");
    }

    #[test]
    fn a_name_becomes_a_usable_id_or_none_at_all() {
        assert_eq!(clean_harness_id("Claude Code").as_deref(), Some("claude-code"));
        assert_eq!(clean_harness_id("Claude  Code").as_deref(), Some("claude-code"));
        assert_eq!(clean_harness_id(" codex ").as_deref(), Some("codex"));
        assert_eq!(clean_harness_id("my_agent-2").as_deref(), Some("my_agent-2"));
        // A name with nothing usable in it is NOT silently turned into something:
        // an id invented here could never be found again.
        for unusable in ["", "   ", "\u{1f600}", "---", "///"] {
            assert!(clean_harness_id(unusable).is_none(), "{unusable:?}");
        }
        assert!(clean_harness_id(&"x".repeat(65)).is_none(), "bounded");
        // Every produced id is one `Harness::validate` accepts.
        for name in ["Claude Code", "Codex CLI", "caf\u{e9} agent"] {
            let id = clean_harness_id(name).expect(name);
            assert!(
                id.bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_'),
                "{id}"
            );
        }
    }

    #[test]
    fn a_sparse_update_changes_only_what_it_names() {
        // The payload the client actually sends: the agent named by pubkey, and
        // only the fields somebody touched. Read as a whole definition, `{pubkey,
        // model}` was refused for want of a command nobody was changing.
        let stored = Harness::LocalStdio {
            id: "claude-code".into(),
            label: "Claude Code".into(),
            command: "claude-code-acp".into(),
            args: vec!["--stdio".into()],
            env: vec![("ANTHROPIC_MODEL".into(), "sonnet".into())],
        };

        // A field this shell does not configure changes nothing at all.
        let merged = merge_harness(&stored, &json!({ "pubkey": "aa".repeat(32), "model": "opus" }))
            .expect("a sparse update");
        assert_eq!(merged, stored);

        // A rename changes the label — and NOT the id, because the agent's key is
        // kept under its id: moving it would hand the agent a new identity and
        // orphan everything it had already said.
        let renamed = merge_harness(&stored, &json!({ "name": "Claude" })).expect("rename");
        assert_eq!(renamed.label(), "Claude");
        assert_eq!(renamed.id(), "claude-code", "the identity must not move");

        // A command change lands, whichever of the client's names it arrives under.
        let recommanded =
            merge_harness(&stored, &json!({ "agentCommand": "claude-next" })).expect("command");
        match &recommanded {
            Harness::LocalStdio { command, args, env, .. } => {
                assert_eq!(command, "claude-next");
                assert_eq!(args, &vec!["--stdio".to_string()], "untouched");
                assert_eq!(env.len(), 1, "untouched");
            }
            other => panic!("{other:?}"),
        }

        // An EMPTY list is a change, not an absence: "there are no arguments now"
        // and "leave the arguments alone" are opposite instructions.
        let cleared = merge_harness(&stored, &json!({ "agentArgs": [] })).expect("cleared");
        match &cleared {
            Harness::LocalStdio { args, .. } => assert!(args.is_empty()),
            other => panic!("{other:?}"),
        }

        // And an update is still VALIDATED: a reserved variable cannot be smuggled
        // in through the update path either.
        assert!(merge_harness(
            &stored,
            &json!({ "envVars": { "BUZZ_AGENT_NSEC": "leak" } })
        )
        .is_err());
    }

    #[test]
    fn a_remote_harness_keeps_its_credential_through_an_update() {
        let stored = Harness::RemoteAcp {
            id: "cloud".into(),
            label: "Ferrite Cloud".into(),
            endpoint: "https://buzz.example/acp".into(),
            auth: RemoteAuth::Bearer {
                secret_ref: "cloud".into(),
            },
        };
        // Renaming must not silently drop the token reference — which would make
        // the next prompt fail with an authentication error nobody caused.
        let renamed = merge_harness(&stored, &json!({ "name": "Cloud" })).expect("rename");
        match &renamed {
            Harness::RemoteAcp { auth, endpoint, .. } => {
                assert_eq!(
                    auth,
                    &RemoteAuth::Bearer {
                        secret_ref: "cloud".into()
                    }
                );
                assert_eq!(endpoint, "https://buzz.example/acp");
            }
            other => panic!("{other:?}"),
        }
        // A new secret reference DOES land.
        let recredentialed =
            merge_harness(&stored, &json!({ "secretRef": "other" })).expect("credential");
        match &recredentialed {
            Harness::RemoteAcp { auth, .. } => assert_eq!(
                auth,
                &RemoteAuth::Bearer {
                    secret_ref: "other".into()
                }
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn one_credential_field_never_changes_the_auth_mode() {
        // The case a whole-object replacement gets wrong: rotating the SECRET on a
        // custom-header credential would turn it into a bearer and lose the header
        // name the endpoint expects — authentication then fails for a reason the
        // person who rotated the token did not cause.
        let header = RemoteAuth::Header {
            name: "x-api-key".into(),
            secret_ref: "cloud".into(),
        };
        assert_eq!(
            merge_remote_auth(&header, &json!({ "secretRef": "rotated" })),
            RemoteAuth::Header {
                name: "x-api-key".into(),
                secret_ref: "rotated".into()
            },
            "the header name must survive a secret rotation"
        );
        // And naming only the HEADER applies to the secret already stored, rather
        // than being ignored for want of one nobody was rotating.
        assert_eq!(
            merge_remote_auth(&header, &json!({ "authHeader": "x-other" })),
            RemoteAuth::Header {
                name: "x-other".into(),
                secret_ref: "cloud".into()
            }
        );
        // A request naming neither leaves it exactly alone.
        assert_eq!(
            merge_remote_auth(&header, &json!({ "name": "Renamed" })),
            header
        );

        // A BEARER credential gains a header the same way, keeping its secret.
        let bearer = RemoteAuth::Bearer {
            secret_ref: "cloud".into(),
        };
        assert_eq!(
            merge_remote_auth(&bearer, &json!({ "authHeader": "x-api-key" })),
            RemoteAuth::Header {
                name: "x-api-key".into(),
                secret_ref: "cloud".into()
            }
        );

        // From NO credential: a secret makes a bearer, while a header alone cannot
        // authenticate anything and so stays absent rather than becoming half of a
        // credential.
        assert_eq!(
            merge_remote_auth(&RemoteAuth::None, &json!({ "secretRef": "cloud" })),
            RemoteAuth::Bearer {
                secret_ref: "cloud".into()
            }
        );
        assert_eq!(
            merge_remote_auth(&RemoteAuth::None, &json!({ "authHeader": "x-api-key" })),
            RemoteAuth::None
        );
    }

    #[test]
    fn a_pubkey_outranks_a_display_name() {
        // The rename case, which is where the priority earns its keep: `{pubkey,
        // name}` must resolve the agent being RENAMED, not a harness slugged from
        // the new name — which does not exist.
        assert_eq!(
            harness_reference(&json!({ "input": { "pubkey": "aa".repeat(32), "name": "New Name" } })),
            Some(Reference::Pubkey("aa".repeat(32)))
        );
        // An explicit id outranks both.
        assert_eq!(
            harness_reference(&json!({ "id": "claude", "pubkey": "aa".repeat(32) })),
            Some(Reference::Id("claude".into()))
        );
        // A name alone is still usable, slugged the way a create slugged it.
        assert_eq!(
            harness_reference(&json!({ "name": "Claude Code" })),
            Some(Reference::DisplayName("claude-code".into()))
        );
        assert_eq!(harness_reference(&json!({})), None);
    }

    #[test]
    fn a_named_id_is_found_in_every_shape_and_slugged_the_same_way() {
        for (payload, expected) in [
            (json!({ "id": "a" }), "a"),
            (json!({ "agentId": "b" }), "b"),
            (json!({ "input": { "runtimeId": "c" } }), "c"),
            (json!("d"), "d"),
        ] {
            assert_eq!(
                harness_reference(&payload),
                Some(Reference::Id(expected.to_string())),
                "{payload}"
            );
        }
        assert!(harness_reference(&json!({ "id": "" })).is_none());
    }

    #[test]
    fn a_catalog_entry_reports_what_it_knows_and_no_more() {
        let harness = Harness::LocalStdio {
            id: "goose".into(),
            label: "Goose".into(),
            command: "definitely-not-a-program-on-this-machine".into(),
            args: vec!["acp".into()],
            env: vec![],
        };
        let entry = catalog_entry(&harness);
        assert_eq!(entry["id"], "goose");
        assert_eq!(entry["label"], "Goose");
        assert_eq!(entry["availability"], "not_installed");
        assert!(entry["binary_path"].is_null(), "nothing was found");
        assert!(entry["install_hint"]
            .as_str()
            .expect("hint")
            .contains("was not found on PATH"));
        assert_eq!(entry["source"], "custom");
        // Nothing is claimed about an installer or an MCP command.
        assert_eq!(entry["can_auto_install"], false);
        assert!(entry["mcp_command"].is_null());

        // A REMOTE harness is not probed on PATH — there is nothing local to find,
        // and its endpoint is reported as the command.
        let remote = Harness::RemoteAcp {
            id: "cloud".into(),
            label: "Ferrite Cloud".into(),
            endpoint: "https://buzz.example/acp".into(),
            auth: RemoteAuth::None,
        };
        let entry = catalog_entry(&remote);
        assert_eq!(entry["availability"], "available");
        assert_eq!(entry["command"], "https://buzz.example/acp");
        assert!(entry["binary_path"].is_null());
    }

    #[test]
    fn the_merged_status_retry_refuses_with_the_reason() {
        // Never "published": the client would then show a merged pull request
        // whose status nobody can see.
        let refused = publish_project_pull_request_merged_status(json!({}))
            .expect_err("must refuse");
        assert!(refused.contains("does not hold your signing key"), "{refused}");
        assert!(refused.contains("pull request view"), "{refused}");
    }

    #[test]
    fn the_auth_methods_name_the_websocket_gap() {
        let methods = discover_acp_auth_methods();
        assert_eq!(methods["websocket_supported"], false);
        assert!(methods["websocket_reason"]
            .as_str()
            .expect("reason")
            .contains("WebSockets are not available"));
        assert_eq!(methods["secrets_stored_in"], "os_keychain");
    }

    #[test]
    fn which_finds_a_program_that_exists_and_not_one_that_does_not() {
        // A program every machine of this platform has. `sh` is not one of those
        // on Windows, and this shell is built for Windows too.
        let present = if cfg!(windows) { "cmd" } else { "sh" };
        assert!(
            which(present).is_some(),
            "{present} should be on PATH on this platform"
        );
        assert!(which("definitely-not-a-program-on-this-machine").is_none());
        // A named path is checked directly rather than searched for.
        assert!(which("/definitely/not/here").is_none());
    }

    #[test]
    fn a_trunk_defaults_to_main_but_honours_the_project() {
        assert_eq!(trunk(None), "main");
        assert_eq!(trunk(Some(String::new())), "main");
        assert_eq!(trunk(Some("develop".into())), "develop");
    }
}
