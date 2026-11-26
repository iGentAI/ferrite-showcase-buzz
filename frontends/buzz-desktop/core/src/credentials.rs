//! Credentials: the store behind git pushes and harness secrets.
//!
//! Two things live here, and keeping them together is the point: a git
//! credential and a harness bearer token are the same KIND of secret — held for
//! the user, never shown, never logged — so they share one store and one
//! redaction discipline.
//!
//! The store is a TRAIT because the real one is the OS keychain, which cannot be
//! exercised in a test on a headless machine: there is no login session to
//! unlock. So the logic is written against the trait, tested against an
//! in-memory double, and the keychain implementation lives in the Tauri crate
//! where the platform actually exists.

use std::collections::BTreeMap;
use std::sync::Mutex;

use crate::{Error, Result};

/// The service name every entry is filed under, so an operator can see at a
/// glance which keychain items belong to this app.
pub const SERVICE: &str = "buzz-desktop";

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Key {
    /// Namespace within the service: `git`, `harness`, `agent`.
    pub kind: String,
    pub subject: String,
}

impl Key {
    #[must_use]
    pub fn git(host: &str) -> Self {
        Self {
            kind: "git".to_string(),
            subject: host.to_ascii_lowercase(),
        }
    }

    #[must_use]
    pub fn harness(id: &str) -> Self {
        Self {
            kind: "harness".to_string(),
            subject: id.to_string(),
        }
    }

    /// One agent's own signing key.
    ///
    /// A DIFFERENT namespace from `harness`, which holds the token used to reach
    /// a remote endpoint: this is the key an agent signs its own messages with,
    /// and conflating the two would mean a harness's endpoint credential and its
    /// workspace identity shared one slot.
    #[must_use]
    pub fn agent(id: &str) -> Self {
        Self {
            kind: "agent".to_string(),
            subject: id.to_string(),
        }
    }

    #[must_use]
    pub fn account(&self) -> String {
        format!("{}:{}", self.kind, self.subject)
    }
}

pub trait Store: Send + Sync {
    fn get(&self, key: &Key) -> Result<Option<String>>;
    fn set(&self, key: &Key, secret: &str) -> Result<()>;
    fn delete(&self, key: &Key) -> Result<()>;
    /// Which subjects of `kind` have a secret. NAMES ONLY — never values, so a
    /// settings screen can list what is configured without reading it.
    fn subjects(&self, kind: &str) -> Result<Vec<String>>;
}

/// An in-memory store, for tests and for a session where no keychain exists.
///
/// It says so about itself ([`Self::is_durable`]) rather than pretending: a
/// credential that silently vanishes on restart is worse than one the user was
/// told is temporary.
#[derive(Debug, Default)]
pub struct MemoryStore {
    entries: Mutex<BTreeMap<String, String>>,
}

impl MemoryStore {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    #[must_use]
    pub const fn is_durable(&self) -> bool {
        false
    }
}

impl Store for MemoryStore {
    fn get(&self, key: &Key) -> Result<Option<String>> {
        Ok(self
            .entries
            .lock()
            .map_err(|_| Error::Internal("credential store is poisoned".into()))?
            .get(&key.account())
            .cloned())
    }

    fn set(&self, key: &Key, secret: &str) -> Result<()> {
        if secret.is_empty() {
            return Err(Error::Invalid("an empty secret is not a credential".into()));
        }
        self.entries
            .lock()
            .map_err(|_| Error::Internal("credential store is poisoned".into()))?
            .insert(key.account(), secret.to_string());
        Ok(())
    }

    fn delete(&self, key: &Key) -> Result<()> {
        self.entries
            .lock()
            .map_err(|_| Error::Internal("credential store is poisoned".into()))?
            .remove(&key.account());
        Ok(())
    }

    fn subjects(&self, kind: &str) -> Result<Vec<String>> {
        let prefix = format!("{kind}:");
        Ok(self
            .entries
            .lock()
            .map_err(|_| Error::Internal("credential store is poisoned".into()))?
            .keys()
            .filter_map(|account| account.strip_prefix(&prefix).map(str::to_string))
            .collect())
    }
}

