# Completion checks

Run `make quality-check` before treating a code change as ready. It runs the four native quality gates:

1. `cargo fmt --all --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo test`
4. `cargo build --release`

- `make cross-check` installs the configured Rust targets, then runs `cargo check --all-targets --all-features --target <target>` for Windows, macOS, and Linux. This is compile-only evidence; it does not establish native runtime behavior.
- `make ci-check` runs the native quality gates and all configured cross-target checks.
- `make audit` scans the lockfile against RustSec advisories; `make outdated` lists newer direct dependencies. Both are optional, network-dependent maintenance commands outside `quality-check`.
- Run `git diff --check` after edits. Serena memory integrity can be checked from the project root with `serena memories check`.
- State clearly when native platform/release checks were not run; do not describe cross-compilation as runtime proof.