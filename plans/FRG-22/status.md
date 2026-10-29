# Status: FRG-22

## Phase Checklist
- [x] Research — vision approval: approved 2026-10-29
- [x] Plan
- [x] Implementation
- [x] Review — rounds: 2/3, result: clean (round 1: one MINOR, fixed; round 2: verified)
- [ ] Ship — PR: pending creation

## Current Gate
none

## Next Phase
Ship

## Notes
- 2026-10-29: Ticket opened from Linear FRG-22 (contract lens
  session). Base: main @ 6f2ed9a incl. FRG-21 (PR #26 merged).
  Blocked-by FRG-20 is Done; blocks FRG-24.
- 2026-10-29: Research complete (`research.md`). Three scouts +
  orchestrator spot-checks. Direction: FRG-21-pattern wrapper +
  new contract.md + host short-circuit on Absent. One gate
  question: stand-down note channel now vs FRG-24.
- 2026-10-29: Vision approved (incl. stand-down split: absent =
  host short-circuit + FRG-20's recorded entry_absent event;
  uncheckable = prompt [], note channel deferred to FRG-24).
  Plan written (plan.md, chunks 0-4, D1-D6). Plan calls made
  alone: category anchors (Contract violation / Contract edit),
  contract severity guide, no Linear mirroring (D6 precedent).
- 2026-10-29: Chunk 1 done (`agent/src/sessions.rs` only:
  `CONTRACT_PROMPT` const, `run_contract_session` with Absent
  short-circuit + FRG-24 allow comment, module doc updated).
  `cargo test --locked` green (112+34+6+3, 1 live ignored),
  `cargo clippy --all-targets -- -D warnings` clean,
  `cargo fmt --check` clean. One fix during work: `Absent` is a
  struct variant, matcher is `Absent { .. }`.
- 2026-10-29: Chunks 2+3 done (`agent/prompts/contract.md`
  authored per D4, 8 `contract_*` tests in `sessions.rs` test
  module, all scenarios as written, no weakening).
  `cargo test --locked` green (112+42+6+3, 1 live ignored),
  `cargo clippy --all-targets -- -D warnings` clean,
  `cargo fmt --check` clean after one `cargo fmt` pass.
  Uncommitted; review/ship left to orchestrator.
- 2026-10-29: Chunk 4 gates re-run by orchestrator: cargo test
  112+42+6+3 green (1 live ignored), clippy -D warnings clean,
  fmt clean, python unittest 168 OK.
- 2026-10-29: Review (orchestrator). Round 1: one MINOR —
  prompt example cited the governing entry by line (AGENTS.md:3),
  unverifiable against head; fixed (entry rules identified by
  verbatim quote; routed docs cited by path:line). Round 2
  verified. No architecture questions. Residual: unwired until
  FRG-24 (D1); uncheckable stand-down note deferred to FRG-24
  (D2, human-approved); live judgment unproven (FRG-29).
