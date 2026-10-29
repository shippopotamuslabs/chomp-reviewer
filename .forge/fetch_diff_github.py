#!/usr/bin/env python3
"""
Forge diff fetcher (GitHub): fetches the PR diff, writes mr.diff.

Usage:
    python3 .forge/fetch_diff_github.py <api_url> <owner/repo> <pr_number> <token> --output mr.diff

The GitHub pulls API serves the complete unified diff with the
application/vnd.github.diff media type — until the diff passes GitHub's
size limits and the endpoint refuses with HTTP 406. Then
github_backend.fetch_pull_diff falls back to assembling the same
document from the paginated pulls files API, like the GitLab fetcher.
"""
import argparse
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from github_backend import fetch_pull_diff


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("api_url", help="GitHub API base URL")
    parser.add_argument("repo", help="Repository as owner/repo")
    parser.add_argument("number", help="Pull request number")
    parser.add_argument("token", help="GitHub token")
    parser.add_argument("--output", default="mr.diff", help="Output diff file")
    args = parser.parse_args()

    document = fetch_pull_diff(args.api_url, args.repo, args.number, args.token)

    if not document.strip():
        print("ERROR: PR has no diff; nothing to review.", file=sys.stderr)
        sys.exit(1)

    with open(args.output, "w") as f:
        f.write(document)
    print(f"Wrote {args.output} with {document.count('diff --git ')} file diffs")


if __name__ == "__main__":
    main()
