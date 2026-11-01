#!/usr/bin/env python3
"""
Chomp master reviewer: dedups findings via LLM, posts ONE consolidated review.

Usage:
    python3 .forge/master.py --iid 133 --sha <sha> \
        --pr-agent pr-agent-findings.md \
        --thermos-a thermos-correctness.json \
        --thermos-b thermos-structure.json \
        --diff mr.diff

Environment:
    OPENROUTER_API_KEY: API key for OpenRouter
    FORGE_MODEL: model to use (default: openrouter/meta/muse-spark-1.3-contributor)
    FORGE_PLATFORM: "github" or "gitlab" (default: github under GitHub
        Actions, gitlab otherwise)
    GitLab: PR_AGENT_GITLAB_TOKEN, CI_API_V4_URL, CI_PROJECT_ID
    GitHub: GITHUB_TOKEN (Forge app installation token), GITHUB_API_URL,
        GITHUB_REPOSITORY — --iid carries the PR number
"""
import argparse
import bisect
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

CONSOLIDATED_MARKER = "chomp:consolidated-review"
# Reviews posted before the Chomp rename carry the legacy marker. Anything
# that RECOGNIZES an existing review (duplicate suppression, verify) must
# accept both; anything that WRITES a review uses CONSOLIDATED_MARKER.
LEGACY_CONSOLIDATED_MARKER = "forge:consolidated-review"
CONSOLIDATED_MARKERS = (CONSOLIDATED_MARKER, LEGACY_CONSOLIDATED_MARKER)


class DuplicateReviewPost(Exception):
    """A consolidated review already exists for this PR/MR.

    Carries the existing comment/note id in existing_id.
    """
    def __init__(self, existing_id):
        super().__init__("consolidated review already exists: %s"
                         % (existing_id,))
        self.existing_id = existing_id

class SupersededReviewPost(Exception):
    """The PR/MR head moved past this run's SHA before it could post.

    Carries the live head SHA in live_sha (CHOMP-58).
    """
    def __init__(self, live_sha):
        super().__init__("head superseded by %s" % (live_sha,))
        self.live_sha = live_sha


def live_head_supersedes(sha, live_sha):
    """True when the live head has moved past the SHA this run targets.

    Queued runs execute minutes after their trigger, so the review they
    computed can be stale at post time. An unknown live head (None)
    never supersedes.
    """
    return bool(sha) and bool(live_sha) and sha != live_sha


def fetch_live_head_sha_gitlab(api_url, project_id, iid, token):
    """Return the MR's live head SHA via the GitLab API, or None.

    gitlab_request exits on HTTP errors; that SystemExit is the failure
    signal here, because an unreadable head must not block a review.
    """
    try:
        mr = gitlab_request(
            "GET",
            f"{api_url}/projects/{project_id}/merge_requests/{iid}",
            token)
    except SystemExit:
        return None
    return (mr or {}).get("sha") or None

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

def substitute_placeholders(template, mapping):
    """Single-pass placeholder substitution. Unlike chained .replace(),
    substituted values are never re-scanned, so findings containing a
    placeholder string cannot corrupt the prompt."""
    pattern = re.compile("|".join(re.escape(k) for k in mapping.keys()))
    return pattern.sub(lambda m: mapping[m.group(0)], template)

HUNK_RE = re.compile(r"^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@")

def _strip_git_prefix(token, prefix):
    if len(token) >= 2 and token.startswith('"') and token.endswith('"'):
        token = token[1:-1]
    if token.startswith(prefix):
        return token[len(prefix):]
    return token

def parse_diff_line_map(diff_text):
    """Parse a unified diff into per-file valid diff-line maps.

    GitLab only accepts inline-discussion positions on lines that are part
    of the MR diff: added/context lines (addressed via new_line) and
    deleted lines (addressed via old_line). Lines outside hunks are invalid
    and make the API reject the discussion with a 400.

    Returns (line_map, file_order) where line_map maps new_path to
    {"old_path", "new_path", "new_lines": set, "old_lines": set,
     "new_to_old": {new: old}} plus an "aliases" sub-dict mapping old_path
    to new_path for renames. file_order lists new_paths in diff order.
    """
    line_map = {}
    aliases = {}
    file_order = []
    cur = None
    old_line = new_line = None
    for raw in diff_text.splitlines():
        if raw.startswith("diff --git "):
            parts = raw[len("diff --git "):].split()
            if len(parts) < 2:
                cur = None
                old_line = new_line = None
                continue
            old_path = _strip_git_prefix(parts[-2], "a/")
            new_path = _strip_git_prefix(parts[-1], "b/")
            cur = {"old_path": old_path, "new_path": new_path,
                   "new_lines": set(), "old_lines": set(), "new_to_old": {}}
            line_map[new_path] = cur
            if old_path != new_path:
                aliases[old_path] = new_path
            file_order.append(new_path)
            old_line = new_line = None
            continue
        if cur is None:
            continue
        if raw.startswith("@@ "):
            m = HUNK_RE.match(raw)
            if not m:
                old_line = new_line = None
                continue
            old_line = int(m.group(1))
            new_line = int(m.group(3))
            continue
        if old_line is None or new_line is None:
            continue  # file headers (---/+++/index/...) outside hunks
        if raw.startswith("+"):
            cur["new_lines"].add(new_line)
            new_line += 1
        elif raw.startswith("-"):
            cur["old_lines"].add(old_line)
            old_line += 1
        elif raw.startswith(" "):
            cur["new_lines"].add(new_line)
            cur["old_lines"].add(old_line)
            cur["new_to_old"][new_line] = old_line
            old_line += 1
            new_line += 1
        # else: "\ No newline at end of file", "Binary files ... differ", etc.
    return {"files": line_map, "aliases": aliases}, file_order

