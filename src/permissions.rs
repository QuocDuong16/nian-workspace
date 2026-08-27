/// Conservative permission model (spec section 5).
///
/// Read access is always enabled; everything else is off by default and must
/// be enabled explicitly via CLI flags. Permissions are never promoted
/// silently at runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Permissions {
    pub read: bool,
    pub write: bool,
    pub exec: bool,
    pub shell: bool,
}

impl Default for Permissions {
    fn default() -> Self {
        Self {
            read: true,
            write: false,
            exec: false,
            shell: false,
        }
    }
}

impl Permissions {
    /// Build permissions from CLI flags, rejecting invalid combinations.
    ///
    /// `--allow-shell` requires `--exec` (spec section 13).
    pub fn from_flags(write: bool, exec: bool, allow_shell: bool) -> anyhow::Result<Self> {
        if allow_shell && !exec {
            anyhow::bail!(
                "--allow-shell requires --exec. \
                 Shell execution is a superset of program execution."
            );
        }
        Ok(Self {
            read: true,
            write,
            exec,
            shell: allow_shell,
        })
    }

    pub fn require_write(&self) -> Result<(), crate::error::ToolError> {
        if self.write {
            Ok(())
        } else {
            Err(crate::error::ToolError::msg(
                "Write permission is disabled. Start nian-workspace with --write to modify workspace files.",
            ))
        }
    }

    pub fn require_exec(&self) -> Result<(), crate::error::ToolError> {
        if self.exec {
            Ok(())
        } else {
            Err(crate::error::ToolError::msg(
                "Command execution is disabled. Start nian-workspace with --exec to run commands.",
            ))
        }
    }

    pub fn require_shell(&self) -> Result<(), crate::error::ToolError> {
        self.require_exec()?;
        if self.shell {
            Ok(())
        } else {
            Err(crate::error::ToolError::msg(
                "Shell execution is disabled. Start nian-workspace with --write --exec --allow-shell to run shell commands.",
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let p = Permissions::default();
        assert!(p.read);
        assert!(!p.write);
        assert!(!p.exec);
        assert!(!p.shell);
    }

    #[test]
    fn read_only_by_default_flags() {
        let p = Permissions::from_flags(false, false, false).unwrap();
        assert_eq!(p, Permissions::default());
    }

    #[test]
    fn write_flag_enables_patch() {
        let p = Permissions::from_flags(true, false, false).unwrap();
        assert!(p.write);
        assert!(p.require_write().is_ok());
        assert!(p.require_exec().is_err());
    }

    #[test]
    fn exec_flag_enables_commands() {
        let p = Permissions::from_flags(true, true, false).unwrap();
        assert!(p.require_exec().is_ok());
        assert!(p.require_shell().is_err());
    }

    #[test]
    fn shell_requires_exec() {
        assert!(Permissions::from_flags(true, false, true).is_err());
        assert!(Permissions::from_flags(true, true, true).is_ok());
    }

    #[test]
    fn error_messages_mention_flags() {
        let p = Permissions::default();
        let err = p.require_write().unwrap_err().to_string();
        assert!(err.contains("--write"));
        let err = p.require_exec().unwrap_err().to_string();
        assert!(err.contains("--exec"));
        // Exec allowed but shell still disabled -> guidance mentions --allow-shell.
        let p = Permissions::from_flags(true, true, false).unwrap();
        let err = p.require_shell().unwrap_err().to_string();
        assert!(err.contains("--allow-shell"));
    }
}
