//! Read-only repository tools for lens sessions (FRG-18).
//!
//! The executor ([`RepoToolExecutor`]) serves the `read_file`,
//! `list_dir`, `grep`, and `read_symbol` tools over one immutable
//! snapshot, with root confinement, a thread-safe cache of successful
//! records, and fail-closed secret redaction. The executor is
//! constructed from a checkout root plus its head SHA; its only shared
//! mutable state is the result cache behind a [`std::sync::Mutex`], so
//! `&self` execution composes with `Arc<dyn ToolExecutor + Send + Sync>`
//! and concurrent sessions share one cache safely.
//!
//! The tool definitions live here too, built from the single source of
//! truth [`TOOL_NAMES`]: [`TOOLS_SCHEMA_JSON`] is the schema channel
//! text FRG-19 drops into
//! [`SessionIoParams`](agent_core::engine::SessionIoParams), and
//! [`tool_catalog_config`] is the catalog value FRG-19 drops into
//! `AgentConfig.tool_semantics`. FRG-18 is complete in scope: the
//! executor plus these definitions are ready for FRG-19 to wire.
//!
//! Redaction runs inside the tool inners, before a record reaches the
//! [`ToolExecutor::execute`] boundary — so every cached record is already
//! redacted, and the executor is session-safe with respect to secret
//! leakage. One executor serves one immutable snapshot for one review, so
//! the cache is unbounded by design and entries never need invalidation.
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
//! The settled `read_symbol` payload shape echoes the request as
//! `{"name": <as given>, "status": "found" | "not_found" |
//! "ambiguous", "path": <definition file, found only>, "line":
//! <definition line, found only>, "start_line": <window start, found
//! only>, "end_line": <window end, found only>, "total_lines":
//! <usize, found only>, "content": <numbered window lines joined by
//! "\n", found only>, "candidates": [{"path": <repo-relative path>,
//! "line": <1-based line number>, "snippet": <first 200 chars of the
//! line>}] (the single candidate when found, up to 10 in (path, line)
//! order when ambiguous, empty when not found)>, "truncated": <bool>,
//! "note": "Multiple definition candidates; pass path to narrow."
//! (ambiguous only)>}`. The `name` needle is the last `::`-separated
//! (then `.`-separated) segment of the input, trimmed and matched as a
//! whole token against a line-shape definition heuristic (see
//! [`is_symbol_definition`]); the payload echoes the original `name`
//! as given. `ToolResultRecord.truncated` mirrors the payload's
//! `truncated`: true for a `found` window that does not cover the
//! whole file, or an `ambiguous` list that was capped at 10.
//!
//! The cache and redaction slices have landed, so the executor is
//! session-safe with respect to secret leakage.

use std::collections::HashMap;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard, OnceLock};

use agent_core::catalog::ToolSpec;
use agent_core::message::{ToolCallRecord, ToolResultRecord};
use agent_core::tool_name::ToolName;
use agent_core::tool_semantics::ToolCatalogConfig;
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

/// Maximum candidates one `read_symbol` call retains, in (path, line)
/// order. `found` always carries exactly one; `ambiguous` carries up
/// to this many while `truncated` reports whether more existed.
const MAX_SYMBOL_CANDIDATES: usize = 10;

/// The single source of truth for the FRG-18 tool name set, in schema
/// order. [`TOOLS_SCHEMA_JSON`], [`tool_specs`], and
/// [`tool_catalog_config`] are all built from (or checked against)
/// this array — never from a second hand-typed list.
pub const TOOL_NAMES: [&str; 4] = ["list_dir", "grep", "read_file", "read_symbol"];

/// The schema channel text for the four FRG-18 tools, in [`TOOL_NAMES`]
/// order. Each entry carries a `name`, a one-or-two-sentence
/// `description` (what the tool does, that paths are repo-relative and
/// confined to the snapshot, and its hard caps), and a `parameters`
/// object whose `required` array matches the corresponding `Args`
/// struct exactly. FRG-19 passes this string to
/// [`SessionIoParams::new`](agent_core::engine::SessionIoParams::new)
/// as `tools_schema_json`.
pub const TOOLS_SCHEMA_JSON: &str = r#"[
  {"name":"list_dir","description":"List one directory of the review snapshot: paths are repo-relative and confined to the snapshot, and at most 200 entries are returned sorted by name.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Repo-relative directory to list; \".\" for the snapshot root."}},"required":["path"],"additionalProperties":false}},
  {"name":"grep","description":"Search snapshot file contents with a regular expression: paths are repo-relative and confined to the snapshot, and at most 50 hits are returned with 200-character snippets.","parameters":{"type":"object","properties":{"pattern":{"type":"string","description":"Regular expression matched against each text line."},"glob":{"type":"string","description":"Optional glob: matched against the file name when it has no slash, else the full repo-relative path."},"path":{"type":"string","description":"Optional file or directory scope; defaults to the snapshot root."}},"required":["pattern"],"additionalProperties":false}},
  {"name":"read_file","description":"Read a numbered window of one snapshot text file: paths are repo-relative and confined to the snapshot, at most 200 lines are returned per call, and binary files are refused.","parameters":{"type":"object","properties":{"path":{"type":"string","description":"Repo-relative file path."},"start_line":{"type":"integer","minimum":1,"description":"Optional 1-based first line; defaults to 1."},"end_line":{"type":"integer","minimum":1,"description":"Optional 1-based last line; defaults to start_line plus 199."}},"required":["path"],"additionalProperties":false}},
  {"name":"read_symbol","description":"Look up a symbol definition in the snapshot and read a 200-line window around it: paths are repo-relative and confined to the snapshot, and an ambiguous name returns up to 10 candidates.","parameters":{"type":"object","properties":{"name":{"type":"string","description":"Symbol name; qualified forms search the last segment."},"path":{"type":"string","description":"Optional file or directory hint; defaults to the snapshot root."}},"required":["name"],"additionalProperties":false}}
]"#;

/// One [`ToolSpec`] per [`TOOL_NAMES`] entry, built from [`TOOL_NAMES`]
/// itself. Every tool is reproducible (same arguments over the same
/// snapshot yield the same result), carries no aliases, and needs no
/// schema hint — the model-facing shape lives in [`TOOLS_SCHEMA_JSON`].
pub fn tool_specs() -> Vec<ToolSpec> {
    TOOL_NAMES
        .iter()
        .map(|name| ToolSpec {
            name: ToolName::new(name).expect("static tool names are valid"),
            aliases: Vec::new(),
            reproducible: true,
            schema_hint: None,
        })
        .collect()
}

/// The catalog value FRG-19 drops into `AgentConfig.tool_semantics`:
/// the FRG-18 tool set with no discovery extras.
pub fn tool_catalog_config() -> ToolCatalogConfig {
    ToolCatalogConfig {
        tools: tool_specs(),
    }
}

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
/// nothing itself — `head_sha` is carried so the cache keys (and the
/// driver) can attribute reads to the snapshot they describe. The cache
/// holds only successful, already-redacted records: redaction happens in
/// the tool inners, before [`ToolExecutor::execute`] can insert.
pub struct RepoToolExecutor {
    root: PathBuf,
    head_sha: String,
    cache: Mutex<HashMap<CacheKey, ToolResultRecord>>,
}

