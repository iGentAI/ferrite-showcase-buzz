//! The `apply_patch` grammar parser. Ported from Codex `apply-patch/src/parser.rs`.
//! Parses a patch string into a list of [`Hunk`]s; does NOT touch
//! the filesystem. Lenient mode (default) additionally strips a `<<'EOF' … EOF`
//! heredoc wrapper that some models emit.
//!
//! Lark grammar (model-facing contract, verbatim from codex):
//! ```text
//! start: begin_patch environment_id? hunk+ end_patch
//! begin_patch: "*** Begin Patch" LF
//! end_patch: "*** End Patch" LF?
//! hunk: add_hunk | delete_hunk | update_hunk
//! add_hunk: "*** Add File: " filename LF add_line+
//! delete_hunk: "*** Delete File: " filename LF
//! update_hunk: "*** Update File: " filename LF change_move? change?
//! change_move: "*** Move to: " filename LF
//! change: (change_context | change_line)+ eof_line?
//! change_context: ("@@" | "@@ " /(.+)/) LF
//! change_line: ("+" | "-" | " ") /(.+)/ LF
//! eof_line: "*** End of File" LF
//! ```

pub const BEGIN_PATCH_MARKER: &str = "*** Begin Patch";
pub const ENVIRONMENT_ID_MARKER: &str = "*** Environment ID: ";
pub const END_PATCH_MARKER: &str = "*** End Patch";
pub const ADD_FILE_MARKER: &str = "*** Add File: ";
pub const DELETE_FILE_MARKER: &str = "*** Delete File: ";
pub const UPDATE_FILE_MARKER: &str = "*** Update File: ";
pub const MOVE_TO_MARKER: &str = "*** Move to: ";
pub const EOF_MARKER: &str = "*** End of File";
pub const CHANGE_CONTEXT_MARKER: &str = "@@ ";
pub const EMPTY_CHANGE_CONTEXT_MARKER: &str = "@@";

/// gpt-4.1 is the only model known to require lenient parsing; codex enables it
/// for everyone to avoid threading a strictness param. We mirror that default.
const PARSE_IN_STRICT_MODE: bool = false;

/// A parse error. `Display` matches codex's `thiserror` formats exactly.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PatchParseError {
    InvalidPatch(String),
    InvalidHunk { message: String, line_number: usize },
}

impl std::fmt::Display for PatchParseError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            PatchParseError::InvalidPatch(m) => write!(f, "invalid patch: {m}"),
            PatchParseError::InvalidHunk {
                message,
                line_number,
            } => {
                write!(f, "invalid hunk at line {line_number}, {message}")
            }
        }
    }
}
impl std::error::Error for PatchParseError {}

use PatchParseError::{InvalidHunk, InvalidPatch};

/// A parsed file operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Hunk {
    AddFile {
        path: String,
        contents: String,
    },
    DeleteFile {
        path: String,
    },
    UpdateFile {
        path: String,
        move_path: Option<String>,
        chunks: Vec<UpdateFileChunk>,
    },
}

impl Hunk {
    /// The path affected, using the move destination for renames.
    pub fn path(&self) -> &str {
        match self {
            Hunk::AddFile { path, .. } => path,
            Hunk::DeleteFile { path } => path,
            Hunk::UpdateFile {
                move_path: Some(p), ..
            } => p,
            Hunk::UpdateFile {
                path,
                move_path: None,
                ..
            } => path,
        }
    }
}

/// One contiguous change region within an `UpdateFile` hunk.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UpdateFileChunk {
    /// A single context line (`@@ ...`) used to narrow the chunk location.
    pub change_context: Option<String>,
    /// Lines that must be present (context + removed), in order.
    pub old_lines: Vec<String>,
    /// Lines to write (context + added), in order.
    pub new_lines: Vec<String>,
    /// Whether `old_lines` must occur at the end of the file.
    pub is_end_of_file: bool,
}

#[derive(Clone, Copy)]
enum ParseMode {
    Strict,
    /// Strip a `<<'EOF' … EOF` heredoc wrapper before parsing (some models emit it).
    Lenient,
}

