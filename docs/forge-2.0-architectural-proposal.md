# Forge Reviewer 2.0 — Repo-Aware Agentic Code Review

**An architectural proposal**

- **Status:** Ratified by Sean on 2026-10-26. Section 14 is the ticketing list; the work is filed as tickets in Linear (Forge Reviewer team) under a parent epic for this proposal.
- **Date:** 2026-10-26
- **Author:** Tensor, from Sean's design direction (2026-10-26, ~03:00–07:45 EDT)
- **Scope:** forge-reviewer (GitHub reusable workflow + `.forge/` producers), agent_core (Rust crate), consumer repos (Stratum, Bloviate, Enclave, and third-party adopters)
- **Supersedes:** the diff-only producer architecture of Forge 1.x (current release v1.1.5)

---

## 1. Executive summary

Forge 1.x reviews a pull request the way a person skims a diff in a hurry: it sees the changed lines and nothing else. Its three producers — Qodo PR-Agent and the two Thermos lenses (correctness, structure) — are all single-shot, diff-only model calls fused by an LLM master into one consolidated review. That architecture has a hard ceiling. A change can be locally impeccable and globally wrong: it breaks a caller two files away, violates an invariant maintained elsewhere, or contradicts the repository's own documented contract. None of that is visible in a diff.

Forge 2.0 removes the ceiling by giving the reviewer the repository.

The design, in one paragraph: a new Rust binary, **forge-agent**, hosts Sean's existing **agent_core** crate (the agent loop Enclave runs in production) with UniFFI disabled. A deterministic Rust orchestration layer checks out the PR, resolves the repo's contract starting from its root agent file (`AGENTS.md` by default), and spawns agent_core sessions in parallel — one per Thermos lens, each equipped with read-only repository tools (`grep`, `read_file`, `list_dir`, `read_symbol`). PR-Agent runs once alongside them as an external producer, demoted from peer reviewer to instrument. All producer results flow to a final agent_core session, the **synthesizer**, which can itself grep the repository to verify contested findings before merging them, and emits the single consolidated review through Forge's existing, tested Python posting and verification layer, which does not change in v1 of this plan.

Three findings from the preparatory audits make this plan cheaper than it looks:

1. **The full repository is already there.** The Forge GitHub workflow already checks out the PR head. The reviewers simply never look at it. No Docker container and no new cloning step are required to give them the tree.
2. **PR-Agent cannot be extended, and does not need to be.** PR-Agent 0.46.0 has no tool-calling surface of any kind (verified at source level, Section 5.1). Its extensibility is static context front-loading. Forge 2.0 therefore orchestrates *around* it rather than through it.
3. **agent_core is already generic.** Its UniFFI dependency is confined to a mirror bridge layer and can be feature-gated in roughly four files; its Enclave-specific content is almost entirely consumer-owned config and test fixtures, with exactly two hardcoded string checks leaking into engine logic (Section 5.2). The total pre-adoption cleanup is an afternoon of work, and both changes improve the crate for Enclave too.

The expected result: a reviewer that traces upstream and downstream data flow before it speaks, checks the diff against the repository's own stated rules as a first-class review dimension, and supports its findings with citations to code it actually read — while remaining a generic product that works on any repository, not a tool shaped around Sean's repos.

---

## 2. Problem statement

### 2.1 What diff-only review cannot see

A diff shows what changed. Review quality depends on what did not change but interacts with the change:

- **Upstream:** who calls this function, with what values, under what assumptions. A signature-preserving change to a function's semantics is invisible to a diff reviewer and obvious to anyone who reads the call sites.
- **Downstream:** what consumes this output, what invariants they rely on, what breaks when the output shape or timing shifts.
- **Contract:** what the repository's own documentation says the rules are — layering constraints, testing conventions, forbidden dependencies, style law. A diff reviewer without the contract can only apply generic taste.
- **Convention in code:** how sibling code solves the same problem. The most common real review comment on any team is "we already have a way to do this, over there."

Human reviewers compensate by knowing the codebase or by clicking through it during review. Forge 1.x can do neither.

### 2.2 Evidence from Forge 1.x itself

The current system already *aspires* to context it does not have. The Thermos correctness prompt instructs the model to review "the full contents of changed files" and to "follow data flow one hop into callees/callers when needed" — but `thermos.py` substitutes the `{file_contents}` placeholder with the literal string *"(See diff above for changed lines. Full file contents not fetched in CI mode.)"* The prompt promises one-hop tracing; the plumbing delivers none. The model complies as best it can, which means it sometimes asserts upstream/downstream facts it has not seen. That is the worst of both worlds: the confidence of context with the evidence of a diff.

Separately, PR-Agent's one genuinely relevant context feature — `repo_context_files`, default `["AGENTS.md"]`, which inlines repo instruction files into the review prompt — is **silently inert in Forge's current invocation**, because Forge runs PR-Agent with `--diff-file`, which selects a plain-diff git provider that does not implement the file-fetch the feature requires (Section 5.1). Forge today does not even get the static AGENTS.md front-load it appears to have.

### 2.3 Why now

Two assets matured into place. First, the GitHub re-platform of Forge (October 2026) put Forge on ephemeral runners with a full checkout already present and a clean reusable-workflow shape. Second, agent_core — written for Enclave, running in production there — turned out, on inspection, to be a generic agent runtime with a clean native host surface. Forge 2.0 is the second production use that runtime was implicitly designed for.

---

## 3. Vision and design principles

**Vision.** Forge is a generic, repo-aware code reviewer for any repository that keeps an agent file at its root. It reviews the way a strong human reviewer with fresh eyes and fast hands does: read the repo's own instructions first, read the diff, pull the surrounding code the diff touches, check all three against each other, and report findings it can cite.

**Principles.**

1. **Generic by construction.** Nothing in Forge 2.0 may hardcode the contract structure, directory layout, language, or conventions of any specific repository, including Sean's. Repository specificity enters only through the repository itself: its agent file and whatever that file routes to.
2. **The agent file is the entry point.** Review begins at the repo's root agent file (default `AGENTS.md`). The reviewer reads it, follows its routing with tool calls, and pulls contract material lazily, as the diff demands — the way a human uses the file. Contracts are not front-loaded wholesale.
3. **Deterministic skeleton, agentic flesh.** Spawning, parallelism, budgets, data flow between stages, and posting are deterministic host code. Judgment — what to grep, which findings survive, how to phrase them — lives inside agent sessions. The model's freedom is concentrated where judgment is the product, and nowhere else.
4. **Read-only by construction.** The reviewer receives no shell, no write access, and no network access except its model endpoint. Read-only is not a policy the model is asked to follow; it is a property of the tool set it is given.
5. **Every claim cites evidence it actually pulled.** A finding that asserts an upstream or downstream fact must cite the file and line the reviewer read. Uncited relational claims are a synthesis defect.
6. **Never silently drop a finding.** Carried over from Forge 1.x as a hard rule: any producer finding the synthesizer merges, downgrades, or discards is recorded with a reason in a machine-readable decision log.
7. **Fail-soft producers, fail-visible orchestration.** As today, a failed producer never blocks a consumer repo's pipeline; but a failed *stage* of the orchestration is reported as such, never disguised as an empty review.
8. **Reuse before rewrite.** agent_core over a new agent loop; the existing Python posting/verify layer over a Rust rewrite; PR-Agent as-is over a fork. Each reuse is a deliberate scope decision, revisited only on evidence.

---

## 4. Goals and non-goals

### 4.1 Goals