impl RepoToolExecutor {
    /// Confine reads under `root`, canonicalized once here so every
    /// later [`ToolExecutor::execute`] compares canonical paths.
    pub fn new(root: PathBuf, head_sha: String) -> std::io::Result<Self> {
        let root = root.canonicalize()?;
        Ok(Self {
            root,
            head_sha,
            cache: Mutex::new(HashMap::new()),
        })
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
        // The effective window shares [`effective_read_window`] with the
        // cache key so dispatch and caching can never disagree on what
        // "the same arguments" means.
        let (start, end) =
            effective_read_window(args.start_line, args.end_line).ok_or_else(|| {
                ToolError::InvalidArguments(
                    "start_line must be at least 1 and end_line must be at least start_line"
                        .to_string(),
                )
            })?;

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
        // Redact before numbering: block state carries across the whole
        // window while line counts and numbers are preserved.
        let redacted = redact_text(&text);
        let lines: Vec<&str> = redacted.lines().collect();
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
        // Entry paths are repository-derived text, so a secret-shaped
        // file name is redacted here before it lands in the payload.
        let listed = redact_line(&normalize_list_path(&args.path));
        let entries: Vec<ListDirEntry> = names
            .into_iter()
            .take(MAX_LIST_ENTRIES)
            .map(|(name, kind, size)| ListDirEntry {
                path: if listed == "." {
                    redact_line(&name)
                } else {
                    redact_line(&format!("{listed}/{name}"))
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
                        // Snippets carry raw file text, so each one is
                        // redacted as a single line before it lands in
                        // the payload (a `-----BEGIN` marker redacts the
                        // snippet line on its own).
                        let snippet: String = line.chars().take(MAX_SNIPPET_CHARS).collect();
                        hits.push(GrepHit {
                            path: redact_line(rel),
                            line: index + 1,
                            snippet: redact_line(&snippet),
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

    /// Serve the `read_symbol` definition lookup: classify every line
    /// of the scope with [`is_symbol_definition`], then report one of
    /// three settled outcomes. Exactly one candidate returns a
    /// `found` window of at most [`MAX_READ_LINES`] lines that
    /// includes the definition line (10 lines before it where the
    /// file allows, shifted back when EOF is near so the window stays
    /// full where the file allows); zero candidates is a `not_found`
    /// success; more than one is an `ambiguous` list with no window,
    /// capped at [`MAX_SYMBOL_CANDIDATES`].
    fn read_symbol(&self, call: &ToolCallRecord) -> ToolResultRecord {
        match self.read_symbol_inner(call) {
            Ok(record) => record,
            Err(error) => soft_failure(call, error),
        }
    }

    fn read_symbol_inner(&self, call: &ToolCallRecord) -> Result<ToolResultRecord, ToolError> {
        let args: ReadSymbolArgs = serde_json::from_str(&call.arguments_json).map_err(|error| {
            ToolError::InvalidArguments(format!(
                "arguments are not a valid read_symbol request: {error}"
            ))
        })?;
        if args.name.trim().is_empty() {
            return Err(ToolError::InvalidArguments(
                "name must not be empty".to_string(),
            ));
        }
        // A qualified input (`Snapshot::capture`, `a.b`) searches the
        // last segment only; the payload still echoes the original.
        let after_colons = args.name.rsplit("::").next().unwrap_or(args.name.as_str());
        let needle = after_colons
            .rsplit('.')
            .next()
            .unwrap_or(after_colons)
            .trim();
        if needle.is_empty() {
            return Err(ToolError::InvalidArguments(
                "name must contain a symbol segment".to_string(),
            ));
        }
        let scope = args.path.as_deref().unwrap_or(".");
        let candidate = self.confine(scope)?;
        let listed = normalize_list_path(scope);

        // An explicit file scope searches only that file: pruning and
        // lockfile exclusion do not apply, exactly like `grep`. A
        // directory scope reuses the `grep` walk discipline.
        let rels: Vec<String> = if fs::metadata(&candidate)
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            vec![listed.clone()]
        } else {
            self.walk_grep_scope(&candidate, &listed)?
        };

        let mut candidates: Vec<SymbolCandidate> = Vec::new();
        for rel in &rels {
            let bytes = fs::read(self.root.join(rel)).unwrap_or_default();
            // Binary, non-UTF-8, and unreadable files are silently
            // skipped, exactly like `grep`.
            if bytes.contains(&0) {
                continue;
            }
            let Ok(text) = String::from_utf8(bytes) else {
                continue;
            };
            for (index, line) in text.lines().enumerate() {
                if is_symbol_definition(line, needle) {
                    // The snippet is redacted now; the path stays raw
                    // here because `found` re-reads the file through it
                    // below — paths are redacted when the payload is
                    // built (see [`redact_symbol_paths`]).
                    let snippet: String = line.chars().take(MAX_SNIPPET_CHARS).collect();
                    candidates.push(SymbolCandidate {
                        path: rel.clone(),
                        line: index + 1,
                        snippet: redact_line(&snippet),
                    });
                }
            }
        }

        if candidates.is_empty() {
            let payload = ReadSymbolPayload {
                name: args.name.clone(),
                status: "not_found".to_string(),
                path: None,
                line: None,
                start_line: None,
                end_line: None,
                total_lines: None,
                content: None,
                candidates,
                truncated: false,
                note: None,
            };
            return Self::render_symbol(call, payload, false);
        }

        if candidates.len() > 1 {
            let truncated = candidates.len() > MAX_SYMBOL_CANDIDATES;
            candidates.truncate(MAX_SYMBOL_CANDIDATES);
            let payload = ReadSymbolPayload {
                name: args.name.clone(),
                status: "ambiguous".to_string(),
                path: None,
                line: None,
                start_line: None,
                end_line: None,
                total_lines: None,
                content: None,
                candidates: redact_symbol_paths(candidates),
                truncated,
                note: Some("Multiple definition candidates; pass path to narrow.".to_string()),
            };
            return Self::render_symbol(call, payload, truncated);
        }

        let found = &candidates[0];
        let bytes = fs::read(self.root.join(&found.path)).map_err(|error| ToolError::Io {
            path: found.path.clone(),
            message: error.to_string(),
        })?;
        if bytes.contains(&0) {
            return Err(ToolError::BinaryFile {
                path: found.path.clone(),
            });
        }
        let text = String::from_utf8(bytes).map_err(|_| ToolError::BinaryFile {
            path: found.path.clone(),
        })?;
        // Redact before numbering, exactly like `read_file`: block
        // state carries across the whole window while line counts and
        // numbers are preserved.
        let redacted = redact_text(&text);
        let lines: Vec<&str> = redacted.lines().collect();
        let total_lines = lines.len();
        // The file held the candidate a moment ago, so it is
        // non-empty and covers the definition line; a concurrent
        // shrink is a typed failure rather than a panic.
        if found.line > total_lines || total_lines == 0 {
            return Err(ToolError::Io {
                path: found.path.clone(),
                message: "file changed while reading".to_string(),
            });
        }
        let mut start_line = found.line.saturating_sub(10).max(1);
        let mut end_line = (start_line + MAX_READ_LINES - 1).min(total_lines);
        if end_line - start_line + 1 < MAX_READ_LINES {
            start_line = end_line.saturating_sub(MAX_READ_LINES - 1).max(1);
            end_line = (start_line + MAX_READ_LINES - 1).min(total_lines);
        }
        let content: String = lines
            .iter()
            .skip(start_line - 1)
            .take(end_line - start_line + 1)
            .enumerate()
            .map(|(index, line)| format!("{}: {line}", start_line + index))
            .collect::<Vec<String>>()
            .join("\n");
        let truncated = start_line > 1 || end_line < total_lines;
        let payload = ReadSymbolPayload {
            name: args.name.clone(),
            status: "found".to_string(),
            path: Some(redact_line(&found.path)),
            line: Some(found.line),
            start_line: Some(start_line),
            end_line: Some(end_line),
            total_lines: Some(total_lines),
            content: Some(content),
            candidates: redact_symbol_paths(candidates),
            truncated,
            note: None,
        };
        Self::render_symbol(call, payload, truncated)
    }

    /// Serialize a `read_symbol` payload and stamp the record. Every
    /// outcome here is a success: `soft_failure` stays false and
    /// `truncated` mirrors the payload.
    fn render_symbol(
        call: &ToolCallRecord,
        payload: ReadSymbolPayload,
        truncated: bool,
    ) -> Result<ToolResultRecord, ToolError> {
        let result_json = serde_json::to_string(&payload)
            .map_err(|error| ToolError::Render(error.to_string()))?;
        Ok(ToolResultRecord {
            call_id: call.call_id.clone(),
            result_json,
            truncated,
            soft_failure: false,
            activated_tool_names: Vec::new(),
            result_metadata: None,
        })
    }
}

impl RepoToolExecutor {
    /// Dispatch a call to its tool without consulting the cache.
    fn dispatch(&self, call: &ToolCallRecord) -> ToolResultRecord {
        match call.name.as_str() {
            "read_file" => self.read_file(call),
            "list_dir" => self.list_dir(call),
            "grep" => self.grep(call),
            "read_symbol" => self.read_symbol(call),
            _ => soft_failure(
                call,
                ToolError::UnknownTool {
                    name: call.name.clone(),
                },
            ),
        }
    }
}

impl ToolExecutor for RepoToolExecutor {
    fn execute(&self, call: &ToolCallRecord) -> ToolResultRecord {
        // Cache boundary: on a hit, clone the stored record and stamp
        // the current call's id on it. The lock is held only for the
        // map lookup/clone and the insert below — never for filesystem
        // work. Two threads missing the same key concurrently may both
        // compute it (a benign duplicate read of an immutable snapshot);
        // the second insert simply overwrites the first with an equal
        // record. Only successes are cached: the tool inners return
        // already-redacted records, so the cache never holds secrets.
        let Some(key) = cache_key_for(&self.head_sha, call) else {
            return self.dispatch(call);
        };
        if let Some(cached) = lock_cache(&self.cache).get(&key).cloned() {
            let mut hit = cached;
            hit.call_id = call.call_id.clone();
            return hit;
        }
        let result = self.dispatch(call);
        if !result.soft_failure {
            lock_cache(&self.cache).insert(key, result.clone());
        }
        result
    }
}

/// Recover the cache guard without panicking: a poisoned mutex still
/// holds a usable map, and a cache must never fail a tool call.
fn lock_cache<'a>(
    cache: &'a Mutex<HashMap<CacheKey, ToolResultRecord>>,
) -> MutexGuard<'a, HashMap<CacheKey, ToolResultRecord>> {
    cache
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// The cache key: the snapshot plus the canonical effective arguments.
/// Only calls whose key is `Some` are cacheable; anything the tools
/// would soft-fail on (unparseable arguments, an invalid window) has no
/// key and dispatches uncached.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum CacheKey {
    ReadFile {
        head_sha: String,
        path: String,
        start: usize,
        end: usize,
    },
    ListDir {
        head_sha: String,
        path: String,
    },
    Grep {
        head_sha: String,
        pattern: String,
        glob: Option<String>,
        scope: String,
    },
    ReadSymbol {
        head_sha: String,
        name: String,
        scope: String,
    },
}

/// Compute the cache key for a call, or `None` when the call is not
/// cacheable (an unknown tool, unparseable arguments, or arguments that
/// dispatch would soft-fail on). Normalization mirrors the tools: the
/// `read_file` window uses [`effective_read_window`], the `list_dir`
/// scope uses [`normalize_list_path`], and the `grep` / `read_symbol`
/// scopes default to `"."` exactly as the inners do.
fn cache_key_for(head_sha: &str, call: &ToolCallRecord) -> Option<CacheKey> {
    match call.name.as_str() {
        "read_file" => {
            let args: ReadFileArgs = serde_json::from_str(&call.arguments_json).ok()?;
            if args.path.is_empty() {
                return None;
            }
            let (start, end) = effective_read_window(args.start_line, args.end_line)?;
            Some(CacheKey::ReadFile {
                head_sha: head_sha.to_string(),
                path: args.path,
                start,
                end,
            })
        }
        "list_dir" => {
            let args: ListDirArgs = serde_json::from_str(&call.arguments_json).ok()?;
            if args.path.is_empty() {
                return None;
            }
            Some(CacheKey::ListDir {
                head_sha: head_sha.to_string(),
                path: normalize_list_path(&args.path),
            })
        }
        "grep" => {
            let args: GrepArgs = serde_json::from_str(&call.arguments_json).ok()?;
            if args.pattern.is_empty() {
                return None;
            }
            let raw_scope = args.path.as_deref().unwrap_or(".");
            if raw_scope.is_empty() {
                return None;
            }
            Some(CacheKey::Grep {
                head_sha: head_sha.to_string(),
                pattern: args.pattern,
                glob: args.glob,
                scope: normalize_list_path(raw_scope),
            })
        }
        "read_symbol" => {
            let args: ReadSymbolArgs = serde_json::from_str(&call.arguments_json).ok()?;
            if args.name.trim().is_empty() {
                return None;
            }
            let raw_scope = args.path.as_deref().unwrap_or(".");
            if raw_scope.is_empty() {
                return None;
            }
            Some(CacheKey::ReadSymbol {
                head_sha: head_sha.to_string(),
                name: args.name,
                scope: normalize_list_path(raw_scope),
            })
        }
        _ => None,
    }
}

/// The canonical `read_file` window: `start_line` defaults to 1,
/// `end_line` defaults to `start_line + 199`, always clamped to at most
/// [`MAX_READ_LINES`] lines. Returns `None` for an invalid window — a
/// zero `start_line`, or an `end_line` that is zero or below `start` —
/// which dispatch reports as a soft failure. Shared by
/// [`RepoToolExecutor::read_file_inner`] and [`cache_key_for`] so the
/// two can never drift apart.
fn effective_read_window(start: Option<usize>, end: Option<usize>) -> Option<(usize, usize)> {
    let effective_start = start.unwrap_or(1);
    if effective_start == 0 {
        return None;
    }
    let cap = effective_start.saturating_add(MAX_READ_LINES - 1);
    let effective_end = match end {
        Some(requested) => {
            if requested == 0 || requested < effective_start {
                return None;
            }
            requested.min(cap)
        }
        None => cap,
    };
    Some((effective_start, effective_end))
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

/// The `read_symbol` request shape: `arguments_json` deserializes
/// into this, so a missing `name` (or malformed JSON) is a parse
/// failure. `path` defaults to the root when absent.
#[derive(Debug, Deserialize)]
struct ReadSymbolArgs {
    name: String,
    path: Option<String>,
}

/// One `read_symbol` candidate: the repo-relative path, the 1-based
/// definition line, and the first [`MAX_SNIPPET_CHARS`] characters of
/// that line.
#[derive(Debug, Serialize)]
struct SymbolCandidate {
    path: String,
    line: usize,
    snippet: String,
}

/// The settled `read_symbol` success payload, in field order `name`,
/// `status`, `path`, `line`, `start_line`, `end_line`, `total_lines`,
/// `content`, `candidates`, `truncated`, `note`. `found` sets every
/// field but `note`; `not_found` sets only `name`, `status`,
/// `candidates: []`, and `truncated: false`; `ambiguous` sets `name`,
/// `status`, up to [`MAX_SYMBOL_CANDIDATES`] `candidates`, `truncated`
/// (true only when the list was capped), and `note`.
#[derive(Debug, Serialize)]
struct ReadSymbolPayload {
    name: String,
    status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_line: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_lines: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<String>,
    candidates: Vec<SymbolCandidate>,
    truncated: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    note: Option<String>,
}

/// Leading words stripped before the definition-keyword checks.
/// `pub` covers both bare `pub` and `pub(...)`-style visibility
/// (`pub(crate)`, `pub(in crate)`, ...), consumed token-wise.
const DEFINITION_MODIFIERS: &[&str] = &[
    "pub",
    "async",
    "unsafe",
    "static",
    "public",
    "private",
    "protected",
    "internal",
    "final",
    "abstract",
    "sealed",
    "partial",
    "virtual",
    "override",
    "export",
    "default",
];

/// Keywords introducing a function-shaped definition once leading
/// modifiers are stripped: `fn capture(`, `def capture(`, ...
const DEFINITION_FN_KEYWORDS: &[&str] = &["fn", "def", "func", "fun", "function"];

/// Keywords introducing a type-shaped definition: `class Snapshot`,
/// `pub struct Snapshot`, ...
const DEFINITION_TYPE_KEYWORDS: &[&str] = &[
    "class",
    "struct",
    "enum",
    "trait",
    "interface",
    "record",
    "type",
    "object",
];

/// Keywords introducing a constant/binding definition. These double
/// as leading modifiers (so `public static MAX = 5` still reads), and
/// are therefore never stripped by the binding check itself.
const DEFINITION_BIND_KEYWORDS: &[&str] = &["const", "static", "let", "var"];

/// First tokens that disqualify the method-shape check: control flow
/// and call-site verbs whose trailing `name(` is a use, not a
/// declaration (`return capture()`, `if ready(x)`, ...).
const METHOD_SHAPE_STOPWORDS: &[&str] = &[
    "if", "else", "for", "while", "loop", "match", "return", "use", "import", "from", "include",
    "require", "print", "println", "eprintln", "echo", "assert", "panic", "throw", "new", "delete",
    "sizeof", "typeof", "case", "switch", "do", "try", "catch", "finally", "yield", "await",
];

/// A line is a definition candidate when the whole-token `needle`
/// matches one of the documented definition shapes.
///
/// This is a grep-based approximation shared across languages, not a
/// parser, and it errs on the side of missing exotic spellings rather
/// than claiming call sites. Known limits: only the listed keywords
/// and modifiers are recognized (a language whose declaration verb is
/// missing here never matches); multi-line signatures match only when
/// the name and its `(` share the declaration's first line; type
/// annotations between a binding keyword and its `:`/`=` (as in
/// `static final int MAX = 5`) are accepted but comma-separated
/// multi-declarators (`let x = 1, y = 2`) are not; a `name(` preceded
/// only by expression punctuation (`=`, `.`, `;`, brackets, quotes,
/// slashes) is treated as a call, so qualified definitions written
/// with a leading `}` or receiver (`} void M() {`, `obj.M() {`) are
/// missed; and comment text naming a symbol (`// fn capture(`) can
/// only match when the comment's own punctuation already disqualifies
/// it — an English sentence shaped like `verb name(` still matches.
fn is_symbol_definition(line: &str, needle: &str) -> bool {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return false;
    }
    if let Some(after) = trimmed.strip_prefix("macro_rules!") {
        if starts_with_whole_token(after.trim_start(), needle) {
            return true;
        }
    }
    let stripped = strip_leading_modifiers(trimmed, false);
    if keyword_definition_match(stripped, needle, DEFINITION_FN_KEYWORDS)
        || keyword_definition_match(stripped, needle, DEFINITION_TYPE_KEYWORDS)
    {
        return true;
    }
    if binding_definition_match(strip_leading_modifiers(trimmed, true), needle) {
        return true;
    }
    method_shape_match(trimmed, needle)
}

/// Strip leading modifier words, returning the remainder. When
/// `keep_binding_keywords` is set, the [`DEFINITION_BIND_KEYWORDS`]
/// stop the strip so the binding check still sees its keyword.
fn strip_leading_modifiers(mut rest: &str, keep_binding_keywords: bool) -> &str {
    loop {
        let trimmed = rest.trim_start();
        let end = trimmed
            .find(|next: char| next.is_whitespace())
            .unwrap_or(trimmed.len());
        if end == 0 {
            return trimmed;
        }
        let token = &trimmed[..end];
        if keep_binding_keywords && DEFINITION_BIND_KEYWORDS.contains(&token) {
            return trimmed;
        }
        if !DEFINITION_MODIFIERS.contains(&token) && !token.starts_with("pub(") {
            return trimmed;
        }
        rest = &trimmed[end..];
        if token.starts_with("pub(") && !token.contains(')') {
            // `pub(in crate)`-style visibility spans tokens: consume
            // through the token holding the closing paren.
            loop {
                let inner = rest.trim_start();
                let inner_end = inner
                    .find(|next: char| next.is_whitespace())
                    .unwrap_or(inner.len());
                if inner_end == 0 {
                    rest = inner;
                    break;
                }
                rest = &inner[inner_end..];
                if inner[..inner_end].contains(')') {
                    break;
                }
            }
        }
    }
}

/// A keyword-shaped definition: the first word is one of `keywords`
/// and the text after it starts with the whole-token `needle`
/// (`fn capture(`, `pub struct Snapshot`, Ruby's paren-less
/// `def capture`, ...).
fn keyword_definition_match(rest: &str, needle: &str, keywords: &[&str]) -> bool {
    let end = rest
        .find(|next: char| next.is_whitespace())
        .unwrap_or(rest.len());
    if !keywords.iter().any(|known| *known == &rest[..end]) {
        return false;
    }
    starts_with_whole_token(rest[end..].trim_start(), needle)
}

/// A binding definition: `keyword <name> [: or =] ...` where `keyword`
/// is one of [`DEFINITION_BIND_KEYWORDS`]. Type words may sit between
/// the keyword and the name (`static final int MAX = 5`), but no
/// expression punctuation may (`let y = capture();` is a call site,
/// and its `y =` run-up disqualifies `capture`).
fn binding_definition_match(rest: &str, needle: &str) -> bool {
    let end = rest
        .find(|next: char| next.is_whitespace())
        .unwrap_or(rest.len());
    if !DEFINITION_BIND_KEYWORDS
        .iter()
        .any(|known| *known == &rest[..end])
    {
        return false;
    }
    let after_keyword = &rest[end..];
    let mut search = after_keyword;
    let mut offset = 0;
    while let Some(found) = search.find(needle) {
        let absolute = offset + found;
        let before_ok = absolute == 0
            || !after_keyword[..absolute]
                .chars()
                .next_back()
                .is_some_and(is_word_char);
        let tail = after_keyword[absolute + needle.len()..].trim_start();
        let between = &after_keyword[..absolute];
        let between_ok = !between.chars().any(|next| {
            matches!(
                next,
                '=' | ';' | '(' | ')' | '{' | '}' | '.' | '"' | '\'' | '`' | '/' | '\\' | ':'
            )
        });
        if before_ok && between_ok && (tail.starts_with(':') || tail.starts_with('=')) {
            return true;
        }
        search = &search[found + needle.len()..];
        offset = absolute + needle.len();
    }
    false
}

/// A method-shaped declaration: the whole-token `needle` is
/// immediately followed by `(` with at least one leading word before
/// it (`public void Capture(`). A bare call at line start
/// (`capture();`) has no leading word and never matches; call sites
/// with expression punctuation in the run-up (`let y = capture();`,
/// `obj.capture();`) are rejected with it.
fn method_shape_match(trimmed: &str, needle: &str) -> bool {
    let first_end = trimmed
        .find(|next: char| next.is_whitespace())
        .unwrap_or(trimmed.len());
    if METHOD_SHAPE_STOPWORDS
        .iter()
        .any(|stop| *stop == &trimmed[..first_end])
    {
        return false;
    }
    let mut search = trimmed;
    let mut offset = 0;
    while let Some(found) = search.find(needle) {
        let absolute = offset + found;
        let before_ok = absolute > 0
            && !trimmed[..absolute]
                .chars()
                .next_back()
                .is_some_and(is_word_char);
        let tail = trimmed[absolute + needle.len()..].trim_start();
        if before_ok && tail.starts_with('(') && method_prefix_ok(&trimmed[..absolute]) {
            return true;
        }
        search = &search[found + needle.len()..];
        offset = absolute + needle.len();
    }
    false
}

/// The run-up before a method-shaped `name(` must hold at least one
/// word (a modifier or return type) and no expression punctuation.
fn method_prefix_ok(prefix: &str) -> bool {
    prefix
        .chars()
        .any(|next| next.is_alphanumeric() || next == '_')
        && !prefix.chars().any(|next| {
            matches!(
                next,
                '.' | '=' | ';' | '(' | ')' | '{' | '}' | '"' | '\'' | '`' | '/' | '\\'
            )
        })
}

/// The needle matches when it appears at the head of `haystack` and
/// the next character cannot extend it into a longer identifier, so
/// `capture` never matches `captures` — on either side, since callers
/// pair this with the same whole-token check on the preceding char.
fn starts_with_whole_token(haystack: &str, needle: &str) -> bool {
    haystack.starts_with(needle)
        && haystack[needle.len()..]
            .chars()
            .next()
            .is_none_or(|next| !is_word_char(next))
}

/// Identifier characters for whole-token matching.
fn is_word_char(next: char) -> bool {
    next.is_alphanumeric() || next == '_'
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

/// Redact `read_symbol` candidate paths for the payload. The single
/// `found` candidate keeps its raw path through the file re-read above;
/// every path is redacted here, at the last step before serialization.
fn redact_symbol_paths(candidates: Vec<SymbolCandidate>) -> Vec<SymbolCandidate> {
    candidates
        .into_iter()
        .map(|candidate| SymbolCandidate {
            path: redact_line(&candidate.path),
            line: candidate.line,
            snippet: candidate.snippet,
        })
        .collect()
}

/// Fail-closed secret redaction (FRG-18 slice 5). Every pattern is
/// compiled once behind a [`OnceLock`]; matching is line-oriented so
/// line counts and line numbers never shift, and the replacement token
/// is always exactly `<redacted>`. When in doubt the value is redacted:
/// a bare word that merely names a secret without an assignment
/// operator is left alone, but any `key`/`=`-style assignment whose key
/// denotes a secret by meaning loses its value.
struct Redactor {
    auth_header: Regex,
    jwt: Regex,
    github_token: Regex,
    gitlab_token: Regex,
    aws_key_id: Regex,
    slack_token: Regex,
    openai_key: Regex,
    bearer_token: Regex,
    url_password: Regex,
    secret_assignment: Regex,
    generic_assignment: Regex,
}

impl Redactor {
    fn new() -> Self {
        // Each literal below is fixed and valid; a failure here is a
        // programming error, never untrusted input.
        let compile = |pattern: &str| Regex::new(pattern).expect("redaction pattern compiles");
        Self {
            // `Authorization: <anything>` — the whole credential tail.
            auth_header: compile(
                r#"(?i)("Authorization"|Authorization)\s*:\s*("[^"]*"|'[^']*'|.+)"#,
            ),
            // Three base64url segments opening with `eyJ`.
            jwt: compile(r"eyJ[A-Za-z0-9_\-]*\.[A-Za-z0-9_\-]+\.[A-Za-z0-9_\-]+"),
            // GitHub `gh[pousr]_` tokens (at least 20 token chars).
            github_token: compile(r"gh[prous]_[A-Za-z0-9]{20,}"),
            gitlab_token: compile(r"glpat-[A-Za-z0-9_\-]+"),
            aws_key_id: compile(r"(AKIA|ASIA)[A-Z0-9]{16}"),
            slack_token: compile(r"xox[baprs]-[A-Za-z0-9\-]+"),
            // `sk-` / `sk-proj-` keys (at least 20 token chars).
            openai_key: compile(r"sk-(?:proj-)?[A-Za-z0-9_\-]{20,}"),
            // The credential after a `Bearer` / `Basic` scheme word.
            bearer_token: compile(r"(?i)\b(Bearer|Basic)\s+[A-Za-z0-9_\-\.=~+/]+"),
            // The password in `scheme://user:password@host`.
            url_password: compile(r"([A-Za-z][A-Za-z0-9+.\-]*://[^/\s:@?#]+:)([^/\s@?#]+)(@)"),
            // A value assigned to a secret-denoting key: the key holds
            // one of the secret segments (`secret`, `token`, `password`,
            // `passwd`, `credential`, `api-key` / `apikey`, `access-key`,
            // `auth`, `private-key`, `session-key`, `signing-key`,
            // `encryption-key`, case-insensitive, camelCase included)
            // followed by `:` or `=` and a quoted or bare value.
            secret_assignment: compile(
                r#"(?i)([A-Za-z0-9_.$\-]*?(?:secret|passwd|password|token|credential|api[_-]?key|access[_-]?key|auth|private[_-]?key|session[_-]?key|signing[_-]?key|encryption[_-]?key)[A-Za-z0-9_.$\-]*)(?:["'])?\s*([:=])\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^\s,;]+)"#,
            ),
            // A candidate assignment of any key shape: a key of
            // identifier characters whose words may be space-separated
            // (as in `application programming interface key`), an
            // optional closing quote, `:` or `=`, and a quoted or bare
            // value. The key is greedy over its identifier run, so
            // `mytokenize = v` classifies on the whole key rather than
            // a suffix of it; a leading quote stays outside the match
            // and is preserved untouched. This pattern never classifies
            // on its own: [`key_denotes_secret`] decides in code whether
            // the key denotes a secret by meaning.
            generic_assignment: compile(
                r#"([A-Za-z0-9_.$\-]+(?:[ \t]+[A-Za-z0-9_.$\-]+)*)(["']?)\s*([:=])\s*("(?:[^"\\]|\\.)*"|'(?:[^'\\]|\\.)*'|[^\s,;]+)"#,
            ),
        }
    }

    fn redact_one_line(&self, line: &str) -> String {
        // The authorization header consumes its whole tail first so a
        // later pattern can never see a fragment of it; the scheme-word
        // pattern runs before the generic assignment so credentials
        // after a scheme word are fully removed.
        let owned = self.auth_header.replace_all(line, "${1}: <redacted>");
        let owned = self.jwt.replace_all(&owned, "<redacted>");
        let owned = self.github_token.replace_all(&owned, "<redacted>");
        let owned = self.gitlab_token.replace_all(&owned, "<redacted>");
        let owned = self.aws_key_id.replace_all(&owned, "<redacted>");
        let owned = self.slack_token.replace_all(&owned, "<redacted>");
        let owned = self.openai_key.replace_all(&owned, "<redacted>");
        let owned = self.bearer_token.replace_all(&owned, "${1} <redacted>");
        let owned = self.url_password.replace_all(&owned, "${1}<redacted>${3}");
        // The enumerated assignment pattern runs first so every key it
        // already covered keeps its exact historical string; the
        // meaning-based pass below only adds coverage for keys the
        // enumeration cannot name.
        let owned = self
            .secret_assignment
            .replace_all(&owned, "${1}${2} <redacted>");
        self.redact_generic_assignments(&owned)
    }

    /// Redact the values of candidate assignments whose key denotes a
    /// secret by meaning ([`key_denotes_secret`]), one overlapping
    /// candidate at a time: a non-secret key advances a single
    /// character so a secret assignment nested behind it (as in `info:
    /// session_id = v`, where `session_id` first parses as the value)
    /// is still examined. Only the value is replaced; the key, any
    /// closing quote, and the separator are preserved verbatim. Never
    /// panics on untrusted text: every index is a regex match boundary
    /// or a `char` boundary derived from one.
    fn redact_generic_assignments(&self, line: &str) -> String {
        let mut redacted = String::with_capacity(line.len());
        let mut pos = 0;
        while pos < line.len() {
            let Some(captures) = self.generic_assignment.captures_at(line, pos) else {
                redacted.push_str(&line[pos..]);
                break;
            };
            let whole = captures.get(0).expect("assignment match covers itself");
            let key = captures.get(1).map(|key| key.as_str()).unwrap_or("");
            redacted.push_str(&line[pos..whole.start()]);
            if key_denotes_secret(key) {
                let close = captures.get(2).map(|quote| quote.as_str()).unwrap_or("");
                let separator = captures.get(3).map(|sep| sep.as_str()).unwrap_or("");
                redacted.push_str(key);
                redacted.push_str(close);
                redacted.push_str(separator);
                redacted.push_str(" <redacted>");
                pos = whole.end();
            } else {
                let step = line[whole.start()..]
                    .chars()
                    .next()
                    .expect("assignment match is non-empty")
                    .len_utf8();
                let next = whole.start() + step;
                redacted.push_str(&line[whole.start()..next]);
                pos = next;
            }
        }
        redacted
    }
}

/// Subtokens marking an assignment key as secret-denoting. The
/// check is whole-subtoken equality after splitting the key on `_`,
/// `-`, `.`, `$`, whitespace, and camelCase humps, so `master_key`
/// (via `key`), `databasePassphrase` (via `passphrase`), and
/// `application programming interface key` (via `key`) all classify
/// while prose without an assignment operator never reaches this
/// check. `authorization` and `authenticate` keep parity with the
/// enumerated pattern's `auth` substring for `=`-shaped assignments
/// (the `:` shape is already consumed by the header pattern).
const SECRET_MEANING_SUBTOKENS: &[&str] = &[
    "secret",
    "secrets",
    "token",
    "tokens",
    "password",
    "passwords",
    "passwd",
    "passphrase",
    "passcode",
    "pwd",
    "credential",
    "credentials",
    "auth",
    "authentication",
    "authorization",
    "authenticate",
    "key",
    "keys",
    "cookie",
    "cookies",
    "session",
    "private",
    "dsn",
];

/// Whether an assignment key denotes a secret by meaning: split the
/// key into subtokens and check each against
/// [`SECRET_MEANING_SUBTOKENS`]. Fail closed on shape, open on prose:
/// an empty key never denotes a secret (there is nothing to name a
/// value with), while any single secret-meaning subtoken redacts the
/// value even when sibling subtokens look benign (`token_count`).
/// Never panics on untrusted text: all splitting is on `char`
/// boundaries.
fn key_denotes_secret(raw_key: &str) -> bool {
    let key = raw_key
        .trim()
        .trim_matches(|next: char| next == '"' || next == '\'');
    if key.is_empty() {
        return false;
    }
    for chunk in key.split(|next: char| {
        next == '_' || next == '-' || next == '.' || next == '$' || next.is_whitespace()
    }) {
        if chunk.is_empty() {
            continue;
        }
        for subtoken in split_camel_humps(chunk) {
            if SECRET_MEANING_SUBTOKENS.contains(&subtoken.to_lowercase().as_str()) {
                return true;
            }
        }
    }
    false
}

/// Split one separator-free chunk on camelCase humps: a boundary sits
/// before an uppercase letter following a lowercase letter or digit
/// (`databasePassphrase` into `database` + `Passphrase`), and before
/// an uppercase letter following capitals when a lowercase letter
/// comes next (`APIKey` into `API` + `Key`). Slices are cut at the
/// `char_indices` boundaries, so multi-byte text cannot misalign them.
fn split_camel_humps(chunk: &str) -> Vec<&str> {
    let indices: Vec<(usize, char)> = chunk.char_indices().collect();
    if indices.is_empty() {
        return Vec::new();
    }
    let mut starts = vec![0usize];
    for (position, (index, next)) in indices.iter().enumerate() {
        if position == 0 || !next.is_uppercase() {
            continue;
        }
        let previous = indices[position - 1].1;
        let next_is_lower = indices
            .get(position + 1)
            .is_some_and(|(_, after)| after.is_lowercase());
        if previous.is_lowercase() || previous.is_numeric() || next_is_lower {
            starts.push(*index);
        }
    }
    starts.push(chunk.len());
    starts
        .windows(2)
        .map(|pair| &chunk[pair[0]..pair[1]])
        .collect()
}

fn redactor() -> &'static Redactor {
    static INSTANCE: OnceLock<Redactor> = OnceLock::new();
    INSTANCE.get_or_init(Redactor::new)
}

/// Redact one repository-derived line: a `-----BEGIN` / `-----END`
/// marker line redacts as a whole on its own (single-line callers have
/// no block state to carry), anything else runs the inline patterns.
fn redact_line(line: &str) -> String {
    if line.contains("-----BEGIN") || line.contains("-----END") {
        return "<redacted>".to_string();
    }
    redactor().redact_one_line(line)
}

/// Redact whole text (a `read_file` / `read_symbol` window) line by
/// line so line counts and numbers never shift. A line holding
/// `-----BEGIN ... PRIVATE KEY-----` starts block state: the header
/// line, every line inside, and the `-----END ...` line are each
/// replaced by one `<redacted>` line. An unterminated block redacts
/// through the end of the input (fail closed).
fn redact_text(text: &str) -> String {
    let mut redacted: Vec<String> = Vec::new();
    let mut in_block = false;
    for line in text.lines() {
        if !in_block && line.contains("-----BEGIN") && line.contains("PRIVATE KEY") {
            in_block = true;
            redacted.push("<redacted>".to_string());
        } else if in_block {
            redacted.push("<redacted>".to_string());
            if line.contains("-----END") {
                in_block = false;
            }
        } else {
            redacted.push(redact_line(line));
        }
    }
    redacted.join("\n")
}

fn soft_failure(call: &ToolCallRecord, error: ToolError) -> ToolResultRecord {
    // Error text is repository-derived too (paths echo the request), so
    // it is redacted as a single line before it lands in the payload.
    let result_json = serde_json::to_string(&ErrorPayload {
        error: redact_line(&error.to_string()),
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

    // -- read_symbol ------------------------------------------------------------

    fn symbol_args(name: &str, path: Option<&str>) -> String {
        let mut value = serde_json::json!({"name": name});
        if let Some(path) = path {
            value["path"] = serde_json::json!(path);
        }
        value.to_string()
    }

    #[test]
    fn rust_fn_found_with_window_while_call_sites_are_ignored() {
        let fixture = Fixture::new("symfn");
        let mut contents: Vec<String> = (1..=250).map(|n| format!("// filler line {n}")).collect();
        contents[19] = "pub fn capture(input: &str) -> bool {".to_string();
        contents[20] = "    capture(input);".to_string();
        contents[21] = "    let hit = capture(input);".to_string();
        contents[22] = "    capture();".to_string();
        fixture.write("src/lib.rs", &contents.join("\n"));
        let executor = fixture.executor();

        let result = executor.execute(&call("read_symbol", &symbol_args("capture", None)));

        assert!(!result.soft_failure);
        assert!(result.truncated, "record mirrors payload truncation");
        assert_eq!(result.call_id, "call-1");
        let body = payload(&result);
        assert_eq!(body["name"], "capture");
        assert_eq!(body["status"], "found");
        assert_eq!(body["path"], "src/lib.rs");
        assert_eq!(body["line"], 20);
        assert_eq!(body["start_line"], 10);
        assert_eq!(body["end_line"], 209);
        assert_eq!(body["total_lines"], 250);
        assert_eq!(body["truncated"], true);
        assert!(body.get("note").is_none(), "found carries no note");
        let content = body["content"].as_str().expect("content is a string");
        let rendered: Vec<&str> = content.lines().collect();
        assert_eq!(rendered.len(), 200);
        assert_eq!(rendered[0], "10: // filler line 10");
        assert_eq!(rendered[10], "20: pub fn capture(input: &str) -> bool {");
        let candidates = body["candidates"]
            .as_array()
            .expect("candidates is an array");
        assert_eq!(candidates.len(), 1);
        assert_eq!(candidates[0]["path"], "src/lib.rs");
        assert_eq!(candidates[0]["line"], 20);
        assert_eq!(
            candidates[0]["snippet"],
            "pub fn capture(input: &str) -> bool {"
        );
    }

    #[test]
    fn type_and_python_definitions_are_found() {
        let fixture = Fixture::new("symtype");
        fixture.write("src/snap.rs", "pub struct Snapshot {\n    frame: u32,\n}\n");
        fixture.write(
            "snap.py",
            "class Snapshot:\n    def capture(self):\n        pass\n",
        );
        let executor = fixture.executor();

        let result = executor.execute(&call("read_symbol", &symbol_args("Snapshot", None)));

        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "ambiguous");
        let paths: Vec<&str> = body["candidates"]
            .as_array()
            .expect("candidates is an array")
            .iter()
            .map(|hit| hit["path"].as_str().expect("candidate path is a string"))
            .collect();
        assert_eq!(paths, vec!["snap.py", "src/snap.rs"]);

        // Narrowing by file turns each side into a `found`.
        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("Snapshot", Some("src/snap.rs")),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "found");
        assert_eq!(body["path"], "src/snap.rs");
        assert_eq!(body["line"], 1);

        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("capture", Some("snap.py")),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "found");
        assert_eq!(body["line"], 2);
        let content = body["content"].as_str().expect("content is a string");
        assert!(content.contains("2:     def capture(self):"), "{content}");
    }

    #[test]
    fn rust_macro_and_const_bindings_are_found() {
        let fixture = Fixture::new("symbind");
        fixture.write(
            "defs.rs",
            "macro_rules! capture {\n    () => {};\n}\npublic static MAX_CAPTURE: u8 = 9;\n",
        );
        let executor = fixture.executor();

        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("capture", Some("defs.rs")),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "found");
        assert_eq!(body["line"], 1);

        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("MAX_CAPTURE", Some("defs.rs")),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "found");
        assert_eq!(body["line"], 4);
    }

