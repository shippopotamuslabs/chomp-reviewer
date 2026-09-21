# Changelog

## v1.0.0 (2026-09-21)

Initial release. Extracted from the Stratum project's CI-native Forge implementation.

- `forge:diff` — fetches MR diff once via GitLab API, shares as artifact
- `forge:review` — PR-Agent (PyPI, unmodified) as silent producer in plain-diff mode
- `forge:thermos-correctness` — OpenRouter LLM correctness/security lens
- `forge:thermos-structure` — OpenRouter LLM structure/maintainability lens
- `forge:master` — LLM semantic dedup, posts one consolidated review
- Review-once guard: skips if a consolidated review already exists
- Shared `.forge/openrouter_client.py` (no duplicated API code)
- `.forge/fetch_diff.py` (no inline Python in YAML)
- Thermos jobs deduplicated via `.forge-thermos-base`
- `FORGE_ENABLED` opt-in flag, `FORGE_MODEL`/`FORGE_REF` overrides
