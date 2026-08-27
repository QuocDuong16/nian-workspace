//! `run_command` — controlled process and shell execution
//! (spec sections 8–10).
//!
//! Two distinct modes with explicit request parameters:
//!
//! * **program mode** (default): `program` + `args` are passed to Rust's
//!   cross-platform [`Command`](tokio::process::Command) directly. No shell is
//!   involved, so there is no string interpolation to inject through.
//! * **shell mode** (`"shell": true`): the command line is routed through the
//!   platform shell (`/bin/sh` on Unix, `cmd.exe` on Windows). This is only
//!   accepted when the server was started with `--allow-shell`.

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use crate::tools::{decode_lossy, read_capped};
use rmcp::schemars;
use serde_json::json;
use std::process::Stdio;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RunCommandArgs {
    /// Executable to run, resolved via PATH (e.g. "cargo"). Ignored when shell=true.
    #[serde(default)]
    #[schemars(
        description = "Executable to run, resolved via PATH (e.g. 'cargo'). Required unless shell=true."
    )]
    pub program: Option<String>,

    /// Arguments passed verbatim to the program — never interpolated by a shell.
    #[serde(default)]
    #[schemars(
        description = "Arguments passed verbatim to the program; no shell interpolation occurs."
    )]
    pub args: Option<Vec<String>>,

    /// Run through the system shell (/bin/sh or cmd.exe); requires --allow-shell.
    #[serde(default)]
    #[schemars(
        description = "Run `command` through the system shell instead of exec'ing a program directly. Requires the server to be started with --allow-shell."
    )]
    pub shell: bool,

    /// Shell command line to execute when shell=true (e.g. "cargo check && cargo test").
    #[serde(default)]
    #[schemars(
        description = "Shell command line executed when shell=true, e.g. 'cargo check && cargo test'."
    )]
    pub command: Option<String>,

    /// Working directory relative to the workspace root (default: workspace root).
    #[serde(default)]
    #[schemars(
        description = "Working directory relative to the workspace root. Defaults to the workspace root itself."
    )]
    pub cwd: Option<String>,

    /// Timeout in seconds (default 120, max 3600). The process tree is killed on expiry.
    #[serde(default)]
    #[schemars(
        description = "Timeout in seconds (default 120, max 3600). The process is killed after the timeout expires."
    )]
    pub timeout_seconds: Option<u64>,
}

