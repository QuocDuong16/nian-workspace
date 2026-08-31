//! End-to-end CLI tests: v0.1 single-workspace behavior must remain intact,
//! and the v0.2 `--workspace-config` registry mode must expose exactly its
//! mode-specific MCP tool surface (M2: discovery only).
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

/// The v0.1 single-workspace tool surface, byte-for-byte.
const SINGLE_MODE_TOOLS: &[&str] = &[
    "apply_patch",
    "git_diff",
    "git_status",
    "list_files",
    "read_file",
    "run_command",
    "search",
    "workspace_info",
];

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

    let (code, stderr) = session.shutdown();
    assert_eq!(code, Some(0), "stderr: {stderr}");
    assert!(
        stderr.contains("starting nian-workspace over stdio"),
        "{stderr}"
    );
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
fn registry_mode_advertises_only_discovery_tools() {
    let tmp = TempDir::new().unwrap();
    let (_cfg_dir, cfg_path) = two_workspace_config(&tmp);
    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);
    session.initialize();

    let tools = session.list_tools();
    let mut names = tools.clone();
    names.sort();
    assert_eq!(
        names,
        ["list_workspaces", "workspace_info"],
        "registry mode must advertise exactly the M2 discovery tools"
    );
    for unavailable in [
        "list_files",
        "read_file",
        "search",
        "apply_patch",
        "run_command",
        "git_status",
        "git_diff",
    ] {
        assert!(
            !tools.iter().any(|t| t == unavailable),
            "unmigrated tool '{unavailable}' must not be advertised: {tools:?}"
        );
    }

    // Registry workspace_info requires the logical selector; list_workspaces
    // takes no arguments.
    let schema = session.tool_schema(3, "workspace_info");
    let required: Vec<&str> = schema["required"]
        .as_array()
        .expect("required array")
        .iter()
        .map(|v| v.as_str().expect("required entry string"))
        .collect();
    assert!(
        required.contains(&"workspace"),
        "registry workspace_info must require 'workspace': {schema}"
    );
    let list_schema = session.tool_schema(4, "list_workspaces");
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
fn registry_direct_invocation_of_unavailable_tool_is_safe() {
    let tmp = TempDir::new().unwrap();
    let (_cfg_dir, cfg_path) = two_workspace_config(&tmp);
    let mut session = McpSession::start(&["--workspace-config", cfg_path.to_str().unwrap()]);
    session.initialize();

    // read_file is not advertised in registry mode — but tools/list hiding
    // is not the boundary. A direct call must fail cleanly inside the
    // router: a protocol-level "tool not found" error, no panic, no
    // workspace access, no default workspace, and the session must remain
    // healthy afterwards.
    let response = session.call_tool(3, "read_file", serde_json::json!({ "path": "secret.txt" }));
    let error = response
        .get("error")
        .unwrap_or_else(|| panic!("unavailable tool must be rejected: {response}"));
    assert!(
        error["message"]
            .as_str()
            .unwrap_or("")
            .contains("tool not found"),
        "unexpected rejection: {response}"
    );

    // The server is still alive and serving after the rejected call.
    let response = session.call_tool(4, "list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");
    assert!(response["result"]["structuredContent"]["workspaces"].is_array());

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
