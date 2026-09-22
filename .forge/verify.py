#!/usr/bin/env python3
"""
Forge verify: on subsequent pushes, check if open Forge discussions are addressed.

For each open discussion recorded by forge:master, uses the LLM to determine
if the new diff addresses the finding. If ADDRESSED, resolves the discussion.

Usage:
    python3 .forge/verify.py --iid 133 --sha <new_sha> --diff mr.diff

Environment:
    OPENROUTER_API_KEY: API key for OpenRouter
    PR_AGENT_GITLAB_TOKEN: GitLab token for API access
    CI_API_V4_URL: GitLab API base URL
    CI_PROJECT_ID: Project ID
    FORGE_MODEL: model to use
"""
import argparse
import json
import os
import re
import sys
import urllib.request
import urllib.error
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from openrouter_client import get_model, call_openrouter


def gitlab_request(method, url, token, data=None, form_data=None):
    """Make a GitLab API request."""
    headers = {"PRIVATE-TOKEN": token}
    if data is not None:
        data = json.dumps(data).encode()
        headers["Content-Type"] = "application/json"
    elif form_data is not None:
        data = urllib.parse.urlencode(form_data).encode()
        headers["Content-Type"] = "application/x-www-form-urlencoded"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            body = resp.read().decode()
            return json.loads(body) if body else {}
    except urllib.error.HTTPError as e:
        body = e.read().decode()[:1000]
        print(f"ERROR: GitLab API {method} {url} failed: {e.code} {body}", file=sys.stderr)
        sys.exit(1)


def get_forge_discussions(api_url, project_id, iid, token):
    """Fetch all discussions, filter to open Forge-recorded ones."""
    discussions = gitlab_request(
        "GET",
        f"{api_url}/projects/{project_id}/merge_requests/{iid}/discussions?per_page=100",
        token,
    )
    forge_open = []
    for d in discussions:
        # Skip resolved discussions
        notes = d.get("notes", [])
        if not notes:
            continue
        # Check if any note is not resolved (discussions resolve as a unit)
        # GitLab marks individual notes as resolved; a discussion is resolved if all are
        if all(n.get("resolved", False) for n in notes):
            continue
        # Check for forge marker in the first note
        first_body = notes[0].get("body", "")
        if "forge:consolidated-review" not in first_body:
            continue
        # Extract finding details from the note
        forge_open.append({
            "id": d.get("id"),
            "notes": notes,
            "body": first_body,
        })
    return forge_open


def resolve_discussion(api_url, project_id, iid, disc_id, token):
    """Resolve one MR discussion via the GitLab API.

    GitLab resolves a thread with PUT on the discussion resource itself:
    PUT /projects/:id/merge_requests/:iid/discussions/:discussion_id
    with {"resolved": true}. There is no .../discussions/:id/resolve
    sub-resource (it 404s).
    """
    return gitlab_request(
        "PUT",
        f"{api_url}/projects/{project_id}/merge_requests/{iid}/discussions/{disc_id}",
        token,
        data={"resolved": True},
    )


def build_verify_prompt(finding_body, diff):
    """Build the verification prompt for the LLM."""
    return f"""You are verifying whether a code review finding has been addressed by a new diff.

## Finding (from the review discussion)
{finding_body}

## New diff (changes since the review)
{diff}

## Task
Determine if the finding has been ADDRESSED by the changes in the diff.

- ADDRESSED: The diff clearly fixes the issue described in the finding. The problematic code pattern is gone, or the recommended fix has been applied.
- NOT_ADDRESSED: The diff does not fix the issue, the issue is still present, or you cannot determine from the diff alone.

Line overlap alone is NOT sufficient — you must verify the semantic fix. For example, if the finding was "missing null check on line 42" and the diff touches line 42 but doesn't add a null check, it is NOT_ADDRESSED.

Respond with ONLY a JSON object:
{{"verdict": "ADDRESSED", "reason": "brief explanation"}}
or
{{"verdict": "NOT_ADDRESSED", "reason": "brief explanation"}}"""


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iid", required=True, help="MR internal ID")
    parser.add_argument("--sha", required=True, help="New MR head SHA")
    parser.add_argument("--diff", required=True, help="Path to diff file (new changes)")
    parser.add_argument("--prompt-dir", default=".forge/prompts")
    parser.add_argument("--dry-run", action="store_true", help="Don't resolve, just report")
    args = parser.parse_args()

    api_key = os.environ.get("OPENROUTER_API_KEY")
    gitlab_token = os.environ.get("PR_AGENT_GITLAB_TOKEN")
    api_url = os.environ.get("CI_API_V4_URL", "https://gitlab.com/api/v4")
    project_id = os.environ.get("CI_PROJECT_ID")

    if not api_key:
        print("ERROR: OPENROUTER_API_KEY not set", file=sys.stderr)
        sys.exit(1)
    if not gitlab_token and not args.dry_run:
        print("ERROR: PR_AGENT_GITLAB_TOKEN not set", file=sys.stderr)
        sys.exit(1)
    if not project_id:
        print("ERROR: CI_PROJECT_ID not set", file=sys.stderr)
        sys.exit(1)

    model = get_model()

    with open(args.diff) as f:
        diff = f.read()

    if not diff.strip():
        print("No diff content; nothing to verify.", file=sys.stderr)
        return

    # Get open Forge discussions
    discussions = get_forge_discussions(api_url, project_id, args.iid, gitlab_token)
    print(f"Found {len(discussions)} open Forge discussions", file=sys.stderr)

    if not discussions:
        print("No open Forge discussions to verify.", file=sys.stderr)
        return

    addressed = 0
    not_addressed = 0

    for disc in discussions:
        disc_id = disc["id"]
        body = disc["body"]
        # Truncate body if too long (keep the finding description)
        if len(body) > 4000:
            body = body[:4000] + "\n...[truncated]"

        prompt = build_verify_prompt(body, diff[:8000])  # Truncate diff if huge

        print(f"Verifying discussion {disc_id}...", file=sys.stderr)
        try:
            response = call_openrouter(api_key, model, prompt)
        except Exception as e:
            print(f"  WARNING: LLM call failed for {disc_id}: {e}", file=sys.stderr)
            not_addressed += 1
            continue

        # Parse verdict
        response = response.strip()
        if response.startswith("```"):
            lines = response.split("\n")
            if lines[0].startswith("```"):
                lines = lines[1:]
            if lines and lines[-1].strip() == "```":
                lines = lines[:-1]
            response = "\n".join(lines).strip()

        try:
            result = json.loads(response)
            verdict = result.get("verdict", "NOT_ADDRESSED")
            reason = result.get("reason", "")
        except json.JSONDecodeError:
            print(f"  WARNING: Invalid JSON for {disc_id}, treating as NOT_ADDRESSED", file=sys.stderr)
            verdict = "NOT_ADDRESSED"
            reason = "LLM returned invalid JSON"

        print(f"  Verdict: {verdict} — {reason}", file=sys.stderr)

        if verdict == "ADDRESSED":
            addressed += 1
            if not args.dry_run:
                resolve_discussion(api_url, project_id, args.iid, disc_id, gitlab_token)
                print(f"  Resolved discussion {disc_id}", file=sys.stderr)
        else:
            not_addressed += 1

    print(f"Verification complete: {addressed} addressed, {not_addressed} still open", file=sys.stderr)


if __name__ == "__main__":
    main()
