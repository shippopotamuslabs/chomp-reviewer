//! Read-only repository tools for lens sessions (FRG-18, slices 1-3).
//!
//! Slice scope: the host port trait ([`ToolExecutor`]), root confinement,
//! and the `read_file`, `list_dir`, and `grep` tools, served by
//! [`RepoToolExecutor`]. The executor is constructed from a checkout
//! root plus its head SHA and shares nothing mutable, so `&self`
//! execution composes with `Arc<dyn ToolExecutor + Send + Sync>`.
//!
//! The settled `read_file` payload shape is `{"path": <repo-relative
//! path as given>, "start_line": <requested start>, "end_line": <last
//! line actually returned, or start_line - 1 when none>, "total_lines":
//! <usize>, "content": "<n>: <line>" joined by "\n" with no trailing
//! newline>, "truncated": <bool>, "note": "Returned lines A-B of T.
//! Call again with start_line=<B+1>."}` — where `note` is present only
//! when `truncated` is true. `ToolResultRecord.truncated` mirrors the
//! payload's `truncated`.
//!
//! The settled `list_dir` payload shape is `{"path": <normalized
//! listed path ("." for the root)>, "entries": [{"path": <full
//! repo-relative path usable as a later tool argument>, "kind": "file"
//! | "dir" | "symlink" | "other" (from the entry's file type without
//! following symlinks), "size": <bytes, or null for directories>}]
//! sorted by entry name, at most 200>, "total_entries": <usize>,
//! "truncated": <bool>, "note": "Returned first 200 of T entries,
//! sorted by name."}` — where `note` is present only when `truncated`
//! is true. `ToolResultRecord.truncated` mirrors the payload's
//! `truncated`.
//!
//! The settled `grep` payload shape is `{"pattern": <as given>,
//! "path": <normalized scope path ("." for the root)>, "glob": <as
//! given or null>, "hits": [{"path": <repo-relative path>, "line":
//! <1-based line number>, "snippet": <first 200 chars of the line>}],
//! at most 50 in (path, line) order>, "total_hits": <usize>,
//! "truncated": <bool>, "note": "Returned first 50 of T hits. Narrow
//! pattern, glob, or path."}` — where `note` is present only when
//! `truncated` is true. `ToolResultRecord.truncated` mirrors the
//! payload's `truncated`.
//!
//! Explicitly NOT in this slice: `read_symbol` (dispatched to a "tool
//! not implemented yet" soft failure), the result cache, redaction,
//! and tool schemas / `ToolSpec`s — those land in later FRG-18 slices.
//! This executor must not be wired into sessions until redaction lands.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use agent_core::message::{ToolCallRecord, ToolResultRecord};
use globset::GlobBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::snapshot::{LOCKFILES, PRUNED_DIRS};

/// Maximum lines one `read_file` call returns.
const MAX_READ_LINES: usize = 200;

/// Maximum entries one `list_dir` call returns, after sorting by name.
const MAX_LIST_ENTRIES: usize = 200;

/// Maximum hits one `grep` call retains, in (path, line) order.
const MAX_GREP_HITS: usize = 50;

/// Maximum characters of one matching line a `grep` snippet carries.
const MAX_SNIPPET_CHARS: usize = 200;

/// The application port for repository tools — the seam between the
/// host's session driver (FRG-19) and the filesystem, following the
/// [`crate::model_transport::ModelTransport`] precedent. Implementations
/// never panic on untrusted input: every failure surfaces as a
/// soft-failure [`ToolResultRecord`].
pub trait ToolExecutor {
    fn execute(&self, call: &ToolCallRecord) -> ToolResultRecord;
}

/// The concrete filesystem executor: serves reads confined under
/// `root` (canonicalized once in [`RepoToolExecutor::new`]) and stamps
/// nothing itself — `head_sha` is carried so later slices (and the
/// driver) can attribute reads to the snapshot they describe.
pub struct RepoToolExecutor {
    root: PathBuf,
    head_sha: String,
}

impl RepoToolExecutor {
    /// Confine reads under `root`, canonicalized once here so every
    /// later [`ToolExecutor::execute`] compares canonical paths.
    pub fn new(root: PathBuf, head_sha: String) -> std::io::Result<Self> {
        let root = root.canonicalize()?;
        Ok(Self { root, head_sha })
    }

