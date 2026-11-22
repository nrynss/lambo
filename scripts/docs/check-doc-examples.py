#!/usr/bin/env python3
"""Check the docs' worked examples against the Lean oracle's evaluated cases.

Usage:
  python3 -I scripts/docs/check-doc-examples.py [--root DIR] [PAGE ...]

With no PAGE, every `site/src/content/docs/*.mdx` and `docs/reference/*.mdx`
under the root is checked. The root defaults to the repository this script
is in.

A worked example on a docs page is backed by one or more cases of a
differential-oracle file, `formal/oracle/<Model>.json` (see formal/README.md,
"Differential oracle"). The page says which with an anchor comment on its own
line, directly above the example's table:

    {/* oracle: Canon.json stage1Passes#2 stage1Passes#11 */}

    | GC sweeps survived | Dependent interactions | Stage 1 |
    |---|---|---|
    | 3 | 2 | passes |
    | 2 | 2 | fails |

A case id is `<function>#<index>`: `<function>` is the last component of a
function's `lean` name in the file (`stage1Passes` for
`Lambo.Canon.stage1Passes`), and `<index>` is the case's 0-based position in
that function's `cases`, the index the Rust replay reports on a mismatch.

The rules are strict:

* the anchor is written exactly `{/* oracle: <file> <case id> ... */}`, alone
  on its line; any other comment that starts with `oracle` (`{/* oracle : ...`,
  `{/*oracle:`, `{/* Oracle: ...`, one split over lines) is an error, not a
  comment the check skips;
* one anchor names cases of one function, and that function has an entry in
  COLUMNS below: the table's header must be exactly that entry's labels;
* the next non-blank line after the anchor starts the table (a header row, a
  separator row, then the data rows);
* the table has one data row per case id, in the anchor's order;
* each row has one cell per input value, in the case's input order, and then
  one cell for the output (an input that is not a JSON array is one value);
* a cell matches a value when, after trimming and removing surrounding `**`
  and backticks, it is: the same integer or number; the same string ignoring
  case; for `true`, `false` or `null`, one of the words the column's
  vocabulary in COLUMNS allows for it. A column with no word for a value
  matches no cell for it.

Exit codes:

* 0: every anchor matches;
* 1: a page disagrees with the oracle (a wrong cell, a wrong header, a
  missing table, file or case, a malformed anchor);
* 2: a usage error (bad arguments, no such page);
* 3: an input cannot be read: a page that is not UTF-8, or an oracle file
  that is not JSON or not in the oracle format.

Every error is reported with the page, line, file, case and column.

Standard library only. Run its tests with:

  python3 -I scripts/docs/test_check_doc_examples.py
"""

from __future__ import annotations

import argparse
import json
import re
import sys
from pathlib import Path

ANCHOR = re.compile(r"\{/\* oracle: (?P<body>[^*]*?) \*/\}")
# Anything that looks like it means to be an oracle anchor. Every match of
# this must also be a match of ANCHOR at the same place.
LOOSE = re.compile(r"\{\s*/\*\s*oracle", re.IGNORECASE)
CASE_ID = re.compile(r"^(?P<fn>[A-Za-z_][A-Za-z0-9_']*)#(?P<index>\d+)$")
SEPARATOR = re.compile(r"^\|(\s*:?-+:?\s*\|)+\s*$")
CELL_SPLIT = re.compile(r"(?<!\\)\|")
INTEGER = re.compile(r"^-?\d+$")

# Vocabularies: the words a column may show for a JSON true, false or null.
NONE: dict = {}
PASSES = {True: {"passes"}, False: {"fails"}}
REFUSED = {None: {"refused"}}

# Per oracle function, `(file, function)`: each column's exact header label
# and vocabulary, inputs in order and then the output. A table may show a
# function only once it has an entry here.
COLUMNS: dict[tuple[str, str], tuple[tuple[str, dict], ...]] = {
    ("Canon.json", "gate"): (
        ("Concept's status", NONE),
        ("Event's from", NONE),
        ("Event's to", NONE),
        ("Result", REFUSED),
    ),
    ("Canon.json", "stage1Passes"): (
        ("GC sweeps survived", NONE),
        ("Dependent interactions", NONE),
        ("Stage 1", PASSES),
    ),
    ("Canon.json", "stage2Passes"): (
        ("Dependent interactions", NONE),
        ("Span of support", NONE),
        ("Capped session extent", NONE),
        ("Stage 2", PASSES),
    ),
}


class InputError(Exception):
    """An input that cannot be read: exit 3."""


def cells(line: str) -> list[str]:
    """The cells of one table row, trimmed."""
    inner = line.strip()
    if inner.startswith("|"):
        inner = inner[1:]
    if inner.endswith("|") and not inner.endswith("\\|"):
        inner = inner[:-1]
    return [c.strip() for c in CELL_SPLIT.split(inner)]


