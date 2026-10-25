#!/usr/bin/env python3
"""Regression tests for .forge/ticket.py deterministic extraction.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import json
import os
import subprocess
import sys
import unittest


def _load_ticket():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".forge", "ticket.py")
    spec = importlib.util.spec_from_file_location("ticket", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


ticket = _load_ticket()


def _keys(items):
    return [(t["provider"], t["key"], t["source"]) for t in items]


class CascadeLevelTest(unittest.TestCase):
    def test_branch_level(self):
        got = ticket.extract_tickets("feature/STR-123", "", "", [])
        self.assertEqual(_keys(got), [("linear", "STR-123", "branch")])

    def test_title_level(self):
        got = ticket.extract_tickets("main", "Fix STR-123 broke", "", [])
        self.assertEqual(_keys(got), [("linear", "STR-123", "title")])

    def test_body_level(self):
        got = ticket.extract_tickets("main", "", "see STR-123 here", [])
        self.assertEqual(_keys(got), [("linear", "STR-123", "body")])

    def test_commits_level(self):
        got = ticket.extract_tickets("main", "", "",
                                     ["wip", "fix STR-123 oops"])
        self.assertEqual(_keys(got), [("linear", "STR-123", "commits")])


class BranchPatternTest(unittest.TestCase):
    def test_dash_suffix_branch(self):
        got = ticket.extract_tickets("STR-123-add-feature", "", "", [])
        self.assertEqual([t["key"] for t in got], ["STR-123"])

    def test_hotfix_nested_branch(self):
        got = ticket.extract_tickets("hotfix/ABC-123/urgent", "", "", [])
        self.assertEqual([t["key"] for t in got], ["ABC-123"])

    def test_gitbranchname_lowercase_normalized(self):
        got = ticket.extract_tickets("sean/frg-5-wire-ticket-extract",
                                     "", "", [])
        self.assertEqual([t["key"] for t in got], ["FRG-5"])
        self.assertEqual(got[0]["provider"], "linear")


class PrecedenceTest(unittest.TestCase):
    def test_branch_beats_title_same_provider(self):
        got = ticket.extract_tickets("feature/STR-123", "Fixes STR-999",
                                     "", [])
        self.assertEqual(_keys(got), [("linear", "STR-123", "branch")])

    def test_title_beats_body_same_provider(self):
        got = ticket.extract_tickets("main", "STR-1", "STR-2", [])
        self.assertEqual(_keys(got), [("linear", "STR-1", "title")])

    def test_per_provider_winners_combine(self):
        # Linear wins at title, github wins at body: both kept.
        got = ticket.extract_tickets("main", "STR-7", "fixes #42", [])
        self.assertEqual(_keys(got), [("linear", "STR-7", "title"),
                                      ("github", "#42", "body")])


class MultiTicketTest(unittest.TestCase):
    def test_multi_ticket_set(self):
        got = ticket.extract_tickets("main", "STR-1 and STR-2", "", [])
        self.assertEqual([t["key"] for t in got], ["STR-1", "STR-2"])

    def test_dedupe_preserves_order(self):
        got = ticket.extract_tickets("main", "", "STR-1 STR-1 STR-2", [])
        self.assertEqual([t["key"] for t in got], ["STR-1", "STR-2"])


class GitHubTest(unittest.TestCase):
    def test_bare_issue(self):
        got = ticket.extract_tickets("main", "", "fixes #123", [])
        self.assertEqual(_keys(got), [("github", "#123", "body")])
        self.assertEqual(got[0]["id"], "123")

    def test_qualified_issue(self):
        got = ticket.extract_tickets("main", "owner/repo#123 broke", "",
                                     [])
        self.assertEqual(_keys(got),
                         [("github", "owner/repo#123", "title")])

    def test_qualified_does_not_double_count_bare(self):
        got = ticket.extract_from_source("body", "see owner/repo#123")
        self.assertEqual(len(got), 1)
        self.assertEqual(got[0]["key"], "owner/repo#123")

    def test_closing_keywords(self):
        for word in ("Closes", "Fixes", "closes", "fixes"):
            got = ticket.extract_tickets("main", "", "%s #7" % word, [])
            self.assertEqual([t["key"] for t in got], ["#7"],
                             msg=word)


class ClosesLinearTest(unittest.TestCase):
    def test_closes_linear_key(self):
        got = ticket.extract_tickets("main", "", "Closes STR-123", [])
        self.assertEqual([t["key"] for t in got], ["STR-123"])

    def test_fixes_linear_key(self):
        got = ticket.extract_tickets("main", "", "Fixes FRG-5", [])
        self.assertEqual([t["key"] for t in got], ["FRG-5"])


class NoMatchTest(unittest.TestCase):
    def test_no_match_returns_empty(self):
        self.assertEqual(ticket.extract_tickets("main", "hello",
                                                "world", ["wip"]), [])

    def test_empty_inputs(self):
        self.assertEqual(ticket.extract_tickets("", "", "", []), [])


class HelperAndCliTest(unittest.TestCase):
    def test_extract_from_source_hint(self):
        got = ticket.extract_from_source("title", "STR-1 and #2",
                                         provider_hint="github")
        self.assertEqual([t["key"] for t in got], ["#2"])
        self.assertTrue(all(t["source"] == "title" for t in got))

    def test_cli_prints_json(self):
        here = os.path.dirname(os.path.abspath(__file__))
        path = os.path.join(here, "..", ".forge", "ticket.py")
        proc = subprocess.run(
            [sys.executable, path, "--branch", "feature/STR-123",
             "--title", "t", "--body", "b"],
            capture_output=True, text=True)
        self.assertEqual(proc.returncode, 0)
        data = json.loads(proc.stdout)
        self.assertEqual([(t["provider"], t["key"]) for t in data],
                         [("linear", "STR-123")])


if __name__ == "__main__":
    unittest.main()
