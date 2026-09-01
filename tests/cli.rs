//! End-to-end CLI tests (the M6 final compatibility/integration suite):
//! the v0.1 single-workspace behavior must remain intact, and the v0.2
//! `--workspace-config` registry mode must expose exactly its mode-specific
//! MCP tool surface — discovery plus the full capability set (read, Git,
//! patching, and command execution), every tool selecting one workspace by
//! logical WorkspaceId, mutation and execution gated by that workspace's own
//! configured capabilities.
//!
//! Both modes are verified by driving a real MCP stdio session:
//! initialize, tools/list, and tools/call exchanges against the actual
//! process, asserting on the JSON-RPC responses clients really receive.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use tempfile::TempDir;

const MCP_HANDSHAKE: &str = concat!(
    r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"cli-test","version":"0"}}}"#,
    "\n",
    r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
    "\n",
);

mod common;

use common::{REGISTRY_MODE_TOOLS, SINGLE_MODE_TOOLS};

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

// ---------------------------------------------------------------------------
// M2 session driver: sequential request/response over a real stdio session
// ---------------------------------------------------------------------------

/// Run a git command in `dir`, asserting success. Test fixtures only:
/// identity is pinned per-invocation (per-child env), never process-wide.
fn git(dir: &Path, args: &[&str]) {
    let out = std::process::Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_AUTHOR_NAME", "t")
        .env("GIT_AUTHOR_EMAIL", "t@example.com")
        .env("GIT_COMMITTER_NAME", "t")
        .env("GIT_COMMITTER_EMAIL", "t@example.com")
        .output()
        .expect("git should be installed for tests");
    assert!(
        out.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&out.stderr)
    );
}

/// Pin repo-locally the behaviors a host ~/.gitconfig could otherwise flip
/// and break fixture assertions with (repo-local config outranks global).
fn pin_repo_config(dir: &Path) {
    git(dir, &["config", "status.showUntrackedFiles", "normal"]);
    git(dir, &["config", "diff.noprefix", "false"]);
}

/// A live MCP stdio session against the real binary. Requests are sent one
/// at a time and each response is waited for, so every later exchange in a
/// test also proves the server survived everything before it.
struct McpSession {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<std::process::ChildStdout>,
}

impl McpSession {
    fn start(args: &[&str]) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_nian-workspace"))
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("failed to spawn nian-workspace");
        let stdin = child.stdin.take().expect("piped stdin");
        let stdout = BufReader::new(child.stdout.take().expect("piped stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn send(&mut self, value: &serde_json::Value) {
        self.stdin
            .write_all(format!("{value}\n").as_bytes())
            .expect("write request to server stdin");
        self.stdin.flush().expect("flush request");
    }

    /// Read one JSON-RPC message from the server, blocking on one line.
    fn recv(&mut self) -> serde_json::Value {
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("read server stdout");
        assert!(
            !line.is_empty(),
            "server closed stdout before answering (crashed or exited early)"
        );
        serde_json::from_str(&line).expect("server wrote a valid JSON-RPC line")
    }

    /// Send a request and return the response with the matching id,
    /// skipping anything else the server sends in between.
    fn request(&mut self, id: u64, method: &str, params: serde_json::Value) -> serde_json::Value {
        self.send(&serde_json::json!({
            "jsonrpc": "2.0", "id": id, "method": method, "params": params
        }));
        loop {
            let message = self.recv();
            if message.get("id").and_then(|v| v.as_u64()) == Some(id) {
                return message;
            }
        }
    }

    /// initialize + initialized notification; asserts handshake success.
    fn initialize(&mut self) -> serde_json::Value {
        let response = self.request(
            1,
            "initialize",
            serde_json::json!({
                "protocolVersion": "2025-06-18",
                "capabilities": {},
                "clientInfo": {"name": "cli-test", "version": "0"}
            }),
        );
        assert!(
            response.get("result").is_some(),
            "initialize failed: {response}"
        );
        self.send(&serde_json::json!({"jsonrpc": "2.0", "method": "notifications/initialized"}));
        response
    }

    /// tools/list; returns the (rmcp-sorted) advertised tool names.
    fn list_tools(&mut self) -> Vec<String> {
        let response = self.request(2, "tools/list", serde_json::json!({}));
        response["result"]["tools"]
            .as_array()
            .expect("tools array in tools/list result")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name").to_string())
            .collect()
    }

    /// The advertised input schema of one tool from tools/list.
    fn tool_schema(&mut self, id: u64, name: &str) -> serde_json::Value {
        let response = self.request(id, "tools/list", serde_json::json!({}));
        response["result"]["tools"]
            .as_array()
            .expect("tools array in tools/list result")
            .iter()
            .find(|tool| tool["name"] == name)
            .unwrap_or_else(|| panic!("tool '{name}' not advertised"))["inputSchema"]
            .clone()
    }

    /// tools/call; returns the raw JSON-RPC response (result or error).
    fn call_tool(
        &mut self,
        id: u64,
        name: &str,
        arguments: serde_json::Value,
    ) -> serde_json::Value {
        self.request(
            id,
            "tools/call",
            serde_json::json!({"name": name, "arguments": arguments}),
        )
    }

    /// Close stdin and wait for a clean exit; returns (code, stderr).
    fn shutdown(self) -> (Option<i32>, String) {
        drop(self.stdin);
        drop(self.stdout);
        let output = self.child.wait_with_output().expect("wait for server exit");
        (
            output.status.code(),
            String::from_utf8_lossy(&output.stderr).into_owned(),
        )
    }
}

/// A tool result that must be a tool-level failure (isError), returning the
/// error text shown to the client.
fn expect_tool_error(response: &serde_json::Value) -> String {
    assert!(
        response.get("error").is_none(),
        "expected a tool result, got protocol error: {response}"
    );
    let result = &response["result"];
    assert_eq!(
        result["isError"], true,
        "expected isError=true tool result: {response}"
    );
    result["content"][0]["text"]
        .as_str()
        .expect("error text content")
        .to_string()
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

/// Two-workspace registry config used by the registry tests.
fn two_workspace_config(tmp: &TempDir) -> (TempDir, PathBuf) {
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
    write_config(&config)
}

/// Two-workspace registry (alpha/beta) with same-named but distinct files,
/// so any cross-workspace bleed shows up as wrong content or structure.
fn cross_workspace_registry() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let alpha = tmp.path().join("alpha");
    let beta = tmp.path().join("beta");
    std::fs::create_dir_all(alpha.join("src")).unwrap();
    std::fs::create_dir_all(beta.join("src")).unwrap();
    std::fs::write(alpha.join("shared.txt"), b"FROM_ALPHA\n").unwrap();
    std::fs::write(beta.join("shared.txt"), b"FROM_BETA\n").unwrap();
    std::fs::write(alpha.join("src/a.rs"), b"UNIQUE_ALPHA_TOKEN\n").unwrap();
    std::fs::write(beta.join("src/b.rs"), b"UNIQUE_BETA_TOKEN\n").unwrap();
    std::fs::write(beta.join("src/only_beta.rs"), b"ONLY_BETA\n").unwrap();
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("alpha", &alpha, ""),
            workspace_entry("beta", &beta, ""),
        ],
    );
    let path = tmp.path().join("workspaces.toml");
    std::fs::write(&path, config).unwrap();
    (tmp, path)
}

/// alpha/beta registry where each workspace is its own committed git
/// repository with distinctly named files, so any cross-workspace bleed in
/// Git output is unambiguous.
fn independent_repos_registry() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let alpha = tmp.path().join("alpha");
    let beta = tmp.path().join("beta");
    for (dir, name) in [(&alpha, "alpha"), (&beta, "beta")] {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "--initial-branch=main"]);
        pin_repo_config(dir);
        std::fs::write(dir.join(format!("{name}_tracked.txt")), "original\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "initial"]);
    }
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("alpha", &alpha, ""),
            workspace_entry("beta", &beta, ""),
        ],
    );
    let path = tmp.path().join("workspaces.toml");
    std::fs::write(&path, config).unwrap();
    (tmp, path)
}

/// One git repository containing both registered workspaces as
/// subdirectories — the parent-repository isolation fixture (v0.2 M4).
fn parent_repo_registry() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let repo = tmp.path().join("repo");
    let alpha = repo.join("alpha");
    let beta = repo.join("beta");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    git(&repo, &["init", "--initial-branch=main"]);
    pin_repo_config(&repo);
    std::fs::write(alpha.join("alpha.txt"), "original\n").unwrap();
    std::fs::write(beta.join("beta.txt"), "original\n").unwrap();
    git(&repo, &["add", "."]);
    git(&repo, &["commit", "-m", "initial"]);
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("alpha", &alpha, ""),
            workspace_entry("beta", &beta, ""),
        ],
    );
    let path = tmp.path().join("workspaces.toml");
    std::fs::write(&path, config).unwrap();
    (tmp, path)
}

/// One committed git repository (alpha) plus one plain directory (plain) —
/// for Git error-presentation and health-after-failure tests.
fn git_and_plain_registry() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let alpha = tmp.path().join("alpha");
    let plain = tmp.path().join("plain");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&plain).unwrap();
    git(&alpha, &["init", "--initial-branch=main"]);
    pin_repo_config(&alpha);
    std::fs::write(alpha.join("alpha_tracked.txt"), "original\n").unwrap();
    git(&alpha, &["add", "."]);
    git(&alpha, &["commit", "-m", "initial"]);
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("alpha", &alpha, ""),
            workspace_entry("plain", &plain, ""),
        ],
    );
    let path = tmp.path().join("workspaces.toml");
    std::fs::write(&path, config).unwrap();
    (tmp, path)
}

