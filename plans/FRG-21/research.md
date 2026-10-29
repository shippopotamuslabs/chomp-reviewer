# FRG-21 — Research: structure lens session in chomp

Date: 2026-10-29. Sources: Linear FRG-21/FRG-19, working tree (FRG-18
branch), FRG-19 implementation on `origin/pr-22-frg19` (PR #22, In Review).
Scout claims spot-checked against the cited lines by the orchestrator.

**Update 2026-10-29 (post-merge):** PR #22 merged as `a9735a7`
(FRG-19), and `main` has since advanced to `d82296f` with FRG-54
(#24, nightly live smoke) and FRG-20 (#25, base-branch contract entry
resolution). FRG-20 changed the session signature — see the corrected
notes inline below. Base for FRG-21 is now simply current `main`; the
stack-vs-wait question is moot.

## What exists today

- **No structure-lens code exists.** `agent/src/sessions.rs` in the working
  tree is a 5-line scaffold naming FRG-19/FRG-21/FRG-22. The lens sessions,
  host driver, and findings rendering landed with FRG-19 — merged to
  `main` (`a9735a7`) but not present on this FRG-18 checkout. FRG-21
  branches from current `main`.
- **The FRG-19 pattern (on PR #22) is almost entirely lens-generic already:**
  - `host.rs::drive_session` — the pull loop over all six `AgentAction`
    variants, `DriveOutcome::{Finished, Failed}` with turn-budget
    (`allow_tool_call: false`) detection, 1024-step cap. Fully generic.
  - `sessions.rs::LensOutcome::{Completed, ProducerFailure, BudgetExhausted}`
    and the `(snapshot, diff_text, entry_file, transport, executor,
    agent_config)` plumbing. Generic; only `run_correctness_session`'s name,
    `CORRECTNESS_PROMPT` const, and prompt file are lens-specific.
  - Opening context (`assemble_opening_context`): diff + full changed-file
    contents paged **through the FRG-18 executor** (200-line `read_file`
    windows; redaction and confinement apply — direct fs reads were
    rejected in the FRG-19 plan precisely because they bypass redaction) +
    `RepoMap::render()`. **Post-FRG-20 correction:** the contract entry no
    longer comes through the executor. Sessions take
    `&ContractResolution` (not `entry_file: &str`) and render the
    governing **base-branch** copy verbatim through the contract channel;
    the head-rooted executor structurally cannot serve base rules. For
    FRG-21 this means the layering rules in opening context are the base
    copy — legitimate input to structural judgment, exactly as the ticket
    wants, now tamper-proof against head edits.
  - Construction: `SessionIoParams::new("openrouter", 200_000,
    TOOLS_SCHEMA_JSON, system_prompt)`, `ToolSemanticsConfig { catalog:
    tool_specs(), ..agnostic() }`, `summarization_allowed: false`,
    caller-supplied `AgentConfig` (`main` passes `default()`, max_turns 20).
  - Output validation (`parse_findings` + `outputs::Finding` with
    `deny_unknown_fields`, `Severity` enum, optional `citations`):
    strict JSON array, non-empty fields, sane line ranges, citations must
    be `path:line`. Invalid output → `ProducerFailure`, never silent `[]`.
  - `main.rs::run_review` runs exactly one lens; `Completed` is the only
    arm that writes outputs; failure/budget → exit 2, no files written.
    Sessions module docs on PR #22 explicitly state FRG-21/FRG-22 follow
    "on the same construction."
- **`agent/prompts/structure.md` is still the old template prompt.** It has
  `{diff}` / `{file_contents}` / `{ticket}` placeholders (lines 61/66/81), a
  closed category vocab (`Maintainability|Performance|Test gap`, line 43),
  no named tools, no pull-and-cite rule, and no `citations` field. FRG-19
  rewrote `correctness.md` into the new shape (`## Your lane`, `## Context
  you are given`, `## Tools and the pull-and-cite rule`, `## Grounding`,
  11-field output incl. optional `citations`, at most 10 findings,
  severity-ordered) and deliberately did not touch `structure.md`.
- **Base APIs FRG-21 consumes already exist:** `ToolExecutor::execute` /
  `RepoToolExecutor::new(root, head_sha)`, `TOOLS_SCHEMA_JSON` /
  `tool_specs()` / `tool_catalog_config()`, `Snapshot::capture`,
  `ModelTransport` / `OpenRouterTransport::from_env` /
  `TransportError::to_failure_report`.
- **Contract entry content:** ~~FRG-20 outstanding~~ **merged** (`d82296f`).
  `contract::resolve_contract(repo, base_ref, config)` reads the entry file
  at the base ref (`git show <ref>:<path>`), truncates past
  `entry_max_lines` (default 2,000, recorded), degrades missing/non-UTF-8
  to `Absent` (recorded stand-down, review proceeds), and errors only on
  an unresolvable base ref (exit 2). `main` records
  `entry_absent`/`entry_truncated` host events into the decision log
  (`write_outputs` now takes `decision_entries`). FRG-21 inherits all of
  this via the shared session/opening code — no structure-specific work.
- **Test pattern (FRG-19) is reusable:** `ScriptedTransport` (scripted
  `ModelResponse` queue + fallback + request log) and `RepoFixture`
  (temp-git repo, `mr.diff`, `Snapshot::capture`) live in the
  `sessions.rs` test module and are lens-generic. FRG-19's tests assert:
  tool pull happens and citation is recorded; opening context carries full
  redacted file contents with no `{file_contents}` stub; clean → `[]`;
  prose → `ProducerFailure`; `max_turns: 1` + infinite tool calls →
  `BudgetExhausted`; bad severity/citation/line-range rejected.

## Proposed direction

FRG-21 is a **prompt + thin-wrapper + fixture** ticket, not new machinery:

1. **Base:** branch from current `main` (`d82296f`; PR #22 and FRG-20
   merged). Linear branch name:
   `shippopotamus/frg-21-structure-lens-session-in-chomp`.
2. **Generalize minimally in `sessions.rs`:** extract the body of
   `run_correctness_session` into a private `run_lens_session(prompt,
   ...)` and keep `run_correctness_session` plus a new
   `run_structure_session` as one-line wrappers pinning their prompt
   consts. Everything else (outcome type, opening assembly, validation,
   budgets) is already shared. This is the smallest change that keeps
   FRG-22 from becoming a third copy.
3. **Rewrite `agent/prompts/structure.md`** to the FRG-19 prompt shape,
   keeping the structure charter (maintainability, layering fit,
   duplication, misplaced responsibilities; defects/security and
   rule-enforcement stay in the correctness/contract lanes):
   - Drop the `{diff}`/`{file_contents}`/`{ticket}` template sections;
     opening context is host-assembled, exactly as correctness.
   - Name the tools; state changed files are already in full.
   - **Structure-flavored pull-and-cite rule:** any "already exists at X"
     or sibling-organization/layering claim must be pulled (`grep` /
     `read_file` / `read_symbol`) and listed in `citations` as
     `path:line`; uncited relational claims are unverified downstream
     (FRG-26 rejects them).
   - Output: same 11-field contract, at most 10 findings,
     severity-ordered, `[]` when clean; category as open naming with
     structure examples (e.g. Duplication, Layering, Maintainability),
     mirroring FRG-19's move away from a closed vocab (`Finding.category`
     is a free `String`).
   - Contract entry content is *input* to judgment, not rules to enforce
     (that is FRG-22's lane) — stated in the prompt.
   - Update the provenance header to "lens behavior defined in FRG-21"
     (the `system_prompt()` strip already handles it).
4. **Tests beside the FRG-19 tests** (scripted transport, no live model):
   duplication fixture (base commit has an existing helper; diff
   reimplements it; script greps/reads the helper, then emits a finding
   citing it — assert citation + that the helper body was not in the
   opening context), false-duplication discipline (assert the citation
   contract path, not model behavior), clean fixture → `[]`, and reuse of
   the existing prose/budget/validation coverage through the shared code
   (add structure-named cases only where behavior could diverge).
5. **No validator change:** uncited "already exists" findings still parse;
   rejection is synthesis's job (FRG-26). Citation discipline is enforced
   by prompt + fixture assertions, per the ticket.

## Alternative considered

**Copy-paste `run_structure_session` as a second full implementation**
(zero refactor of FRG-19 code). It loses: ~250 lines duplicated on day one,
a third copy coming in FRG-22, and every future fix (budget mapping,
paging, validation) landing two or three times. The ticket says "only the
charter/prompt differs" — a private shared body with two named wrappers is
that statement made literal, and FRG-19's tests pin the shared behavior so
the extraction is low-risk.

## Risks / unresolved

- ~~Sequencing on PR #22~~ resolved: PR #22 merged; base is `main`.
- **CLI wiring scope is genuinely ambiguous** (see open questions): FRG-19
  left `main` single-lens on purpose ("Fan-out … land in their own
  tickets" = FRG-24), but FRG-21's acceptance says the session "runs end
  to end on a fixture PR," which the scripted-transport tests satisfy
  without touching `main`.
- Ticket-file context: the old structure prompt consumed `{ticket}`; the
  FRG-19 opening context has no ticket section for any lens. Parity says
  drop it until ticket plumbing lands universally.
