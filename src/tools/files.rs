//! `list_files` and `read_file` (spec sections 7.2 / 7.3).

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use crate::tools::{clip_line, is_generated_or_vcs_dir};
use rmcp::schemars;
use serde_json::json;
use std::io::{BufRead, Read, Seek, SeekFrom};
use std::path::PathBuf;
use walkdir::WalkDir;

// ---------------------------------------------------------------------------
// list_files
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ListFilesArgs {
    /// Directory to list, relative to the workspace root. Empty or omitted lists the workspace root.
    #[serde(default)]
    #[schemars(
        description = "Directory to list, relative to the workspace root. Empty or omitted lists the workspace root."
    )]
    pub path: Option<String>,

    /// How many directory levels to descend (default 2, max 10).
    #[serde(default)]
    #[schemars(description = "How many directory levels to descend (default 2, max 10).")]
    pub depth: Option<u32>,

    /// Include dotfiles and dot-directories (default false).
    #[serde(default)]
    #[schemars(description = "Include dotfiles and dot-directories (default false).")]
    pub include_hidden: bool,

    /// Optional glob pattern matched against workspace-relative paths, e.g. "*.rs" or "src/*.rs".
    #[serde(default)]
    #[schemars(
        description = "Optional glob pattern matched against workspace-relative paths, e.g. '*.rs' or 'src/*.rs'."
    )]
    pub glob: Option<String>,
}

pub(crate) fn list_files(state: &AppState, args: ListFilesArgs) -> ToolResult<serde_json::Value> {
    let ws = state.workspace();
    let limits = state.limits();

    let dir = ws.resolve(args.path.as_deref())?;
    if !dir.exists() {
        return Err(ToolError::msg(format!(
            "Path does not exist: {}",
            ws.display_relative(&dir)
        )));
    }
    if !dir.is_dir() {
        return Err(ToolError::msg(format!(
            "'{}' is not a directory.",
            ws.display_relative(&dir)
        )));
    }

    let glob_set = match &args.glob {
        Some(pattern) if !pattern.trim().is_empty() => {
            let glob = globset::GlobBuilder::new(pattern.trim())
                .literal_separator(true)
                .build()
                .map_err(|e| ToolError::msg(format!("Invalid glob pattern '{pattern}': {e}")))?
                .compile_matcher();
            Some(glob)
        }
        _ => None,
    };

    let depth = args.depth.unwrap_or(2).clamp(1, 10);
    let mut entries: Vec<serde_json::Value> = Vec::new();
    let mut truncated = false;

    for entry in WalkDir::new(&dir)
        .min_depth(1)
        .max_depth(depth as usize)
        .follow_links(false)
        .sort_by_file_name()
        .into_iter()
        .filter_entry(|e| entry_is_visible(e, args.include_hidden))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("list_files: skipping unreadable entry: {err}");
                continue;
            }
        };
        let rel = ws.display_relative(entry.path());
        let file_type = entry.file_type();
        let kind = if file_type.is_symlink() {
            "symlink"
        } else if file_type.is_dir() {
            "dir"
        } else {
            "file"
        };
        if kind == "file" && file_type.is_file() {
            // regular file only; symlink/other sizes are not meaningful here
        }
        if let Some(glob) = &glob_set {
            let matches = glob.is_match(&rel)
                || entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| glob.is_match(name));
            if !matches {
                continue;
            }
        }
        if entries.len() >= limits.max_list_entries {
            truncated = true;
            break;
        }
        let size = if file_type.is_file() {
            entry.metadata().ok().map(|m| m.len())
        } else {
            None
        };
        let mut item = json!({
            "path": rel,
            "type": kind,
        });
        if let Some(size) = size {
            item["size"] = json!(size);
        }
        entries.push(item);
    }

    Ok(json!({
        "root": ws.display_relative(&dir),
        "depth": depth,
        "count": entries.len(),
        "truncated": truncated,
        "entries": entries,
    }))
}

