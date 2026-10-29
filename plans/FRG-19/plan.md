# FRG-19 — Correctness lens session end to end

Ticket text governs; this plan records the calls the ticket left open.

## Decisions

- **Open-time reads go through the FRG-18 executor** (the ticket requires
  picking one path and asserting it in tests). Rationale: the executor is
  the uniform, SHA-stamped path and — decisive here — it redacts.
  Opening context goes to the model verbatim; direct fs reads would
  bypass the redaction layer and could carry secrets into the session.
  Test assertion: a changed file containing a secret shape appears in
  the opening context only as `<redacted>`. Full contents are assembled
  by paging `read_file` windows (200 lines per call) until `total_lines`
  is covered; a soft failure (deleted/unreadable/binary) renders a note
  in place of the contents.
- **Driver and session construction stay in the binary target**
  (`src/host.rs`, `src/sessions.rs`), where FRG-15 scaffolded them.
  Acceptance tests are in-module tests with a scripted fake
  `ModelTransport`; no live OpenRouter call is needed (or possible —
  no API key in this environment, same constraint as FRG-16).
- **System prompt is embedded** with `include_str!("../prompts/correctness.md")`
  (the file remains the authored source; a shipped binary cannot rely on
  a source-tree-relative runtime path). A leading HTML provenance
  comment is stripped before the prompt is sent.
- **Outcome type** (`sessions::LensOutcome`): `Completed(Vec<Finding>)`,
  `ProducerFailure { reason }`, `BudgetExhausted { reason }`. Only
  `Completed` writes a findings file — a producer failure or budget
  exhaustion maps to exit 2 in `main` with the reason on stderr, never
  a fabricated `[]` (ticket: never a silent empty result).
- **Findings validation is strict**: bare JSON array, required fields,
  severity in {Critical, High, Medium, Low}, line range sane,
  `citations` (when present) an array of `path:line` strings. Anything
  else is a producer failure.
- Terminal text comes from `export_snapshot`'s last assistant message
  after `Finished`.

## Shape

- `host.rs`: `drive_session(engine, handle, transport, executor) ->
  DriveOutcome::{Finished, Failed(String)}`; step-capped; CallModel via
  the port, failures fed back with `to_failure_report()`; ExecuteTools
  through the executor; RequestSummary fails loud (lens sessions
  disable summarization; evidence is never summarized silently).
- `sessions.rs`: `run_correctness_session(snapshot, diff_text, config,
  transport, executor, agent_config) -> LensOutcome`; opening-message
  assembly; session construction per the ticket sketch (FRG-18 schema
  text + typed catalog, agnostic base, summarization off, Structured).
- `outputs.rs`: `Finding` type + `write_outputs` takes the findings.
- `main.rs`: `run_review` wires snapshot -> executor -> transport ->
  session -> outputs; non-Completed outcomes are orchestration errors.
- `prompts/correctness.md`: rewritten per the parked prompt audit
  (grounding contract, one output format with matching example,
  <=10 severity-ordered cap, no behavior-preserving wording, anti-quota
  lines kept) plus the ticket's pull-and-cite instruction.
