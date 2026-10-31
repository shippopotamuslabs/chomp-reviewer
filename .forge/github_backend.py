#!/usr/bin/env python3
"""
GitHub backend for Chomp: REST + GraphQL helpers used by master.py and
verify.py when running under GitHub Actions.

Selected by environment: FORGE_PLATFORM=github, or automatically when
GITHUB_ACTIONS=true. The GitLab code paths stay the default and are
unchanged; this module is the GitHub equivalent of the GitLab helpers in
master.py / verify.py.

Environment:
    GITHUB_TOKEN:       Forge GitHub App installation token (minted by the
                        workflow via actions/create-github-app-token).
    GITHUB_API_URL:     API base (Actions sets it; default api.github.com).
    GITHUB_REPOSITORY:  "owner/repo" of the repo under review.

Stdlib only, like the rest of .forge/.
"""
import json
import os
import sys
import urllib.request
import urllib.error


def github_request(method, url, token, data=None, fatal=True, accept=None):
    """Make a GitHub REST API request.

    With fatal=True (default) an HTTP error prints and exits, for requests
    the run cannot proceed without. With fatal=False the HTTP error is
    raised as RuntimeError instead (mirrors verify.py's gitlab_request),
    so one failing item does not abort the whole run.
    """
    headers = {
        "Authorization": f"Bearer {token}",
        "Accept": accept or "application/vnd.github+json",
        "X-GitHub-Api-Version": "2022-11-28",
        "User-Agent": "chomp-reviewer",
    }
    body = None
    if data is not None:
        body = json.dumps(data).encode()
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=body, method=method, headers=headers)
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            raw = resp.read().decode()
            return json.loads(raw) if raw else {}
    except urllib.error.HTTPError as e:
        err_body = e.read().decode()[:1000]
        msg = f"ERROR: GitHub API {method} {url} failed: {e.code} {err_body}"
        if fatal:
            print(msg, file=sys.stderr)
            sys.exit(1)
        raise RuntimeError(msg)


def github_graphql(api_url, token, query, variables=None, fatal=False):
    """POST a GraphQL query. Returns the response's "data" object.

    Review-thread resolution (resolveReviewThread) exists only in the
    GraphQL API, so thread state reads and resolution go through here.
    Defaults to fatal=False: a GraphQL failure raises RuntimeError.
    """
    payload = {"query": query, "variables": variables or {}}
    result = github_request(
        "POST", f"{api_url}/graphql", token, data=payload, fatal=fatal)
    if isinstance(result, dict) and result.get("errors"):
        msg = f"ERROR: GitHub GraphQL failed: {result['errors']}"
        if fatal:
            print(msg, file=sys.stderr)
            sys.exit(1)
        raise RuntimeError(msg)
    return (result or {}).get("data", {})


def gh_env():
    """Read the GitHub Actions environment. Returns (api_url, repo, token)."""
    api_url = os.environ.get("GITHUB_API_URL", "https://api.github.com")
    repo = os.environ.get("GITHUB_REPOSITORY", "")
    token = os.environ.get("GITHUB_TOKEN", "")
    return api_url, repo, token


# ---------------------------------------------------------------------------
# Pull request data
# ---------------------------------------------------------------------------

def get_pull_request(api_url, repo, number, token):
    """Fetch one pull request (head/base SHAs, state)."""
    return github_request(
        "GET", f"{api_url}/repos/{repo}/pulls/{number}", token)


def fetch_pull_files(api_url, repo, number, token):
    """List every file changed in the PR via the pulls files API.

    Paginated like list_issue_comments. Each entry carries the file's
    status, its previous_filename for renames, and (usually) its patch.
    """
    files = []
    page = 1
    while True:
        batch = github_request(
            "GET",
            f"{api_url}/repos/{repo}/pulls/{number}/files"
            f"?per_page=100&page={page}",
            token,
        )
        if not batch:
            break
        files.extend(batch)
        if len(batch) < 100:
            break
        page += 1
    return files


PATCH_UNAVAILABLE_MARKER = (
    "[forge: no patch served by the GitHub API for this file "
    "(binary or too large); contents not shown]"
)


