#!/usr/bin/env python3
"""Stale-head guard + rollup currency tests (CHOMP-58, CHOMP-59).

Bug (CHOMP-58): the reusable workflow serializes runs per PR with a
queue, so a run executes minutes after its trigger event. Verify and
master acted on the trigger SHA — verify posted NOT_ADDRESSED verdicts
for findings already fixed on the live head (PR #17: verdicts at
128b1fe/30d0206 posted while head was already 70fd7ef), and the first
review posted labelled with a SHA that was no longer head.

Fix: before verify posts any reply and before master posts the review,
fetch the live head; a superseded run posts nothing and exits as a
no-op, and the queued run for the live head does the real work. An
unreadable live head never supersedes.

CHOMP-59: the consolidated rollup's sha stays at the reviewed SHA for
the life of the PR. After a head verify, verify edits the rollup in
place: the marker line (load-bearing for duplicate suppression) is
untouched, and a last-verified line carries currency.

All API access is stubbed; no network.
"""
import importlib.util
import json
import os
import tempfile
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def _load(name):
    path = os.path.join(HERE, "..", ".forge", "%s.py" % name)
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


verify = _load("verify")
master = _load("master")
github_backend = _load("github_backend")

SHA_A = "a" * 40
SHA_B = "b" * 40

ROLLUP = (
    "<!-- chomp:consolidated-review sha:%s -->\n"
    "## PR Reviewer Guide\n\nSummary.\n" % SHA_A
)


class SupersededByTest(unittest.TestCase):
    def test_same_sha_not_superseded(self):
        self.assertFalse(verify.superseded_by(SHA_A, SHA_A))

    def test_moved_head_supersedes(self):
        self.assertTrue(verify.superseded_by(SHA_A, SHA_B))

    def test_unknown_live_head_never_supersedes(self):
        self.assertFalse(verify.superseded_by(SHA_A, None))
        self.assertFalse(verify.superseded_by(SHA_A, ""))

    def test_master_helper_agrees(self):
        self.assertTrue(master.live_head_supersedes(SHA_A, SHA_B))
        self.assertFalse(master.live_head_supersedes(SHA_A, SHA_A))
        self.assertFalse(master.live_head_supersedes(SHA_A, None))


class FetchLiveHeadShaGithubTest(unittest.TestCase):
    def test_returns_head_sha(self):
        orig = github_backend.github_request
        github_backend.github_request = (
            lambda *a, **k: {"head": {"sha": SHA_B}})
        try:
            self.assertEqual(
                github_backend.fetch_live_head_sha(
                    "https://api.github.com", "o/r", 7, "TOK"), SHA_B)
        finally:
            github_backend.github_request = orig

    def test_failure_returns_none(self):
        orig = github_backend.github_request

        def boom(*a, **k):
            raise RuntimeError("nope")

        github_backend.github_request = boom
        try:
            self.assertIsNone(
                github_backend.fetch_live_head_sha(
                    "https://api.github.com", "o/r", 7, "TOK"))
        finally:
            github_backend.github_request = orig

    def test_verify_dispatches_to_backend(self):
        orig = verify.github_backend.fetch_live_head_sha
        verify.github_backend.fetch_live_head_sha = (
            lambda api, repo, iid, tok: SHA_B)
        try:
            self.assertEqual(
                verify.live_head_sha("github", None, None, "7", None,
                                     "api", "o/r", "TOK"), SHA_B)
        finally:
            verify.github_backend.fetch_live_head_sha = orig


class FetchLiveHeadShaGitlabTest(unittest.TestCase):
    def test_verify_reads_mr_sha(self):
        orig = verify.gitlab_request
        verify.gitlab_request = (
            lambda *a, **k: {"sha": SHA_B})
        try:
            self.assertEqual(
                verify.live_head_sha("gitlab", "http://g/api/v4", "1",
                                     "7", "TOK", None, None, None), SHA_B)
        finally:
            verify.gitlab_request = orig

    def test_verify_failure_returns_none(self):
        orig = verify.gitlab_request

        def boom(*a, **k):
            raise RuntimeError("nope")

        verify.gitlab_request = boom
        try:
            self.assertIsNone(
                verify.live_head_sha("gitlab", "http://g/api/v4", "1",
                                     "7", "TOK", None, None, None))
        finally:
            verify.gitlab_request = orig

    def test_master_reads_mr_sha(self):
        orig = master.gitlab_request
        master.gitlab_request = lambda *a, **k: {"sha": SHA_B}
        try:
            self.assertEqual(
                master.fetch_live_head_sha_gitlab(
                    "http://g/api/v4", "1", "7", "TOK"), SHA_B)
        finally:
            master.gitlab_request = orig

    def test_master_exit_failure_returns_none(self):
        orig = master.gitlab_request

        def boom(*a, **k):
            raise SystemExit(1)

        master.gitlab_request = boom
        try:
            self.assertIsNone(
                master.fetch_live_head_sha_gitlab(
                    "http://g/api/v4", "1", "7", "TOK"))
        finally:
            master.gitlab_request = orig