def normalise(cell: str) -> str:
    """A cell without surrounding emphasis or code marks, trimmed."""
    text = cell.strip()
    changed = True
    while changed:
        changed = False
        for mark in ("**", "`"):
            if len(text) >= 2 * len(mark) and text.startswith(mark) and text.endswith(mark):
                text = text[len(mark) : -len(mark)].strip()
                changed = True
    return text


def matches(cell: str, value: object, vocab: dict) -> bool:
    """Whether a table cell shows this JSON value (see the module doc)."""
    text = normalise(cell)
    word = text.casefold()
    if value is True or value is False or value is None:
        return word in vocab.get(value, set())
    if isinstance(value, int):
        return bool(INTEGER.match(text)) and int(text) == value
    if isinstance(value, float):
        try:
            return float(text) == value
        except ValueError:
            return False
    if isinstance(value, str):
        return word == value.casefold()
    return False


def load_oracle(root: Path, name: str, cache: dict) -> dict:
    """`formal/oracle/<name>`, parsed and shape-checked once.

    A missing file is the page's error (ValueError); a file that is not
    JSON or not in the oracle format is an InputError."""
    if name not in cache:
        if "/" in name or "\\" in name or not name.endswith(".json"):
            raise ValueError(f"oracle file {name!r} must be a bare `<Model>.json` name")
        path = root / "formal" / "oracle" / name
        rel = path.relative_to(root)
        try:
            text = path.read_text(encoding="utf-8")
        except FileNotFoundError:
            raise ValueError(f"no oracle file {rel}") from None
        except (OSError, UnicodeDecodeError) as e:
            raise InputError(f"{rel}: cannot be read: {e}") from None
        try:
            data = json.loads(text)
        except json.JSONDecodeError as e:
            raise InputError(f"{rel} is not JSON: {e}") from None
        functions = data.get("functions") if isinstance(data, dict) else None
        if not isinstance(functions, list):
            raise InputError(f"{rel} has no `functions` list")
        for i, f in enumerate(functions):
            if not isinstance(f, dict) or not isinstance(f.get("lean"), str):
                raise InputError(f"{rel}: functions[{i}] has no `lean` name")
            if not isinstance(f.get("cases"), list):
                raise InputError(f"{rel}: {f['lean']} has no `cases` list")
            for j, case in enumerate(f["cases"]):
                if not isinstance(case, dict) or "input" not in case or "output" not in case:
                    raise InputError(f"{rel}: {f['lean']} case {j} lacks `input` or `output`")
        cache[name] = data
    return cache[name]


def find_case(oracle: dict, name: str, case_id: str) -> tuple[str, dict]:
    """The function name and the case `<function>#<index>` of an oracle file."""
    m = CASE_ID.match(case_id)
    if not m:
        raise ValueError(f"case id {case_id!r} is not `<function>#<index>`")
    fn, index = m.group("fn"), int(m.group("index"))
    found = [f for f in oracle["functions"] if f["lean"].rsplit(".", 1)[-1] == fn]
    if len(found) != 1:
        raise ValueError(f"{name} has {len(found)} functions named {fn!r}, expected exactly 1")
    cases = found[0]["cases"]
    if index >= len(cases):
        raise ValueError(f"{name} {fn} has {len(cases)} cases, so no case #{index}")
    return fn, cases[index]


