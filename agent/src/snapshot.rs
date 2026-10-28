//! Repository snapshot: the run's view of the PR-head checkout (FRG-17).
//!
//! [`Snapshot::capture`] takes the workflow's existing PR-head checkout
//! and records three things, all stamped with the head SHA (§9 snapshot
//! integrity — every later read happens under this SHA):
//!
//! - the head SHA itself (`git rev-parse HEAD`; a depth-1 shallow
//!   checkout is fine, absent history is never an error),
//! - the pruned repo map ([`RepoMap`]): every content file with its
//!   size, sorted by path, capped at [`MAX_REPO_MAP_ENTRIES`] with the
//!   over-cap case made visible (`total_files` + `truncated`) rather
//!   than silently short,
//! - the changed-file list parsed from the PR diff, in diff order.
//!
//! Consumers are the lens sessions (§8.5), the synthesizer (§8.8), and
//! the PR-Agent adapter, which receive the map as session context
//! (hence the serde derives). There is deliberately no port trait here:
//! capture is directly testable against fixture directory trees — the
//! filesystem is the seam, exactly like the tool executor's root path
//! in FRG-18. If the FRG-19 driver wants a trait, it is purely additive
//! then.
//!
//! Failures are typed [`SnapshotError`]s — never panics, never silent
//! skips. An unreadable entry mid-walk is an error, not an omission: a
//! partial map presented as complete would mislead the lenses.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;

use serde::{Deserialize, Serialize};

/// Cap on repo-map entries. The walk still counts every file, so a repo
/// over the cap is visible through [`RepoMap::total_files`] and
/// [`RepoMap::truncated`] instead of silently short.
pub const MAX_REPO_MAP_ENTRIES: usize = 10_000;

/// Directories the walk never descends, by name, at any depth: VCS
/// internals and build/dependency output are not review content.
const PRUNED_DIRS: [&str; 7] = [
    ".git",
    "target",
    "build",
    "node_modules",
    "vendor",
    ".gradle",
    "__pycache__",
];

/// Lockfiles excluded from the map, by file name, at any depth: they are
/// generated manifests, not reviewable source.
const LOCKFILES: [&str; 9] = [
    "Cargo.lock",
    "package-lock.json",
    "yarn.lock",
    "pnpm-lock.yaml",
    "Gemfile.lock",
    "poetry.lock",
    "Pipfile.lock",
    "go.sum",
    "composer.lock",
];

/// One content file in the repo map: repo-relative path (forward
/// slashes on every platform) and its size in bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoEntry {
    pub path: String,
    pub size: u64,
}

/// The pruned map of the snapshot tree, stamped with the head SHA it
/// was walked from — the artifact itself carries the stamp, so any
/// consumer holding the map knows which tree it describes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RepoMap {
    pub head_sha: String,
    /// Files in the map, sorted by path, at most the capture limit.
    pub entries: Vec<RepoEntry>,
    /// Every content file the walk counted (after pruning and lockfile
    /// exclusion, before the cap).
    pub total_files: usize,
    /// True when `total_files` exceeded the cap and `entries` holds
    /// only the first entries in sorted order.
    pub truncated: bool,
}

impl RepoMap {
    /// Plain-text rendering for prompt-shaped consumption: one
    /// `path (size)` line per entry, each line newline-terminated.
    pub fn render(&self) -> String {
        let mut out = String::new();
        for entry in &self.entries {
            out.push_str(&format!("{} ({})\n", entry.path, entry.size));
        }
        out
    }
}

/// How one file changed in the PR diff.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ChangeKind {
    Added,
    Modified,
    Deleted,
    /// Renamed; `from` is the pre-rename repo-relative path and the
    /// owning [`ChangedFile::path`] is the new path.
    Renamed {
        from: String,
    },
}

/// One entry of the changed-file list, in diff order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChangedFile {
    pub path: String,
    pub change: ChangeKind,
}

