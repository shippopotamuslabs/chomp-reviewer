#!/usr/bin/env python3
"""
Chomp verify: on subsequent pushes, check if open Chomp discussions are addressed.

For each open discussion recorded by forge:master, uses the LLM to determine
if the new diff addresses the finding. If ADDRESSED, resolves the discussion.

Usage:
    python3 .forge/verify.py --iid 133 --sha <new_sha> --diff mr.diff

Environment:
    OPENROUTER_API_KEY: API key for OpenRouter
    FORGE_MODEL: model to use
    FORGE_PLATFORM: "github" or "gitlab" (default: github under GitHub
        Actions, gitlab otherwise)
    GitLab: PR_AGENT_GITLAB_TOKEN, CI_API_V4_URL, CI_PROJECT_ID
    GitHub: GITHUB_TOKEN (Forge app installation token), GITHUB_API_URL,
        GITHUB_REPOSITORY — --iid carries the PR number
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
import github_backend


def detect_platform():
    """Select the review platform backend from the environment.

    FORGE_PLATFORM wins when set; otherwise runs under GitHub Actions
    (GITHUB_ACTIONS=true) use the GitHub backend and everything else
    keeps the historical GitLab default.
    """
    explicit = os.environ.get("FORGE_PLATFORM", "").strip().lower()
    if explicit in ("github", "gitlab"):
        return explicit
    if os.environ.get("GITHUB_ACTIONS") == "true":
        return "github"
    return "gitlab"


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
    """Fetch all discussions, filter to open Chomp-recorded ones."""
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
        if not any(m in first_body for m in
                   ("chomp:consolidated-review", "forge:consolidated-review")):
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


def build_verdict_reply(verdict, reason, sha):
    """Build the inline reply body for a verification verdict.

    Posted as a reply on the original discussion so the outcome is visible
    where the finding was raised, not just in the job log.
    """
    short = (sha or "")[:8] or "unknown"
    if verdict == "ADDRESSED":
        header = f"✅ Chomp verification at `{short}`: **ADDRESSED**"
    else:
        header = f"⏳ Chomp verification at `{short}`: **NOT_ADDRESSED** (still open)"
    reason = (reason or "").strip()
    if reason:
        return f"{header}\n\n{reason}"
    return header


def post_discussion_reply(api_url, project_id, iid, disc_id, token, body):
    """Post a reply note on one MR discussion via the GitLab API.

    Replies inherit the discussion's thread, so the verdict lands inline on
    the original comment. A discussion-notes POST has no /resolve suffix.
    """
    # fatal=False: one unreplyable thread must not abort verification of
    # the remaining discussions; the caller logs the ERROR and continues.
    return gitlab_request(
        "POST",
        f"{api_url}/projects/{project_id}/merge_requests/{iid}/discussions/{disc_id}/notes",
        token,
        data={"body": body},
        fatal=False,
    )


def superseded_by(run_sha, live_sha):
    """True when the live head has moved past the SHA this run targets.

    Queued runs execute minutes after their trigger (workflow concurrency
    queue), so a verify run's --sha routinely names a superseded commit.
    Verdicts computed against it are false noise on the current head —
    the queued run for the newer head performs the real verify. An
    unknown live head (None) never supersedes (CHOMP-58).
    """
    return bool(run_sha) and bool(live_sha) and run_sha != live_sha


def live_head_sha(platform, api_url, project_id, iid, token,
                  gh_api, gh_repo, gh_token):
    """Fetch the MR/PR's live head SHA, or None when it cannot be read."""
    if platform == "github":
        return github_backend.fetch_live_head_sha(
            gh_api, gh_repo, iid, gh_token)
    try:
        mr = gitlab_request(
            "GET",
            f"{api_url}/projects/{project_id}/merge_requests/{iid}",
            token, fatal=False)
    except Exception:
        return None
    return (mr or {}).get("sha") or None


