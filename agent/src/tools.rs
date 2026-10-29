//! Read-only repository tools for lens sessions (FRG-18, slices 1-2).
//!
//! Slice scope: the host port trait ([`ToolExecutor`]), root confinement,
//! and the `read_file` and `list_dir` tools only, served by
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
//! Explicitly NOT in this slice: `grep` / `read_symbol` (dispatched to
//! "tool not implemented yet" soft failures), the result cache,
//! redaction, tool schemas / `ToolSpec`s, and globset matching — those
//! land in later FRG-18 slices. This executor must not be wired into
//! sessions until redaction lands.

use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

use agent_core::message::{ToolCallRecord, ToolResultRecord};
use serde::{Deserialize, Serialize};

/// Maximum lines one `read_file` call returns.
const MAX_READ_LINES: usize = 200;

/// Maximum entries one `list_dir` call returns, after sorting by name.
const MAX_LIST_ENTRIES: usize = 200;

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
}

impl ToolExecutor for RepoToolExecutor {
    fn execute(&self, call: &ToolCallRecord) -> ToolResultRecord {
        match call.name.as_str() {
            "read_file" => self.read_file(call),
            "list_dir" => self.list_dir(call),
            "grep" | "read_symbol" => soft_failure(
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

    // -- dispatch ---------------------------------------------------------------------

    #[test]
    fn unimplemented_tool_is_a_soft_failure() {
        let fixture = Fixture::new("unimpl");
        let executor = fixture.executor();

        for name in ["grep", "read_symbol"] {
            let result = executor.execute(&call(name, &read_args("x")));
            assert!(result.soft_failure, "{name} must soft-fail");
            assert_eq!(result.call_id, "call-1");
            assert!(!result.truncated);
            assert!(result.activated_tool_names.is_empty());
            assert!(result.result_metadata.is_none());
            let body = payload(&result);
            assert_eq!(body["error"], format!("tool not implemented yet: {name}"));
        }
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
