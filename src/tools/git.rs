//! Dedicated Git tools (spec section 11): `git_status` and `git_diff`.
//!
//! Both shell out to the system `git` through the shared hardened helper
//! ([`crate::tools::git_process`]) with fixed argument lists — arbitrary Git
//! commands are deliberately not exposed; clients holding `--exec` can use
//! `run_command` for anything beyond these two read-only views. External
//! diff/textconv/fsmonitor/pager execution paths are disabled there and here.
//!
//! The behavior lives in context-based cores ([`git_status_for_context`] /
//! [`git_diff_for_context`]) shared by both server modes (v0.2 M4): the
//! single-mode wrappers resolve the fixed workspace, and the registry-mode
//! wrappers select a workspace by logical [`WorkspaceId`] and attach
//! provenance to the response. Client-visible path rendering — including
//! error text — follows the mode's [`PathPresentation`] contract, so
//! registry responses and errors never disclose canonical roots.

use crate::config::{AppState, Limits};
use crate::error::{ToolError, ToolResult};
use crate::tools::discovery::{resolve_registry_workspace, with_workspace_provenance};
use crate::tools::git_process::{inside_git_worktree, run_git_bounded, GitInvocation};
use crate::workspace::{PathPresentation, WorkspaceContext};
use crate::workspace_id::WorkspaceId;
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

/// Registry-mode `git_status` input: the logical workspace selector. The
/// single-mode `git_status` takes no further arguments, so the registry API
/// is exactly `{ workspace }` — no invented Git options.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RegistryGitStatusArgs {
    /// Logical workspace ID to operate on — exactly one of the IDs reported by list_workspaces.
    #[schemars(
        description = "Logical workspace ID to operate on — exactly one of the IDs reported by list_workspaces, as configured by the operator at startup. Not a path."
    )]
    pub workspace: WorkspaceId,
    #[serde(flatten)]
    pub args: GitStatusArgs,
}

/// Registry-mode `git_diff` input: the logical workspace selector plus the
/// unchanged single-mode arguments, flattened into one MCP input schema
/// (no nested `args` object).
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RegistryGitDiffArgs {
    /// Logical workspace ID to operate on — exactly one of the IDs reported by list_workspaces.
    #[schemars(
        description = "Logical workspace ID to operate on — exactly one of the IDs reported by list_workspaces, as configured by the operator at startup. Not a path."
    )]
    pub workspace: WorkspaceId,
    #[serde(flatten)]
    pub args: GitDiffArgs,
}

/// Single-workspace mode entry point (v0.1 behavior, unchanged).
pub(crate) fn git_status(state: &AppState, args: GitStatusArgs) -> ToolResult<serde_json::Value> {
    git_status_for_context(
        state.single_workspace(),
        state.limits(),
        PathPresentation::SingleCompatible,
        args,
    )
}

/// Single-workspace mode entry point (v0.1 behavior, unchanged).
pub(crate) fn git_diff(state: &AppState, args: GitDiffArgs) -> ToolResult<serde_json::Value> {
    git_diff_for_context(
        state.single_workspace(),
        state.limits(),
        PathPresentation::SingleCompatible,
        args,
    )
}

/// Registry-mode `git_status`: exact [`WorkspaceId`] lookup, the shared
/// context-based core, then logical workspace provenance in the response.
pub(crate) fn registry_git_status(
    state: &AppState,
    args: RegistryGitStatusArgs,
) -> ToolResult<serde_json::Value> {
    let RegistryGitStatusArgs { workspace, args } = args;
    let ctx = resolve_registry_workspace(state, &workspace)?;
    let value = git_status_for_context(
        &ctx,
        state.limits(),
        PathPresentation::RegistryRelative,
        args,
    )?;
    Ok(with_workspace_provenance(value, &workspace))
}

/// Registry-mode `git_diff`: exact [`WorkspaceId`] lookup, the shared
/// context-based core, then logical workspace provenance in the response.
pub(crate) fn registry_git_diff(
    state: &AppState,
    args: RegistryGitDiffArgs,
) -> ToolResult<serde_json::Value> {
    let RegistryGitDiffArgs { workspace, args } = args;
    let ctx = resolve_registry_workspace(state, &workspace)?;
    let value = git_diff_for_context(
        &ctx,
        state.limits(),
        PathPresentation::RegistryRelative,
        args,
    )?;
    Ok(with_workspace_provenance(value, &workspace))
}

