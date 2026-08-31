//! End-to-end tests for the Streamable HTTP transport (v0.2 M6).
//!
//! Both mode-specific MCP servers are driven through a real HTTP socket:
//! the actual binary is started with `--transport http`, and the tests speak
//! HTTP/1.1 to `POST /mcp` — initialize, tools/list, tools/call — the same
//! JSON-RPC exchanges the stdio suite (tests/cli.rs) drives against the same
//! surface constants. Together the two suites prove transport parity: stdio
//! and HTTP expose the same tool surface and permission behavior for the
//! same RuntimeMode, and the mode-specific server really is what is mounted.
//!
//! The client is deliberately dependency-free (`std::net::TcpStream`): every
//! request is sent with `Connection: close` and read to EOF, decoding
//! chunked transfer encoding and both response encodings the rmcp
//! streamable-HTTP layer produces for a request (`application/json` and
//! one-event `text/event-stream` bodies). Servers are killed and reaped on
//! drop, so no child process or port is left behind.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

mod common;

use common::{REGISTRY_MODE_TOOLS, SINGLE_MODE_TOOLS};

const MCP_PROTOCOL_VERSION: &str = "2025-06-18";

// ---------- minimal HTTP/1.1 client ----------

struct HttpResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl HttpResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(k, _)| k.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// POST one JSON payload to `/mcp` over a fresh connection and read the
/// complete response (the `Connection: close` header makes the body end at
/// EOF, so no persistent-connection bookkeeping is needed).
fn post_mcp(port: u16, session: Option<&str>, body: &str) -> HttpResponse {
    let mut stream = TcpStream::connect(("127.0.0.1", port))
        .expect("connect to the nian-workspace HTTP endpoint");
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("read timeout");
    let mut request = format!(
        "POST /mcp HTTP/1.1\r\n\
         Host: 127.0.0.1:{port}\r\n\
         Accept: application/json, text/event-stream\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         Connection: close\r\n",
        body.len()
    );
    if let Some(id) = session {
        request.push_str(&format!("Mcp-Session-Id: {id}\r\n"));
    }
    request.push_str("\r\n");
    request.push_str(body);
    stream.write_all(request.as_bytes()).expect("write request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response to EOF");
    parse_response(&raw)
}

fn parse_response(raw: &[u8]) -> HttpResponse {
    let split = raw
        .windows(4)
        .position(|window| window == b"\r\n\r\n")
        .expect("complete HTTP head");
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let payload = &raw[split + 4..];
    let mut lines = head.lines();
    let status_line = lines.next().expect("status line");
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse().ok())
        .unwrap_or_else(|| panic!("unparsable status line: {status_line}"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_string(), value.trim().to_string()))
        .collect();
    let chunked = headers.iter().any(|(name, value)| {
        name.eq_ignore_ascii_case("transfer-encoding")
            && value.to_ascii_lowercase().contains("chunked")
    });
    let bytes = if chunked {
        dechunk(payload)
    } else {
        payload.to_vec()
    };
    HttpResponse {
        status,
        headers,
        body: String::from_utf8_lossy(&bytes).into_owned(),
    }
}

fn dechunk(mut payload: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while !payload.is_empty() {
        let line_end = payload
            .windows(2)
            .position(|window| window == b"\r\n")
            .expect("chunk size line");
        let size_text = String::from_utf8_lossy(&payload[..line_end]).into_owned();
        let size = usize::from_str_radix(size_text.trim(), 16)
            .unwrap_or_else(|_| panic!("chunk size '{size_text}'"));
        payload = &payload[line_end + 2..];
        if size == 0 {
            break;
        }
        assert!(payload.len() >= size, "truncated chunk");
        out.extend_from_slice(&payload[..size]);
        payload = &payload[size..];
        if payload.starts_with(b"\r\n") {
            payload = &payload[2..];
        }
    }
    out
}

