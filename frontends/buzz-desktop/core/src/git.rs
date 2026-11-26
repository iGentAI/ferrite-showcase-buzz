//! Running `git`, and parsing what it says.
//!
//! `git` itself is the git implementation here — not a linked library. It is
//! already installed wherever someone works with repositories, it is the
//! program whose behaviour their hooks and muscle memory expect, and its output
//! cannot disagree with the command line about what a repository contains. A
//! library would be a second implementation to keep in agreement with the
//! first.
//!
//! Two invocation details are load-bearing rather than incidental, and both are
//! set for every call in [`Git::output`]:
//!
//! * **`GIT_TERMINAL_PROMPT=0`.** A git that cannot authenticate will otherwise
//!   sit waiting for a username on a terminal that does not exist, and the
//!   command hangs forever with no output — which presents to the user as the
//!   app freezing rather than as a credential problem.
//! * **`LC_ALL=C`.** Every parser below reads git's own words. Under another
//!   locale those words change, and a parser that depends on the user's
//!   language is a parser that works on the developer's machine.
//!
//! The parsers are free functions over `&str` on purpose: they are where the
//! interesting mistakes live, and this way they are tested without a repository.

use std::path::Path;
use std::process::Command;

use serde::{Deserialize, Serialize};

use crate::{Error, Result};

/// Field separator inside one `git log` record (ASCII unit separator).
pub const FIELD: char = '\u{1f}';
/// Record separator between `git log` entries (ASCII record separator).
pub const RECORD: char = '\u{1e}';

/// The `git log` format every commit read here uses.
///
/// Separated by control characters rather than by spaces, tabs or newlines
/// because a commit SUBJECT can contain all three. A subject with a tab in it is
/// unusual; a parser that silently mis-attributes an author because of one is a
/// bug that only appears in somebody else's repository.
pub const LOG_FORMAT: &str = "%H\u{1f}%h\u{1f}%an\u{1f}%ae\u{1f}%at\u{1f}%s\u{1e}";

/// One commit, in the wire shape `projectGit.ts` reads.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Commit {
    pub hash: String,
    pub short_hash: String,
    pub author_name: String,
    pub author_email: String,
    pub timestamp: u64,
    pub subject: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Worktree {
    pub modified: Vec<String>,
    pub untracked: Vec<String>,
}

impl Worktree {
    #[must_use]
    pub fn is_dirty(&self) -> bool {
        !self.modified.is_empty()
    }

