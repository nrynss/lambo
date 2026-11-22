#!/usr/bin/env python3
"""Check the docs' source anchors resolve to the Lean, the pins and the code.

Usage:
  python3 -I scripts/docs/check-doc-anchors.py [--root DIR] [PAGE ...]

With no PAGE, every `site/src/content/docs/*.mdx` and `docs/reference/*.mdx`
under the root is checked. The root defaults to the repository this script
is in.

A docs page names the source of a sentence in an invisible MDX comment, an
anchor, written exactly as one of:

    {/* lean: Canon.stage1Passes, Policy.Edge.dependence */}
    {/* const: MIN_GC_SURVIVED, ScoringWeights::default */}
    {/* code: src/canon/stage3.rs stage3_passes, src/config.rs Config::default */}
    {/* test: src/mcp/server/tests/schemas.rs the_router_publishes_exactly_the_seven_spec_tools */}
    {/* oracle: Canon.json stage1Passes#2 */}

The rules:

* `lean:` a comma-separated list of `<Model>.<name>`. `formal/LamboProofs/
  <Model>.lean` must declare `<name>` with `theorem`, `lemma`, `def`,
  `abbrev`, `structure`, `inductive`, `class` or `instance`, and `<name>`
  must equal the declared name exactly once the `Lambo.` and `<Model>.`
  namespace prefixes are dropped: `Canon.Status.rank` matches `def
  Status.rank` inside `namespace Lambo.Canon`, and `Canon.rank` does not.
* `const:` a comma-separated list of names, each pinned by
  `src/formal_constants_tests.rs`: every identifier in the name (split at
  `::`, `.` and `()`) must appear there as a whole word in code, not in a
  comment or a string, and none may be a Rust keyword.
* `code:` paths, each followed by the items it names: groups split by `;`
  or `,`, where a group that starts with a path sets the file and a group
  without one adds items to the last path. Text in parentheses is a note and
  is not checked. Each path must exist under the root. Each item is a Rust
  name that the file declares, outside comments and strings: a `fn`,
  `struct`, `enum`, `const`, `static`, `type`, `trait`, `mod`, `union` or
  `macro_rules!`, a field (`name:` opening a line) or an UpperCamel enum
  variant opening a line. In `A::b`, `A` must be a type, trait or module the
  file declares or implements, and `b` a declaration as above.
* `test:` paths and names, grouped as in `code:`, notes in parentheses
  included. Each path must exist under the root, and each name must be a
  plain Rust identifier that the file declares as a `fn`, outside comments
  and strings, with a test attribute above it: walking up from the `fn`
  over its qualifiers (`pub`, `async`, ...) and the attributes stacked on
  it, no further than ATTR_LINES lines, one attribute's path must have a
  `test` segment (`#[test]`, `#[tokio::test]`, `#[tokio::test(flavor =
  ...)]`). So a helper fn fails, and `#[cfg(test)]`, whose path is `cfg`,
  does not count.
* `oracle:` is checked by check-doc-examples.py; here it only counts.
* Any other comment that starts like an anchor (`{/* lean : ...`,
  `{/*const:`, `{/* Code: ...`, `{/* orcale: ...`, `{/* tests: ...`, one
  split over lines) is an error, not a comment the check skips. A comment
  whose first word is one edit from a kind counts, so `{/* text ...` is
  refused too.
* A page in REQUIRED must carry at least one anchor of each kind it lists,
  so a page cannot lose its anchors and still pass.

Known limits (the check is textual, not a Rust parser):

* In `A::b`, `A` and `b` are found separately, so `b` may be declared on
  another type in the same file: `CanonizationStatus::Dependency` passes
  because `Dependency` is an `EdgeType` variant, and a free function cited as
  a method passes.
* The field and variant rules match a name opening a line, so a struct
  literal's initialiser (`last_demotion_time: Some(..)`) or a line opening
  with `Ok(` or `Some(` passes as a declaration.
* A variant or field that does not open its line (a one-line `enum P { A, B
  }`, or an attribute on the same line) is not found. rustfmt puts each on
  its own line, so this only matters for unformatted code.
* `test:` checks that a name is a test fn in that file, not that the test is
  compiled, run or passing: a test behind a feature or marked `#[ignore]`
  passes. Two fns of one name in one file (in two `mod`s) pass when either
  is a test. A test that a macro generates (`proptest!`, a `macro_rules!`
  that expands to `#[test]`) is not found. Nothing checks that the test
  asserts what the sentence says; that is the writer's job.

Exit codes: 0 when every anchor resolves; 1 when one does not; 2 on a usage
error; 3 when an input cannot be read (a page or source file that is not
UTF-8).

Standard library only. Run its tests with:

  python3 -I scripts/docs/test_check_doc_anchors.py
"""