/// Extract the JSON-RPC message from a response body: rmcp answers requests
/// either with `application/json` or with a one-event
/// `text/event-stream` body, depending on the response status.
fn body_message(response: &HttpResponse) -> serde_json::Value {
    let content_type = response.header("content-type").unwrap_or_default();
    if content_type.contains("text/event-stream") {
        for line in response.body.lines().rev() {
            if let Some(data) = line.strip_prefix("data:") {
                return serde_json::from_str(data.trim_start()).unwrap_or_else(|error| {
                    panic!("SSE data is not JSON: {error}; body: {}", response.body)
                });
            }
        }
        panic!("no data line in SSE body: {}", response.body);
    }
    serde_json::from_str(&response.body)
        .unwrap_or_else(|error| panic!("body is not JSON: {error}; body: {}", response.body))
}

/// A tool call that must surface as a tool-level failure (isError result),
/// returning the error text shown to the client — the same envelope the
/// stdio transport produces.
fn expect_http_tool_error(response: &serde_json::Value) -> String {
    let result = &response["result"];
    assert_eq!(
        result["isError"], true,
        "expected isError=true tool result over HTTP: {response}"
    );
    result["content"][0]["text"]
        .as_str()
        .expect("error text content")
        .to_string()
}

// ---------- server + session harness ----------

/// A running nian-workspace HTTP server; killed and reaped on drop so no
/// child process or port outlives a test.
struct HttpServer {
    child: Child,
    port: u16,
}

impl HttpServer {
    fn start(extra_args: &[&str]) -> Self {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_nian-workspace"))
            .args([
                "--transport",
                "http",
                "--host",
                "127.0.0.1",
                "--port",
                &port.to_string(),
            ])
            .args(extra_args)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("failed to spawn nian-workspace over HTTP");
        wait_for_endpoint(port);
        HttpServer { child, port }
    }
}

impl Drop for HttpServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn free_port() -> u16 {
    TcpListener::bind(("127.0.0.1", 0))
        .expect("bind ephemeral listener")
        .local_addr()
        .expect("local address")
        .port()
}

fn wait_for_endpoint(port: u16) {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if TcpStream::connect(("127.0.0.1", port)).is_ok() {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "HTTP server did not start listening on {port}"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

/// One MCP session over the streamable HTTP transport: initialize captures
/// the session id, every later exchange reuses it.
struct HttpMcpSession {
    port: u16,
    session_id: Option<String>,
    next_id: u64,
}

impl HttpMcpSession {
    fn start(server: &HttpServer) -> Self {
        let mut session = HttpMcpSession {
            port: server.port,
            session_id: None,
            next_id: 1,
        };
        session.initialize();
        session
    }

    fn initialize(&mut self) {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": "initialize",
            "params": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": {},
                "clientInfo": { "name": "http-test", "version": "0" }
            }
        })
        .to_string();
        self.next_id += 1;
        let response = post_mcp(self.port, None, &body);
        assert_eq!(response.status, 200, "initialize failed: {}", response.body);
        self.session_id = response.header("mcp-session-id").map(|id| id.to_string());
        let message = body_message(&response);
        assert!(
            message["result"]["serverInfo"]["name"] == "nian-workspace",
            "initialize result: {message}"
        );

        // notifications/initialized is accepted with no response body.
        let note = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        })
        .to_string();
        let response = post_mcp(self.port, self.session_id.as_deref(), &note);
        assert_eq!(
            response.status, 202,
            "initialized notification: {}",
            response.body
        );
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let body = serde_json::json!({
            "jsonrpc": "2.0",
            "id": self.next_id,
            "method": method,
            "params": params
        })
        .to_string();
        self.next_id += 1;
        let response = post_mcp(self.port, self.session_id.as_deref(), &body);
        assert_eq!(
            response.status, 200,
            "{method} over HTTP failed: {}",
            response.body
        );
        body_message(&response)
    }

    fn list_tools(&mut self) -> Vec<String> {
        let message = self.request("tools/list", serde_json::json!({}));
        message["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|tool| tool["name"].as_str().expect("tool name").to_string())
            .collect()
    }

    fn call_tool(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        )
    }
}

// ---------- fixtures ----------

/// TOML literal strings (single quotes) need no path escaping on any
/// platform.
fn workspace_entry(id: &str, root: &std::path::Path, capabilities: &str) -> String {
    format!(
        "[workspaces.{id}]\nroot = '{}'\n{capabilities}\n",
        root.display()
    )
}

