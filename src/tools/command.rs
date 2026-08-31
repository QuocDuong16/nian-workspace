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
//!   accepted when shell execution is allowed for the workspace.
//!
//! The process machinery lives in the context-based core
//! ([`run_command_for_context`]) shared by both server modes (v0.2 M5): the
//! single-mode wrapper keeps the exact v0.1 CLI permission gates, and the
//! registry-mode wrapper selects a workspace by logical [`WorkspaceId`],
//! enforces that workspace's own `exec`/`shell` capabilities before any
//! process can be spawned, and attaches provenance to the response. The
//! child's working directory always resolves through the selected context's
//! hardened workspace resolver — never a process-global cwd.

use crate::config::{AppState, Limits};
use crate::error::{ToolError, ToolResult};
use crate::process::ProcessTreeGuard;
use crate::tools::discovery::{
    require_registry_exec, require_registry_shell, resolve_registry_workspace,
    with_workspace_provenance,
};
use crate::tools::{decode_lossy, read_capped, CappedBytes};
use crate::workspace::{PathPresentation, WorkspaceContext};
use crate::workspace_id::WorkspaceId;
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

/// Registry-mode `run_command` input: the logical workspace selector plus the
/// unchanged single-mode arguments, flattened into one MCP input schema
/// (no nested `args` object). The selected workspace's own `exec` capability
/// (plus `shell` for shell=true) decides whether anything is spawned.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct RegistryRunCommandArgs {
    /// Logical workspace ID to operate on — exactly one of the IDs reported by list_workspaces.
    #[schemars(
        description = "Logical workspace ID to operate on — exactly one of the IDs reported by list_workspaces, as configured by the operator at startup. Not a path."
    )]
    pub workspace: WorkspaceId,
    #[serde(flatten)]
    pub args: RunCommandArgs,
}

/// Single-workspace mode entry point: the exact v0.1 behavior and permission
/// gates, then the shared core with v0.1 path presentation.
pub(crate) async fn handle(
    state: &AppState,
    args: RunCommandArgs,
) -> ToolResult<serde_json::Value> {
    run_command_for_context(
        state.single_workspace(),
        state.limits(),
        PathPresentation::SingleCompatible,
        args,
    )
    .await
}

/// Registry-mode `run_command`: exact [`WorkspaceId`] lookup, then the
/// selected workspace's own `exec` (and, for shell requests, `shell`)
/// capability enforced **before** any cwd resolution or process creation —
/// a denied workspace never spawns anything. The shared context-based core
/// runs the process; the response carries logical workspace provenance.
pub(crate) async fn registry_run_command(
    state: &AppState,
    args: RegistryRunCommandArgs,
) -> ToolResult<serde_json::Value> {
    let RegistryRunCommandArgs { workspace, args } = args;
    let ctx = resolve_registry_workspace(state, &workspace)?;
    require_registry_exec(&ctx, &workspace)?;
    if args.shell {
        require_registry_shell(&ctx, &workspace)?;
    }
    let value = run_command_for_context(
        &ctx,
        state.limits(),
        PathPresentation::RegistryRelative,
        args,
    )
    .await?;
    Ok(with_workspace_provenance(value, &workspace))
}

