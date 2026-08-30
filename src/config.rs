use crate::cli::Cli;
use crate::permissions::Permissions;
use crate::registry::WorkspaceRegistry;
use crate::workspace::{Workspace, WorkspaceContext};
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
    /// Hard internal ceiling on one source line's size while reading a file
    /// (before any output clipping). Guards against multi-gigabyte
    /// single-line inputs driving unbounded Vec growth in read_file.
    pub max_source_line_bytes: usize,
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
            max_source_line_bytes: 1024 * 1024,
            max_command_stdout: 256 * 1024,
            max_command_stderr: 256 * 1024,
            default_command_timeout_secs: 120,
            max_command_timeout_secs: 3_600,
            max_git_output: 256 * 1024,
        }
    }
}

/// The runtime workspace model, decided entirely at startup (v0.2 M1).
///
/// There is deliberately no mutable "current workspace": MCP requests cannot
/// switch roots, and no default workspace exists in registry mode. A later
/// milestone will dispatch tool calls through either mode explicitly; M1
/// leaves this representation in place so that dispatch needs no redesign.
#[derive(Debug, Clone)]
pub enum RuntimeMode {
    /// v0.1 behavior: one fixed canonical workspace root plus CLI permissions.
    SingleWorkspace(Arc<WorkspaceContext>),
    /// v0.2: an immutable registry of validated workspace contexts. Built
    /// completely at startup; roots, ids, and permissions never change while
    /// the process runs.
    WorkspaceRegistry(Arc<WorkspaceRegistry>),
}

/// Central shared runtime state used by all MCP tools (spec section 15).
#[derive(Clone)]
pub struct AppState {
    mode: RuntimeMode,
    limits: Limits,
}

impl AppState {
    /// Single-workspace state, exactly as in v0.1: an opened root plus
    /// CLI-derived permissions.
    pub fn new(workspace: Workspace, permissions: Permissions, limits: Limits) -> Self {
        Self {
            mode: RuntimeMode::SingleWorkspace(Arc::new(WorkspaceContext::new(
                None,
                workspace,
                permissions,
            ))),
            limits,
        }
    }

    pub fn mode(&self) -> &RuntimeMode {
        &self.mode
    }

    pub fn limits(&self) -> &Limits {
        &self.limits
    }

    /// The single workspace context. Only meaningful in
    /// [`RuntimeMode::SingleWorkspace`] — registry mode stops before MCP
    /// serving in M1, so tools can never observe a registry-mode state.
    pub fn single_workspace(&self) -> &WorkspaceContext {
        match &self.mode {
            RuntimeMode::SingleWorkspace(ctx) => ctx,
            RuntimeMode::WorkspaceRegistry(_) => {
                unreachable!("single-workspace accessors are unavailable in registry mode")
            }
        }
    }

    /// The root-bound path resolver of the single workspace (v0.1 accessor).
    pub fn workspace(&self) -> &Workspace {
        self.single_workspace().resolver()
    }

    /// The permissions of the single workspace (v0.1 accessor).
    pub fn permissions(&self) -> &Permissions {
        self.single_workspace().permissions()
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
        cli.validate()?;

        if let Some(config_path) = cli.workspace_config.as_deref() {
            // Registry mode: permissions come from the per-workspace
            // configuration, never from the CLI permission flags.
            let registry = WorkspaceRegistry::from_file(config_path)?;
            return Ok(Self {
                mode: RuntimeMode::WorkspaceRegistry(Arc::new(registry)),
                limits: Limits::default(),
            });
        }

        let permissions = Permissions::from_flags(cli.write, cli.exec, cli.allow_shell)?;
        let root = resolve_workspace_root(cli.workspace.as_deref())?;
        let workspace = Workspace::open(&root)?;
        Ok(Self::new(workspace, permissions, Limits::default()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::workspace_id::WorkspaceId;
    use tempfile::TempDir;

    fn temp_workspace() -> (TempDir, Workspace) {
        let tmp = TempDir::new().expect("tempdir");
        let ws = Workspace::open(tmp.path()).expect("open workspace");
        (tmp, ws)
    }

    #[test]
    fn new_wraps_single_workspace_context_without_id() {
        let (_tmp, ws) = temp_workspace();
        let state = AppState::new(ws, Permissions::default(), Limits::default());
        assert!(matches!(state.mode(), RuntimeMode::SingleWorkspace(_)));
        assert!(state.single_workspace().id().is_none());
        assert!(!state.permissions().write);
    }

    #[test]
    fn registry_mode_state_carries_immutable_registry() {
        let tmp = TempDir::new().expect("tempdir");
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let config = format!(
            "version = 1\n\n[workspaces.demo]\nroot = '{}'\n",
            ws.display()
        );
        let registry = WorkspaceRegistry::from_toml_str(&config).expect("valid config");
        let state = AppState {
            mode: RuntimeMode::WorkspaceRegistry(Arc::new(registry)),
            limits: Limits::default(),
        };
        match state.mode() {
            RuntimeMode::WorkspaceRegistry(reg) => {
                let ctx = reg
                    .get(&WorkspaceId::parse("demo").unwrap())
                    .expect("registered");
                assert_eq!(ctx.id().unwrap().as_str(), "demo");
                assert!(!ctx.permissions().write);
            }
            _ => panic!("expected registry mode"),
        }
    }
}
