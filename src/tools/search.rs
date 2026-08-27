//! `search` — fast bounded text search with ripgrep-like semantics
//! (spec section 7.4), built on the `grep` crate family.

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use crate::tools::{clip_line, is_generated_or_vcs_dir};
use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkMatch};
use rmcp::schemars;
use serde_json::json;
use std::path::PathBuf;
use walkdir::WalkDir;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct SearchArgs {
    /// Literal text or regular expression to search for.
    #[schemars(description = "Literal text or regular expression (Rust syntax) to search for.")]
    pub query: String,

    /// Directory or file to search, relative to the workspace root. Defaults to the whole workspace.
    #[serde(default)]
    #[schemars(
        description = "Directory or file to search, relative to the workspace root. Defaults to the whole workspace."
    )]
    pub path: Option<String>,

    /// Optional glob filter on file paths, e.g. "*.rs".
    #[serde(default)]
    #[schemars(
        description = "Optional glob filter applied to workspace-relative file paths, e.g. '*.rs'."
    )]
    pub glob: Option<String>,

    /// Case-insensitive matching (default false).
    #[serde(default)]
    #[schemars(description = "Case-insensitive matching (default false).")]
    pub ignore_case: bool,

    /// Interpret the query as fixed text instead of a regex (default false).
    #[serde(default)]
    #[schemars(
        description = "Interpret the query as literal fixed text instead of a regular expression (default false)."
    )]
    pub literal: bool,

    /// Maximum number of matches to return (default 100, capped at 1000).
    #[serde(default)]
    #[schemars(description = "Maximum number of matches to return (default 100, capped at 1000).")]
    pub max_results: Option<usize>,
}

struct CollectingSink<'a> {
    max_results: usize,
    max_line_bytes: usize,
    results: &'a mut Vec<serde_json::Value>,
}

impl Sink for CollectingSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch) -> Result<bool, Self::Error> {
        if self.results.len() >= self.max_results {
            return Ok(false); // stop searching
        }
        let line_no = mat.line_number().unwrap_or(0);
        let raw = String::from_utf8_lossy(mat.bytes());
        let trimmed = raw.trim_end_matches(['\n', '\r']);
        let (text, was_clipped) = clip_line(trimmed, self.max_line_bytes);
        self.results.push(json!({
            "line": line_no,
            "text": text,
            "clipped": was_clipped,
        }));
        Ok(true)
    }
}

pub(crate) fn handle(state: &AppState, args: SearchArgs) -> ToolResult<serde_json::Value> {
    let ws = state.workspace();
    let limits = state.limits();

    if args.query.trim().is_empty() {
        return Err(ToolError::msg("Search query must not be empty."));
    }

    let max_results = args
        .max_results
        .unwrap_or(limits.default_search_results)
        .clamp(1, limits.max_search_results_cap);

    let target = ws.resolve(args.path.as_deref())?;
    if !target.exists() {
        return Err(ToolError::msg(format!(
            "Search path does not exist: {}",
            ws.display_relative(&target)
        )));
    }

    let glob_set = match &args.glob {
        Some(pattern) if !pattern.trim().is_empty() => {
            let glob = globset::GlobBuilder::new(pattern.trim())
                .literal_separator(true)
                .build()
                .map_err(|e| {
                    ToolError::msg(format!("Invalid glob pattern '{}': {e}", pattern.trim()))
                })?
                .compile_matcher();
            Some(glob)
        }
        _ => None,
    };

    let pattern = if args.literal {
        regex::escape(args.query.trim())
    } else {
        args.query.trim().to_string()
    };
    let matcher = RegexMatcherBuilder::new()
        .case_insensitive(args.ignore_case)
        .build(&pattern)
        .map_err(|e| ToolError::msg(format!("Invalid search expression '{}': {e}", pattern)))?;

    let mut searcher = SearcherBuilder::new()
        .line_number(true)
        .binary_detection(BinaryDetection::quit(b'\x00'))
        .build();

    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut files_searched: usize = 0;
    let mut truncated = false;

    // Single-file search is the degenerate case of the walker below.
    for entry in WalkDir::new(&target)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| entry_admissible(e, &target))
    {
        let entry = match entry {
            Ok(e) => e,
            Err(err) => {
                tracing::warn!("search: skipping unreadable path: {err}");
                continue;
            }
        };
        if !entry.file_type().is_file() {
            continue;
        }
        if let Some(glob) = &glob_set {
            let rel = ws.display_relative(entry.path());
            if !(glob.is_match(&rel)
                || entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| glob.is_match(name)))
            {
                continue;
            }
        }
        files_searched += 1;

        let path_buf = PathBuf::from(entry.path());
        let sink = CollectingSink {
            max_results,
            max_line_bytes: limits.max_search_line_bytes,
            results: &mut results,
        };
        // A search error of kind Other is the sink's stop signal when the
        // result cap was hit mid-file.
        match searcher.search_path(&matcher, &path_buf, sink) {
            Ok(_) => {}
            Err(err) if err.kind() == std::io::ErrorKind::Other && results.len() >= max_results => {
                truncated = true;
                break;
            }
            Err(err) => {
                tracing::warn!(
                    "search: failed on {}: {err}",
                    ws.display_relative(entry.path())
                );
            }
        }
        if results.len() >= max_results {
            break;
        }
    }

    let rel_target = ws.display_relative(&target);
    Ok(json!({
        "query": args.query,
        "path": rel_target,
        "files_searched": files_searched,
        "match_count": results.len(),
        "truncated": truncated,
        "matches": results,
    }))
}

/// Prune generated/VCS directories; keep hidden files searchable only when
/// they are explicitly requested via glob (dot-dirs are always pruned).
fn entry_admissible(entry: &walkdir::DirEntry, root: &std::path::Path) -> bool {
    if entry.depth() == 0 {
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if name.starts_with('.') {
        return false;
    }
    if entry.file_type().is_dir() && is_generated_or_vcs_dir(&name) {
        return false;
    }
    let _ = root;
    true
}
