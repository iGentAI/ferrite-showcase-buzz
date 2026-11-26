//! The `apply_patch` edit tool. `parser` + `seek_sequence` + the line transform
//! in `apply` are PURE; the filesystem is injected via [`apply::FileStore`], and
//! `workspace_fs` provides the Workspace-backed stores.

pub mod apply;
pub mod parser;
pub mod seek_sequence;
pub mod workspace_fs;

pub use apply::{
    apply_patch_to_store, derive_new_contents, format_summary, path_under, AffectedPaths,
    ApplyError, FileStore, InMemoryFs, VersionedFileStore,
};
pub use parser::{parse_patch, Hunk, PatchParseError, UpdateFileChunk};
pub use workspace_fs::{WorkspaceFs, WorkspaceFsRef};

use ferrite_app::serde_json::{self, Value};

pub const APPLY_PATCH_TOOL_NAME: &str = "apply_patch";

/// The verbatim model-facing instructions.
pub const APPLY_PATCH_INSTRUCTIONS: &str =
    include_str!("../templates/tools/apply_patch_instructions.md");

/// JSON tool spec for `apply_patch`.
pub fn apply_patch_spec() -> Value {
    serde_json::json!({
        "name": APPLY_PATCH_TOOL_NAME,
        "description": APPLY_PATCH_INSTRUCTIONS,
        "strict": false,
        "parameters": {
            "type": "object",
            "properties": {
                "input": {
                    "type": "string",
                    "description": "The full apply_patch envelope, beginning with '*** Begin Patch' and ending with '*** End Patch'."
                }
            },
            "required": ["input"],
            "additionalProperties": false
        }
    })
}

/// Extract the patch text from the tool args, accepting either a `{"input": "..."}`
/// JSON wrapper or the raw freeform patch body.
pub fn extract_patch_text(args: &[u8]) -> String {
    let s = String::from_utf8_lossy(args);
    let trimmed = s.trim_start();
    if trimmed.starts_with('{') {
        if let Ok(v) = ferrite_app::serde_json::from_str::<Value>(s.trim()) {
            if let Some(input) = v.get("input").and_then(Value::as_str) {
                return input.to_string();
            }
        }
    }
    s.into_owned()
}

/// Validate that every path a patch touches stays inside the writable root.
pub fn validate_patch_paths(hunks: &[Hunk]) -> std::result::Result<(), String> {
    for hunk in hunks {
        let mut paths: Vec<&str> = vec![hunk.path()];
        if let Hunk::UpdateFile {
            path,
            move_path: Some(_),
            ..
        } = hunk
        {
            paths.push(path);
        }
        for path in paths {
            validate_one_path(path)?;
        }
    }
    Ok(())
}

const MAX_PATH_BYTES: usize = 1024;

fn path_reason(path: &str) -> Option<String> {
    if path.trim().is_empty() {
        return Some("empty path".to_string());
    }
    if path.len() > MAX_PATH_BYTES {
        return Some(format!("path too long: '{path}'"));
    }
    if path.starts_with('/') || (path.len() >= 2 && path.as_bytes()[1] == b':') {
        return Some(format!("absolute path not allowed: '{path}'"));
    }
    if path.as_bytes().contains(&0x1f) {
        return Some(format!("path contains the structural separator: '{path}'"));
    }
    if path.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Some(format!("path contains a control character: {path:?}"));
    }
    // A backslash is a separator on Windows, so `..\..\x` would escape the
    // workspace when it is exported there.
    if path.contains('\\') {
        return Some(format!("backslash not allowed in path: '{path}'"));
    }
    for comp in path.split('/') {
        if comp == ".." {
            return Some(format!("parent-directory traversal not allowed: '{path}'"));
        }
    }
    if path.ends_with('/') || apply::normalize_store_path(path).is_empty() {
        return Some(format!("path does not name a file: '{path}'"));
    }
    None
}

/// Validate a workspace-relative path for the file tools.
pub fn validate_path(path: &str) -> std::result::Result<(), String> {
    match path_reason(path) {
        Some(r) => Err(format!("invalid path: {r}")),
        None => Ok(()),
    }
}

