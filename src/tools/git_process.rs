//! Shared hardened Git process execution.
//!
//! Both read-only Git tools (`git_status`, `git_diff`) and the
//! `workspace_info` probe create their Git processes here — nowhere else — so
//! repository- or user-configured external program execution cannot be
//! triggered by what a workspace contains or what environment leaks in:
//!
//! * `-c core.fsmonitor=false` stops Git from spawning an fsmonitor hook or
//!   daemon during our queries
//! * pagers are disabled by flag, config, and environment (`--no-pager`,
//!   `pager.*=false`, `core.pager=cat`, `PAGER=cat`)
//! * `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`/`GIT_EXTERNAL_DIFF`/… are
//!   stripped per invocation so stale shell state cannot redirect the query
//!   into another repository or an external diff program
//! * `diff` callers additionally pass `--no-ext-diff --no-textconv`
//! * stdout and stderr are captured with hard caps; past the cap data is
//!   discarded with fixed-size reads instead of accumulating in memory

use crate::error::{ToolError, ToolResult};
use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

/// Environment variables that could redirect git into a different repository,
/// index, editor, or external-program execution path. Removed for every
/// invocation. Deliberately narrow — the rest of the environment is inherited.
const STRIPPED_GIT_ENV: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_EXTERNAL_DIFF",
    "GIT_PAGER",
    "GIT_EDITOR",
    "GIT_SEQUENCE_EDITOR",
    "GIT_PAGER_IN_USE",
];

/// Config overrides applied to every invocation so neither repository-local
/// `.git/config` nor user/system config can re-enable these execution paths.
const HARDENED_CONFIG: &[&str] = &[
    "-c",
    "core.fsmonitor=false",
    "-c",
    "core.pager=cat",
    "-c",
    "pager.diff=false",
    "-c",
    "pager.status=false",
];

#[derive(Debug)]
pub(crate) struct GitOutput {
    pub stdout: String,
    pub truncated: bool,
}

/// One hardened git invocation: fixed root, fixed subcommand args, output
/// caps, and the exact exit codes accepted as success.
pub(crate) struct GitInvocation<'a> {
    pub root: &'a Path,
    /// Subcommand and its arguments (after all `-c` overrides).
    pub args: &'a [&'a str],
    /// Byte caps applied independently to stdout and stderr retention.
    pub cap: usize,
    /// Exit codes considered successful. Read-only callers pass `[0]`;
    /// nothing treats generic non-zero as success implicitly.
    pub ok_exit_codes: &'a [i32],
}