def assemble_pull_diff(files):
    """Assemble pulls-files entries into one unified diff document.

    The fallback twin of fetch_diff.py's assemble_mr_diff: each file's
    patch is preserved byte-for-byte apart from the synthesized
    'diff --git' / '---' / '+++' headers (only newline padding is
    trimmed, never trailing whitespace content, so a hunk's final blank
    context line survives). A changed file whose patch the API omitted
    (binary, or too large for the endpoint to serve) gets an explicit
    marker line in its section — never a silent drop. A pure rename
    (no patch, zero changes) is headers + rename lines only, exactly
    as git serves it. Returns "" when there are no files.

    Added/removed files also get git's 'new file mode' / 'deleted
    file mode' line: diff parsers (unidiff, which pr-agent reads
    mr.diff with) only treat a section as added/deleted when that
    line sets the /dev/null side — without it a deletion's
    '+++ /dev/null' is a parse error. The files API does not report
    modes, so the standard 100644 is synthesized; no Forge consumer
    acts on the mode value.
    """
    parts = []
    for entry in files:
        new_path = entry.get("filename", "")
        old_path = entry.get("previous_filename") or new_path
        status = entry.get("status", "modified")
        lines = [f"diff --git a/{old_path} b/{new_path}"]
        if status == "added":
            lines.append("new file mode 100644")
        elif status == "removed":
            lines.append("deleted file mode 100644")
        if status in ("renamed", "copied") and old_path != new_path:
            verb = "rename" if status == "renamed" else "copy"
            lines.append(f"{verb} from {old_path}")
            lines.append(f"{verb} to {new_path}")
        patch = entry.get("patch")
        if patch:
            lines.append(
                "--- /dev/null" if status == "added" else f"--- a/{old_path}")
            lines.append(
                "+++ /dev/null" if status == "removed" else f"+++ b/{new_path}")
            lines.append(patch.strip("\n"))
        elif status in ("renamed", "copied") and not entry.get("changes"):
            pass  # pure rename: the header block above is the whole section
        else:
            lines.append(
                "--- /dev/null" if status == "added" else f"--- a/{old_path}")
            lines.append(
                "+++ /dev/null" if status == "removed" else f"+++ b/{new_path}")
            lines.append(PATCH_UNAVAILABLE_MARKER)
        parts.append("\n".join(lines))

    if not parts:
        return ""

    return "\n".join(parts) + "\n"


