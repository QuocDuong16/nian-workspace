//! v0.2 M1 workspace registry: a versioned TOML configuration and the
//! immutable set of workspace contexts built from it entirely at startup.
//!
//! Security model (unchanged from v0.1): MCP requests must never choose
//! filesystem roots. Registry mode is the foundation for routing requests by
//! an operator-configured logical [`WorkspaceId`]; the registry itself is
//! built completely before serving and is immutable afterwards — no
//! add/remove/reload APIs exist, and workspace roots, ids, and permissions
//! never change while the process runs.
//!
//! Startup validation fails before serving if the configuration is invalid:
//!
//! 1. `version` exists and equals `1`;
//! 2. at least one workspace is declared;
//! 3. every workspace id is valid (see [`WorkspaceId::parse`]);
//! 4. every root exists;
//! 5. every root canonicalizes successfully;
//! 6. every root is a directory;
//! 7. duplicate canonical roots are rejected (including symlink aliases);
//! 8. overlapping/nested canonical roots are rejected in both directions —
//!    a broader writable workspace would otherwise bypass a narrower
//!    read-only one;
//! 9. `allow_shell = true` requires `exec = true`;
//! 10. unknown/malformed fields fail via strict TOML deserialization.
//!
//! Containment checks use filesystem path/component semantics (never a raw
//! string-prefix comparison, so `project` and `project-other` are siblings,
//! not a nesting violation), honoring platform path-case behavior.

use crate::permissions::Permissions;
use crate::workspace::{Workspace, WorkspaceContext};
use crate::workspace_id::WorkspaceId;
use anyhow::{bail, Context};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The only supported configuration format version.
pub const SUPPORTED_CONFIG_VERSION: u64 = 1;

/// On-disk registry configuration schema (v0.2 M1).
///
/// Deserialization is strict: unknown fields are rejected rather than
/// silently ignored, so typos in capability names cannot weaken a
/// configuration unnoticed. Defaults are conservative — every capability
/// except implicit read access is off.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConfigFile {
    version: u64,
    workspaces: BTreeMap<String, WorkspaceConfigEntry>,
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct WorkspaceConfigEntry {
    root: PathBuf,
    #[serde(default)]
    write: bool,
    #[serde(default)]
    exec: bool,
    #[serde(default)]
    allow_shell: bool,
}

/// An immutable registry of fully validated workspace contexts, keyed by
/// exact logical id. Built completely during startup; never mutated again.
#[derive(Debug, Clone)]
pub struct WorkspaceRegistry {
    workspaces: HashMap<WorkspaceId, Arc<WorkspaceContext>>,
}

impl WorkspaceRegistry {
    /// Load, parse, and fully validate a registry configuration file.
    pub fn from_file(path: &Path) -> anyhow::Result<Self> {
        let raw = std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read workspace config '{}'", path.display()))?;
        Self::from_toml_str(&raw)
            .with_context(|| format!("in workspace config '{}'", path.display()))
    }

    /// Parse and fully validate a registry configuration from TOML text.
    pub fn from_toml_str(raw: &str) -> anyhow::Result<Self> {
        let config: WorkspaceConfigFile = toml::from_str(raw)
            .map_err(|e| anyhow::anyhow!("invalid workspace configuration: {e}"))?;
        Self::build(config)
    }

    /// Exact logical-id lookup. No case folding, no aliasing, no fallbacks.
    ///
    /// Reserved for workspace-id request routing in a later milestone (M2);
    /// M1 tools never call it.
    #[allow(dead_code)]
    pub fn get(&self, id: &WorkspaceId) -> Option<&Arc<WorkspaceContext>> {
        self.workspaces.get(id)
    }

    /// All contexts in deterministic workspace-id order, for diagnostics.
    pub fn iter_sorted(&self) -> Vec<&Arc<WorkspaceContext>> {
        let mut contexts: Vec<&Arc<WorkspaceContext>> = self.workspaces.values().collect();
        contexts.sort_by_key(|ctx| ctx.id().cloned());
        contexts
    }

