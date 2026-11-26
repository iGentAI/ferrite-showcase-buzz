//! The repositories directory, and what a safe path under it is.
//!
//! Every git command in this shell is handed a project identifier that came
//! from an EVENT — a `d` tag on a NIP-34 repository announcement, which anybody
//! in the workspace can publish. So the identifier decides a directory name on
//! the user's disk, and the only thing standing between a hostile `d` tag and
//! an arbitrary path is this module. `../../.ssh` is a `d` tag somebody could
//! publish; `repo_dir` is why it does not become a checkout location.

use std::path::{Component, Path, PathBuf};

use crate::{Error, Result};

/// Longest accepted project identifier. Generous for a slug, bounded because it
/// becomes a filesystem name.
pub const MAX_SEGMENT: usize = 100;

/// The default place clones live: `~/.buzz/repos`.
///
/// Under the app's own directory rather than somewhere like `~/src`, because
/// these are checkouts the app manages — mixing them with a developer's own
/// work would make "clean up" an ambiguous operation.
#[must_use]
pub fn default_repos_dir(home: &Path) -> PathBuf {
    home.join(".buzz").join("repos")
}

/// Is this a safe single path component?
///
/// Lowercase-insensitive ASCII alphanumerics plus `.`, `-` and `_`, non-empty,
/// bounded, and never `.` or `..`. Deliberately narrower than the filesystem
/// allows: a name that cannot contain a separator, a NUL, a drive letter or a
/// leading dash cannot be smuggled into a path or mistaken for an option by a
/// program this shell invokes.
#[must_use]
pub fn is_safe_segment(segment: &str) -> bool {
    if segment.is_empty() || segment.len() > MAX_SEGMENT {
        return false;
    }
    if segment == "." || segment == ".." {
        return false;
    }
    // A leading dash would be read as an option by git and by the shell.
    if segment.starts_with('-') {
        return false;
    }
    segment
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-' || b == b'_')
}

/// Validate a configured repositories directory.
///
/// Absolute, with no `..` component and not the filesystem root. The root check
/// is not paranoia: a mis-set preference of `/` would make "the repositories
/// directory" mean the whole disk, and every listing and cleanup below it would
/// act at that scope.
pub fn validate_repos_dir(path: &Path) -> Result<PathBuf> {
    if path.as_os_str().is_empty() {
        return Err(Error::Invalid("the repositories directory is empty".into()));
    }
    if !path.is_absolute() {
        return Err(Error::Invalid(format!(
            "the repositories directory must be an absolute path, not {}",
            path.display()
        )));
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir))
    {
        return Err(Error::Invalid(
            "the repositories directory must not contain '..'".into(),
        ));
    }
    // A ROOT is not a repositories directory: a mis-set preference of `/` (or
    // `C:\`) would make "the repositories directory" mean the whole disk, and
    // every listing and cleanup below it would act at that scope. Counted over
    // NORMAL components rather than all of them, because a Windows volume root
    // carries a Prefix and a RootDir and would otherwise pass a bare count.
    if !path
        .components()
        .any(|component| matches!(component, Component::Normal(_)))
    {
        return Err(Error::Invalid(
            "the repositories directory must not be a filesystem root".into(),
        ));
    }
    Ok(path.to_path_buf())
}