from __future__ import annotations

import argparse
import re
import sys
from pathlib import Path

KINDS = ("lean", "const", "code", "test", "oracle")
ANCHOR = re.compile(r"\{/\* (?P<kind>lean|const|code|test|oracle): (?P<body>[^*]*?) \*/\}")
# A comment's first word. A comment whose first word is an anchor kind, or
# one edit away from one, must match ANCHOR at the same place.
COMMENT_WORD = re.compile(r"\{\s*/\*\s*(?P<word>[A-Za-z][A-Za-z0-9_]*)")

# Pages that carry anchors today, and the kinds each must keep.
REQUIRED: dict[str, tuple[str, ...]] = {
    "site/src/content/docs/canonization.mdx": ("lean", "const", "code", "oracle"),
    "site/src/content/docs/config.mdx": ("code",),
    "docs/reference/config.mdx": ("code",),
}

CONSTANTS_FILE = "src/formal_constants_tests.rs"
LEAN_DIR = "formal/LamboProofs"

LEAN_DECL = re.compile(
    r"^\s*(?:@\[[^\]]*\]\s*)*"
    r"(?:(?:private|protected|noncomputable|partial|unsafe|nonrec)\s+)*"
    r"(?:theorem|lemma|def|abbrev|structure|inductive|class|instance)\s+"
    r"(?P<name>[^\s:({\[]+)"
)
LEAN_NAMESPACE = re.compile(r"^\s*namespace\s+(?P<name>\S+)")
LEAN_SECTION = re.compile(r"^\s*(?:noncomputable\s+)?section\b")
LEAN_END = re.compile(r"^\s*end(?:\s+(?P<name>\S+))?\s*$")
LEAN_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_']*(\.[A-Za-z_][A-Za-z0-9_']*)+$")
RUST_IDENT = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*$")
RUST_ITEM = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*(::[A-Za-z_][A-Za-z0-9_]*)*$")
CONST_NAME = re.compile(r"^[A-Za-z_][A-Za-z0-9_]*((::|\.)[A-Za-z_][A-Za-z0-9_]*(\(\))?)*$")
PATH_LIKE = re.compile(r"/|\.(rs|lean|toml|json|md|mdx|sh|py)$")
NOTE = re.compile(r"\([^()]+\)")


class InputError(Exception):
    """An input that cannot be read: exit 3."""


def near(word: str, kind: str) -> bool:
    """Whether `word` is `kind`, or one edit (an insertion, a deletion, a
    substitution, or two neighbours swapped) away from it, ignoring case."""
    a, b = word.casefold(), kind
    if a == b:
        return True
    if abs(len(a) - len(b)) > 1:
        return False
    if len(a) == len(b):
        diff = [i for i in range(len(a)) if a[i] != b[i]]
        if len(diff) == 1:
            return True
        return len(diff) == 2 and diff[1] == diff[0] + 1 and a[diff[0]] == b[diff[1]] and a[diff[1]] == b[diff[0]]
    short, long_ = (a, b) if len(a) < len(b) else (b, a)
    return any(long_[:i] + long_[i + 1 :] == short for i in range(len(long_)))


