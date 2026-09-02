# Architecture

This document describes how `nian-workspace` is built — the model, the runtime structure, and the design invariants that keep it predictable to maintain and extend.

- [Bridge, not agent](#bridge-not-agent)
- [One process, one mode](#one-process-one-mode)
- [Workspace contexts and the resolver](#workspace-contexts-and-the-resolver)
- [The immutable registry](#the-immutable-registry)
- [Tool cores shared by both modes](#tool-cores-shared-by-both-modes)
- [Transports](#transports)
- [Design invariants](#design-invariants)

## Bridge, not agent

`nian-workspace` is not an AI agent and contains no planning, prompting, or model-calling logic. The MCP client is the agent; `nian-workspace` is the local capability layer it operates through: workspace discovery, file access, search, Git inspection, patching, and controlled command execution.

The security posture follows from that split: the *client* decides what to do, while the *server* decides what is allowed — roots, permissions, and boundaries are fixed by the operator at startup and never chosen by MCP requests.

## One process, one mode

The runtime model is decided entirely at startup and captured in a single runtime mode:

- **`SingleWorkspace`** (v0.1 model): one canonical workspace root plus CLI-derived permissions. Constructed from the positional `WORKSPACE` argument (default: the current directory).
- **`WorkspaceRegistry`** (v0.2): an immutable registry of validated workspace contexts built from a TOML configuration file, keyed by exact logical workspace ID.

Each mode constructs its own MCP server implementation with its own tool router: a single-workspace server advertising the eight v0.1 tools, and a registry server advertising those eight plus `list_workspaces`. A registry-mode request can therefore never reach single-workspace tool code and vice versa — mode separation is by construction, not by runtime checks.

## Workspace contexts and the resolver

A workspace context bundles three things:

- the **workspace ID** (absent in single mode, where the root itself is the identity);
- the **root-bound path resolver** — the single authority deciding what a request may touch (see [security: filesystem boundary model](security.md#filesystem-boundary-model));
- the **effective permissions** (read implicit; `write`/`exec`/`shell` explicit).

In single mode the context is built from CLI flags; in registry mode one context exists per configured workspace, each with its own permissions. Contexts are shared as cheap, immutable handles; concurrent requests for different workspaces are independent.

## The immutable registry

The registry is built completely before the server serves and is never mutated afterwards:

- Configuration is parsed with strict deserialization (unknown fields rejected) and fully validated — version, size bound, ID grammar, absolute/exists/directory roots, duplicate and nested root rejection by filesystem identity, and the `allow_shell ⇒ exec` rule (full checklist in [configuration: validation and limits](configuration.md#validation-and-limits)).
- There is no runtime add/remove/reload API and no "current workspace" state: every request carries its own explicit workspace ID, looked up exactly (no case folding, aliases, or fallbacks).
- Because `list_workspaces` has no pagination and never truncates, the registry size is bounded at startup (at most 64 workspaces) rather than bounding discovery output after the fact.

## Tool cores shared by both modes

Tool behavior lives in context-based cores that both modes share. The mode-specific wrappers differ only in three respects:

1. **Workspace selection** — single-mode wrappers use the fixed context; registry wrappers route by logical ID first (with a bounded, non-enumerating error for unknown IDs).
2. **Permission gating** — single mode gates on CLI flags; registry mode gates on the selected context's configured capabilities, before any parsing, filesystem access, or process spawn.
3. **Path presentation** — single mode renders the root as its canonical absolute path (v0.1 compatibility); registry mode renders paths workspace-relative with the root as `"."`, keeping canonical roots out of all client-visible output.

Everything else — resolution, bounding, atomic writes, process containment, git hardening — is identical across modes.

## Transports

The same MCP server (mode + tool surface) is served over either transport:

- **stdio** (default): the protocol runs on stdout, logs on stderr.
- **Streamable HTTP**: loopback-only bind (`127.0.0.1`/`::1`/`localhost`), MCP endpoint at `/mcp`, no authentication (see [security: HTTP loopback restriction](security.md#http-loopback-restriction)).

The transport layer carries no policy: it only moves MCP messages between the client and the mode-specific server.

## Design invariants

Invariants that shape contributions:

- **MCP requests never choose filesystem roots.** Roots are operator-configured and canonicalized once at startup; requests select workspaces by logical ID only.
- **The registry (and every root, ID, and permission in it) is immutable after startup.** No hot reload, no runtime registration, no workspace switching.
- **Permissions are fixed at startup and enforced per request.** Nothing is promoted silently at runtime; capability checks run before any observable side effect.
- **Strict input handling.** Unknown configuration fields and malformed workspace IDs fail loudly instead of being ignored or defaulted.
- **Bounded outputs everywhere.** Every large response is capped with truncation metadata; where output cannot be bounded (discovery), the input is bounded instead (registry size).
- **Bounded memory.** File and command readers use fixed internal ceilings, so pathological inputs (gigabyte single lines, pipe floods) cannot drive unbounded allocation.
- **One resolver, one authority.** All path containment lives in the root-bound resolver; presentation is a separate concern and never weakens containment.
- **Registry clients never see filesystem roots** — not in metadata, errors, or git diagnostics; single mode preserves its v0.1 absolute-path presentation for compatibility.