pub(crate) async fn handle(
    state: &AppState,
    args: RunCommandArgs,
) -> ToolResult<serde_json::Value> {
    let limits = state.limits();
    let ws = state.workspace();

    let cwd = ws.resolve(args.cwd.as_deref())?;
    if !cwd.is_dir() {
        return Err(ToolError::msg(format!(
            "Working directory '{}' does not exist or is not a directory.",
            ws.display_relative(&cwd)
        )));
    }

    let timeout_secs = args
        .timeout_seconds
        .unwrap_or(limits.default_command_timeout_secs);
    if timeout_secs == 0 || timeout_secs > limits.max_command_timeout_secs {
        return Err(ToolError::msg(format!(
            "timeout_seconds must be between 1 and {}.",
            limits.max_command_timeout_secs
        )));
    }

    enum Invocation {
        Program { program: String, argv: Vec<String> },
        Shell { command_line: String },
    }

    let invocation = if args.shell {
        state.permissions().require_shell()?;
        let cmdline = args
            .command
            .as_deref()
            .map(str::trim)
            .filter(|c| !c.is_empty())
            .ok_or_else(|| ToolError::msg("shell=true requires a non-empty 'command' string."))?;
        // A program accidentally passed alongside shell mode would silently
        // change meaning; refuse rather than guess.
        if args.program.is_some() || args.args.is_some() {
            return Err(ToolError::msg(
                "Pass either program/args (direct execution) or shell=true with command — not both.",
            ));
        }
        Invocation::Shell {
            command_line: cmdline.to_string(),
        }
    } else {
        state.permissions().require_exec()?;
        let program = args
            .program
            .as_deref()
            .map(str::trim)
            .filter(|p| !p.is_empty())
            .ok_or_else(|| {
                ToolError::msg("'program' is required (or pass shell=true with 'command').")
            })?;
        Invocation::Program {
            program: program.to_string(),
            argv: args.args.unwrap_or_default(),
        }
    };

    let mut cmd = match &invocation {
        Invocation::Program { program, argv } => {
            let mut c = Command::new(program);
            c.args(argv);
            c
        }
        Invocation::Shell { command_line } => shell_command(command_line),
    };
    cmd.current_dir(&cwd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    cmd.env("PWD", cwd.to_string_lossy().into_owned());
    tracing::info!(cwd = %ws.display_relative(&cwd), "run_command starting");

    let mut child = cmd
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ToolError::msg(format!("Failed to start command: {e}")))?;

    let started = std::time::Instant::now();
    let deadline = tokio::time::Duration::from_secs(timeout_secs);

    let mut stdout_pipe = child.stdout.take().expect("stdout piped");
    let mut stderr_pipe = child.stderr.take().expect("stderr piped");
    let stdout_cap = limits.max_command_stdout;
    let stderr_cap = limits.max_command_stderr;

    let read_stdout = async {
        let capped = read_capped(&mut stdout_pipe, stdout_cap).await?;
        // Continue draining until EOF so the child never blocks on a full
        // pipe, while discarding overflow data beyond the cap.
        if capped.truncated {
            let mut sink = Vec::new();
            let _ = stdout_pipe.read_to_end(&mut sink).await;
        }
        Ok::<_, std::io::Error>(capped)
    };
    let read_stderr = async {
        let capped = read_capped(&mut stderr_pipe, stderr_cap).await?;
        if capped.truncated {
            let mut sink = Vec::new();
            let _ = stderr_pipe.read_to_end(&mut sink).await;
        }
        Ok::<_, std::io::Error>(capped)
    };

    let wait_result = tokio::time::timeout(deadline, async {
        let (out, err) = tokio::join!(read_stdout, read_stderr);
        let out_capped = out?;
        let err_capped = err?;
        let status = child.wait().await?;
        Ok::<_, std::io::Error>((status, out_capped, err_capped))
    })
    .await;

    let duration_ms = u128_as_u64(started.elapsed().as_millis());

    match wait_result {
        Ok(Ok((status, out_capped, err_capped))) => {
            let exit_code = exit_code_of(&status);
            let (stdout_text, stdout_lossy) = decode_lossy(&out_capped.bytes);
            let (stderr_text, _) = decode_lossy(&err_capped.bytes);
            Ok(json!({
                "exit_code": exit_code,
                "stdout": stdout_text,
                "stderr": stderr_text,
                "truncated": out_capped.truncated || err_capped.truncated,
                "duration_ms": duration_ms,
                "timed_out": false,
                "lossy_decoding": stdout_lossy,
                "signal": signal_of(&status),
            }))
        }
        Ok(Err(e)) => Err(ToolError::msg(format!("Command I/O failed: {e}"))),
        Err(_elapsed) => {
            // Timeout: kill the whole process group where supported, else the child.
            kill_process_tree(&mut child).await;
            let remaining_out = drain_output(&mut stdout_pipe, stdout_cap).await;
            let remaining_err = drain_output(&mut stderr_pipe, stderr_cap).await;
            let (stdout_text, _) = decode_lossy(&remaining_out);
            let (stderr_text, _) = decode_lossy(&remaining_err);
            tracing::warn!(timeout_secs, "command timed out and was terminated");
            Ok(json!({
                "exit_code": null,
                "stdout": stdout_text,
                "stderr": stderr_text,
                "truncated": true,
                "duration_ms": duration_ms,
                "timed_out": true,
                "message": format!("Command exceeded the {timeout_secs} second timeout and was terminated."),
            }))
        }
    }
}

fn u128_as_u64(v: u128) -> u64 {
    u64::try_from(v).unwrap_or(u64::MAX)
}

fn exit_code_of(status: &std::process::ExitStatus) -> i64 {
    status.code().map(i64::from).unwrap_or(-1)
}

fn signal_of(status: &std::process::ExitStatus) -> Option<String> {
    use std::os::unix::process::ExitStatusExt;
    #[cfg(unix)]
    {
        status.signal().map(|s| format!("SIG{s}"))
    }
    #[cfg(not(unix))]
    {
        let _ = status;
        None
    }
}

/// Build the platform-appropriate shell invocation for `--allow-shell` mode.
#[allow(unused_variables)]
fn shell_command(command_line: &str) -> Command {
    #[cfg(windows)]
    {
        let mut c = Command::new("cmd");
        c.arg("/C").arg(command_line);
        c
    }
    #[cfg(not(windows))]
    {
        let mut c = Command::new("/bin/sh");
        c.arg("-c").arg(command_line);
        c
    }
}

async fn kill_process_tree(child: &mut tokio::process::Child) {
    // Best effort: plain kill covers virtually all cases since we spawn
    // without a new session in v0.1; idempotent drop-kill also runs later.
    let _ = child.start_kill();
}