/// Assert that `candidate` really is inside `repos_dir` ON DISK, not merely by
/// its spelling.
///
/// Lexical validation ([`is_safe_segment`]) stops `../../.ssh`, and it is not
/// enough on its own: a directory named `my-app` can be a SYMLINK to `/etc` or
/// to somebody's home, and every git command run "inside the repositories
/// directory" would then act outside it. So both paths are resolved through the
/// filesystem — which follows symlinks — and compared.
///
/// A path that does not exist yet (the clone target, before cloning; a
/// repositories directory on first run) cannot be canonicalized. It is resolved
/// through its NEAREST EXISTING ANCESTOR instead, with the missing components
/// appended: that accounts for a symlink anywhere above the missing part, which
/// is the case a bare "does not exist, so nothing to check" would have waved
/// through. Any error that is not "not found" — a permission failure, an I/O
/// fault — is propagated rather than read as absence.
pub fn contained(repos_dir: &Path, candidate: &Path) -> Result<PathBuf> {
    let root = validate_repos_dir(repos_dir)?;
    let resolved_root = resolve_through_existing(&root)?;
    let resolved = resolve_through_existing(candidate)?;
    if !resolved.starts_with(&resolved_root) {
        return Err(Error::Refused(format!(
            "{} resolves to {}, which is outside the repositories directory ({}); \
             a path that leaves the managed root — through a symlink or otherwise \
             — is refused rather than followed",
            candidate.display(),
            resolved.display(),
            resolved_root.display()
        )));
    }
    Ok(resolved)
}

/// Resolve `path` as far as the filesystem can, then re-append what does not
/// exist yet.
///
/// The existing prefix is canonicalized, so every symlink in it is followed; the
/// missing tail is appended literally. A `..` in the tail is refused rather than
/// resolved, because appending it to a canonical prefix would silently climb
/// back out of it.
fn resolve_through_existing(path: &Path) -> Result<PathBuf> {
    let mut missing: Vec<std::ffi::OsString> = Vec::new();
    let mut cursor = path.to_path_buf();
    loop {
        match cursor.symlink_metadata() {
            Ok(_) => {
                let canonical = cursor.canonicalize().map_err(|e| {
                    // An existing entry that cannot be canonicalized is a broken
                    // symlink or an unreadable directory. Refused, not assumed.
                    Error::Refused(format!(
                        "{} cannot be resolved ({e}); it may be a broken symlink",
                        cursor.display()
                    ))
                })?;
                let mut out = canonical;
                for name in missing.iter().rev() {
                    out.push(name);
                }
                return Ok(out);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let Some(name) = cursor.file_name().map(std::ffi::OsStr::to_os_string) else {
                    return Err(Error::Invalid(format!(
                        "{} has no existing ancestor to resolve against",
                        path.display()
                    )));
                };
                if name == ".." {
                    return Err(Error::Invalid(format!(
                        "{} contains '..' above the part that exists",
                        path.display()
                    )));
                }
                missing.push(name);
                let Some(parent) = cursor.parent().map(Path::to_path_buf) else {
                    return Err(Error::Invalid(format!(
                        "{} has no existing ancestor to resolve against",
                        path.display()
                    )));
                };
                cursor = parent;
            }
            // NOT absence: a permission failure or an I/O fault must not read as
            // "nothing here, carry on".
            Err(e) => {
                return Err(Error::Internal(format!(
                    "could not inspect {}: {e}",
                    cursor.display()
                )))
            }
        }
    }
}

/// Where one project's checkout lives.
///
/// The identifier is validated as a single safe component, so a `d` tag of
/// `../../.ssh` or `/etc` is refused rather than resolved.
pub fn repo_dir(repos_dir: &Path, project_dtag: &str) -> Result<PathBuf> {
    let repos = validate_repos_dir(repos_dir)?;
    if !is_safe_segment(project_dtag) {
        return Err(Error::Invalid(format!(
            "{project_dtag:?} is not a usable repository name: it must be 1–{MAX_SEGMENT} \
             characters of letters, digits, '.', '-' or '_', and must not start with '-'"
        )));
    }
    contained(&repos, &repos.join(project_dtag))
}