/// The captured snapshot: head SHA, checkout root, repo map, and the
/// changed-file list. `head_sha` here is the single source the driver
/// (FRG-19+) stamps findings and decision logs from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub head_sha: String,
    pub root: PathBuf,
    pub repo_map: RepoMap,
    pub changed_files: Vec<ChangedFile>,
}

/// Typed snapshot failures. Every failure path lands in one of these
/// variants; capture never panics and never returns a partial result.
#[derive(Debug)]
pub enum SnapshotError {
    /// The checkout's HEAD could not be resolved: the root is not a git
    /// checkout (or has no commits). `message` carries git's stderr.
    NotAGitCheckout { root: PathBuf, message: String },
    /// `git` could not be spawned at all.
    GitUnavailable(String),
    /// A filesystem read failed at `path` (walking the tree, reading
    /// the diff, ...).
    Io { path: PathBuf, message: String },
    /// The diff's structure could not be parsed.
    DiffParse { message: String },
}

impl fmt::Display for SnapshotError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SnapshotError::NotAGitCheckout { root, message } => {
                write!(
                    f,
                    "not a git checkout (or no HEAD commit): {}: {message}",
                    root.display()
                )
            }
            SnapshotError::GitUnavailable(detail) => {
                write!(f, "git is unavailable: {detail}")
            }
            SnapshotError::Io { path, message } => {
                write!(f, "cannot read {}: {message}", path.display())
            }
            SnapshotError::DiffParse { message } => {
                write!(f, "cannot parse diff: {message}")
            }
        }
    }
}

impl std::error::Error for SnapshotError {}

fn io_error(path: &Path, error: std::io::Error) -> SnapshotError {
    SnapshotError::Io {
        path: path.to_path_buf(),
        message: error.to_string(),
    }
}

impl Snapshot {
    /// Capture the snapshot of the PR-head checkout at `repo_root`,
    /// with the changed-file list parsed from the unified diff at
    /// `diff_path`.
    pub fn capture(repo_root: &Path, diff_path: &Path) -> Result<Snapshot, SnapshotError> {
        Self::capture_with_limit(repo_root, diff_path, MAX_REPO_MAP_ENTRIES)
    }

    /// [`Snapshot::capture`] with an explicit repo-map cap — the seam
    /// the over-cap test drives so it need not synthesize 10,000 files.
    fn capture_with_limit(
        repo_root: &Path,
        diff_path: &Path,
        limit: usize,
    ) -> Result<Snapshot, SnapshotError> {
        let head_sha = head_sha(repo_root)?;
        let repo_map = build_repo_map(repo_root, &head_sha, limit)?;
        let diff_text =
            fs::read_to_string(diff_path).map_err(|error| io_error(diff_path, error))?;
        let changed_files = parse_changed_files(&diff_text)?;
        Ok(Snapshot {
            head_sha,
            root: repo_root.to_path_buf(),
            repo_map,
            changed_files,
        })
    }
}

/// Resolve `HEAD` in the checkout at `root`. Works on a depth-1 shallow
/// checkout; a missing `git` binary is [`SnapshotError::GitUnavailable`]
/// and a failed resolution is [`SnapshotError::NotAGitCheckout`].
fn head_sha(root: &Path) -> Result<String, SnapshotError> {
    let output = Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", "HEAD"])
        .output()
        .map_err(|error| SnapshotError::GitUnavailable(error.to_string()))?;
    if !output.status.success() {
        return Err(SnapshotError::NotAGitCheckout {
            root: root.to_path_buf(),
            message: String::from_utf8_lossy(&output.stderr).trim().to_string(),
        });
    }
    let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if sha.is_empty() {
        return Err(SnapshotError::NotAGitCheckout {
            root: root.to_path_buf(),
            message: "git rev-parse HEAD produced no output".to_string(),
        });
    }
    Ok(sha)
}