/// Parse a patch string into its hunks (lenient mode by default). The
/// `*** Environment ID:` preamble, if present, is parsed and discarded.
pub fn parse_patch(patch: &str) -> Result<Vec<Hunk>, PatchParseError> {
    let mode = if PARSE_IN_STRICT_MODE {
        ParseMode::Strict
    } else {
        ParseMode::Lenient
    };
    parse_patch_text(patch, mode)
}

fn parse_patch_text(patch: &str, mode: ParseMode) -> Result<Vec<Hunk>, PatchParseError> {
    let lines: Vec<&str> = patch.trim().lines().collect();
    let (_patch_lines, hunk_lines) = match mode {
        ParseMode::Strict => check_patch_boundaries_strict(&lines)?,
        ParseMode::Lenient => check_patch_boundaries_lenient(&lines)?,
    };

    let (_environment_id, mut remaining, mut line_number) =
        parse_environment_id_preamble(hunk_lines)?;
    let mut hunks: Vec<Hunk> = Vec::new();
    while !remaining.is_empty() {
        let (hunk, consumed) = parse_one_hunk(remaining, line_number)?;
        hunks.push(hunk);
        line_number += consumed;
        remaining = &remaining[consumed..];
    }
    Ok(hunks)
}

fn parse_environment_id_preamble<'a>(
    hunk_lines: &'a [&'a str],
) -> Result<(Option<String>, &'a [&'a str], usize), PatchParseError> {
    let Some(first) = hunk_lines.first() else {
        return Ok((None, hunk_lines, 2));
    };
    let Some(env_id) = first.trim_start().strip_prefix(ENVIRONMENT_ID_MARKER) else {
        return Ok((None, hunk_lines, 2));
    };
    let env_id = env_id.trim();
    if env_id.is_empty() {
        return Err(InvalidPatch(
            "apply_patch environment_id cannot be empty".to_string(),
        ));
    }
    Ok((Some(env_id.to_string()), &hunk_lines[1..], 3))
}

fn check_patch_boundaries_strict<'a>(
    lines: &'a [&'a str],
) -> Result<(&'a [&'a str], &'a [&'a str]), PatchParseError> {
    let (first, last) = match lines {
        [] => (None, None),
        [first] => (Some(first), Some(first)),
        [first, .., last] => (Some(first), Some(last)),
    };
    check_start_and_end_lines_strict(first, last)?;
    Ok((lines, &lines[1..lines.len() - 1]))
}

fn check_patch_boundaries_lenient<'a>(
    original: &'a [&'a str],
) -> Result<(&'a [&'a str], &'a [&'a str]), PatchParseError> {
    let original_err = match check_patch_boundaries_strict(original) {
        Ok(ok) => return Ok(ok),
        Err(e) => e,
    };
    match original {
        [first, .., last] => {
            if (first == &"<<EOF" || first == &"<<'EOF'" || first == &"<<\"EOF\"")
                && last.ends_with("EOF")
                && original.len() >= 4
            {
                check_patch_boundaries_strict(&original[1..original.len() - 1])
            } else {
                Err(original_err)
            }
        }
        _ => Err(original_err),
    }
}

fn check_start_and_end_lines_strict(
    first: Option<&&str>,
    last: Option<&&str>,
) -> Result<(), PatchParseError> {
    let first = first.map(|l| l.trim());
    let last = last.map(|l| l.trim());
    match (first, last) {
        (Some(f), Some(l)) if f == BEGIN_PATCH_MARKER && l == END_PATCH_MARKER => Ok(()),
        (Some(f), _) if f != BEGIN_PATCH_MARKER => Err(InvalidPatch(
            "The first line of the patch must be '*** Begin Patch'".to_string(),
        )),
        _ => Err(InvalidPatch(
            "The last line of the patch must be '*** End Patch'".to_string(),
        )),
    }
}

