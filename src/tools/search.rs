//! `search` — fast bounded text search with ripgrep-like semantics
//! (spec section 7.4), built on the `grep` crate family.

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use crate::tools::is_generated_or_vcs_dir;
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

    /// Optional glob filter on file paths, e.g. "*.rs" or ".env*".
    #[serde(default)]
    #[schemars(
        description = "Optional glob filter applied to workspace-relative file paths, e.g. '*.rs'. Hidden files are searched only when this glob matches them or when they are requested explicitly via path."
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

/// Search visibility rule:
/// * generated/VCS directories (`.git`, `node_modules`, `target`, …) are
///   always pruned, even explicitly
/// * normal recursive search skips hidden files/directories
/// * an explicit request rooted at a hidden path is honoured
/// * an explicit glob may pull hidden files into a search
fn entry_admissible(
    entry: &walkdir::DirEntry,
    root_is_hidden: bool,
    glob: &Option<globset::GlobMatcher>,
) -> bool {
    if entry.depth() == 0 {
        // The requested root itself is always admitted; it was resolved and
        // authorized through the workspace resolver before walking.
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if entry.file_type().is_dir() && is_generated_or_vcs_dir(&name) {
        return false;
    }
    if !name.starts_with('.') {
        return true;
    }
    // Hidden entry: pruned in a normal walk...
    if root_is_hidden {
        // ...unless the user explicitly asked for this subtree.
        return true;
    }
    if let Some(glob) = glob {
        if glob.is_match(name.as_ref()) {
            return true;
        }
    }
    false
}

struct CollectingSink<'a> {
    /// Workspace-relative path of the file being searched, attached to every
    /// match so results identify their source across multiple files.
    file_path: String,
    max_results: usize,
    max_line_bytes: usize,
    results: &'a mut Vec<serde_json::Value>,
    /// Set once pushing another match would exceed the caller's cap while
    /// the underlying stream may still hold more matches.
    hit_limit: &'a mut bool,
}

