//! Central workspace path resolver (spec section 6).
//!
//! Every filesystem-facing tool goes through [`Workspace::resolve`] before
//! touching disk. The resolver performs three ordered stages:
//!
//! 1. **Lexical normalization** — collapse `.`, drop redundant separators,
//!    and reject any request whose `..` components climb above their base
//!    directory before the OS is ever consulted.
//! 2. **Physical resolution** — canonicalize the deepest existing ancestor,
//!    so symlinked directories inside the workspace cannot retarget a path
//!    outside it. Missing leaf components stay lexical (that is what allows
//!    write tools to address files that do not exist yet).
//! 3. **Containment verification** — the fully resolved path must sit inside
//!    the canonical workspace root, otherwise the request is rejected.
//!
//! This logic must never be duplicated in individual tools.

use crate::error::{ToolError, ToolResult};
use crate::permissions::Permissions;
use crate::workspace_id::WorkspaceId;
use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

#[derive(Debug, Clone)]
pub struct Workspace {
    root_canonical: PathBuf,
}

impl Workspace {
    /// Open a workspace rooted at `root`, canonicalizing it once up front.
    pub fn open(root: &Path) -> anyhow::Result<Self> {
        let root_canonical = std::fs::canonicalize(root).map_err(|e| {
            anyhow::anyhow!("Failed to resolve workspace root '{}': {e}", root.display())
        })?;
        tracing::debug!(root = %root_canonical.display(), "workspace opened");
        Ok(Self { root_canonical })
    }

    pub fn root(&self) -> &Path {
        &self.root_canonical
    }

    pub fn name(&self) -> String {
        self.root_canonical
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| self.root_canonical.to_string_lossy().into_owned())
    }

    /// Render `path` relative to the workspace root for display purposes.
    pub fn display_relative(&self, path: &Path) -> String {
        match path.strip_prefix(&self.root_canonical) {
            Ok(rel) if !rel.as_os_str().is_empty() => rel.to_string_lossy().into_owned(),
            _ => path.to_string_lossy().into_owned(),
        }
    }

    /// Resolve an untrusted, client-supplied path against the workspace.
    ///
    /// Accepts `""`/`None` (the root itself), plain relative paths such as
    /// `"src/main.rs"`, and absolute paths. The returned path may refer to a
    /// file that does not exist yet.
    pub fn resolve(&self, requested: Option<&str>) -> ToolResult<PathBuf> {
        let requested = requested.unwrap_or("");
        if requested.contains('\0') {
            return Err(ToolError::msg(
                "Invalid path: NUL bytes are not allowed in workspace paths.",
            ));
        }
        if requested.trim().is_empty() {
            return Ok(self.root_canonical.clone());
        }

        let req = Path::new(requested);
        let escape_err = || {
            ToolError::msg(format!(
                "Path resolves outside the configured workspace: {requested}"
            ))
        };

        // Absolute request that matches the canonical root string form?
        let rooted_rel = if req.is_absolute() {
            req.strip_prefix(&self.root_canonical).ok()
        } else {
            None
        };

        // Stage 1: lexical normalization into a flat list of names.
        let mut stack: Vec<OsString> = Vec::new();
        let mut absolute_rebuild = PathBuf::new();
        let treat_as_absolute;

        if let Some(rel) = rooted_rel {
            // Absolute spelling of something under the workspace: normalize
            // the remainder exactly like a relative path.
            collect_lexical(rel, &mut stack, escape_err)?;
            treat_as_absolute = false;
        } else if req.is_absolute() {
            // Foreign absolute path: normalize while keeping its prefix,
            // then let stages 2–3 decide containment physically (this also
            // resolves alias spellings such as macOS `/var` vs `/private/var`).
            for component in req.components() {
                match component {
                    Component::Normal(name) => stack.push(name.to_os_string()),
                    Component::CurDir => {}
                    Component::Prefix(p) => absolute_rebuild.push(p.as_os_str()),
                    Component::RootDir => {
                        absolute_rebuild.push(Path::new("/").as_os_str());
                    }
                    Component::ParentDir => {
                        if stack.pop().is_none() {
                            return Err(escape_err());
                        }
                    }
                }
            }
            treat_as_absolute = true;
        } else {
            collect_lexical(req, &mut stack, escape_err)?;
            treat_as_absolute = false;
        }

        let mut candidate = if treat_as_absolute {
            absolute_rebuild
        } else {
            self.root_canonical.clone()
        };
        if stack.is_empty() && !treat_as_absolute {
            return Ok(self.root_canonical.clone());
        }
        if stack.is_empty() && treat_as_absolute && !candidate.as_os_str().is_empty() {
            // Bare "/" (or a drive root) cannot be inside a real workspace
            // unless the workspace root *is* that root; the containment check
            // below settles it either way once canonicalized.
            let canon = std::fs::canonicalize(&candidate).map_err(|_| {
                ToolError::msg(format!(
                    "Path resolves outside the configured workspace: {requested}"
                ))
            })?;
            if !canon.starts_with(&self.root_canonical) {
                return Err(escape_err());
            }
            return Ok(canon);
        }
        for part in &stack {
            candidate.push(part);
        }

        // Stage 2: physical resolution through existing ancestors (symlink-aware).
        let resolved = canonicalize_deepest(&candidate)?;

        // Stage 3: containment verification.
        if !resolved.starts_with(&self.root_canonical) {
            return Err(escape_err());
        }
        Ok(resolved)
    }
}

