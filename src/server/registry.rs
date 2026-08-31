//! Registry-mode MCP server (v0.2 M2): workspace discovery only.
//!
//! Constructed **only** in [`RuntimeMode::WorkspaceRegistry`], so this tool
//! surface is the complete, authoritative registry-mode surface. The
//! unmigrated single-workspace tools (files, search, patches, commands, git
//! status/diff) are not registered here: `tools/list` shows only the two
//! discovery tools, and direct invocation of anything else is rejected by
//! the router as `tool not found` before any workspace is touched.
//!
//! Every request carries its own explicit logical `WorkspaceId` resolved by
//! exact lookup into the immutable registry — no mutable "current
//! workspace", no default, no shared selection state, so concurrent calls
//! for different workspaces are independent.

use crate::config::AppState;
use crate::tools::{discovery, error_result, result_from_value};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

/// MCP server for registry mode (v0.2 M2): discovery only.
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
                "Workspace discovery only (registry mode): list_workspaces reports the \
                 operator-configured workspace IDs and workspace_info inspects one of them \
                 by its required `workspace` argument. File, search, patch, command, and \
                 git status/diff tools are not available in this mode yet; there is no \
                 default workspace.",
            )
    }
}

impl AsRef<AppState> for RegistryServer {
    fn as_ref(&self) -> &AppState {
        &self.state
    }
}
