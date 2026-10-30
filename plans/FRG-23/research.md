# Research: FRG-23 — PR-Agent adapter (Move 1)

Date: 2026-10-29. Intake base: FRG-22 branch @ `c49c4dc`
(origin/main @ `6f2ed9a`); `pr_agent.rs` identical on both.
Five read-only scouts (adapter scaffold, config/snapshot seams,
1.x invocation, PR-Agent 0.46.0 upstream facts, tests/proposal)
plus a completeness critic. Orchestrator spot-checked the
decisive claims directly: `pr_agent.rs`, `outputs.rs`,
`main.rs` run sequence, `contract.rs` context-file rule,
`snapshot.rs` render, and the upstream v0.46.0 capture gates
(`pr_reviewer.py`, `cli.py`, `pr_agent.py`, `artifacts.py`
fetched from the `v0.46.0` tag).

## What exists today

### The adapter site is an empty scaffold; every seam it needs has landed

- `agent/src/pr_agent.rs` is 4 lines of module doc, zero
  types/functions, declared in the binary only
  (`agent/src/main.rs:12`), never called.
- CLI contract exists: `chomp review --pr-url … --out-pr-agent …`
  parsed (`main.rs:27-64,147-155`); exit 0 = review produced,
  exit 2 = orchestration failure (`main.rs:23-24,282-300`).
- `run_review` today: load_config → validate → read diff →
  `Snapshot::capture` → `resolve_contract` → correctness lens →
  `write_outputs` (`main.rs:225-279`). The guide is written as
  empty bytes (`outputs.rs:94`: `write_file(pr_agent_path, b"",
  "PR-Agent guide")`); the doc comment says so.
- Config seam (FRG-15/20): `ForgeConfig { entry_file,
  pr_agent_context_files, entry_max_lines }`, defaults
  `AGENTS.md` / `[entry_file]` / 2000, parsed from `[forge]` in
  `<repo>/.pr_agent.toml` (`contract.rs:19-47,79-112`).
  The adapter's exact input rule already exists:
  `ContractResolution::pr_agent_context_files(&config)` —
  configured list when `Governing`, **empty when `Absent`**
  (§8.7 rule 4) — marked "the consumer is FRG-23's adapter"
  (`contract.rs:147-161`, spot-checked). Consequence: the
  adapter must pass an *explicit empty list* on stand-down,
  because passing nothing lets PR-Agent's own default
  `["AGENTS.md"]` apply (see upstream below).
- Repo-map seam (FRG-17): `RepoMap::render()` — one
  `path (size)\n` line per entry, in-memory string, no file
  writer (`snapshot.rs:93-102`, spot-checked). Snapshot carries
  `head_sha` stamped into artifacts.
- Decision-log seam: `host_event(kind, event, detail)` →
  `{kind,event,detail}` entries (`outputs.rs:51-66`), with the
  comment that richer producer dispositions are FRG-25/26's.
  No producer-outage convention exists yet.
- **No fail-soft slot exists in `main`**: only
  `LensOutcome::Completed` writes outputs;
  `ProducerFailure`/`BudgetExhausted` become an orchestration
  `Err` → exit 2 (`main.rs:266-279`, spot-checked). The
  ticket's "fail-soft, as today" overstates 2.0: softness
  exists only in the 1.x GitHub workflow (below). FRG-23 has
  to *build* the soft slot for its producer, whatever its shape.
- Subprocess precedent is `Command::new("git")` only
  (`contract.rs:221-230`, `snapshot.rs:216-237`), typed errors
  (`SnapshotError`, `ContractError`), absence-as-value.
  Deps are the audited short list (`agent/Cargo.toml:13-20`:
  agent_core, globset, regex, serde, serde_json, toml, ureq) —
  a subprocess adapter needs no new crate.
- Gospel texts could not be inspected: `contracts/gospel/`
  submodule is uninitialized in this checkout (`git submodule
  status` shows `-52bf479…`). Architecture discipline below
  rests on in-repo precedent, which is consistent (port
  traits, typed errors, checked facts as types).

