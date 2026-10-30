# Plan: FRG-23 — PR-Agent adapter (Move 1, option C)

## Goal

Chomp gains a PR-Agent producer: once per review it runs the
unmodified PR-Agent 0.49.0 CLI against the real PR URL through
the GitHub provider — so `repo_context_files` works for the
first time — with the rendered repo map injected via
`[artifacts]`, publishing disabled, and the structured review
captured silently via `--json-output` as the evidence that
will land at `--out-pr-agent`. Every failure mode returns a
producer-failure outcome instead of an error. The Forge pin
moves 0.46.0 → 0.49.0 everywhere it is installed.

## Architecture decisions

### D1: Capture = stock CLI `--json-output` + `publish_output=false` (vision-gate approved 2026-10-29, "option C")

Invocation: `<python> -m pr_agent.cli --pr_url=<url>
--json-output=<run-dir>/review.json review`. Evidence is the
structured JSON file PR-Agent writes (parsed `review` data +
`usage` + `coverage`), returned byte-for-byte as written.
This **redefines ticket AC4**: what lands at `--out-pr-agent`
(FRG-24 wiring) is structured JSON, not guide markdown; the
human approved the trade at the gate. Research basis:
addendum in `research.md` (silent-capture verified at the
v0.49.0 tag: JSON write in `run()`'s `finally`,
`pr_reviewer.py:508-539`; markdown guide has no silent
channel on any version).

### D2: Pin bump 0.46.0 → 0.49.0, with provider extras

`--json-output` PR-URL support needs >= 0.48.0; 0.48 also
split provider deps into pip extras. Installs become
`pr-agent[github]==0.49.0` (GitHub workflow + adapter env
contract) and `pr-agent[gitlab]==0.49.0` (GitLab template).
Sites: `.github/workflows/forge-review.yml:65-68,209`,
`templates/forge.gitlab-ci.yml:117`, `README.md:29,139`
(version strings), `tests/test_forge_review_workflow.py:95`
(asserted install string). Plain-diff mode is unchanged
upstream, so 1.x behavior is preserved; the Chunk 0 probe
smoke-tests the plain-diff CLI at 0.49.0 before the bump
lands. Proposal doc (§5.1 etc.) is historical — not edited.

### D3: Unwired adapter (human-approved Q3); standalone-testable

`agent/src/pr_agent.rs` only (plus its tests). `main.rs`,
`outputs.rs` untouched; public items carry
`#[allow(dead_code)]` with an FRG-24 comment, exactly like
`run_structure_session`. FRG-24 wires it (parallel fan-out,
stage timeout enforcement at orchestration level, outage
recording into the decision log, guide bytes into
`write_outputs`). Consequence recorded plainly: ticket ACs
phrased against a full chomp run ("chomp run continues",
"lands at `--out-pr-agent`") are proven at adapter level here
(outcome type + returned bytes) and end-to-end at FRG-24/28.

### D4: Adapter surface mirrors the lens outcome discipline

```rust
pub struct PrAgentRequest {
    pub pr_url: String,
    pub context_files: Vec<String>,   // from ContractResolution::pr_agent_context_files
    pub repo_map: String,             // RepoMap::render() output
    pub entry_max_lines: usize,       // ForgeConfig cap -> repo_context_max_lines
    pub timeout: Duration,            // caller-supplied; FRG-24 passes the stage budget
}
pub enum PrAgentOutcome {
    Completed { json: String },
    ProducerFailure { reason: String },
}
pub fn run_pr_agent(request: &PrAgentRequest) -> PrAgentOutcome
```

No `Result`: like `LensOutcome`, producer failure is a value.
Command construction is a pure function
(`build_invocation(request, run_dir, env-source) -> (program,
args, env pairs)`) so tests assert the wire shape without a
subprocess, matching the `StubServer` exact-wire precedent.

### D5: Env contract (FRG-23 defines, FRG-28 provisions — human-approved Q2)

Adapter reads its own environment:

- `FORGE_PR_AGENT_PYTHON` — interpreter for `-m pr_agent.cli`;
  default `python3`. FRG-28 points it at the workflow venv.
- `GITHUB__USER_TOKEN` — the GitHub provider credential,
  forwarded verbatim to the subprocess. Absent/empty →
  `ProducerFailure` before spawn (never spawn a run that
  cannot read the PR). Which real credential FRG-28 puts
  there is FRG-28's decision; `publish_output=false` means
  read-only suffices.
- `OPENROUTER_API_KEY` — already required by the transport;
  adapter maps it to `OPENROUTER__KEY` for the subprocess.
  Absent → `ProducerFailure` before spawn.
- `FORGE_MODEL` — resolved as the transport resolves it
  (`transport.rs` `DEFAULT_MODEL` fallback), then given the
  `openrouter/` prefix PR-Agent's litellm routing requires
  when no provider prefix is present (1.x precedent:
  `openrouter/meta/muse-spark-1.3-contributor`).

