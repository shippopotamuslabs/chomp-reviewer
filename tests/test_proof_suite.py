#!/usr/bin/env python3
"""Regression tests for the FRG-29 proof suite fixtures and harness.

Covers what can be proven without a live model: the scratch tree
shape, the case diffs applying cleanly, the case JSON contracts, and
the harness's mechanical scoring (hint checks, drop audit, check
mode). The live --run path needs OPENROUTER_API_KEY and is exercised
by the operator, not CI.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import json
import os
import subprocess
import sys
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))
PROOF = os.path.join(HERE, "..", "proof")
SCRATCH = os.path.join(PROOF, "scratch")
CASES = os.path.join(PROOF, "cases")


def _load_harness():
    path = os.path.join(PROOF, "run_proof.py")
    spec = importlib.util.spec_from_file_location("run_proof", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


harness = _load_harness()

EXPECTED_CASES = {
    "class1-upstream-parse",
    "class2-downstream-shape",
    "class3a-contract-direct-read",
    "class3b-contract-self-excuse",
    "class3c-no-invented-law",
    "class4a-control-injection",
    "class4b-control-swallowed",
    "structure-duplicate-helper",
}


def _read(rel):
    with open(os.path.join(SCRATCH, rel), encoding="utf-8") as fh:
        return fh.read()


class ScratchTreeTest(unittest.TestCase):
    def test_tree_has_the_realistic_shape(self):
        for rel in ("AGENTS.md", "docs/db-conventions.md", ".pr_agent.toml",
                    "src/parse.py", "src/main.py", "src/store.py",
                    "src/consumer.py", "src/helpers.py", "src/billing.py",
                    "tests/test_parse.py"):
            self.assertTrue(os.path.isfile(os.path.join(SCRATCH, rel)), rel)

    def test_contract_is_routed_not_decorative(self):
        self.assertIn("docs/db-conventions.md", _read("AGENTS.md"))
        self.assertIn("with_retry", _read("docs/db-conventions.md"))
        self.assertIn("def format_money", _read("src/helpers.py"))

    def test_no_docstring_rule_is_stated(self):
        self.assertNotIn("docstring", _read("AGENTS.md").lower(),
                         "class3c needs an unstated rule to test no-invented-law")


class CaseContractTest(unittest.TestCase):
    def test_all_eight_cases_present(self):
        names = {os.path.splitext(f)[0] for f in os.listdir(CASES) if f.endswith(".json")}
        self.assertEqual(names, EXPECTED_CASES)

    def test_case_json_contract(self):
        for name in EXPECTED_CASES:
            with open(os.path.join(CASES, name + ".json"), encoding="utf-8") as fh:
                case = json.load(fh)
            with self.subTest(case=name):
                self.assertEqual(case["name"], name)
                for key in ("class", "title", "defect", "diff",
                            "expect_2x", "expect_1x", "fp_budget"):
                    self.assertIn(key, case, key)
                self.assertTrue(os.path.isfile(os.path.join(CASES, case["diff"])))
                for cited in case["expect_2x"].get("must_cite", []) or []:
                    self.assertTrue(os.path.isfile(os.path.join(SCRATCH, cited)),
                                    f"{name}: must_cite {cited} resolves in the tree")

    def test_classes_cover_one_through_four(self):
        classes = set()
        for name in EXPECTED_CASES:
            with open(os.path.join(CASES, name + ".json"), encoding="utf-8") as fh:
                classes.add(json.load(fh)["class"])
        self.assertTrue({1, 2, 3, 4} <= classes, classes)


class HarnessMechanicsTest(unittest.TestCase):
    def test_check_mode_passes(self):
        proc = subprocess.run(
            [sys.executable, os.path.join(PROOF, "run_proof.py"), "--check"],
            capture_output=True, text=True, timeout=120)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        self.assertIn("fixtures OK: 8 cases", proc.stdout)

    def test_list_mode_needs_no_keys(self):
        env = {k: v for k, v in os.environ.items() if k != "OPENROUTER_API_KEY"}
        proc = subprocess.run(
            [sys.executable, os.path.join(PROOF, "run_proof.py"), "--list"],
            capture_output=True, text=True, env=env, timeout=60)
        self.assertEqual(proc.returncode, 0, proc.stderr)
        for name in EXPECTED_CASES:
            self.assertIn(name, proc.stdout)

    def test_run_mode_refuses_cleanly_without_a_key(self):
        env = {k: v for k, v in os.environ.items() if k != "OPENROUTER_API_KEY"}
        proc = subprocess.run(
            [sys.executable, os.path.join(PROOF, "run_proof.py"),
             "--run", "class1-upstream-parse", "--chomp", "/bin/true"],
            capture_output=True, text=True, env=env, timeout=60)
        self.assertNotEqual(proc.returncode, 0)
        self.assertIn("OPENROUTER_API_KEY", proc.stderr)

    def test_hint_checks(self):
        findings = [{"file": "src/main.py", "title": "t",
                     "description": "empty PORT crashes",
                     "evidence": "e", "recommended_fix": "r",
                     "fix_focus_areas": [], "citations": ["src/main.py:4"]}]
        hints = harness.hint_checks(
            {"verdict": "caught", "must_cite": ["src/main.py"],
             "must_mention": ["empty", "absent-word"]}, findings)
        self.assertTrue(hints["cites:src/main.py"])
        self.assertTrue(hints["mentions:empty"])
        self.assertFalse(hints["mentions:absent-word"])
        self.assertTrue(hints["has_findings"])
        self.assertTrue(harness.hint_checks(
            {"verdict": "clean"}, [])["no_findings"])
        self.assertFalse(harness.hint_checks(
            {"verdict": "clean"}, findings)["no_findings"])

    def test_drop_audit(self):
        log = {"version": 1, "entries": [
            {"kind": "synthesis_disposition", "producer": 1,
             "cluster": "cluster-0001", "disposition": "kept", "final": 1},
            {"kind": "synthesis_disposition", "producer": 2,
             "cluster": "cluster-0001", "disposition": "merged-into:1", "final": 1},
            {"kind": "synthesis_disposition", "producer": 3,
             "cluster": "cluster-0002", "disposition": "dropped:unverified-claim",
             "reason": "unverified-claim; no caller evidence"},
            {"kind": "synthesis", "event": "completed", "detail": "x"},
        ]}
        records, issues = harness.drop_audit(log)
        self.assertEqual(len(records), 3)
        self.assertEqual(issues, [])
        self.assertEqual(records[2]["reason"], "unverified-claim; no caller evidence")

        bad = {"version": 1, "entries": [
            {"kind": "synthesis_disposition", "producer": 1,
             "cluster": "cluster-0001", "disposition": "dropped:  "},
            {"kind": "synthesis_disposition", "producer": 2,
             "cluster": "cluster-0002", "disposition": "kept"},
        ]}
        _, issues = harness.drop_audit(bad)
        self.assertEqual(len(issues), 2)


if __name__ == "__main__":
    unittest.main()
