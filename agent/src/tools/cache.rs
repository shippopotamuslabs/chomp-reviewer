use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use agent_core::message::{ToolCallRecord, ToolResultRecord};

use super::confine::normalize_rel_path;
use super::grep::GrepArgs;
use super::list_dir::ListDirArgs;
use super::read_file::{effective_read_window, ReadFileArgs};
use super::read_symbol::ReadSymbolArgs;

/// Recover the cache guard without panicking: a poisoned mutex still
/// holds a usable map, and a cache must never fail a tool call.
pub(super) fn lock_cache<'a>(
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
pub(super) enum CacheKey {
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
/// `read_file` window uses
/// [`effective_read_window`](super::read_file::effective_read_window),
/// paths and scopes use
/// [`normalize_rel_path`](super::confine::normalize_rel_path), and the
/// `grep` / `read_symbol`
/// scopes default to `"."` exactly as the inners do.
pub(super) fn cache_key_for(head_sha: &str, call: &ToolCallRecord) -> Option<CacheKey> {
    match call.name.as_str() {
        "read_file" => {
            let args: ReadFileArgs = serde_json::from_str(&call.arguments_json).ok()?;
            if args.path.is_empty() {
                return None;
            }
            let (start, end) = effective_read_window(args.start_line, args.end_line)?;
            Some(CacheKey::ReadFile {
                head_sha: head_sha.to_string(),
                path: normalize_rel_path(&args.path),
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
                path: normalize_rel_path(&args.path),
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
                scope: normalize_rel_path(raw_scope),
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
                scope: normalize_rel_path(raw_scope),
            })
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use super::super::test_support::*;
    use super::super::ToolExecutor;

    // -- result cache ---------------------------------------------------------

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
    fn read_file_cache_normalizes_path_spelling() {
        let fixture = Fixture::new("cachespell");
        fixture.write("doc.txt", "version one\n");
        let executor = fixture.executor();

        let first = executor.execute(&call("read_file", &read_args("./doc.txt")));
        assert!(!first.soft_failure);

        fixture.write("doc.txt", "version two\n");
        let second = executor.execute(&call("read_file", &read_args("doc.txt")));

        assert!(!second.soft_failure);
        assert_eq!(
            payload(&second)["content"],
            "1: version one",
            "./doc.txt and doc.txt share one cache entry"
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
}
