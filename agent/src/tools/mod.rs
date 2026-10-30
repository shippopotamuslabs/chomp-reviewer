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
//! path as given, redacted>, "start_line": <requested start>, "end_line": <last
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
//! The settled `grep` payload shape is `{"pattern": <as given,
//! redacted>,
//! "path": <normalized scope path ("." for the root), redacted>, "glob": <as
//! given, redacted, or null>, "hits": [{"path": <repo-relative path>, "line":
//! <1-based line number>, "snippet": <first 200 chars of the line>}],
//! at most 50 in (path, line) order>, "total_hits": <usize>,
//! "truncated": <bool>, "note": "Returned first 50 of T hits. Narrow
//! pattern, glob, or path."}` — where `note` is present only when
//! `truncated` is true. `ToolResultRecord.truncated` mirrors the
//! payload's `truncated`.
//!
//! The settled `read_symbol` payload shape echoes the request as
//! `{"name": <as given, redacted>, "status": "found" | "not_found" |
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
//! `symbol_shape::is_symbol_definition`); the payload echoes the original `name`
//! as given. `ToolResultRecord.truncated` mirrors the payload's
//! `truncated`: true for a `found` window that does not cover the
//! whole file, or an `ambiguous` list that was capped at 10.

mod cache;
mod confine;
mod definitions;
mod error;
mod grep;
mod list_dir;
mod read_file;
mod read_symbol;
mod redaction;
mod symbol_shape;

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use agent_core::message::{ToolCallRecord, ToolResultRecord};
use serde::Serialize;

use self::cache::{cache_key_for, lock_cache, CacheKey};
use self::confine::SnapshotRoot;
use self::redaction::redact_line;

pub use self::definitions::{
    synthesis_tool_catalog_config, synthesis_tool_specs, tool_catalog_config, tool_specs,
    SYNTHESIS_SCHEMA_JSON, SYNTHESIS_TOOL_NAMES, TOOLS_SCHEMA_JSON, TOOL_NAMES,
};
pub use self::error::ToolError;

/// Maximum lines one `read_file` call returns.
const MAX_READ_LINES: usize = 200;

/// Maximum characters of one matching line a `grep` snippet carries.
const MAX_SNIPPET_CHARS: usize = 200;

/// The application port for repository tools — the seam between the
/// host's session driver (FRG-19) and the filesystem, following the
/// [`crate::model_transport::ModelTransport`] precedent. Implementations
/// never panic on untrusted input: every failure surfaces as a
/// soft-failure [`ToolResultRecord`] — the model-facing report form.
/// Host code that needs to *match* a failure instead of showing it to
/// the model uses the typed surface,
/// [`RepoToolExecutor::execute_typed`], which returns [`ToolError`]
/// (ARCHITECTURE §15: typed errors inside the library, reports only
/// at the edge — and at this port, the model is the edge's reader).
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
    root: SnapshotRoot,
    head_sha: String,
    cache: Mutex<HashMap<CacheKey, ToolResultRecord>>,
}
impl RepoToolExecutor {
    /// Confine reads under `root`, canonicalized once here so every
    /// later [`ToolExecutor::execute`] compares canonical paths.
    pub fn new(root: PathBuf, head_sha: String) -> std::io::Result<Self> {
        let root = SnapshotRoot::canonicalize(root)?;
        Ok(Self {
            root,
            head_sha,
            cache: Mutex::new(HashMap::new()),
        })
    }
    /// The canonicalized checkout root reads are confined under.
    pub fn root(&self) -> &Path {
        self.root.as_path()
    }

    /// The head SHA the root was captured at.
    pub fn head_sha(&self) -> &str {
        &self.head_sha
    }
}

impl RepoToolExecutor {
    /// Dispatch a call to its tool without consulting the cache.
    fn dispatch(&self, call: &ToolCallRecord) -> Result<ToolResultRecord, ToolError> {
        match call.name.as_str() {
            "read_file" => self.read_file(call),
            "list_dir" => self.list_dir(call),
            "grep" => self.grep(call),
            "read_symbol" => self.read_symbol(call),
            _ => Err(ToolError::UnknownTool {
                name: call.name.clone(),
            }),
        }
    }

    /// The typed execution surface: like [`ToolExecutor::execute`],
    /// but failures come back as a matchable [`ToolError`] instead of
    /// a soft-failure record. This is the host-side API — telemetry,
    /// policy, and tests match variants here; the record conversion
    /// happens only in the port implementation below, which renders
    /// the failure for the model. Successes are the same
    /// already-redacted records the port returns, cached the same
    /// way; failures are never cached.
    pub fn execute_typed(&self, call: &ToolCallRecord) -> Result<ToolResultRecord, ToolError> {
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
            return Ok(hit);
        }
        let result = self.dispatch(call)?;
        lock_cache(&self.cache).insert(key, result.clone());
        Ok(result)
    }
}

impl ToolExecutor for RepoToolExecutor {
    fn execute(&self, call: &ToolCallRecord) -> ToolResultRecord {
        // The port boundary is the one place typed failures become
        // model-facing reports.
        match self.execute_typed(call) {
            Ok(record) => record,
            Err(error) => soft_failure(call, error),
        }
    }
}

