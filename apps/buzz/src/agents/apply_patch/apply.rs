//! Applying parsed hunks. The line transform is pure; the filesystem is injected
//! via the [`FileStore`] trait.

use std::future::Future;

use super::parser::{Hunk, UpdateFileChunk};
use super::seek_sequence::seek_sequence;
use crate::agents::error::{AgentError, Result};

/// An error applying a parsed patch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ApplyError {
    Compute(String),
    MissingFile(String),
    Io(String),
}

impl std::fmt::Display for ApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApplyError::Compute(m) | ApplyError::MissingFile(m) | ApplyError::Io(m) => {
                write!(f, "{m}")
            }
        }
    }
}
impl std::error::Error for ApplyError {}

impl From<AgentError> for ApplyError {
    fn from(e: AgentError) -> Self {
        ApplyError::Io(e.to_string())
    }
}

/// Transform `original` file content by applying `chunks`. Pure; `path` is used
/// only for error messages.
pub fn derive_new_contents(
    original: &str,
    chunks: &[UpdateFileChunk],
    path: &str,
) -> std::result::Result<String, ApplyError> {
    // A CRLF file keeps CRLF: lines are compared without the `\r` (hunk lines
    // never carry one) and rejoined with the file's own line ending.
    let crlf = original.contains("\r\n");
    let mut original_lines: Vec<String> = original
        .split('\n')
        .map(|line| {
            if crlf {
                line.strip_suffix('\r').unwrap_or(line).to_string()
            } else {
                line.to_string()
            }
        })
        .collect();
    // Only the final newline's empty element is dropped; a real trailing blank
    // line stays a line.
    if original_lines.last().is_some_and(String::is_empty) {
        original_lines.pop();
    }
    let replacements = compute_replacements(&original_lines, path, chunks)?;
    let new_lines = apply_replacements(original_lines, &replacements);
    if new_lines.is_empty() {
        return Ok(String::new());
    }
    let eol = if crlf { "\r\n" } else { "\n" };
    let mut out = new_lines.join(eol);
    out.push_str(eol);
    Ok(out)
}

fn compute_replacements(
    original_lines: &[String],
    path: &str,
    chunks: &[UpdateFileChunk],
) -> std::result::Result<Vec<(usize, usize, Vec<String>)>, ApplyError> {
    let mut replacements: Vec<(usize, usize, Vec<String>)> = Vec::new();
    let mut line_index: usize = 0;

    for chunk in chunks {
        if let Some(ctx) = &chunk.change_context {
            if let Some(idx) =
                seek_sequence(original_lines, std::slice::from_ref(ctx), line_index, false)
            {
                line_index = idx + 1;
            } else {
                return Err(ApplyError::Compute(format!(
                    "Failed to find context '{ctx}' in {path}"
                )));
            }
        }

        if chunk.old_lines.is_empty() {
            // A pure insertion goes right after its `@@` context line when it
            // names one, and at the end of the file otherwise.
            let insertion_idx = if chunk.change_context.is_some() {
                line_index
            } else if original_lines.last().is_some_and(String::is_empty) {
                original_lines.len() - 1
            } else {
                original_lines.len()
            };
            replacements.push((insertion_idx, 0, chunk.new_lines.clone()));
            line_index = insertion_idx;
            continue;
        }

        let mut pattern: &[String] = &chunk.old_lines;
        let mut found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        let mut new_slice: &[String] = &chunk.new_lines;

        if found.is_none() && pattern.last().is_some_and(String::is_empty) {
            pattern = &pattern[..pattern.len() - 1];
            if new_slice.last().is_some_and(String::is_empty) {
                new_slice = &new_slice[..new_slice.len() - 1];
            }
            found = seek_sequence(original_lines, pattern, line_index, chunk.is_end_of_file);
        }

        if let Some(start_idx) = found {
            replacements.push((start_idx, pattern.len(), new_slice.to_vec()));
            line_index = start_idx + pattern.len();
        } else {
            return Err(ApplyError::Compute(format!(
                "Failed to find expected lines in {path}:\n{}",
                chunk.old_lines.join("\n")
            )));
        }
    }

    replacements.sort_by_key(|(offset, _, _)| *offset);
    // Chunks that claim overlapping lines would delete each other's edits.
    for pair in replacements.windows(2) {
        let (prev_start, prev_len, _) = &pair[0];
        let (next_start, _, _) = &pair[1];
        if prev_start + prev_len > *next_start {
            return Err(ApplyError::Compute(format!(
                "Overlapping chunks in {path}"
            )));
        }
    }
    Ok(replacements)
}

