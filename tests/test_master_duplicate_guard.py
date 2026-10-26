#!/usr/bin/env python3
"""Duplicate-review guard tests for master.py (FRG-11).

Concurrent runs of the reusable workflow can both pass the marker check
before either posts, so master re-checks the marker immediately before
posting on GitHub AND GitLab and raises DuplicateReviewPost instead of
posting a second consolidated review. main() turns that into a
no-op recorded run (duplicate_suppressed) with exit 0.

All API access is stubbed; no network. Follows the fake/stub style of
tests/test_github_backend.py (PostReviewGithubTest): patch attributes
directly and restore every patched attribute in finally blocks.
"""
import importlib.util
import os
import unittest

HERE = os.path.dirname(os.path.abspath(__file__))


def _load(name):
    path = os.path.join(HERE, "..", ".forge", "%s.py" % name)
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


master = _load("master")

API = "https://api.github.com"
REPO = "shippopotamuslabs/stratum"

DIFF = """diff --git a/src/app.py b/src/app.py
--- a/src/app.py
+++ b/src/app.py
@@ -1,3 +1,3 @@
 line1
-old2
+new2
 line3
"""

FINDINGS = [{
    "file": "src/app.py", "line_start": 2, "line_end": 2,
    "severity": "High", "category": "Bug", "title": "Bad line",
    "description": "D", "evidence": "E",
    "fix_focus_areas": ["src/app.py[2-2]"],
    "recommended_fix": "R", "sources": ["thermos-a"],
}]


class DuplicateReviewPostTest(unittest.TestCase):
    def test_exception_carries_existing_id(self):
        err = master.DuplicateReviewPost(123)
        self.assertEqual(err.existing_id, 123)


class PostReviewGithubGuardTest(unittest.TestCase):
    def test_marker_present_raises_and_posts_nothing(self):
        orig_find = master.github_backend.find_marker_comment
        orig_issue = master.github_backend.post_issue_comment
        orig_attempt = master.github_backend.post_review_comment_attempt
        calls = []

        def fake_find(api_url, repo, number, token, marker):
            self.assertEqual(marker, "forge:consolidated-review")
            return {"id": 999}

        def fail_issue(*a, **k):
            calls.append("issue")
            raise AssertionError("rollup POST must not happen")

        def fail_attempt(*a, **k):
            calls.append("attempt")
            raise AssertionError("inline POST must not happen")

        master.github_backend.find_marker_comment = fake_find
        master.github_backend.post_issue_comment = fail_issue
        master.github_backend.post_review_comment_attempt = fail_attempt
        try:
            with self.assertRaises(master.DuplicateReviewPost) as ctx:
                master.post_review_github(
                    API, REPO, "7", "TOK", "abc123", "Summary.",
                    FINDINGS, DIFF)
        finally:
            master.github_backend.find_marker_comment = orig_find
            master.github_backend.post_issue_comment = orig_issue
            master.github_backend.post_review_comment_attempt = orig_attempt
        self.assertEqual(ctx.exception.existing_id, 999)
        self.assertEqual(calls, [])

    def test_marker_absent_posts_rollup_and_inline(self):
        orig_find = master.github_backend.find_marker_comment
        orig_issue = master.github_backend.post_issue_comment
        orig_attempt = master.github_backend.post_review_comment_attempt
        posted = {}

        def fake_find(api_url, repo, number, token, marker):
            return None

        def fake_issue(api_url, repo, number, token, body):
            posted["rollup"] = body
            return {"id": 555}

        def fake_attempt(api_url, repo, number, token, body, commit_id,
                         attempt):
            posted["attempt"] = attempt
            return {"id": 777}

        master.github_backend.find_marker_comment = fake_find
        master.github_backend.post_issue_comment = fake_issue
        master.github_backend.post_review_comment_attempt = fake_attempt
        try:
            recorded, failed = master.post_review_github(
                API, REPO, "7", "TOK", "abc123", "Summary.",
                FINDINGS, DIFF)
        finally:
            master.github_backend.find_marker_comment = orig_find
            master.github_backend.post_issue_comment = orig_issue
            master.github_backend.post_review_comment_attempt = orig_attempt
        self.assertEqual(failed, [])
        self.assertIn("forge:consolidated-review", posted["rollup"])
        self.assertEqual(recorded[0]["id"], 777)


class PostReviewGitlabGuardTest(unittest.TestCase):
    def _stub_gitlab(self, notes):
        posts = []
        orig = master.gitlab_request

        def fake(method, url, token, data=None):
            if method == "GET" and url.endswith("/projects/1"):
                return {"path_with_namespace": "o/r"}
            if method == "GET" and "/merge_requests/2" in url \
                    and "/notes" not in url:
                return {"diff_refs": {"base_sha": "b", "head_sha": "h",
                                      "start_sha": "s"}}
            if method == "GET" and url.endswith("/notes?per_page=100"):
                return notes
            if method == "POST" and url.endswith("/notes"):
                posts.append(data)
                return {"id": 111}
            if method == "POST" and "/discussions" in url:
                return {"id": 222}
            raise AssertionError("unexpected gitlab_request %s %s"
                                 % (method, url))

        master.gitlab_request = fake
        return orig, posts

    def test_marker_present_raises_and_posts_nothing(self):
        orig, posts = self._stub_gitlab(
            [{"id": 999, "body": "<!-- forge:consolidated-review -->"}])
        try:
            with self.assertRaises(master.DuplicateReviewPost) as ctx:
                master.post_review_gitlab(
                    "http://g/api/v4", "1", "2", "TOK", "sha9",
                    "Summary.", FINDINGS, DIFF)
        finally:
            master.gitlab_request = orig
        self.assertEqual(ctx.exception.existing_id, 999)
        self.assertEqual(posts, [])

    def test_marker_absent_posts_rollup(self):
        orig, posts = self._stub_gitlab([{"id": 1, "body": "plain"}])
        try:
            recorded, failed = master.post_review_gitlab(
                "http://g/api/v4", "1", "2", "TOK", "sha9",
                "Summary.", [], "")
        finally:
            master.gitlab_request = orig
        self.assertEqual(len(posts), 1)
        self.assertIn("forge:consolidated-review", posts[0]["body"])
        self.assertEqual(failed, [])


class FindMarkerNoteGitlabTest(unittest.TestCase):
    def test_returns_matching_note(self):
        orig = master.gitlab_request
        seen = {}

        def fake(method, url, token, data=None):
            seen["url"] = url
            return [{"id": 1, "body": "plain"},
                    {"id": 42, "body": "x forge:consolidated-review y"}]

        master.gitlab_request = fake
        try:
            found = master.find_marker_note_gitlab(
                "http://g/api/v4", "1", "2", "TOK",
                "forge:consolidated-review")
        finally:
            master.gitlab_request = orig
        self.assertEqual(found["id"], 42)
        self.assertIn("/notes?per_page=100", seen["url"])

    def test_returns_none_when_absent(self):
        orig = master.gitlab_request
        master.gitlab_request = lambda *a, **k: [{"id": 1, "body": "plain"}]
        try:
            found = master.find_marker_note_gitlab(
                "http://g/api/v4", "1", "2", "TOK",
                "forge:consolidated-review")
        finally:
            master.gitlab_request = orig
        self.assertIsNone(found)


if __name__ == "__main__":
    unittest.main()
