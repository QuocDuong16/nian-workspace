//! `apply_patch` — the single write primitive (spec section 7.5).
//!
//! Accepts a unified diff (created with `diff -u`, `git diff`, or produced by
//! an AI client), validates it against the workspace boundary, and applies it
//! per file: each file's new content is built fully in memory and only then
//! written. All hunks of all files are validated (against the original
//! content, in memory) before any disk mutation happens; a failed hunk
//! aborts the whole patch without touching any file.
//!
//! Durability guarantees, stated honestly:
//! * every hunk is validated before mutation
//! * each individual file replacement is atomic where the filesystem
//!   supports rename (sibling temp file + `rename`)
//! * an unexpected filesystem failure during the final commit phase may
//!   leave a multi-file patch partially applied — this tool does not provide
//!   rollback across files

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use rmcp::schemars;
use serde_json::json;
use std::path::{Path, PathBuf};

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ApplyPatchArgs {
    /// Unified diff text (`diff -u` / `git diff` format). Multi-file diffs are applied as one unit: every hunk must apply cleanly or nothing is written.
    #[schemars(
        description = "Unified diff text in 'diff -u'/'git diff' format. May contain multiple files. All hunks are validated before mutation; each individual file replacement is atomic, but an unexpected filesystem failure during the commit phase can leave a multi-file patch partially applied."
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
        let newline_style = detect_newline_style(&original_text);
        let mut working: Vec<String> = if original_text.is_empty() {
            Vec::new()
        } else {
            // Split without \r: lines keep their content; the separator is
            // re-applied uniformly on join below.
            split_lines_normalized(&original_text)
        };
        let had_trailing_newline = original_text.ends_with('\n');

        let mut applied_offset: i64 = 0;
        for (idx, hunk) in file.hunks.iter().enumerate() {
            apply_hunk(
                &mut working,
                hunk,
                idx + 1,
                &file.new_path,
                file.creation,
                // Hunks carry original-file line numbers; earlier hunks may
                // have shifted content, so pass the accumulated delta down.
                applied_offset,
            )?;
            applied_offset = hunk_delta(applied_offset, hunk);
        }

        let line_sep = match newline_style {
            NewlineStyle::Crlf => "\r\n",
            NewlineStyle::Lf => "\n",
        };
        let mut out = working.join(line_sep);
        if !out.is_empty() && (had_trailing_newline || file.creation) {
            out.push_str(if matches!(newline_style, NewlineStyle::Crlf) {
                "\r\n"
            } else {
                "\n"
            });
        }
        staged.push((rel_display.clone(), resolved.clone(), out.into_bytes()));
    }

    // Everything applied cleanly in memory — commit to disk now.
    // Note: each replacement is individually atomic, but a failure part-way
    // through this loop leaves earlier files written (documented limitation).
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum NewlineStyle {
    Lf,
    Crlf,
}

/// Decide which separator re-joins modified lines. The first CRLF wins —
/// mixed-ending files keep whatever style appears (first), rather than being
/// normalized to something they never were.
fn detect_newline_style(text: &str) -> NewlineStyle {
    let mut bytes = text.as_bytes();
    while let Some(pos) = bytes.iter().position(|&b| b == b'\n') {
        if pos > 0 && bytes[pos - 1] == b'\r' {
            return NewlineStyle::Crlf;
        }
        bytes = &bytes[pos + 1..];
    }
    NewlineStyle::Lf
}

/// Split into content lines exactly like `str::lines()` but treat `\r\n` and
/// `\n` identically (which `.lines()` already does) — kept as a named helper
/// so the newline-preservation contract has one obvious home.
fn split_lines_normalized(text: &str) -> Vec<String> {
    text.lines().map(String::from).collect()
}

fn decode_utf8(bytes: &[u8], display: &str) -> ToolResult<String> {
    String::from_utf8(bytes.to_vec()).map_err(|_| {
        ToolError::msg(format!(
            "Cannot patch '{display}': file is not valid UTF-8."
        ))
    })
}

