# Changelog

## v1.1.8 (2026-10-29)

- Chore: bump the pinned PR-Agent from 0.46.0 to 0.49.0 (FRG-23). The reusable workflow's `pr_agent_version` input now defaults to `0.49.0` and installs `pr-agent[github]==…`; the GitLab template installs `pr-agent[gitlab]==0.49.0` (0.48 split provider dependencies into pip extras, so a bare install no longer carries the provider SDKs). Plain-diff mode — the silent-producer path both integrations use — is unchanged upstream, so 1.x review behavior is preserved; consumers can still pin their own version via the workflow input. Note one upstream removal rides along: 0.48 no longer reads a `[tool.pr-agent]` table from a consumer's `pyproject.toml` (move those settings to `.pr_agent.toml`).
- Feat (Forge 2.0, not yet wired into review runs): PR-Agent subprocess adapter for chomp in `agent/src/pr_agent.rs` (FRG-23, proposal §8.6 Move 1). It invokes the unmodified PR-Agent CLI once per review against the real PR URL through the GitHub provider — replacing the `--diff-file` plain-diff invocation under which `repo_context_files` was a silent no-op — with Forge's configured context files, the host-rendered repo map injected via `[artifacts]`, publishing disabled, and the structured review captured via `--json-output` as synthesis evidence. Every failure (missing credentials, spawn failure, nonzero exit, timeout kill, invalid output) is a producer-failure value, never a crashed run. Fan-out wiring is FRG-24; workflow and credential provisioning is FRG-28.

## v1.1.7 (2026-10-29)

- Fix: GitHub diff fetch survives PRs past GitHub's diff size limits (FRG-53). `github_backend.fetch_pull_diff` made one request with the diff media type and exited on any error; that endpoint answers HTTP 406 once a diff is too large, so the forge-review job died at Fetch PR diff and reviewed nothing (observed on Stratum PR #46, 770 files, where the failed check then blocked the merge). On 406 the fetcher now pages the pulls files API (`fetch_pull_files`) and assembles the same unified-diff document from per-file patches (`assemble_pull_diff`, mirroring `fetch_diff.py`'s GitLab assembly): synthesized `diff --git` / `---` / `+++` headers with patches byte-for-byte, `rename from` / `rename to` for renames, and git's `new file mode` / `deleted file mode` lines for added/removed files — diff parsers (unidiff, which pr-agent reads `mr.diff` with) only accept a `/dev/null` side when that line marks it, and the files API reports no modes, so the standard 100644 is synthesized. A changed file whose patch the API omitted (binary, or too large to serve) keeps its section with an explicit `[forge: no patch served...]` marker instead of being silently dropped. Verified live against Stratum PR #46: all 770 file diffs assembled (346 renames, 18 additions, 3 deletions), parsing clean under unidiff, master's line map, and verify's section splitter. Adds `tests/test_github_backend.py` coverage for the assembly shapes, the multi-page 406 fallback, non-406 errors staying fatal, and parser parity.

## v1.1.6 (2026-10-27)

- Fix: verification focus-file extraction now matches cited paths with fewer than two directory components (FRG-48). The pattern behind `extract_focus_files` required `dir/dir/file`, so a finding citing `scripts/play-upload-internal.py` or `app/build.gradle.kts` extracted no focus files at all; its evidence silently degraded to the 8000-char head-truncated whole-PR diff — the cited file's hunks past the cut — and the final-state ground-truth fetch never ran, yielding a false NOT_ADDRESSED verdict for a fix that was present (observed on Stratum PR #33, where the sibling `.github/workflows/ci.yml` finding on the very same commit verified ADDRESSED). The pattern now accepts any number of leading directory segments, including none. Adds regression tests to `tests/test_verify_diff_scoping.py` covering single-directory, one-slash, and root-level citations plus a shallow-file scoping case.

## v1.1.5 (2026-10-25)

- Fix: serialize Forge reviews per PR so concurrent runs cannot post duplicate consolidated reviews (FRG-11). The reusable workflow carries a per-PR job concurrency group (`forge-review-<repo>-<PR>`, `cancel-in-progress: false`, `queue: max`) so queued pushes each still get their verify pass in FIFO order, and a marker re-check immediately before Master routes a queued run to verify; `master.py` re-checks the marker immediately before posting on GitHub and GitLab alike (GitLab parity: the template already checked at master time, the new guard closes the remaining dedup window; template unchanged) and records the suppression as `duplicate_suppressed` in `forge-recorded.json` instead of posting. Adds `tests/test_master_duplicate_guard.py` and extends `tests/test_forge_review_workflow.py` and `tests/test_github_backend.py`.

## v1.1.4 (2026-10-25)

- Fix: render finding descriptions and evidence as wrapped markdown instead of `<pre>` blocks (FRG-8). `build_rollup` drops the `<pre>` wrappers and prefixes every line of multi-line description/evidence with the blockquote marker so all paragraphs stay inside the details structure; `build_inline` emits the description as a normal markdown paragraph. The Agent Prompt triple-backtick fence is kept deliberately as a copy-paste artifact. Adds `tests/test_master_wrapped_markdown.py`.

