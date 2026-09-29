# Completion checks

Run all four local quality gates before treating a code change as ready:

1. `cargo fmt --all --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo test`
4. `cargo build --release`

- Cross-target CI uses `cargo check --all-targets --all-features --target <target>` for Windows, macOS, and Linux targets. This is compile-only evidence; it does not establish native runtime behavior.
- For a full target check, install the target with `rustup target add <target>` first.
- Run `git diff --check` after edits. Serena memory integrity can be checked from the project root with `serena memories check`.
- State clearly when native platform/release checks were not run; do not describe cross-compilation as runtime proof.