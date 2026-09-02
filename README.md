# nian-workspace

A secure local workspace bridge for web-hosted AI clients using MCP.

`nian-workspace` lets a remote AI client — such as ChatGPT — work with local coding projects through the Model Context Protocol, without exposing an unauthenticated public filesystem or shell service. One process serves either a single workspace directory or a fixed set of operator-configured workspaces, providing file inspection, search, edits, controlled command execution, and Git access, always scoped to the configured directories.

> **What it is not:** `nian-workspace` is not an AI agent.
> The MCP client is the agent. `nian-workspace` is the controlled local
> workspace capability layer it operates through.

```text
ChatGPT / MCP client
        |
        | MCP
        v
 Secure MCP Tunnel
        |
        v
  nian-workspace
        |
        v
 local coding workspaces
```

## Quick start

1. Install `nian-workspace` — a prebuilt binary from the [GitHub Releases page](https://github.com/QuocDuong16/nian-workspace/releases) needs no Rust toolchain (see [Install](#install)).

2. Describe the workspaces to serve in a TOML registry file, e.g. `workspaces.toml`:

   ```toml
   version = 1

   [workspaces.nian-workspace]
   root = "/home/user/Workspace/nian-workspace"
   write = true
   exec = true

   [workspaces.nian-vision]
   root = "/home/user/Workspace/nian-vision"
   write = true
   ```

3. Run the server (stdio is the default MCP transport):

   ```bash
   nian-workspace --workspace-config /home/user/workspaces.toml
   ```

4. Connect it to ChatGPT through Secure MCP Tunnel — see [Secure MCP Tunnel](#secure-mcp-tunnel).

Once connected, the client calls `list_workspaces` to discover the configured IDs (`nian-workspace`, `nian-vision`) and passes one as the `workspace` argument of every other tool.

Prefer one workspace per process? Skip the TOML file and run `nian-workspace /path/to/project` (read-only by default; see [Basic usage](#basic-usage)).

## Install

Prebuilt, checksummed native archives are published on the [GitHub Releases page](https://github.com/QuocDuong16/nian-workspace/releases). No Rust toolchain is required to run them.

| Platform | Archive |
|---|---|
| Linux x86_64 | `nian-workspace-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `nian-workspace-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 (MSVC) | `nian-workspace-vX.Y.Z-x86_64-pc-windows-msvc.zip` |
| macOS x86_64 | `nian-workspace-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| macOS arm64 | `nian-workspace-vX.Y.Z-aarch64-apple-darwin.tar.gz` |

Each archive contains the binary plus `README.md`, `LICENSE`, and `RELEASE_NOTES.md`. A `SHA256SUMS` file covering all archives is attached to the release for verification. Tagged releases are built and tested natively on each platform — see [docs/release.md](docs/release.md).

### Build from source

Building from source requires a Rust toolchain; Rust 1.98 or newer is supported (pinned in [`rust-toolchain.toml`](rust-toolchain.toml)).

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

## Basic usage

```
nian-workspace [WORKSPACE] [OPTIONS]
```

Without a positional workspace root, the current directory is served. The server starts **read-only**; every capability beyond reading must be enabled explicitly and is never promoted silently at runtime.

| Flag | Effect |
|---|---|
| `--write` | Allow file edits through `apply_patch` |
| `--exec` | Allow direct process execution (`run_command`) |
| `--allow-shell` | Allow shell-mode execution — the system shell interprets the whole command line; requires `--exec` |
| `--workspace-config <PATH>` | Serve a registry of named workspaces instead of one — see [Registry mode](#registry-mode) |
| `--transport <stdio\|http>` | MCP transport (default: `stdio`) |
| `--host <HOST>` / `--port <PORT>` | HTTP bind address — loopback only (default `127.0.0.1:8787`) |
| `--log-level <LEVEL>` | `error`, `warn`, `info`, `debug` (or set `RUST_LOG`) |

Permission progression:

```bash
nian-workspace .                               # read-only
nian-workspace . --write                       # + apply_patch
nian-workspace . --write --exec                # + run_command (no shell)
nian-workspace . --write --exec --allow-shell  # + shell syntax (cmd.exe / /bin/sh)
```

In registry mode the CLI permission flags are not used at all — each workspace's permissions come from the configuration file.

## Registry mode

`--workspace-config <PATH>` loads a versioned TOML registry of named workspaces. One process serves all of them; each MCP request selects one workspace by its logical ID.

```toml
version = 1

[workspaces.nian-workspace]
root = "/home/user/Workspace/nian-workspace"
write = true
exec = true
allow_shell = false

[workspaces.nian-vision]
root = "/home/user/Workspace/nian-vision"
write = true
exec = true
allow_shell = false
```

The operational rules:

- **Roots must be absolute** and must exist and be directories.
- **Workspace IDs are logical names** — lowercase, 1–64 characters (`[a-z0-9][a-z0-9._-]{0,63}`). They are never filesystem paths.
- **Roots are fixed at startup** — the registry is immutable while the process runs; there is no runtime add/remove/reload and no workspace switching.
- **Overlapping or nested roots are rejected** at startup — including the same directory registered twice under different spellings — so a broader writable workspace cannot bypass a narrower read-only one.
- **Permissions are per workspace**: read access is implicit; `write`, `exec`, and `allow_shell` default to `false`.
- **`allow_shell = true` requires `exec = true`**.
- `--workspace-config` is mutually exclusive with a positional `WORKSPACE` and with `--write`/`--exec`/`--allow-shell`.

The full configuration reference — ID grammar and quoting, validation order, and a single-vs-registry comparison — is in [docs/configuration.md](docs/configuration.md).

## Secure MCP Tunnel

This is the primary ChatGPT integration. ChatGPT connects to remote MCP servers rather than spawning local processes; Secure MCP Tunnel bridges a local `nian-workspace` stdio process to ChatGPT without exposing an unauthenticated HTTP listener to the public internet.

**Supported baseline: `tunnel-client` v0.0.14 or newer.**

1. Create (or reuse) a Secure MCP Tunnel and assign it to the ChatGPT workspace that will use the connector; otherwise it may not appear in the tunnel picker.
2. Create a **Restricted Runtime API Key** for `tunnel-client` with only **Tunnels: Read** and **Tunnels: Use**. Do not use an Admin API key for the daemon.
3. Export the key and tunnel ID, initialize a profile, and run it (registry-mode example):

   ```bash
   export CONTROL_PLANE_API_KEY='sk-...'
   export CONTROL_PLANE_TUNNEL_ID='tunnel_...'

   tunnel-client init \
     --sample sample_mcp_stdio_local \
     --profile nian-workspace \
     --tunnel-id "$CONTROL_PLANE_TUNNEL_ID" \
     --mcp-command "/home/user/.local/bin/nian-workspace --workspace-config /home/user/workspaces.toml"

   tunnel-client doctor --profile nian-workspace --explain
   tunnel-client run --profile nian-workspace
   ```

4. In ChatGPT, configure the custom MCP app/connector:

   ```text
   Settings
     → Connectors / custom MCP app
     → Connection: Tunnel
     → select the tunnel / tunnel ID
     → Authentication: None
   ```

Notes:

- `CONTROL_PLANE_API_KEY` authenticates `tunnel-client` to the tunnel control plane. It is **not** MCP authentication for `nian-workspace` itself, which implements no OAuth or bearer-token MCP authentication.
- **Single workspace instead of a registry:** point `--mcp-command` at one workspace root with its flags — `--mcp-command "/path/to/nian-workspace /path/to/my-project --write --exec"` — and keep the rest of the setup identical. To work on several projects this way, create several profiles with the same tunnel ID and run them one at a time.
- **Only one backend should actively own a given tunnel at a time.** Stop the currently running profile before starting another profile that reuses the same tunnel — a profile switch changes which `nian-workspace` process is reachable.
- If tool calls fail, first confirm the `nian-workspace` backend is healthy (`tunnel-client doctor --profile <name>`, process still running) and inspect the `tunnel-client` and `nian-workspace` logs for diagnostics.

### Local MCP clients (stdio)

Any MCP-compatible local client can use `nian-workspace` as a direct stdio backend. This is supported, but the primary design target is the Secure MCP Tunnel path above.

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

Use an absolute binary path if the client does not inherit your shell `PATH`. Logs go to stderr while the protocol runs on stdout, so redirecting stderr will not corrupt a session.

### Streamable HTTP

For clients that can reach the machine over a trusted local path, run the built-in loopback-only HTTP transport:

```bash
nian-workspace . --write --exec --transport http --host 127.0.0.1 --port 8787
```

The MCP endpoint is then served at `http://127.0.0.1:8787/mcp`. Non-loopback binds are refused and there is no built-in authentication — for remote access, put a secure tunnel or another authenticated TLS layer in front of `127.0.0.1`.

## Tools

The tool surface is mode-specific: single-workspace mode exposes exactly the eight tools below with the v0.1 schemas, while registry mode exposes those eight **plus `list_workspaces`** — every tool except `list_workspaces` then takes a required logical `workspace` argument.

| Tool | Purpose | Permission |
|---|---|---|
| `workspace_info` | Workspace metadata: permissions, Git branch | read (implicit) |
| `list_files` | Bounded directory listing | read (implicit) |
| `read_file` | Line-numbered text read | read (implicit) |
| `search` | Regex or literal text search | read (implicit) |
| `git_status` | Working-tree status | read (implicit) |
| `git_diff` | Unstaged or staged diff | read (implicit) |
| `apply_patch` | Apply a unified diff | `write` |
| `run_command` | Run a program directly (`shell = true` for shell syntax) | `exec` (shell mode also needs `allow_shell`) |
| `list_workspaces` | *(registry mode only)* Configured IDs and their permissions | read (implicit) |

The complete reference — argument schemas, output shapes, registry selectors, and bounded-output behavior — is in [docs/tools.md](docs/tools.md).

## Security

**`nian-workspace` is not an OS sandbox.** Read this before enabling `--write`, `--exec`, or `--allow-shell`.

- **Filesystem tools are restricted to the configured roots.** `list_files`, `read_file`, `search`, `apply_patch`, `run_command`'s working directory, and `git_diff` path filters reject `../` traversal, absolute paths outside the root, and symlinks that resolve outside it.
- **Command execution runs real local processes** with your OS user's full permissions. A spawned command can access files outside the workspace, use the network, and spawn descendants. Enable `exec`/`write`/`shell` only for MCP clients you trust.
- **`allow_shell` is a separate, stronger capability** because the shell (`/bin/sh` on Unix, `cmd.exe` on Windows) interprets the entire command line, enabling chaining, redirection, and expansion. It requires `exec`.
- **HTTP mode is loopback-only.** Non-loopback bind addresses are rejected at startup: there is no authentication layer, so anything that can reach the port can use the enabled tools against your filesystem.
- **Outputs are bounded** (~256 KiB per channel by default) with truncation metadata, so large logs and dumps cannot flood model context.

No security guarantees beyond these mechanisms are made. The full security model — filesystem boundary internals, the command-execution threat model, and Git hardening — is in [docs/security.md](docs/security.md).

## Documentation

| Document | Contents |
|---|---|
| [docs/configuration.md](docs/configuration.md) | Registry format, workspace IDs, root validation, permissions, single vs registry mode |
| [docs/tools.md](docs/tools.md) | Complete MCP tool reference: schemas, outputs, provenance |
| [docs/security.md](docs/security.md) | Filesystem boundary model, command-execution threat model, hardening details |
| [docs/architecture.md](docs/architecture.md) | Bridge-not-agent design, server modes, transports, design invariants |
| [docs/development.md](docs/development.md) | Local development, Rust toolchain, CI |
| [docs/release.md](docs/release.md) | Release pipeline, platforms, artifacts |
| [RELEASE_NOTES.md](RELEASE_NOTES.md) | Release history |

## Development

Local quality gates:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

Development changes are validated by CI, and tagged releases are built and tested natively for all supported platforms — see [docs/development.md](docs/development.md) and [docs/release.md](docs/release.md).

## License

MIT. See [LICENSE](LICENSE).
