# FRG-23: PR-Agent adapter: invoke against the real PR URL, wire repo_context_files and artifacts, capture the guide

Linear: https://linear.app/shippopotamus-labs/issue/FRG-23
Status at intake: Backlog. Parent: FRG-12 (Forge Reviewer 2.0 epic).
Blocked by FRG-15 (In Review; scaffold landed as `chomp`).
Blocks FRG-27, FRG-24. Related: FRG-17, FRG-28, FRG-26.
Branch (Linear gitBranchName): shippopotamus/frg-23-pr-agent-adapter-invoke-against-the-real-pr-url-wire

(The ticket body below is the human's vision for the Synthitect gate, verbatim in substance; Linear issue references rendered as FRG-xx keys.)

---

Repo: shippopotamuslabs/forge-reviewer (`agent/src/pr_agent.rs`). Proposal §8.6 Move 1, grounded in §5.1. PR-Agent's role changes from peer producer to instrument. This ticket is Move 1 — the deterministic run, wired correctly for the first time.

## Grounding: why the current wiring is broken (§5.1)

Verified at source level against the pinned PR-Agent 0.46.0 wheel:

* PR-Agent has **no tool-calling surface**: zero `tool_calls`/`function_call`/`mcp` in the package; its AI handler is `chat_completion(model, system, user)` — strings in, string out. `skills_loader.py` states it: *"PR-Agent runs single-shot model calls and has no tool-use loop."* All of its extensibility is static front-loading: `extra_instructions`, Jinja prompt templates, `[skills]`, `[artifacts]`, and `repo_context_files`.
* `repo_context_files` (default `["AGENTS.md"]`, capped by `repo_context_max_lines`, default 500) is read from the **default branch** via the git provider's API.
* **It is inert in Forge today:** Forge invokes `pr_agent.cli --diff-file=…`, which selects `PlainDiffGitProvider`; that provider does not implement `get_repo_file_content`, so the feature logs a warning and returns empty. Forge does not currently get even the static AGENTS.md front-load it appears to have.

## What this builds (Move 1)

* The host invokes PR-Agent **once per review, unmodified**, as a subprocess — but against the **real PR URL** through the GitHub provider rather than `--diff-file`, so `repo_context_files` functions for the first time.
* Config wiring: `repo_context_files` is supplied from Forge's config mapping (§8.7) — the `pr_agent_context_files` setting, defaulting to the configured entry file (default `AGENTS.md`). Corrected 2026-10-28: this setting already landed as `ForgeConfig::pr_agent_context_files` in `agent/src/contract.rs` (defaults to the entry file); this ticket wires it through to the PR-Agent invocation, it does not author the config surface.
* `[artifacts]` injection: the host-generated **repo map** (FRG-17; FG-3 in proposal §14) is injected as the pre-produced context file — the one static channel PR-Agent has, used for the one artifact the host can compute better than PR-Agent can guess. Corrected 2026-10-28: the render seam already exists as `RepoMap::render()` in `agent/src/snapshot.rs` (one `path (size)` line per entry); this ticket consumes it for `[artifacts]` injection, it does not design the map shape.
* The guide text is captured verbatim to the `--out-pr-agent` output path and passed to synthesis as evidence, as in 1.x. Corrected 2026-10-28: the output contract already exists — `chomp review --out-pr-agent <path>` is parsed in `agent/src/main.rs` and the scaffold `write_outputs()` in `agent/src/outputs.rs` writes the (currently empty) guide file; this ticket replaces the empty write with the captured guide, byte-for-byte.
* PR-Agent's Python environment setup (currently a workflow step) is driven by this adapter or remains workflow-managed — either way it stays the workflow's one Python dependency alongside the posting layer (workflow wiring in FRG-28; FG-14 in proposal §14).
* **Fail-soft, as today:** a PR-Agent failure records a producer outage (consumed by the FRG-24/FRG-26 decision log; FG-10/FG-12 in proposal §14) and synthesis proceeds. A PR-Agent failure must never block the agentic review — the reverse of 1.x's dependency shape (§15 risk note).

## Explicit non-actions

PR-Agent is not forked, not patched, and not given tools (non-goal NG2). If a future PR-Agent release adds a genuine tool loop, this adapter is the seam where Forge adopts it — the subprocess boundary makes that a configuration question instead of a redesign. (License note: the 0.46.0 wheel is MIT, used unmodified as a subprocess.)

## Acceptance criteria

* In CI on a scratch repo, the adapter produces a PR-Agent guide for a real PR URL — not a diff file.
* Repo context is **verifiably present**: the run's evidence shows the entry file's content reached PR-Agent (e.g. a fixture AGENTS.md containing a distinctive instruction that the guide demonstrably reflects, or provider-level logging captured in the run artifacts). The 1.x silent no-op must be provably dead.
* The repo map artifact is injected via `[artifacts]` and visible in the run record.
* Guide text lands byte-for-byte at the `--out-pr-agent` path.
* Failure injection (bad model config / killed subprocess) → outage recorded, adapter returns a producer-failure result, chomp run continues.
