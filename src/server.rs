//! Mode-aware MCP server boundary (v0.2 M2/M3).
//!
//! There are two separate MCP servers, each with its own tool router, and
//! each transport selects exactly one from [`RuntimeMode`] at startup:
//!
//! - [`NianWorkspaceServer`] (single-workspace mode): the complete,
//!   unchanged v0.1 tool surface, rooted at the one configured workspace.
//! - [`RegistryServer`] (registry mode): the full v0.1 tool capability set —
//!   `list_workspaces` plus workspace-selecting `workspace_info`,
//!   `list_files`, `read_file`, `search`, `git_status`, `git_diff`,
//!   `apply_patch`, and `run_command`, mutation and execution gated by the
//!   selected workspace's own configured capabilities.
//!
//! This is genuine mode-specific routing, not cosmetic hiding: a tool that
//! is not available in a mode is not registered on that mode's router at
//! all, so a direct `tools/call` for it fails inside the rmcp router with a
//! clean `invalid_params("tool not found")` protocol error — before any
//! tool handler runs, without touching a workspace, without panicking, and
//! without any default-workspace fallback. The server stays fully usable
//! afterwards.
//!
//! Registry tools share the single-mode implementations through
//! context-based cores: mode selection happens at this server boundary,
//! workspace selection (and per-workspace permission enforcement) inside the
//! registry wrappers, and tool behavior in the shared `*_for_context`
//! functions over the selected [`WorkspaceContext`] (v0.2 M3–M5).

mod registry;
mod single;

pub use registry::RegistryServer;
pub use single::NianWorkspaceServer;

/// Runtime-host guidance appended to both servers' MCP initialize
/// instructions (`ServerInfo.instructions`): the host OS/architecture from
/// Rust's compile/runtime constants, the exact shell `shell = true` invokes,
/// and the PATH-based resolution of direct execution. Purely presentational
/// and side-effect free: no shell, distro, hostname, PATH, or installed-
/// program probing is performed, and no platform command recommendations are
/// made — the MCP client decides which commands to use.
pub(crate) fn runtime_environment_instructions() -> String {
    let host = format!(
        "Runtime host: {}/{}.",
        std::env::consts::OS,
        std::env::consts::ARCH
    );
    // The shell wording must mirror the actual platform branches of
    // `shell_command` in tools/command.rs: cmd.exe /C on Windows, /bin/sh -c
    // everywhere else this implementation supports.
    #[cfg(windows)]
    let shell = "shell=true uses cmd.exe /C. Direct run_command execution resolves programs through PATH without a shell. PowerShell is not implied by shell=true; invoke pwsh or powershell.exe directly only when available.";
    #[cfg(not(windows))]
    let shell = "shell=true uses /bin/sh -c. Direct run_command execution resolves programs through PATH without a shell.";
    format!("{host} {shell}")
}

#[cfg(test)]
mod runtime_environment_instructions_tests {
    use super::runtime_environment_instructions;

    #[test]
    fn names_the_current_os_and_architecture() {
        let text = runtime_environment_instructions();
        assert!(
            text.contains(&format!(
                "Runtime host: {}/{}.",
                std::env::consts::OS,
                std::env::consts::ARCH
            )),
            "instructions must name the compile-target host: {text}"
        );
    }

    #[test]
    fn states_the_direct_execution_path_semantics() {
        let text = runtime_environment_instructions();
        assert!(
            text.contains(
                "Direct run_command execution resolves programs through PATH without a shell."
            ),
            "{text}"
        );
    }

    #[cfg(windows)]
    #[test]
    fn describes_the_windows_shell_truthfully() {
        let text = runtime_environment_instructions();
        assert!(text.contains("cmd.exe"), "{text}");
        assert!(text.contains("/C"), "{text}");
        // PowerShell is an available-program caveat, never the shell=true claim.
        assert!(
            text.contains("PowerShell is not implied by shell=true"),
            "{text}"
        );
        assert!(
            !text.to_lowercase().contains("shell=true uses powershell"),
            "shell=true must not be claimed to use PowerShell: {text}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn describes_the_unix_shell_truthfully() {
        let text = runtime_environment_instructions();
        assert!(text.contains("/bin/sh"), "{text}");
        assert!(text.contains("-c"), "{text}");
        assert!(!text.contains("cmd.exe"), "{text}");
    }
}