## v1.1.3 (2026-10-24)

- Feat: ticket conformance verdict in the master rollup, covered by the verify loop (FRG-7). Master now renders a `## Ticket conformance` section (resolved tickets with links, per-criterion addressed/not evident/contradicted with evidence, neutral unrelated-changes list, one overall conforms/partially conforms/does not conform/no linked ticket line; advisory only, never a merge gate, no invented criteria for sparse tickets), Thermos correctness treats ticket-specified behavior as checkable fact and structure judges refactors against the ticket, and `verify.py` re-evaluates open criteria on re-push with the same final-state evidence as findings, posting one summary re-check comment. Adds `tests/test_conformance_rollup.py` and `tests/test_verify_conformance.py`.

## v1.1.2 (2026-10-24)

- Feat: deterministic ticket resolution for all producers (FRG-6). New `.forge/ticket.py` extracts ticket refs with a strict branch → title → body → commits cascade (first match per provider wins, distinct tickets kept, no match is normal), fetches Linear issues via GraphQL with the consumer `LINEAR_API_KEY` and GitHub issues via the Forge app token, clips descriptions, and writes `ticket.md`/`ticket.json` beside `mr.diff`; all producers receive the artifact as delimited untrusted quoted context and master records resolved IDs in `forge-recorded.json`. Fully fail-soft. Adds `tests/test_ticket_extract.py` and `tests/test_ticket_fetch.py`.

## v1.1.1 (2026-10-24)

- Feat: wire PR-Agent ticket-context plumbing into the GitHub workflow (FRG-5). The reusable workflow declares an optional `LINEAR_API_KEY` secret (absence changes nothing) forwarded to the PR-Agent and ticket steps, and enables `pr_reviewer.check_pr_additional_content` via the dynaconf `PR_REVIEWER__CHECK_PR_ADDITIONAL_CONTENT` env override. Verified against the pinned pr-agent 0.46.0 source: in `--diff-file` plain-diff mode its provider hardcodes title/branch/description, so no PR context can be consumed there and `LINEAR_API_KEY`/`check_pr_additional_content` are inert until upstream honors them — Forge's own ticket artifact (v1.1.2) is the working ticket path. Adds `tests/test_forge_review_workflow.py`.

## v1.1.0 (2026-10-24)

- Feat: GitHub Actions platform. Forge now runs on GitHub as a reusable workflow (`.github/workflows/forge-review.yml`) that consuming repos call on `pull_request`: the Shippopotamus Forge GitHub App mints an installation token, producers run silent and fail-soft exactly as in the GitLab template, master posts one consolidated review (rollup as a PR conversation comment with github.com blob links, inline comments through the pulls comments API anchored by the same diff line map that positions GitLab discussions), and re-pushes run verification — verdict replies through the review-comment replies endpoint, thread resolution through GraphQL `resolveReviewThread`. New `.forge/github_backend.py` (REST + GraphQL, stdlib only) and `.forge/fetch_diff_github.py` (pulls API diff media type); `master.py` and `verify.py` select the backend by environment (`FORGE_PLATFORM`, defaulting to GitHub under GitHub Actions) with the GitLab paths unchanged, and scripts come from a checkout of this repo instead of the GitLab file-download the template relied on. `openrouter_client.get_model()` treats an empty `FORGE_MODEL` as unset. Adds `tests/test_github_backend.py` (suite: 55 tests). The GitLab template keeps working as is; per the GitHub PR flow, the v1.1.0 tag is cut on the merge commit.

## v1.0.12 (2026-10-04)

- Feat: post verify verdicts as inline replies on the original discussions. `verify.py` printed verdicts (ADDRESSED / NOT_ADDRESSED) to the job log only, so reviewers saw nothing inline. Each verdict is now posted via `POST .../discussions/:id/notes` (replying inherits the thread) for both outcomes — ✅ ADDRESSED before resolving, ⏳ NOT_ADDRESSED leaving the thread open — with the verified sha and LLM reason in the reply body. Reply failures log ERROR without aborting the run (non-fatal requests, same as resolution) and are surfaced at the end alongside resolve failures. Adds `tests/test_verify_replies.py` regression coverage.

## v1.0.11 (2026-09-25)

- Fix: ground verify verdicts in the verified commit's final file state. Evidence is the net MR diff vs target, so a fix that removes lines the same MR added shows no removal hunk and the LLM concludes "no removal happened" (Stratum MR169 M1: the display field was removed in the fix commit but added two commits earlier, so the net diff showed only the new sidecar type). `build_final_state_block()` fetches cited files at the verified sha via the repository API and presents them as ground truth for whether cited code still exists — deleted vs unknown fetch outcomes stay distinct — with the prompt instructed that absence plus the recommended fix elsewhere is ADDRESSED. Adds `tests/test_verify_final_state.py` regression coverage.

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