### 1.x invocation (the thing being replaced)

- Both platforms run plain-diff mode:
  `python -m pr_agent.cli --diff-file=mr.diff
  --output=pr-agent-findings.md review`
  (`.github/workflows/forge-review.yml:213-215`;
  `templates/forge.gitlab-ci.yml:124`). No PR-URL invocation
  exists anywhere.
- Pinned `pr-agent==0.46.0`, fresh venv per run, workflow-owned
  (`forge-review.yml:65-68,208-209`).
- Model/key wiring that **works in production today** is bare
  dynaconf env: `CONFIG__MODEL`, `OPENROUTER__KEY`,
  `CONFIG__CUSTOM_MODEL_MAX_TOKENS`
  (`forge-review.yml:210-212`). This answers half the critic's
  env-form unknown: bare `<TABLE>__<KEY>` is the executed
  precedent for scalars. List-valued env encoding
  (`repo_context_files`) is still unverified.
- Forge sets **no** `.pr_agent.toml` keys of its own; the
  consumer repo supplies the file and Forge only checks it
  exists. No `[github]`/`[artifacts]`/`repo_context_files`
  example ships in this repo.
- Today the guide is validated by grepping `## PR Reviewer
  Guide`; the GitHub path substitutes a placeholder guide and
  exits 0 on any producer failure (fail-soft), the GitLab path
  `exit 1`s (`forge-review.yml:203-221`;
  `forge.gitlab-ci.yml:106-128`).
- Established product rule: "The producers post nothing
  themselves" (`README.md:3,29`) — only the master/posting
  layer writes to the PR.

### PR-Agent 0.46.0 PR-URL mode (upstream, verified at the tag)

Scout-verified from `qodo-ai/pr-agent@v0.46.0` source;
capture gates re-verified by the orchestrator.

- Invocation: `python -m pr_agent.cli --pr_url=<URL> review`;
  provider comes from `config.git_provider` (default
  `"github"`), not URL sniffing. Per-run overrides:
  `--section.key="value"` rest args, env `<TABLE>__<KEY>`, or
  the repo's `.pr_agent.toml` (read from the default branch).
  No `--config_path` flag exists.
- GitHub provider credential: `github.user_token`
  (env `GITHUB__USER_TOKEN`) with `deployment_type="user"`.
  Bare `GITHUB_TOKEN` is not consumed by PR-Agent. Model:
  `CONFIG__MODEL` + `OPENROUTER__KEY`, as 1.x already does.
- `repo_context_files`: `[config]` keys, default
  `["AGENTS.md"]`, cap `repo_context_max_lines=500`, read from
  the **default branch** via `get_repo_file_content` —
  implemented by the GitHub provider, absent on
  `PlainDiffGitProvider` (the 1.x silent no-op, confirmed).
  Note the cap mismatch: PR-Agent 500 vs Forge
  `entry_max_lines` 2000 — an entry file of 501–2000 lines is
  silently truncated for PR-Agent unless the adapter sets the
  cap.
- `[artifacts]`: keys `enable`, `artifact_path`,
  `artifact_instructions`, `artifact_label`, `target_tools`,
  `max_artifact_size=50000`. Env turn-on: `ARTIFACT_PATH`
  alone enables it (orchestrator-verified `artifacts.py:116-132`).
  Path rule (verified `artifacts.py:16-41`): absolute paths
  resolve as-is, **but if `GITHUB_WORKSPACE` is set the file
  must live under it** or it is rejected with a warning.
  Content is appended to the target tools' `extra_instructions`;
  injection runs in the CLI entry point (`cli.py:198`), not in
  `PRAgent.handle_request`.
- `pr_agent_context_files` stand-down subtlety: also note
  `configuration.toml` marks `repo_context_files` as not
  comment-overridable; whether a `--config.repo_context_files=…`
  rest arg is accepted (vs env-only) is unverified.

## The load-bearing unknown: silent guide capture does not exist in CLI PR-URL mode