- **G1.** Reviewer can read any file in the PR's repository at the PR head, via bounded, capped tools, driven by its own judgment within a fixed budget.
- **G2.** Contract compliance is a first-class review dimension: a dedicated lens session whose charter is the repo's own rules, discovered from the root agent file and its routing.
- **G3.** All Thermos lenses gain repository access, so correctness and structure findings can be grounded in callers, callees, and sibling code.
- **G4.** PR-Agent remains a producer, correctly wired for the first time (its static repo-context feature actually functioning), invoked deterministically once per review, with a bounded agentic follow-up path during synthesis.
- **G5.** Synthesis verifies before it merges: the synthesizer session can pull code to adjudicate conflicting or doubtful producer findings.
- **G6.** The existing review lifecycle is preserved exactly: review once on PR open, verify-and-resolve on subsequent pushes, one consolidated review, per-PR concurrency serialization.
- **G7.** agent_core gains a supported non-UniFFI build and loses its two Enclave-literal leaks, becoming a two-consumer crate without any change to Enclave's behavior or generated bindings.
- **G8.** Measurable improvement: Forge 2.0 demonstrably catches defect classes Forge 1.x cannot, on a purpose-built proof suite, at an acceptable false-positive rate (Section 12).

### 4.2 Non-goals

- **NG1.** Rewriting the GitHub posting, thread-resolution, or verify logic in Rust. The Python layer stays (Section 8.9).
- **NG2.** Forking or modifying PR-Agent.
- **NG3.** A codebase index, embedding store, or RAG service. Live grep over the checkout is the retrieval mechanism (rationale in Section 13).
- **NG4.** Reviewing with write access, suggesting applied patches, or auto-fixing. Forge reports; humans and coding agents fix.
- **NG5.** GitLab parity in v1. Forge 2.0 ships on the GitHub reusable workflow; the GitLab template continues on 1.x semantics until a separate decision.
- **NG6.** Docker packaging in v1. Reconsidered only if Forge moves to persistent self-hosted runners (Section 8.10).
- **NG7.** Multi-PR or repo-history awareness (blame, history search). The checkout is shallow by design in v1 (Open Decision D8).

---

## 5. Grounding: what the audits established

This plan rests on two source-level audits performed 2026-10-26. Claims below are from the code, not vendor documentation.

### 5.1 PR-Agent 0.46.0 (the pinned version in Forge's workflow)

- **No tool-calling surface exists.** Zero occurrences of `tool_calls`, `function_call`, or `mcp` in the package. The AI-handler interface is `chat_completion(model, system, user)` — strings in, string out — and its LiteLLM implementation never passes a `tools` argument. The authors state it in `skills_loader.py`: *"PR-Agent runs single-shot model calls and has no tool-use loop, so progressive disclosure is not implementable here."* Its "tools" are the slash-commands (`/review`, `/improve`, …), a hardcoded dispatch table.
- **All extensibility is static and front-loaded:** `extra_instructions` per command; replaceable Jinja prompt templates; `[skills]` (host-path `SKILL.md` text inlined into prompts); `[artifacts]` (one pre-produced CI file injected into the prompt); and `repo_context_files`.
- **`repo_context_files`** defaults to `["AGENTS.md"]`, is capped by `repo_context_max_lines` (default 500), and is deliberately read from the **default branch** via the git provider's API — a trust choice this plan adopts (Section 8.7). It is an explicit path list: no globbing, no following of pointers from AGENTS.md into other files.
- **It is inert in Forge today.** Forge invokes `pr_agent.cli --diff-file=…`, which selects `PlainDiffGitProvider`; that provider does not implement `get_repo_file_content`, so the feature logs a warning and returns empty. Activating it requires invoking PR-Agent against a real PR URL through an API-backed provider.
- **Commercial Qodo Merge is a different product.** Its context engine, multi-repo RAG, and MCP agentic tools are not in the OSS package; even there, custom tools are restricted to an admin allow-list. No part of this plan depends on commercial features.
- **License:** the 0.46.0 wheel is MIT. (This corrects Forge's older internal note describing PR-Agent as AGPL-3.0.)

### 5.2 agent_core (main @ 9ad9215, the HEAD Enclave consumes)

- **Shape:** a single library crate, v0.3.0, edition 2021. No binary except `uniffi-bindgen`. Two host surfaces: UniFFI/Kotlin for Android, and a native Rust surface that the README directs native hosts to use.
- **Native host surface:** three synchronous, object-safe traits in `src/host.rs` — `AgentTransport`, `AgentToolExecutor`, `AgentSummarizer`. The engine (`AgentEngine`, `src/engine/mod.rs`) is pull-based and host-driven: the host loops on `next_action()` (`CallModel` / `ExecuteTools` / `RequestSummary` / `Finished` / `Failed`) and submits results back. `examples/mock_desktop_loop.rs` (~100 lines) is a working native host.
- **Machinery included:** tool-call parsing with JSON repair and schema coercion; a text-envelope tool-call ingest path for models without native function calling; guardrails (max turns, tool budgets, soft-failure streaks); tiered context compaction (fifo / micro / truncate / summary); model-call retry with HTTP status handling.
- **Tools are host-owned, entirely.** Tool definitions are opaque JSON strings the host supplies (`tool_catalog_json`, `tool_schema_registry_json`, `tools_schema_json` in `SessionIoParams`); execution is the host's `AgentToolExecutor` receiving `ToolCallRecord { call_id, name, arguments_json }`. The crate ships no file, grep, or shell tools.
- **No provider code.** The crate contains zero HTTP and zero inference code; "provider" is a label string. In Enclave, the Kotlin host implements `AgentTransport`. A CI consumer must write that transport itself (Section 8.3) — the plan's single largest build item.
- **Build weight:** light. Direct dependencies are serde, serde_json, thiserror, strsim, log, tiktoken-rs, once_cell, regex, uuid, and uniffi 0.29. No tokio, no native dependencies, no llama.cpp.
- **UniFFI confinement:** 56 `uniffi` mentions in `src/`: 51 in `src/ffi/` (5 files, 2,065 lines, ~11% of the crate), 4 in `src/lib.rs` (the `setup_scaffolding!()` macro, the module declaration and re-exports, three `#[uniffi::export(callback_interface)]` traits), 1 in the bindgen binary. Zero UniFFI annotations on core types. The FFI layer is a parallel mirror — distinct `Ffi*` types with 58 `From`/`Into` conversions, and a separate `AgentException` mirroring the core `AgentError`. The engine API never takes a UniFFI type.
- **Enclave-specificity:** mostly consumer-owned data. The consumer tool catalog (`resources/tool_catalog.json`, 20 tools) is a parity snapshot read only by test fixtures — production hosts pass their own catalog JSON, and the README states the catalog lives with the consumer. Enclave semantics JSON is test-fixture-only. Public config types (`AgentConfig`, `SessionCapabilities`, `ToolSemanticsConfig`) are generic loop machinery; `ToolSemanticsConfig::agnostic()` empties them. **Exactly two leaks into engine logic:** hardcoded `append_blocks` / `"totalBlocks"` literal checks in `src/engine/mod.rs` (~lines 1521–1539, compaction-failure deferral) and `src/compact/tool_spill.rs` (line 120, spill exemption) — both bypassing the existing `ToolSemanticsConfig.compaction.artifact_squash` config that neighboring code correctly consults.

---

## 6. Proposed architecture

### 6.1 Overview

```
                        GitHub PR event (opened / synchronize / reopened)
                                        │
                                        ▼
                    ┌───────────────────────────────────────┐
                    │  forge-review.yml (reusable workflow)  │
                    │  checkout PR head ──► repo/            │
                    │  checkout Forge ────► forge/           │
                    │  fetch diff ────────► mr.diff          │
                    │  marker check (review exists?)         │
                    └───────────────┬───────────────────────┘
                          first review│            subsequent pushes
                                        ▼                     │
                    ┌───────────────────────────────────────┐  │
                    │  forge-agent (Rust binary, this plan)  │  │
                    │                                        │  │
                    │  Host orchestrator (deterministic)     │  │
                    │   1. snapshot + repo map               │  │
                    │   2. contract resolution (base branch) │  │
                    │   3. fan-out (parallel sessions)       │  │
                    │   4. synthesis                         │  │
                    │   5. decision log + findings JSON      │  │
                    │                                        │  │
                    │  agent_core sessions:                  │  │
                    │   • Thermos correctness  (tools)       │  │
                    │   • Thermos structure    (tools)       │  │
                    │   • Thermos contract     (tools)  NEW  │  │
                    │   • Synthesizer          (tools)       │  │
                    │                                        │  │
                    │  Host tools: grep · read_file ·        │  │
                    │  list_dir · read_symbol ·              │  │
                    │  run_pr_agent (synthesizer only)       │  │
                    │                                        │  │
                    │  AgentTransport ──► OpenRouter         │  │
                    └───────────────┬───────────────────────┘  │
                                    │ findings JSON +          │
                                    │ synthesis decision log   │
                                    ▼                          ▼
                    ┌───────────────────────────────────────────────┐
                    │  Existing Python layer (UNCHANGED in v1)       │
                    │  master.py — post consolidated review          │
                    │  verify.py — on re-push: verdicts + resolve    │
                    │  github_backend.py — GitHub API                │
                    └───────────────────────────────────────────────┘

        External producer, invoked once by the host during fan-out:
        PR-Agent (Python subprocess, unmodified, against the real PR URL)
                                    │ guide text
                                    └────────► into synthesis as evidence
```

### 6.2 The two-layer split

The architecture deliberately separates:

- **The host orchestrator (new Rust, deterministic).** Owns everything that must be predictable: process lifecycle, checkout handling, stage ordering, parallelism, budgets, timeouts, retries of whole stages, output contracts between stages, and the decision log. It contains no prompts and makes no judgments.
- **agent_core sessions (agentic).** Own everything that is judgment: which files to pull, whether a pattern is a defect, whether two findings are the same finding, how severe something is. Each session is an `AgentEngine` driven by the host over the three host traits.

PR-Agent and the existing Python layer are neither: PR-Agent is an external instrument the host invokes; the Python layer is the unchanged hands that post and verify.

### 6.3 Stage flow for a first review

1. **Preflight (workflow, existing).** Secrets check, Forge App token mint, checkouts, diff fetch, consolidated-review marker check — all as today. If a consolidated review exists, the workflow skips forge-agent entirely and runs the existing verify path.
2. **Snapshot (host).** forge-agent takes the `repo/` checkout as the read snapshot at the PR head SHA and records the SHA in every output artifact. It builds the **repo map**: a pruned file tree (excluding `.git`, build outputs, vendored trees, and lockfiles) with file sizes, plus the changed-file list parsed from `mr.diff`.
3. **Contract resolution (host + one short session, or host alone — see 8.7).** The configured entry file is read **from the PR's base branch** (fetched into a separate read-only tree or via the GitHub API), never from the PR head. Its content, plus the repo map, opens every lens session. Routing beyond the entry file is *not* host work: lens sessions follow pointers with their own tools, at the PR head (contract *targets* such as `contracts/X.md` are read at head, but the *entry file's* instructions come from base — see 8.7 for the rule and its rationale).
4. **Fan-out (host, parallel).** Three lens sessions and the PR-Agent subprocess run concurrently:
   - Thermos **correctness** session (tools enabled)
   - Thermos **structure** session (tools enabled)
   - Thermos **contract** session (tools enabled, new lens)
   - **PR-Agent** subprocess (once, deterministic; Section 8.6)
   Each lens session ends by emitting findings in the existing Thermos JSON schema. PR-Agent emits its reviewer guide as text evidence.
