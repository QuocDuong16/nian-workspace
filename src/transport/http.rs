//! Streamable HTTP transport (spec section 12).
//!
//! Purely a transport layer: binds to the configured loopback host/port,
//! serves the MCP endpoint at `/mcp`, and applies no tunneling, auth
//! platform, or session-persistence logic.
//!
//! v0.1 policy: **loopback bind addresses only**. The server has no built-in
//! authentication, so exposing it beyond 127.0.0.1/::1 would hand filesystem,
//! command, and git access to everyone on the network; remote access belongs
//! behind an external secure tunnel instead. Non-loopback binds are rejected
//! with a clear error rather than merely warned about.

use crate::config::{AppState, RuntimeMode};
use crate::server::{NianWorkspaceServer, RegistryServer};
use anyhow::{bail, Context};
use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};
use rmcp::ServerHandler;
use std::net::IpAddr;

/// Resolve the CLI `--host` into a bind IP, enforcing loopback-only.
///
/// Accepts any loopback IP literal (127.x.y.z, ::1) and the name
/// `localhost` (resolved through the system resolver; every resolved address
/// must be loopback). Anything else is rejected up front.
pub(crate) fn resolve_loopback_bind(host: &str) -> anyhow::Result<IpAddr> {
    if host == "localhost" {
        let addrs: Vec<IpAddr> = std::net::ToSocketAddrs::to_socket_addrs(&(host, 0))
            .with_context(|| format!("failed to resolve '{host}'"))?
            .map(|a| a.ip())
            .collect();
        if addrs.is_empty() {
            bail!("'localhost' did not resolve to any address.");
        }
        if let Some(non_loopback) = addrs.iter().find(|ip| !ip.is_loopback()) {
            bail!(
                "'{host}' resolves to {non_loopback}, which is not a loopback address. \
                 nian-workspace only binds loopback (127.0.0.1 / ::1); \
                 use an external secure tunnel for remote access."
            );
        }
        return Ok(address_preference(&addrs));
    }

    let ip: IpAddr = host
        .parse()
        .with_context(|| format!("invalid bind host '{host}'"))?;
    if !ip.is_loopback() {
        bail!(
            "Refusing to bind {ip}: nian-workspace only binds loopback addresses \
             (127.0.0.1 / ::1). It has no authentication, and remote clients would \
             gain full tool access to your filesystem. Use an external secure tunnel \
             for remote access."
        );
    }
    Ok(ip)
}

fn address_preference(addrs: &[IpAddr]) -> IpAddr {
    // Prefer IPv4 loopback for maximal client compatibility.
    addrs
        .iter()
        .copied()
        .find(|ip| ip.is_ipv4())
        .unwrap_or(addrs[0])
}

pub async fn serve(state: AppState, host: &str, port: u16) -> anyhow::Result<()> {
    let addr_ip = resolve_loopback_bind(host)?;
    let endpoint = format!("http://{addr_ip}:{port}/mcp");

    match state.mode() {
        RuntimeMode::SingleWorkspace(_) => {
            let ws = state.single_workspace();
            tracing::info!(
                workspace = %ws.root().display(),
                write = ws.permissions().write,
                exec = ws.permissions().exec,
                shell = ws.permissions().shell,
                endpoint = %endpoint,
                "starting nian-workspace over streamable HTTP"
            );
            let state_for_factory = state.clone();
            run_http(
                move || Ok(NianWorkspaceServer::new(state_for_factory.clone())),
                addr_ip,
                port,
            )
            .await
        }
        RuntimeMode::WorkspaceRegistry(_) => {
            tracing::info!(
                workspaces = state.registry().iter_sorted().len(),
                endpoint = %endpoint,
                "starting nian-workspace over streamable HTTP (registry mode)"
            );
            let state_for_factory = state.clone();
            run_http(
                move || Ok(RegistryServer::new(state_for_factory.clone())),
                addr_ip,
                port,
            )
            .await
        }
    }
}

async fn run_http<S, F>(service_factory: F, addr_ip: IpAddr, port: u16) -> anyhow::Result<()>
where
    S: ServerHandler + Send + 'static,
    F: Fn() -> Result<S, std::io::Error> + Send + Sync + 'static,
{
    let ct = tokio_util::sync::CancellationToken::new();
    let service = StreamableHttpService::new(
        service_factory,
        LocalSessionManager::default().into(),
        StreamableHttpServerConfig::default().with_cancellation_token(ct.child_token()),
    );

    let router = axum::Router::new().nest_service("/mcp", service);
    let listener = tokio::net::TcpListener::bind((addr_ip, port))
        .await
        .with_context(|| format!("failed to bind http endpoint on {addr_ip}:{port}"))?;

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_loopback_literals() {
        assert!(resolve_loopback_bind("127.0.0.1").unwrap().is_loopback());
        assert_eq!(
            resolve_loopback_bind("127.5.4.3").unwrap(),
            "127.5.4.3".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            resolve_loopback_bind("::1").unwrap(),
            "::1".parse::<IpAddr>().unwrap()
        );
    }

    #[test]
    fn localhost_resolves_to_loopback() {
        let ip = resolve_loopback_bind("localhost").expect("hosts file maps localhost");
        assert!(
            ip.is_loopback(),
            "localhost must resolve loopback, got {ip}"
        );
    }

    #[test]
    fn rejects_non_loopback_and_wildcard_binds() {
        for bad in ["0.0.0.0", "192.168.1.10", "8.8.8.8", "::"] {
            let err = resolve_loopback_bind(bad).unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("loopback"),
                "rejection for '{bad}' should mention loopback: {msg}"
            );
        }
    }

    #[test]
    fn rejects_garbage_hosts_with_parse_error() {
        assert!(resolve_loopback_bind("not-a-host").is_err());
        assert!(resolve_loopback_bind("").is_err());
    }
}
