#!/usr/bin/env python3
"""Regression tests for .forge/verify.py diff scoping.

Bug: verify passed `diff[:8000]` to the LLM, silently dropping whole files
from large MR diffs. The LLM then verdicted NOT_ADDRESSED with
"diff does not touch X" for fixes that were present (Stratum MR168: the
74k-char diff's 8k window covered only app/ files, so every domain-file
finding — fork validation, atomic reset, add-custom tiers — was reported
unaddressed).

Fix: scope the verification diff to the finding's cited files
(`path[N-M]` Fix Focus Areas entries and `[path[LN-M]](url)` Code links),
with an explicit marker (never a silent cut) when truncation still applies,
and legacy head-truncation only when no files are cited.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import os
import unittest


def _load_verify():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".forge", "verify.py")
    spec = importlib.util.spec_from_file_location("verify", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


verify = _load_verify()

FORK = "core/domain/src/main/kotlin/com/stratum/domain/lift/ForkEquipmentUseCase.kt"
VM = "app/src/main/java/com/stratum/liftconfig/LiftIncrementsViewModel.kt"


def _section(path, body):
    return f"diff --git a/{path} b/{path}\n{body}"


class ExtractFocusFilesTest(unittest.TestCase):
    def test_parses_fix_focus_areas_and_code_links(self):
        body = (
            "## Fix Focus Areas - " + FORK + "[22-33] - " + VM + "[425-433] ##\n"
            "Code [" + VM + "[L425-433]](https://example.com/blob/x#L425-433)"
        )
        self.assertEqual(verify.extract_focus_files(body), [FORK, VM])

    def test_dedupes_and_ignores_non_paths(self):
        body = (
            "Fix Focus Areas - " + FORK + "[22-33] - " + FORK + "[22-33]. "
            "See v1.0.9 (2026-09-23) and note [3] for context."
        )
        self.assertEqual(verify.extract_focus_files(body), [FORK])

    def test_empty_when_nothing_cited(self):
        self.assertEqual(verify.extract_focus_files("No files here, just prose."), [])


class SplitDiffSectionsTest(unittest.TestCase):
    def test_splits_on_headers_keyed_by_new_path(self):
        doc = _section(FORK, "@@ -1 +1 @@\n-old\n+new") + "\n" + _section(VM, "@@ -2 +2 @@\n-x\n+y")
        sections = verify.split_diff_sections(doc)
        self.assertEqual([p for p, _ in sections], [FORK, VM])
        self.assertTrue(sections[0][1].startswith("diff --git "))
        self.assertIn("+new", sections[0][1])
        self.assertIn("+y", sections[1][1])


class SelectDiffForFindingTest(unittest.TestCase):
    def _big_diff(self):
        # Cited file sits far past char 8000, as in the MR168 74k diff.
        filler = _section(VM, "@@ -1 +1 @@\n" + " context line\n" * 600)
        target = _section(FORK, "@@ -22,10 +22,15 @@\n+validateMicroAgainstSource")
        other = _section("core/domain/X.kt", "@@ -1 +1 @@\n+unrelated")
        return "\n".join([filler, other, target])

    def test_cited_file_past_truncation_is_included(self):
        diff = self._big_diff()
        self.assertGreater(len(diff), 8000)
        self.assertNotIn("validateMicroAgainstSource", diff[:8000])
        body = "High finding. Fix Focus Areas - " + FORK + "[22-33]"
        scoped = verify.select_diff_for_finding(diff, body)
        self.assertIn("validateMicroAgainstSource", scoped)
        self.assertIn("diff --git a/" + FORK, scoped)
        # Unrelated files stay out.
        self.assertNotIn("unrelated", scoped)
        self.assertNotIn(VM, scoped)

    def test_missing_cited_file_falls_back_to_head(self):
        diff = _section(VM, "@@ -1 +1 @@\n+vm change")
        body = "Fix Focus Areas - missing/Nope.kt[1-2]"
        self.assertEqual(verify.select_diff_for_finding(diff, body), diff)

    def test_no_cited_files_keeps_legacy_head_truncation(self):
        diff = "x" * 9000
        scoped = verify.select_diff_for_finding(diff, "plain prose finding")
        self.assertTrue(scoped.startswith("x" * 8000))
        # Legacy fallback still marks the cut instead of failing silently.
        self.assertIn("NOT reviewed", scoped)


class TruncateMarkedTest(unittest.TestCase):
    def test_passthrough_under_budget(self):
        self.assertEqual(verify.truncate_marked("short", 8000), "short")

    def test_marks_truncation_explicitly(self):
        out = verify.truncate_marked("y" * 100, 10)
        self.assertTrue(out.startswith("y" * 10))
        self.assertIn("NOT reviewed", out)


if __name__ == "__main__":
    unittest.main()
