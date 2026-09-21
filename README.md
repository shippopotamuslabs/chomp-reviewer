# Forge Reviewer

A CI-native consolidated code reviewer for GitLab. Combines three independent review sources into **one** deduplicated review:

1. **PR-Agent** (Qodo) — runs as a silent producer in plain-diff mode
2. **Thermos correctness lens** — LLM review focused on bugs, security, logic
3. **Thermos structure lens** — LLM review focused on maintainability, architecture

A master job semantically deduplicates all findings via LLM and posts **one** `## PR Reviewer Guide` rollup plus one deduplicated inline discussion set. No independent posts from producers. Review-once semantics: later pushes verify-and-resolve, never re-review.

Runs entirely in GitLab CI. No external cron, no sandbox, no subagents.

## Usage

Add to your `.gitlab-ci.yml`:

```yaml
include:
  - project: 'browntown.development/forge-reviewer'
    ref: v1.0.0          # pin to a release tag
    file: '/templates/forge.gitlab-ci.yml'

variables:
  FORGE_ENABLED: "true"  # opt-in
```

Set these CI variables in your project (masked):

| Variable | Required | Purpose |
|----------|----------|---------|
| `PR_AGENT_GITLAB_TOKEN` | Yes | GitLab token with `api` scope. Fetches diffs, downloads Forge scripts, posts reviews. |
| `OPENROUTER_API_KEY` | Yes | OpenRouter key for thermos lenses + master dedup. |
| `FORGE_MODEL` | No | Model ID. Default: `openrouter/meta/muse-spark-1.3-contributor`. The `openrouter/` prefix is stripped for direct API calls. |
| `FORGE_REF` | No | forge-reviewer ref for script downloads. Default matches the `include` ref. Change both when upgrading. |
| `FORGE_ENABLED` | No | Set `"true"` to enable. Default `"false"`. |

Add a `.pr_agent.toml` to your repo root with review guidance. The `forge:review` job fails if it's missing.

## Pipeline

```
forge:diff
  ├── forge:review (PR-Agent, silent producer)
  ├── forge:thermos-correctness
  └── forge:thermos-structure
        │
        ▼
  forge:master (dedup + single post)
```

All jobs run in the `lint` stage and execute in parallel with your other CI jobs. The three producers depend only on `forge:diff` via `needs:`, so they start the moment the diff is ready.

## Runner requirements

Jobs are tagged `home-server`. If your runner uses a different tag, fork the template or override the `tags:` in your `.gitlab-ci.yml` with `extends:`.

## Upgrading

1. Check the [CHANGELOG](CHANGELOG.md) for breaking changes.
2. Bump the `ref:` in your `include:` **and** the `FORGE_REF` variable to the new tag.
3. Push a test MR to verify.

## Versioning

Releases are Git tags (`v1.0.0`, `v1.1.0`, ...). The template downloads scripts pinned to `FORGE_REF`, so upgrading is just changing two lines.

## License

The `.forge/` scripts are original work. PR-Agent (invoked as an unmodified PyPI package at runtime) is AGPL-3.0 — running it unmodified in your own CI does not trigger source-offer obligations.
