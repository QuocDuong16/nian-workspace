use crate::cli::Cli;
use crate::permissions::Permissions;
use crate::workspace::Workspace;
use anyhow::{bail, Context};
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const SERVER_NAME: &str = "nian-workspace";
pub const SERVER_VERSION: &str = env!("CARGO_PKG_VERSION");

/// Bounded defaults for every potentially large output (spec section 16).
#[derive(Debug, Clone, Copy)]
pub struct Limits {
    /// read_file maximum response in bytes.
    pub max_read_bytes: usize,
    /// Lines returned by read_file when no explicit range is requested.
    pub default_read_lines: u64,
    /// Line span used when only `start_line` is supplied to read_file.
    pub read_span_when_start_only: u64,
    /// Maximum entries returned by list_files.
    pub max_list_entries: usize,
    /// Default number of search results.
    pub default_search_results: usize,
    /// Maximum allowed value of the search `max_results` parameter.
    pub max_search_results_cap: usize,
    /// Maximum bytes for a single matched search line kept in output.
    pub max_search_line_bytes: usize,
    /// run_command stdout cap in bytes.
    pub max_command_stdout: usize,
    /// run_command stderr cap in bytes.
    pub max_command_stderr: usize,
    /// Default command timeout in seconds.
    pub default_command_timeout_secs: u64,
    /// Maximum accepted command timeout in seconds.
    pub max_command_timeout_secs: u64,
    /// git_status / git_diff output cap in bytes.
    pub max_git_output: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_read_bytes: 256 * 1024,
            default_read_lines: 2_000,
            read_span_when_start_only: 1_000,
            max_list_entries: 2_000,
            default_search_results: 100,
            max_search_results_cap: 1_000,
            max_search_line_bytes: 300,
            max_command_stdout: 256 * 1024,
            max_command_stderr: 256 * 1024,
            default_command_timeout_secs: 120,
            max_command_timeout_secs: 3_600,
            max_git_output: 256 * 1024,
        }
    }
}

/// Central shared runtime state used by all MCP tools (spec section 15).
#[derive(Clone)]
pub struct AppState {
    workspace: Arc<Workspace>,
    permissions: Permissions,
    limits: Limits,
}

impl AppState {
    pub fn new(workspace: Workspace, permissions: Permissions, limits: Limits) -> Self {
        Self {
            workspace: Arc::new(workspace),
            permissions,
            limits,
        }
    }

    pub fn workspace(&self) -> &Workspace {
        &self.workspace
    }

    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }
}

fn resolve_workspace_root(cli_workspace: Option<&Path>) -> anyhow::Result<PathBuf> {
    let requested = cli_workspace.unwrap_or_else(|| Path::new("."));
    if !requested.exists() {
        bail!("Workspace root '{}' does not exist.", requested.display());
    }
    let root = std::fs::canonicalize(requested)
        .with_context(|| format!("Failed to resolve workspace root '{}'", requested.display()))?;
    if !root.is_dir() {
        bail!(
            "Workspace root '{}' is not a directory.",
            requested.display()
        );
    }
    Ok(root)
}

impl AppState {
    /// Build shared state from parsed CLI arguments, validating permission
    /// combinations and resolving the workspace root up front.
    pub fn from_cli(cli: &Cli) -> anyhow::Result<Self> {
        let permissions = Permissions::from_flags(cli.write, cli.exec, cli.allow_shell)?;
        let root = resolve_workspace_root(cli.workspace.as_deref())?;
        let workspace = Workspace::open(&root)?;
        Ok(Self::new(workspace, permissions, Limits::default()))
    }
}