/// Walk `root` and build the repo map: files only, repo-relative
/// forward-slash paths, sizes from metadata. Entries are collected in
/// full, sorted by path for determinism, then cut to `limit` — so the
/// surviving entries are the first `limit` in sorted order and
/// `total_files` stays exact. Pruned directories are never descended
/// and lockfiles are never counted; symlinked directories are not
/// followed (a symlinked *file* is content, sized by its target).
fn build_repo_map(root: &Path, head_sha: &str, limit: usize) -> Result<RepoMap, SnapshotError> {
    let mut collected: Vec<RepoEntry> = Vec::new();
    let mut stack: Vec<(PathBuf, String)> = vec![(root.to_path_buf(), String::new())];
    while let Some((dir, rel)) = stack.pop() {
        let read_dir = fs::read_dir(&dir).map_err(|error| io_error(&dir, error))?;
        for entry in read_dir {
            let entry = entry.map_err(|error| io_error(&dir, error))?;
            let path = entry.path();
            let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                return Err(SnapshotError::Io {
                    path: path.clone(),
                    message: "file name is not valid UTF-8".to_string(),
                });
            };
            let rel_path = if rel.is_empty() {
                name.clone()
            } else {
                format!("{rel}/{name}")
            };
            let file_type = entry.file_type().map_err(|error| io_error(&path, error))?;
            if file_type.is_symlink() {
                // Followed once, here, for classification only: a
                // symlinked directory is not descended (and is not a
                // file); a symlinked file is content sized by target.
                let metadata = fs::metadata(&path).map_err(|error| io_error(&path, error))?;
                if metadata.is_dir() || LOCKFILES.contains(&name.as_str()) {
                    continue;
                }
                collected.push(RepoEntry {
                    path: rel_path,
                    size: metadata.len(),
                });
            } else if file_type.is_dir() {
                if PRUNED_DIRS.contains(&name.as_str()) {
                    continue;
                }
                stack.push((path, rel_path));
            } else if file_type.is_file() {
                if LOCKFILES.contains(&name.as_str()) {
                    continue;
                }
                let metadata = entry.metadata().map_err(|error| io_error(&path, error))?;
                collected.push(RepoEntry {
                    path: rel_path,
                    size: metadata.len(),
                });
            }
            // Anything else (fifo, socket, device) is not a file and
            // not map content.
        }
    }
    collected.sort_by(|a, b| a.path.cmp(&b.path));
    let total_files = collected.len();
    let truncated = total_files > limit;
    collected.truncate(limit);
    Ok(RepoMap {
        head_sha: head_sha.to_string(),
        entries: collected,
        total_files,
        truncated,
    })
}

/// Parse the changed-file list from a unified diff, in diff order.
///
/// `diff --git` headers supply the paths; the header block of each file
/// section (everything before its first `@@` hunk header) supplies the
/// kind markers: `new file mode` / `--- /dev/null` → Added,
/// `deleted file mode` / `+++ /dev/null` → Deleted, `rename from` /
/// `rename to` → Renamed, otherwise Modified. Binary diffs carry only
/// headers, so a binary change with no add/delete/rename marker is
/// Modified. Markers are only read in the header block — hunk content
/// lines can mimic them (a removed line whose text is `-- /dev/null`).
fn parse_changed_files(text: &str) -> Result<Vec<ChangedFile>, SnapshotError> {
    /// One in-progress file section of the diff.
    #[derive(Default)]
    struct Pending {
        a_path: Option<String>,
        b_path: Option<String>,
        added: bool,
        deleted: bool,
        rename_from: Option<String>,
        rename_to: Option<String>,
    }

    fn finish(pending: Pending) -> Result<ChangedFile, SnapshotError> {
        let Pending {
            a_path,
            b_path,
            added,
            deleted,
            rename_from,
            rename_to,
        } = pending;
        if let (Some(from), Some(to)) = (rename_from, rename_to) {
            return Ok(ChangedFile {
                path: to,
                change: ChangeKind::Renamed { from },
            });
        }
        let unparsed = || SnapshotError::DiffParse {
            message: "diff file section is missing its header paths".to_string(),
        };
        if added {
            Ok(ChangedFile {
                path: b_path.ok_or_else(unparsed)?,
                change: ChangeKind::Added,
            })
        } else if deleted {
            Ok(ChangedFile {
                path: a_path.ok_or_else(unparsed)?,
                change: ChangeKind::Deleted,
            })
        } else {
            Ok(ChangedFile {
                path: b_path.or(a_path).ok_or_else(unparsed)?,
                change: ChangeKind::Modified,
            })
        }
    }

    let mut changed = Vec::new();
    let mut pending: Option<Pending> = None;
    let mut in_hunks = false;
    for line in text.lines() {
        if let Some(rest) = line.strip_prefix("diff --git ") {
            if let Some(section) = pending.take() {
                changed.push(finish(section)?);
            }
            in_hunks = false;
            let (a_path, b_path) =
                parse_diff_git_paths(rest).ok_or_else(|| SnapshotError::DiffParse {
                    message: format!("cannot parse paths from diff header: {line}"),
                })?;
            pending = Some(Pending {
                a_path: Some(a_path),
                b_path: Some(b_path),
                ..Pending::default()
            });
            continue;
        }
        let Some(section) = pending.as_mut() else {
            continue; // preamble before the first file section
        };
        if in_hunks {
            continue;
        }
        if line.starts_with("@@") {
            in_hunks = true;
        } else if line.starts_with("new file mode") {
            section.added = true;
        } else if line.starts_with("deleted file mode") {
            section.deleted = true;
        } else if line == "--- /dev/null" {
            section.added = true;
        } else if line == "+++ /dev/null" {
            section.deleted = true;
        } else if let Some(from) = line.strip_prefix("rename from ") {
            section.rename_from = Some(unquote_path(from.trim()));
        } else if let Some(to) = line.strip_prefix("rename to ") {
            section.rename_to = Some(unquote_path(to.trim()));
        }
    }
    if let Some(section) = pending.take() {
        changed.push(finish(section)?);
    }
    Ok(changed)
}

