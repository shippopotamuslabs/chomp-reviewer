#!/usr/bin/env python3
"""Regression tests for .forge/fetch_diff.py mr.diff assembly.

A file diff ending in a blank context line (a single ' ' line) must survive
assembly byte-for-byte. Trimming trailing whitespace per file drops that
line, leaves the hunk one body line short, and makes pr-agent's unidiff
parse abort with 'Hunk diff line expected: diff --git ...'.

Run:  python3 -m unittest discover -s tests -v
"""
import importlib.util
import os
import shutil
import subprocess
import tempfile
import unittest

try:
    from unidiff import PatchSet
except ImportError:  # pragma: no cover
    PatchSet = None


def _load_fetch_diff():
    here = os.path.dirname(os.path.abspath(__file__))
    path = os.path.join(here, "..", ".forge", "fetch_diff.py")
    spec = importlib.util.spec_from_file_location("fetch_diff", path)
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


fetch_diff = _load_fetch_diff()


def _two_file_changes():
    """GitLab-API-style payload: first file's final hunk ends with ' '. """
    frag1 = (
        "--- a/foo.txt\n"
        "+++ b/foo.txt\n"
        "@@ -1,3 +1,3 @@\n"
        " line1\n"
        " line3\n"
        " \n"  # blank context line: last line of this file's diff
    )
    frag2 = (
        "--- a/bar.txt\n"
        "+++ b/bar.txt\n"
        "@@ -1,1 +1,1 @@\n"
        "-old\n"
        "+new\n"
    )
    return [
        {"old_path": "foo.txt", "new_path": "foo.txt", "diff": frag1},
        {"old_path": "bar.txt", "new_path": "bar.txt", "diff": frag2},
    ]


@unittest.skipUnless(PatchSet is not None, "unidiff not installed")
class AssembleMrDiffTest(unittest.TestCase):
    def test_trailing_blank_context_line_parses(self):
        """Synthetic diff with trailing ' ' line must parse with zero errors."""
        document = fetch_diff.assemble_mr_diff(_two_file_changes())

        # The blank context line survived assembly (byte check).
        self.assertIn(" line3\n \n", document)

        # The exact failure from the bug report must not occur.
        patch = PatchSet(document)
        self.assertEqual(len(patch), 2)
        self.assertEqual(
            [f.path for f in patch], ["foo.txt", "bar.txt"]
        )
        hunk = patch[0][0]
        self.assertEqual((hunk.source_start, hunk.source_length), (1, 3))
        self.assertEqual((hunk.target_start, hunk.target_length), (1, 3))
        self.assertEqual(hunk.added, 0)
        self.assertEqual(hunk.removed, 0)

    def test_skips_empty_fragments(self):
        changes = [
            {"old_path": "empty.txt", "new_path": "empty.txt", "diff": "  \n"},
            {"old_path": "gone.txt", "new_path": "gone.txt", "diff": ""},
        ] + _two_file_changes()[:1]
        document = fetch_diff.assemble_mr_diff(changes)
        patch = PatchSet(document)
        self.assertEqual([f.path for f in patch], ["foo.txt"])

    def test_no_diffs_returns_empty(self):
        self.assertEqual(fetch_diff.assemble_mr_diff([]), "")
        self.assertEqual(
            fetch_diff.assemble_mr_diff(
                [{"old_path": "x", "new_path": "x", "diff": "\n"}]
            ),
            "",
        )

    @unittest.skipUnless(shutil.which("git"), "git not available")
    def test_byte_identity_with_raw_git_diff(self):
        """Fragments pass through byte-for-byte; only headers are added."""
        tmp = tempfile.mkdtemp(prefix="forge-diff-fixture-")
        try:
            self._git(tmp, "init", "-q")
            self._git(tmp, "config", "user.email", "test@example.com")
            self._git(tmp, "config", "user.name", "test")
            with open(os.path.join(tmp, "notes.txt"), "w") as f:
                f.write("alpha\nbeta\n\n")
            self._git(tmp, "add", "notes.txt")
            self._git(tmp, "commit", "-qm", "base")
            with open(os.path.join(tmp, "notes.txt"), "w") as f:
                f.write("alpha\nBETA\n\n")
            raw = self._git(
                tmp, "diff", "--no-color", "--no-ext-diff"
            )

            # Fixture must actually exercise the bug: a trailing ' ' line.
            self.assertIn("\n \n", raw + "\n")

            # Simulate GitLab API fragments (no 'diff --git'/'index' lines).
            sections = [
                s for s in raw.split("diff --git ") if s.strip()
            ]
            self.assertEqual(len(sections), 1)
            lines = sections[0].split("\n")
            header_line = lines[0]  # "a/notes.txt b/notes.txt"
            a_path, b_path = header_line.split(" ")[:2]
            fragment = "\n".join(
                l for l in lines[1:] if not l.startswith("index ")
            )
            path = a_path[2:]
            self.assertTrue(fragment.endswith(" \n") or fragment.endswith(" "))

            document = fetch_diff.assemble_mr_diff(
                [{"old_path": path, "new_path": b_path[2:], "diff": fragment}]
            )

            # Byte-identity: section body equals the input fragment
            # modulo the single trailing-newline normalization.
            body = document.split("diff --git ", 1)[1].split("\n", 1)[1]
            self.assertEqual(body, fragment.strip("\n") + "\n")

            # And the assembled artifact parses cleanly.
            patch = PatchSet(document)
            self.assertEqual(len(patch), 1)
            self.assertEqual(patch[0].path, path)
        finally:
            shutil.rmtree(tmp, ignore_errors=True)

    @staticmethod
    def _git(workdir, *args):
        out = subprocess.run(
            ["git", "-C", workdir, *args],
            capture_output=True,
            text=True,
            check=True,
        )
        return out.stdout


if __name__ == "__main__":
    unittest.main()
