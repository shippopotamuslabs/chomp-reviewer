# FRG-17 — Snapshot and repo map (plan)

Ticket: `agent/src/snapshot.rs` — capture the workflow's existing PR-head
checkout as the run's snapshot: record the head SHA, build the pruned
repo map, parse the changed-file list from the diff. (§9 snapshot
integrity: all reads occur under the recorded head SHA; consumers are
the lens sessions §8.5, the synthesizer §8.8, and the PR-Agent adapter.)

## Design — agent/src/snapshot.rs (new), exposed via lib.rs

- `Snapshot::capture(repo_root: &Path, diff_path: &Path) ->
  Result<Snapshot, SnapshotError>`:
  - **Head SHA:** `git -C <root> rev-parse HEAD` (subprocess). Works on
    a depth-1 shallow checkout; absence of history is never an error.
    Failure (not a git checkout, git missing) = typed error.
  - **Repo map:** recursive std-only walk, files only, repo-relative
    forward-slash paths, sizes from metadata, sorted by path for
    determinism. Pruning (named, tested): directories `.git`, `target`,
    `build`, `node_modules`, `vendor`, `.gradle`, `__pycache__` are not
    descended; lockfiles are excluded as files (`Cargo.lock`,
    `package-lock.json`, `yarn.lock`, `pnpm-lock.yaml`, `Gemfile.lock`,
    `poetry.lock`, `Pipfile.lock`, `go.sum`, `composer.lock`). Symlinked
    directories are not followed. Everything else — including `.github/`
    and dotfiles — is content.
  - **Bound:** the map caps at `MAX_REPO_MAP_ENTRIES` (10,000) entries;
    the walk still counts every file, and `RepoMap` records
    `total_files` + `truncated: bool` so an over-cap repo is visible,
    never silently short. (Tensor's call; cheap to change.)
  - **Changed files:** parsed from the unified diff at `diff_path`:
    `diff --git` headers supply paths; `new file mode` / `--- /dev/null`
    -> Added; `deleted file mode` / `+++ /dev/null` -> Deleted;
    `rename from/to` -> Renamed { from, to }; otherwise Modified.
    Binary diffs (headers only) -> Modified. Order = diff order.
  - **SHA stamping:** `RepoMap` carries `head_sha` in its struct and its
    serialization — the artifact this stage emits is stamped. Findings /
    decision-log stamping lands with the driver that writes them
    (FRG-19+); `Snapshot.head_sha` is the single source they will read.
- Types: `Snapshot { head_sha, root, repo_map, changed_files }`,
  `RepoMap { head_sha, entries: Vec<RepoEntry>, total_files, truncated }`,
  `RepoEntry { path, size }`, `ChangedFile { path, change }`,
  `ChangeKind::{Added, Modified, Deleted, Renamed { from }}` — all
  serde-serializable (consumers pass the map as session context).
  Also a plain-text `RepoMap::render()` (one `path (size)` line per
  entry) for prompt-shaped consumption.
- `SnapshotError`: typed variants (NotAGitCheckout, GitUnavailable,
  Io { path, message }, DiffParse context where relevant) — no panics,
  no silent skips (an unreadable file mid-walk is an error, not an
  omission: a partial map presented as complete would mislead lenses).

## No port trait (deliberate)

Unlike the transport (FRG-16), capture is directly testable against
fixture directory trees — the filesystem is the seam, exactly like the
tool executor's root path in FRG-18. If the FRG-19 driver wants a trait
here, it is purely additive then.

## Tests (fixture-driven, no network)

- Fixture tree in a temp dir: ordinary sources included with correct
  sizes; every pruned category present and excluded (`.git` dir,
  `target/`, `node_modules/`, `vendor/`, a lockfile at root and nested);
  `.github/workflows/x.yml` included; deterministic sorted order.
- Real git fixture (`git init` + one commit, configured via `-c`
  flags): `head_sha` equals `git rev-parse HEAD` output; capture works
  with no further history.
- Diff fixture string: modified + added + deleted + renamed + binary
  entries -> the exact `changed_files` list, in order.
- Over-cap behavior: synthesize >cap tiny files cheaply OR set the cap
  via a capture_with_limit internal seam the test can drive — the test
  must prove `truncated` flips and `total_files` stays exact.
- Error paths: non-git directory -> NotAGitCheckout; missing diff file
  -> typed error.

## Non-goals

- Wiring capture into `chomp review` / the driver (FRG-19), the
  base-branch contract channel (FRG-22 / FG-6), workflow-side base fetch
  (FG-14), blame/history anything (NG7).