fn apply_replacements(
    mut lines: Vec<String>,
    replacements: &[(usize, usize, Vec<String>)],
) -> Vec<String> {
    for (start_idx, old_len, new_segment) in replacements.iter().rev() {
        let start_idx = *start_idx;
        for _ in 0..*old_len {
            if start_idx < lines.len() {
                lines.remove(start_idx);
            }
        }
        for (offset, new_line) in new_segment.iter().enumerate() {
            lines.insert(start_idx + offset, new_line.clone());
        }
    }
    lines
}

/// The injected filesystem boundary. Paths are `/`-separated, validated by the
/// tool layer before they reach here.
pub trait FileStore {
    fn read(&mut self, path: &str) -> impl Future<Output = Result<Option<String>>> + Send;
    fn write(&mut self, path: &str, content: &str) -> impl Future<Output = Result<()>> + Send;
    fn delete(&mut self, path: &str) -> impl Future<Output = Result<()>> + Send;
    fn exists(&mut self, path: &str) -> impl Future<Output = Result<bool>> + Send;
    fn mkdir_p(&mut self, path: &str) -> impl Future<Output = Result<()>> + Send;
    fn list(&mut self, prefix: &str) -> impl Future<Output = Result<Vec<String>>> + Send;
}

/// A [`FileStore`] that keeps prior versions of each path.
pub trait VersionedFileStore: FileStore {
    fn history(&mut self, path: &str) -> impl Future<Output = Result<Vec<u64>>> + Send;
    fn read_version(
        &mut self,
        path: &str,
        version: u64,
    ) -> impl Future<Output = Result<Option<String>>> + Send;
}

/// Paths affected by a patch, grouped by op (for the summary).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AffectedPaths {
    pub added: Vec<String>,
    pub modified: Vec<String>,
    pub deleted: Vec<String>,
}

fn parent_dir(path: &str) -> Option<&str> {
    path.rfind('/')
        .map(|i| &path[..i])
        .filter(|p| !p.is_empty())
}

