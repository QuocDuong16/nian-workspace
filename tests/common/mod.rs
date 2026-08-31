//! Tool-surface constants shared by the transport E2E suites — `tests/cli.rs`
//! drives stdio, `tests/http.rs` drives streamable HTTP — so both transports
//! are asserted against the same lists, proving stdio and HTTP expose the
//! same tool surface for the same mode (M6 transport parity).
//!
//! `#![allow(dead_code)]`: each test crate includes this module separately
//! and neither suite uses every item.

#![allow(dead_code)]

/// The v0.1 single-workspace tool surface, byte-for-byte (sorted).
pub const SINGLE_MODE_TOOLS: &[&str] = &[
    "apply_patch",
    "git_diff",
    "git_status",
    "list_files",
    "read_file",
    "run_command",
    "search",
    "workspace_info",
];

/// The v0.2 registry-mode tool surface: workspace discovery plus the full
/// v0.1 capability set, every tool except `list_workspaces` selecting one
/// workspace by logical WorkspaceId (sorted).
pub const REGISTRY_MODE_TOOLS: &[&str] = &[
    "apply_patch",
    "git_diff",
    "git_status",
    "list_files",
    "list_workspaces",
    "read_file",
    "run_command",
    "search",
    "workspace_info",
];