def _nearest_line(sorted_lines, target):
    i = bisect.bisect_left(sorted_lines, target)
    cands = []
    if i < len(sorted_lines):
        cands.append(sorted_lines[i])
    if i > 0:
        cands.append(sorted_lines[i - 1])
    return min(cands, key=lambda c: (abs(c - target), c))

def _safe_int(value, default=0):
    try:
        return int(value)
    except (TypeError, ValueError):
        return default

def build_inline_attempts(parsed, file_order, file_path, line_start, line_end):
    """Build ordered inline-position attempts for one finding.

    Returns (attempts, meta). Each attempt is {"new_path", "old_path",
    "new_line" or None, "old_line" or None}. Attempts are guaranteed
    non-empty whenever the diff contains any valid diff line, so with the
    retry loop below every finding lands as an inline discussion instead
    of being silently dropped. meta carries "relocated" (True when the
    finding's own file/line was unusable) and the requested location.
    """
    requested = {"file": file_path, "line_start": line_start, "line_end": line_end}
    files = parsed["files"]
    entry = files.get(file_path)
    if entry is None and file_path in parsed["aliases"]:
        entry = files.get(parsed["aliases"][file_path])
    if entry is None:
        # Tolerate a stray a-/b- prefix from the model.
        for prefix in ("a/", "b/"):
            if file_path.startswith(prefix) and file_path[len(prefix):] in files:
                entry = files[file_path[len(prefix):]]
                break

    attempts = []
    if entry is not None and entry["new_lines"]:
        new_sorted = sorted(entry["new_lines"])
        in_range = [n for n in range(line_start, line_end + 1) if n in entry["new_lines"]]
        if in_range:
            primary = in_range[0]
        else:
            primary = _nearest_line(new_sorted, line_start)
        ordered = [primary] + [n for n in sorted(new_sorted, key=lambda c: (abs(c - line_start), c)) if n != primary]
        # Cap retries: primary + closest alternates + file's first diff line.
        seen = set()
        for n in ordered + ([new_sorted[0]] if new_sorted[0] != primary else []):
            if n not in seen:
                seen.add(n)
                attempts.append({"new_path": entry["new_path"], "old_path": entry["old_path"],
                                 "new_line": n, "old_line": None})
            if len(attempts) >= 5:
                break
        relocated = (file_path != entry["new_path"]) or (line_start not in entry["new_lines"] and not in_range)
        return attempts, {"relocated": relocated, "requested": requested}
    if entry is not None and entry["old_lines"]:
        # Deletion-only file: nothing survives on the new side, anchor via old_line.
        old_sorted = sorted(entry["old_lines"])
        in_range = [n for n in range(line_start, line_end + 1) if n in entry["old_lines"]]
        primary = in_range[0] if in_range else _nearest_line(old_sorted, line_start)
        seen = set()
        for n in [primary] + [c for c in sorted(old_sorted, key=lambda c: (abs(c - line_start), c)) if c != primary]:
            if n not in seen:
                seen.add(n)
                attempts.append({"new_path": entry["new_path"], "old_path": entry["old_path"],
                                 "new_line": None, "old_line": n})
            if len(attempts) >= 5:
                break
        return attempts, {"relocated": line_start not in entry["old_lines"] and not in_range, "requested": requested}
    # Unknown file (or file with no diff lines): fall back to the first
    # valid diff lines elsewhere so the finding still lands inline. The
    # body notes the originally flagged location.
    for fallback_path in file_order[:3]:
        fb = files[fallback_path]
        if fb["new_lines"]:
            n = sorted(fb["new_lines"])[0]
            attempts.append({"new_path": fb["new_path"], "old_path": fb["old_path"],
                             "new_line": n, "old_line": None})
    return attempts, {"relocated": True, "requested": requested}