/// Shared visibility rule for listing/pruning: hide dot-entries unless asked,
/// and prune generated/VCS directories entirely.
fn entry_is_visible(entry: &walkdir::DirEntry, include_hidden: bool) -> bool {
    if entry.depth() == 0 {
        return true; // never prune the requested search root itself
    }
    let name = entry.file_name().to_string_lossy();
    if !include_hidden && name.starts_with('.') {
        return false;
    }
    if entry.file_type().is_dir() && is_generated_or_vcs_dir(&name) {
        return false;
    }
    true
}

// ---------------------------------------------------------------------------
// read_file
// ---------------------------------------------------------------------------

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct ReadFileArgs {
    /// File path relative to the workspace root.
    #[schemars(description = "File path relative to the workspace root.")]
    pub path: String,

    /// First line to return (1-based). Defaults to 1.
    #[serde(default)]
    #[schemars(description = "First line to return (1-based). Defaults to 1.")]
    pub start_line: Option<u64>,

    /// Last line to return (inclusive). With only start_line given, a bounded span is returned.
    #[serde(default)]
    #[schemars(
        description = "Last line to return (inclusive). If omitted together with start_line, a bounded default range starting at line 1 is returned."
    )]
    pub end_line: Option<u64>,
}

struct EmittedLine {
    number: u64,
    text: String,
}

pub(crate) fn read_file(state: &AppState, args: ReadFileArgs) -> ToolResult<serde_json::Value> {
    let ws = state.workspace();
    let limits = state.limits();

    let path: PathBuf = ws.resolve(Some(args.path.as_str()))?;
    let meta = std::fs::metadata(&path).map_err(|e| {
        ToolError::msg(format!("Cannot read '{}': {e}", ws.display_relative(&path)))
    })?;
    if meta.is_dir() {
        return Err(ToolError::msg(format!(
            "'{}' is a directory; use list_files instead.",
            ws.display_relative(&path)
        )));
    }

    const BINARY_SNIFF_BYTES: usize = 8 * 1024;
    let mut reader = std::io::BufReader::new(std::fs::File::open(&path).map_err(|e| {
        ToolError::msg(format!("Cannot open '{}': {e}", ws.display_relative(&path)))
    })?);

    // Binary sniff on the leading bytes.
    let mut sniff = [0u8; BINARY_SNIFF_BYTES];
    let sniff_len = read_at_most(&mut reader, &mut sniff)?;
    if sniff[..sniff_len].contains(&0u8) {
        return Err(ToolError::msg(format!(
            "'{}' appears to be a binary file and was not read.",
            ws.display_relative(&path)
        )));
    }
    reader.seek(SeekFrom::Start(0))?;

    let start = args.start_line.unwrap_or(1).max(1);
    let end = match args.end_line {
        Some(explicit) => explicit.max(start),
        None => start + limits.read_span_when_start_only - 1,
    };
    // When neither bound was supplied this is the documented default window.
    let default_span = args.start_line.is_none() && args.end_line.is_none();

    let first = if default_span { 1 } else { start };
    let last_limit = if default_span {
        start + limits.default_read_lines - 1
    } else {
        end
    };

    let budget = limits.max_read_bytes;
    let mut lines: Vec<EmittedLine> = Vec::new();
    let mut emitted_bytes = 0usize;
    let mut truncated_by_budget = false;
    let mut lossy = false;
    let mut current_line: u64 = 0;

    loop {
        let mut raw_buf: Vec<u8> = Vec::with_capacity(512);
        // Bounded line read: a pathological no-newline file cannot drive
        // allocation past max_source_line_bytes per line. Excess bytes for
        // an oversized line are consumed and dropped inside the reader; the
        // clipped rendering path below flags the truncation.
        let (n, _oversized) = read_line_bounded(
            &mut reader,
            b'\n',
            &mut raw_buf,
            limits.max_source_line_bytes,
        )?;
        if n == 0 {
            break; // EOF
        }
        current_line += 1;
        if current_line < first || current_line > last_limit {
            continue;
        }
        strip_eol(&mut raw_buf);

        let (text, replaced) = crate::tools::decode_lossy(&raw_buf);
        if replaced {
            lossy = true;
        }
        let formatted_number = current_line;
        let formatted_len_estimate = text.len() + number_prefix_len(current_line);

        if emitted_bytes > 0 && emitted_bytes + formatted_len_estimate > budget {
            truncated_by_budget = true;
            break;
        }
        if emitted_bytes == 0 && text.len() > budget {
            // A single line over the whole response budget: return its head
            // (with the usual line-number prefix) and stop.
            let (clipped, _) = clip_line(
                &text,
                budget
                    .saturating_sub(number_prefix_len(current_line))
                    .min(budget),
            );
            #[allow(unused_assignments)]
            {
                emitted_bytes += clipped.len() + number_prefix_len(current_line);
            }
            lines.push(EmittedLine {
                number: formatted_number,
                text: format!("{formatted_number}: {clipped}"),
            });
            truncated_by_budget = true;
            break;
        }

        let (text, _) = clip_line(&text, limits.max_search_line_bytes * 8); // generous per-line cap
        emitted_bytes += text.len() + number_prefix_len(formatted_number);
        lines.push(EmittedLine {
            number: formatted_number,
            text: format!("{formatted_number}: {text}"),
        });

        if current_line >= last_limit {
            break;
        }
    }

    // Determine whether content continues beyond what we returned.
    let has_more_lines = if truncated_by_budget {
        true
    } else {
        let mut probe = [0u8; 1];
        reader.read_exact(&mut probe).is_ok()
    };

    let last_returned = lines.last().map(|l| l.number).unwrap_or(0);
    Ok(json!({
        "path": ws.display_relative(&path),
        "start_line": lines.first().map(|l| l.number),
        "end_line": if lines.is_empty() { None } else { Some(last_returned) },
        "line_count": lines.len(),
        "truncated": truncated_by_budget,
        "has_more_lines": has_more_lines,
        "lossy_decoding": lossy,
        "lines": lines.iter().map(|l| l.text.clone()).collect::<Vec<_>>(),
    }))
}

