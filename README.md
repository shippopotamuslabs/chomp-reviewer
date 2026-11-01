# Chomp Reviewer

Chomp posts one code review per pull request. Three reviewers run first, each on its own: PR-Agent, plus two LLM lenses called Thermos. A fourth job reads all three outputs, merges findings that flag the same problem, and posts the result as a single review: one summary comment and one set of inline comments. The producers post nothing themselves.

Chomp runs as a reusable GitHub Actions workflow inside a project's own repository. A project adds one small caller workflow, sets two secrets, and gets a review on every pull request. Reviews run once per pull request. Pushes after the first review get verification: Chomp checks its open review threads against the new code and resolves the ones the push fixed.

## The Chomp engine: 2.0 (opt-in)

The 2.0 engine gives the reviewer the full repository instead of only the diff. A Rust orchestration host (`chomp`, built on the agent_core crate) runs one agent session per review lens in parallel; each session reads the repository's own agent file first and pulls the code and contract documents it needs with read-only tools. Producer findings pass through deterministic pre-dedup clustering, then a final synthesizer session verifies and adjudicates every finding — with read-only follow-up tools, a recorded decision log, and a bounded number of PR-Agent re-runs — and posts the single consolidated review. PR-Agent stays in the lineup as an unmodified producer. The full architectural proposal is checked in at [docs/forge-2.0-architectural-proposal.md](docs/forge-2.0-architectural-proposal.md).

2.0 is merged on `main` but gated behind the `chomp_agent` workflow input (default `off`; `forge_agent` remains accepted as a legacy alias): the 1.x path below remains the default behavior. Opt in per caller:

- `chomp_agent: "on"` — the 2.0 path runs the review and posts it.
- `chomp_agent: "shadow"` — the 2.0 path runs alongside the 1.x review, posts nothing, and uploads its findings plus the decision log as run artifacts for comparison.

2.0 needs a `chomp_ref` that contains the Rust host (any ref at or after the 2.0 merge; the `v2.0.0` tag once cut). The reusable workflow checks out Chomp at that ref, builds `chomp`, and fetches the PR's base branch so the contract lens can check the diff against the governing contract — no extra setup on the consumer side. Status and caveats, stated plainly: the proof-suite live evaluation has not run yet, shadow measurements and tuning are pending, per-lens model overrides are not implemented, and the 1.x verify loop is unchanged on both paths. Everything below this section describes the default 1.x path, and it stays accurate.

## How a review runs

Five stages:

```
chomp:diff
  |-- chomp:review              (PR-Agent)
  |-- chomp:thermos-correctness
  |-- chomp:thermos-structure
  +-- chomp:master              (runs after the three producers)
```

`chomp:diff` fetches the pull request changes from the GitHub API and assembles them into one unified diff, shared with the other stages as the `mr.diff` artifact. The three producers depend only on `chomp:diff`, so they start as soon as the diff exists. `chomp:master` waits for all three producer artifacts.

The workflow checks out the `.forge/` scripts and prompts from this repo at the ref the caller pins. The code under review always runs a pinned, released version of Chomp.

## The three producers

**PR-Agent.** Qodo's PR-Agent, installed from PyPI at the pinned version 0.49.0 and run unmodified, reviews `mr.diff` in plain-diff mode with posting turned off. Its markdown review becomes one of the three master inputs.

**Thermos correctness lens.** An LLM prompt scoped to defects: bugs, crashes, security holes, data integrity, API misuse, concurrency hazards. It stays out of style and structure. A clean diff gets an empty result.

**Thermos structure lens.** An LLM prompt scoped to maintainability and design. It flags files a change pushes past 1,000 lines, new branches woven into already tangled methods, duplicated logic where a helper already exists, and abstractions that leak across module boundaries. The prompt calls itself a thermonuclear maintainability audit, and it rates hard.

Both lenses run the same script, `.forge/thermos.py`, with a different prompt file. `chomp:diff` is the hard gate. No diff, no review.

## Thermos

Thermos is the pair of LLM review lenses, `.forge/thermos.py` plus two prompt files in `.forge/prompts/`:

- `thermos-correctness.md` for the correctness and security lens
- `thermos-structure.md` for the structure and maintainability lens