def post_inline_attempt(api_url, project_id, iid, token, body, attempt,
                        base_sha, head_sha, start_sha):
    """Post one inline-discussion attempt. Returns the parsed discussion."""
    params = {
        "body": body,
        "position[position_type]": "text",
        "position[base_sha]": base_sha,
        "position[head_sha]": head_sha,
        "position[start_sha]": start_sha,
        "position[new_path]": attempt["new_path"],
    }
    if attempt.get("new_line") is not None:
        params["position[new_line]"] = str(attempt["new_line"])
    if attempt.get("old_line") is not None:
        params["position[old_path]"] = attempt["old_path"]
        params["position[old_line]"] = str(attempt["old_line"])
    data = urllib.parse.urlencode(params).encode()
    req = urllib.request.Request(
        f"{api_url}/projects/{project_id}/merge_requests/{iid}/discussions",
        data=data,
        method="POST",
        headers={
            "PRIVATE-TOKEN": token,
            "Content-Type": "application/x-www-form-urlencoded",
        },
    )
    with urllib.request.urlopen(req, timeout=30) as resp:
        return json.load(resp)

def post_inline_with_fallback(post_fn, body, attempts):
    """Try each attempt in order. Returns (result, index of winning attempt).

    Raises RuntimeError if every attempt fails.
    """
    last_err = None
    for i, attempt in enumerate(attempts):
        try:
            return post_fn(body, attempt), i
        except Exception as e:
            last_err = e
    raise RuntimeError(f"all {len(attempts)} inline positions rejected: {last_err}")

def post_inline_findings(findings, diff, sha, post_fn):
    """Post one inline comment per finding via post_fn(body, attempt).

    post_fn returns the created object (a dict with an "id"). Positions
    come from the diff line map: both GitLab and GitHub reject comments
    on lines outside the diff, so every finding is resolved to the
    nearest valid diff line (with retries) instead of trusting the
    model's line number blindly. Returns (recorded, failed) in the
    forge-recorded.json shape; unpostable findings are returned in
    failed, never silently dropped.
    """
    parsed_map, file_order = parse_diff_line_map(diff)
    n_valid = sum(len(e["new_lines"]) + len(e["old_lines"]) for e in parsed_map["files"].values())
    print(f"Diff covers {len(file_order)} files with {n_valid} positionable lines", file=sys.stderr)

    recorded = []
    failed = []
    for f in findings:
        file_path = (f.get("file", "") or "").strip()
        line_start = _safe_int(f.get("line_start", 0))
        line_end = _safe_int(f.get("line_end", line_start), line_start)
        if line_end < line_start:
            line_start, line_end = line_end, line_start

        attempts, meta = build_inline_attempts(parsed_map, file_order, file_path, line_start, line_end)
        inline_body = build_inline(sha, f, meta)
        if not attempts:
            print(f"  ERROR: No positionable lines in diff; cannot post inline for "
                  f"{file_path}:{line_start}-{line_end}", file=sys.stderr)
            failed.append({"file": file_path, "line_start": line_start,
                           "line_end": line_end, "error": "empty diff"})
            continue

        anchor = attempts[0]
        anchor_desc = (f"{anchor['new_path']}:{anchor['new_line']}"
                       if anchor.get("new_line") is not None
                       else f"{anchor['old_path']}:{anchor['old_line']} (old)")
        print(f"Posting inline for {file_path}:{line_start}-{line_end} "
              f"(resolved {anchor_desc}, {len(attempts)} attempts)...", file=sys.stderr)

        try:
            discussion, won = post_inline_with_fallback(post_fn, inline_body, attempts)
            used = attempts[won]
            disc_id = discussion.get("id")
            print(f"  Discussion {disc_id} posted at "
                  f"{used['new_path']}:{used.get('new_line') or used.get('old_line')}", file=sys.stderr)
            entry = {
                "id": disc_id,
                "file": file_path,
                "line_start": line_start,
                "line_end": line_end,
                "source": "+".join(f.get("sources", ["forge"])),
                "resolved_path": used["new_path"],
                "resolved_line": used.get("new_line") if used.get("new_line") is not None else used.get("old_line"),
                "anchor": "new" if used.get("new_line") is not None else "old",
                "relocated": meta["relocated"] or won > 0,
            }
            if discussion.get("html_url"):
                entry["url"] = discussion["html_url"]
            recorded.append(entry)
        except Exception as e:
            print(f"  ERROR: Failed to post inline for {file_path}:{line_start}: {e}", file=sys.stderr)
            failed.append({"file": file_path, "line_start": line_start,
                           "line_end": line_end, "error": str(e)})
            # Continue with other findings
    return recorded, failed

def _load_ticket_artifact(ticket_path, text):
    """Parse the ticket.json artifact for ticket_path (or None)."""
    if ticket_path.endswith(".json"):
        try:
            return json.loads(text)
        except ValueError:
            return None
    sibling = os.path.join(os.path.dirname(os.path.abspath(ticket_path)),
                           "ticket.json")
    try:
        with open(sibling) as f:
            return json.load(f)
    except (OSError, ValueError) as e:
        print(f"WARNING: cannot read ticket JSON {sibling}: {e}",
              file=sys.stderr)
        return None


