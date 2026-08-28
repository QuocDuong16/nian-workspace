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
use crate::process::ProcessTreeGuard;
use crate::tools::{decode_lossy, read_capped, CappedBytes};
use rmcp::schemars;
use serde_json::json;
use std::process::Stdio;
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
    // Contain the whole tree before spawn: own process group on Unix,
    // Job Object attachment right after spawn on Windows.
    crate::process::configure(&mut cmd);
    tracing::info!(cwd = %ws.display_relative(&cwd), "run_command starting");

    let mut child = cmd
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| ToolError::msg(format!("Failed to start command: {e}")))?;
    let tree = ProcessTreeGuard::attach(child.id());
    let started = std::time::Instant::now();
    let deadline = tokio::time::Duration::from_secs(timeout_secs);

    let stdout_cap = limits.max_command_stdout;
    let stderr_cap = limits.max_command_stderr;
    // Output readers run as independent tasks that own their pipes: bytes
    // consumed before a timeout stay buffered inside the task instead of
    // vanishing with a cancelled future. Output emitted right before a
    // timeout is usually the most useful diagnostic, so it must survive.
    // The AbortOnDrop wrapper guarantees a cancelled run_command call can
    // never leave them detached.
    let mut stdout_task = AbortOnDrop::spawn(read_capped(
        child.stdout.take().expect("stdout piped"),
        stdout_cap,
    ));
    let mut stderr_task = AbortOnDrop::spawn(read_capped(
        child.stderr.take().expect("stderr piped"),
        stderr_cap,
    ));

    // Main deadline: the lifetime of the DIRECT command process — nothing
    // else. A descendant that inherits the pipe write-ends must never turn a
    // completed command into a timeout.
    let wait_result = tokio::time::timeout(deadline, child.wait()).await;

    match wait_result {
        Ok(Ok(status)) => {
            let drain_result = tokio::time::timeout(
                POST_EXIT_DRAIN_GRACE,
                collect_readers(&mut stdout_task, &mut stderr_task),
            )
            .await;
            let (out_res, err_res) = match drain_result {
                Ok((out, err)) => (Some(out), Some(err)),
                Err(_) => {
                    // Descendants still hold the pipes: terminate the
                    // remaining contained tree so their handles close, then
                    // allow one final short collection window. This is
                    // leftover cleanup — never a command timeout.
                    tracing::debug!(
                        "output pipes still open after child exit; terminating leftover descendants"
                    );
                    tree.terminate_remaining_descendants();
                    match tokio::time::timeout(
                        POST_EXIT_DRAIN_GRACE,
                        collect_readers(&mut stdout_task, &mut stderr_task),
                    )
                    .await
                    {
                        Ok((out, err)) => (Some(out), Some(err)),
                        Err(_) => {
                            // Escaped pipe-holder: abort the readers and
                            // report the honest, incomplete state.
                            stdout_task.abort();
                            stderr_task.abort();
                            tracing::warn!("output collection incomplete after normal child exit");
                            (None, None)
                        }
                    }
                }
            };

            let (out_capped, out_full) = reader_outcome(out_res);
            let (err_capped, err_full) = reader_outcome(err_res);
            let exit_code = exit_code_of(&status);
            let (stdout_text, stdout_lossy) = decode_lossy(&out_capped.bytes);
            let (stderr_text, _) = decode_lossy(&err_capped.bytes);
            let mut result = json!({
                "exit_code": exit_code,
                "stdout": stdout_text,
                "stderr": stderr_text,
                "truncated": !out_full
                    || !err_full
                    || out_capped.truncated
                    || err_capped.truncated,
                "duration_ms": u128_as_u64(started.elapsed().as_millis()),
                "timed_out": false,
                "lossy_decoding": stdout_lossy,
                "signal": signal_of(&status),
            });
            if !(out_full && err_full) {
                result["message"] = json!(
                    "Command exited, but output collection was incomplete because descendant processes kept pipe handles open."
                );
            }
            Ok(result)
        }
        Ok(Err(e)) => {
            // Reading the exit status failed; nothing worth collecting. The
            // reader tasks are aborted so no pipe handles linger.
            stdout_task.abort();
            stderr_task.abort();
            Err(ToolError::msg(format!("Command I/O failed: {e}")))
        }
        Err(_elapsed) => {
            // Timeout. Invariants: a command timeout bounds how long this
            // function can remain stuck afterwards, and output emitted before
            // the kill is preserved (it lives in the reader tasks).
            //
            // Teardown sequence, all inside a small fixed grace deadline:
            //   1. terminate the whole process tree
            //   2. reap the direct child (wait)
            //   3. collect the reader tasks — an escaped descendant may hold
            //      the inherited pipe write-ends open forever, so collection
            //      is bounded too; if the grace expires the tasks are
            //      aborted, which also closes the pipe read-ends.
            const POST_KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

            let teardown = async {
                tree.terminate_tree(&mut child).await;
                // Reap the direct child so it cannot linger as a zombie.
                let reaped = child.wait().await.is_ok();
                let out = (&mut stdout_task).await;
                let err = (&mut stderr_task).await;
                (reaped, out, err)
            };
            let teardown_result = tokio::time::timeout(POST_KILL_GRACE, teardown).await;

            let (reaped, out_res, err_res) = match teardown_result {
                Ok((reaped, out, err)) => (reaped, Some(out), Some(err)),
                Err(_) => {
                    // Grace expired: abort the reader tasks so nothing
                    // lingers behind an escaped pipe-holder.
                    stdout_task.abort();
                    stderr_task.abort();
                    tracing::warn!(timeout_secs, "post-kill cleanup exceeded its grace period");
                    (false, None, None)
                }
            };
            let (remaining_out, out_full) = reader_outcome(out_res);
            let (remaining_err, err_full) = reader_outcome(err_res);
            let cleaned_up = reaped && out_full && err_full;

            tracing::warn!(
                timeout_secs,
                graceful_teardown = cleaned_up,
                "command timed out and was terminated"
            );
            let (stdout_text, _) = decode_lossy(&remaining_out.bytes);
            let (stderr_text, _) = decode_lossy(&remaining_err.bytes);
            Ok(json!({
                "exit_code": null,
                "stdout": stdout_text,
                "stderr": stderr_text,
                "truncated": !out_full
                    || !err_full
                    || remaining_out.truncated
                    || remaining_err.truncated,
                "duration_ms": u128_as_u64(started.elapsed().as_millis()),
                "timed_out": true,
                "message": format!(
                    "Command exceeded the {timeout_secs} second timeout and was terminated{}.",
                    if cleaned_up {
                        ""
                    } else {
                        "; some post-kill cleanup did not finish within its grace period"
                    }
                ),
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
    #[cfg(unix)]
    {
        use std::os::unix::process::ExitStatusExt;
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

fn empty_capped() -> CappedBytes {
    CappedBytes {
        bytes: Vec::new(),
        truncated: false,
    }
}

/// Fixed grace for collecting pipe output after the direct child has exited.
/// EOF normally arrives within milliseconds of exit; only a descendant that
/// inherited the write-ends can delay it, and the remaining tree is
/// terminated when this window lapses. Not user-configurable in v0.1.
const POST_EXIT_DRAIN_GRACE: std::time::Duration = std::time::Duration::from_secs(1);

/// RAII wrapper around a spawned reader task: dropping the guard aborts the
/// task, so a cancelled `run_command` future (client disconnect, transport or
/// server shutdown) can never leave detached tasks blocked on pipes forever.
/// Awaiting yields the task's result; aborting a finished task is a no-op.
struct AbortOnDrop<T> {
    handle: tokio::task::JoinHandle<T>,
}

impl<T> AbortOnDrop<T> {
    fn spawn(task: impl std::future::Future<Output = T> + Send + 'static) -> Self
    where
        T: Send + 'static,
    {
        Self {
            handle: tokio::spawn(task),
        }
    }

    fn abort(&self) {
        self.handle.abort();
    }
}

impl<T> Drop for AbortOnDrop<T> {
    fn drop(&mut self) {
        self.handle.abort();
    }
}

impl<T> std::future::Future for AbortOnDrop<T> {
    type Output = Result<T, tokio::task::JoinError>;

    fn poll(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Self::Output> {
        std::pin::Pin::new(&mut self.handle).poll(cx)
    }
}

/// One reader task's result: the bounded buffer, or why it could not be
/// obtained (pipe I/O error or task failure).
type ReaderResult = Result<Result<CappedBytes, std::io::Error>, tokio::task::JoinError>;

/// Await both reader tasks for their final buffers. Called with a fresh
/// borrow each time it is (re)tried within a grace deadline.
async fn collect_readers(
    stdout_task: &mut AbortOnDrop<std::io::Result<CappedBytes>>,
    stderr_task: &mut AbortOnDrop<std::io::Result<CappedBytes>>,
) -> (ReaderResult, ReaderResult) {
    let out = stdout_task.await;
    let err = stderr_task.await;
    (out, err)
}

/// Flatten one reader task's result into `(buffer, collected_fully)`. Anything
/// other than a clean EOF-with-buffer — I/O error, task panic, or collection
/// cut short by a grace deadline — counts as incomplete and is surfaced
/// through the public `truncated` flag rather than silently dropped.
fn reader_outcome(res: Option<ReaderResult>) -> (CappedBytes, bool) {
    match res {
        Some(Ok(Ok(capped))) => (capped, true),
        Some(Ok(Err(e))) => {
            tracing::warn!("command output pipe read failed: {e}");
            (empty_capped(), false)
        }
        Some(Err(join)) => {
            tracing::warn!("command output reader failed: {join}");
            (empty_capped(), false)
        }
        None => (empty_capped(), false),
    }
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
    async fn output_emitted_before_timeout_is_preserved() {
        let (_t, state) = exec_state();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "echo important-diagnostic; echo err-diagnostic >&2; sleep 30".into(),
                ]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["timed_out"], json!(true));
        let stdout = out["stdout"].as_str().unwrap();
        assert!(
            stdout.contains("important-diagnostic"),
            "stdout emitted before the timeout was lost: {stdout:?}"
        );
        let stderr = out["stderr"].as_str().unwrap();
        assert!(
            stderr.contains("err-diagnostic"),
            "stderr emitted before the timeout was lost: {stderr:?}"
        );
        // Everything was collected and nothing discarded: the truncation
        // flag must reflect that instead of being forced true.
        assert_eq!(out["truncated"], json!(false));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_before_timeout_remains_bounded() {
        // Emits far more than the cap, then hangs: the retained portion must
        // still respect the cap and report truncation.
        let tmp = TempDir::new().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        let state = AppState::new(
            ws,
            Permissions::from_flags(true, true, false).unwrap(),
            Limits {
                max_command_stdout: 256,
                ..Limits::default()
            },
        );
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    format!(
                        "for i in $(seq 1 200); do echo '{}'; done; sleep 30",
                        "y".repeat(40)
                    ),
                ]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["timed_out"], json!(true));
        assert!(out["stdout"].as_str().unwrap().len() <= 256);
        assert_eq!(out["truncated"], json!(true));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn background_pipe_holder_does_not_cause_a_timeout() {
        // The direct shell exits 0 almost immediately while the background
        // sleep inherits stdout/stderr and holds them for 10s. The command
        // must be reported as completed — never as a timeout.
        let (_t, state) = exec_state();
        let started = std::time::Instant::now();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec!["-c".into(), "sleep 10 & exit 0".into()]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(2),
            },
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(out["timed_out"], json!(false));
        assert_eq!(out["exit_code"], json!(0));
        assert!(
            elapsed < std::time::Duration::from_secs(9),
            "run_command waited for the background pipe holder: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn output_before_background_holder_exit_is_preserved() {
        let (_t, state) = exec_state();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec!["-c".into(), "echo marker; sleep 10 & exit 0".into()]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(2),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["timed_out"], json!(false));
        assert_eq!(out["exit_code"], json!(0));
        assert!(
            out["stdout"].as_str().unwrap().contains("marker"),
            "output emitted before the holder was killed was lost"
        );
        // Killing the leftover descendant closed the pipes, so the readers
        // finished with everything they consumed and nothing was discarded.
        assert_eq!(out["truncated"], json!(false));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn escaped_pipe_holder_cannot_turn_normal_exit_into_timeout() {
        // A setsid descendant escapes containment, so killing the tree cannot
        // close the pipes. The command still reports its real, successful
        // exit; collection is flagged incomplete instead.
        let (_t, state) = exec_state();
        let started = std::time::Instant::now();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec!["-c".into(), "setsid sleep 5 &\nexit 0".into()]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(2),
            },
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(out["timed_out"], json!(false));
        assert_eq!(out["exit_code"], json!(0));
        assert!(
            elapsed < std::time::Duration::from_secs(9),
            "run_command stayed stuck {:?} on an escaped pipe holder",
            elapsed
        );
        assert_eq!(out["truncated"], json!(true));
        assert!(
            out["message"]
                .as_str()
                .unwrap()
                .contains("output collection was incomplete"),
            "missing incomplete-collection explanation: {out}"
        );
    }

    #[tokio::test]
    async fn reader_guard_aborts_task_when_dropped() {
        // The abort-on-drop wrapper must cancel its task instead of letting
        // it detach: dropping the future fires the captured flag.
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::Arc;

        struct Flag(Arc<AtomicBool>);
        impl Drop for Flag {
            fn drop(&mut self) {
                self.0.store(true, Ordering::SeqCst);
            }
        }

        let flag = Arc::new(AtomicBool::new(false));
        let task_flag = Flag(flag.clone());
        let (started_tx, started_rx) = tokio::sync::oneshot::channel();
        let task = AbortOnDrop::spawn(async move {
            let _dropped_when_aborted = task_flag;
            let _ = started_tx.send(());
            std::future::pending::<()>().await;
        });
        started_rx.await.unwrap(); // task is running and parked forever
        drop(task); // must abort, not detach

        // Aborting drops the task's future, which fires the flag. Generous
        // window: the assert only fails if the abort never happens.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        assert!(
            flag.load(Ordering::SeqCst),
            "dropping the guard must abort the reader task"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_terminates_descendants_of_the_child() {
        // The shell spawns a grandchild; both must be gone after the timeout
        // fires. The grandchild writes a marker only if it is still alive
        // five seconds after being orphaned, which would fail this test.
        let tmp = TempDir::new().unwrap();
        let marker = tmp.path().join("survived");
        let ws = Workspace::open(tmp.path()).unwrap();
        let state = AppState::new(
            ws,
            Permissions::from_flags(true, true, false).unwrap(),
            Limits::default(),
        );
        let script = format!(
            "sh -c 'sleep 5 && touch {marker}' & sleep 600",
            marker = marker.display()
        );
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec!["-c".into(), script]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["timed_out"], json!(true));

        tokio::time::sleep(std::time::Duration::from_secs(6)).await;
        assert!(
            !marker.exists(),
            "grandchild outlived the timed-out command"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn post_timeout_cleanup_cannot_block_indefinitely() {
        // A descendant that escapes the process group (setsid) but keeps our
        // inherited pipes open: it dies on its own after 9 seconds, so an
        // implementation waiting for pipe EOF would hang ~8 extra seconds.
        // The bounded grace period must return well before that.
        let (_t, state) = exec_state();
        let started = std::time::Instant::now();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec!["-c".into(), "setsid sleep 9 &\nsleep 600".into()]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(1),
            },
        )
        .await
        .unwrap();
        let elapsed = started.elapsed();
        assert_eq!(out["timed_out"], json!(true));
        assert_eq!(out["exit_code"], serde_json::Value::Null);
        assert!(
            elapsed < std::time::Duration::from_secs(6),
            "run_command stayed stuck {:?} after its own deadline",
            elapsed
        );
        // The escaped pipe-holder prevented full output collection within
        // the grace period; the response must say so instead of pretending.
        assert_eq!(out["truncated"], json!(true));
        assert!(out["message"].as_str().unwrap().contains("grace period"));
    }

    #[cfg(unix)]
    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn timed_out_child_is_reaped_before_returning() {
        // Behavioral check for wait/reap ordering: once run_command returns
        // from a timeout, no zombie child of this test process may remain.
        let (_t, state) = exec_state();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sleep".into()),
                args: Some(vec!["60".into()]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(1),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["timed_out"], json!(true));

        let self_pid = std::process::id().to_string();
        // Sum the per-thread children lists of this test process.
        let mut children_text = String::new();
        if let Ok(entries) = std::fs::read_dir(format!("/proc/{self_pid}/task")) {
            for entry in entries.flatten() {
                if let Ok(c) = std::fs::read_to_string(entry.path().join("children")) {
                    children_text.push_str(&c);
                }
            }
        }

        let zombies: Vec<String> = children_text
            .split_whitespace()
            .filter(|pid| {
                std::fs::read_to_string(format!("/proc/{pid}/stat"))
                    .map(|stat| {
                        stat.rsplit(')')
                            .next()
                            .unwrap_or("")
                            .trim_start()
                            .starts_with('Z')
                    })
                    .unwrap_or(false)
            })
            .map(str::to_owned)
            .collect();
        assert!(
            zombies.is_empty(),
            "zombie children left behind after timeout teardown: {zombies:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_output_is_discarded_without_unbounded_allocation() {
        // ~32 MiB of stdout against a tiny cap: must complete without
        // deadlock and retain at most `cap` bytes.
        let tmp = TempDir::new().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        let state = AppState::new(
            ws,
            Permissions::from_flags(true, true, false).unwrap(),
            Limits {
                max_command_stdout: 256,
                ..Limits::default()
            },
        );
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("head".into()),
                args: Some(vec![
                    "-c".into(),
                    (32 * 1024 * 1024).to_string(),
                    "/dev/zero".into(),
                ]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(60),
            },
        )
        .await
        .unwrap();
        assert!(out["stdout"].as_str().unwrap().len() <= 256);
        assert_eq!(out["truncated"], json!(true));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn oversized_stderr_is_bounded_without_deadlock() {
        // ~32 MiB on stderr against a tiny stderr cap; stdout stays empty.
        let tmp = TempDir::new().unwrap();
        let ws = Workspace::open(tmp.path()).unwrap();
        let state = AppState::new(
            ws,
            Permissions::from_flags(true, true, false).unwrap(),
            Limits {
                max_command_stderr: 256,
                ..Limits::default()
            },
        );
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "dd if=/dev/zero bs=1M count=32 1>&2 2>/dev/null".into(),
                ]),
                shell: false,
                command: None,
                cwd: None,
                timeout_seconds: Some(60),
            },
        )
        .await
        .unwrap();
        assert_eq!(out["exit_code"], json!(0));
        assert!(out["stderr"].as_str().unwrap().len() <= 256);
        assert_eq!(out["truncated"], json!(true));
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
