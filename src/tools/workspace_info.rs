//! `workspace_info` — basic workspace metadata (spec section 7.1).

use crate::config::AppState;
use crate::error::ToolResult;
use serde_json::json;

/// Probe `git` for repository status and branch without disturbing it.
fn git_probe(root: &std::path::Path) -> Option<(bool, Option<String>)> {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .arg("rev-parse")
        .arg("--is-inside-work-tree")
        .arg("--abbrev-ref")
        .arg("HEAD")
        .env("GIT_OPTIONAL_LOCKS", "1")
        .output()
        .ok()?;

    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    let mut lines = text.lines();
    let is_repo = lines.next()? == "true";
    if !is_repo {
        return Some((false, None));
    }
    let branch = lines
        .next()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());
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
