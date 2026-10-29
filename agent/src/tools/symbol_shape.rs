//! The `read_symbol` line-classification engine, split from the
//! executor orchestration in `read_symbol.rs`: pure line-shape
//! heuristics (no I/O), shared across languages. [`is_symbol_definition`]
//! is the entry point; everything else is its machinery.

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
pub(super) fn is_symbol_definition(line: &str, needle: &str) -> bool {
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
