# FRG-21 — Plan: structure lens session in chomp

Ticket text governs (`ticket.md`); research in `research.md`. This plan
records the calls the ticket and research left open. Base: current
`main` (`d82296f` — FRG-19 #22 and FRG-20 #25 merged). Branch:
`shippopotamus/frg-21-structure-lens-session-in-chomp`.

## Goal

Add the structure lens as a second `agent_core` session on the exact
FRG-19 construction — same opening context, tools, budgets, outcome and
validation contract — differing only in its system prompt
(`agent/prompts/structure.md`): maintainability, layering fit,
duplication, misplaced responsibilities, with every "already exists"
or sibling-organization claim pulled via tools and cited `path:line`.

## Architecture decisions (must not drift)

- **D1 — Shared body, named wrappers** (human-confirmed 2026-10-29).
  Extract the body of `run_correctness_session`
  (`agent/src/sessions.rs:67` on main) into
  `fn run_lens_session(prompt: &'static str, snapshot: &Snapshot,
  diff_text: &str, contract: &ContractResolution,
  transport: &dyn ModelTransport,
  executor: &Arc<dyn ToolExecutor + Send + Sync>,
  agent_config: AgentConfig) -> LensOutcome`.
  `run_correctness_session` keeps its exact public signature and
  delegates with `CORRECTNESS_PROMPT`; new `run_structure_session`
  (identical signature) delegates with `STRUCTURE_PROMPT`.
  `system_prompt()` becomes `system_prompt(prompt: &str) -> String`
  (same provenance-strip logic). No other signature changes;
  `assemble_opening_context`, `read_full_file`, `parse_findings`,
  `host::drive_session`, and `outputs::Finding` are untouched.
- **D2 — `main` stays correctness-only** (human-confirmed 2026-10-29).
  No `main.rs` wiring in this ticket; dual-lens fan-out is FRG-24.
  Consequence: `run_structure_session` is unreachable from the binary
  until FRG-24 and the crate builds with `-D warnings` in CI gates —
  put a targeted `#[allow(dead_code)]` on `run_structure_session` with
  a one-line comment (`wired into main by FRG-24 fan-out`), mirroring
  the temporary-allowance precedent from FRG-16/FRG-17. Do not add a
  module-level allow. Sessions stay in the binary target (FRG-19
  decision); do not move them into `lib.rs` to dodge the warning.
- **D3 — Citation discipline is prompt + synthesis, not validator.**
  `parse_findings` already rejects malformed citations but accepts
  absent ones; keep that. An uncited "already exists" finding must
  still parse to `Completed` — FRG-26 treats it as unverified. The
  ticket explicitly says assert the discipline, not model perfection.
- **D4 — Prompt parity with the FRG-19 shape.** `structure.md` is
  rewritten to: provenance header (`lens behavior defined in FRG-21`),
  `# Lens B` title, `## Posture` (keep the existing strict
  maintainability posture and non-negotiable standards, trimmed to fit
  the shape), `## Your lane` (structure only; defects/security →
  correctness lane; stated-rule enforcement → contract lane FRG-22;
  contract entry is *input* to judgment, not law to enforce; no quota
  findings; behavior-preserving recommendations only),
  `## Context you are given` (same text contract as correctness.md —
  note the contract entry is the governing base-branch copy, per
  FRG-20), `## Tools and the pull-and-cite rule` (structure flavor:
  before claiming a helper/utility already exists, or that sibling
  modules are organized a certain way, pull it with
  `grep`/`read_file`/`read_symbol`/`list_dir` and list every location
  read in `citations`; uncited relational claims are unverified
  downstream), `## Grounding` (same four bullets as correctness.md,
  including the `<redacted>` rule — it replaces the old standalone
  secret-redaction section), `## Output` (ONLY a JSON array, ≤10
  findings severity-ordered, `[]` when clean, the same 11-field
  example with `citations`, `severity` vocab, open `category` naming
  with structure examples — `Duplication`, `Layering`,
  `Maintainability`, `Misplaced responsibility` — and a
  structure-flavored severity guide: Critical = structural damage
  that blocks merge (e.g. layering inversion, file pushed past 1k
  lines by tangled growth); High = clear duplication of an existing
  helper or misplaced responsibility that will compound; Medium =
  design smell with an obvious cleaner structure; Low = minor
  structural polish. No crashes/security language — wrong lane.)
  The `{diff}` / `{file_contents}` / `{ticket}` template sections are
  deleted (opening context is host-assembled; no lens receives ticket
  text today — parity with FRG-19, decided in research).
- **D5 — No live-model tests.** All new tests use the existing
  in-module `ScriptedTransport` + `RepoFixture` (sessions.rs test
  module). `agent/tests/live_smoke.rs` (FRG-54) is untouched — it
  exercises the CLI, which does not change.
- **D6 — No Linear chunk mirroring.** Repo precedent (FRG-19, FRG-20)
  is one ticket → one PR with the plan in `plans/`; no sub-issues.

## Chunks

### Chunk 0 — Branch setup
Scope: `git checkout -b shippopotamus/frg-21-structure-lens-session-in-chomp origin/main`,
carrying `plans/FRG-21/` (currently untracked on the FRG-18 checkout).
Files: none changed. Done when `cargo test --manifest-path
agent/Cargo.toml` is green on the untouched branch.

### Chunk 1 — Generalize `sessions.rs`
Scope: `agent/src/sessions.rs` only. Implement D1 + D2: add
`STRUCTURE_PROMPT` const (`include_str!("../prompts/structure.md")`),
extract `run_lens_session`, parameterize `system_prompt`, add
`run_structure_session` with the targeted `#[allow(dead_code)]`.
Adapt the existing `system_prompt_strips_the_provenance_comment` test
to the parameterized helper (still asserting the correctness prompt).
Acceptance scenarios:
- Given the untouched FRG-19/FRG-20 test suite, when the extraction
  lands with `structure.md` not yet rewritten, then every existing
  sessions/host/outputs/cli test passes unchanged (renames inside the
  one prompt test excepted).
- Given `cargo clippy --all-targets -- -D warnings`, then no
  dead-code or other warnings from the new wrapper.

### Chunk 2 — Rewrite `agent/prompts/structure.md`
Scope: prompt file only, per D4. Acceptance scenarios:
- Given the rewritten prompt, then it contains no `{diff}`,
  `{file_contents}`, or `{ticket}` tokens, names all four tools,
  states the pull-and-cite rule for existing-helper and
  sibling-organization claims, documents `citations` in the output
  example, and its provenance header reads `defined in FRG-21`.
  (Asserted by the Chunk 3 prompt test, not by eyeball.)

### Chunk 3 — Structure session tests
Scope: test module in `agent/src/sessions.rs` only. New fixture:
base commit carries `src/util.rs` with an existing helper (e.g.
`pub fn slugify(...)` under a unique comment such as
`// canonical slug helper — reuse, do not reimplement`) committed and
unchanged by the diff; the diff adds `src/feature.rs` reimplementing
the same logic under a different name. Scenarios:
- **Duplication pulled and cited:** script =
  [`grep` for the helper name/logic, `read_file src/util.rs`,
  text_response(DUP_FINDING)] where DUP_FINDING cites
  `src/util.rs:<helper line>`. Then outcome is
  `Completed` with one finding whose `citations` equals that
  location; `request_text(0)` does NOT contain the unique helper
  comment (not in opening context — had to be pulled) and does NOT
  contain `{file_contents}`; the post-tool request DOES contain it.
- **Clean structure:** same fixture repo, diff that only calls the
  existing helper; script = [`[]`]. Then `Completed(vec![])` via
  `run_structure_session`.
- **Uncited claim still parses (D3):** script returns a finding
  asserting duplication with **no** `citations` field. Then
  `Completed` with `citations == None` — pinning that rejection is
  FRG-26's job, not the validator's.
- **Budget parity:** structure wrapper + `max_turns: 1` + infinite
  tool-call fallback → `BudgetExhausted` (mirrors FRG-19's
  `tiny_turn_budget_is_a_budget_outcome`, proving the wrapper shares
  the outcome mapping).
- **Prompt shape:** `system_prompt(STRUCTURE_PROMPT)` strips the
  provenance comment, contains `citations` and the four tool names,
  and contains none of the three placeholder tokens.
Reuse `executor_for`, `governing_notes()`/`absent_contract()` helpers
already in the test module; pass a `ContractResolution` exactly as
the FRG-19 tests now do. Do not duplicate ScriptedTransport.

### Chunk 4 — Gates
- `cargo fmt --check` (run in `agent/` per repo convention)
- `cargo clippy --all-targets -- -D warnings`
- `cargo test --locked` (in `agent/`)
- `python3 -m unittest discover -s tests -v` (repo root; untouched
  surface, must stay green)
All four observed green in-session before Review.

## Test strategy

New tests: Chunk 3 only (5 scenarios above). Guard suites: all
existing `sessions.rs` tests (class-1 pull/cite, paging, prose →
ProducerFailure, budget, validation, FRG-20 contract-channel and
truncation-note tests), `host.rs`, `outputs.rs`, `tests/cli.rs`,
`tests/transport_loop_test.rs` — these pin the shared body Chunk 1
refactors, which is why the extraction is safe.

## Risks and unknowns

- **Refactor drift in Chunk 1** — if any existing test needs more
  than the one prompt-test adaptation, stop: the extraction has
  changed behavior; fix the extraction, not the tests.
- **Dead-code allow spreading** — if clippy still warns beyond the
  wrapper (e.g. on `STRUCTURE_PROMPT`), keep allows item-targeted;
  never module-level. Escalate if a target cannot carry one cleanly.
- **Fixture line numbers** — the cited `src/util.rs:N` must be the
  real helper line in the fixture as written; compute it from the
  fixture content in the test or pin the content so N is stable.
  Off-by-one here fails the headline scenario.
- **Prompt length** — the old structure standards list is long; if
  keeping all of it makes the prompt dwarf correctness.md, trim
  wording, never standards. Proceed without escalating.

## Out of scope

- `main.rs` / CLI wiring of the structure lens (FRG-24 fan-out).
- Synthesis-side rejection of uncited claims (FRG-26); validator
  changes of any kind.
- Contract lens (FRG-22); contract resolution changes (FRG-20, done).
- Ticket-text plumbing into opening context (no lens has it today).
- Live/OpenRouter structure smoke (FRG-54 covers the CLI surface;
  structure joins it only when FRG-24 wires it in).
- `.forge/**` Python producers and prompts (retirement is FRG-33).

## Ship (after Review, per AGENTS.md)

Single functional commit `feat(agent): add structure lens session
(FRG-21)` (+ `plans/FRG-21/` artifacts), push branch, PR with the
same title.

Amendment 2026-10-29 (Review round 1): the original Ship section
prescribed a changelog commit + version tag. Verified wrong for
this surface: the AGENTS.md release process governs functional
changes in `.forge/` (Forge 1.x), and the FRG-19 (`a9735a7`) and
FRG-20 (`d82296f`) squash commits contain no CHANGELOG change and
cut no tag (latest tag `v1.1.7` is the FRG-53 `.forge/` fix).
Chomp (`agent/`) tickets ship as one PR, no changelog, no tag.
