//! End-to-end CLI tests: v0.1 single-workspace behavior must remain intact,
//! and the v0.2 M1 `--workspace-config` mode rules must be explicit and
//! deterministic.
//!
//! Accepted single-workspace invocations are verified by driving a real MCP
//! stdio session: the handshake is written to stdin, stdin is closed, and
//! the process must serve `initialize` and exit cleanly.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use tempfile::TempDir;

const MCP_HANDSHAKE: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cli-test","version":"0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    "\n",
);

struct RunResult {
    code: Option<i32>,
    stdout: String,
    stderr: String,
}

/// Run the built binary with `args`, feed `stdin_data`, then close stdin.
/// Write failures (EPIPE when the child exits before reading, e.g. after an
/// argument error) are expected and ignored.
fn run(args: &[&str], stdin_data: &str) -> RunResult {
    let mut child = Command::new(env!("CARGO_BIN_EXE_nian-workspace"))
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("failed to spawn nian-workspace");
    let mut stdin = child.stdin.take().expect("piped stdin");
    let _ = stdin.write_all(stdin_data.as_bytes());
    drop(stdin);
    let out = child.wait_with_output().expect("failed to wait for child");
    RunResult {
        code: out.status.code(),
        stdout: String::from_utf8_lossy(&out.stdout).into_owned(),
        stderr: String::from_utf8_lossy(&out.stderr).into_owned(),
    }
}

/// TOML literal strings (single quotes) need no escaping on any platform.
fn workspace_entry(id: &str, root: &Path, capabilities: &str) -> String {
    format!(
        "[workspaces.{id}]\nroot = '{}'\n{capabilities}\n",
        root.display()
    )
}

fn registry_config(version: &str, entries: &[String]) -> String {
    let mut out = format!("{version}\n\n");
    for entry in entries {
        out.push_str(entry);
        out.push('\n');
    }
    out
}

fn write_config(config: &str) -> (TempDir, PathBuf) {
    let dir = TempDir::new().expect("tempdir for config");
    let path = dir.path().join("workspaces.toml");
    std::fs::write(&path, config).expect("write config");
    (dir, path)
}

#[test]
fn single_workspace_invocation_still_serves() {
    let tmp = TempDir::new().unwrap();
    let result = run(&[tmp.path().to_str().unwrap()], MCP_HANDSHAKE);

    assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
    assert!(
        result.stderr.contains("starting nian-workspace over stdio"),
        "stderr: {}",
        result.stderr
    );
    // The initialize response proves an actual MCP session was served.
    assert!(
        result.stdout.contains("serverInfo"),
        "stdout: {}",
        result.stdout
    );
}

#[test]
fn single_workspace_permission_progression_remains_accepted() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().to_str().unwrap();
    let result = run(&[ws, "--write", "--exec", "--allow-shell"], MCP_HANDSHAKE);

    assert_eq!(result.code, Some(0), "stderr: {}", result.stderr);
    assert!(
        result.stderr.contains("write=true"),
        "stderr: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("exec=true"),
        "stderr: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("shell=true"),
        "stderr: {}",
        result.stderr
    );
}

#[test]
fn allow_shell_still_requires_exec_in_single_workspace_mode() {
    let tmp = TempDir::new().unwrap();
    let result = run(
        &[tmp.path().to_str().unwrap(), "--write", "--allow-shell"],
        "",
    );

    assert_ne!(result.code, Some(0), "stderr: {}", result.stderr);
    assert!(
        result.stderr.contains("--allow-shell requires --exec"),
        "stderr: {}",
        result.stderr
    );
}

