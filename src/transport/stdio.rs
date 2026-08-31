//! stdio transport — the primary local-client transport.
//!
//! stdout carries only MCP protocol frames; all logging is written to stderr
//! by the subscriber installed in `main`.
//!
//! The transport selects the mode-specific MCP server exactly once at
//! startup (v0.2 M2): single-workspace mode serves the unchanged v0.1 tool
//! surface, registry mode serves discovery only.

use crate::config::{AppState, RuntimeMode};
use crate::server::{NianWorkspaceServer, RegistryServer};
use anyhow::Context;
use rmcp::transport::stdio;
use rmcp::{ServerHandler, ServiceExt};

pub async fn serve(state: AppState) -> anyhow::Result<()> {
    match state.mode() {
        RuntimeMode::SingleWorkspace(_) => {
            let ws = state.single_workspace();
            tracing::info!(
                workspace = %ws.root().display(),
                write = ws.permissions().write,
                exec = ws.permissions().exec,
                shell = ws.permissions().shell,
                "starting nian-workspace over stdio"
            );
            serve_stdio(NianWorkspaceServer::new(state)).await
        }
        RuntimeMode::WorkspaceRegistry(_) => {
            tracing::info!(
                workspaces = state.registry().iter_sorted().len(),
                "starting nian-workspace over stdio (registry mode)"
            );
            serve_stdio(RegistryServer::new(state)).await
        }
    }
}

async fn serve_stdio<S>(server: S) -> anyhow::Result<()>
where
    S: ServerHandler + Clone + Send + Sync + 'static,
{
    let running = server
        .serve(stdio())
        .await
        .context("failed to start MCP stdio server")?;
    running
        .waiting()
        .await
        .context("MCP stdio server terminated with error")?;
    Ok(())
}