def load_ticket(ticket_path):
    """Load ticket context for the prompt and recorded keys.

    Returns (ticket_text, keys_or_None). keys is None when no --ticket
    was passed (historical behavior); otherwise the resolved key list
    (possibly empty, which notes the no-linked-ticket outcome).
    Ticket text is untrusted quoted data, never instructions.
    """
    if not ticket_path:
        return "(no linked ticket)", None
    text = "(no linked ticket)"
    try:
        with open(ticket_path) as f:
            text = f.read()
    except OSError as e:
        print(f"WARNING: cannot read ticket file {ticket_path}: {e}",
              file=sys.stderr)
    artifact = _load_ticket_artifact(ticket_path, text)
    keys = []
    if isinstance(artifact, dict):
        keys = [t.get("key") for t in artifact.get("tickets", []) or []
                if isinstance(t, dict) and t.get("key")]
    return text, keys


def load_ticket_details(ticket_path):
    """Load per-ticket display details for the conformance section.

    Returns a list of {"key", "title", "url"} dicts (possibly empty).
    Empty when no --ticket was passed or the artifact is unreadable.
    """
    if not ticket_path:
        return []
    text = "(no linked ticket)"
    try:
        with open(ticket_path) as f:
            text = f.read()
    except OSError as e:
        print(f"WARNING: cannot read ticket file {ticket_path}: {e}",
              file=sys.stderr)
    artifact = _load_ticket_artifact(ticket_path, text)
    details = []
    if isinstance(artifact, dict):
        for t in artifact.get("tickets", []) or []:
            if isinstance(t, dict) and t.get("key"):
                details.append({
                    "key": t.get("key"),
                    "title": t.get("title") or "",
                    "url": t.get("url") or "",
                })
    return details


def find_marker_note_gitlab(api_url, project_id, iid, token, marker):
    """Return the first MR note containing marker, or None.

    Same first-100-notes scope as the template's review-once check at
    master time; a marker beyond the first page is not seen here.
    """
    notes = gitlab_request(
        "GET",
        f"{api_url}/projects/{project_id}/merge_requests/{iid}/notes?per_page=100",
        token)
    for note in notes or []:
        if marker in (note.get("body") or ""):
            return note
    return None


def synthesis_outputs(decision_log):
    """Extract the rollup summary and conformance object from a chomp
    decision log (2.0 post-only path).

    The synthesizer records both as log entries (FRG-26): the first
    kind=synthesis/event=summary detail is the rollup summary, and the
    first kind=synthesis_conformance object is the conformance context.
    Absent entries degrade to an empty summary and no conformance —
    the rollup posts what it has, never invented text.
    """
    summary = ""
    conformance = None
    if not isinstance(decision_log, dict):
        return summary, conformance
    for entry in decision_log.get("entries") or []:
        if not isinstance(entry, dict):
            continue
        if entry.get("kind") == "synthesis" and entry.get("event") == "summary" and not summary:
            detail = entry.get("detail")
            if isinstance(detail, str):
                summary = detail
        if entry.get("kind") == "synthesis_conformance" and conformance is None:
            candidate = entry.get("conformance")
            if isinstance(candidate, dict):
                conformance = candidate
    return summary, conformance


def post_review_gitlab(api_url, project_id, iid, token, sha, summary, findings, diff,
                       tickets=None, conformance=None, ticket_details=None):
    """Post the consolidated review to GitLab: rollup note + inline
    discussions. Returns (recorded, failed)."""
    # Get project path for blob links
    project = gitlab_request("GET", f"{api_url}/projects/{project_id}", token)
    project_path = project.get("path_with_namespace", "browntown.development-group/stratum")

    # Get MR diff_refs for positioning
    mr = gitlab_request("GET", f"{api_url}/projects/{project_id}/merge_requests/{iid}", token)
    diff_refs = mr.get("diff_refs", {})
    base_sha = diff_refs.get("base_sha", "")
    head_sha = diff_refs.get("head_sha", sha)
    start_sha = diff_refs.get("start_sha", "")

    # Post rollup first
    rollup_body = build_rollup(sha, summary, findings, project_path,
                               tickets=tickets, conformance=conformance,
                               ticket_details=ticket_details)
    existing = None
    for marker in CONSOLIDATED_MARKERS:
        existing = find_marker_note_gitlab(
            api_url, project_id, iid, token, marker)
        if existing is not None:
            break
    if existing is not None:
        raise DuplicateReviewPost(existing.get("id"))
    print("Posting rollup...", file=sys.stderr)
    note = gitlab_request(
        "POST",
        f"{api_url}/projects/{project_id}/merge_requests/{iid}/notes",
        token,
        {"body": rollup_body}
    )
    print(f"Rollup posted: note {note.get('id')}", file=sys.stderr)

    def post_fn(body, attempt):
        return post_inline_attempt(api_url, project_id, iid, token, body, attempt,
                                   base_sha, head_sha, start_sha)

    return post_inline_findings(findings, diff, sha, post_fn)

