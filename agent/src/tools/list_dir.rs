use std::fs;

use agent_core::message::{ToolCallRecord, ToolResultRecord};
use serde::{Deserialize, Serialize};

use super::confine::normalize_rel_path;
use super::error::ToolError;
use super::redaction::redact_line;
use super::render_record;
use super::RepoToolExecutor;

/// Maximum entries one `list_dir` call returns, after sorting by name.
const MAX_LIST_ENTRIES: usize = 200;

/// The `list_dir` request shape: `arguments_json` deserializes into
/// this, so a missing `path` (or malformed JSON) is a parse failure.
#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ListDirArgs {
    pub(super) path: String,
}

/// A `list_dir` entry kind as a state space, not a string: the
/// payload serializes it as `file` / `dir` / `symlink` / `other`
/// (ARCHITECTURE §15: statuses are ADTs, never string fields).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
enum EntryKind {
    File,
    Dir,
    Symlink,
    Other,
}

/// One `list_dir` entry: the full repo-relative path plus the entry's
/// own kind (`size` is null for directories).
#[derive(Debug, Serialize)]
struct ListDirEntry {
    path: String,
    kind: EntryKind,
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

impl RepoToolExecutor {
    /// Serve the name-sorted `list_dir` window: at most
    /// [`MAX_LIST_ENTRIES`] entries, with `total_entries` counting the
    /// whole directory. `"."` addresses the snapshot root; a trailing
    /// `/` or leading `./` segments on the input are normalized away
    /// in the payload, and every entry carries its full repo-relative
    /// path so it can feed a later tool call directly. Entry kinds
    /// come from the entry's own file type without following symlinks.
    pub(super) fn list_dir(&self, call: &ToolCallRecord) -> Result<ToolResultRecord, ToolError> {
        self.list_dir_inner(call)
    }

    fn list_dir_inner(&self, call: &ToolCallRecord) -> Result<ToolResultRecord, ToolError> {
        let args: ListDirArgs = serde_json::from_str(&call.arguments_json).map_err(|error| {
            ToolError::InvalidArguments(format!(
                "arguments are not a valid list_dir request: {error}"
            ))
        })?;

        let candidate = self.root.confine(&args.path)?;
        let metadata = fs::metadata(candidate.as_path()).map_err(|error| ToolError::Io {
            path: args.path.clone(),
            message: error.to_string(),
        })?;
        if !metadata.is_dir() {
            return Err(ToolError::NotADirectory {
                path: args.path.clone(),
            });
        }
        let read_dir = fs::read_dir(candidate.as_path()).map_err(|error| ToolError::Io {
            path: args.path.clone(),
            message: error.to_string(),
        })?;
        let mut names: Vec<(String, EntryKind, Option<u64>)> = Vec::new();
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
                EntryKind::Symlink
            } else if file_type.is_dir() {
                EntryKind::Dir
            } else if file_type.is_file() {
                EntryKind::File
            } else {
                EntryKind::Other
            };
            // `DirEntry::metadata` does not traverse symlinks, so a
            // link reports its own length. Directories carry no size.
            // One entry's unreadable metadata must not fail the whole
            // listing, so it degrades to `None`.
            let size = if kind == EntryKind::Dir {
                None
            } else {
                entry.metadata().ok().map(|metadata| metadata.len())
            };
            names.push((entry.file_name().to_string_lossy().into_owned(), kind, size));
        }
        names.sort_by(|left, right| left.0.cmp(&right.0));
        let total_entries = names.len();
        let truncated = total_entries > MAX_LIST_ENTRIES;
        // Entry paths are repository-derived text, so a secret-shaped
        // file name is redacted here before it lands in the payload.
        let listed = redact_line(&normalize_rel_path(&args.path));
        let entries: Vec<ListDirEntry> = names
            .into_iter()
            .take(MAX_LIST_ENTRIES)
            .map(|(name, kind, size)| ListDirEntry {
                path: if listed == "." {
                    redact_line(&name)
                } else {
                    redact_line(&format!("{listed}/{name}"))
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
        render_record(call, &payload, payload.truncated)
    }
}

#[cfg(test)]
mod tests {
    use std::fs;

    use super::super::test_support::*;
    use super::super::ToolExecutor;

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
    #[test]
    fn list_dir_redacts_secret_shaped_names() {
        let token = "ghp_abcdefghij1234567890XY";
        let fixture = Fixture::new("redactname");
        fixture.write(&format!("{token}.txt"), "x\n");
        let executor = fixture.executor();

        let result = executor.execute(&call("list_dir", &list_args(".")));

        assert!(!result.soft_failure);
        assert!(
            !result.result_json.contains(token),
            "{}",
            result.result_json
        );
        assert!(
            result.result_json.contains("<redacted>"),
            "{}",
            result.result_json
        );
    }
}
