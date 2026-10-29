//! Root confinement for the tool executor: the canonicalized
//! [`SnapshotRoot`] plus the [`ConfinedPath`] proof that a path
//! resolved inside it. Only [`crate::tools::RepoToolExecutor`]
//! holds a root; only [`SnapshotRoot::confine`] mints a confined
//! path, so a tool method holding one never re-checks containment.

use std::path::{Path, PathBuf};

use super::error::ToolError;

/// The canonicalized snapshot root reads are confined under.
/// Built once in [`crate::tools::RepoToolExecutor::new`] so every
/// later tool call compares canonical paths.
pub(super) struct SnapshotRoot(PathBuf);

impl SnapshotRoot {
    /// Canonicalize `root` once; every later confinement check
    /// compares canonical paths.
    pub(super) fn canonicalize(root: PathBuf) -> std::io::Result<Self> {
        root.canonicalize().map(Self)
    }

    /// Resolve a repo-relative `rel` to a canonical path under the
    /// root. Absolute paths and escapes (including via symlinks, which
    /// `canonicalize` follows) are refused; a target that cannot be
    /// resolved is missing.
    pub(super) fn confine(&self, rel: &str) -> Result<ConfinedPath, ToolError> {
        if rel.is_empty() {
            return Err(ToolError::InvalidArguments(
                "path must not be empty".to_string(),
            ));
        }
        if Path::new(rel).is_absolute() {
            return Err(ToolError::PathEscapesRoot {
                path: rel.to_string(),
            });
        }
        let candidate = self.0.join(rel).canonicalize().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                ToolError::NotFound {
                    path: rel.to_string(),
                }
            } else {
                ToolError::Io {
                    path: rel.to_string(),
                    message: error.to_string(),
                }
            }
        })?;
        if !candidate.starts_with(&self.0) {
            return Err(ToolError::PathEscapesRoot {
                path: rel.to_string(),
            });
        }
        Ok(ConfinedPath(candidate))
    }

    /// Join a repo-relative `rel` onto the root without resolving
    /// it: for re-reading a path confinement already minted.
    pub(super) fn join_rel(&self, rel: &str) -> PathBuf {
        self.0.join(rel)
    }

    /// The canonicalized root path.
    pub(super) fn as_path(&self) -> &Path {
        &self.0
    }
}

/// A path [`SnapshotRoot::confine`] resolved inside the root.
/// Constructible only in this module, so holding one proves
/// confinement without re-checking.
pub(super) struct ConfinedPath(PathBuf);

impl ConfinedPath {
    /// The confined canonical path.
    pub(super) fn as_path(&self) -> &Path {
        &self.0
    }
}

/// Normalize a `list_dir` input for the payload: strip a trailing
/// `/` and any leading `./` segments, so `sub/`, `./sub`, and `sub`
/// all report `sub` while `.` (the snapshot root) stays `.`.
pub(super) fn normalize_rel_path(input: &str) -> String {
    let mut rest = input.trim_end_matches('/');
    if rest.is_empty() {
        return ".".to_string();
    }
    while let Some(stripped) = rest.strip_prefix("./") {
        rest = stripped.trim_end_matches('/');
        if rest.is_empty() {
            return ".".to_string();
        }
    }
    rest.to_string()
}
