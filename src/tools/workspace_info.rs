//! `workspace_info` — basic workspace metadata (spec section 7.1).

use crate::config::AppState;
use crate::error::ToolResult;
use crate::tools::git_process::{inside_git_worktree, run_git_bounded, GitInvocation};
use serde_json::json;

/// Probe the repository branch without disturbing it, using the same hardened
/// process rules as every other Git invocation in this server.
fn git_probe(root: &std::path::Path) -> Option<(bool, Option<String>)> {
    if !inside_git_worktree(root) {
        return Some((false, None));
    }
    let out = run_git_bounded(&GitInvocation {
        root,
        args: &["--no-pager", "rev-parse", "--abbrev-ref", "HEAD"],
        cap: 4 * 1024,
        ok_exit_codes: &[0],
    })
    .ok()?;
    let branch = out
        .stdout
        .trim()
        .to_string()
        .lines()
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty() && s != "HEAD");
    Some((true, branch))
}

pub(crate) fn handle(state: &AppState) -> ToolResult<serde_json::Value> {
    let ws = state.workspace();
    let perms = state.permissions();

    let git = git_probe(ws.root());

    Ok(json!({
        "root": ws.root().to_string_lossy(),
        "name": ws.name(),
        "server_version": crate::config::SERVER_VERSION,
        "permissions": {
            "read": perms.read,
            "write": perms.write,
            "exec": perms.exec,
            "shell": perms.shell,
        },
        "git": {
            "is_repository": git.as_ref().is_some_and(|(r, _)| *r),
            "branch": git.as_ref().and_then(|(_, b)| b.clone()),
        }
    }))
}
