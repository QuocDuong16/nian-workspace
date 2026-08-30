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
    about = "A secure local workspace bridge for web-hosted AI clients using MCP.",
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

    /// TOML workspace registry configuration (v0.2); mutually exclusive with
    /// a positional WORKSPACE and with --write/--exec/--allow-shell
    #[arg(long, value_name = "PATH")]
    pub workspace_config: Option<PathBuf>,

    /// MCP transport
    #[arg(long, value_enum, default_value_t = TransportChoice::Stdio)]
    pub transport: TransportChoice,

    /// HTTP bind host (loopback only: 127.0.0.1, ::1, or localhost)
    #[arg(long, default_value = "127.0.0.1")]
    pub host: String,

    /// HTTP MCP port
    #[arg(long, default_value_t = 8787)]
    pub port: u16,

    /// Log level (error, warn, info, debug); defaults to RUST_LOG or "info"
    #[arg(long)]
    pub log_level: Option<String>,
}

impl Cli {
    /// Enforce the v0.2 M1 mode rules beyond what argument parsing checks.
    ///
    /// These rules are explicit and deterministic rather than silently
    /// lenient:
    ///
    /// - a positional workspace root and a workspace registry are mutually
    ///   exclusive — there is never a mode where both are active;
    /// - in registry mode, permissions come from the per-workspace
    ///   configuration, so `--write`/`--exec`/`--allow-shell` are rejected
    ///   instead of being promoted onto every configured workspace.
    ///
    /// The `--allow-shell`-requires-`--exec` rule for single-workspace mode
    /// stays in [`crate::permissions::Permissions::from_flags`], exactly as
    /// in v0.1.
    pub fn validate(&self) -> anyhow::Result<()> {
        if self.workspace_config.is_none() {
            return Ok(());
        }
        if self.workspace.is_some() {
            anyhow::bail!(
                "--workspace-config cannot be combined with a positional WORKSPACE. \
                 Use either a single workspace root or a workspace registry \
                 configuration, never both."
            );
        }
        if self.write {
            anyhow::bail!(
                "--write cannot be combined with --workspace-config. \
                 In registry mode, permissions are configured per workspace \
                 (write/exec/allow_shell) in the configuration file."
            );
        }
        if self.exec {
            anyhow::bail!(
                "--exec cannot be combined with --workspace-config. \
                 In registry mode, permissions are configured per workspace \
                 (write/exec/allow_shell) in the configuration file."
            );
        }
        if self.allow_shell {
            anyhow::bail!(
                "--allow-shell cannot be combined with --workspace-config. \
                 In registry mode, permissions are configured per workspace \
                 (write/exec/allow_shell) in the configuration file."
            );
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cli(workspace: Option<PathBuf>, workspace_config: Option<PathBuf>) -> Cli {
        Cli {
            workspace,
            write: false,
            exec: false,
            allow_shell: false,
            workspace_config,
            transport: TransportChoice::Stdio,
            host: "127.0.0.1".to_string(),
            port: 8787,
            log_level: None,
        }
    }

    #[test]
    fn single_workspace_mode_validates() {
        cli(Some(PathBuf::from(".")), None).validate().unwrap();
        cli(None, None).validate().unwrap();
    }

    #[test]
    fn registry_mode_alone_validates() {
        cli(None, Some(PathBuf::from("workspaces.toml")))
            .validate()
            .unwrap();
    }

    #[test]
    fn positional_workspace_and_registry_are_rejected() {
        let err = cli(
            Some(PathBuf::from(".")),
            Some(PathBuf::from("workspaces.toml")),
        )
        .validate()
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("cannot be combined with a positional WORKSPACE"),
            "{err}"
        );
    }

    #[test]
    fn permission_flags_are_rejected_in_registry_mode() {
        let config = Some(PathBuf::from("workspaces.toml"));

        let mut c = cli(None, config.clone());
        c.write = true;
        let err = c.validate().unwrap_err().to_string();
        assert!(
            err.contains("--write cannot be combined with --workspace-config"),
            "{err}"
        );

        let mut c = cli(None, config.clone());
        c.exec = true;
        let err = c.validate().unwrap_err().to_string();
        assert!(
            err.contains("--exec cannot be combined with --workspace-config"),
            "{err}"
        );

        let mut c = cli(None, config);
        c.allow_shell = true;
        let err = c.validate().unwrap_err().to_string();
        assert!(
            err.contains("--allow-shell cannot be combined with --workspace-config"),
            "{err}"
        );
    }
}