def check_anchor(
    root: Path, rel: str, lines: list[str], at: int, body: str, cache: dict
) -> tuple[list[str], int]:
    """Check one anchor (on line index `at`) and its table. Returns the errors
    and the number of cases checked."""
    where = f"{rel}:{at + 1}"
    tokens = body.split()
    if len(tokens) < 2:
        return [f"{where}: an oracle anchor names a file and at least one case id"], 0
    name, case_ids = tokens[0], tokens[1:]
    try:
        oracle = load_oracle(root, name, cache)
        found = [find_case(oracle, name, cid) for cid in case_ids]
    except ValueError as e:
        return [f"{where}: {e}"], 0
    fns = sorted({fn for fn, _ in found})
    if len(fns) != 1:
        return [f"{where}: {name}: one anchor names one function's cases, not {', '.join(fns)}"], 0
    spec = COLUMNS.get((name, fns[0]))
    if spec is None:
        return [f"{where}: {name} {fns[0]}: no column labels for this function in COLUMNS"], 0
    cases = [case for _, case in found]

    i = at + 1
    while i < len(lines) and not lines[i].strip():
        i += 1
    table = []
    while i < len(lines) and lines[i].lstrip().startswith("|"):
        table.append((i, lines[i]))
        i += 1
    if len(table) < 2 or not SEPARATOR.match(table[1][1].strip()):
        return [f"{where}: {name}: no table (header and separator rows) follows the anchor"], 0
    header = [normalise(c) for c in cells(table[0][1])]
    labels = [label for label, _ in spec]
    if header != labels:
        return [
            f"{rel}:{table[0][0] + 1}: {name} {fns[0]}: the header is {header}, "
            f"but the columns are {labels}"
        ], 0
    rows = table[2:]
    if len(rows) != len(case_ids):
        return [
            f"{where}: {name}: the anchor names {len(case_ids)} case(s) "
            f"but the table has {len(rows)} data row(s)"
        ], 0

    errors = []
    for (line_no, row), cid, case in zip(rows, case_ids, cases):
        here = f"{rel}:{line_no + 1}: {name} {cid}"
        inputs = case["input"] if isinstance(case["input"], list) else [case["input"]]
        values = inputs + [case["output"]]
        shown = cells(row)
        if len(shown) != len(values) or len(spec) != len(values):
            errors.append(
                f"{here}: the row has {len(shown)} cell(s) and the header {len(spec)}, "
                f"but the case has {len(inputs)} input(s) and an output"
            )
            continue
        for col, (cell, value, (label, vocab)) in enumerate(zip(shown, values, spec)):
            role = "output" if col == len(values) - 1 else f"input {col + 1}"
            if not matches(cell, value, vocab):
                errors.append(
                    f"{here}: column {col + 1} ({label!r}) shows {cell!r}, "
                    f"but the case's {role} is {json.dumps(value)}"
                )
    return errors, len(cases)


def read_page(page: Path, rel: str) -> list[str]:
    """A page's lines; an InputError when it is not UTF-8."""
    try:
        return page.read_text(encoding="utf-8").splitlines()
    except UnicodeDecodeError as e:
        raise InputError(f"{rel}: not UTF-8: {e}") from None
    except OSError as e:
        raise InputError(f"{rel}: cannot be read: {e}") from None


def check_page(root: Path, page: Path, cache: dict) -> tuple[list[str], int, int]:
    """Check every oracle anchor of one page: errors, anchors, cases."""
    rel = str(page.relative_to(root)) if page.is_relative_to(root) else str(page)
    lines = read_page(page, rel)
    errors: list[str] = []
    anchors = cases = 0
    for at, line in enumerate(lines):
        strict = {m.start(): m for m in ANCHOR.finditer(line)}
        for loose in LOOSE.finditer(line):
            if loose.start() not in strict:
                errors.append(
                    f"{rel}:{at + 1}: a comment that starts like an oracle anchor is not "
                    f"`{{/* oracle: <file> <case id> ... */}}` on one line"
                )
        for m in strict.values():
            anchors += 1
            if line.strip() != m.group(0):
                errors.append(f"{rel}:{at + 1}: an oracle anchor must be alone on its line")
                continue
            errs, n = check_anchor(root, rel, lines, at, m.group("body"), cache)
            errors.extend(errs)
            cases += n
    return errors, anchors, cases


def check(root: Path, pages: list[Path]) -> tuple[list[str], int, int]:
    """Check the pages: all errors, and how many anchors and cases were read.
    Raises InputError for an unreadable page or a malformed oracle file."""
    cache: dict = {}
    errors: list[str] = []
    anchors = cases = 0
    for page in pages:
        errs, a, c = check_page(root, page, cache)
        errors.extend(errs)
        anchors += a
        cases += c
    return errors, anchors, cases


def default_pages(root: Path) -> list[Path]:
    """The pages checked when none is named."""
    return sorted((root / "site" / "src" / "content" / "docs").glob("*.mdx")) + sorted(
        (root / "docs" / "reference").glob("*.mdx")
    )


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=Path(__file__).resolve().parents[2])
    parser.add_argument("pages", nargs="*", type=Path)
    try:
        args = parser.parse_args(argv)
    except SystemExit as e:
        return 0 if e.code == 0 else 2
    root = args.root.resolve()
    pages = [p.resolve() for p in args.pages] or default_pages(root)
    missing = [p for p in pages if not p.is_file()]
    if missing:
        for p in missing:
            print(f"check-doc-examples: no such page: {p}", file=sys.stderr)
        return 2
    try:
        errors, anchors, cases = check(root, pages)
    except InputError as e:
        print(f"check-doc-examples: {e}", file=sys.stderr)
        return 3
    for e in errors:
        print(e, file=sys.stderr)
    if errors:
        print(f"check-doc-examples: {len(errors)} error(s) in {anchors} oracle anchor(s)", file=sys.stderr)
        return 1
    print(f"check-doc-examples: {anchors} oracle anchor(s), {cases} case(s), all match")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