/// Extract the `(a-side, b-side)` repo-relative paths from the part of
/// a `diff --git` line after the prefix, stripping the `a/` / `b/`
/// markers. Handles git's C-style quoting on either side (used for
/// paths with spaces or special characters).
fn parse_diff_git_paths(rest: &str) -> Option<(String, String)> {
    let (a_raw, b_raw) = split_diff_git_paths(rest)?;
    let a = a_raw.strip_prefix("a/").unwrap_or(&a_raw);
    let b = b_raw.strip_prefix("b/").unwrap_or(&b_raw);
    if a.is_empty() || b.is_empty() {
        return None;
    }
    Some((a.to_string(), b.to_string()))
}

/// Split `a/<path> b/<path>` (either side possibly quoted) into its two
/// raw sides. The unquoted split is the first ` b/` — sound unless a
/// path itself contains ` b/`, which git's own tooling also assumes
/// away.
fn split_diff_git_paths(rest: &str) -> Option<(String, String)> {
    if rest.starts_with('"') {
        let (a, after) = take_quoted(rest)?;
        let after = after.strip_prefix(' ')?;
        let b = if after.starts_with('"') {
            take_quoted(after)?.0
        } else {
            after.to_string()
        };
        Some((a, b))
    } else if let Some(index) = rest.find(" \"b/") {
        let b = take_quoted(&rest[index + 1..])?.0;
        Some((rest[..index].to_string(), b))
    } else {
        let index = rest.find(" b/")?;
        Some((rest[..index].to_string(), rest[index + 1..].to_string()))
    }
}

/// Unquote a possibly C-style-quoted path (the `rename from` / `rename
/// to` form); unquoted paths pass through unchanged.
fn unquote_path(raw: &str) -> String {
    if raw.starts_with('"') {
        take_quoted(raw)
            .map(|(content, _)| content)
            .unwrap_or_else(|| raw.to_string())
    } else {
        raw.to_string()
    }
}