/// Run a git command in `dir`, asserting success. Test fixtures only;
/// identity is pinned per-invocation (per-child env), never process-wide.
fn git(dir: &std::path::Path, args: &[&str]) {
    let out = Command::new("git")
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
        "git {args:?} failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
}

// ---------- single mode over HTTP ----------

#[test]
fn http_single_mode_serves_the_v01_surface() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("note.txt"), b"hello over http\n").unwrap();
    let root_arg = tmp.path().to_str().unwrap().to_string();
    let server = HttpServer::start(&[root_arg.as_str()]);
    let mut session = HttpMcpSession::start(&server);

    // Exactly the v0.1 surface — the same list the stdio suite asserts.
    let mut names = session.list_tools();
    names.sort();
    assert_eq!(
        names, SINGLE_MODE_TOOLS,
        "single mode over HTTP must expose the v0.1 surface"
    );

    // A representative read works with the v0.1 response shape and without
    // registry provenance.
    let response = session.call_tool("read_file", serde_json::json!({ "path": "note.txt" }));
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["lines"][0], serde_json::json!("1: hello over http"));
    assert!(
        content.get("workspace").is_none(),
        "single-mode read must not carry provenance over HTTP: {content}"
    );
}

#[test]
fn http_single_mode_mutations_follow_cli_flags() {
    let tmp = tempfile::TempDir::new().unwrap();
    std::fs::write(tmp.path().join("patched.txt"), b"one\ntwo\n").unwrap();
    let root_arg = tmp.path().to_str().unwrap().to_string();
    let server = HttpServer::start(&[root_arg.as_str(), "--write", "--exec"]);
    let mut session = HttpMcpSession::start(&server);

    // apply_patch with the exact v0.1 input: no workspace selector.
    let response = session.call_tool(
        "apply_patch",
        serde_json::json!({
            "patch": "--- patched.txt\n+++ patched.txt\n@@ -1,2 +1,2 @@\n-one\n+ONE\n-two\n+TWO\n"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(
        content["changed_files"][0],
        serde_json::json!("patched.txt")
    );
    assert!(
        content.get("workspace").is_none(),
        "single-mode mutation must not carry provenance over HTTP: {content}"
    );
    assert_eq!(
        std::fs::read_to_string(tmp.path().join("patched.txt")).unwrap(),
        "ONE\nTWO\n"
    );

    // run_command runs where permitted, again without provenance.
    let response = session.call_tool(
        "run_command",
        serde_json::json!({ "program": "git", "args": ["--version"] }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["exit_code"], serde_json::json!(0));
    assert!(content.get("workspace").is_none(), "{content}");
}

// ---------- registry mode over HTTP ----------

#[test]
fn http_registry_mode_serves_the_full_capability_set() {
    let tmp = tempfile::TempDir::new().unwrap();
    let alpha = tmp.path().join("alpha");
    std::fs::create_dir(&alpha).unwrap();
    std::fs::write(alpha.join("tracked.txt"), b"original\n").unwrap();
    git(&alpha, &["init", "--quiet"]);
    git(&alpha, &["add", "."]);
    git(&alpha, &["commit", "--no-gpg-sign", "--message", "init"]);
    std::fs::write(alpha.join("untracked.txt"), b"").unwrap();
    let locked = tmp.path().join("locked");
    std::fs::create_dir(&locked).unwrap();
    std::fs::write(locked.join("patch.txt"), b"LOCKED\n").unwrap();

    let config = format!(
        "version = 1\n\n{}{}",
        workspace_entry("alpha", &alpha, "write = true\nexec = true\n"),
        workspace_entry("locked", &locked, "")
    );
    let cfg_path = tmp.path().join("workspaces.toml");
    std::fs::write(&cfg_path, config).unwrap();
    let cfg_arg = cfg_path.to_str().unwrap().to_string();

    let server = HttpServer::start(&["--workspace-config", cfg_arg.as_str()]);
    let mut session = HttpMcpSession::start(&server);

    // Same 9-tool surface over HTTP as over stdio (transport parity).
    let mut names = session.list_tools();
    names.sort();
    assert_eq!(names, REGISTRY_MODE_TOOLS);

    // Discovery.
    let response = session.call_tool("list_workspaces", serde_json::json!({}));
    assert!(response.get("error").is_none(), "{response}");

    // Representative read and Git tool, with provenance.
    let response = session.call_tool(
        "read_file",
        serde_json::json!({ "workspace": "alpha", "path": "tracked.txt" }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("alpha"));

    let response = session.call_tool("git_status", serde_json::json!({ "workspace": "alpha" }));
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("alpha"));
    assert!(
        content["output"]
            .as_str()
            .unwrap()
            .contains("?? untracked.txt"),
        "{content}"
    );

    // Allowed mutation over HTTP: only alpha changes.
    let response = session.call_tool(
        "apply_patch",
        serde_json::json!({
            "workspace": "alpha",
            "patch": "--- tracked.txt\n+++ tracked.txt\n@@ -1,1 +1,1 @@\n-original\n+PATCHED_OVER_HTTP\n"
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("alpha"));

    // Allowed command over HTTP: the child cwd is the selected workspace.
    let response = session.call_tool(
        "run_command",
        serde_json::json!({
            "workspace": "alpha",
            "program": "git",
            "args": ["status", "--short"]
        }),
    );
    assert!(response.get("error").is_none(), "{response}");
    let content = &response["result"]["structuredContent"];
    assert_eq!(content["workspace"], serde_json::json!("alpha"));
    assert!(
        content["stdout"]
            .as_str()
            .unwrap()
            .contains("?? untracked.txt"),
        "{content}"
    );

    // Denied mutation and command: bounded tool errors naming the logical
    // id, no roots, and — as over stdio — no spawn side effect.
    let response = session.call_tool(
        "apply_patch",
        serde_json::json!({
            "workspace": "locked",
            "patch": "--- patch.txt\n+++ patch.txt\n@@ -1,1 +1,1 @@\n-LOCKED\n+X\n"
        }),
    );
    let text = expect_http_tool_error(&response);
    assert!(
        text.contains("Workspace 'locked' does not allow file writes."),
        "{text}"
    );
    assert!(
        !text.contains(tmp.path().to_str().unwrap()),
        "HTTP tool error must not expose roots: {text}"
    );

    let response = session.call_tool(
        "run_command",
        serde_json::json!({
            "workspace": "locked",
            "program": "git",
            "args": ["init", "spawned-anyway"]
        }),
    );
    let text = expect_http_tool_error(&response);
    assert!(
        text.contains("Workspace 'locked' does not allow command execution."),
        "{text}"
    );
    assert!(
        !locked.join("spawned-anyway").exists(),
        "denied command spawned a process over HTTP"
    );

    // The session survived the denials.
    let response = session.call_tool(
        "read_file",
        serde_json::json!({ "workspace": "locked", "path": "patch.txt" }),
    );
    assert!(
        response.get("error").is_none(),
        "session must survive denials: {response}"
    );

    // Workspace state is exactly what the successful calls made it.
    assert_eq!(
        std::fs::read_to_string(alpha.join("tracked.txt")).unwrap(),
        "PATCHED_OVER_HTTP\n"
    );
    assert_eq!(
        std::fs::read_to_string(locked.join("patch.txt")).unwrap(),
        "LOCKED\n"
    );
}

// ---------- loopback-only bind (security regression) ----------

#[test]
fn http_binary_refuses_non_loopback_bind() {
    let tmp = tempfile::TempDir::new().unwrap();
    let root_arg = tmp.path().to_str().unwrap().to_string();
    for bad in ["0.0.0.0", "192.168.1.10"] {
        let out = Command::new(env!("CARGO_BIN_EXE_nian-workspace"))
            .args([
                "--transport",
                "http",
                "--host",
                bad,
                "--port",
                &free_port().to_string(),
                root_arg.as_str(),
            ])
            .output()
            .expect("run binary");
        assert!(
            !out.status.success(),
            "binding {bad} must be refused by the binary itself"
        );
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            stderr.contains("loopback"),
            "rejection for {bad} must explain the loopback policy: {stderr}"
        );
    }
}