    #[test]
    fn csharp_method_found_while_bare_call_is_not() {
        let fixture = Fixture::new("symsharp");
        fixture.write(
            "Cam.cs",
            "class Cam {\n    public void Capture() {\n    }\n}\n",
        );
        fixture.write("main.cs", "Capture();\n");
        let executor = fixture.executor();

        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("Capture", Some("Cam.cs")),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "found");
        assert_eq!(body["line"], 2);

        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("Capture", Some("main.cs")),
        ));
        assert!(!result.soft_failure);
        assert!(!result.truncated);
        let body = payload(&result);
        assert_eq!(body["status"], "not_found");
        assert_eq!(body["candidates"], serde_json::json!([]));
        assert_eq!(body["truncated"], false);
    }

    #[test]
    fn unknown_symbol_is_a_not_found_success() {
        let fixture = Fixture::new("symmiss");
        fixture.write("a.txt", "nothing to see here\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_symbol", &symbol_args("ghost", None)));

        assert!(!result.soft_failure);
        assert!(!result.truncated);
        assert_eq!(result.call_id, "call-1");
        assert!(result.activated_tool_names.is_empty());
        assert!(result.result_metadata.is_none());
        let body = payload(&result);
        assert_eq!(body["name"], "ghost");
        assert_eq!(body["status"], "not_found");
        assert_eq!(body["candidates"], serde_json::json!([]));
        assert_eq!(body["truncated"], false);
        assert!(body.get("content").is_none(), "not_found carries no window");
        assert!(body.get("note").is_none(), "not_found carries no note");
    }

    #[test]
    fn ambiguous_lists_candidates_and_narrows_with_a_path_hint() {
        let fixture = Fixture::new("symamb");
        fixture.write("a.rs", "fn capture() {}\n");
        fixture.write("b.rs", "fn capture() {}\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_symbol", &symbol_args("capture", None)));

        assert!(!result.soft_failure);
        assert!(!result.truncated);
        let body = payload(&result);
        assert_eq!(body["status"], "ambiguous");
        assert_eq!(
            body["note"],
            "Multiple definition candidates; pass path to narrow."
        );
        assert_eq!(body["truncated"], false);
        assert!(body.get("content").is_none(), "ambiguous carries no window");
        let candidates = body["candidates"]
            .as_array()
            .expect("candidates is an array");
        assert_eq!(candidates.len(), 2);
        assert_eq!(candidates[0]["path"], "a.rs");
        assert_eq!(candidates[0]["line"], 1);
        assert_eq!(candidates[0]["snippet"], "fn capture() {}");
        assert_eq!(candidates[1]["path"], "b.rs");

        let result = executor.execute(&call("read_symbol", &symbol_args("capture", Some("b.rs"))));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "found");
        assert_eq!(body["path"], "b.rs");
        assert_eq!(body["line"], 1);
        assert_eq!(body["start_line"], 1);
        assert_eq!(body["end_line"], 1);
        assert_eq!(body["total_lines"], 1);
        assert_eq!(body["content"], "1: fn capture() {}");
    }

    #[test]
    fn qualified_name_searches_the_last_segment_but_echoes_the_input() {
        let fixture = Fixture::new("symqual");
        fixture.write("snap.rs", "pub struct Snapshot;\nfn capture() {}\n");
        let executor = fixture.executor();

        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("Snapshot::capture", None),
        ));

        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["name"], "Snapshot::capture");
        assert_eq!(body["status"], "found");
        assert_eq!(body["line"], 2);

        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("camera.capture", Some("snap.rs")),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["name"], "camera.capture");
        assert_eq!(body["status"], "found");
        assert_eq!(body["line"], 2);
    }

    #[test]
    fn found_window_is_capped_and_shifted_near_eof() {
        let fixture = Fixture::new("symwin");
        let mut contents: Vec<String> = (1..=250).map(|n| format!("// filler line {n}")).collect();
        contents[149] = "fn capture() {}".to_string();
        fixture.write("big.rs", &contents.join("\n"));
        let half: Vec<String> = (1..=250).map(|n| format!("// tail line {n}")).collect();
        let mut tail = half;
        tail[247] = "fn capture() {}".to_string();
        fixture.write("tail.rs", &tail.join("\n"));
        let mut top: Vec<String> = (1..=250).map(|n| format!("// head line {n}")).collect();
        top[2] = "fn capture() {}".to_string();
        fixture.write("top.rs", &top.join("\n"));
        let executor = fixture.executor();

        // A mid-file definition shifts back to a full 200-line window.
        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("capture", Some("big.rs")),
        ));
        assert!(!result.soft_failure);
        assert!(result.truncated, "record mirrors payload truncation");
        let body = payload(&result);
        assert_eq!(body["status"], "found");
        assert_eq!(body["line"], 150);
        assert_eq!(body["start_line"], 51);
        assert_eq!(body["end_line"], 250);
        assert_eq!(body["total_lines"], 250);
        assert_eq!(body["truncated"], true);
        let rendered: Vec<&str> = body["content"]
            .as_str()
            .expect("content is a string")
            .lines()
            .collect();
        assert_eq!(rendered.len(), 200);
        assert_eq!(rendered[0], "51: // filler line 51");
        assert_eq!(rendered[99], "150: fn capture() {}");

        // A definition near the top starts the window at line 1.
        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("capture", Some("top.rs")),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "found");
        assert_eq!(body["line"], 3);
        assert_eq!(body["start_line"], 1);
        assert_eq!(body["end_line"], 200);
        assert_eq!(body["total_lines"], 250);

        // A definition on line 248 of 250 shifts the window back to a
        // full 200 lines still containing it.
        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("capture", Some("tail.rs")),
        ));
        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "found");
        assert_eq!(body["line"], 248);
        assert_eq!(body["start_line"], 51);
        assert_eq!(body["end_line"], 250);
        assert_eq!(body["total_lines"], 250);
        let rendered: Vec<&str> = body["content"]
            .as_str()
            .expect("content is a string")
            .lines()
            .collect();
        assert_eq!(rendered.len(), 200);
        assert_eq!(rendered[0], "51: // tail line 51");
        assert_eq!(rendered[197], "248: fn capture() {}");
        assert_eq!(rendered[199], "250: // tail line 250");
    }

    #[test]
    fn many_candidates_cap_at_ten_with_truncation() {
        let fixture = Fixture::new("symmany");
        for index in 0..12 {
            fixture.write(&format!("f-{index:02}.rs"), "fn capture() {}\n");
        }
        let executor = fixture.executor();

        let result = executor.execute(&call("read_symbol", &symbol_args("capture", None)));

        assert!(!result.soft_failure);
        assert!(result.truncated, "record mirrors payload truncation");
        let body = payload(&result);
        assert_eq!(body["status"], "ambiguous");
        assert_eq!(body["truncated"], true);
        assert_eq!(
            body["note"],
            "Multiple definition candidates; pass path to narrow."
        );
        let candidates = body["candidates"]
            .as_array()
            .expect("candidates is an array");
        assert_eq!(candidates.len(), 10);
        assert_eq!(candidates[0]["path"], "f-00.rs");
        assert_eq!(candidates[9]["path"], "f-09.rs");
    }

    #[test]
    fn malformed_symbol_requests_are_soft_failures() {
        let fixture = Fixture::new("symbad");
        fixture.write("a.txt", "fn capture() {}\n");
        let executor = fixture.executor();

        for arguments in [
            "not json{{".to_string(),
            r#"{"path": "."}"#.to_string(),
            symbol_args("", None),
            symbol_args("   ", None),
            symbol_args("Snapshot::", None),
            symbol_args("capture", Some("nope.rs")),
            symbol_args("capture", Some("")),
            symbol_args("capture", Some("../outside.txt")),
        ] {
            let result = executor.execute(&call("read_symbol", &arguments));

            assert!(result.soft_failure, "{arguments} must soft-fail");
            assert_eq!(result.call_id, "call-1");
            assert!(!result.truncated);
            assert!(payload(&result).get("error").is_some(), "{arguments}");
        }
    }

    #[test]
    fn whole_token_matching_rejects_longer_identifiers() {
        let fixture = Fixture::new("symtok");
        fixture.write(
            "ids.rs",
            "fn captures() {}\nfn recapture() {}\nlet captured = 1;\n",
        );
        let executor = fixture.executor();

        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("capture", Some("ids.rs")),
        ));

        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["status"], "not_found");
        assert_eq!(body["candidates"], serde_json::json!([]));

        // The longer names themselves are still findable.
        let result = executor.execute(&call(
            "read_symbol",
            &symbol_args("captures", Some("ids.rs")),
        ));
        assert!(!result.soft_failure);
        assert_eq!(payload(&result)["status"], "found");
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

    // -- result cache ---------------------------------------------------------

    fn call_with_id(id: &str, name: &str, arguments_json: &str) -> ToolCallRecord {
        ToolCallRecord {
            call_id: id.to_string(),
            name: name.to_string(),
            arguments_json: arguments_json.to_string(),
            attempt_metadata: None,
            repair_metadata: None,
        }
    }

    #[test]
    fn read_file_cache_hit_returns_original_with_new_call_id() {
        let fixture = Fixture::new("cachehit");
        fixture.write("doc.txt", "version one\n");
        let executor = fixture.executor();

        let first = executor.execute(&call("read_file", &read_args("doc.txt")));
        assert!(!first.soft_failure);
        assert_eq!(payload(&first)["content"], "1: version one");

        // Mutate on disk, then read again with identical effective
        // arguments but a different JSON shape/order and the defaults
        // spelled out explicitly.
        fixture.write("doc.txt", "version two\n");
        let restated =
            serde_json::json!({"end_line": 200, "start_line": 1, "path": "doc.txt"}).to_string();
        let second = executor.execute(&call_with_id("call-2", "read_file", &restated));

        assert!(!second.soft_failure);
        assert_eq!(second.call_id, "call-2");
        assert_eq!(
            payload(&second)["content"],
            "1: version one",
            "the cached record wins over the mutated file"
        );
    }

    #[test]
    fn soft_failures_are_not_cached() {
        let fixture = Fixture::new("cacheneg");
        let executor = fixture.executor();

        let missing = executor.execute(&call("read_file", &read_args("missing.txt")));
        assert!(missing.soft_failure);

        fixture.write("missing.txt", "now here\n");
        let found = executor.execute(&call("read_file", &read_args("missing.txt")));

        assert!(
            !found.soft_failure,
            "a soft failure must not poison the cache"
        );
        assert_eq!(payload(&found)["content"], "1: now here");
    }

    #[test]
    fn read_file_cache_is_per_effective_window() {
        let fixture = Fixture::new("cachewindow");
        fixture.write("w.txt", "one\ntwo\nthree\n");
        let executor = fixture.executor();

        let first = executor.execute(&call("read_file", &read_args("w.txt")));
        assert!(!first.soft_failure);
        assert_eq!(payload(&first)["content"], "1: one\n2: two\n3: three");

        fixture.write("w.txt", "uno\ndos\ntres\n");
        let args = serde_json::json!({"path": "w.txt", "start_line": 2}).to_string();
        let second = executor.execute(&call("read_file", &args));

        assert!(!second.soft_failure);
        assert_eq!(
            payload(&second)["content"],
            "2: dos\n3: tres",
            "a different start_line is a different key and re-reads"
        );
    }

    #[test]
    fn concurrent_cached_reads_share_correct_content() {
        let fixture = Fixture::new("cachethreads");
        fixture.write("a.txt", "alpha\n");
        fixture.write("b.txt", "beta\n");
        let executor: Arc<dyn ToolExecutor + Send + Sync> = Arc::new(fixture.executor());

        std::thread::scope(|scope| {
            let mut handles = Vec::new();
            for index in 0..8 {
                let target = if index % 2 == 0 { "a.txt" } else { "b.txt" };
                let worker = Arc::clone(&executor);
                handles.push(scope.spawn(move || {
                    let result = worker.execute(&call("read_file", &read_args(target)));
                    (target.to_string(), result)
                }));
            }
            for handle in handles {
                let (target, result) = handle.join().expect("worker threads join");
                assert!(!result.soft_failure, "{target} must succeed");
                let expected = if target == "a.txt" {
                    "1: alpha"
                } else {
                    "1: beta"
                };
                assert_eq!(payload(&result)["content"], expected, "{target}");
            }
        });
    }

    #[test]
    fn grep_results_are_cached() {
        let fixture = Fixture::new("cachegrep");
        fixture.write("g.txt", "needle one\n");
        let executor = fixture.executor();

        let first = executor.execute(&call("grep", &grep_args("needle", None, None)));
        assert!(!first.soft_failure);
        assert_eq!(payload(&first)["total_hits"], 1);

        fixture.write("g.txt", "needle one\nneedle two\n");
        let second = executor.execute(&call("grep", &grep_args("needle", None, None)));

        assert!(!second.soft_failure);
        assert_eq!(
            payload(&second)["total_hits"],
            1,
            "the cached hit list wins over the mutated file"
        );
    }

    #[test]
    fn list_dir_results_are_cached() {
        let fixture = Fixture::new("cachelist");
        fixture.write("a.txt", "aye\n");
        let executor = fixture.executor();

        let first = executor.execute(&call("list_dir", &list_args(".")));
        assert!(!first.soft_failure);
        assert_eq!(payload(&first)["total_entries"], 1);

        fixture.write("b.txt", "bee\n");
        let second = executor.execute(&call("list_dir", &list_args(".")));

        assert!(!second.soft_failure);
        assert_eq!(
            payload(&second)["total_entries"],
            1,
            "the cached listing wins over the mutated directory"
        );
    }

    #[test]
    fn read_symbol_results_are_cached() {
        let fixture = Fixture::new("cachesym");
        fixture.write("s.rs", "fn capture() {}\n");
        let executor = fixture.executor();

        let first = executor.execute(&call("read_symbol", &symbol_args("capture", None)));
        assert!(!first.soft_failure);
        assert_eq!(payload(&first)["status"], "found");

        // A second definition would turn a fresh lookup ambiguous; the
        // cached `found` record wins instead.
        fixture.write("s.rs", "fn capture() {}\nfn capture() {}\n");
        let second = executor.execute(&call("read_symbol", &symbol_args("capture", None)));

        assert!(!second.soft_failure);
        assert_eq!(payload(&second)["status"], "found");
        assert_eq!(payload(&second)["line"], 1);
    }

    // -- redaction ------------------------------------------------------------

    #[test]
    fn redactor_covers_secret_shapes() {
        let cases = [
            (
                "OPENAI_API_KEY=sk-abcdefghij1234567890XY",
                "abcdefghij1234567890",
            ),
            ("OPENAI_API_KEY=sk-proj-abcdefghij1234567890XY", "sk-proj-"),
            ("\"client_secret\": \"s3cr3t-value-99\"", "s3cr3t-value-99"),
            ("authToken = 'hunter2-value'", "hunter2"),
            ("service_credential_value=topsecret1", "topsecret1"),
            ("password: hunter2", "hunter2"),
            (
                "Authorization: Bearer abcdef1234567890",
                "abcdef1234567890",
            ),
            ("token = Basic dXNlcjpwYXNz", "dXNlcjpwYXNz"),
            ("aws_key = AKIAIOSFODNN7EXAMPLE", "AKIAIOSFODNN7EXAMPLE"),
            (
                "deploy_key = ghp_abcdefghij1234567890XY",
                "ghp_abcdefghij1234567890",
            ),
            ("ci_token: glpat-abcdefghij1234567890", "glpat-abcdefghij"),
            ("slack = xoxb-123456789012-abcdefghij", "xoxb-123456789012"),
            (
                "jwt eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0In0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c end",
                "SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV",
            ),
            (
                "clone https://deploy:hunter2pw@git.example.com/repo.git now",
                "hunter2pw",
            ),
        ];
        for (input, secret) in cases {
            let redacted = redact_line(input);
            assert!(
                !redacted.contains(secret),
                "{input} must lose its secret: {redacted}"
            );
            assert!(
                redacted.contains("<redacted>"),
                "{input} must carry the replacement token: {redacted}"
            );
        }
        // The URL user survives while the password does not.
        let url = redact_line("clone https://deploy:hunter2pw@git.example.com/repo.git now");
        assert!(url.contains("deploy"), "{url}");
    }

    #[test]
    fn redactor_preserves_private_key_line_count() {
        let key = [
            "-----BEGIN RSA PRIVATE KEY-----",
            "MIIEpAIBAAKCAQEA7bQxabc123",
            "QUJDRA==more-base64",
            "-----END RSA PRIVATE KEY-----",
        ]
        .join("\n");
        let redacted = redact_text(&key);
        let lines: Vec<&str> = redacted.lines().collect();
        assert_eq!(lines.len(), 4, "block redaction preserves line count");
        assert!(lines.iter().all(|line| *line == "<redacted>"), "{redacted}");
        assert!(!redacted.contains("MIIEpAIB"), "{redacted}");

        // An unterminated block redacts through the end (fail closed).
        let unterminated = "header\n-----BEGIN EC PRIVATE KEY-----\nQUJDRA==\ntail\n";
        let redacted = redact_text(unterminated);
        let lines: Vec<&str> = redacted.lines().collect();
        assert_eq!(lines.len(), 4, "{redacted}");
        assert_eq!(lines[0], "header");
        assert!(lines[1..].iter().all(|line| *line == "<redacted>"));
        assert!(!redacted.contains("QUJDRA"), "{redacted}");

        // A lone marker line redacts on its own in single-line mode.
        assert_eq!(
            redact_line("snippet -----BEGIN RSA PRIVATE KEY----- tail"),
            "<redacted>"
        );
    }

    #[test]
    fn redactor_leaves_benign_text_untouched() {
        for benign in [
            "the token count is 3",
            "pub fn tokenize(input) {",
            "commit 0123456789abcdef0123456789abcdef01234567",
            "contact the author of this file",
        ] {
            assert_eq!(redact_line(benign), benign, "{benign} must pass through");
        }
        assert_eq!(
            redact_text("line one\nline two\n"),
            "line one\nline two",
            "plain multi-line text keeps its lines"
        );
    }

    #[test]
    fn tool_results_redact_secrets() {
        let password = "hunter2-value-99";
        let api_secret = "ghp_abcdefghij1234567890XY";
        let fixture = Fixture::new("redacttools");
        fixture.write(
            "leak.rs",
            &format!("fn capture() {{}} // password={password}\napi_key = \"{api_secret}\"\n"),
        );
        let executor = fixture.executor();

        let read = executor.execute(&call("read_file", &read_args("leak.rs")));
        assert!(!read.soft_failure);
        assert!(
            read.result_json.contains("<redacted>"),
            "{}",
            read.result_json
        );
        assert!(!read.result_json.contains(password), "{}", read.result_json);
        assert!(
            !read.result_json.contains(api_secret),
            "{}",
            read.result_json
        );

        let grepped = executor.execute(&call("grep", &grep_args("password", None, None)));
        assert!(!grepped.soft_failure);
        let body = payload(&grepped);
        assert_eq!(body["total_hits"], 1);
        let snippet = body["hits"][0]["snippet"]
            .as_str()
            .expect("snippet is a string");
        assert!(snippet.contains("<redacted>"), "{snippet}");
        assert!(
            !grepped.result_json.contains(password),
            "{}",
            grepped.result_json
        );

        let symbol = executor.execute(&call("read_symbol", &symbol_args("capture", None)));
        assert!(!symbol.soft_failure);
        let body = payload(&symbol);
        assert_eq!(body["status"], "found");
        let content = body["content"].as_str().expect("content is a string");
        assert!(content.contains("<redacted>"), "{content}");
        let candidate = body["candidates"][0]["snippet"]
            .as_str()
            .expect("candidate snippet is a string");
        assert!(candidate.contains("<redacted>"), "{candidate}");
        assert!(
            !symbol.result_json.contains(password),
            "{}",
            symbol.result_json
        );
        assert!(
            !symbol.result_json.contains(api_secret),
            "{}",
            symbol.result_json
        );
    }

    #[test]
    fn redactor_covers_secret_meaning_assignments() {
        let cases = [
            ("master_key = \"topsecret-master-1\"", "topsecret-master-1"),
            (
                "\"databasePassphrase\": \"topsecret-db-2\"",
                "topsecret-db-2",
            ),
            (
                "application_programming_interface_key = \"topsecret-api-3\"",
                "topsecret-api-3",
            ),
            (
                "\"session_id\": \"topsecret-session-4\"",
                "topsecret-session-4",
            ),
            (
                "cookie = \"sessionid=topsecret-cookie-5\"",
                "topsecret-cookie-5",
            ),
            (
                "application programming interface key = \"topsecret-spaced-6\"",
                "topsecret-spaced-6",
            ),
            (
                "encryptionPassphrase = \"topsecret-enc-7\"",
                "topsecret-enc-7",
            ),
        ];
        for (input, secret) in cases {
            let redacted = redact_line(input);
            assert!(
                !redacted.contains(secret),
                "{input} must lose its secret: {redacted}"
            );
            assert!(
                redacted.contains("<redacted>"),
                "{input} must carry the replacement token: {redacted}"
            );
        }
        // Only the value is redacted: the key, any closing quote, and
        // the separator survive verbatim (up to the historical single
        // space after the separator).
        assert_eq!(
            redact_line("master_key = \"topsecret-master-1\""),
            "master_key= <redacted>"
        );
        assert_eq!(
            redact_line("\"session_id\": \"topsecret-session-4\""),
            "\"session_id\": <redacted>"
        );
        assert_eq!(
            redact_line("cookie = \"sessionid=topsecret-cookie-5\""),
            "cookie= <redacted>"
        );
        // A secret assignment nested behind a benign one on the same
        // line is still found: `info` parses first with `session_id`
        // as its value, then the scan re-examines the overlap.
        let nested = redact_line("info: session_id = topsecret-nested-8");
        assert!(!nested.contains("topsecret-nested-8"), "{nested}");
        assert!(nested.contains("<redacted>"), "{nested}");
    }

    #[test]
    fn read_file_redacts_secret_meaning_assignments() {
        let fixture = Fixture::new("redactmeaning");
        fixture.write(
            "config.env",
            "master_key = \"topsecret-master-1\"\n\"session_id\": \"topsecret-session-4\"\nplain line here\n",
        );
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("config.env")));

        assert!(!result.soft_failure);
        assert!(
            !result.result_json.contains("topsecret-master-1"),
            "{}",
            result.result_json
        );
        assert!(
            !result.result_json.contains("topsecret-session-4"),
            "{}",
            result.result_json
        );
        assert!(
            result.result_json.contains("<redacted>"),
            "{}",
            result.result_json
        );
        assert!(
            result.result_json.contains("plain line here"),
            "{}",
            result.result_json
        );
    }

    #[test]
    fn secret_meaning_redaction_is_unicode_safe() {
        // Multi-byte text adjacent to (or inside) an assignment never
        // panics, and a secret value with non-ASCII characters still
        // redacts through the quoted-value shape.
        let redacted = redact_line("🔑 session_id = \"topsecret-unicode-1\"");
        assert!(!redacted.contains("topsecret-unicode-1"), "{redacted}");
        assert!(redacted.contains("<redacted>"), "{redacted}");
        let redacted = redact_line("session_id = \"sécret-unicode-2\"");
        assert!(!redacted.contains("sécret-unicode-2"), "{redacted}");
        assert!(redacted.contains("<redacted>"), "{redacted}");
        // No assignment operator, no identifier-run key: untouched.
        assert_eq!(redact_line("café naïve résumé"), "café naïve résumé");
        assert_eq!(redact_line("token💧 = hunter2"), "token💧 = hunter2");
    }

    // -- FRG-18 slice 6: tool definitions consistency ------------------------

    #[test]
    fn tool_definitions_are_consistent() {
        use agent_core::engine::SessionIoParams;

        // 1. The schema channel parses at the session boundary.
        SessionIoParams::new(
            "openrouter".to_string(),
            200_000,
            TOOLS_SCHEMA_JSON.to_string(),
            "system".to_string(),
        )
        .expect("tool schema parses");

        // 2. The same name set everywhere, with no duplicates.
        let schema: serde_json::Value =
            serde_json::from_str(TOOLS_SCHEMA_JSON).expect("tool schema is JSON");
        let schema_names: Vec<String> = schema
            .as_array()
            .expect("tool schema is an array")
            .iter()
            .map(|entry| {
                entry["name"]
                    .as_str()
                    .expect("schema entry has a name")
                    .to_string()
            })
            .collect();
        let spec_names: Vec<String> = tool_specs()
            .iter()
            .map(|spec| spec.name.as_str().to_string())
            .collect();
        let catalog_names: Vec<String> = tool_catalog_config()
            .tools
            .iter()
            .map(|spec| spec.name.as_str().to_string())
            .collect();
        let tool_names: Vec<String> = TOOL_NAMES.iter().map(|name| name.to_string()).collect();
        for names in [&schema_names, &spec_names, &catalog_names, &tool_names] {
            let mut sorted = names.clone();
            sorted.sort();
            sorted.dedup();
            assert_eq!(sorted.len(), 4, "no duplicate tool names: {names:?}");
        }
        let mut expected = tool_names.clone();
        expected.sort();
        for names in [&schema_names, &spec_names, &catalog_names] {
            let mut sorted = names.clone();
            sorted.sort();
            assert_eq!(sorted, expected, "name set matches TOOL_NAMES");
        }

        // 3. Dispatch covers exactly the schema names: every TOOL_NAMES
        // entry with empty arguments fails softly on the arguments —
        // never as an unknown tool — while a bogus name still does.
        let fixture = Fixture::new("tooldefs");
        let executor = fixture.executor();
        for name in TOOL_NAMES {
            let result = executor.execute(&call(name, "{}"));
            assert!(
                !result.result_json.contains("unknown tool"),
                "{name} dispatches: {}",
                result.result_json
            );
        }
        let bogus = executor.execute(&call("no_such_tool", "{}"));
        assert!(bogus.soft_failure);
        assert!(
            bogus.result_json.contains("unknown tool"),
            "{}",
            bogus.result_json
        );

        // 4. Every entry's required array matches its Args struct, and
        // every entry carries a non-empty description.
        let expected_required = [
            ("list_dir", vec!["path"]),
            ("grep", vec!["pattern"]),
            ("read_file", vec!["path"]),
            ("read_symbol", vec!["name"]),
        ];
        assert_eq!(
            schema.as_array().expect("tool schema is an array").len(),
            expected_required.len(),
            "schema has exactly one entry per tool"
        );
        for (name, required) in expected_required {
            let entry = schema
                .as_array()
                .expect("tool schema is an array")
                .iter()
                .find(|entry| entry["name"] == name)
                .unwrap_or_else(|| panic!("schema has an entry for {name}"));
            let actual: Vec<String> = entry["parameters"]["required"]
                .as_array()
                .unwrap_or_else(|| panic!("{name} parameters has a required array"))
                .iter()
                .map(|value| {
                    value
                        .as_str()
                        .unwrap_or_else(|| panic!("{name} required entries are strings"))
                        .to_string()
                })
                .collect();
            assert_eq!(actual, required, "{name} required matches its Args struct");
            assert!(
                entry["description"]
                    .as_str()
                    .is_some_and(|description| !description.is_empty()),
                "{name} has a non-empty description"
            );
        }
    }

    #[test]
    fn list_dir_redacts_secret_shaped_names() {
        let token = "ghp_abcdefghij1234567890XY";
        let fixture = Fixture::new("redactname");
        fixture.write(&format!("{token}.txt"), "x\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args(".")));

        assert!(!result.soft_failure);
        assert!(
            !result.result_json.contains(token),
            "{}",
            result.result_json
        );
        assert!(
            result.result_json.contains("<redacted>"),
            "{}",
            result.result_json
        );
    }
}
