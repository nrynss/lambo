#!/usr/bin/env python3
"""Render release notes from the template and CHANGELOG.

"What's new" is the CHANGELOG's `## <version>` section verbatim, so an
automatic tag-push release can never publish the template's fill-in
placeholder. Fails closed when the version section or either placeholder
is missing (a silent pass-through is how raw scaffolding shipped).

    python3 scripts/ci/render_release_notes.py --version 0.10.1 \\
        --template .github/release/release-notes-template.md \\
        --changelog CHANGELOG.md --out /tmp/notes-0.10.1.md
"""

import argparse
import re
import sys
from pathlib import Path

VERSION_RE = re.compile(
    r"[0-9]+\.[0-9]+\.[0-9]+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?"
)
WHATS_NEW_PLACEHOLDER = (
    "- _Summarize the user-visible changes since the previous release, "
    "one bullet each._"
)
LIMITS_PLACEHOLDER = "- _Add any limits specific to this release._"


class NotesError(RuntimeError):
    pass


def changelog_section(changelog: str, version: str) -> str:
    """The body of CHANGELOG's `## <version>` section (heading excluded).

    The heading may carry a date (`## 0.10.1 (2026-11-10)`). The section
    ends at the next level-2 heading or end of file.
    """
    lines = changelog.splitlines(keepends=True)
    start = None
    for i, line in enumerate(lines):
        stripped = line.rstrip("\n")
        if stripped == f"## {version}" or stripped.startswith(f"## {version} "):
            start = i
            break
    if start is None:
        raise NotesError(f"CHANGELOG has no '## {version}' section")
    end = len(lines)
    for i in range(start + 1, len(lines)):
        if lines[i].startswith("## "):
            end = i
            break
    body = "".join(lines[start + 1 : end]).strip("\n")
    if not body:
        raise NotesError(f"CHANGELOG '## {version}' section is empty")
    return body + "\n"


def render(template: str, version: str, section: str) -> str:
    """Substitute the version, splice the CHANGELOG section into What's
    new, and drop the Known-limits fill-in line (the static bedrock bullet
    stays). The maintainer HTML comment is stripped: published notes carry
    no scaffolding. Missing placeholders fail: the template drifted."""
    if "__LAMBO_VERSION__" not in template:
        raise NotesError("template has no __LAMBO_VERSION__ placeholder")
    out = template.replace("__LAMBO_VERSION__", version)
    out = re.sub(r"<!-- Maintainer:.*?-->\n?", "", out, flags=re.DOTALL)
    if WHATS_NEW_PLACEHOLDER not in out:
        raise NotesError("template's What's-new placeholder is gone")
    out = out.replace(WHATS_NEW_PLACEHOLDER, section.rstrip("\n"))
    if LIMITS_PLACEHOLDER not in out:
        raise NotesError("template's Known-limits placeholder is gone")
    out = out.replace(LIMITS_PLACEHOLDER + "\n", "")
    return out


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--version", required=True)
    parser.add_argument("--template", required=True)
    parser.add_argument("--changelog", required=True)
    parser.add_argument("--out", required=True)
    args = parser.parse_args(argv)
    try:
        if not VERSION_RE.fullmatch(args.version):
            raise NotesError(f"not a release version: {args.version!r}")
        template = Path(args.template).read_text()
        changelog = Path(args.changelog).read_text()
        section = changelog_section(changelog, args.version)
        Path(args.out).write_text(render(template, args.version, section))
    except (OSError, NotesError) as error:
        print(f"render_release_notes: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