#[test]
fn registry_mode_loads_and_reports_transitional_limitation() {
    let tmp = TempDir::new().unwrap();
    let vision = tmp.path().join("nian-vision");
    let home = tmp.path().join("nian-home");
    std::fs::create_dir_all(&vision).unwrap();
    std::fs::create_dir_all(&home).unwrap();
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("nian-vision", &vision, "write = true\nexec = true"),
            workspace_entry("nian-home", &home, ""),
        ],
    );
    let (_cfg_dir, cfg_path) = write_config(&config);

    let result = run(
        &["--workspace-config", cfg_path.to_str().unwrap()],
        MCP_HANDSHAKE,
    );

    // M1 registry mode validates successfully, then stops explicitly before
    // MCP serving instead of inventing a default workspace.
    assert_ne!(result.code, Some(0), "stdout: {}", result.stdout);
    assert!(
        result.stderr.contains("workspace registered"),
        "stderr: {}",
        result.stderr
    );
    // The quoted field form cannot collide with the root path, which also
    // contains the id.
    assert!(
        result.stderr.contains(r#"workspace_id="nian-vision""#),
        "stderr: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains(r#"workspace_id="nian-home""#),
        "stderr: {}",
        result.stderr
    );
    assert!(
        result.stderr.contains("write=true"),
        "per-workspace permission should be logged: {}",
        result.stderr
    );
    assert!(
        result
            .stderr
            .contains("not yet available for MCP tool serving"),
        "stderr: {}",
        result.stderr
    );
    // No MCP session may start in registry mode.
    assert!(
        !result.stdout.contains("serverInfo"),
        "registry mode must not serve MCP in M1; stdout: {}",
        result.stdout
    );
}

#[test]
fn registry_mode_rejects_invalid_configuration_before_serving() {
    // A nested-root configuration must fail at startup with a clear error.
    let tmp = TempDir::new().unwrap();
    let parent = tmp.path().join("parent");
    let child = tmp.path().join("parent").join("project");
    std::fs::create_dir_all(&child).unwrap();
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("parent", &parent, "write = true"),
            workspace_entry("child", &child, ""),
        ],
    );
    let (_cfg_dir, cfg_path) = write_config(&config);

    let result = run(&["--workspace-config", cfg_path.to_str().unwrap()], "");

    assert_ne!(result.code, Some(0), "stderr: {}", result.stderr);
    assert!(
        result.stderr.contains("is nested inside"),
        "stderr: {}",
        result.stderr
    );
}

#[test]
fn positional_workspace_and_workspace_config_are_mutually_exclusive() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let config = registry_config("version = 1", &[workspace_entry("ws", &ws, "")]);
    let (_cfg_dir, cfg_path) = write_config(&config);

    let result = run(
        &[
            tmp.path().to_str().unwrap(),
            "--workspace-config",
            cfg_path.to_str().unwrap(),
        ],
        "",
    );

    assert_ne!(result.code, Some(0), "stderr: {}", result.stderr);
    assert!(
        result
            .stderr
            .contains("cannot be combined with a positional WORKSPACE"),
        "stderr: {}",
        result.stderr
    );
}

#[test]
fn permission_flags_are_rejected_with_workspace_config() {
    let tmp = TempDir::new().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let config = registry_config("version = 1", &[workspace_entry("ws", &ws, "write = true")]);
    let (_cfg_dir, cfg_path) = write_config(&config);
    let cfg_arg = cfg_path.to_str().unwrap();

    for flag in ["--write", "--exec", "--allow-shell"] {
        let result = run(&["--workspace-config", cfg_arg, flag], "");
        assert_ne!(result.code, Some(0), "flag {flag}: {}", result.stderr);
        let expected = format!("{flag} cannot be combined with --workspace-config");
        assert!(
            result.stderr.contains(&expected),
            "flag {flag} should be rejected explicitly: {}",
            result.stderr
        );
    }
}

#[test]
fn missing_workspace_config_file_is_rejected() {
    let result = run(
        &["--workspace-config", "/nonexistent/path/workspaces.toml"],
        "",
    );
    assert_ne!(result.code, Some(0), "stderr: {}", result.stderr);
    assert!(
        result.stderr.contains("Failed to read workspace config"),
        "stderr: {}",
        result.stderr
    );
}
