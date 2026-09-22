#!/usr/bin/env python3
"""Regression tests for .forge/verify.py discussion resolution.

Bug: verify used PUT .../merge_requests/:iid/discussions/:id/resolve,
which does not exist in the GitLab API, so every ADDRESSED verdict failed
with "404 Not Found" and no discussion was ever resolved.

Fix: PUT the discussion resource itself —
PUT .../merge_requests/:iid/discussions/:discussion_id
with {"resolved": True}.

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


class ResolveDiscussionTest(unittest.TestCase):
    def test_puts_discussion_resource_without_resolve_suffix(self):
        calls = []

        def fake_gitlab_request(method, url, token, data=None, form_data=None):
            calls.append({"method": method, "url": url,
                          "token": token, "data": data})
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


if __name__ == "__main__":
    unittest.main()