class PostAndRecordSupersededTest(unittest.TestCase):
    """master.post_and_record posts nothing when live head has moved."""

    def _run(self, live_sha):
        posted = []
        orig_live = master.github_backend.fetch_live_head_sha
        orig_env = master.github_backend.gh_env
        orig_post = master.post_review_github
        master.github_backend.fetch_live_head_sha = (
            lambda api, repo, iid, tok: live_sha)
        master.github_backend.gh_env = (
            lambda: ("https://api.github.com", "o/r", "TOK"))

        def fake_post(*a, **k):
            posted.append(True)
            return [], []

        master.post_review_github = fake_post
        args = type("Args", (), {"iid": "7", "sha": SHA_A})()
        cwd = os.getcwd()
        with tempfile.TemporaryDirectory() as td:
            os.chdir(td)
            try:
                master.post_and_record(
                    "github", args, "Summary.", [], "diff", [], None,
                    [], None, None, None)
                with open("forge-recorded.json") as f:
                    recorded = json.load(f)
            finally:
                os.chdir(cwd)
                master.github_backend.fetch_live_head_sha = orig_live
                master.github_backend.gh_env = orig_env
                master.post_review_github = orig_post
        return posted, recorded

    def test_superseded_posts_nothing_and_records_marker(self):
        posted, recorded = self._run(SHA_B)
        self.assertEqual(posted, [])
        self.assertEqual(recorded["superseded_by"], SHA_B)
        self.assertEqual(recorded["discussions"], [])

    def test_current_head_posts_normally(self):
        posted, recorded = self._run(SHA_A)
        self.assertEqual(len(posted), 1)
        self.assertNotIn("superseded_by", recorded)


class ApplyRollupCurrencyTest(unittest.TestCase):
    def test_inserts_after_marker_and_keeps_marker(self):
        out = verify.apply_rollup_currency(ROLLUP, SHA_B)
        self.assertIn(
            "<!-- chomp:consolidated-review sha:%s -->" % SHA_A, out)
        self.assertIn("<!-- chomp:last-verified sha:%s -->" % SHA_B, out)
        self.assertIn("Last verified at `bbbbbbbb`.", out)
        lines = out.split("\n")
        self.assertEqual(
            lines[1], "<!-- chomp:last-verified sha:%s -->" % SHA_B)

    def test_reapply_replaces_instead_of_stacking(self):
        once = verify.apply_rollup_currency(ROLLUP, SHA_B)
        twice = verify.apply_rollup_currency(once, SHA_A)
        self.assertEqual(twice.count("chomp:last-verified"), 1)
        self.assertIn("Last verified at `aaaaaaaa`.", twice)
        # Marker recognition still finds the rollup by its marker.
        self.assertIn("chomp:consolidated-review", twice)

    def test_no_marker_inserts_at_top(self):
        out = verify.apply_rollup_currency("plain body\n", SHA_B)
        self.assertTrue(
            out.startswith("<!-- chomp:last-verified sha:%s -->" % SHA_B))


class MarkRollupSupersededTest(unittest.TestCase):
    def test_labels_and_is_idempotent(self):
        out = verify.mark_rollup_superseded(ROLLUP, SHA_B)
        self.assertIn("Superseded by a review at `bbbbbbbb`.", out)
        self.assertIn("chomp:consolidated-review", out)
        again = verify.mark_rollup_superseded(out, SHA_B)
        self.assertEqual(again.count("chomp:superseded"), 1)


class UpdateRollupCurrencyGithubTest(unittest.TestCase):
    def test_edits_the_marker_comment(self):
        edits = []
        orig_find = verify.github_backend.find_marker_comment
        orig_update = verify.github_backend.update_issue_comment
        verify.github_backend.find_marker_comment = (
            lambda api, repo, iid, tok, marker:
                {"id": 55, "body": ROLLUP}
                if marker.startswith("chomp:") else None)
        verify.github_backend.update_issue_comment = (
            lambda api, repo, cid, tok, body: edits.append((cid, body)))
        try:
            failed = verify.update_rollup_currency(
                "github", None, None, "7", SHA_B, None,
                "api", "o/r", "TOK")
        finally:
            verify.github_backend.find_marker_comment = orig_find
            verify.github_backend.update_issue_comment = orig_update
        self.assertFalse(failed)
        self.assertEqual(len(edits), 1)
        self.assertEqual(edits[0][0], 55)
        self.assertIn("Last verified at `bbbbbbbb`.", edits[0][1])

    def test_no_rollup_is_not_a_failure(self):
        orig_find = verify.github_backend.find_marker_comment
        verify.github_backend.find_marker_comment = (
            lambda *a, **k: None)
        try:
            self.assertFalse(verify.update_rollup_currency(
                "github", None, None, "7", SHA_B, None,
                "api", "o/r", "TOK"))
        finally:
            verify.github_backend.find_marker_comment = orig_find

    def test_edit_failure_reports_failure(self):
        orig_find = verify.github_backend.find_marker_comment
        orig_update = verify.github_backend.update_issue_comment
        verify.github_backend.find_marker_comment = (
            lambda *a, **k: {"id": 55, "body": ROLLUP})

        def boom(*a, **k):
            raise RuntimeError("nope")

        verify.github_backend.update_issue_comment = boom
        try:
            self.assertTrue(verify.update_rollup_currency(
                "github", None, None, "7", SHA_B, None,
                "api", "o/r", "TOK"))
        finally:
            verify.github_backend.find_marker_comment = orig_find
            verify.github_backend.update_issue_comment = orig_update


if __name__ == "__main__":
    unittest.main()