fn number_prefix_len(n: u64) -> usize {
    let digits = n.to_string().len();
    digits + 2 // ": "
}

fn read_at_most(file: &mut impl Read, buf: &mut [u8]) -> ToolResult<usize> {
    let mut filled = 0;
    while filled < buf.len() {
        let n = file.read(&mut buf[filled..]).map_err(ToolError::from)?;
        if n == 0 {
            break;
        }
        filled += n;
    }
    Ok(filled)
}

/// Read one delimiter-terminated line into `buf`, keeping at most
/// `max_bytes` bytes (excess bytes for that line are consumed but dropped,
/// so allocation stays bounded). Returns (bytes_consumed_marker, oversized)
/// where the first element is 0 only at EOF. The returned marker counts the
/// retained bytes plus whether the line exceeded the bound.
fn read_line_bounded(
    reader: &mut impl BufRead,
    delim: u8,
    buf: &mut Vec<u8>,
    max_bytes: usize,
) -> ToolResult<(usize, bool)> {
    let mut consumed_any = false;
    let mut oversized = false;
    loop {
        let available = match reader.fill_buf() {
            Ok(slice) => slice,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(ToolError::from(e)),
        };
        if available.is_empty() {
            // EOF.
            return Ok((if consumed_any { 1 } else { 0 }, oversized));
        }
        consumed_any = true;
        match memchr(delim, available) {
            Some(pos) => {
                let keep = pos.min(max_bytes.saturating_sub(buf.len()));
                buf.extend_from_slice(&available[..keep]);
                if pos > keep {
                    oversized = true;
                }
                reader.consume(pos + 1);
                return Ok((buf.len() + 1, oversized));
            }
            None => {
                let take = available.len().min(max_bytes.saturating_sub(buf.len()));
                buf.extend_from_slice(&available[..take]);
                if !available.is_empty() && take < available.len() {
                    oversized = true;
                }
                let len = available.len();
                reader.consume(len);
            }
        }
    }
}

