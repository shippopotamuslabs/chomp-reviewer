# Forge Reviewer

Forge posts one code review per pull request. Three reviewers run first, each on its own: PR-Agent, plus two LLM lenses called Thermos. A fourth job reads all three outputs, merges findings that flag the same problem, and posts the result as a single review: one summary comment and one set of inline comments. The producers post nothing themselves.

Forge runs as a reusable GitHub Actions workflow inside a project's own repository. A project adds one small caller workflow, sets two secrets, and gets a review on every pull request. Reviews run once per pull request. Pushes after the first review get verification: Forge checks its open review threads against the new code and resolves the ones the push fixed.

## How a review runs

Five stages:

```
forge:diff
  |-- forge:review              (PR-Agent)
  |-- forge:thermos-correctness
  |-- forge:thermos-structure
  +-- forge:master              (runs after the three producers)
```

`forge:diff` fetches the pull request changes from the GitHub API and assembles them into one unified diff, shared with the other stages as the `mr.diff` artifact. The three producers depend only on `forge:diff`, so they start as soon as the diff exists. `forge:master` waits for all three producer artifacts.

The workflow checks out the `.forge/` scripts and prompts from this repo at the ref the caller pins. The code under review always runs a pinned, released version of Forge.

## The three producers

**PR-Agent.** Qodo's PR-Agent, installed from PyPI at the pinned version 0.46.0 and run unmodified, reviews `mr.diff` in plain-diff mode with posting turned off. Its markdown review becomes one of the three master inputs.

**Thermos correctness lens.** An LLM prompt scoped to defects: bugs, crashes, security holes, data integrity, API misuse, concurrency hazards. It stays out of style and structure. A clean diff gets an empty result.

**Thermos structure lens.** An LLM prompt scoped to maintainability and design. It flags files a change pushes past 1,000 lines, new branches woven into already tangled methods, duplicated logic where a helper already exists, and abstractions that leak across module boundaries. The prompt calls itself a thermonuclear maintainability audit, and it rates hard.

Both lenses run the same script, `.forge/thermos.py`, with a different prompt file. `forge:diff` is the hard gate. No diff, no review.

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

GitHub accepts review comment positions only on lines inside the diff. Master parses `mr.diff` into per-file line maps and resolves each finding to the nearest valid line, trying up to five candidate positions before giving up on a placement. A comment that had to move carries an invisible `forge:anchor` note with the originally flagged location. A finding with no placeable position gets logged at ERROR level and written to the `failed` list in `forge-recorded.json`, a job artifact that records every posted comment ID and anchor.

## Review once, then verify

The rollup and every inline comment carry a `forge:consolidated-review` marker. On later runs for the same pull request, `forge:master` checks the pull request comments for that marker. Finding it switches the job to `.forge/verify.py`.

Since v1.1.5 the reusable workflow serializes runs per pull request itself: one job-level concurrency group per repo and PR (`forge-review-<repo>-<PR>`, never cancelled mid-post), so queued pushes each still get their verify pass in turn, and master re-checks the marker immediately before posting (on GitHub and GitLab) as a final guard against double-posting. Callers need no change beyond bumping both pins to the new tag together; pins before v1.1.5 can double-post on a rapid double-push.

Verify collects the open Forge review threads, skipping the rollup comment and threads that already resolved. For each finding it builds the evidence fresh:

- the current pull request diff, scoped to the files the finding cites
- the cited files fetched at the new head SHA, as ground truth for whether the flagged code still exists

The final-state fetch matters because the diff is net against the target branch. Code the pull request added and later removed shows no removal hunk, and a verdict based on the diff alone reads that as still present.

The model returns ADDRESSED or NOT_ADDRESSED with a reason. Verify posts the verdict as a reply on the original thread, with the verified SHA in the reply. ADDRESSED threads get resolved. A reply or resolve failure logs an ERROR, the run continues through the remaining discussions, and the job exits 1 at the end if any failed.

## Setup

Add a caller workflow to the project's `.github/workflows/`:

```yaml
# .github/workflows/forge.yml
name: forge
on: [pull_request]

jobs:
  forge-review:
    uses: shippopotamuslabs/forge-reviewer/.github/workflows/forge-review.yml@v1.1.0
    with:
      forge_ref: v1.1.0   # the release this repo runs
      forge_app_id: 1234567   # your Forge app's ID - see "Your own Forge app" below
    secrets:
      OPENROUTER_API_KEY: ${{ secrets.OPENROUTER_API_KEY }}
      FORGE_APP_PRIVATE_KEY: ${{ secrets.FORGE_APP_PRIVATE_KEY }}
```

Pass exactly these two secrets, by name. Never use `secrets: inherit`: it hands the workflow every secret the repo holds, and Forge needs two.

Set these secrets on the consuming project:

| Secret | Required | Purpose |
|--------|----------|---------|
| `OPENROUTER_API_KEY` | Yes | OpenRouter key for the Thermos lenses, the master dedup, and verification. |
| `FORGE_APP_PRIVATE_KEY` | Yes | Private key of your Forge GitHub App. The workflow mints a short-lived token from it, and your app's identity posts the reviews. |
| `LINEAR_API_KEY` | No | Forge's ticket step fetches linked Linear issues with it and forwards it to the PR-Agent producer step. Absence changes nothing: GitHub-issue linking still works, Linear tickets just are not fetched. |

