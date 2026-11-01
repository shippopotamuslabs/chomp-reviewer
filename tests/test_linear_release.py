#!/usr/bin/env python3
"""Tests for CHOMP-57: the tag-triggered Linear release script.

Covers the pure key extraction, the git range logic against a scratch
repo (previous tag is the release boundary), and the flip selection in
main() with the Linear API stubbed out: only issues Linear currently
shows as Merged may flip, and a missing API key is a green no-op.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import os
import subprocess
import sys
import tempfile
import unittest
from unittest import mock


def _load_script():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".github", "scripts", "linear_release.py")
    spec = importlib.util.spec_from_file_location("linear_release", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


lr = _load_script()


def _git(cwd, *args):
    subprocess.run(["git", *args], cwd=cwd, check=True,
                   capture_output=True, text=True)


class ExtractKeysTest(unittest.TestCase):
    def test_subject_and_body_keys_deduped_in_order(self):
        text = ("fix(workflow): marker lookup fix (CHOMP-56) (#38)\n"
                "\nFixes CHOMP-56\nRefs CHOMP-12 and CHOMP-56 again\n")
        self.assertEqual(lr.extract_keys(text), ["CHOMP-56", "CHOMP-12"])

    def test_ignores_other_teams_and_lookalikes(self):
        text = "feat: thing (FRG-24), STR-228, CHOMP-7, CHOMP-ABC, CHOMP-"
        self.assertEqual(lr.extract_keys(text), ["CHOMP-7"])

    def test_no_keys(self):
        self.assertEqual(lr.extract_keys("chore: bump pin"), [])


class RangeTest(unittest.TestCase):
    def setUp(self):
        self.repo = tempfile.mkdtemp(prefix="linear-release-")
        _git(self.repo, "init", "-q", "-b", "main")
        _git(self.repo, "config", "user.email", "test@example.com")
        _git(self.repo, "config", "user.name", "Test")
        self._commit("feat(core): first (CHOMP-1)", tag="v1.0.0")
        self._commit("fix(core): second (CHOMP-2)\n\nFixes CHOMP-3")
        self._commit("chore: unkeyed")
        self._commit("feat(core): third (CHOMP-2)", tag="v1.1.0")
        self._cwd = os.getcwd()
        os.chdir(self.repo)

    def tearDown(self):
        os.chdir(self._cwd)

    def _commit(self, message, tag=None):
        with open(os.path.join(self.repo, "f.txt"), "a") as fh:
            fh.write(message + "\n")
        _git(self.repo, "add", "f.txt")
        _git(self.repo, "commit", "-q", "-m", message)
        if tag:
            _git(self.repo, "tag", tag)

    def test_previous_tag_is_the_boundary(self):
        self.assertEqual(lr.previous_tag("v1.1.0"), "v1.0.0")
        self.assertIsNone(lr.previous_tag("v1.0.0"))

    def test_shipped_keys_cover_only_the_new_range(self):
        self.assertEqual(lr.shipped_keys("v1.1.0", "v1.0.0"),
                         ["CHOMP-2", "CHOMP-3"])
        self.assertEqual(lr.shipped_keys("v1.0.0", None), ["CHOMP-1"])


class FlipSelectionTest(unittest.TestCase):
    def setUp(self):
        self.repo = tempfile.mkdtemp(prefix="linear-release-flip-")
        _git(self.repo, "init", "-q", "-b", "main")
        _git(self.repo, "config", "user.email", "test@example.com")
        _git(self.repo, "config", "user.name", "Test")
        with open(os.path.join(self.repo, "f.txt"), "w") as fh:
            fh.write("x\n")
        _git(self.repo, "add", "f.txt")
        _git(self.repo, "commit", "-q", "-m",
             "feat: ships (CHOMP-10) and (CHOMP-11)")
        _git(self.repo, "tag", "v1.0.0")
        self._cwd = os.getcwd()
        os.chdir(self.repo)
        self.flipped = []

    def tearDown(self):
        os.chdir(self._cwd)

    def _run_main(self, env):
        def fake_flip(api_key, issue_id, state_id):
            self.flipped.append(issue_id)
            return True

        with mock.patch.object(lr, "team_id", return_value="team-1"), \
             mock.patch.object(lr, "released_state_id", return_value="state-9"), \
             mock.patch.object(lr, "merged_issues", return_value=[
                 {"id": "uuid-10", "identifier": "CHOMP-10"},
             ]), \
             mock.patch.object(lr, "flip", side_effect=fake_flip), \
             mock.patch.dict(os.environ, env):
            return lr.main(["linear_release.py", "v1.0.0"])

    def test_only_merged_issues_flip(self):
        # CHOMP-11 is keyed in the range but not Merged in Linear.
        self.assertEqual(self._run_main({"LINEAR_API_KEY": "k"}), 0)
        self.assertEqual(self.flipped, ["uuid-10"])

    def test_missing_key_is_a_green_noop(self):
        self.assertEqual(self._run_main({"LINEAR_API_KEY": ""}), 0)
        self.assertEqual(self.flipped, [])


if __name__ == "__main__":
    unittest.main()