impl Sink for CollectingSink<'_> {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch) -> Result<bool, Self::Error> {
        if self.results.len() >= self.max_results {
            // Explicitly tracked stop condition — no reliance on grep's
            // error-kind signalling downstream.
            *self.hit_limit = true;
            return Ok(false); // stop searching this file
        }
        let line_no = mat.line_number().unwrap_or(0);
        let raw = String::from_utf8_lossy(mat.bytes());
        let trimmed = raw.trim_end_matches(['\n', '\r']);
        let (text, was_clipped) = crate::tools::clip_line(trimmed, self.max_line_bytes);
        self.results.push(json!({
            "path": self.file_path,
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

    // Hidden-root searches are explicit requests: the client navigated into
    // hidden territory by naming it (e.g. path=".config"), so hidden entries
    // inside that subtree are intentional. Decided from the *requested*
    // argument, never the absolute location — a workspace merely living
    // under a dot-directory stays a normal workspace.
    let root_is_hidden = match args.path.as_deref() {
        Some(requested) => std::path::Path::new(requested)
            .components()
            .any(|c| {
                matches!(c, std::path::Component::Normal(name) if name.to_string_lossy().starts_with('.'))
            }),
        None => false,
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
    'outer: for entry in WalkDir::new(&target)
        .follow_links(false)
        .into_iter()
        .filter_entry(|e| entry_admissible(e, root_is_hidden, &glob_set))
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
        let rel_file_path = ws.display_relative(&path_buf);
        let mut hit_limit = false;
        let sink = CollectingSink {
            file_path: rel_file_path,
            max_results,
            max_line_bytes: limits.max_search_line_bytes,
            results: &mut results,
            hit_limit: &mut hit_limit,
        };
        match searcher.search_path(&matcher, &path_buf, sink) {
            Ok(_) => {}
            Err(err) => {
                tracing::warn!(
                    "search: failed on {}: {err}",
                    ws.display_relative(entry.path())
                );
            }
        }
        if hit_limit && results.len() >= max_results {
            // The cap was reached mid-search; more matches may exist in this
            // file or later ones, so report truncation explicitly.
            truncated = true;
            break 'outer;
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::permissions::Permissions;
    use crate::workspace::Workspace;
    use tempfile::TempDir;

    fn state(root: &std::path::Path) -> AppState {
        AppState::new(
            Workspace::open(root).unwrap(),
            Permissions::default(),
            Limits::default(),
        )
    }

    fn args(query: &str) -> SearchArgs {
        SearchArgs {
            query: query.into(),
            path: None,
            glob: None,
            ignore_case: false,
            literal: false,
            max_results: None,
        }
    }

    #[test]
    fn result_limit_sets_truncated_flag() {
        let tmp = TempDir::new().unwrap();
        // 50 files x 10 matches = 500 hits; cap at 7.
        for i in 0..50 {
            std::fs::write(tmp.path().join(format!("f{i}.txt")), "needle\n".repeat(10)).unwrap();
        }
        let st = state(tmp.path());
        let mut a = args("needle");
        a.max_results = Some(7);
        let out = handle(&st, a).unwrap();
        assert_eq!(out["match_count"], json!(7));
        assert_eq!(
            out["truncated"],
            json!(true),
            "cap reached with more matches present must report truncated"
        );
    }

    #[test]
    fn exhaustive_search_is_not_truncated() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "hit one\nmiss\nhit two\n").unwrap();
        let st = state(tmp.path());
        let out = handle(&st, args("hit")).unwrap();
        assert_eq!(out["match_count"], json!(2));
        assert_eq!(out["truncated"], json!(false));
    }

    #[test]
    fn hidden_files_are_skipped_unless_glob_or_path_selects_them() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("normal.txt"), "secret_token\n").unwrap();
        std::fs::write(tmp.path().join(".env"), "secret_token=1\n").unwrap();
        std::fs::create_dir_all(tmp.path().join(".config")).unwrap();
        std::fs::write(tmp.path().join(".config/creds.txt"), "secret_token\n").unwrap();

        let st = state(tmp.path());

        // Default: hidden entries excluded entirely.
        let out = handle(&st, args("secret_token")).unwrap();
        assert_eq!(out["files_searched"], json!(1));
        assert_eq!(out["matches"][0]["text"], json!("secret_token"));

        // A glob explicitly naming the hidden file includes it — the hit now
        // comes from .env instead of the visible file.
        let mut a = args("secret_token");
        a.glob = Some(".env*".into());
        let out = handle(&st, a).unwrap();
        assert_eq!(out["files_searched"], json!(1));
        assert_eq!(
            out["matches"][0]["text"],
            json!("secret_token=1"),
            "hidden .env was searched because the glob named it"
        );

        // Explicit hidden directory root is searched.
        let mut a = args("secret_token");
        a.path = Some(".config".into());
        let out = handle(&st, a).unwrap();
        assert_eq!(out["files_searched"], json!(1));
        assert_eq!(out["match_count"], json!(1));

        // .git is never searched even when a broad glob is given.
        std::fs::create_dir_all(tmp.path().join(".git")).unwrap();
        std::fs::write(tmp.path().join(".git/leaked.txt"), "secret_token\n").unwrap();
        let mut a = args("secret_token");
        a.glob = Some("*".into());
        let out = handle(&st, a).unwrap();
        assert_eq!(
            out["files_searched"].as_u64().unwrap(),
            3,
            "must see normal.txt + .env + .config/creds.txt — but never .git/leaked.txt"
        );
    }

    #[test]
    fn generated_directories_still_skipped() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/pkg")).unwrap();
        std::fs::write(
            tmp.path().join("node_modules/pkg/index.js"),
            "targetstring\n",
        )
        .unwrap();
        std::fs::write(tmp.path().join("src.rs"), "targetstring\n").unwrap();
        let st = state(tmp.path());
        let out = handle(&st, args("targetstring")).unwrap();
        assert_eq!(out["files_searched"], json!(1));
        assert_eq!(out["matches"][0]["line"], json!(1));
    }

    #[test]
    fn every_match_carries_workspace_relative_path() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("src/auth")).unwrap();
        std::fs::write(tmp.path().join("src/auth/login.rs"), "handle_login();\n").unwrap();
        std::fs::write(
            tmp.path().join("src/logout.rs"),
            "handle_logout();\nhandle_login_admin();\n",
        )
        .unwrap();

        let st = state(tmp.path());
        let out = handle(&st, args("handle_")).unwrap();
        let matches = out["matches"].as_array().unwrap();
        assert_eq!(matches.len(), 3);

        // File visit order is unspecified; look hits up per file.
        let login: Vec<_> = matches
            .iter()
            .filter(|m| m["path"] == json!("src/auth/login.rs"))
            .collect();
        assert_eq!(login.len(), 1);
        assert_eq!(login[0]["line"], json!(1));
        assert_eq!(login[0]["text"], json!("handle_login();"));

        let logout: Vec<_> = matches
            .iter()
            .filter(|m| m["path"] == json!("src/logout.rs"))
            .collect();
        assert_eq!(logout.len(), 2);
        assert_eq!(logout[0]["line"], json!(1));
        assert_eq!(logout[0]["text"], json!("handle_logout();"));
        assert_eq!(logout[1]["line"], json!(2));
        for m in matches {
            let p = m["path"].as_str().unwrap();
            assert!(!p.starts_with('/'), "absolute path leaked: {p}");
            assert!(!p.contains(".."), "traversal leaked: {p}");
        }
    }
}
