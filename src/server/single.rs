//! Single-workspace MCP server: the complete v0.1 tool surface, unchanged.
//!
//! Constructed **only** in [`RuntimeMode::SingleWorkspace`]: every tool is
//! rooted at the one fixed workspace with CLI-derived permissions, and no
//! tool takes a workspace selector (v0.1 client compatibility). Registry
//! mode uses the separate [`super::RegistryServer`] instead.
//!
//! Every tool returns either a successful [`CallToolResult`] carrying JSON
//! (both structured content and a pretty-printed text fallback) or a
//! tool-level error result whose message is visible to the AI client.
//! Protocol-level errors (`Err(ErrorData)`) are reserved for infrastructure
//! failures, so actionable messages are never swallowed by the client.

use crate::config::AppState;
use crate::tools::{
    command, error_result, files, git, patch, result_from_value, search, workspace_info,
};
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Implementation, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ServerHandler};

#[derive(Clone)]
pub struct NianWorkspaceServer {
    state: AppState,
    tool_router: rmcp::handler::server::router::tool::ToolRouter<Self>,
}

#[tool_router]
impl NianWorkspaceServer {
    pub fn new(state: AppState) -> Self {
        Self {
            state,
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Return workspace metadata: root path, name, enabled permissions, and Git repository status."
    )]
    fn workspace_info(&self) -> Result<CallToolResult, rmcp::ErrorData> {
        match workspace_info::handle(&self.state) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "List files and directories under a workspace path with bounded depth. Hidden entries and generated directories (node_modules, target, .git, ...) are skipped unless include_hidden is set."
    )]
    fn list_files(
        &self,
        Parameters(args): Parameters<files::ListFilesArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match files::list_files(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Read a UTF-8 text file with 1-based line numbers and line-range support. Binary files are rejected. Output is bounded; read more with start_line/end_line."
    )]
    fn read_file(
        &self,
        Parameters(args): Parameters<files::ReadFileArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match files::read_file(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Fast regex/literal text search over workspace files: every match reports its workspace-relative path and line number. Results are bounded. Hidden and generated directories are searched only when the requested path itself enters them (e.g. '.config', 'node_modules'); .git/.hg/.svn are never searched, not even through symlinks."
    )]
    fn search(
        &self,
        Parameters(args): Parameters<search::SearchArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match search::handle(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Modify workspace files by applying a unified diff ('diff -u'/'git diff' format). Requires --write. All hunks must apply cleanly or nothing is written; new-file creation via /dev/null headers is supported."
    )]
    fn apply_patch(
        &self,
        Parameters(args): Parameters<patch::ApplyPatchArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match patch::handle(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Execute a program directly (no shell) inside the workspace: {program, args, cwd, timeout_seconds}. Requires --exec. Use shell=true plus 'command' for shell syntax — that additionally requires --allow-shell."
    )]
    async fn run_command(
        &self,
        Parameters(args): Parameters<command::RunCommandArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        // Blocking process execution runs on the blocking-friendly runtime
        // implicitly; capture send-safe state up front.
        let state = self.state.clone();
        let outcome = command::handle(&state, args).await;
        match outcome {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Show working-tree status ('git status --short --branch' equivalent) for the workspace repository."
    )]
    fn git_status(
        &self,
        Parameters(_args): Parameters<git::GitStatusArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match git::git_status(&self.state, _args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }

    #[tool(
        description = "Show the unified diff of unstaged changes (or staged ones with staged=true), optionally limited to one path. Diff paths are relative to the workspace root, so output can be fed into apply_patch directly. Output is bounded."
    )]
    fn git_diff(
        &self,
        Parameters(args): Parameters<git::GitDiffArgs>,
    ) -> Result<CallToolResult, rmcp::ErrorData> {
        match git::git_diff(&self.state, args) {
            Ok(value) => result_from_value(value),
            Err(err) => Ok(error_result(err)),
        }
    }
}

/// Permission gating for disabled capabilities happens inside each tool
/// handler so clients still see every tool listed, but receive explicit
/// guidance in the error message when they call one that is unavailable.
#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for NianWorkspaceServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
            .with_server_info(Implementation::new(
                crate::config::SERVER_NAME.to_string(),
                crate::config::SERVER_VERSION.to_string(),
            ))
            .with_instructions(format!(
                "Workspace-scoped file/search/exec/git tools rooted at '{}'. \
                 Writes require --write; commands require --exec; shell syntax requires --allow-shell. {}",
                self.state.workspace().root().display(),
                super::runtime_environment_instructions()
            ))
    }
}

impl AsRef<AppState> for NianWorkspaceServer {
    fn as_ref(&self) -> &AppState {
        &self.state
    }
}
