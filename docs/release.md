# Release process

How tagged releases of `nian-workspace` are built and published.

- [Trigger policy](#trigger-policy)
- [Tag/version/notes verification](#tagversionnotes-verification)
- [Native build matrix](#native-build-matrix)
- [Archives](#archives)
- [Checksums and publication](#checksums-and-publication)

## Trigger policy

Release builds run on **GitHub Actions, triggered only by a `v*` tag push**. The workflow has deliberately no branch, pull-request, scheduled, or manual trigger, so GitHub-hosted native runners are consumed only at release time — never on ordinary development pushes, which are handled by the [development CI](development.md#development-ci) instead.

## Tag/version/notes verification

A lightweight consistency job runs first and fails the release before any expensive native build if:

- the pushed tag does not match the `nian-workspace` version in `Cargo.toml`, or
- `RELEASE_NOTES.md` is not headed with the same version (e.g. tag `v0.2.0` requires a first heading `# v0.2.0`).

When preparing a release: bump `Cargo.toml`, update `RELEASE_NOTES.md`, and push a matching `v*` tag.

## Native build matrix

Each release target builds on a native GitHub-hosted runner of the matching architecture:

| Platform | Rust target | Runner | Release artifact |
|---|---|---|---|
| Linux x86_64 | `x86_64-unknown-linux-gnu` | `ubuntu-24.04` | `nian-workspace-vX.Y.Z-x86_64-unknown-linux-gnu.tar.gz` |
| Linux arm64 | `aarch64-unknown-linux-gnu` | `ubuntu-24.04-arm` | `nian-workspace-vX.Y.Z-aarch64-unknown-linux-gnu.tar.gz` |
| Windows x86_64 | `x86_64-pc-windows-msvc` | `windows-2025` | `nian-workspace-vX.Y.Z-x86_64-pc-windows-msvc.zip` |
| macOS x86_64 | `x86_64-apple-darwin` | `macos-15-intel` | `nian-workspace-vX.Y.Z-x86_64-apple-darwin.tar.gz` |
| macOS arm64 | `aarch64-apple-darwin` | `macos-15` | `nian-workspace-vX.Y.Z-aarch64-apple-darwin.tar.gz` |

Every job in the matrix runs the same release validation on its native runner:

1. `cargo test --locked` — the full test suite, natively;
2. a release build with an explicit `--target` (artifact identity comes from the matrix, not host assumptions);
3. a binary smoke test (`--help`) on the runner;
4. upload of exactly one archive.

A native test failure on any platform blocks the release — failing native tests are not disabled to produce an artifact.

## Archives

Each archive contains the binary, `README.md`, `LICENSE`, and `RELEASE_NOTES.md` inside a top-level `nian-workspace-vX.Y.Z-<target>/` directory. Format is `.tar.gz` everywhere except Windows (`.zip`).

## Checksums and publication

One authoritative `SHA256SUMS` file covering all five archives is generated and verified centrally before publication, and is attached to the release so users can verify downloads.

The GitHub Release body comes from `RELEASE_NOTES.md`. Publication is rerunnable: if the release already exists, assets are re-uploaded with `--clobber` instead of creating a duplicate release.

Actions used by the workflow are pinned to immutable full commit SHAs (official actions only; publication uses the GitHub CLI, no third-party release action).
