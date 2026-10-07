# Development commands

- Run `make help` to list project commands.
- Trust and install project tools pinned by mise: `mise trust`, then `make toolchain-install` (`mise install --locked rust cargo:cargo-audit cargo:cargo-outdated`).
- Run native quality gates with `make quality-check`; individual targets are `make format-check`, `make clippy`, `make test`, and `make build`.
- Run `make audit` to scan locked dependencies against RustSec advisories and `make outdated` to list newer direct dependency versions.
- Run the cross-target compile checks with `make cross-check`; `make ci-check` runs native and cross-target gates.
- Run the CLI help: `cargo run -- --help`.
- Run one workspace (read-only by default): `cargo run -- /path/to/workspace`.
- Run a TOML registry: `cargo run -- --workspace-config /path/to/workspaces.toml`.
- Run the local HTTP transport: `cargo run -- . --transport http --host 127.0.0.1 --port 8787`. Keep HTTP on loopback; it has no authentication.
- Install the binary locally: `make install` (`cargo install --path . --locked`).
- Build an optimized binary: `cargo build --release --locked`.
- `make clean` removes Cargo build artifacts.
- After changing a mise tool pin, refresh the lock with `mise lock`.
- For the required local quality gates, see `mem:task_completion`.