/// Serialize a success payload and stamp its record: the one render
/// tail every tool shares, so the record's `truncated` mirror of the
/// payload is single-sourced. Serialization failure is the typed
/// [`ToolError::Render`] soft failure.
pub(super) fn render_record<T: Serialize>(
    call: &ToolCallRecord,
    payload: &T,
    truncated: bool,
) -> Result<ToolResultRecord, ToolError> {
    let result_json =
        serde_json::to_string(payload).map_err(|error| ToolError::Render(error.to_string()))?;
    Ok(ToolResultRecord {
        call_id: call.call_id.clone(),
        result_json,
        truncated,
        soft_failure: false,
        activated_tool_names: Vec::new(),
        result_metadata: None,
    })
}

/// The error payload shape shared by every soft failure.
#[derive(Debug, Serialize)]
struct ErrorPayload {
    error: String,
}

pub(super) fn soft_failure(call: &ToolCallRecord, error: ToolError) -> ToolResultRecord {
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

/// Shared test helpers for the tool modules: the temp-directory
/// [`Fixture`](test_support::Fixture), the [`call`](test_support::call)
/// constructors, and the per-tool argument builders. Every `tests`
/// module below imports these; nothing here is behavior.
#[cfg(test)]
pub(super) mod test_support {
    use std::fs;
    use std::path::PathBuf;
    use std::time::{SystemTime, UNIX_EPOCH};

    use agent_core::message::{ToolCallRecord, ToolResultRecord};

    use super::RepoToolExecutor;

    /// A temp checkout root cleaned up on drop, in the
    /// `snapshot.rs` test-helper style.
    pub(super) struct Fixture {
        pub(super) dir: PathBuf,
    }

    impl Fixture {
        pub(super) fn new(label: &str) -> Self {
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

        pub(super) fn write(&self, rel: &str, contents: &str) {
            let path = self.dir.join(rel);
            fs::create_dir_all(path.parent().expect("fixture path has a parent"))
                .expect("create fixture dirs");
            fs::write(&path, contents).expect("write fixture file");
        }

        pub(super) fn write_bytes(&self, rel: &str, contents: &[u8]) {
            let path = self.dir.join(rel);
            fs::create_dir_all(path.parent().expect("fixture path has a parent"))
                .expect("create fixture dirs");
            fs::write(&path, contents).expect("write fixture file");
        }

        pub(super) fn executor(&self) -> RepoToolExecutor {
            RepoToolExecutor::new(self.dir.clone(), "abc123".to_string())
                .expect("temp dir canonicalizes")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    pub(super) fn call(name: &str, arguments_json: &str) -> ToolCallRecord {
        ToolCallRecord {
            call_id: "call-1".to_string(),
            name: name.to_string(),
            arguments_json: arguments_json.to_string(),
            attempt_metadata: None,
            repair_metadata: None,
        }
    }

    pub(super) fn read_args(path: &str) -> String {
        serde_json::json!({"path": path}).to_string()
    }

    pub(super) fn list_args(path: &str) -> String {
        serde_json::json!({"path": path}).to_string()
    }

    pub(super) fn payload(result: &ToolResultRecord) -> serde_json::Value {
        serde_json::from_str(&result.result_json).expect("result is JSON")
    }
    pub(super) fn grep_args(pattern: &str, glob: Option<&str>, path: Option<&str>) -> String {
        let mut value = serde_json::json!({"pattern": pattern});
        if let Some(glob) = glob {
            value["glob"] = serde_json::json!(glob);
        }
        if let Some(path) = path {
            value["path"] = serde_json::json!(path);
        }
        value.to_string()
    }
    pub(super) fn symbol_args(name: &str, path: Option<&str>) -> String {
        let mut value = serde_json::json!({"name": name});
        if let Some(path) = path {
            value["path"] = serde_json::json!(path);
        }
        value.to_string()
    }
    pub(super) fn call_with_id(id: &str, name: &str, arguments_json: &str) -> ToolCallRecord {
        ToolCallRecord {
            call_id: id.to_string(),
            name: name.to_string(),
            arguments_json: arguments_json.to_string(),
            attempt_metadata: None,
            repair_metadata: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::test_support::*;
    use super::ToolExecutor;

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
    // -- typed surface ------------------------------------------------------

    #[test]
    fn typed_surface_returns_matchable_errors() {
        use super::ToolError;

        let fixture = Fixture::new("typed");
        let executor = fixture.executor();

        // An existing file outside the root, so the escape is real.
        let outside = fixture.dir.join("../typed-outside.txt");
        std::fs::write(&outside, "outside\n").expect("write outside file");

        // The same call through both surfaces: typed failure for the
        // host, soft-failure record for the model.
        let typed = executor.execute_typed(&call("read_file", &read_args("../typed-outside.txt")));
        assert!(
            matches!(typed, Err(ToolError::PathEscapesRoot { .. })),
            "{typed:?}"
        );
        let port = executor.execute(&call("read_file", &read_args("../typed-outside.txt")));
        assert!(port.soft_failure);
        std::fs::remove_file(&outside).expect("clean outside file");

        let typed = executor.execute_typed(&call("read_file", &read_args("missing.txt")));
        assert!(
            matches!(typed, Err(ToolError::NotFound { .. })),
            "{typed:?}"
        );

        let typed = executor.execute_typed(&call("shell_exec", "{}"));
        assert!(
            matches!(typed, Err(ToolError::UnknownTool { .. })),
            "{typed:?}"
        );

        // Typed successes are the port's successes.
        fixture.write("ok.txt", "fine\n");
        let typed = executor
            .execute_typed(&call("read_file", &read_args("ok.txt")))
            .expect("typed success");
        assert!(!typed.soft_failure);
        assert_eq!(payload(&typed)["content"], "1: fine");
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
