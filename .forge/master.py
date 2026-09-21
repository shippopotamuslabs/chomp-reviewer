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
    PR_AGENT_GITLAB_TOKEN: GitLab token for posting comments
    CI_API_V4_URL: GitLab API base URL
    CI_PROJECT_ID: Project ID
    FORGE_MODEL: model to use (default: openrouter/meta/muse-spark-1.3-contributor)
"""
import argparse
import json
import os
import sys
import urllib.request
import urllib.error
import urllib.parse

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from openrouter_client import get_model, call_openrouter

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

def build_rollup(sha, summary, findings, project_path):
    """Build the rollup comment markdown."""
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

    return "\n".join(lines)

def build_inline(sha, finding):
    """Build the inline discussion body."""
    badge = severity_badge(finding.get("severity", "Low"))
    cat = category_tag(finding.get("category", ""))
    title = finding.get("title", "Untitled")
    return f"""<!-- forge:consolidated-review sha:{sha} -->
{badge} — {title} <code>{cat}</code>

<pre>
{finding.get('description', '')}
</pre>

{build_agent_prompt(finding)}"""

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--iid", required=True, help="MR internal ID")
    parser.add_argument("--sha", required=True, help="MR head SHA")
    parser.add_argument("--pr-agent", required=True, help="Path to PR-Agent findings markdown")
    parser.add_argument("--thermos-a", required=True, help="Path to thermos-a JSON")
    parser.add_argument("--thermos-b", required=True, help="Path to thermos-b JSON")
    parser.add_argument("--diff", required=True, help="Path to diff file")
    parser.add_argument("--prompt-dir", default=".forge/prompts")
    parser.add_argument("--dry-run", action="store_true", help="Don't post, just print")
    args = parser.parse_args()

    api_key = os.environ.get("OPENROUTER_API_KEY")
    gitlab_token = os.environ.get("PR_AGENT_GITLAB_TOKEN")
    api_url = os.environ.get("CI_API_V4_URL", "https://gitlab.com/api/v4")
    project_id = os.environ.get("CI_PROJECT_ID", "83700143")

    if not api_key:
        print("ERROR: OPENROUTER_API_KEY not set", file=sys.stderr)
        sys.exit(1)
    if not gitlab_token and not args.dry_run:
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

    prompt = (template
              .replace("{pr_agent_md}", pr_agent_md)
              .replace("{thermos_a_json}", thermos_a)
              .replace("{thermos_b_json}", thermos_b)
              .replace("{diff}", diff))

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
        return

    # Get project path for blob links
    project = gitlab_request("GET", f"{api_url}/projects/{project_id}", gitlab_token)
    project_path = project.get("path_with_namespace", "browntown.development-group/stratum")

    # Get MR diff_refs for positioning
    mr = gitlab_request("GET", f"{api_url}/projects/{project_id}/merge_requests/{args.iid}", gitlab_token)
    diff_refs = mr.get("diff_refs", {})
    base_sha = diff_refs.get("base_sha", "")
    head_sha = diff_refs.get("head_sha", args.sha)
    start_sha = diff_refs.get("start_sha", "")

    # Post rollup first
    rollup_body = build_rollup(args.sha, summary, findings, project_path)
    print("Posting rollup...", file=sys.stderr)
    note = gitlab_request(
        "POST",
        f"{api_url}/projects/{project_id}/merge_requests/{args.iid}/notes",
        gitlab_token,
        {"body": rollup_body}
    )
    print(f"Rollup posted: note {note.get('id')}", file=sys.stderr)

    # Post inline discussions
    recorded = []
    for f in findings:
        file_path = f.get("file", "")
        line_start = f.get("line_start", 0)
        inline_body = build_inline(args.sha, f)

        # Build position
        # For simplicity, use new_path/new_line (assumes added lines; the LLM should ground line numbers in the diff)
        position = {
            "position_type": "text",
            "base_sha": base_sha,
            "head_sha": head_sha,
            "start_sha": start_sha,
            "new_path": file_path,
            "new_line": line_start,
        }

        print(f"Posting inline for {file_path}:{line_start}...", file=sys.stderr)
        # GitLab discussions API with position
        # We need to use form-encoded for nested position params
        import urllib.parse
        params = {
            "body": inline_body,
            "position[position_type]": "text",
            "position[base_sha]": base_sha,
            "position[head_sha]": head_sha,
            "position[start_sha]": start_sha,
            "position[new_path]": file_path,
            "position[new_line]": str(line_start),
        }
        data = urllib.parse.urlencode(params).encode()
        req = urllib.request.Request(
            f"{api_url}/projects/{project_id}/merge_requests/{args.iid}/discussions",
            data=data,
            method="POST",
            headers={
                "PRIVATE-TOKEN": gitlab_token,
                "Content-Type": "application/x-www-form-urlencoded",
            },
        )
        try:
            with urllib.request.urlopen(req, timeout=30) as resp:
                discussion = json.load(resp)
                disc_id = discussion.get("id")
                print(f"  Discussion {disc_id} posted", file=sys.stderr)
                recorded.append({
                    "id": disc_id,
                    "file": file_path,
                    "line_start": line_start,
                    "line_end": f.get("line_end", line_start),
                    "source": "+".join(f.get("sources", ["forge"])),
                })
        except Exception as e:
            print(f"  WARNING: Failed to post inline for {file_path}:{line_start}: {e}", file=sys.stderr)
            # Continue with other findings

    # Write recorded discussions for the verify job
    with open("forge-recorded.json", "w") as f:
        json.dump({
            "iid": args.iid,
            "sha": args.sha,
            "discussions": recorded,
        }, f, indent=2)
    print(f"Recorded {len(recorded)} discussions to forge-recorded.json", file=sys.stderr)

    # Summary counts
    counts = {"Critical": 0, "High": 0, "Medium": 0, "Low": 0}
    for f in findings:
        sev = f.get("severity", "Low")
        if sev in counts:
            counts[sev] += 1
    print(f"Posted consolidated review: {counts}", file=sys.stderr)

if __name__ == "__main__":
    main()
