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
//! 3. at most [`MAX_REGISTRY_WORKSPACES`] are declared — registry-mode
//!    discovery (`list_workspaces`) has no pagination and is never
//!    truncated, so registry size is bounded up front to keep discovery
//!    output bounded;
//! 4. every workspace id is valid (see [`WorkspaceId::parse`]);
//! 5. every root is an absolute path — a relative root would make the
//!    security policy depend on the directory the server happened to be
//!    started from;
//! 6. every root exists and is a directory;
//! 7. every root canonicalizes successfully;
//! 8. duplicate roots — two spellings of the same filesystem directory,
//!    including symlink aliases and case-variant spellings on a
//!    case-insensitive filesystem — are rejected;
//! 9. nested/overlapping roots are rejected in both directions using real
//!    filesystem ancestry — a broader writable workspace would otherwise
//!    bypass a narrower read-only one;
//! 10. `allow_shell = true` requires `exec = true`;
//! 11. unknown/malformed fields fail via strict TOML deserialization.
//!
//! Rules 7 and 8 use **filesystem identity**, never path strings or string
//! case folding: the OS reports whether two paths denote the same directory
//! (device + inode on Unix, volume serial + file index on Windows). Two
//! genuinely distinct directories therefore never collide, even when their
//! names differ only by case on a case-sensitive filesystem, and sibling
//! names such as `project` and `project-other` are not nesting. A root that
//! cannot be probed aborts startup instead of being silently treated as
//! "different".

use crate::permissions::Permissions;
use crate::workspace::{Workspace, WorkspaceContext};
use crate::workspace_id::WorkspaceId;
use anyhow::{bail, Context};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// The only supported configuration format version.
pub const SUPPORTED_CONFIG_VERSION: u64 = 1;

/// Maximum number of workspaces a registry configuration may declare.
///
/// `list_workspaces` is the authoritative discovery mechanism and M2 gives
/// it no pagination and no truncation, so the registry size itself is
/// bounded at startup instead. Worst case — the full 64 workspaces, each
/// with a maximum-length 64-character id and every permission enabled —
/// serializes to roughly 25 KiB across the response's two representations
/// (structured content plus pretty text fallback), comfortably inside the
/// server's ~256 KiB bounded-output envelope, while leaving room for any
/// realistic operator layout. Exceeding the limit is a startup error, not a
/// silent shortening of discovery output.
pub const MAX_REGISTRY_WORKSPACES: usize = 64;

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
    /// Returns the context as an owned [`Arc`] so a request handler can hold
    /// it independently of the registry borrow; cloning an `Arc` is cheap and
    /// the registry itself remains immutable.
    pub fn get(&self, id: &WorkspaceId) -> Option<Arc<WorkspaceContext>> {
        self.workspaces.get(id).cloned()
    }

    /// All contexts in deterministic workspace-id order, for diagnostics.
    pub fn iter_sorted(&self) -> Vec<&Arc<WorkspaceContext>> {
        let mut contexts: Vec<&Arc<WorkspaceContext>> = self.workspaces.values().collect();
        contexts.sort_by_key(|ctx| ctx.id().cloned());
        contexts
    }

    /// Validate every declared workspace, then reject duplicate or nested
    /// roots by filesystem identity, and only then freeze the registry.
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
        // Checked before any per-workspace probing: an oversized registry is
        // rejected up front, deterministically, and before MCP serving.
        if config.workspaces.len() > MAX_REGISTRY_WORKSPACES {
            bail!(
                "Workspace config declares {} workspaces, but a registry may contain at most \
                 {MAX_REGISTRY_WORKSPACES}. list_workspaces is the authoritative discovery \
                 mechanism and is never truncated, so registry size is bounded at startup \
                 instead.",
                config.workspaces.len()
            );
        }

        // BTreeMap keys iterate in sorted order, so validation errors are
        // deterministic across runs regardless of declaration order.
        let mut contexts: Vec<(WorkspaceId, PathBuf, Arc<WorkspaceContext>, FsIdentity)> =
            Vec::new();
        for (raw_id, entry) in &config.workspaces {
            let id = WorkspaceId::parse(raw_id).map_err(anyhow::Error::msg)?;

            if entry.allow_shell && !entry.exec {
                bail!(
                    "Workspace '{id}': allow_shell = true requires exec = true. \
                     Shell execution is a superset of program execution."
                );
            }

            if !entry.root.is_absolute() {
                bail!(
                    "Workspace '{id}': root '{}' is a relative path. Registry roots \
                     must be absolute so the policy cannot change with the server's \
                     working directory.",
                    entry.root.display()
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
            let identity = FsIdentity::probe(&canonical)?;

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
                identity,
            ));
        }

        // Duplicate and nested roots are rejected in both directions using
        // filesystem identity, so symlink aliases and case-variant spellings
        // cannot smuggle a second registration of the same directory in,
        // while genuinely distinct directories never collide. A probe error
        // aborts startup: an unobservable comparison must not default to
        // "different".
        for i in 0..contexts.len() {
            for j in (i + 1)..contexts.len() {
                let (id_a, root_a, _, identity_a) = &contexts[i];
                let (id_b, root_b, _, identity_b) = &contexts[j];
                if identity_a.same_as(identity_b) {
                    bail!(
                        "Workspaces '{id_a}' and '{id_b}' resolve to the same directory ('{}'). \
                         Every workspace must have a distinct root.",
                        root_a.display()
                    );
                }
                if identity_within(identity_a, root_b)? {
                    bail!(
                        "Workspace '{id_b}' root '{}' is nested inside workspace '{id_a}' root '{}'. \
                         Nested workspace roots are forbidden: a broader writable workspace would \
                         bypass a narrower read-only one.",
                        root_b.display(),
                        root_a.display()
                    );
                }
                if identity_within(identity_b, root_a)? {
                    bail!(
                        "Workspace '{id_a}' root '{}' is nested inside workspace '{id_b}' root '{}'. \
                         Nested workspace roots are forbidden: a broader writable workspace would \
                         bypass a narrower read-only one.",
                        root_a.display(),
                        root_b.display()
                    );
                }
            }
        }

        let workspaces = contexts
            .into_iter()
            .map(|(id, _, ctx, _)| (id, ctx))
            .collect::<HashMap<_, _>>();
        tracing::debug!(count = workspaces.len(), "workspace registry built");
        Ok(Self { workspaces })
    }
}

