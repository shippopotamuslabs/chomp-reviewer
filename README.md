# Forge Reviewer

Forge posts one code review per merge request. Three reviewers run first, each on its own: PR-Agent, plus two LLM lenses called Thermos. A fourth job reads all three outputs, merges findings that flag the same problem, and posts the result as a single review: one summary comment and one set of inline discussions. The producers post nothing themselves.

Forge runs as jobs inside a project's own GitLab CI pipeline. A project includes one template file, sets two tokens, and gets a review on every merge request pipeline. Reviews run once per merge request. Pushes after the first review get verification: Forge checks its open discussions against the new code and resolves the ones the push fixed.

## How a review runs

Five jobs, all in the `lint` stage:

```
forge:diff
  |-- forge:review              (PR-Agent)
  |-- forge:thermos-correctness
  |-- forge:thermos-structure
  +-- forge:master              (runs after the three producers)
```

`forge:diff` fetches the merge request changes from the GitLab API and assembles them into one unified diff, shared with the other jobs as the `mr.diff` artifact. The three producers depend only on `forge:diff`, so they start as soon as the diff exists. `forge:master` waits for all three producer artifacts.

Every job starts by downloading the `.forge/` scripts and prompts from this repo at the tag named by `FORGE_REF`. The included template holds job definitions only. The code under review always runs a pinned, released version of Forge.

## The three producers

**PR-Agent.** Qodo's PR-Agent, installed from PyPI at the pinned version 0.46.0 and run unmodified, reviews `mr.diff` in plain-diff mode with posting turned off. Its markdown review becomes one of the three master inputs.

**Thermos correctness lens.** An LLM prompt scoped to defects: bugs, crashes, security holes, data integrity, API misuse, concurrency hazards. It stays out of style and structure. A clean diff gets an empty result.

**Thermos structure lens.** An LLM prompt scoped to maintainability and design. It flags files a change pushes past 1,000 lines, new branches woven into already tangled methods, duplicated logic where a helper already exists, and abstractions that leak across module boundaries. The prompt calls itself a thermonuclear maintainability audit, and it rates hard.

Both lenses run the same script, `.forge/thermos.py`, with a different prompt file. The producer jobs and `forge:master` carry `allow_failure: true` in the template, so a failed review job never blocks a merge pipeline. `forge:diff` runs with `allow_failure: false`. No diff, no review.

## Thermos

Thermos is the pair of LLM review lenses, `.forge/thermos.py` plus two prompt files in `.forge/prompts/`:

- `thermos-correctness.md` for the correctness and security lens
- `thermos-structure.md` for the structure and maintainability lens

Each lens sends the merge request diff to an OpenRouter chat model and gets back a JSON array of findings. Every finding carries a file, a line range, a severity (Critical, High, Medium, Low), a category, a title, a description, evidence, fix focus areas, and a recommended fix. The shared client in `.forge/openrouter_client.py` calls the model at temperature 0.1 with a 16,000 token response cap. The model comes from `FORGE_MODEL` and defaults to `meta/muse-spark-1.3-contributor` on OpenRouter.

The lenses read the diff. CI mode skips fetching full changed-file contents, so a finding rests on the diff and the context inside it.

## The master job

`.forge/master.py` sends the three producer outputs and the diff to the same model family with the dedup prompt in `.forge/prompts/master-dedup.md`. Findings with the same root cause on overlapping lines collapse into one, with the evidence combined and every contributing source recorded (`pr-agent`, `thermos-a`, `thermos-b`). A finding the model cannot decide on stays in, capped at Medium.

Master posts two things:

1. One rollup comment headed `## PR Reviewer Guide`, with severity counts and a section per finding: description, a code link to the flagged lines, evidence, and an Agent Prompt block that restates the issue, the fix focus areas, and the recommended fix in a copyable prompt for AI coding tools.
2. One inline discussion per finding, anchored to the flagged lines.

GitLab accepts inline positions only on lines inside the diff. Master parses `mr.diff` into per-file line maps and resolves each finding to the nearest valid line, trying up to five candidate positions before giving up on a placement. A comment that had to move carries an invisible `forge:anchor` note with the originally flagged location. A finding with no placeable position gets logged at ERROR level and written to the `failed` list in `forge-recorded.json`, a job artifact that records every posted discussion ID and anchor.

## Review once, then verify

The rollup and every inline discussion carry a `forge:consolidated-review` marker. On later pipelines for the same merge request, `forge:master` checks the merge request notes for that marker. Finding it switches the job to `.forge/verify.py`.

Verify collects the open Forge discussions, skipping the rollup note and threads that already resolved. For each finding it builds the evidence fresh:

- the current merge request diff, scoped to the files the finding cites
- the cited files fetched at the new head SHA, as ground truth for whether the flagged code still exists

The final-state fetch matters because the diff is net against the target branch. Code the merge request added and later removed shows no removal hunk, and a verdict based on the diff alone reads that as still present.

The model returns ADDRESSED or NOT_ADDRESSED with a reason. Verify posts the verdict as a reply on the original discussion, with the verified SHA in the reply. ADDRESSED threads get resolved. A reply or resolve failure logs an ERROR, the run continues through the remaining discussions, and the job exits 1 at the end if any failed.

## Setup

Add the template to a project's `.gitlab-ci.yml`:

