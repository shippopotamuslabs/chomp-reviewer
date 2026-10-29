use std::fmt;

/// Typed tool failures. Every failure path lands in one of these
/// variants; the trait boundary converts each into a soft-failure
/// `ToolResultRecord` whose `result_json` is `{"error": "<display>"}`.
#[derive(Debug)]
pub(crate) enum ToolError {
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
