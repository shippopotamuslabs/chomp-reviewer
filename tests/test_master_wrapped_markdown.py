#!/usr/bin/env python3
"""Regression tests for FRG-8: wrapped markdown descriptions/evidence.

Finding descriptions/evidence must render as ordinary wrapped markdown,
not inside HTML <pre> blocks. The Agent Prompt triple-backtick fence is
a deliberate copy-paste artifact and is left unchanged.

Run:  python3 -m unittest tests.test_master_wrapped_markdown -v
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


LONG_DESC = (
    "This is a very long description line that should wrap naturally "
    "in rendered markdown instead of scrolling horizontally inside a "
    "preformatted block, and it keeps going with plenty of detail."
)
MULTI_DESC = "First paragraph of the description.\n\nSecond paragraph of the description."
MULTI_EVIDENCE = "First evidence line.\n\nSecond evidence line."


def _finding(**kw):
    f = {
        "severity": "High",
        "category": "Bug",
        "title": "T",
        "file": "src/a.py",
        "line_start": 1,
        "line_end": 2,
        "description": LONG_DESC,
        "evidence": "e",
        "fix_focus_areas": [],
        "recommended_fix": "fix it",
    }
    f.update(kw)
    return f


def _rollup(finding):
    return master.build_rollup(
        sha="abc", summary="S", findings=[finding], project_path="o/r")


def _description_section(out):
    start = out.index("<summary>Description</summary>")
    end = out.index("<summary>Code</summary>")
    return out[start:end]


def _evidence_section(out):
    start = out.index("<summary>Evidence</summary>")
    end = out.index("Agent Prompt")
    return out[start:end]


class NoPreTest(unittest.TestCase):
    def test_rollup_has_no_pre(self):
        self.assertNotIn("<pre", _rollup(_finding()))
        self.assertNotIn("</pre>", _rollup(_finding()))

    def test_inline_has_no_pre(self):
        out = master.build_inline("abc", _finding())
        self.assertNotIn("<pre", out)
        self.assertNotIn("</pre>", out)


class DescriptionQuotingTest(unittest.TestCase):
    def test_multiline_description_lines_carry_quote_prefix(self):
        out = _rollup(_finding(description=MULTI_DESC))
        section = _description_section(out)
        for line in MULTI_DESC.split("\n"):
            if line.strip():
                self.assertIn("> " + line, section)
            else:
                self.assertIn("\n>\n", section)

    def test_multiline_evidence_lines_carry_quote_prefix(self):
        out = _rollup(_finding(evidence=MULTI_EVIDENCE))
        section = _evidence_section(out)
        for line in MULTI_EVIDENCE.split("\n"):
            if line.strip():
                self.assertIn("> " + line, section)


class MarkerTest(unittest.TestCase):
    def test_marker_present_in_both_outputs(self):
        finding = _finding()
        self.assertIn("forge:consolidated-review", _rollup(finding))
        self.assertIn("forge:consolidated-review",
                      master.build_inline("abc", finding))


if __name__ == "__main__":
    unittest.main()
