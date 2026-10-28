# FRG-15 — Scaffold the chomp agent crate (plan)

Ticket: FRG-15 (FG-1). Governing text: ticket body + proposal §8.1, §8.2, §8.7, §11.
Product renamed by Sean 2026-10-28: Forge Reviewer → **Chomp**. This crate is
born under the new name: package `chomp`, binary `chomp`, help/docs say Chomp.
Interface-layer names stay until FRG-52's deferred layer: repo name, the
`.pr_agent.toml` filename, the `[forge]` table name, `FORGE_MODEL` env.

## Reconciliations (code wins over proposal where they disagree)

1. **host.rs**: proposal §8.1 describes host.rs as implementing agent_core's
   AgentTransport / AgentToolExecutor / AgentSummarizer traits. Those traits
   no longer exist — ENC-285 deleted the callback host model; agent_core is
   pull-based (`create_session` + `next_action`, verified on main 05eafa8).
   host.rs is therefore a documented skeleton for the host driver (the
   pull-loop that feeds transport/tool/summarizer results back), no trait
   impls invented. Real hosting lands with FG-2/FG-4/FG-5.
2. **agent_core dependency**: public repo; pin by git rev to main 05eafa8
   (FRG-51 merge) with `default-features = false`. Acceptance check is real:
   `cargo tree` must show no uniffi crates.
3. **TOML parsing**: §8.1's dependency list (serde, serde_json, ureq, regex)
   has no TOML parser and additions need ticket-level justification. The
   [forge] surface is two keys. Decision: a strict, section-scoped,
   hand-rolled parser for exactly that table (quoted string; array of quoted
   strings; comments; unknown keys in [forge] are an error so adopter typos
   fail loud; other tables skipped line-wise). Pinned by fixture tests.
   If Sean prefers the `toml` crate, that is a one-line plan change.

## Build chunks

- **C1 crate skeleton**: agent/Cargo.toml (package chomp, bin chomp; deps
  agent_core git-pinned no-default-features, serde, serde_json, ureq, regex),
  src/ modules per §8.1 (main, host, transport, tools, snapshot, contract,
  sessions, pr_agent, synthesis, outputs) — non-owned modules are doc-comment
  skeletons naming their landing ticket; prompts/ populated: correctness.md
  ← .forge/prompts/thermos-correctness.md, structure.md ← thermos-structure.md,
  synthesis.md ← master-dedup.md (each with a one-line provenance header),
  contract.md a stub stating FG-8 (FRG-22) authors it.
- **C2 CLI + orchestration (main.rs)**: `chomp review` with the §8.2 flags
  (--repo, --diff, --base-ref, --pr-url required; --config, --ticket-file
  optional; --out-findings, --out-decisions, --out-pr-agent required outs).
  Hand-rolled arg parsing (no clap — dep discipline), `--help` prints the
  full contract. Stage flow for the scaffold: parse args → load config →
  validate inputs (repo dir exists, diff file readable) → write outputs →
  exit 0. Validation/orchestration failure → stderr message + exit 2.
  No panics on malformed input anywhere (typed errors, exit 2).
- **C3 config (contract.rs)**: ForgeConfig { entry_file, pr_agent_context_files }
  with defaults AGENTS.md / [entry_file]; precedence: --config file >
  <repo>/.pr_agent.toml > defaults (the --config channel is how workflow
  inputs override, per D2). Parser per reconciliation 3.
- **C4 outputs (outputs.rs)**: findings file = `[]` (legitimately empty review,
  existing schema is an array); decisions file = minimal versioned object
  {"version": 1, "entries": []} (FG-11/12 evolve it); pr-agent out file =
  empty file when requested. Writes are atomic-ish (write + rename not
  needed at scaffold stage; plain write, errors → exit 2).
- **C5 tests**: cargo tests in agent/ — config parser (defaults, file values,
  precedence, malformed [forge] → error, foreign tables ignored), CLI parse
  (missing required flag → exit 2 path), end-to-end fixture run in a temp
  dir asserting exit 0 + all three outputs, and a failure fixture (missing
  diff file → exit 2, no outputs written). Acceptance sweep: release build,
  cargo tree uniffi check, --help output captured in a test or verified
  manually and logged in status.md.

## Out of scope (named, not smuggled)
Transport behavior (FRG-16), snapshot (FRG-17), tools (FRG-18), sessions
(FRG-19/21/22), contract resolution beyond config parsing (FRG-20), PR-Agent
adapter (FRG-23), synthesis (FRG-25/26), workflow wiring (FRG-28), CHANGELOG
+ release (ticket defers to FG-18/FRG-32 — overrides the repo AGENTS.md
release flow for this change), repo Python suite untouched.

## Gates
cargo build --release; cargo test (agent/); cargo tree | grep -i uniffi must
be empty; fmt + clippy -D warnings on the agent crate; repo python suite
still green (untouched, verified once); fresh-context review; PR with repo
title convention `feat(agent): ... (FRG-15)`; Forge threads dispositioned;
CI green before merge.
