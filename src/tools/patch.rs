//! `apply_patch` — the single write primitive (spec section 7.5).
//!
//! Accepts a unified diff (created with `diff -u`, `git diff`, or produced by
//! an AI client), validates it against the workspace boundary, and applies it
//! per file: each file's new content is built fully in memory and only then
//! written atomically. A failed hunk aborts the whole patch without
//! corrupting any file.

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use rmcp::schemars;
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ApplyPatchArgs {
    /// Unified diff text (`diff -u` / `git diff` format). Multi-file diffs are applied as one unit: every hunk must apply cleanly or nothing is written.
    #[schemars(
        description = "Unified diff text in 'diff -u'/'git diff' format. May contain multiple files; all hunks must apply cleanly or nothing is written."
    )]
    pub patch: String,
}

#[derive(Debug)]
struct FilePatch {
    old_path: String,
    new_path: String,
    /// None means file creation (only additions, no expected context).
    creation: bool,
    hunks: Vec<Hunk>,
}

#[derive(Debug)]
struct Hunk {
    old_start: u64,
    ops: Vec<LineOp>,
}

/// One line of a hunk body.
#[derive(Debug, Clone, PartialEq, Eq)]
enum LineOp {
    Context(String),
    Remove(String),
    Add(String),
}

impl Hunk {
    fn expected(&self) -> Vec<&str> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                LineOp::Context(s) | LineOp::Remove(s) => Some(s.as_str()),
                LineOp::Add(_) => None,
            })
            .collect()
    }

    fn replacement(&self) -> Vec<String> {
        self.ops
            .iter()
            .filter_map(|op| match op {
                LineOp::Context(s) | LineOp::Add(s) => Some(s.clone()),
                LineOp::Remove(_) => None,
            })
            .collect()
    }

    /// Declared old-line count from the @@ header (None if absent).
    fn declared_old_len(header: &str) -> Option<u64> {
        // header like "@@ -3,7 +3,8 @@ optional section"
        let spec = header.split_whitespace().nth(1)?;
        let (_, len_str) = spec.strip_prefix('-')?.split_once(',')?;
        len_str.parse().ok()
    }
}

// ---------------------------------------------------------------------------
// Tool entry point
// ---------------------------------------------------------------------------

pub(crate) fn handle(state: &AppState, args: ApplyPatchArgs) -> ToolResult<serde_json::Value> {
    state.permissions().require_write()?;
    let ws = state.workspace();

    if args.patch.trim().is_empty() {
        return Err(ToolError::msg("Patch text is empty."));
    }

    for keyword in ["rename from", "copy from", "GIT binary patch"] {
        if args.patch.contains(keyword) {
            return Err(ToolError::msg(format!(
                "Patch contains unsupported '{keyword}' header; regenerate a plain unified diff."
            )));
        }
    }

    let parsed = parse_patch(&args.patch)?;

    // Resolve every target path up front so a workspace escape fails before
    // any disk mutation occurs.
    let mut plans: Vec<(String, PathBuf)> = Vec::new();
    for file in &parsed {
        let requested = strip_a_b(file.new_path.as_str());
        let resolved = ws.resolve(Some(requested)).map_err(|e| {
            ToolError::msg(format!("Rejecting patch target '{}': {e}", file.new_path))
        })?;
        plans.push((ws.display_relative(&resolved), resolved));
    }

    // Build all new contents fully in memory first.
    let mut staged: Vec<(String, PathBuf, Vec<u8>)> = Vec::new();
    for (file, (rel_display, resolved)) in parsed.iter().zip(plans.iter()) {
        let original_bytes = match std::fs::read(resolved) {
            Ok(bytes) => bytes,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                if file.creation {
                    Vec::new()
                } else {
                    return Err(ToolError::msg(format!(
                        "Cannot patch '{}': file does not exist.",
                        file.old_path
                    )));
                }
            }
            Err(e) => {
                return Err(ToolError::msg(format!(
                    "Cannot read '{}': {e}",
                    file.old_path
                )))
            }
        };

        let original_text = decode_utf8(&original_bytes, &file.old_path)?;
        let mut working: Vec<String> = if original_text.is_empty() {
            Vec::new()
        } else {
            original_text.lines().map(String::from).collect()
        };
        // A final newline on a non-empty file; "\ No newline at end of file"
        // markers are tolerated but the common case is preserved.
        let had_trailing_newline = original_text.ends_with('\n');

        for (idx, hunk) in file.hunks.iter().enumerate() {
            apply_hunk(&mut working, hunk, idx + 1, &file.new_path, file.creation)?;
        }

        let mut out = working.join("\n");
        if !out.is_empty() && (had_trailing_newline || file.creation) {
            out.push('\n');
        }
        staged.push((rel_display.clone(), resolved.clone(), out.into_bytes()));
    }

    // Everything applied cleanly in memory — commit to disk now.
    for (_rel, resolved, bytes) in &staged {
        atomic_write(resolved, bytes)?;
        tracing::info!(target = %resolved.display(), "applied patch");
    }

    let changed_files: Vec<String> = staged.into_iter().map(|(rel, _, _)| rel).collect();
    Ok(json!({
        "changed_files": changed_files,
    }))
}

