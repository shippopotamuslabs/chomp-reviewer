# Status: FRG-23

## Phase Checklist
- [x] Research — vision approval: approved 2026-10-29 (option C; Q2/Q3 as recorded)
- [x] Plan
- [x] Implementation
- [x] Review — rounds: 2/3, result: clean (round 1: 2 MINORs, fixed; round 2: verified)
- [ ] Ship — PR: https://github.com/shippopotamuslabs/forge-reviewer/pull/28 (open, awaiting external review/merge); tag v1.1.8 pushed

## Current Gate
none

## Next Phase
Ship

## Notes
- 2026-10-29: Ticket opened from Linear FRG-23 (PR-Agent adapter, Move 1)
  at the human's request to use FRG-23 as the Synthitect vision.
  Intake base: working tree on FRG-22 branch @ c49c4dc (PR #27 open);
  origin/main @ 6f2ed9a. FRG-23 implementation branch not yet created —
  branch + baseline are Plan Chunk 0 material (from origin/main,
  Linear branch name in ticket.md).
- Blocked-by FRG-15 is In Review (scaffold landed: `chomp` crate,
  CLI incl. `--pr-url`/`--out-pr-agent`, `ForgeConfig`); FRG-17
  (snapshot/repo map) In Review, landed on main. Blocks FRG-24
  (fan-out + outage recording) and FRG-27 (`run_pr_agent` tool).
  FRG-28 owns workflow wiring incl. the Python env step.
- 2026-10-29: Research complete (`research.md`). Five scouts +
  critic; orchestrator spot-checked the decisive claims incl.
  upstream v0.46.0 capture gates. Head finding: no
  `pr_agent.cli` PR-URL invocation yields guide bytes without
  posting (`--output` plain-diff-only; `publish_output=false`
  traps guide in `settings.data`, CLI discards it;
  `push_outputs` emits only when publishing). Proposed
  direction: thin host-owned Python driver over
  `PRAgent.handle_request` + `publish_output=false`. Three
  gate questions: capture architecture, token/holder,
  main-wiring scope. Awaiting vision approval.
- 2026-10-29: Gate round 2. Human settled Q2 (FRG-23 defines
  token env contract, FRG-28 provisions) and Q3 (leave adapter
  unwired from main, must stay standalone-testable). Latest-
  version check (research.md addendum): latest is 0.49.0
  (2026-10-25); 0.48+ `--json-output` + `publish_output=false`
  captures structured JSON silently via stock CLI, but not
  the markdown guide (still no silent channel). Q1 now:
  (A) driver/verbatim markdown vs (C) 0.49.0 bump +
  `--json-output`/structured-JSON evidence.
- 2026-10-29: Vision approved — option C incl. the pin bump.
  Plan written (`plan.md`, chunks 0-4, D1-D9). Plan calls made
  alone: env var names (`FORGE_PR_AGENT_PYTHON` default
  `python3`; token as `GITHUB__USER_TOKEN`), run-dir workspace
  anchoring, no-retry failure mapping, ticket AC4 redefinition
  deferred to a proposed ticket edit at Ship (D9).
- 2026-10-29 (Chunk 0): branch
  `shippopotamus/frg-23-pr-agent-adapter-invoke-against-the-real-pr-url-wire`
  created from origin/main @ b397833 (note: origin/main had moved
  past the plan's 6f2ed9a — FRG-22 PR #27 merged as b397833; branch
  is at the newer main, `plans/FRG-23/` untracked dir carried over
  intact). Baselines green: `cargo test --locked` in `agent/`
  (112 lib + 42 bin + 6 cli + 3 transport_loop = 161 passed, 0
  failed; live_smoke 1 ignored) and
  `python3 -m unittest discover -s tests -v` at root (168 ran, OK).
  Probe venv /tmp/frg23probe: `pip install "pr-agent[github]==0.49.0"`
  OK (0.49.0 confirmed). Settings probe via
  `pr_agent.config_loader.get_settings`, JSON-ish encoding wins on
  first try (no iteration needed):
  `CONFIG__REPO_CONTEXT_FILES='["AGENTS.md", "docs/x.md"]'` parses
  to `dynaconf.nodes.DataList ['AGENTS.md', 'docs/x.md']` with
  `list(v) == ["AGENTS.md", "docs/x.md"]` True;
  `CONFIG__REPO_CONTEXT_FILES='[]'` parses to `DataList []` with
  `list(v) == []` True;
  `CONFIG__PUBLISH_OUTPUT='false'` parses to `bool False`
  (`is False` True);
  `CONFIG__REPO_CONTEXT_MAX_LINES='2000'` parses to `int 2000`;
  `CONFIG__MODEL='openrouter/meta/muse-spark-1.3-contributor'`
  stays `str` verbatim. CLI probe:
  `python -m pr_agent.cli --help` works (`--json-output` flag
  present: "Write a parsed review and token usage to this JSON
  file (place before the review command)");
  `python -m pr_agent.cli --pr_url=https://github.com/owner/repo/pull/1
  --json-output=/tmp/x.json review` gets PAST argparse (no
  argparse/`--json-output-mode` error; process exit 0) and fails
  at provider construction on auth — `ValueError: GitHub token is
  required when using user deployment` via
  `get_git_provider_with_context` (`Failed to get git provider
  for https://github.com/owner/repo/pull/1`); no /tmp/x.json
  written. Second venv /tmp/frg23probe_gl:
  `pip install "pr-agent[gitlab]==0.49.0"` resolves (0.49.0,
  python-gitlab 8.6.0 importable). Winning list encoding for D7:
  JSON-ish (`'["a", "b"]'` / `'[]'`). D7 stop condition not
  triggered. NO product code touched.
- 2026-10-29 (Chunk 1): pin bump 0.46.0 -> 0.49.0 landed per plan
  D2. Changed: `.github/workflows/forge-review.yml:68` default
  `"0.46.0"` -> `"0.49.0"`; `:209` install string
  `"pr-agent==${{ inputs.pr_agent_version }}"` ->
  `"pr-agent[github]==${{ inputs.pr_agent_version }}"`;
  `templates/forge.gitlab-ci.yml:117`
  `"pr-agent==0.46.0"` -> `"pr-agent[gitlab]==0.49.0"`;
  `README.md:29,139` version strings only (claims untouched);
  `tests/test_forge_review_workflow.py:9` docstring 0.46.0 ->
  0.49.0 and `:95` install-string assertion updated to the
  `[github]`-extra form. Remaining `0.46.0` hits are historical
  and out of scope per plan: CHANGELOG.md past entries,
  `docs/forge-2.0-architectural-proposal.md` (proposal doc not
  edited). Tests: `python3 -m unittest discover -s tests -v`
  at root: 168 ran, OK. No commit (per chunk instructions).
- 2026-10-29 (Chunks 2+3): adapter core + tests landed in
  `agent/src/pr_agent.rs` ONLY (plus this Notes entry and a dated
  D5 amendment in `plan.md`); `main.rs` untouched (`mod pr_agent`
  already declared). `PrAgentRequest`/`PrAgentOutcome`/`run_pr_agent`
  per D4 with `#[allow(dead_code)]` + FRG-24 comment (same convention
  as `run_structure_session`); pure `build_invocation` with an
  env-lookup seam (private core `run_pr_agent_with_env`; public API
  stays env-based). D5: `transport::resolve_model` is private to the
  lib target, so the `DEFAULT_MODEL` fallback rule was REPLICATED
  (trim, strip one `openrouter/` prefix, fall back) using
  `chomp::transport::DEFAULT_MODEL`, then unconditional `openrouter/`
  prefix (see plan.md Amendments for rationale — deviates from the
  plan's "when no provider prefix present" letter). D6: per-run dir
  `temp_dir()/chomp-pr-agent-<pid>-<nanos>` holding `repo-map.txt` +
  `review.json`; `GITHUB_WORKSPACE` pinned to it; empty map omits
  `ARTIFACT_PATH`; cleanup best-effort on all paths. D7: context
  files always sent via `serde_json` (`'["a"]'` / `'[]'` — the Chunk 0
  probed encoding). D8: threaded stdout/stderr drain (no pipe
  deadlock), 10ms `try_wait` poll, kill on timeout WITHOUT joining
  readers (a forked grandchild holding pipes would block EOF — first
  timeout test took 30.0s before this fix, now 0.2s), 5s-grace
  bounded joins on the exit path, bounded stderr tail (2000 chars),
  distinct reasons per bad-output mode, secrets never in reasons.
  Tests (11 new, fake `sh` fixture made executable, `exec sleep 30`
  for the kill case): happy-path byte-identical JSON + exact
  argv/env assertions + artifact byte-equality + run-dir removal;
  exit-3 (status + stderr in reason, secrets absent); 200ms timeout
  kill; 4 bad-output variants with distinct reasons; missing-token /
  missing-key no-spawn proofs (fake record absent). Env-mutating
  tests serialized via process-wide `Mutex` + `EnvRestore` Drop
  guard (poison-tolerant). Gates in `agent/`: `cargo test --locked`
  112 lib + 53 bin + 6 cli + 3 transport_loop = 174 passed, 0 failed;
  `cargo clippy --all-targets -- -D warnings` clean;
  `cargo fmt` + `--check` clean. Root
  `python3 -m unittest discover -s tests`: 168 ran, OK.
  `git status`: only Chunk 1 files + `agent/src/pr_agent.rs` +
  `?? plans/FRG-23/`; no `.pyc`, no collateral. No commit.
- 2026-10-29 (Review round 1 fixes): two MINORs in
  `build_invocation` (`agent/src/pr_agent.rs`), both fixed, no
  commit. (1) Stale output: `review.json` is now deleted when it
  already exists in the reused nonce dir (failure to delete is an
  `Err` that also removes the run dir) — chose delete-after-create
  over `create_dir`-with-retry as the smaller change. Context-file
  encoding additionally moved BEFORE dir creation, so that `Err`
  path cannot leak a dir at all. (2) Run-dir leak: every `Err`
  after `create_dir_all` now removes the run dir best-effort
  (create-dir failure, stale-JSON clear failure, artifact write
  failure). New regression test
  `fresh_invocation_has_no_predated_review_json` (postcondition:
  no `review.json` predates the spawn); the leak-path cleanups
  have no test — forcing an artifact-write/encode IO failure
  needs a new seam (injected dir or fallible-fs hook) and was
  judged not worth the machinery. Gates in `agent/`:
  `cargo test --locked` 112 lib + 54 bin + 6 cli + 3
  transport_loop = 175 passed, 0 failed;
  `cargo clippy --all-targets -- -D warnings` clean;
  `cargo fmt --check` clean (one wrap fixed via `cargo fmt`).
- 2026-10-29: Review (orchestrator, fresh pass over the diff;
  gates re-run independently, not taken from implementer
  reports). Round 1: 2 MINORs in build_invocation run-dir
  handling — stale review.json could predate spawn on a nonce
  collision; Err paths after create_dir_all leaked the run
  dir. Fixed by implementer (stale file cleared, encode moved
  pre-fs, remove_dir_all on all post-create Err paths; +1
  regression test). Round 2 verified by reading the fix and
  re-running: cargo test 112+54+6+3 green (1 live ignored),
  clippy -D warnings clean, fmt clean, python unittest 168 OK.
  No architecture questions. Residuals (named in plan):
  adapter unwired until FRG-24 (human-approved Q3); live
  end-to-end proof on a real PR belongs to FRG-28 wiring;
  leak-path cleanups lack forced-IO-failure tests (accepted:
  would need a new seam); chunked-review structured-JSON
  shape still unexecuted. Uncommitted on the FRG-23 branch;
  ship (commits + PR) awaits an explicit ask. Open ship
  question: Chunk 1 pin sits on the released workflow surface
  — changelog + tag per AGENTS.md release process, or defer
  to the next 1.x release (agent/ FRG precedent skipped both)?
- 2026-10-29: Shipped on the human's ask. Commits: 91357e8
  feat(agent) adapter + plans, 9accf24 chore(pr-agent) pin
  0.49.0, dd61407 docs changelog v1.1.8 (changelog approved by
  the human; agent/ no-changelog precedent overridden because
  the pin sits on the released surface). Tag v1.1.8 on dd61407
  pushed with the branch and verified via git ls-remote
  (both at dd61407). PR #28 opened:
  https://github.com/shippopotamuslabs/forge-reviewer/pull/28
