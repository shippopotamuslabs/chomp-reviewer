#!/usr/bin/env python3
"""
Shared OpenRouter API client for Forge CI jobs.

Both thermos.py and master.py use this to call the OpenRouter chat completions API.
"""
import json
import os
import sys
import urllib.request
import urllib.error


def get_model():
    """Get the model ID, stripping the openrouter/ provider prefix if present."""
    model = os.environ.get("FORGE_MODEL", "openrouter/meta/muse-spark-1.3-contributor")
    if model.startswith("openrouter/"):
        model = model[len("openrouter/"):]
    return model


def call_openrouter(api_key, model, prompt, max_tokens=16000):
    """Call OpenRouter chat completions API. Returns the response content."""
    url = "https://openrouter.ai/api/v1/chat/completions"
    data = {
        "model": model,
        "messages": [{"role": "user", "content": prompt}],
        "max_tokens": max_tokens,
        "temperature": 0.1,
    }
    req = urllib.request.Request(
        url,
        data=json.dumps(data).encode(),
        headers={
            "Content-Type": "application/json",
            "Authorization": f"Bearer {api_key}",
            "HTTP-Referer": "https://gitlab.com/browntown.development-group/stratum",
            "X-Title": "Forge Code Reviewer",
        },
    )
    try:
        with urllib.request.urlopen(req, timeout=300) as resp:
            result = json.load(resp)
            return result["choices"][0]["message"]["content"]
    except urllib.error.HTTPError as e:
        body = e.read().decode()[:2000]
        print(f"ERROR: OpenRouter API call failed: HTTP {e.code}: {body}", file=sys.stderr)
        sys.exit(1)
    except Exception as e:
        print(f"ERROR: OpenRouter API call failed: {e}", file=sys.stderr)
        sys.exit(1)