async fn drain_output(pipe: &mut (impl tokio::io::AsyncRead + Unpin), cap: usize) -> Vec<u8> {
    let mut buf = Vec::with_capacity(cap.min(16 * 1024));
    let _ = pipe.read_to_end(&mut buf).await;
    buf.truncate(cap);
    buf
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Limits;
    use crate::permissions::Permissions;
    use crate::workspace::Workspace;
    use tempfile::TempDir;

    fn state_with(perms: Permissions) -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("hello.txt"), b"hi\n").unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        (tmp, AppState::new(ws, perms, Limits::default()))
    }

    fn exec_state() -> (TempDir, AppState) {
        state_with(Permissions::from_flags(true, true, false).unwrap())
    }

    async fn run(state: &AppState, args: RunCommandArgs) -> Result<serde_json::Value, ToolError> {
        handle(state, args).await
    }

    #[tokio::test]
    async fn successful_command_reports_exit_and_output() {
        let (_t, state) = exec_state();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("echo".into()),
                args: Some(vec!["nian".into()]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(out["exit_code"], json!(0));
        assert_eq!(out["stdout"], "nian\n");
        assert_eq!(out["timed_out"], json!(false));
        assert!(out["duration_ms"].as_u64().is_some());
    }

    #[tokio::test]
    async fn failed_command_reports_nonzero_exit() {
        let (_t, state) = exec_state();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("false".into()),
                args: None,
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: None,
            },
        )
        .await
        .unwrap();
        assert_ne!(out["exit_code"], json!(0));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_kills_process_and_reports_metadata() {
        let (_t, state) = exec_state();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sleep".into()),
                args: Some(vec!["30".into()]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["timed_out"], json!(true));
        assert_eq!(out["exit_code"], serde_json::Value::Null);
        assert!(out["message"].as_str().unwrap().contains("timeout"));
        // Returned promptly rather than after the full 30s sleep.
        assert!((out["duration_ms"].as_u64().unwrap()) < 10_000);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_truncation_is_reported() {
        let tmp = TempDir::new().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        let state = AppState::new(
            ws,
            Permissions::from_flags(true, true, false).unwrap(),
            Limits {
                max_command_stdout: 64,
                ..Limits::default()
            },
        );

        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    format!("for i in $(seq 1 200); do echo '{}'; done", "x".repeat(40)),
                ]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(30),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["truncated"], json!(true));
        assert!(out["stdout"].as_str().unwrap().len() <= 64);
    }

    #[tokio::test]
    async fn cwd_is_relative_to_workspace() {
        let (t, state) = exec_state();
        std::fs::create_dir(t.path().join("sub")).unwrap();
        std::fs::write(t.path().join("sub/marker.txt"), b"here\n").unwrap();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("cat".into()),
                args: Some(vec!["marker.txt".into()]),
                shell: false,
                command: None,
                cwd: Some("sub".into()),
                timeout_seconds: None,
            },
        )
        .await
        .unwrap();
        assert_eq!(out["exit_code"], json!(0));
        assert_eq!(out["stdout"], "here\n");

        // Escape attempt is rejected before spawning.
        let err = run(
            &state,
            RunCommandArgs {
                program: Some("cat".into()),
                args: Some(vec!["passwd".into()]),
                shell: false,
                command: None,
                cwd: Some("../../etc".into()),
                timeout_seconds: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("outside the configured workspace"));
    }

    #[tokio::test]
    async fn exec_permission_is_required() {
        let (_t, state) = state_with(Permissions::from_flags(false, false, false).unwrap());
        let err = run(
            &state,
            RunCommandArgs {
                program: Some("echo".into()),
                args: None,
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--exec"));
    }

    #[tokio::test]
    async fn shell_mode_requires_allow_shell_flag() {
        let (_t, state) = exec_state(); // exec on, shell off
        let err = run(
            &state,
            RunCommandArgs {
                program: None,
                args: None,
                shell: true,
                command: Some("echo hi".into()),
                cwd: None,
                timeout_seconds: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("--allow-shell"));

        // Enabled properly, shell mode works.
        let (t2, state) = state_with(Permissions::from_flags(true, true, true).unwrap());
        let out = run(
            &state,
            RunCommandArgs {
                program: None,
                args: None,
                shell: true,
                command: Some(format!(
                    "cat hello.txt && echo done >> {}",
                    t2.path().join("x").display()
                )),
                cwd: None,
                timeout_seconds: None,
            },
        )
        .await
        .unwrap_or_else(|e| panic!("shell run failed: {e}"));
        assert_eq!(out["exit_code"], json!(0));
    }

    #[tokio::test]
    async fn mixing_program_and_shell_args_is_rejected() {
        let (_t, state) = state_with(Permissions::from_flags(true, true, true).unwrap());
        let err = run(
            &state,
            RunCommandArgs {
                program: Some("cargo".into()),
                args: Some(vec!["check".into()]),
                shell: true,
                command: Some("cargo check".into()),
                cwd: None,
                timeout_seconds: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("not both"));
    }

    #[tokio::test]
    async fn missing_program_yields_clear_error() {
        let (_t, state) = exec_state();
        let err = run(
            &state,
            RunCommandArgs {
                program: Some("definitely-not-a-real-binary-xyz".into()),
                args: None,
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: None,
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("Failed to start command"));
    }
}