/// Take a C-style quoted string at the start of `s` (`s` begins with
/// `"`); returns the unquoted contents and the remainder after the
/// closing quote. A backslash escape collapses to the escaped byte —
/// exact for `\"` and `\\`, the cases that matter for path splitting;
/// git's octal escapes for non-ASCII bytes are not decoded. Non-ASCII
/// UTF-8 sequences pass through byte-for-byte (their bytes never equal
/// `"` or `\`).
fn take_quoted(s: &str) -> Option<(String, &str)> {
    let bytes = s.as_bytes();
    if bytes.first() != Some(&b'"') {
        return None;
    }
    let mut out: Vec<u8> = Vec::new();
    let mut index = 1;
    while index < bytes.len() {
        match bytes[index] {
            b'\\' => {
                out.push(*bytes.get(index + 1)?);
                index += 2;
            }
            b'"' => {
                return Some((String::from_utf8_lossy(&out).into_owned(), &s[index + 1..]));
            }
            byte => {
                out.push(byte);
                index += 1;
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(label: &str) -> PathBuf {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock after epoch")
            .as_nanos();
        let dir = std::env::temp_dir().join(format!(
            "chomp-snapshot-{label}-{}-{nonce}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).expect("create temp dir");
        dir
    }

    fn write_file(root: &Path, rel: &str, contents: &str) {
        let path = root.join(rel);
        fs::create_dir_all(path.parent().expect("fixture path has a parent"))
            .expect("create fixture dirs");
        fs::write(&path, contents).expect("write fixture file");
    }

    fn paths(map: &RepoMap) -> Vec<&str> {
        map.entries
            .iter()
            .map(|entry| entry.path.as_str())
            .collect()
    }

    // -- repo map ---------------------------------------------------------

    #[test]
    fn repo_map_includes_content_and_prunes_the_rest() {
        let root = temp_dir("prune");
        write_file(&root, "src/main.rs", "fn main() {}\n");
        write_file(&root, "README.md", "hello\n");
        write_file(&root, ".github/workflows/x.yml", "on: push\n");
        write_file(&root, ".hidden", "h");
        write_file(&root, "sub/Cargo.toml", "[package]\n");
        // Every pruned category, present and required to be absent:
        // pruned directories (incl. nested by name), lockfiles at root
        // and nested.
        write_file(&root, ".git/HEAD", "ref: refs/heads/main\n");
        write_file(&root, "target/debug/app", "binary");
        write_file(&root, "node_modules/pkg/index.js", "x");
        write_file(&root, "vendor/lib.rs", "x");
        write_file(&root, "build/out.bin", "x");
        write_file(&root, ".gradle/cache.bin", "x");
        write_file(&root, "__pycache__/mod.pyc", "x");
        write_file(&root, "src/vendor/x.rs", "x");
        write_file(&root, "Cargo.lock", "lock");
        write_file(&root, "sub/package-lock.json", "lock");

        let map = build_repo_map(&root, "deadbeef", MAX_REPO_MAP_ENTRIES).expect("build map");
        assert_eq!(map.head_sha, "deadbeef");
        assert_eq!(
            paths(&map),
            vec![
                ".github/workflows/x.yml",
                ".hidden",
                "README.md",
                "src/main.rs",
                "sub/Cargo.toml",
            ]
        );
        assert_eq!(map.entries[3].size, 13); // src/main.rs, "fn main() {}\n"
        assert_eq!(map.total_files, 5);
        assert!(!map.truncated);
        assert_eq!(
            map.render(),
            ".github/workflows/x.yml (9)\n\
             .hidden (1)\n\
             README.md (6)\n\
             src/main.rs (13)\n\
             sub/Cargo.toml (10)\n"
        );
        fs::remove_dir_all(&root).expect("clean temp dir");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directories_are_not_followed() {
        use std::os::unix::fs::symlink;
        let root = temp_dir("links");
        write_file(&root, "real/file.txt", "data");
        symlink(root.join("real"), root.join("linkdir")).expect("link dir");
        symlink(root.join("real/file.txt"), root.join("linkfile.txt")).expect("link file");

        let map = build_repo_map(&root, "sha", MAX_REPO_MAP_ENTRIES).expect("build map");
        // The symlinked directory contributes nothing; the symlinked
        // file is content, sized by its target.
        assert_eq!(paths(&map), vec!["linkfile.txt", "real/file.txt"]);
        assert!(map.entries.iter().all(|entry| entry.size == 4));
        assert_eq!(map.total_files, 2);
        fs::remove_dir_all(&root).expect("clean temp dir");
    }

    // -- diff parsing -------------------------------------------------------

    const DIFF_FIXTURE: &str = "\
diff --git a/src/main.rs b/src/main.rs
index 1111111..2222222 100644
--- a/src/main.rs
+++ b/src/main.rs
@@ -1 +1 @@
-old
+new
diff --git a/src/new.rs b/src/new.rs
new file mode 100644
index 0000000..3333333
--- /dev/null
+++ b/src/new.rs
@@ -0,0 +1 @@
+brand new
diff --git a/src/old.rs b/src/old.rs
deleted file mode 100644
index 4444444..0000000
--- a/src/old.rs
+++ /dev/null
@@ -1 +0,0 @@
-gone
diff --git a/src/before.rs b/src/after.rs
similarity index 100%
rename from src/before.rs
rename to src/after.rs
diff --git a/assets/logo.png b/assets/logo.png
index 5555555..6666666 100644
Binary files a/assets/logo.png and b/assets/logo.png differ
";

    #[test]
    fn diff_fixture_parses_to_the_exact_changed_list() {
        let changed = parse_changed_files(DIFF_FIXTURE).expect("parse diff");
        assert_eq!(
            changed,
            vec![
                ChangedFile {
                    path: "src/main.rs".to_string(),
                    change: ChangeKind::Modified,
                },
                ChangedFile {
                    path: "src/new.rs".to_string(),
                    change: ChangeKind::Added,
                },
                ChangedFile {
                    path: "src/old.rs".to_string(),
                    change: ChangeKind::Deleted,
                },
                ChangedFile {
                    path: "src/after.rs".to_string(),
                    change: ChangeKind::Renamed {
                        from: "src/before.rs".to_string(),
                    },
                },
                ChangedFile {
                    path: "assets/logo.png".to_string(),
                    change: ChangeKind::Modified,
                },
            ]
        );
    }

    #[test]
    fn diff_paths_handle_renames_and_quoting() {
        assert_eq!(
            parse_diff_git_paths("a/src/x.rs b/src/y.rs"),
            Some(("src/x.rs".to_string(), "src/y.rs".to_string()))
        );
        assert_eq!(
            parse_diff_git_paths("\"a/my file.rs\" \"b/my file.rs\""),
            Some(("my file.rs".to_string(), "my file.rs".to_string()))
        );
        assert_eq!(parse_diff_git_paths("a/only.rs"), None);
    }

    #[test]
    fn unparseable_diff_header_is_a_typed_error() {
        let error = parse_changed_files("diff --git a/only.rs\n").expect_err("must fail");
        assert!(
            matches!(error, SnapshotError::DiffParse { .. }),
            "{error:?}"
        );
    }

    #[test]
    fn empty_diff_parses_to_no_changes() {
        assert_eq!(parse_changed_files("").expect("parse empty"), vec![]);
    }

    // -- capture (real git fixture) -----------------------------------------

    fn git(dir: &Path, args: &[&str]) -> String {
        let output = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.email=test@example.com",
                "-c",
                "user.name=Test",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .expect("run git");
        assert!(
            output.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout).trim().to_string()
    }

    /// A temp base holding `repo/` (a real git checkout with `files`
    /// committed) — the diff file lives beside the repo so the walk
    /// never sees it. Returns (base, repo).
    fn git_fixture(label: &str, files: &[&str]) -> (PathBuf, PathBuf) {
        let base = temp_dir(label);
        let repo = base.join("repo");
        fs::create_dir_all(&repo).expect("create repo dir");
        git(&repo, &["init"]);
        for file in files {
            write_file(&repo, file, "x\n");
        }
        git(&repo, &["add", "-A"]);
        git(&repo, &["commit", "-m", "initial"]);
        (base, repo)
    }

    #[test]
    fn capture_records_head_sha_map_and_changes() {
        let (base, repo) = git_fixture("capture", &["README.md", "src/lib.rs"]);
        let expected_sha = git(&repo, &["rev-parse", "HEAD"]);
        let diff = base.join("pr.diff");
        fs::write(
            &diff,
            "diff --git a/src/lib.rs b/src/lib.rs\n\
             index 1111111..2222222 100644\n\
             --- a/src/lib.rs\n\
             +++ b/src/lib.rs\n\
             @@ -1 +1 @@\n\
             -x\n\
             +y\n",
        )
        .expect("write diff");

        let snapshot = Snapshot::capture(&repo, &diff).expect("capture");
        assert_eq!(snapshot.head_sha, expected_sha);
        assert_eq!(snapshot.root, repo);
        assert_eq!(snapshot.repo_map.head_sha, expected_sha);
        assert_eq!(paths(&snapshot.repo_map), vec!["README.md", "src/lib.rs"]);
        assert_eq!(snapshot.repo_map.total_files, 2);
        assert!(!snapshot.repo_map.truncated);
        assert_eq!(
            snapshot.changed_files,
            vec![ChangedFile {
                path: "src/lib.rs".to_string(),
                change: ChangeKind::Modified,
            }]
        );
        fs::remove_dir_all(&base).expect("clean temp dir");
    }

    #[test]
    fn over_cap_capture_reports_truncation_with_exact_total() {
        let (base, repo) = git_fixture("cap", &["a.txt", "b.txt", "c.txt", "d.txt", "e.txt"]);
        let diff = base.join("pr.diff");
        fs::write(&diff, "").expect("write empty diff");

        let snapshot = Snapshot::capture_with_limit(&repo, &diff, 2).expect("capture with limit");
        assert_eq!(paths(&snapshot.repo_map), vec!["a.txt", "b.txt"]);
        assert_eq!(snapshot.repo_map.total_files, 5);
        assert!(snapshot.repo_map.truncated);

        let full = Snapshot::capture(&repo, &diff).expect("capture");
        assert_eq!(full.repo_map.total_files, 5);
        assert!(!full.repo_map.truncated);
        fs::remove_dir_all(&base).expect("clean temp dir");
    }

    // -- error paths --------------------------------------------------------

    #[test]
    fn non_git_directory_is_a_typed_error() {
        let root = temp_dir("nogit");
        let error = Snapshot::capture(&root, Path::new("unused.diff"))
            .expect_err("non-git capture must fail");
        assert!(
            matches!(error, SnapshotError::NotAGitCheckout { .. }),
            "{error:?}"
        );
        fs::remove_dir_all(&root).expect("clean temp dir");
    }

    #[test]
    fn missing_diff_file_is_a_typed_io_error() {
        let (base, repo) = git_fixture("nodiff", &["a.txt"]);
        let missing = base.join("nope.diff");
        let error = Snapshot::capture(&repo, &missing).expect_err("missing diff must fail");
        assert!(
            matches!(&error, SnapshotError::Io { path, .. } if path == &missing),
            "{error:?}"
        );
        fs::remove_dir_all(&base).expect("clean temp dir");
    }

    // -- serialization --------------------------------------------------------

    #[test]
    fn snapshot_types_round_trip_through_serde() {
        let snapshot = Snapshot {
            head_sha: "abc123".to_string(),
            root: PathBuf::from("/repo"),
            repo_map: RepoMap {
                head_sha: "abc123".to_string(),
                entries: vec![RepoEntry {
                    path: "src/main.rs".to_string(),
                    size: 13,
                }],
                total_files: 1,
                truncated: false,
            },
            changed_files: vec![ChangedFile {
                path: "new.rs".to_string(),
                change: ChangeKind::Renamed {
                    from: "old.rs".to_string(),
                },
            }],
        };
        let json = serde_json::to_string(&snapshot).expect("serialize snapshot");
        // The emitted artifact carries the SHA stamp.
        assert!(json.contains("\"head_sha\":\"abc123\""), "{json}");
        let back: Snapshot = serde_json::from_str(&json).expect("deserialize snapshot");
        assert_eq!(back, snapshot);
    }
}