    /// The canonicalized checkout root reads are confined under.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The head SHA the root was captured at.
    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }

    /// Resolve a repo-relative `rel` to a canonical path under the
    /// root. Absolute paths and escapes (including via symlinks, which
    /// `canonicalize` follows) are refused; a target that cannot be
    /// resolved is missing.
    fn confine(&self, rel: &str) -> Result<PathBuf, ToolError> {
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
        let candidate = self.root.join(rel).canonicalize().map_err(|error| {
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
        if !candidate.starts_with(&self.root) {
            return Err(ToolError::PathEscapesRoot {
                path: rel.to_string(),
            });
        }
        Ok(candidate)
    }

    /// Serve the only implemented tool: a numbered `read_file` window.
    /// Lines are 1-based; `start_line` defaults to 1 and `end_line`
    /// defaults to `start_line + 199`, always clamped to at most
    /// [`MAX_READ_LINES`] lines. Reading past EOF (and empty files) is
    /// a success with empty content.
    fn read_file(&self, call: &ToolCallRecord) -> ToolResultRecord {
        match self.read_file_inner(call) {
            Ok(record) => record,
            Err(error) => soft_failure(call, error),
        }
    }

    fn read_file_inner(&self, call: &ToolCallRecord) -> Result<ToolResultRecord, ToolError> {
        let args: ReadFileArgs = serde_json::from_str(&call.arguments_json).map_err(|error| {
            ToolError::InvalidArguments(format!(
                "arguments are not a valid read_file request: {error}"
            ))
        })?;
        let start = args.start_line.unwrap_or(1);
        if start == 0 {
            return Err(ToolError::InvalidArguments(
                "start_line must be at least 1".to_string(),
            ));
        }
        let default_end = start.saturating_add(MAX_READ_LINES - 1);
        let mut end = match args.end_line {
            Some(end) => {
                if end == 0 || end < start {
                    return Err(ToolError::InvalidArguments(
                        "end_line must be at least start_line and at least 1".to_string(),
                    ));
                }
                end
            }
            None => default_end,
        };
        end = end.min(start.saturating_add(MAX_READ_LINES - 1));

        let candidate = self.confine(&args.path)?;
        let metadata = fs::metadata(&candidate).map_err(|error| ToolError::Io {
            path: args.path.clone(),
            message: error.to_string(),
        })?;
        if !metadata.is_file() {
            return Err(ToolError::NotAFile {
                path: args.path.clone(),
            });
        }
        let bytes = fs::read(&candidate).map_err(|error| ToolError::Io {
            path: args.path.clone(),
            message: error.to_string(),
        })?;
        if bytes.contains(&0) {
            return Err(ToolError::BinaryFile {
                path: args.path.clone(),
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| ToolError::BinaryFile {
            path: args.path.clone(),
        })?;
        let lines: Vec<&str> = text.lines().collect();
        let total_lines = lines.len();
        // `last` is the 1-based number of the final returned line;
        // `start - 1` when nothing is returned (start past EOF, or an
        // empty file). `truncated` is true whenever the tree holds more
        // lines than that — including a small requested subwindow.
        let last = end.min(total_lines);
        let (content, end_line) = if start > total_lines {
            (String::new(), start - 1)
        } else {
            let numbered: Vec<String> = lines
                .iter()
                .skip(start - 1)
                .take(last - start + 1)
                .enumerate()
                .map(|(index, line)| format!("{}: {line}", start + index))
                .collect();
            (numbered.join("\n"), last)
        };
        let truncated = total_lines > end_line;
        let note = truncated.then(|| {
            format!(
                "Returned lines {start}-{end_line} of {total_lines}. \
                 Call again with start_line={}.",
                end_line + 1
            )
        });
        let payload = ReadFilePayload {
            path: args.path.clone(),
            start_line: start,
            end_line,
            total_lines,
            content,
            truncated,
            note,
        };
        let result_json = serde_json::to_string(&payload)
            .map_err(|error| ToolError::Render(error.to_string()))?;
        Ok(ToolResultRecord {
            call_id: call.call_id.clone(),
            result_json,
            truncated: payload.truncated,
            soft_failure: false,
            activated_tool_names: Vec::new(),
            result_metadata: None,
        })
    }

    /// Serve the name-sorted `list_dir` window: at most
    /// [`MAX_LIST_ENTRIES`] entries, with `total_entries` counting the
    /// whole directory. `"."` addresses the snapshot root; a trailing
    /// `/` or leading `./` segments on the input are normalized away
    /// in the payload, and every entry carries its full repo-relative
    /// path so it can feed a later tool call directly. Entry kinds
    /// come from the entry's own file type without following symlinks.
    fn list_dir(&self, call: &ToolCallRecord) -> ToolResultRecord {
        match self.list_dir_inner(call) {
            Ok(record) => record,
            Err(error) => soft_failure(call, error),
        }
    }

    fn list_dir_inner(&self, call: &ToolCallRecord) -> Result<ToolResultRecord, ToolError> {
        let args: ListDirArgs = serde_json::from_str(&call.arguments_json).map_err(|error| {
            ToolError::InvalidArguments(format!(
                "arguments are not a valid list_dir request: {error}"
            ))
        })?;

        let candidate = self.confine(&args.path)?;
        let metadata = fs::metadata(&candidate).map_err(|error| ToolError::Io {
            path: args.path.clone(),
            message: error.to_string(),
        })?;
        if !metadata.is_dir() {
            return Err(ToolError::NotADirectory {
                path: args.path.clone(),
            });
        }
        let read_dir = fs::read_dir(&candidate).map_err(|error| ToolError::Io {
            path: args.path.clone(),
            message: error.to_string(),
        })?;
        let mut names: Vec<(String, String, Option<u64>)> = Vec::new();
        for entry in read_dir {
            let entry = entry.map_err(|error| ToolError::Io {
                path: args.path.clone(),
                message: error.to_string(),
            })?;
            let file_type = entry.file_type().map_err(|error| ToolError::Io {
                path: args.path.clone(),
                message: error.to_string(),
            })?;
            // Symlinks report as symlinks even when they point at a
            // file or a directory: the type is never followed here.
            let kind = if file_type.is_symlink() {
                "symlink"
            } else if file_type.is_dir() {
                "dir"
            } else if file_type.is_file() {
                "file"
            } else {
                "other"
            };
            // `DirEntry::metadata` does not traverse symlinks, so a
            // link reports its own length. Directories carry no size.
            // One entry's unreadable metadata must not fail the whole
            // listing, so it degrades to `None`.
            let size = if kind == "dir" {
                None
            } else {
                entry.metadata().ok().map(|metadata| metadata.len())
            };
            names.push((
                entry.file_name().to_string_lossy().into_owned(),
                kind.to_string(),
                size,
            ));
        }
        names.sort_by(|left, right| left.0.cmp(&right.0));
        let total_entries = names.len();
        let truncated = total_entries > MAX_LIST_ENTRIES;
        let listed = normalize_list_path(&args.path);
        let entries: Vec<ListDirEntry> = names
            .into_iter()
            .take(MAX_LIST_ENTRIES)
            .map(|(name, kind, size)| ListDirEntry {
                path: if listed == "." {
                    name
                } else {
                    format!("{listed}/{name}")
                },
                kind,
                size,
            })
            .collect();
        let note = truncated.then(|| {
            format!("Returned first {MAX_LIST_ENTRIES} of {total_entries} entries, sorted by name.")
        });
        let payload = ListDirPayload {
            path: listed,
            entries,
            total_entries,
            truncated,
            note,
        };
        let result_json = serde_json::to_string(&payload)
            .map_err(|error| ToolError::Render(error.to_string()))?;
        Ok(ToolResultRecord {
            call_id: call.call_id.clone(),
            result_json,
            truncated: payload.truncated,
            soft_failure: false,
            activated_tool_names: Vec::new(),
            result_metadata: None,
        })
    }

    /// Serve the `grep` regex search: one hit per matching line, at
    /// most [`MAX_GREP_HITS`] retained in (path, line) order, with the
    /// true count visible through `total_hits` and `truncated`.
    fn grep(&self, call: &ToolCallRecord) -> ToolResultRecord {
        match self.grep_inner(call) {
            Ok(record) => record,
            Err(error) => soft_failure(call, error),
        }
    }

    fn grep_inner(&self, call: &ToolCallRecord) -> Result<ToolResultRecord, ToolError> {
        let args: GrepArgs = serde_json::from_str(&call.arguments_json).map_err(|error| {
            ToolError::InvalidArguments(format!("arguments are not a valid grep request: {error}"))
        })?;
        if args.pattern.is_empty() {
            return Err(ToolError::InvalidArguments(
                "pattern must not be empty".to_string(),
            ));
        }
        let regex = Regex::new(&args.pattern)
            .map_err(|error| ToolError::InvalidRegex(error.to_string()))?;
        // A glob without `/` matches the file name only (so `*.rs`
        // works at any depth); a glob with `/` matches the full
        // repo-relative path. `literal_separator` keeps `*` from
        // crossing `/` in the path form.
        let match_path = args.glob.as_deref().is_some_and(|glob| glob.contains('/'));
        let matcher = args
            .glob
            .as_deref()
            .map(|glob| {
                GlobBuilder::new(glob)
                    .literal_separator(true)
                    .build()
                    .map(|pattern| pattern.compile_matcher())
                    .map_err(|error| ToolError::InvalidGlob(error.to_string()))
            })
            .transpose()?;
        let scope = args.path.as_deref().unwrap_or(".");
        let candidate = self.confine(scope)?;
        let listed = normalize_list_path(scope);

        // An explicit file scope searches only that file: pruning and
        // lockfile exclusion do not apply, but the glob filter still
        // does. A directory scope is walked recursively.
        let rels: Vec<String> = if fs::metadata(&candidate)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            vec![listed.clone()]
                .into_iter()
                .filter(|rel| {
                    matcher.as_ref().is_none_or(|matcher| {
                        matcher.is_match(if match_path {
                            rel.as_str()
                        } else {
                            file_name_of(rel)
                        })
                    })
                })
                .collect()
        } else {
            self.walk_grep_scope(&candidate, &listed)?
                .into_iter()
                .filter(|rel| {
                    matcher.as_ref().is_none_or(|matcher| {
                        matcher.is_match(if match_path {
                            rel.as_str()
                        } else {
                            file_name_of(rel)
                        })
                    })
                })
                .collect()
        };

        let mut hits: Vec<GrepHit> = Vec::new();
        let mut total_hits: usize = 0;
        for rel in &rels {
            let bytes = fs::read(self.root.join(rel)).unwrap_or_default();
            // Binary and unreadable files are silently skipped: grep
            // never fails a whole call (and never panics) over one
            // file's contents. An unreadable file reads as empty here
            // (contributing no lines); a missing file the same.
            if bytes.contains(&0) {
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if regex.is_match(line) {
                    total_hits += 1;
                    if hits.len() < MAX_GREP_HITS {
                        hits.push(GrepHit {
                            path: rel.clone(),
                            line: index + 1,
                            snippet: line.chars().take(MAX_SNIPPET_CHARS).collect(),
                        });
                    }
                }
            }
        }
        let truncated = total_hits > MAX_GREP_HITS;
        let note = truncated.then(|| {
            format!(
                "Returned first {MAX_GREP_HITS} of {total_hits} hits. \
                 Narrow pattern, glob, or path."
            )
        });
        let payload = GrepPayload {
            pattern: args.pattern.clone(),
            path: listed,
            glob: args.glob.clone(),
            hits,
            total_hits,
            truncated,
            note,
        };
        let result_json = serde_json::to_string(&payload)
            .map_err(|error| ToolError::Render(error.to_string()))?;
        Ok(ToolResultRecord {
            call_id: call.call_id.clone(),
            result_json,
            truncated: payload.truncated,
            soft_failure: false,
            activated_tool_names: Vec::new(),
            result_metadata: None,
        })
    }

    /// Walk a `grep` directory scope and return the candidate
    /// repo-relative file paths in ascending order, mirroring the
    /// snapshot walk's discipline: directories named in
    /// [`PRUNED_DIRS`] are never descended (at any depth below the
    /// scope — the scope directory itself is always searched), files
    /// named in [`LOCKFILES`] are never candidates, symlinked
    /// directories are not descended, and a symlinked file is a
    /// candidate only when its canonical target stays inside the
    /// executor root (escaping, dangling, and otherwise unreadable
    /// links are skipped silently). Hidden files and directories are
    /// included: only the named lists prune. The scope itself must be
    /// a directory; `scope_rel` is its normalized repo-relative path
    /// (`"."` for the root).
    fn walk_grep_scope(&self, scope: &Path, scope_rel: &str) -> Result<Vec<String>, ToolError> {
        let base = if scope_rel == "." {
            String::new()
        } else {
            scope_rel.to_string()
        };
        let mut rels: Vec<String> = Vec::new();
        let mut stack: Vec<(PathBuf, String)> = vec![(scope.to_path_buf(), base)];
        while let Some((dir, rel)) = stack.pop() {
            let read_dir = fs::read_dir(&dir).map_err(|error| ToolError::Io {
                path: dir.to_string_lossy().into_owned(),
                message: error.to_string(),
            })?;
            for entry in read_dir {
                // One unreadable entry must not fail the whole call.
                let Ok(entry) = entry else { continue };
                let Some(name) = entry.file_name().to_str().map(str::to_string) else {
                    continue;
                };
                let rel_path = if rel.is_empty() {
                    name.clone()
                } else {
                    format!("{rel}/{name}")
                };
                let Ok(file_type) = entry.file_type() else {
                    continue;
                };
                let path = entry.path();
                if file_type.is_symlink() {
                    // Followed once, here, for classification only: a
                    // symlinked directory is never descended, and a
                    // symlinked file is content only when its target
                    // stays inside the root — grep must never read
                    // outside it.
                    let Ok(metadata) = fs::metadata(&path) else {
                        continue;
                    };
                    if metadata.is_dir() || LOCKFILES.contains(&name.as_str()) {
                        continue;
                    }
                    if path
                        .canonicalize()
                        .map(|target| target.starts_with(&self.root))
                        .unwrap_or(false)
                    {
                        rels.push(rel_path);
                    }
                } else if file_type.is_dir() {
                    if PRUNED_DIRS.contains(&name.as_str()) {
                        continue;
                    }
                    stack.push((path, rel_path));
                } else if file_type.is_file() {
                    if LOCKFILES.contains(&name.as_str()) {
                        continue;
                    }
                    rels.push(rel_path);
                }
                // Anything else (fifo, socket, device) is not a file
                // and never a candidate.
            }
        }
        rels.sort();
        Ok(rels)
    }
}

impl ToolExecutor for RepoToolExecutor {
    fn execute(&self, call: &ToolCallRecord) -> ToolResultRecord {
        match call.name.as_str() {
            "read_file" => self.read_file(call),
            "list_dir" => self.list_dir(call),
            "grep" => self.grep(call),
            "read_symbol" => soft_failure(
                call,
                ToolError::NotImplemented {
                    name: call.name.clone(),
                },
            ),
            _ => soft_failure(
                call,
                ToolError::UnknownTool {
                    name: call.name.clone(),
                },
            ),
        }
    }
}

/// Typed tool failures. Every failure path lands in one of these
/// variants; the trait boundary converts each into a soft-failure
/// [`ToolResultRecord`] whose `result_json` is `{"error": "<display>"}`.
#[derive(Debug)]
enum ToolError {
    /// The call's `arguments_json` could not be parsed into the tool's
    /// request shape, or a field value is out of range (empty path,
    /// `start_line` of 0, `end_line` below `start_line`, ...).
    InvalidArguments(String),
    /// The confined path resolved to nothing (including an
    /// unresolvable `canonicalize`).
    NotFound { path: String },
    /// An absolute path, or a relative path whose canonical target
    /// escapes the root (including via a symlink).
    PathEscapesRoot { path: String },
    /// The confined path exists but is not a regular file.
    NotAFile { path: String },
    /// The confined path exists but is not a directory.
    NotADirectory { path: String },
    /// The file holds NUL bytes or is not valid UTF-8.
    BinaryFile { path: String },
    /// A filesystem failure that is not a missing file (permission
    /// denied, an unreadable symlink loop, ...). The source
    /// [`std::io::Error`]'s message is preserved in `message`.
    Io { path: String, message: String },
    /// The `grep` pattern is not a valid regular expression. The
    /// regex compiler's message is preserved.
    InvalidRegex(String),
    /// The `grep` glob is not a valid glob pattern. The glob
    /// compiler's message is preserved.
    InvalidGlob(String),
    /// The success payload could not be serialized for the wire.
    Render(String),
    /// A settled later-slice tool name that this slice does not serve.
    NotImplemented { name: String },
    /// A tool name outside the settled FRG-18 vocabulary.
    UnknownTool { name: String },
}

impl fmt::Display for ToolError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ToolError::InvalidArguments(detail) => {
                write!(f, "invalid arguments: {detail}")
            }
            ToolError::NotFound { path } => {
                write!(f, "file not found: {path}")
            }
            ToolError::PathEscapesRoot { path } => {
                write!(f, "path escapes the repository root: {path}")
            }
            ToolError::NotAFile { path } => {
                write!(f, "not a regular file: {path}")
            }
            ToolError::NotADirectory { path } => {
                write!(f, "not a directory: {path}")
            }
            ToolError::BinaryFile { path } => {
                write!(f, "refusing to read binary file: {path}")
            }
            ToolError::Io { path, message } => {
                write!(f, "cannot read file {path}: {message}")
            }
            ToolError::InvalidRegex(detail) => {
                write!(f, "invalid regex: {detail}")
            }
            ToolError::InvalidGlob(detail) => {
                write!(f, "invalid glob: {detail}")
            }
            ToolError::Render(detail) => {
                write!(f, "cannot render tool result: {detail}")
            }
            ToolError::NotImplemented { name } => {
                write!(f, "tool not implemented yet: {name}")
            }
            ToolError::UnknownTool { name } => {
                write!(f, "unknown tool: {name}")
            }
        }
    }
}