fn base_command(invocation: &GitInvocation<'_>) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C").arg(invocation.root).args(HARDENED_CONFIG);
    // Environment: keep the parent's normal environment (PATH etc.) but pin
    // the variables that matter for non-interactive, repository-local,
    // hook-free operation.
    cmd.env("GIT_OPTIONAL_LOCKS", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("PAGER", "cat");
    for var in STRIPPED_GIT_ENV {
        cmd.env_remove(var);
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd
}

/// Run git as described, bounding both stdout and stderr.
///
/// Concurrency: stderr drains on a side thread while stdout is read on the
/// calling thread, so neither pipe can fill and deadlock the child. Errors
/// carry bounded stderr text so clients see git's own diagnostics.
pub(crate) fn run_git_bounded(invocation: &GitInvocation<'_>) -> ToolResult<GitOutput> {
    let mut child = base_command(invocation)
        .args(invocation.args)
        .spawn()
        .map_err(|e| ToolError::msg(format!("Failed to start git: {e}")))?;

    let mut stdout_pipe = child.stdout.take().expect("git stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("git stderr piped");

    let cap = invocation.cap;
    let drain_stderr = std::thread::spawn(move || collect_capped(&mut stderr_pipe, cap));
    let (stdout_bytes, read_err) = collect_capped(&mut stdout_pipe, cap);
    if let Some(e) = read_err {
        let _ = child.kill();
        let _ = drain_stderr.join();
        return Err(ToolError::msg(format!("Failed reading git output: {e}")));
    }
    let (stderr_bytes, _) = drain_stderr.join().unwrap_or((Vec::new(), None));

    let status = child
        .wait()
        .map_err(|e| ToolError::msg(format!("git failed: {e}")))?;
    let exit_code = status.code().unwrap_or(-1);

    if !invocation.ok_exit_codes.contains(&exit_code) {
        let stderr_text = crate::tools::decode_lossy(&stderr_bytes).0;
        return Err(ToolError::msg(format!(
            "git exited with code {exit_code}: {}",
            stderr_text.trim()
        )));
    }

    let (stdout, _) = crate::tools::decode_lossy(&stdout_bytes);
    Ok(GitOutput {
        stdout,
        truncated: stdout_bytes.len() >= cap || stderr_bytes.len() >= cap,
    })
}

/// Cheap work-tree membership check using the same hardened rules.
pub(crate) fn inside_git_worktree(root: &Path) -> bool {
    let invocation = GitInvocation {
        root,
        args: &["rev-parse", "--is-inside-work-tree"],
        cap: 1024,
        ok_exit_codes: &[0],
    };
    run_git_bounded(&invocation)
        .map(|o| o.stdout.trim() == "true")
        .unwrap_or(false)
}

/// Read at most `cap` bytes from `pipe`; once full, continue draining to EOF
/// discarding everything in fixed-size chunks (never grows past `cap`).
/// Returns (bytes, io_error_if_any).
fn collect_capped(pipe: &mut impl Read, cap: usize) -> (Vec<u8>, Option<std::io::Error>) {
    let mut bytes: Vec<u8> = Vec::with_capacity(cap.min(64 * 1024));
    let mut chunk = [0u8; 16 * 1024];
    loop {
        match pipe.read(&mut chunk) {
            Ok(0) => return (bytes, None),
            Ok(n) => {
                let take = n.min(cap.saturating_sub(bytes.len()));
                bytes.extend_from_slice(&chunk[..take]);
                // Past the cap: discard the rest chunk-wise, no allocation.
            }
            Err(e) => return (bytes, Some(e)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Structural guarantee: every hardened command strips the environment
    /// variables that could redirect git into another repository or an
    /// external-program execution path, regardless of what the parent shell
    /// leaks in. (A behavioral end-to-end variant would require mutating
    /// this process's own environment, which races parallel test threads.)
    #[test]
    fn hardened_command_strips_redirection_vars() {
        let root = std::path::Path::new(".");
        let invocation = GitInvocation {
            root,
            args: &["status", "--short"],
            cap: 1024,
            ok_exit_codes: &[0],
        };
        let cmd = base_command(&invocation);

        let removed: Vec<String> = cmd
            .get_envs()
            .filter(|(_, v)| v.is_none())
            .map(|(k, _)| k.to_string_lossy().into_owned())
            .collect();

        for var in [
            "GIT_DIR",
            "GIT_WORK_TREE",
            "GIT_INDEX_FILE",
            "GIT_EXTERNAL_DIFF",
        ] {
            assert!(
                removed.iter().any(|k| k == var),
                "hardened command must strip {var}; removals = {removed:?}"
            );
        }

        let pinned: Vec<(String, String)> = cmd
            .get_envs()
            .filter_map(|(k, v)| {
                v.map(|v| {
                    (
                        k.to_string_lossy().into_owned(),
                        v.to_string_lossy().into_owned(),
                    )
                })
            })
            .collect();
        for (key, expected) in [("GIT_OPTIONAL_LOCKS", "1"), ("GIT_TERMINAL_PROMPT", "0")] {
            assert_eq!(
                pinned
                    .iter()
                    .find(|(k, _)| k == key)
                    .map(|(_, v)| v.as_str()),
                Some(expected),
                "{key} must be pinned for non-interactive hardened runs"
            );
        }
        assert!(
            pinned.iter().any(|(k, v)| k == "PAGER" && v == "cat"),
            "pagers must be disabled via environment too"
        );
    }
}
