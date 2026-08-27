//! Dedicated Git tools (spec section 11): `git_status` and `git_diff`.
//!
//! Both shell out to the system `git` through the shared hardened helper
//! ([`crate::tools::git_process`]) with fixed argument lists — arbitrary Git
//! commands are deliberately not exposed; clients holding `--exec` can use
//! `run_command` for anything beyond these two read-only views. External
//! diff/textconv/fsmonitor/pager execution paths are disabled there and here.

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use crate::tools::git_process::{inside_git_worktree, run_git_bounded, GitInvocation};
use rmcp::schemars;
use serde_json::json;

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

pub(crate) fn git_status(state: &AppState, _args: GitStatusArgs) -> ToolResult<serde_json::Value> {
    ensure_git(state)?;
    let ws = state.workspace();
    let cap = state.limits().max_git_output;

    let out = run_git_bounded(&GitInvocation {
        root: ws.root(),
        args: &["--no-pager", "status", "--short", "--branch"],
        cap,
        ok_exit_codes: &[0],
    })?;

    Ok(json!({
        "output": out.stdout,
        "truncated": out.truncated,
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

    // --no-ext-diff / --no-textconv make it impossible for a crafted
    // repository (diff.external, diff.<driver>.textconv in .git/config or
    // attributes) to execute an external program during our diff.
    let mut cmd_args: Vec<&str> = vec![
        "--no-pager",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
    ];
    if args.staged {
        cmd_args.push("--cached");
    }
    if let Some(rel) = &resolved_path {
        cmd_args.push("--");
        cmd_args.push(rel.as_str());
    }

    let out = run_git_bounded(&GitInvocation {
        root: ws.root(),
        args: &cmd_args,
        cap,
        ok_exit_codes: &[0],
    })?;

    Ok(json!({
        "staged": args.staged,
        "path": resolved_path,
        "diff": out.stdout,
        "truncated": out.truncated,
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
    use std::path::Path;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    /// Point every git child at empty global/system config so results cannot
    /// depend on the CI host's ~/.gitconfig (e.g. status.showUntrackedFiles).
    /// Idempotent and race-free: all writers set identical values.
    fn isolate_host_config() {
        std::env::set_var("GIT_CONFIG_GLOBAL", "/dev/null");
        std::env::set_var("GIT_CONFIG_SYSTEM", "/dev/null");
        std::env::set_var("GIT_CONFIG_NOSYSTEM", "1");
    }

    fn git(dir: &Path, args: &[&str]) {
        isolate_host_config();
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
        isolate_host_config();
        let (_t, state) = repo_state();
        let out = git_status(&state, GitStatusArgs {}).expect("status should succeed");
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
        isolate_host_config();
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
        isolate_host_config();
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
        isolate_host_config();
        let (_t, state) = repo_state();
        let root = state.workspace().root();
        std::fs::write(root.join("tracked.txt"), b"line one\nSTAGED\n").unwrap();
        git(root, &["add", "tracked.txt"]);

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
        isolate_host_config();
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
        isolate_host_config();
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

    #[test]
    fn malicious_repo_config_cannot_execute_external_diff() {
        isolate_host_config();
        // Configure an external diff driver that would create a marker file
        // if executed; git_status/git_diff must NOT run it.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git(root, &["init", "--initial-branch=main"]);
        std::fs::write(root.join("payload.txt"), "before\n").unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "initial"]);

        let marker = root.join("external-diff-ran");
        let fake_driver = format!("#!/bin/sh\ntouch '{}'\n", marker.display());
        std::fs::write(root.join("fake-driver.sh"), fake_driver).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(
                root.join("fake-driver.sh"),
                std::fs::Permissions::from_mode(0o755),
            )
            .unwrap();
        }
        let driver = root.join("fake-driver.sh").display().to_string();
        git(root, &["config", "diff.external", &driver]);
        git(root, &["config", "diff.payload.driver", &driver]);
        std::fs::write(root.join(".gitattributes"), "payload.txt diff=payload\n").unwrap();

        std::fs::write(root.join("payload.txt"), "after\n").unwrap();

        let ws = Workspace::open(root).unwrap();
        let state = AppState::new(ws, Permissions::default(), Limits::default());
        let out = git_diff(
            &state,
            GitDiffArgs {
                staged: false,
                path: None,
            },
        )
        .expect("hardened diff must still succeed against hostile config");
        assert!(out["diff"].as_str().unwrap().contains("+after"));

        let result = git_status(&state, GitStatusArgs {});
        assert!(result.is_ok());

        // Give any hypothetically-spawned external process time to run.
        std::thread::sleep(std::time::Duration::from_millis(150));
        assert!(
            !marker.exists(),
            "external diff driver was executed despite --no-ext-diff hardening"
        );
    }

    #[test]
    fn environment_redirection_is_ignored() {
        isolate_host_config();
        // GIT_DIR/GIT_WORK_TREE pointing elsewhere must not redirect our
        // queries away from the workspace repository. The helper strips
        // these variables for every child it spawns.
        let (tmp_a, state_a) = repo_state();
        let tmp_b = TempDir::new().unwrap();
        git(tmp_b.path(), &["init", "--initial-branch=main"]);
        std::fs::write(tmp_b.path().join("decoy.txt"), "decoy\n").unwrap();
        git(tmp_b.path(), &["add", "."]);
        git(
            tmp_b.path(),
            &[
                "-c",
                "user.email=t@example.com",
                "-c",
                "user.name=t",
                "commit",
                "-m",
                "x",
            ],
        );

        let prev_dir = std::env::var("GIT_DIR").ok();
        let prev_tree = std::env::var("GIT_WORK_TREE").ok();
        std::env::set_var("GIT_DIR", tmp_b.path().join(".git").display().to_string());
        std::env::set_var("GIT_WORK_TREE", tmp_b.path().display().to_string());

        let result = git_status(&state_a, GitStatusArgs {});

        // Restore before asserting so failure paths don't poison other tests.
        match (prev_dir, prev_tree) {
            (Some(d), Some(t)) => {
                std::env::set_var("GIT_DIR", d);
                std::env::set_var("GIT_WORK_TREE", t);
            }
            _ => {
                std::env::remove_var("GIT_DIR");
                std::env::remove_var("GIT_WORK_TREE");
            }
        }

        let out = result.expect("status should succeed despite redirection env");
        let output = out["output"].as_str().unwrap();
        assert!(
            output.contains("## main"),
            "wrong repository queried: {output}"
        );
        assert!(
            !output.contains("decoy"),
            "env redirected the query: {output}"
        );
        drop(tmp_a);
    }

    #[test]
    fn oversized_git_output_is_bounded() {
        isolate_host_config();
        // A file large enough that its diff exceeds a tiny cap must still be
        // processed without unbounded retention, reporting truncation.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git(root, &["init", "--initial-branch=main"]);
        let big_before = "x\n".repeat(1_000);
        std::fs::write(root.join("big.txt"), big_before).unwrap();
        git(root, &["add", "."]);
        git(root, &["commit", "-m", "initial"]);
        let big_after = "y".repeat(4 * 1024 * 1024).into_bytes();
        std::fs::write(root.join("big.txt"), big_after).unwrap();

        let ws = Workspace::open(root).unwrap();
        let state = AppState::new(
            ws,
            Permissions::default(),
            Limits {
                max_git_output: 4096,
                ..Limits::default()
            },
        );
        let out = git_diff(
            &state,
            GitDiffArgs {
                staged: false,
                path: None,
            },
        )
        .unwrap();
        assert_eq!(out["truncated"], json!(true));
        assert!(out["diff"].as_str().unwrap().len() <= 4096 + 1024);
    }

    #[test]
    fn nonzero_git_exit_becomes_an_error() {
        isolate_host_config();
        let (_t, state) = repo_state();
        // Ask the hardened runner directly with a command that fails when its
        // exit code is not accepted. `git status --unsupported-flag` exits 129.
        let root = state.workspace().root().to_path_buf();
        let invocation = GitInvocation {
            root: &root,
            args: &["status", "--definitely-not-a-real-flag"],
            cap: 1024,
            ok_exit_codes: &[0],
        };
        let err = run_git_bounded(&invocation).unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("git exited with code"),
            "expected exit-code error, got: {msg}"
        );
    }
}
