#!/usr/bin/env python3
"""Deterministic, LLM-free ticket extraction (FRG-6a, extraction only).

Strict precedence cascade over sources:
  1. head branch name
  2. PR title
  3. PR body (incl. Closes/Fixes KEY-123 and GitHub #123 / owner/repo#123)
  4. commit messages on the PR

Per provider (``linear`` / ``github``), the first source that yields any
match wins for that provider; lower sources contribute nothing for that
provider. Distinct tickets within the winning source are all kept,
deduped preserving order. No match is a normal outcome (``[]``).

Stdlib only. No network, no file writes.
"""

import argparse
import json
import re

LINEAR_RE = re.compile(r"\b([A-Z][A-Z0-9]+)-(\d+)\b", re.IGNORECASE)
QUALIFIED_GH_RE = re.compile(r"\b([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)#(\d+)\b")
BARE_GH_RE = re.compile(r"(?<![A-Za-z0-9_/#])#(\d+)\b")

SOURCES = ("branch", "title", "body", "commits")


def _linear_from_text(text):
    out = []
    for m in LINEAR_RE.finditer(text or ""):
        key = "%s-%s" % (m.group(1).upper(), m.group(2))
        out.append({"provider": "linear", "key": key, "id": m.group(2)})
    return out


def _github_from_text(text):
    text = text or ""
    out = []
    taken = []  # spans of qualified matches, to suppress inner bare matches
    for m in QUALIFIED_GH_RE.finditer(text):
        out.append({"provider": "github",
                    "key": "%s#%s" % (m.group(1), m.group(2)),
                    "id": m.group(2)})
        taken.append(m.span())
    for m in BARE_GH_RE.finditer(text):
        s = m.start()
        if any(a <= s < b for a, b in taken):
            continue
        out.append({"provider": "github",
                    "key": "#%s" % m.group(1), "id": m.group(1)})
    return out


def _dedupe(items):
    seen = set()
    out = []
    for t in items:
        sig = (t["provider"], t["key"])
        if sig in seen:
            continue
        seen.add(sig)
        out.append(t)
    return out


def extract_from_source(source_name, text, provider_hint=None):
    """Extract tickets from one source text.

    Returns list of dicts with provider/key/id/source keys.
    provider_hint optionally restricts to "linear" or "github".
    """
    items = []
    if provider_hint in (None, "linear"):
        items.extend(_linear_from_text(text))
    if provider_hint in (None, "github"):
        items.extend(_github_from_text(text))
    items = _dedupe(items)
    for t in items:
        t["source"] = source_name
    return items


def extract_tickets(branch, title, body, commits):
    """Extract tickets across the 4-source cascade.

    commits: iterable of message strings (or a single string / None).
    """
    if isinstance(commits, str):
        commit_texts = [commits]
    else:
        commit_texts = list(commits or [])
    texts = {
        "branch": branch or "",
        "title": title or "",
        "body": body or "",
        "commits": "\n".join(commit_texts),
    }
    result = []
    for provider in ("linear", "github"):
        for source in SOURCES:
            found = extract_from_source(source, texts[source],
                                        provider_hint=provider)
            if found:
                result.extend(found)
                break
    return result


def main(argv=None):
    parser = argparse.ArgumentParser(
        description="Extract ticket refs (no network). Prints JSON list.")
    parser.add_argument("--branch", default="")
    parser.add_argument("--title", default="")
    parser.add_argument("--body", default="")
    parser.add_argument("--commit", dest="commits", action="append",
                        default=[])
    args = parser.parse_args(argv)
    print(json.dumps(extract_tickets(args.branch, args.title,
                                     args.body, args.commits)))


if __name__ == "__main__":
    main()
