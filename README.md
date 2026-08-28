# nian-workspace

A minimal, secure local MCP workspace server for AI coding clients.

> **What it is not:** it is not an AI agent.
>
> The MCP client — ChatGPT, Codex, Claude Code, Cursor, or any compatible
> client — is the agent. `nian-workspace` is only the execution and workspace
> capability layer it operates through.

`nian-workspace` exposes one local directory (the *workspace*) to MCP clients so they can inspect files, search code, edit files, run controlled commands, and inspect Git state — always inside the configured root.

Each running `nian-workspace` process serves exactly one workspace root. Start another process (or switch the process/tunnel profile) when you want to expose a different project.

```bash
nian-workspace .
```

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

## Tools

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

One process owns one workspace root. This is deliberate: the workspace boundary is fixed when the process starts instead of being switched by an MCP request.

### stdio (recommended for local clients)

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

### ChatGPT + Secure MCP Tunnel

ChatGPT connects to remote MCP servers rather than directly spawning a local stdio process. Secure MCP Tunnel can bridge a local `nian-workspace` stdio command to ChatGPT without exposing an unauthenticated HTTP listener to the public internet.

After installing and authenticating the Secure MCP Tunnel client, create a profile that starts `nian-workspace` for one project:

```bash
export CONTROL_PLANE_TUNNEL_ID='your-tunnel-id'

tunnel-client init \
  --sample sample_mcp_stdio_local \
  --profile nian-workspace-my-project \
  --tunnel-id "$CONTROL_PLANE_TUNNEL_ID" \
  --mcp-command "/absolute/path/to/nian-workspace /absolute/path/to/my-project --write --exec"

tunnel-client run --profile nian-workspace-my-project
```

Use the MCP endpoint produced by Secure MCP Tunnel when creating the custom MCP app/connector in ChatGPT, then scan the server tools. ChatGPT's exact developer-mode availability and write-action controls depend on the plan and workspace policy, so follow the current ChatGPT UI for enabling the custom app.

The same tunnel can be reused for several local projects by creating several profiles with the same tunnel ID and a different `--mcp-command` workspace path. Only **one backend should actively own a given tunnel at a time**. Stop the currently running profile before starting another profile that reuses the same tunnel. A profile switch changes which `nian-workspace` process is reachable; it does not make one process serve multiple roots.

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

- **`nian-workspace` is not an OS sandbox.** Workspace isolation prevents *filesystem tools* from addressing paths outside the configured root; command execution runs real local processes with the full permissions of your OS user. A command can trivially read and modify files anywhere on your system regardless of the workspace boundary. Enable `--exec` only for MCP clients you trust.
- **Workspace isolation** covers every filesystem-facing tool (`list_files`, `read_file`, `search`, `apply_patch`, `run_command` cwd, `git_diff` path). Requests containing `../` traversal, absolute paths outside the root, drive-letter tricks, or symlinks that resolve outside the root are rejected with explicit errors — including paths whose final component does not exist yet.
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
