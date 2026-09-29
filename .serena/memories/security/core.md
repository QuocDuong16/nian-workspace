# Security boundaries

- `Workspace::resolve` is the authority for client-supplied filesystem paths: lexical normalization, symlink-aware canonicalization through the deepest existing ancestor, then containment under the canonical workspace root. It also validates paths whose leaf does not exist yet.
- Registry roots must be absolute, existing directories. The strict TOML registry is built before serving, rejects duplicate or nested roots by filesystem identity, and is immutable. Workspace IDs are exact logical names, never paths or aliases.
- Read is implicit. `apply_patch` requires write; `run_command` requires exec; shell mode additionally requires shell permission. Invalid capability combinations fail at startup, and per-request gates precede filesystem/process side effects.
- Workspace isolation is not an OS sandbox. A spawned command runs with the full OS-user permissions and may access files/network outside the workspace; only its working directory is root-resolved. Enable exec/write/shell only for trusted clients.
- HTTP transport binds to loopback only and has no authentication. Anything that can reach it can use the enabled tools, so do not expose it publicly.
- Outputs and readers are bounded; discovery is bounded by limiting the registry size. Process timeout handling terminates process trees best-effort (Unix process groups, Windows Job Objects).
- Git tools are hardened for read-only intent: child environment redirects and pagers are disabled, external diff/textconv/fsmonitor execution is blocked, and prompts are disabled.