/// Replace `target` atomically: write a fresh exclusive sibling temp file,
/// carry over the old file's permissions where applicable, then rename over
/// the target. Readers never observe a torn file, and the temp name cannot
/// collide or be pre-planted (exclusive creation in the same directory).
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

    // NamedTempFile keeps the file open with O_EXCL semantics; the prefix
    // makes any leaked temp identifiable and .gitignore-able.
    let mut tmp = tempfile::Builder::new()
        .prefix(".nian-patch-tmp")
        .tempfile_in(
            target
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or_else(|| Path::new(".")),
        )
        .map_err(|e| {
            ToolError::msg(format!(
                "Failed to create temp file next to '{}': {e}",
                target.display()
            ))
        })?;
    tmp.write_all(bytes)
        .map_err(|e| ToolError::msg(format!("Failed to write temp file: {e}")))?;
    tmp.flush()
        .map_err(|e| ToolError::msg(format!("Failed to flush temp file: {e}")))?;

    // Preserve existing permission bits so, e.g., an executable script
    // stays executable after being patched (POSIX platforms only). A
    // failure to apply the preserved mode aborts the replacement: dropping
    // `tmp` deletes it, so the original file remains untouched — README
    // promises preservation, so either keep the bits or fail clearly.
    #[cfg(unix)]
    match std::fs::metadata(target) {
        Ok(meta) => {
            use std::os::unix::fs::PermissionsExt;
            apply_preserved_mode(tmp.path(), meta.permissions().mode())?;
        }
        // New file (creation patch): default exclusive-temp permissions apply.
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => {
            return Err(ToolError::msg(format!(
                "Cannot stat '{}' to preserve its permissions: {e}",
                target.display()
            )))
        }
    }

    tmp.persist(target)
        .map_err(|e| ToolError::msg(format!("Failed to replace '{}': {e}", target.display())))?;
    Ok(())
}

/// Carry `mode` over to the replacement file at `dest`. Any failure is an
/// error — never silently ignored — so the caller aborts BEFORE persisting
/// and the original file stays untouched.
#[cfg(unix)]
fn apply_preserved_mode(dest: &Path, mode: u32) -> ToolResult<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(dest, std::fs::Permissions::from_mode(mode)).map_err(|e| {
        ToolError::msg(format!(
            "Failed to preserve permission mode {:o} on replacement for '{}': {e}. \
             Original file left unchanged.",
            mode,
            dest.display()
        ))
    })
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

/// Net line-count change a hunk introduces (added minus removed).
fn hunk_delta(current_offset: i64, hunk: &Hunk) -> i64 {
    let expected_len = hunk.expected().len() as i64;
    let replacement_len = hunk.replacement().len() as i64;
    current_offset + replacement_len - expected_len
}

