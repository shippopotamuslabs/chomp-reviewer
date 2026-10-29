# Status: FRG-21

## Phase Checklist
- [x] Research — vision approval: approved 2026-10-29
- [x] Plan
- [x] Implementation
- [x] Review — rounds: 2/3, result: clean (round 1: one MINOR
  plan-fidelity finding, fixed; round 2: verified, no findings)
- [ ] Ship — PR: pending creation

## Current Gate
none

## Next Phase
Ship

## Notes
- 2026-10-29: Ticket opened from Linear FRG-21 (structure lens session).
  Blocked by FRG-19, which is In Review with PR #22 open — FRG-21 must
  reuse, not re-derive, the FRG-19 session-construction pattern.
- 2026-10-29: PR #22 merged (`a9735a7`); `main` now `d82296f` incl.
  FRG-20 (base-branch contract resolution; sessions take
  `&ContractResolution`). Research updated; base = `main`. Remaining
  vision-gate questions: CLI wiring now vs FRG-24, and confirm the
  `run_lens_session` extraction.
- Current checkout: branch
  `shippopotamus/frg-18-read-only-tool-executor-for-chomp-grep-read_file-list_dir`;
  `agent/src/sessions.rs` and `agent/src/host.rs` are still scaffolds here.
  `agent/prompts/structure.md` already exists.
- 2026-10-29: Research complete (`research.md`). Four read-only scouts over
  the working tree + `origin/pr-22-frg19`; orchestrator spot-checked the
  decisive claims (FRG-19 session signature/outcome enum, main.rs wiring,
  structure.md placeholders, correctness.md pull-and-cite lines).
  Direction: minimal `run_lens_session` extraction + structure prompt
  rewrite + duplication fixture tests. Awaiting vision approval.
- 2026-10-29: Vision approved. Decisions: (1) `main` stays
  correctness-only until FRG-24 fan-out; (2) shared `run_lens_session`
  extraction confirmed. Plan written (`plan.md`, chunks 0-4, decisions
  D1-D6). Plan made three calls alone: targeted `#[allow(dead_code)]`
  on `run_structure_session` until FRG-24 (D2 consequence), open
  category naming + structure severity guide in the prompt (D4), no
  Linear sub-issue mirroring per FRG-19/FRG-20 precedent (D6).
- 2026-10-29: Chunk 1 done (`agent/src/sessions.rs` only): added
  `STRUCTURE_PROMPT` const, extracted behavior-identical
  `run_lens_session`, added `run_structure_session` with targeted
  `#[allow(dead_code)]` (no const-level allow needed — clippy clean),
  parameterized `system_prompt(prompt: &str)`, adapted only the
  provenance test, module doc now says structure lands here. Gates in
  `agent/`: `cargo test --locked` green (112 + 29 + 6 + 3 passed,
  1 live ignored), `cargo clippy --all-targets -- -D warnings` clean,
  `cargo fmt --check` clean. Nothing committed.
- 2026-10-29: Chunks 2-3 done. Chunk 2 rewrote
  `agent/prompts/structure.md` per D4 (provenance header `defined in
  FRG-21`, posture + trimmed non-negotiable standards, lane/context/
  pull-and-cite/grounding/output sections mirroring correctness.md,
  structure severity guide, no template tokens). Chunk 3 added 5
  `structure_*` tests in `agent/src/sessions.rs` (grep-then-read
  duplication pull-and-cite, clean call, uncited-still-parses, budget
  parity, prompt shape) with a pinned `src/util.rs` fixture
  (`slugify` on line 2). Gates in `agent/`: `cargo test --locked`
  green (112 + 34 + 6 + 3 passed, 1 live ignored),
  `cargo clippy --all-targets -- -D warnings` clean,
  `cargo fmt --check` clean (one `cargo fmt` reflow applied). No
  production code changes; no deviation from plan (grep-then-read
  used as written). Nothing committed.
- 2026-10-29: Chunk 4 gates re-run by orchestrator: `cargo test
  --locked` (112 + 34 + 6 + 3 passed, 1 live ignored), clippy
  `-D warnings` clean, fmt clean, `python3 -m unittest discover
  -s tests` 168 OK.
- 2026-10-29: Review (orchestrator, fresh to the code). Round 1:
  one MINOR — structure.md Context section omitted plan D4's
  base-branch governing-copy note; fixed by implementer, verified
  in round 2 (5 structure_* tests green in orchestrator rerun).
  Also corrected plan Ship section: `agent/` tickets cut no
  changelog/tag (FRG-19/FRG-20 precedent; releases govern
  `.forge/`). No architecture questions. Residual risk: structure
  session unreachable from CLI until FRG-24 (by D2 design; proven
  via scripted transport, no live-model run in this ticket).
