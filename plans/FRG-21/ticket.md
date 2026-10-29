# FRG-21 — Structure lens session in chomp

Repo: shippopotamuslabs/forge-reviewer (`agent/src/sessions.rs`,
`agent/prompts/structure.md`). Proposal §8.5 (structure lens). Follows the
session-construction pattern established by FRG-19 (correctness) — and only
the charter/prompt differs. Corrected 2026-10-28: do NOT re-derive
construction mechanics here; implement exactly the FRG-19 corrected pattern:
pull-based `AgentEngine::new()` (non-generic) + `create_session(initial_messages, AgentConfig, SessionCapabilities, session_context, SessionIoParams)`
+ `next_action()` / `submit_*` loop,
`SessionIoParams::new(provider, context_window_tokens, tools_schema_json, system_prompt)`,
the settled FRG-18 `ToolExecutor` trait
(`execute(&self, &ToolCallRecord)`, shared as
`Arc<dyn ToolExecutor + Send + Sync>`; concrete executor in `chomp::tools`
constructed from `(root, head_sha)`) with its authored `ToolSpec`s/schemas,
engine-owned `AgentConfig.max_turns` + `tool_retry_limit` guardrails and
per-result byte budgets with `SessionCapabilities.tool_result_budget_tokens`
override (no executor-side cumulative budget in v1),
`ToolSemanticsConfig::agnostic()` with catalog override,
`summarization_allowed=false` (so `RequestSummary` never fires — there is no
`NoOpSummarizer` type and no host summarizer callback), and the common
findings output contract. Code wins over this paragraph where they disagree.

## Charter (§8.5)

Unchanged from the 1.x Thermos structure lens: maintainability,
layering/architecture fit, duplication, misplaced responsibilities. Defects
and security are the correctness lens's lane; stated-rule violations are the
contract lens's lane; this lens applies engineering judgment about
structure, not the repo's written law and not bug-hunting.

Opening context, as for every lens: system prompt —
`agent/prompts/structure.md`, which has landed (provenance header: evolved
from `.forge/prompts/thermos-structure.md` for the Chomp agent) — the diff,
full contents of changed files, the repo map, the contract entry content
(the repo's stated layering rules are legitimate *input* to structural
judgment — enforcing them as rules is FRG-22's charter; this lens may still
reason with awareness of them), and the shared tool executor.

## What tools change here

Duplication findings change character when the lens can grep for the
existing implementation it claims exists. A structure finding of the form
"a utility/helper for this already exists at X" is only as good as the pull
that found X — and under the common output contract (§8.5) it must carry
`citations` for the locations actually read. **Uncited "already exists"
findings are synthesis-rejectable** (FRG-26 treats uncited relational claims
as unverified). The same applies to layering claims that depend on how
sibling modules are organized: pull the sibling, cite it.

## Output

The common findings schema (FRG-19): `file`, `line_start`, `line_end`,
`severity`, `category`, `title`, `description`, `evidence`,
`fix_focus_areas`, `recommended_fix`, plus optional `citations`.

## Acceptance criteria

* Structure session runs end to end on a fixture PR and emits schema-valid
  findings (or `[]` for a structurally clean fixture).
* A duplication fixture (new code reimplementing an existing helper
  elsewhere in the tree) is caught, with the existing implementation's
  location in `citations`.
* A fixture whose diff merely *resembles* duplication (no existing
  equivalent in tree) does not produce an "already exists" finding — or
  produces one that synthesis can reject as uncited/unverified (assert the
  citation discipline, not model perfection).
* Session obeys the same budget/partial-result behavior as FRG-19.

Source: https://linear.app/shippopotamus-labs/issue/FRG-21 (fetched
2026-10-29). Blocked by FRG-19; blocks FRG-24.
