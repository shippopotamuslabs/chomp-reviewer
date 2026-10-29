# FRG-22: Contract lens session: the repository's own rules as a review dimension

Linear: https://linear.app/shippopotamus-labs/issue/FRG-22
Status at intake: Backlog. Blocked by FRG-20 (Done). Blocks FRG-24.
Branch (Linear gitBranchName): shippopotamus/frg-22-contract-lens-session-the-repositorys-own-rules-as-a-review

(The ticket body below is the human's vision for the Synthitect gate, verbatim in substance; Linear issue references rendered as FRG-xx keys.)

---

Repo: shippopotamuslabs/forge-reviewer (`agent/src/sessions.rs`, `agent/prompts/contract.md` — a NEW prompt authored by this ticket; the current file is a stub left by FRG-15). Proposal §8.5 (contract lens), §8.7. This ticket creates Chomp's third review dimension (binary/product in the agent crate is Chomp/chomp; keep `forge_agent` flag, `FORGE_MODEL`, `[forge]`, repo name, `mr.diff`): contract compliance as a first-class lens (goal G2).

## Charter (§8.5)

**The repository's own rules, and only those.**

Sources, in order:

1. The entry file's direct statements — the governing copy from the base branch, via FRG-20's contract resolution. Corrected 2026-10-28: this channel is FRG-20's resolution output passed in as session context, NOT tool output.
2. Contract material the entry file routes to, pulled by this session with its FRG-18 tools **at the PR head**. Corrected 2026-10-28: the settled FRG-18 `ToolExecutor` reads the PR-head tree only — tools cannot read the base tree, so the base governing copy must always come through channel 1, never through a tool call.

## Hard rules for this lens

* Report violations of **stated** rules only: quote the rule and cite the violating code. Every finding carries the rule text it enforces and `citations` for the code read.
* **Forbidden from enforcing generic best practice.** If the repo does not state it, this lens does not flag it — generic judgment is the structure lens's lane, defects are correctness's lane. Invented rules are the worst failure mode of this lens and the prompt must say so explicitly.
* **Contract-edit detection (its own finding category):** because routing targets are read at head (§8.7 rule 2), this lens watches for the PR editing a contract document (including the entry file itself, visible in the diff) in a way that excuses the PR's own violation. Editing the rule to match the violation is a finding, not a defense. (The governing copy still comes from base — FRG-20 — so the edit never actually amends the rules for this review.)
* **Stand-down is legitimate:** if the entry file is absent (FRG-20 absence path) or routes nowhere and states nothing checkable, this lens returns an empty finding set plus a recorded note. That is a correct outcome, not an error, and must not be dressed up as findings.

## Session construction

As per the common pattern (FRG-19): diff, full changed-file contents, repo map, entry content, shared executor, `agnostic()` semantics, per-lens budgets — implement exactly the FRG-19 corrected pattern, not a re-description here. (2026-10-28 correction details: pull-based engine, settled ToolExecutor trait, engine-owned budgets, summarization_allowed=false. Code wins over the ticket paragraph where they disagree.) Its tool use is dominated by routed contract pulls (§10 estimates ~3–10 turns for this session).

## Output

The common findings schema (FRG-19), with the enforced rule quoted inside `description`/`evidence` and code locations in `citations`.

## Acceptance criteria (proof class 3, §12)

* Fixture: code that is generically fine but violates a rule stated only in the fixture repo's AGENTS.md → caught, rule quoted, code cited.
* Fixture: the same violation plus a PR edit to the contract doc excusing it → BOTH findings present (the violation against the base rules, and the contract edit itself).
* Fixture: a plausible-sounding rule the repo does NOT state → no finding (the lens does not invent law).
* Fixture: repo with no entry file / entry file with no checkable rules → `[]` plus the recorded stand-down note.
* Prompt file `agent/prompts/contract.md` is new, versioned with Chomp, and contains the hard rules above verbatim in substance.