Each lens sends the pull request diff to an OpenRouter chat model and gets back a JSON array of findings. Every finding carries a file, a line range, a severity (Critical, High, Medium, Low), a category, a title, a description, evidence, fix focus areas, and a recommended fix. The shared client in `.forge/openrouter_client.py` calls the model at temperature 0.1 with a 16,000 token response cap. The model comes from `FORGE_MODEL` and defaults to `meta/muse-spark-1.3-contributor` on OpenRouter.

The lenses read the diff. CI mode skips fetching full changed-file contents, so a finding rests on the diff and the context inside it.

## The master job

`.forge/master.py` sends the three producer outputs and the diff to the same model family with the dedup prompt in `.forge/prompts/master-dedup.md`. Findings with the same root cause on overlapping lines collapse into one, with the evidence combined and every contributing source recorded (`pr-agent`, `thermos-a`, `thermos-b`). A finding the model cannot decide on stays in, capped at Medium.

Master posts two things:

1. One rollup comment headed `## PR Reviewer Guide`, with severity counts and a section per finding: description, a code link to the flagged lines, evidence, and an Agent Prompt block that restates the issue, the fix focus areas, and the recommended fix in a copyable prompt for AI coding tools.
2. One inline review comment per finding, anchored to the flagged lines.

GitHub accepts review comment positions only on lines inside the diff. Master parses `mr.diff` into per-file line maps and resolves each finding to the nearest valid line, trying up to five candidate positions before giving up on a placement. A comment that had to move carries an invisible `chomp:anchor` note with the originally flagged location. A finding with no placeable position gets logged at ERROR level and written to the `failed` list in `forge-recorded.json`, a job artifact that records every posted comment ID and anchor.

## Review once, then verify

The rollup and every inline comment carry a `chomp:consolidated-review` marker. On later runs for the same pull request, `chomp:master` checks the pull request comments for that marker. Reviews posted before the rename carry the legacy `forge:consolidated-review` marker; both are recognized, so pre-rename reviews keep verifying. Finding it switches the job to `.forge/verify.py`.

Since v1.1.5 the reusable workflow serializes runs per pull request itself: one job-level concurrency group per repo and PR (`chomp-review-<repo>-<PR>`, never cancelled mid-post), so queued pushes each still get their verify pass in turn, and master re-checks the marker immediately before posting (on GitHub and GitLab) as a final guard against double-posting. Callers need no change beyond bumping both pins to the new tag together; pins before v1.1.5 can double-post on a rapid double-push.

Since v2.1.2 a queued run checks the live head before it posts anything. A run whose head has moved on posts nothing and logs the superseding SHA; the queued run for the live head does the review or verify. Verdicts and reviews therefore always describe the head they name. After a verify pass, the rollup gains a `Last verified at` line naming the head the open findings were re-checked against; the marker line keeps the originally reviewed SHA.

Verify collects the open Chomp review threads, skipping the rollup comment and threads that already resolved. For each finding it builds the evidence fresh:

- the current pull request diff, scoped to the files the finding cites
- the cited files fetched at the new head SHA, as ground truth for whether the flagged code still exists

The final-state fetch matters because the diff is net against the target branch. Code the pull request added and later removed shows no removal hunk, and a verdict based on the diff alone reads that as still present.

The model returns ADDRESSED or NOT_ADDRESSED with a reason. Verify posts the verdict as a reply on the original thread, with the verified SHA in the reply. ADDRESSED threads get resolved. A reply or resolve failure logs an ERROR, the run continues through the remaining discussions, and the job exits 1 at the end if any failed.

## Setup

Add a caller workflow to the project's `.github/workflows/`:

```yaml
# .github/workflows/chomp.yml
name: Chomp Review
on: [pull_request]

jobs:
  chomp-review:
    uses: shippopotamuslabs/chomp-reviewer/.github/workflows/chomp-review.yml@v2.1.0
    with:
      chomp_ref: v2.1.0   # the release this repo runs
      chomp_app_id: 1234567   # your Chomp app's ID - see "Your own Chomp app" below
      # chomp_agent: "on"   # opt-in: run reviews through the 2.0 engine path (see "The Chomp engine")
    secrets:
      OPENROUTER_API_KEY: ${{ secrets.OPENROUTER_API_KEY }}
      FORGE_APP_PRIVATE_KEY: ${{ secrets.FORGE_APP_PRIVATE_KEY }}
```