fn strip_a_b(p: &str) -> &str {
    p.strip_prefix("a/")
        .or_else(|| p.strip_prefix("b/"))
        .unwrap_or(p)
}

fn decode_utf8(bytes: &[u8], display: &str) -> ToolResult<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        ToolError::msg(format!(
            "Cannot patch '{display}': file is not valid UTF-8."
        ))
    })
}

/// Write via a sibling temp file + rename so readers never observe a torn file.
fn atomic_write(target: &Path, bytes: &[u8]) -> ToolResult<()> {
    use std::io::Write;
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            ToolError::msg(format!(
                "Failed to create directory '{}': {e}",
                parent.display()
            ))
        })?;
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0);
    let tmp_name = format!(
        ".{}.tmp{}",
        target
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("file"),
        nanos
    );
    let tmp_path = target.with_file_name(tmp_name);
    {
        let mut f = std::fs::File::create(&tmp_path).map_err(|e| {
            ToolError::msg(format!(
                "Failed to create temp file '{}': {e}",
                tmp_path.display()
            ))
        })?;
        f.write_all(bytes)
            .map_err(|e| ToolError::msg(format!("Failed to write temp file: {e}")))?;
    }
    std::fs::rename(&tmp_path, target)
        .map_err(|e| ToolError::msg(format!("Failed to replace '{}': {e}", target.display())))?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Parsing
// ---------------------------------------------------------------------------

fn parse_patch(patch: &str) -> ToolResult<Vec<FilePatch>> {
    let mut files: Vec<FilePatch> = Vec::new();
    let mut lines = patch.lines().peekable();

    while let Some(line) = lines.next() {
        if !line.starts_with("--- ") {
            continue;
        }
        let old_path = unquote(line.trim_start_matches("--- ").trim());
        let new_line = lines.next().ok_or_else(|| {
            ToolError::msg("Malformed patch: '---' header not followed by '+++' header.")
        })?;
        if !new_line.starts_with("+++ ") {
            return Err(ToolError::msg(
                "Malformed patch: expected '+++' header after '---' header.",
            ));
        }
        let new_path = unquote(new_line.trim_start_matches("+++ ").trim());

        let old_target = strip_a_b(old_path.as_str());
        if old_target != "/dev/null" && strip_a_b(new_path.as_str()) == "/dev/null" {
            return Err(ToolError::msg(format!(
                "Patch deletes '{old_target}'; file deletion is not supported by apply_patch. \
                 Remove the file manually with run_command if --exec is enabled."
            )));
        }
        let creation = old_target == "/dev/null";

        let mut hunks: Vec<Hunk> = Vec::new();
        while let Some(h) = lines.peek().copied() {
            if h.starts_with("@@ -") {
                lines.next();
                hunks.push(parse_hunk(h, &mut lines)?);
            } else {
                break;
            }
        }

        if hunks.is_empty() {
            return Err(ToolError::msg(format!(
                "Malformed patch: no '@@' hunks found after headers for '{new_path}'. Include full @@ sections.",
            )));
        }

        files.push(FilePatch {
            old_path,
            new_path,
            creation,
            hunks,
        });
    }

    if files.is_empty() {
        return Err(ToolError::msg(
            "No usable unified-diff sections found. Expected lines starting with '--- ', '+++ ', '@@', '+', '-', or ' '.",
        ));
    }
    Ok(files)
}

fn unquote(raw: &str) -> String {
    let raw = raw.trim();
    if raw.len() >= 2 && raw.starts_with('"') && raw.ends_with('"') {
        raw[1..raw.len() - 1].to_string()
    } else {
        // Strip tab-separated timestamp suffix ("file\t2026-01-01 ...").
        raw.split('\t').next().unwrap_or(raw).to_string()
    }
}