5. **Deterministic pre-dedup (host).** Exact and near duplicates (same file, overlapping line range, same category) are clustered before synthesis, so the synthesizer spends judgment on conflicts and quality, not on string matching. Clustering is recorded, not destructive: cluster members all reach the synthesizer.
6. **Synthesis (session, tools enabled).** The synthesizer receives: the diff, all clustered findings, the PR-Agent guide, the ticket artifact (from FRG-6, when present), and the repo map. It may grep/read to adjudicate conflicts and verify relational claims, and may invoke `run_pr_agent` as a bounded follow-up (Section 8.6). It emits the final findings array plus a **decision entry for every producer finding it merged, downgraded, or dropped, with a reason**.
7. **Hand-off (existing Python).** `master.py` consumes the final findings JSON and decision log, posts the consolidated review exactly as today (rollup + inline discussions), and writes `forge-recorded.json` in its current shape, so `verify.py` on later pushes is unaffected.

### 6.4 Stage flow for subsequent pushes

Unchanged from 1.x: the marker check routes to `verify.py`, which asks the model per open finding whether the new diff addresses it and resolves only addressed threads. Forge 2.0 does not alter the verify contract in v1. (A repo-aware verify — letting the verifier read surrounding code before declaring a finding addressed — is an obvious v2 candidate and is noted in Section 13, not planned here.)

---

## 7. Component design: agent_core preparation

Two changes to agent_core precede everything else. Both are independently valuable to Enclave, neither changes Enclave's behavior, and both are verified by Enclave's own test surface.

### 7.1 Feature-gate UniFFI (proposed ticket AC-1)

**Change (approx. 4 files, tens of lines of `cfg`):**

1. `Cargo.toml`: make `uniffi` optional; add `default = ["ffi"]` and `ffi = ["dep:uniffi"]`. Enclave's resolved feature set is identical to today's.
2. `Cargo.toml`: add `required-features = ["ffi"]` to the `uniffi-bindgen` `[[bin]]`.
3. `src/lib.rs`: gate, **in place**, the `setup_scaffolding!()` macro, `pub mod ffi`, the `Ffi*` re-export block, and the three callback-interface traits behind `#[cfg(feature = "ffi")]`. The callback traits must remain at the crate root under the gate — their root placement is what registers `FfiConverter` for `Arc<dyn …Callback>`; relocating them into the module is the one move to avoid.
4. `tests/finish_boundary_test.rs` and `tests/compaction_boundary_test.rs` (the only non-FFI consumers of `Ffi*` types): `required-features = ["ffi"]`.

**Non-issues:** the FFI types are mirrors, so no core type needs a plain/UniFFI duplicate — the duplication the gate would usually force already exists by design. `[lib] crate-type = ["cdylib", "staticlib", "rlib"]` cannot be feature-gated, but it is irrelevant to a Rust consumer, which receives an rlib from the dependency graph regardless.

**Verification (acceptance criteria):**

- `cargo test` passes with default features and with `--no-default-features`.
- Kotlin bindings generated from the pre-change library and the post-change library (feature on) are **byte-identical**.
- Enclave's Rust-bridge contract tests pass unchanged.
- The `--no-default-features` build's dependency tree contains no `uniffi` or `uniffi_bindgen` crates (checked via `cargo tree`), quantifying the compile-time saving for Forge.

### 7.2 Parameterize the artifact-append literal checks (proposed ticket AC-2)

