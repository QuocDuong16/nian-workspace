# Development

Notes for working on `nian-workspace` itself. For using it, start with the [README](../README.md).

- [Toolchain](#toolchain)
- [Local quality gates](#local-quality-gates)
- [Project layout](#project-layout)
- [Continuous integration](#continuous-integration)

## Toolchain

Rust 1.99.0 or newer is required to build from source. For reproducible development, [`mise.toml`](../mise.toml) and [`mise.lock`](../mise.lock) pin Rust 1.99.0, matching [`rust-toolchain.toml`](../rust-toolchain.toml) (with `rustfmt` and `clippy`). Direct `cargo` commands also select this version through rustup.

```bash
mise trust
make toolchain-install
```

The project-level mise configuration pins Rust plus the `cargo-audit` and `cargo-outdated` developer tools. `make toolchain-install` installs all three. Neither Node nor pnpm is a build dependency of this Rust project. To refresh the tool lock after changing a pin, run `mise lock`.

No system libraries are required: the project has no TLS or other C dependencies.

## Local quality gates

The Makefile is the local entry point. Run `make help` to see all commands and `make quality-check` to run the native gates:

```bash
make quality-check
```

The individual gates are `make format-check`, `make clippy`, `make test`, and `make build`. Clippy runs with warnings denied — new lints can break the build, which is intended. Run `make audit` to check the lockfile against the RustSec advisory database and `make outdated` to list newer direct dependency versions. These dependency reports are separate from the native quality gates. `make cross-check` installs the target standard libraries and runs compile-only checks for Windows, macOS, and Linux ARM64; `make ci-check` runs both the native and cross-target gates.

## Project layout

```
src/
  main.rs          entry point: CLI parsing, mode decision, transport wiring
  cli.rs           argument definitions and mode-compatibility validation
  config.rs        runtime state, bounded-output limits, runtime mode
  registry.rs      v0.2 workspace registry: TOML schema and startup validation
  workspace.rs     root-bound path resolver (containment authority)
  workspace_id.rs  logical workspace ID grammar and (de)serialization
  permissions.rs   permission flags and the allow-shell ⇒ exec rule
  server.rs        shared server helpers (runtime-host instructions)
  server/          mode-specific MCP servers and tool routers
  tools/           one module per tool (files, search, git, patch, command, …)
  process/         cross-platform process-tree containment (Unix/Windows)
  transport/       stdio and Streamable HTTP transports
tests/             integration tests (CLI, HTTP, common fixtures)
```

## Continuous integration

There is no push- or pull-request CI for development changes. Run the gates locally before merging:

```bash
make ci-check
```

`make ci-check` runs both sets of checks:

1. **Native gates** (`make quality-check`) — fmt check, clippy (`-D warnings`), the full test suite, and a native release build using the pinned Rust toolchain.
2. **Cross-target checks** (`make cross-check`) — compile validation (`cargo check --all-targets --all-features`) for `x86_64-pc-windows-msvc`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, and `aarch64-unknown-linux-gnu` (after `rustup target add`). Cross-target checks are **compile-only**: no foreign binary is executed and no emulators are used. They catch unguarded platform-specific code and target-specific dependency errors; native runtime testing for foreign platforms happens only at release time (see [release](release.md)).

Release builds run only on `v*` tags via GitHub Actions — see [release.md](release.md) for the tag-triggered pipeline.
