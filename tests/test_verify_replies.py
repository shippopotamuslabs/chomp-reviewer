#!/usr/bin/env python3
"""Regression tests for .forge/verify.py verdict replies.

Bug: verify.py printed verdicts (ADDRESSED / NOT_ADDRESSED) to the job log
and resolved addressed discussions, but never posted the verdict back as a
reply on the original discussion — so reviewers saw nothing inline.

Fix: after each verdict, POST the verdict + reason as a note on that
discussion (POST .../discussions/:id/notes) for both ADDRESSED and
NOT_ADDRESSED, with per-item failures logged without aborting the run.

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


class BuildVerdictReplyTest(unittest.TestCase):
    def test_addressed_reply_carries_verdict_reason_and_sha(self):
        body = verify.build_verdict_reply(
            "ADDRESSED", "suspension-gap helper added", "abc1234def5678")
        self.assertIn("ADDRESSED", body)
        self.assertIn("suspension-gap helper added", body)
        self.assertIn("abc1234", body)

    def test_not_addressed_reply_carries_verdict_reason_and_sha(self):
        body = verify.build_verdict_reply(
            "NOT_ADDRESSED", "wall-clock seam still present", "abc1234def5678")
        self.assertIn("NOT_ADDRESSED", body)
        self.assertIn("wall-clock seam still present", body)
        self.assertIn("abc1234", body)


class PostDiscussionReplyTest(unittest.TestCase):
    def test_posts_to_discussion_notes_without_resolve_suffix(self):
        calls = []

        def fake_gitlab_request(method, url, token, data=None,
                                form_data=None, fatal=True):
            calls.append({"method": method, "url": url, "token": token,
                          "data": data, "fatal": fatal})
            return {"id": 1}

        original = verify.gitlab_request
        verify.gitlab_request = fake_gitlab_request
        try:
            verify.post_discussion_reply(
                "https://gitlab.com/api/v4",
                "83700143",
                "146",
                "e76c87a1a24cf43fe5a4015f1b989070b5eaced3",
                "TOKEN",
                "✅ Forge verification: ADDRESSED",
            )
        finally:
            verify.gitlab_request = original

        self.assertEqual(len(calls), 1)
        call = calls[0]
        self.assertEqual(call["method"], "POST")
        self.assertEqual(
            call["url"],
            "https://gitlab.com/api/v4/projects/83700143"
            "/merge_requests/146/discussions/"
            "e76c87a1a24cf43fe5a4015f1b989070b5eaced3/notes",
        )
        self.assertNotIn("/resolve", call["url"])
        self.assertEqual(call["data"], {"body": "✅ Forge verification: ADDRESSED"})
        self.assertEqual(call["token"], "TOKEN")
        # One bad thread must not abort the run via sys.exit.
        self.assertFalse(call["fatal"])


if __name__ == "__main__":
    unittest.main()
