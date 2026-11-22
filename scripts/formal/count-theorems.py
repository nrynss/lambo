#!/usr/bin/env python3
"""Count the theorems in the Lean model files, by the rule formal/README.md states.

  python3 -I scripts/formal/count-theorems.py [--repo DIR]

The model files are `formal/LamboProofs/*.lean` other than `Audit.lean`. A
theorem is a declaration line: outside every comment (`--` line comments and
`/- ... -/` block comments, which nest, doc comments included) and outside
string literals, a line that starts at column 0 with `theorem`, or with
`private theorem` or `protected theorem`, optionally after one or more
attributes (`@[simp]`), followed by the theorem's name. The binders and colon
may be on that line or on the lines after it: a line holding only
`theorem name` counts. Prose that happens to start with "theorem" is never
counted, since it is inside a comment. `example`s are not counted.

Prints one line per file (count, path) and the total, with the number of
files.
"""

import argparse
import re
import sys
from pathlib import Path

DECL = re.compile(
    r"^(?:@\[[^\]]*\]\s*)*(?:(?:private|protected)\s+)?theorem\s+[^\s(\[{:⦃]+"
)


def strip_comments(src):
    """The source with every comment and string literal blanked (newlines kept,
    so line numbers and columns stay where they were)."""
    out = []
    i, n = 0, len(src)
    depth = 0
    while i < n:
        c = src[i]
        two = src[i:i + 2]
        if depth > 0:
            if two == "/-":
                depth += 1
                out.append("  ")
                i += 2
            elif two == "-/":
                depth -= 1
                out.append("  ")
                i += 2
            else:
                out.append("\n" if c == "\n" else " ")
                i += 1
        elif two == "/-":
            depth = 1
            out.append("  ")
            i += 2
        elif two == "--":
            while i < n and src[i] != "\n":
                out.append(" ")
                i += 1
        elif c == "'" and (m := CHAR_LIT.match(src, i)) and (i == 0 or not (src[i - 1].isalnum() or src[i - 1] in "_'")):
            # A char literal ('"', '\'', 'a'), not a prime in a name such as
            # h'. Blanked so a quote inside it opens no string.
            out.append(" " * len(m.group(0)))
            i = m.end()
        elif c == '"':
            out.append(" ")
            i += 1
            while i < n and src[i] != '"':
                if src[i] == "\\" and i + 1 < n:
                    out.append("  ")
                    i += 2
                    continue
                out.append("\n" if src[i] == "\n" else " ")
                i += 1
            if i < n:
                out.append(" ")
                i += 1
        else:
            out.append(c)
            i += 1
    return "".join(out)


CHAR_LIT = re.compile(r"'(?:\\.|[^'\\\n])'")
README_TOTAL = re.compile(r"([0-9][0-9,]*) theorems across ([0-9]+) model files")


def count(src):
    """The number of theorem declaration lines in one Lean source."""
    return sum(1 for line in strip_comments(src).split("\n") if DECL.match(line))


def model_files(repo):
    root = Path(repo) / "formal" / "LamboProofs"
    return sorted(p for p in root.glob("*.lean") if p.name != "Audit.lean")


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--repo", default=".", help="repository root (default: .)")
    ap.add_argument(
        "--check-readme",
        action="store_true",
        help="exit 1 unless formal/README.md states the same total and file count",
    )
    args = ap.parse_args(argv)
    files = model_files(args.repo)
    if not files:
        print(f"no model files under {args.repo}/formal/LamboProofs", file=sys.stderr)
        return 2
    total = 0
    for p in files:
        k = count(p.read_text(encoding="utf-8"))
        total += k
        print(f"{k:5d}  {p.relative_to(Path(args.repo))}")
    print(f"{total:5d}  total, {len(files)} model files")
    if args.check_readme:
        readme = (Path(args.repo) / "formal" / "README.md").read_text(encoding="utf-8")
        m = README_TOTAL.search(" ".join(readme.split()))
        if not m:
            print("formal/README.md states no '<N> theorems across <M> model files'", file=sys.stderr)
            return 1
        stated = (int(m.group(1).replace(",", "")), int(m.group(2)))
        if stated != (total, len(files)):
            print(
                f"formal/README.md says {m.group(1)} theorems across {m.group(2)} model files; "
                f"the count is {total:,} across {len(files)}",
                file=sys.stderr,
            )
            return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
