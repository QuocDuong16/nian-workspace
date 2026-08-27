//! Dedicated Git tools (spec section 11): `git_status` and `git_diff`.
//!
//! Both shell out to the system `git` with fixed argument lists — arbitrary
//! Git commands are deliberately not exposed; clients holding `--exec` can
//! use `run_command` for anything beyond these two read-only views.

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use rmcp::schemars;
use serde_json::json;
use std::io::Read;
use std::path::Path;
use std::process::Stdio;

#[derive(Debug, Default, serde::Deserialize, schemars::JsonSchema)]
pub struct GitStatusArgs {}

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct GitDiffArgs {
    /// Show staged changes instead of unstaged ones (default false).
    #[serde(default)]
    #[schemars(description = "Show staged (cached) changes instead of unstaged changes.")]
    pub staged: bool,

    /// Limit the diff to one path relative to the workspace root.
    #[serde(default)]
    #[schemars(
        description = "Optional path relative to the workspace root to restrict the diff to."
    )]
    pub path: Option<String>,
}

fn inside_git_worktree(root: &Path) -> bool {
    std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

struct GitOutput {
    stdout: String,
}

/// Run `git` with a fixed argument list, draining both pipes concurrently so
/// no pipe buffer can deadlock, and bounding captured stdout.
fn run_git_bounded(root: &Path, extra_args: &[&str], cap: usize) -> ToolResult<GitOutput> {
    let mut child = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(extra_args)
        .env("GIT_OPTIONAL_LOCKS", "1")
        // Defensive: stale env from parent shells must not redirect the
        // repository we operate on.
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| ToolError::msg(format!("Failed to start git: {e}")))?;

    let mut stdout_pipe = child.stdout.take().expect("git stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("git stderr piped");

    let drain_stderr = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let _ = stderr_pipe.read_to_end(&mut buf);
        buf
    });

    let mut stdout_bytes = Vec::new();
    loop {
        let mut chunk = [0u8; 16 * 1024];
        match stdout_pipe.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                let take = n.min(cap.saturating_sub(stdout_bytes.len()));
                stdout_bytes.extend_from_slice(&chunk[..take]);
                if stdout_bytes.len() >= cap {
                    let _ = stdout_pipe.read_to_end(&mut Vec::new());
                    break;
                }
            }
            Err(e) => return Err(ToolError::msg(format!("Failed reading git output: {e}"))),
        }
    }

    let status = child
        .wait()
        .map_err(|e| ToolError::msg(format!("git failed: {e}")))?;
    let stderr_bytes = drain_stderr.join().unwrap_or_default();
    let exit_code = status.code().unwrap_or(-1);

    // Exit code 1 from `git diff` means "differences found" — legitimate.
    // Anything else non-zero is a real failure; surface git's stderr verbatim.
    if exit_code != 0 && exit_code != 1 {
        let stderr_text = String::from_utf8_lossy(&stderr_bytes);
        return Err(ToolError::msg(format!(
            "git exited with code {exit_code}: {}",
            stderr_text.trim()
        )));
    }

    let (stdout, _) = crate::tools::decode_lossy(&stdout_bytes);
    Ok(GitOutput { stdout })
}

pub(crate) fn git_status(state: &AppState, _args: GitStatusArgs) -> ToolResult<serde_json::Value> {
    ensure_git(state)?;
    let ws = state.workspace();
    let cap = state.limits().max_git_output;

    let out = run_git_bounded(ws.root(), &["status", "--short", "--branch"], cap)?;

    Ok(json!({
        "output": out.stdout,
        "truncated": out.stdout.len() >= cap,
    }))
}

pub(crate) fn git_diff(state: &AppState, args: GitDiffArgs) -> ToolResult<serde_json::Value> {
    ensure_git(state)?;
    let ws = state.workspace();
    let cap = state.limits().max_git_output;

    let resolved_path = match args.path.as_deref() {
        Some(p) => {
            let resolved = ws
                .resolve(Some(p))
                .map_err(|e| ToolError::msg(format!("Invalid diff path '{p}': {e}")))?;
            Some(ws.display_relative(&resolved))
        }
        None => None,
    };

    let mut cmd_args: Vec<&str> = vec!["diff", "--no-color"];
    if args.staged {
        cmd_args.push("--cached");
    }
    if let Some(rel) = &resolved_path {
        cmd_args.push("--");
        cmd_args.push(rel.as_str());
    }

    let out = run_git_bounded(ws.root(), &cmd_args, cap)?;

    Ok(json!({
        "staged": args.staged,
        "path": resolved_path,
        "diff": out.stdout,
        "truncated": out.stdout.len() >= cap,
    }))
}