def post_review_github(api_url, repo, number, token, sha, summary, findings, diff,
                       tickets=None, conformance=None, ticket_details=None):
    """Post the consolidated review to GitHub: rollup as a PR conversation
    comment + inline review comments. Returns (recorded, failed)."""
    rollup_body = build_rollup(sha, summary, findings, repo,
                               blob_base=f"https://github.com/{repo}/blob",
                               tickets=tickets, conformance=conformance,
                               ticket_details=ticket_details)
    existing = None
    for marker in CONSOLIDATED_MARKERS:
        existing = github_backend.find_marker_comment(
            api_url, repo, number, token, marker)
        if existing is not None:
            break
    if existing is not None:
        raise DuplicateReviewPost(existing.get("id"))
    print("Posting rollup...", file=sys.stderr)
    comment = github_backend.post_issue_comment(api_url, repo, number, token, rollup_body)
    print(f"Rollup posted: comment {comment.get('id')}", file=sys.stderr)

    def post_fn(body, attempt):
        return github_backend.post_review_comment_attempt(
            api_url, repo, number, token, body, sha, attempt)

    return post_inline_findings(findings, diff, sha, post_fn)

def gitlab_request(method, url, token, data=None):
    """Make a GitLab API request."""
    headers = {"PRIVATE-TOKEN": token}
    if data is not None:
        data = json.dumps(data).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return json.load(resp)
    except urllib.error.HTTPError as e:
        body = e.read().decode()[:1000]
        print(f"ERROR: GitLab API {method} {url} failed: {e.code} {body}", file=sys.stderr)
        sys.exit(1)

def severity_badge(severity):
    return {
        "Critical": "🔴 Critical",
        "High": "🟣 High",
        "Medium": "🟡 Medium",
        "Low": "⚪ Low",
    }.get(severity, "⚪ Low")

def category_tag(category):
    return {
        "Bug": "🐞 Bug",
        "Security": "🔒 Security",
        "Correctness": "≡ Correctness",
        "Performance": "⚡ Performance",
        "Maintainability": "🏗️ Maintainability",
        "Test gap": "🧪 Test gap",
    }.get(category, category)

def _quote_block_lines(text):
    """Prefix every line with the blockquote marker so multi-line text
    stays inside the surrounding quote/details structure."""
    lines = str(text).split("\n") if str(text) else [""]
    return ["> " + line if line.strip() else ">" for line in lines]


def build_agent_prompt(finding):
    """Build the Agent Prompt block."""
    focus_areas = "\n".join(f"- {area}" for area in finding.get("fix_focus_areas", []))
    return f"""<details>
<summary><strong>Agent Prompt</strong></summary>

```
## Issue description
{finding.get('description', '')}

## Fix Focus Areas
{focus_areas}

## Recommended Fix
{finding.get('recommended_fix', '')}
```

<code>ⓘ Copy this prompt and use it to remediate the issue with your preferred AI generation tools</code>
</details>"""

def ticket_note_lines(tickets):
    """Render the linked-ticket note for the rollup.

    tickets is None when no --ticket artifact was passed (no note at
    all); [] notes the no-linked-ticket outcome; otherwise the keys.
    """
    if tickets is None:
        return []
    if tickets:
        return ["", "Linked tickets: " + ", ".join(tickets)]
    return ["", "_No linked ticket found for this MR._"]


CONFORMANCE_STATUSES = ("addressed", "not evident", "contradicted")


def conformance_section_lines(tickets, ticket_details, conformance):
    """Render the "## Ticket conformance" section lines, or [] for none.

    Deterministic rule: conformance None means the LLM emitted none —
    render no section (do not invent), except when tickets == [] (--ticket
    was given but resolved nothing), which renders overall
    "no linked ticket". tickets None (no --ticket) renders no section.
    """
    if conformance is None:
        if tickets is not None and len(tickets) == 0:
            conformance = {"overall": "no linked ticket"}
        else:
            return []
    if not isinstance(conformance, dict):
        return []
    lines = ["## Ticket conformance", ""]
    if tickets:
        by_key = {}
        for d in ticket_details or []:
            if isinstance(d, dict) and d.get("key"):
                by_key[d["key"]] = d
        parts = []
        for key in tickets:
            detail = by_key.get(key, {})
            label = str(key)
            title = (detail.get("title") or "").strip()
            url = (detail.get("url") or "").strip()
            if title:
                label += ' "%s"' % title
            if url:
                label += " (%s)" % url
            parts.append(label)
        lines.append("Resolved ticket(s): " + "; ".join(parts))
        lines.append("")
    overall = conformance.get("overall", "no linked ticket")
    lines.append("Overall: %s" % overall)
    lines.append("")
    note = (conformance.get("note") or "")
    if isinstance(note, str) and note.strip():
        lines.append(note.strip())
        lines.append("")
    for criterion in conformance.get("criteria") or []:
        if not isinstance(criterion, dict):
            continue
        status = criterion.get("status", "not evident")
        text = criterion.get("criterion", "")
        evidence = (criterion.get("evidence") or "")
        line = "- [%s] %s" % (status, text)
        if isinstance(evidence, str) and evidence.strip():
            line += " (evidence: %s)" % evidence.strip()
        lines.append(line)
    if conformance.get("criteria"):
        lines.append("")
    unrelated = conformance.get("unrelated_changes") or []
    if unrelated:
        lines.append("Unrelated changes:")
        lines.append("")
        for item in unrelated:
            lines.append("- %s" % item)
        lines.append("")
    return lines