The ticket assumes PR-URL mode can produce guide bytes for
`--out-pr-agent` without PR-Agent posting anything. Verified
against v0.46.0, **no `pr_agent.cli` invocation does that**:

- `--output` is rejected outside plain-diff mode
  (`cli.py` `_validate_output_options`, orchestrator-read).
- With `config.publish_output=false`, nothing is posted —
  including the "Preparing review..." temporary comment, which
  is gated on `publish_output` at `pr_reviewer.py:319`
  (this resolves the critic's temp-comment-leak item: no leak)
  — but the guide is trapped in-process:
  `get_settings().data = {"artifact": pr_review}` then return
  (`pr_reviewer.py:342-353`, orchestrator-read). The CLI
  discards `handle_request`'s boolean result and never reads
  `settings.data`.
- The local capture channel, `[push_outputs]` (stdout/file
  JSONL of `{type,timestamp,payload,markdown}`), emits the
  review **only when `publish_output=true`**
  (`pr_reviewer.py:1243-1246`, orchestrator-read) — i.e. only
  when PR-Agent also posts its own review comment on the PR.
- Residual scrape channel: `get_logger().debug("PR output",
  artifact=pr_review)` (`pr_reviewer.py:334`). Log-scraping a
  debug record is not a contract; rejected without further
  consideration.

So "invoke `pr_agent.cli` against the PR URL, capture the
guide byte-for-byte, post nothing" is not implementable as
literally written. Something in that sentence has to give.
This is the vision-gate decision.

## Proposed direction

**A thin host-owned Python driver as the subprocess**, instead
of `pr_agent.cli` directly. Chomp spawns
`<python> <driver> --pr-url …` (driver shipped with Forge —
e.g. under `agent/` or `.forge/` — exact home is plan-level).
The driver, in ~30 lines: sets `config.publish_output=false`
(plus the CLI-mode/config-branch reconciliation and
`inject_artifact_context()` call the CLI normally performs —
all public functions in the installed package), awaits
`PRAgent().handle_request(pr_url, ["review", …])`, then writes
`get_settings().data["artifact"]` verbatim to stdout/a file
and exits 0/nonzero on success/failure.

Why this direction:

- PR-Agent stays **unmodified, installed from PyPI, one
  subprocess per review** (NG2 honored). Driving it through
  `PRAgent.handle_request` is how its own servers embed it;
  the CLI is a thin wrapper over the same call
  (`cli.py:202-205`).
- Zero PR comments: `publish_output=false` gates every publish
  path found, temp comment included — so the token the
  provider needs can in principle be read-only, and
  "producers post nothing" survives into 2.0.
- `repo_context_files` and `[artifacts]` work for real this
  time (GitHub provider + explicit artifact injection), which
  is the ticket's entire point.
- The Rust adapter stays a normal subprocess boundary:
  build env (model, key, token, context files, artifact path,
  caps), write the rendered repo map to a file under the
  workspace, spawn, capture stdout bytes → `--out-pr-agent`,
  map every failure (spawn error, nonzero exit, empty/missing
  `## PR Reviewer Guide`, timeout) to a typed producer-failure
  outcome. Testable with a fake driver executable (a script
  fixture standing in for `<python> <driver>`), same spirit as
  `ScriptedTransport`/`StubServer`; no live model or network
  in unit tests.
- FRG-27 (`run_pr_agent` tool) reuses exactly this driver with
  scope/focus mapped to `extra_instructions` — the seam the
  ticket promises.

Wiring scope proposed for FRG-23 (vs the FRG-21/22 unwired
pattern): **wire it sequentially into `main`** — run the
adapter after snapshot+contract resolution, pass guide bytes
into `write_outputs` (replacing the empty write), record a
failure as `host_event("pr_agent", …)` in the decision log
and continue with exit 0. Rationale: the ticket's acceptance
criteria are written against chomp-run behavior ("guide lands
at `--out-pr-agent`", "chomp run continues"), unlike the lens
tickets whose wiring FRG-24 explicitly owns. FRG-24 then
parallelizes this stage and adds stage timeouts and the
all-producers-fail rule; it does not re-decide this adapter.

Plan-level calls the orchestrator will make alone unless the
human objects: set `CONFIG__REPO_CONTEXT_MAX_LINES` (or the
verified override form) from `ForgeConfig.entry_max_lines` so
the 500/2000 caps stop diverging silently; Python environment
stays workflow-managed (FRG-28) with the adapter taking the
interpreter from an env var with a documented default;
outage record starts as `host_event("pr_agent", …)`;
repo-map artifact file written under the run workspace
(satisfying the `GITHUB_WORKSPACE` anchoring rule) after
snapshot capture so it cannot appear in its own map; an
executed settings-precedence probe (env list encoding,
empty-list stand-down, artifact injection) as an early
implementation chunk, since those three behaviors are
code-read but not yet executed anywhere.

## Alternative considered

**`pr_agent.cli` + `publish_output=true` + `[push_outputs]`
file capture.** The only pure-CLI way to get guide bytes in
PR-URL mode. It loses on product grounds: PR-Agent posts its
own persistent review comment (plus labels/state machinery)
on every PR — a second, unadjudicated review beside Forge's
consolidated one, breaking "producers post nothing", needing
a write-scoped token in the producer, and racing the
marker/verify routing. Harvest-then-delete-the-comment was
considered and is strictly worse (visible flicker, delete
failures, more token scope).

Also considered: **stay in plain-diff mode and front-load
context host-side** (host reads the entry file and passes it
via `extra_instructions`; `--output` keeps silent capture).
Loses because it abandons the ticket's core: the provider
channel (`repo_context_files` from the default branch)
remains dead code, PR metadata/file APIs stay unused, and
AC1/AC2 ("real PR URL", "provably dead no-op") fail by
construction.

## Addendum 2026-10-29: 0.46.0 is not latest; 0.49.0 changes the capture calculus

Two scouts + orchestrator spot-checks at the `v0.49.0` tag
(repo moved `qodo-ai/pr-agent` → `The-PR-Agent/pr-agent`).

- **Latest stable is 0.49.0** (PyPI 2026-10-25); 0.46.0 is
  2026-09-21, then 0.47.0 (10-02), 0.48.0 (10-16), no patch
  releases. Python >=3.12 and MIT license unchanged.
- **New in 0.48.0 (#4033): `--json-output` works with
  `--pr_url` for `review`.** Orchestrator-verified execution
  path at v0.49.0: `_prepare_pr_review` stashes structured
  data when `review_json_output_path` is set
  (`pr_reviewer.py:1360-1378`), and
  `write_review_json_output(...)` runs in `run()`'s `finally`
  block (`pr_reviewer.py:508-539`) — so it fires even though
  the `publish_output=false` path returns early at the
  `settings.data` trap (`:396-400`, unchanged). Result:
  `CONFIG publish_output=false` + `--json-output=<path>`
  yields a silent local capture through the stock CLI.
- **But the capture is structured JSON, not the guide.**
  Contents: the parsed model output (`data["review"]`, e.g.
  `key_issues_to_review`) plus host-added `usage` and
  `coverage` blocks. The rendered `## PR Reviewer Guide`
  markdown still has no silent channel in 0.49.0
  (`--output` remains plain-diff-only, `cli.py:98-100`;
  `push_outputs` still gated on publishing).
  Byte-for-byte-guide (ticket AC4 as written) therefore
  still requires the driver; `--json-output` requires
  redefining the evidence that lands at `--out-pr-agent`
  (synthesis/FRG-26 and FRG-27 consume structured JSON
  instead of guide markdown).
- Other relevant 0.47–0.49 changes: `repo_context_files`
  globs + fallbacks (#4220), multi-file `[artifacts]`
  (#4338), local-provider repo context (#3863),
  `github.agent_login` for App tokens (#4329, bears on Q2),
  bare-token log redaction (#4081), host-only config-key
  hardening (#3840 et al.), default model bump
  (irrelevant — Forge sets `CONFIG__MODEL`). Still **no
  MCP/tool-calling surface** (zero `mcp`/`tool_call` paths).
- **Pin-bump costs:** 0.48 splits provider deps into pip
  extras — plain `pr-agent==0.49.0` has no PyGithub; need
  `pr-agent[github]==0.49.0` (orchestrator-verified
  `pyproject.toml:92-95`). `[tool.pr-agent]` in consumer
  pyprojects no longer read (#3941). `merge_recommendation`
  values renamed (#4010; appears inside review output).
  Plain-diff mode (`--diff-file` + `--output`) verified still
  present and wired the same, so 1.x should survive a bump
  with the extras-corrected install — but that is a workflow
  default change (FRG-28 / release territory), and shadow
  mode would run 1.x and chomp off one venv, so the bumped
  version must satisfy both.

Capture options after the addendum: (A) driver on the current
pin — verbatim markdown, host shim on an internal surface;
(B) CLI publish + `push_outputs` — posts to the PR, rejected;
(C) bump pin to 0.49.0 + CLI `--json-output` +
`publish_output=false` — stock CLI, silent, supported
upstream feature, but evidence shape becomes structured JSON
and the bump's blast radius lands in FRG-28's workflow work.

Gate decisions recorded 2026-10-29: **Q2 settled** — FRG-23
defines the token env contract, FRG-28 provisions the
credential. **Q3 settled** — leave the adapter unwired from
`main` (FRG-21/22 pattern into FRG-24), provided the adapter
is fully testable standalone (fake-executable fixtures +
executed settings probe; the `--out-pr-agent` file write then
lands with FRG-24's wiring, and AC phrasing gets adjusted in
the plan). Q1 remains open pending this addendum.

## Open questions (vision gate)

1. **Capture architecture.** Approve the thin Python driver
   (recommended above), or accept PR-Agent posting its own
   comment via the `push_outputs` route, or another shape?
   This decides the adapter's whole structure.
2. **Token and holder.** The GitHub provider needs
   `github.user_token`. Which credential — the Forge App
   installation token (today held only by the Python posting
   steps; FRG-28's §9 text says it must stay out of chomp's
   reach), the workflow `GITHUB_TOKEN`, or a dedicated
   read-only credential? And is naming it FRG-23's job (env
   contract + assumed name) with provisioning deferred to
   FRG-28, as proposed? Note `publish_output=false` means the
   adapter never writes, so a read-only credential suffices
   for the driver direction.
3. **Wiring scope.** Wire sequentially into `main` in FRG-23
   as proposed, or follow the FRG-21/22 pattern (adapter +
   outcome type only, unwired until FRG-24)?

## Risks I could not resolve

- **Executed-behavior gap.** Every PR-URL-mode fact above is
  source-verified at the v0.46.0 tag, none is executed. List
  env encoding, explicit-empty `repo_context_files`, artifact
  injection under `publish_output=false`, and
  `settings.data` contents on partial/chunked reviews all
  need the probe chunk before the design hardens. If the
  probe contradicts the code reading, Q1 reopens.
- **Driver/version coupling.** The driver depends on
  `PRAgent.handle_request`, `inject_artifact_context`, and
  `settings.data["artifact"]` — public-ish but not contractual
  surfaces. The pin (0.46.0) makes this safe today; every
  future `pr_agent_version` bump must re-verify the capture
  path. (Same class of risk 1.x carries with `--output`.)
- **Scratch-repo CI proof (AC1/AC2).** Needs a token, network,
  a consumer `.pr_agent.toml`, and a model key in CI — no
  precedent exists in-repo, and full workflow wiring is
  FRG-28's. FRG-23 can prove everything except the live
  end-to-end run; whether the live proof lands as a manual/
  nightly job here (FRG-54's live-smoke precedent) or waits
  for FRG-28 is a plan-level call the human may want at the
  gate.
- **Gospel submodule uninitialized** in this checkout, so
  architecture/ testing gospel text was not consulted;
  in-repo precedent was used instead.
