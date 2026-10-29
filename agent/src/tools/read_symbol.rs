use std::fs::{self, File};
use std::io::{BufRead, BufReader};

use agent_core::message::{ToolCallRecord, ToolResultRecord};
use serde::{Deserialize, Serialize};

use super::confine::normalize_rel_path;
use super::error::ToolError;
use super::redaction::{redact_line, BlockTracker};
use super::render_record;
use super::soft_failure;
use super::symbol_shape::is_symbol_definition;
use super::RepoToolExecutor;
use super::MAX_READ_LINES;
use super::MAX_SNIPPET_CHARS;

/// Maximum candidates one `read_symbol` call retains, in (path, line)
/// order. `found` always carries exactly one; `ambiguous` carries up
/// to this many while `truncated` reports whether more existed.
const MAX_SYMBOL_CANDIDATES: usize = 10;

/// The `read_symbol` request shape: `arguments_json` deserializes
/// into this, so a missing `name` (or malformed JSON) is a parse
/// failure. `path` defaults to the root when absent.
#[derive(Debug, Deserialize)]
pub(super) struct ReadSymbolArgs {
    pub(super) name: String,
    pub(super) path: Option<String>,
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

impl RepoToolExecutor {
    /// Serve the `read_symbol` definition lookup: classify every line
    /// of the scope with [`is_symbol_definition`], then report one of
    /// three settled outcomes. Exactly one candidate returns a
    /// `found` window of at most [`MAX_READ_LINES`] lines that
    /// includes the definition line (10 lines before it where the
    /// file allows, shifted back when EOF is near so the window stays
    /// full where the file allows); zero candidates is a `not_found`
    /// success; more than one is an `ambiguous` list with no window,
    /// capped at [`MAX_SYMBOL_CANDIDATES`].
    pub(super) fn read_symbol(&self, call: &ToolCallRecord) -> ToolResultRecord {
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
        let candidate = self.root.confine(scope)?;
        let listed = normalize_rel_path(scope);

        // An explicit file scope searches only that file: pruning and
        // lockfile exclusion do not apply, exactly like `grep`. A
        // directory scope reuses the shared scope-walk discipline.
        let rels: Vec<String> = if fs::metadata(candidate.as_path())
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            vec![listed.clone()]
        } else {
            self.walk_scope(candidate.as_path(), &listed)?
        };

        let mut candidates: Vec<SymbolCandidate> = Vec::new();
        for rel in &rels {
            let bytes = fs::read(self.root.join_rel(rel)).unwrap_or_default();
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
                name: redact_line(&args.name),
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
            return render_record(call, &payload, false);
        }

        if candidates.len() > 1 {
            let truncated = candidates.len() > MAX_SYMBOL_CANDIDATES;
            candidates.truncate(MAX_SYMBOL_CANDIDATES);
            let payload = ReadSymbolPayload {
                name: redact_line(&args.name),
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
            return render_record(call, &payload, truncated);
        }

        let found = &candidates[0];
        // Re-read the definition file line by line (the scan above
        // reads whole files like `grep`, but the window render must
        // not pattern-redact the whole file the way the old
        // whole-text redaction did): collect the raw lines, then
        // render the window with block state carried across every
        // line and pattern redaction paid only for returned lines. A
        // NUL byte or an invalid-UTF-8 line anywhere makes the file
        // binary.
        let file = File::open(self.root.join_rel(&found.path)).map_err(|error| ToolError::Io {
            path: found.path.clone(),
            message: error.to_string(),
        })?;
        let mut raw_lines: Vec<String> = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    ToolError::BinaryFile {
                        path: found.path.clone(),
                    }
                } else {
                    ToolError::Io {
                        path: found.path.clone(),
                        message: error.to_string(),
                    }
                }
            })?;
            if line.contains('\0') {
                return Err(ToolError::BinaryFile {
                    path: found.path.clone(),
                });
            }
            raw_lines.push(line);
        }
        let total_lines = raw_lines.len();
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
        let mut tracker = BlockTracker::new();
        let content: String = raw_lines
            .iter()
            .enumerate()
            .filter_map(|(index, line)| {
                let block = tracker.classify(line);
                let number = index + 1;
                if !(start_line..=end_line).contains(&number) {
                    return None;
                }
                let rendered = if block {
                    "<redacted>".to_string()
                } else {
                    redact_line(line)
                };
                Some(format!("{number}: {rendered}"))
            })
            .collect::<Vec<String>>()
            .join("\n");
        let truncated = start_line > 1 || end_line < total_lines;
        let payload = ReadSymbolPayload {
            name: redact_line(&args.name),
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
        render_record(call, &payload, truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::ToolExecutor;

    // -- read_symbol ------------------------------------------------------------

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
}