Pass exactly these two secrets, by name. Never use `secrets: inherit`: it hands the workflow every secret the repo holds, and Chomp needs two.

Set these secrets on the consuming project:

| Secret | Required | Purpose |
|--------|----------|---------|
| `OPENROUTER_API_KEY` | Yes | OpenRouter key for the Thermos lenses, the master dedup, and verification — and, on the 2.0 path, for every lens and synthesis session. |
| `CHOMP_APP_PRIVATE_KEY` | Yes | Private key of your Chomp GitHub App. (The pre-rename name `FORGE_APP_PRIVATE_KEY` is still accepted.) The workflow mints a short-lived token from it, and your app's identity posts the reviews. |
| `LINEAR_API_KEY` | No | Chomp's ticket step fetches linked Linear issues with it and forwards it to the PR-Agent producer step. Absence changes nothing: GitHub-issue linking still works, Linear tickets just are not fetched. |

The reusable workflow also takes a `model` input for the review model. Default: `openrouter/meta/muse-spark-1.3-contributor`. The `openrouter/` prefix comes off before direct API calls.

The project also needs a `.pr_agent.toml` at its root with review guidance for PR-Agent. The review step fails when the file is missing.

## Your own Chomp app

Reviews post under a GitHub App identity, not a user account. The workflow's `chomp_app_id` input defaults to the Shippopotamus Chomp app, which installs only on Shippopotamus repos - so every other adopter creates their own app once and passes its ID in the caller workflow, as in the Setup example above.

1. Go to Settings → Developer settings → GitHub Apps → **New GitHub App** and pick a name. The name is what shows on reviews, as `<name>[bot]`.
2. Under Webhook, uncheck **Active**. Chomp receives no webhooks; Actions calls it.
3. Set exactly these repository permissions, nothing more:
   - Pull requests: **Read and write** (posts the review, replies to threads, resolves them)
   - Checks: **Read and write** (the master job reports its verdict as a check)
   - Contents: **Read-only** (reads the diff)
   - Metadata: **Read-only** (mandatory for every app)