/// Whether a credential exists for this host — the fact [`crate::project::sync_gate`]
/// needs, without reading the secret.
pub fn has_git_credential(store: &dyn Store, host: &str) -> bool {
    store
        .get(&Key::git(host))
        .ok()
        .flatten()
        .is_some_and(|secret| !secret.is_empty())
}

/// Whether GIT ITSELF can supply a credential for this REMOTE.
///
/// This exists because the shell's own store is not the only place a credential
/// lives: a developer's machine usually already has one, in
/// `credential.helper=osxkeychain`, `manager`, `store`, or a helper of their own.
/// Treating "the shell holds no token" as "no credential exists" would refuse a
/// push that plain `git push` performs happily — the app would be strictly less
/// capable than the terminal beside it, for no reason.
///
/// The request carries the WHOLE clone URL as `url=`, not protocol and host, and
/// that is not a shortcut: git decomposes the URL itself, exactly as it does for
/// a push, so a PATH-SCOPED credential is found. With `credential.useHttpPath`
/// set — or a helper that stores per-repository entries, which several do — a
/// protocol-and-host query answers "none" for a credential git locates at once.
///
/// `git credential fill` is the question git answers about itself: it consults
/// the configured helpers and, with `GIT_TERMINAL_PROMPT=0`, answers without
/// prompting. A reply carrying a non-empty password means a credential exists.
/// Nothing is read INTO this app — the value is looked at only to see that it is
/// there, and never returned or logged.
pub fn git_can_supply(git: &crate::git::Git, clone_url: &str) -> bool {
    // The credential protocol's request shape: key=value lines, blank line to end.
    let request = format!("url={clone_url}\n\n");
    let Ok(out) = git.output_with_stdin(None, &["credential", "fill"], Some(&request)) else {
        return false;
    };
    if !out.ok {
        return false;
    }
    parse_credential_request(&out.stdout)
        .get("password")
        .is_some_and(|password| !password.is_empty())
}

/// Whether this remote authenticates with a PASSWORD-shaped credential at all.
///
/// Only the HTTP family does, and the inversion is deliberate: an unrecognised
/// scheme is treated as *not* password-based, so the gate steps aside rather than
/// refusing a transport it does not understand.
///
/// * **SSH** — `ssh://`, and git's two compound spellings `git+ssh://` and
///   `ssh+git://`, plus the scp-style `git@host:owner/repo` with no scheme at
///   all. Authentication there is a key or an agent, which the credential-helper
///   protocol cannot describe, so asking it answers "no credential" for a remote
///   that works perfectly.
/// * **A local path or `file://`** — nothing authenticates.
/// * **`git://`, `rsync://`, anything newer** — not a password either.
///
/// Schemes are compared case-INSENSITIVELY, because URL schemes are.
#[must_use]
pub fn uses_password_credential(clone_url: &str) -> bool {
    match clone_url.trim().split_once("://") {
        Some((scheme, _)) => matches!(
            scheme.to_ascii_lowercase().as_str(),
            "http" | "https" | "ftp" | "ftps"
        ),
        // No scheme: either scp-style SSH (`[user@]host:path`) or a local path.
        // Neither uses a password.
        None => false,
    }
}

/// Whether a credential for this remote is available to git AT ALL — the shell's
/// own, or the user's.
///
/// **A transport that does not use a password is exempt, and that is the point
/// rather than a gap.** The credential-helper protocol describes passwords; it
/// does not model an SSH key, an agent, or a local path. What authenticates an
/// SSH remote is a key this app cannot inspect and has no business gating on — so
/// the gate steps aside, and if the key really is missing, git's own error says
/// so in words the user can act on.
///
/// For the HTTP family the store is checked FIRST: it is a memory lookup, and a
/// credential this app was given is the one it should prefer. Only when there is
/// none does it ask git, which costs a subprocess.
pub fn credential_available(
    store: &dyn Store,
    git: &crate::git::Git,
    clone_url: &str,
    host: &str,
) -> bool {
    if !uses_password_credential(clone_url) {
        return true;
    }
    has_git_credential(store, host) || git_can_supply(git, clone_url)
}

/// Parse a git credential request (`key=value` lines terminated by a blank line).
#[must_use]
pub fn parse_credential_request(input: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    for line in input.lines() {
        if line.is_empty() {
            break;
        }
        if let Some((key, value)) = line.split_once('=') {
            out.insert(key.trim().to_string(), value.trim().to_string());
        }
    }
    out
}