/// Normalize a workspace-relative store path so equivalent spellings such as
/// `a.txt`, `./a.txt`, and `dir/./a.txt` resolve to the same canonical key.
#[must_use]
pub fn normalize_store_path(path: &str) -> String {
    let mut parts: Vec<&str> = Vec::new();
    for seg in path.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                if parts.last().is_some_and(|last| *last != "..") {
                    parts.pop();
                } else {
                    parts.push("..");
                }
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

/// One verified store operation, staged until every hunk has been checked.
enum StagedOp {
    Write(String, String),
    Delete(String),
}

/// Read `path` as the patch so far left it: staged writes and deletes first,
/// then the store.
async fn staged_read<S: FileStore>(
    store: &mut S,
    overlay: &BTreeMap<String, Option<String>>,
    path: &str,
) -> std::result::Result<Option<String>, ApplyError> {
    match overlay.get(&normalize_store_path(path)) {
        Some(staged) => Ok(staged.clone()),
        None => Ok(store.read(path).await?),
    }
}

/// Apply parsed `hunks` to `store`, returning the affected paths.
///
/// Every hunk is verified against an in-memory overlay before anything is
/// written, so a patch whose later hunk fails leaves the store untouched (the
/// model is told a failed `apply_patch` did not apply).
pub async fn apply_patch_to_store<S: FileStore>(
    store: &mut S,
    hunks: &[Hunk],
) -> std::result::Result<AffectedPaths, ApplyError> {
    let mut affected = AffectedPaths::default();
    let mut overlay: BTreeMap<String, Option<String>> = BTreeMap::new();
    let mut ops: Vec<StagedOp> = Vec::new();
    for hunk in hunks {
        match hunk {
            Hunk::AddFile { path, contents } => {
                overlay.insert(normalize_store_path(path), Some(contents.clone()));
                ops.push(StagedOp::Write(path.clone(), contents.clone()));
                affected.added.push(path.clone());
            }
            Hunk::DeleteFile { path } => {
                if staged_read(store, &overlay, path).await?.is_none() {
                    return Err(ApplyError::MissingFile(format!(
                        "Failed to delete file {path}: it does not exist"
                    )));
                }
                overlay.insert(normalize_store_path(path), None);
                ops.push(StagedOp::Delete(path.clone()));
                affected.deleted.push(path.clone());
            }
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                let original = staged_read(store, &overlay, path).await?.ok_or_else(|| {
                    ApplyError::MissingFile(format!("Failed to read file to update {path}"))
                })?;
                // A pure rename keeps the bytes exactly.
                let new_contents = if chunks.is_empty() {
                    original
                } else {
                    derive_new_contents(&original, chunks, path)?
                };
                match move_path {
                    Some(dest) if normalize_store_path(dest) != normalize_store_path(path) => {
                        overlay.insert(normalize_store_path(dest), Some(new_contents.clone()));
                        overlay.insert(normalize_store_path(path), None);
                        ops.push(StagedOp::Write(dest.clone(), new_contents));
                        ops.push(StagedOp::Delete(path.clone()));
                        affected.modified.push(dest.clone());
                    }
                    _ => {
                        overlay.insert(normalize_store_path(path), Some(new_contents.clone()));
                        ops.push(StagedOp::Write(path.clone(), new_contents));
                        affected.modified.push(path.clone());
                    }
                }
            }
        }
    }
    for op in ops {
        match op {
            StagedOp::Write(path, contents) => {
                if let Some(parent) = parent_dir(&path) {
                    store.mkdir_p(parent).await?;
                }
                store.write(&path, &contents).await?;
            }
            StagedOp::Delete(path) => store.delete(&path).await?,
        }
    }
    Ok(affected)
}

/// The verbatim success summary.
pub fn format_summary(affected: &AffectedPaths) -> String {
    let mut s = String::from("Success. Updated the following files:\n");
    for p in &affected.added {
        s.push_str(&format!("A {p}\n"));
    }
    for p in &affected.modified {
        s.push_str(&format!("M {p}\n"));
    }
    for p in &affected.deleted {
        s.push_str(&format!("D {p}\n"));
    }
    s
}

use std::collections::BTreeMap;

/// A simple in-memory [`FileStore`]. `mkdir_p` is a no-op (paths are flat keys).
#[derive(Debug, Default, Clone)]
pub struct InMemoryFs {
    files: BTreeMap<String, String>,
}

impl InMemoryFs {
    pub fn new() -> Self {
        Self::default()
    }
    pub fn with(files: &[(&str, &str)]) -> Self {
        let mut m = BTreeMap::new();
        for (k, v) in files {
            m.insert(normalize_store_path(k), (*v).to_string());
        }
        Self { files: m }
    }
    pub fn get(&self, path: &str) -> Option<&String> {
        self.files.get(&normalize_store_path(path))
    }
    pub fn paths(&self) -> Vec<String> {
        self.files.keys().cloned().collect()
    }
}

impl FileStore for InMemoryFs {
    async fn read(&mut self, path: &str) -> Result<Option<String>> {
        Ok(self.files.get(&normalize_store_path(path)).cloned())
    }
    async fn write(&mut self, path: &str, content: &str) -> Result<()> {
        self.files
            .insert(normalize_store_path(path), content.to_string());
        Ok(())
    }
    async fn delete(&mut self, path: &str) -> Result<()> {
        self.files.remove(&normalize_store_path(path));
        Ok(())
    }
    async fn exists(&mut self, path: &str) -> Result<bool> {
        Ok(self.files.contains_key(&normalize_store_path(path)))
    }
    async fn mkdir_p(&mut self, _path: &str) -> Result<()> {
        Ok(())
    }
    async fn list(&mut self, prefix: &str) -> Result<Vec<String>> {
        Ok(self
            .files
            .keys()
            .filter(|k| path_under(k, prefix))
            .cloned()
            .collect())
    }
}

