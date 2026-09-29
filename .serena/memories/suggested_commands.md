# Development commands

- Trust and install the pinned Rust toolchain: `mise trust`, then `mise install --locked rust`.
- Run the CLI help: `cargo run -- --help`.
- Run one workspace (read-only by default): `cargo run -- /path/to/workspace`.
- Run a TOML registry: `cargo run -- --workspace-config /path/to/workspaces.toml`.
- Run the local HTTP transport: `cargo run -- . --transport http --host 127.0.0.1 --port 8787`. Keep HTTP on loopback; it has no authentication.
- Install the binary locally: `cargo install --path . --locked`.
- Build an optimized binary: `cargo build --release --locked`.
- For the required local quality gates, see `mem:task_completion`.