#!/usr/bin/env python3
"""Shape tests for the repo's own CI workflow (.github/workflows/tests.yml).

FRG-4: forge-reviewer's own suites must run in GitHub Actions on pull
requests and be required on main. These tests pin the workflow's contract:
both suites present, the documented commands, PR trigger types matching
the ticket (opened, synchronize, reopened, edited).

Run:  python3 -m unittest discover -s tests -v
"""
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
WORKFLOW = os.path.join(HERE, "..", ".github", "workflows", "tests.yml")


def _read(path):
    with open(path, "r", encoding="utf-8") as fh:
        return fh.read()


class TestsWorkflowShape(unittest.TestCase):
    @classmethod
    def setUpClass(cls):
        cls.text = _read(WORKFLOW)

    def test_triggers_on_pull_requests_with_ticket_types(self):
        self.assertIn("pull_request:", self.text)
        for trigger in ("opened", "synchronize", "reopened", "edited"):
            self.assertIn(trigger, self.text)

    def test_python_suite_job(self):
        self.assertIn("python-tests", self.text)
        self.assertIn("python3 -m unittest discover -s tests", self.text)

    def test_rust_suite_job(self):
        self.assertIn("rust-tests", self.text)
        self.assertIn("working-directory: agent", self.text)
        self.assertIn("cargo fmt --check", self.text)
        self.assertIn("cargo clippy --all-targets", self.text)
        self.assertIn("cargo test --locked", self.text)
        self.assertIn("cargo build --release --locked", self.text)

    def test_uniffi_free_tree_assertion(self):
        self.assertIn("cargo tree", self.text)
        self.assertIn("grep -i uniffi", self.text)


if __name__ == "__main__":
    unittest.main()
