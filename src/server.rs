//! Mode-aware MCP server boundary (v0.2 M2).
//!
//! There are two separate MCP servers, each with its own tool router, and
//! each transport selects exactly one from [`RuntimeMode`] at startup:
//!
//! - [`NianWorkspaceServer`] (single-workspace mode): the complete,
//!   unchanged v0.1 tool surface, rooted at the one configured workspace.
//! - [`RegistryServer`] (registry mode): discovery only — `list_workspaces`
//!   and a workspace-selecting `workspace_info`.
//!
//! This is genuine mode-specific routing, not cosmetic hiding: a tool that
//! is not available in a mode is not registered on that mode's router at
//! all, so a direct `tools/call` for it fails inside the rmcp router with a
//! clean `invalid_params("tool not found")` protocol error — before any
//! tool handler runs, without touching a workspace, without panicking, and
//! without any default-workspace fallback. The server stays fully usable
//! afterwards.
//!
//! The intended longer-term shape (M3+): migrate scoped tools onto
//! `WorkspaceContext` one group at a time and register them on the
//! registry-mode router as they become workspace-id aware.

mod registry;
mod single;

pub use registry::RegistryServer;
pub use single::NianWorkspaceServer;
