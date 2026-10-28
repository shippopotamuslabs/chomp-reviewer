# FRG-17 — status

- State: implemented, all gates green (2026-10-28) — ready for review / PR
- Branch: shippopotamus/frg-17-snapshot (forge-reviewer)

## What was built

`agent/src/snapshot.rs` (replaces the FRG-15 scaffold stub; registered
as `pub mod snapshot;` in `agent/src/lib.rs`):

- `Snapshot::capture(repo_root, diff_path)` — head SHA via
  `git -C <root> rev-parse HEAD` (spawn failure → `GitUnavailable`,
  non-zero exit → `NotAGitCheckout` with git's stderr), pruned repo
  map, changed files parsed from the diff; `Snapshot.head_sha` is the
  stamp source for FRG-19+.
- `RepoMap { head_sha, entries, total_files, truncated }` — the SHA
  stamp is in the struct and its serialization. Walk: files only,
  repo-relative forward-slash paths, sorted by path; prunes `.git`,
  `target`, `build`, `node_modules`, `vendor`, `.gradle`,
  `__pycache__` by name at any depth and the nine named lockfiles;
  symlinked directories not followed, symlinked files recorded with
  target size; any unreadable entry (incl. non-UTF-8 file names) is a
  typed `Io` error, never a silent omission. Plus `RepoMap::render()`
  (`path (size)` per line).
- Diff parsing: `diff --git` headers supply paths (C-style quoting on
  either side handled); markers read only from each section's header
  block (before the first `@@`) so hunk content cannot mimic them;
  rename → `ChangeKind::Renamed { from }` with the new path on
  `ChangedFile.path`; binary-with-no-markers → Modified; order = diff
  order; an unparseable header is a typed `DiffParse` error.
- All data types serde-serializable (round-trip tested).

## Truncation seam

`Snapshot::capture_with_limit(root, diff, limit)` — a private
associated function; `capture` delegates with `MAX_REPO_MAP_ENTRIES`
(10,000). The walk collects every counted file, sorts, then truncates,
so surviving entries are the first `limit` in sorted order (fully
deterministic) and `total_files` stays exact. The over-cap test drives
the seam with limit 2 on a 5-file git fixture — no 10,000-file
synthesis needed.

## Notes / deviations from the plan

- The plan's file already existed as a 3-line scaffold stub declared in
  the binary's module tree (`main.rs`), so the module now compiles
  under both lib and bin targets and its 11 unit tests run in both.
  Because nothing in the binary calls it until FRG-19, the bin target
  raised dead-code warnings; snapshot.rs carries a commented,
  module-level `#![allow(dead_code)]` marked for removal with FRG-19
  (the same temporary allowance FRG-16 carried). `main.rs` was out of
  scope for this ticket, so the FRG-16-style lib/bin collapse was not
  repeated here.
- Otherwise the plan matched reality throughout; no design changes.

## Gates (2026-10-28)

- `cargo fmt --check`: clean
- `cargo clippy --all-targets -- -D warnings`: clean
- `cargo test --locked`: 63 passed, 0 failed — lib 32 (21 transport +
  11 snapshot), bin 23 (12 + 11 snapshot), cli integration 5,
  transport loop 3
- `cargo tree | grep -i uniffi`: empty
- Python suite (repo root): 159 tests, OK (4 skipped)

## Next

Review, then PR. Wiring into `chomp review` stays with FRG-19 as
planned.

## Orchestrator follow-up (post-implementation)

Applied the FRG-16/PR-#17 collapse to the dual-target wart the
implementation record flagged: `mod snapshot;` removed from main.rs
(nothing in the bin referenced it), so snapshot compiles only in the lib
target, its 11 tests run once, and the temporary module-level
`#![allow(dead_code)]` came out. Re-verified: cargo 52/0 (lib 32, bin 12,
cli 5, loop 3), fmt + clippy clean.

## Review round 1 fixes (2026-10-28)

- **MEDIUM — git's C-style quoting is now decoded for real.**
  `take_quoted` no longer collapses every escape to the escaped char
  (which turned `\303` into the literal digit `3`, silently mangling
  non-ASCII paths). It decodes into a byte buffer: `\NNN` (exactly
  three octal digits forming a byte, first digit 0–3) → that byte;
  `\t` / `\n` → their control characters; any other `\<char>` → that
  char. The decoded bytes are then read as UTF-8 via
  `String::from_utf8`; an invalid sequence is a typed
  `SnapshotError::DiffParse`, never the old lossy replacement. To make
  that error reachable, `take_quoted`, `split_diff_git_paths`,
  `parse_diff_git_paths`, and `unquote_path` now return `Result`
  (previously `Option` / raw-string fallback — the rename path used to
  fall back to the quoted raw text, recording a path the tree does not
  contain). Tests: `octal_quoted_non_ascii_paths_decode_to_utf8`
  (café.md octal-escaped on the `diff --git` side and on a rename
  from/to side, both decoding to the true UTF-8 paths),
  `quoted_escapes_decode_to_their_characters`, and
  `quoted_path_with_invalid_utf8_is_a_typed_error` (`\377` on both the
  header and rename sides).
- **LOW — headline parser property now has its regression test.** The
  shared diff fixture's first hunk carries content lines that mimic
  header markers (a removed line whose text is `-- /dev/null`, an
  added line reading `+new file mode 100644`); the exact-list test
  asserts src/main.rs still classifies Modified, proving markers are
  read only from the header block before the first `@@`.
- **LOW — dangling symlinks no longer abort capture (conscious
  deviation from the plan).** The plan's line was blanket: "an
  unreadable file mid-walk is an error". A dangling symlink is not an
  unreadable *entry* — the link itself is present in the tree and its
  target may legitimately live outside the checkout, so aborting the
  whole snapshot over it would be wrong. In the walk's symlink branch,
  an `fs::metadata` failure with `ErrorKind::NotFound` now records the
  entry at its own `symlink_metadata` size (lockfile exclusion still
  applies); any other failure (permission, link loop, ...) remains a
  typed `Io` error. Documented in the module docs next to the
  no-silent-skips rule. Test:
  `dangling_symlinks_are_recorded_at_link_size`.

## Gates (after review round 1, 2026-10-28)

- `cargo fmt --check`: clean
- `cargo clippy --all-targets -- -D warnings`: clean
- `cargo test --locked`: 56 passed, 0 failed — lib 36 (32 + 4 new
  round-1 tests), bin 12, cli integration 5, transport loop 3
- `cargo tree | grep -i uniffi`: empty
- Python suite (repo root): 159 tests, OK (4 skipped)