**Change:** the two hardcoded sites — `src/engine/mod.rs` (~lines 1521–1539) and `src/compact/tool_spill.rs` (line 120) — stop matching the literals `append_blocks` and `"totalBlocks"` directly and instead consult `ToolSemanticsConfig.compaction.artifact_squash` (`append_tool_name`, `total_blocks_result_field`, and siblings), exactly as `src/artifact_finalize.rs` and `src/tool_parse/mod.rs` already do.

**Behavior guarantee:** with Enclave's semantics config, the values consulted are the same strings, so Enclave behavior is unchanged by construction. With `ToolSemanticsConfig::agnostic()` (no artifact tool configured), both checks become inert rather than matching a tool that does not exist in the session.

**Verification:** existing agent_core test suite green in both configurations from 7.1; a new unit test demonstrating that a custom `artifact_squash` tool name engages the deferral path, and that `agnostic()` does not.

### 7.3 What deliberately does not change in agent_core

- No HTTP or provider code is added to the crate. The OpenRouter transport lives in forge-agent (Section 8.3). Keeping transport a host responsibility is the crate's existing design, and Forge honoring it is what makes this reuse rather than a fork.
- The consumer tool catalog and Enclave semantics fixtures stay where they are. They are consumer data and test snapshots, not engine concerns; Forge simply never loads them.
- No new cargo features beyond `ffi`. The existing `lalr-harness` test feature is untouched.

---

## 8. Component design: forge-agent

forge-agent is a new Rust binary crate in the forge-reviewer repository (working layout: `agent/` alongside the existing `.forge/` Python). It depends on agent_core with `default-features = false`. It is a host for agent_core, an owner of tools, and nothing else.

### 8.1 Crate layout and dependencies

```
agent/
  Cargo.toml            # bin: forge-agent; deps: agent_core (no default
                        # features), serde, serde_json, ureq (blocking
                        # HTTP for the transport), regex — nothing else
                        # without a ticket-level justification
  src/
    main.rs             # CLI, stage orchestration, exit codes
    host.rs             # AgentTransport / AgentToolExecutor /
                        # AgentSummarizer implementations
    transport.rs        # OpenRouter AgentTransport (Section 8.3)
    tools.rs            # read-only repo tools + shared result cache (8.4)
    snapshot.rs         # repo map, changed-file extraction, pruning
    contract.rs         # entry-file resolution from base branch (8.7)
    sessions.rs         # lens session construction: prompts, budgets,
                        # catalog/schema JSON per lens
    pr_agent.rs         # PR-Agent subprocess adapter (8.6)
    synthesis.rs        # pre-dedup clustering + synthesizer session (8.8)
    outputs.rs          # findings JSON + decision log writing
  prompts/              # lens prompts, evolved from .forge/prompts/
    correctness.md
    structure.md
    contract.md         # NEW
    synthesis.md        # evolved from master-dedup.md
```

The stdlib-only discipline of `.forge/` has a Rust analogue here: the dependency list stays short enough to audit in one screen, because this binary runs in other people's CI with a Forge App token in its environment.

### 8.2 CLI and I/O contract

forge-agent is invoked by the workflow as a single command; it is not a daemon and holds no state between runs.

```
forge-agent review \
  --repo <path to PR-head checkout> \
  --diff <path to mr.diff> \
  --base-ref <base branch ref or SHA> \
  --pr-url <url, for the PR-Agent adapter> \
  --config <path to forge config in repo, optional> \
  --ticket-file <path, optional: FRG-6 ticket artifact> \
  --out-findings <path: final findings JSON> \
  --out-decisions <path: synthesis decision log JSON> \
  --out-pr-agent <path: raw PR-Agent guide, for the record>
```

