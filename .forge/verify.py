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


def gitlab_request(method, url, token, data=None, form_data=None, fatal=True):
    """Make a GitLab API request.

    With fatal=True (default) an HTTP error prints and exits, for requests
    the run cannot proceed without (e.g. listing discussions). Pass
    fatal=False for per-item requests where one failure must not abort the
    whole run; the HTTP error is raised as RuntimeError instead.
    """
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
        msg = f"ERROR: GitLab API {method} {url} failed: {e.code} {body}"
        if fatal:
            print(msg, file=sys.stderr)
            sys.exit(1)
        raise RuntimeError(msg)


def get_forge_discussions(api_url, project_id, iid, token):
    """Fetch all discussions, filter to open Forge-recorded ones."""
    discussions = gitlab_request(
        "GET",
        f"{api_url}/projects/{project_id}/merge_requests/{iid}/discussions?per_page=100",
        token,
    )
    forge_open = []
    for d in discussions:
        # Skip the standalone rollup note (posted via POST /notes): it
        # surfaces in the discussions list as an individual (non-thread)
        # note and is not resolvable. PUT {"resolved": true} on it fails
        # with 403 Forbidden.
        if d.get("individual_note"):
            continue
        notes = d.get("notes", [])
        if not notes:
            continue
        # Skip threads GitLab marks as not resolvable.
        if notes[0].get("resolvable") is False:
            continue
        # Skip resolved discussions
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
    # fatal=False: one unresolvable thread (e.g. a 403 for a token
    # without the Developer role) must not abort verification of the
    # remaining discussions; the caller logs the ERROR and continues.
    return gitlab_request(
        "PUT",
        f"{api_url}/projects/{project_id}/merge_requests/{iid}/discussions/{disc_id}",
        token,
        data={"resolved": True},
        fatal=False,
    )


FOCUS_FILES_RE = re.compile(r"([\w\-.]+/[\w\-.]+/\S+?\.\w+)\[L?\d+(?:-\d+)?\]")
DIFF_BUDGET_CHARS = 30000


def split_diff_sections(diff):
    """Split an assembled mr.diff into (new_path, section) pairs.

    Sections split on 'diff --git' headers; the section keeps its header so
    it stays a valid fragment on its own. Paths come from the b/ side.
    """
    sections = []
    current_path = None
    current_lines = []
    for line in diff.split("\n"):
        if line.startswith("diff --git "):
            if current_path is not None:
                sections.append((current_path, "\n".join(current_lines)))
            parts = line.split(" b/", 1)
            current_path = parts[1] if len(parts) == 2 else line
            current_lines = [line]
        elif current_path is not None:
            current_lines.append(line)
    if current_path is not None:
        sections.append((current_path, "\n".join(current_lines)))
    return sections


def extract_focus_files(finding_body):
    """Extract repo-relative file paths cited by a finding.

    Findings cite files as `path[N-M]` (Fix Focus Areas) and as
    `[path[LN-M]](url)` (Code links). Only the file is used — cited line
    numbers go stale after pushes, so hunks are never filtered by line.
    Returns deduplicated paths in first-seen order.
    """
    seen = []
    for match in FOCUS_FILES_RE.finditer(finding_body):
        path = match.group(1).rstrip(".,;:")
        if path not in seen:
            seen.append(path)
    return seen


def truncate_marked(text, budget):
    """Truncate to budget chars with an explicit marker, or return unchanged.

    A silent mid-file cut makes the LLM report "diff does not touch X" for
    files past the cut; the marker keeps that failure honest instead.
    """
    if len(text) <= budget:
        return text
    return (
        text[:budget]
        + f"\n...[diff truncated: showing first {budget} of {len(text)} chars; "
        + "files beyond this point were NOT reviewed]"
    )


FINAL_STATE_BUDGET_CHARS = 12000


def fetch_file_at_ref(api_url, project_id, path, ref, token):
    """Fetch a file's content at a commit via the repository API.

    Returns (status, text) with status "present", "absent" (404: deleted or
    never added — itself a verdict signal), or "unknown" (network/API failure,
    which must never be treated as proof of absence).
    """
    encoded = urllib.parse.quote(path, safe="")
    url = f"{api_url}/projects/{project_id}/repository/files/{encoded}/raw?ref={ref}"
    req = urllib.request.Request(url, headers={"PRIVATE-TOKEN": token})
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            raw = resp.read().decode("utf-8", errors="replace")
        return "present", raw
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return "absent", ""
        print(f"  WARNING: file fetch failed for {path}: {e}", file=sys.stderr)
        return "unknown", ""
    except Exception as e:
        print(f"  WARNING: file fetch failed for {path}: {e}", file=sys.stderr)
        return "unknown", ""