impl std::error::Error for ToolError {}

/// The `read_file` request shape: `arguments_json` deserializes into
/// this, so a missing `path` (or malformed JSON) is a parse failure.
#[derive(Debug, Deserialize)]
struct ReadFileArgs {
    path: String,
    start_line: Option<usize>,
    end_line: Option<usize>,
}

/// The `list_dir` request shape: `arguments_json` deserializes into
/// this, so a missing `path` (or malformed JSON) is a parse failure.
#[derive(Debug, Deserialize)]
struct ListDirArgs {
    path: String,
}

/// One `list_dir` entry: the full repo-relative path plus the entry's
/// own kind (`size` is null for directories).
#[derive(Debug, Serialize)]
struct ListDirEntry {
    path: String,
    kind: String,
    size: Option<u64>,
}

/// The settled `list_dir` success payload. `note` serializes only
/// when truncated.
#[derive(Debug, Serialize)]
struct ListDirPayload {
    path: String,
    entries: Vec<ListDirEntry>,
    total_entries: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// The settled `read_file` success payload. `note` serializes only
/// when truncated.
#[derive(Debug, Serialize)]
struct ReadFilePayload {
    path: String,
    start_line: usize,
    end_line: usize,
    total_lines: usize,
    content: String,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// The `grep` request shape: `arguments_json` deserializes into
/// this, so a missing `pattern` (or malformed JSON) is a parse
/// failure. `path` defaults to the root when absent; `glob` defaults
/// to matching every walked file.
#[derive(Debug, Deserialize)]
struct GrepArgs {
    pattern: String,
    glob: Option<String>,
    path: Option<String>,
}

/// One `grep` hit: the repo-relative path, the 1-based line number,
/// and the first [`MAX_SNIPPET_CHARS`] characters of the line.
#[derive(Debug, Serialize)]
struct GrepHit {
    path: String,
    line: usize,
    snippet: String,
}

/// The settled `grep` success payload. `glob` serializes as null when
/// no glob was given; `note` serializes only when truncated.
#[derive(Debug, Serialize)]
struct GrepPayload {
    pattern: String,
    path: String,
    glob: Option<String>,
    hits: Vec<GrepHit>,
    total_hits: usize,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// The error payload shape shared by every soft failure.
#[derive(Debug, Serialize)]
struct ErrorPayload {
    error: String,
}

/// Normalize a `list_dir` input for the payload: strip a trailing
/// `/` and any leading `./` segments, so `sub/`, `./sub`, and `sub`
/// all report `sub` while `.` (the snapshot root) stays `.`.
fn normalize_list_path(input: &str) -> String {
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

/// The file name of a repo-relative `path` built with `/`
/// separators: everything after the final `/`, or the whole path.
fn file_name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

fn soft_failure(call: &ToolCallRecord, error: ToolError) -> ToolResultRecord {
    let result_json = serde_json::to_string(&ErrorPayload {
        error: error.to_string(),
    })
    .unwrap_or_else(|_| r#"{"error":"cannot render tool error"}"#.to_string());
    ToolResultRecord {
        call_id: call.call_id.clone(),
        result_json,
        truncated: false,
        soft_failure: true,
        activated_tool_names: Vec::new(),
        result_metadata: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A temp checkout root cleaned up on drop, in the
    /// `snapshot.rs` test-helper style.
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(label: &str) -> Self {
            let nonce = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .expect("system clock after epoch")
                .as_nanos();
            let dir = std::env::temp_dir().join(format!(
                "chomp-tools-{label}-{}-{nonce}",
                std::process::id()
            ));
            fs::create_dir_all(&dir).expect("create temp dir");
            Self { dir }
        }

        fn write(&self, rel: &str, contents: &str) {
            let path = self.dir.join(rel);
            fs::create_dir_all(path.parent().expect("fixture path has a parent"))
                .expect("create fixture dirs");
            fs::write(&path, contents).expect("write fixture file");
        }

        fn write_bytes(&self, rel: &str, contents: &[u8]) {
            let path = self.dir.join(rel);
            fs::create_dir_all(path.parent().expect("fixture path has a parent"))
                .expect("create fixture dirs");
            fs::write(&path, contents).expect("write fixture file");
        }

        fn executor(&self) -> RepoToolExecutor {
            RepoToolExecutor::new(self.dir.clone(), "abc123".to_string())
                .expect("temp dir canonicalizes")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    fn call(name: &str, arguments_json: &str) -> ToolCallRecord {
        ToolCallRecord {
            call_id: "call-1".to_string(),
            name: name.to_string(),
            arguments_json: arguments_json.to_string(),
            attempt_metadata: None,
            repair_metadata: None,
        }
    }

    fn read_args(path: &str) -> String {
        serde_json::json!({"path": path}).to_string()
    }

    fn list_args(path: &str) -> String {
        serde_json::json!({"path": path}).to_string()
    }

    fn payload(result: &ToolResultRecord) -> serde_json::Value {
        serde_json::from_str(&result.result_json).expect("result is JSON")
    }

    // -- basic read ---------------------------------------------------------

    #[test]
    fn basic_read_returns_numbered_defaults() {
        let fixture = Fixture::new("basic");
        fixture.write("src/main.rs", "fn a() {}\nfn b() {}\nfn c() {}\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("src/main.rs")));

        assert!(!result.soft_failure);
        assert!(!result.truncated);
        assert_eq!(result.call_id, "call-1");
        assert!(result.activated_tool_names.is_empty());
        assert!(result.result_metadata.is_none());
        let body = payload(&result);
        assert_eq!(body["path"], "src/main.rs");
        assert_eq!(body["start_line"], 1);
        assert_eq!(body["end_line"], 3);
        assert_eq!(body["total_lines"], 3);
        assert_eq!(body["content"], "1: fn a() {}\n2: fn b() {}\n3: fn c() {}");
        assert_eq!(body["truncated"], false);
        assert!(body.get("note").is_none(), "no note when complete");
    }

    // -- truncation ----------------------------------------------------------

    #[test]
    fn explicit_subwindow_reports_truncation_and_next_start() {
        let fixture = Fixture::new("subwindow");
        let contents: Vec<String> = (1..=10).map(|n| format!("line {n}")).collect();
        fixture.write("log.txt", &contents.join("\n"));
        let executor = fixture.executor();

        let args =
            serde_json::json!({"path": "log.txt", "start_line": 3, "end_line": 5}).to_string();
        let result = executor.execute(&call("read_file", &args));

        assert!(!result.soft_failure);
        assert!(result.truncated, "record mirrors payload truncation");
        let body = payload(&result);
        assert_eq!(body["start_line"], 3);
        assert_eq!(body["end_line"], 5);
        assert_eq!(body["total_lines"], 10);
        assert_eq!(body["content"], "3: line 3\n4: line 4\n5: line 5");
        assert_eq!(body["truncated"], true);
        assert_eq!(
            body["note"],
            "Returned lines 3-5 of 10. Call again with start_line=6."
        );
    }

    #[test]
    fn read_clamps_to_two_hundred_lines() {
        let fixture = Fixture::new("clamp");
        let contents: Vec<String> = (1..=250).map(|n| format!("line {n}")).collect();
        fixture.write("big.txt", &contents.join("\n"));
        let executor = fixture.executor();

        // A request beyond the cap still returns exactly 200 lines.
        let args =
            serde_json::json!({"path": "big.txt", "start_line": 1, "end_line": 250}).to_string();
        let result = executor.execute(&call("read_file", &args));

        assert!(!result.soft_failure);
        assert!(result.truncated);
        let body = payload(&result);
        assert_eq!(body["start_line"], 1);
        assert_eq!(body["end_line"], 200);
        assert_eq!(body["total_lines"], 250);
        let content = body["content"].as_str().expect("content is a string");
        let rendered: Vec<&str> = content.lines().collect();
        assert_eq!(rendered.len(), 200);
        assert_eq!(rendered[0], "1: line 1");
        assert_eq!(rendered[199], "200: line 200");
        assert_eq!(
            body["note"],
            "Returned lines 1-200 of 250. Call again with start_line=201."
        );
    }

    // -- empty and past-EOF reads ---------------------------------------------

    #[test]
    fn empty_file_is_a_success_with_end_line_zero() {
        let fixture = Fixture::new("empty");
        fixture.write("empty.txt", "");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("empty.txt")));

        assert!(!result.soft_failure);
        assert!(!result.truncated);
        let body = payload(&result);
        assert_eq!(body["start_line"], 1);
        assert_eq!(body["end_line"], 0);
        assert_eq!(body["total_lines"], 0);
        assert_eq!(body["content"], "");
        assert_eq!(body["truncated"], false);
    }

    #[test]
    fn start_past_eof_is_a_success_with_empty_content() {
        let fixture = Fixture::new("pasteof");
        fixture.write("two.txt", "one\ntwo\n");
        let executor = fixture.executor();

        let args = serde_json::json!({"path": "two.txt", "start_line": 9}).to_string();
        let result = executor.execute(&call("read_file", &args));

        assert!(!result.soft_failure);
        assert!(!result.truncated);
        let body = payload(&result);
        assert_eq!(body["start_line"], 9);
        assert_eq!(body["end_line"], 8);
        assert_eq!(body["total_lines"], 2);
        assert_eq!(body["content"], "");
        assert_eq!(body["truncated"], false);
    }

    // -- invalid arguments -----------------------------------------------------

    #[test]
    fn malformed_json_is_a_soft_failure() {
        let fixture = Fixture::new("badjson");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", "not json{{"));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(!result.truncated);
        let body = payload(&result);
        assert!(
            body["error"]
                .as_str()
                .expect("error is a string")
                .contains("invalid arguments"),
            "{body}"
        );
    }

    #[test]
    fn missing_path_is_a_soft_failure() {
        let fixture = Fixture::new("nopath");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", r#"{"start_line": 1}"#));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    #[test]
    fn empty_path_is_a_soft_failure() {
        let fixture = Fixture::new("emptypath");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    #[test]
    fn zero_start_line_is_a_soft_failure() {
        let fixture = Fixture::new("zerostart");
        fixture.write("a.txt", "one\n");
        let executor = fixture.executor();

        let args = serde_json::json!({"path": "a.txt", "start_line": 0}).to_string();
        let result = executor.execute(&call("read_file", &args));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    #[test]
    fn end_before_start_is_a_soft_failure() {
        let fixture = Fixture::new("endbefore");
        fixture.write("a.txt", "one\ntwo\nthree\n");
        let executor = fixture.executor();

        let args = serde_json::json!({"path": "a.txt", "start_line": 3, "end_line": 2}).to_string();
        let result = executor.execute(&call("read_file", &args));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    // -- missing and non-file targets --------------------------------------------

    #[test]
    fn missing_file_is_a_soft_failure() {
        let fixture = Fixture::new("missing");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("nope.txt")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        let body = payload(&result);
        assert!(
            body["error"]
                .as_str()
                .expect("error is a string")
                .contains("nope.txt"),
            "{body}"
        );
    }

    #[test]
    fn directory_is_a_soft_failure() {
        let fixture = Fixture::new("dirtarget");
        fixture.write("sub/file.txt", "data\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("sub")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    // -- binary refusal ----------------------------------------------------------

    #[test]
    fn nul_bytes_are_refused() {
        let fixture = Fixture::new("nul");
        fixture.write_bytes("blob.bin", b"hello\x00world\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("blob.bin")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    #[test]
    fn invalid_utf8_is_refused() {
        let fixture = Fixture::new("badutf8");
        fixture.write_bytes("bad.txt", b"valid prefix \xff\xfe not utf8\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("bad.txt")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    // -- confinement ---------------------------------------------------------------

    #[test]
    fn absolute_path_is_refused() {
        let fixture = Fixture::new("absolute");
        fixture.write("inside.txt", "data\n");
        let executor = fixture.executor();
        let absolute = fixture.dir.join("inside.txt");
        let absolute = absolute.to_str().expect("temp path is UTF-8");

        let args = read_args(absolute);
        let result = executor.execute(&call("read_file", &args));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        let body = payload(&result);
        assert!(
            body["error"]
                .as_str()
                .expect("error is a string")
                .contains("escapes"),
            "{body}"
        );
    }

    #[test]
    fn dotdot_escape_is_refused() {
        let fixture = Fixture::new("escape");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("../outside.txt")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escaping_the_root_is_refused() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("symescape");
        let outside = Fixture::new("symescape-outside");
        outside.write("secret.txt", "secret\n");
        symlink(outside.dir.join("secret.txt"), fixture.dir.join("link.txt"))
            .expect("create escaping symlink");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("link.txt")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    #[cfg(unix)]
    #[test]
    fn symlink_inside_the_root_is_readable() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("syminside");
        fixture.write("real/data.txt", "one\ntwo\n");
        symlink(
            fixture.dir.join("real/data.txt"),
            fixture.dir.join("link.txt"),
        )
        .expect("create inner symlink");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("link.txt")));

        assert!(!result.soft_failure, "inner link reads: {result:?}");
        let body = payload(&result);
        assert_eq!(body["total_lines"], 2);
        assert_eq!(body["content"], "1: one\n2: two");
    }

    // -- list_dir -----------------------------------------------------------------------

    #[test]
    fn root_listing_reports_bare_names_sorted_by_name() {
        let fixture = Fixture::new("listroot");
        fixture.write("b.txt", "bee\n");
        fixture.write("a.txt", "aye\n");
        fixture.write("sub/inner.txt", "in\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args(".")));

        assert!(!result.soft_failure);
        assert!(!result.truncated);
        assert_eq!(result.call_id, "call-1");
        let body = payload(&result);
        assert_eq!(body["path"], ".");
        assert_eq!(body["total_entries"], 3);
        assert_eq!(body["truncated"], false);
        assert!(body.get("note").is_none(), "no note when complete");
        let entries = body["entries"].as_array().expect("entries is an array");
        let paths: Vec<&str> = entries
            .iter()
            .map(|entry| entry["path"].as_str().expect("entry path is a string"))
            .collect();
        assert_eq!(paths, vec!["a.txt", "b.txt", "sub"]);
        assert_eq!(entries[0]["kind"], "file");
        assert!(entries[0]["size"].as_u64().is_some(), "files carry a size");
        assert_eq!(entries[2]["kind"], "dir");
        assert!(entries[2].get("size").is_some(), "size key is present");
        assert_eq!(entries[2]["size"], serde_json::Value::Null);
    }

    #[test]
    fn subdirectory_listing_uses_full_relative_paths() {
        let fixture = Fixture::new("listsub");
        fixture.write("sub/b.txt", "bee\n");
        fixture.write("sub/a.txt", "aye\n");
        let executor = fixture.executor();

        for input in ["sub", "sub/", "./sub"] {
            let result = executor.execute(&call("list_dir", &list_args(input)));

            assert!(!result.soft_failure, "{input} must succeed");
            let body = payload(&result);
            assert_eq!(body["path"], "sub", "{input} normalizes");
            assert_eq!(body["total_entries"], 2);
            assert_eq!(body["truncated"], false);
            let paths: Vec<&str> = body["entries"]
                .as_array()
                .expect("entries is an array")
                .iter()
                .map(|entry| entry["path"].as_str().expect("entry path is a string"))
                .collect();
            assert_eq!(paths, vec!["sub/a.txt", "sub/b.txt"], "{input}");
        }
    }

    #[test]
    fn empty_directory_succeeds_with_no_entries() {
        let fixture = Fixture::new("listempty");
        fs::create_dir_all(fixture.dir.join("empty")).expect("create empty dir");
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args("empty")));

        assert!(!result.soft_failure);
        assert!(!result.truncated);
        let body = payload(&result);
        assert_eq!(body["path"], "empty");
        assert_eq!(body["entries"], serde_json::json!([]));
        assert_eq!(body["total_entries"], 0);
        assert_eq!(body["truncated"], false);
    }

    #[test]
    fn listing_caps_at_two_hundred_sorted_entries() {
        let fixture = Fixture::new("listcap");
        for index in 0..201 {
            fixture.write(&format!("many/f-{index:03}.txt"), "x\n");
        }
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args("many")));

        assert!(!result.soft_failure);
        assert!(result.truncated, "record mirrors payload truncation");
        let body = payload(&result);
        assert_eq!(body["path"], "many");
        assert_eq!(body["total_entries"], 201);
        assert_eq!(body["truncated"], true);
        assert_eq!(
            body["note"],
            "Returned first 200 of 201 entries, sorted by name."
        );
        let entries = body["entries"].as_array().expect("entries is an array");
        assert_eq!(entries.len(), 200);
        assert_eq!(entries[0]["path"], "many/f-000.txt");
        assert_eq!(entries[199]["path"], "many/f-199.txt");
    }

    #[test]
    fn file_target_is_a_soft_failure() {
        let fixture = Fixture::new("listfile");
        fixture.write("plain.txt", "data\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args("plain.txt")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(!result.truncated);
        let body = payload(&result);
        assert!(
            body["error"]
                .as_str()
                .expect("error is a string")
                .contains("not a directory"),
            "{body}"
        );
    }

    #[test]
    fn missing_directory_is_a_soft_failure() {
        let fixture = Fixture::new("listmissing");
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args("nope")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    #[test]
    fn malformed_list_dir_json_is_a_soft_failure() {
        let fixture = Fixture::new("listbadjson");
        let executor = fixture.executor();

        for arguments in ["not json{{", r#"{"other": "x"}"#, r#"{"path": ""}"#] {
            let result = executor.execute(&call("list_dir", arguments));

            assert!(result.soft_failure, "{arguments} must soft-fail");
            assert_eq!(result.call_id, "call-1");
            let body = payload(&result);
            assert!(
                body["error"]
                    .as_str()
                    .expect("error is a string")
                    .contains("invalid arguments"),
                "{body}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_entries_report_symlink_without_following() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("listsym");
        fixture.write("real/file.txt", "data\n");
        fixture.write("realdir/inner.txt", "in\n");
        symlink(
            fixture.dir.join("real/file.txt"),
            fixture.dir.join("link.txt"),
        )
        .expect("create file symlink");
        symlink(fixture.dir.join("realdir"), fixture.dir.join("linkdir"))
            .expect("create dir symlink");
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args(".")));

        assert!(!result.soft_failure, "root listing reads: {result:?}");
        let body = payload(&result);
        let by_name = |name: &str| {
            body["entries"]
                .as_array()
                .expect("entries is an array")
                .iter()
                .find(|entry| entry["path"] == name)
                .unwrap_or_else(|| panic!("{name} is listed: {body}"))
                .clone()
        };
        assert_eq!(by_name("link.txt")["kind"], "symlink");
        assert_eq!(by_name("linkdir")["kind"], "symlink");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_inside_the_root_lists_through_the_link_name() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("listsymdir");
        fixture.write("realdir/inner.txt", "in\n");
        symlink(fixture.dir.join("realdir"), fixture.dir.join("linkdir"))
            .expect("create dir symlink");
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args("linkdir")));

        assert!(!result.soft_failure, "inner link lists: {result:?}");
        let body = payload(&result);
        assert_eq!(body["path"], "linkdir");
        assert_eq!(body["total_entries"], 1);
        let entries = body["entries"].as_array().expect("entries is an array");
        assert_eq!(entries[0]["path"], "linkdir/inner.txt");
        assert_eq!(entries[0]["kind"], "file");
    }

    #[cfg(unix)]
    #[test]
    fn symlinked_directory_escaping_the_root_is_refused() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("listsymescape");
        let outside = Fixture::new("listsymescape-outside");
        outside.write("secret.txt", "secret\n");
        symlink(outside.dir.clone(), fixture.dir.join("linkdir")).expect("create escaping symlink");
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args("linkdir")));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(payload(&result).get("error").is_some());
    }

    // -- grep ---------------------------------------------------------------------

    fn grep_args(pattern: &str, glob: Option<&str>, path: Option<&str>) -> String {
        let mut value = serde_json::json!({"pattern": pattern});
        if let Some(glob) = glob {
            value["glob"] = serde_json::json!(glob);
        }
        if let Some(path) = path {
            value["path"] = serde_json::json!(path);
        }
        value.to_string()
    }

    #[test]
    fn basic_grep_returns_path_then_line_ordered_hits() {
        let fixture = Fixture::new("grepbase");
        fixture.write(
            "a.txt",
            "first needle here\nno match\nneedle twice needle\n",
        );
        fixture.write("sub/b.txt", "needle at top\nnothing\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("grep", &grep_args("needle", None, None)));

        assert!(!result.soft_failure);
        assert!(!result.truncated);
        assert_eq!(result.call_id, "call-1");
        let body = payload(&result);
        assert_eq!(body["pattern"], "needle");
        assert_eq!(body["path"], ".");
        assert_eq!(body["glob"], serde_json::Value::Null);
        assert_eq!(body["total_hits"], 3);
        assert_eq!(body["truncated"], false);
        assert!(body.get("note").is_none(), "no note when complete");
        let hits = body["hits"].as_array().expect("hits is an array");
        assert_eq!(hits.len(), 3);
        // One hit per matching line even when a line matches twice.
        assert_eq!(hits[0]["path"], "a.txt");
        assert_eq!(hits[0]["line"], 1);
        assert_eq!(hits[0]["snippet"], "first needle here");
        assert_eq!(hits[1]["path"], "a.txt");
        assert_eq!(hits[1]["line"], 3);
        assert_eq!(hits[2]["path"], "sub/b.txt");
        assert_eq!(hits[2]["line"], 1);
    }

    #[test]
    fn grep_caps_at_fifty_hits_with_the_true_total() {
        let fixture = Fixture::new("grepcape");
        let a_lines: Vec<String> = (1..=40).map(|n| format!("hit {n}")).collect();
        let b_lines: Vec<String> = (1..=30).map(|n| format!("hit {n}")).collect();
        fixture.write("a.txt", &a_lines.join("\n"));
        fixture.write("b.txt", &b_lines.join("\n"));
        let executor = fixture.executor();

        let result = executor.execute(&call("grep", &grep_args("hit", None, None)));

        assert!(!result.soft_failure);
        assert!(result.truncated, "record mirrors payload truncation");
        let body = payload(&result);
        assert_eq!(body["total_hits"], 70);
        assert_eq!(body["truncated"], true);
        assert_eq!(
            body["note"],
            "Returned first 50 of 70 hits. Narrow pattern, glob, or path."
        );
        let hits = body["hits"].as_array().expect("hits is an array");
        assert_eq!(hits.len(), 50);
        // The first 50 in deterministic (path, line) order: all of
        // a.txt, then the first 10 lines of b.txt.
        assert_eq!(hits[0]["path"], "a.txt");
        assert_eq!(hits[0]["line"], 1);
        assert_eq!(hits[39]["path"], "a.txt");
        assert_eq!(hits[39]["line"], 40);
        assert_eq!(hits[40]["path"], "b.txt");
        assert_eq!(hits[40]["line"], 1);
        assert_eq!(hits[49]["path"], "b.txt");
        assert_eq!(hits[49]["line"], 10);
    }

    #[test]
    fn grep_snippet_is_capped_at_two_hundred_chars_not_bytes() {
        let fixture = Fixture::new("grepsnip");
        // 150 multi-byte chars, then an ASCII tail: the 200-char cut
        // lands inside the ASCII run, which a byte slice could split
        // mid-character on other inputs.
        let line = format!("{}needle{}", "é".repeat(150), "y".repeat(100));
        assert_eq!(line.chars().count(), 256);
        fixture.write("long.txt", &format!("{line}\n"));
        let executor = fixture.executor();

        let result = executor.execute(&call("grep", &grep_args("needle", None, None)));

        assert!(!result.soft_failure, "must not panic: {result:?}");
        let body = payload(&result);
        assert_eq!(body["total_hits"], 1);
        let hits = body["hits"].as_array().expect("hits is an array");
        let snippet = hits[0]["snippet"].as_str().expect("snippet is a string");
        assert_eq!(snippet.chars().count(), 200);
        let expected: String = line.chars().take(200).collect();
        assert_eq!(snippet, expected);
    }

    #[test]
    fn grep_rejects_bad_patterns_globs_and_arguments() {
        let fixture = Fixture::new("grepbad");
        fixture.write("a.txt", "needle\n");
        let executor = fixture.executor();

        // Invalid regex preserves the compiler's message.
        let result = executor.execute(&call("grep", &grep_args("(", None, None)));
        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(!result.truncated);
        let body = payload(&result);
        assert!(
            body["error"]
                .as_str()
                .expect("error is a string")
                .contains("invalid regex"),
            "{body}"
        );

        // Invalid glob is a soft failure, not a panic.
        let result = executor.execute(&call("grep", &grep_args("needle", Some("["), None)));
        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        let body = payload(&result);
        assert!(
            body["error"]
                .as_str()
                .expect("error is a string")
                .contains("invalid glob"),
            "{body}"
        );

        // Empty pattern, malformed JSON, and a missing pattern are all
        // invalid arguments.
        for arguments in [
            grep_args("", None, None),
            "not json{{".to_string(),
            r#"{"path": "."}"#.to_string(),
        ] {
            let result = executor.execute(&call("grep", &arguments));
            assert!(result.soft_failure, "{arguments} must soft-fail");
            assert_eq!(result.call_id, "call-1");
            assert!(!result.truncated);
            let body = payload(&result);
            assert!(
                body["error"]
                    .as_str()
                    .expect("error is a string")
                    .contains("invalid arguments"),
                "{body}"
            );
        }
    }

    #[test]
    fn grep_glob_without_slash_matches_file_names_at_any_depth() {
        let fixture = Fixture::new("grepglob");
        fixture.write("src/main.rs", "needle\n");
        fixture.write("src/deep/nested.rs", "needle\n");
        fixture.write("notes.txt", "needle\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("grep", &grep_args("needle", Some("*.rs"), None)));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["glob"], "*.rs");
        assert_eq!(body["total_hits"], 2);
        let paths: Vec<&str> = body["hits"]
            .as_array()
            .expect("hits is an array")
            .iter()
            .map(|hit| hit["path"].as_str().expect("hit path is a string"))
            .collect();
        assert_eq!(paths, vec!["src/deep/nested.rs", "src/main.rs"]);
    }

    #[test]
    fn grep_glob_with_slash_matches_full_paths() {
        let fixture = Fixture::new("greppath");
        fixture.write("src/main.rs", "needle\n");
        fixture.write("src/deep/nested.rs", "needle\n");
        let executor = fixture.executor();

        // `*` does not cross `/`, so only the nested file matches.
        let result = executor.execute(&call(
            "grep",
            &grep_args("needle", Some("src/deep/*.rs"), None),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["total_hits"], 1);
        let hits = body["hits"].as_array().expect("hits is an array");
        assert_eq!(hits[0]["path"], "src/deep/nested.rs");

        // A glob matching nothing is a success with zero hits.
        let result = executor.execute(&call("grep", &grep_args("needle", Some("*.zzz"), None)));
        assert!(!result.soft_failure);
        assert!(!result.truncated);
        let body = payload(&result);
        assert_eq!(body["hits"], serde_json::json!([]));
        assert_eq!(body["total_hits"], 0);
        assert_eq!(body["truncated"], false);
    }

    #[test]
    fn grep_subdirectory_scope_excludes_the_rest() {
        let fixture = Fixture::new("grepscope");
        fixture.write("sub/a.txt", "target-word\n");
        fixture.write("other.txt", "target-word\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("grep", &grep_args("target-word", None, Some("sub"))));

        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["path"], "sub");
        assert_eq!(body["total_hits"], 1);
        let hits = body["hits"].as_array().expect("hits is an array");
        assert_eq!(hits[0]["path"], "sub/a.txt");
    }

    #[test]
    fn grep_single_file_scope_searches_only_that_file() {
        let fixture = Fixture::new("grepfile");
        fixture.write("only.txt", "lone-needle\n");
        fixture.write("rest.txt", "lone-needle\n");
        let executor = fixture.executor();

        let result = executor.execute(&call(
            "grep",
            &grep_args("lone-needle", None, Some("only.txt")),
        ));

        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["path"], "only.txt");
        assert_eq!(body["total_hits"], 1);
        let hits = body["hits"].as_array().expect("hits is an array");
        assert_eq!(hits[0]["path"], "only.txt");

        // A lockfile name as an explicit scope bypasses the walked-dir
        // lockfile exclusion.
        let fixture = Fixture::new("greplock");
        fixture.write("Cargo.lock", "pinned-needle\n");
        fixture.write("src/x.rs", "pinned-needle\n");
        let executor = fixture.executor();

        let result = executor.execute(&call(
            "grep",
            &grep_args("pinned-needle", None, Some("Cargo.lock")),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["total_hits"], 1);
        let hits = body["hits"].as_array().expect("hits is an array");
        assert_eq!(hits[0]["path"], "Cargo.lock");
    }

    #[test]
    fn grep_missing_scope_is_a_soft_failure() {
        let fixture = Fixture::new("grepmiss");
        let executor = fixture.executor();

        let result = executor.execute(&call("grep", &grep_args("x", None, Some("nope"))));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(!result.truncated);
        assert!(payload(&result).get("error").is_some());
    }

    #[test]
    fn grep_skips_pruned_directories_and_lockfiles() {
        let fixture = Fixture::new("grepprune");
        fixture.write("target/app.txt", "prune-me\n");
        fixture.write(".git/objects/x", "prune-me\n");
        fixture.write("node_modules/pkg/index.js", "prune-me\n");
        fixture.write("Cargo.lock", "prune-me\n");
        fixture.write("src/real.rs", "prune-me\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("grep", &grep_args("prune-me", None, None)));

        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["total_hits"], 1);
        let hits = body["hits"].as_array().expect("hits is an array");
        assert_eq!(hits[0]["path"], "src/real.rs");
    }

    #[test]
    fn grep_silently_skips_binary_and_non_utf8_files() {
        let fixture = Fixture::new("grepbin");
        fixture.write_bytes("blob.bin", b"needle\x00bytes\n");
        fixture.write_bytes("bad.txt", b"needle \xff\xfe not utf8\n");
        fixture.write("ok.txt", "needle\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("grep", &grep_args("needle", None, None)));

        assert!(!result.soft_failure);
        assert!(!result.truncated);
        let body = payload(&result);
        assert_eq!(body["total_hits"], 1);
        let hits = body["hits"].as_array().expect("hits is an array");
        assert_eq!(hits[0]["path"], "ok.txt");
        assert_eq!(hits[0]["line"], 1);
    }

    #[cfg(unix)]
    #[test]
    fn grep_follows_inside_file_links_and_ignores_the_rest() {
        use std::os::unix::fs::symlink;
        let fixture = Fixture::new("greplink");
        let outside = Fixture::new("greplink-outside");
        fixture.write("real/inner.txt", "link-needle\n");
        outside.write("secret.txt", "link-needle\n");
        symlink(
            fixture.dir.join("real/inner.txt"),
            fixture.dir.join("link.txt"),
        )
        .expect("create inner file symlink");
        symlink(
            outside.dir.join("secret.txt"),
            fixture.dir.join("escape.txt"),
        )
        .expect("create escaping file symlink");
        symlink(fixture.dir.join("real"), fixture.dir.join("linkdir")).expect("create dir symlink");
        symlink("missing-target.txt", fixture.dir.join("dangling.txt"))
            .expect("create dangling symlink");
        let executor = fixture.executor();

        let result = executor.execute(&call("grep", &grep_args("link-needle", None, None)));

        assert!(
            !result.soft_failure,
            "dangling links must not fail: {result:?}"
        );
        let body = payload(&result);
        assert_eq!(body["total_hits"], 2);
        let paths: Vec<&str> = body["hits"]
            .as_array()
            .expect("hits is an array")
            .iter()
            .map(|hit| hit["path"].as_str().expect("hit path is a string"))
            .collect();
        // The inside link is searched via its link path; the escaping
        // link, the symlinked directory, and the dangling link
        // contribute nothing.
        assert_eq!(paths, vec!["link.txt", "real/inner.txt"]);
    }

    // -- dispatch ---------------------------------------------------------------------

    #[test]
    fn read_symbol_is_not_implemented_yet() {
        let fixture = Fixture::new("unimpl");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_symbol", &read_args("x")));
        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        assert!(!result.truncated);
        assert!(result.activated_tool_names.is_empty());
        assert!(result.result_metadata.is_none());
        let body = payload(&result);
        assert_eq!(body["error"], "tool not implemented yet: read_symbol");
    }

    #[test]
    fn unknown_tool_is_a_soft_failure() {
        let fixture = Fixture::new("unknown");
        let executor = fixture.executor();

        let result = executor.execute(&call("shell_exec", r#"{}"#));

        assert!(result.soft_failure);
        assert_eq!(result.call_id, "call-1");
        let body = payload(&result);
        assert_eq!(body["error"], "unknown tool: shell_exec");
    }

    // -- sharing --------------------------------------------------------------------------

    #[test]
    fn executor_shares_across_scoped_threads() {
        let fixture = Fixture::new("shared");
        fixture.write("shared.txt", "alpha\nbeta\n");
        let executor: Arc<dyn ToolExecutor + Send + Sync> = Arc::new(fixture.executor());

        std::thread::scope(|scope| {
            let handles: Vec<_> = (0..2)
                .map(|_| {
                    scope.spawn(|| executor.execute(&call("read_file", &read_args("shared.txt"))))
                })
                .collect();
            for handle in handles {
                let result = handle.join().expect("worker threads join");
                assert!(!result.soft_failure);
                assert_eq!(result.call_id, "call-1");
                let body = payload(&result);
                assert_eq!(body["content"], "1: alpha\n2: beta");
            }
        });
    }
}
