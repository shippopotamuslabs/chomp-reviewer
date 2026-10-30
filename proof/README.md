# Forge 2.0 proof suite (FRG-29, proposal §12)

Purpose-built defects for the §12 hypothesis — repository access
catches real defects that diff-only review cannot — plus the harness
that runs both generations over them. Gates FRG-30; the scored
report attaches to FRG-29.

## Layout

- `scratch/` — the scratch repo tree. Sean creates the scratch repo
  from it (`git init`, commit, push as `shippopotamuslabs/<scratch>`):
  a Python mini-service with caller/lib/consumer split, a stated
  contract (`AGENTS.md` + routed `docs/db-conventions.md`), and a
  `.pr_agent.toml` so the 1.x producer runs.
- `cases/<name>.json` + `<name>.diff` — 8 cases in four classes
  (plus the structure-lens explicit case and the no-invented-law FP
  check). Each JSON states the defect, the diff, and both
  generations' expectations (`must_cite` files, `must_mention`
  words, FP budget share).
- `run_proof.py` — the harness (stdlib only):
  - `--list` — list cases.
  - `--check` — validate fixtures (tree, diffs apply, JSON
    schema, expectation references). No keys, no model calls.
  - `--run [case ...]` — live evaluation (needs
    `OPENROUTER_API_KEY` and `--chomp` pointing at a release
    build). Per case: fresh scratch clone, apply diff, run 1.x
    (PR-Agent plain-diff + thermos x2 + master LLM `--dry-run`) and
    2.0 (`chomp review`), record `results/<case>/`, run the
    mechanical drop audit, and update `report.json`.
- `results/` (gitignored) and `report.json` — measurements plus
  `verdict_1x/verdict_2x/fp_1x/fp_2x` fields the operator fills in
  from the recorded outputs. Mechanical hints (`hints_1x/hints_2x`)
  precompute which expectations are textually satisfied.

## Running the live evaluation (Sean)

1. Create the scratch repo from `proof/scratch/` and push it.
2. `cargo build --release` in `agent/`.
3. Confirm the spend: default model is Spark
   (`openrouter/meta/muse-spark-1.3-contributor` unless `FORGE_MODEL`
   overrides); 8 small-diff cases, single-digit dollars at Spark
   rates — confirm before running anything bigger.
4. `python3 proof/run_proof.py --run --chomp agent/target/release/chomp`
   with `OPENROUTER_API_KEY` in the environment.
5. Score `proof/report.json` per class (caught/missed/FP), run the
   drop audit by reading each non-kept disposition's reason, attach
   the report to FRG-29. Gates (§12): classes 1–3 materially higher
   for 2.0, no class-4 regression, ≤1 FP per proof PR, cost vs the
   §10 model.

## Status

Fixtures + harness landed stub-verified (the 2.0 path runs against a
stub model server; the 1.x path needs the live key by construction).
The live run, the scored report, and the gate decision wait on the
scratch repo, the key, and the model/spend confirmation.
