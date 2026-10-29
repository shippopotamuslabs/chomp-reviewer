use std::fs;

use agent_core::message::{ToolCallRecord, ToolResultRecord};
use serde::{Deserialize, Serialize};

use super::confine::normalize_rel_path;
use super::error::ToolError;
use super::redaction::{redact_line, redact_text};
use super::soft_failure;
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
        // directory scope reuses the `grep` walk discipline.
        let rels: Vec<String> = if fs::metadata(candidate.as_path())
            .map(|metadata| metadata.is_file())
            .unwrap_or(false)
        {
            vec![listed.clone()]
        } else {
            self.walk_grep_scope(candidate.as_path(), &listed)?
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
        let bytes = fs::read(self.root.join_rel(&found.path)).map_err(|error| ToolError::Io {
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
