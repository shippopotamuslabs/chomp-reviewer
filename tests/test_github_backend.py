#!/usr/bin/env python3
"""Tests for the GitHub backend (.forge/github_backend.py) and the
GitHub paths in master.py / verify.py.

The backend is selected by environment (FORGE_PLATFORM / GITHUB_ACTIONS)
and must mirror the GitLab behavior: rollup as one marked comment, inline
comments anchored to valid diff lines, review-once marker detection, and
verify verdicts replied + resolved (GraphQL resolveReviewThread).

All API access is mocked at urllib.request.urlopen or by monkeypatching
the backend module, following the convention of the other test files.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import io
import json
import os
import unittest
import urllib.error
from unittest import mock


def _load(name):
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".forge", f"{name}.py")
    spec = importlib.util.spec_from_file_location(name, path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


backend = _load("github_backend")
master = _load("master")
verify = _load("verify")
openrouter_client = _load("openrouter_client")

API = "https://api.github.com"
REPO = "shippopotamuslabs/stratum"


def _resp(payload):
    m = mock.MagicMock()
    m.read.return_value = json.dumps(payload).encode()
    m.__enter__.return_value = m
    return m


def _capture_urlopen(test_case, payload):
    """Patch backend urlopen; return the list of captured Requests."""
    captured = []

    def fake_urlopen(req, timeout=None):
        captured.append(req)
        return _resp(payload)

    patcher = mock.patch.object(
        backend.urllib.request, "urlopen", side_effect=fake_urlopen)
    test_case.addCleanup(patcher.stop)
    patcher.start()
    return captured


class GithubRequestTest(unittest.TestCase):
    def test_bearer_auth_and_json_body(self):
        captured = _capture_urlopen(self, {"id": 1})
        backend.github_request("POST", f"{API}/x", "TOK", data={"a": 1})
        req = captured[0]
        self.assertEqual(req.get_header("Authorization"), "Bearer TOK")
        self.assertEqual(req.method, "POST")
        self.assertEqual(json.loads(req.data.decode()), {"a": 1})

    def test_http_error_nonfatal_raises(self):
        err = urllib.error.HTTPError(
            "http://x", 422, "Unprocessable", {}, io.BytesIO(b"{}"))
        with mock.patch.object(backend.urllib.request, "urlopen",
                               side_effect=err):
            with self.assertRaises(RuntimeError):
                backend.github_request("POST", f"{API}/x", "TOK",
                                       data={}, fatal=False)


class ReviewCommentTest(unittest.TestCase):
    def test_right_side_payload(self):
        captured = _capture_urlopen(self, {"id": 9})
        backend.post_review_comment(
            API, REPO, "7", "TOK", "body", "sha123", "src/A.kt", 42, "RIGHT")
        req = captured[0]
        self.assertTrue(req.full_url.endswith("/pulls/7/comments"))
        payload = json.loads(req.data.decode())
        self.assertEqual(payload, {
            "body": "body", "commit_id": "sha123", "path": "src/A.kt",
            "line": 42, "side": "RIGHT"})

    def test_attempt_old_line_maps_to_left_old_path(self):
        captured = _capture_urlopen(self, {"id": 10})
        attempt = {"new_path": "new.txt", "old_path": "old.txt",
                   "new_line": None, "old_line": 3}
        backend.post_review_comment_attempt(
            API, REPO, "7", "TOK", "body", "sha123", attempt)
        payload = json.loads(captured[0].data.decode())
        self.assertEqual(payload["path"], "old.txt")
        self.assertEqual(payload["line"], 3)
        self.assertEqual(payload["side"], "LEFT")


class MarkerCommentTest(unittest.TestCase):
    def test_finds_marker_and_stops_paginating(self):
        pages = [
            [{"id": 1, "body": "hello"}],
            [],  # never reached: page 1 is short (<100) so paging stops
        ]
        calls = []

        def fake_request(method, url, token, data=None, fatal=True, accept=None):
            calls.append(url)
            return pages[len(calls) - 1]

        original = backend.github_request
        backend.github_request = fake_request
        try:
            found = backend.find_marker_comment(
                API, REPO, "7", "TOK", "forge:consolidated-review")
            self.assertIsNone(found)
            self.assertEqual(len(calls), 1)
        finally:
            backend.github_request = original

    def test_returns_marked_comment(self):
        marked = {"id": 5, "body": "<!-- forge:consolidated-review sha:x -->"}
        original = backend.github_request
        backend.github_request = lambda *a, **k: [{"id": 4, "body": "no"}, marked]
        try:
            found = backend.find_marker_comment(
                API, REPO, "7", "TOK", "forge:consolidated-review")
            self.assertEqual(found["id"], 5)
        finally:
            backend.github_request = original


class FetchPullDiffTest(unittest.TestCase):
    def test_diff_media_type_header(self):
        captured = []

        def fake_urlopen(req, timeout=None):
            captured.append(req)
            m = mock.MagicMock()
            m.read.return_value = b"diff --git a/x b/x\n"
            m.__enter__.return_value = m
            return m

        with mock.patch.object(backend.urllib.request, "urlopen",
                               side_effect=fake_urlopen):
            diff = backend.fetch_pull_diff(API, REPO, "7", "TOK")
        self.assertIn("diff --git", diff)
        self.assertEqual(captured[0].get_header("Accept"),
                         "application/vnd.github.diff")


def _http_error(code):
    return urllib.error.HTTPError(
        "http://x", code, "err", {}, io.BytesIO(b"too large"))


class AssemblePullDiffTest(unittest.TestCase):
    def test_modified_file_patch_verbatim(self):
        # Patch ends in a blank context line (' '): preserved, like the
        # GitLab assembly — dropping it leaves the hunk one line short.
        patch = "@@ -1,3 +1,3 @@\n line1\n-old\n+new\n "
        files = [{"filename": "src/a.py", "status": "modified",
                  "changes": 2, "patch": patch}]
        doc = backend.assemble_pull_diff(files)
        self.assertEqual(
            doc,
            "diff --git a/src/a.py b/src/a.py\n"
            "--- a/src/a.py\n+++ b/src/a.py\n" + patch + "\n")

    def test_added_and_removed_use_dev_null(self):
        # The mode lines are load-bearing, not decoration: unidiff
        # (pr-agent's parser) only accepts '+++ /dev/null' as a
        # deletion when 'deleted file mode' marked the target /dev/null
        # — without it the whole mr.diff fails to parse (observed live
        # on stratum PR #46, whose split deletes files outright).
        files = [
            {"filename": "new.py", "status": "added", "changes": 1,
             "patch": "@@ -0,0 +1 @@\n+x"},
            {"filename": "gone.py", "status": "removed", "changes": 1,
             "patch": "@@ -1 +0,0 @@\n-x"},
        ]
        doc = backend.assemble_pull_diff(files)
        self.assertIn(
            "diff --git a/new.py b/new.py\nnew file mode 100644\n"
            "--- /dev/null\n+++ b/new.py\n",
            doc)
        self.assertIn(
            "diff --git a/gone.py b/gone.py\ndeleted file mode 100644\n"
            "--- a/gone.py\n+++ /dev/null\n", doc)

    def test_rename_with_patch_has_rename_headers(self):
        files = [{"filename": "src/new.py", "previous_filename": "src/old.py",
                  "status": "renamed", "changes": 2,
                  "patch": "@@ -1 +1 @@\n-a\n+b"}]
        doc = backend.assemble_pull_diff(files)
        self.assertEqual(
            doc,
            "diff --git a/src/old.py b/src/new.py\n"
            "rename from src/old.py\nrename to src/new.py\n"
            "--- a/src/old.py\n+++ b/src/new.py\n@@ -1 +1 @@\n-a\n+b\n")

    def test_pure_rename_without_patch_is_headers_only(self):
        files = [{"filename": "src/new.py", "previous_filename": "src/old.py",
                  "status": "renamed", "changes": 0}]
        doc = backend.assemble_pull_diff(files)
        self.assertEqual(
            doc,
            "diff --git a/src/old.py b/src/new.py\n"
            "rename from src/old.py\nrename to src/new.py\n")
        self.assertNotIn(backend.PATCH_UNAVAILABLE_MARKER, doc)

    def test_missing_patch_gets_marker_never_dropped(self):
        # Binary file, and a text file too large for the API to patch:
        # both arrive patchless with changes > 0. The section must stay
        # in the document with the explicit marker.
        files = [
            {"filename": "img/logo.png", "status": "modified", "changes": 4},
            {"filename": "gen/huge.kt", "status": "modified", "changes": 900},
        ]
        doc = backend.assemble_pull_diff(files)
        self.assertEqual(doc.count("diff --git "), 2)
        self.assertEqual(doc.count(backend.PATCH_UNAVAILABLE_MARKER), 2)
        self.assertIn(
            "diff --git a/img/logo.png b/img/logo.png\n"
            "--- a/img/logo.png\n+++ b/img/logo.png\n", doc)

    def test_no_files_assembles_to_empty(self):
        self.assertEqual(backend.assemble_pull_diff([]), "")


class FetchPullDiffFallbackTest(unittest.TestCase):
    """On HTTP 406 the media-type fetch falls back to the paginated
    files API; the assembled document must feed the same parsers the
    served diff feeds (master's line map, verify's section splitter)."""

    PAGE2 = [
        {"filename": "src/new.py", "previous_filename": "src/old.py",
         "status": "renamed", "changes": 2, "patch": "@@ -1 +1 @@\n-a\n+b"},
        {"filename": "img/logo.png", "status": "modified", "changes": 4},
    ]

    def _fake_urlopen(self, pages, captured):
        def fake_urlopen(req, timeout=None):
            captured.append(req)
            if "/files?" in req.full_url:
                page = int(req.full_url.rsplit("page=", 1)[1])
                return _resp(pages[page - 1])
            raise _http_error(406)
        return fake_urlopen

    def test_406_assembles_every_page(self):
        page1 = [{"filename": f"src/f{i}.py", "status": "modified",
                  "changes": 2, "patch": "@@ -1 +1 @@\n-x\n+y"}
                 for i in range(100)]
        captured = []
        with mock.patch.object(
                backend.urllib.request, "urlopen",
                side_effect=self._fake_urlopen([page1, self.PAGE2],
                                               captured)):
            diff = backend.fetch_pull_diff(API, REPO, "7", "TOK")
        self.assertEqual(diff.count("diff --git "), 102)
        file_calls = [r for r in captured if "/files?" in r.full_url]
        self.assertEqual(len(file_calls), 2)  # stopped after short page 2
        self.assertIn("rename from src/old.py", diff)
        self.assertEqual(diff.count(backend.PATCH_UNAVAILABLE_MARKER), 1)

    def test_non_406_error_is_still_fatal(self):
        def fake_urlopen(req, timeout=None):
            raise _http_error(500)
        with mock.patch.object(backend.urllib.request, "urlopen",
                               side_effect=fake_urlopen):
            with self.assertRaises(SystemExit):
                backend.fetch_pull_diff(API, REPO, "7", "TOK")

    def test_assembled_diff_parses_like_a_served_diff(self):
        files = [
            {"filename": "src/app.py", "status": "modified", "changes": 2,
             "patch": "@@ -1,3 +1,3 @@\n line1\n-old2\n+new2\n line3"},
        ] + self.PAGE2
        doc = backend.assemble_pull_diff(files)
        line_map, order = master.parse_diff_line_map(doc)
        self.assertEqual(
            order, ["src/app.py", "src/new.py", "img/logo.png"])
        self.assertEqual(
            line_map["files"]["src/app.py"]["new_lines"], {1, 2, 3})
        self.assertEqual(
            line_map["aliases"], {"src/old.py": "src/new.py"})
        sections = verify.split_diff_sections(doc)
        self.assertEqual([p for p, _ in sections], order)
        self.assertIn(backend.PATCH_UNAVAILABLE_MARKER, sections[2][1])


class FetchFileAtRefGithubTest(unittest.TestCase):
    def test_present_absent_unknown(self):
        with mock.patch.object(backend.urllib.request, "urlopen",
                               return_value=_resp_raw("content")):
            self.assertEqual(
                backend.fetch_file_at_ref(API, REPO, "a/b.kt", "sha", "TOK"),
                ("present", "content"))
        err404 = urllib.error.HTTPError("http://x", 404, "NF", {}, None)
        with mock.patch.object(backend.urllib.request, "urlopen",
                               side_effect=err404):
            self.assertEqual(
                backend.fetch_file_at_ref(API, REPO, "a/b.kt", "sha", "TOK"),
                ("absent", ""))
        err500 = urllib.error.HTTPError("http://x", 500, "SE", {}, None)
        with mock.patch.object(backend.urllib.request, "urlopen",
                               side_effect=err500):
            status, _ = backend.fetch_file_at_ref(
                API, REPO, "a/b.kt", "sha", "TOK")
            self.assertEqual(status, "unknown")


def _resp_raw(text):
    m = mock.MagicMock()
    m.read.return_value = text.encode()
    m.__enter__.return_value = m
    return m


class ForgeReviewThreadsTest(unittest.TestCase):
    def _thread(self, tid, resolved, body, dbid=100):
        return {
            "id": tid,
            "isResolved": resolved,
            "comments": {"nodes": [
                {"databaseId": dbid, "body": body, "path": "a.kt",
                 "line": 1, "originalLine": 1}]},
        }

    def test_selects_open_marked_threads(self):
        payload = {
            "repository": {"pullRequest": {"reviewThreads": {
                "pageInfo": {"hasNextPage": False, "endCursor": None},
                "nodes": [
                    self._thread("T1", False,
                                 "<!-- forge:consolidated-review -->", 11),
                    self._thread("T2", True,
                                 "<!-- forge:consolidated-review -->", 12),
                    self._thread("T3", False, "human comment", 13),
                ],
            }}},
        }
        original = backend.github_graphql
        backend.github_graphql = lambda *a, **k: payload
        try:
            found = backend.get_forge_review_threads(API, REPO, "7", "TOK")
        finally:
            backend.github_graphql = original
        self.assertEqual(len(found), 1)
        self.assertEqual(found[0]["id"], "T1")
        self.assertEqual(found[0]["comment_id"], 11)

    def test_resolve_mutation_variables(self):
        captured = {}

        def fake_graphql(api_url, token, query, variables=None, fatal=False):
            captured["query"] = query
            captured["variables"] = variables
            return {"resolveReviewThread": {"thread": {"id": "T1"}}}

        original = backend.github_graphql
        backend.github_graphql = fake_graphql
        try:
            backend.resolve_review_thread(API, "TOK", "T1")
        finally:
            backend.github_graphql = original
        self.assertIn("resolveReviewThread", captured["query"])
        self.assertEqual(captured["variables"], {"threadId": "T1"})


class GetModelTest(unittest.TestCase):
    def test_empty_env_falls_back_to_default(self):
        # The GitHub workflow passes FORGE_MODEL straight from an
        # optional input; unset means set-but-empty in the step env.
        with mock.patch.dict(os.environ, {"FORGE_MODEL": ""}, clear=True):
            self.assertEqual(openrouter_client.get_model(),
                             "meta/muse-spark-1.3-contributor")

    def test_prefix_stripped(self):
        with mock.patch.dict(os.environ,
                             {"FORGE_MODEL": "openrouter/acme/model"},
                             clear=True):
            self.assertEqual(openrouter_client.get_model(), "acme/model")


class DetectPlatformTest(unittest.TestCase):
    def test_env_selection(self):
        with mock.patch.dict(os.environ, {}, clear=True):
            self.assertEqual(master.detect_platform(), "gitlab")
            self.assertEqual(verify.detect_platform(), "gitlab")
        with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true"},
                             clear=True):
            self.assertEqual(master.detect_platform(), "github")
        with mock.patch.dict(os.environ, {"GITHUB_ACTIONS": "true",
                                          "FORGE_PLATFORM": "gitlab"},
                             clear=True):
            self.assertEqual(master.detect_platform(), "gitlab")


DIFF = """diff --git a/src/app.py b/src/app.py
--- a/src/app.py
+++ b/src/app.py
@@ -1,3 +1,3 @@
 line1
