use std::fs::{self, File};
use std::io::{BufRead, BufReader};

use agent_core::message::{ToolCallRecord, ToolResultRecord};
use serde::{Deserialize, Serialize};

use super::error::ToolError;
use super::redaction::{redact_line, BlockTracker};
use super::render_record;
use super::RepoToolExecutor;
use super::MAX_READ_LINES;

/// The `read_file` request shape: `arguments_json` deserializes into
/// this, so a missing `path` (or malformed JSON) is a parse failure.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ReadFileArgs {
    pub(super) path: String,
    pub(super) start_line: Option<usize>,
    pub(super) end_line: Option<usize>,
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

/// The canonical `read_file` window: `start_line` defaults to 1,
/// `end_line` defaults to `start_line + 199`, always clamped to at most
/// [`MAX_READ_LINES`] lines. Returns `None` for an invalid window — a
/// zero `start_line`, or an `end_line` that is zero or below `start` —
/// which dispatch reports as a soft failure. Shared by
/// [`RepoToolExecutor::read_file_inner`] and
/// [`cache_key_for`](super::cache::cache_key_for) so the
/// two can never drift apart.
pub(super) fn effective_read_window(
    start: Option<usize>,
    end: Option<usize>,
) -> Option<(usize, usize)> {
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

impl RepoToolExecutor {
    /// Serve `read_file`: a numbered window of one snapshot text
    /// file. Lines are 1-based; `start_line` defaults to 1 and
    /// `end_line` defaults to `start_line + 199`, always clamped to at
    /// most [`MAX_READ_LINES`] lines. Reading past EOF (and empty
    /// files) is a success with empty content. The file is streamed
    /// line by line: every line is counted and fed to the redaction
    /// block tracker, but only the returned window's lines pay the
    /// pattern-redaction cost or are held in memory.
    pub(super) fn read_file(&self, call: &ToolCallRecord) -> Result<ToolResultRecord, ToolError> {
        self.read_file_inner(call)
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

        let candidate = self.root.confine(&args.path)?;
        let metadata = fs::metadata(candidate.as_path()).map_err(|error| ToolError::Io {
            path: args.path.clone(),
            message: error.to_string(),
        })?;
        if !metadata.is_file() {
            return Err(ToolError::NotAFile {
                path: args.path.clone(),
            });
        }
        let file = File::open(candidate.as_path()).map_err(|error| ToolError::Io {
            path: args.path.clone(),
            message: error.to_string(),
        })?;
        // Stream the file once: count every line (a NUL byte or an
        // invalid-UTF-8 line anywhere makes the file binary, exactly
        // as the whole-file read judged it), carry private-key block
        // state across the whole file, and keep only the window.
        let mut tracker = BlockTracker::new();
        let mut total_lines: usize = 0;
        let mut window: Vec<String> = Vec::new();
        for line in BufReader::new(file).lines() {
            let line = line.map_err(|error| {
                if error.kind() == std::io::ErrorKind::InvalidData {
                    ToolError::BinaryFile {
                        path: args.path.clone(),
                    }
                } else {
                    ToolError::Io {
                        path: args.path.clone(),
                        message: error.to_string(),
                    }
                }
            })?;
            if line.contains('\0') {
                return Err(ToolError::BinaryFile {
                    path: args.path.clone(),
                });
            }
            total_lines += 1;
            let block = tracker.classify(&line);
            if (start..=end).contains(&total_lines) {
                window.push(if block {
                    "<redacted>".to_string()
                } else {
                    redact_line(&line)
                });
            }
        }
        // `end_line` is the 1-based number of the final returned line;
        // `start - 1` when nothing is returned (start past EOF, or an
        // empty file). `truncated` is true whenever the file holds more
        // lines than that — including a small requested subwindow.
        let (content, end_line) = if start > total_lines {
            (String::new(), start - 1)
        } else {
            let numbered: Vec<String> = window
                .iter()
                .enumerate()
                .map(|(index, line)| format!("{}: {line}", start + index))
                .collect();
            (numbered.join("\n"), end.min(total_lines))
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
            path: redact_line(&args.path),
            start_line: start,
            end_line,
            total_lines,
            content,
            truncated,
            note,
        };
        render_record(call, &payload, truncated)
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::ToolExecutor;

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
    fn unknown_argument_fields_are_refused() {
        // The published schema says additionalProperties: false; the
        // DTO enforces it, so a misspelled argument fails closed
        // instead of being silently ignored (ARCHITECTURE §15: DTOs
        // state an unknown-field policy).
        let fixture = Fixture::new("unknownfield");
        fixture.write("a.txt", "one\n");
        let executor = fixture.executor();

        let args = serde_json::json!({"path": "a.txt", "startline": 1}).to_string();
        let result = executor.execute(&call("read_file", &args));

        assert!(result.soft_failure);
        assert!(payload(&result).get("error").is_some());
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

    // -- regression: review fixes --------------------------------------

    #[test]
    fn trailing_blank_line_is_a_counted_line() {
        let fixture = Fixture::new("blankline");
        fixture.write("a.txt", "a\n\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args("a.txt")));

        assert!(!result.soft_failure);
        let body = payload(&result);
        assert_eq!(body["total_lines"], 2);
        assert_eq!(body["end_line"], 2);
        assert_eq!(body["content"], "1: a\n2: ");
    }

    #[test]
    fn key_block_starting_before_the_window_still_redacts_inside_it() {
        let fixture = Fixture::new("blockwindow");
        let mut lines: Vec<String> = (1..=210).map(|n| format!("line {n}")).collect();
        lines[198] = "-----BEGIN RSA PRIVATE KEY-----".to_string();
        lines[199] = "SECRETBODYLINE".to_string();
        lines[200] = "-----END RSA PRIVATE KEY-----".to_string();
        lines[201] = "after".to_string();
        fixture.write("key.txt", &lines.join("\n"));
        let executor = fixture.executor();

        let args = serde_json::json!({"path": "key.txt", "start_line": 200}).to_string();
        let result = executor.execute(&call("read_file", &args));

        assert!(!result.soft_failure);
        assert!(
            !result.result_json.contains("SECRETBODYLINE"),
            "{}",
            result.result_json
        );
        let body = payload(&result);
        let content = body["content"].as_str().expect("content is a string");
        assert!(content.contains("200: <redacted>"), "{content}");
        assert!(content.contains("201: <redacted>"), "{content}");
        assert!(content.contains("202: after"), "{content}");
    }

    #[test]
    fn secret_shaped_file_name_is_redacted_in_the_payload_path() {
        let token = "ghp_abcdefghij1234567890XY";
        let fixture = Fixture::new("tokenname");
        fixture.write(&format!("{token}.txt"), "benign\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("read_file", &read_args(&format!("{token}.txt"))));

        assert!(!result.soft_failure);
        assert!(
            !result.result_json.contains(token),
            "{}",
            result.result_json
        );
        assert_eq!(payload(&result)["path"], "<redacted>.txt");
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
}