fn parse_one_hunk(lines: &[&str], line_number: usize) -> Result<(Hunk, usize), PatchParseError> {
    let first = lines[0].trim();
    if let Some(path) = first.strip_prefix(ADD_FILE_MARKER) {
        let mut contents = String::new();
        let mut parsed = 1;
        for add_line in &lines[1..] {
            if let Some(to_add) = add_line.strip_prefix('+') {
                contents.push_str(to_add);
                contents.push('\n');
                parsed += 1;
            } else {
                break;
            }
        }
        return Ok((
            Hunk::AddFile {
                path: path.to_string(),
                contents,
            },
            parsed,
        ));
    } else if let Some(path) = first.strip_prefix(DELETE_FILE_MARKER) {
        return Ok((
            Hunk::DeleteFile {
                path: path.to_string(),
            },
            1,
        ));
    } else if let Some(path) = first.strip_prefix(UPDATE_FILE_MARKER) {
        let mut remaining = &lines[1..];
        let mut parsed = 1;
        let move_path = remaining
            .first()
            .and_then(|x| x.strip_prefix(MOVE_TO_MARKER));
        if move_path.is_some() {
            remaining = &remaining[1..];
            parsed += 1;
        }
        let mut chunks = Vec::new();
        while !remaining.is_empty() {
            // Only a truly empty line is skipped; a ' '-prefixed blank line is
            // leading context and belongs to the chunk.
            if remaining[0].is_empty() {
                parsed += 1;
                remaining = &remaining[1..];
                continue;
            }
            if remaining[0].starts_with('*') {
                break;
            }
            let (chunk, consumed) =
                parse_update_file_chunk(remaining, line_number + parsed, chunks.is_empty())?;
            chunks.push(chunk);
            parsed += consumed;
            remaining = &remaining[consumed..];
        }
        // A pure rename (`*** Move to:` with no chunks) is a valid hunk.
        if chunks.is_empty() && move_path.is_none() {
            return Err(InvalidHunk {
                message: format!("Update file hunk for path '{path}' is empty"),
                line_number,
            });
        }
        return Ok((
            Hunk::UpdateFile {
                path: path.to_string(),
                move_path: move_path.map(str::to_string),
                chunks,
            },
            parsed,
        ));
    }
    Err(InvalidHunk {
        message: format!(
            "'{first}' is not a valid hunk header. Valid hunk headers: '*** Add File: {{path}}', '*** Delete File: {{path}}', '*** Update File: {{path}}'"
        ),
        line_number,
    })
}

