//! Registry-mode discovery tools (v0.2 M2): `list_workspaces` and the
//! workspace-selecting `workspace_info`.
//!
//! These are the only tools the registry-mode server exposes. Discovery is
//! read-only and entirely startup-configured: there is no runtime
//! registration, workspace switching, aliasing, or path-based selection.
//! Absolute roots and configuration paths are never reported — clients see
//! logical ids and effective permissions only.

use crate::config::AppState;
use crate::error::{ToolError, ToolResult};
use crate::registry::WorkspaceRegistry;
use crate::tools::workspace_info::{self, WorkspaceIdentity};
use crate::workspace_id::WorkspaceId;
use rmcp::schemars;
use serde_json::json;

/// Input schema for registry-mode `workspace_info`: one required logical
/// workspace selector. Deserialization goes through
/// [`WorkspaceId`] (grammar + no path interpretation), so an invalid
/// selector fails strictly at the boundary before any lookup is attempted.
#[derive(Debug, serde::Deserialize, schemars::JsonSchema)]
pub struct WorkspaceInfoArgs {
    /// Logical workspace ID to inspect — exactly one of the IDs reported by
    /// list_workspaces, as configured by the operator at startup. Not a path;
    /// there is no case folding, aliasing, or fallback workspace.
    #[schemars(
        description = "Logical workspace ID to inspect — exactly one of the IDs reported by list_workspaces, as configured by the operator at startup. Not a path; there is no case folding, aliasing, or fallback workspace."
    )]
    pub workspace: WorkspaceId,
}

/// `list_workspaces`: the configured logical workspace ids in deterministic
/// [`WorkspaceId`] order, each with its effective permissions.
pub(crate) fn list_workspaces(state: &AppState) -> ToolResult<serde_json::Value> {
    let workspaces: Vec<serde_json::Value> = state
        .registry()
        .iter_sorted()
        .into_iter()
        .map(|ctx| {
            let id = ctx.id().expect("registry workspaces always have an id");
            json!({
                "id": id.as_str(),
                "permissions": workspace_info::permissions_json(ctx.permissions()),
            })
        })
        .collect();
    Ok(json!({ "workspaces": workspaces }))
}

/// Registry-mode `workspace_info`: exact [`WorkspaceId`] lookup into the
/// immutable registry, then the shared context-based metadata.
pub(crate) fn workspace_info(
    state: &AppState,
    args: WorkspaceInfoArgs,
) -> ToolResult<serde_json::Value> {
    let registry = state.registry();
    let ctx = registry
        .get(&args.workspace)
        .ok_or_else(|| unknown_workspace(registry, &args.workspace))?;
    workspace_info::describe(&ctx, WorkspaceIdentity::Logical(&args.workspace))
}

/// Unknown ids are rejected explicitly; the error may enumerate the
/// configured ids (logical names only, never roots) to help the client
/// recover without a second round trip.
fn unknown_workspace(registry: &WorkspaceRegistry, requested: &WorkspaceId) -> ToolError {
    let ids: Vec<&str> = registry
        .iter_sorted()
        .into_iter()
        .map(|ctx| {
            ctx.id()
                .expect("registry workspaces always have an id")
                .as_str()
        })
        .collect();
    ToolError::msg(format!(
        "Unknown workspace '{}'. Configured workspaces: {}. \
         Use list_workspaces to discover valid workspace IDs.",
        requested,
        ids.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    /// Build a registry-mode AppState from workspace names declared in the
    /// given (unsorted) order; every workspace gets the same capabilities.
    fn registry_state(names: &[&str]) -> (TempDir, AppState) {
        let tmp = TempDir::new().expect("tempdir");
        let mut config = String::from("version = 1\n\n");
        for name in names {
            let dir = tmp.path().join(name);
            std::fs::create_dir_all(&dir).expect("workspace fixture dir");
            config.push_str(&format!(
                "[workspaces.{name}]\nroot = '{}'\nwrite = true\nexec = true\n\n",
                dir.display()
            ));
        }
        let registry = WorkspaceRegistry::from_toml_str(&config).expect("valid config");
        (tmp, AppState::from_registry(registry))
    }

    #[test]
    fn list_workspaces_is_sorted_and_hides_roots() {
        let (tmp, state) = registry_state(&["zeta", "alpha", "middle"]);

        let value = list_workspaces(&state).expect("list_workspaces");
        let ids: Vec<&str> = value["workspaces"]
            .as_array()
            .expect("workspaces array")
            .iter()
            .map(|w| w["id"].as_str().expect("id string"))
            .collect();
        assert_eq!(ids, ["alpha", "middle", "zeta"]);

        // Every entry carries the effective permissions; nothing in the
        // response may leak the configured roots.
        for entry in value["workspaces"].as_array().unwrap() {
            assert_eq!(entry["permissions"]["read"], json!(true));
            assert_eq!(entry["permissions"]["write"], json!(true));
            assert_eq!(entry["permissions"]["exec"], json!(true));
            assert_eq!(entry["permissions"]["shell"], json!(false));
        }
        let rendered = value.to_string();
        assert!(
            !rendered.contains(tmp.path().to_string_lossy().as_ref()),
            "list_workspaces must not expose filesystem roots: {rendered}"
        );
        assert!(
            !rendered.contains("root"),
            "list_workspaces must not contain root fields: {rendered}"
        );
    }

    #[test]
    fn registry_workspace_info_reports_requested_id_without_paths() {
        let (_tmp, state) = registry_state(&["beta", "alpha"]);

        for name in ["alpha", "beta"] {
            let id = WorkspaceId::parse(name).expect("fixture id");
            let value = workspace_info(&state, WorkspaceInfoArgs { workspace: id })
                .expect("workspace_info");
            assert_eq!(value["workspace"], json!(name));
            assert_eq!(value["permissions"]["write"], json!(true));
            assert_eq!(value["git"]["is_repository"], json!(false));
            assert!(
                value.get("root").is_none() && value.get("name").is_none(),
                "registry workspace_info must not expose filesystem paths: {value}"
            );
        }
    }

    #[test]
    fn unknown_workspace_error_lists_ids_without_roots() {
        let (tmp, state) = registry_state(&["beta", "alpha"]);

        let err = workspace_info(
            &state,
            WorkspaceInfoArgs {
                workspace: WorkspaceId::parse("does-not-exist").expect("valid grammar"),
            },
        )
        .expect_err("unknown workspace must be rejected");

        let message = err.to_string();
        assert!(
            message.contains("Unknown workspace 'does-not-exist'"),
            "{message}"
        );
        assert!(
            message.contains("Configured workspaces: alpha, beta"),
            "error should enumerate ids in deterministic order: {message}"
        );
        assert!(
            !message.contains(tmp.path().to_string_lossy().as_ref()),
            "errors must not expose filesystem roots: {message}"
        );
    }

    #[test]
    fn consecutive_lookups_are_independent() {
        let (_tmp, state) = registry_state(&["beta", "alpha"]);

        let first = workspace_info(
            &state,
            WorkspaceInfoArgs {
                workspace: WorkspaceId::parse("alpha").unwrap(),
            },
        )
        .unwrap();
        let second = workspace_info(
            &state,
            WorkspaceInfoArgs {
                workspace: WorkspaceId::parse("beta").unwrap(),
            },
        )
        .unwrap();
        // No shared mutable selection state: each call reflects only its own
        // requested id.
        assert_eq!(first["workspace"], json!("alpha"));
        assert_eq!(second["workspace"], json!("beta"));
    }
}