def build_rollup(sha, summary, findings, project_path, blob_base=None,
                 tickets=None, conformance=None, ticket_details=None):
    """Build the rollup comment markdown.

    blob_base overrides the blob-link prefix (GitHub passes
    "https://github.com/<owner>/<repo>/blob"); the default keeps the
    historical GitLab URL shape. tickets is None by default, which keeps
    the historical output byte-identical; pass [] or [keys...] to note
    the linked-ticket outcome. conformance is the parsed LLM conformance
    object (or None); ticket_details carries {"key","title","url"} dicts
    for the tickets list. All new params default to no-section output.
    """
    # Count by severity
    counts = {"Critical": 0, "High": 0, "Medium": 0, "Low": 0}
    for f in findings:
        sev = f.get("severity", "Low")
        if sev in counts:
            counts[sev] += 1

    lines = [
        f"<!-- {CONSOLIDATED_MARKER} sha:{sha} -->",
        "## PR Reviewer Guide 🔍",
        "",
        summary,
        "",
        f"<code>🔴 Critical ({counts['Critical']})</code> <code>🟣 High ({counts['High']})</code> <code>🟡 Medium ({counts['Medium']})</code> <code>⚪ Low ({counts['Low']})</code>",
        "",
        "<br/>",
        "",
    ]

    # Sort by severity
    order = {"Critical": 0, "High": 1, "Medium": 2, "Low": 3}
    sorted_findings = sorted(findings, key=lambda f: order.get(f.get("severity", "Low"), 3))

    for i, f in enumerate(sorted_findings, 1):
        badge = severity_badge(f.get("severity", "Low"))
        cat = category_tag(f.get("category", ""))
        title = f.get("title", "Untitled")
        file = f.get("file", "")
        ls = f.get("line_start", 0)
        le = f.get("line_end", 0)

        # Blob link
        if blob_base:
            blob_url = f"{blob_base}/{sha}/{file}#L{ls}-L{le}"
        else:
            blob_url = f"https://gitlab.com/{project_path}/-/blob/{sha}/{file}#L{ls}-{le}"

        lines.extend([
            f"{badge}",
            "",
            "<details>",
            f"<summary>{i}. {title} <code>{cat}</code></summary>",
            "",
            "<br/>",
            "",
            "> <details open>",
            "> <summary>Description</summary>",
            "> <br/>",
            *_quote_block_lines(f.get('description', '')),
            "> </details>",
            "",
            "> <details>",
            "> <summary>Code</summary>",
            "> <br/>",
            f"> <code>[{file}[L{ls}-{le}]]({blob_url})</code>",
            "> </details>",
            "",
            "> <details>",
            "> <summary>Evidence</summary>",
            "> <br/>",
            *_quote_block_lines(f.get('evidence', '')),
            "> </details>",
            "",
            build_agent_prompt(f),
            "",
            "</details>",
            "",
        ])

    lines.extend(ticket_note_lines(tickets))
    lines.extend(conformance_section_lines(tickets, ticket_details,
                                           conformance))

    return "\n".join(lines)

def build_inline(sha, finding, meta=None):
    """Build the inline discussion body.

    When the comment had to be anchored to a different line than the one
    flagged (nearest valid diff line), an invisible HTML comment records
    the originally requested location for debugging.
    """
    badge = severity_badge(finding.get("severity", "Low"))
    cat = category_tag(finding.get("category", ""))
    title = finding.get("title", "Untitled")
    anchor_note = ""
    if meta and meta.get("relocated"):
        req = meta.get("requested", {})
        anchor_note = (
            f"\n<!-- chomp:anchor originally flagged "
            f"{req.get('file')}[L{req.get('line_start')}-{req.get('line_end')}] "
            f"— anchored to nearest valid diff line -->"
        )
    return f"""<!-- {CONSOLIDATED_MARKER} sha:{sha} -->{anchor_note}
{badge} — {title} <code>{cat}</code>

{finding.get('description', '')}

{build_agent_prompt(finding)}"""

def write_recorded_json(iid, sha, platform, recorded, failed, tickets,
                        conformance, duplicate_suppressed=False,
                        superseded_by=None):
    """Write forge-recorded.json for the verify job (write-only context)."""
    payload = {
        "iid": iid,
        "sha": sha,
        "platform": platform,
        "discussions": recorded,
        "failed": failed,
        "tickets": tickets if tickets is not None else [],
        "conformance": conformance,
    }
    if duplicate_suppressed:
        payload["duplicate_suppressed"] = True
    if superseded_by:
        payload["superseded_by"] = superseded_by
    with open("forge-recorded.json", "w") as f:
        json.dump(payload, f, indent=2)


