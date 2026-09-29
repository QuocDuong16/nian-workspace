# Design and contribution conventions

- Keep runtime mode selection at startup. Do not add mutable global/current-workspace state or runtime registry mutation/reload.
- Keep shared tool behavior in context-based cores under `src/tools/`; mode-specific routers should only select the explicit workspace context, apply permission gates, and format mode-specific path presentation.
- Resolve every client-supplied filesystem path through the root-bound `Workspace::resolve`; do not introduce separate containment rules in individual tools.
- Registry config is strict and fail-closed: unknown fields, invalid IDs, unsupported versions, unsafe roots, and invalid permission combinations abort startup.
- Perform capability checks before parsing inputs that could cause side effects, touching files, or spawning processes. Read is implicit; write, direct execution, and shell execution are explicit.
- Keep outputs bounded and report truncation. Registry responses use workspace-relative paths and must not disclose canonical roots; single-workspace mode retains absolute-path presentation for compatibility.
- The toolchain pin is coordinated across Cargo, rustup, mise, and workflows. Renovate handles Cargo dependencies and workflow actions; the mise Rust runtime pin stays manual.