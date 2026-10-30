#!/usr/bin/env python3
"""Regression tests for the FRG-5 wiring in .github/workflows/forge-review.yml.

FRG-5 wires PR-Agent's ticket context into the Forge GitHub workflow:
an optional LINEAR_API_KEY secret (declared, and forwarded to the PR-Agent
producer step only) plus pr_reviewer.check_pr_additional_content enabled via
dynaconf env override. The producer stays silent and fail-soft.

Verified against the pinned pr-agent 0.49.0 source (wheel inspect):
- The package contains no "linear" reference: LINEAR_API_KEY is not a key
  pr-agent reads; forwarding is future-proof and a no-op when absent.
- check_pr_additional_content appears only in pr-agent's help docs; no .py
  module or configuration.toml default reads it. Setting
  PR_REVIEWER__CHECK_PR_ADDITIONAL_CONTENT follows the same dynaconf
  SECTION__KEY env convention the workflow already uses (CONFIG__MODEL,
  OPENROUTER__KEY), so a pr-agent version that honors the flag picks it up.
- In --diff-file plain-diff mode the handler cannot resolve tickets:
  PlainDiffGitProvider.get_pr_title() returns "Local diff review",
  get_pr_description_full()/get_pr_branch()/get_commit_messages() return "",
  so ticket extraction has no title/description/branch to work from. The
  narrow fallback (secret forwarding + flag, no PR-context guessing) is
  therefore the whole of this change; no invented env keys are passed.

Run:  python3 -m unittest discover -s tests -v
"""
import os
import re
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
WORKFLOW = os.path.join(HERE, "..", ".github", "workflows", "forge-review.yml")
README = os.path.join(HERE, "..", "README.md")


def _read(path):
    with open(path, "r", encoding="utf-8") as fh:
        return fh.read()


def _steps(text):
    """Split the jobs: review steps into (name, body) pairs, stdlib-only."""
    chunks = re.split(r"(?m)^      - name: ", text)
    named = []
    for chunk in chunks[1:]:
        name, _, body = chunk.partition("\n")
        named.append((name.strip(), body))
    return named


class ForgeReviewWorkflowTest(unittest.TestCase):
    def test_linear_api_key_declared_optional(self):
        text = _read(WORKFLOW)
        decl = re.search(
            r"(?m)^    secrets:\n(?:^ {6,}.*\n)*?"
            r"^      LINEAR_API_KEY:\n^        required: false\n",
            text,
        )
        self.assertIsNotNone(
            decl, "workflow_call.secrets must declare LINEAR_API_KEY with required: false"
        )

    def test_pr_agent_step_forwards_secret_and_flag(self):
        steps = _steps(_read(WORKFLOW))
        producer = [b for n, b in steps if n.startswith("PR-Agent producer")]
        self.assertEqual(len(producer), 1, "expected exactly one PR-Agent producer step")
        body = producer[0]
        self.assertIn("LINEAR_API_KEY: ${{ secrets.LINEAR_API_KEY }}", body)
        self.assertIn('PR_REVIEWER__CHECK_PR_ADDITIONAL_CONTENT: "true"', body)

    def test_linear_secret_forwarded_to_pr_agent_and_ticket_steps(self):
        # FRG-6b: the ticket step also receives LINEAR_API_KEY (consumer
        # key for the Linear GraphQL fetch); no other step may hold it.
        steps = _steps(_read(WORKFLOW))
        holders = [n for n, b in steps if "secrets.LINEAR_API_KEY" in b]
        self.assertEqual(
            holders,
            ["Fetch linked ticket context",
             "PR-Agent producer (silent, plain-diff)"],
            "LINEAR_API_KEY must be forwarded only to the ticket and "
            "PR-Agent producer steps",
        )

    def test_producer_stays_silent_plain_diff_fail_soft(self):
        text = _read(WORKFLOW)
        # fallback() shape preserved exactly.
        self.assertIn(
            "          fallback() {\n"
            '            echo "::warning::$1"\n'
            "            printf '## PR Reviewer Guide\\n\\n(%s)\\n' \"$1\" > ../forge/pr-agent-findings.md\n"
            "          }",
            text,
        )
        # Still plain-diff mode, still pinned unmodified install, still soft exits.
        self.assertIn("--diff-file=../forge/mr.diff", text)
        self.assertIn('"pr-agent[github]==${{ inputs.pr_agent_version }}"', text)
        steps = _steps(_read(WORKFLOW))
        body = [b for n, b in steps if n.startswith("PR-Agent producer")][0]
        self.assertEqual(body.count("exit 0"), 2, "both fallback paths must stay fail-soft")

    def test_readme_documents_optional_secret(self):
        text = _read(README)
        self.assertIn("LINEAR_API_KEY", text)
        self.assertIn("CHECK_PR_ADDITIONAL_CONTENT", text)

    def test_job_level_concurrency_serializes_per_pr(self):
        # FRG-11: the review job serializes runs per caller-repo PR so
        # concurrent pushes cannot post duplicate consolidated reviews.
        # Extract the job block so a stray copy elsewhere cannot pass.
        text = _read(WORKFLOW)
        m = re.search(r"(?m)^  review:\n(.*?)(?=^    steps:\n)", text,
                      re.DOTALL)
        self.assertIsNotNone(m, "jobs.review block must exist")
        job_head = m.group(1)
        conc = re.search(
            r"(?m)^    concurrency:\n"
            r"^      group: forge-review-\$\{\{ github\.repository \}\}"
            r"-\$\{\{ github\.event\.pull_request\.number \}\}\n"
            r"^      cancel-in-progress: false\n"
            r"^      queue: max\n",
            job_head,
        )
        self.assertIsNotNone(
            conc, "jobs.review must carry the per-PR concurrency block")

    def test_marker_recheck_step_order_and_routing(self):
        # FRG-11: a marker re-check immediately before Master routes the
        # queued run to verify; producers keep gating on the first check.
        steps = _steps(_read(WORKFLOW))
        names = [n for n, _ in steps]
        self.assertEqual(names.count(
            "Re-check for an existing consolidated review"), 1,
            "expected exactly one marker re-check step")
        idx_recheck = names.index(
            "Re-check for an existing consolidated review")
        self.assertEqual(
            names[idx_recheck - 1], "Thermos structure lens",
            "re-check must run after the Thermos structure lens")
        self.assertTrue(
            names[idx_recheck + 1].startswith("Master"),
            "re-check must sit immediately before the Master step")
        recheck_id = [b for n, b in steps
                      if n == "Re-check for an existing consolidated review"][0]
        self.assertIn("id: marker_recheck", recheck_id)
        self.assertIn("forge:consolidated-review", recheck_id)
        self.assertIn("GITHUB_OUTPUT", recheck_id)
        self.assertIn('"exists=%s', recheck_id)
        self.assertIn('"yes" if found else "no"', recheck_id)
        self.assertNotIn("LINEAR_API_KEY", recheck_id)
        master_body = [b for n, b in steps if n.startswith("Master")][0]
        self.assertIn("steps.marker_recheck.outputs.exists", master_body)
        self.assertNotIn("steps.marker.outputs.exists", master_body)
        for n, b in steps:
            if n.startswith(("PR-Agent producer", "Thermos correctness",
                             "Thermos structure")):
                self.assertIn("steps.marker.outputs.exists", b,
                              "%s must still gate on the first marker check"
                              % n)


if __name__ == "__main__":
    unittest.main()