def read_text(path: Path, rel: str) -> str:
    try:
        return path.read_text(encoding="utf-8")
    except UnicodeDecodeError as e:
        raise InputError(f"{rel}: not UTF-8: {e}") from None
    except OSError as e:
        raise InputError(f"{rel}: cannot be read: {e}") from None


def strip_lean_comments(text: str) -> str:
    """Lean source with `--` line comments and nested `/- -/` block comments
    blanked out, newlines kept. A string literal outside a comment is kept
    as it is, so a `/-` or `--` inside one starts no comment."""
    out = []
    depth = 0
    i, n = 0, len(text)
    while i < n:
        two = text[i : i + 2]
        if depth == 0 and text[i] == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            out.append(text[i : j + 1])
            i = j + 1
            continue
        if depth == 0 and two == "--":
            while i < n and text[i] != "\n":
                i += 1
            continue
        if two == "/-":
            depth += 1
            i += 2
            continue
        if depth and two == "-/":
            depth -= 1
            i += 2
            continue
        if depth == 0 or text[i] == "\n":
            out.append(text[i])
        i += 1
    return "".join(out)


def lean_names(text: str) -> set[str]:
    """Every declared name of a Lean file, qualified by its namespaces."""
    names: set[str] = set()
    stack: list[str | None] = []  # a namespace's name, or None for a section
    for line in strip_lean_comments(text).splitlines():
        if m := LEAN_NAMESPACE.match(line):
            stack.append(m.group("name"))
            continue
        if LEAN_SECTION.match(line):
            stack.append(None)
            continue
        if m := LEAN_END.match(line):
            if stack:
                stack.pop()
            continue
        if m := LEAN_DECL.match(line):
            prefix = ".".join(s for s in stack if s)
            name = m.group("name")
            names.add(f"{prefix}.{name}" if prefix else name)
    return names


class Sources:
    """The source files anchors point at, each read once."""

    def __init__(self, root: Path):
        self.root = root
        self._lean: dict[str, set[str] | None] = {}
        self._text: dict[str, str | None] = {}

    def text(self, rel: str) -> str | None:
        if rel not in self._text:
            path = self.root / rel
            self._text[rel] = read_text(path, rel) if path.is_file() else None
        return self._text[rel]

    def lean(self, model: str) -> set[str] | None:
        if model not in self._lean:
            text = self.text(f"{LEAN_DIR}/{model}.lean")
            self._lean[model] = None if text is None else lean_names(text)
        return self._lean[model]


RUST_KEYWORDS = frozenset(
    "as async await break const continue crate dyn else enum extern false fn for if impl in "
    "let loop match mod move mut pub ref return self Self static struct super trait true type "
    "union unsafe use where while".split()
)
RUST_CHAR = re.compile(r"'(?:\\.|\\u\{[0-9A-Fa-f]{1,6}\}|[^'\\\n])'")
RUST_RAW = re.compile(r'b?r(#*)"')
DECL_KW = r"(?:fn|struct|enum|const|static|type|trait|mod|union)"
TYPE_KW = r"(?:struct|enum|trait|type|mod|union)"


def rust_code(text: str) -> str:
    """Rust source with comments and string and char literals blanked,
    newlines kept. Block comments nest, as in Rust, so a declaration inside a
    commented-out block that itself holds a comment stays blanked."""
    out: list[str] = []
    i, n = 0, len(text)

    def blank(j: int) -> int:
        out.append("\n" * text.count("\n", i, j))
        return j

    while i < n:
        two = text[i : i + 2]
        if two == "//":
            j = text.find("\n", i)
            i = blank(n if j < 0 else j)
            continue
        if two == "/*":
            depth, j = 1, i + 2
            while j < n and depth:
                if text.startswith("/*", j):
                    depth, j = depth + 1, j + 2
                elif text.startswith("*/", j):
                    depth, j = depth - 1, j + 2
                else:
                    j += 1
            i = blank(j)
            continue
        if (i == 0 or not (text[i - 1].isalnum() or text[i - 1] == "_")) and (m := RUST_RAW.match(text, i)):
            end = text.find('"' + m.group(1), m.end())
            i = blank(n if end < 0 else end + 1 + len(m.group(1)))
            continue
        if text[i] == '"':
            j = i + 1
            while j < n and text[j] != '"':
                j += 2 if text[j] == "\\" else 1
            i = blank(j + 1)
            continue
        if text[i] == "'" and (m := RUST_CHAR.match(text, i)):
            i = blank(m.end())
            continue
        out.append(text[i])
        i += 1
    return "".join(out)


