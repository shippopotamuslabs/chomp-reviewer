#!/usr/bin/env python3
"""
Chomp size gate (CHOMP-61): refuse oversized PRs deterministically.

Runs in the reusable workflow after the diff is fetched and before any
producer runs. A PR whose changed lines (additions + deletions) exceed
the limit is not reviewable — by Chomp or by any reviewer — and the
2.0 synthesis died on exactly such a diff (stratum PR #96: 13,419
changed lines) with a context-compaction failure: the posting steps
were skipped, the PR got no review at all, and the red check was
indistinguishable from an infrastructure flake.

The gate instead measures mr.diff itself (the document the producers
would consume, already fetched by the workflow — no extra API calls),
posts the consolidated review carrying a single size verdict, and
exits non-zero so the chomp-review check fails with the refusal as
its visible reason. A PR at or under the limit exits 0 and the review
proceeds exactly as before.

Verify-mode runs never reach this script: the workflow step is gated
on no consolidated review existing yet, so re-pushes still verify.

Usage:
    python3 .forge/size_gate.py --diff mr.diff --max-lines 2000 \
        --iid <pr number> --sha <head sha>

Environment (GitHub): GITHUB_TOKEN, GITHUB_API_URL, GITHUB_REPOSITORY
(read via github_backend.gh_env()). When GITHUB_STEP_SUMMARY is set,
the verdict is appended to the job summary as well.

Exit codes: 0 = within the limit, review proceeds; 1 = over the
limit, verdict posted, check must fail; 2 = bad configuration or an
unreadable diff.

Stdlib only, like the rest of .forge/.
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import github_backend

# The consolidated-review marker (master.CONSOLIDATED_MARKER). The
# verdict IS the PR's consolidated review: it carries the marker so
# the review-once machinery recognizes it, and the sha: suffix names
# the head it was posted for, exactly like a master rollup.
MARKER = "chomp:consolidated-review"

DEFAULT_MAX_LINES = 2000


def measure_diff(diff_text):
    """Measure a unified diff. Returns (changed_lines, file_count).

    changed_lines counts added + deleted content lines. The +++/---
    header lines never count: they sit outside hunks, and a content
    line inside a hunk that itself starts with ++ or -- (a deleted
    "-- comment" line, say) still counts, because hunk state — not
    the prefix alone — decides what is content. A file section with
    no hunks (binary, pure rename, or a patch the API did not serve)
    counts toward file_count with zero changed lines.
    """
    changed = 0
    files = 0
    in_hunk = False
    for line in diff_text.splitlines():
        if line.startswith("diff --git "):
            files += 1
            in_hunk = False
        elif line.startswith("@@"):
            in_hunk = True
        elif in_hunk and line[:1] in ("+", "-"):
            changed += 1
    return changed, files


def _files(count):
    return f"{count} file" if count == 1 else f"{count} files"


def build_size_verdict(sha, changed_lines, file_count, max_lines):
    """Build the consolidated-review body for an oversized PR."""
    return f"""<!-- {MARKER} sha:{sha} -->
## PR Reviewer Guide 🔍

Too big. Break it up.

This pull request changes {changed_lines:,} lines across {_files(file_count)}. The review limit is {max_lines:,} changed lines, so Chomp posted this verdict in place of a review.

Split the change into pull requests at or under the limit and each one gets a full review."""


def run(diff_text, max_lines, sha, post_fn):
    """Measure, and post the size verdict when over the limit.

    post_fn(body) posts the verdict as the consolidated review. It is
    only called for an oversized PR. Returns the process exit code:
    1 when the verdict was posted (the check must fail), 0 when the
    PR is within the limit and the review proceeds.
    """
    changed, files = measure_diff(diff_text)
    print(f"PR size: {changed:,} changed lines across {_files(files)} "
          f"(limit {max_lines:,}).", file=sys.stderr)
    if changed <= max_lines:
        print("Within the review limit; proceeding.", file=sys.stderr)
        return 0
    post_fn(build_size_verdict(sha, changed, files, max_lines))
    print(f"::error::PR too big to review: {changed:,} changed lines "
          f"across {_files(files)} exceeds the {max_lines:,}-line limit. "
          f"Size verdict posted; break the PR up.")
    return 1


def main(argv=None):
    parser = argparse.ArgumentParser()
    parser.add_argument("--diff", required=True, help="Path to the PR diff (mr.diff)")
    parser.add_argument("--max-lines", type=int, default=DEFAULT_MAX_LINES,
                        help="Review limit in changed lines (additions + deletions)")
    parser.add_argument("--iid", required=True, help="Pull request number")
    parser.add_argument("--sha", required=True, help="Pull request head SHA")
    args = parser.parse_args(argv)

    if args.max_lines <= 0:
        print(f"ERROR: --max-lines must be a positive line count, "
              f"got {args.max_lines}.", file=sys.stderr)
        return 2
    try:
        with open(args.diff, "r", encoding="utf-8", errors="replace") as fh:
            diff_text = fh.read()
    except OSError as e:
        print(f"ERROR: could not read diff {args.diff}: {e}", file=sys.stderr)
        return 2

    def post_fn(body):
        api_url, repo, token = github_backend.gh_env()
        comment = github_backend.post_issue_comment(
            api_url, repo, args.iid, token, body)
        print(f"Size verdict posted: comment {comment.get('id')}",
              file=sys.stderr)
        summary_path = os.environ.get("GITHUB_STEP_SUMMARY")
        if summary_path:
            with open(summary_path, "a", encoding="utf-8") as fh:
                fh.write(body + "\n")

    return run(diff_text, args.max_lines, args.sha, post_fn)


if __name__ == "__main__":
    sys.exit(main())
