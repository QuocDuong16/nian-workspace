//! `nian-workspace` — a minimal, secure local MCP workspace server.
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

    // v0.2 M1 transitional limitation, made explicit rather than hidden:
    // registry mode builds and fully validates the workspace registry, but
    // MCP tool serving in registry mode is not implemented yet. Serving the
    // existing single-workspace tool API against a registry would require a
    // default workspace — exactly the ambiguous routing M1 must avoid — so
    // the process stops here instead. Workspace-id request routing arrives
    // in a later milestone; single-workspace mode is unchanged.
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
        let ids: Vec<String> = reg
            .iter_sorted()
            .iter()
            .map(|ctx| {
                ctx.id()
                    .expect("registry workspaces always have an id")
                    .to_string()
            })
            .collect();
        anyhow::bail!(
            "Workspace registry loaded and validated {} workspace(s) [{}], but registry mode \
             is not yet available for MCP tool serving in v0.2 M1: no workspace selector, \
             default workspace, or workspace-id routing exists yet. \
             For MCP serving, use single-workspace mode (positional WORKSPACE with optional \
             --write/--exec/--allow-shell).",
            reg.iter_sorted().len(),
            ids.join(", ")
        );
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