4. Create the app, then **Generate a private key** on its settings page. A `.pem` file downloads.
5. Install the app on each repo Chomp should review (the app's page → Install App).
6. The app's ID sits at the top of its settings page. Put it in the caller as `chomp_app_id`, and store the `.pem` contents as the repo secret `CHOMP_APP_PRIVATE_KEY`.

Rotating the identity later is the same shape: generate a new key, replace the secret, delete the old key on the app page.

The 2.0 engine changes nothing in the list above: same app, same permissions, same two secrets, same `model` input. The 2.0-specific wiring is inside the reusable workflow — it builds `chomp`, fetches the PR's base branch, and keeps the credentials separated: the Chomp app token can only touch the consumer repo (base fetch, PR-Agent via the GitHub provider, posting), while the `OPENROUTER_API_KEY` secret only ever reaches the model API. A consumer opting into `chomp_agent` adds the one input and needs nothing else.

## Linked tickets & conformance

Chomp links PRs to Linear tickets and GitHub issues deterministically - no LLM involved. The ticket step extracts refs with a strict cascade: head branch name, then PR title, then PR body, then commit messages. The first source that yields a match wins per provider; multiple distinct tickets in that source are all checked. No match is normal: the review proceeds and the rollup notes no linked ticket.

| Pattern | Example |
|---------|---------|
| Linear key in branch | `feature/FRG-5-login`, `FRG-5-slug`, `hotfix/FRG-5/...`, or Linear's canonical `gitBranchName` (e.g. `sean/frg-5-...`) |
| Linear key in title/body/commits | `CHOMP-5`, `Closes CHOMP-5`, `Fixes CHOMP-5` |
| GitHub issue | `#123`, `owner/repo#123` |

To get linking: name the branch or PR after the ticket key, or reference it in the title, body, or commits. For Linear ticket fetching, set the optional `LINEAR_API_KEY` secret; GitHub issues need no new secret (Chomp app token).

The ticket step writes `ticket.md` / `ticket.json` beside `mr.diff` and passes them to PR-Agent, both Thermos lenses, and master as delimited untrusted quoted context - ticket text is data, never instructions. The master rollup gains a `## Ticket conformance` section: resolved tickets with links, per-criterion addressed / not evident / contradicted verdicts with file/line evidence, a neutral unrelated-changes list, and one overall line (`conforms`, `partially conforms`, `does not conform`, or `no linked ticket`). On re-push, verify re-evaluates open criteria and posts a summary re-check comment. Advisory only, never a merge gate. Sparse tickets produce sparse verdicts.

## The Chomp engine: configuration reference

Every adopter-visible setting on the 2.0 path, defaults verified against the shipped `chomp` (`agent/src/contract.rs`) and the reusable workflow. The workflow inputs are the whole consumer surface in v1: `chomp` also reads an optional `--config` file for the remaining knobs, but the workflow does not pass one yet, so those run at their compiled-in defaults.

| Setting | Shipped default | Set by | Meaning |
|---------|----------------|--------|---------|
| `forge_agent` | `off` | workflow input | `off`: 1.x behavior. `on`: 2.0 runs and posts. `shadow`: 2.0 runs, 1.x posts, 2.0 findings + decision log upload as artifacts. |
| `model` | `openrouter/meta/muse-spark-1.3-contributor` | workflow input (empty = default) | Review model for every 2.0 session and for PR-Agent. The `openrouter/` prefix is stripped before direct API calls. Per-lens model overrides are not implemented. |
| `pr_agent_version` | `0.49.0` | workflow input | PR-Agent version installed unmodified from PyPI. |
| `forge_ref` | `main` | caller (pinned per repo) | Forge ref the workflow checks out and builds `chomp` from. For 2.0 it must contain the Rust host. |
| `entry_file` | `AGENTS.md` | compiled default | The agent file every session reads first. |
| `entry_max_lines` | `2000` | compiled default | Lines of the entry file kept before the session sees the routing table instead. |
| `pr_agent_context_files` | `[entry_file]` | compiled default | Extra context files front-loaded into the PR-Agent producer. |
| `deep_review` | on | compiled default | Per-producer deep review inside `chomp`. |
| `lens_timeout_secs` | `600` | compiled default | Wall-clock budget per lens session. |
| `pr_agent_timeout_secs` | `900` | compiled default | Wall-clock budget for the PR-Agent producer run. |
| `synthesis_timeout_secs` | `600` | compiled default | Wall-clock budget for the synthesizer session. |
| `pr_agent_followups` | `2` | compiled default | How many times the synthesizer may re-run PR-Agent with focused follow-up guidance. `0` disables the tool. |

Deliberately not knobs: Chomp always runs the full fan-out (every lens, every review), the workflow ceiling stays 45 minutes, and the per-stage budgets above sit under it with room for the `chomp` build. Tuning of these defaults against shadow measurements is pending — the values above are the as-shipped ones, not tuned recommendations.

## Writing a contract Chomp can use

The 2.0 contract lens reads the consumer repo's agent file the way a new team member would: entry file first (`AGENTS.md` by default), routing table when the file is long, then the linked contract documents. What makes a repo review well:

- **Stated rules beat implied ones.** A checkable statement ("every migration has a down path", "no new `unwrap()` outside tests") is something the lens can verify against the diff; a vibe ("keep it clean") is something it will either ignore or hallucinate into findings.
- **Say where things live.** The lens finds documents through the entry file's routing, so keep the routes current: which file governs the architecture, where the testing rules are, what the review gates are.
- **The base branch governs.** The lens always reads the contract documents at the PR's base revision, never at the PR head — a PR cannot smuggle in a weakened contract and then comply with it. Keep `main`'s contract documents authoritative and review changes to them as carefully as code.
- **Absent is degraded, not fatal.** A repo with no agent file still gets reviewed: the lens says so in its output and the review leans on the diff, the code, and the generic lenses. Adding even a short agent file with pointers gives the contract lens something checkable to work with.

The PR-Agent producer step also sets `PR_REVIEWER__CHECK_PR_ADDITIONAL_CONTENT=true` (the dynaconf env override for `pr_reviewer.check_pr_additional_content`), intended to flag changes unrelated to the stated intent. Note: in the pinned pr-agent 0.49.0 plain-diff mode that flag and the forwarded `LINEAR_API_KEY` are not consumed upstream; Chomp's own ticket artifact and conformance section provide the ticket awareness today.

## Run the pieces yourself

The `.forge/` scripts import nothing outside the Python standard library. They run on any machine with Python 3, given the right inputs and environment variables.

Run the test suite (offline, standard library only):

```
python3 -m unittest discover -s tests -v
```

Run one Thermos lens against a diff file:

```
OPENROUTER_API_KEY=<key> python3 .forge/thermos.py --lens correctness --diff mr.diff --output thermos-correctness.json
```

Fetch a pull request diff the way `forge:diff` does:

```
python3 .forge/fetch_diff_github.py https://api.github.com <owner/repo> <pr_number> <token> --output mr.diff
```

Preview a consolidated review without posting it. This takes the three producer artifacts as inputs and prints the deduplicated review as JSON. It still calls OpenRouter, so it spends an API call:

```
OPENROUTER_API_KEY=<key> python3 .forge/master.py --iid <pr_number> --sha <head_sha> \
  --pr-agent pr-agent-findings.md \
  --thermos-a thermos-correctness.json \
  --thermos-b thermos-structure.json \
  --diff mr.diff --dry-run
```

The full flow runs in GitHub Actions. Master and verify read pull request state, post comments, and resolve threads through the GitHub API and GraphQL, using the repository, pull request number, and head SHA from the workflow environment plus the Chomp app token the workflow mints. Verify has a `--dry-run` flag that reports verdicts without resolving, and it still needs a real pull request and token to read threads from.

## Repository layout

| Path | Job |
| --- | --- |
| `.forge/fetch_diff_github.py` | Fetches PR changes from the GitHub API |
| `.forge/github_backend.py` | The GitHub backend: PR and diff reads, rollup and inline review posting, review-thread resolution |
| `.forge/ticket.py` | Extracts linked ticket refs (branch, title, body, commits) and fetches Linear/GitHub ticket context for the producers |
| `.github/workflows/forge-review.yml` | The reusable workflow consuming repos call |
| `.forge/thermos.py` | Runs one Thermos lens against a diff, writes findings JSON |
| `.forge/master.py` | Dedups the three producer outputs, posts the review |
| `.forge/verify.py` | Verdicts open threads on later pushes, resolves fixed ones |
| `.forge/openrouter_client.py` | Shared OpenRouter client and model selection |
| `.forge/prompts/` | The prompt files: `thermos-correctness.md`, `thermos-structure.md`, `master-dedup.md` |
| `tests/` | Regression tests for the .forge scripts, unittest based |
| `CHANGELOG.md` | One entry per release tag |
| `AGENTS.md` | Contributor rules: tests, release process, conventions |

## Status

Chomp runs on GitHub. This repo lives in the `shippopotamuslabs` org, and a GitHub App named Shippopotamus Chomp holds the review identity there: pull request and check permissions, with contents read-only. The reusable workflow ships from this repo, and consuming repos pin a release tag. Stratum is the first consumer: a proof pull request with deliberate defects caught every planted defect class before the port landed. The v1.1.0 tag lands on the port's merge commit.

Chomp came out of the Stratum project's CI in September 2026 (as Forge; renamed 2026-10-30) and has reviewed its changes since v1.0.0. The 1.x release line stands at v1.1.8. The 2.0 agent path is merged on `main` behind the `chomp_agent` flag (default `off`) and is described in `CHANGELOG.md` under `v2.0.0`; the tag and release follow once the proof-suite evaluation and shadow measurements report.

## Versioning and upgrades

Releases are Git tags: `v1.0.0` through `v1.1.8` at the time of writing, plus an unreleased `v2.0.0` changelog entry for the flag-gated 2.0 path. There is no version file. `CHANGELOG.md` carries one entry per tag, and the entries describe the failure each fix addresses.

To upgrade a consuming project, check the changelog, then change the pinned ref in the caller workflow and the `chomp_ref` input to the same new value. Projects pin their own ref; nothing upgrades underneath a consumer.

## Contributing

Read `AGENTS.md` before changing anything. The short version: every behavior change in `.forge/` ships with a focused regression test under `tests/`, and a release takes a changelog entry, a tag on the changelog commit, and the tag pushed to the remote in the same flow.

## License

0BSD license. Do whatever you want with the code. Just don't expect it to work or sue me.

PR-Agent runs unmodified from PyPI at review time and stays under its own license, MIT.