/// Context-based core shared by both server modes: identical hardened status
/// behavior, rooted at the selected context. `presentation` decides how
/// client-visible paths are rendered, including failure text.
pub(crate) fn git_status_for_context(
    ctx: &WorkspaceContext,
    limits: &Limits,
    presentation: PathPresentation,
    _args: GitStatusArgs,
) -> ToolResult<serde_json::Value> {
    ensure_git(ctx, presentation)?;
    let cap = limits.max_git_output;

    // Pathspec "-- ." is the workspace-boundary guarantee: when the
    // workspace is a subdirectory of a larger repository, plain `status`
    // would report changes for the WHOLE repository (git discovers the repo
    // root upward). Restricting to "." keeps output inside the selected
    // workspace, no matter where the enclosing repo extends — independently
    // for every selected context, without relying on process-global state
    // (v0.2 M4 parent-repository isolation).
    //
    // Path text is pinned workspace-relative by the shared hardened config
    // (`-c status.relativePaths=true`); a user's global `relativePaths=false`
    // would otherwise leak the enclosing repo's directory prefix into every
    // line.
    let out = run_git_bounded(&GitInvocation {
        root: ctx.root(),
        args: &["--no-pager", "status", "--short", "--branch", "--", "."],
        cap,
        ok_exit_codes: &[0],
    })
    .map_err(|err| presented_git_error(&err.to_string(), ctx, presentation))?;

    Ok(json!({
        "output": out.stdout,
        "truncated": out.stdout_truncated,
    }))
}

/// Context-based core shared by both server modes: identical hardened diff
/// behavior, rooted at the selected context.
pub(crate) fn git_diff_for_context(
    ctx: &WorkspaceContext,
    limits: &Limits,
    presentation: PathPresentation,
    args: GitDiffArgs,
) -> ToolResult<serde_json::Value> {
    ensure_git(ctx, presentation)?;
    let ws = ctx.resolver();
    let cap = limits.max_git_output;

    let resolved_path = match args.path.as_deref() {
        Some(p) => {
            // The workspace resolver stays the single path-isolation
            // authority: the diff pathspec is derived only from an
            // already-validated workspace-relative path, never from raw
            // client input, so a Git path selector can never become a second
            // way to address another workspace or the parent repository.
            let resolved = ws
                .resolve(Some(p))
                .map_err(|e| ToolError::msg(format!("Invalid diff path '{p}': {e}")))?;
            Some(ws.display_relative_as(&resolved, presentation))
        }
        None => None,
    };

    // --no-ext-diff / --no-textconv make it impossible for a crafted
    // repository (diff.external, diff.<driver>.textconv in .git/config or
    // attributes) to execute an external program during our diff.
    //
    // --relative renders a/ and b/ headers relative to the invocation cwd
    // (pinned to the workspace root), not the enclosing repository root, so
    // a nested workspace's diff can be fed straight into apply_patch —
    // whose paths resolve against the workspace root — without producing a
    // workspace/workspace/… target.
    //
    // Trailing pathspec: a specific workspace-relative path when given
    // (already validated through Workspace::resolve), otherwise "." — the
    // same scoping rule as git_status. ../ or absolute external pathspecs
    // never reach this point.
    let mut cmd_args: Vec<&str> = vec![
        "--no-pager",
        "diff",
        "--no-color",
        "--no-ext-diff",
        "--no-textconv",
        "--relative",
    ];
    if args.staged {
        cmd_args.push("--cached");
    }
    cmd_args.push("--");
    match &resolved_path {
        Some(rel) => cmd_args.push(rel.as_str()),
        None => cmd_args.push("."),
    }

    let out = run_git_bounded(&GitInvocation {
        root: ctx.root(),
        args: &cmd_args,
        cap,
        ok_exit_codes: &[0],
    })
    .map_err(|err| presented_git_error(&err.to_string(), ctx, presentation))?;

    Ok(json!({
        "staged": args.staged,
        "path": resolved_path,
        "diff": out.stdout,
        "truncated": out.stdout_truncated,
    }))
}

fn ensure_git(ctx: &WorkspaceContext, presentation: PathPresentation) -> ToolResult<()> {
    if inside_git_worktree(ctx.root()) {
        Ok(())
    } else {
        Err(ToolError::msg(format!(
            "'{}' does not appear to be inside a Git working tree.",
            ctx.resolver().display_relative_as(ctx.root(), presentation)
        )))
    }
}

