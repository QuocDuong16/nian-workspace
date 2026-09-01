//! Registry-mode MCP server (v0.2 M5): workspace discovery plus the full
//! v0.1 tool capability set — filesystem, Git, patching, and command
//! execution — over operator-configured workspaces.
//!
//! Constructed **only** in [`RuntimeMode::WorkspaceRegistry`], so this tool
//! surface is the complete, authoritative registry-mode surface. Every tool
//! that touches a workspace requires an explicit logical [`WorkspaceId`],
//! resolved by exact lookup into the immutable registry and served by the
//! same hardened, context-based implementations the single-workspace server
//! uses — no duplicated logic, no default workspace, no mutable selection
//! state, so concurrent calls for different workspaces are independent.
//! Mutation and execution are gated by the **selected workspace's own
//! configured capabilities** (`write`, `exec`, `allow_shell`); there is no
//! global permission promotion.
//!
//! Any tool name not registered here is rejected by the router as
//! `tool not found` before any workspace is touched.

use crate::config::AppState;
use crate::tools::{
    command, discovery, error_result, files, git, patch, result_from_value, search,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

/// MCP server for registry mode (v0.2 M5): discovery + per-workspace capability set.
#[derive(Clone)]
pub struct RegistryServer {
    state: AppState,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[tool_router]
impl RegistryServer {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "List the logical workspace IDs configured by the operator at server startup, with each workspace's effective permissions. These IDs are the only valid `workspace` selector values for tools that accept one; they are fixed for the lifetime of the server — there is no runtime registration, workspace switching, aliasing, or path-based selection."
    )]
    fn list_workspaces(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        match discovery::list_workspaces(&self.state) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Return metadata for one operator-configured workspace selected by its logical ID: effective permissions and Git repository status. Use list_workspaces to discover the valid IDs. Unknown IDs and selectors that violate the workspace-id grammar are rejected — no fallback workspace is ever chosen."
    )]
    fn workspace_info(
        &self,
        Parameters(args): Parameters<discovery::WorkspaceInfoArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match discovery::workspace_info(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "List files and directories under a path in one selected workspace (logical ID from list_workspaces) with bounded depth. Paths are workspace-relative. Hidden entries and generated directories (node_modules, target, .git, ...) are skipped unless include_hidden is set."
    )]
    fn list_files(
        &self,
        Parameters(args): Parameters<files::RegistryListFilesArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match files::registry_list_files(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Read a UTF-8 text file from one selected workspace (logical ID from list_workspaces) with 1-based line numbers and line-range support. Paths are workspace-relative. Binary files are rejected. Output is bounded; read more with start_line/end_line."
    )]
    fn read_file(
        &self,
        Parameters(args): Parameters<files::RegistryReadFileArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match files::registry_read_file(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Fast regex/literal text search over the files of one selected workspace (logical ID from list_workspaces): every match reports its workspace-relative path and line number. Results are bounded. Hidden and generated directories are searched only when the requested path itself enters them (e.g. '.config', 'node_modules'); .git/.hg/.svn are never searched, not even through symlinks."
    )]
    fn search(
        &self,
        Parameters(args): Parameters<search::RegistrySearchArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match search::registry_search(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Show working-tree status ('git status --short --branch' equivalent) for one selected workspace (logical ID from list_workspaces), scoped to that workspace even when it sits inside a larger parent repository. Read-only: works regardless of the workspace's write/exec permissions."
    )]
    fn git_status(
        &self,
        Parameters(args): Parameters<git::RegistryGitStatusArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match git::registry_git_status(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Show the unified diff of unstaged changes (or staged ones with staged=true) in one selected workspace (logical ID from list_workspaces), optionally limited to one workspace-relative path. Diff paths are relative to the selected workspace root and never leak sibling workspaces through a parent repository. Output is bounded."
    )]
    fn git_diff(
        &self,
        Parameters(args): Parameters<git::RegistryGitDiffArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match git::registry_git_diff(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Modify files of one selected workspace (logical ID from list_workspaces) by applying a unified diff ('diff -u'/'git diff' format). Requires the selected workspace's write capability (write = true in the registry configuration) — the request is rejected before anything is parsed or changed otherwise. All hunks must apply cleanly or nothing is written; new-file creation via /dev/null headers is supported."
    )]
    fn apply_patch(
        &self,
        Parameters(args): Parameters<patch::RegistryApplyPatchArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match patch::registry_apply_patch(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Execute a program directly (no shell) inside one selected workspace (logical ID from list_workspaces): {program, args, cwd, timeout_seconds}. Requires the selected workspace's exec capability; shell=true additionally requires its shell capability — a denied workspace never spawns anything. cwd is workspace-restricted; the spawned process itself is NOT sandboxed and runs with the full permissions of the OS user."
    )]
    async fn run_command(
        &self,
        Parameters(args): Parameters<command::RegistryRunCommandArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Blocking process execution runs on the blocking-friendly runtime
        // implicitly; capture send-safe state up front.
        let state = self.state.clone();
        match command::registry_run_command(&state, args).await {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }
}

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for RegistryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                crate::config::SERVER_NAME.to_string(),
                crate::config::SERVER_VERSION.to_string(),
            ))
            .with_instructions(format!(
                "Registry mode over operator-configured workspaces: list_workspaces reports \
                 the workspace IDs, and every other tool requires a `workspace` argument \
                 selecting one of those IDs. Read tools (workspace_info, list_files, \
                 read_file, search, git_status, git_diff) are always available; apply_patch \
                 requires the selected workspace's write capability and run_command its \
                 exec capability (shell=true additionally requires the shell capability) \
                 as configured at startup. Paths are workspace-relative, server-generated \
                 metadata never exposes filesystem roots, and responses carry the selected \
                 workspace's logical ID as provenance. There is no default workspace. {}",
                super::runtime_environment_instructions()
            ))
    }
}

impl AsRef<AppState> for RegistryServer {
    fn as_ref(&self) -> &AppState {
        &self.state
    }
}