LAST_VERIFIED_RE = re.compile(
    r"<!-- chomp:last-verified sha:[0-9a-f]+ -->\n"
    r"Last verified at `[^`]+`\.\n?")


def apply_rollup_currency(body, sha):
    """Return the rollup body with its currency line set to sha (CHOMP-59).

    The consolidated-review marker line is load-bearing (duplicate
    suppression and verify both find the rollup by it), so it is never
    rewritten: the reviewed SHA stays as provenance and a separate
    last-verified line carries currency. Re-applying replaces the
    previous currency line instead of stacking a new one.
    """
    block = (f"<!-- chomp:last-verified sha:{sha} -->\n"
             f"Last verified at `{(sha or '')[:8]}`.")
    body = LAST_VERIFIED_RE.sub("", body or "")
    lines = body.split("\n")
    for i, line in enumerate(lines):
        if "consolidated-review" in line:
            lines.insert(i + 1, block)
            return "\n".join(lines)
    lines.insert(0, block)
    return "\n".join(lines)


SUPERSEDED_RE = re.compile(
    r"<!-- chomp:superseded sha:[0-9a-f]+ -->\n"
    r"Superseded by a review at `[^`]+`\.\n?")


def mark_rollup_superseded(body, by_sha):
    """Return the rollup body labelled superseded by a newer review.

    For a wholesale replacement (a fresh consolidated review posted for a
    newer head): the old rollup stays readable as history but never reads
    as the current review. Idempotent per by_sha, like the currency line.
    """
    block = (f"<!-- chomp:superseded sha:{by_sha} -->\n"
             f"Superseded by a review at `{(by_sha or '')[:8]}`.")
    body = SUPERSEDED_RE.sub("", body or "")
    lines = body.split("\n")
    for i, line in enumerate(lines):
        if "consolidated-review" in line:
            lines.insert(i + 1, block)
            return "\n".join(lines)
    lines.insert(0, block)
    return "\n".join(lines)


def update_rollup_currency(platform, api_url, project_id, iid, sha, token,
                           gh_api, gh_repo, gh_token):
    """Edit the consolidated rollup so its currency names sha (CHOMP-59).

    Returns True when the edit was needed but failed (the caller counts
    it like a reply failure); False when updated or when there is no
    rollup to update. GitHub edits the issue comment; GitLab edits the
    rollup note.
    """
    try:
        if platform == "github":
            comment = None
            for marker in ("chomp:consolidated-review",
                           "forge:consolidated-review"):
                comment = github_backend.find_marker_comment(
                    gh_api, gh_repo, iid, gh_token, marker)
                if comment:
                    break
            if not comment:
                return False
            new_body = apply_rollup_currency(comment.get("body") or "", sha)
            if new_body == (comment.get("body") or ""):
                return False
            github_backend.update_issue_comment(
                gh_api, gh_repo, comment["id"], gh_token, new_body)
        else:
            notes = gitlab_request(
                "GET",
                f"{api_url}/projects/{project_id}/merge_requests/{iid}"
                f"/notes?per_page=100",
                token, fatal=False)
            note = None
            for candidate in notes or []:
                if any(m in (candidate.get("body") or "") for m in
                       ("chomp:consolidated-review",
                        "forge:consolidated-review")):
                    note = candidate
                    break
            if not note:
                return False
            new_body = apply_rollup_currency(note.get("body") or "", sha)
            if new_body == (note.get("body") or ""):
                return False
            gitlab_request(
                "PUT",
                f"{api_url}/projects/{project_id}/merge_requests/{iid}"
                f"/notes/{note['id']}",
                token, data={"body": new_body}, fatal=False)
        print(f"Rollup currency updated to {sha[:8]}", file=sys.stderr)
        return False
    except Exception as e:
        print(f"ERROR: rollup currency update failed: {e}", file=sys.stderr)
        return True


FOCUS_FILES_RE = re.compile(r"((?:[\w\-.]+/)*[\w\-.]+?\.\w+)\[L?\d+(?:-\d+)?\]")
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