fn parse_hunk<'a>(
    header: &str,
    lines: &mut std::iter::Peekable<std::str::Lines<'a>>,
) -> ToolResult<Hunk> {
    let old_spec = header
        .trim_start_matches("@@")
        .split_whitespace()
        .next()
        .ok_or_else(|| ToolError::msg("Malformed patch: empty '@@' header."))?;

    if !old_spec.starts_with('-') {
        return Err(ToolError::msg(
            "Malformed patch: first range in '@@' header must start with '-'.",
        ));
    }
    let old_num = old_spec
        .trim_start_matches('-')
        .split(',')
        .next()
        .unwrap_or("1");
    let old_start: u64 = old_num.parse().map_err(|_| {
        ToolError::msg(format!(
            "Malformed patch: bad line number '{old_num}' in '@@' header."
        ))
    })?;
    if old_start == 0 && Hunk::declared_old_len(header).is_none() {
        return Err(ToolError::msg(
            "Malformed patch: '@@ -0,0' style headers must include an explicit ',0' length.",
        ));
    }

    let mut ops: Vec<LineOp> = Vec::new();
    while let Some(l) = lines.peek().copied() {
        if l.starts_with("@@")
            || l.starts_with("--- ")
            || l.starts_with("diff --git")
            || l.starts_with("Index: ")
        {
            break;
        }
        if l.starts_with('\\') {
            // "\ No newline at end of file" and friends — no line content.
            lines.next();
            continue;
        }
        let (kind, rest) = l.split_at(l.chars().next().map_or(0, char::len_utf8));
        match kind {
            " " => ops.push(LineOp::Context(rest.to_string())),
            "-" => ops.push(LineOp::Remove(rest.to_string())),
            "+" => ops.push(LineOp::Add(rest.to_string())),
            "" => ops.push(LineOp::Context(String::new())), // trimmed empty context line
            _ => break,
        }
        lines.next();
    }

    if ops.is_empty() {
        return Err(ToolError::msg("Malformed patch: hunk has no body lines."));
    }

    let declared = Hunk::declared_old_len(header);
    if old_start == 0 && declared == Some(0) {
        // Creation-style hunk: only '+' lines may appear.
        if ops.iter().any(|op| !matches!(op, LineOp::Add(_))) {
            return Err(ToolError::msg(
                "Malformed patch: a creation hunk ('@@ -0,0') must contain only added ('+') lines.",
            ));
        }
    } else if let Some(expected_count) = declared {
        let actual = ops
            .iter()
            .filter(|op| matches!(op, LineOp::Context(_) | LineOp::Remove(_)))
            .count() as u64;
        if actual != expected_count {
            return Err(ToolError::msg(format!(
                "Malformed patch: '@@' header declares {expected_count} old-side lines but body has {actual}."
            )));
        }
    }

    Ok(Hunk {
        old_start: old_start.max(1),
        ops,
    })
}

// ---------------------------------------------------------------------------
// Application
// ---------------------------------------------------------------------------