/// Whether `path` is at or under directory/path `prefix`.
pub fn path_under(path: &str, prefix: &str) -> bool {
    let p = prefix.trim_end_matches('/');
    p.is_empty() || path == p || path.starts_with(&format!("{p}/"))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn apply(
        store: &mut InMemoryFs,
        body: &str,
    ) -> std::result::Result<AffectedPaths, ApplyError> {
        let text = format!("*** Begin Patch\n{body}\n*** End Patch");
        let hunks = super::super::parser::parse_patch(&text).unwrap();
        apply_patch_to_store(store, &hunks).await
    }

    #[tokio::test]
    async fn add_update_delete_end_to_end() {
        let mut fs = InMemoryFs::new();
        apply(&mut fs, "*** Add File: a.txt\n+hello").await.unwrap();
        assert_eq!(fs.get("a.txt").unwrap(), "hello\n");
        apply(&mut fs, "*** Update File: a.txt\n@@\n-hello\n+goodbye")
            .await
            .unwrap();
        assert_eq!(fs.get("a.txt").unwrap(), "goodbye\n");
        apply(&mut fs, "*** Delete File: a.txt").await.unwrap();
        assert!(fs.get("a.txt").is_none());
    }

    #[tokio::test]
    async fn update_missing_file_errors() {
        let mut fs = InMemoryFs::new();
        let e = apply(&mut fs, "*** Update File: nope.txt\n@@\n-x\n+y")
            .await
            .unwrap_err();
        assert!(matches!(e, ApplyError::MissingFile(_)));
    }

    #[tokio::test]
    async fn update_self_move_preserves_updated_file() {
        let mut fs = InMemoryFs::with(&[("a.txt", "hello\n"), ("dir/b.txt", "one\n")]);
        let affected = apply(
            &mut fs,
            "*** Update File: a.txt\n*** Move to: a.txt\n@@\n-hello\n+goodbye",
        )
        .await
        .unwrap();
        assert_eq!(fs.get("a.txt").map(String::as_str), Some("goodbye\n"));
        assert_eq!(affected.modified, vec!["a.txt".to_string()]);
        assert!(affected.deleted.is_empty());

        // Equivalent store path spellings (`a.txt` vs `./a.txt`, `dir/b.txt` vs `dir/./b.txt`)
        // must also be treated as self-moves and never delete the updated file.
        let affected_dot = apply(
            &mut fs,
            "*** Update File: a.txt\n*** Move to: ./a.txt\n@@\n-goodbye\n+still here",
        )
        .await
        .unwrap();
        assert_eq!(fs.get("a.txt").map(String::as_str), Some("still here\n"));
        assert_eq!(affected_dot.modified, vec!["a.txt".to_string()]);
        assert!(affected_dot.deleted.is_empty());

        let affected_sub = apply(
            &mut fs,
            "*** Update File: ./dir/b.txt\n*** Move to: dir/./b.txt\n@@\n-one\n+two",
        )
        .await
        .unwrap();
        assert_eq!(fs.get("dir/b.txt").map(String::as_str), Some("two\n"));
        assert_eq!(affected_sub.modified, vec!["./dir/b.txt".to_string()]);
        assert!(affected_sub.deleted.is_empty());
    }

    #[tokio::test]
    async fn a_failing_later_hunk_leaves_earlier_files_untouched() {
        let mut fs = InMemoryFs::with(&[("a.txt", "one\n"), ("b.txt", "two\n")]);
        let e = apply(
            &mut fs,
            "*** Update File: a.txt\n@@\n-one\n+uno\n*** Delete File: b.txt\n*** Update File: c.txt\n@@\n-x\n+y",
        )
        .await
        .unwrap_err();
        assert!(matches!(e, ApplyError::MissingFile(_)));
        assert_eq!(fs.get("a.txt").map(String::as_str), Some("one\n"));
        assert_eq!(fs.get("b.txt").map(String::as_str), Some("two\n"));
    }

    #[tokio::test]
    async fn deleting_a_missing_file_is_refused() {
        let mut fs = InMemoryFs::new();
        let e = apply(&mut fs, "*** Delete File: nope.txt").await.unwrap_err();
        assert!(matches!(e, ApplyError::MissingFile(_)));
    }

    #[tokio::test]
    async fn later_hunks_see_earlier_staged_changes() {
        let mut fs = InMemoryFs::new();
        apply(
            &mut fs,
            "*** Add File: a.txt\n+hello\n*** Update File: a.txt\n@@\n-hello\n+goodbye",
        )
        .await
        .unwrap();
        assert_eq!(fs.get("a.txt").map(String::as_str), Some("goodbye\n"));
    }

    fn chunk(ctx: Option<&str>, old: &[&str], new: &[&str], eof: bool) -> UpdateFileChunk {
        UpdateFileChunk {
            change_context: ctx.map(str::to_string),
            old_lines: old.iter().map(|s| s.to_string()).collect(),
            new_lines: new.iter().map(|s| s.to_string()).collect(),
            is_end_of_file: eof,
        }
    }

    #[test]
    fn trailing_blank_lines_survive_an_edit_elsewhere() {
        let out = derive_new_contents("a\nb\n\n", &[chunk(None, &["a"], &["A"], false)], "f")
            .unwrap();
        assert_eq!(out, "A\nb\n\n");
        let out = derive_new_contents("a\n", &[chunk(None, &["a"], &[""], false)], "f").unwrap();
        assert_eq!(out, "\n");
    }

    #[test]
    fn crlf_files_keep_crlf() {
        let out = derive_new_contents("a\r\nb\r\n", &[chunk(None, &["a"], &["A"], false)], "f")
            .unwrap();
        assert_eq!(out, "A\r\nb\r\n");
    }

    #[test]
    fn a_pure_insertion_lands_after_its_context_line() {
        let out = derive_new_contents(
            "fn a() {}\nfn b() {}\nfn c() {}\n",
            &[chunk(Some("fn a() {}"), &[], &["// after a"], false)],
            "f",
        )
        .unwrap();
        assert_eq!(out, "fn a() {}\n// after a\nfn b() {}\nfn c() {}\n");
    }

    #[test]
    fn overlapping_chunks_are_refused() {
        let e = derive_new_contents(
            "a\nb\nc\n",
            &[
                chunk(None, &["b", "c"], &["B", "C"], false),
                chunk(None, &["c"], &["Z"], true),
            ],
            "f",
        )
        .unwrap_err();
        assert!(matches!(e, ApplyError::Compute(_)));
    }

    #[tokio::test]
    async fn a_pure_rename_moves_the_bytes_verbatim() {
        let mut fs = InMemoryFs::with(&[("old.txt", "keep\n\nexactly")]);
        apply(&mut fs, "*** Update File: old.txt\n*** Move to: new.txt")
            .await
            .unwrap();
        assert!(fs.get("old.txt").is_none());
        assert_eq!(fs.get("new.txt").map(String::as_str), Some("keep\n\nexactly"));
    }

    #[tokio::test]
    async fn stacked_context_markers_narrow_the_location() {
        let mut fs = InMemoryFs::with(&[(
            "a.py",
            "class A:\n    def m():\n        pass\nclass B:\n    def m():\n        pass\n",
        )]);
        apply(
            &mut fs,
            "*** Update File: a.py\n@@ class B:\n@@     def m():\n-        pass\n+        return 1",
        )
        .await
        .unwrap();
        assert_eq!(
            fs.get("a.py").map(String::as_str),
            Some("class A:\n    def m():\n        pass\nclass B:\n    def m():\n        return 1\n")
        );
    }

    #[tokio::test]
    async fn a_leading_blank_context_line_is_kept() {
        let mut fs = InMemoryFs::with(&[("a.txt", "y\n\ny\n")]);
        apply(&mut fs, "*** Update File: a.txt\n \n-y\n+Y").await.unwrap();
        assert_eq!(fs.get("a.txt").map(String::as_str), Some("y\n\nY\n"));
    }

    #[test]
    fn path_under_semantics() {
        assert!(path_under("src/a.rs", "src"));
        assert!(path_under("src/a.rs", ""));
        assert!(!path_under("tests/a.rs", "src"));
    }
}
