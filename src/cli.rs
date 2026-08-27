use clap::{Parser, ValueEnum};
use std::path::PathBuf;

#[derive(Debug, Clone, Copy, PartialEq, Eq, ValueEnum)]
pub enum TransportChoice {
    Stdio,
    Http,
}

#[derive(Debug, Parser)]
#[command(
    name = "nian-workspace",
    version,
    about = "A minimal, secure local MCP workspace server for AI coding clients.",
    long_about = None
)]
pub struct Cli {
    /// Workspace root directory (defaults to the current directory)
    pub workspace: Option<PathBuf>,

    /// Allow modifying workspace files (apply_patch)
    #[arg(long)]
    pub write: bool,

    /// Allow executing local programs (run_command)
    #[arg(long)]
    pub exec: bool,

    /// Allow commands through a system shell (requires --exec)
    #[arg(long)]
    pub allow_shell: bool,

    /// MCP transport
    #[arg(long, value_enum, default_value_t = TransportChoice::Stdio)]
    pub transport: TransportChoice,

    /// HTTP bind host (loopback by default; binding a public address is explicit opt-in)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// HTTP MCP port
    #[arg(long, default_value_t = 8787)]
    pub port: u16,

    /// Log level (error, warn, info, debug); defaults to RUST_LOG or "info"
    #[arg(long)]
    pub log_level: Option<String>,
}
