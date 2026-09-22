#!/usr/bin/env python3
"""
Forge diff fetcher: fetches the MR diff via GitLab API, writes mr.diff.

Usage:
    python3 .forge/fetch_diff.py <api_url> <project_id> <mr_iid> <token> --output mr.diff

The output is a unified diff with proper 'diff --git' headers prepended
to each file's diff fragment (the GitLab API returns fragments without headers).
"""
import argparse
import json
import sys
import urllib.request


def assemble_mr_diff(changes):
    """Assemble per-file GitLab API diff fragments into one mr.diff document.

    Each fragment is preserved byte-for-byte apart from the prepended
    'diff --git' header and newline separators between file sections. In
    particular a file diff ending in a blank context line (a single ' '
    line) must keep that line: dropping it leaves the hunk one body line
    short and unidiff aborts with 'Hunk diff line expected: diff --git ...'.
    Only newline padding is trimmed, never trailing whitespace content.
    Returns the document string, or "" when there are no diffs.
    """
    diff_parts = []
    for change in changes:
        diff = change.get("diff", "")
        old_path = change.get("old_path", "")
        new_path = change.get("new_path", "")
        if diff and diff.strip():
            header = f"diff --git a/{old_path} b/{new_path}"
            diff_parts.append(header + "\n" + diff.strip("\n"))

    if not diff_parts:
        return ""

    return "\n".join(diff_parts) + "\n"


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("api_url", help="GitLab API base URL")
    parser.add_argument("project_id", help="Project ID")
    parser.add_argument("mr_iid", help="MR internal ID")
    parser.add_argument("token", help="GitLab private token")
    parser.add_argument("--output", default="mr.diff", help="Output diff file")
    args = parser.parse_args()

    url = f"{args.api_url}/projects/{args.project_id}/merge_requests/{args.mr_iid}/changes?per_page=100"
    req = urllib.request.Request(url, headers={"PRIVATE-TOKEN": args.token})
    try:
        data = json.load(urllib.request.urlopen(req, timeout=30))
    except Exception as e:
        print(f"ERROR: could not fetch MR changes: {e}", file=sys.stderr)
        sys.exit(1)

    document = assemble_mr_diff(data.get("changes", []))

    if not document:
        print("ERROR: MR has no diffs; nothing to review.", file=sys.stderr)
        sys.exit(1)

    with open(args.output, "w") as f:
        f.write(document)
    print(f"Wrote {args.output} with {document.count('diff --git ')} file diffs")


if __name__ == "__main__":
    main()
