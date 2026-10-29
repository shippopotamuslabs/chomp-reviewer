use std::sync::OnceLock;

use regex::Regex;

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
pub(super) fn redact_line(line: &str) -> String {
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
pub(super) fn redact_text(text: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::super::test_support::*;
    use super::super::ToolExecutor;
    use super::{redact_line, redact_text};

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
}