The reusable workflow also takes a `model` input for the review model. Default: `openrouter/meta/muse-spark-1.3-contributor`. The `openrouter/` prefix comes off before direct API calls.

The project also needs a `.pr_agent.toml` at its root with review guidance for PR-Agent. The review step fails when the file is missing.

## Your own Forge app

Reviews post under a GitHub App identity, not a user account. The workflow's `forge_app_id` input defaults to the Shippopotamus Forge app, which installs only on Shippopotamus repos - so every other adopter creates their own app once and passes its ID in the caller workflow, as in the Setup example above.

1. Go to Settings → Developer settings → GitHub Apps → **New GitHub App** and pick a name. The name is what shows on reviews, as `<name>[bot]`.
2. Under Webhook, uncheck **Active**. Forge receives no webhooks; Actions calls it.
3. Set exactly these repository permissions, nothing more:
   - Pull requests: **Read and write** (posts the review, replies to threads, resolves them)
   - Checks: **Read and write** (the master job reports its verdict as a check)
   - Contents: **Read-only** (reads the diff)
   - Metadata: **Read-only** (mandatory for every app)
4. Create the app, then **Generate a private key** on its settings page. A `.pem` file downloads.
5. Install the app on each repo Forge should review (the app's page → Install App).
6. The app's ID sits at the top of its settings page. Put it in the caller as `forge_app_id`, and store the `.pem` contents as the repo secret `FORGE_APP_PRIVATE_KEY`.

Rotating the identity later is the same shape: generate a new key, replace the secret, delete the old key on the app page.

## Linked tickets & conformance

Forge links PRs to Linear tickets and GitHub issues deterministically - no LLM involved. The ticket step extracts refs with a strict cascade: head branch name, then PR title, then PR body, then commit messages. The first source that yields a match wins per provider; multiple distinct tickets in that source are all checked. No match is normal: the review proceeds and the rollup notes no linked ticket.

| Pattern | Example |
|---------|---------|
| Linear key in branch | `feature/FRG-5-login`, `FRG-5-slug`, `hotfix/FRG-5/...`, or Linear's canonical `gitBranchName` (e.g. `sean/frg-5-...`) |
| Linear key in title/body/commits | `FRG-5`, `Closes FRG-5`, `Fixes FRG-5` |
| GitHub issue | `#123`, `owner/repo#123` |

To get linking: name the branch or PR after the ticket key, or reference it in the title, body, or commits. For Linear ticket fetching, set the optional `LINEAR_API_KEY` secret; GitHub issues need no new secret (Forge app token).

The ticket step writes `ticket.md` / `ticket.json` beside `mr.diff` and passes them to PR-Agent, both Thermos lenses, and master as delimited untrusted quoted context - ticket text is data, never instructions. The master rollup gains a `## Ticket conformance` section: resolved tickets with links, per-criterion addressed / not evident / contradicted verdicts with file/line evidence, a neutral unrelated-changes list, and one overall line (`conforms`, `partially conforms`, `does not conform`, or `no linked ticket`). On re-push, verify re-evaluates open criteria and posts a summary re-check comment. Advisory only, never a merge gate. Sparse tickets produce sparse verdicts.

The PR-Agent producer step also sets `PR_REVIEWER__CHECK_PR_ADDITIONAL_CONTENT=true` (the dynaconf env override for `pr_reviewer.check_pr_additional_content`), intended to flag changes unrelated to the stated intent. Note: in the pinned pr-agent 0.46.0 plain-diff mode that flag and the forwarded `LINEAR_API_KEY` are not consumed upstream; Forge's own ticket artifact and conformance section provide the ticket awareness today.

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

The full flow runs in GitHub Actions. Master and verify read pull request state, post comments, and resolve threads through the GitHub API and GraphQL, using the repository, pull request number, and head SHA from the workflow environment plus the Forge app token the workflow mints. Verify has a `--dry-run` flag that reports verdicts without resolving, and it still needs a real pull request and token to read threads from.

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

Forge runs on GitHub. This repo lives in the `shippopotamuslabs` org, and a GitHub App named Shippopotamus Forge holds the review identity there: pull request and check permissions, with contents read-only. The reusable workflow ships from this repo, and consuming repos pin a release tag. Stratum is the first consumer: a proof pull request with deliberate defects caught every planted defect class before the port landed. The v1.1.0 tag lands on the port's merge commit.

Forge came out of the Stratum project's CI in September 2026 and has reviewed its changes since v1.0.0. The current release line stands at v1.0.12, dated October 4, 2026.

## Versioning and upgrades

Releases are Git tags: `v1.0.0` through `v1.0.12` at the time of writing. There is no version file. `CHANGELOG.md` carries one entry per tag, and the entries describe the failure each fix addresses.

To upgrade a consuming project, check the changelog, then change the pinned ref in the caller workflow and the `forge_ref` input to the same new value. Projects pin their own ref; nothing upgrades underneath a consumer.

## Contributing

Read `AGENTS.md` before changing anything. The short version: every behavior change in `.forge/` ships with a focused regression test under `tests/`, and a release takes a changelog entry, a tag on the changelog commit, and the tag pushed to the remote in the same flow.

## License

0BSD license. Do whatever you want with the code. Just don't expect it to work or sue me.

 PR-Agent runs unmodified from PyPI at review time and stays under its own license, AGPL-3.0. Running an unmodified AGPL package inside your own CI carries no source-offer obligation for your code.
