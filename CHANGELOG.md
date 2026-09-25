# Changelog

## v1.0.10 (2026-09-24)

- Fix: scope `verify.py` evidence to the finding's cited files instead of a flat 8000-char head-truncation. Large MR diffs dropped whole files from the verification prompt (a 74k-char diff showed only the first 8k, covering app/ files but no domain files), so the LLM verdicted NOT_ADDRESSED with "diff does not touch X" for fixes that were present. `select_diff_for_finding()` parses `path[N-M]` Fix Focus Areas entries and `[path[LN-M]](url)` Code links, passes those whole file sections (cited line numbers go stale after pushes and never filter hunks), marks any remaining truncation explicitly instead of cutting silently, and keeps legacy head-truncation only when no files are cited. Adds `tests/test_verify_diff_scoping.py` regression coverage.

## v1.0.9 (2026-09-23)

- Fix: skip unresolvable discussions in `verify.py` selection. `get_forge_discussions()` included the standalone rollup note (posted via POST `/notes`, `individual_note=true`, not resolvable), so the first ADDRESSED verdict attempted `PUT {"resolved": true}` on it and failed with `403 Forbidden` — and `gitlab_request`'s `sys.exit(1)` aborted verification of the remaining discussions. Selection now skips individual notes and threads GitLab marks `resolvable: false`; resolution uses non-fatal requests so one failure logs ERROR and the run continues through all discussions (exiting 1 at the end if any resolve failed). Extends `tests/test_verify_resolve.py` regression coverage.

## v1.0.8 (2026-09-22)

- Fix: resolve addressed discussions via the correct GitLab endpoint. `verify.py` used `PUT .../merge_requests/:iid/discussions/:id/resolve`, which does not exist, so every ADDRESSED verdict failed with `404 Not Found` and no discussion was ever resolved. Resolution now uses `PUT .../merge_requests/:iid/discussions/:discussion_id` with `{"resolved": true}` via a new `resolve_discussion()` helper. Adds `tests/test_verify_resolve.py` regression coverage.

## v1.0.7 (2026-09-22)

- Fix: post every finding as an inline discussion. `forge:master` positioned each comment with a single-shot `position[new_line]=line_start` and silently dropped any finding GitLab rejected (WARNING only) — deleted lines, lines outside a hunk, or mismatched paths never appeared inline even though the rollup Guide listed them. `master.py` now parses `mr.diff` into per-file valid diff-line maps, resolves each finding to the nearest valid line (in-range lines first, `old_line` anchoring for deletion-only files, cross-file fallback for unknown paths so a comment is always inline), and retries across candidate positions instead of dropping the finding. Relocated comments carry an invisible `forge:anchor` note with the originally flagged location, and `forge-recorded.json` now records the resolved anchor plus any failures. The dedup prompt requires diff-grounded line numbers. Adds `tests/test_master_position.py` regression coverage.

## v1.0.6 (2026-09-22)

- Fix: preserve trailing blank context lines in `forge:diff` assembly. `fetch_diff.py` trimmed each per-file fragment with `.strip()`, dropping a file diff's final blank context line (`' '`), which left the hunk one body line short and crashed `forge:review` in pr-agent with `UnidiffParseError: Hunk diff line expected: diff --git ...`. Fragments now pass through byte-for-byte apart from the prepended header (only newline padding is trimmed). Adds `tests/test_fetch_diff.py` regression coverage.
- Chore: untrack compiled bytecode (`.forge/__pycache__/*.pyc`) and ignore `__pycache__/`.

## v1.0.5 (2026-09-21)

- Fix: include `verify.py` in the `.forge-setup` download list (was missing, causing `forge:master` to fail on verification runs).

## v1.0.4 (2026-09-21)

- Feat: add `verify.py` for AI-based discussion resolution on subsequent pushes. `forge:master` now checks for an existing consolidated review; if found, it runs verification (LLM determines ADDRESSED/NOT_ADDRESSED for each open Forge discussion, resolves addressed ones) instead of re-posting the full review. Eliminates reviewer churn.

## v1.0.3 (2026-09-21)

- Fix: set `OPENROUTER__KEY` (dynaconf) and `CONFIG__MODEL` for PR-Agent; without these it fell back to GPT-5/dummy_token.

## v1.0.2 (2026-09-21)

- Fix: add missing `review` command to `pr_agent.cli` invocation (was failing with "arguments are required: command, rest").

## v1.0.1 (2026-09-21)

- Fix: single-pass placeholder substitution in `thermos.py` and `master.py`. The chained `.replace()` could corrupt prompts if a diff or findings contained a placeholder string (e.g. `{file_contents}`).
- Fix: pin `pr-agent==0.46.0` in a venv (was unpinned `pip install pr-agent` with `--break-system-packages`).

## v1.0.0 (2026-09-21)

Initial release. Extracted from the Stratum project's CI-native Forge implementation.

- `forge:diff` — fetches MR diff once via GitLab API, shares as artifact
- `forge:review` — PR-Agent (PyPI, unmodified) as silent producer in plain-diff mode
- `forge:thermos-correctness` — OpenRouter LLM correctness/security lens
- `forge:thermos-structure` — OpenRouter LLM structure/maintainability lens
- `forge:master` — LLM semantic dedup, posts one consolidated review
- Review-once guard: skips if a consolidated review already exists
- Shared `.forge/openrouter_client.py` (no duplicated API code)
- `.forge/fetch_diff.py` (no inline Python in YAML)
- Thermos jobs deduplicated via `.forge-thermos-base`
- `FORGE_ENABLED` opt-in flag, `FORGE_MODEL`/`FORGE_REF` overrides