def has_word(text: str, word: str) -> bool:
    return re.search(rf"(?<![A-Za-z0-9_]){re.escape(word)}(?![A-Za-z0-9_])", text) is not None


def declares(code: str, name: str, owner: bool = False) -> bool:
    """Whether Rust `code` declares `name`. As an owner (the `A` of `A::b`):
    a struct, enum, trait, type alias, module or union, or an `impl` of it.
    Otherwise also a fn, const, static or macro, a field (`name:` opening a
    line) or an enum variant (an UpperCamel `name` opening a line, followed by
    `,`, `(`, `{`, `=` or the line's end)."""
    n = re.escape(name)
    pats = [rf"\b{TYPE_KW}\s+{n}\b", rf"\bimpl\b[^{{;]*?\b(?:for\s+)?{n}\b(?:<[^{{;]*>)?\s*(?:where\b[^{{]*)?\{{"]
    if not owner:
        pats += [
            rf"\b{DECL_KW}\s+{n}\b",
            rf"\bmacro_rules!\s*{n}\b",
            rf"^\s*(?:pub(?:\([^)]*\))?\s+)?{n}\s*:(?!:)",
        ]
        if name[:1].isupper():
            pats.append(rf"^\s*{n}\s*(?:,|\(|\{{|=|$)")
    return any(re.search(p, code, re.MULTILINE) for p in pats)


def lean_short(full: str, model: str) -> str:
    """A declared name without its `Lambo.` and `<Model>.` namespace prefixes."""
    for prefix in ("Lambo.", f"{model}."):
        if full.startswith(prefix):
            full = full[len(prefix) :]
    return full


def check_lean(body: str, sources: Sources) -> list[str]:
    errors = []
    for ref in (r.strip() for r in body.split(",")):
        if not LEAN_NAME.match(ref):
            errors.append(f"lean: {ref!r} is not `<Model>.<name>`")
            continue
        model, name = ref.split(".", 1)
        names = sources.lean(model)
        if names is None:
            errors.append(f"lean: {ref}: no file {LEAN_DIR}/{model}.lean")
        elif name not in {lean_short(n, model) for n in names}:
            errors.append(f"lean: {ref}: {LEAN_DIR}/{model}.lean declares no {name}")
    return errors


def check_const(body: str, sources: Sources) -> list[str]:
    text = sources.text(CONSTANTS_FILE)
    if text is None:
        return [f"const: no file {CONSTANTS_FILE}"]
    errors = []
    for ref in (r.strip() for r in body.split(",")):
        if not CONST_NAME.match(ref):
            errors.append(f"const: {ref!r} is not a Rust name")
            continue
        parts = [p for p in re.split(r"::|\.|\(\)", ref) if p]
        keywords = [p for p in parts if p in RUST_KEYWORDS]
        if keywords:
            errors.append(f"const: {ref}: {', '.join(keywords)} is a Rust keyword, not a name")
            continue
        code = rust_code(text)
        missing = [p for p in parts if not has_word(code, p)]
        if missing:
            errors.append(f"const: {ref}: {CONSTANTS_FILE} does not name {', '.join(missing)}")
    return errors