/// A fully validated workspace execution context (v0.2 M1 refactor).
///
/// Bundles the three things every tool ultimately operates against:
///
/// - the logical identity (`Some` only in registry mode; single-workspace
///   mode is anonymous), carried by [`WorkspaceId`];
/// - the canonical root plus the hardened root-bound resolver, which remains
///   the single authority for traversal rejection, absolute-path boundary
///   checks, symlink-escape protection, and lazy write-target resolution;
/// - the capability set for this workspace.
///
/// The context is immutable once built at startup. It deliberately adds no
/// path logic of its own — isolation decisions stay in [`Workspace`].
#[derive(Debug, Clone)]
pub struct WorkspaceContext {
    id: Option<WorkspaceId>,
    resolver: Workspace,
    permissions: Permissions,
}

impl WorkspaceContext {
    /// Assemble a context from an already-opened resolver.
    ///
    /// Registry mode passes `Some(id)`; single-workspace mode passes `None`.
    pub fn new(id: Option<WorkspaceId>, resolver: Workspace, permissions: Permissions) -> Self {
        Self {
            id,
            resolver,
            permissions,
        }
    }

    /// Logical workspace id (registry mode only).
    pub fn id(&self) -> Option<&WorkspaceId> {
        self.id.as_ref()
    }

    /// Canonical workspace root.
    pub fn root(&self) -> &Path {
        self.resolver.root()
    }

    /// The root-bound path resolver — the single path-isolation authority.
    pub fn resolver(&self) -> &Workspace {
        &self.resolver
    }

    /// Capability set for this workspace.
    pub fn permissions(&self) -> &Permissions {
        &self.permissions
    }
}

/// Push normalized components of `rel` onto `stack`, failing if the path
/// tries to ascend above its base directory.
fn collect_lexical(
    rel: &Path,
    stack: &mut Vec<OsString>,
    escape_err: impl Fn() -> ToolError,
) -> ToolResult<()> {
    for component in rel.components() {
        match component {
            Component::Normal(name) => stack.push(name.to_os_string()),
            Component::CurDir => {}
            Component::ParentDir => {
                if stack.pop().is_none() {
                    return Err(escape_err());
                }
            }
            // Only reachable for the post-strip remainder of an absolute
            // path whose prefix matched the workspace root; such remainders
            // contain no additional RootDir/Prefix components.
            Component::RootDir | Component::Prefix(_) => {}
        }
    }
    Ok(())
}

