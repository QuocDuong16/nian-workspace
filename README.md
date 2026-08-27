# nian-workspace

A minimal, secure local MCP workspace server for AI coding clients.

> **What it is not:** it is not an AI agent.
>
> The MCP client — ChatGPT, Codex, Claude Code, Cursor, or any compatible
> client — is the agent. `nian-workspace` is only the execution and workspace
> capability layer it operates through.

`nian-workspace` exposes one local directory (the *workspace*) to MCP clients so they can inspect files, search code, edit files, run controlled commands, and inspect Git state — always inside the configured root.

```bash
nian-workspace .
```

## Install

Requires Rust stable.

```bash
cargo install --path .
# or build only:
cargo build --release   # binary at target/release/nian-workspace
```

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
| `--host <HOST>` | HTTP bind host (default: `127.0.0.1`; never bind a public address unintentionally) |
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
| `search` | ✔ | Regex or literal, ripgrep-like semantics, capped results |
| `git_status` | ✔ | `git status --short --branch` equivalent |
| `git_diff` | ✔ | Unstaged or staged diff, optional path filter, bounded |
| `apply_patch` | ✗ needs `--write` | Unified diff (`diff -u` / `git diff`). Atomic per call: failed hunks write nothing. New-file creation via `/dev/null` headers; renames/deletions rejected. |
| `run_command` | ✗ needs `--exec` | Direct process execution (`program` + `args`), no shell interpolation. Optional `shell:true` requests need `--allow-shell`. Timeout kills the process; stdout/stderr are capped. |

## Client setup

### stdio (recommended for local clients)

Point your client's MCP config at the binary:

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

Logs go to stderr; the protocol runs on stdout, so stderr redirection in a wrapper script will not corrupt the session.

### Streamable HTTP

```bash
nian-workspace . --write --exec --transport http --host 127.0.0.1 --port 8787
```

The MCP endpoint is then served at:

```text
http://127.0.0.1:8787/mcp
```

Remote clients such as ChatGPT Web may require a separate secure tunnel or an MCP connector depending on the client. `nian-workspace` does not bundle, manage, or authenticate tunnels — put your own TLS/auth layer in front if you expose the endpoint beyond localhost.

## Security

Read this before enabling flags.

- **`nian-workspace` is not an OS sandbox.** Workspace isolation prevents *filesystem tools* from addressing paths outside the configured root; command execution runs real local processes with the full permissions of your OS user. A command can trivially read and modify files anywhere on your system regardless of the workspace boundary. Enable `--exec` only for MCP clients you trust.
- **Workspace isolation** covers every filesystem-facing tool (`list_files`, `read_file`, `search`, `apply_patch`, `run_command` cwd, `git_diff` path). Requests containing `../` traversal, absolute paths outside the root, drive-letter tricks, or symlinks that resolve outside the root are rejected with explicit errors — including paths whose final component does not exist yet.
- **`--allow-shell` is a separate flag because shell execution is strictly more dangerous**: `/bin/sh` (Unix) or `cmd.exe` (Windows) interprets the whole command line, enabling chaining, redirection, and expansion. It also requires `--exec`.
- **Outputs are bounded** (~256 KiB per channel by default) with truncation metadata, so large logs and dumps cannot flood model context.
- **HTTP mode binds to loopback** unless you explicitly pass another host. There is no authentication layer; anything that can reach the port can use the enabled tools against your filesystem.

No security guarantees beyond these mechanisms are made. Do not overstate them in deployments.

## Development

```bash
cargo fmt --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

CI runs the same gates on Forgejo (see `.forgejo/workflows/quality.yml`), pinned to Rust 1.98.0 in a Linux container — matching `rust-toolchain.toml`, so local builds and CI use the same toolchain.

## License

MIT. See [LICENSE](LICENSE).
