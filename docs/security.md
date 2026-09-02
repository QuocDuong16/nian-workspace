# Security model

This document describes the security mechanisms `nian-workspace` actually implements — and, just as importantly, what it does not.

**`nian-workspace` is not an OS sandbox.** Workspace isolation prevents *filesystem tools* from addressing paths outside the configured root. Command execution runs real local processes with the full permissions of your OS user. No security guarantees beyond the mechanisms described here are made; do not overstate them in deployments.

- [Filesystem boundary model](#filesystem-boundary-model)
- [Registry-mode boundary](#registry-mode-boundary)
- [Command execution threat model](#command-execution-threat-model)
- [Shell risk](#shell-risk)
- [HTTP loopback restriction](#http-loopback-restriction)
- [Git hardening](#git-hardening)
- [Output and process limits](#output-and-process-limits)

## Filesystem boundary model

Every filesystem-facing tool (`list_files`, `read_file`, `search`, `apply_patch`, `run_command`'s cwd, `git_diff` path filters) resolves paths through a single hardened resolver rooted at the configured workspace root. The resolver performs three ordered stages:

1. **Lexical join** — the requested relative path is joined onto the canonical workspace root.
2. **Physical resolution** — the deepest existing ancestor is canonicalized, so symlinked directories inside the workspace cannot retarget a path outward (this also normalizes alias spellings such as macOS `/var` vs `/private/var`).
3. **Containment verification** — the fully resolved path must sit inside the canonical workspace root, or the request is rejected.

Consequences:

- `../` traversal, absolute paths outside the root, and drive-letter tricks are rejected with an explicit error.
- A symlink that points outside the workspace is rejected — including toward another registered workspace, which remains outside each neighboring workspace's root.
- Paths whose final component does not exist yet are still verified: the resolver canonicalizes the deepest existing ancestor and rejects if the implied path would escape (relevant for patch targets that create files).
- The workspace root itself is canonicalized once at startup; all later decisions compare against that canonical form.

## Registry-mode boundary

Registry mode (see [configuration](configuration.md#registry-mode)) adds these properties:

- **MCP requests never choose filesystem roots.** Workspaces are selected by operator-configured logical ID only; the registry is built and validated entirely at startup and is immutable afterwards (no add/remove/reload/switch).
- **Duplicate and nested roots are rejected at startup** by OS filesystem identity, not path strings — symlink aliases and case-variant spellings cannot smuggle a second registration of the same directory in, and a broader writable workspace cannot bypass a narrower read-only one.
- **Per-workspace capability gates run before any work**: `apply_patch` requires the selected workspace's `write` and `run_command` its `exec` (`shell = true` additionally requires `allow_shell`) — each check runs before any patch is parsed, any file is touched, or any process is spawned, and is enforced independently for every request. One workspace's capabilities never promote another's.
- **Root non-disclosure**: server-generated registry metadata and errors never contain filesystem roots or the configuration path. Registry responses carry the logical workspace ID as provenance, and canonical paths in git diagnostics are re-rendered workspace-relative.
- **Git output stays workspace-scoped** even when the selected workspace sits inside a larger parent repository, and `git_diff` pathspecs go through the same hardened resolver as every other path.

## Command execution threat model

`run_command` spawns real local processes:

- The child runs with **the full permissions of the OS user** running `nian-workspace`. It is *not* confined to the workspace, *not* network-restricted, and *not* resource-limited beyond its own timeout.
- The child may access files outside the workspace, open network connections, and spawn descendants.
- It receives arbitrary arguments; its stdout/stderr is unsanitized program output that may contain absolute host paths or sensitive local data.
- Only `run_command`'s working directory is workspace-restricted (resolved through the same hardened resolver as every other path); the spawned program itself is not sandboxed.

Because of this, enable `exec`/`write`/`shell` — via `--exec`-style flags or per-workspace registry settings — **only for MCP clients you trust**.

Timeout containment: `timeout_seconds` bounds the direct command process; expiry terminates the whole process tree (process group on Unix, Job Object on Windows), best-effort. A descendant that inherits the pipes can neither fake a timeout nor block the result indefinitely; after the direct child exits, a short fixed grace window collects remaining pipe output, then the rest of the tree is terminated.

## Shell risk

`allow_shell` (single mode) / `allow_shell = true` (registry mode) is a separate capability because shell execution is strictly more dangerous than direct execution:

- The platform shell (`cmd.exe /C` on Windows, `/bin/sh -c` on Unix) interprets the whole command line, enabling chaining (`&&`, `;`), redirection, expansion, and quoting games.
- It requires `exec` to be enabled as well — shell execution is a superset of program execution (enforced at startup in both modes).
- Direct execution (the default) passes `args` verbatim to the program with no shell interpolation, which is safer for ordinary use. Add `--allow-shell` only when the client genuinely needs shell syntax.

## HTTP loopback restriction

The Streamable HTTP transport is **loopback-only**:

- Only `127.0.0.1`, `::1`, other `127.x.y.z` literals, and `localhost` (resolved through the system resolver; every resolved address must be loopback) are accepted as `--host`. Anything else is rejected at startup.
- There is **no authentication layer**: anything that can reach the port can use every enabled tool against your filesystem. Exposing the port beyond loopback would hand filesystem, patch, and command execution to anything on the network.
- For remote access, put an external secure tunnel or another authenticated TLS layer in front of `127.0.0.1`; do not punch a public port through to it. The reference setup is [Secure MCP Tunnel](../README.md#secure-mcp-tunnel).

## Git hardening

Git tools (`git_status`, `git_diff`, and the branch probe in `workspace_info`) are hardened for read-only use. Each invocation:

- strips repository/user-controllable environment redirection per child process — `GIT_DIR`, `GIT_WORK_TREE`, `GIT_INDEX_FILE`, `GIT_OBJECT_DIRECTORY`, `GIT_ALTERNATE_OBJECT_DIRECTORIES`, `GIT_EXTERNAL_DIFF`, `GIT_PAGER`, `GIT_EDITOR`, `GIT_SEQUENCE_EDITOR`, `GIT_PAGER_IN_USE`;
- disables pagers by flag, config, and environment (`--no-pager`, `pager.*=false`, `core.pager=cat`, `PAGER=cat`);
- turns off repository- or user-configured execution paths: `--no-ext-diff` and `--no-textconv` on `diff` callers, and `-c core.fsmonitor=false` so Git cannot spawn an fsmonitor hook;
- sets `GIT_OPTIONAL_LOCKS=1` (avoid taking background locks) and `GIT_TERMINAL_PROMPT=0` (never prompt for credentials).

These settings prevent configured hooks, external diff drivers, textconv filters, and fsmonitor daemons from executing programs under Git's name, and keep the invocations non-interactive and read-only in intent.

## Output and process limits

Every potentially large output is bounded by default; responses report truncation rather than flooding model context.

| Limit | Default |
|---|---|
| `read_file` response | 256 KiB |
| `read_file` default window / start-only span | 2,000 / 1,000 lines |
| `read_file` single source line (internal ceiling) | 1 MiB |
| `list_files` entries | 2,000 |
| `search` results (default / cap) | 100 / 1,000 |
| `search` matched line kept | 300 bytes |
| `run_command` stdout / stderr cap | 256 KiB each |
| `run_command` timeout (default / max) | 120 s / 3,600 s |
| `git_status` / `git_diff` output | 256 KiB |

Mechanics:

- Command output is truly bounded: stdout/stderr are retained up to their caps and everything past them is discarded in fixed-size chunks; output emitted before a timeout is preserved up to the caps.
- `read_file` reads with a per-line internal byte ceiling, so a pathological multi-gigabyte single-line file cannot drive unbounded allocation; oversized lines are consumed, clipped, and flagged.
- `list_workspaces` is never truncated or paginated; registry size is instead bounded at startup (at most 64 workspaces), keeping worst-case discovery output in the low tens of kilobytes.
