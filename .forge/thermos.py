#!/usr/bin/env python3
"""
Forge thermos runner: calls OpenRouter API with a lens prompt, outputs JSON findings.

Usage:
    python3 .forge/thermos.py --lens correctness --diff mr.diff --output thermos-correctness.json
    python3 .forge/thermos.py --lens structure --diff mr.diff --output thermos-structure.json

Environment:
    OPENROUTER_API_KEY: API key for OpenRouter
    FORGE_MODEL: model to use (default: openrouter/meta/muse-spark-1.3-contributor)
"""
import argparse
import json
import os
import sys

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from openrouter_client import get_model, call_openrouter

def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("--lens", required=True, choices=["correctness", "structure"])
    parser.add_argument("--diff", required=True, help="Path to unified diff file")
    parser.add_argument("--output", required=True, help="Path to output JSON file")
    parser.add_argument("--prompt-dir", default=".forge/prompts")
    args = parser.parse_args()

    api_key = os.environ.get("OPENROUTER_API_KEY")
    if not api_key:
        print("ERROR: OPENROUTER_API_KEY not set", file=sys.stderr)
        sys.exit(1)

    model = get_model()

    # Load prompt template
    prompt_file = os.path.join(args.prompt_dir, f"thermos-{args.lens}.md")
    with open(prompt_file) as f:
        template = f.read()

    # Load diff
    with open(args.diff) as f:
        diff = f.read()

    # For file contents, we'll use a placeholder - the full file fetch
    # would require GitLab API calls. The diff is the primary input.
    # In CI, we can fetch file contents via the API if needed.
    file_contents = "(See diff above for changed lines. Full file contents not fetched in CI mode.)"

    prompt = template.replace("{diff}", diff).replace("{file_contents}", file_contents)

    print(f"Calling OpenRouter ({model}) for {args.lens} lens...", file=sys.stderr)
    response = call_openrouter(api_key, model, prompt)

    # Clean response: strip markdown fences if present
    response = response.strip()
    if response.startswith("```"):
        lines = response.split("\n")
        # Remove first and last fence lines
        if lines[0].startswith("```"):
            lines = lines[1:]
        if lines and lines[-1].strip() == "```":
            lines = lines[:-1]
        response = "\n".join(lines).strip()
        # Handle ```json prefix
        if response.startswith("json\n"):
            response = response[5:]

    # Validate JSON
    try:
        findings = json.loads(response)
        if not isinstance(findings, list):
            print(f"ERROR: Expected JSON array, got {type(findings)}", file=sys.stderr)
            print(f"Response: {response[:500]}", file=sys.stderr)
            sys.exit(1)
    except json.JSONDecodeError as e:
        print(f"ERROR: Invalid JSON in response: {e}", file=sys.stderr)
        print(f"Response (first 1000 chars): {response[:1000]}", file=sys.stderr)
        sys.exit(1)

    with open(args.output, "w") as f:
        json.dump(findings, f, indent=2)

    print(f"Wrote {len(findings)} findings to {args.output}", file=sys.stderr)

if __name__ == "__main__":
    main()
