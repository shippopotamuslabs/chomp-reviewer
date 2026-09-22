#!/usr/bin/env python3
"""Regression tests for .forge/master.py inline-discussion positioning.

Bug: master posted each inline with a single blind
`position[new_line] = line_start` guess. GitLab rejects positions on lines
outside the MR diff (deleted lines, lines outside hunks, unknown paths),
so those findings were silently dropped from inline — the rollup Guide
listed N issues while fewer than N appeared as inline discussions.

Fix: parse mr.diff into valid diff-line maps, resolve each finding to the
nearest valid line (old_line anchoring for deletion-only files, cross-file
fallback for unknown paths), and retry across candidate positions.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import os
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
@@ -10,7 +10,8 @@ def handle(req):
 line10
-old11
+new11
+added12
 line13
@@ -50,3 +51,2 @@ def other():
 ctx51
-del52
-del53
 ctx54
diff --git a/old.txt b/old.txt
--- a/old.txt
+++ b/old.txt
@@ -1,2 +1,0 @@ def gone():
-gone1
-gone2
diff --git a/new.txt b/new.txt
--- /dev/null
+++ b/new.txt
@@ -0,0 +1,3 @@ def fresh():
+l1
+l2
+l3
"""


def _attempts(filename, start, end=None):
    parsed, order = master.parse_diff_line_map(DIFF)
    return master.build_inline_attempts(
        parsed, order, filename, start, end if end is not None else start)


def _assert_all_valid(test, parsed, attempts):
    test.assertTrue(attempts, "every finding must yield at least one attempt")
    for a in attempts:
        entry = parsed["files"][a["new_path"]]
        if a.get("new_line") is not None:
            test.assertIn(a["new_line"], entry["new_lines"])
        else:
            test.assertIn(a["old_line"], entry["old_lines"])


class ParseDiffLineMapTest(unittest.TestCase):
    def test_added_context_and_deleted_lines(self):
        parsed, order = master.parse_diff_line_map(DIFF)
        app = parsed["files"]["src/app.py"]
        self.assertEqual(app["new_lines"], {10, 11, 12, 13, 51, 52})
        self.assertEqual(app["old_lines"], {10, 11, 12, 50, 51, 52, 53})
        self.assertEqual(order, ["src/app.py", "old.txt", "new.txt"])

    def test_deletion_only_and_new_files(self):
        parsed, _ = master.parse_diff_line_map(DIFF)
        self.assertEqual(parsed["files"]["old.txt"]["new_lines"], set())
        self.assertEqual(parsed["files"]["old.txt"]["old_lines"], {1, 2})
        self.assertEqual(parsed["files"]["new.txt"]["new_lines"], {1, 2, 3})

    def test_empty_diff_has_no_positions(self):
        parsed, order = master.parse_diff_line_map("")
        self.assertEqual(parsed["files"], {})
        self.assertEqual(order, [])


class BuildInlineAttemptsTest(unittest.TestCase):
    def test_exact_hit_is_not_relocated(self):
        attempts, meta = _attempts("src/app.py", 11, 12)
        self.assertEqual(attempts[0]["new_line"], 11)
        self.assertEqual(attempts[0]["new_path"], "src/app.py")
        self.assertFalse(meta["relocated"])

    def test_off_diff_line_resolves_to_nearest(self):
        # Line 30 is outside any hunk; nearest valid new lines are 13 and 51.
        attempts, meta = _attempts("src/app.py", 30)
        self.assertEqual(attempts[0]["new_line"], 13)
        self.assertTrue(meta["relocated"])

    def test_range_prefers_in_range_line(self):
        attempts, _ = _attempts("src/app.py", 30, 52)
        self.assertEqual(attempts[0]["new_line"], 51)

    def test_deletion_only_file_uses_old_anchor(self):
        attempts, meta = _attempts("old.txt", 1, 2)
        self.assertIsNone(attempts[0]["new_line"])
        self.assertEqual(attempts[0]["old_line"], 1)
        self.assertEqual(attempts[0]["old_path"], "old.txt")
        self.assertFalse(meta["relocated"])

    def test_unknown_file_still_lands_inline(self):
        attempts, meta = _attempts("no/such.py", 99)
        self.assertTrue(attempts)
        self.assertTrue(meta["relocated"])
        # Falls back to a real diff line, so GitLab still accepts it inline.
        self.assertIn(attempts[0]["new_path"], ("src/app.py", "old.txt", "new.txt"))

    def test_every_finding_yields_valid_positions(self):
        """The reported bug: 3 Guide issues but only 1 inline. All three
        shapes below (valid, deletion-only, off-hunk) must resolve to
        positions GitLab accepts."""
        parsed, order = master.parse_diff_line_map(DIFF)
        for filename, start, end in [("src/app.py", 11, 12),
                                     ("old.txt", 1, 2),
                                     ("src/app.py", 30, 30)]:
            attempts, _ = master.build_inline_attempts(parsed, order, filename, start, end)
            _assert_all_valid(self, parsed, attempts)


class PostInlineWithFallbackTest(unittest.TestCase):
    def test_retries_until_accepted(self):
        calls = []

        def post_fn(body, attempt):
            calls.append(attempt)
            if len(calls) < 3:
                raise Exception("400 Invalid position")
            return {"id": 7}

        attempts = [{"new_path": "src/app.py", "old_path": "src/app.py",
                     "new_line": n, "old_line": None} for n in (30, 31, 11)]
        result, won = master.post_inline_with_fallback(post_fn, "body", attempts)
        self.assertEqual(result, {"id": 7})
        self.assertEqual(won, 2)
        self.assertEqual(len(calls), 3)

    def test_all_rejected_raises(self):
        def post_fn(body, attempt):
            raise Exception("400 Invalid position")

        with self.assertRaises(RuntimeError):
            master.post_inline_with_fallback(post_fn, "body", [
                {"new_path": "x", "old_path": "x", "new_line": 1, "old_line": None}])


class BuildInlineAnchorNoteTest(unittest.TestCase):
    def test_relocated_body_notes_original_location(self):
        finding = {"severity": "High", "category": "Bug", "title": "T",
                   "description": "D", "fix_focus_areas": [], "recommended_fix": "R"}
        body = master.build_inline(
            "sha", finding,
            {"relocated": True,
             "requested": {"file": "src/app.py", "line_start": 30, "line_end": 30}})
        self.assertIn("forge:consolidated-review", body)
        self.assertIn("src/app.py[L30-30]", body)

    def test_exact_hit_has_no_anchor_note(self):
        finding = {"severity": "Low", "category": "Bug", "title": "T",
                   "description": "D", "fix_focus_areas": [], "recommended_fix": "R"}
        body = master.build_inline(
            "sha", finding,
            {"relocated": False,
             "requested": {"file": "src/app.py", "line_start": 11, "line_end": 11}})
        self.assertIn("forge:consolidated-review", body)
        self.assertNotIn("forge:anchor", body)


if __name__ == "__main__":
    unittest.main()