def path_groups(kind: str, body: str, sources: Sources) -> tuple[list[str], list[tuple[str, list[str]]]]:
    """A `code:` or `test:` body split into groups, notes in parentheses
    dropped: the errors, and each group's (path, items) where the path
    exists. A group that starts with a path sets the file; a group without
    one adds items to the last path."""
    errors: list[str] = []
    out: list[tuple[str, list[str]]] = []
    current: str | None = None
    bare = NOTE.sub(" ", body)
    if "(" in bare.replace("()", "") or ")" in bare.replace("()", ""):
        return [f"{kind}: unbalanced or nested parentheses in {body!r}"], []
    groups = [g.split() for g in re.split(r"[;,]", bare)]
    for tokens in groups:
        if not tokens:
            errors.append(f"{kind}: an empty group in {body!r}")
            continue
        if PATH_LIKE.search(tokens[0]):
            current, items = tokens[0], tokens[1:]
            path = sources.root / current
            if not path.exists():
                errors.append(f"{kind}: no path {current}")
                current = None
                continue
            if items and not path.is_file():
                errors.append(f"{kind}: {current} is not a file, so it names no items")
                continue
        elif current is None:
            errors.append(f"{kind}: {' '.join(tokens)!r} names no path before it")
            continue
        else:
            items = tokens
        out.append((current, items))
    return errors, out


def check_code(body: str, sources: Sources) -> list[str]:
    errors, groups = path_groups("code", body, sources)
    for current, items in groups:
        code = rust_code(sources.text(current) or "") if items else ""
        for item in items:
            if not RUST_ITEM.match(item) or any(p in RUST_KEYWORDS for p in item.split("::")):
                errors.append(f"code: {current}: {item!r} is not a Rust name (put notes in parentheses)")
                continue
            parts = item.split("::")
            missing = [
                p for i, p in enumerate(parts) if not declares(code, p, owner=i < len(parts) - 1)
            ]
            if missing:
                errors.append(f"code: {current} declares no {', '.join(missing)} (for {item})")
    return errors


# How far above a test fn its test attribute may sit, in lines: room for a
# stack such as `#[tokio::test(flavor = ...)]`, `#[ignore]` and `#[serial]`.
ATTR_LINES = 8
FN_DECL = (
    r"(?:\bpub(?:\([^)]*\))?\s+)?(?:\bconst\s+)?(?:\basync\s+)?(?:\bunsafe\s+)?"
    r"(?:\bextern\s+(?:[A-Za-z_][A-Za-z0-9_]*\s+)?)?\bfn\s+{name}\b"
)
ATTR_PATH = re.compile(r"#!?\[\s*(?P<path>(?:[A-Za-z_][A-Za-z0-9_]*\s*::\s*)*[A-Za-z_][A-Za-z0-9_]*)")


def attributes_above(code: str, start: int) -> list[str]:
    """The paths of the attributes stacked directly above position `start`
    of Rust `code` (comments and strings already blanked), nearest first,
    no further up than ATTR_LINES lines."""
    floor = start
    for _ in range(ATTR_LINES + 1):
        floor = code.rfind("\n", 0, floor)
        if floor < 0:
            floor = 0
            break
    paths: list[str] = []
    i = start
    while True:
        while i > floor and code[i - 1].isspace():
            i -= 1
        if i <= floor or code[i - 1] != "]":
            return paths
        depth, j = 0, i - 1
        while j >= floor:
            if code[j] == "]":
                depth += 1
            elif code[j] == "[":
                depth -= 1
                if depth == 0:
                    break
            j -= 1
        if j < floor:
            return paths
        hash_at = j - 2 if j >= 2 and code[j - 1] == "!" else j - 1
        if hash_at < floor or code[hash_at] != "#":
            return paths
        if m := ATTR_PATH.match(code, hash_at):
            paths.append(re.sub(r"\s+", "", m.group("path")))
        i = hash_at