def build_final_state_block(api_url, project_id, focus_files, ref, token, fetcher=None):
    """Ground verdicts in the verified commit's actual file state.

    The evidence diff is net vs target, so a fix that removes lines the same
    MR added shows no removal hunk — the LLM then concludes "no removal
    happened" for a fix that is present. The final-state block is ground
    truth for whether cited code still exists; absence here plus a matching
    fix is ADDRESSED even when the diff shows no removal.

    fetcher overrides the file fetch (same (api_url, project, path, ref,
    token) -> (status, text) contract); the GitHub path passes
    github_backend.fetch_file_at_ref, whose project slot carries the repo.
    """
    fetch = fetcher if fetcher is not None else fetch_file_at_ref
    blocks = []
    for path in focus_files:
        status, text = fetch(api_url, project_id, path, ref, token)
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


CONFORMANCE_OPEN_STATUSES = ("not evident", "contradicted")

CONFORMANCE_CRITERION_RE = re.compile(r"^-\s*\[([^\]]+)\]\s*(.*?)\s*$")
CONFORMANCE_EVIDENCE_RE = re.compile(r"\(evidence:\s*(.*)\)\s*$")


def parse_conformance_criteria(rollup_body):
    """Parse the ## Ticket conformance section from a rollup comment body.

    Returns (overall, criteria). overall is None when the rollup carries
    no conformance section (the caller no-ops). Each criterion is
    {"status", "criterion", "evidence"}, mirroring the lines
    conformance_section_lines in master.py renders:
    "- [status] criterion (evidence: ...)".
    """
    if not rollup_body or "## Ticket conformance" not in rollup_body:
        return None, []
    section = rollup_body.split("## Ticket conformance", 1)[1]
    overall = ""
    match = re.search(r"^Overall:\s*(.+?)\s*$", section, re.MULTILINE)
    if match:
        overall = match.group(1).strip()
    criteria = []
    for line in section.splitlines():
        match = CONFORMANCE_CRITERION_RE.match(line.strip())
        if not match:
            continue
        rest = match.group(2).strip()
        evidence = ""
        ev = CONFORMANCE_EVIDENCE_RE.search(rest)
        if ev:
            evidence = ev.group(1).strip()
            rest = rest[:ev.start()].strip()
        criteria.append({
            "status": match.group(1).strip(),
            "criterion": rest,
            "evidence": evidence,
        })
    return overall, criteria


def open_conformance_criteria(criteria):
    """Select criteria still needing work: not evident or contradicted."""
    return [c for c in criteria
            if c.get("status", "").strip() in CONFORMANCE_OPEN_STATUSES]


def build_conformance_prompt(criterion_text, evidence):
    """Build the re-check prompt for one conformance criterion."""
    return f"""You are re-checking whether a ticket conformance criterion is now addressed by new changes.

## Criterion (from the review's Ticket conformance section)
{criterion_text}

## Diff under review (changes scoped to this criterion's files)
{evidence}

## Task
Determine if the criterion is now ADDRESSED by the changes in the evidence.

- ADDRESSED: The evidence clearly shows the criterion's requirement is now met. The previously missing behavior exists, or the contradicted claim is now true of the code.
- NOT_ADDRESSED: The requirement is still not met, the code still contradicts it, or you cannot determine this from the evidence alone.
- The diff is net vs target: code the MR added and then removed shows NO removal hunk. The final-state section is ground truth for whether cited code still exists — absence there plus the required behavior elsewhere is ADDRESSED, even when the diff shows no removal.

Respond with ONLY a JSON object:
{{"verdict": "ADDRESSED", "reason": "brief explanation"}}
or
{{"verdict": "NOT_ADDRESSED", "reason": "brief explanation"}}"""


