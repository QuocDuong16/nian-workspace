//! Streamable HTTP transport (spec section 12).
//!
//! Purely a transport layer: binds to the configured host/port, serves the
//! MCP endpoint at `/mcp`, and applies no tunneling, auth platform, or
//! session-persistence logic. Binding is loopback unless explicitly requested.

use crate::config::AppState;
use crate::server::NianWorkspaceServer;
use anyhow::Context;
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use std::net::IpAddr;

pub async fn serve(state: AppState, host: &str, port: u16) -> anyhow::Result<()> {
    let addr_ip: IpAddr = host
        .parse()
        .with_context(|| format!("invalid bind host '{host}'"))?;
    if !addr_ip.is_loopback() {
        tracing::warn!(
            "HTTP transport is binding to a NON-loopback address ({host}). \
             Anyone who can reach this port can exercise the enabled tools against your filesystem."
        );
    }

    tracing::info!(
        workspace = %state.workspace().root().display(),
        write = state.permissions().write,
        exec = state.permissions().exec,
        shell = state.permissions().shell,
        endpoint = %format!("http://{host}:{port}/mcp"),
        "starting nian-workspace over streamable HTTP"
    );

    let ct = tokio_util::sync::CancellationToken::new();
    let service = StreamableHttpService::new(
        move || Ok(NianWorkspaceServer::new(state.clone())),
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind((addr_ip, port))
        .await
        .with_context(|| format!("failed to bind http endpoint on {host}:{port}"))?;

    let shutdown_token = ct.clone();
    axum::serve(listener, router)
        .with_graceful_shutdown(async move {
            shutdown_signal().await;
            shutdown_token.cancel();
        })
        .await
        .context("http server error")?;

    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {},
        _ = terminate => {},
    }
}
