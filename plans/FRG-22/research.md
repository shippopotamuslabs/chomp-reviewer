# Research: FRG-22 — Contract lens session

Date: 2026-10-29. Base: `main` @ `6f2ed9a` (incl. FRG-21 PR #26).
Three read-only scouts (sessions/contract, prompts/proposal, test
harness); orchestrator spot-checked the decisive claims
(`LensOutcome`, `recorded_event`, opening-context rendering,
decision-log wiring in `main`).

## What exists today

### Session construction (settled, reuse as-is)

- `run_lens_session(prompt, ...)` shared body (FRG-21 extraction),
  `run_correctness_session` / `run_structure_session` one-line
  wrappers (`agent/src/sessions.rs:72-115`). Third lens = one
  `CONTRACT_PROMPT` const + one wrapper, `#[allow(dead_code)]`
  until FRG-24 (same as structure).
- Opening context already renders the governing entry under
  `# Contract entry ({file}) — governing copy from the base
  branch` (`sessions.rs:265-286`). `Absent` renders nothing.
- `LensOutcome` (`sessions.rs:60-65`): `Completed(Vec<Finding>)`
  | `ProducerFailure` | `BudgetExhausted`. **No note channel.**
- `main` calls only correctness; structure is deliberate dead
  code until FRG-24.

### Contract resolution (FRG-20, done)

- `ContractResolution::Governing { entry_file, content,
  truncated_from_lines }` | `Absent { entry_file, reason }`
  (`agent/src/contract.rs:126-144`). Content only — **no routing
  parse, no checkability judgment** (by §8.7 rule 2 design:
  routed material is pulled by the lens at head via tools).
- `recorded_event()` (`contract.rs:164-193`): `Absent` →
  decision-log host event `entry_absent` ("contract channel
  empty"); truncated → `entry_truncated`; clean governing →
  `None`. `main.rs:242-246` already writes this into the
  decision log (`outputs::host_event("contract", ...)`).
  **The absent-case stand-down note already exists host-side.**

### Prompt

- `agent/prompts/contract.md` is a 5-line FRG-15 stub. This
  ticket authors it fresh ("Lens C"), mirroring the shipped
  skeleton: Posture → Your lane → Context → Tools/pull-and-cite
  → Grounding → Output.
- Proposal §8.5/§8.7 (docs/forge-2.0-architectural-proposal.md
  :343, :373-378): stated rules only, rule quoted + code cited,
  no generic best practice, routed pulls at head, contract-edit
  detection its own category, stand-down legitimate. §12 class 3
  (:474) = contract-only proof incl. the self-excusing edit.

### Test harness

- Scripted transport + `RepoFixture` (single base commit, head =
  dirty tree, `git diff` unstaged). "PR edits AGENTS.md AND adds
  violating code in one diff" is expressible today (channel
  test precedent, `sessions.rs:792-854`): base copy hand-built
  as `ContractResolution::Governing` literal, head copy in the
  tree/diff, tools serve head.
- All four acceptance fixtures are expressible as scripted
  session tests; "no finding for unstated rule" is asserted as
  citation discipline + scripted `[]` (model judgment itself is
  not testable without a live model — same posture as
  FRG-19/21).

## The one real unknown: the stand-down note

Ticket: absent entry **or** "routes nowhere and states nothing
checkable" → `[]` plus a *recorded note*. Neither the ticket nor
the proposal (§8.5, §8.7.4, D1) defines where the note lives.
Facts:

- Absent case: already recorded by FRG-20 (`entry_absent`
  decision-log event) regardless of any lens. The lens adds
  nothing to record.
- Uncheckable-governing case: only the model can judge
  "nothing checkable" — recording it needs a note channel out
  of the session. `LensOutcome` has none; `main` builds
  decision entries only from the resolution; and the contract
  session (like structure) won't be called by `main` until
  FRG-24, whose charter includes per-lens outcome recording
  ("outage recording").

## Proposed direction

1. **Wrapper + prompt, FRG-21 pattern.** `run_contract_session`
   delegating to `run_lens_session(CONTRACT_PROMPT, ...)`; new
   `contract.md` carrying the hard rules verbatim in substance:
   stated rules only, quote-the-rule + cite-the-code, generic
   best practice explicitly forbidden ("invented rules are the
   worst failure mode"), routed pulls at head, contract-edit
   detection as its own category, stand-down to `[]`.
2. **Host short-circuit for `Absent`.** In
   `run_contract_session` only: `Absent` → return
   `Completed(vec![])` *without driving a session* (zero model
   turns; test asserts the transport saw no requests). The
   recorded note is FRG-20's `entry_absent` event, already in
   the decision log. Deterministic stand-down per §8.7.4
   ("contract lens stands down (recorded), other lenses
   proceed").
3. **Uncheckable governing copy: prompt-level stand-down.**
   Session runs, model returns `[]`. No new note channel in
   this ticket; a first-class per-lens note/outcome record
   belongs to FRG-24 fan-out (it owns wiring all three
   outcomes into outputs). This ticket's test asserts `[]`;
   the plan will name the note gap explicitly as deferred,
   not pretend it away.
4. **`main` unchanged** (correctness-only until FRG-24, your
   FRG-21 call carried over). Targeted `#[allow(dead_code)]`.
5. **Tests** (sessions.rs): stated-rule violation caught with
   rule text in evidence + code in citations; violation +
   self-excusing contract edit → both findings (governing base
   copy vs head edit in diff, channel-test fixture shape);
   routed-doc pull (entry routes to `docs/rules.md`, scripted
   `read_file` at head, rule from routed doc enforced);
   unstated plausible rule → scripted `[]` + prompt-shape
   assertion; absent → `Completed([])` with zero transport
   requests; uncheckable governing → `[]`; budget parity.

## Alternative considered

Add a note channel now — e.g. `LensOutcome::StoodDown { note }`
or `Completed { findings, note: Option<String> }` — so the
uncheckable case records a note at session level. Loses here:
it changes the shared outcome type for all three lenses and
`main`'s mapping for a consumer that doesn't exist until
FRG-24; FRG-24 will design per-lens outcome recording once,
for all lenses (including outages). Building half of it now
risks designing it twice. (If you disagree, this is cheap to
reverse at plan stage — it's the gate question below.)

## Open questions

1. **Stand-down note scope** — accept the split above (absent =
   host short-circuit + FRG-20's existing recorded event;
   uncheckable = prompt `[]`, note plumbing deferred to
   FRG-24)? Or do you want the `LensOutcome` note channel
   built in this ticket?

## Risks I could not resolve

- None blocking. Note: "does not invent law" is ultimately
  model behavior; like FRG-19/21, tests pin the prompt and
  the parse/discipline, not a live model (live proof is
  FRG-29's class-3 run).