fn validate_one_path(path: &str) -> std::result::Result<(), String> {
    match path_reason(path) {
        Some(r) => Err(format!("patch rejected: {r}")),
        None => Ok(()),
    }
}

/// Run the `apply_patch` tool against a [`FileStore`], returning the model-facing
/// result bytes. Never panics.
pub async fn run_apply_patch<S: FileStore>(store: &mut S, args: &[u8]) -> Vec<u8> {
    let text = extract_patch_text(args);
    if text.trim().is_empty() {
        return b"patch rejected: empty patch".to_vec();
    }
    let hunks = match parse_patch(&text) {
        Ok(h) => h,
        Err(e) => return e.to_string().into_bytes(),
    };
    if hunks.is_empty() {
        return b"patch rejected: empty patch".to_vec();
    }
    if let Err(msg) = validate_patch_paths(&hunks) {
        return msg.into_bytes();
    }
    match apply_patch_to_store(store, &hunks).await {
        Ok(aff) => format_summary(&aff).into_bytes(),
        Err(e) => format!("apply_patch verification failed: {e}").into_bytes(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(body: &str) -> String {
        format!("*** Begin Patch\n{body}\n*** End Patch")
    }

    #[test]
    fn extract_handles_freeform_and_json_wrapper() {
        let raw = wrap("*** Add File: a\n+x");
        assert_eq!(extract_patch_text(raw.as_bytes()), raw);
        let wrapped = serde_json::json!({ "input": raw }).to_string();
        assert_eq!(extract_patch_text(wrapped.as_bytes()), raw);
    }

    #[test]
    fn spec_carries_instructions() {
        let s = apply_patch_spec();
        assert_eq!(s["name"], "apply_patch");
        assert!(s["description"].as_str().unwrap().contains("Begin Patch"));
        assert_eq!(s["parameters"]["required"], serde_json::json!(["input"]));
    }

    #[test]
    fn path_safety_rejects_escapes() {
        let bad = parse_patch(&wrap("*** Add File: ../../etc/evil\n+x")).unwrap();
        assert!(validate_patch_paths(&bad).is_err());
        let abs = parse_patch(&wrap("*** Add File: /etc/evil\n+x")).unwrap();
        assert!(validate_patch_paths(&abs).is_err());
        let ok = parse_patch(&wrap("*** Add File: src/ok.txt\n+x")).unwrap();
        assert!(validate_patch_paths(&ok).is_ok());
    }

    #[test]
    fn path_safety_rejects_backslashes_controls_and_non_files() {
        for bad in [
            "..\\..\\evil",
            "sub\\..\\..\\evil",
            "\\\\server\\share\\x",
            ".",
            "./",
            "./.",
            "src/",
            "a\0b",
            "a\nb",
            "a\rb",
            "a\u{7f}b",
        ] {
            assert!(validate_path(bad).is_err(), "{bad:?} must be refused");
        }
        assert!(validate_path("./src/ok.txt").is_ok());
        assert!(validate_path("src/ok.txt").is_ok());
    }

    #[tokio::test]
    async fn run_returns_summary_or_error_bytes_never_panics() {
        let mut fs = InMemoryFs::new();
        assert_eq!(
            run_apply_patch(&mut fs, b"").await,
            b"patch rejected: empty patch"
        );
        let out = run_apply_patch(&mut fs, b"total garbage not a patch").await;
        assert!(String::from_utf8_lossy(&out).contains("invalid patch"));
        let ok = run_apply_patch(&mut fs, wrap("*** Add File: a.txt\n+hi").as_bytes()).await;
        assert_eq!(
            String::from_utf8_lossy(&ok),
            "Success. Updated the following files:\nA a.txt\n"
        );
        assert_eq!(fs.get("a.txt").unwrap(), "hi\n");
        let esc = run_apply_patch(&mut fs, wrap("*** Add File: ../evil\n+x").as_bytes()).await;
        assert!(String::from_utf8_lossy(&esc).contains("traversal not allowed"));
    }
}