/// Render a failed git invocation for the client under the active
/// presentation contract (v0.2 M4 root non-disclosure).
///
/// Git's own bounded stderr diagnostics are preserved — raw `git` error text
/// is genuinely useful and blindly stripping it would hide real problems —
/// but under [`PathPresentation::RegistryRelative`] any occurrence of the
/// selected workspace's canonical root, or of the enclosing repository's
/// root (when git can discover it), is re-rendered workspace-relative so a
/// raw diagnostic can never disclose an absolute filesystem root. Single
/// mode keeps git's text byte-for-byte, as in v0.1.
///
/// Best-effort on Windows, where git's own path spelling in stderr may use
/// different separators than the canonical root; the primary disclosure
/// guards remain the workspace-relative rendering above and the workspace
/// scoping in [`git_process`].
fn presented_git_error(
    raw: &str,
    ctx: &WorkspaceContext,
    presentation: PathPresentation,
) -> ToolError {
    if presentation == PathPresentation::SingleCompatible {
        return ToolError::msg(raw.to_string());
    }
    let mut text = raw.to_string();
    // The enclosing repository root, when discoverable: parent repositories
    // are exactly what the workspace scoping exists to cross, so their
    // absolute paths must not leak either. One extra hardened probe, on the
    // failure path only; if it fails, the workspace root below is still
    // scrubbed.
    let toplevel = run_git_bounded(&GitInvocation {
        root: ctx.root(),
        args: &["--no-pager", "rev-parse", "--show-toplevel"],
        cap: 4 * 1024,
        ok_exit_codes: &[0],
    })
    .ok()
    .map(|out| out.stdout.trim().trim_end_matches('/').to_string())
    .unwrap_or_default();
    if !toplevel.is_empty() && toplevel != "/" {
        text = replace_path_prefix(&text, &toplevel, ".");
    }
    text = replace_path_prefix(&text, ctx.root().to_string_lossy().as_ref(), ".");
    ToolError::msg(text)
}