    #[must_use]
    pub fn has_untracked(&self) -> bool {
        !self.untracked.is_empty()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NumStat {
    pub additions: u32,
    pub deletions: u32,
    pub path: String,
    /// True for a binary file, where git reports `-` instead of counts. Kept as
    /// a fact rather than folded into zeroes: "no lines changed" and "lines are
    /// not how this file changes" are different answers.
    pub binary: bool,
}

#[derive(Debug, Clone)]
pub struct Output {
    pub ok: bool,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    fn from_process(done: std::process::Output) -> Self {
        Self {
            ok: done.status.success(),
            stdout: String::from_utf8_lossy(&done.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&done.stderr).into_owned(),
        }
    }
}

#[derive(Debug, Clone)]
pub struct Git {
    program: String,
    /// Extra environment for every call — how the credential helper is attached.
    env: Vec<(String, String)>,
}

impl Default for Git {
    fn default() -> Self {
        Self::new()
    }
}

impl Git {
    #[must_use]
    pub fn new() -> Self {
        Self {
            program: "git".to_string(),
            env: Vec::new(),
        }
    }

    /// A git at an explicit path — what `discover_git_bash_prerequisite` finds on
    /// Windows, and what a test uses to stand in a script.
    #[must_use]
    pub fn with_program(program: &str) -> Self {
        Self {
            program: program.to_string(),
            env: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_env(mut self, key: &str, value: &str) -> Self {
        self.env.push((key.to_string(), value.to_string()));
        self
    }

    #[must_use]
    pub fn program(&self) -> &str {
        &self.program
    }

    pub fn output(&self, cwd: Option<&Path>, args: &[&str]) -> Result<Output> {
        self.output_with_stdin(cwd, args, None)
    }

    /// Run `git` and return trimmed non-empty stdout when it succeeds, or `None`.
    #[must_use]
    pub fn try_stdout(&self, cwd: Option<&Path>, args: &[&str]) -> Option<String> {
        self.output(cwd, args)
            .ok()
            .filter(|out| out.ok)
            .map(|out| out.stdout.trim().to_string())
            .filter(|text| !text.is_empty())
    }

    /// [`Self::output`], with something written to git's stdin.
    ///
    /// Only the credential protocol needs this (`git credential fill` reads its
    /// request from stdin), and it is a separate entry point on purpose: every
    /// OTHER call closes stdin, which is half of why a failing authentication
    /// cannot hang — see this module's own documentation.
    pub fn output_with_stdin(
        &self,
        cwd: Option<&Path>,
        args: &[&str],
        stdin: Option<&str>,
    ) -> Result<Output> {
        let mut command = Command::new(&self.program);
        command.args(args);
        if let Some(cwd) = cwd {
            command.current_dir(cwd);
        }
        // See the module docs: without these two a failing authentication hangs
        // forever, and a non-English locale breaks every parser below.
        command.env("GIT_TERMINAL_PROMPT", "0");
        command.env("LC_ALL", "C");
        // An interactive editor would wait for input that can never arrive.
        command.env("GIT_EDITOR", "true");
        for (key, value) in &self.env {
            command.env(key, value);
        }
        let Some(input) = stdin else {
            command.stdin(std::process::Stdio::null());
            let done = command.output().map_err(|e| self.unavailable(&e))?;
            return Ok(Output::from_process(done));
        };
        command
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped());
        let mut child = command.spawn().map_err(|e| self.unavailable(&e))?;
        {
            use std::io::Write;
            let Some(pipe) = child.stdin.as_mut() else {
                return Err(Error::Internal("git has no stdin".into()));
            };
            // A write failure here means git exited before reading, which its own
            // output explains — so it is not raised over that explanation.
            let _ = pipe.write_all(input.as_bytes());
        }
        // Dropped so git sees EOF; without it `git credential fill` waits.
        drop(child.stdin.take());
        let done = child
            .wait_with_output()
            .map_err(|e| Error::Tool {
                program: self.program.clone(),
                detail: format!("could not read git's output: {e}"),
            })?;
        Ok(Output::from_process(done))
    }

    fn unavailable(&self, e: &std::io::Error) -> Error {
        Error::Tool {
            program: self.program.clone(),
            detail: format!("could not run git ({e}). Is git installed and on PATH?"),
        }
    }

    /// Run git, and fail with git's OWN message when it fails.
    pub fn run(&self, cwd: Option<&Path>, args: &[&str]) -> Result<String> {
        let out = self.output(cwd, args)?;
        if out.ok {
            return Ok(out.stdout);
        }
        // git's diagnostics are better than any paraphrase, and a user who sees
        // one can usually act on it. Passed through, trimmed, never rewritten.
        let detail = if out.stderr.trim().is_empty() {
            format!("`git {}` failed", args.join(" "))
        } else {
            out.stderr.trim().to_string()
        };
        Err(Error::Tool {
            program: self.program.clone(),
            detail,
        })
    }

    pub fn version(&self) -> Result<String> {
        Ok(self.run(None, &["--version"])?.trim().to_string())
    }
}

/// Parse `git log --format=LOG_FORMAT`.
///
/// Tolerant of a trailing record separator and of blank input; a malformed
/// record is SKIPPED rather than failing the read, because one unparseable
/// commit must not blank out a repository's whole history.
#[must_use]
pub fn parse_log(text: &str) -> Vec<Commit> {
    let mut out = Vec::new();
    for record in text.split(RECORD) {
        let record = record.trim_matches('\n');
        if record.is_empty() {
            continue;
        }
        // `splitn` rather than `split`: the SUBJECT is the last field, and a
        // stray separator byte inside it must stay in the subject rather than
        // shear the record into extra fields and truncate the text.
        let fields: Vec<&str> = record.splitn(6, FIELD).collect();
        if fields.len() < 6 {
            continue;
        }
        let timestamp = fields[4].trim().parse::<u64>().unwrap_or(0);
        out.push(Commit {
            hash: fields[0].trim().to_string(),
            short_hash: fields[1].trim().to_string(),
            author_name: fields[2].to_string(),
            author_email: fields[3].to_string(),
            timestamp,
            // NOT trimmed: a subject's own leading space is the author's.
            subject: fields[5].to_string(),
        });
    }
    out
}

/// Parse `git status --porcelain`.
///
/// The two-character status prefix is fixed-width, so the path starts at column
/// 3. A renamed entry reads `R  old -> new`; the NEW name is what changed.
#[must_use]
pub fn parse_status(text: &str) -> Worktree {
    let mut worktree = Worktree::default();
    for line in text.lines() {
        if line.len() < 4 {
            continue;
        }
        let code = &line[..2];
        let path = line[3..].trim();
        let path = match path.split_once(" -> ") {
            Some((_, new)) => new,
            None => path,
        };
        // Quoted when the path has unusual bytes; the quotes are git's, not the
        // name's.
        let path = unquote_path(path);
        if code == "??" {
            worktree.untracked.push(path);
        } else {
            worktree.modified.push(path);
        }
    }
    worktree
}

/// Parse `git rev-list --left-right --count base...head` — `(behind, ahead)`.
///
/// The order is git's, and it is the opposite of how a person says it: the left
/// count is commits the OTHER side has (we are behind by that many), the right
/// is ours (ahead). Naming the return makes the swap impossible to make twice.
#[must_use]
pub fn parse_ahead_behind(text: &str) -> (u32, u32) {
    let mut parts = text.split_whitespace();
    let behind = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    let ahead = parts.next().and_then(|v| v.parse().ok()).unwrap_or(0);
    (behind, ahead)
}

#[must_use]
pub fn parse_numstat(text: &str) -> Vec<NumStat> {
    let mut out = Vec::new();
    for line in text.lines() {
        let mut fields = line.split('\t');
        let (Some(added), Some(removed), Some(path)) =
            (fields.next(), fields.next(), fields.next())
        else {
            continue;
        };
        // git writes `-` for both counts on a binary file.
        let binary = added == "-" || removed == "-";
        out.push(NumStat {
            additions: added.parse().unwrap_or(0),
            deletions: removed.parse().unwrap_or(0),
            path: unquote_path(path.trim()),
            binary,
        });
    }
    out
}

#[must_use]
pub fn parse_branch_list(text: &str) -> Vec<String> {
    text.lines()
        .map(str::trim)
        // A detached HEAD is listed as `(HEAD detached at abc1234)`, which is a
        // state rather than a branch anybody can check out by name.
        .filter(|line| !line.is_empty() && !line.starts_with('('))
        .map(str::to_string)
        .collect()
}

/// Split a unified diff into `(path, patch)` per file.
///
/// Keyed by the **b-side** path — where the file ended up — because a rename
/// reported under its old name would not match the file the caller is looking
/// at.
///
/// Two subtleties, both of which have a wrong answer that looks right:
///
/// * The path comes from the `+++`/`---` marker lines rather than from the
///   `diff --git a/X b/Y` header, because that header is genuinely AMBIGUOUS
///   when a path contains a space. But marker parsing STOPS at the first hunk:
///   an added line whose own text begins with `++ ` appears in the patch as
///   `+++ …`, and reading that as a file marker would rename the patch after
///   its own content.
/// * A block can legitimately carry no marker at all — a binary file, a
///   mode-only change — so those are recovered from `Binary files … differ`,
///   from rename metadata, and finally from the header itself, where the
///   ambiguity is resolvable for the non-rename case (both sides are the same
///   path). Dropping such a block would silently lose a real change.
#[must_use]
pub fn split_patches(diff: &str) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = Vec::new();
    let mut block: Option<Block> = None;
    for line in diff.lines() {
        if line.starts_with("diff --git ") {
            if let Some(done) = block.take() {
                out.push(done.finish());
            }
            block = Some(Block::new(line));
            continue;
        }
        if let Some(block) = block.as_mut() {
            block.push(line);
        }
    }
    if let Some(done) = block.take() {
        out.push(done.finish());
    }
    out.retain(|(path, _)| !path.is_empty());
    out
}

struct Block<'a> {
    header: &'a str,
    lines: Vec<&'a str>,
    /// Set once a hunk (or binary payload) starts: after that, a line beginning
    /// with `+++` or `---` is content, not a marker.
    in_body: bool,
    from_marker: Option<String>,
    from_old_marker: Option<String>,
    from_rename: Option<String>,
    from_binary: Option<String>,
}

impl<'a> Block<'a> {
    fn new(header: &'a str) -> Self {
        Self {
            header,
            lines: vec![header],
            in_body: false,
            from_marker: None,
            from_old_marker: None,
            from_rename: None,
            from_binary: None,
        }
    }