def is_test_attribute(path: str) -> bool:
    """Whether an attribute path names a test: a `test` segment, as in
    `test`, `tokio::test`. `cfg` (of `#[cfg(test)]`) does not."""
    return "test" in path.split("::")


def check_test(body: str, sources: Sources) -> list[str]:
    errors, groups = path_groups("test", body, sources)
    for current, names in groups:
        code = rust_code(sources.text(current) or "") if names else ""
        for name in names:
            if not RUST_IDENT.match(name) or name in RUST_KEYWORDS:
                errors.append(f"test: {current}: {name!r} is not a fn name (put notes in parentheses)")
                continue
            decls = list(re.finditer(FN_DECL.format(name=re.escape(name)), code))
            if not decls:
                errors.append(f"test: {current} declares no fn {name}")
            elif not any(is_test_attribute(p) for d in decls for p in attributes_above(code, d.start())):
                errors.append(
                    f"test: {current}: fn {name} is not a test: no #[test] or #[tokio::test] above it"
                )
    return errors


CHECKS = {"lean": check_lean, "const": check_const, "code": check_code, "test": check_test}


def check_page(root: Path, page: Path, sources: Sources) -> tuple[list[str], dict[str, int]]:
    """Every anchor of one page: the errors, and the anchors of each kind."""
    rel = page.relative_to(root).as_posix() if page.is_relative_to(root) else str(page)
    lines = read_text(page, rel).splitlines()
    errors: list[str] = []
    counts = dict.fromkeys(KINDS, 0)
    for at, line in enumerate(lines):
        strict = {m.start(): m for m in ANCHOR.finditer(line)}
        for c in COMMENT_WORD.finditer(line):
            if c.start() in strict:
                continue
            word = c.group("word")
            if any(near(word, kind) for kind in KINDS):
                errors.append(
                    f"{rel}:{at + 1}: a comment that starts like an anchor ({word!r}) is not "
                    f"`{{/* <kind>: <body> */}}` on one line, with <kind> one of {', '.join(KINDS)}"
                )
        for m in strict.values():
            kind = m.group("kind")
            counts[kind] += 1
            if kind in CHECKS:
                for e in CHECKS[kind](m.group("body"), sources):
                    errors.append(f"{rel}:{at + 1}: {e}")
    for kind in REQUIRED.get(rel, ()):
        if counts[kind] == 0:
            errors.append(f"{rel}: carries no {kind}: anchor, and REQUIRED says it must")
    return errors, counts


def check(root: Path, pages: list[Path]) -> tuple[list[str], dict[str, int]]:
    """Check the pages: all errors, and the anchors of each kind."""
    sources = Sources(root)
    errors: list[str] = []
    totals = dict.fromkeys(KINDS, 0)
    for page in pages:
        errs, counts = check_page(root, page, sources)
        errors.extend(errs)
        for k, v in counts.items():
            totals[k] += v
    return errors, totals


def default_pages(root: Path) -> list[Path]:
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
    named = bool(args.pages)
    pages = [p.resolve() for p in args.pages] or default_pages(root)
    missing = [p for p in pages if not p.is_file()]
    if missing:
        for p in missing:
            print(f"check-doc-anchors: no such page: {p}", file=sys.stderr)
        return 2
    try:
        errors, totals = check(root, pages)
    except InputError as e:
        print(f"check-doc-anchors: {e}", file=sys.stderr)
        return 3
    if not named:
        # A full run also fails when a REQUIRED page has gone.
        for rel in REQUIRED:
            if not (root / rel).is_file():
                errors.append(f"{rel}: REQUIRED to carry anchors, but there is no such page")
    for e in errors:
        print(e, file=sys.stderr)
    summary = ", ".join(f"{totals[k]} {k}" for k in KINDS)
    if errors:
        print(f"check-doc-anchors: {len(errors)} error(s) in anchors ({summary})", file=sys.stderr)
        return 1
    print(f"check-doc-anchors: anchors ({summary}) all resolve")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
