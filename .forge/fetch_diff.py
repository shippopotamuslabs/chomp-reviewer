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

    diff_parts = []
    for change in data.get("changes", []):
        diff = change.get("diff", "")
        old_path = change.get("old_path", "")
        new_path = change.get("new_path", "")
        if diff and diff.strip():
            header = f"diff --git a/{old_path} b/{new_path}"
            diff_parts.append(header + "\n" + diff.strip())

    if not diff_parts:
        print("ERROR: MR has no diffs; nothing to review.", file=sys.stderr)
        sys.exit(1)

    with open(args.output, "w") as f:
        f.write("\n".join(diff_parts) + "\n")
    print(f"Wrote {args.output} with {len(diff_parts)} file diffs")


if __name__ == "__main__":
    main()
