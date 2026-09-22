# AGENTS.md — forge-reviewer contributor notes

## Tests

- Run: `python3 -m unittest discover -s tests -v` (stdlib only, no network).
- Every bug fix or behavior change in `.forge/` must add or update a focused
  test under `tests/`. Probe in `/tmp` if needed, but the durable test lives
  in the repo.

## Release process (required for every functional change)

1. Implement the fix in `.forge/` plus its regression test.
2. Add a `## vX.Y.Z (YYYY-MM-DD)` entry at the top of `CHANGELOG.md`.
3. Commit following repo convention: functional commit first
   (`fix:` / `feat:` / `chore:`), then a separate `docs: changelog for vX.Y.Z`
   commit. Patch-bump for fixes (`v1.0.6` → `v1.0.7`).
4. Tag the changelog commit (`git tag vX.Y.Z`). Versions ARE git tags;
   there is no version file.
5. Never `push` (branch or tags) unless the user explicitly asks.

## Conventions

- `.forge/` scripts are stdlib-only (`urllib`, no third-party imports) so
  they run on a bare runner. Keep it that way.
- Do not bump the `FORGE_REF` default (`v1.0.0`) in
  `templates/forge.gitlab-ci.yml` per release — consumers pin `ref:` and
  `FORGE_REF` themselves (see README "Upgrading").
- Compiled bytecode is untracked and gitignored (`__pycache__/`). Never
  commit `.pyc` files.
- `forge:master` must never silently drop a finding: unpostable items log
  ERROR (not WARNING) and are recorded in `forge-recorded.json`.
- Keep `verify.py` compatible with any `forge-recorded.json` shape changes
  (it re-fetches discussions via API; the JSON file is write-only context).