/// Index of `needle` in `hay`, or None. Small non-cryptographic scan; a
/// dependency-free memchr replacement is fine at these buffer sizes.
fn memchr(needle: u8, hay: &[u8]) -> Option<usize> {
    hay.iter().position(|&b| b == needle)
}

fn strip_eol(buf: &mut Vec<u8>) {
    while matches!(buf.last(), Some(b'\n') | Some(b'\r')) {
        buf.pop();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::permissions::Permissions;
    use crate::workspace::Workspace;
    use tempfile::TempDir;

    fn test_state(root: &std::path::Path) -> AppState {
        AppState::new(
            Workspace::open(root).unwrap(),
            Permissions::default(),
            Limits {
                max_read_bytes: 200,
                ..Limits::default()
            },
        )
    }

    fn fixture(content: &[u8]) -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("sample.txt"), content).unwrap();
        let state = test_state(tmp.path());
        (tmp, state)
    }

    #[test]
    fn reads_full_small_file_with_numbers() {
        let (_t, state) = fixture(b"alpha\nbeta\ngamma\n");
        let out = read_file(
            &state,
            ReadFileArgs {
                path: "sample.txt".into(),
                start_line: None,
                end_line: None,
            },
        )
        .unwrap();
        assert_eq!(out["line_count"], json!(3));
        let lines = out["lines"].as_array().unwrap();
        assert_eq!(lines[0], "1: alpha");
        assert_eq!(lines[2], "3: gamma");
        assert_eq!(out["truncated"], json!(false));
        assert_eq!(out["has_more_lines"], json!(false));
    }

    #[test]
    fn honours_explicit_ranges() {
        let content = (1..=100).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("line{i}\n"));
            acc
        });
        let (_t, state) = fixture(content.as_bytes());
        let out = read_file(
            &state,
            ReadFileArgs {
                path: "sample.txt".into(),
                start_line: Some(40),
                end_line: Some(45),
            },
        )
        .unwrap();
        assert_eq!(out["start_line"], json!(40));
        assert_eq!(out["end_line"], json!(45));
        assert_eq!(out["line_count"], json!(6));
    }

    #[test]
    fn flags_more_lines_when_range_ends_early() {
        let content = (1..=50).fold(String::new(), |mut acc, i| {
            acc.push_str(&format!("row {i}\n"));
            acc
        });
        let (_t, state) = fixture(content.as_bytes());
        let out = read_file(
            &state,
            ReadFileArgs {
                path: "sample.txt".into(),
                start_line: Some(1),
                end_line: Some(3),
            },
        )
        .unwrap();
        assert_eq!(out["truncated"], json!(false));
        assert_eq!(out["has_more_lines"], json!(true));
    }

    #[test]
    fn truncates_large_files_by_byte_budget() {
        let long_line = "x".repeat(500);
        let content = format!("{long_line}\n{long_line}\n{long_line}\n");
        let (_t, state) = fixture(content.as_bytes());
        let out = read_file(
            &state,
            ReadFileArgs {
                path: "sample.txt".into(),
                start_line: None,
                end_line: None,
            },
        )
        .unwrap();
        assert_eq!(out["truncated"], json!(true));
        assert_eq!(out["has_more_lines"], json!(true));
        assert!(out["line_count"].as_u64().unwrap() < 3);
    }

    #[test]
    fn rejects_binary_files() {
        let bytes: &[u8] = &[b'P', b'N', b'G', 0x89, 0x00, 0x00, 0x00, 0x0D, 0x49, 0x48];
        let (_t, state) = fixture(bytes);
        let err = read_file(
            &state,
            ReadFileArgs {
                path: "sample.txt".into(),
                start_line: None,
                end_line: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("binary"));
    }

    #[test]
    fn rejects_directories_with_helpful_error() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("sub")).unwrap();
        let state = test_state(tmp.path());
        let err = read_file(
            &state,
            ReadFileArgs {
                path: "sub".into(),
                start_line: None,
                end_line: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("directory"));
    }

    #[test]
    fn reports_missing_files_via_io_error_text() {
        let (_t, state) = fixture(b"x\n");
        let err = read_file(
            &state,
            ReadFileArgs {
                path: "missing.txt".into(),
                start_line: None,
                end_line: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("missing.txt"));
    }

    #[test]
    fn cannot_escape_workspace_via_read_args() {
        let (_t, state) = fixture(b"x\n");
        let err = read_file(
            &state,
            ReadFileArgs {
                path: "../../etc/passwd".into(),
                start_line: None,
                end_line: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("outside the configured workspace"));
    }

    #[test]
    fn very_long_single_line_stays_memory_bounded() {
        // 64 MiB with no newline at all — must not allocate anywhere near
        // that inside the reader (bound is max_source_line_bytes = 1 MiB).
        let huge = "z".repeat(64 * 1024 * 1024);
        let (_t, state) = fixture(huge.as_bytes());
        let out = read_file(
            &state,
            ReadFileArgs {
                path: "sample.txt".into(),
                start_line: None,
                end_line: None,
            },
        )
        .unwrap();
        assert_eq!(out["line_count"], json!(1));
        // Output still capped by the response budget, marked truncated.
        assert_eq!(out["truncated"], json!(true));
        let text = out["lines"][0].as_str().unwrap();
        assert!(text.len() < limits_of(&state).max_read_bytes + 64);
    }

    #[test]
    fn long_line_in_middle_is_clipped_without_losing_other_lines() {
        let big = "y".repeat(3 * 1024 * 1024);
        let content = format!("first\n{big}\nlast\n");
        let (_t, state) = fixture(content.as_bytes());
        let out = read_file(
            &state,
            ReadFileArgs {
                path: "sample.txt".into(),
                start_line: Some(1),
                end_line: Some(1),
            },
        )
        .unwrap();
        assert_eq!(out["lines"][0], json!("1: first"));
        // A follow-up read over the oversized line must not blow up either.
        let out = read_file(
            &state,
            ReadFileArgs {
                path: "sample.txt".into(),
                start_line: Some(2),
                end_line: Some(2),
            },
        )
        .unwrap();
        let text = out["lines"][0].as_str().unwrap();
        assert!(text.starts_with("2: "), "oversized line rendered wrong");
        // The byte-capped output flags truncation but stays bounded.
        assert!(text.len() < limits_of(&state).max_read_bytes + 64);
    }

    fn limits_of(state: &AppState) -> Limits {
        *state.limits()
    }

    #[test]
    fn listing_prunes_hidden_and_generated_dirs() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(tmp.path().join("src/deep/deeper")).unwrap();
        std::fs::write(tmp.path().join(".hidden"), b"x").unwrap();
        std::fs::write(tmp.path().join("src/main.rs"), b"x").unwrap();
        let state = test_state(tmp.path());

        let out = list_files(
            &state,
            ListFilesArgs {
                path: None,
                depth: Some(5),
                include_hidden: false,
                glob: None,
            },
        )
        .unwrap();
        let paths: Vec<&str> = out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"src"));
        assert!(paths.contains(&"src/main.rs"));
        assert!(paths.contains(&"src/deep"));
        assert!(!paths.iter().any(|p| p.starts_with(".git")));
        assert!(!paths.iter().any(|p| p.starts_with("node_modules")));
        assert!(!paths.contains(&".hidden"));
    }

    #[test]
    fn listing_glob_filters_entries() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir(tmp.path().join("src")).unwrap();
        std::fs::write(tmp.path().join("src/a.rs"), b"").unwrap();
        std::fs::write(tmp.path().join("src/b.txt"), b"").unwrap();
        let state = test_state(tmp.path());

        let out = list_files(
            &state,
            ListFilesArgs {
                path: Some("src".into()),
                depth: Some(1),
                include_hidden: false,
                glob: Some("*.rs".into()),
            },
        )
        .unwrap();
        let paths: Vec<&str> = out["entries"]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&"src/a.rs"));
        assert!(!paths.contains(&"src/b.txt"));
    }
}
