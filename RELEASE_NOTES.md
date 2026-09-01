# v0.2.0

v0.2.0 adds multi-workspace registry mode on top of the v0.1 single-workspace MCP server, with no change to existing v0.1 behavior.

## Multi-workspace registry

- New explicit registry mode: `nian-workspace --workspace-config /path/to/workspaces.toml`.
- One process serves a fixed set of operator-configured workspaces; the registry is immutable after startup.
- MCP clients select workspaces through logical WorkspaceId values, never filesystem roots.

## Registry discovery

- New tool: `list_workspaces` — returns the configured logical workspace IDs in deterministic order, each with its effective permissions.
- Registry mode exposes nine tools total: `list_workspaces`, `workspace_info`, `list_files`, `read_file`, `search`, `git_status`, `git_diff`, `apply_patch`, `run_command`. Every tool except `list_workspaces` takes a required logical `workspace` argument.

## Per-workspace capabilities

- Each configured workspace independently controls `write`, `exec`, and `allow_shell`; defaults remain conservative (`false`).
- Read access is implicit. Git inspection (`git_status`, `git_diff`) is read-only and does not require `write` or `exec`.

## Isolation

- Roots are explicitly operator-configured and fixed for the lifetime of the process.
- Every request is routed by WorkspaceId; MCP requests never supply filesystem roots.
- `../` traversal, absolute-path escapes, and symlinks that resolve outside the selected root are rejected.
- Overlapping or nested registered roots are rejected at startup — including symlink aliases and case-variant spellings, compared by OS filesystem identity, so a broader writable workspace cannot bypass a narrower read-only one.
- `git_status`/`git_diff` output remains scoped to the selected workspace even when the workspace sits inside a larger parent Git repository.
- Process execution is not filesystem sandboxed: only `run_command`'s working directory is workspace-restricted.

## Mutation and execution

- `apply_patch` routes through the selected workspace and requires that workspace's `write` capability.
- `run_command` routes through the selected workspace and requires `exec`; shell mode additionally requires `allow_shell`.
- `run_command` is NOT an OS sandbox. Only its cwd is workspace-restricted; the spawned program runs with the OS user's privileges and may access resources outside the workspace.

## Compatibility

- Single-workspace mode is unchanged: the existing v0.1 eight-tool MCP surface and the CLI permission model keep working. Existing usage such as `nian-workspace ./project --write --exec` continues to work.

## Transport validation

- v0.2 supports both `stdio` and Streamable HTTP transports, with mode-specific tool surfaces validated through real MCP sessions.

## Real integration validation

- The v0.2 registry flow was validated end to end through ChatGPT + OpenAI Secure MCP Tunnel against a real multi-workspace `nian-workspace` backend. This validates that deployment path; it is not a broad certification of any client or platform.
