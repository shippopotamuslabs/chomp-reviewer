#!/usr/bin/env python3
"""Regression tests for .forge/verify.py conformance re-check (FRG-7b).

On re-push, conformance criteria that were "not evident" or
"contradicted" are re-evaluated with the same final-state evidence the
finding loop uses. Criteria live in the rollup comment (not resolvable
threads), so the outcome is posted as ONE summary comment and addressed
criteria are marked resolved in text.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import os
import unittest
from unittest import mock


def _load_verify():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".forge", "verify.py")
    spec = importlib.util.spec_from_file_location("verify_conformance", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


verify = _load_verify()

ROLLUP = """<!-- forge:consolidated-review sha:abc123 -->
## PR Reviewer Guide LOREM

summary here

## Ticket conformance

Resolved ticket(s): FRG-7 "Verify loop" (http://example.com/FRG-7)

Overall: partial

- [addressed] The pipeline blocks merge on failure (evidence: src/forge/pipe.py[10-20])
- [not evident] The loop re-checks criteria on re-push (evidence: src/forge/loop.py[1-9])
- [contradicted] All criteria report success (evidence: src/forge/report.py[3-7])

Unrelated changes:

- misc cleanup
"""

DIFF = """diff --git a/src/forge/loop.py b/src/forge/loop.py
@@ -1,0 +1,10 @@
+def recheck(criteria):
+    return [c for c in criteria]
diff --git a/src/forge/report.py b/src/forge/report.py
@@ -1,0 +1,5 @@
+def report(results):
+    return results
diff --git a/other/file.py b/other/file.py
@@ -1,0 +1,5 @@
+unrelated change
"""


class ParseConformanceCriteriaTest(unittest.TestCase):
    def test_parses_overall_and_all_statuses(self):
        overall, criteria = verify.parse_conformance_criteria(ROLLUP)
        self.assertEqual(overall, "partial")
        self.assertEqual(len(criteria), 3)
        by_status = {c["status"]: c for c in criteria}
        self.assertIn("addressed", by_status)
        self.assertIn("not evident", by_status)
        self.assertIn("contradicted", by_status)
        self.assertEqual(
            by_status["not evident"]["criterion"],
            "The loop re-checks criteria on re-push",
        )
        self.assertEqual(
            by_status["not evident"]["evidence"], "src/forge/loop.py[1-9]")
        # The "Unrelated changes" bullets carry no [status] and are ignored.
        self.assertNotIn("misc cleanup",
                         [c["criterion"] for c in criteria])

    def test_no_section_returns_none(self):
        overall, criteria = verify.parse_conformance_criteria(
            "just a review without the section")
        self.assertIsNone(overall)
        self.assertEqual(criteria, [])

    def test_empty_body_returns_none(self):
        self.assertEqual(verify.parse_conformance_criteria(""), (None, []))

    def test_only_open_statuses_selected(self):
        _, criteria = verify.parse_conformance_criteria(ROLLUP)
        pending = verify.open_conformance_criteria(criteria)
        self.assertEqual(
            sorted(c["status"] for c in pending),
            ["contradicted", "not evident"],
        )


class BuildConformancePromptTest(unittest.TestCase):
    def test_prompt_includes_final_state_evidence(self):
        # Evidence is built exactly like the finding loop: scoped diff +
        # final-state block at the new sha.
        text = ("The loop re-checks criteria on re-push "
                "(evidence: src/forge/loop.py[1-9])")
        evidence = verify.select_diff_for_finding(DIFF, text)
        self.assertIn("recheck", evidence)
        self.assertNotIn("unrelated change", evidence)

        def fake_fetch(api_url, project, path, ref, token):
            return ("present", "def recheck(criteria):\n    return 'ground-truth'")

        with mock.patch.object(verify, "fetch_file_at_ref",
                               side_effect=fake_fetch):
            final_state = verify.build_final_state_block(
                "https://example.com/api", 1,
                verify.extract_focus_files(text), "newsha9", "tok")
        self.assertIn("ground-truth", final_state)
        prompt = verify.build_conformance_prompt(
            text, evidence + "\n\n" + final_state)
        self.assertIn("The loop re-checks criteria on re-push", prompt)
        self.assertIn("ground-truth", prompt)
        self.assertIn("Final file state at newsha9", prompt)


class ParseVerdictResponseTest(unittest.TestCase):
    def test_plain_json(self):
        verdict, reason = verify.parse_verdict_response(
            '{"verdict": "ADDRESSED", "reason": "fix present"}')
        self.assertEqual((verdict, reason), ("ADDRESSED", "fix present"))

    def test_fenced_json(self):
        verdict, _ = verify.parse_verdict_response(
            '```json\n{"verdict": "NOT_ADDRESSED", "reason": "still missing"}\n```')
        self.assertEqual(verdict, "NOT_ADDRESSED")

    def test_invalid_json_is_not_addressed(self):
        verdict, reason = verify.parse_verdict_response("not json at all")
        self.assertEqual(verdict, "NOT_ADDRESSED")
        self.assertTrue(reason)

    def test_unknown_verdict_is_not_addressed(self):
        verdict, _ = verify.parse_verdict_response(
            '{"verdict": "MAYBE", "reason": "unsure"}')
        self.assertEqual(verdict, "NOT_ADDRESSED")


def _recheck_kwargs(**overrides):
    kwargs = dict(
        platform="gitlab",
        api_url="https://example.com/api",
        project_id="1",
        iid="133",
        sha="newsha9000",
        diff=DIFF,
        token="tok",
        gh_api="https://api.github.com",
        gh_repo="o/r",
        gh_token="ghtok",
        api_key="key",
        model="model",
        dry_run=False,
    )
    kwargs.update(overrides)
    return kwargs


class VerifyConformanceRecheckTest(unittest.TestCase):
    def _gitlab_notes(self, *args, **kwargs):
        # GET call from get_rollup_body_gitlab; POST handled separately.
        if args[0] == "GET":
            return [{"body": ROLLUP}]
        self.posted.append(kwargs.get("data") or args)
        return {}

    def test_addressed_and_open_classified_in_one_post(self):
        self.posted = []
        llm = [
            '{"verdict": "ADDRESSED", "reason": "loop now re-checks"}',
            '{"verdict": "NOT_ADDRESSED", "reason": "report still wrong"}',
        ]
        with mock.patch.object(verify, "gitlab_request",
                               side_effect=self._gitlab_notes), \
             mock.patch.object(verify, "fetch_file_at_ref",
                               return_value=("unknown", "")), \
             mock.patch.object(verify, "call_openrouter",
                               side_effect=llm):
            failed = verify.verify_conformance_recheck(
                **_recheck_kwargs())
        self.assertFalse(failed)
        # Exactly ONE summary post for both criteria.
        self.assertEqual(len(self.posted), 1)
        body = self.posted[0]["body"]
        self.assertIn("Forge conformance re-check at `newsha90`", body)
        self.assertIn("RESOLVED [not evident]", body)
        self.assertIn("STILL OPEN [contradicted]", body)
        # The already-addressed criterion is not re-evaluated.
        self.assertNotIn("blocks merge on failure", body)

    def test_no_section_is_noop_without_post(self):
        with mock.patch.object(verify, "gitlab_request",
                               return_value=[{"body": "no conformance here"}]), \
             mock.patch.object(verify, "call_openrouter") as llm:
            failed = verify.verify_conformance_recheck(
                **_recheck_kwargs())
        self.assertFalse(failed)
        llm.assert_not_called()

    def test_no_linked_ticket_is_noop(self):
        rollup = ROLLUP.replace("Overall: partial", "Overall: no linked ticket")
        with mock.patch.object(verify, "gitlab_request",
                               return_value=[{"body": rollup}]), \
             mock.patch.object(verify, "call_openrouter") as llm:
            failed = verify.verify_conformance_recheck(
                **_recheck_kwargs())
        self.assertFalse(failed)
        llm.assert_not_called()

    def test_all_addressed_is_noop(self):
        rollup = (ROLLUP.replace("[not evident]", "[addressed]")
                  .replace("[contradicted]", "[addressed]"))
        with mock.patch.object(verify, "gitlab_request",
                               return_value=[{"body": rollup}]), \
             mock.patch.object(verify, "call_openrouter") as llm:
            failed = verify.verify_conformance_recheck(
                **_recheck_kwargs())
        self.assertFalse(failed)
        llm.assert_not_called()

    def test_dry_run_reports_without_posting(self):
        posted = []
        with mock.patch.object(verify, "gitlab_request") as gl:
            def fake_gitlab(method, *args, **kwargs):
                if method == "GET":
                    return [{"body": ROLLUP}]
                posted.append(kwargs.get("data"))
                return {}
            gl.side_effect = fake_gitlab
            with mock.patch.object(verify, "fetch_file_at_ref",
                                   return_value=("unknown", "")), \
                 mock.patch.object(verify, "call_openrouter",
                                   return_value='{"verdict": "ADDRESSED", "reason": "ok"}'):
                failed = verify.verify_conformance_recheck(
                    **_recheck_kwargs(dry_run=True))
        self.assertFalse(failed)
        self.assertEqual(posted, [])

    def test_post_failure_returns_true(self):
        def fake_gitlab(method, *args, **kwargs):
            if method == "GET":
                return [{"body": ROLLUP}]
            raise RuntimeError("403 Forbidden")
        with mock.patch.object(verify, "gitlab_request",
                               side_effect=fake_gitlab), \
             mock.patch.object(verify, "fetch_file_at_ref",
                               return_value=("unknown", "")), \
             mock.patch.object(verify, "call_openrouter",
                               return_value='{"verdict": "ADDRESSED", "reason": "ok"}'):
            failed = verify.verify_conformance_recheck(
                **_recheck_kwargs())
        self.assertTrue(failed)

    def test_llm_failure_skips_criterion_fail_soft(self):
        posted = []

        def fake_gitlab(method, *args, **kwargs):
            if method == "GET":
                return [{"body": ROLLUP}]
            posted.append(kwargs.get("data"))
            return {}

        llm = [RuntimeError("boom"),
               '{"verdict": "NOT_ADDRESSED", "reason": "still open"}']
        with mock.patch.object(verify, "gitlab_request",
                               side_effect=fake_gitlab), \
             mock.patch.object(verify, "fetch_file_at_ref",
                               return_value=("unknown", "")), \
             mock.patch.object(verify, "call_openrouter",
                               side_effect=llm):
            failed = verify.verify_conformance_recheck(
                **_recheck_kwargs())
        self.assertFalse(failed)
        self.assertEqual(len(posted), 1)
        # Only the successfully evaluated criterion is reported.
        self.assertNotIn("re-checks criteria", posted[0]["body"])
        self.assertIn("STILL OPEN [contradicted]", posted[0]["body"])

    def test_github_posts_one_issue_comment(self):
        posted = []
        with mock.patch.object(verify.github_backend,
                               "find_marker_comment",
                               return_value={"body": ROLLUP}), \
             mock.patch.object(verify.github_backend,
                               "fetch_file_at_ref",
                               return_value=("unknown", "")), \
             mock.patch.object(verify.github_backend,
                               "github_request",
                               side_effect=lambda *a, **k: posted.append(
                                   k.get("data")) or {}), \
             mock.patch.object(verify, "call_openrouter",
                               return_value='{"verdict": "ADDRESSED", "reason": "ok"}'):
            failed = verify.verify_conformance_recheck(
                **_recheck_kwargs(platform="github"))
        self.assertFalse(failed)
        self.assertEqual(len(posted), 1)
        self.assertIn("Forge conformance re-check", posted[0]["body"])


if __name__ == "__main__":
    unittest.main()