Subprocess env set by the adapter (on top of inherited env):
`CONFIG__PUBLISH_OUTPUT=false`,
`CONFIG__PUBLISH_OUTPUT_PROGRESS=false`, `CONFIG__MODEL`,
`CONFIG__CUSTOM_MODEL_MAX_TOKENS=1000000` (1.x precedent),
`OPENROUTER__KEY`, `GITHUB__USER_TOKEN`,
`CONFIG__REPO_CONTEXT_FILES` (encoding per Chunk 0 probe),
`CONFIG__REPO_CONTEXT_MAX_LINES=<entry_max_lines>`,
`ARTIFACT_PATH=<abs repo-map file>`, and `GITHUB_WORKSPACE`
pinned to the run dir (D6). Secrets never appear in `reason`
strings, logs, or test assertions.

### D6: Run dir + artifact anchoring

Per-run temp dir (`temp_dir() + pid + nanos nonce`, the
established fixture pattern) holds `repo-map.txt` (rendered
map, written by the adapter) and `review.json` (PR-Agent's
output). `GITHUB_WORKSPACE` for the subprocess is set to this
dir so `ARTIFACT_PATH` satisfies upstream's under-workspace
rule regardless of the caller's environment; the PR-head
checkout is never written to. Cleanup: best-effort remove on
all paths; failure to clean is not a producer failure.
Empty `repo_map` (zero-entry snapshot render is `""`) →
skip `ARTIFACT_PATH` entirely rather than inject an empty
artifact. Map exceeding upstream `max_artifact_size=50000`:
pass it through anyway (upstream truncates per its own rule);
do not invent a second truncation here.

### D7: Context files are always explicit

`CONFIG__REPO_CONTEXT_FILES` is set on every run, including
the empty stand-down list (§8.7 rule 4 via
`ContractResolution::pr_agent_context_files`): omitting it
would resurrect upstream's `["AGENTS.md"]` default and defeat
the stand-down. Exact list encoding is whatever the Chunk 0
executed probe proves dynaconf parses (expected JSON-ish);
if no env encoding parses as a list, Chunk 0 stops and
escalates (fallback shapes: `--config.repo_context_files=`
rest arg, or a generated settings file — human call).

### D8: Timeout + failure mapping

Runner spawns with piped stdout/stderr, polls `try_wait`
(std-only, short sleep), kills the child at `timeout`, and
maps to `ProducerFailure` with a bounded stderr tail in
`reason`: spawn error, nonzero exit, timeout kill,
missing `review.json`, empty file, unparseable JSON, or JSON
without a `review` object. Exit 0 + valid JSON with `review`
→ `Completed` with the file's exact bytes. No retries.

### D9: No Linear sub-issue mirroring

FRG-19/20/21/22 precedent: one ticket → one PR; chunks live
here. Ticket-text updates (AC4 rewording per D1, pin version)
are proposed to the human at Ship, not edited silently.

## Chunks

### Chunk 0 — Branch, baseline, executed probe (no product code)

Branch `shippopotamus/frg-23-pr-agent-adapter-invoke-against-the-real-pr-url-wire`
from `origin/main` (`6f2ed9a`); carry `plans/FRG-23/`.
Baselines: `cargo test --locked` in `agent/`,
`python3 -m unittest discover -s tests` — record counts.
Probe in `/tmp` (throwaway venv, nothing committed):
`pip install "pr-agent[github]==0.49.0"`, then executed
checks, results recorded verbatim in `status.md`:

- Settings probe (no network/token): load PR-Agent settings
  with `CONFIG__REPO_CONTEXT_FILES` set to a two-item list
  and to an empty list; print parsed values. Same for
  `CONFIG__PUBLISH_OUTPUT=false`,
  `CONFIG__REPO_CONTEXT_MAX_LINES`.
- CLI surface probe: `python -m pr_agent.cli --help` (flags
  exist), and a plain-diff run against a trivial diff with a
  dummy key is NOT required (would need a live model); module
  import + `--pr_url … --json-output …` argument validation
  reaching provider construction (fails on auth, not on arg
  parsing) is the bar.
- GitLab extra sanity: `pip install "pr-agent[gitlab]==0.49.0"`
  in a second venv resolves.

Acceptance: branch exists; both baselines green with counts
recorded; probe prints a list encoding that parses to the
exact input lists (incl. empty). If no encoding works →
stop, escalate per D7.

### Chunk 1 — Pin bump (workflow, template, README, workflow test)

Files: `.github/workflows/forge-review.yml` (default
`"0.49.0"`, install `pr-agent[github]==${{ inputs.pr_agent_version }}`),
`templates/forge.gitlab-ci.yml` (`pr-agent[gitlab]==0.49.0`),
`README.md:29,139` (version strings only),
`tests/test_forge_review_workflow.py` (install-string assertion
+ docstring version). Nothing in `.forge/`.

Acceptance scenarios:

- Given the workflow at default inputs, when the producer
  step installs PR-Agent, then it installs
  `pr-agent[github]==0.49.0` (test asserts the exact string).
- Given the GitLab template, when `forge:review` installs,
  then it installs `pr-agent[gitlab]==0.49.0`.
- `python3 -m unittest discover -s tests -v` fully green.

### Chunk 2 — Adapter core (`agent/src/pr_agent.rs` only)

