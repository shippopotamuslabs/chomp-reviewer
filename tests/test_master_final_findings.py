#!/usr/bin/env python3
"""Regression tests for the FRG-28 post-only mode in .forge/master.py.

With the flag on, chomp already synthesized: master.py must post the
final findings JSON plus the summary/conformance extracted from the
decision log, without invoking the LLM half and without changing the
posting functions or the forge-recorded.json shape.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import json
import os
import subprocess
import sys
import tempfile
import unittest


def _load_master():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".forge", "master.py")
    spec = importlib.util.spec_from_file_location("master", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


master = _load_master()

DIFF = """diff --git a/src/app.py b/src/app.py
--- a/src/app.py
+++ b/src/app.py
@@ -10,3 +10,4 @@ def handle(req):
 line10
+added11
 line12
"""


def _write(directory, name, obj):
    path = os.path.join(directory, name)
    with open(path, "w", encoding="utf-8") as fh:
        json.dump(obj, fh)
    return path


class SynthesisOutputsTest(unittest.TestCase):
    def test_extracts_summary_and_conformance(self):
        log = {
            "version": 1,
            "entries": [
                {"kind": "contract", "event": "entry_absent", "detail": "x"},
                {"kind": "synthesis", "event": "completed", "detail": "1 finding"},
                {"kind": "synthesis", "event": "summary", "detail": "One real bug."},
                {"kind": "synthesis_conformance",
                 "conformance": {"overall": "conforms", "criteria": []}},
            ],
        }
        summary, conformance = master.synthesis_outputs(log)
        self.assertEqual(summary, "One real bug.")
        self.assertEqual(conformance, {"overall": "conforms", "criteria": []})

    def test_missing_entries_degrade_to_empty(self):
        self.assertEqual(master.synthesis_outputs({"version": 1, "entries": []}), ("", None))
        self.assertEqual(master.synthesis_outputs({"version": 1}), ("", None))
        self.assertEqual(master.synthesis_outputs(None), ("", None))

    def test_wrong_shapes_are_ignored_not_trusted(self):
        log = {
            "version": 1,
            "entries": [
                {"kind": "synthesis", "event": "summary", "detail": ["not", "text"]},
                {"kind": "synthesis_conformance", "conformance": ["not", "an", "object"]},
                "not-an-entry",
            ],
        }
        self.assertEqual(master.synthesis_outputs(log), ("", None))

    def test_first_summary_wins(self):
        log = {
            "version": 1,
            "entries": [
                {"kind": "synthesis", "event": "summary", "detail": "first"},
                {"kind": "synthesis", "event": "summary", "detail": "second"},
            ],
        }
        summary, _ = master.synthesis_outputs(log)
        self.assertEqual(summary, "first")


class PostOnlyDryRunTest(unittest.TestCase):
    def _run_post_only(self, extra_args, env_extra=None):
        here = os.path.dirname(os.path.abspath(__file__))
        with tempfile.TemporaryDirectory() as directory:
            findings = _write(directory, "findings.json", [
                {"file": "src/app.py", "line_start": 11, "line_end": 11,
                 "severity": "High", "category": "Correctness",
                 "title": "t", "description": "d", "evidence": "e",
                 "fix_focus_areas": ["src/app.py[11-11]"], "recommended_fix": "r"},
            ])
            decisions = _write(directory, "decisions.json", {
                "version": 1,
                "entries": [
                    {"kind": "synthesis", "event": "summary", "detail": "One real bug."},
                ],
            })
            diff = os.path.join(directory, "mr.diff")
            with open(diff, "w", encoding="utf-8") as fh:
                fh.write(DIFF)
            env = dict(os.environ)
            env["OPENROUTER_API_KEY"] = "dry-run-dummy"
            env["FORGE_PLATFORM"] = "github"
            if env_extra:
                env.update(env_extra)
            proc = subprocess.run(
                [sys.executable,
                 os.path.join(here, "..", ".forge", "master.py"),
                 "--iid", "7", "--sha", "abc123",
                 "--final-findings", findings,
                 "--decisions", decisions,
                 "--diff", diff] + extra_args,
                capture_output=True, text=True, env=env, timeout=120,
            )
            return proc

    def test_dry_run_prints_final_payload_without_posting(self):
        proc = self._run_post_only(["--dry-run"])
        self.assertEqual(proc.returncode, 0, proc.stderr)
        payload = json.loads(proc.stdout)
        self.assertEqual(payload["summary"], "One real bug.")
        self.assertEqual(len(payload["findings"]), 1)
        self.assertEqual(payload["findings"][0]["title"], "t")
        self.assertIsNone(payload["conformance"])
        self.assertIn("Posting 1 chomp findings", proc.stderr)

    def test_half_modes_are_rejected(self):
        here = os.path.dirname(os.path.abspath(__file__))
        with tempfile.TemporaryDirectory() as directory:
            findings = _write(directory, "findings.json", [])
            diff = os.path.join(directory, "mr.diff")
            with open(diff, "w", encoding="utf-8") as fh:
                fh.write(DIFF)
            env = dict(os.environ)
            env["OPENROUTER_API_KEY"] = "dry-run-dummy"
            env["FORGE_PLATFORM"] = "github"
            proc = subprocess.run(
                [sys.executable,
                 os.path.join(here, "..", ".forge", "master.py"),
                 "--iid", "7", "--sha", "abc123",
                 "--final-findings", findings,
                 "--diff", diff, "--dry-run"],
                capture_output=True, text=True, env=env, timeout=120,
            )
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("--final-findings and --decisions must be given together",
                          proc.stderr)

    def test_bad_findings_shape_is_rejected(self):
        here = os.path.dirname(os.path.abspath(__file__))
        with tempfile.TemporaryDirectory() as directory:
            findings = _write(directory, "findings.json", {"not": "a list"})
            decisions = _write(directory, "decisions.json", {"version": 1, "entries": []})
            diff = os.path.join(directory, "mr.diff")
            with open(diff, "w", encoding="utf-8") as fh:
                fh.write(DIFF)
            env = dict(os.environ)
            env["OPENROUTER_API_KEY"] = "dry-run-dummy"
            env["FORGE_PLATFORM"] = "github"
            proc = subprocess.run(
                [sys.executable,
                 os.path.join(here, "..", ".forge", "master.py"),
                 "--iid", "7", "--sha", "abc123",
                 "--final-findings", findings,
                 "--decisions", decisions,
                 "--diff", diff, "--dry-run"],
                capture_output=True, text=True, env=env, timeout=120,
            )
            self.assertNotEqual(proc.returncode, 0)
            self.assertIn("must be a JSON array", proc.stderr)


if __name__ == "__main__":
    unittest.main()
