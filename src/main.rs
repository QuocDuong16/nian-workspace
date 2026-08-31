//! `nian-workspace` — a secure local workspace bridge for web-hosted AI clients using MCP.
//!
//! stdout carries MCP protocol frames only (stdio mode); all logs go to
//! stderr.

mod cli;
mod config;
mod error;
mod permissions;
mod process;
mod registry;
mod server;
mod tools;
mod transport;
mod workspace;
mod workspace_id;

use anyhow::Context;
use clap::Parser;
use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt, EnvFilter};

fn main() -> anyhow::Result<()> {
    let cli = cli::Cli::parse();

    init_logging(cli.log_level.as_deref())?;

    let state = config::AppState::from_cli(&cli)?;

    // Registry mode: log the operator-configured workspaces once at startup
    // (stderr only — stdout carries MCP protocol frames). Roots appear only
    // in these operator-facing logs, never in MCP responses.
    if let config::RuntimeMode::WorkspaceRegistry(reg) = state.mode() {
        for ctx in reg.iter_sorted() {
            let id = ctx.id().expect("registry workspaces always have an id");
            tracing::info!(
                workspace_id = id.as_str(),
                root = %ctx.root().display(),
                write = ctx.permissions().write,
                exec = ctx.permissions().exec,
                shell = ctx.permissions().shell,
                "workspace registered"
            );
        }
    }

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .context("failed to start async runtime")?;

    let result = runtime.block_on(async move {
        match cli.transport {
            cli::TransportChoice::Stdio => transport::stdio::serve(state).await,
            cli::TransportChoice::Http => transport::http::serve(state, &cli.host, cli.port).await,
        }
    });
    runtime.shutdown_timeout(std::time::Duration::from_secs(2));
    result
}

/// Install the tracing subscriber writing to stderr, never to protocol stdout.
fn init_logging(explicit_level: Option<&str>) -> anyhow::Result<()> {
    let filter = match explicit_level {
        Some(level) => {
            EnvFilter::try_new(level).with_context(|| format!("invalid --log-level '{level}'"))?
        }
        None => EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
    };
    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_writer(std::io::stderr)
        .with_ansi(false);
    tracing_subscriber::registry()
        .with(filter)
        .with(stderr_layer)
        .init();
    Ok(())
}