/// Apply one hunk at its stated position, falling back to a ±100-line scan,
/// mirroring how AI clients produce slightly stale line numbers.
fn apply_hunk(
    target: &mut Vec<String>,
    hunk: &Hunk,
    index: usize,
    display_name: &str,
    creation: bool,
) -> ToolResult<()> {
    let expected = hunk.expected();

    if creation || expected.is_empty() {
        // Pure insertion at/after line old_start.
        let pos = (hunk.old_start as usize).min(target.len());
        let replacement: Vec<String> = hunk.replacement();
        target.splice(pos..pos, replacement);
        return Ok(());
    }

    let starts_at = |candidate_1based: i64| -> bool {
        if candidate_1based < 1 {
            return false;
        }
        let base = candidate_1based as usize - 1;
        base + expected.len() <= target.len()
            && target[base..base + expected.len()]
                .iter()
                .zip(expected.iter())
                .all(|(got, want)| got.as_str() == *want)
    };

    let start = hunk.old_start as i64;
    let position: i64 = if starts_at(start) {
        start
    } else {
        let limit = target.len() as i64;
        (1..=100i64)
            .flat_map(|d| [start - d, start + d])
            .find(|&c| c >= 1 && c <= limit.max(1) && starts_at(c))
            .ok_or_else(|| {
                ToolError::msg(format!(
                    "Failed to apply hunk #{index} to '{display_name}': no matching context found near line {}. \
                     Re-read the file around that line and regenerate the patch.",
                    hunk.old_start
                ))
            })?
    };

    let replacement = hunk.replacement();
    let base = position as usize - 1;
    target.splice(base..base + expected.len(), replacement);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::permissions::Permissions;
    use crate::workspace::Workspace;
    use tempfile::TempDir;

    fn writable_state(root: &std::path::Path) -> AppState {
        AppState::new(
            Workspace::open(root).unwrap(),
            Permissions::from_flags(true, false, false).unwrap(),
            Limits::default(),
        )
    }

    fn read_only_state(root: &std::path::Path) -> AppState {
        AppState::new(
            Workspace::open(root).unwrap(),
            Permissions::default(),
            Limits::default(),
        )
    }

    const BASE: &str = "alpha\nbeta\ngamma\ndelta\nepsilon\n";

    fn fixture(content: &str) -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("sample.txt"), content).unwrap();
        let state = writable_state(tmp.path());
        (tmp, state)
    }

    fn simple_diff(body: &str) -> String {
        format!(
            "--- sample.txt\n+++ sample.txt\n@@ -1,2 +1,2 @@\n-alpha\n-beta\n+ALPHA\n+BETA\n{body}"
        )
    }

    #[test]
    fn applies_simple_patch() {
        let (_t, state) = fixture(BASE);
        let out = handle(
            &state,
            ApplyPatchArgs {
                patch: simple_diff(""),
            },
        )
        .unwrap();
        assert_eq!(out["changed_files"][0], json!("sample.txt"));
        let content = std::fs::read_to_string(_t.path().join("sample.txt")).unwrap();
        assert_eq!(content, "ALPHA\nBETA\ngamma\ndelta\nepsilon\n");
    }

    #[test]
    fn preserves_trailing_newline_absence() {
        let (_t, state) = fixture("one\ntwo"); // no trailing newline
        let patch = "--- sample.txt\n+++ sample.txt\n@@ -1,2 +1,2 @@\n-one\n-two\n+ONE\n+TWO\n";
        handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        let content = std::fs::read_to_string(_t.path().join("sample.txt")).unwrap();
        assert_eq!(content, "ONE\nTWO");
    }

    #[test]
    fn fails_on_stale_context_without_touching_file() {
        let (_t, state) = fixture("totally different content\nsecond line\n");
        let err = handle(
            &state,
            ApplyPatchArgs {
                patch: simple_diff(""),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to apply hunk"));
        assert!(err.to_string().contains("Re-read the file"));
        let unchanged = std::fs::read_to_string(_t.path().join("sample.txt")).unwrap();
        assert_eq!(unchanged, "totally different content\nsecond line\n");
    }

    #[test]
    fn write_permission_is_required() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("f.txt"), BASE).unwrap();
        let state = read_only_state(tmp.path());
        let err = handle(
            &state,
            ApplyPatchArgs {
                patch: simple_diff(""),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("--write"));
    }

    #[test]
    fn rejects_workspace_escape_in_target() {
        let (_t, state) = fixture(BASE);
        let patch = "--- ../../outside.txt\n+++ ../../outside.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n";
        let err = handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Rejecting patch target"));
    }

    #[test]
    fn creates_new_file_from_dev_null_header() {
        let (_t, state) = fixture(BASE);
        let patch = "--- /dev/null\n+++ brand-new.txt\n@@ -0,0 +1,2 @@\n+first\n+second\n";
        let out = handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        assert_eq!(out["changed_files"][0], json!("brand-new.txt"));
        let content = std::fs::read_to_string(_t.path().join("brand-new.txt")).unwrap();
        assert_eq!(content, "first\nsecond\n");
    }

    #[test]
    fn multi_file_patch_applies_both_files() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "A1\nA2\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "B1\nB2\n").unwrap();
        let state = writable_state(tmp.path());
        let patch = concat!(
            "--- a.txt\n+++ a.txt\n@@ -1,1 +1,1 @@\n-A1\n+AX1\n",
            "--- b.txt\n+++ b.txt\n@@ -1,1 +1,1 @@\n-B1\n+BB1\n",
        );
        let out = handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        let files = out["changed_files"].as_array().unwrap();
        assert_eq!(files.len(), 2);
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "AX1\nA2\n"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(),
            "BB1\nB2\n"
        );
    }

    #[test]
    fn fuzzy_application_tolerates_drifted_line_numbers() {
        let (_t, state) = fixture(BASE);
        // Header claims line 5 but the context actually sits at line 1.
        let patch =
            "--- sample.txt\n+++ sample.txt\n@@ -5,2 +5,2 @@\n-alpha\n-beta\n+ALPHA\n+BETA\n";
        handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        let content = std::fs::read_to_string(_t.path().join("sample.txt")).unwrap();
        assert_eq!(content, "ALPHA\nBETA\ngamma\ndelta\nepsilon\n");
    }

    #[test]
    fn appends_when_no_context_matches_anywhere_but_addition_only() {
        let (_t, state) = fixture(BASE);
        let patch = "--- sample.txt\n+++ sample.txt\n@@ -6,0 +6,1 @@\n+zeta\n";
        handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        let content = std::fs::read_to_string(_t.path().join("sample.txt")).unwrap();
        assert_eq!(content, "alpha\nbeta\ngamma\ndelta\nepsilon\nzeta\n");
    }

    #[test]
    fn rejects_malformed_headers() {
        let (_t, state) = fixture(BASE);
        let patch = "this is not a patch\n";
        let err = handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("No usable unified-diff"));

        let patch = "--- f\nno plus header\n";
        let err = handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("'+++' header"));
    }

    #[test]
    fn rejects_rename_and_binary_ext_headers() {
        let (_t, state) = fixture(BASE);
        let patch = "rename from x\nrename to y\n";
        let err = handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("rename from"));
    }
}
