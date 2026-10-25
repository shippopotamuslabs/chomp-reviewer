#!/usr/bin/env python3
"""
Forge master reviewer: dedups findings via LLM, posts ONE consolidated review.

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
    artifact = None
    if ticket_path.endswith(".json"):
        try:
            artifact = json.loads(text)
        except ValueError:
            artifact = None
    else:
        sibling = os.path.join(os.path.dirname(os.path.abspath(ticket_path)),
                               "ticket.json")
        try:
            with open(sibling) as f:
                artifact = json.load(f)
        except (OSError, ValueError) as e:
            print(f"WARNING: cannot read ticket JSON {sibling}: {e}",
                  file=sys.stderr)
    keys = []
    if isinstance(artifact, dict):
        keys = [t.get("key") for t in artifact.get("tickets", []) or []
                if isinstance(t, dict) and t.get("key")]
    return text, keys


def post_review_gitlab(api_url, project_id, iid, token, sha, summary, findings, diff,
                       tickets=None):
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
                               tickets=tickets)
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
                       tickets=None):
    """Post the consolidated review to GitHub: rollup as a PR conversation
    comment + inline review comments. Returns (recorded, failed)."""
    rollup_body = build_rollup(sha, summary, findings, repo,
                               blob_base=f"https://github.com/{repo}/blob",
                               tickets=tickets)
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


def build_rollup(sha, summary, findings, project_path, blob_base=None,
                 tickets=None):
    """Build the rollup comment markdown.

    blob_base overrides the blob-link prefix (GitHub passes
    "https://github.com/<owner>/<repo>/blob"); the default keeps the
    historical GitLab URL shape. tickets is None by default, which keeps
    the historical output byte-identical; pass [] or [keys...] to note
    the linked-ticket outcome.
    """
    # Count by severity
    counts = {"Critical": 0, "High": 0, "Medium": 0, "Low": 0}
    for f in findings:
        sev = f.get("severity", "Low")
        if sev in counts:
            counts[sev] += 1

    lines = [
        f"<!-- forge:consolidated-review sha:{sha} -->",
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
            "> <pre>",
            f"> {f.get('description', '')}",
            "> </pre>",
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
            "> <pre>",
            f"> {f.get('evidence', '')}",
            "> </pre>",
            "> </details>",
            "",
            build_agent_prompt(f),
            "",
            "</details>",
            "",
        ])

    lines.extend(ticket_note_lines(tickets))

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
            f"\n<!-- forge:anchor originally flagged "
            f"{req.get('file')}[L{req.get('line_start')}-{req.get('line_end')}] "
            f"— anchored to nearest valid diff line -->"
        )
    return f"""<!-- forge:consolidated-review sha:{sha} -->{anchor_note}
{badge} — {title} <code>{cat}</code>

<pre>
{finding.get('description', '')}
</pre>

{build_agent_prompt(finding)}"""

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iid", required=True, help="MR internal ID (GitHub: PR number)")
    parser.add_argument("--sha", required=True, help="MR head SHA")
    parser.add_argument("--pr-agent", required=True, help="Path to PR-Agent findings markdown")
    parser.add_argument("--thermos-a", required=True, help="Path to thermos-a JSON")
    parser.add_argument("--thermos-b", required=True, help="Path to thermos-b JSON")
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

    if platform == "github":
        gh_api, gh_repo, gh_token = github_backend.gh_env()
        recorded, failed = post_review_github(
            gh_api, gh_repo, args.iid, gh_token, args.sha, summary, findings, diff,
            tickets=ticket_keys)
    else:
        recorded, failed = post_review_gitlab(
            api_url, project_id, args.iid, gitlab_token, args.sha, summary, findings, diff,
            tickets=ticket_keys)

    # Write recorded discussions for the verify job
    with open("forge-recorded.json", "w") as f:
        json.dump({
            "iid": args.iid,
            "sha": args.sha,
            "platform": platform,
            "discussions": recorded,
            "failed": failed,
            "tickets": ticket_keys if ticket_keys is not None else [],
        }, f, indent=2)
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

if __name__ == "__main__":
    main()