/// Filesystem identity of an existing path, used for security-sensitive
/// root comparison during registry validation.
///
/// Comparison never consults path strings or case folding: the OS-level
/// identity (device + inode on Unix, volume serial + file index on Windows)
/// makes two roots that denote the same directory — through symlinks, or
/// through differently-cased spellings on a case-insensitive volume —
/// compare equal, while genuinely distinct directories never collide
/// regardless of how similarly they are named. This deliberately avoids
/// assuming that a whole platform's filesystems share one case behavior.
struct FsIdentity(same_file::Handle);

impl FsIdentity {
    /// Probe identity, failing loudly: startup validation that cannot
    /// observe the filesystem must abort rather than guess "different".
    fn probe(path: &Path) -> anyhow::Result<Self> {
        same_file::Handle::from_path(path)
            .map(Self)
            .with_context(|| {
                format!(
                    "Failed to inspect filesystem identity of '{}' while validating \
                     workspace roots.",
                    path.display()
                )
            })
    }

    /// True when both handles denote the same on-disk object.
    fn same_as(&self, other: &FsIdentity) -> bool {
        self.0 == other.0
    }
}

/// True when `ancestor`'s identity appears on `candidate`'s real ancestor
/// chain (strictly above it; equality is decided by identity comparison at
/// the call site).
///
/// `candidate` must be a canonical path, so its `Path::parent` chain is the
/// actual on-disk hierarchy — symlink resolution has already happened — and
/// every ancestor therefore exists, making each probe sound. This is what
/// keeps sibling names such as `project` and `project-other` distinct: the
/// chain of `project-other` never passes through `project`.
fn identity_within(ancestor: &FsIdentity, candidate: &Path) -> anyhow::Result<bool> {
    let mut current = candidate.parent();
    while let Some(dir) = current {
        if FsIdentity::probe(dir)?.same_as(ancestor) {
            return Ok(true);
        }
        current = dir.parent();
    }
    Ok(false)
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
    fn accepts_registry_with_exactly_the_maximum_workspace_count() {
        let tmp = TempDir::new().unwrap();
        let mut entries: Vec<(String, PathBuf, String)> = Vec::new();
        for i in 0..MAX_REGISTRY_WORKSPACES {
            let dir = make_dir(&tmp, &format!("ws{i:02}"));
            entries.push((
                format!("w{i:02}"),
                dir,
                "write = true\nexec = true".to_string(),
            ));
        }
        let refs: Vec<(&str, &Path, &str)> = entries
            .iter()
            .map(|(id, dir, perms)| (id.as_str(), dir.as_path(), perms.as_str()))
            .collect();
        let registry = build_ok(&config_for(&refs));

        // Deterministic WorkspaceId order is preserved at the maximum size.
        let ids: Vec<String> = registry
            .iter_sorted()
            .iter()
            .map(|ctx| ctx.id().unwrap().to_string())
            .collect();
        assert_eq!(ids.len(), MAX_REGISTRY_WORKSPACES);
        assert_eq!(ids.first().unwrap(), "w00");
        assert_eq!(ids.last().unwrap(), "w63");
    }

    #[test]
    fn rejects_registry_above_the_maximum_workspace_count() {
        // The count check runs before any per-workspace probing, so
        // nonexistent roots are sufficient here: the rejection must be the
        // registry-size bound itself, not a missing-directory error.
        let mut config = String::from("version = 1\n\n");
        for i in 0..=MAX_REGISTRY_WORKSPACES {
            config.push_str(&format!(
                "[workspaces.w{i:02}]\nroot = '/nonexistent/ws{i:02}'\n\n"
            ));
        }

        let err = build_err(&config);
        assert!(
            err.contains(&format!(
                "declares {} workspaces",
                MAX_REGISTRY_WORKSPACES + 1
            )),
            "{err}"
        );
        assert!(
            err.contains(&format!("at most {MAX_REGISTRY_WORKSPACES}")),
            "{err}"
        );
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
        assert!(err.contains("same directory"), "{err}");
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
        assert!(err.contains("same directory"), "{err}");
    }

    #[test]
    fn accepts_absolute_root() {
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");
        assert!(ws.is_absolute(), "temp roots are absolute on all targets");
        let registry = build_ok(&config_for(&[("ws", &ws, "")]));
        assert_eq!(registry.iter_sorted()[0].root(), ws.canonicalize().unwrap());
    }

    #[test]
    fn rejects_relative_root() {
        let err = build_err("version = 1\n\n[workspaces.ws]\nroot = 'relative/project'\n");
        assert!(err.contains("must be absolute"), "{err}");
    }

    #[test]
    fn case_distinct_roots_follow_filesystem_case_semantics() {
        // Create two case-distinct candidate directories and let the
        // filesystem decide whether they are the same object: on a
        // case-insensitive volume `Alpha`/`alpha` collide and must be
        // rejected as duplicates; on a case-sensitive volume they are two
        // real directories and both must register.
        let tmp = TempDir::new().unwrap();
        let upper = make_dir(&tmp, "Alpha");
        let lower = tmp.path().join("alpha");
        if lower.exists() {
            let err = build_err(&config_for(&[("upper", &upper, ""), ("lower", &lower, "")]));
            assert!(err.contains("same directory"), "{err}");
        } else {
            std::fs::create_dir(&lower).unwrap();
            let registry = build_ok(&config_for(&[("upper", &upper, ""), ("lower", &lower, "")]));
            assert_eq!(registry.iter_sorted().len(), 2);
        }
    }

    #[test]
    fn quoted_dotted_workspace_id_loads_as_one_id() {
        let tmp = TempDir::new().unwrap();
        let ws = make_dir(&tmp, "ws");
        // `.` is valid inside a WorkspaceId, but TOML dotted table syntax
        // would split an unquoted header into nested tables, so the id must
        // be quoted.
        let toml = format!(
            "version = 1\n\n[workspaces.\"project.v2\"]\nroot = '{}'\n",
            ws.display()
        );
        let registry = build_ok(&toml);
        assert_eq!(registry.iter_sorted().len(), 1);
        assert_eq!(
            registry.iter_sorted()[0].id().unwrap().as_str(),
            "project.v2"
        );
        assert!(registry
            .get(&WorkspaceId::parse("project.v2").unwrap())
            .is_some());

        // Unquoted, the header describes a workspace literally named
        // `project` with an unknown `v2` field — rejected strictly.
        let unquoted = format!(
            "version = 1\n\n[workspaces.project.v2]\nroot = '{}'\n",
            ws.display()
        );
        let err = build_err(&unquoted);
        assert!(err.contains("unknown field"), "{err}");
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
