# nian-workspace

A secure local workspace bridge for web-hosted AI clients using MCP.

`nian-workspace` lets a remote or web-hosted AI work with a local coding project through the Model Context Protocol — without exposing an unauthenticated public filesystem or shell service. A single process serves one local workspace root, providing file inspection, search, edits, controlled command execution, and Git access, always scoped to the configured directory. Alternatively, `--workspace-config` starts in registry mode over a fixed set of operator-configured workspaces; that mode serves the same full capability set — workspace discovery, file access, search, Git inspection, patching, and controlled command execution — every request selecting one workspace by logical ID, with mutation and execution gated by that workspace's own configured permissions.

The reference integration is **ChatGPT + Secure MCP Tunnel**, which connects a web-hosted AI client to a local workspace without requiring the machine to accept inbound network connections.

```text
ChatGPT / web-hosted AI
        |
        | MCP
        v
 Secure MCP Tunnel
        |
        v
  nian-workspace
        |
        +-- files / search
        +-- patches
        +-- commands
        +-- Git
        |
        v
  local workspace
```

Standard MCP compatibility also allows compatible local MCP clients to use `nian-workspace` as a direct stdio backend — see [Local MCP clients (stdio)](#local-mcp-clients-stdio) — but the primary design target is bridging a local workspace to a web-hosted AI through a secure tunnel.

> **What it is not:** `nian-workspace` is not an AI agent.
> The MCP client is the agent. `nian-workspace` is the local execution and
> workspace capability layer it operates through.

## Install

Requires Rust 1.98 or newer.

### Build or install from source

```bash
git clone https://github.com/QuocDuong16/nian-workspace.git
cd nian-workspace

# Install into Cargo's binary directory (normally ~/.cargo/bin).
cargo install --path . --locked

# Or build an optimized binary without installing it.
cargo build --release --locked
# Linux/macOS: target/release/nian-workspace
# Windows:     target/release/nian-workspace.exe
```

Tagged releases also publish checksummed archives for the platforms the current Forgejo runner can genuinely link. See [Platform support and CI honesty](#platform-support-and-ci-honesty) for the distinction between native, cross-compiled, and compile-only targets.

## Usage

```
nian-workspace [WORKSPACE] [OPTIONS]
```

| Flag | Effect |
|---|---|
| `--write` | Allow modifying workspace files (`apply_patch`) |
| `--exec` | Allow executing local programs (`run_command`) |
| `--allow-shell` | Allow commands through a system shell; requires `--exec` |
| `--workspace-config <PATH>` | Load a versioned TOML workspace registry instead of a single workspace — see [Workspace registry configuration (v0.2)](#workspace-registry-configuration-v02) |
| `--transport <stdio\|http>` | MCP transport (default: `stdio`) |
| `--host <HOST>` | HTTP bind host — loopback only (`127.0.0.1`, `::1`, or `localhost`); non-loopback addresses are rejected |
| `--port <PORT>` | HTTP port (default: `8787`) |
| `--log-level <LEVEL>` | `error`, `warn`, `info`, `debug` (or set `RUST_LOG`) |

Permissions are cumulative and conservative: read-only by default; nothing is promoted silently at runtime.

### Permission progression

```bash
nian-workspace .                               # read-only
nian-workspace . --write                       # + apply_patch
nian-workspace . --write --exec                # + run_command (no shell)
nian-workspace . --write --exec --allow-shell  # + shell syntax (cmd.exe / /bin/sh)
```

### Workspace registry configuration (v0.2)

Instead of a single positional workspace root, `--workspace-config <PATH>` loads an explicitly operator-configured registry of named workspace contexts:

```toml
version = 1

[workspaces.nian-vision]
root = "/home/user/Workspace/nian-vision"
write = true
exec = true
allow_shell = false

[workspaces.nian-home]
root = "/home/user/Workspace/nian-home"
```

Registry roots must be **absolute paths**; a relative root is rejected at startup so the policy cannot depend on the directory the server was started from.

Workspace IDs may contain dots (`.`). TOML dotted table syntax would split such an id into nested tables, so quote it in the header:

```toml
[workspaces."project.v2"]
root = "/home/user/Workspace/project-v2"
```

The security properties of this configuration are fixed by design:

- **Roots are explicitly operator-configured** and canonicalized once at startup; they are never chosen, switched, or supplied by MCP requests.
- **Roots are fixed for the lifetime of the process** — the registry is immutable after startup (no runtime add/remove/reload).
- **Roots may not overlap**: duplicate roots (the same directory reached through different spellings, including symlink aliases and case-variant names on case-insensitive filesystems) and nested roots are rejected at startup in both directions, so a broader writable workspace cannot bypass a narrower read-only one. Comparison uses OS filesystem identity, not path strings.
- **Permissions are per workspace and conservative by default**: `write`, `exec`, and `allow_shell` default to `false`; read access is implicit; `allow_shell = true` requires `exec = true`.
- **Workspace IDs are validated logical names** (`[a-z0-9][a-z0-9._-]{0,63}`) — lowercase, 1–64 characters, no path semantics, no aliases, no case folding.
- **Registry size is bounded**: a configuration may declare at most **64** workspaces. `list_workspaces` is the authoritative discovery mechanism and is never truncated or paginated, so the bound is enforced at startup instead — even worst-case discovery output (maximum-length IDs, all permissions) stays in the low tens of kilobytes.
- Unknown or misspelled configuration fields are rejected rather than silently ignored.

`--workspace-config` is mutually exclusive with a positional `WORKSPACE` root and with the `--write`/`--exec`/`--allow-shell` flags; combining them is rejected at startup. Transport and logging options are unchanged.

**Registry-mode MCP tools (v0.2 M5): the full v0.1 tool set, with per-workspace capabilities.** Registry mode serves the complete single-workspace tool set plus discovery — exactly nine tools, each (except `list_workspaces`) selecting its workspace with a required logical `workspace` argument:

| Tool | Arguments | Effect |
|---|---|---|
| `list_workspaces` | none | The configured logical workspace IDs in deterministic ID order, each with its effective permissions. These IDs are the only valid workspace selectors; they are fixed by the operator at startup. |
| `workspace_info` | `workspace` (required) | Metadata for one selected workspace: effective permissions and Git repository status. |
| `list_files` | `workspace` (required) + the single-mode `list_files` arguments | Bounded-depth listing of one selected workspace; paths are workspace-relative. |
| `read_file` | `workspace` (required) + the single-mode `read_file` arguments | Bounded, line-numbered text read from one selected workspace; binary files are rejected. |
| `search` | `workspace` (required) + the single-mode `search` arguments | Bounded regex/literal search across one selected workspace; matches carry workspace-relative paths. |
| `git_status` | `workspace` (required) | Working-tree status (`git status --short --branch` equivalent) scoped to the selected workspace — including when it sits inside a larger parent repository. |
| `git_diff` | `workspace` (required) + the single-mode `git_diff` arguments (`staged`, `path`) | Unstaged or staged diff scoped to the selected workspace, optionally limited to one workspace-relative path. |
| `apply_patch` | `workspace` (required) + the single-mode `apply_patch` arguments | Unified-diff patching of the selected workspace. Requires that workspace's `write = true`; denied workspaces are rejected before anything is parsed or changed. |
| `run_command` | `workspace` (required) + the single-mode `run_command` arguments | Direct process (or shell) execution inside the selected workspace. Requires that workspace's `exec = true`; `shell = true` additionally requires `allow_shell = true`. Denied workspaces never spawn a process. |

- The `workspace` argument must be an exact, operator-configured workspace ID — no case folding, no aliases, no path interpretation, no default or fallback workspace. Unknown or malformed IDs are rejected with a bounded explicit error; `list_workspaces` is the recovery path.
- **Mutation and execution are gated per workspace, not globally**: `apply_patch` requires the selected workspace's `write = true`, `run_command` requires its `exec = true`, and `shell = true` requires `allow_shell = true` (which itself requires `exec = true`). These checks run before any patch is parsed, any file is touched, or any process is spawned, and they are enforced independently for every request — one workspace's capabilities never promote another's.
- Registry tools go through the same hardened workspace resolver as single-workspace mode: `../` traversal, absolute paths outside the selected root, and symlinks that resolve outside it are rejected — including toward other registered workspaces, which remain outside each other's roots. This covers patch targets and `run_command`'s `cwd`; `run_command` is **not an OS sandbox** (see [Security](#security)).
- Registry Git tools are read-only and workspace-scoped: output covers only the selected workspace even when Git discovers a larger parent repository above it, and `git_diff` pathspecs go through the same workspace resolver as every other path.
- Registry-mode responses carry the selected workspace's logical ID as provenance and server-generated metadata and errors never contain filesystem roots or the configuration path. Child process output (`run_command` stdout/stderr) is the program's own output and is deliberately not sanitized.
- There is no mutable "current workspace": every request carries its own explicit ID, and concurrent calls for different workspaces are independent.
- Tools that are not part of this surface are not registered on the registry-mode router at all, so directly invoking one is rejected as a clean `tool not found` MCP error while the server stays usable.

## Tools

The tool surface is mode-specific: single-workspace mode (a positional `WORKSPACE`) advertises exactly the table below with its v0.1 schemas, while registry mode (`--workspace-config`) advertises the same tool set plus `list_workspaces`, every tool except `list_workspaces` taking a required logical `workspace` argument and mutation/execution gated by the selected workspace's configured capabilities (see [the registry section](#workspace-registry-configuration-v02)).

| Tool | Read-only | Notes |
|---|:-:|---|
| `workspace_info` | ✔ | Root, name, permissions, Git branch |
| `list_files` | ✔ | Bounded depth, glob filter, skips `.git`/`node_modules`/`target`/… |
| `read_file` | ✔ | 1-based line ranges, binary detection, bounded output |
| `search` | ✔ | Regex or literal, capped results, every match carries its workspace-relative path. `.git`/`.hg`/`.svn` are never searched — not even through symlink aliases. Hidden and generated dirs are searched only when the requested path itself enters that territory (e.g. `path=".config"`, `path="node_modules"`); rooting at `src` does not unlock `src/.hidden` or `src/node_modules`. |
| `git_status` | ✔ | `git status --short --branch` equivalent, paths relative to the workspace root |
| `git_diff` | ✔ | Unstaged or staged diff, optional path filter, bounded; paths relative to the workspace root so output feeds `apply_patch` directly, even for workspaces nested inside a larger repository |
| `apply_patch` | ✗ needs `--write` | Unified diff (`diff -u` / `git diff`). All hunks are validated before mutation, and each individual file replacement is atomic; an unexpected filesystem failure during the commit phase may leave a multi-file patch partially applied. New-file creation via `/dev/null` headers; renames/deletions rejected. Preserves existing newline style (LF/CRLF) and POSIX permission bits. |
| `run_command` | ✗ needs `--exec` | Direct process execution (`program` + `args`), no shell interpolation. Optional `shell:true` requests need `--allow-shell`. `timeout_seconds` bounds the direct command process; expiry terminates the whole process tree (Unix and Windows). stdout/stderr are capped, output emitted before the timeout is preserved up to the caps, and a descendant holding the pipes after a successful exit can neither fake a timeout nor block the result. |

## Client setup

One process owns one fixed workspace (or, with `--workspace-config`, one fixed set of operator-configured workspaces). This is deliberate: the workspace boundary is fixed when the process starts instead of being switched by an MCP request.

### ChatGPT + Secure MCP Tunnel

This is the primary integration path. ChatGPT connects to remote MCP servers rather than directly spawning a local stdio process. Secure MCP Tunnel bridges a local `nian-workspace` stdio command to ChatGPT without exposing an unauthenticated HTTP listener to the public internet.

Create or reuse a Secure MCP Tunnel, then create a **Restricted Runtime API Key** for `tunnel-client` with only **Tunnels: Read** and **Tunnels: Use**. Do not use an Admin API key for the daemon.

When the tunnel is intended for ChatGPT, assign it to the ChatGPT workspace that will use the connector; otherwise it may not appear in the Tunnel picker.

Export the runtime API key and tunnel ID before configuring the local profile:

```bash
export CONTROL_PLANE_API_KEY='sk-...'
export CONTROL_PLANE_TUNNEL_ID='tunnel_...'

tunnel-client init \
  --sample sample_mcp_stdio_local \
  --profile nian-workspace-my-project \
  --tunnel-id "$CONTROL_PLANE_TUNNEL_ID" \
  --mcp-command "/absolute/path/to/nian-workspace /absolute/path/to/my-project --write --exec"

tunnel-client doctor \
  --profile nian-workspace-my-project \
  --explain

tunnel-client run \
  --profile nian-workspace-my-project
```

`CONTROL_PLANE_API_KEY` authenticates `tunnel-client` to the OpenAI tunnel control plane. It is **not** MCP authentication for `nian-workspace` itself; `nian-workspace` does not implement OAuth or bearer-token MCP authentication.

In ChatGPT, configure the custom MCP app/connector using the tunnel directly:

```text
Settings
  → Connectors / custom MCP app
  → Connection: Tunnel
  → select the tunnel / tunnel ID
  → Authentication: None
```

The same tunnel can be reused for several local projects by creating several profiles with the same tunnel ID and a different `--mcp-command` workspace path. Only **one backend should actively own a given tunnel at a time**. Stop the currently running profile before starting another profile that reuses the same tunnel. A profile switch changes which `nian-workspace` process is reachable; it does not make one process serve multiple roots.

#### Long-running stdio backend: serialize concurrent MCP calls

When `tunnel-client` forwards to a single long-lived `nian-workspace` stdio process, set `max_concurrent_requests` to `1` under the `mcp:` section of the profile configuration:

```yaml
mcp:
  max_concurrent_requests: 1
```

`tunnel-client` itself supports concurrent MCP execution. When it forwards to a single long-lived `nian-workspace` stdio backend, repeated `tools/call` 502 failures have been observed during long-running tunnel sessions. Setting `max_concurrent_requests: 1` serializes MCP calls through the shared stdio backend and has proven more reliable in actual use, avoiding those failures. The precise root cause has not been established.

This is a **runtime interoperability recommendation** for the `tunnel-client` + stdio deployment — it is not a protocol requirement of `nian-workspace` itself.

The field is added to the existing `mcp:` mapping generated by `tunnel-client init` and must not replace the existing `commands:` entry:

```yaml
# ~/.config/tunnel-client/nian-workspace-my-project.yaml
# ...
control_plane:
  tunnel_id: "tunnel_..."
  api_key: env:CONTROL_PLANE_API_KEY

mcp:
  commands:
    - channel: main
      command: "/absolute/path/to/nian-workspace /absolute/path/to/my-project --write --exec"
  max_concurrent_requests: 1
```

To set it per-run without editing the profile file, use the environment variable equivalent:

```bash
MCP_MAX_CONCURRENT_REQUESTS=1 \
  tunnel-client run --profile nian-workspace-my-project
```

#### Troubleshooting: tunnel-client MCP tools/call failures

If `tunnel-client` MCP `tools/call` requests fail with the following pattern:

```json
{
  "status_code": 502,
  "failure_source": "client_internal",
  "upstream_response_received": false
}
```

first confirm the `nian-workspace` stdio backend itself is healthy (check that the process is still running and responds to `tunnel-client doctor --profile <profile>`). If the backend is healthy and the failure recurs, the recommended first mitigation is to set `max_concurrent_requests: 1` (as described above) or pass the equivalent environment variable. This does not guarantee the issue is resolved. If failures continue, inspect the `tunnel-client` and `nian-workspace` logs for additional diagnostics.

### Local MCP clients (stdio)

Any MCP-compatible local client can use `nian-workspace` as a direct stdio backend. This is supported and useful, but it is a secondary use case — the primary design target is the web-hosted AI + Secure MCP Tunnel path described above.

Point your client's MCP config at the installed binary and choose the project root in `args`:

```json
{
  "mcpServers": {
    "nian-workspace": {
      "command": "/usr/local/bin/nian-workspace",
      "args": ["/absolute/path/to/project", "--write", "--exec"]
    }
  }
}
```

Use an absolute binary path if the client does not inherit your shell `PATH`. Add `--allow-shell` only when the client genuinely needs shell syntax; `--exec` alone is safer for ordinary process execution.

Logs go to stderr; the protocol runs on stdout, so stderr redirection in a wrapper script will not corrupt the session.

### Streamable HTTP

For clients that can reach the machine directly through a trusted local/private path, run the built-in loopback-only Streamable HTTP transport:

```bash
nian-workspace . --write --exec --transport http --host 127.0.0.1 --port 8787
```

The MCP endpoint is then served at:

```text
http://127.0.0.1:8787/mcp
```

`nian-workspace` deliberately refuses non-loopback HTTP binds and does not implement public-network authentication. For remote access, put a secure tunnel or another authenticated TLS layer in front of `127.0.0.1`; do not punch a public port through to it.

## Security

Read this before enabling flags.

- **`nian-workspace` is not an OS sandbox.** Workspace isolation prevents *filesystem tools* from addressing paths outside the configured root; command execution runs real local processes with the full permissions of your OS user. A command can trivially read and modify files anywhere on your system regardless of the workspace boundary — this applies in registry mode too, where only `run_command`'s `cwd` is workspace-restricted while the spawned program is not sandboxed and its stdout/stderr is arbitrary program output that may contain absolute host paths. Enable `--exec` (or a workspace's `exec = true` in registry mode) only for MCP clients you trust.
- **Workspace isolation** covers every filesystem-facing tool (`list_files`, `read_file`, `search`, `apply_patch`, `run_command` cwd, `git_diff` path). Requests containing `../` traversal, absolute paths outside the root, drive-letter tricks, or symlinks that resolve outside the root are rejected with explicit errors — including paths whose final component does not exist yet.
- **Registry mode is startup-configured with per-workspace capabilities.** MCP requests select workspaces by operator-configured logical ID — never by path — and server-generated registry metadata and errors never contain filesystem roots; workspace responses carry the logical ID as provenance instead. Registry tools go through the same hardened workspace resolver as single-workspace mode, and registry Git output stays scoped to the selected workspace even when a larger parent repository is discovered. `apply_patch` requires the selected workspace's `write` capability and `run_command` its `exec` capability (`shell = true` additionally requires `allow_shell`), each enforced before anything is parsed or spawned. Tools not registered on a mode's router are not merely hidden from `tools/list`: they are rejected inside the router as `tool not found`, before any handler or workspace is touched.
- **`--allow-shell` is a separate flag because shell execution is strictly more dangerous**: `/bin/sh` (Unix) or `cmd.exe` (Windows) interprets the whole command line, enabling chaining, redirection, and expansion. It also requires `--exec`.
- **Outputs are bounded** (~256 KiB per channel by default) with truncation metadata, so large logs and dumps cannot flood model context.
- **HTTP mode is loopback-only.** Non-loopback bind addresses (`0.0.0.0`, LAN IPs) are rejected at startup: there is no authentication layer, so anything that can reach the port can use the enabled tools against your filesystem. For remote access, put an external secure tunnel (TLS/auth) in front of `127.0.0.1`.
- **Command output is truly bounded**: stdout/stderr are retained up to their caps and everything past them is discarded in fixed-size chunks; a timeout terminates the entire process tree (process group on Unix, Job Object on Windows), best-effort.
- **Git tools are hardened for read-only use**: `GIT_DIR`/`GIT_WORK_TREE`/`GIT_INDEX_FILE`/`GIT_EXTERNAL_DIFF` and similar environment redirection is stripped per invocation, pagers are disabled, and repository- or user-configured external diff, textconv, and fsmonitor execution paths are turned off (`--no-ext-diff`, `--no-textconv`, `-c core.fsmonitor=false`).

No security guarantees beyond these mechanisms are made. Do not overstate them in deployments.

## Development

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

CI runs the same gates on Forgejo (see `.forgejo/workflows/quality.yml`), pinned to Rust 1.98.0 in a Linux container — matching `rust-toolchain.toml`, so local builds and CI use the same toolchain. `.forgejo/workflows/release.yml` handles release packaging when a `v*` tag is pushed.

### Platform support and CI honesty

The current Forgejo runner is an x86_64 Linux Docker runner. Release coverage is intentionally split by what the runner can actually do:

| Platform | CI validation | Release artifact | Notes |
|---|---|---|---|
| Linux x86_64 (`x86_64-unknown-linux-gnu`) | Native tests + native release build + binary smoke test | Yes | Runtime-tested on the runner OS/architecture. |
| Linux arm64 (`aarch64-unknown-linux-gnu`) | Cross-target `cargo check` + cross-linked release build | Yes | Linked with the Debian AArch64 GNU cross-toolchain; not executed in CI. |
| Windows x86_64 GNU (`x86_64-pc-windows-gnu`) | Cross-linked release build | Yes | Produces a real PE executable with MinGW-w64; not executed in CI. |
| Windows x86_64 MSVC (`x86_64-pc-windows-msvc`) | Cross-target `cargo check` only | No | A Linux Docker runner does not provide the native MSVC linker/runtime. |
| macOS x86_64 (`x86_64-apple-darwin`) | Cross-target `cargo check` only | No | Linking requires an Apple SDK/toolchain or a native macOS runner. |
| macOS arm64 (`aarch64-apple-darwin`) | Cross-target `cargo check` only | No | Linking requires an Apple SDK/toolchain or a native macOS runner. |

The release workflow publishes `nian-workspace-vX.Y.Z-<target>` archives plus a `SHA256SUMS` file. The existing quality workflow continues to compile-check Windows MSVC, both macOS targets, and Linux ARM64 exactly as compile-only targets. Native Windows/macOS release binaries should be added only after corresponding native runners (or a properly licensed SDK/toolchain) exist.

## License

MIT. See [LICENSE](LICENSE).