def parse_verdict_response(response):
    """Strip fences and parse an LLM verdict JSON object.

    Anything unparseable is NOT_ADDRESSED (mirrors the discussion loop's
    invalid-JSON handling); only an exact "ADDRESSED" verdict counts.
    """
    text = (response or "").strip()
    if text.startswith("```"):
        lines = text.split("\n")
        if lines[0].startswith("```"):
            lines = lines[1:]
        if lines and lines[-1].strip() == "```":
            lines = lines[:-1]
        text = "\n".join(lines).strip()
    try:
        result = json.loads(text)
        if not isinstance(result, dict):
            raise ValueError("verdict is not a JSON object")
        verdict = result.get("verdict", "NOT_ADDRESSED")
        reason = result.get("reason", "")
    except (ValueError, AttributeError):
        return "NOT_ADDRESSED", "LLM returned invalid JSON"
    if verdict != "ADDRESSED":
        verdict = "NOT_ADDRESSED"
    return verdict, reason


def build_conformance_recheck_body(sha, results):
    """Build the single re-check summary comment.

    results entries carry {"criterion", "status", "verdict", "reason"}.
    Addressed criteria are marked resolved in text; still-open ones stay
    listed. Never a merge gate.
    """
    short = (sha or "")[:8] or "unknown"
    lines = [f"Chomp conformance re-check at `{short}`", ""]
    for r in results:
        reason = (r.get("reason") or "").strip()
        suffix = f" — {reason}" if reason else ""
        if r.get("verdict") == "ADDRESSED":
            lines.append(
                f"- ✅ RESOLVED [{r.get('status')}] {r.get('criterion')}{suffix}")
        else:
            lines.append(
                f"- ⏳ STILL OPEN [{r.get('status')}] {r.get('criterion')}{suffix}")
    return "\n".join(lines)


def get_rollup_body_gitlab(api_url, project_id, iid, token):
    """Return the rollup note body carrying ## Ticket conformance, or None.

    fatal=False: a listing failure raises RuntimeError for the caller to
    treat as a fail-soft skip, never an abort.
    """
    notes = gitlab_request(
        "GET",
        f"{api_url}/projects/{project_id}/merge_requests/{iid}/notes?per_page=100",
        token,
        fatal=False,
    )
    for note in notes or []:
        body = (note or {}).get("body") or ""
        if "## Ticket conformance" in body:
            return body
    return None


