#!/usr/bin/env python3
"""Tests for the CHOMP-61 size gate (.forge/size_gate.py).

The gate measures the fetched PR diff before any producer runs. Over
the limit it posts the consolidated review carrying a single size
verdict and exits non-zero, so an oversized PR fails the chomp-review
check deterministically instead of dying in synthesis with a
compaction error that posts nothing. At or under the limit it exits 0
and posts nothing.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import os
import tempfile
import unittest
from unittest import mock


def _load(name):
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".forge", f"{name}.py")
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


size_gate = _load("size_gate")


def _diff_with_additions(n, path="f.py"):
    lines = [
        f"diff --git a/{path} b/{path}",
        f"--- a/{path}",
        f"+++ b/{path}",
        f"@@ -0,0 +1,{n} @@",
    ]
    lines += [f"+line {i}" for i in range(n)]
    return "\n".join(lines) + "\n"


SIMPLE_DIFF = """\
diff --git a/src/a.py b/src/a.py
--- a/src/a.py
+++ b/src/a.py
@@ -1,3 +1,4 @@
 context
-old
+new
+added
 tail
"""

# Content lines inside a hunk can themselves start with -- or ++ (a
# deleted line whose text is "-- old header"): hunk state, not the
# prefix, decides they are content, and the real +++/--- header lines
# above the hunk never count.
TRICKY_DIFF = """\
diff --git a/notes.md b/notes.md
--- a/notes.md
+++ b/notes.md
@@ -1,2 +1,2 @@
--- old header text
+++ new header text
"""

MULTI_DIFF = """\
diff --git a/x.py b/x.py
--- a/x.py
+++ b/x.py
@@ -1 +1 @@
-a
\\ No newline at end of file
+b
diff --git a/logo.png b/logo.png
Binary files a/logo.png and b/logo.png differ
"""


class MeasureDiffTest(unittest.TestCase):
    def test_counts_added_and_deleted_lines(self):
        self.assertEqual(size_gate.measure_diff(SIMPLE_DIFF), (3, 1))

    def test_header_lines_never_count_but_hunk_content_does(self):
        self.assertEqual(size_gate.measure_diff(TRICKY_DIFF), (2, 1))

    def test_multiple_files_and_binary_section(self):
        self.assertEqual(size_gate.measure_diff(MULTI_DIFF), (2, 2))

    def test_empty_diff(self):
        self.assertEqual(size_gate.measure_diff(""), (0, 0))

    def test_generated_counts(self):
        self.assertEqual(size_gate.measure_diff(_diff_with_additions(7)), (7, 1))


class RunGateTest(unittest.TestCase):
    def test_over_limit_posts_verdict_and_fails(self):
        posted = []
        rc = size_gate.run(_diff_with_additions(2001), 2000, "abc123", posted.append)
        self.assertEqual(rc, 1)
        self.assertEqual(len(posted), 1)
        body = posted[0]
        self.assertIn("<!-- chomp:consolidated-review sha:abc123 -->", body)
        self.assertIn("Too big. Break it up.", body)
        self.assertIn("2,001", body)
        self.assertIn("2,000", body)
        self.assertIn("across 1 file.", body)

    def test_exactly_at_limit_proceeds_without_posting(self):
        posted = []
        rc = size_gate.run(_diff_with_additions(2000), 2000, "abc123", posted.append)
        self.assertEqual(rc, 0)
        self.assertEqual(posted, [])

    def test_under_limit_proceeds_without_posting(self):
        posted = []
        rc = size_gate.run(SIMPLE_DIFF, 2000, "abc123", posted.append)
        self.assertEqual(rc, 0)
        self.assertEqual(posted, [])


class MainTest(unittest.TestCase):
    def _write_diff(self, text):
        fh = tempfile.NamedTemporaryFile(
            "w", suffix=".diff", delete=False, encoding="utf-8")
        with fh:
            fh.write(text)
        self.addCleanup(os.unlink, fh.name)
        return fh.name

    def test_over_limit_posts_through_github_backend(self):
        diff_path = self._write_diff(_diff_with_additions(5))
        summary = self._write_diff("")
        with mock.patch.object(
                size_gate.github_backend, "post_issue_comment",
                return_value={"id": 42}) as post, \
             mock.patch.dict(os.environ, {"GITHUB_STEP_SUMMARY": summary}):
            rc = size_gate.main([
                "--diff", diff_path, "--max-lines", "3",
                "--iid", "96", "--sha", "deadbeef"])
        self.assertEqual(rc, 1)
        self.assertEqual(post.call_count, 1)
        body = post.call_args[0][4]
        self.assertIn("sha:deadbeef", body)
        self.assertIn("Too big. Break it up.", body)
        with open(summary, encoding="utf-8") as fh:
            self.assertIn("Too big. Break it up.", fh.read())

    def test_nonpositive_limit_is_a_config_error(self):
        diff_path = self._write_diff(_diff_with_additions(5))
        with mock.patch.object(
                size_gate.github_backend, "post_issue_comment") as post:
            rc = size_gate.main([
                "--diff", diff_path, "--max-lines", "0",
                "--iid", "96", "--sha", "deadbeef"])
        self.assertEqual(rc, 2)
        post.assert_not_called()

    def test_missing_diff_is_an_error(self):
        rc = size_gate.main([
            "--diff", "/nonexistent/mr.diff", "--max-lines", "2000",
            "--iid", "96", "--sha", "deadbeef"])
        self.assertEqual(rc, 2)


if __name__ == "__main__":
    unittest.main()