/// Apply one hunk at its stated position adjusted by the cumulative offset of
/// all previously applied hunks in the same file (their line numbers refer to
/// the original file). A small residual drift scan (±10 lines) tolerates
/// stale headers from AI clients without masking genuine mismatches.
fn apply_hunk(
    target: &mut Vec<String>,
    hunk: &Hunk,
    index: usize,
    display_name: &str,
    creation: bool,
    offset: i64,
) -> ToolResult<()> {
    let expected = hunk.expected();

    if creation || expected.is_empty() {
        // Pure insertion at/after line old_start (+ accumulated offset).
        let pos = ((hunk.old_start as i64) + offset).clamp(0, target.len() as i64) as usize;
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

    // Expected position: stated original line + net change from earlier hunks.
    let start = (hunk.old_start as i64) + offset;
    let position: i64 = if starts_at(start) {
        start
    } else {
        let limit = target.len() as i64;
        (1..=10i64)
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

    // -- multi-hunk offset correctness (review pass #8) ---------------------

    #[test]
    fn earlier_insertion_shifts_later_hunk() {
        let (_t, state) = fixture(BASE);
        // Hunk 1 inserts 3 lines near the top; hunk 2's header still refers
        // to the ORIGINAL line numbers ("gamma" was original line 3).
        let patch = concat!(
            "--- sample.txt\n+++ sample.txt\n",
            "@@ -1,2 +1,5 @@\n",
            "-alpha\n-beta\n+alpha\n+ins1\n+ins2\n+ins3\n+beta\n",
            "@@ -3,1 +6,1 @@\n",
            "-gamma\n+GAMMA\n",
        );
        handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        let content = std::fs::read_to_string(_t.path().join("sample.txt")).unwrap();
        assert_eq!(
            content,
            "alpha\nins1\nins2\nins3\nbeta\nGAMMA\ndelta\nepsilon\n"
        );
    }

    #[test]
    fn earlier_deletion_shifts_later_hunk() {
        let (_t, state) = fixture(BASE);
        // Hunk 1 deletes two lines; hunk 2 edits a line whose original
        // position is 4 but which now sits at line 2.
        let patch = concat!(
            "--- sample.txt\n+++ sample.txt\n",
            "@@ -1,2 +1,0 @@\n",
            "-alpha\n-beta\n",
            "@@ -4,1 +2,1 @@\n",
            "-delta\n+DELTA\n",
        );
        handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        let content = std::fs::read_to_string(_t.path().join("sample.txt")).unwrap();
        assert_eq!(content, "gamma\nDELTA\nepsilon\n");
    }

    #[test]
    fn multiple_hunks_on_one_file_apply_sequentially() {
        let (_t, state) = fixture("l1\nl2\nl3\nl4\nl5\nl6\n");
        let patch = concat!(
            "--- sample.txt\n+++ sample.txt\n",
            "@@ -1,1 +1,1 @@\n-l1\n+L1\n",
            "@@ -3,1 +3,1 @@\n-l3\n+L3\n",
            "@@ -6,1 +6,1 @@\n-l6\n+L6\n",
        );
        handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        let content = std::fs::read_to_string(_t.path().join("sample.txt")).unwrap();
        assert_eq!(content, "L1\nl2\nL3\nl4\nl5\nL6\n");
    }

    #[test]
    fn failed_later_hunk_in_second_file_writes_nothing() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "A1\nA2\n").unwrap();
        std::fs::write(tmp.path().join("b.txt"), "B1\nB2\n").unwrap();
        let state = writable_state(tmp.path());
        let patch = concat!(
            "--- a.txt\n+++ a.txt\n@@ -1,1 +1,1 @@\n-A1\n+AX1\n",
            "--- b.txt\n+++ b.txt\n@@ -1,1 +1,1 @@\n-NOT-THE-CONTENT\n+nope\n",
        );
        let err = handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Failed to apply hunk"));
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("a.txt")).unwrap(),
            "A1\nA2\n",
            "file A must not be written when file B fails validation"
        );
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("b.txt")).unwrap(),
            "B1\nB2\n"
        );
    }

    // -- newline-style preservation (review pass #10) ------------------------

    #[test]
    fn crlf_files_stay_crlf() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("crlf.txt"), "one\r\ntwo\r\n").unwrap();
        let state = writable_state(tmp.path());
        let patch = "--- crlf.txt\n+++ crlf.txt\n@@ -1,2 +1,2 @@\n-one\r\n-two\r\n+ONE\r\n+TWO\r\n";
        handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        let bytes = std::fs::read(tmp.path().join("crlf.txt")).unwrap();
        assert_eq!(
            String::from_utf8(bytes).unwrap(),
            "ONE\r\nTWO\r\n",
            "CRLF endings must survive the edit"
        );
    }

    #[test]
    fn lf_files_stay_lf() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("lf.txt"), "one\ntwo\n").unwrap();
        let state = writable_state(tmp.path());
        let patch = "--- lf.txt\n+++ lf.txt\n@@ -1,2 +1,2 @@\n-one\n-two\n+ONE\n+TWO\n";
        handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();
        let bytes = std::fs::read(tmp.path().join("lf.txt")).unwrap();
        assert_eq!(bytes, b"ONE\nTWO\n", "LF endings must survive the edit");
    }

    // -- temp-file safety (review pass #11) ----------------------------------

    #[test]
    fn no_leftover_temp_files_after_patch() {
        let (tmp, state) = fixture(BASE);
        handle(
            &state,
            ApplyPatchArgs {
                patch: simple_diff(""),
            },
        )
        .unwrap();
        let leftovers: Vec<_> = std::fs::read_dir(tmp.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_string_lossy()
                    .starts_with(".nian-patch-tmp")
            })
            .collect();
        assert!(leftovers.is_empty(), "temp files leaked: {leftovers:?}");
    }

    // -- permission preservation (review pass #9, Unix-only) -----------------

    #[cfg(unix)]
    #[test]
    fn permission_preservation_failure_is_surfaced_not_ignored() {
        // The contract is "keep the bits or fail loudly", never "silently
        // lose the bits and persist anyway". Point the mode-application step
        // at a path that cannot exist so set_permissions fails, and require
        // a clear error naming the preserved mode.
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("no-such-tmp-file");
        let err = apply_preserved_mode(&missing, 0o755).unwrap_err();
        assert!(
            err.to_string()
                .contains("Failed to preserve permission mode"),
            "set_permissions failure must be surfaced: {}",
            err
        );
    }

    #[cfg(unix)]
    #[test]
    fn executable_permissions_survive_replacement() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let script = tmp.path().join("script.sh");
        std::fs::write(&script, b"#!/bin/sh\necho one\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();

        let state = writable_state(tmp.path());
        let patch =
            "--- script.sh\n+++ script.sh\n@@ -1,2 +1,2 @@\n-#!/bin/sh\n-echo one\n+#!/bin/sh\n+echo two\n";
        handle(
            &state,
            ApplyPatchArgs {
                patch: patch.into(),
            },
        )
        .unwrap();

        let mode = std::fs::metadata(&script).unwrap().permissions().mode();
        assert_eq!(
            mode & 0o777,
            0o755,
            "permission bits changed by atomic replacement"
        );
    }
}
