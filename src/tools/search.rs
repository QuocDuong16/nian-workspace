//! `search` — fast bounded text search with ripgrep-like semantics
//! (spec section 7.4), built on the `grep` crate family.

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use crate::tools::{is_generated_dir, is_vcs_metadata_dir};
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

    /// Directory or file to search, relative to the workspace root. Defaults to the whole workspace. Hidden entries and generated directories are searched only when the requested path itself enters that territory (e.g. ".config", "node_modules"); VCS metadata (.git/.hg/.svn) is never searchable, not even through symlinks.
    #[serde(default)]
    #[schemars(
        description = "Directory or file to search, relative to the workspace root. Defaults to the whole workspace. Version-control metadata (.git/.hg/.svn) is rejected — even through symlink aliases; hidden or generated directories inside the requested path are searched only when the path itself enters that territory (e.g. path='.config' or path='node_modules')."
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

/// Search visibility policy (see handle() for how the requested-path state is
/// derived):
/// * VCS metadata directories are pruned ALWAYS, anywhere in the walk
/// * generated directories are entered only when the requested path itself
///   sits inside generated territory (e.g. path="node_modules")
/// * hidden entries are entered only when the requested path sits inside
///   hidden territory (e.g. path=".config"), or the glob names them
///   explicitly
fn entry_admissible(
    entry: &walkdir::DirEntry,
    requested_hidden: bool,
    requested_generated: bool,
    glob: &Option<globset::GlobMatcher>,
) -> bool {
    if entry.depth() == 0 {
        // The requested root was already screened by handle(): it exists
        // inside the workspace and is not VCS metadata.
        return true;
    }
    let name = entry.file_name().to_string_lossy();
    if entry.file_type().is_dir() && is_vcs_metadata_dir(&name) {
        return false; // even under an explicitly requested subtree
    }
    if name.starts_with('.') {
        if requested_hidden {
            return true;
        }
        if let Some(glob) = glob {
            if glob.is_match(name.as_ref()) {
                return true;
            }
        }
        return false;
    }
    if entry.file_type().is_dir() && is_generated_dir(name.as_ref()) {
        return requested_generated;
    }
    true
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

    // Policy is decided from the RESOLVED target, not the raw request:
    // Workspace::resolve canonicalizes symlinks, so these components reflect
    // what the walk will physically enter.
    //
    // A. VCS metadata (.git/.hg/.svn) is rejected outright, wherever it
    //    appears in the resolved path — including a symlink alias such as
    //    `metadata -> .git`, and independent of letter casing (Windows
    //    filesystems are commonly case-insensitive).
    // B. requested_hidden: the requested path sits inside hidden territory
    //    (e.g. ".config" or "src/.env"), so hidden entries are admitted.
    //    Merely rooting at "src" does NOT admit "src/.hidden".
    // C. requested_generated: the requested path sits inside generated
    //    territory (e.g. "node_modules" or "node_modules/pkg"), so generated
    //    directories are admitted. Merely rooting at "src" does NOT admit
    //    "src/node_modules".
    let rel_components: Vec<String> = target
        .strip_prefix(ws.root())
        .ok()
        .map(|rel| {
            rel.components()
                .filter_map(|c| match c {
                    std::path::Component::Normal(name) => Some(name.to_string_lossy().into_owned()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default();

    if let Some(vcs_dir) = rel_components.iter().find(|c| is_vcs_metadata_dir(c)) {
        return Err(ToolError::msg(format!(
            "'{}' lies inside version-control metadata ('{vcs_dir}'); \
             the search tool never inspects VCS internals.",
            args.path.as_deref().unwrap_or("<workspace root>")
        )));
    }

    let requested_hidden = rel_components.iter().any(|c| c.starts_with('.'));
    let requested_generated = rel_components.iter().any(|c| is_generated_dir(c));

    // Existence is checked only after the VCS policy: a request for .git
    // (under any casing or alias) is rejected with the VCS message even when
    // the path does not exist, never with a probe-friendly existence answer.
    if !target.exists() {
        return Err(ToolError::msg(format!(
            "Search path does not exist: {}",
            ws.display_relative(&target)
        )));
    }

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
        .filter_entry(|e| entry_admissible(e, requested_hidden, requested_generated, &glob_set))
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

    // -- explicit generated / VCS boundary policy ----------------------------

    #[test]
    fn path_src_does_not_expose_hidden_or_generated_descendants() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        std::fs::create_dir_all(src.join(".hidden")).unwrap();
        std::fs::create_dir_all(src.join("node_modules/pkg")).unwrap();
        std::fs::create_dir_all(src.join("target/debug")).unwrap();
        std::fs::write(src.join(".hidden/secret.txt"), "marker\n").unwrap();
        std::fs::write(src.join("node_modules/pkg/index.js"), "marker\n").unwrap();
        std::fs::write(src.join("target/debug/artifact"), "marker\n").unwrap();
        std::fs::write(src.join("code.rs"), "marker\n").unwrap();

        let st = state(tmp.path());
        let mut a = args("marker");
        a.path = Some("src".into());
        let out = handle(&st, a).unwrap();
        assert_eq!(
            out["files_searched"],
            json!(1),
            "rooting at 'src' must not unlock hidden/generated descendants: {out}"
        );
        assert_eq!(out["matches"][0]["path"], json!("src/code.rs"));
    }

    #[test]
    fn explicit_hidden_path_admits_hidden_subtree_but_not_generated_children() {
        let tmp = TempDir::new().unwrap();
        let cfg = tmp.path().join(".config");
        std::fs::create_dir_all(cfg.join(".deep")).unwrap();
        std::fs::create_dir_all(cfg.join("node_modules/pkg")).unwrap();
        std::fs::write(cfg.join("creds.txt"), "marker\n").unwrap();
        std::fs::write(cfg.join(".deep/y.txt"), "marker\n").unwrap();
        std::fs::write(cfg.join("node_modules/pkg/index.js"), "marker\n").unwrap();

        let st = state(tmp.path());
        let mut a = args("marker");
        a.path = Some(".config".into());
        let out = handle(&st, a).unwrap();
        assert_eq!(
            out["files_searched"],
            json!(2),
            "hidden subtree admitted, generated child still skipped: {out}"
        );
        let paths: Vec<&str> = out["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap())
            .collect();
        assert!(paths.contains(&".config/creds.txt"));
        assert!(paths.contains(&".config/.deep/y.txt"));
        assert!(
            !paths.iter().any(|p| p.contains("node_modules")),
            "generated dir under an explicit hidden root leaked: {paths:?}"
        );
    }

    #[test]
    fn explicit_generated_path_allows_generated_subtree() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/pkg/inner")).unwrap();
        std::fs::write(
            tmp.path().join("node_modules/pkg/inner/index.js"),
            "marker\n",
        )
        .unwrap();
        // Outside the requested subtree a generated dir stays pruned.
        std::fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        std::fs::write(tmp.path().join("target/debug/artifact"), "marker\n").unwrap();

        let st = state(tmp.path());
        let mut a = args("marker");
        a.path = Some("node_modules/pkg".into());
        let out = handle(&st, a).unwrap();
        assert_eq!(out["match_count"], json!(1));
        assert_eq!(
            out["matches"][0]["path"],
            json!("node_modules/pkg/inner/index.js")
        );
    }

    #[test]
    fn explicit_generated_directory_is_searchable() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("node_modules/left-pad")).unwrap();
        std::fs::write(
            tmp.path().join("node_modules/left-pad/index.js"),
            "marker_here\n",
        )
        .unwrap();
        // Outside the requested subtree a generated dir stays pruned.
        std::fs::create_dir_all(tmp.path().join("target/debug")).unwrap();
        std::fs::write(tmp.path().join("target/debug/artifact"), "marker_here\n").unwrap();

        let st = state(tmp.path());
        let mut a = args("marker_here");
        a.path = Some("node_modules".into());
        let out = handle(&st, a).expect("explicit node_modules path must be allowed");
        assert_eq!(out["match_count"], json!(1));
        let p = out["matches"][0]["path"].as_str().unwrap();
        assert!(p.starts_with("node_modules/"), "unexpected match path: {p}");
    }

    #[test]
    fn explicit_vcs_metadata_roots_are_rejected() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/refs")).unwrap();
        std::fs::write(tmp.path().join(".git/config"), "[core]\n").unwrap();
        std::fs::create_dir_all(tmp.path().join(".hg")).unwrap();
        std::fs::create_dir_all(tmp.path().join(".svn")).unwrap();
        let st = state(tmp.path());

        for attempt in [".git", ".git/refs", ".hg", ".svn"] {
            let mut a = args("core");
            a.path = Some(attempt.into());
            let err = handle(&st, a).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("version-control metadata"),
                "'{attempt}' must be rejected with the VCS message, got: {msg}"
            );
        }
    }

    #[test]
    fn recursive_search_never_enters_git_even_via_glob() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("top.txt"), "needle_nested_git\n").unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/objects/pack")).unwrap();
        std::fs::write(
            tmp.path().join(".git/objects/pack/found.txt"),
            "needle_nested_git\n",
        )
        .unwrap();
        let st = state(tmp.path());

        let mut a = args("needle_nested_git");
        a.glob = Some("*".into());
        let out = handle(&st, a).unwrap();
        assert!(
            !serde_json::to_string(&out).unwrap().contains(".git"),
            ".git content leaked into results: {out}"
        );

        // The only conceivable route to .git content — an explicit root — is
        // rejected outright by handle().
        let mut a = args("needle_nested_git");
        a.path = Some(".git".into());
        assert!(
            handle(&st, a).is_err(),
            "explicit .git root must be rejected"
        );
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

    // -- resolved-path VCS metadata enforcement ------------------------------

    #[cfg(unix)]
    #[test]
    fn symlink_alias_into_vcs_metadata_is_rejected() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join(".git/refs")).unwrap();
        std::fs::write(tmp.path().join(".git/config"), "[core]\n").unwrap();
        std::os::unix::fs::symlink(".git", tmp.path().join("metadata")).unwrap();

        let st = state(tmp.path());
        for attempt in ["metadata", "metadata/config", "metadata/refs"] {
            let mut a = args("core");
            a.path = Some(attempt.into());
            let err = handle(&st, a).unwrap_err();
            assert!(
                err.to_string().contains("version-control metadata"),
                "'{attempt}' resolves into .git and must be rejected, got: {err}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn normal_symlink_to_ordinary_directory_is_searchable() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("real")).unwrap();
        std::fs::write(tmp.path().join("real/data.txt"), "ordinary_marker\n").unwrap();
        std::os::unix::fs::symlink("real", tmp.path().join("alias")).unwrap();

        let st = state(tmp.path());
        let mut a = args("ordinary_marker");
        a.path = Some("alias".into());
        let out = handle(&st, a).unwrap();
        assert_eq!(out["match_count"], json!(1));
        // The walk follows the resolved physical location, so the match is
        // reported under the physical workspace-relative path.
        assert_eq!(out["matches"][0]["path"], json!("real/data.txt"));
    }

    #[test]
    fn vcs_metadata_names_match_case_insensitively() {
        // On case-insensitive Windows filesystems .GIT may be the same
        // directory as .git; the policy must not fail on casing.
        let tmp = TempDir::new().unwrap();
        let st = state(tmp.path());
        for attempt in [".GIT", ".Git", ".HG", ".SVN"] {
            let mut a = args("x");
            a.path = Some(attempt.into());
            let err = handle(&st, a).unwrap_err();
            assert!(
                err.to_string().contains("version-control metadata"),
                "'{attempt}' must be treated as VCS metadata, got: {err}"
            );
        }
    }
}
