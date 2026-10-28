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