def post_and_record(platform, args, summary, findings, diff, ticket_keys,
                    conformance, ticket_details, api_url, project_id, gitlab_token):
    """Shared publish tail: post through the platform backend, then write
    forge-recorded.json in its current shape for the verify path.

    Both review paths end here — the 1.x LLM dedup and the 2.0 chomp
    post-only mode. Posting functions are unchanged; this only factors
    the common call, record, and count reporting.
    """
    # Live-head guard (CHOMP-58): this run may have waited in the
    # workflow queue while head moved. Posting now would pin a review
    # computed for a superseded SHA onto the current tip. Post nothing;
    # the queued run for the live head finds no marker and reviews it.
    if platform == "github":
        gh_api0, gh_repo0, gh_token0 = github_backend.gh_env()
        live_sha = github_backend.fetch_live_head_sha(
            gh_api0, gh_repo0, args.iid, gh_token0)
    else:
        live_sha = fetch_live_head_sha_gitlab(
            api_url, project_id, args.iid, gitlab_token)
    if live_head_supersedes(args.sha, live_sha):
        print(f"Superseded: this run targets {args.sha[:8]} but live "
              f"head is {live_sha[:8]}; posting nothing, the run for "
              f"the live head will review.", file=sys.stderr)
        write_recorded_json(args.iid, args.sha, platform, [], [],
                            ticket_keys, conformance,
                            superseded_by=live_sha)
        print("Recorded 0 discussions to forge-recorded.json",
              file=sys.stderr)
        return

    try:
        if platform == "github":
            gh_api, gh_repo, gh_token = github_backend.gh_env()
            recorded, failed = post_review_github(
                gh_api, gh_repo, args.iid, gh_token, args.sha, summary, findings, diff,
                tickets=ticket_keys, conformance=conformance,
                ticket_details=ticket_details)
        else:
            recorded, failed = post_review_gitlab(
                api_url, project_id, args.iid, gitlab_token, args.sha, summary, findings, diff,
                tickets=ticket_keys, conformance=conformance,
                ticket_details=ticket_details)
    except DuplicateReviewPost as e:
        # No-op, not a verify invocation: a concurrent run posted first.
        # Verify routing is the workflow re-check's job; just record the
        # suppression and exit 0 so the queued run stays green.
        print(f"Duplicate consolidated review suppressed (existing id "
              f"{e.existing_id}); this run posted nothing, a later run "
              f"will verify.", file=sys.stderr)
        write_recorded_json(args.iid, args.sha, platform, [], [], ticket_keys,
                            conformance, duplicate_suppressed=True)
        print("Recorded 0 discussions to forge-recorded.json", file=sys.stderr)
        return

    # Write recorded discussions for the verify job
    write_recorded_json(args.iid, args.sha, platform, recorded, failed,
                        ticket_keys, conformance)
    print(f"Recorded {len(recorded)} discussions to forge-recorded.json", file=sys.stderr)
    if failed:
        print(f"ERROR: {len(failed)} findings could not be posted inline: {failed}", file=sys.stderr)

    # Summary counts
    counts = {"Critical": 0, "High": 0, "Medium": 0, "Low": 0}
    for f in findings:
        sev = f.get("severity", "Low")
        if sev in counts:
            counts[sev] += 1
    print(f"Posted consolidated review: {counts}", file=sys.stderr)


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iid", required=True, help="MR internal ID (GitHub: PR number)")
    parser.add_argument("--sha", required=True, help="MR head SHA")
    parser.add_argument("--pr-agent", default=None, help="Path to PR-Agent findings markdown (1.x path)")
    parser.add_argument("--thermos-a", default=None, help="Path to thermos-a JSON (1.x path)")
    parser.add_argument("--thermos-b", default=None, help="Path to thermos-b JSON (1.x path)")
    parser.add_argument("--final-findings", default=None,
                        help="Path to chomp final findings JSON (2.0 post-only path)")
    parser.add_argument("--decisions", default=None,
                        help="Path to chomp decision log JSON (2.0 post-only path)")
    parser.add_argument("--diff", required=True, help="Path to diff file")
    parser.add_argument("--prompt-dir", default=".forge/prompts")
    parser.add_argument("--ticket", default=None,
                        help="Path to ticket.md artifact (quoted context)")
    parser.add_argument("--dry-run", action="store_true", help="Don't post, just print")
    args = parser.parse_args()

    platform = detect_platform()

    api_key = os.environ.get("OPENROUTER_API_KEY")
    gitlab_token = os.environ.get("PR_AGENT_GITLAB_TOKEN")
    api_url = os.environ.get("CI_API_V4_URL", "https://gitlab.com/api/v4")
    project_id = os.environ.get("CI_PROJECT_ID", "83700143")

    if not api_key:
        print("ERROR: OPENROUTER_API_KEY not set", file=sys.stderr)
        sys.exit(1)
    if not args.dry_run:
        if platform == "github":
            if not os.environ.get("GITHUB_TOKEN"):
                print("ERROR: GITHUB_TOKEN not set", file=sys.stderr)
                sys.exit(1)
            if not os.environ.get("GITHUB_REPOSITORY"):
                print("ERROR: GITHUB_REPOSITORY not set", file=sys.stderr)
                sys.exit(1)
        elif not gitlab_token:
            print("ERROR: PR_AGENT_GITLAB_TOKEN not set", file=sys.stderr)
            sys.exit(1)

    model = get_model()

    # 2.0 post-only path: chomp already synthesized. The LLM half is
    # skipped entirely; findings, summary, and conformance come from
    # chomp's outputs and post through the unchanged posting half.
    post_only = args.final_findings is not None or args.decisions is not None
    if post_only:
        if not (args.final_findings and args.decisions):
            print("ERROR: --final-findings and --decisions must be given together",
                  file=sys.stderr)
            sys.exit(1)
        with open(args.diff) as f:
            diff = f.read()
        try:
            with open(args.final_findings) as f:
                findings = json.load(f)
        except (OSError, ValueError) as e:
            print(f"ERROR: cannot load final findings: {e}", file=sys.stderr)
            sys.exit(1)
        if not isinstance(findings, list):
            print("ERROR: final findings must be a JSON array", file=sys.stderr)
            sys.exit(1)
        try:
            with open(args.decisions) as f:
                decision_log = json.load(f)
        except (OSError, ValueError) as e:
            print(f"ERROR: cannot load decision log: {e}", file=sys.stderr)
            sys.exit(1)
        summary, conformance = synthesis_outputs(decision_log)
        ticket_text, ticket_keys = load_ticket(args.ticket)
        ticket_details = load_ticket_details(args.ticket)
        print(f"Posting {len(findings)} chomp findings", file=sys.stderr)
        if args.dry_run:
            print(json.dumps({"summary": summary, "findings": findings,
                              "conformance": conformance}, indent=2))
            return
        post_and_record(platform, args, summary, findings, diff, ticket_keys,
                        conformance, ticket_details, api_url, project_id, gitlab_token)
        return

    if not (args.pr_agent and args.thermos_a and args.thermos_b):
        print("ERROR: 1.x path requires --pr-agent, --thermos-a, and --thermos-b",
              file=sys.stderr)
        sys.exit(1)

    # Load inputs
    with open(args.pr_agent) as f:
        pr_agent_md = f.read()
    with open(args.thermos_a) as f:
        thermos_a = f.read()
    with open(args.thermos_b) as f:
        thermos_b = f.read()
    with open(args.diff) as f:
        diff = f.read()

    # Validate PR-Agent output
    if "## PR Reviewer Guide" not in pr_agent_md:
        print("ERROR: PR-Agent findings lack the Guide header; holding publication.", file=sys.stderr)
        sys.exit(1)

    # Load master prompt
    with open(os.path.join(args.prompt_dir, "master-dedup.md")) as f:
        template = f.read()

    # Ticket context is untrusted quoted data, never instructions.
    ticket_text, ticket_keys = load_ticket(args.ticket)
    ticket_details = load_ticket_details(args.ticket)

    prompt = substitute_placeholders(template, {
        "{pr_agent_md}": pr_agent_md,
        "{thermos_a_json}": thermos_a,
        "{thermos_b_json}": thermos_b,
        "{diff}": diff,
        "{ticket}": ticket_text,
    })

    print("Calling OpenRouter for master dedup...", file=sys.stderr)
    response = call_openrouter(api_key, model, prompt)

    # Clean response
    response = response.strip()
    if response.startswith("```"):
        lines = response.split("\n")
        if lines[0].startswith("```"):
            lines = lines[1:]
        if lines and lines[-1].strip() == "```":
            lines = lines[:-1]
        response = "\n".join(lines).strip()
        if response.startswith("json\n"):
            response = response[5:]

    try:
        result = json.loads(response)
    except json.JSONDecodeError as e:
        print(f"ERROR: Invalid JSON in master response: {e}", file=sys.stderr)
        print(f"Response: {response[:1000]}", file=sys.stderr)
        sys.exit(1)

    summary = result.get("summary", "")
    findings = result.get("findings", [])
    conformance = result.get("conformance")
    if not isinstance(conformance, dict):
        conformance = None
    print(f"Deduped to {len(findings)} findings", file=sys.stderr)

    if args.dry_run:
        print(json.dumps(result, indent=2))
        if ticket_keys is not None:
            if ticket_keys:
                print("Linked tickets: %s" % ", ".join(ticket_keys),
                      file=sys.stderr)
            else:
                print("No linked ticket found.", file=sys.stderr)
        return

    post_and_record(platform, args, summary, findings, diff, ticket_keys,
                    conformance, ticket_details, api_url, project_id, gitlab_token)

if __name__ == "__main__":
    main()