/// alpha/beta registry, each workspace a committed git repo with the
/// same-named file but different content, both write=true; locked is a plain
/// read-only workspace — for apply_patch isolation E2E (v0.2 M5).
fn writable_repos_registry() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let alpha = tmp.path().join("alpha");
    let beta = tmp.path().join("beta");
    let locked = tmp.path().join("locked");
    for (dir, content) in [
        (&alpha, "ALPHA_ORIGINAL\n"),
        (&beta, "BETA_ORIGINAL\n"),
        (&locked, "LOCKED_ORIGINAL\n"),
    ] {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join("shared.txt"), content).unwrap();
    }
    for dir in [&alpha, &beta] {
        git(dir, &["init", "--initial-branch=main"]);
        pin_repo_config(dir);
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "initial"]);
    }
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("alpha", &alpha, "write = true"),
            workspace_entry("beta", &beta, "write = true"),
            workspace_entry("locked", &locked, ""),
        ],
    );
    let path = tmp.path().join("workspaces.toml");
    std::fs::write(&path, config).unwrap();
    (tmp, path)
}

/// Capability-matrix registry (v0.2 M5): alpha/beta are exec-only git repos
/// with distinct untracked markers (cwd isolation via git status), gamma is
/// exec-only (shell denied), delta exec+shell, locked nothing.
fn exec_matrix_registry() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();
    let alpha = tmp.path().join("alpha");
    let beta = tmp.path().join("beta");
    let gamma = tmp.path().join("gamma");
    let delta = tmp.path().join("delta");
    let locked = tmp.path().join("locked");
    for dir in [&alpha, &beta] {
        std::fs::create_dir_all(dir).unwrap();
        git(dir, &["init", "--initial-branch=main"]);
        pin_repo_config(dir);
        std::fs::write(dir.join("tracked.txt"), "original\n").unwrap();
        git(dir, &["add", "."]);
        git(dir, &["commit", "-m", "initial"]);
    }
    std::fs::write(alpha.join("untracked_alpha.txt"), "ALPHA_MARKER\n").unwrap();
    std::fs::write(beta.join("untracked_beta.txt"), "BETA_MARKER\n").unwrap();
    for dir in [&gamma, &delta, &locked] {
        std::fs::create_dir_all(dir).unwrap();
    }
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("alpha", &alpha, "exec = true"),
            workspace_entry("beta", &beta, "exec = true"),
            workspace_entry("gamma", &gamma, "exec = true"),
            workspace_entry("delta", &delta, "exec = true\nallow_shell = true"),
            workspace_entry("locked", &locked, ""),
        ],
    );
    let path = tmp.path().join("workspaces.toml");
    std::fs::write(&path, config).unwrap();
    (tmp, path)
}

/// Capability-matrix registry (v0.2 M6): four workspaces covering every
/// permission quadrant for end-to-end matrix verification —
///
///   readonly: no capabilities (a committed git repo, so the read-only Git
///             tools keep working);
///   writer:   write = true only;
///   executor: exec = true only (untracked marker for the child-cwd proof);
///   full:     write = true, exec = true, allow_shell = true.
///
/// Every quadrant is exercised through real MCP tools/call in one session,
/// so a capability granted to one workspace can never promote another.
fn capability_matrix_registry() -> (TempDir, PathBuf) {
    let tmp = TempDir::new().unwrap();

    let readonly = tmp.path().join("readonly");
    std::fs::create_dir_all(&readonly).unwrap();
    std::fs::write(readonly.join("const.txt"), "READONLY\n").unwrap();
    git(&readonly, &["init", "--initial-branch=main"]);
    pin_repo_config(&readonly);
    git(&readonly, &["add", "."]);
    git(&readonly, &["commit", "-m", "initial"]);

    let writer = tmp.path().join("writer");
    std::fs::create_dir_all(&writer).unwrap();
    std::fs::write(writer.join("patch.txt"), "WRITER ORIGINAL\n").unwrap();

    let executor = tmp.path().join("executor");
    std::fs::create_dir_all(&executor).unwrap();
    git(&executor, &["init", "--initial-branch=main"]);
    pin_repo_config(&executor);
    std::fs::write(executor.join("executor-marker.txt"), "EXECUTOR_MARKER\n").unwrap();

    let full = tmp.path().join("full");
    std::fs::create_dir_all(&full).unwrap();
    std::fs::write(full.join("patch.txt"), "FULL ORIGINAL\n").unwrap();

    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("readonly", &readonly, ""),
            workspace_entry("writer", &writer, "write = true"),
            workspace_entry("executor", &executor, "exec = true"),
            workspace_entry(
                "full",
                &full,
                "write = true\nexec = true\nallow_shell = true",
            ),
        ],
    );
    let path = tmp.path().join("workspaces.toml");
    std::fs::write(&path, config).unwrap();
    (tmp, path)
}