def verify_conformance_recheck(platform, api_url, project_id, iid, sha, diff,
                               token, gh_api, gh_repo, gh_token,
                               api_key, model, dry_run):
    """Re-evaluate open conformance criteria against the new diff.

    Criteria live in the rollup comment, not in resolvable threads, so the
    outcome is posted as ONE summary comment: a new PR conversation
    comment on GitHub (issue comments have no replies, so replying on the
    rollup is impossible) and a new MR note on GitLab (the rollup is an
    individual, non-resolvable note that verify deliberately skips). The
    rollup itself is never resolved. Fail-soft per criterion: an LLM
    failure logs ERROR and skips that criterion; only a failed summary
    post returns True (the caller exits 1, mirroring reply_failed).
    """
    try:
        if platform == "github":
            comment = github_backend.find_marker_comment(
                gh_api, gh_repo, iid, gh_token, "## Ticket conformance")
            rollup_body = (comment or {}).get("body") if comment else None
        else:
            rollup_body = get_rollup_body_gitlab(api_url, project_id, iid, token)
    except Exception as e:
        print(f"  WARNING: conformance rollup fetch failed: {e}", file=sys.stderr)
        return False

    overall, criteria = parse_conformance_criteria(rollup_body or "")
    if overall is None:
        print("No Ticket conformance section; skipping conformance re-check.",
              file=sys.stderr)
        return False
    if overall == "no linked ticket":
        print("No linked ticket; skipping conformance re-check.", file=sys.stderr)
        return False
    pending = open_conformance_criteria(criteria)
    if not pending:
        print("No open conformance criteria; nothing to re-check.",
              file=sys.stderr)
        return False

    print(f"Re-checking {len(pending)} open conformance criteria...",
          file=sys.stderr)
    results = []
    for criterion in pending:
        text = criterion["criterion"]
        if criterion.get("evidence"):
            text = f"{text} (evidence: {criterion['evidence']})"
        evidence = select_diff_for_finding(diff, text)
        if platform == "github":
            final_state = build_final_state_block(
                gh_api, gh_repo, extract_focus_files(text), sha, gh_token,
                fetcher=github_backend.fetch_file_at_ref,
            )
        else:
            final_state = build_final_state_block(
                api_url, project_id, extract_focus_files(text), sha, token,
            )
        if final_state:
            evidence = evidence + "\n\n" + final_state
        prompt = build_conformance_prompt(text, evidence)
        try:
            response = call_openrouter(api_key, model, prompt)
        except Exception as e:
            print(f"  ERROR: conformance LLM call failed for "
                  f"{criterion['criterion'][:80]}: {e}", file=sys.stderr)
            continue
        verdict, reason = parse_verdict_response(response)
        print(f"  Criterion verdict: {verdict} — {criterion['criterion'][:80]}",
              file=sys.stderr)
        results.append({
            "criterion": criterion["criterion"],
            "status": criterion["status"],
            "verdict": verdict,
            "reason": reason,
        })

    if not results:
        print("Conformance re-check produced no verdicts; nothing to post.",
              file=sys.stderr)
        return False

    body = build_conformance_recheck_body(sha, results)
    if dry_run:
        print(f"Would post conformance re-check:\n{body}", file=sys.stderr)
        return False
    try:
        if platform == "github":
            github_backend.github_request(
                "POST", f"{gh_api}/repos/{gh_repo}/issues/{iid}/comments",
                gh_token, data={"body": body}, fatal=False)
        else:
            gitlab_request(
                "POST",
                f"{api_url}/projects/{project_id}/merge_requests/{iid}/notes",
                token, data={"body": body}, fatal=False)
        print(f"Posted conformance re-check for {len(results)} criteria.",
              file=sys.stderr)
    except Exception as e:
        print(f"ERROR: conformance re-check post failed: {e}", file=sys.stderr)
        return True
    return False


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iid", required=True, help="MR internal ID")
    parser.add_argument("--sha", required=True, help="New MR head SHA")
    parser.add_argument("--diff", required=True, help="Path to diff file (new changes)")
    parser.add_argument("--prompt-dir", default=".forge/prompts")
    parser.add_argument("--dry-run", action="store_true", help="Don't resolve, just report")
    args = parser.parse_args()

    platform = detect_platform()

    api_key = os.environ.get("OPENROUTER_API_KEY")
    gitlab_token = os.environ.get("PR_AGENT_GITLAB_TOKEN")
    api_url = os.environ.get("CI_API_V4_URL", "https://gitlab.com/api/v4")
    project_id = os.environ.get("CI_PROJECT_ID")
    gh_api, gh_repo, gh_token = github_backend.gh_env()

    if not api_key:
        print("ERROR: OPENROUTER_API_KEY not set", file=sys.stderr)
        sys.exit(1)
    if platform == "github":
        if not gh_token and not args.dry_run:
            print("ERROR: GITHUB_TOKEN not set", file=sys.stderr)
            sys.exit(1)
        if not gh_repo:
            print("ERROR: GITHUB_REPOSITORY not set", file=sys.stderr)
            sys.exit(1)
    else:
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

    # Live-head guard (CHOMP-58): a queued run executes minutes after its
    # trigger, behind earlier runs. If head has moved past this run's
    # SHA, every verdict it could post would judge a superseded commit —
    # post nothing; the queued run for the live head verifies for real.
    if not args.dry_run:
        live_sha = live_head_sha(platform, api_url, project_id, args.iid,
                                 gitlab_token, gh_api, gh_repo, gh_token)
        if superseded_by(args.sha, live_sha):
            print(f"Superseded: this run targets {args.sha[:8]} but live "
                  f"head is {live_sha[:8]}; posting nothing, the run for "
                  f"the live head will verify.", file=sys.stderr)
            return

    # Get open Forge discussions (GitHub: review threads)
    if platform == "github":
        discussions = github_backend.get_forge_review_threads(
            gh_api, gh_repo, args.iid, gh_token)
    else:
        discussions = get_forge_discussions(api_url, project_id, args.iid, gitlab_token)
    print(f"Found {len(discussions)} open Chomp discussions", file=sys.stderr)

    if not discussions:
        print("No open Chomp discussions to verify.", file=sys.stderr)

    addressed = 0
    not_addressed = 0
    resolve_failed = 0
    reply_failed = 0

    for disc in discussions:
        disc_id = disc["id"]
        body = disc["body"]
        # Truncate body if too long (keep the finding description)
        if len(body) > 4000:
            body = body[:4000] + "\n...[truncated]"

        evidence = select_diff_for_finding(diff, body)
        if platform == "github":
            final_state = build_final_state_block(
                gh_api, gh_repo, extract_focus_files(body), args.sha, gh_token,
                fetcher=github_backend.fetch_file_at_ref,
            )
        else:
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

        # Post the verdict as an inline reply so it is visible on the
        # original comment, not just in the job log. Reply while the
        # discussion is still open, before resolving.
        if not args.dry_run:
            try:
                if platform == "github":
                    github_backend.reply_to_review_comment(
                        gh_api, gh_repo, args.iid, gh_token, disc["comment_id"],
                        build_verdict_reply(verdict, reason, args.sha),
                    )
                else:
                    post_discussion_reply(
                        api_url, project_id, args.iid, disc_id, gitlab_token,
                        build_verdict_reply(verdict, reason, args.sha),
                    )
                print(f"  Replied to discussion {disc_id}", file=sys.stderr)
            except Exception as e:
                reply_failed += 1
                print(f"  ERROR: Failed to reply to discussion {disc_id}: {e}",
                      file=sys.stderr)

        if verdict == "ADDRESSED":
            addressed += 1
            if not args.dry_run:
                try:
                    if platform == "github":
                        github_backend.resolve_review_thread(
                            gh_api, gh_token, disc_id)
                    else:
                        resolve_discussion(api_url, project_id, args.iid, disc_id, gitlab_token)
                    print(f"  Resolved discussion {disc_id}", file=sys.stderr)
                except Exception as e:
                    resolve_failed += 1
                    print(f"  ERROR: Failed to resolve discussion {disc_id}: {e}", file=sys.stderr)
        else:
            not_addressed += 1

    if discussions:
        print(f"Verification complete: {addressed} addressed, {not_addressed} still open", file=sys.stderr)
    if reply_failed:
        print(f"ERROR: {reply_failed} discussion replies could not be posted", file=sys.stderr)
    if resolve_failed:
        print(f"ERROR: {resolve_failed} discussions could not be resolved", file=sys.stderr)

    # Rollup currency (CHOMP-59): this run re-checked the open findings
    # against args.sha, so the consolidated comment's last-verified line
    # now names it. Only when at least one discussion was verified — a
    # run that verified nothing must not claim a re-check happened.
    currency_failed = False
    if discussions and not args.dry_run:
        currency_failed = update_rollup_currency(
            platform, api_url, project_id, args.iid, args.sha,
            gitlab_token, gh_api, gh_repo, gh_token)

    conformance_post_failed = verify_conformance_recheck(
        platform, api_url, project_id, args.iid, args.sha, diff,
        gitlab_token, gh_api, gh_repo, gh_token,
        api_key, model, args.dry_run,
    )
    if conformance_post_failed:
        print("ERROR: conformance re-check could not be posted", file=sys.stderr)
    if reply_failed or resolve_failed or conformance_post_failed \
            or currency_failed:
        sys.exit(1)


if __name__ == "__main__":
    main()