def build_final_state_block(api_url, project_id, focus_files, ref, token):
    """Ground verdicts in the verified commit's actual file state.

    The evidence diff is net vs target, so a fix that removes lines the same
    MR added shows no removal hunk — the LLM then concludes "no removal
    happened" for a fix that is present. The final-state block is ground
    truth for whether cited code still exists; absence here plus a matching
    fix is ADDRESSED even when the diff shows no removal.
    """
    blocks = []
    for path in focus_files:
        status, text = fetch_file_at_ref(api_url, project_id, path, ref, token)
        if status == "absent":
            blocks.append(f"### {path}\n[file deleted at {ref}]")
        elif status == "unknown":
            blocks.append(f"### {path}\n[file state unavailable — judge from the diff]")
        else:
            blocks.append(f"### {path}\n{truncate_marked(text, FINAL_STATE_BUDGET_CHARS)}")
    if not blocks:
        return ""
    return (
        f"## Final file state at {ref} (ground truth for whether cited code "
        + "still exists; net diffs hide add-then-remove within the MR)\n"
        + "\n".join(blocks)
    )


def select_diff_for_finding(diff, finding_body):
    """Scope the verification diff to the finding's cited files.

    A flat head-truncation of a large MR diff drops whole files (e.g. every
    domain file when app/ sorts first), and the LLM then verdicts
    NOT_ADDRESSED with "diff does not touch X" for fixes that are present.
    Scoping to the cited files keeps each verdict's evidence complete.
    Falls back to legacy head-truncation when no files are cited.
    """
    focus = extract_focus_files(finding_body)
    if focus:
        by_path = {}
        for new_path, section in split_diff_sections(diff):
            by_path.setdefault(new_path, section)
        picked = []
        for path in focus:
            for new_path, section in by_path.items():
                if new_path == path or new_path.endswith("/" + path):
                    picked.append(section)
                    break
        scoped = "\n".join(picked)
        if scoped.strip():
            return truncate_marked(scoped, DIFF_BUDGET_CHARS)
    return truncate_marked(diff, 8000)


def build_verify_prompt(finding_body, diff):
    """Build the verification prompt for the LLM."""
    return f"""You are verifying whether a code review finding has been addressed by a new diff.

## Finding (from the review discussion)
{finding_body}

## Diff under review (MR changes scoped to this finding's files)
{diff}

## Task
Determine if the finding has been ADDRESSED by the changes in the diff.

- ADDRESSED: The diff clearly fixes the issue described in the finding. The problematic code pattern is gone, or the recommended fix has been applied.
- NOT_ADDRESSED: The diff does not fix the issue, the issue is still present, or you cannot determine from the diff alone.
- The diff is net vs target: code the MR added and then removed shows NO removal hunk. The final-state section is ground truth for whether cited code still exists — absence there plus the recommended fix elsewhere is ADDRESSED, even when the diff shows no removal.

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
    resolve_failed = 0

    for disc in discussions:
        disc_id = disc["id"]
        body = disc["body"]
        # Truncate body if too long (keep the finding description)
        if len(body) > 4000:
            body = body[:4000] + "\n...[truncated]"

        evidence = select_diff_for_finding(diff, body)
        final_state = build_final_state_block(
            api_url, project_id, extract_focus_files(body), args.sha, gitlab_token,
        )
        if final_state:
            evidence = evidence + "\n\n" + final_state
        prompt = build_verify_prompt(body, evidence)

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
                try:
                    resolve_discussion(api_url, project_id, args.iid, disc_id, gitlab_token)
                    print(f"  Resolved discussion {disc_id}", file=sys.stderr)
                except Exception as e:
                    resolve_failed += 1
                    print(f"  ERROR: Failed to resolve discussion {disc_id}: {e}", file=sys.stderr)
        else:
            not_addressed += 1

    print(f"Verification complete: {addressed} addressed, {not_addressed} still open", file=sys.stderr)
    if resolve_failed:
        print(f"ERROR: {resolve_failed} discussions could not be resolved", file=sys.stderr)
        sys.exit(1)


if __name__ == "__main__":
    main()