-old2
+new2
 line3
"""


class PostReviewGithubTest(unittest.TestCase):
    def test_rollup_blob_links_and_inline_recording(self):
        """End-to-end master GitHub posting with a fake backend transport:
        rollup carries github.com blob links, the finding lands inline,
        and the recorded entry keeps the comment URL."""
        findings = [{
            "file": "src/app.py", "line_start": 2, "line_end": 2,
            "severity": "High", "category": "Bug", "title": "Bad line",
            "description": "D", "evidence": "E",
            "fix_focus_areas": ["src/app.py[2-2]"],
            "recommended_fix": "R", "sources": ["thermos-a"],
        }]
        posted = {}

        def fake_post_issue(api_url, repo, number, token, body):
            posted["rollup"] = body
            return {"id": 555}

        def fake_attempt(api_url, repo, number, token, body, commit_id,
                         attempt):
            posted["attempt"] = attempt
            posted["commit_id"] = commit_id
            return {"id": 777,
                    "html_url": "https://github.com/x/pull/7#discussion_r777"}

        orig_issue = master.github_backend.post_issue_comment
        orig_attempt = master.github_backend.post_review_comment_attempt
        orig_find = master.github_backend.find_marker_comment
        master.github_backend.post_issue_comment = fake_post_issue
        master.github_backend.post_review_comment_attempt = fake_attempt
        master.github_backend.find_marker_comment = lambda *a, **k: None
        try:
            recorded, failed = master.post_review_github(
                API, REPO, "7", "TOK", "abc123", "Summary.", findings, DIFF)
        finally:
            master.github_backend.post_issue_comment = orig_issue
            master.github_backend.post_review_comment_attempt = orig_attempt
            master.github_backend.find_marker_comment = orig_find

        self.assertEqual(failed, [])
        self.assertIn(
            "https://github.com/shippopotamuslabs/stratum/blob/abc123/"
            "src/app.py#L2-L2", posted["rollup"])
        self.assertEqual(posted["commit_id"], "abc123")
        self.assertEqual(recorded[0]["id"], 777)
        self.assertEqual(recorded[0]["url"],
                         "https://github.com/x/pull/7#discussion_r777")
        self.assertEqual(recorded[0]["anchor"], "new")


class VerifyFinalStateFetcherTest(unittest.TestCase):
    def test_github_fetcher_override(self):
        def fake_fetch(api_url, project, path, ref, token):
            self.assertEqual(project, REPO)
            return "absent", ""

        block = verify.build_final_state_block(
            API, REPO, ["a/b.kt"], "sha9", "TOK", fetcher=fake_fetch)
        self.assertIn("[file deleted at sha9]", block)


if __name__ == "__main__":
    unittest.main()
