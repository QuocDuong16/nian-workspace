//! `workspace_info` — workspace metadata (spec section 7.1; registry variant
//! added in v0.2 M2).
//!
//! The core logic lives in [`describe`], which operates on a single
//! [`WorkspaceContext`] and is shared by both server modes; only what the
//! response exposes about the workspace's identity differs per mode
//! ([`WorkspaceIdentity`]).

use crate::config::AppState;
use crate::error::ToolResult;
use crate::permissions::Permissions;
use crate::tools::git_process::{inside_git_worktree, run_git_bounded, GitInvocation};
use crate::workspace::WorkspaceContext;
use crate::workspace_id::WorkspaceId;
use serde_json::{json, Value};

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

/// What the response exposes about a workspace's identity (per server mode).
pub(crate) enum WorkspaceIdentity<'a> {
    /// v0.1 single-workspace schema: canonical root path and directory name.
    /// Preserved unchanged for v0.1 client compatibility.
    SingleRoot,
    /// v0.2 M2 registry schema: the logical id only. Filesystem paths are
    /// never exposed to registry-mode MCP clients.
    Logical(&'a WorkspaceId),
}

/// The effective per-workspace capability set, shared by `workspace_info` and
/// `list_workspaces` so both report permissions identically.
pub(crate) fn permissions_json(perms: &Permissions) -> Value {
    json!({
        "read": perms.read,
        "write": perms.write,
        "exec": perms.exec,
        "shell": perms.shell,
    })
}

/// Metadata for one workspace context, keyed by the requested identity form.
pub(crate) fn describe(
    ctx: &WorkspaceContext,
    identity: WorkspaceIdentity<'_>,
) -> ToolResult<Value> {
    let git = git_probe(ctx.root());
    let git = json!({
        "is_repository": git.as_ref().is_some_and(|(r, _)| *r),
        "branch": git.as_ref().and_then(|(_, b)| b.clone()),
    });

    let mut body = match identity {
        WorkspaceIdentity::SingleRoot => json!({
            "root": ctx.root().to_string_lossy(),
            "name": ctx.resolver().name(),
        }),
        WorkspaceIdentity::Logical(id) => json!({
            "workspace": id.as_str(),
        }),
    };
    let object = body.as_object_mut().expect("identity object literal");
    object.insert(
        "server_version".to_string(),
        json!(crate::config::SERVER_VERSION),
    );
    object.insert(
        "permissions".to_string(),
        permissions_json(ctx.permissions()),
    );
    object.insert("git".to_string(), git);
    Ok(body)
}

/// Single-workspace mode entry point (v0.1 schema, unchanged).
pub(crate) fn handle(state: &AppState) -> ToolResult<Value> {
    describe(state.single_workspace(), WorkspaceIdentity::SingleRoot)
}
