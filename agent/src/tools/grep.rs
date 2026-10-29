use std::fs;
use std::path::{Path, PathBuf};

use agent_core::message::{ToolCallRecord, ToolResultRecord};
use globset::GlobBuilder;
use regex::Regex;
use serde::{Deserialize, Serialize};

use crate::snapshot::{LOCKFILES, PRUNED_DIRS};

use super::confine::normalize_rel_path;
use super::error::ToolError;
use super::redaction::redact_line;
use super::soft_failure;
use super::RepoToolExecutor;
use super::MAX_SNIPPET_CHARS;

/// Maximum hits one `grep` call retains, in (path, line) order.
const MAX_GREP_HITS: usize = 50;

/// The `grep` request shape: `arguments_json` deserializes into
/// this, so a missing `pattern` (or malformed JSON) is a parse
/// failure. `path` defaults to the root when absent; `glob` defaults
/// to matching every walked file.
#[derive(Debug, Deserialize)]
pub(super) struct GrepArgs {
    pub(super) pattern: String,
    pub(super) glob: Option<String>,
    pub(super) path: Option<String>,
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

/// The file name of a repo-relative `path` built with `/`
/// separators: everything after the final `/`, or the whole path.
fn file_name_of(path: &str) -> &str {
    path.rsplit('/').next().unwrap_or(path)
}

impl RepoToolExecutor {
    /// Serve the `grep` regex search: one hit per matching line, at
    /// most [`MAX_GREP_HITS`] retained in (path, line) order, with the
    /// true count visible through `total_hits` and `truncated`.
    pub(super) fn grep(&self, call: &ToolCallRecord) -> ToolResultRecord {
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
        let candidate = self.root.confine(scope)?;
        let listed = normalize_rel_path(scope);

        // An explicit file scope searches only that file: pruning and
        // lockfile exclusion do not apply, but the glob filter still
        // does. A directory scope is walked recursively.
        let rels: Vec<String> = if fs::metadata(candidate.as_path())
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
            self.walk_grep_scope(candidate.as_path(), &listed)?
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
            let bytes = fs::read(self.root.join_rel(rel)).unwrap_or_default();
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
    pub(super) fn walk_grep_scope(
        &self,
        scope: &Path,
        scope_rel: &str,
    ) -> Result<Vec<String>, ToolError> {
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
                        .map(|target| target.starts_with(self.root.as_path()))
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

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::ToolExecutor;

    // -- grep ---------------------------------------------------------------------

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
}