/// The username and password one stored secret encodes.
///
/// A bare token is used as the PASSWORD with a placeholder username, which is
/// what both GitHub and Ferrite's RCS expect; a stored `user:token` form
/// carries its own username.
#[must_use]
pub fn split_secret(secret: &str) -> (&str, &str) {
    match secret.split_once(':') {
        Some((user, token)) if !user.is_empty() && !token.is_empty() => (user, token),
        _ => ("x-access-token", secret),
    }
}

/// One host's stored credential, prepared for git's own configuration.
///
/// `config_key` is `credential.<scheme>://<host[:port]>.helper` — a URL-SCOPED
/// helper, which git consults only for requests whose context matches that
/// origin. The scoping is the security property, not a nicety: an unscoped
/// helper answers EVERY host, so a merge whose source named a hostile remote
/// would be handed the target's token.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScopedCredential {
    pub config_key: String,
    pub username: String,
    pub password: String,
}

/// The scoped credential for one clone URL, or `None` when the transport does
/// not authenticate with a password at all (SSH, `file://`, a local path) —
/// configuring one there would add a helper git never consults.
///
/// A stored secret is never offered over a plaintext transport (`http://`,
/// `ftp://`) except to a loopback host. The keychain entry is looked up by
/// hostname alone while the helper is scoped to the remote's own scheme, so a
/// relay-authored `http://<host>/…` remote (a pull request's source, say) would
/// otherwise have git send that host's token in the clear to whoever answers on
/// the path. A loopback development fleet over `http://` keeps working.
#[must_use]
pub fn scoped_credential(clone_url: &str, secret: &str) -> Option<ScopedCredential> {
    if secret.is_empty() || !uses_password_credential(clone_url) {
        return None;
    }
    let (scheme, rest) = clone_url.trim().split_once("://")?;
    let authority = rest.split(['/', '?', '#']).next()?;
    let host_port = authority.rsplit('@').next()?;
    if host_port.is_empty() {
        return None;
    }
    let plaintext = matches!(scheme.to_ascii_lowercase().as_str(), "http" | "ftp");
    if plaintext && !is_loopback_authority(host_port) {
        return None;
    }
    let (username, password) = split_secret(secret);
    Some(ScopedCredential {
        config_key: format!(
            "credential.{}://{}.helper",
            scheme.to_ascii_lowercase(),
            host_port.to_ascii_lowercase()
        ),
        username: username.to_string(),
        password: password.to_string(),
    })
}

/// Whether `host[:port]` names this machine: `localhost`, `127.0.0.0/8` or `[::1]`.
fn is_loopback_authority(host_port: &str) -> bool {
    let host = if let Some(rest) = host_port.strip_prefix('[') {
        rest.split(']').next().unwrap_or("")
    } else {
        host_port.rsplit_once(':').map_or(host_port, |(host, _)| host)
    };
    host.eq_ignore_ascii_case("localhost")
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|ip| ip.is_loopback())
}

/// The environment that hands `credentials` to git for ONE invocation.
///
/// Two decisions carry the security. The secrets travel in ENVIRONMENT
/// variables the helper reads back — never in the helper text itself, which is
/// command line a process listing could show. And every helper is URL-scoped
/// (see [`ScopedCredential`]) and ADDED beside the user's own configuration
/// rather than replacing it, so a credential their own helper already serves
/// keeps working exactly as it did.
///
/// The `!f() { …; }; f` form is a shell helper, which git runs through its own
/// `sh` — present wherever git is, Git for Windows included (the same mechanism
/// this module's tests configure helpers with).
#[must_use]
pub fn credential_env(credentials: &[ScopedCredential]) -> Vec<(String, String)> {
    if credentials.is_empty() {
        return Vec::new();
    }
    let mut env = Vec::with_capacity(credentials.len() * 4 + 1);
    env.push((
        "GIT_CONFIG_COUNT".to_string(),
        credentials.len().to_string(),
    ));
    for (index, credential) in credentials.iter().enumerate() {
        let user_var = format!("BUZZ_GIT_USERNAME_{index}");
        let pass_var = format!("BUZZ_GIT_PASSWORD_{index}");
        env.push((
            format!("GIT_CONFIG_KEY_{index}"),
            credential.config_key.clone(),
        ));
        env.push((
            format!("GIT_CONFIG_VALUE_{index}"),
            format!("!f() {{ echo \"username=${user_var}\"; echo \"password=${pass_var}\"; }}; f"),
        ));
        env.push((user_var, credential.username.clone()));
        env.push((pass_var, credential.password.clone()));
    }
    env
}

