# AGENTS.md — forge-reviewer contributor notes

## Gospels

The cross-repo canon lives in the `shippopotamuslabs/gospels` repo,
pinned in this repo as a submodule at `contracts/gospel/`. Read the
gospels there; bump the pin deliberately, and never edit the
submodule's contents from this repo.

- Architecture of the Rust crate (`agent/`) follows
  `contracts/gospel/ARCHITECTURE-GOSPEL.md` §15, the Rust dialect.
  §§0–14 state the intent §15 translates; where the two differ for
  Rust, §15 governs.
- Anything a person reads — `.forge/prompts/`, posted review
  comments, docs — follows `contracts/gospel/WUMBO-GOSPEL.md`.
- Tests follow `contracts/gospel/TESTING-GOSPEL.md`: the general
  sections apply in full; its Android-specific §6 does not apply
  here.
- The UI gospels (Palette, Compose, Glass Bars, Button Shape) do
  not apply to this repo; it has no UI surface.

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

## PR titles and commit messages

PR titles follow Conventional Commits, because squash merging makes the
PR title the commit subject on main - and Forge's ticket cascade reads
titles when linking a PR to its ticket.

Format:

    type(scope): imperative subject (KEY-123)

- type - feat, fix, polish, chore, docs, refactor, test, ci, build,
  perf, style. Types mirror the Linear type labels (feature -> feat,
  bug -> fix).
- scope - the area the change touches, lowercase: a module, layer, or
  surface.
- subject - imperative mood, lowercase, no trailing period:
  "add linear progression", not "adds" or "added".
- (KEY-123) - the Linear ticket key in parentheses at the end, when
  the work has a ticket. Branch names stay the primary ticket link
  (name the branch after the key); the title key is the backup the
  cascade reads second.

Examples:

    feat(tickets): add linked-ticket extraction cascade (FRG-6)
    docs(setup): document bring-your-own GitHub App (FRG-2)

A CI check (pr-title) enforces the shape on every PR. Individual
commit messages on a branch are NOT linted - write them for the
humans and agents reading the branch, and let the PR title carry
the standard.
