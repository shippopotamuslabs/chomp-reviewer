#!/usr/bin/env python3
"""Regression tests for .forge/verify.py discussion resolution.

Bug 1: verify used PUT .../merge_requests/:iid/discussions/:id/resolve,
which does not exist in the GitLab API, so every ADDRESSED verdict failed
with "404 Not Found" and no discussion was ever resolved.

Bug 2: get_forge_discussions selected the standalone rollup note
(posted via POST /notes, individual_note=true, not resolvable) for
resolution. PUT {"resolved": true} on it fails with "403 Forbidden",
and gitlab_request's sys.exit(1) aborted verification of the remaining
discussions.

Fix: PUT the discussion resource itself, skip individual/unresolvable
notes when selecting, and resolve with fatal=False so one failure logs
an ERROR without aborting the run.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import io
import os
import unittest
import urllib.error
import urllib.request


def _load_verify():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".forge", "verify.py")
    spec = importlib.util.spec_from_file_location("verify", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


verify = _load_verify()


class ResolveDiscussionTest(unittest.TestCase):
    def test_puts_discussion_resource_without_resolve_suffix(self):
        calls = []

        def fake_gitlab_request(method, url, token, data=None, form_data=None, fatal=True):
            calls.append({"method": method, "url": url,
                          "token": token, "data": data, "fatal": fatal})
            return {"resolved": True}

        original = verify.gitlab_request
        verify.gitlab_request = fake_gitlab_request
        try:
            verify.resolve_discussion(
                "https://gitlab.com/api/v4",
                "83700143",
                "146",
                "e76c87a1a24cf43fe5a4015f1b989070b5eaced3",
                "TOKEN",
            )
        finally:
            verify.gitlab_request = original

        self.assertEqual(len(calls), 1)
        call = calls[0]
        self.assertEqual(call["method"], "PUT")
        self.assertEqual(
            call["url"],
            "https://gitlab.com/api/v4/projects/83700143"
            "/merge_requests/146/discussions/"
            "e76c87a1a24cf43fe5a4015f1b989070b5eaced3",
        )
        self.assertNotIn("/resolve", call["url"])
        self.assertEqual(call["data"], {"resolved": True})
        self.assertEqual(call["token"], "TOKEN")
        # One bad thread must not abort the run via sys.exit.
        self.assertFalse(call["fatal"])


class SelectResolvableDiscussionsTest(unittest.TestCase):
    def test_skips_rollup_and_unresolvable_threads(self):
        payload = [
            # Standalone rollup note (POST /notes): not a thread.
            {"id": "rollup", "individual_note": True,
             "notes": [{"body": "<!-- forge:consolidated-review sha:x -->\nGuide",
                        "resolvable": False}]},
            # Non-resolvable thread even though it is not individual.
            {"id": "locked", "individual_note": False,
             "notes": [{"body": "<!-- forge:consolidated-review sha:x -->\nOld",
                        "resolvable": False, "resolved": False}]},
            # Open, resolvable Forge thread: the only one to verify.
            {"id": "open-inline", "individual_note": False,
             "notes": [{"body": "<!-- forge:consolidated-review sha:x -->\nFinding",
                        "type": "DiffNote", "resolvable": True,
                        "resolved": False}]},
            # Already resolved: skip.
            {"id": "done", "individual_note": False,
             "notes": [{"body": "<!-- forge:consolidated-review sha:x -->\nFixed",
                        "type": "DiffNote", "resolvable": True,
                        "resolved": True}]},
            # Not a Forge thread: skip.
            {"id": "human", "individual_note": False,
             "notes": [{"body": "looks good", "type": "DiffNote",
                        "resolvable": True, "resolved": False}]},
        ]

        original = verify.gitlab_request
        verify.gitlab_request = lambda *a, **k: payload
        try:
            found = verify.get_forge_discussions(
                "https://gitlab.com/api/v4", "83700143", "157", "TOKEN")
        finally:
            verify.gitlab_request = original

        self.assertEqual([d["id"] for d in found], ["open-inline"])


class NonFatalResolveTest(unittest.TestCase):
    def test_403_raises_instead_of_exiting(self):
        def fail_403(method, url, token, data=None, form_data=None, fatal=True):
            raise RuntimeError(
                f"ERROR: GitLab API {method} {url} failed: "
                '403 {"message":"403 Forbidden"}')

        original = verify.gitlab_request
        verify.gitlab_request = fail_403
        try:
            with self.assertRaises(RuntimeError):
                verify.resolve_discussion(
                    "https://gitlab.com/api/v4", "83700143", "157",
                    "7e28efcc2f91795c0cb1945a59c8290a098ed997", "TOKEN")
        finally:
            verify.gitlab_request = original

    def test_gitlab_request_fatal_false_raises_on_http_error(self):
        err = urllib.error.HTTPError(
            "https://gitlab.com/api/v4/x", 403, "Forbidden", {},
            io.BytesIO(b'{"message":"403 Forbidden"}'))
        original_urlopen = urllib.request.urlopen

        def raise_403(req, timeout=None):
            raise err

        urllib.request.urlopen = raise_403
        try:
            with self.assertRaises(RuntimeError) as cm:
                verify.gitlab_request(
                    "PUT", "https://gitlab.com/api/v4/x", "TOKEN",
                    data={"resolved": True}, fatal=False)
        finally:
            urllib.request.urlopen = original_urlopen
        self.assertIn("403", str(cm.exception))


if __name__ == "__main__":
    unittest.main()