    /// Validate every declared workspace, then reject duplicate or nested
    /// canonical roots, and only then freeze the registry.
    fn build(config: WorkspaceConfigFile) -> anyhow::Result<Self> {
        if config.version != SUPPORTED_CONFIG_VERSION {
            bail!(
                "Unsupported workspace config version: {} (supported: {}).",
                config.version,
                SUPPORTED_CONFIG_VERSION
            );
        }
        if config.workspaces.is_empty() {
            bail!("Workspace config must declare at least one workspace under [workspaces.<id>].");
        }

        // BTreeMap keys iterate in sorted order, so validation errors are
        // deterministic across runs regardless of declaration order.
        let mut contexts: Vec<(WorkspaceId, PathBuf, Arc<WorkspaceContext>)> = Vec::new();
        for (raw_id, entry) in &config.workspaces {
            let id = WorkspaceId::parse(raw_id).map_err(anyhow::Error::msg)?;

            if entry.allow_shell && !entry.exec {
                bail!(
                    "Workspace '{id}': allow_shell = true requires exec = true. \
                     Shell execution is a superset of program execution."
                );
            }

            if !entry.root.exists() {
                bail!(
                    "Workspace '{id}': root '{}' does not exist.",
                    entry.root.display()
                );
            }
            let resolver = Workspace::open(&entry.root)
                .with_context(|| format!("Workspace '{id}': invalid root"))?;
            let canonical = resolver.root().to_path_buf();
            if !canonical.is_dir() {
                bail!(
                    "Workspace '{id}': root '{}' is not a directory.",
                    entry.root.display()
                );
            }

            let permissions = Permissions {
                read: true,
                write: entry.write,
                exec: entry.exec,
                shell: entry.allow_shell,
            };
            contexts.push((
                id.clone(),
                canonical,
                Arc::new(WorkspaceContext::new(Some(id), resolver, permissions)),
            ));
        }

        // Duplicate and nested canonical roots are rejected in both
        // directions; comparisons use canonical paths so symlink aliases
        // cannot smuggle a second registration of the same directory in.
        for i in 0..contexts.len() {
            for j in (i + 1)..contexts.len() {
                let (id_a, path_a, _) = &contexts[i];
                let (id_b, path_b, _) = &contexts[j];
                let (a, b) = (path_a, path_b);
                if path_contains(a, b) && path_contains(b, a) {
                    bail!(
                        "Workspaces '{id_a}' and '{id_b}' resolve to the same canonical root '{}'. \
                         Every workspace must have a distinct root.",
                        a.display()
                    );
                }
                if path_contains(a, b) {
                    bail!(
                        "Workspace '{id_b}' root '{}' is nested inside workspace '{id_a}' root '{}'. \
                         Nested workspace roots are forbidden: a broader writable workspace would \
                         bypass a narrower read-only one.",
                        b.display(),
                        a.display()
                    );
                }
                if path_contains(b, a) {
                    bail!(
                        "Workspace '{id_a}' root '{}' is nested inside workspace '{id_b}' root '{}'. \
                         Nested workspace roots are forbidden: a broader writable workspace would \
                         bypass a narrower read-only one.",
                        a.display(),
                        b.display()
                    );
                }
            }
        }

        let workspaces = contexts
            .into_iter()
            .map(|(id, _, ctx)| (id, ctx))
            .collect::<HashMap<_, _>>();
        tracing::debug!(count = workspaces.len(), "workspace registry built");
        Ok(Self { workspaces })
    }
}

/// Component-wise containment check: `child` is inside or equal to `parent`.
///
/// Both inputs are canonical paths (absolute, no `.`/`..`), so comparing
/// `Components` element-wise is exact — and unlike `String::starts_with`,
/// sibling names such as `project` and `project-other` never collide.
fn path_contains(parent: &Path, child: &Path) -> bool {
    let mut parent_components = parent.components();
    let mut child_components = child.components();
    loop {
        match (parent_components.next(), child_components.next()) {
            // Parent exhausted: everything matched (or parent was empty).
            (None, _) => return true,
            // Child exhausted while the parent still has components.
            (Some(_), None) => return false,
            (Some(p), Some(c)) => {
                if !component_equivalent(p.as_os_str(), c.as_os_str()) {
                    return false;
                }
            }
        }
    }
}

/// Compare two path components, honoring platform filesystem semantics:
/// case-insensitively on Windows and macOS (case-insensitive filesystems by
/// default), exactly everywhere else.
#[cfg(any(target_os = "windows", target_os = "macos"))]
fn component_equivalent(a: &OsStr, b: &OsStr) -> bool {
    a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
}

