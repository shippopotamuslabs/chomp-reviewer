#!/usr/bin/env python3
"""Regression tests for the FRG-5 wiring in .github/workflows/forge-review.yml.

FRG-5 wires PR-Agent's ticket context into the Forge GitHub workflow:
an optional LINEAR_API_KEY secret (declared, and forwarded to the PR-Agent
producer step only) plus pr_reviewer.check_pr_additional_content enabled via
dynaconf env override. The producer stays silent and fail-soft.

Verified against the pinned pr-agent 0.46.0 source (wheel inspect):
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

    def test_linear_secret_forwarded_only_to_pr_agent_step(self):
        steps = _steps(_read(WORKFLOW))
        holders = [n for n, b in steps if "secrets.LINEAR_API_KEY" in b]
        self.assertEqual(
            holders,
            ["PR-Agent producer (silent, plain-diff)"],
            "LINEAR_API_KEY must be forwarded only to the PR-Agent producer step",
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
        self.assertIn('"pr-agent==${{ inputs.pr_agent_version }}"', text)
        steps = _steps(_read(WORKFLOW))
        body = [b for n, b in steps if n.startswith("PR-Agent producer")][0]
        self.assertEqual(body.count("exit 0"), 2, "both fallback paths must stay fail-soft")

    def test_readme_documents_optional_secret(self):
        text = _read(README)
        self.assertIn("LINEAR_API_KEY", text)
        self.assertIn("CHECK_PR_ADDITIONAL_CONTENT", text)


if __name__ == "__main__":
    unittest.main()