fn ensure_git(state: &AppState) -> ToolResult<()> {
    if inside_git_worktree(state.workspace().root()) {
        Ok(())
    } else {
        Err(ToolError::msg(format!(
            "'{}' does not appear to be inside a Git working tree.",
            state.workspace().root().display()
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::permissions::Permissions;
    use crate::workspace::Workspace;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    fn git(dir: &Path, args: &[&str]) {
        let out = StdCommand::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .env("GIT_AUTHOR_NAME", "t")
            .env("GIT_AUTHOR_EMAIL", "t@example.com")
            .env("GIT_COMMITTER_NAME", "t")
            .env("GIT_COMMITTER_EMAIL", "t@example.com")
            .output()
            .expect("git should be installed for tests");
        assert!(
            out.status.success(),
            "git {:?} failed: {}",
            args,
            String::from_utf8_lossy(&out.stderr)
        );
    }

    fn repo_state() -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git(root, &["init", "--initial-branch=main"]);
        std::fs::write(root.join("tracked.txt"), "line one\nline two\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "initial"]);
        let ws = Workspace::open(root).unwrap();
        let state = AppState::new(ws, Permissions::default(), Limits::default());
        (tmp, state)
    }

    #[test]
    fn status_shows_branch_and_clean_tree() {
        let (_t, state) = repo_state();
        let out = git_status(&state, GitStatusArgs {}).unwrap();
        let output = out["output"].as_str().unwrap();
        assert!(
            output.contains("## main"),
            "expected branch header in: {output}"
        );
        assert!(
            !output.contains('M'),
            "clean tree should have no modifications"
        );
    }

    #[test]
    fn status_reports_untracked_and_modified_files() {
        let (_t, state) = repo_state();
        std::fs::write(state.workspace().root().join("new.txt"), b"x\n").unwrap();
        std::fs::write(
            state.workspace().root().join("tracked.txt"),
            b"line one\nchanged\n",
        )
        .unwrap();

        let out = git_status(&state, GitStatusArgs {}).unwrap();
        let output = out["output"].as_str().unwrap();
        assert!(
            output.contains("?? new.txt"),
            "untracked file missing: {output}"
        );
        assert!(
            output.contains(" M tracked.txt"),
            "modified file missing: {output}"
        );
    }

    #[test]
    fn diff_shows_unstaged_changes() {
        let (_t, state) = repo_state();
        std::fs::write(
            state.workspace().root().join("tracked.txt"),
            b"line one\nCHANGED\n",
        )
        .unwrap();

        let out = git_diff(
            &state,
            GitDiffArgs {
                staged: false,
                path: None,
            },
        )
        .unwrap();
        let diff = out["diff"].as_str().unwrap();
        assert!(
            diff.contains("+CHANGED"),
            "diff body missing addition: {diff}"
        );
        assert_eq!(out["truncated"], json!(false));
    }

    #[test]
    fn staged_flag_uses_cached_diff() {
        let (_t, state) = repo_state();
        let root = state.workspace().root();
        std::fs::write(root.join("tracked.txt"), b"line one\nSTAGED\n").unwrap();
        git(root, &["add", "tracked.txt"]);

        // Unstaged diff is empty; staged diff contains the change.
        let unstaged = git_diff(
            &state,
            GitDiffArgs {
                staged: false,
                path: None,
            },
        )
        .unwrap();
        assert!(unstaged["diff"].as_str().unwrap().trim().is_empty());

        let staged = git_diff(
            &state,
            GitDiffArgs {
                staged: true,
                path: None,
            },
        )
        .unwrap();
        assert!(staged["diff"].as_str().unwrap().contains("+STAGED"));
        assert_eq!(staged["staged"], json!(true));
    }

    #[test]
    fn diff_can_be_restricted_to_one_path() {
        let (_t, state) = repo_state();
        let root = state.workspace().root();
        std::fs::write(root.join("tracked.txt"), b"line one\nXXX\n").unwrap();
        std::fs::write(root.join("other.txt"), b"brand new\n").unwrap();

        let out = git_diff(
            &state,
            GitDiffArgs {
                staged: false,
                path: Some("tracked.txt".into()),
            },
        )
        .unwrap();
        let diff = out["diff"].as_str().unwrap();
        assert!(diff.contains("+XXX"));
        assert!(
            !diff.contains("other.txt"),
            "path filter leaked other file: {diff}"
        );
    }

    #[test]
    fn tools_fail_outside_a_git_repository() {
        let tmp = TempDir::new().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        let state = AppState::new(ws, Permissions::default(), Limits::default());

        let err = git_status(&state, GitStatusArgs {}).unwrap_err();
        assert!(err.to_string().contains("Git working tree"));
        let err = git_diff(
            &state,
            GitDiffArgs {
                staged: false,
                path: None,
            },
        )
        .unwrap_err();
        assert!(err.to_string().contains("Git working tree"));
    }
}
