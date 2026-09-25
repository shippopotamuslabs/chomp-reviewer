#!/usr/bin/env python3
"""Regression tests for .forge/verify.py final-state grounding.

Bug: verification evidence is the NET MR diff vs target, so a fix that
removes lines the same MR added shows no removal hunk. The LLM then
verdicts NOT_ADDRESSED with "shows no removal" for a fix that is present
(Stratum MR169 M1: microIncrements was removed from the domain proposal
in the fix commit, but the MR had added it two commits earlier, so the net
diff showed only the new sidecar type and the field's absence was
undecidable from the diff).

Fix: ground each verdict in the verified commit's actual file state —
fetch cited files at the verified sha and present them as ground truth
for whether cited code still exists, with deleted/unknown distinguished.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import io
import os
import unittest
import urllib.error
from unittest import mock


def _load_verify():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".forge", "verify.py")
    spec = importlib.util.spec_from_file_location("verify", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


verify = _load_verify()

PROPOSAL = "core/domain/src/main/kotlin/com/stratum/domain/lift/EquipmentIncrementProposal.kt"


def _resp(text):
    m = mock.MagicMock()
    m.read.return_value = text.encode()
    m.__enter__.return_value = m
    return m


def _http_404():
    return urllib.error.HTTPError("http://x", 404, "Not Found", {}, None)


def _http_500():
    return urllib.error.HTTPError("http://x", 500, "Server Error", {}, None)


class FetchFileAtRefTest(unittest.TestCase):
    def test_present_returns_content(self):
        with mock.patch.object(
            verify.urllib.request, "urlopen", return_value=_resp("data class X")
        ):
            self.assertEqual(
                verify.fetch_file_at_ref("https://example.com/api", 1, PROPOSAL, "abc", "tok"),
                ("present", "data class X"),
            )

    def test_404_returns_absent(self):
        with mock.patch.object(
            verify.urllib.request, "urlopen", side_effect=_http_404()
        ):
            self.assertEqual(
                verify.fetch_file_at_ref("https://example.com/api", 1, PROPOSAL, "abc", "tok"),
                ("absent", ""),
            )

    def test_server_error_returns_unknown_not_absent(self):
        with mock.patch.object(
            verify.urllib.request, "urlopen", side_effect=_http_500()
        ):
            self.assertEqual(
                verify.fetch_file_at_ref("https://example.com/api", 1, PROPOSAL, "abc", "tok"),
                ("unknown", ""),
            )


class BuildFinalStateBlockTest(unittest.TestCase):
    def _block(self, fetch_map):
        def fake_fetch(api_url, project_id, path, ref, token):
            return fetch_map[path]

        with mock.patch.object(verify, "fetch_file_at_ref", side_effect=fake_fetch):
            return verify.build_final_state_block(
                "https://example.com/api", 1, list(fetch_map), "abc123", "tok"
            )

    def test_empty_focus_gives_empty_block(self):
        with mock.patch.object(verify, "fetch_file_at_ref") as m:
            self.assertEqual(
                verify.build_final_state_block("https://example.com/api", 1, [], "abc", "tok"), ""
            )
            m.assert_not_called()

    def test_present_absent_unknown_marked_distinctly(self):
        block = self._block(
            {
                "a.kt": ("present", "class A"),
                "b.kt": ("absent", ""),
                "c.kt": ("unknown", ""),
            }
        )
        self.assertIn("### a.kt\nclass A", block)
        self.assertIn("### b.kt\n[file deleted at abc123]", block)
        self.assertIn("### c.kt\n[file state unavailable", block)

    def test_large_file_truncated_with_marker(self):
        block = self._block({"big.kt": ("present", "z" * 20000)})
        self.assertIn("NOT reviewed", block)
        self.assertLess(len(block), 20000)


class NetDiffBlindnessScenarioTest(unittest.TestCase):
    def test_removed_field_decidable_from_evidence(self):
        # Net diff: field added-then-removed nets to zero; only the sidecar
        # type shows. Final state must carry the ground-truth absence.
        net_diff = (
            f"diff --git a/{PROPOSAL} b/{PROPOSAL}\n"
            "@@ -1,0 +50,10 @@\n+data class EquipmentProposalAnalysis(\n"
        )
        final_content = "data class EquipmentIncrementProposal(\n    val id: String,\n)\n"
        body = f"Fix Focus Areas - {PROPOSAL}[36-45]"

        with mock.patch.object(
            verify, "fetch_file_at_ref", return_value=("present", final_content)
        ):
            evidence = verify.select_diff_for_finding(net_diff, body)
            evidence += "\n\n" + verify.build_final_state_block(
                "https://example.com/api", 1, verify.extract_focus_files(body), "abc", "tok"
            )

        self.assertNotIn("-    val microIncrements", evidence)
        self.assertIn("EquipmentProposalAnalysis", evidence)
        self.assertIn("Final file state at abc", evidence)
        state_section = evidence.split("Final file state")[1]
        self.assertNotIn("microIncrements", state_section)


if __name__ == "__main__":
    unittest.main()
