//! Registry-mode MCP server (v0.2 M3): workspace discovery plus read-only
//! filesystem access.
//!
//! Constructed **only** in [`RuntimeMode::WorkspaceRegistry`], so this tool
//! surface is the complete, authoritative registry-mode surface. Every
//! filesystem tool requires an explicit logical `WorkspaceId`, resolved by
//! exact lookup into the immutable registry and served by the same
//! hardened, context-based implementations the single-workspace server uses
//! — no duplicated path logic, no default workspace, no mutable selection
//! state, so concurrent calls for different workspaces are independent.
//!
//! Unmigrated tools (patches, commands, git status/diff) are not registered
//! here: `tools/list` shows only the available tools, and direct invocation
//! of anything else is rejected by the router as `tool not found` before
//! any workspace is touched.

use crate::config::AppState;
use crate::tools::{discovery, error_result, files, result_from_value, search};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

/// MCP server for registry mode (v0.2 M3): discovery + read-only file access.
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
}

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for RegistryServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                crate::config::SERVER_NAME.to_string(),
                crate::config::SERVER_VERSION.to_string(),
            ))
            .with_instructions(
                "Workspace discovery plus read-only file access (registry mode): \
                 list_workspaces reports the operator-configured workspace IDs, and \
                 workspace_info, list_files, read_file, and search each require a \
                 `workspace` argument selecting one of those IDs. Paths are \
                 workspace-relative and responses carry the selected workspace's logical \
                 ID as provenance. Patching, command execution, and git status/diff are \
                 not available in registry mode yet; there is no default workspace.",
            )
    }
}

impl AsRef<AppState> for RegistryServer {
    fn as_ref(&self) -> &AppState {
        &self.state
    }
}
