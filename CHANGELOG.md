# Changelog

## v1.0.5 (2026-09-21)

- Fix: include `verify.py` in the `.forge-setup` download list (was missing, causing `forge:master` to fail on verification runs).

## v1.0.4 (2026-09-21)

- Feat: add `verify.py` for AI-based discussion resolution on subsequent pushes. `forge:master` now checks for an existing consolidated review; if found, it runs verification (LLM determines ADDRESSED/NOT_ADDRESSED for each open Forge discussion, resolves addressed ones) instead of re-posting the full review. Eliminates reviewer churn.

## v1.0.3 (2026-09-21)

- Fix: set `OPENROUTER__KEY` (dynaconf) and `CONFIG__MODEL` for PR-Agent; without these it fell back to GPT-5/dummy_token.

## v1.0.2 (2026-09-21)

- Fix: add missing `review` command to `pr_agent.cli` invocation (was failing with "arguments are required: command, rest").

## v1.0.1 (2026-09-21)

- Fix: single-pass placeholder substitution in `thermos.py` and `master.py`. The chained `.replace()` could corrupt prompts if a diff or findings contained a placeholder string (e.g. `{file_contents}`).
- Fix: pin `pr-agent==0.46.0` in a venv (was unpinned `pip install pr-agent` with `--break-system-packages`).

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