Environment: `OPENROUTER_API_KEY` (transport only), `FORGE_MODEL` (existing convention), optional per-lens model overrides (Section 11, Decision D5). Exit codes: `0` review produced (including a legitimately empty one); `2` orchestration failure (a stage failed — the workflow then falls back to posting a visible "Forge review incomplete" note rather than silence, preserving principle 7); producer failures inside the fan-out do **not** fail the run — they are recorded in the decision log as producer outages and synthesis proceeds with what exists (principle 7's fail-soft half, matching 1.x behavior).

### 8.3 The OpenRouter transport

The one substantial new component. It implements `AgentTransport::complete_model_turn`:

- Renders `ModelRequest.session_messages` (including `raw_tool_calls` replay metadata, for which agent_core's `src/engine/provider_tool_calls.rs` was explicitly built for OpenAI-compatible APIs) into an OpenAI-style chat-completions request; POSTs to OpenRouter with a blocking client (`ureq`); maps the response back to `ModelResponse { assistant_text, tool_calls, finish_reason }`.
- **Two tool-call paths, both supported:** if the model returns native tool calls, they are mapped directly; if it returns text, the plain text is handed back and agent_core's envelope ingest extracts tool calls from it. This makes lens behavior independent of any single model's native function-calling support — a real concern, since Forge's default model is configurable and the fleet of OpenRouter models varies in tool support.
- Synchronous traits meet a blocking client: no async runtime enters the binary for this.
- Retries, HTTP-status handling, and backoff are delegated to agent_core's model-API retry machinery (`submit_model_turn_failure`) rather than reimplemented; the transport's job is faithful rendering and mapping, not policy.
- The reference implementation for message-shape decisions is Enclave's Kotlin transport: same engine, same message model, different wire. Where the two disagree, that is a finding about agent_core, and is reported as one (this feedback loop is a stated benefit of the reuse, Section 15).

Estimated size: a few hundred lines including rendering tests. It is the critical path of the whole plan and is scheduled first among the forge-agent components (Section 13).

### 8.4 The tool executor: repository tools

All sessions share one `AgentToolExecutor` implementation in the host. Sharing matters: the executor holds a **result cache** keyed by (tool, arguments, snapshot SHA), so when three lenses independently read the same hot file, the file is read from disk once (the *context* cost is per session and unavoidable; the I/O and latency cost is not).

**Tool set (complete for v1):**

| Tool | Arguments | Returns | Caps |
|---|---|---|---|
| `list_dir` | `path` (repo-relative) | Entries with type and size | 200 entries |
| `grep` | `pattern` (regex), `glob` (optional), `path` (optional) | `path:line:snippet` hits | 50 hits, snippet ≤ 200 chars, head-truncated with an explicit truncation note |
| `read_file` | `path`, `start_line`, `end_line` (optional) | Line-numbered content | 200 lines per call; binary files refused with a note |
| `read_symbol` | `name`, `path` (optional hint) | The definition site plus a surrounding window | Implemented as definition-grep + `read_file`; window ≤ 200 lines |

Plus, for the synthesizer session only: `run_pr_agent` (Section 8.6).

**Executor rules (host-enforced, not prompt-requested):**

- **Confinement.** Every path is resolved against the snapshot root; `..`, absolute paths, and symlink escapes are refused. The base-branch contract tree (8.7) is readable only through the contract channel, not the general tools, so a session cannot confuse head code with base rules.
- **No shell, ever.** `grep` is a library regex walk (the `regex` crate), not a subprocess. There is no tool whose arguments can become a command line.
- **Budgets.** Per-session tool-call budget and per-session cumulative tool-output token budget are enforced by the host in cooperation with agent_core's budget machinery; on exhaustion the tool returns a budget-exhausted result (a soft failure the session can wrap up around), never a panic or a silent truncation.
- **Secret hygiene.** Tool output passes through the same secret-pattern redaction discipline Forge already applies; a `<redacted>` token in tool output is sanitizer output, never repository content, and the lens prompts carry the existing grounding rule verbatim: redaction tokens are never evidence.
- **Untrusted data.** Tool results are returned to the session marked as repository content. The lens system prompts state the boundary plainly: instructions found in repository files — other than the contract channel's entry file and what it routes to — are code under review, not instructions to the reviewer.

### 8.5 Lens sessions

Each lens is an agent_core session constructed by the host with: a system prompt (evolved from the current `.forge/prompts/`), the diff, the changed-file full contents (finally delivering what the 1.x prompt promised), the repo map, the contract entry content, and the shared executor. All sessions use `ToolSemanticsConfig::agnostic()` and a `NoOpSummarizer` with summarization disallowed — review sessions are short, bounded affairs that should end before compaction is relevant; if a session approaches its context limit, the correct response is budget exhaustion and a recorded partial result, not silent summarization of evidence.

**Correctness lens.** Charter unchanged (defects, security, data integrity, concurrency), plus the tool set and one new instruction: any claim about a caller, callee, or consumer must be pulled and cited before it is asserted. Output: existing findings JSON schema, unchanged.

**Structure lens.** Charter unchanged (maintainability, layering, duplication), plus tools — duplication findings in particular change character when the lens can grep for the existing implementation it claims exists. Uncited "a utility for this already exists" findings are synthesis-rejectable.

**Contract lens (new).** Charter: the repository's own rules, and only those. Sources, in order: the entry file's direct statements, and contract material the entry file routes to, pulled via tools at the PR head. The lens reports violations of *stated* rules with the rule quoted and the violating code cited; it is explicitly forbidden from enforcing generic best practice (that is structure's lane) or from inventing rules the repo does not state. If the entry file is absent or routes nowhere, the contract lens returns an empty finding set and a recorded note — a legitimate outcome, not an error (see Decision D1 on fallbacks).

**Common output contract.** All lenses emit the existing schema (`file`, `line_start`, `line_end`, severity, category, title, description, `evidence`, `fix_focus_areas`, `recommended_fix`), extended with one optional field: `citations` — the list of `path:line` locations the session actually read in support of relational claims. Synthesis treats uncited relational claims as unverified (Section 8.8).

### 8.6 PR-Agent integration

PR-Agent's role changes from peer producer to instrument, in two moves:

**Move 1 — wire it correctly (deterministic run).** The host invokes PR-Agent once per review, unmodified, as a subprocess — but against the **real PR URL** through the GitHub provider rather than `--diff-file`, so `repo_context_files` functions for the first time. Forge's workflow config supplies `repo_context_files` (defaulting to the configured entry file) via the caller's `.pr_agent.toml` / Forge config mapping (Section 8.7), and `[artifacts]` injects the host-generated repo map as the pre-produced context file — the one static channel PR-Agent has, used for the one artifact the host can compute better than PR-Agent can guess. Its guide text is captured verbatim to `--out-pr-agent` and passed to synthesis as evidence, as in 1.x. Fail-soft as today: a PR-Agent failure records an outage and synthesis proceeds.

**Move 2 — bounded agentic follow-up (synthesizer tool).** The synthesizer session — and only it — gets a `run_pr_agent` tool: arguments are a code scope (file list) and a focus question; the host runs PR-Agent scoped accordingly and returns its guide text. Hard cap: **2 invocations per review** (Decision D6), enforced by the host. This preserves the original intuition — the orchestrating agent can ask PR-Agent for a second look at a specific area — without paying full-review overhead per chunk or fragmenting context across chunked runs as a normal mode. PR-Agent's own internal large-PR chunking remains its business.

**What does not happen:** PR-Agent is not forked, not patched, and not given tools. If a future PR-Agent release adds a genuine tool loop, this adapter is the seam where Forge would adopt it — the subprocess boundary makes that a configuration question instead of a redesign.

### 8.7 Contract configuration and resolution

**Configuration surface.** Forge gains a small config, deliberately modeled on the shape PR-Agent proved out, in the caller's existing `.pr_agent.toml` when present, else a Forge section in the workflow inputs (final home: Decision D2):

```toml
[forge]
# The repo's agent/contract entry file, relative to repo root.
entry_file = "AGENTS.md"
# Optional: additional context files handed to PR-Agent's static
# repo_context_files channel. The lens sessions do NOT need this
# list — they follow the entry file's routing with tools.
pr_agent_context_files = ["AGENTS.md"]
```

Defaults: `entry_file = "AGENTS.md"`. An adopter whose house standard is a different root file (the `CLAUDE.md` case) sets one line. Forge ships no other built-in contract assumptions.

**Resolution rules.**

1. The entry file is read from the **base branch** of the PR, not the head. Rationale, adopted from PR-Agent's `repo_context_from_default_branch`: the rules a PR is judged against must not be rewritable by the PR itself. If the PR modifies the entry file, that modification is itself reviewable code (and the contract lens sees the diff), but it does not govern this review.
2. The entry file's *content* is the only contract material the host loads directly. Everything it routes to is pulled by lens sessions with tools, at the PR head — routing targets are documentation under review like any other file, and reading them at head is what lets the contract lens notice when a PR edits a contract doc to match its own violation (a finding category of its own).
3. Size guard: an entry file beyond a generous line ceiling is truncated with a recorded note, following PR-Agent's `repo_context_max_lines` precedent (their default is 500; Forge's default should be larger, since the entry file is the review's constitution — proposed 2,000 lines, Decision D3).
4. Absence: no entry file at the configured path on the base branch → contract lens stands down (recorded), other lenses proceed without contract context, PR-Agent runs with an empty `repo_context_files`. Forge degrades to a 1.x-quality review rather than failing an adopter's pipeline over a missing file.

### 8.8 Synthesis

**Pre-dedup (host, deterministic).** Findings are clustered on (file, overlapping range, category). Clustering annotates; it deletes nothing.

**Synthesizer session.** Inputs as listed in 6.3 step 6. Its charter, evolved from the current `master-dedup.md`:

- Merge clusters into single findings with combined evidence; resolve contradictions between producers by pulling the code — the adjudication must cite what it read.
- Verify relational claims: a finding whose `citations` are absent and whose description asserts caller/callee facts is either verified (citations added) or downgraded/dropped with reason `unverified-claim`.
- Weigh the PR-Agent guide as one producer's evidence, equal in standing to a lens, not senior to them.
- Apply severity calibration and the ticket-conformance context (FRG-7 artifact, when present) as today.
- Emit the final findings array in the existing schema, and the **decision log**: one entry per producer finding with disposition (`kept`, `merged-into:<id>`, `downgraded:<reason>`, `dropped:<reason>`) — satisfying principle 6 mechanically, and giving `verify.py` and future debugging a complete audit trail.

**Follow-up budget.** The synthesizer's tool budget is separate from and larger than a lens's (it adjudicates across the whole review), and its `run_pr_agent` cap is 2 (8.6). On budget exhaustion mid-synthesis, the host falls back to a deterministic union of producer findings (clustered, unmerged) rather than losing the review — a worse review is posted with a recorded note, never no review.

### 8.9 What stays: the Python layer

`master.py`'s posting half, `github_backend.py`, and `verify.py` are unchanged in v1. forge-agent's outputs are shaped to the contracts they already consume: findings JSON in the Thermos/master schema and `forge-recorded.json` compatibility preserved. The `.forge/` Python that *does* retire is `thermos.py` and `openrouter_client.py` (their function moves into forge-agent sessions and the transport) and the LLM half of `master.py` (its dedup prompt is succeeded by the synthesizer). Retiring the posting layer into Rust is explicitly a later, separate proposal justified only by operating evidence (NG1).

### 8.10 Workflow and runner integration

Changes to `forge-review.yml` (reusable workflow):

- Add a **build stage** for forge-agent: `cargo build --release` with the standard Rust cache actions, keyed on agent_core + forge-agent revisions. Expected build time is minutes on a stock Ubuntu runner given agent_core's dependency weight (and less with the UniFFI tree removed, per 7.1); the binary is cached across runs, so steady-state cost is a cache restore.
- Replace the three producer steps (PR-Agent venv step, two Thermos steps) with the single forge-agent invocation plus the master posting step consuming its outputs. The PR-Agent Python environment setup remains, now driven by the adapter (8.6) — or moves into the adapter's own venv handling; either way it remains the workflow's one Python dependency alongside the posting layer.
- **Checkout depth:** today's shallow checkout suffices for head reads; base-branch entry-file resolution needs the base ref present. The workflow fetches the base branch (a targeted fetch, not a full `--unshallow`) into the separate contract channel location. Full history remains out of scope (NG7, Decision D8).
- **Permissions and secrets** are unchanged: Forge App token for posting, `OPENROUTER_API_KEY` passed explicitly per FRG-3's pattern. forge-agent receives the OpenRouter key in its environment for the transport; the tool executor process-side holds no other credentials, and lens sessions have no channel to the posting token at all — a separation 1.x could not make, since its producers and poster were sibling steps with the same environment.
- **Concurrency:** the per-PR serialization from FRG-11 is preserved at the workflow level and not reimplemented in the binary.
- **Timeouts:** the existing 45-minute workflow ceiling stands. Per-stage host timeouts (fan-out lenses, PR-Agent, synthesis) sum to well under it (Section 11) and are host-enforced.
- **Docker:** not used (NG6). The binary-plus-checkout shape needs no container on ephemeral runners. If Forge later targets persistent self-hosted runners, containerization returns as an isolation question — running untrusted-repo-adjacent workloads on a persistent box — and is decided then, on that threat model.

---

## 9. Security and trust model

- **Untrusted input, structurally.** Everything read from the reviewed repository — code, comments, docs, the diff itself — is data. Lens prompts state this; more importantly, the architecture enforces it: sessions can only act through the read-only executor, so a prompt injection embedded in a code comment can at worst waste budget or produce a strange finding a human can dismiss. It cannot execute, exfiltrate, or post — posting is a separate layer the sessions never touch.
- **Contract trust asymmetry.** The one repository artifact treated as instructions (the entry file and its routed contracts) is taken from the base branch for its governing copy (8.7), so the reviewed PR cannot amend its own constitution. Routed targets read at head are treated as *claims about* the contract, and the contract lens is chartered to notice edits to them.
- **Credential separation.** The Forge App installation token exists only in the workflow steps that post (Python layer) — forge-agent never receives it. The OpenRouter key exists only in forge-agent's transport. A fully compromised lens session reaches neither a posting credential nor a shell.
- **Snapshot integrity.** All reads occur under the recorded head SHA; the decision log and findings artifacts carry it, so any finding can be re-derived against an immutable tree.
- **Redaction.** Existing secret-redaction grounding carries over verbatim into prompts and tool output handling (8.4).
- **Licensing posture.** agent_core and forge-agent are Sean's code under the house 0BSD convention; PR-Agent 0.46.0 is MIT and used unmodified as a subprocess — the cleanest possible license posture for a distributed reviewer.

---

## 10. Cost and performance model

Estimates below are planning figures derived from the 1.x system's shape, to be replaced by measurements from the Phase 1 prototype before any rollout decision. They are stated so the plan's economics are falsifiable, not as promises.

**Per first review, Forge 1.x (baseline):** 3 single-shot model calls (PR-Agent, 2 Thermos) + 1 master call. Diff-sized inputs. Wall-clock: minutes.

**Per first review, Forge 2.0 (proposed):**

| Stage | Model calls | Notes |
|---|---|---|
| Correctness session | ~4–12 turns | Diff + changed files upfront; tool turns as needed, budget-capped |
| Structure session | ~4–12 turns | Same envelope |
| Contract session | ~3–10 turns | Entry file upfront; routed pulls dominate |
| PR-Agent | 1 run (internally chunked by PR-Agent for large PRs) | As today |
| Synthesizer session | ~3–8 turns + ≤2 PR-Agent follow-ups | Adjudication greps are cheap turns |

Net expectation: roughly **3–6× the model tokens of 1.x** per review, and wall-clock still dominated by model latency, parallelized across the fan-out — comfortably inside the 45-minute workflow ceiling, with host timeouts per stage (proposed: 10 min per lens, 15 min PR-Agent, 10 min synthesis) summing below it.

**Cost controls, in order of leverage:** per-session tool and token budgets (host-enforced); the shared executor cache (no repeated disk work, though context cost per session stands); model selection per lens (Decision D5 — e.g., the contract lens may run a cheaper model than correctness, since its task is comparison against stated text); and the trigger policy (Decision D4 — whether every PR gets the full fan-out or large/risky PRs get depth while small ones get a reduced mode). The plan takes no position that money should be spent uniformly; it builds the knobs and lets Phase 4 measurements set them.

**Verify pushes** are unchanged in cost shape from 1.x.

---

## 11. Configuration summary (adopter-facing)

The complete adopter-visible surface of Forge 2.0:

| Setting | Default | Meaning |
|---|---|---|
| `entry_file` | `AGENTS.md` | Root agent/contract file, read from the base branch |
| `entry_max_lines` | 2,000 (D3) | Truncation ceiling for the entry file |
| `pr_agent_context_files` | `[entry_file]` | Static files for PR-Agent's `repo_context_files` channel |
| `model` | Forge default (existing `FORGE_MODEL` flow) | Model for all sessions unless overridden |
| `lens_models.*` | unset (D5) | Optional per-lens model override |
| `deep_review` | on (D4) | Whether the full fan-out runs vs. a reduced diff-only mode |
| `pr_agent_followups` | 2 (D6) | Synthesizer's `run_pr_agent` cap |
| Budgets (per lens / synthesis) | host defaults | Turn, tool-call, and tool-output token ceilings; exposed for tuning, sane out of the box |

Everything else — prompts, schemas, tool implementations, stage wiring — is Forge's, versioned with Forge, not adopter configuration.

---

## 12. Evaluation plan

The central hypothesis — repository access catches real defects that diff-only review cannot — is testable, and this plan does not proceed past prototype on faith.

**Proof suite.** Extend Forge's existing proof-PR pattern (the deliberate-defect PR used during the GitHub re-platform) into a small suite of PRs against a scratch repo, with defects in four classes:

1. **Upstream-invisible:** a change correct in isolation whose existing caller passes the one value that breaks it.
2. **Downstream-invisible:** an output or invariant change whose breakage appears only in a consumer elsewhere in the tree.
3. **Contract-only:** code that is generically fine and violates a rule stated only in the scratch repo's `AGENTS.md` / routed contract (including a variant where the PR also edits the contract doc to excuse itself — the contract lens should flag the edit, per 8.7).
4. **Diff-visible controls:** ordinary defects both generations should catch, guarding against regression.

**Method.** Run Forge 1.x and Forge 2.0 over the suite; score per class: caught / missed / false positive. **Acceptance gates for proceeding to rollout (Phase 4):** 2.0 catches classes 1–3 at a materially higher rate than 1.x (which is structurally near zero on class 3 and evidence-free on 1–2); no regression on class 4; false positives per review no worse than a stated bound (proposed: ≤1 per proof PR); and per-review cost measured against the Section 10 model.

**Field check.** Before consumer repos flip, run 2.0 in shadow mode on Stratum (Forge's most demanding consumer) for a small number of real PRs: producers run, findings recorded but not posted, and Sean compares against the 1.x review that was posted. Shadow mode is a workflow flag, and building it is part of Phase 3, not an afterthought.

---

## 13. Migration plan

Phases are ordered so each lands independently, Forge never has a broken in-between state on consumer repos, and the evaluation gates sit before the expensive commitments.

**Phase 0 — agent_core preparation (independent of Forge).**
AC-1 (UniFFI feature gate) and AC-2 (artifact-literal parameterization), each with the verification in Section 7. These can land immediately on Sean's confirmation; Enclave consumes the result through its normal submodule bump.

**Phase 1 — forge-agent skeleton and the transport.**
New `agent/` crate; CLI and I/O contract (8.2); OpenRouter transport (8.3) with rendering tests; snapshot + repo map (8.4 support); the executor with `list_dir` / `grep` / `read_file`; **one** session — correctness — running end to end locally against a real checkout and diff. Exit criterion: the correctness session, given the class-1 proof defect, pulls the caller and cites it — the single behavior the entire project exists for.

**Phase 2 — full fan-out.**
Structure and contract sessions; contract resolution (8.7) and Forge config parsing; PR-Agent adapter with real-PR-URL invocation and artifacts injection (8.6, Move 1); parallel orchestration with budgets and outage recording. Exit criterion: full fan-out runs locally on a real Stratum PR and produces three findings files plus a PR-Agent guide.

**Phase 3 — synthesis and workflow integration.**
Pre-dedup clustering; synthesizer session with adjudication tools and the capped `run_pr_agent` follow-up (8.6, Move 2); decision log; workflow rewiring (8.10) behind a `forge_agent` workflow input flag (default off); shadow mode. Exit criterion: the Section 12 proof suite passes its gates with the flag on, in CI, on the scratch repo.

**Phase 4 — rollout.**
Flip the flag for Stratum → Bloviate → Enclave, in that order, each after a short shadow period; tune budgets and per-lens models from measurements; update adopter docs (README, BYO Forge App guide) for the 2.0 configuration surface; version and release as Forge v2.0 with the normal changelog/tag discipline. Consumer repos change only their workflow pin, as with every Forge release.

**Phase 5 — deferred, separately decided.**
GitLab template parity; repo-aware verify; Docker packaging for self-hosted runners; retiring the Python posting layer; history-aware tools. None of these are implied by confirmation of this proposal.

**Rollback.** At every phase through rollout, the workflow flag restores 1.x producer behavior from the same release. The Python producers being replaced (`thermos.py`, `openrouter_client.py`, master's LLM half) are removed only in the release *after* the flag has been on by default through a full consumer cycle.

---

## 14. Ticket breakdown (to be filed on confirmation)

Keys below are placeholders: **AC-** tickets land in agent_core's tracking, **FRG-** tickets continue the Forge Reviewer Linear sequence (next available numbers at filing time; FRG-8 remains open and untouched by this plan). Dependencies are blocking unless noted.

### agent_core

| ID | Ticket | Depends on | Acceptance sketch |
|---|---|---|---|
| AC-1 | Feature-gate UniFFI behind default-on `ffi` cargo feature | — | Section 7.1 verification incl. byte-identical Kotlin bindings |
| AC-2 | Parameterize `append_blocks` / `totalBlocks` literal checks through `ToolSemanticsConfig` | — | Section 7.2 verification incl. agnostic() inertness test |

### forge-agent core

| ID | Ticket | Depends on | Acceptance sketch |
|---|---|---|---|
| FG-1 | Scaffold `agent/` crate: CLI, I/O contract, exit codes, config parsing | AC-1 | Binary builds with `--no-default-features` agent_core; `--help` and a no-op review run against fixtures |
| FG-2 | OpenRouter `AgentTransport` implementation | FG-1 | Rendering tests for message + tool-call round-trips on both native and envelope paths; live smoke call against OpenRouter |
| FG-3 | Snapshot + repo map (pruned tree, changed files, SHA stamping) | FG-1 | Fixture-repo tests incl. pruning rules |
| FG-4 | Read-only tool executor (`list_dir`, `grep`, `read_file`, `read_symbol`) with cache, caps, confinement, redaction | FG-1 | Confinement escape tests; cap/truncation tests; cache hit tests |
| FG-5 | Correctness lens session end to end | FG-2, FG-3, FG-4 | Class-1 proof defect caught with citation (Phase 1 exit) |

### Producers and contract

| ID | Ticket | Depends on | Acceptance sketch |
|---|---|---|---|
| FG-6 | Contract resolution: base-branch entry file, Forge config, size guard, absence degradation | FG-1 | Tests for base-vs-head rule incl. PR edits its own AGENTS.md |
| FG-7 | Structure lens session | FG-5 pattern | Proof-suite structure defects caught |
| FG-8 | Contract lens session (new prompt) | FG-6 | Class-3 proof defects caught incl. contract-edit variant |
| FG-9 | PR-Agent adapter: real-PR-URL invocation, `repo_context_files` + artifacts wiring, guide capture | FG-1 | Guide produced in CI with repo context verifiably present (the 1.x silent no-op is dead) |
| FG-10 | Parallel fan-out orchestration: budgets, timeouts, outage recording | FG-5, FG-7, FG-8, FG-9 | Outage-injection tests; decision log records producer failures |

### Synthesis and integration

| ID | Ticket | Depends on | Acceptance sketch |
|---|---|---|---|
| FG-11 | Deterministic pre-dedup clustering | FG-10 | Cluster tests; nothing deleted, only annotated |
| FG-12 | Synthesizer session: adjudication tools, decision log, budget-exhaustion fallback | FG-11 | Contradiction fixtures adjudicated with citations; fallback path tested |
| FG-13 | `run_pr_agent` synthesizer follow-up tool (cap 2) | FG-9, FG-12 | Cap enforced by host; invocations logged in decision log |
| FG-14 | Workflow integration behind `forge_agent` flag + shadow mode | FG-10, FG-12 | Scratch-repo CI run posts via existing Python layer; shadow mode records without posting |
| FG-15 | Proof-suite construction + 1.x vs 2.0 evaluation run | FG-14 | Section 12 gates measured and reported |

### Rollout

| ID | Ticket | Depends on | Acceptance sketch |
|---|---|---|---|
| FG-16 | Shadow runs on Stratum + tuning report (budgets, models, cost) | FG-15 | Measured cost/quality vs Section 10 model |
| FG-17 | Consumer rollout: Stratum → Bloviate → Enclave flag flips + pin bumps | FG-16 | Per-repo verification read-backs |
| FG-18 | Adopter docs for 2.0 (README, config reference, BYO App guide delta) + v2.0 release | FG-17 | Docs match shipped config surface (Section 11) |
| FG-19 | Remove retired Python producers (`thermos.py`, `openrouter_client.py`, master LLM half) | FG-17 + one full cycle | Separate release; rollback flag removed in the same release |

---

## 15. Risks and mitigations

| Risk | Impact | Mitigation |
|---|---|---|
| **Prompt injection via reviewed code** steers a lens | Wasted budget, junk findings | Read-only executor (no actuation channel), posting separated from sessions, contract from base branch, human-visible decision log |
| **Cost blowout** on large PRs | 3–6× becomes 20× | Host-enforced turn/tool/token budgets per session; PR size still bounded by 1.x diff handling; D4 trigger policy; Phase 4 measured tuning before defaults harden |
| **Synthesizer drops real findings** | Worse than 1.x union behavior | Principle 6: decision log with reasons; budget-exhaustion fallback posts the unmerged union; proof suite includes drop auditing |
| **agent_core changes regress Enclave** | Production consumer harmed | Default-on feature preserves Enclave's exact build configuration; byte-identical bindings check; AC-2 behavior-preserving by construction under Enclave's config |
| **Model without native tool calls** degrades sessions | Lens quality varies by `FORGE_MODEL` | agent_core envelope ingest is the second path; transport supports both; lens smoke test in FG-2 covers a non-native model |
| **Transport is the critical path** and slips | Whole plan slips | It is scheduled first (FG-2), is a few hundred lines, and Enclave's Kotlin transport is a working reference for message shape |
| **PR-Agent via PR URL behaves differently** than diff-file mode (provider surface, permissions) | Adapter surprises | FG-9 isolates it; fail-soft outage recording means a PR-Agent failure never blocks the agentic review — the reverse dependency of 1.x |
| **Scope creep into a platform rewrite** | The BLO-153 failure mode, at larger scale | Non-goals are explicit (Section 4.2); the Python layer boundary (8.9) is a plan-level commitment; Phase 5 items require separate proposals |
| **Review latency** grows past usefulness | Slow PR feedback | Fan-out is parallel; stage timeouts sum under the existing 45-minute ceiling; shadow phase measures real latency before rollout |

A benefit worth naming alongside the risks: **the feedback loop into agent_core.** Forge will exercise agent_core's transport-rendering, tool-parsing, and budget machinery against cloud models and adversarial code inputs at a volume Enclave alone never would. Bugs found there are Enclave bugs found in CI instead of on a device. This is a design goal of the reuse, not a side effect, and transport/engine discrepancies surfaced during FG-2 and FG-5 should be filed upstream into agent_core as a matter of routine.

---

## 16. Alternatives considered

- **Fork PR-Agent and add a tool loop.** Rejected: the loop would be a maintained fork of a fast-moving codebase whose handler interface is strings-in/strings-out at its core; and it spends the effort on the producer Forge controls least. The orchestrate-around shape gets the same capability with PR-Agent unmodified.
- **Write the agent loop in Python inside `.forge/`.** Rejected as the primary path: a happy-path loop is short, but the value is in the repair, budget, compaction, and envelope machinery around it — which agent_core already has, tested, in production. A Python loop also forfeits the second-consumer benefit to agent_core. (It remains the fallback if FG-2's transport work uncovers a blocker in the native host surface.)
- **Embedding index / RAG over the repo (the commercial Qodo approach).** Rejected for Forge's scale: live grep is exact, fresh at the head SHA, requires no index infrastructure, and produces citable lines by construction. Embeddings earn their complexity at monorepo scales Forge's adopters do not have; if one ever does, the executor is the seam where an index-backed `grep` could land without changing sessions.
- **Docker-first packaging.** Deferred: on ephemeral GitHub runners the checkout already exists and a container adds build time without adding capability. The isolation argument becomes real only on persistent self-hosted runners (NG6).
- **MCP as the tool protocol.** Not adopted in v1: the executor is in-process and host-native; MCP buys interoperability with external tool servers Forge does not need yet. The tool schemas are kept JSON-shaped so an MCP bridge later is an adapter, not a redesign.
- **Front-load all contract files (PR-Agent's model, generalized).** Rejected as the primary mechanism: it burns context on rules irrelevant to most diffs and cannot follow routing the entry file expresses conditionally. Static front-loading remains in exactly one place — PR-Agent's own channel — because it is the only channel PR-Agent has.

---

## 17. Open decisions for Sean

Confirmation of this proposal does not require resolving all of these; defaults proposed here will be used in ticketing unless Sean directs otherwise. D1 and D2 are the two that shape tickets most.

- **D1 — Entry-file fallback.** If the configured entry file is absent, should Forge (a) stand the contract lens down silently-with-note (proposed), or (b) probe a fallback chain (`AGENTS.md` → `CLAUDE.md` → `.cursorrules`)? Proposed: (a) — one documented standard, predictable behavior, no guessing at an adopter's intent.
- **D2 — Config home.** Forge config in (a) the caller's `.pr_agent.toml` under a `[forge]` table (proposed — one config file per repo, alongside PR-Agent's), (b) a dedicated `.forge.toml`, or (c) workflow inputs only? Proposed: (a), with workflow inputs able to override.
- **D3 — Entry-file size ceiling.** Proposed 2,000 lines (PR-Agent's precedent is 500, but their file is advisory context; Forge's entry file is the review's constitution). 
- **D4 — Trigger policy.** Full fan-out on every PR (proposed for v1, tuned by Phase 4 measurements), or a reduced diff-only mode for small PRs from the start?
- **D5 — Per-lens models.** Ship one global model first (proposed), add per-lens overrides when measurements argue for them — the config surface (Section 11) reserves the keys either way.
- **D6 — PR-Agent follow-up cap.** Proposed 2 per review, synthesizer only.
- **D7 — Ticket artifact interplay.** FRG-6/7 (ticket resolution + conformance) already landed in the 1.x line; this plan consumes their artifact as a synthesizer input. No decision needed unless Sean wants conformance promoted to a fourth lens session instead — proposed: keep as synthesizer input, revisit after rollout.
- **D8 — Checkout depth.** Shallow + targeted base fetch (proposed), vs. full history enabling blame/history tools in a later phase.

---

## Appendix A — Evidence index

Key source locations behind Section 5, for reviewers who want to re-derive the audits:

- PR-Agent 0.46.0 wheel: `pr_agent/algo/ai_handlers/base_ai_handler.py` (handler signature); `pr_agent/algo/skills_loader.py` (single-shot admission); `pr_agent/settings/configuration.toml` (`repo_context_files`, `[skills]`, `[artifacts]`, `[push_outputs]`); `pr_agent/algo/repo_context.py` (`build_repo_context`); `pr_agent/agent/pr_agent.py` (`command2class`); Forge's invocation at `.github/workflows/forge-review.yml` in forge-reviewer (PR-Agent step, `--diff-file`).
- agent_core @ 9ad9215: `Cargo.toml` (deps, crate types); `src/lib.rs` (scaffolding, callback traits); `src/host.rs` (three host traits); `src/engine/mod.rs` (`AgentEngine`, literal checks ~1521–1539); `src/compact/tool_spill.rs:120`; `src/tool_semantics.rs` (`agnostic()`); `src/ffi/` (mirror layer, 58 conversions in `types.rs`); `examples/mock_desktop_loop.rs`; `resources/tool_catalog.json` (consumer snapshot).
- Forge 1.x: `.forge/thermos.py` (`{file_contents}` stub); `.forge/prompts/thermos-correctness.md` (the unfulfilled one-hop promise); `.forge/master.py`, `.forge/verify.py`, `.forge/github_backend.py` (the retained layer).

## Appendix B — Glossary

- **Lens / Thermos lens:** a single-charter review session (correctness, structure, contract).
- **Producer:** anything that emits findings or finding-evidence into synthesis: the three lenses and PR-Agent.
- **Synthesizer:** the final agent_core session that merges producer output into the consolidated review, with adjudication tools.
- **Entry file:** the repository's root agent file (default `AGENTS.md`), the start of contract resolution.
- **Contract channel:** the host's read path for the base-branch entry file, kept separate from the general repo tools.
- **Decision log:** the per-finding disposition record emitted by synthesis (kept / merged / downgraded / dropped, with reason).
- **Shadow mode:** running 2.0 producers and synthesis without posting, for comparison against the posted 1.x review.
