# Toolchain and dependencies

- One Rust 2021 binary built with Cargo. The Rust toolchain is pinned in `mise.toml`, `rust-toolchain.toml`, `Cargo.toml` (`rust-version`), and CI/release workflow definitions; keep those pins coordinated. Authoritative files hold the exact version.
- `mise.lock` locks the Rust toolchain and the `cargo-audit` / `cargo-outdated` developer tools configured in `mise.toml`. `rust-toolchain.toml` includes `rustfmt` and `clippy`.
- Runtime uses rmcp for MCP, Tokio for async/processes, Axum for Streamable HTTP, Serde/TOML for registry config, and Clap for CLI parsing. Filesystem search, Git inspection, atomic patch writes, and cross-platform process containment are implemented in the crate.
- No system libraries or Node/pnpm build dependencies are required. Node used by CI action bootstrapping is not part of the product toolchain.
- Dependency PRs are managed by Renovate. The Rust toolchain is intentionally kept manual because its pin is duplicated across Cargo, mise, rustup, and CI.