    fn push(&mut self, line: &'a str) {
        self.lines.push(line);
        if line.starts_with("@@") {
            self.in_body = true;
            return;
        }
        if let Some(rest) = line.strip_prefix("Binary files ") {
            self.in_body = true;
            self.from_binary = binary_path(rest);
            return;
        }
        if self.in_body {
            return;
        }
        if let Some(rest) = line.strip_prefix("+++ ") {
            self.from_marker = marker_path(rest);
        } else if let Some(rest) = line.strip_prefix("--- ") {
            self.from_old_marker = marker_path(rest);
        } else if let Some(rest) = line.strip_prefix("rename to ") {
            self.from_rename = Some(strip_side_prefix(&unquote_path(rest.trim())));
        }
    }

    /// The path, in order of how unambiguous each source is.
    fn finish(self) -> (String, String) {
        let path = self
            .from_marker
            .or(self.from_rename)
            .or(self.from_binary)
            .or(self.from_old_marker)
            .or_else(|| header_path(self.header.trim_start_matches("diff --git ")))
            .unwrap_or_default();
        (path, self.lines.join("\n"))
    }
}

fn marker_path(rest: &str) -> Option<String> {
    let rest = rest.trim();
    (rest != "/dev/null").then(|| strip_side_prefix(&unquote_path(rest)))
}

fn unquote_side(quoted: &str) -> String {
    strip_side_prefix(&unquote_path(&format!("\"{}\"", quoted.trim_matches('"'))))
}
/// The path out of a `Binary files a/X and b/Y differ` line.
///
/// ` and ` is not a safe delimiter: `foo and bar.png` is a perfectly ordinary
/// filename, and splitting on the first occurrence keys the patch under
/// `bar.png and b/foo and bar.png`. So the sides are resolved the same way the
/// header is — by SYMMETRY, since both sides name the same file for everything
/// except a rename — and an ambiguous line DECLINES rather than guessing, which
/// lets the header fallback (where the same symmetry argument applies) answer
/// instead of being overridden by a wrong value.
fn binary_path(rest: &str) -> Option<String> {
    let body = rest.trim();
    let body = body.strip_suffix(" differ").unwrap_or(body);
    // ONE-SIDED first, quoted or not: an added or deleted binary names a single
    // path, and `/dev/null` is not one. Checked before the two-side branch
    // because a quoted deletion (`"a/caf\303\251.png" and /dev/null`) starts
    // with a quote and has no `" and "` separator — read as a two-side form it
    // would yield a mangled name.
    if let Some(added) = body.strip_prefix("/dev/null and ") {
        return Some(strip_side_prefix(&unquote_path(added.trim())));
    }
    if let Some(deleted) = body.strip_suffix(" and /dev/null") {
        return Some(strip_side_prefix(&unquote_path(deleted.trim())));
    }
    if body.starts_with('"') {
        let sides: Vec<&str> = body.split("\" and \"").collect();
        if sides.len() != 2 {
            return None;
        }
        return sides.last().map(|last| unquote_side(last));
    }
    let a_rest = body.strip_prefix("a/")?;
    for (at, _) in a_rest.match_indices(" and b/") {
        let (a_side, b_side) = (&a_rest[..at], &a_rest[at + " and b/".len()..]);
        if a_side == b_side {
            return Some(b_side.to_string());
        }
    }
    None
}

/// The b-side path out of a `diff --git a/X b/Y` header.
///
/// Ambiguous in general — a space in a path cannot be told from the separator —
/// but resolvable for the case that matters: when the two sides are the SAME
/// path (everything except a rename), exactly one split point makes them equal.
/// A rename with spaces falls back to the last ` b/`, and renames also carry
/// `rename to` metadata, which is preferred over this function.
fn header_path(header: &str) -> Option<String> {
    let header = header.trim();
    if header.starts_with('"') {
        let sides: Vec<&str> = header.split("\" \"").collect();
        return sides.last().map(|last| unquote_side(last));
    }
    let rest = header.strip_prefix("a/")?;
    for (at, _) in rest.match_indices(" b/") {
        let (a_side, b_side) = (&rest[..at], &rest[at + 3..]);
        if a_side == b_side {
            return Some(b_side.to_string());
        }
    }
    rest.rsplit_once(" b/").map(|(_, b)| b.to_string())
}

/// Undo git's C-style path quoting.
///
/// git quotes a path containing unusual bytes and escapes them — `café.txt`
/// becomes `"caf\303\251.txt"` (UTF-8 as octal escapes). Merely trimming the
/// quotes would leave the escapes in, and the resulting string would match no
/// real file: a diff of a non-ASCII path would silently fail to line up with
/// the repository's own listing.
#[must_use]
pub fn unquote_path(raw: &str) -> String {
    if !(raw.starts_with('"') && raw.ends_with('"') && raw.len() >= 2) {
        return raw.to_string();
    }
    let inner = &raw[1..raw.len() - 1];
    let mut bytes: Vec<u8> = Vec::with_capacity(inner.len());
    let mut chars = inner.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '\\' {
            let mut buf = [0u8; 4];
            bytes.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
            continue;
        }
        match chars.next() {
            Some('n') => bytes.push(b'\n'),
            Some('t') => bytes.push(b'\t'),
            Some('r') => bytes.push(b'\r'),
            Some('"') => bytes.push(b'"'),
            Some('\\') => bytes.push(b'\\'),
            // Three octal digits, which is how git writes a non-ASCII byte.
            Some(first @ '0'..='7') => {
                let mut value = first.to_digit(8).unwrap_or(0);
                for _ in 0..2 {
                    match chars.peek().and_then(|c| c.to_digit(8)) {
                        Some(digit) => {
                            value = value * 8 + digit;
                            chars.next();
                        }
                        None => break,
                    }
                }
                bytes.push(u8::try_from(value).unwrap_or(b'?'));
            }
            // An escape this function does not know: keep it literal rather than
            // dropping a character out of a filename.
            Some(other) => {
                bytes.push(b'\\');
                let mut buf = [0u8; 4];
                bytes.extend_from_slice(other.encode_utf8(&mut buf).as_bytes());
            }
            None => bytes.push(b'\\'),
        }
    }
    String::from_utf8_lossy(&bytes).into_owned()
}

