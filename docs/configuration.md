# Configuration reference

`nian-workspace` decides its entire runtime model at startup: either a single workspace root with CLI permission flags (v0.1 model), or a versioned TOML registry of named workspaces (v0.2 registry mode). There is no runtime switching between modes and no mutable "current workspace".

- [Single-workspace mode](#single-workspace-mode)
- [Registry mode](#registry-mode)
  - [File format](#file-format)
  - [Workspace IDs](#workspace-ids)
  - [Root validation](#root-validation)
  - [Permissions](#permissions)
  - [Validation order and limits](#validation-order-and-limits)
- [Choosing between the modes](#choosing-between-the-modes)

## Single-workspace mode

```
nian-workspace [WORKSPACE] [OPTIONS]
```

The positional `WORKSPACE` is one directory; when omitted, the current directory is served. The root is canonicalized once at startup and fixed for the lifetime of the process.

Permissions come from CLI flags:

| Flag | Unlocks | Default |
|---|---|---|
| *(none)* | read access (implicit) | ✔ |
| `--write` | `apply_patch` on the workspace | off |
| `--exec` | direct `run_command` on the workspace | off |
| `--allow-shell` | shell-mode `run_command` (`shell = true`); **requires `--exec`** | off |

`--allow-shell` without `--exec` is rejected at startup: shell execution is a superset of program execution.

All other CLI options (`--transport`, `--host`, `--port`, `--log-level`) are described in the [README](../README.md#basic-usage).

## Registry mode

```
nian-workspace --workspace-config /path/to/workspaces.toml
```

One process serves every configured workspace. Each MCP request selects one workspace by its logical ID — never by path — and mutation/execution are gated by that workspace's own configured permissions.

### File format

```toml
version = 1

[workspaces.nian-workspace]
root = "/home/user/Workspace/nian-workspace"
write = true
exec = true
allow_shell = false

[workspaces."project.v2"]
root = "/home/user/Workspace/project-v2"
```

| Field | Type | Meaning |
|---|---|---|
| `version` | integer | Must be exactly `1`. Other versions are rejected. |
| `workspaces` | table | One `[workspaces.<id>]` entry per workspace; at least one is required. |
| `root` | string | Absolute path to the workspace directory. Required. |
| `write` | boolean | `apply_patch` allowed (default `false`). |
| `exec` | boolean | direct `run_command` allowed (default `false`). |
| `allow_shell` | boolean | shell-mode `run_command` allowed (default `false`); requires `exec = true`. |

Deserialization is **strict**: unknown or misspelled fields are rejected rather than silently ignored, so a typo such as `wriet = true` cannot quietly disable a capability. A workspace literally named with a dot (e.g. `project.v2`) must be quoted in the TOML header — unquoted dotted headers split into nested tables and are rejected as an unknown field.

### Workspace IDs

A workspace ID is a pure logical name chosen by the operator. Grammar: `[a-z0-9][a-z0-9._-]{0,63}` — 1–64 ASCII characters, starting with a lowercase letter or digit.

- Uppercase, leading `.`, leading `-`, path separators, traversal segments, whitespace, and non-ASCII characters are rejected.
- IDs have **no path semantics, no aliases, and no case folding**: two IDs are the same only if their strings are equal.
- MCP clients select workspaces with these exact strings; the same grammar is advertised in each tool's `workspace` argument schema so clients can validate before sending.

### Root validation

Every root is validated fully before the server starts serving:

- **Absolute paths only.** A relative root is rejected so the security policy cannot depend on the directory the server was started from.
- The root must exist, must be a directory, and must canonicalize successfully.
- **Duplicate roots are rejected** — the same directory reached through different spellings, including symlink aliases and case-variant names on case-insensitive filesystems. Comparison uses OS filesystem identity (device + inode on Unix, volume serial + file index on Windows), never path strings.
- **Nested/overlapping roots are rejected in both directions** using real filesystem ancestry: a workspace may not sit inside another registered workspace. This prevents a broader writable workspace from bypassing a narrower read-only one. Distinct sibling directories (e.g. `project` and `project-other`) are accepted — the ancestor chain of one never passes through the other.
- A root whose filesystem identity cannot be probed aborts startup instead of being silently treated as "different".

The validated registry is **immutable**: roots, IDs, and permissions never change while the process runs. No add/remove/reload API exists.

### Permissions

- **Read access is implicit**: every configured workspace is readable, and the Git read tools (`git_status`, `git_diff`) work on every workspace regardless of `write`/`exec`.
- `write`, `exec`, and `allow_shell` default to `false` and gate, respectively: `apply_patch`, direct `run_command`, and shell-mode `run_command`.
- `allow_shell = true` requires `exec = true` — enforced at startup.
- Permissions are **per workspace and enforced per request**: one workspace's capabilities never promote another's.
- The single-mode `--write`/`--exec`/`--allow-shell` flags are rejected together with `--workspace-config` rather than promoted onto every configured workspace.

### Validation order and limits

Startup fails before serving if any check fails, in this order:

1. `version` exists and equals `1`;
2. at least one workspace is declared;
3. at most **64** workspaces are declared — `list_workspaces` has no pagination and is never truncated, so registry size is bounded up front (worst-case discovery output stays in the low tens of kilobytes, inside the server's ~256 KiB bounded-output envelope);
4. every workspace ID is valid;
5. every root is absolute;
6. every root exists and is a directory;
7. every root canonicalizes;
8. duplicate roots are rejected (filesystem identity);
9. nested roots are rejected in both directions;
10. `allow_shell = true` requires `exec = true`;
11. unknown/malformed fields fail via strict TOML deserialization.

`--workspace-config` is mutually exclusive with a positional `WORKSPACE` root and with `--write`/`--exec`/`--allow-shell`; combining them is rejected at startup. Transport and logging options are unchanged.

## Choosing between the modes

| | Single-workspace mode | Registry mode |
|---|---|---|
| Invocation | `nian-workspace <root> [flags]` | `nian-workspace --workspace-config <file>` |
| Workspaces served | one fixed root (default: cwd) | a fixed set, each with a logical ID |
| Tools | exactly the eight v0.1 tools | those eight plus `list_workspaces` |
| Selection | none — the one workspace is implicit | required `workspace` argument per call |
| Permissions | CLI flags, one set for the workspace | per-workspace in the TOML file |
| Compatibility | v0.1 behavior unchanged | v0.2 |
