# MCP tool reference

`nian-workspace` exposes a mode-specific tool surface over MCP (stdio or Streamable HTTP):

- **Single-workspace mode** (positional `WORKSPACE`) advertises exactly the eight v0.1 tools with their v0.1 schemas. The served workspace is implicit — no tool takes a workspace selector.
- **Registry mode** (`--workspace-config`) advertises those eight tools **plus `list_workspaces`**. Every tool except `list_workspaces` takes a required logical `workspace` argument, flattened into the same input schema (no nested `args` object).

Tools that are not part of a mode's surface are not registered on that mode's router at all: invoking one (e.g. `list_workspaces` in single-workspace mode) returns a clean `tool not found` MCP error while the server stays usable. Within a mode, all advertised tools are always listed; a capability that is disabled for the workspace (`write`, `exec`, `allow_shell`) produces an explicit permission error from the tool call instead of hiding the tool.

- [Conventions](#conventions)
- [Tools](#tools)
  - [`list_workspaces` (registry mode only)](#list_workspaces-registry-mode-only)
  - [`workspace_info`](#workspace_info)
  - [`list_files`](#list_files)
  - [`read_file`](#read_file)
  - [`search`](#search)
  - [`git_status`](#git_status)
  - [`git_diff`](#git_diff)
  - [`apply_patch`](#apply_patch)
  - [`run_command`](#run_command)
- [Server instructions](#server-instructions)

## Conventions

**Paths.** All `path`, `cwd`, and glob arguments are workspace-relative; absolute or `../` paths that resolve outside the configured root are rejected with an explicit error (see [security](security.md#filesystem-boundary-model)).

**Path presentation.** Single-workspace mode keeps the v0.1 presentation: the workspace root renders as its canonical absolute path, including inside error messages. Registry mode never exposes filesystem roots: paths are workspace-relative, the root itself renders as `"."`, and server-generated metadata and errors never contain canonical roots or the configuration path.

**Provenance.** Registry-mode responses carry the selected workspace's logical ID in a top-level `workspace` field so a client operating across several workspaces can tell results apart. Child process output (`run_command` stdout/stderr) is the program's own output and is deliberately not sanitized.

**Bounded outputs.** Every potentially large output is capped (defaults in [security: output and process limits](security.md#output-and-process-limits)) and reports truncation, so logs and dumps cannot flood model context.

**Selectors.** The registry-mode `workspace` argument must be an exact, operator-configured workspace ID (grammar `[a-z0-9][a-z0-9._-]{0,63}`). There is no case folding, aliasing, path interpretation, or fallback workspace. Unknown or malformed IDs are rejected with a bounded explicit error that does not enumerate the configured IDs; `list_workspaces` is the recovery path.

## Tools

### `list_workspaces` (registry mode only)

Returns the configured logical workspace IDs in deterministic ID order, each with its effective permissions. These IDs are the only valid `workspace` selector values; they are fixed by the operator at startup — there is no runtime registration, workspace switching, aliasing, or path-based selection.

- Arguments: none.
- Output:

```json
{
  "workspaces": [
    { "id": "alpha", "permissions": { "read": true, "write": true, "exec": false, "shell": false } },
    { "id": "beta",  "permissions": { "read": true, "write": false, "exec": false, "shell": false } }
  ]
}
```

### `workspace_info`

Metadata for one workspace: its effective permissions and Git repository status (current branch).

- Arguments: *(registry mode)* `workspace` (required). *(single mode)* none.
- Identity differs by mode: single mode reports the canonical root path and directory name (v0.1 schema, preserved for compatibility); registry mode reports only the logical ID — filesystem paths are never exposed to registry-mode clients.

### `list_files`

Bounded-depth directory listing.

- Arguments:

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `path` | string | *(root)* | Directory to list, relative to the workspace root. |
| `depth` | integer | `2` | How many directory levels to descend (max `10`). |
| `include_hidden` | boolean | `false` | Include dotfiles and dot-directories. |
| `glob` | string | — | Optional glob filter on workspace-relative paths (e.g. `*.rs`, `src/*.rs`). |

- Behavior: `.git`, `node_modules`, `target`, and similar generated/VCS directories are pruned entirely; symlinks are reported as `symlink` and not followed. Output is capped (default 2,000 entries) and flagged with `truncated`.
- Output: `{ root, depth, count, truncated, entries: [{ path, type: "file"|"dir"|"symlink", size? }] }` — `size` is present for regular files. `root` echoes the listed directory in the mode's path presentation.

### `read_file`

Bounded, line-numbered text read.

- Arguments:

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `path` | string | — | File path relative to the workspace root. |
| `start_line` | integer | `1` | First line to return (1-based). |
| `end_line` | integer | — | Last line to return (inclusive). With only `start_line`, a bounded span is returned. |

- Behavior: with no range at all, a default window (2,000 lines) starting at line 1 is returned; with only `start_line`, a 1,000-line span is returned. Binary files are rejected (NUL-byte sniff of the leading bytes). Output is capped at 256 KiB; a pathological single line cannot drive unbounded memory. Non-UTF-8 bytes are replaced lossily and flagged.
- Output: `{ path, start_line, end_line, line_count, truncated, has_more_lines, lossy_decoding, lines: ["1: …", "2: …"] }` — `has_more_lines` distinguishes "range ended early" from true EOF.

### `search`

Regex or literal text search across the workspace.

- Arguments:

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `query` | string | — | Literal text or regular expression. |
| `path` | string | *(root)* | Directory or file to search, relative to the workspace root. |
| `glob` | string | — | Optional glob filter on file paths (e.g. `*.rs`, `.env*`). |
| `ignore_case` | boolean | `false` | Case-insensitive matching. |
| `literal` | boolean | `false` | Interpret `query` as fixed text instead of a regex. |
| `max_results` | integer | `100` | Maximum matches (capped at 1,000). |

- Visibility policy: VCS metadata (`.git`/`.hg`/`.svn`) is never searched — not even through symlink aliases. Hidden entries and generated directories (`node_modules`, `target`, …) are searched only when the requested `path` itself enters that territory (e.g. `path=".config"`, `path="node_modules"`) or the glob names them explicitly; rooting at `src` does not unlock `src/.hidden` or `src/node_modules`.
- Output: `{ query, path, files_searched, match_count, truncated, matches: [{ path, line, text, clipped }] }` — every match carries its workspace-relative `path`, so results identify their source across files. Match text is clipped per line (default 300 bytes kept) with a `clipped` flag.

### `git_status`

Working-tree status, equivalent to `git status --short --branch`, with paths relative to the workspace root.

- Arguments: *(registry mode)* `workspace` (required). *(single mode)* none.
- Behavior: read-only, works on every workspace regardless of `write`/`exec`. Output is scoped to the selected workspace even when Git discovers a larger parent repository above it. Output is capped at 256 KiB. Git invocations are hardened for read-only use (see [security: Git hardening](security.md#git-hardening)).

### `git_diff`

Unstaged (or staged) diff.

- Arguments:

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `staged` | boolean | `false` | Show staged changes instead of unstaged ones. |
| `path` | string | — | Limit the diff to one workspace-relative path. |

- *(registry mode)* plus the required `workspace` selector.
- Behavior: diff paths are relative to the workspace root so output feeds `apply_patch` directly, even for workspaces nested inside a larger repository. Output is capped at 256 KiB.

### `apply_patch`

Modify files by applying a unified diff (`diff -u` / `git diff` format).

- Arguments:

| Argument | Type | Meaning |
|---|---|---|
| `patch` | string | Unified diff text. Multi-file diffs are applied as one unit. |

- *(registry mode)* plus the required `workspace` selector.
- Permission: requires the workspace's `write` capability (`--write` in single mode, `write = true` in registry mode). A denied workspace is rejected **before** anything is parsed or changed.
- Behavior: all hunks are validated before mutation, and each individual file replacement is atomic (same-directory exclusive temp file + rename); an unexpected filesystem failure during the commit phase may leave a multi-file patch partially applied. New-file creation via `/dev/null` headers is supported; renames and deletions are rejected. Existing newline style (LF/CRLF) and POSIX permission bits are preserved.
- Output: `{ changed_files: [...] }` — the workspace-relative paths of the patched files.

### `run_command`

Execute a program inside the workspace.

- Arguments:

| Argument | Type | Default | Meaning |
|---|---|---|---|
| `program` | string | — | Executable to run, resolved via PATH (e.g. `cargo`). Ignored when `shell = true`. |
| `args` | array of strings | — | Arguments passed verbatim to the program — never interpolated by a shell. |
| `shell` | boolean | `false` | Run `command` through the platform shell (`/bin/sh -c` on Unix, `cmd.exe /C` on Windows). |
| `command` | string | — | Shell command line to execute when `shell = true` (e.g. `cargo check && cargo test`). |
| `cwd` | string | *(root)* | Working directory relative to the workspace root. |
| `timeout_seconds` | integer | `120` | Timeout (max `3600`). The whole process tree is killed on expiry. |

- *(registry mode)* plus the required `workspace` selector.
- Permission: requires `exec` (`--exec` in single mode, `exec = true` in registry mode); `shell = true` additionally requires the shell capability (`--allow-shell` / `allow_shell = true`). Denied workspaces never spawn a process.
- Behavior: direct execution resolves the program through the process PATH without a shell. The timeout bounds the direct command process; expiry terminates the entire process tree (process group on Unix, Job Object on Windows). stdout/stderr are capped at 256 KiB each; output emitted before a timeout is preserved up to the caps; a descendant holding the pipes after a successful exit can neither fake a timeout nor block the result.
- Output: `{ exit_code, stdout, stderr, truncated, duration_ms, timed_out, lossy_decoding, signal? }` — on timeout, `exit_code` is `null`, `timed_out` is `true`, and a `message` explains the termination. `signal` is present when the process was terminated by a signal. stdout/stderr are the program's own, unsanitized output.

## Server instructions

During MCP initialization, the server reports the runtime host in its instructions so clients can pick platform-appropriate command syntax:

- the host OS/architecture (e.g. `Runtime host: linux/x86_64`);
- the shell that `shell = true` actually invokes (`cmd.exe /C` on Windows, `/bin/sh -c` on Unix);
- the fact that direct execution resolves programs through the process PATH.

The environment is never probed: installed programs are not detected or listed, and PowerShell is not implied by `shell = true` on Windows — invoke `pwsh` or `powershell.exe` directly only when available.
