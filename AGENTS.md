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
5. Publish the release as part of the same flow — including when the
   change is going out via MR. Push the branch and the tag
   (`git push origin <branch> vX.Y.Z`) and verify with
   `git ls-remote --tags origin`. A local-only tag is NOT a release
   (this bit us with v1.0.7: upstream showed only v1.0.6 until the tag
   was pushed, blocking consumers pinning `FORGE_REF`).
6. Any other `push` still needs an explicit ask.

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