/// The repository directories directly under `repos_dir`, newest name order.
///
/// A directory is a repository when it holds a `.git` entry — checked rather
/// than assumed, so an unrelated folder a user dropped in there is not reported
/// as a checkout and then operated on.
pub fn local_repositories(repos_dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let repos = validate_repos_dir(repos_dir)?;
    let entries = match std::fs::read_dir(&repos) {
        Ok(entries) => entries,
        // Nothing cloned yet is an empty list, not an error: the caller renders
        // "no repositories" and offering an error instead reads as a fault.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => {
            return Err(Error::Internal(format!(
                "could not read {}: {e}",
                repos.display()
            )))
        }
    };
    let mut out = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        // A SYMLINK is skipped rather than followed: `repos/project ->
        // /somebody/else` would otherwise be reported as a managed checkout and
        // then operated on, which is the containment hole `contained` closes for
        // named lookups. Reported by `file_type`, which does not follow links.
        match entry.file_type() {
            Ok(kind) if kind.is_symlink() => continue,
            Ok(kind) if !kind.is_dir() => continue,
            Ok(_) => {}
            Err(_) => continue,
        }
        if !path.join(".git").exists() {
            continue;
        }
        // Belt and braces: even a non-symlink entry is confirmed to resolve
        // inside the root before it is reported (a bind mount, say).
        if contained(&repos, &path).is_err() {
            continue;
        }
        if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
            out.push((name.to_string(), path));
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_default_directory_is_under_the_apps_own_home() {
        let dir = default_repos_dir(Path::new("/home/ana"));
        assert_eq!(dir, PathBuf::from("/home/ana/.buzz/repos"));
    }

    #[test]
    fn safe_segments_are_narrow_on_purpose() {
        for good in ["my-app", "my_app", "app.v2", "a", "App123"] {
            assert!(is_safe_segment(good), "{good} should be safe");
        }
        for bad in [
            "",
            ".",
            "..",
            "../etc",
            "a/b",
            "a\\b",
            "-rf",
            "a b",
            "caf\u{e9}",
            "a\0b",
            "C:app",
            &"x".repeat(MAX_SEGMENT + 1),
        ] {
            assert!(!is_safe_segment(bad), "{bad:?} must not be safe");
        }
    }

    #[test]
    fn a_hostile_project_identifier_cannot_escape_the_repositories_directory() {
        // The threat this module exists for: a `d` tag is publishable by anybody
        // in the workspace, and it names a directory on the user's disk.
        let repos = Path::new("/home/ana/.buzz/repos");
        for hostile in [
            "../../.ssh",
            "..",
            "/etc/passwd",
            "a/../../b",
            "a/b",
            ".",
        ] {
            let refused = repo_dir(repos, hostile);
            assert!(refused.is_err(), "{hostile:?} must be refused");
        }
        // And the legitimate case resolves exactly where it should: below /home
        // as the filesystem names it. On Linux that is /home itself (or, where
        // it does not exist, the literal path); on macOS /home is a link to
        // /System/Volumes/Data/home, which `contained` follows like any other.
        let home = Path::new("/home")
            .canonicalize()
            .unwrap_or_else(|_| PathBuf::from("/home"));
        assert_eq!(
            repo_dir(repos, "my-app").expect("safe"),
            home.join("ana/.buzz/repos/my-app")
        );
    }

    #[test]
    fn the_repositories_directory_itself_is_validated() {
        assert!(validate_repos_dir(Path::new("")).is_err());
        assert!(validate_repos_dir(Path::new("relative/path")).is_err());
        assert!(validate_repos_dir(Path::new("/")).is_err(), "root is not a repos dir");
        assert!(validate_repos_dir(Path::new("/home/../etc")).is_err());
        assert!(validate_repos_dir(Path::new("/home/ana/.buzz/repos")).is_ok());
    }

    #[test]
    fn a_symlinked_repository_is_refused_rather_than_followed() {
        // The hole a lexical check alone leaves open: the NAME is safe and the
        // directory still points somewhere else entirely.
        let base = std::env::temp_dir().join(format!("buzz-link-{}", std::process::id()));
        let repos = base.join("repos");
        let outside = base.join("outside");
        std::fs::create_dir_all(&repos).expect("mkdir repos");
        std::fs::create_dir_all(outside.join(".git")).expect("mkdir outside");

        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(&outside, repos.join("escapee")).expect("symlink");
            let refused = repo_dir(&repos, "escapee");
            assert!(
                refused.is_err(),
                "a repository directory that resolves outside the root must be refused"
            );
            assert!(
                matches!(refused, Err(Error::Refused(_))),
                "and refused as a permission answer, not a malformed name"
            );
            // ...and it is not listed as a managed checkout either.
            let listed = local_repositories(&repos).expect("listing");
            assert!(
                listed.is_empty(),
                "a symlinked entry must not be reported as a repository: {listed:?}"
            );
        }

        // A real directory inside the root still resolves.
        std::fs::create_dir_all(repos.join("real/.git")).expect("mkdir real");
        let resolved = repo_dir(&repos, "real").expect("contained");
        assert!(resolved.ends_with("real"));
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn a_clone_target_that_does_not_exist_yet_is_still_contained() {
        let base = std::env::temp_dir().join(format!("buzz-pre-{}", std::process::id()));
        let repos = base.join("repos");
        std::fs::create_dir_all(&repos).expect("mkdir");
        // Nothing on disk yet — resolution through the existing parent is the
        // guarantee.
        let target = repo_dir(&repos, "not-cloned-yet").expect("contained");
        assert!(target.ends_with("not-cloned-yet"));
        // A candidate whose parent is NOT the root is refused.
        assert!(contained(&repos, &base.join("elsewhere/thing")).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_root_that_does_not_exist_yet_is_resolved_through_its_symlinked_ancestor() {
        // The hole a bare "cannot canonicalize, so nothing to check" leaves: the
        // repositories directory has not been created yet, and an ancestor is a
        // symlink. Containment must still be decided against where the path
        // REALLY leads.
        let base = std::env::temp_dir().join(format!("buzz-anc-{}", std::process::id()));
        let real = base.join("real");
        std::fs::create_dir_all(&real).expect("mkdir real");
        std::os::unix::fs::symlink(&real, base.join("link")).expect("symlink");

        // repos does not exist yet, and its parent is a symlink.
        let repos = base.join("link").join("repos");
        let inside = repo_dir(&repos, "app").expect("a clone target under the root");
        // Resolved through the link, so the path names where it really lands.
        assert!(
            inside.starts_with(real.canonicalize().expect("canonical real")),
            "{}",
            inside.display()
        );

        // ...and a sibling outside that root is still refused.
        assert!(contained(&repos, &base.join("outside/app")).is_err());
        std::fs::remove_dir_all(&base).ok();
    }

    #[cfg(unix)]
    #[test]
    fn a_broken_symlink_is_refused_rather_than_treated_as_absent() {
        let base = std::env::temp_dir().join(format!("buzz-broken-{}", std::process::id()));
        let repos = base.join("repos");
        std::fs::create_dir_all(&repos).expect("mkdir");
        std::os::unix::fs::symlink(base.join("nowhere"), repos.join("dangling"))
            .expect("symlink");
        let refused = repo_dir(&repos, "dangling");
        assert!(refused.is_err(), "a dangling symlink is not a clone target");
        std::fs::remove_dir_all(&base).ok();
    }

    #[test]
    fn listing_an_absent_directory_is_an_empty_list_not_an_error() {
        let missing = Path::new("/nonexistent-for-tests/buzz/repos");
        assert_eq!(local_repositories(missing).expect("empty"), Vec::new());
    }

    #[test]
    fn only_directories_holding_a_git_entry_are_repositories() {
        let base = std::env::temp_dir().join(format!("buzz-paths-{}", std::process::id()));
        let repos = base.join("repos");
        std::fs::create_dir_all(repos.join("real/.git")).expect("mkdir");
        std::fs::create_dir_all(repos.join("not-a-repo")).expect("mkdir");
        let found = local_repositories(&repos).expect("listing");
        assert_eq!(
            found.iter().map(|(name, _)| name.as_str()).collect::<Vec<_>>(),
            vec!["real"],
            "a folder without .git is not a checkout"
        );
        std::fs::remove_dir_all(&base).ok();
    }
}
