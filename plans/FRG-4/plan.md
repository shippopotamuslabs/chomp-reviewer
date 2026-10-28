# FRG-4 — Run the repo's own test suites in GitHub Actions (plan)

Ticket scope: workflow running `python3 -m unittest discover -s tests` on PRs
(opened, synchronize, reopened, edited), then the check made required on main
(PR required, 0 approvals, up-to-date branches on).

Extension (Sean, 2026-10-28, "actually have the CI run tests"): the repo now
also has the Rust agent crate (FRG-15), so the workflow gets a second job —
cargo fmt --check, clippy -D warnings, cargo test --locked, release build,
and the no-uniffi cargo-tree assertion — and all three checks (pr-title,
python-tests, rust-tests) become required. Push-to-main trigger added so
main itself produces the check runs.

Files: .github/workflows/tests.yml (new), tests/test_tests_workflow.py
(workflow-shape test mirroring tests/test_forge_review_workflow.py),
no .forge changes, no CHANGELOG (CI-only; release flow is for .forge
functional changes).

Branch protection via the GitHub API with the Tensor app token; if the
token lacks admin on this repo, hand Sean the exact settings instead.
