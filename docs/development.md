# Development

Notes for working on `nian-workspace` itself. For using it, start with the [README](../README.md).

- [Toolchain](#toolchain)
- [Local quality gates](#local-quality-gates)
- [Project layout](#project-layout)
- [Development CI](#development-ci)

## Toolchain

Rust 1.98 or newer is required to build from source. The exact toolchain is pinned in [`rust-toolchain.toml`](../rust-toolchain.toml) (currently 1.98.0, with `rustfmt` and `clippy`), so `cargo` commands automatically use the same version locally and in CI.

No system libraries are required: the project has no TLS or other C dependencies.

## Local quality gates

The gates CI enforces, runnable locally:

```bash
cargo fmt --all --check
cargo clippy --all-targets --all-features -- -D warnings
cargo test
cargo build --release
```

All four must pass before a change can land. Clippy runs with warnings denied — new lints can break the build, which is intended.

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

## Development CI

Ordinary development changes are validated by CI on every push and pull request. The development CI runs on a self-hosted Forgejo instance using Forgejo Actions with Docker-in-Docker (`.forgejo/workflows/quality.yml`) — this is maintainer infrastructure and irrelevant to using the product.

What it runs, in two jobs:

1. **`rust`** — fmt check, clippy (`-D warnings`), the full test suite, and a native Linux x86_64 release build, inside a `rust:1.98.0-bookworm` container matching the pinned toolchain. The runner container ships without Node.js, so the workflow first installs a pinned, checksum-verified Node.js runtime for the checkout action; the buildpack-deps base image otherwise provides everything needed.
2. **`cross-target`** — compile validation (`cargo check --all-targets --all-features`) for `x86_64-pc-windows-msvc`, `x86_64-apple-darwin`, `aarch64-apple-darwin`, and `aarch64-unknown-linux-gnu` (after `rustup target add`). Cross-target jobs are **compile-only**: no foreign binary is executed and no emulators are used. They catch unguarded platform-specific code and target-specific dependency errors; native runtime testing for foreign platforms happens only at release time (see [release](release.md)).

Release builds are deliberately not part of development CI — see [release.md](release.md) for the tag-triggered pipeline.