/// Context-based core shared by both server modes: identical invocation,
/// timeout, containment, and bounded-output behavior, with the child's cwd
/// resolved through the selected context's hardened resolver. `presentation`
/// decides how server-resolved cwd paths are rendered in client-visible
/// errors; the internal permission gates preserve the exact v0.1 semantics
/// for single mode (registry wrappers enforce their own gates earlier).
pub(crate) async fn run_command_for_context(
    ctx: &WorkspaceContext,
    limits: &Limits,
    presentation: PathPresentation,
    args: RunCommandArgs,
) -> ToolResult<serde_json::Value> {
    let ws = ctx.resolver();

    let cwd = ws.resolve(args.cwd.as_deref())?;
    if !cwd.is_dir() {
        return Err(ToolError::msg(format!(
            "Working directory '{}' does not exist or is not a directory.",
            ws.display_relative_as(&cwd, presentation)
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
        ctx.permissions().require_shell()?;
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
        ctx.permissions().require_exec()?;
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
    // Completed reader results live HERE, in the handler — outside any
    // future a grace deadline may cancel — and are committed the moment a
    // reader finishes. Once a slot is Some, that task is never polled again.
    let mut stdout_result: Option<ReaderResult> = None;
    let mut stderr_result: Option<ReaderResult> = None;

    // Main deadline: the lifetime of the DIRECT command process — nothing
    // else. A descendant that inherits the pipe write-ends must never turn a
    // completed command into a timeout.
    let wait_result = tokio::time::timeout(deadline, child.wait()).await;

    match wait_result {
        Ok(Ok(status)) => {
            let drained = tokio::time::timeout(
                POST_EXIT_DRAIN_GRACE,
                collect_pending_readers(
                    &mut stdout_task,
                    &mut stderr_task,
                    &mut stdout_result,
                    &mut stderr_result,
                ),
            )
            .await;
            if drained.is_err() {
                // Descendants still hold the pipes: terminate the remaining
                // contained tree so their handles close, then allow one final
                // short collection window. This is leftover cleanup — never a
                // command timeout. Results committed during the first window
                // survive this cancellation untouched.
                tracing::debug!(
                    "output pipes still open after child exit; terminating leftover descendants"
                );
                tree.terminate_remaining_descendants();
                let final_drain = tokio::time::timeout(
                    POST_EXIT_DRAIN_GRACE,
                    collect_pending_readers(
                        &mut stdout_task,
                        &mut stderr_task,
                        &mut stdout_result,
                        &mut stderr_result,
                    ),
                )
                .await;
                if final_drain.is_err() {
                    // Escaped pipe-holder: abort only the readers that never
                    // completed and report the honest, incomplete state.
                    if stdout_result.is_none() {
                        stdout_task.abort();
                    }
                    if stderr_result.is_none() {
                        stderr_task.abort();
                    }
                    tracing::warn!("output collection incomplete after normal child exit");
                }
            }

            let (out_capped, out_full) = reader_outcome(stdout_result.take());
            let (err_capped, err_full) = reader_outcome(stderr_result.take());
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
            //      is bounded too; if the grace expires only the readers that
            //      never completed are aborted. Results already committed to
            //      stdout_result/stderr_result survive this cancellation.
            const POST_KILL_GRACE: std::time::Duration = std::time::Duration::from_secs(3);

            let teardown = async {
                tree.terminate_tree(&mut child).await;
                // Reap the direct child so it cannot linger as a zombie.
                let reaped = child.wait().await.is_ok();
                collect_pending_readers(
                    &mut stdout_task,
                    &mut stderr_task,
                    &mut stdout_result,
                    &mut stderr_result,
                )
                .await;
                reaped
            };
            let teardown_result = tokio::time::timeout(POST_KILL_GRACE, teardown).await;

            let reaped = match teardown_result {
                Ok(reaped) => reaped,
                Err(_) => {
                    // Grace expired: abort only the readers that never
                    // completed; committed results are kept.
                    if stdout_result.is_none() {
                        stdout_task.abort();
                    }
                    if stderr_result.is_none() {
                        stderr_task.abort();
                    }
                    tracing::warn!(timeout_secs, "post-kill cleanup exceeded its grace period");
                    false
                }
            };
            let (remaining_out, out_full) = reader_outcome(stdout_result.take());
            let (remaining_err, err_full) = reader_outcome(stderr_result.take());
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

/// Wait for whichever readers are still incomplete, committing each result
/// to the caller-owned slots the moment it becomes ready. The slots live in
/// `handle()`, outside any future a grace deadline may cancel, so a dropped
/// call loses nothing. The `if` guards are the completed-result invariant:
/// a reader whose result is already stored is never polled again.
async fn collect_pending_readers(
    stdout_task: &mut AbortOnDrop<std::io::Result<CappedBytes>>,
    stderr_task: &mut AbortOnDrop<std::io::Result<CappedBytes>>,
    stdout_result: &mut Option<ReaderResult>,
    stderr_result: &mut Option<ReaderResult>,
) {
    while stdout_result.is_none() || stderr_result.is_none() {
        tokio::select! {
            out = &mut *stdout_task, if stdout_result.is_none() => {
                *stdout_result = Some(out);
            }
            err = &mut *stderr_task, if stderr_result.is_none() => {
                *stderr_result = Some(err);
            }
        }
    }
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

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_stdout_survives_pending_stderr_holder() {
        // The shell writes to stdout and exits; the background sleep
        // redirects its own stdout to /dev/null but inherits stderr, so
        // stdout reaches EOF while stderr stays held. The committed stdout
        // result must survive the drain-grace cancellation and the leftover
        // cleanup — and the completed JoinHandle must never be polled again.
        let (_t, state) = exec_state();
        let started = std::time::Instant::now();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "echo marker; sleep 10 >/dev/null & exit 0".into(),
                ]),
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
            out["stdout"].as_str().unwrap().contains("marker"),
            "completed stdout was lost while stderr stayed pending: {out}"
        );
        assert!(
            elapsed < std::time::Duration::from_secs(9),
            "run_command waited for the stderr pipe holder: {elapsed:?}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn completed_stderr_survives_pending_stdout_holder() {
        // Mirror image: stderr reaches EOF, stdout stays held by the
        // background sleep. The completed stderr result must survive.
        let (_t, state) = exec_state();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "echo err-marker >&2; sleep 10 2>/dev/null & exit 0".into(),
                ]),
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
            out["stderr"].as_str().unwrap().contains("err-marker"),
            "completed stderr was lost while stdout stayed pending: {out}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn timeout_preserves_output_from_completed_reader() {
        // A direct child that times out while one reader has already
        // completed: the escaped holder redirects its own stdout away, so
        // stdout EOFs and completes during teardown, while stderr stays
        // held past POST_KILL_GRACE. The committed stdout must be in the
        // response even though collection was cut short.
        let (_t, state) = exec_state();
        let out = run(
            &state,
            RunCommandArgs {
                program: Some("sh".into()),
                args: Some(vec![
                    "-c".into(),
                    "echo marker; setsid sleep 30 1>/dev/null & sleep 600".into(),
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
        assert_eq!(out["exit_code"], serde_json::Value::Null);
        assert!(
            out["stdout"].as_str().unwrap().contains("marker"),
            "stdout completed before teardown was lost when the grace expired: {out}"
        );
        assert_eq!(out["truncated"], json!(true));
        assert!(out["message"].as_str().unwrap().contains("grace period"));
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

    // -- registry-mode wrapper (v0.2 M5) --------------------------------------

    fn rid(s: &str) -> WorkspaceId {
        WorkspaceId::parse(s).expect("fixture workspace id")
    }

    /// Five-workspace registry for the capability matrix: alpha/beta exec-only
    /// with identical relative file layouts but distinct content (cwd
    /// isolation), gamma exec-only (shell denied), delta exec+shell, locked
    /// nothing.
    fn registry_fixture() -> (TempDir, AppState) {
        let tmp = TempDir::new().unwrap();
        let mut config = String::from("version = 1\n\n");
        for (name, caps) in [
            ("alpha", "exec = true\n"),
            ("beta", "exec = true\n"),
            ("gamma", "exec = true\n"),
            ("delta", "exec = true\nallow_shell = true\n"),
            ("locked", ""),
        ] {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(dir.join("sub")).unwrap();
            std::fs::write(dir.join("sub/marker.txt"), format!("{name} marker\n")).unwrap();
            config.push_str(&format!(
                "[workspaces.{name}]\nroot = '{}'\n{caps}\n",
                dir.display()
            ));
        }
        let registry = crate::registry::WorkspaceRegistry::from_toml_str(&config).unwrap();
        (tmp, AppState::from_registry(registry))
    }

    fn reg_args(workspace: &str, args: RunCommandArgs) -> RegistryRunCommandArgs {
        RegistryRunCommandArgs {
            workspace: rid(workspace),
            args,
        }
    }

    fn direct(program: &str, argv: &[&str]) -> RunCommandArgs {
        RunCommandArgs {
            program: Some(program.into()),
            args: Some(argv.iter().map(|s| s.to_string()).collect()),
            shell: false,
            command: None,
            cwd: None,
            timeout_seconds: None,
        }
    }

    fn shell(command_line: &str) -> RunCommandArgs {
        RunCommandArgs {
            program: None,
            args: None,
            shell: true,
            command: Some(command_line.into()),
            cwd: None,
            timeout_seconds: None,
        }
    }

    #[tokio::test]
    async fn registry_command_runs_in_the_selected_workspace_and_carries_provenance() {
        let (_t, state) = registry_fixture();
        // Identical relative requests against two workspaces must each see
        // only their own context's content: the selected WorkspaceContext
        // alone decides the child cwd — no process-global cwd, no mutable
        // current workspace.
        let out = registry_run_command(
            &state,
            reg_args("alpha", direct("cat", &["sub/marker.txt"])),
        )
        .await
        .unwrap();
        assert_eq!(out["workspace"], json!("alpha"));
        assert_eq!(out["exit_code"], json!(0));
        assert_eq!(out["stdout"], "alpha marker\n");

        let out =
            registry_run_command(&state, reg_args("beta", direct("cat", &["sub/marker.txt"])))
                .await
                .unwrap();
        assert_eq!(out["workspace"], json!("beta"));
        assert_eq!(out["stdout"], "beta marker\n");
    }

    #[tokio::test]
    async fn registry_command_denied_exec_never_spawns_a_process() {
        let (tmp, state) = registry_fixture();
        // If a process were spawned despite the denial it would create this
        // marker inside the denied workspace.
        let marker = tmp.path().join("locked").join("spawned-anyway");
        let err = registry_run_command(
            &state,
            reg_args(
                "locked",
                direct("touch", &[marker.to_str().expect("utf-8 tmp path")]),
            ),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Workspace 'locked' does not allow command execution."),
            "{msg}"
        );
        assert!(
            !msg.contains(tmp.path().to_string_lossy().as_ref()),
            "permission error must not expose roots: {msg}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!marker.exists(), "denied command spawned a process anyway");

        // shell=true on the same workspace is stopped by the exec gate that
        // runs first — also without any spawn.
        let err = registry_run_command(&state, reg_args("locked", shell("true")))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("Workspace 'locked' does not allow command execution."),
            "{err}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_shell_denial_never_spawns_a_shell() {
        let (tmp, state) = registry_fixture();
        let marker = tmp.path().join("gamma").join("shell-ran");
        let err = registry_run_command(
            &state,
            reg_args("gamma", shell(&format!("touch {}", marker.display()))),
        )
        .await
        .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Workspace 'gamma' does not allow shell execution."),
            "{msg}"
        );
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        assert!(!marker.exists(), "denied shell spawned a process anyway");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_shell_allowed_when_the_workspace_permits_it() {
        let (_t, state) = registry_fixture();
        let out = registry_run_command(&state, reg_args("delta", shell("echo shell-marker")))
            .await
            .unwrap();
        assert_eq!(out["workspace"], json!("delta"));
        assert_eq!(out["exit_code"], json!(0));
        assert!(
            out["stdout"].as_str().unwrap().contains("shell-marker"),
            "{out}"
        );
    }

    #[tokio::test]
    async fn registry_command_cwd_stays_inside_the_selected_workspace() {
        let (tmp, state) = registry_fixture();
        // A subdirectory cwd resolves inside the selected workspace.
        let out = registry_run_command(
            &state,
            reg_args(
                "alpha",
                RunCommandArgs {
                    cwd: Some("sub".into()),
                    ..direct("cat", &["marker.txt"])
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(out["stdout"], "alpha marker\n");

        // Sibling traversal is rejected by the resolver before any spawn.
        let err = registry_run_command(
            &state,
            reg_args(
                "alpha",
                RunCommandArgs {
                    cwd: Some("../beta".into()),
                    ..direct("cat", &["sub/marker.txt"])
                },
            ),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("outside the configured workspace"),
            "{err}"
        );

        // Absolute paths outside the selected root are rejected too — even
        // ancestors of the workspace.
        let err = registry_run_command(
            &state,
            reg_args(
                "alpha",
                RunCommandArgs {
                    cwd: Some("/".into()),
                    ..direct("true", &[])
                },
            ),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("outside the configured workspace"),
            "{err}"
        );

        // The registered neighbor was never touched by any of it.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("beta/sub/marker.txt")).unwrap(),
            "beta marker\n"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_command_rejects_symlinked_cwd_into_sibling() {
        let (tmp, state) = registry_fixture();
        std::os::unix::fs::symlink("../beta", tmp.path().join("alpha/leak")).unwrap();
        let err = registry_run_command(
            &state,
            reg_args(
                "alpha",
                RunCommandArgs {
                    cwd: Some("leak".into()),
                    ..direct("cat", &["sub/marker.txt"])
                },
            ),
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("outside the configured workspace"),
            "{err}"
        );
        // A registered sibling is not unlocked by being registered: alpha's
        // boundary stays closed.
        assert_eq!(
            std::fs::read_to_string(tmp.path().join("beta/sub/marker.txt")).unwrap(),
            "beta marker\n"
        );
    }

    #[tokio::test]
    async fn registry_unknown_workspace_is_bounded_and_spawns_nothing() {
        let (_t, state) = registry_fixture();
        let err = registry_run_command(&state, reg_args("does-not-exist", direct("echo", &["hi"])))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("Unknown workspace 'does-not-exist'")
                && msg.contains("Use list_workspaces to discover valid workspace IDs"),
            "{msg}"
        );
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn registry_timeout_metadata_and_provenance_are_preserved() {
        let (_t, state) = registry_fixture();
        let out = registry_run_command(
            &state,
            reg_args(
                "alpha",
                RunCommandArgs {
                    timeout_seconds: Some(1),
                    ..direct("sleep", &["30"])
                },
            ),
        )
        .await
        .unwrap();
        assert_eq!(out["workspace"], json!("alpha"));
        assert_eq!(out["timed_out"], json!(true));
        assert_eq!(out["exit_code"], serde_json::Value::Null);
        assert!(out["message"].as_str().unwrap().contains("timeout"));
        assert!(out["duration_ms"].as_u64().is_some());
    }
}
