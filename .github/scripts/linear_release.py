#!/usr/bin/env python3
"""Flip the Linear issues shipped in a release tag from Merged to Released.

CHOMP-57. Invoked by .github/workflows/linear-release.yml on every v*
tag push. The release boundary is the previous tag reachable from the
new one; every CHOMP key mentioned in a commit message inside that
range names a candidate issue. A candidate flips only when Linear
currently shows it in the Merged state, so re-running a tag is
harmless and issues in any other state are never touched.

Stdlib only. Auth is the LINEAR_API_KEY env var (a Linear personal API
key, stored as a repo secret). Without it the script prints a notice
and exits 0: tag pushes must not go red while the secret is pending.

Usage: linear_release.py <tag> [--dry-run]
"""

import json
import os
import re
import subprocess
import sys
import urllib.request

API_URL = "https://api.linear.app/graphql"
TEAM_KEY = "CHOMP"
KEY_RE = re.compile(r"\bCHOMP-\d+\b")


def git(*args):
    return subprocess.run(
        ["git", *args], check=True, capture_output=True, text=True
    ).stdout.strip()


def previous_tag(tag):
    try:
        return git("describe", "--tags", "--abbrev=0", f"{tag}^")
    except subprocess.CalledProcessError:
        return None


def extract_keys(text):
    keys = []
    for key in KEY_RE.findall(text):
        if key not in keys:
            keys.append(key)
    return keys


def shipped_keys(tag, prev):
    rev_range = f"{prev}..{tag}" if prev else tag
    return extract_keys(git("log", rev_range, "--format=%B%x1e"))


def graphql(api_key, query, variables=None):
    payload = json.dumps({"query": query, "variables": variables or {}}).encode()
    request = urllib.request.Request(
        API_URL,
        data=payload,
        headers={"Authorization": api_key, "Content-Type": "application/json"},
    )
    with urllib.request.urlopen(request) as response:
        body = json.load(response)
    if body.get("errors"):
        raise RuntimeError(f"Linear API errors: {body['errors']}")
    return body["data"]


def team_id(api_key):
    data = graphql(api_key, "{ teams { nodes { id key } } }")
    for team in data["teams"]["nodes"]:
        if team["key"] == TEAM_KEY:
            return team["id"]
    raise RuntimeError(f"Linear team {TEAM_KEY} not found")


def released_state_id(api_key, team):
    data = graphql(api_key, "{ workflowStates { nodes { id name team { id } } } }")
    for state in data["workflowStates"]["nodes"]:
        if state["name"] == "Released" and state["team"]["id"] == team:
            return state["id"]
    raise RuntimeError(f"No Released workflow state on team {TEAM_KEY}")


def merged_issues(api_key, team):
    query = """
      query($team: ID!, $cursor: String) {
        issues(
          filter: { team: { id: { eq: $team } }, state: { name: { eq: "Merged" } } }
          first: 250
          after: $cursor
        ) {
          nodes { id identifier }
          pageInfo { hasNextPage endCursor }
        }
      }
    """
    issues, cursor = [], None
    while True:
        page = graphql(api_key, query, {"team": team, "cursor": cursor})["issues"]
        issues.extend(page["nodes"])
        if not page["pageInfo"]["hasNextPage"]:
            return issues
        cursor = page["pageInfo"]["endCursor"]


def flip(api_key, issue_id, state_id):
    mutation = """
      mutation($id: String!, $state: String!) {
        issueUpdate(id: $id, input: { stateId: $state }) {
          success
          issue { identifier state { name } }
        }
      }
    """
    result = graphql(api_key, mutation, {"id": issue_id, "state": state_id})
    return result["issueUpdate"]["success"]


def main(argv):
    if len(argv) < 2:
        print(__doc__)
        return 2
    tag = argv[1]
    dry_run = "--dry-run" in argv[2:]

    prev = previous_tag(tag)
    keys = shipped_keys(tag, prev)
    print(f"Tag {tag}; previous tag: {prev or '(none)'}")
    print(f"CHOMP keys in range: {', '.join(keys) if keys else '(none)'}")
    if dry_run or not keys:
        return 0

    api_key = os.environ.get("LINEAR_API_KEY", "").strip()
    if not api_key:
        print(
            "LINEAR_API_KEY is not set; skipping the Linear update. "
            "Add the secret to flip issues automatically on the next tag."
        )
        return 0

    team = team_id(api_key)
    state = released_state_id(api_key, team)
    merged = {issue["identifier"]: issue["id"] for issue in merged_issues(api_key, team)}

    flipped = []
    for key in keys:
        if key not in merged:
            print(f"{key}: not in Merged, left alone")
            continue
        if flip(api_key, merged[key], state):
            flipped.append(key)
            print(f"{key}: Merged -> Released")
        else:
            print(f"{key}: Linear reported the update did not succeed")
    print(f"Flipped {len(flipped)} of {len(keys)} keyed issues.")
    return 0 if len(flipped) == sum(1 for k in keys if k in merged) else 1


if __name__ == "__main__":
    sys.exit(main(sys.argv))