def fetch_pull_diff(api_url, repo, number, token):
    """Fetch the PR's unified diff.

    Fast path: the pulls endpoint with the diff media type serves the
    complete document in one request. GitHub refuses that endpoint
    with HTTP 406 once a diff passes its size limits (observed on a
    770-file PR, where the review died unfetched), so on 406 the same
    document is assembled from the paginated files API instead.

    Returns the diff text. An empty diff means nothing to review; the
    caller treats that like fetch_diff.py's no-diffs error.
    """
    req = urllib.request.Request(
        f"{api_url}/repos/{repo}/pulls/{number}",
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github.diff",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "chomp-reviewer",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=60) as resp:
            return resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        if e.code != 406:
            print(f"ERROR: could not fetch PR diff: {e}", file=sys.stderr)
            sys.exit(1)
        print(
            "WARNING: PR diff endpoint returned 406 (diff too large); "
            "assembling the diff from the pulls files API instead.",
            file=sys.stderr)
    except Exception as e:
        print(f"ERROR: could not fetch PR diff: {e}", file=sys.stderr)
        sys.exit(1)
    return assemble_pull_diff(fetch_pull_files(api_url, repo, number, token))


def fetch_file_at_ref(api_url, repo, path, ref, token):
    """Fetch a file's raw content at a commit via the contents API.

    Returns (status, text) with status "present", "absent" (404), or
    "unknown" (any other failure) — the same contract as verify.py's
    GitLab fetch_file_at_ref, so verdict grounding behaves identically.
    """
    import urllib.parse
    encoded = urllib.parse.quote(path, safe="/")
    url = f"{api_url}/repos/{repo}/contents/{encoded}?ref={ref}"
    req = urllib.request.Request(
        url,
        headers={
            "Authorization": f"Bearer {token}",
            "Accept": "application/vnd.github.raw+json",
            "X-GitHub-Api-Version": "2022-11-28",
            "User-Agent": "chomp-reviewer",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=30) as resp:
            return "present", resp.read().decode("utf-8", errors="replace")
    except urllib.error.HTTPError as e:
        if e.code == 404:
            return "absent", ""
        print(f"  WARNING: file fetch failed for {path}: {e}", file=sys.stderr)
        return "unknown", ""
    except Exception as e:
        print(f"  WARNING: file fetch failed for {path}: {e}", file=sys.stderr)
        return "unknown", ""


# ---------------------------------------------------------------------------
# Comments: rollup, inline, marker detection
# ---------------------------------------------------------------------------

def list_issue_comments(api_url, repo, number, token):
    """List all comments on the PR conversation (paginated)."""
    comments = []
    page = 1
    while True:
        batch = github_request(
            "GET",
            f"{api_url}/repos/{repo}/issues/{number}/comments"
            f"?per_page=100&page={page}",
            token,
        )
        if not batch:
            break
        comments.extend(batch)
        if len(batch) < 100:
            break
        page += 1
    return comments


def find_marker_comment(api_url, repo, number, token, marker):
    """Return the first conversation comment containing marker, or None.

    The GitHub equivalent of the GitLab review-once check over MR notes:
    the rollup comment carries the consolidated-review marker
    (chomp:consolidated-review, or the legacy forge:consolidated-review on
    reviews posted before the rename).
    """
    for comment in list_issue_comments(api_url, repo, number, token):
        if marker in (comment.get("body") or ""):
            return comment
    return None


def post_issue_comment(api_url, repo, number, token, body):
    """Post the rollup as a PR conversation comment."""
    return github_request(
        "POST", f"{api_url}/repos/{repo}/issues/{number}/comments",
        token, data={"body": body})


def post_review_comment(api_url, repo, number, token, body, commit_id,
                        path, line, side):
    """Post one inline review comment anchored to a diff line.

    GitHub anchors review comments by (commit_id, path, line, side) and
    only accepts lines that are part of the PR diff — the same constraint
    master's diff line map already resolves for GitLab positions.
    side is "RIGHT" (new side) or "LEFT" (old side).
    """
    return github_request(
        "POST", f"{api_url}/repos/{repo}/pulls/{number}/comments",
        token,
        data={
            "body": body,
            "commit_id": commit_id,
            "path": path,
            "line": line,
            "side": side,
        },
        fatal=False,
    )


def post_review_comment_attempt(api_url, repo, number, token, body,
                                commit_id, attempt):
    """Adapt master's attempt dicts to post_review_comment.

    Attempt {"new_line": n} posts on the RIGHT side at n; an attempt with
    only {"old_line": n} posts on the LEFT side at n against old_path.
    Signature matches master's post_fn(body, attempt) contract so
    post_inline_with_fallback drives it unchanged.
    """
    if attempt.get("new_line") is not None:
        return post_review_comment(
            api_url, repo, number, token, body, commit_id,
            attempt["new_path"], attempt["new_line"], "RIGHT")
    return post_review_comment(
        api_url, repo, number, token, body, commit_id,
        attempt["old_path"], attempt["old_line"], "LEFT")


# ---------------------------------------------------------------------------
# Verify phase: review threads via GraphQL
# ---------------------------------------------------------------------------

THREADS_QUERY = """
query($owner: String!, $name: String!, $number: Int!, $cursor: String) {
  repository(owner: $owner, name: $name) {
    pullRequest(number: $number) {
      reviewThreads(first: 100, after: $cursor) {
        pageInfo { hasNextPage endCursor }
        nodes {
          id
          isResolved
          comments(first: 100) {
            nodes {
              databaseId
              body
              path
              line
              originalLine
            }
          }
        }
      }
    }
  }
}
"""

RESOLVE_MUTATION = """
mutation($threadId: ID!) {
  resolveReviewThread(input: {threadId: $threadId}) {
    thread { id isResolved }
  }
}
"""


def get_forge_review_threads(api_url, repo, number, token):
    """Collect open Chomp review threads for verify.

    Mirrors verify.py's get_forge_discussions selection: threads whose
    first comment carries a consolidated-review marker (chomp:, or the
    legacy forge: marker from before the rename) and that
    are not resolved. Returns dicts {"id", "comment_id", "body",
    "comments"} where id is the GraphQL thread node ID (needed for
    resolution) and comment_id is the REST database ID of the first
    comment (needed for verdict replies).
    """
    owner, name = repo.split("/", 1)
    threads = []
    cursor = None
    while True:
        data = github_graphql(api_url, token, THREADS_QUERY, {
            "owner": owner, "name": name, "number": int(number),
            "cursor": cursor,
        }, fatal=True)
        conn = data["repository"]["pullRequest"]["reviewThreads"]
        threads.extend(conn["nodes"])
        if not conn["pageInfo"]["hasNextPage"]:
            break
        cursor = conn["pageInfo"]["endCursor"]

    forge_open = []
    for thread in threads:
        if thread.get("isResolved"):
            continue
        comments = (thread.get("comments") or {}).get("nodes") or []
        if not comments:
            continue
        first_body = comments[0].get("body") or ""
        if not any(m in first_body for m in
                   ("chomp:consolidated-review", "forge:consolidated-review")):
            continue
        forge_open.append({
            "id": thread["id"],
            "comment_id": comments[0].get("databaseId"),
            "body": first_body,
            "comments": comments,
        })
    return forge_open


def reply_to_review_comment(api_url, repo, number, token, comment_id, body):
    """Post a verdict reply in a review thread (REST replies endpoint)."""
    return github_request(
        "POST",
        f"{api_url}/repos/{repo}/pulls/{number}/comments/{comment_id}/replies",
        token, data={"body": body}, fatal=False)


def resolve_review_thread(api_url, token, thread_id):
    """Resolve one review thread via GraphQL resolveReviewThread."""
    return github_graphql(
        api_url, token, RESOLVE_MUTATION, {"threadId": thread_id},
        fatal=False)
