//! stdio transport — the primary local-client transport.
//!
//! stdout carries only MCP protocol frames; all logging is written to stderr
//! by the subscriber installed in `main`.

use crate::config::AppState;
use crate::server::NianWorkspaceServer;
use anyhow::Context;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

pub async fn serve(state: AppState) -> anyhow::Result<()> {
    tracing::info!(
        workspace = %state.workspace().root().display(),
        write = state.permissions().write,
        exec = state.permissions().exec,
        shell = state.permissions().shell,
        "starting nian-workspace over stdio"
    );
    let server = NianWorkspaceServer::new(state);
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