/// Redact anything secret-shaped out of a string bound for a log.
///
/// Not a substitute for not logging secrets — it is the backstop for the case
/// where a git error message quotes the URL it was given, which is how tokens
/// end up in log files in practice.
#[must_use]
pub fn redact(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    // `scheme://userinfo@host` — the userinfo is the part worth hiding.
    while let Some(at) = rest.find("://") {
        let (before, after) = rest.split_at(at + 3);
        out.push_str(before);
        match after.find('@') {
            Some(user_end) => {
                let boundary = after[..user_end].find(['/', ' ']).unwrap_or(user_end);
                if boundary == user_end {
                    out.push_str("<redacted>@");
                    rest = &after[user_end + 1..];
                } else {
                    out.push_str(&after[..=boundary]);
                    rest = &after[boundary + 1..];
                }
            }
            None => {
                out.push_str(after);
                return out;
            }
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod plaintext_tests {
    use super::scoped_credential;

    #[test]
    fn a_stored_token_is_never_offered_to_a_plaintext_remote() {
        assert!(scoped_credential("http://github.com/o/r.git", "ghp_dummy").is_none());
        assert!(scoped_credential("HTTP://github.com:80/o/r.git", "ghp_dummy").is_none());
        assert!(scoped_credential("ftp://example.com/r.git", "x").is_none());
        let https = scoped_credential("https://github.com/o/r.git", "ghp_dummy").expect("https");
        assert_eq!(https.config_key, "credential.https://github.com.helper");
        // A loopback development fleet over plain http keeps its credential.
        assert!(scoped_credential("http://127.0.0.1:8080/x.git", "t").is_some());
        assert!(scoped_credential("http://localhost:7000/x.git", "t").is_some());
        assert!(scoped_credential("http://[::1]:7000/x.git", "t").is_some());
        assert!(scoped_credential("http://127.0.0.1.evil.test/x.git", "t").is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_namespaced_and_host_case_insensitive() {
        assert_eq!(Key::git("GitHub.com").account(), "git:github.com");
        assert_eq!(Key::harness("claude").account(), "harness:claude");
        assert_eq!(Key::agent("claude").account(), "agent:claude");
        assert_ne!(Key::git("a").account(), Key::harness("a").account());
        // A harness's ENDPOINT token and its own signing key are different
        // secrets and must never share a slot.
        assert_ne!(Key::harness("claude").account(), Key::agent("claude").account());
    }

    #[test]
    fn a_store_round_trips_and_lists_names_only() {
        let store = MemoryStore::new();
        store.set(&Key::git("github.com"), "ghp_x").expect("set");
        store.set(&Key::harness("claude"), "sk-y").expect("set");
        assert_eq!(
            store.get(&Key::git("github.com")).expect("get").as_deref(),
            Some("ghp_x")
        );
        assert_eq!(store.subjects("git").expect("list"), vec!["github.com"]);
        assert_eq!(store.subjects("harness").expect("list"), vec!["claude"]);
        store.delete(&Key::git("github.com")).expect("delete");
        assert!(store.get(&Key::git("github.com")).expect("get").is_none());
        // An empty secret is not a credential.
        assert!(store.set(&Key::git("github.com"), "").is_err());
        assert!(!store.is_durable(), "and it says so about itself");
    }

    #[test]
    fn presence_is_answerable_without_reading_the_secret() {
        let store = MemoryStore::new();
        assert!(!has_git_credential(&store, "github.com"));
        store.set(&Key::git("github.com"), "ghp_x").expect("set");
        assert!(has_git_credential(&store, "github.com"));
        assert!(has_git_credential(&store, "GITHUB.COM"), "host case does not matter");
    }

    #[test]
    fn a_stored_credential_answers_without_asking_git() {
        // The store short-circuits, which matters twice: it is a memory lookup
        // rather than a subprocess, and a credential this app was GIVEN is the
        // one it should prefer. Proven by pointing at a git that cannot run: if
        // it were consulted, this would answer false.
        let store = MemoryStore::new();
        store.set(&Key::git("github.com"), "ghp_x").expect("set");
        let broken = crate::git::Git::with_program("definitely-not-a-real-git");
        assert!(credential_available(
            &store,
            &broken,
            "https://github.com/acme/widgets.git",
            "github.com"
        ));
    }

    /// A git carrying a credential helper, configured through the ENVIRONMENT so
    /// the developer's own git config is neither read nor written.
    ///
    /// `GIT_CONFIG_COUNT`/`_KEY_n`/`_VALUE_n` is git's own mechanism for exactly
    /// this, and a leading `!` is how git spells "this helper is a shell
    /// command" — so the whole thing is deterministic and leaves no trace.
    /// Git whose ONLY credential helper is `helper`. The developer's own
    /// configuration is shut out: the global file, the system file, and the
    /// configuration a vendor build reads beside its own installation (Apple's
    /// git sets `credential.helper=osxkeychain` there, and GIT_CONFIG_SYSTEM does
    /// not replace it; GIT_CONFIG_NOSYSTEM drops both). Otherwise a helper of
    /// the developer's that holds a credential for github.com answers queries
    /// these tests expect to go unanswered.
    fn git_with_helper(helper: &str) -> crate::git::Git {
        let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
        crate::git::Git::new()
            .with_env("GIT_CONFIG_GLOBAL", null_device)
            .with_env("GIT_CONFIG_SYSTEM", null_device)
            .with_env("GIT_CONFIG_NOSYSTEM", "1")
            .with_env("GIT_CONFIG_COUNT", "2")
            .with_env("GIT_CONFIG_KEY_0", "credential.helper")
            .with_env("GIT_CONFIG_VALUE_0", helper)
            // Path scoping ON, so a query has to carry the path to match.
            .with_env("GIT_CONFIG_KEY_1", "credential.useHttpPath")
            .with_env("GIT_CONFIG_VALUE_1", "true")
    }

    #[test]
    fn a_credential_the_users_own_helper_holds_is_found() {
        // The defect this closes: refusing a push because the SHELL has no token,
        // on a machine where `git push` works perfectly through the user's own
        // helper.
        //
        // The helper answers with a username AND a password, which is what a real
        // one does — measured: with a password alone git goes on to ask for the
        // username, and with prompting disabled the whole `fill` fails (exit 128,
        // "could not read Username"). So a password-only helper is not a working
        // credential non-interactively, and answering `false` for it is correct
        // rather than a limitation.
        let store = MemoryStore::new();
        let git = git_with_helper("!f() { echo username=x; echo password=tok-from-helper; }; f");
        assert!(
            git_can_supply(&git, "https://github.com/acme/widgets.git"),
            "a configured helper's credential must be found"
        );
        assert!(credential_available(
            &store,
            &git,
            "https://github.com/acme/widgets.git",
            "github.com"
        ));
    }

    #[test]
    fn the_query_carries_the_repositorys_path() {
        // With `credential.useHttpPath` set, a helper may hold per-repository
        // entries — so a protocol-and-host query would answer "none" for a
        // credential git finds at once. This helper answers ONLY when the path
        // reached it, which is what proves the whole URL was handed over.
        let path_only = git_with_helper(
            "!f() { if grep -q '^path=acme/widgets.git$'; then echo username=x; \
             echo password=tok; fi; }; f",
        );
        assert!(
            git_can_supply(&path_only, "https://github.com/acme/widgets.git"),
            "the credential query must include the repository path"
        );
        // ...and a DIFFERENT repository on the same host is not a match, so the
        // assertion above cannot pass by accident.
        assert!(!git_can_supply(
            &path_only,
            "https://github.com/acme/other.git"
        ));
    }

    #[test]
    fn a_transport_with_no_password_is_never_gated_on_one() {
        // The credential-helper protocol describes passwords; an SSH key, an
        // agent and a local path are none of those, so asking about them answers
        // "no credential" for remotes that work. Every spelling git accepts is
        // covered, including its compound schemes and mixed case.
        let store = MemoryStore::new();
        let broken = crate::git::Git::with_program("definitely-not-a-real-git");
        for exempt in [
            "git@github.com:acme/widgets.git",
            "ssh://git@git.example/acct/app.git",
            "git+ssh://git@git.example/acct/app.git",
            "ssh+git://git@git.example/acct/app.git",
            "SSH://git@git.example/acct/app.git",
            "GIT+SSH://git@git.example/acct/app.git",
            "file:///home/ana/repo.git",
            "/home/ana/repo.git",
            // Not a password transport either, and an unknown scheme must fail
            // OPEN rather than refuse a push this shell does not understand.
            "git://git.example/acct/app.git",
        ] {
            assert!(!uses_password_credential(exempt), "{exempt}");
            assert!(
                credential_available(&store, &broken, exempt, "git.example"),
                "{exempt} must not be blocked on a password it never uses"
            );
        }
        // The HTTP family IS password-based, whatever its case, and is checked.
        for checked in [
            "https://github.com/acme/widgets.git",
            "HTTPS://github.com/acme/widgets.git",
            "http://127.0.0.1:8080/x.git",
        ] {
            assert!(uses_password_credential(checked), "{checked}");
        }
    }

    #[test]
    fn git_is_asked_when_the_shell_holds_nothing() {
        // A host nobody has a credential for answers false, which is the negative
        // assertion this environment can make honestly.
        let store = MemoryStore::new();
        let git = crate::git::Git::new();
        assert!(!credential_available(
            &store,
            &git,
            "https://host-nobody-has-a-credential-for.invalid/x.git",
            "host-nobody-has-a-credential-for.invalid"
        ));
        // An unusable git is not a credential either — and must not panic.
        let broken = crate::git::Git::with_program("definitely-not-a-real-git");
        assert!(!credential_available(
            &store,
            &broken,
            "https://github.com/acme/widgets.git",
            "github.com"
        ));
    }

    #[test]
    fn a_credential_request_parses_gits_line_protocol() {
        let fields = parse_credential_request(
            "protocol=https\nhost=github.com\npath=acme/widgets.git\n\nignored=yes\n",
        );
        assert_eq!(fields.get("protocol").map(String::as_str), Some("https"));
        assert_eq!(fields.get("host").map(String::as_str), Some("github.com"));
        assert!(
            !fields.contains_key("ignored"),
            "the blank line ends the request"
        );
    }

    #[test]
    fn a_secret_splits_into_the_credential_git_needs() {
        assert_eq!(split_secret("ghp_token"), ("x-access-token", "ghp_token"));
        assert_eq!(split_secret("ana:tok"), ("ana", "tok"));
        // A degenerate colon form is a bare token, not an empty credential half.
        assert_eq!(split_secret(":tok"), ("x-access-token", ":tok"));
        assert_eq!(split_secret("ana:"), ("x-access-token", "ana:"));
    }

    #[test]
    fn a_scoped_credential_names_one_origin_and_only_password_transports() {
        let cred = scoped_credential("https://github.com/acme/widgets.git", "ana:tok")
            .expect("an https remote takes a password");
        assert_eq!(cred.config_key, "credential.https://github.com.helper");
        assert_eq!(cred.username, "ana");
        assert_eq!(cred.password, "tok");

        // The port is part of the origin, and userinfo in the URL is not.
        let with_port =
            scoped_credential("https://Git.Example:8443/a/b.git", "tok").expect("with port");
        assert_eq!(
            with_port.config_key,
            "credential.https://git.example:8443.helper"
        );
        let userinfo =
            scoped_credential("https://x@git.example/a.git", "tok").expect("userinfo");
        assert_eq!(userinfo.config_key, "credential.https://git.example.helper");

        // A transport with no password gets no helper at all.
        for exempt in [
            "git@github.com:acme/widgets.git",
            "ssh://git@git.example/a.git",
            "/home/ana/repo.git",
            "file:///home/ana/repo.git",
        ] {
            assert!(scoped_credential(exempt, "tok").is_none(), "{exempt}");
        }
        assert!(scoped_credential("https://github.com/a.git", "").is_none());
    }

    #[test]
    fn the_credential_env_keeps_secrets_out_of_the_helper_text() {
        let credentials = [
            scoped_credential("https://github.com/a.git", "ghp_secret").expect("one"),
            scoped_credential("https://git.example/b.git", "ana:tok").expect("two"),
        ];
        let env = credential_env(&credentials);
        let value = |name: &str| {
            env.iter()
                .find(|(key, _)| key == name)
                .map(|(_, value)| value.as_str())
                .unwrap_or_else(|| panic!("{name} missing"))
        };
        assert_eq!(value("GIT_CONFIG_COUNT"), "2");
        assert_eq!(
            value("GIT_CONFIG_KEY_0"),
            "credential.https://github.com.helper"
        );
        assert_eq!(
            value("GIT_CONFIG_KEY_1"),
            "credential.https://git.example.helper"
        );
        assert_eq!(value("BUZZ_GIT_PASSWORD_0"), "ghp_secret");
        assert_eq!(value("BUZZ_GIT_USERNAME_1"), "ana");
        // The helper STRING carries variable references only: the command line
        // is what a process listing shows, and a secret must never be on it.
        for (key, value) in &env {
            if key.starts_with("GIT_CONFIG_VALUE_") {
                assert!(
                    !value.contains("ghp_secret") && !value.contains("tok"),
                    "{value}"
                );
            }
        }
        assert!(credential_env(&[]).is_empty(), "no credentials, no config");
    }

    #[test]
    fn git_consumes_a_stored_credential_for_its_own_host_and_no_other() {
        // The wiring proven with REAL git: the env this module builds is enough
        // for `git credential fill` to answer — and the URL scoping means the
        // same environment offers NOTHING for a different host, which is what
        // stops a stored token leaking to a hostile remote named beside it.
        let credentials = [
            scoped_credential("https://github.com/acme/widgets.git", "ana:tok").expect("cred"),
        ];
        let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let mut git = crate::git::Git::new()
            // Ambient configuration is isolated so this test says the same
            // thing on every machine, whatever helpers the developer has. That
            // includes the configuration Apple's git reads beside its own
            // installation (credential.helper=osxkeychain), which only
            // GIT_CONFIG_NOSYSTEM shuts out.
            .with_env("GIT_CONFIG_GLOBAL", null_device)
            .with_env("GIT_CONFIG_SYSTEM", null_device)
            .with_env("GIT_CONFIG_NOSYSTEM", "1");
        for (key, value) in credential_env(&credentials) {
            git = git.with_env(&key, &value);
        }

        let ours = git
            .output_with_stdin(
                None,
                &["credential", "fill"],
                Some("url=https://github.com/acme/widgets.git\n\n"),
            )
            .expect("git runs");
        assert!(ours.ok, "{}", ours.stderr);
        let fields = parse_credential_request(&ours.stdout);
        assert_eq!(fields.get("username").map(String::as_str), Some("ana"));
        assert_eq!(fields.get("password").map(String::as_str), Some("tok"));

        // Another host gets nothing from this environment: with prompts
        // disabled and no helper in scope, the fill itself fails.
        let theirs = git
            .output_with_stdin(
                None,
                &["credential", "fill"],
                Some("url=https://other.example/x.git\n\n"),
            )
            .expect("git runs");
        assert!(
            !theirs.ok,
            "a credential scoped to github.com must not answer for other.example: {}",
            theirs.stdout
        );
    }

    #[test]
    fn redaction_removes_userinfo_from_anything_bound_for_a_log() {
        // The real-world path: git quotes the URL it was handed, tokens and all.
        assert_eq!(
            redact("fatal: could not read https://x:ghp_secret@github.com/acme/w.git"),
            "fatal: could not read https://<redacted>@github.com/acme/w.git"
        );
        // Nothing to redact is left exactly alone.
        assert_eq!(
            redact("fatal: repository https://github.com/acme/w.git not found"),
            "fatal: repository https://github.com/acme/w.git not found"
        );
        assert!(!redact("https://user:pass@host/path").contains("pass"));
        // A URL with no authority section must not eat the rest of the message.
        assert_eq!(redact("see https://example.com/ for help"), "see https://example.com/ for help");
    }
}
