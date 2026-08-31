//! Mode-aware MCP server boundary (v0.2 M2/M3).
//!
//! There are two separate MCP servers, each with its own tool router, and
//! each transport selects exactly one from [`RuntimeMode`] at startup:
//!
//! - [`NianWorkspaceServer`] (single-workspace mode): the complete,
//!   unchanged v0.1 tool surface, rooted at the one configured workspace.
//! - [`RegistryServer`] (registry mode): workspace discovery plus read-only
//!   filesystem and Git access — `list_workspaces`, `workspace_info`, and
//!   workspace-selecting `list_files`, `read_file`, `search`, `git_status`,
//!   and `git_diff`.
//!
//! This is genuine mode-specific routing, not cosmetic hiding: a tool that
//! is not available in a mode is not registered on that mode's router at
//! all, so a direct `tools/call` for it fails inside the rmcp router with a
//! clean `invalid_params("tool not found")` protocol error — before any
//! tool handler runs, without touching a workspace, without panicking, and
//! without any default-workspace fallback. The server stays fully usable
//! afterwards.
//!
//! Registry filesystem and Git tools share the single-mode implementations
//! through context-based cores: mode selection happens at this server
//! boundary, workspace selection inside the registry wrappers, and tool
//! behavior in the shared `*_for_context` functions over the selected
//! [`WorkspaceContext`] (v0.2 M3/M4). Later milestones migrate further
//! scoped tool groups the same way, one group at a time.

mod registry;
mod single;

pub use registry::RegistryServer;
pub use single::NianWorkspaceServer;