/// Replace occurrences of `root` in `text` with `replacement` only where the
/// following byte ends a path component (separator, quote, whitespace, or
/// end of text), so `/ws/root` never rewrites an unrelated `/ws/root-2`.
fn replace_path_prefix(text: &str, root: &str, replacement: &str) -> String {
    if root.is_empty() {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find(root) {
        out.push_str(&rest[..at]);
        rest = &rest[at + root.len()..];
        let ends_component = rest.as_bytes().first().is_none_or(|&b| {
            matches!(b, b'/' | b'\'' | b'"' | b' ' | b'\t' | b':' | b'\n' | b'\r')
        });
        if ends_component {
            out.push_str(replacement);
        } else {
            out.push_str(root);
        }
    }
    out.push_str(rest);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::permissions::Permissions;
    use crate::tools::patch;
    use crate::workspace::Workspace;
    use std::path::Path;
    use std::process::Command as StdCommand;
    use tempfile::TempDir;

    /// Fixture git invocations deliberately avoid mutating process-wide
    /// environment variables (unsafe racy across parallel tests). Where a
    /// test needs to pin behavior the host ~/.gitconfig could otherwise
    /// influence, the fixture pins it as REPO-LOCAL config (which outranks
    /// global config) instead.
    ///
    /// Identity comes from Command-scoped env below; that is per-child state,
    /// not process state, and is therefore race-free.
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

    /// Pin behaviors a hostile/benign host gitconfig could otherwise flip.
    fn pin_local_config(repo_root: &Path) {
        git(
            repo_root,
            &["config", "status.showUntrackedFiles", "normal"],
        );
    }

    fn repo_state() -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git(root, &["init", "--initial-branch=main"]);
        pin_local_config(root);
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

    #[test]
    fn malicious_repo_config_cannot_execute_external_diff() {
        // Configure an external diff driver that would create a marker file
        // if executed; git_status/git_diff must NOT run it.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git(root, &["init", "--initial-branch=main"]);
        pin_local_config(root);
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
    fn oversized_git_output_is_bounded() {
        // A file large enough that its diff exceeds a tiny cap must still be
        // processed without unbounded retention, reporting truncation.
        let tmp = TempDir::new().unwrap();
        let root = tmp.path();
        git(root, &["init", "--initial-branch=main"]);
        pin_local_config(root);
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

    // -- workspace-boundary scope (parent repository) ------------------------

    /// Parent repo layout:
    ///
    /// ```text
    /// repo/
    /// ├── outside.txt      (outside the configured workspace)
    /// └── workspace/
    ///     └── inside.txt   (the workspace root)
    /// ```
    fn parent_repo_state() -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        let repo_root = tmp.path();
        let ws_dir = repo_root.join("workspace");
        std::fs::create_dir(&ws_dir).unwrap();
        git(repo_root, &["init", "--initial-branch=main"]);
        pin_local_config(repo_root);
        std::fs::write(repo_root.join("outside.txt"), "original\n").unwrap();
        std::fs::write(ws_dir.join("inside.txt"), "original\n").unwrap();
        git(repo_root, &["add", "."]);
        git(repo_root, &["commit", "-m", "initial"]);

        let ws = Workspace::open(&ws_dir).unwrap();
        let state = AppState::new(ws, Permissions::default(), Limits::default());
        (tmp, state)
    }

    #[test]
    fn status_excludes_changes_outside_the_workspace() {
        let (_t, state) = parent_repo_state();
        let repo_root = state.workspace().root().join("..");
        std::fs::write(repo_root.join("outside.txt"), "MODIFIED OUTSIDE\n").unwrap();
        std::fs::write(
            state.workspace().root().join("inside.txt"),
            "modified inside\n",
        )
        .unwrap();

        let out = git_status(&state, GitStatusArgs {}).expect("status must succeed");
        let output = out["output"].as_str().unwrap();
        assert!(
            output.contains("inside.txt"),
            "workspace change missing from status: {output}"
        );
        assert!(
            !output.contains("outside.txt"),
            "OUT-OF-WORKSPACE change leaked into status: {output}"
        );
    }

    #[test]
    fn diff_excludes_changes_outside_the_workspace() {
        let (_t, state) = parent_repo_state();
        let repo_root = state.workspace().root().join("..");
        std::fs::write(repo_root.join("outside.txt"), "DIFFED OUTSIDE\n").unwrap();
        std::fs::write(
            state.workspace().root().join("inside.txt"),
            "diffed inside\n",
        )
        .unwrap();

        let out = git_diff(
            &state,
            GitDiffArgs {
                staged: false,
                path: None,
            },
        )
        .expect("diff must succeed");
        let diff = out["diff"].as_str().unwrap();
        assert!(
            diff.contains("+diffed inside"),
            "expected inside change: {diff}"
        );
        assert!(
            !diff.contains("+DIFFED OUTSIDE") && !diff.contains("outside.txt"),
            "out-of-workspace change leaked into diff: {diff}"
        );
    }

    #[test]
    fn staged_diff_is_also_workspace_scoped() {
        let (_t, state) = parent_repo_state();
        let repo_root = state.workspace().root().join("..");
        std::fs::write(repo_root.join("outside.txt"), "STAGED OUTSIDE\n").unwrap();
        std::fs::write(
            state.workspace().root().join("inside.txt"),
            "staged inside\n",
        )
        .unwrap();
        // Stage everything in the whole repository.
        git(state.workspace().root(), &["add", ":/"]);

        let out = git_diff(
            &state,
            GitDiffArgs {
                staged: true,
                path: None,
            },
        )
        .unwrap();
        let diff = out["diff"].as_str().unwrap();
        assert!(
            diff.contains("+staged inside"),
            "expected inside change: {diff}"
        );
        assert!(
            !diff.contains("+STAGED OUTSIDE") && !diff.contains("outside.txt"),
            "out-of-workspace staged change leaked: {diff}"
        );

        // Unstaged view stays empty — the only staged change inside the
        // workspace is what we just created.
        let unstaged = git_diff(
            &state,
            GitDiffArgs {
                staged: false,
                path: None,
            },
        )
        .unwrap();
        assert!(unstaged["diff"].as_str().unwrap().trim().is_empty());
    }

    #[test]
    fn explicit_diff_path_may_not_reference_outside_paths() {
        let (_t, state) = parent_repo_state();
        for attempt in ["../outside.txt", "/etc/passwd"] {
            let err = git_diff(
                &state,
                GitDiffArgs {
                    staged: false,
                    path: Some(attempt.into()),
                },
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("Invalid diff path") || msg.contains("outside the configured"),
                "'{attempt}' should be rejected as a pathspec, got: {msg}"
            );
        }
    }

    // -- workspace-relative path rendering -----------------------------------

    #[test]
    fn status_paths_are_workspace_relative_in_a_nested_repo() {
        let (_t, state) = parent_repo_state();
        let root = state.workspace().root().to_path_buf();
        std::fs::write(root.join("inside.txt"), "modified\n").unwrap();
        std::fs::write(root.join("untracked.txt"), "new\n").unwrap();

        let out = git_status(&state, GitStatusArgs {}).unwrap();
        let output = out["output"].as_str().unwrap();
        assert!(
            output.contains(" M inside.txt") && output.contains("?? untracked.txt"),
            "status paths must be workspace-relative: {output}"
        );
        assert!(
            !output.contains("workspace/"),
            "status leaked the enclosing repo's directory prefix: {output}"
        );
        assert!(
            !output.contains("outside.txt"),
            "out-of-workspace change leaked: {output}"
        );
    }

    #[test]
    fn diff_paths_are_workspace_relative_in_a_nested_repo() {
        let (_t, state) = parent_repo_state();
        let root = state.workspace().root().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/inside.txt"), "original\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "add src file"]);
        std::fs::write(root.join("src/inside.txt"), "modified\n").unwrap();

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
            diff.contains("--- a/src/inside.txt") && diff.contains("+++ b/src/inside.txt"),
            "diff headers must be workspace-relative: {diff}"
        );
        assert!(
            !diff.contains("workspace/src/inside.txt"),
            "diff leaked the enclosing repo's directory prefix: {diff}"
        );

        // The same holds for the staged view.
        git(&root, &["add", "."]);
        let staged = git_diff(
            &state,
            GitDiffArgs {
                staged: true,
                path: None,
            },
        )
        .unwrap();
        let diff = staged["diff"].as_str().unwrap();
        assert!(
            diff.contains("+++ b/src/inside.txt") && !diff.contains("workspace/"),
            "staged diff headers must be workspace-relative: {diff}"
        );
    }

    #[test]
    fn git_diff_output_applies_via_apply_patch_in_a_nested_workspace() {
        // The two core tools must compose: a diff captured from a workspace
        // nested inside a larger repository feeds apply_patch against the
        // same workspace without creating a workspace/workspace/… path.
        let (_t, state) = parent_repo_state();
        let root = state.workspace().root().to_path_buf();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/notes.txt"), "alpha\nbeta\n").unwrap();
        git(&root, &["add", "."]);
        git(&root, &["commit", "-m", "add notes"]);

        // 1. Modify the file and capture the diff.
        std::fs::write(root.join("src/notes.txt"), "alpha\ngamma\n").unwrap();
        let out = git_diff(
            &state,
            GitDiffArgs {
                staged: false,
                path: None,
            },
        )
        .unwrap();
        let diff = out["diff"].as_str().unwrap().to_string();
        assert!(diff.contains("b/src/notes.txt"), "diff missing: {diff}");

        // 2. Restore the original content, so the patch applies against it.
        std::fs::write(root.join("src/notes.txt"), "alpha\nbeta\n").unwrap();

        // 3. Feed the diff into apply_patch (write permission required).
        let writable = AppState::new(
            state.workspace().clone(),
            Permissions::from_flags(true, false, false).unwrap(),
            Limits::default(),
        );
        patch::handle(
            &writable,
            patch::ApplyPatchArgs {
                patch: diff.clone(),
            },
        )
        .expect("git_diff output must apply cleanly via apply_patch");

        // 4. The intended workspace file was modified; no stray
        //    workspace/workspace path exists.
        assert_eq!(
            std::fs::read_to_string(root.join("src/notes.txt")).unwrap(),
            "alpha\ngamma\n"
        );
        assert!(
            !root.join("workspace").exists(),
            "apply_patch created a workspace/workspace path from repo-root-relative headers"
        );
    }

    // -- registry-mode wrappers (v0.2 M4) ------------------------------------

    fn rid(s: &str) -> WorkspaceId {
        WorkspaceId::parse(s).expect("fixture workspace id")
    }

    /// Two-workspace registry where each workspace is its own committed
    /// repository with distinctly named files, so any cross-workspace bleed
    /// is unambiguous.
    fn registry_repos_fixture() -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        let alpha = tmp.path().join("alpha");
        let beta = tmp.path().join("beta");
        for (dir, name) in [(&alpha, "alpha"), (&beta, "beta")] {
            std::fs::create_dir_all(dir).unwrap();
            git(dir, &["init", "--initial-branch=main"]);
            pin_local_config(dir);
            std::fs::write(dir.join(format!("{name}_tracked.txt")), "original\n").unwrap();
            git(dir, &["add", "."]);
            git(dir, &["commit", "-m", "initial"]);
        }
        let config = format!(
            "version = 1\n\n[workspaces.alpha]\nroot = '{}'\n\n[workspaces.beta]\nroot = '{}'\n",
            alpha.display(),
            beta.display()
        );
        let registry = crate::registry::WorkspaceRegistry::from_toml_str(&config).unwrap();
        (tmp, AppState::from_registry(registry))
    }

    /// One parent repository containing both registered workspaces as
    /// subdirectories — the parent-repository isolation fixture.
    fn registry_parent_repo_fixture() -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        let alpha = tmp.path().join("alpha");
        let beta = tmp.path().join("beta");
        std::fs::create_dir_all(&alpha).unwrap();
        std::fs::create_dir_all(&beta).unwrap();
        git(tmp.path(), &["init", "--initial-branch=main"]);
        pin_local_config(tmp.path());
        std::fs::write(alpha.join("alpha.txt"), "original\n").unwrap();
        std::fs::write(beta.join("beta.txt"), "original\n").unwrap();
        git(tmp.path(), &["add", "."]);
        git(tmp.path(), &["commit", "-m", "initial"]);
        let config = format!(
            "version = 1\n\n[workspaces.alpha]\nroot = '{}'\n\n[workspaces.beta]\nroot = '{}'\n",
            alpha.display(),
            beta.display()
        );
        let registry = crate::registry::WorkspaceRegistry::from_toml_str(&config).unwrap();
        (tmp, AppState::from_registry(registry))
    }

    #[test]
    fn registry_status_and_diff_are_scoped_and_carry_provenance() {
        let (tmp, state) = registry_repos_fixture();
        let alpha = tmp.path().join("alpha");
        let beta = tmp.path().join("beta");
        // One unstaged modification + one untracked file in alpha; one
        // staged change in beta.
        std::fs::write(alpha.join("alpha_tracked.txt"), "ALPHA_MARK\n").unwrap();
        std::fs::write(alpha.join("alpha_untracked.txt"), "x\n").unwrap();
        std::fs::write(beta.join("beta_tracked.txt"), "BETA_MARK\n").unwrap();
        git(&beta, &["add", "."]);

        let out = registry_git_status(
            &state,
            RegistryGitStatusArgs {
                workspace: rid("alpha"),
                args: GitStatusArgs {},
            },
        )
        .expect("registry status must succeed");
        assert_eq!(out["workspace"], json!("alpha"));
        let output = out["output"].as_str().unwrap();
        assert!(
            output.contains(" M alpha_tracked.txt") && output.contains("?? alpha_untracked.txt"),
            "alpha changes missing: {output}"
        );
        assert!(
            !output.contains("beta_tracked.txt") && !output.contains("BETA_MARK"),
            "beta change leaked: {output}"
        );

        let out = registry_git_diff(
            &state,
            RegistryGitDiffArgs {
                workspace: rid("alpha"),
                args: GitDiffArgs {
                    staged: false,
                    path: None,
                },
            },
        )
        .unwrap();
        assert_eq!(out["workspace"], json!("alpha"));
        assert_eq!(out["staged"], json!(false));
        let diff = out["diff"].as_str().unwrap();
        assert!(diff.contains("+ALPHA_MARK"), "{diff}");
        assert!(
            !diff.contains("BETA_MARK") && !diff.contains("beta"),
            "{diff}"
        );

        // staged selection matches single-mode semantics for the same context
        let out = registry_git_diff(
            &state,
            RegistryGitDiffArgs {
                workspace: rid("beta"),
                args: GitDiffArgs {
                    staged: true,
                    path: None,
                },
            },
        )
        .unwrap();
        assert_eq!(out["staged"], json!(true));
        assert!(out["diff"].as_str().unwrap().contains("+BETA_MARK"));
        let out = registry_git_diff(
            &state,
            RegistryGitDiffArgs {
                workspace: rid("beta"),
                args: GitDiffArgs {
                    staged: false,
                    path: None,
                },
            },
        )
        .unwrap();
        assert!(out["diff"].as_str().unwrap().trim().is_empty());

        // No absolute roots anywhere in the success responses (the final
        // `out` above is beta's staged diff).
        let out_str = out.to_string();
        for root in [alpha.as_path(), beta.as_path()] {
            let root = root.to_string_lossy();
            assert!(
                !out_str.contains(root.trim_start_matches('/')),
                "root leaked into registry response: {out_str}"
            );
        }
    }

    #[test]
    fn registry_parent_repo_isolation_holds_per_context() {
        let (_tmp, state) = registry_parent_repo_fixture();
        let registry = state.registry();
        let alpha = registry.get(&rid("alpha")).unwrap().root().to_path_buf();
        let beta = registry.get(&rid("beta")).unwrap().root().to_path_buf();
        std::fs::write(alpha.join("alpha.txt"), "original\nALPHA_MARK\n").unwrap();
        std::fs::write(beta.join("beta.txt"), "original\nBETA_MARK\n").unwrap();

        // status(alpha) reports alpha's change only, even though both
        // workspaces resolve into the same parent repository.
        let out = registry_git_status(
            &state,
            RegistryGitStatusArgs {
                workspace: rid("alpha"),
                args: GitStatusArgs {},
            },
        )
        .unwrap();
        let output = out["output"].as_str().unwrap();
        assert!(output.contains(" M alpha.txt"), "{output}");
        assert!(
            !output.contains("beta.txt") && !output.contains("BETA_MARK"),
            "sibling change leaked through the parent repository: {output}"
        );

        let out = registry_git_diff(
            &state,
            RegistryGitDiffArgs {
                workspace: rid("alpha"),
                args: GitDiffArgs {
                    staged: false,
                    path: None,
                },
            },
        )
        .unwrap();
        let diff = out["diff"].as_str().unwrap();
        assert!(
            diff.contains("--- a/alpha.txt") && diff.contains("+++ b/alpha.txt"),
            "diff headers must be workspace-relative: {diff}"
        );
        assert!(diff.contains("+ALPHA_MARK"), "{diff}");
        assert!(
            !diff.contains("beta.txt") && !diff.contains("BETA_MARK"),
            "sibling diff leaked through the parent repository: {diff}"
        );

        // The inverse view for beta.
        let out = registry_git_status(
            &state,
            RegistryGitStatusArgs {
                workspace: rid("beta"),
                args: GitStatusArgs {},
            },
        )
        .unwrap();
        let output = out["output"].as_str().unwrap();
        assert!(output.contains(" M beta.txt"), "{output}");
        assert!(
            !output.contains("alpha.txt") && !output.contains("ALPHA_MARK"),
            "sibling change leaked through the parent repository: {output}"
        );

        let out = registry_git_diff(
            &state,
            RegistryGitDiffArgs {
                workspace: rid("beta"),
                args: GitDiffArgs {
                    staged: false,
                    path: None,
                },
            },
        )
        .unwrap();
        let diff = out["diff"].as_str().unwrap();
        assert!(
            diff.contains("+++ b/beta.txt") && diff.contains("+BETA_MARK"),
            "{diff}"
        );
        assert!(
            !diff.contains("alpha.txt") && !diff.contains("ALPHA_MARK"),
            "sibling diff leaked through the parent repository: {diff}"
        );
    }

    #[test]
    fn registry_diff_path_is_validated_against_the_selected_workspace() {
        let (tmp, state) = registry_repos_fixture();
        let alpha = tmp.path().join("alpha");
        std::fs::create_dir_all(alpha.join("src")).unwrap();
        std::fs::write(alpha.join("src/a.rs"), "original\n").unwrap();
        std::fs::write(alpha.join("src/b.rs"), "original\n").unwrap();
        git(&alpha, &["add", "."]);
        git(&alpha, &["commit", "-m", "add src"]);
        std::fs::write(alpha.join("src/a.rs"), "MARK_A\n").unwrap();
        std::fs::write(alpha.join("src/b.rs"), "MARK_B\n").unwrap();

        // A valid workspace-relative path filters the diff.
        let out = registry_git_diff(
            &state,
            RegistryGitDiffArgs {
                workspace: rid("alpha"),
                args: GitDiffArgs {
                    staged: false,
                    path: Some("src/a.rs".into()),
                },
            },
        )
        .unwrap();
        assert_eq!(out["path"], json!("src/a.rs"));
        let diff = out["diff"].as_str().unwrap();
        assert!(diff.contains("+MARK_A"), "{diff}");
        assert!(!diff.contains("MARK_B") && !diff.contains("b.rs"), "{diff}");

        // A sibling-workspace pathspec is rejected by the workspace resolver
        // before any git process runs.
        let err = registry_git_diff(
            &state,
            RegistryGitDiffArgs {
                workspace: rid("alpha"),
                args: GitDiffArgs {
                    staged: false,
                    path: Some("../beta/beta_tracked.txt".into()),
                },
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Invalid diff path") && msg.contains("outside the configured workspace"),
            "'{msg}"
        );
        assert!(
            !msg.contains(tmp.path().to_string_lossy().as_ref()),
            "path rejection must not expose roots: {msg}"
        );
    }

    #[test]
    fn registry_non_repository_error_presents_dot_not_the_root() {
        let tmp = TempDir::new().unwrap();
        let plain = tmp.path().join("plain");
        std::fs::create_dir_all(&plain).unwrap();
        let config = format!(
            "version = 1\n\n[workspaces.plain]\nroot = '{}'\n",
            plain.display()
        );
        let registry = crate::registry::WorkspaceRegistry::from_toml_str(&config).unwrap();
        let state = AppState::from_registry(registry);

        for err in [
            registry_git_status(
                &state,
                RegistryGitStatusArgs {
                    workspace: rid("plain"),
                    args: GitStatusArgs {},
                },
            )
            .unwrap_err(),
            registry_git_diff(
                &state,
                RegistryGitDiffArgs {
                    workspace: rid("plain"),
                    args: GitDiffArgs {
                        staged: false,
                        path: None,
                    },
                },
            )
            .unwrap_err(),
        ] {
            let msg = err.to_string();
            assert!(
                msg.contains("does not appear to be inside a Git working tree"),
                "{msg}"
            );
            assert!(
                msg.contains("'.'"),
                "registry presentation must render the root as '.': {msg}"
            );
            assert!(
                !msg.contains(plain.to_string_lossy().as_ref()),
                "registry error must not expose the canonical root: {msg}"
            );
        }
    }

    #[test]
    fn single_mode_non_repository_error_keeps_the_v01_absolute_root() {
        let tmp = TempDir::new().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        let state = AppState::new(ws, Permissions::default(), Limits::default());
        let msg = git_status(&state, GitStatusArgs {})
            .unwrap_err()
            .to_string();
        let canonical = tmp.path().canonicalize().unwrap();
        assert!(
            msg.contains(canonical.to_string_lossy().as_ref()),
            "v0.1 presentation keeps the canonical absolute root in the error: {msg}"
        );
    }

    #[test]
    fn registry_unknown_and_malformed_ids_reach_no_git_process() {
        let (tmp, state) = registry_repos_fixture();

        // Valid grammar, unknown id: the bounded M2 error. Routing fails
        // inside resolve_registry_workspace, before any git invocation.
        let err = registry_git_status(
            &state,
            RegistryGitStatusArgs {
                workspace: rid("does-not-exist"),
                args: GitStatusArgs {},
            },
        )
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Unknown workspace 'does-not-exist'")
                && msg.contains("Use list_workspaces to discover valid workspace IDs"),
            "{msg}"
        );
        assert!(
            !msg.contains(tmp.path().to_string_lossy().as_ref()),
            "bounded error must not expose roots: {msg}"
        );

        // Grammar violations cannot even enter the tool layer: the
        // WorkspaceId boundary rejects them at deserialization (verified
        // end-to-end in tests/cli.rs).
        for bad in ["../foo", "/tmp/foo", "Nian-Vision"] {
            assert!(
                WorkspaceId::parse(bad).is_err(),
                "'{bad}' must fail the id grammar"
            );
        }
    }

    #[test]
    fn git_error_presentation_scrubs_roots_at_component_boundaries() {
        let tmp = TempDir::new().unwrap();
        let ws_dir = tmp.path().join("ws");
        std::fs::create_dir_all(&ws_dir).unwrap();
        let ws = Workspace::open(&ws_dir).unwrap();
        let ctx = WorkspaceContext::new(Some(rid("ws")), ws, Permissions::default());
        let root = ctx.root().to_string_lossy().into_owned();

        // Outside a repository the toplevel probe simply fails; the scrubber
        // must tolerate that and still scrub the workspace root.
        let raw = format!("fatal: trouble at '{root}/.git' and {root}-2/kept plus {root} tail");
        let msg = presented_git_error(&raw, &ctx, PathPresentation::RegistryRelative).to_string();
        assert!(msg.contains("'./.git'"), "quoted root scrubbed: {msg}");
        assert!(
            msg.contains(&format!("{root}-2/kept")),
            "a longer sibling path must not be rewritten: {msg}"
        );
        assert!(
            msg.matches(&root).count() == 1,
            "only the sibling occurrence of the root text may remain: {msg}"
        );
        assert!(msg.contains(". tail"), "trailing root scrubbed: {msg}");

        // Single mode keeps git's text byte-for-byte (v0.1 contract).
        let kept = presented_git_error(&raw, &ctx, PathPresentation::SingleCompatible).to_string();
        assert_eq!(kept, raw);
    }

    #[test]
    fn git_error_presentation_scrubs_the_parent_repository_root() {
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("repo");
        let ws_dir = repo.join("ws");
        std::fs::create_dir_all(&ws_dir).unwrap();
        git(&repo, &["init", "--initial-branch=main"]);
        pin_local_config(&repo);
        let ws = Workspace::open(&ws_dir).unwrap();
        let ctx = WorkspaceContext::new(Some(rid("ws")), ws, Permissions::default());

        let repo_str = repo.canonicalize().unwrap().to_string_lossy().into_owned();
        let raw = format!("fatal: broken objects under {repo_str}/objects");
        let msg = presented_git_error(&raw, &ctx, PathPresentation::RegistryRelative).to_string();
        assert!(
            msg.contains("fatal: broken objects under ./objects"),
            "parent repository root must be scrubbed: {msg}"
        );
        assert!(
            !msg.contains(repo_str.trim_start_matches('/')),
            "parent repository root leaked: {msg}"
        );
    }
}