fn parse_update_file_chunk(
    lines: &[&str],
    line_number: usize,
    allow_missing_context: bool,
) -> Result<(UpdateFileChunk, usize), PatchParseError> {
    if lines.is_empty() {
        return Err(InvalidHunk {
            message: "Update hunk does not contain any lines".to_string(),
            line_number,
        });
    }
    let (change_context, start_index) = if lines[0] == EMPTY_CHANGE_CONTEXT_MARKER {
        (None, 1)
    } else if let Some(ctx) = lines[0].strip_prefix(CHANGE_CONTEXT_MARKER) {
        (Some(ctx.to_string()), 1)
    } else {
        if !allow_missing_context {
            return Err(InvalidHunk {
                message: format!(
                    "Expected update hunk to start with a @@ context marker, got: '{}'",
                    lines[0]
                ),
                line_number,
            });
        }
        (None, 0)
    };
    if start_index >= lines.len() {
        return Err(InvalidHunk {
            message: "Update hunk does not contain any lines".to_string(),
            line_number: line_number + 1,
        });
    }
    // Stacked `@@` markers (`@@ class A` then `@@ def m():`) narrow the
    // location step by step: an outer marker becomes a context-only chunk
    // and the next marker starts the chunk that carries the lines.
    if change_context.is_some()
        && (lines[1] == EMPTY_CHANGE_CONTEXT_MARKER || lines[1].starts_with(CHANGE_CONTEXT_MARKER))
    {
        return Ok((
            UpdateFileChunk {
                change_context,
                old_lines: Vec::new(),
                new_lines: Vec::new(),
                is_end_of_file: false,
            },
            1,
        ));
    }
    let mut chunk = UpdateFileChunk {
        change_context,
        old_lines: Vec::new(),
        new_lines: Vec::new(),
        is_end_of_file: false,
    };
    let mut parsed = 0;
    for line in &lines[start_index..] {
        match *line {
            EOF_MARKER => {
                if parsed == 0 {
                    return Err(InvalidHunk {
                        message: "Update hunk does not contain any lines".to_string(),
                        line_number: line_number + 1,
                    });
                }
                chunk.is_end_of_file = true;
                parsed += 1;
                break;
            }
            contents => match contents.chars().next() {
                None => {
                    chunk.old_lines.push(String::new());
                    chunk.new_lines.push(String::new());
                }
                Some(' ') => {
                    chunk.old_lines.push(contents[1..].to_string());
                    chunk.new_lines.push(contents[1..].to_string());
                }
                Some('+') => chunk.new_lines.push(contents[1..].to_string()),
                Some('-') => chunk.old_lines.push(contents[1..].to_string()),
                _ => {
                    if parsed == 0 {
                        return Err(InvalidHunk {
                            message: format!(
                                "Unexpected line found in update hunk: '{contents}'. Every line should start with ' ' (context line), '+' (added line), or '-' (removed line)"
                            ),
                            line_number: line_number + 1,
                        });
                    }
                    break; // start of the next chunk
                }
            },
        }
        parsed += 1;
    }
    Ok((chunk, parsed + start_index))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wrap(body: &str) -> String {
        format!("*** Begin Patch\n{body}\n*** End Patch")
    }

    #[test]
    fn parse_add_delete_update_move() {
        let p = wrap(
            "*** Add File: a.txt\n+hello\n+world\n*** Delete File: b.txt\n*** Update File: c.txt\n*** Move to: d.txt\n@@ fn main\n context\n-old\n+new",
        );
        let hunks = parse_patch(&p).unwrap();
        assert_eq!(hunks.len(), 3);
        assert_eq!(
            hunks[0],
            Hunk::AddFile {
                path: "a.txt".into(),
                contents: "hello\nworld\n".into()
            }
        );
        assert_eq!(
            hunks[1],
            Hunk::DeleteFile {
                path: "b.txt".into()
            }
        );
        match &hunks[2] {
            Hunk::UpdateFile {
                path,
                move_path,
                chunks,
            } => {
                assert_eq!(path, "c.txt");
                assert_eq!(move_path.as_deref(), Some("d.txt"));
                assert_eq!(chunks[0].change_context.as_deref(), Some("fn main"));
                assert_eq!(chunks[0].old_lines, vec!["context", "old"]);
                assert_eq!(chunks[0].new_lines, vec!["context", "new"]);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn missing_boundaries_error() {
        // starts with Begin but never ends with End → last-line error.
        assert_eq!(
            parse_patch("*** Begin Patch\n*** Update File: x"),
            Err(InvalidPatch(
                "The last line of the patch must be '*** End Patch'".into()
            ))
        );
        assert_eq!(
            parse_patch("not a patch\n*** End Patch"),
            Err(InvalidPatch(
                "The first line of the patch must be '*** Begin Patch'".into()
            ))
        );
    }

    #[test]
    fn lenient_strips_heredoc() {
        let p = format!("<<'EOF'\n{}\nEOF", wrap("*** Add File: a\n+x"));
        let hunks = parse_patch(&p).unwrap();
        assert_eq!(
            hunks[0],
            Hunk::AddFile {
                path: "a".into(),
                contents: "x\n".into()
            }
        );
    }

    #[test]
    fn bad_hunk_header() {
        let err = parse_patch(&wrap("bad header line")).unwrap_err();
        match err {
            InvalidHunk { message, .. } => assert!(message.contains("is not a valid hunk header")),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn eof_marker_sets_flag() {
        let hunks = parse_patch(&wrap("*** Update File: x\n@@\n-a\n+b\n*** End of File")).unwrap();
        match &hunks[0] {
            Hunk::UpdateFile { chunks, .. } => assert!(chunks[0].is_end_of_file),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn empty_environment_id_rejected() {
        let err = parse_patch(&wrap("*** Environment ID: \n*** Add File: a\n+x")).unwrap_err();
        assert_eq!(
            err,
            InvalidPatch("apply_patch environment_id cannot be empty".into())
        );
    }
}