#[cfg(not(any(target_os = "windows", target_os = "macos")))]
fn component_equivalent(a: &OsStr, b: &OsStr) -> bool {
    a == b
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use tempfile::TempDir;

    /// Create `<dir>/<name>` and return its path.
    fn make_dir(base: &TempDir, name: &str) -> PathBuf {
        let p = base.path().join(name);
        std::fs::create_dir_all(&p).expect("create fixture dir");
        p
    }

    /// TOML literal strings (single quotes) need no escaping on any platform.
    fn config_for(entries: &[(&str, &Path, &str)]) -> String {
        let mut out = String::from("version = 1\n\n");
        for (id, root, perms) in entries {
            out.push_str(&format!(
                "[workspaces.{id}]\nroot = '{}'\n{perms}\n",
                root.display()
            ));
        }
        out
    }

    fn build_ok(toml: &str) -> WorkspaceRegistry {
        WorkspaceRegistry::from_toml_str(toml).expect("config should be valid")
    }

    fn build_err(toml: &str) -> String {
        WorkspaceRegistry::from_toml_str(toml)
            .expect_err("config should be rejected")
            .to_string()
    }

    #[test]
    fn accepts_one_workspace_with_conservative_defaults() {
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");
        let registry = build_ok(&config_for(&[("solo", &ws, "")]));
        assert_eq!(registry.iter_sorted().len(), 1);
        let ctx = registry.iter_sorted()[0].clone();
        assert_eq!(ctx.id().unwrap().as_str(), "solo");
        assert_eq!(ctx.root(), ws.canonicalize().unwrap());
        let perms = ctx.permissions();
        assert!(perms.read);
        assert!(!perms.write);
        assert!(!perms.exec);
        assert!(!perms.shell);
    }

    #[test]
    fn accepts_multiple_workspaces_and_explicit_permissions() {
        let tmp = TempDir::new().unwrap();
        let a = make_dir(&tmp, "alpha");
        let b = make_dir(&tmp, "beta");
        let registry = build_ok(&config_for(&[
            ("alpha", &a, "write = true\nexec = true"),
            ("beta", &b, ""),
        ]));
        let ids: Vec<_> = registry
            .iter_sorted()
            .iter()
            .map(|ctx| ctx.id().unwrap().to_string())
            .collect();
        assert_eq!(ids, ["alpha", "beta"]);

        let alpha = registry
            .get(&WorkspaceId::parse("alpha").unwrap())
            .unwrap()
            .clone();
        assert!(alpha.permissions().write);
        assert!(alpha.permissions().exec);
        assert!(!alpha.permissions().shell);

        let beta = registry
            .get(&WorkspaceId::parse("beta").unwrap())
            .unwrap()
            .clone();
        assert!(!beta.permissions().write);
        assert!(!beta.permissions().exec);
    }

    #[test]
    fn accepts_sibling_roots_and_prefix_sharing_names() {
        let tmp = TempDir::new().unwrap();
        let project = make_dir(&tmp, "project");
        let project_other = make_dir(&tmp, "project-other");
        let registry = build_ok(&config_for(&[
            ("one", &project, ""),
            ("two", &project_other, ""),
        ]));
        assert_eq!(registry.iter_sorted().len(), 2);
    }

    #[test]
    fn rejects_unsupported_and_missing_version() {
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");
        let err = build_err(&format!(
            "version = 2\n\n[workspaces.ws]\nroot = '{}'\n",
            ws.display()
        ));
        assert!(
            err.contains("Unsupported workspace config version: 2"),
            "{err}"
        );

        let err = build_err(&format!("[workspaces.ws]\nroot = '{}'\n", ws.display()));
        assert!(err.contains("missing field `version`"), "{err}");

        let err = build_err(&format!(
            "version = \"1\"\n\n[workspaces.ws]\nroot = '{}'\n",
            ws.display()
        ));
        assert!(err.contains("invalid workspace configuration"), "{err}");
    }

    #[test]
    fn rejects_empty_registry() {
        let err = build_err("version = 1\n\n[workspaces]\n");
        assert!(err.contains("at least one workspace"), "{err}");

        let err = build_err("version = 1\n");
        assert!(err.contains("missing field `workspaces`"), "{err}");
    }

    #[test]
    fn rejects_malformed_toml() {
        assert!(WorkspaceRegistry::from_toml_str("version = 1\n[not closed").is_err());
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");
        // Duplicate table headers are a TOML parse error.
        let dup = format!(
            "version = 1\n\n[workspaces.ws]\nroot = '{}'\n\n[workspaces.ws]\nroot = '{}'\n",
            ws.display(),
            ws.display()
        );
        assert!(WorkspaceRegistry::from_toml_str(&dup).is_err());
    }

    #[test]
    fn rejects_unknown_fields_strictly() {
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");

        let err = build_err(&format!(
            "version = 1\nunknown_top_level = true\n\n[workspaces.ws]\nroot = '{}'\n",
            ws.display()
        ));
        assert!(err.contains("unknown field"), "{err}");

        let err = build_err(&format!(
            "version = 1\n\n[workspaces.ws]\nroot = '{}'\nadmin = true\n",
            ws.display()
        ));
        assert!(err.contains("unknown field"), "{err}");

        // A misspelled capability must not silently default to false.
        let err = build_err(&format!(
            "version = 1\n\n[workspaces.ws]\nroot = '{}'\nwriet = true\n",
            ws.display()
        ));
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn rejects_invalid_workspace_ids_in_config() {
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");
        for bad_id in ["Nian-Vision", ".hidden", "-lead", "has/slash", "has space"] {
            let err = build_err(&format!(
                "version = 1\n\n[workspaces.'{bad_id}']\nroot = '{}'\n",
                ws.display()
            ));
            assert!(
                err.contains("invalid workspace id"),
                "id '{bad_id}' should be rejected: {err}"
            );
        }
    }

    #[test]
    fn rejects_allow_shell_without_exec() {
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");
        let err = build_err(&config_for(&[("ws", &ws, "allow_shell = true")]));
        assert!(
            err.contains("allow_shell = true requires exec = true"),
            "{err}"
        );
    }

    #[test]
    fn rejects_nonexistent_root() {
        let tmp = TempDir::new().unwrap();
        let missing = tmp.path().join("does-not-exist");
        let err = build_err(&config_for(&[("ws", &missing, "")]));
        assert!(err.contains("does not exist"), "{err}");
    }

    #[test]
    fn rejects_file_as_root() {
        let tmp = TempDir::new().unwrap();
        let file = tmp.path().join("plain-file");
        std::fs::write(&file, b"not a directory").unwrap();
        let err = build_err(&config_for(&[("ws", &file, "")]));
        assert!(err.contains("is not a directory"), "{err}");
    }

    #[test]
    fn rejects_duplicate_canonical_roots() {
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");
        // Same directory under two ids via a lexically different spelling.
        let alias_spelling = tmp.path().join(".").join("ws");
        let err = build_err(&config_for(&[
            ("one", &ws, ""),
            ("two", &alias_spelling, ""),
        ]));
        assert!(err.contains("same canonical root"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn rejects_symlink_alias_to_same_root() {
        use std::os::unix::fs::symlink;
        let tmp = TempDir::new().unwrap();
        let real = make_dir(&tmp, "real");
        let link = tmp.path().join("alias");
        symlink(&real, &link).unwrap();
        let err = build_err(&config_for(&[("real", &real, ""), ("alias", &link, "")]));
        assert!(err.contains("same canonical root"), "{err}");
    }

    #[test]
    fn rejects_nested_roots_in_both_declaration_orders() {
        let tmp = TempDir::new().unwrap();
        let parent = make_dir(&tmp, "parent");
        let child = make_dir(&tmp, "parent").join("project");
        std::fs::create_dir_all(&child).unwrap();

        let err = build_err(&config_for(&[
            ("parent", &parent, "write = true"),
            ("child", &child, ""),
        ]));
        assert!(err.contains("is nested inside"), "{err}");
        assert!(err.contains("forbidden"), "{err}");

        // Reversed declaration order must fail identically.
        let err = build_err(&config_for(&[
            ("child", &child, ""),
            ("parent", &parent, "write = true"),
        ]));
        assert!(err.contains("is nested inside"), "{err}");
    }

    #[test]
    fn path_contains_uses_component_semantics_not_string_prefix() {
        let parent = Path::new("/ws/project");
        assert!(path_contains(parent, Path::new("/ws/project/src/main.rs")));
        assert!(path_contains(parent, Path::new("/ws/project")));
        assert!(!path_contains(parent, Path::new("/ws/project-other")));
        assert!(!path_contains(parent, Path::new("/ws/projectx/sub")));
        assert!(!path_contains(parent, Path::new("/ws")));
    }

    #[cfg(unix)]
    #[test]
    fn workspace_context_resolves_paths_within_registry_workspace() {
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");
        let registry = build_ok(&config_for(&[("ws", &ws, "write = true")]));
        let ctx = registry.iter_sorted()[0].clone();
        // The hardened resolver stays the single authority per context.
        assert!(ctx.resolver().resolve(Some("src/main.rs")).is_ok());
        assert!(ctx.resolver().resolve(Some("../../etc/passwd")).is_err());
        assert_eq!(ctx.root(), ws.canonicalize().unwrap());
        assert_eq!(ctx.id().unwrap().as_str(), "ws");
    }
}