```yaml
include:
  - project: 'browntown.development/forge-reviewer'
    ref: v1.0.12         # pin to a release tag
    file: '/templates/forge.gitlab-ci.yml'

variables:
  FORGE_ENABLED: "true"  # opt-in
```

Set these CI variables on the consuming project, masked:

| Variable | Required | Purpose |
|----------|----------|---------|
| `PR_AGENT_GITLAB_TOKEN` | Yes | GitLab token with `api` scope. Fetches diffs, downloads the Forge scripts, posts reviews. |
| `OPENROUTER_API_KEY` | Yes | OpenRouter key for the Thermos lenses, the master dedup, and verification. |
| `FORGE_MODEL` | No | Model ID. Default: `openrouter/meta/muse-spark-1.3-contributor`. The `openrouter/` prefix comes off before direct API calls. |
| `FORGE_REF` | No | The forge-reviewer tag the jobs download scripts from. Match it to the `include` ref. |
| `FORGE_ENABLED` | No | `"true"` turns Forge on for the project. Default `"false"`. |

The project also needs a `.pr_agent.toml` at its root with review guidance for PR-Agent. The `forge:review` job fails when the file is missing.

Jobs carry the `home-server` runner tag. Projects on differently tagged runners override `tags:` with `extends:` or fork the template.

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

Fetch a merge request diff the way `forge:diff` does:

```
python3 .forge/fetch_diff.py https://gitlab.com/api/v4 <project_id> <mr_iid> <token> --output mr.diff
```

Preview a consolidated review without posting it. This takes the three producer artifacts as inputs and prints the deduplicated review as JSON. It still calls OpenRouter, so it spends an API call:

```
OPENROUTER_API_KEY=<key> python3 .forge/master.py --iid <mr_iid> --sha <head_sha> \
  --pr-agent pr-agent-findings.md \
  --thermos-a thermos-correctness.json \
  --thermos-b thermos-structure.json \
  --diff mr.diff --dry-run
```

The full flow needs GitLab. Master and verify read merge request state, post discussions, and resolve threads through the GitLab API, using `CI_API_V4_URL`, `CI_PROJECT_ID`, `CI_MERGE_REQUEST_IID`, and `CI_COMMIT_SHA` from the pipeline environment plus the two tokens above. Verify has a `--dry-run` flag that reports verdicts without resolving, and it still needs a real merge request and token to read discussions from.

## Repository layout

| Path | Job |
| --- | --- |
| `.forge/fetch_diff.py` | Fetches MR changes from the GitLab API, assembles mr.diff |
| `.forge/fetch_diff_github.py` | Fetches PR changes from the GitHub API |
| `.forge/github_backend.py` | The GitHub backend: PR and diff reads, rollup and inline review posting, review-thread resolution |
| `.github/workflows/forge-review.yml` | The reusable workflow consuming repos call (the GitHub port, in review) |
| `.forge/thermos.py` | Runs one Thermos lens against a diff, writes findings JSON |
| `.forge/master.py` | Dedups the three producer outputs, posts the review |
| `.forge/verify.py` | Verdicts open discussions on later pushes, resolves fixed ones |
| `.forge/openrouter_client.py` | Shared OpenRouter client and model selection |
| `.forge/prompts/` | The prompt files: `thermos-correctness.md`, `thermos-structure.md`, `master-dedup.md` |
| `templates/forge.gitlab-ci.yml` | The include template consuming projects use |
| `tests/` | Regression tests for the .forge scripts, unittest based |
| `CHANGELOG.md` | One entry per release tag |
| `AGENTS.md` | Contributor rules: tests, release process, conventions |

## Platform status

GitLab is the shipping platform. The current release is v1.0.12, dated October 4, 2026, and every released job, script, and prompt targets the GitLab API. Forge came out of the Stratum project's CI in September 2026 and has run on GitLab merge request pipelines since v1.0.0.

The GitHub port exists and is in review. This repo moved to the `shippopotamuslabs` GitHub org on October 24, 2026, and a GitHub App named Shippopotamus Forge holds the review identity there: pull request and check permissions, with contents read-only. The GitHub version ships as a reusable workflow (`.github/workflows/forge-review.yml`) that consuming repos call; the scripts pick their platform backend from the environment, and the GitLab code paths stay in place. Stratum is the first consumer: its wiring and a proof pull request with deliberate defects stand open while the port gets verified end to end. The GitHub release tag for the port lands with its merge.

## Versioning and upgrades

Releases are Git tags: `v1.0.0` through `v1.0.12` at the time of writing. There is no version file. `CHANGELOG.md` carries one entry per tag, and the entries describe the failure each fix addresses.

To upgrade a consuming project, check the changelog, then change the tag in the `include:` ref and in `FORGE_REF` to the same new value. The template's built-in `FORGE_REF` default stays at `v1.0.0` on purpose. Projects pin their own ref.

## Contributing

Read `AGENTS.md` before changing anything. The short version: every behavior change in `.forge/` ships with a focused regression test under `tests/`, and a release takes a changelog entry, a tag on the changelog commit, and the tag pushed to the remote in the same flow.

## License

The `.forge/` scripts are original work. PR-Agent runs unmodified from PyPI at review time and stays under its own license, AGPL-3.0. Running an unmodified AGPL package inside your own CI carries no source-offer obligation for your code.
