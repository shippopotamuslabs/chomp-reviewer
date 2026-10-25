#!/usr/bin/env python3
"""Regression tests for FRG-7a: ticket-conformance rollup section.

master emits an optional "conformance" object; build_rollup renders a
"## Ticket conformance" section (resolved tickets, per-criterion verdicts
with evidence, neutral unrelated-changes list, single overall line); the
verdict is advisory only and defaults keep historical output byte-identical.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import json
import os
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

PROMPT_DIR = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                          "..", ".forge", "prompts")

DETAILS = [{"key": "STR-123", "title": "Cap hourly syncs",
            "url": "https://linear.app/x/issue/STR-123"}]

CONF = {
    "overall": "partially conforms",
    "criteria": [
        {"criterion": "Cap syncs at 5/hr",
         "status": "contradicted",
         "evidence": "src/sync.py/L40-60"},
        {"criterion": "Retry with backoff",
         "status": "addressed",
         "evidence": "src/sync.py/L70-80"},
        {"criterion": "Emit a metric per sync",
         "status": "not evident",
         "evidence": "no metric call in src/sync.py"},
    ],
    "unrelated_changes": ["Reformats src/other.py with no ticket basis"],
}


def _rollup(**kw):
    args = dict(sha="abc", summary="S", findings=[],
                project_path="o/r")
    args.update(kw)
    return master.build_rollup(**args)


class ConformanceOverallVerdictsTest(unittest.TestCase):
    def test_all_four_overall_verdicts_render_verbatim(self):
        for overall in ("conforms", "partially conforms",
                        "does not conform", "no linked ticket"):
            conf = {"overall": overall, "criteria": [],
                    "unrelated_changes": []}
            out = _rollup(tickets=["STR-123"], ticket_details=DETAILS,
                          conformance=conf)
            self.assertIn("## Ticket conformance", out)
            self.assertIn("Overall: %s" % overall, out)


class ConformanceSectionContentTest(unittest.TestCase):
    def test_resolved_tickets_show_identifier_title_link(self):
        out = _rollup(tickets=["STR-123"], ticket_details=DETAILS,
                      conformance=CONF)
        self.assertIn("STR-123", out)
        self.assertIn("Cap hourly syncs", out)
        self.assertIn("https://linear.app/x/issue/STR-123", out)

    def test_criteria_lines_carry_status_and_evidence(self):
        out = _rollup(tickets=["STR-123"], ticket_details=DETAILS,
                      conformance=CONF)
        self.assertIn("- [contradicted] Cap syncs at 5/hr", out)
        self.assertIn("src/sync.py/L40-60", out)
        self.assertIn("- [addressed] Retry with backoff", out)
        self.assertIn("- [not evident] Emit a metric per sync", out)

    def test_unrelated_changes_listed_neutrally(self):
        out = _rollup(tickets=["STR-123"], ticket_details=DETAILS,
                      conformance=CONF)
        self.assertIn("Unrelated changes:", out)
        self.assertIn("Reformats src/other.py", out)

    def test_no_ticket_section_when_no_ticket(self):
        out = _rollup(tickets=[])
        self.assertIn("## Ticket conformance", out)
        self.assertIn("Overall: no linked ticket", out)

    def test_no_section_without_ticket_or_conformance(self):
        self.assertNotIn("## Ticket conformance", _rollup())
        self.assertNotIn("## Ticket conformance",
                         _rollup(tickets=["STR-123"],
                                 ticket_details=DETAILS))

    def test_sparse_ticket_invents_no_criteria(self):
        conf = {"overall": "conforms", "criteria": [],
                "unrelated_changes": [],
                "note": "No stated criteria in the linked ticket."}
        out = _rollup(tickets=["STR-123"], ticket_details=DETAILS,
                      conformance=conf)
        self.assertIn("Overall: conforms", out)
        self.assertIn("No stated criteria", out)
        self.assertNotIn("- [addressed]", out)
        self.assertNotIn("- [not evident]", out)
        self.assertNotIn("- [contradicted]", out)


class ConformanceDefaultsTest(unittest.TestCase):
    def test_defaults_stay_byte_identical(self):
        args = dict(sha="abc", summary="S", findings=[],
                    project_path="o/r")
        self.assertEqual(
            master.build_rollup(**args),
            master.build_rollup(conformance=None, ticket_details=None,
                                **args))


class TicketDetailsTest(unittest.TestCase):
    def test_details_carry_title_and_url(self):
        with tempfile.TemporaryDirectory() as d:
            with open(os.path.join(d, "ticket.md"), "w") as f:
                f.write("## Linked ticket context\n")
            with open(os.path.join(d, "ticket.json"), "w") as f:
                json.dump({"tickets": [
                    {"provider": "linear", "key": "STR-1",
                     "title": "T", "url": "https://x/STR-1"}],
                    "sources": []}, f)
            details = master.load_ticket_details(
                os.path.join(d, "ticket.md"))
            self.assertEqual(details, [{"key": "STR-1", "title": "T",
                                        "url": "https://x/STR-1"}])

    def test_no_ticket_means_no_details(self):
        self.assertEqual(master.load_ticket_details(None), [])


class PromptTicketInstructionsTest(unittest.TestCase):
    def _read(self, name):
        with open(os.path.join(PROMPT_DIR, name)) as f:
            return f.read()

    def test_master_prompt_documents_conformance(self):
        text = self._read("master-dedup.md")
        self.assertIn('"conformance"', text)
        self.assertIn("conforms|partially conforms|does not conform"
                      "|no linked ticket", text)
        self.assertIn("ONLY from the ticket", text)
        self.assertIn("advisory only", text)
        self.assertIn("NOT findings", text)
        self.assertIn("No stated criteria in the linked ticket.", text)

    def test_correctness_prompt_treats_ticket_as_fact(self):
        text = self._read("thermos-correctness.md")
        self.assertIn("checkable fact", text)
        self.assertIn("5/hr", text)

    def test_structure_prompt_scopes_to_ticket(self):
        text = self._read("thermos-structure.md")
        self.assertIn("ticket-requested refactors are in scope", text)
        self.assertIn("against the ticket", text)


if __name__ == "__main__":
    unittest.main()