/// Canonicalize `candidate` by walking up to the deepest ancestor that
/// exists on disk, then re-appending the missing suffix lexically.
///
/// Because callers have already removed all `..` components, the missing
/// suffix consists purely of regular names and can be trusted lexically;
/// symlinks anywhere along the existing spine are fully resolved.
fn canonicalize_deepest(candidate: &Path) -> ToolResult<PathBuf> {
    if let Ok(canon) = std::fs::canonicalize(candidate) {
        return Ok(canon);
    }

    let mut missing: Vec<OsString> = Vec::new();
    let mut probe = candidate.to_path_buf();
    loop {
        match std::fs::canonicalize(&probe) {
            Ok(existing) => {
                let mut resolved = existing;
                for part in missing.iter().rev() {
                    resolved.push(part);
                }
                return Ok(resolved);
            }
            Err(_) => match probe.file_name() {
                Some(name) => {
                    missing.push(name.to_os_string());
                    if !probe.pop() {
                        return Err(ToolError::msg(
                                "Failed to resolve path: reached the filesystem root without finding an existing ancestor.",
                            ));
                    }
                }
                None => {
                    return Err(ToolError::msg(
                        "Failed to resolve path: exhausted all path components.",
                    ));
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_workspace(files: &[&str]) -> (TempDir, Workspace) {
        let tmp = TempDir::new().expect("tempdir");
        for rel in files {
            let p = tmp.path().join(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, b"contents\n").expect("write fixture");
        }
        let ws = Workspace::open(tmp.path()).expect("open workspace");
        (tmp, ws)
    }

    #[test]
    fn plain_relative_paths_resolve() {
        let (_tmp, ws) = make_workspace(&["src/main.rs"]);
        let resolved = ws.resolve(Some("src/main.rs")).unwrap();
        assert_eq!(resolved, ws.root().join("src").join("main.rs"));
    }

    #[test]
    fn dot_components_are_collapsed() {
        let (_tmp, ws) = make_workspace(&["src/main.rs"]);
        let resolved = ws.resolve(Some("./src/../src/./main.rs")).unwrap();
        assert_eq!(resolved, ws.root().join("src").join("main.rs"));
    }

    #[test]
    fn nested_directories_inside_remain_inside() {
        let (_tmp, ws) = make_workspace(&["a/b/c.txt"]);
        let resolved = ws.resolve(Some("a/b/../b/c.txt")).unwrap();
        assert!(resolved.ends_with("a/b/c.txt"));
    }

    #[test]
    fn single_parent_traversal_outside_is_rejected() {
        let (_tmp, ws) = make_workspace(&[]);
        assert!(ws.resolve(Some("../escape")).is_err());
    }

    #[test]
    fn deep_parent_traversal_is_rejected() {
        let (_tmp, ws) = make_workspace(&[]);
        assert!(ws.resolve(Some("../../.ssh/id_ed25519")).is_err());
    }

    #[test]
    fn traversal_then_back_inside_is_allowed_only_if_it_stays_in() {
        let (_tmp, ws) = make_workspace(&["in/a.txt", "in/b.txt"]);
        let resolved = ws.resolve(Some("in/../in/b.txt")).unwrap();
        assert!(resolved.ends_with("in/b.txt"));
        assert!(ws.resolve(Some("in/../../dir/a.txt")).is_err());
    }

    #[test]
    fn absolute_path_outside_workspace_is_rejected() {
        let (_tmp, ws) = make_workspace(&[]);
        let outside = if cfg!(windows) {
            r"C:\Windows\System32"
        } else {
            "/etc/passwd"
        };
        assert!(
            ws.resolve(Some(outside)).is_err(),
            "expected rejection for {outside}"
        );
    }

    #[test]
    fn absolute_path_inside_workspace_resolves() {
        let (_tmp, ws) = make_workspace(&["file.txt"]);
        let abs = ws.root().join("file.txt");
        let as_str = abs.to_string_lossy().into_owned();
        let resolved = ws.resolve(Some(&as_str)).unwrap();
        assert_eq!(resolved, abs);
    }

    #[test]
    fn missing_file_under_existing_directory_resolves_lazily() {
        let (_tmp, ws) = make_workspace(&["dir"]);
        let resolved = ws.resolve(Some("dir/new-file.rs")).unwrap();
        assert_eq!(resolved, ws.root().join("dir").join("new-file.rs"));
        assert!(!resolved.exists());
    }

    #[test]
    fn missing_file_under_missing_parents_resolves_lazily() {
        let (_tmp, ws) = make_workspace(&[]);
        let resolved = ws.resolve(Some("a/b/c/d.txt")).unwrap();
        assert_eq!(
            resolved,
            ws.root().join("a").join("b").join("c").join("d.txt")
        );
    }

    #[cfg(unix)]
    mod unix_symlinks {
        use super::*;
        use std::os::unix::fs::symlink;

        #[test]
        fn symlink_escape_to_outside_target_is_rejected() {
            let (tmp, ws) = make_workspace(&[]);

            // Directory symlink pointing outside the workspace.
            symlink("/", tmp.path().join("evil_root")).unwrap();

            assert!(
                ws.resolve(Some("evil_root/etc/passwd")).is_err(),
                "symlink escape via existing target must be rejected"
            );

            // Relative traversal symlink pointing outside.
            symlink("..", tmp.path().join("up")).unwrap();
            assert!(ws.resolve(Some("up/secret")).is_err());

            let _ = tmp;
        }

        #[test]
        fn symlink_to_inside_target_is_allowed() {
            let (tmp, ws) = make_workspace(&["real/data.txt"]);
            symlink(tmp.path().join("real"), tmp.path().join("alias")).unwrap();
            let resolved = ws.resolve(Some("alias/data.txt")).unwrap();
            assert!(resolved.ends_with("data.txt"));
            assert!(resolved.starts_with(ws.root()));
        }

        #[test]
        fn dangling_symlink_to_missing_outside_is_rejected_on_existing_spine() {
            let (tmp, ws) = make_workspace(&[]);
            symlink("/", tmp.path().join("rootlink")).unwrap();
            assert!(ws.resolve(Some("rootlink/nonexistent/deep/file")).is_err());
        }
    }

    #[cfg(windows)]
    mod windows_specific {
        use super::*;

        #[test]
        fn drive_letter_request_outside_is_rejected() {
            let (_tmp, ws) = make_workspace(&[]);
            assert!(ws.resolve(Some(r"C:\some\where\else")).is_err());
        }

        #[test]
        fn backslash_separators_are_accepted_within_workspace() {
            let (_tmp, ws) = make_workspace(&["src/main.rs"]);
            let resolved = ws.resolve(Some(r"src\main.rs")).unwrap();
            assert!(resolved.ends_with("src\\main.rs"));
        }
    }

    #[test]
    fn nul_bytes_are_rejected() {
        let (_tmp, ws) = make_workspace(&[]);
        assert!(ws.resolve(Some("bad\0name")).is_err());
    }

    #[test]
    fn empty_and_dot_requests_return_the_root() {
        let (_tmp, ws) = make_workspace(&[]);
        assert_eq!(ws.resolve(None).unwrap(), ws.root());
        assert_eq!(ws.resolve(Some("")).unwrap(), ws.root());
        assert_eq!(ws.resolve(Some(".")).unwrap(), ws.root());
    }

    #[test]
    fn display_relative_strips_prefix() {
        let (_tmp, ws) = make_workspace(&["src/main.rs"]);
        let p = ws.resolve(Some("src/main.rs")).unwrap();
        assert_eq!(ws.display_relative(&p), "src/main.rs");
    }
}
