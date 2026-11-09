#!/usr/bin/env python3
"""Scope CI work without mistaking a lightweight run for full Rust validation."""

import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys


RUST_PREFIXES = (
    "src/", "tests/", "migrations/", "fixtures/", "examples/", "web/",
    ".cargo/", ".github/workflows/", "scripts/ci/",
)
RUST_FILES = {
    "Cargo.toml", "Cargo.lock", "rust-toolchain", "rust-toolchain.toml", "build.rs",
    "lambo.example.toml",
    # Committed inputs read by the regular test suite, outside fixtures/.
    "evidence/issue-87-calibration/calibration.json",
    "evidence/issue-198-trigger/cosines-bge-m3-self.json",
    "evidence/issue-221/calibration-qwen3-4b-2560.json",
    "evidence/issue-221/calibration-qwen3-4b-1024.json",
    "evidence/issue-221/repro-8concept-qwen3-4b-2560.json",
    "evidence/issue-221/repro-8concept-qwen3-4b-1024.json",
    "evidence/issue-237/calibration-elastic-jina-v3-1024.json",
}
MIRROR_FILES = {
    "docs/reference/cli.mdx", "docs/reference/mcp.mdx",
    "docs/reference/installation.mdx", "docs/reference/config.mdx",
    "site/src/content/docs/cli.mdx", "site/src/content/docs/mcp.mdx",
    "site/src/content/docs/installation.mdx", "site/src/content/docs/config.mdx",
    "scripts/docs/check-mirror-drift.sh",
}


def classify_paths(paths):
    """Include deleted paths and both sides of renames in the supplied paths."""
    paths = set(paths)
    rust = any(path in RUST_FILES or path.startswith(RUST_PREFIXES) for path in paths)
    # Workflow/classifier changes validate both branches of the classifier.
    mirror = bool(paths & MIRROR_FILES) or any(
        path.startswith((".github/workflows/", "scripts/ci/")) for path in paths
    )
    return {"rust": rust, "mirror": mirror}


def classify_event(event_name, event, repo="."):
    """Diff the event's base against the checked-out commit; uncertainty runs all."""
    if event_name == "workflow_dispatch":
        return {"rust": True, "mirror": True}
    if event_name == "pull_request":
        base = event.get("pull_request", {}).get("base", {}).get("sha")
    elif event_name == "push":
        base = event.get("before")
    else:
        return {"rust": True, "mirror": True}
    if not isinstance(base, str) or not re.fullmatch(r"[0-9a-fA-F]{40}", base) or set(base) == {"0"}:
        return {"rust": True, "mirror": True}
    try:
        # --no-renames makes a move out of a Rust input appear as a deletion.
        # -z handles filenames containing whitespace and newlines correctly.
        diff = subprocess.run(
            ["git", "diff", "--name-only", "--no-renames", "-z", base, "HEAD", "--"],
            cwd=repo, check=True, stdout=subprocess.PIPE, stderr=subprocess.PIPE,
        )
    except (OSError, subprocess.CalledProcessError):
        # A force-push can make the old SHA unavailable even in a full clone.
        # Running all validation is safe; silently skipping it is not.
        print("Unable to compare the event base; running full validation", file=sys.stderr)
        return {"rust": True, "mirror": True}
    paths = [os.fsdecode(path) for path in diff.stdout.split(b"\0") if path]
    return classify_paths(paths)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--event-name", default=os.environ.get("GITHUB_EVENT_NAME", ""))
    parser.add_argument("--event-path", default=os.environ.get("GITHUB_EVENT_PATH", ""))
    parser.add_argument("--output", default=os.environ.get("GITHUB_OUTPUT"))
    args = parser.parse_args()
    try:
        event = json.loads(Path(args.event_path).read_text())
        if not isinstance(event, dict):
            raise ValueError("event must be an object")
        result = classify_event(args.event_name, event)
    except (OSError, ValueError, TypeError, AttributeError):
        print("Unable to read the event; running full validation", file=sys.stderr)
        result = {"rust": True, "mirror": True}
    print(json.dumps(result, sort_keys=True))
    if args.output:
        with open(args.output, "a", encoding="utf-8") as output:
            for key, value in result.items():
                output.write(f"{key}={str(value).lower()}\n")


if __name__ == "__main__":
    main()