Types + `build_invocation` + `run_pr_agent` per D4–D8.
`mod pr_agent;` already declared in `main.rs:12` — no
`main.rs` edit expected; if visibility needs `pub(crate)`
items only, keep them crate-private. Module doc rewritten:
§8.6 Move 1, option-C capture, env contract table, FRG-24
wiring note.

Acceptance scenarios (proven in Chunk 3):

- Given token/key absent, when `run_pr_agent` is called,
  then `ProducerFailure` naming the missing env var, and no
  process is spawned.
- Given a request with context files `["AGENTS.md"]`, cap
  2000, non-empty map, when the invocation is built, then
  args are `-m pr_agent.cli --pr_url=<url>
  --json-output=<run-dir>/review.json review` and env carries
  D5's exact pairs incl. `ARTIFACT_PATH`.
- Given an empty context-file list, then
  `CONFIG__REPO_CONTEXT_FILES` is present with the probed
  empty-list encoding (never omitted).
- Given an empty repo map, then `ARTIFACT_PATH` is omitted.

### Chunk 3 — Adapter tests (fake executable fixtures)

In `pr_agent.rs` test module. Fake interpreter: a shell
script written into a temp fixture dir (established nonce
pattern), selected via `FORGE_PR_AGENT_PYTHON`; it records
argv + selected env to a file and writes canned JSON to the
`--json-output` path parsed from its argv. Env mutation in
tests must be serialized (single lock / one test fn driving
cases, or pass an env-source seam if D4's builder needs one
— a tiny internal `fn(...) -> Option<String>` parameter on
the private core is acceptable; public API stays env-based).

Acceptance scenarios:

- Happy path: fake writes `{"review":{...}}` JSON → outcome
  `Completed` with byte-identical `json`; recorded argv/env
  match Chunk 2 scenarios; repo-map file content equals the
  request's `repo_map` exactly; run dir removed after.
- Failure injection: fake exits 3 with stderr text →
  `ProducerFailure` whose reason contains the exit status
  and stderr tail, not secrets.
- Killed subprocess: fake sleeps; request timeout ~200ms →
  `ProducerFailure` (timeout), child actually dead, test
  completes in well under the sleep duration.
- Exit 0 but: no output file / empty file / non-JSON /
  JSON without `review` → each `ProducerFailure` with a
  distinct reason.
- Missing `GITHUB__USER_TOKEN` / missing
  `OPENROUTER_API_KEY` → `ProducerFailure`, fake's record
  file absent (proves no spawn).

### Chunk 4 — Gates

`cargo test --locked`, `cargo clippy --all-targets -- -D
warnings`, `cargo fmt --check` in `agent/`;
`python3 -m unittest discover -s tests -v`. All green, counts
recorded in `status.md`. `git status` shows only planned
files (no `.pyc`, no collateral).

## Test strategy

New: Chunk 3 fake-executable suite (adapter behavior, failure
injection incl. the ticket's killed-subprocess case) +
Chunk 1's updated workflow assertion. Existing suites guard
the rest; Chunk 0's probe is executed evidence recorded in
`status.md`, not a committed test (needs network/pip). The
live end-to-end proof (real PR URL, entry-file content
visibly reaching PR-Agent, artifact in the run record)
remains AC-level work for FRG-28's workflow wiring — named
gap, per Q3.

## Risks and unknowns

- **Probe contradicts code reading** (list encoding,
  `--json-output` arg validation): Chunk 0 stops the line;
  D7 fallbacks need a human call. Do not improvise.
- **Chunked reviews on large PRs:** `_structured_review_data`
  on multi-chunk runs is code-read, not executed; if a later
  live run shows partial JSON, that lands as an FRG-28/proof
  finding, not silently patched here.
- **0.49.0 behavioral drift in 1.x:** plain-diff verified at
  source level; first consumer run on the new pin is the real
  proof. Pin stays an overridable workflow input, so rollback
  is a consumer-side input change.
- **Release mechanics for Chunk 1:** the pin sits on the
  released workflow surface. AGENTS.md's release process
  (changelog + tag + push) is decided at Ship with the human;
  agent/-only FRG tickets skipped it, Chunk 1 may not.

## Out of scope

Main/fan-out wiring, decision-log outage entries, stage
timeouts at orchestration level (FRG-24); `run_pr_agent`
tool (FRG-27); workflow `forge_agent` flag, venv provisioning,
credential provisioning, shadow mode (FRG-28); any change to
`.forge/` Python or 1.x producer behavior beyond the pin;
fork/patch/tooling of PR-Agent (NG2).

## Amendments

- 2026-10-29 (Chunks 2+3): D5 model-prefix rule implemented as
  unconditional `openrouter/` (strip one existing prefix per the
  transport rule, then add). Rationale: the subprocess receives only
  `OPENROUTER__KEY`, so every value must route through litellm's
  OpenRouter provider — a passed-through `anthropic/...` would make
  litellm try Anthropic with no key. Reproduces the 1.x precedent
  exactly and round-trips `FORGE_MODEL=openrouter/...` unchanged.