fn strip_side_prefix(path: &str) -> String {
    path.strip_prefix("b/")
        .or_else(|| path.strip_prefix("a/"))
        .unwrap_or(path)
        .to_string()
}

/// Cut a patch to `max_lines`, reporting whether anything was dropped.
///
/// A diff of a generated file can be tens of thousands of lines, and the whole
/// of it would ride one IPC message into a renderer that shows the first screen.
/// The `truncated` flag is why the client can say so rather than silently
/// showing a partial patch as if it were the whole change.
#[must_use]
pub fn truncate_patch(patch: &str, max_lines: usize) -> (String, bool) {
    let mut lines = patch.lines();
    let kept: Vec<&str> = lines.by_ref().take(max_lines).collect();
    let truncated = lines.next().is_some();
    (kept.join("\n"), truncated)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_log_record_survives_a_subject_containing_control_characters() {
        // The reason for the separators: a subject can contain a tab or a
        // newline, and a space- or newline-delimited format would mis-parse it.
        let text = format!(
            "abc123{F}abc12{F}Ana Smith{F}ana@example.com{F}1700000000{F}fix: tab\there{R}\
             def456{F}def45{F}Bo{F}bo@example.com{F}1700000100{F}second line\nin subject{R}",
            F = FIELD,
            R = RECORD
        );
        let commits = parse_log(&text);
        assert_eq!(commits.len(), 2);
        assert_eq!(commits[0].hash, "abc123");
        assert_eq!(commits[0].author_name, "Ana Smith");
        assert_eq!(commits[0].timestamp, 1_700_000_000);
        assert_eq!(commits[0].subject, "fix: tab\there");
        assert_eq!(commits[1].subject, "second line\nin subject");

        // Even the field separator itself, inside a SUBJECT, stays in the
        // subject: `splitn` keeps everything after the fifth separator whole.
        let text = format!(
            "abc{F}a{F}n{F}e{F}5{F}subject with {F} inside{R}",
            F = FIELD,
            R = RECORD
        );
        let commits = parse_log(&text);
        assert_eq!(commits.len(), 1);
        assert_eq!(
            commits[0].subject,
            format!("subject with {FIELD} inside")
        );
    }

    #[test]
    fn a_malformed_log_record_is_skipped_not_fatal() {
        let text = format!("broken-record{R}abc{F}a{F}n{F}e{F}5{F}s{R}", F = FIELD, R = RECORD);
        let commits = parse_log(&text);
        assert_eq!(commits.len(), 1, "one bad record must not blank the history");
        assert_eq!(commits[0].hash, "abc");
        assert!(parse_log("").is_empty());
    }

    #[test]
    fn status_separates_modified_from_untracked() {
        let text = " M src/lib.rs\nA  src/new.rs\n?? notes.txt\nR  old.rs -> new.rs\n";
        let worktree = parse_status(text);
        assert_eq!(
            worktree.modified,
            vec!["src/lib.rs", "src/new.rs", "new.rs"],
            "a rename reports the NEW name"
        );
        assert_eq!(worktree.untracked, vec!["notes.txt"]);
        assert!(worktree.is_dirty());
        assert!(worktree.has_untracked());

        let clean = parse_status("");
        assert!(!clean.is_dirty() && !clean.has_untracked());
    }

    #[test]
    fn a_quoted_status_path_loses_gits_quotes_not_its_own_characters() {
        let worktree = parse_status("?? \"file with space.txt\"\n M \"caf\\303\\251.txt\"\n");
        assert_eq!(worktree.untracked, vec!["file with space.txt"]);
        assert_eq!(worktree.modified, vec!["caf\u{e9}.txt"]);
        let stats = parse_numstat("4\t1\t\"caf\\303\\251.txt\"\n");
        assert_eq!(stats.len(), 1);
        assert_eq!(stats[0].path, "caf\u{e9}.txt");
    }

    #[test]
    fn ahead_and_behind_are_read_in_gits_order() {
        // git prints LEFT then RIGHT for `base...head`: left is what the other
        // side has (behind), right is ours (ahead). Getting this backwards would
        // offer a push where a pull is needed.
        assert_eq!(parse_ahead_behind("3\t7\n"), (3, 7));
        assert_eq!(parse_ahead_behind("0 0"), (0, 0));
        assert_eq!(parse_ahead_behind(""), (0, 0));
    }

    #[test]
    fn numstat_keeps_binary_files_distinguishable() {
        let text = "12\t3\tsrc/lib.rs\n-\t-\tlogo.png\n0\t0\tempty.txt\n";
        let stats = parse_numstat(text);
        assert_eq!(stats.len(), 3);
        assert_eq!(stats[0].additions, 12);
        assert_eq!(stats[0].deletions, 3);
        assert!(!stats[0].binary);
        assert!(stats[1].binary, "a binary file is not a zero-line change");
        assert!(!stats[2].binary);
    }

    #[test]
    fn the_branch_list_drops_a_detached_head() {
        let text = "main\nfeature/x\n(HEAD detached at abc1234)\n\n";
        assert_eq!(parse_branch_list(text), vec!["main", "feature/x"]);
    }

    #[test]
    fn patches_split_per_file_and_key_on_the_b_side_path() {
        let diff = "diff --git a/src/lib.rs b/src/lib.rs\n\
                    index 111..222 100644\n\
                    --- a/src/lib.rs\n\
                    +++ b/src/lib.rs\n\
                    @@ -1 +1 @@\n\
                    -old\n\
                    +new\n\
                    diff --git a/old.rs b/renamed.rs\n\
                    similarity index 95%\n\
                    rename from old.rs\n\
                    rename to renamed.rs\n\
                    --- a/old.rs\n\
                    +++ b/renamed.rs\n\
                    @@ -1 +1 @@\n\
                    -a\n\
                    +b\n";
        let patches = split_patches(diff);
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0].0, "src/lib.rs");
        assert!(patches[0].1.contains("+new"));
        assert!(patches[0].1.starts_with("diff --git"), "the header stays in the patch");
        assert_eq!(
            patches[1].0, "renamed.rs",
            "a rename is reported where the file ENDED UP"
        );
    }

    #[test]
    fn a_path_with_spaces_survives_the_split() {
        // The `diff --git` header is ambiguous here — `a/my file.txt b/my
        // file.txt` cannot be split on whitespace — so the marker lines are
        // what the path comes from.
        let diff = "diff --git a/my file.txt b/my file.txt\n\
                    --- a/my file.txt\n\
                    +++ b/my file.txt\n\
                    @@ -1 +1 @@\n-a\n+b\n";
        let patches = split_patches(diff);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, "my file.txt");
    }

    #[test]
    fn a_deleted_file_is_named_by_the_side_it_had() {
        let diff = "diff --git a/gone.rs b/gone.rs\n\
                    deleted file mode 100644\n\
                    --- a/gone.rs\n\
                    +++ /dev/null\n\
                    @@ -1 +0,0 @@\n-was here\n";
        let patches = split_patches(diff);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, "gone.rs", "/dev/null is not a path");
    }

    #[test]
    fn a_pure_rename_with_no_hunks_is_still_named() {
        let diff = "diff --git a/old.rs b/new.rs\n\
                    similarity index 100%\n\
                    rename from old.rs\n\
                    rename to new.rs\n";
        let patches = split_patches(diff);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, "new.rs");
    }

    #[test]
    fn gits_c_style_quoting_is_undone() {
        // git writes a non-ASCII path as octal escapes inside quotes. Trimming
        // the quotes alone would leave `caf\303\251.txt`, which matches no file.
        assert_eq!(unquote_path("\"caf\\303\\251.txt\""), "caf\u{e9}.txt");
        assert_eq!(unquote_path("\"with\\tab.txt\""), "with\tab.txt");
        assert_eq!(unquote_path("\"quote\\\"inside.txt\""), "quote\"inside.txt");
        assert_eq!(unquote_path("\"back\\\\slash.txt\""), "back\\slash.txt");
        // Unquoted input is returned untouched.
        assert_eq!(unquote_path("plain.txt"), "plain.txt");
        assert_eq!(unquote_path("has space.txt"), "has space.txt");

        let diff = "diff --git \"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"\n\
                    --- \"a/caf\\303\\251.txt\"\n\
                    +++ \"b/caf\\303\\251.txt\"\n\
                    @@ -1 +1 @@\n-a\n+b\n";
        assert_eq!(split_patches(diff)[0].0, "caf\u{e9}.txt");
    }

    #[test]
    fn a_content_line_that_looks_like_a_marker_does_not_rename_the_patch() {
        // An added line whose own text starts with `++ ` renders as `+++ …`.
        // Read as a file marker, it would rename the patch after its content.
        let diff = "diff --git a/notes.md b/notes.md\n\
                    --- a/notes.md\n\
                    +++ b/notes.md\n\
                    @@ -1,2 +1,4 @@\n\
                     context\n\
                    +++ not a marker, just text\n\
                    +--- also not a marker\n";
        let patches = split_patches(diff);
        assert_eq!(patches.len(), 1);
        assert_eq!(
            patches[0].0, "notes.md",
            "marker parsing must stop at the first hunk"
        );
        assert!(patches[0].1.contains("+++ not a marker"), "content is kept");
    }

    #[test]
    fn a_binary_change_is_kept_and_named() {
        let diff = "diff --git a/logo.png b/logo.png\n\
                    index 111..222 100644\n\
                    Binary files a/logo.png and b/logo.png differ\n";
        let patches = split_patches(diff);
        assert_eq!(patches.len(), 1, "a binary change is a real change");
        assert_eq!(patches[0].0, "logo.png");
    }

    #[test]
    fn a_binary_filename_containing_the_delimiter_is_still_correct() {
        // ` and ` is an ordinary part of a filename, so splitting on the first
        // occurrence would key this patch under nonsense.
        let diff = "diff --git a/foo and bar.png b/foo and bar.png\n\
                    index 111..222 100644\n\
                    Binary files a/foo and bar.png and b/foo and bar.png differ\n";
        let patches = split_patches(diff);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, "foo and bar.png");

        // The parser itself: symmetry resolves it, and a genuinely ambiguous
        // line declines so the header fallback answers instead.
        assert_eq!(
            binary_path("a/foo and bar.png and b/foo and bar.png differ").as_deref(),
            Some("foo and bar.png")
        );
        assert_eq!(binary_path("a/old.png and b/new.png differ"), None);
        // Added and deleted binaries name one side only.
        assert_eq!(
            binary_path("/dev/null and b/added.png differ").as_deref(),
            Some("added.png")
        );
        assert_eq!(
            binary_path("a/gone.png and /dev/null differ").as_deref(),
            Some("gone.png")
        );
        assert_eq!(
            binary_path("\"a/caf\\303\\251.png\" and \"b/caf\\303\\251.png\" differ").as_deref(),
            Some("caf\u{e9}.png")
        );
        // A QUOTED one-sided line: starts with a quote and has no `" and "`
        // separator, so the two-side branch would mangle it.
        assert_eq!(
            binary_path("\"a/caf\\303\\251.png\" and /dev/null differ").as_deref(),
            Some("caf\u{e9}.png")
        );
        assert_eq!(
            binary_path("/dev/null and \"b/caf\\303\\251.png\" differ").as_deref(),
            Some("caf\u{e9}.png")
        );
    }

    #[test]
    fn a_renamed_binary_falls_back_to_its_rename_metadata() {
        // The ambiguous binary line declines, and the rename metadata — which is
        // unambiguous — is what names the file.
        let diff = "diff --git a/old.png b/new.png\n\
                    similarity index 90%\n\
                    rename from old.png\n\
                    rename to new.png\n\
                    Binary files a/old.png and b/new.png differ\n";
        let patches = split_patches(diff);
        assert_eq!(patches.len(), 1);
        assert_eq!(patches[0].0, "new.png");
    }

    #[test]
    fn a_mode_only_change_is_kept_and_named_from_the_header() {
        let diff = "diff --git a/run.sh b/run.sh\n\
                    old mode 100644\n\
                    new mode 100755\n";
        let patches = split_patches(diff);
        assert_eq!(patches.len(), 1, "chmod is a change worth showing");
        assert_eq!(patches[0].0, "run.sh");
    }

    #[test]
    fn the_header_fallback_resolves_a_spaced_path_by_symmetry() {
        // Both sides are the same path, so exactly one split point makes them
        // equal — which is what makes the ambiguous header readable here.
        assert_eq!(
            header_path("a/my file.txt b/my file.txt").as_deref(),
            Some("my file.txt")
        );
        assert_eq!(header_path("a/src/lib.rs b/src/lib.rs").as_deref(), Some("src/lib.rs"));
        // A rename falls back to the last ` b/` — and real renames carry
        // `rename to`, which is preferred over this.
        assert_eq!(header_path("a/old.rs b/new.rs").as_deref(), Some("new.rs"));
        assert_eq!(
            header_path("\"a/caf\\303\\251.txt\" \"b/caf\\303\\251.txt\"").as_deref(),
            Some("caf\u{e9}.txt")
        );
    }

    #[test]
    fn a_block_with_no_resolvable_path_is_dropped() {
        // Better than reporting a patch under an empty name, which would attach
        // a change to whichever row rendered "" first.
        let diff = "diff --git nonsense\nBinary files differ\n";
        assert!(split_patches(diff).is_empty());
    }

    #[test]
    fn truncation_reports_itself() {
        let patch = (0..10).map(|i| format!("line {i}")).collect::<Vec<_>>().join("\n");
        let (cut, truncated) = truncate_patch(&patch, 4);
        assert_eq!(cut.lines().count(), 4);
        assert!(truncated, "a cut patch must say so, or it reads as the whole change");
        let (whole, truncated) = truncate_patch(&patch, 100);
        assert_eq!(whole.lines().count(), 10);
        assert!(!truncated);
    }

    #[test]
    fn a_missing_git_reports_something_actionable() {
        let git = Git::with_program("definitely-not-a-real-git-binary");
        let err = git.version().expect_err("must fail");
        let message = err.to_string();
        assert!(message.contains("Is git installed"), "{message}");
        assert!(!err.is_final(), "a missing tool is a fixable condition");
    }

    #[test]
    fn the_real_git_reports_a_version_and_a_bad_repository_honestly() {
        let git = Git::new();
        // The test machine has git; if it ever does not, this test is the one
        // that should say so rather than a confusing failure elsewhere.
        let version = git.version().expect("git --version");
        assert!(version.starts_with("git version"), "{version}");

        let missing = std::env::temp_dir().join("buzz-desktop-not-a-repo");
        std::fs::create_dir_all(&missing).expect("mkdir");
        let err = git
            .run(Some(&missing), &["rev-parse", "HEAD"])
            .expect_err("not a repository");
        // git's own words, not a paraphrase.
        assert!(err.to_string().contains("not a git repository"), "{err}");
    }
}