#[test]
fn single_mode_tools_list_is_exactly_the_v01_surface() {
    let tmp = TempDir::new().unwrap();
    let mut session = McpSession::start(&[tmp.path().to_str().unwrap()]);

    let init = session.initialize();
    assert!(
        init["result"]["serverInfo"]["name"] == "nian-workspace",
        "{init}"
    );

    // The advertised surface must be exactly the v0.1 tools — no
    // list_workspaces, nothing added, nothing removed.
    let tools = session.list_tools();
    let mut names = tools.clone();
    names.sort();
    assert_eq!(names, SINGLE_MODE_TOOLS, "single-mode tools/list changed");

    // workspace_info keeps its v0.1 no-argument input shape.
    let response = session.call_tool(3, "workspace_info", serde_json::json!({}));
    assert!(
        response.get("error").is_none() && response["result"]["isError"] != true,
        "workspace_info with no arguments must still work: {response}"
    );
    let content = &response["result"]["structuredContent"];
    assert!(
        content.get("root").is_some(),
        "v0.1 response shape: {content}"
    );
    assert!(
        content.get("name").is_some(),
        "v0.1 response shape: {content}"
    );

    // M3–M5 must not have grown a workspace selector on any single-mode
    // tool: their advertised schemas keep the exact v0.1 shape.
    for (id, tool) in (10..).zip([
        "list_files",
        "read_file",
        "search",
        "git_status",
        "git_diff",
        "apply_patch",
        "run_command",
    ]) {
        let schema = session.tool_schema(id, tool);
        let required = schema["required"].as_array().cloned().unwrap_or_default();
        assert!(
            !required.iter().any(|r| r == "workspace"),
            "single-mode {tool} must not require workspace: {schema}"
        );
        assert!(
            schema["properties"].get("workspace").is_none(),
            "single-mode {tool} must not have a workspace property: {schema}"
        );
        // Shared argument schemas stay mode-neutral (M6): the CLI-flag
        // wording belongs to single-mode *tool* descriptions, never to the
        // input schemas both modes share.
        if tool == "apply_patch" || tool == "run_command" {
            let raw = schema.to_string();
            assert!(
                !raw.contains("--write")
                    && !raw.contains("--exec")
                    && !raw.contains("--allow-shell"),
                "single-mode {tool} schema must be mode-neutral: {schema}"
            );
        }
    }
    // And the v0.1 calls still succeed without any selector.
    let response = session.call_tool(20, "list_files", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    let response = session.call_tool(21, "search", serde_json::json!({ "query": "anything" }));
    assert!(response.get("error").is_none(), "{response}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(
        stderr.contains("starting nian-workspace over stdio"),
        "{stderr}"
    );
}

#[test]
fn single_mode_root_paths_follow_the_v01_contract() {
    let tmp = TempDir::new().unwrap();
    std::fs::write(tmp.path().join("note.txt"), b"v01 marker\n").unwrap();
    // The server canonicalizes the positional root exactly like this, so
    // this is the byte-exact string v0.1 clients saw before M3.
    let root = tmp
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut session = McpSession::start(&[tmp.path().to_str().unwrap()]);
    session.initialize();

    // list_files at the workspace root: `root` is the canonical absolute
    // root, not "." — the pre-M3 v0.1 presentation contract.
    let response = session.call_tool(3, "list_files", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["root"], serde_json::json!(root), "{content}");

    // search without `path`: `path` is the canonical absolute root too.
    let response = session.call_tool(4, "search", serde_json::json!({ "query": "v01 marker" }));
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["path"], serde_json::json!(root), "{content}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn single_mode_git_tools_keep_the_v01_contract() {
    // A real repository: both Git tools work with the exact v0.1 input
    // format, and neither response grows a workspace provenance field.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    git(root, &["init", "--initial-branch=main"]);
    pin_repo_config(root);
    std::fs::write(root.join("tracked.txt"), "line one\n").unwrap();
    git(root, &["add", "."]);
    git(root, &["commit", "-m", "initial"]);
    std::fs::write(root.join("tracked.txt"), "line CHANGED\n").unwrap();

    let mut session = McpSession::start(&[tmp.path().to_str().unwrap()]);
    session.initialize();

    let response = session.call_tool(3, "git_status", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert!(
        content["output"]
            .as_str()
            .unwrap()
            .contains(" M tracked.txt"),
        "{content}"
    );
    assert!(
        content.get("workspace").is_none(),
        "single-mode git_status must not carry provenance: {content}"
    );

    let response = session.call_tool(4, "git_diff", serde_json::json!({ "staged": false }));
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert!(
        content["diff"].as_str().unwrap().contains("+line CHANGED"),
        "{content}"
    );
    assert!(
        content.get("workspace").is_none(),
        "single-mode git_diff must not carry provenance: {content}"
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn single_mode_git_error_keeps_the_v01_absolute_root_presentation() {
    // Outside a repository, the v0.1 error text names the canonical
    // absolute workspace root — that presentation must not be changed
    // merely to suit registry mode.
    let tmp = TempDir::new().unwrap();
    let root = tmp
        .path()
        .canonicalize()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    let mut session = McpSession::start(&[tmp.path().to_str().unwrap()]);
    session.initialize();

    let response = session.call_tool(3, "git_status", serde_json::json!({}));
    let text = expect_tool_error(&response);
    assert!(
        text.contains("does not appear to be inside a Git working tree"),
        "{text}"
    );
    assert!(
        text.contains(&root),
        "v0.1 presentation keeps the canonical absolute root: {text}"
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn single_mode_mutation_tools_keep_the_v01_contract() {
    // apply_patch and run_command keep their exact v0.1 input format, flags,
    // and response shapes — no workspace selector, no provenance field.
    let tmp = TempDir::new().unwrap();
    let root = tmp.path();
    std::fs::write(root.join("patched.txt"), "one\ntwo\n").unwrap();

    let mut session = McpSession::start(&[tmp.path().to_str().unwrap(), "--write", "--exec"]);
    session.initialize();

    let response = session.call_tool(
        3,
        "apply_patch",
        serde_json::json!({
            "patch": "--- patched.txt\n+++ patched.txt\n@@ -1,2 +1,2 @@\n-one\n+ONE\n-two\n+TWO\n"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["changed_files"][0],
        serde_json::json!("patched.txt"),
        "{content}"
    );
    assert!(
        content.get("workspace").is_none(),
        "single-mode apply_patch must not carry provenance: {content}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("patched.txt")).unwrap(),
        "ONE\nTWO\n"
    );

    let response = session.call_tool(
        4,
        "run_command",
        serde_json::json!({ "program": "git", "args": ["--version"] }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["exit_code"], serde_json::json!(0));
    assert!(
        content["stdout"]
            .as_str()
            .unwrap()
            .starts_with("git version"),
        "{content}"
    );
    assert!(
        content.get("workspace").is_none(),
        "single-mode run_command must not carry provenance: {content}"
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn single_mode_read_tools_never_carry_registry_provenance() {
    // Single-mode read tools must not gain registry provenance either —
    // only the shared implementations now support registry mode; the v0.1
    // response contracts stay untouched.
    let tmp2 = TempDir::new().unwrap();
    std::fs::write(tmp2.path().join("note.txt"), b"plain\n").unwrap();
    let mut session = McpSession::start(&[tmp2.path().to_str().unwrap()]);
    session.initialize();
    for (id, (tool, args)) in (3u64..).zip([
        ("workspace_info", serde_json::json!({})),
        ("list_files", serde_json::json!({ "path": "." })),
        ("read_file", serde_json::json!({ "path": "note.txt" })),
        ("search", serde_json::json!({ "query": "plain" })),
    ]) {
        let response = session.call_tool(id, tool, args);
        assert!(response.get("error").is_none(), "{tool}: {response}");
        let content = &response["result"]["structuredContent"];
        assert!(
            content.get("workspace").is_none(),
            "single-mode {tool} must not carry provenance: {content}"
        );
        assert!(
            response["result"].get("workspace").is_none(),
            "single-mode {tool} result must not carry a workspace field: {response}"
        );
    }
    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn registry_root_requests_present_as_dot_without_disclosure() {
    let (tmp, cfg) = cross_workspace_registry();
    let alpha_str = tmp.path().join("alpha").to_string_lossy().into_owned();
    let alpha_trimmed = alpha_str.trim_start_matches('/').to_string();
    let cfg_str = cfg.to_string_lossy().into_owned();
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // list_files at the selected root: root == "." — never the absolute
    // alpha root, anywhere in the response.
    let response = session.call_tool(3, "list_files", serde_json::json!({ "workspace": "alpha" }));
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["root"], serde_json::json!("."), "{content}");
    let raw = response.to_string();
    assert!(
        !raw.contains(alpha_str.as_str()) && !raw.contains(alpha_trimmed.as_str()),
        "list_files must not expose the canonical root: {raw}"
    );

    // search at the selected root: path == ".".
    let response = session.call_tool(
        4,
        "search",
        serde_json::json!({ "workspace": "alpha", "query": "UNIQUE_" }),
    );
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["path"], serde_json::json!("."), "{content}");
    let raw = response.to_string();
    assert!(
        !raw.contains(alpha_str.as_str()) && !raw.contains(alpha_trimmed.as_str()),
        "search must not expose the canonical root: {raw}"
    );

    // read_file at the selected root: the normal "is a directory" failure,
    // with neither the absolute root nor the registry config path in the
    // error text.
    let response = session.call_tool(
        5,
        "read_file",
        serde_json::json!({ "workspace": "alpha", "path": "." }),
    );
    let text = expect_tool_error(&response);
    assert!(text.contains("is a directory"), "{text}");
    assert!(
        !text.contains(alpha_str.as_str()) && !text.contains(alpha_trimmed.as_str()),
        "read_file error must not expose the canonical root: {text}"
    );
    assert!(
        !text.contains(cfg_str.as_str()),
        "error must not expose the registry config path: {text}"
    );

    // The server remains fully usable afterwards.
    let response = session.call_tool(6, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    let response = session.call_tool(
        7,
        "read_file",
        serde_json::json!({ "workspace": "alpha", "path": "shared.txt" }),
    );
    assert_eq!(
        response["result"]["structuredContent"]["lines"][0],
        serde_json::json!("1: FROM_ALPHA")
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_mode_serves_mcp_after_valid_config() {
    let tmp = TempDir::new().unwrap();
    let (_cfg_dir, cfg_path) = two_workspace_config(&tmp);
    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);

    // The M1 transitional stop is gone: a valid registry now starts MCP.
    let init = session.initialize();
    assert!(
        init["result"]["serverInfo"]["name"] == "nian-workspace",
        "{init}"
    );

    // Serving really works, not just the handshake.
    let tools = session.list_tools();
    assert!(tools.contains(&"list_workspaces".to_string()), "{tools:?}");
    let response = session.call_tool(3, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(stderr.contains("workspace registered"), "{stderr}");
    assert!(stderr.contains(r#"workspace_id="nian-vision""#), "{stderr}");
    assert!(stderr.contains(r#"workspace_id="nian-home""#), "{stderr}");
    assert!(
        !stderr.contains("not yet available for MCP tool serving"),
        "M1 transitional stop must be gone: {stderr}"
    );
}

#[test]
fn registry_mode_advertises_the_full_capability_set() {
    let tmp = TempDir::new().unwrap();
    let (_cfg_dir, cfg_path) = two_workspace_config(&tmp);
    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);
    session.initialize();

    let tools = session.list_tools();
    let mut names = tools.clone();
    names.sort();
    assert_eq!(names, REGISTRY_MODE_TOOLS);

    // Every selector tool requires the logical workspace id; list_workspaces
    // takes no arguments. The selector schema uses the WorkspaceId grammar
    // (inlined or via $defs/$ref, as schemars renders it).
    for (id, tool, also_required) in [
        (3u64, "list_files", vec![]),
        (4, "read_file", vec!["path"]),
        (5, "search", vec!["query"]),
        (6, "workspace_info", vec![]),
        (7, "git_status", vec![]),
        (8, "git_diff", vec![]),
        (9, "apply_patch", vec!["patch"]),
        (10, "run_command", vec![]),
    ] {
        let schema = session.tool_schema(id, tool);
        let required: Vec<&str> = schema["required"]
            .as_array()
            .expect("required array")
            .iter()
            .map(|v| v.as_str().expect("required entry string"))
            .collect();
        assert!(
            required.contains(&"workspace"),
            "registry {tool} must require 'workspace': {schema}"
        );
        for extra in also_required {
            assert!(
                required.contains(&extra),
                "registry {tool} must still require '{extra}': {schema}"
            );
        }
        // The schema must stay flat: workspace sits beside the existing
        // fields, never wrapped in a nested arguments object. (A tool may
        // legitimately own a top-level "args" field — run_command's verbatim
        // program arguments do — so nesting is detected by type, not name.)
        assert!(
            schema["properties"].get("workspace").is_some(),
            "registry {tool} schema must have a flat workspace property: {schema}"
        );
        let nests_arguments = schema["properties"]
            .get("args")
            .and_then(|a| a.get("type"))
            .and_then(|t| match t {
                serde_json::Value::String(s) => Some(s == "object"),
                serde_json::Value::Array(items) => {
                    Some(items.iter().any(|t| t.as_str() == Some("object")))
                }
                _ => None,
            })
            .unwrap_or(false);
        assert!(
            !nests_arguments,
            "registry {tool} schema must not nest arguments: {schema}"
        );
        let pattern = workspace_selector_pattern(&schema)
            .expect("workspace selector must use the WorkspaceId pattern");
        assert_eq!(pattern, "^[a-z0-9][a-z0-9._-]{0,63}$");
        // Shared argument schemas stay mode-neutral (M6): permissions come
        // from the selected workspace's configuration, so the schemas must
        // not carry single-mode CLI-flag wording.
        if tool == "apply_patch" || tool == "run_command" {
            let raw = schema.to_string();
            assert!(
                !raw.contains("--write")
                    && !raw.contains("--exec")
                    && !raw.contains("--allow-shell"),
                "registry {tool} schema must be mode-neutral: {schema}"
            );
        }
    }

    // git_diff keeps every existing GitDiffArgs field available with its
    // previous optional semantics.
    let diff_schema = session.tool_schema(11, "git_diff");
    let diff_required: Vec<&str> = diff_schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert!(
        !diff_required.iter().any(|r| *r == "staged" || *r == "path"),
        "git_diff staged/path must stay optional: {diff_schema}"
    );
    assert!(
        diff_schema["properties"].get("staged").is_some()
            && diff_schema["properties"].get("path").is_some(),
        "git_diff must keep the staged and path properties: {diff_schema}"
    );

    // run_command keeps every existing argument available at the top level,
    // still optional, with no drift in required fields.
    let cmd_schema = session.tool_schema(12, "run_command");
    let cmd_required: Vec<&str> = cmd_schema["required"]
        .as_array()
        .map(|r| r.iter().filter_map(|v| v.as_str()).collect())
        .unwrap_or_default();
    assert_eq!(
        cmd_required,
        ["workspace"],
        "run_command must require only workspace: {cmd_schema}"
    );
    for field in [
        "program",
        "args",
        "shell",
        "command",
        "cwd",
        "timeout_seconds",
    ] {
        assert!(
            cmd_schema["properties"].get(field).is_some(),
            "run_command must keep '{field}' available: {cmd_schema}"
        );
        assert!(
            !cmd_required.contains(&field),
            "run_command '{field}' must stay optional: {cmd_schema}"
        );
    }
    // The top-level `args` field is run_command's own verbatim program
    // arguments (an array), not a nested arguments wrapper.
    assert_eq!(
        cmd_schema["properties"]["args"]["type"],
        serde_json::json!(["array", "null"]),
        "run_command 'args' must remain the program-argument array: {cmd_schema}"
    );

    let list_schema = session.tool_schema(13, "list_workspaces");
    assert!(
        list_schema
            .get("required")
            .and_then(|r| r.as_array())
            .is_none_or(|r| r.is_empty()),
        "list_workspaces must require no arguments: {list_schema}"
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

/// Resolve the JSON-schema pattern of a tool's `workspace` property,
/// following a `$defs/$ref` indirection when schemars emits one.
fn workspace_selector_pattern(tool_schema: &serde_json::Value) -> Option<String> {
    let property = &tool_schema["properties"]["workspace"];
    if let Some(pattern) = property.get("pattern").and_then(|p| p.as_str()) {
        return Some(pattern.to_string());
    }
    let reference = property.get("$ref").and_then(|r| r.as_str())?;
    let name = reference.rsplit('/').next()?;
    tool_schema["$defs"][name]["pattern"]
        .as_str()
        .map(|p| p.to_string())
}

#[test]
fn registry_list_workspaces_is_sorted_without_roots() {
    let tmp = TempDir::new().unwrap();
    let zeta = tmp.path().join("zeta-root");
    let alpha = tmp.path().join("alpha-root");
    let middle = tmp.path().join("middle-root");
    for dir in [&zeta, &alpha, &middle] {
        std::fs::create_dir_all(dir).unwrap();
    }
    // Declared deliberately out of WorkspaceId order.
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("zeta", &zeta, "write = true\nexec = true"),
            workspace_entry("alpha", &alpha, ""),
            workspace_entry("middle", &middle, "write = true"),
        ],
    );
    let (_cfg_dir, cfg_path) = write_config(&config);

    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);
    session.initialize();

    let response = session.call_tool(3, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    let workspaces = response["result"]["structuredContent"]["workspaces"]
        .as_array()
        .expect("workspaces array")
        .to_vec();

    let ids: Vec<&str> = workspaces
        .iter()
        .map(|w| w["id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        ids,
        ["alpha", "middle", "zeta"],
        "deterministic WorkspaceId order"
    );

    let permissions_of = |id: &str| -> serde_json::Value {
        workspaces
            .iter()
            .find(|w| w["id"] == id)
            .expect("declared workspace")["permissions"]
            .clone()
    };
    assert_eq!(permissions_of("alpha")["write"], serde_json::json!(false));
    assert_eq!(permissions_of("alpha")["read"], serde_json::json!(true));
    assert_eq!(permissions_of("middle")["write"], serde_json::json!(true));
    assert_eq!(permissions_of("middle")["exec"], serde_json::json!(false));
    assert_eq!(permissions_of("zeta")["write"], serde_json::json!(true));
    assert_eq!(permissions_of("zeta")["exec"], serde_json::json!(true));

    // Logical ids only: no absolute roots anywhere in the protocol output.
    let raw = response.to_string();
    for root in [&zeta, &alpha, &middle] {
        let root = root.to_string_lossy().into_owned();
        assert!(
            !raw.contains(root.trim_start_matches('/')),
            "list_workspaces must not expose roots: {raw}"
        );
    }

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn registry_workspace_info_selects_the_requested_workspace() {
    let tmp = TempDir::new().unwrap();
    let alpha = tmp.path().join("alpha-ws");
    let beta = tmp.path().join("beta-ws");
    std::fs::create_dir_all(&alpha).unwrap();
    std::fs::create_dir_all(&beta).unwrap();
    let config = registry_config(
        "version = 1",
        &[
            workspace_entry("alpha", &alpha, ""),
            workspace_entry("beta", &beta, "write = true\nexec = true"),
        ],
    );
    let (_cfg_dir, cfg_path) = write_config(&config);

    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);
    session.initialize();

    // Sequential explicit selections; each response must reflect only its
    // own requested id — no shared current-workspace state. Both orders are
    // exercised to rule out order-dependent state.
    let mut id = 10;
    for (first, second) in [("alpha", "beta"), ("beta", "alpha")] {
        let response = session.call_tool(
            id,
            "workspace_info",
            serde_json::json!({ "workspace": first }),
        );
        id += 1;
        assert!(response.get("error").is_none(), "{response}");
        let content = &response["result"]["structuredContent"];
        assert_eq!(content["workspace"], serde_json::json!(first));
        assert_eq!(content["permissions"]["read"], serde_json::json!(true));

        let response = session.call_tool(
            id,
            "workspace_info",
            serde_json::json!({ "workspace": second }),
        );
        id += 1;
        assert!(response.get("error").is_none(), "{response}");
        let content = &response["result"]["structuredContent"];
        assert_eq!(content["workspace"], serde_json::json!(second));

        // Registry responses carry logical identity, not filesystem paths.
        assert!(
            content.get("root").is_none() && content.get("name").is_none(),
            "registry workspace_info must not expose roots: {content}"
        );
    }

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn registry_unknown_workspace_is_clean_error_and_server_survives() {
    let tmp = TempDir::new().unwrap();
    let (_cfg_dir, cfg_path) = two_workspace_config(&tmp);
    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);
    session.initialize();

    let response = session.call_tool(
        3,
        "workspace_info",
        serde_json::json!({ "workspace": "does-not-exist" }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Unknown workspace 'does-not-exist'"),
        "unexpected error text: {text}"
    );
    // The error is bounded by construction: it must not enumerate every
    // configured workspace id (registry size is unbounded from the client's
    // perspective); discovery via list_workspaces is the recovery path.
    assert!(
        text.contains("Use list_workspaces to discover valid workspace IDs"),
        "{text}"
    );
    assert!(
        !text.contains("nian-home") && !text.contains("nian-vision"),
        "bounded error must not enumerate configured ids: {text}"
    );

    // The server must remain fully usable after the failed call.
    let response = session.call_tool(4, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    let ids: Vec<&str> = response["result"]["structuredContent"]["workspaces"]
        .as_array()
        .unwrap()
        .iter()
        .map(|w| w["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, ["nian-home", "nian-vision"]);

    let (code, stderr) = session.shutdown();
    assert_eq!(
        code,
        Some(0),
        "clean exit after unknown workspace: {stderr}"
    );
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_invalid_workspace_selectors_fail_cleanly() {
    let tmp = TempDir::new().unwrap();
    let (_cfg_dir, cfg_path) = two_workspace_config(&tmp);
    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);
    session.initialize();

    // Grammar violations (traversal, absolute path, uppercase) must be
    // rejected at the WorkspaceId boundary — no path resolution, no panic,
    // no fallback — and the server must stay usable afterwards.
    for (i, bad) in ["../foo", "/tmp/foo", "Nian-Vision"].iter().enumerate() {
        let id = 10 + i as u64;
        let response = session.call_tool(
            id,
            "workspace_info",
            serde_json::json!({ "workspace": bad }),
        );
        let text = expect_tool_error(&response);
        assert!(
            text.contains("invalid workspace id") && text.contains(bad),
            "selector '{bad}' must fail the id grammar check: {text}"
        );
    }

    let response = session.call_tool(20, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "server alive: {response}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_direct_invocation_of_unregistered_tool_is_safe() {
    let tmp = TempDir::new().unwrap();
    let (_cfg_dir, cfg_path) = two_workspace_config(&tmp);
    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);
    session.initialize();

    // Every v0.1 tool is now registered in registry mode (M5), so the
    // router boundary is exercised with names that are deliberately not
    // part of any surface — Git mutation and workspace switching are
    // explicit non-goals. A direct call must fail cleanly inside the
    // router: a protocol-level "tool not found" error, no panic, no
    // workspace access, no default workspace, and the session must remain
    // healthy afterwards.
    for (id, unregistered) in (3u64..).zip(["git_commit", "switch_workspace", "run_shell"]) {
        let response = session.call_tool(id, unregistered, serde_json::json!({}));
        let error = response
            .get("error")
            .unwrap_or_else(|| panic!("unregistered tool must be rejected: {response}"));
        assert!(
            error["message"]
                .as_str()
                .unwrap_or("")
                .contains("tool not found"),
            "unexpected rejection: {response}"
        );
    }

    // The server is still alive and serving after the rejected calls.
    let response = session.call_tool(10, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    assert!(response["result"]["structuredContent"]["workspaces"].is_array());

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_read_tools_isolate_workspaces_and_carry_provenance() {
    let (tmp, cfg) = cross_workspace_registry();
    let alpha_root = tmp.path().join("alpha");
    let beta_root = tmp.path().join("beta");
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    let mut protocol_output: Vec<String> = Vec::new();

    // read_file: identical relative filename, different content per root —
    // no bleed in either direction.
    let response = session.call_tool(
        3,
        "read_file",
        serde_json::json!({ "workspace": "alpha", "path": "shared.txt" }),
    );
    protocol_output.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["workspace"],
        serde_json::json!("alpha"),
        "{content}"
    );
    assert_eq!(content["lines"][0], serde_json::json!("1: FROM_ALPHA"));

    let response = session.call_tool(
        4,
        "read_file",
        serde_json::json!({ "workspace": "beta", "path": "shared.txt" }),
    );
    protocol_output.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("beta"), "{content}");
    assert_eq!(content["lines"][0], serde_json::json!("1: FROM_BETA"));

    // list_files: different directory structures per root.
    let response = session.call_tool(
        5,
        "list_files",
        serde_json::json!({ "workspace": "alpha", "path": "src" }),
    );
    protocol_output.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("alpha"));
    let paths: Vec<&str> = content["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    assert_eq!(paths, ["src/a.rs"]);

    let response = session.call_tool(
        6,
        "list_files",
        serde_json::json!({ "workspace": "beta", "path": "src" }),
    );
    protocol_output.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("beta"));
    let mut paths: Vec<&str> = content["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["path"].as_str().unwrap())
        .collect();
    paths.sort_unstable();
    assert_eq!(paths, ["src/b.rs", "src/only_beta.rs"]);

    // search: distinct markers per root, no cross-workspace results.
    let response = session.call_tool(
        7,
        "search",
        serde_json::json!({ "workspace": "alpha", "query": "UNIQUE_" }),
    );
    protocol_output.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("alpha"));
    assert_eq!(content["match_count"], serde_json::json!(1));
    assert_eq!(content["matches"][0]["path"], serde_json::json!("src/a.rs"));
    // A search rooted at the workspace itself must not carry the absolute
    // root — the workspace-relative rendering is ".".
    assert_eq!(content["path"], serde_json::json!("."));

    let response = session.call_tool(
        8,
        "search",
        serde_json::json!({ "workspace": "beta", "query": "UNIQUE_" }),
    );
    protocol_output.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("beta"));
    assert_eq!(content["matches"][0]["path"], serde_json::json!("src/b.rs"));

    // Logical provenance + workspace-relative paths only: no absolute root
    // may appear anywhere in the protocol output.
    let all = protocol_output.join("\n");
    for root in [alpha_root, beta_root] {
        let root = root.to_string_lossy().into_owned();
        assert!(
            !all.contains(&root) || root.is_empty(),
            "absolute root leaked into output"
        );
        assert!(
            !all.contains(root.trim_start_matches('/')),
            "root path leaked into output: {root}"
        );
    }

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn registry_read_tools_reject_traversal_into_sibling_workspace() {
    let (_tmp, cfg) = cross_workspace_registry();
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    let response = session.call_tool(
        3,
        "read_file",
        serde_json::json!({ "workspace": "alpha", "path": "../beta/shared.txt" }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("outside the configured workspace"),
        "read_file traversal must be rejected: {text}"
    );

    let response = session.call_tool(
        4,
        "list_files",
        serde_json::json!({ "workspace": "alpha", "path": "../beta" }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("outside the configured workspace"),
        "list_files traversal must be rejected: {text}"
    );

    let response = session.call_tool(
        5,
        "search",
        serde_json::json!({ "workspace": "alpha", "query": "TOKEN", "path": "../beta" }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("outside the configured workspace"),
        "search traversal must be rejected: {text}"
    );

    // Registered neighbors stay outside alpha's root — and the server stays
    // healthy after the rejections.
    let response = session.call_tool(6, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[cfg(unix)]
#[test]
fn registry_read_tools_reject_symlink_escape_into_sibling_workspace() {
    let (tmp, cfg) = cross_workspace_registry();
    std::os::unix::fs::symlink("../beta", tmp.path().join("alpha/leak")).unwrap();
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    let response = session.call_tool(
        3,
        "read_file",
        serde_json::json!({ "workspace": "alpha", "path": "leak/shared.txt" }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("outside the configured workspace"),
        "symlinked read must be rejected: {text}"
    );

    let response = session.call_tool(
        4,
        "list_files",
        serde_json::json!({ "workspace": "alpha", "path": "leak" }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("outside the configured workspace"),
        "symlinked listing must be rejected: {text}"
    );

    let response = session.call_tool(
        5,
        "search",
        serde_json::json!({ "workspace": "alpha", "query": "TOKEN", "path": "leak" }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("outside the configured workspace"),
        "symlinked search root must be rejected: {text}"
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn registry_read_tools_reject_invalid_workspace_selectors() {
    let (_tmp, cfg) = cross_workspace_registry();
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // Grammar violations fail at the WorkspaceId boundary — before any path
    // handling — for every read-tool family, with the offending value named.
    let mut id = 10;
    for (tool, extra) in [
        ("list_files", serde_json::json!({})),
        ("read_file", serde_json::json!({ "path": "shared.txt" })),
        ("search", serde_json::json!({ "query": "x" })),
    ] {
        for bad in ["../foo", "Nian-Vision"] {
            let mut arguments = extra.clone();
            arguments["workspace"] = serde_json::json!(bad);
            let response = session.call_tool(id, tool, arguments);
            id += 1;
            let text = expect_tool_error(&response);
            assert!(
                text.contains("invalid workspace id") && text.contains(bad),
                "{tool} selector '{bad}' must fail the id grammar check: {text}"
            );
        }
    }
    // Absolute-path selectors are not workspace ids either.
    let response = session.call_tool(
        id,
        "read_file",
        serde_json::json!({ "workspace": "/tmp/foo", "path": "shared.txt" }),
    );
    let text = expect_tool_error(&response);
    assert!(text.contains("invalid workspace id"), "{text}");

    let response = session.call_tool(30, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_read_tools_unknown_workspace_is_bounded_then_healthy() {
    let (_tmp, cfg) = cross_workspace_registry();
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // A valid but unregistered id gets the bounded M2 error, per tool
    // family — no enumeration, no fallback, no default workspace.
    for (id, (tool, extra)) in (10..).zip([
        ("read_file", serde_json::json!({ "path": "shared.txt" })),
        ("list_files", serde_json::json!({})),
        ("search", serde_json::json!({ "query": "x" })),
    ]) {
        let mut arguments = extra;
        arguments["workspace"] = serde_json::json!("does-not-exist");
        let response = session.call_tool(id, tool, arguments);
        let text = expect_tool_error(&response);
        assert!(
            text.contains("Unknown workspace 'does-not-exist'")
                && text.contains("Use list_workspaces to discover valid workspace IDs"),
            "{tool}: {text}"
        );
        assert!(
            !text.contains("alpha") && !text.contains("beta"),
            "bounded error must not enumerate configured ids: {text}"
        );
    }

    // The server remained healthy throughout: a valid read still works.
    let response = session.call_tool(
        20,
        "read_file",
        serde_json::json!({ "workspace": "alpha", "path": "shared.txt" }),
    );
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["lines"][0],
        serde_json::json!("1: FROM_ALPHA")
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_git_tools_are_scoped_in_independent_repositories() {
    let (tmp, cfg) = independent_repos_registry();
    let alpha = tmp.path().join("alpha");
    let beta = tmp.path().join("beta");
    // One unstaged modification + one untracked file in alpha; one staged
    // change in beta.
    std::fs::write(alpha.join("alpha_tracked.txt"), "ALPHA_DIFF_MARK\n").unwrap();
    std::fs::write(alpha.join("alpha_extra.txt"), "new\n").unwrap();
    std::fs::write(beta.join("beta_tracked.txt"), "BETA_STAGED_MARK\n").unwrap();
    git(&beta, &["add", "."]);

    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();
    let mut outputs: Vec<String> = Vec::new();

    // git_status(alpha): alpha's changes only.
    let response = session.call_tool(3, "git_status", serde_json::json!({ "workspace": "alpha" }));
    outputs.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["workspace"],
        serde_json::json!("alpha"),
        "{content}"
    );
    assert_eq!(content["truncated"], serde_json::json!(false));
    let output = content["output"].as_str().unwrap();
    assert!(
        output.contains("?? alpha_extra.txt") && output.contains("alpha_tracked.txt"),
        "alpha changes missing: {output}"
    );
    assert!(
        !output.contains("beta_tracked.txt") && !output.contains("BETA_STAGED_MARK"),
        "beta change leaked into alpha's status: {output}"
    );

    // git_status(beta): the inverse view.
    let response = session.call_tool(4, "git_status", serde_json::json!({ "workspace": "beta" }));
    outputs.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("beta"), "{content}");
    let output = content["output"].as_str().unwrap();
    assert!(
        output.contains("beta_tracked.txt"),
        "beta change missing: {output}"
    );
    assert!(
        !output.contains("alpha_tracked.txt") && !output.contains("alpha_extra.txt"),
        "alpha change leaked into beta's status: {output}"
    );

    // git_diff(alpha): unstaged by default, only alpha's hunks.
    let response = session.call_tool(5, "git_diff", serde_json::json!({ "workspace": "alpha" }));
    outputs.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["workspace"],
        serde_json::json!("alpha"),
        "{content}"
    );
    assert_eq!(content["staged"], serde_json::json!(false));
    let diff = content["diff"].as_str().unwrap();
    assert!(diff.contains("+ALPHA_DIFF_MARK"), "{diff}");
    assert!(
        !diff.contains("BETA"),
        "beta content leaked into alpha's diff: {diff}"
    );

    // git_diff(beta): staged=false selects nothing; staged=true the staged
    // hunk — the same selection single mode would make for this context.
    let response = session.call_tool(
        6,
        "git_diff",
        serde_json::json!({ "workspace": "beta", "staged": false }),
    );
    outputs.push(response.to_string());
    assert!(
        response["result"]["structuredContent"]["diff"]
            .as_str()
            .unwrap()
            .trim()
            .is_empty(),
        "unstaged diff of beta must be empty"
    );
    let response = session.call_tool(
        7,
        "git_diff",
        serde_json::json!({ "workspace": "beta", "staged": true }),
    );
    outputs.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("beta"), "{content}");
    assert_eq!(content["staged"], serde_json::json!(true));
    assert!(
        content["diff"]
            .as_str()
            .unwrap()
            .contains("+BETA_STAGED_MARK"),
        "{content}"
    );

    // Logical provenance + workspace-relative paths only: no absolute roots
    // anywhere in the protocol output.
    let all = outputs.join("\n");
    for root in [&alpha, &beta] {
        let root = root.to_string_lossy().into_owned();
        assert!(
            !all.contains(root.trim_start_matches('/')),
            "absolute root leaked into Git output: {root}"
        );
    }

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_git_tools_do_not_leak_through_a_parent_repository() {
    let (tmp, cfg) = parent_repo_registry();
    let repo = tmp.path().join("repo");
    let alpha = repo.join("alpha");
    let beta = repo.join("beta");
    // Both workspaces sit inside the same parent repository; each gets its
    // own modification.
    std::fs::write(alpha.join("alpha.txt"), "original\nALPHA_PARENT_MARK\n").unwrap();
    std::fs::write(beta.join("beta.txt"), "original\nBETA_PARENT_MARK\n").unwrap();

    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();
    let mut outputs: Vec<String> = Vec::new();

    // git_status(alpha): only alpha's change is visible...
    let response = session.call_tool(3, "git_status", serde_json::json!({ "workspace": "alpha" }));
    outputs.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["workspace"],
        serde_json::json!("alpha"),
        "{content}"
    );
    let output = content["output"].as_str().unwrap();
    assert!(
        output.contains(" M alpha.txt"),
        "selected change missing: {output}"
    );
    assert!(
        !output.contains("beta.txt") && !output.contains("BETA_PARENT_MARK"),
        "sibling change leaked through the parent repository: {output}"
    );

    // ...and the inverse view for beta.
    let response = session.call_tool(4, "git_status", serde_json::json!({ "workspace": "beta" }));
    outputs.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("beta"), "{content}");
    let output = content["output"].as_str().unwrap();
    assert!(
        output.contains(" M beta.txt"),
        "selected change missing: {output}"
    );
    assert!(
        !output.contains("alpha.txt") && !output.contains("ALPHA_PARENT_MARK"),
        "sibling change leaked through the parent repository: {output}"
    );

    // git_diff(alpha): only alpha's hunks, with headers workspace-relative —
    // the same rendering single mode produces for a workspace nested inside
    // a parent repository.
    let response = session.call_tool(5, "git_diff", serde_json::json!({ "workspace": "alpha" }));
    outputs.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["workspace"],
        serde_json::json!("alpha"),
        "{content}"
    );
    let diff = content["diff"].as_str().unwrap();
    assert!(
        diff.contains("--- a/alpha.txt") && diff.contains("+++ b/alpha.txt"),
        "diff headers must be workspace-relative: {diff}"
    );
    assert!(diff.contains("+ALPHA_PARENT_MARK"), "{diff}");
    assert!(
        !diff.contains("beta.txt") && !diff.contains("BETA_PARENT_MARK"),
        "sibling diff leaked through the parent repository: {diff}"
    );

    // git_diff(beta): the inverse view.
    let response = session.call_tool(6, "git_diff", serde_json::json!({ "workspace": "beta" }));
    outputs.push(response.to_string());
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("beta"), "{content}");
    let diff = content["diff"].as_str().unwrap();
    assert!(
        diff.contains("+++ b/beta.txt") && diff.contains("+BETA_PARENT_MARK"),
        "{diff}"
    );
    assert!(
        !diff.contains("alpha.txt") && !diff.contains("ALPHA_PARENT_MARK"),
        "sibling diff leaked through the parent repository: {diff}"
    );

    // Neither the selected roots nor the parent repository root may appear
    // anywhere in the protocol output (both workspace roots sit inside the
    // repo directory, so checking the repo path covers all three).
    let all = outputs.join("\n");
    let repo_str = repo.to_string_lossy().into_owned();
    assert!(
        !all.contains(repo_str.trim_start_matches('/')),
        "parent repository root leaked into Git output: {repo_str}"
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_git_diff_path_filter_stays_inside_the_workspace() {
    let (tmp, cfg) = independent_repos_registry();
    let alpha = tmp.path().join("alpha");
    std::fs::create_dir_all(alpha.join("src")).unwrap();
    std::fs::write(alpha.join("src/a.rs"), "original\n").unwrap();
    std::fs::write(alpha.join("src/b.rs"), "original\n").unwrap();
    git(&alpha, &["add", "."]);
    git(&alpha, &["commit", "-m", "add src"]);
    std::fs::write(alpha.join("src/a.rs"), "MARK_A\n").unwrap();
    std::fs::write(alpha.join("src/b.rs"), "MARK_B\n").unwrap();

    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // A workspace-relative path filters the diff to one file.
    let response = session.call_tool(
        3,
        "git_diff",
        serde_json::json!({ "workspace": "alpha", "path": "src/a.rs" }),
    );
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["workspace"],
        serde_json::json!("alpha"),
        "{content}"
    );
    assert_eq!(content["path"], serde_json::json!("src/a.rs"), "{content}");
    let diff = content["diff"].as_str().unwrap();
    assert!(diff.contains("+MARK_A"), "{diff}");
    assert!(
        !diff.contains("MARK_B") && !diff.contains("b.rs"),
        "path filter leaked the other file: {diff}"
    );

    // A root-targeted path renders as "." under the registry contract and
    // scopes the diff to the whole workspace.
    let response = session.call_tool(
        4,
        "git_diff",
        serde_json::json!({ "workspace": "alpha", "path": "." }),
    );
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["path"], serde_json::json!("."), "{content}");
    let diff = content["diff"].as_str().unwrap();
    assert!(
        diff.contains("+MARK_A") && diff.contains("+MARK_B"),
        "{diff}"
    );
    let raw = response.to_string();
    assert!(
        !raw.contains(alpha.to_string_lossy().as_ref()),
        "root-targeted diff must not expose the root: {raw}"
    );

    // A sibling-workspace pathspec is rejected before any git process runs,
    // and the server stays healthy afterwards.
    let response = session.call_tool(
        5,
        "git_diff",
        serde_json::json!({ "workspace": "alpha", "path": "../beta/beta_tracked.txt" }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Invalid diff path") || text.contains("outside the configured workspace"),
        "'{text}"
    );
    let response = session.call_tool(6, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_git_errors_stay_clean_and_the_server_stays_healthy() {
    let (tmp, cfg) = git_and_plain_registry();
    let alpha = tmp.path().join("alpha");
    let plain = tmp.path().join("plain");
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // Non-repository workspace: the normal v0.1-style failure, but under
    // the registry presentation the root renders as "." — neither the
    // selected root nor any other configured root may appear.
    let response = session.call_tool(3, "git_status", serde_json::json!({ "workspace": "plain" }));
    let text = expect_tool_error(&response);
    assert!(
        text.contains("does not appear to be inside a Git working tree"),
        "{text}"
    );
    assert!(
        text.contains("'.'"),
        "registry root rendering is '.': {text}"
    );
    assert!(
        !text.contains(plain.to_string_lossy().as_ref())
            && !text.contains(alpha.to_string_lossy().as_ref()),
        "non-repository error must not expose roots: {text}"
    );

    let response = session.call_tool(4, "git_diff", serde_json::json!({ "workspace": "plain" }));
    let text = expect_tool_error(&response);
    assert!(
        text.contains("does not appear to be inside a Git working tree"),
        "{text}"
    );
    assert!(
        !text.contains(plain.to_string_lossy().as_ref()),
        "non-repository error must not expose roots: {text}"
    );

    // Unknown workspace: the bounded M2 error — no git discovery, no fallback.
    let response = session.call_tool(
        5,
        "git_status",
        serde_json::json!({ "workspace": "does-not-exist" }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Unknown workspace 'does-not-exist'")
            && text.contains("Use list_workspaces to discover valid workspace IDs"),
        "{text}"
    );

    // Malformed selectors fail the id grammar at the boundary.
    for (i, bad) in ["../foo", "Nian-Vision"].iter().enumerate() {
        let response = session.call_tool(
            6 + i as u64,
            "git_status",
            serde_json::json!({ "workspace": bad }),
        );
        let text = expect_tool_error(&response);
        assert!(
            text.contains("invalid workspace id") && text.contains(bad),
            "selector '{bad}' must fail the id grammar check: {text}"
        );
    }

    // The server is still healthy: a valid git call and discovery both work.
    std::fs::write(alpha.join("alpha_tracked.txt"), "MODIFIED\n").unwrap();
    let response = session.call_tool(8, "git_status", serde_json::json!({ "workspace": "alpha" }));
    assert!(response.get("error").is_none(), "{response}");
    let output = response["result"]["structuredContent"]["output"]
        .as_str()
        .unwrap();
    assert!(output.contains(" M alpha_tracked.txt"), "{output}");
    let response = session.call_tool(9, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_apply_patch_isolates_workspaces_and_requires_write() {
    let (tmp, cfg) = writable_repos_registry();
    let alpha = tmp.path().join("alpha");
    let beta = tmp.path().join("beta");
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // Patch the selected (writable) workspace only.
    let response = session.call_tool(
        3,
        "apply_patch",
        serde_json::json!({
            "workspace": "alpha",
            "patch": "--- shared.txt\n+++ shared.txt\n@@ -1,1 +1,1 @@\n-ALPHA_ORIGINAL\n+ALPHA_PATCHED\n"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["workspace"],
        serde_json::json!("alpha"),
        "{content}"
    );
    assert_eq!(content["changed_files"], serde_json::json!(["shared.txt"]));
    let raw = response.to_string();
    assert!(
        !raw.contains(alpha.to_string_lossy().as_ref())
            && !raw.contains(beta.to_string_lossy().as_ref()),
        "patch response must not expose roots: {raw}"
    );

    // The registered neighbor is byte-for-byte unchanged.
    let response = session.call_tool(
        4,
        "read_file",
        serde_json::json!({ "workspace": "beta", "path": "shared.txt" }),
    );
    assert_eq!(
        response["result"]["structuredContent"]["lines"][0],
        serde_json::json!("1: BETA_ORIGINAL")
    );

    // The patch composes with the read-only Git tool on the same workspace.
    let response = session.call_tool(5, "git_diff", serde_json::json!({ "workspace": "alpha" }));
    assert!(response.get("error").is_none(), "{response}");
    let diff = response["result"]["structuredContent"]["diff"]
        .as_str()
        .unwrap();
    assert!(diff.contains("+ALPHA_PATCHED"), "{diff}");

    // A read-only workspace rejects patching before anything is parsed or
    // changed, with a bounded error naming the logical id — no roots.
    let response = session.call_tool(
        6,
        "apply_patch",
        serde_json::json!({
            "workspace": "locked",
            "patch": "--- shared.txt\n+++ shared.txt\n@@ -1,1 +1,1 @@\n-LOCKED_ORIGINAL\n+HACKED\n"
        }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Workspace 'locked' does not allow file writes."),
        "{text}"
    );
    assert!(
        !text.contains(locked_str(&tmp).as_str()),
        "permission error must not expose roots: {text}"
    );
    let response = session.call_tool(
        7,
        "read_file",
        serde_json::json!({ "workspace": "locked", "path": "shared.txt" }),
    );
    assert_eq!(
        response["result"]["structuredContent"]["lines"][0],
        serde_json::json!("1: LOCKED_ORIGINAL")
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

/// The canonical spelling of the locked workspace's root, for leak checks.
fn locked_str(tmp: &TempDir) -> String {
    tmp.path()
        .join("locked")
        .to_string_lossy()
        .trim_start_matches('/')
        .to_string()
}

#[test]
fn registry_patch_rejects_escapes_and_stays_healthy() {
    let (tmp, cfg) = writable_repos_registry();
    let alpha = tmp.path().join("alpha");
    let beta = tmp.path().join("beta");
    #[cfg(unix)]
    std::os::unix::fs::symlink("../beta", alpha.join("leak")).unwrap();

    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // A sibling-workspace target is rejected by the resolver.
    let response = session.call_tool(
        3,
        "apply_patch",
        serde_json::json!({
            "workspace": "alpha",
            "patch": "--- ../beta/shared.txt\n+++ ../beta/shared.txt\n@@ -1,1 +1,1 @@\n-BETA_ORIGINAL\n+HACKED\n"
        }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Rejecting patch target")
            && text.contains("outside the configured workspace"),
        "{text}"
    );

    // A multi-file patch with one valid target and one escaping target must
    // fail validation as a whole — the valid hunk is not committed.
    let response = session.call_tool(
        4,
        "apply_patch",
        serde_json::json!({
            "workspace": "alpha",
            "patch": "--- shared.txt\n+++ shared.txt\n@@ -1,1 +1,1 @@\n-ALPHA_ORIGINAL\n+ALPHA_PATCHED\n--- ../locked/escaped.txt\n+++ ../locked/escaped.txt\n@@ -1,1 +1,1 @@\n-x\n+y\n"
        }),
    );
    let text = expect_tool_error(&response);
    assert!(text.contains("Rejecting patch target"), "{text}");

    // On Unix, patching through a symlink into the sibling is rejected too.
    #[cfg(unix)]
    {
        let response = session.call_tool(
            5,
            "apply_patch",
            serde_json::json!({
                "workspace": "alpha",
                "patch": "--- leak/shared.txt\n+++ leak/shared.txt\n@@ -1,1 +1,1 @@\n-BETA_ORIGINAL\n+HACKED\n"
            }),
        );
        let text = expect_tool_error(&response);
        assert!(
            text.contains("Rejecting patch target")
                && text.contains("outside the configured workspace"),
            "{text}"
        );
    }

    // Neither alpha, beta, nor locked changed anywhere.
    assert_eq!(
        std::fs::read_to_string(alpha.join("shared.txt")).unwrap(),
        "ALPHA_ORIGINAL\n",
        "the valid hunk must not be partially committed"
    );
    assert_eq!(
        std::fs::read_to_string(beta.join("shared.txt")).unwrap(),
        "BETA_ORIGINAL\n"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("locked/shared.txt")).unwrap(),
        "LOCKED_ORIGINAL\n"
    );

    // The server remains healthy: a valid patch still applies.
    let response = session.call_tool(
        6,
        "apply_patch",
        serde_json::json!({
            "workspace": "beta",
            "patch": "--- shared.txt\n+++ shared.txt\n@@ -1,1 +1,1 @@\n-BETA_ORIGINAL\n+BETA_PATCHED\n"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    assert_eq!(
        response["result"]["structuredContent"]["workspace"],
        serde_json::json!("beta")
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_run_command_matrix_and_cwd_isolation() {
    let (tmp, cfg) = exec_matrix_registry();
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // Selected cwd isolation, no shell involved: each workspace's own git
    // repo answers with its own untracked marker only.
    let response = session.call_tool(
        3,
        "run_command",
        serde_json::json!({
            "workspace": "alpha",
            "program": "git",
            "args": ["status", "--short"],
            "cwd": "."
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("alpha"));
    assert_eq!(content["exit_code"], serde_json::json!(0));
    let stdout = content["stdout"].as_str().unwrap();
    assert!(stdout.contains("?? untracked_alpha.txt"), "{stdout}");
    assert!(
        !stdout.contains("untracked_beta.txt") && !stdout.contains("BETA_MARKER"),
        "beta leaked into alpha's command output: {stdout}"
    );

    let response = session.call_tool(
        4,
        "run_command",
        serde_json::json!({
            "workspace": "beta",
            "program": "git",
            "args": ["status", "--short"],
            "cwd": "."
        }),
    );
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("beta"));
    let stdout = content["stdout"].as_str().unwrap();
    assert!(stdout.contains("?? untracked_beta.txt"), "{stdout}");
    assert!(!stdout.contains("untracked_alpha.txt"), "{stdout}");

    // Root cwd (".") runs inside the selected workspace: the git prefix of
    // the repo the child actually landed in is empty at the root (the
    // cross-platform cwd proof — no Unix utilities).
    let response = session.call_tool(
        5,
        "run_command",
        serde_json::json!({
            "workspace": "alpha",
            "program": "git",
            "args": ["rev-parse", "--show-prefix"],
            "cwd": "."
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["exit_code"], serde_json::json!(0));
    assert!(
        content["stdout"].as_str().unwrap().trim().is_empty(),
        "root cwd must resolve to the workspace root: {content}"
    );

    // exec denied: tool error, no side effect (`git init` would create the
    // directory if the process were spawned — cross-platform probe).
    let response = session.call_tool(
        6,
        "run_command",
        serde_json::json!({
            "workspace": "locked",
            "program": "git",
            "args": ["init", "spawned-anyway"]
        }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Workspace 'locked' does not allow command execution."),
        "{text}"
    );
    assert!(
        !text.contains(tmp.path().to_string_lossy().as_ref()),
        "permission error must not expose roots: {text}"
    );
    assert!(
        !tmp.path().join("locked/spawned-anyway").exists(),
        "denied command spawned a process anyway"
    );

    // shell denied on an exec-only workspace: rejected before any shell
    // spawn, no side effect.
    let response = session.call_tool(
        7,
        "run_command",
        serde_json::json!({
            "workspace": "gamma",
            "shell": true,
            "command": "git init shell-ran"
        }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Workspace 'gamma' does not allow shell execution."),
        "{text}"
    );
    assert!(
        !tmp.path().join("gamma/shell-ran").exists(),
        "denied shell spawned a process anyway"
    );

    // shell allowed where the workspace permits it (cross-platform: the
    // `git init` side effect proves the shell really executed).
    let response = session.call_tool(
        8,
        "run_command",
        serde_json::json!({
            "workspace": "delta",
            "shell": true,
            "command": "git init shell-ran-dir"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("delta"));
    assert_eq!(content["exit_code"], serde_json::json!(0));
    assert!(
        tmp.path().join("delta/shell-ran-dir/.git").exists(),
        "the shell command must have run inside delta: {content}"
    );

    // cwd traversal is rejected before spawn; the server stays healthy.
    let response = session.call_tool(
        9,
        "run_command",
        serde_json::json!({
            "workspace": "alpha",
            "program": "git",
            "args": ["status", "--short"],
            "cwd": "../beta"
        }),
    );
    let text = expect_tool_error(&response);
    assert!(text.contains("outside the configured workspace"), "{text}");
    assert!(
        !text.contains(tmp.path().to_string_lossy().as_ref()),
        "cwd resolution error must not expose roots: {text}"
    );

    // Health after all failures: discovery, a read, and a valid command.
    let response = session.call_tool(10, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    let response = session.call_tool(
        11,
        "read_file",
        serde_json::json!({ "workspace": "alpha", "path": "tracked.txt" }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let response = session.call_tool(
        12,
        "run_command",
        serde_json::json!({
            "workspace": "beta",
            "program": "git",
            "args": ["status", "--short"]
        }),
    );
    assert!(response.get("error").is_none(), "{response}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_capability_matrix_over_mcp() {
    let (tmp, cfg) = capability_matrix_registry();
    let readonly = tmp.path().join("readonly");
    let writer = tmp.path().join("writer");
    let executor = tmp.path().join("executor");
    let full = tmp.path().join("full");
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // -- readonly: reads and Git inspection need no capability ---------------
    let response = session.call_tool(
        3,
        "read_file",
        serde_json::json!({ "workspace": "readonly", "path": "const.txt" }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let response = session.call_tool(
        4,
        "git_status",
        serde_json::json!({ "workspace": "readonly" }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("readonly"));

    // Mutation and execution are both denied.
    let response = session.call_tool(
        5,
        "apply_patch",
        serde_json::json!({
            "workspace": "readonly",
            "patch": "--- const.txt\n+++ const.txt\n@@ -1,1 +1,1 @@\n-READONLY\n+X\n"
        }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Workspace 'readonly' does not allow file writes."),
        "{text}"
    );
    assert!(
        !text.contains(tmp.path().to_string_lossy().as_ref()),
        "permission error must not expose roots: {text}"
    );

    let response = session.call_tool(
        6,
        "run_command",
        serde_json::json!({
            "workspace": "readonly",
            "program": "git",
            "args": ["--version"]
        }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Workspace 'readonly' does not allow command execution."),
        "{text}"
    );

    // -- writer: write = true allows patching only ---------------------------
    let response = session.call_tool(
        7,
        "apply_patch",
        serde_json::json!({
            "workspace": "writer",
            "patch": "--- patch.txt\n+++ patch.txt\n@@ -1,1 +1,1 @@\n-WRITER ORIGINAL\n+WRITER PATCHED\n"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("writer"));
    assert_eq!(
        std::fs::read_to_string(writer.join("patch.txt")).unwrap(),
        "WRITER PATCHED\n"
    );

    // ...but exec stays denied even though executor and full run commands.
    let response = session.call_tool(
        8,
        "run_command",
        serde_json::json!({
            "workspace": "writer",
            "program": "git",
            "args": ["--version"]
        }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Workspace 'writer' does not allow command execution."),
        "{text}"
    );

    // -- executor: exec = true allows direct commands only --------------------
    // The child answers from the selected workspace's cwd (cross-platform
    // git fixture), proving the selected context decided where it ran.
    let response = session.call_tool(
        9,
        "run_command",
        serde_json::json!({
            "workspace": "executor",
            "program": "git",
            "args": ["status", "--short"]
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("executor"));
    assert!(
        content["stdout"]
            .as_str()
            .unwrap()
            .contains("?? executor-marker.txt"),
        "{content}"
    );

    // Shell stays denied even though full allows it — and no shell runs.
    let response = session.call_tool(
        10,
        "run_command",
        serde_json::json!({
            "workspace": "executor",
            "shell": true,
            "command": "git init shell-ran"
        }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Workspace 'executor' does not allow shell execution."),
        "{text}"
    );
    assert!(
        !executor.join("shell-ran").exists(),
        "denied shell spawned a process anyway"
    );

    // ...and write stays denied even though writer and full patch.
    std::fs::write(executor.join("patch.txt"), "EXECUTOR ORIGINAL\n").unwrap();
    let response = session.call_tool(
        11,
        "apply_patch",
        serde_json::json!({
            "workspace": "executor",
            "patch": "--- patch.txt\n+++ patch.txt\n@@ -1,1 +1,1 @@\n-EXECUTOR ORIGINAL\n+X\n"
        }),
    );
    let text = expect_tool_error(&response);
    assert!(
        text.contains("Workspace 'executor' does not allow file writes."),
        "{text}"
    );
    assert_eq!(
        std::fs::read_to_string(executor.join("patch.txt")).unwrap(),
        "EXECUTOR ORIGINAL\n"
    );

    // -- full: every capability applies, and nothing more --------------------
    let response = session.call_tool(
        12,
        "apply_patch",
        serde_json::json!({
            "workspace": "full",
            "patch": "--- patch.txt\n+++ patch.txt\n@@ -1,1 +1,1 @@\n-FULL ORIGINAL\n+FULL PATCHED\n"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("full"));

    let response = session.call_tool(
        13,
        "run_command",
        serde_json::json!({
            "workspace": "full",
            "program": "git",
            "args": ["--version"]
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["exit_code"], serde_json::json!(0));

    // shell = true additionally unlocks shell-mode commands.
    let response = session.call_tool(
        14,
        "run_command",
        serde_json::json!({
            "workspace": "full",
            "shell": true,
            "command": "git init shell-ran"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["exit_code"], serde_json::json!(0));
    assert!(
        full.join("shell-ran/.git").exists(),
        "the shell command must have run inside full: {content}"
    );

    // -- health: discovery still works after the full matrix -----------------
    let response = session.call_tool(15, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");

    // The denied patch never touched the readonly workspace's file.
    assert_eq!(
        std::fs::read_to_string(readonly.join("const.txt")).unwrap(),
        "READONLY\n"
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}

#[test]
fn registry_mutation_tools_reject_bad_selectors_cleanly() {
    let (tmp, cfg) = writable_repos_registry();
    let mut session = McpSession::start(&["--workspace-config", cfg.to_str().unwrap()]);
    session.initialize();

    // Malformed selectors fail the id grammar before any patch parsing or
    // process spawning.
    for (id, tool, extra) in [
        (
            3u64,
            "apply_patch",
            serde_json::json!({ "patch": "--- a\n+++ a\n@@ -1,1 +1,1 @@\n-x\n+y\n" }),
        ),
        (
            4,
            "run_command",
            serde_json::json!({ "program": "git", "args": ["--version"] }),
        ),
    ] {
        for bad in ["../foo", "Nian-Vision"] {
            let mut arguments = extra.clone();
            arguments["workspace"] = serde_json::json!(bad);
            let response = session.call_tool(id, tool, arguments);
            let text = expect_tool_error(&response);
            assert!(
                text.contains("invalid workspace id") && text.contains(bad),
                "{tool} selector '{bad}' must fail the id grammar check: {text}"
            );
        }
    }

    // Valid grammar, unknown id: the bounded M2 error for both tools.
    for id in [10u64, 11] {
        let tool = if id == 10 {
            "apply_patch"
        } else {
            "run_command"
        };
        let mut arguments = if id == 10 {
            serde_json::json!({ "patch": "not even a patch" })
        } else {
            serde_json::json!({ "program": "git" })
        };
        arguments["workspace"] = serde_json::json!("does-not-exist");
        let response = session.call_tool(id, tool, arguments);
        let text = expect_tool_error(&response);
        assert!(
            text.contains("Unknown workspace 'does-not-exist'")
                && text.contains("Use list_workspaces to discover valid workspace IDs"),
            "{tool}: {text}"
        );
        assert!(
            !text.contains(tmp.path().to_string_lossy().as_ref()),
            "bounded error must not expose roots: {text}"
        );
    }

    // The server is still fully usable.
    let response = session.call_tool(
        12,
        "apply_patch",
        serde_json::json!({
            "workspace": "alpha",
            "patch": "--- shared.txt\n+++ shared.txt\n@@ -1,1 +1,1 @@\n-ALPHA_ORIGINAL\n+OK\n"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
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

// ---------------------------------------------------------------------------
// Runtime-host instructions: ServerInfo.instructions must tell the MCP client
// which OS/architecture it is talking to and what shell=true actually invokes,
// before the client picks command syntax. Asserted against real initialize
// responses for both modes, because the feature exists only if it reaches the
// initialize result.
// ---------------------------------------------------------------------------

/// Runtime-host assertions shared by both modes: current OS/ARCH (the server
/// binary's own compile/runtime constants), the direct-execution PATH
/// semantics, and the platform-correct shell=true wording (macOS and Linux
/// share the Unix branch; only Windows has the PowerShell caveat).
fn assert_runtime_host_instructions(instructions: &str) {
    assert!(
        instructions.contains(std::env::consts::OS),
        "instructions must contain the host OS: {instructions}"
    );
    assert!(
        instructions.contains(std::env::consts::ARCH),
        "instructions must contain the host architecture: {instructions}"
    );
    assert!(
        instructions.contains(
            "Direct run_command execution resolves programs through PATH without a shell."
        ),
        "instructions must describe direct execution: {instructions}"
    );

    #[cfg(windows)]
    {
        assert!(instructions.contains("cmd.exe"), "{instructions}");
        assert!(instructions.contains("/C"), "{instructions}");
        assert!(
            instructions.contains("PowerShell is not implied by shell=true"),
            "{instructions}"
        );
        assert!(
            !instructions
                .to_lowercase()
                .contains("shell=true uses powershell"),
            "shell=true must not be claimed to use PowerShell: {instructions}"
        );
    }
    #[cfg(unix)]
    {
        assert!(instructions.contains("/bin/sh"), "{instructions}");
        assert!(instructions.contains("-c"), "{instructions}");
    }
}

#[test]
fn single_mode_initialize_reports_runtime_host_environment() {
    let tmp = TempDir::new().unwrap();
    let mut session = McpSession::start(&[tmp.path().to_str().unwrap()]);
    let init = session.initialize();

    let instructions = init["result"]["instructions"]
        .as_str()
        .expect("server instructions in initialize result");
    assert_runtime_host_instructions(instructions);

    // The v0.1 single-mode guidance is preserved alongside the runtime host.
    assert!(instructions.contains("--write"), "{instructions}");
    assert!(instructions.contains("--exec"), "{instructions}");
    assert!(instructions.contains("--allow-shell"), "{instructions}");

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
}

#[test]
fn registry_mode_initialize_reports_runtime_host_environment() {
    let tmp = TempDir::new().unwrap();
    let (_cfg_dir, cfg_path) = two_workspace_config(&tmp);
    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);
    let init = session.initialize();

    let instructions = init["result"]["instructions"]
        .as_str()
        .expect("server instructions in initialize result");
    assert_runtime_host_instructions(instructions);

    // Registry guidance remains present, and no configured filesystem root
    // leaks into the instructions.
    assert!(instructions.contains("list_workspaces"), "{instructions}");
    assert!(
        instructions.contains("There is no default workspace"),
        "{instructions}"
    );
    assert!(
        !instructions.contains(tmp.path().to_str().unwrap()),
        "instructions must not expose configured roots: {instructions}"
    );

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(!stderr.contains("panic"), "{stderr}");
}
