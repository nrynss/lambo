#!/usr/bin/env python3
"""Flag Lean models whose Rust changed since the model was last read (#291).

Usage:
  python3 -I scripts/formal/check-model-drift.py [--strict] [--repo DIR] [--rev REV]
  python3 -I scripts/formal/check-model-drift.py --bump MODEL [MODEL ...] [--repo DIR]
  python3 -I scripts/formal/check-model-drift.py --bump-all [--repo DIR]
  python3 -I scripts/formal/check-model-drift.py --self-test

`formal/models.toml` maps every model file under `formal/LamboProofs/` to the
Rust it models (`rust`, a list of refs) and to `read_digest`, a fingerprint of
that Rust as it was when the model was last read against it: a SHA-256 over
the entry's refs, sorted, each with its normalised code (below), written
`v<N>:sha256:<hex>` where N is the digest scheme (DIGEST_VERSION). A model has
drifted when the fingerprint of its refs now differs from its `read_digest`.

The fingerprint needs no git history, so it holds across squash and rebase
merges and in a shallow clone. A drift warning stays on every run, on every
later commit, until someone re-reads the model and runs `--bump MODEL`;
editing the Lean file alone does not clear it. The ref list is part of the
fingerprint, so dropping a ref, narrowing a whole file to some of its items or
re-pointing a ref at moved code is drift too until the model is bumped. The
Lean path is not part of it: renaming a model file (and its entry) is not.

The check reads the working tree (in CI, the checked-out commit); `--rev REV`
reads that commit's tree instead.

A ref's normalised code:

  * `path/to/file.rs`: the file's code with every comment (doc comments
    included) and every `#[cfg(test)]` item (typically an inline `mod tests`)
    blanked, and whitespace collapsed outside string and char literals
    (literals are kept verbatim). A change to comments, to whitespace between
    tokens or to test-only code alone is therefore not drift.
  * `path/to/file.rs#name`: the same, restricted to the item called `name`:
    every `fn`, `struct`, `enum`, `trait`, `type`, `const`, `static`, `mod`,
    `union` or `macro_rules!` of that name in the file, with the attributes
    above it, and every `impl` block whose self type is `name` (`impl name`,
    `impl Trait for name`, `impl dyn name`). `path#Type::name` restricts it
    to the `fn name` items inside `impl Type` (or `trait Type`) blocks. Test
    code is blanked before items are looked for, so nothing inside
    `#[cfg(test)]` code is ever part of a ref (an item that exists only there
    is not found); every other match counts, so a same-named item elsewhere
    in the file (in a nested non-test `mod`, or a second `impl`) is part of
    the ref too. That can only add warnings. An item ref covers the item's
    own text only: not the `use` imports it relies on, nor the helpers,
    constants or types it calls or names. List those as refs of their own
    when the model depends on them.

`read_against` (optional) records the commit the model was read against. It
is information for people and a hint for the report; only its format is
checked (7 to 64 lowercase hex digits), not that it exists. When a model
drifts, its `read_against` is a commit in this clone and the entry's refs
there fingerprint to exactly its `read_digest` (so it is the state the digest
was taken at), each ref is compared there and now, and only the refs that
changed are reported, each at its first changed line with a `git diff` hint.
Otherwise (no `read_against`, a commit missing from the clone such as a
squash-merged branch's or a commit outside a shallow clone's depth, or a
commit that is not the digest's state) every ref of the model is listed, an item at its
keyword line and a whole file with a file-level annotation. `--bump` does not
change it: update it by hand when you re-read the model.

`--bump MODEL ...` recomputes `read_digest` for the named models (`Canon`,
`Canon.lean` or `formal/LamboProofs/Canon.lean`) from the working tree and
rewrites only those lines of the manifest (inserting the line if it is
missing), leaving the rest of the file byte-for-byte (a symlinked manifest is
written through the link). It notes any ref whose code in the working tree
differs from HEAD, since the digest then records uncommitted edits. `--bump-all`
bumps every model: for bootstrapping, or after a change to this script's
normalisation (which bumps DIGEST_VERSION and so changes every fingerprint). A
bump is refused (exit 3, nothing written) for a model whose Rust refs do not
all resolve, and for any model when the manifest holds a digest of a newer
scheme than this script computes (this script is older than the manifest, so
bumping would downgrade it).

Drift is a warning: the exit code stays 0 unless `--strict` is given, in which
case it is 1.

The manifest itself is checked on every run, and a broken manifest fails
(exit 3) even without `--strict`: every `.lean` file under
`formal/LamboProofs/` (subdirectories included) other than the top-level
`Audit.lean`, the history layers and concrete cases (`Drain/History.lean`,
`Drain/Examples.lean`, `Tiered/History.lean`), the proofs layer of a model in
Spec and Proofs form (`<Model>/Proofs.lean` beside a `<Model>/Spec.lean` or
`<Model>/Spec/`, or `<Model>/Proofs/<X>.lean` beside `<Model>/Spec/<X>.lean`:
its `Spec` files carry the entries) and the shared lemma libraries
under `Lib/` must have exactly one entry, every Lean file and Rust path an
entry names must exist, every `#item` must be found, `read_digest` must be
`v<N>:sha256:` and 64 lowercase hex digits (a bare `sha256:` is scheme 1) with
N this script's DIGEST_VERSION, and `read_against`, when given, a hex commit
id. A digest from an older scheme is reported once, as "digest scheme changed;
run --bump-all", not as drift. A digest from a newer scheme fails the same
way but says this script is older than the manifest, and `--bump` /
`--bump-all` refuse rather than downgrade it.

Under GitHub Actions (`GITHUB_ACTIONS` set) findings are also printed as
workflow commands, escaped as GitHub requires: a drift `::warning` (an
`::error` with `--strict`) for each drifted ref on its Rust file (at the
first changed line when `read_against` narrows the report, else at the item's
keyword line, or on the whole file for a whole-file ref), and an `::error` on
`formal/models.toml` for a broken manifest.

Needs Python 3.11 or later (tomllib) and git.

Exit codes: 0 clean, or drift without --strict; 1 drift with --strict;
2 usage or git error; 3 broken manifest, or a refused --bump.
"""

import argparse
import difflib
import hashlib
import os
import re
import subprocess
import sys
import tempfile
import tomllib

MANIFEST = "formal/models.toml"
MODEL_DIR = "formal/LamboProofs"
# Relative to MODEL_DIR, the Lean files that read no Rust of their own and so
# have no entry: the audit index; the history layers (the drain worker before
# #295 and the replay before #294; the tiered mirror before #319 and the fixes
# it did not take), kept as the evidence for their findings, which docs do not
# cite; and the drain model's concrete cases, evaluated by `decide` over the
# definitions of the other drain files, which have entries.
SKIP_MODELS = {"Audit.lean", "Drain/History.lean", "Drain/Examples.lean", "Tiered/History.lean"}
# Relative to MODEL_DIR, directories of shared lemma libraries: generic facts
# about lists and the like, which describe no Rust.
SKIP_DIRS = ("Lib/",)


def is_proofs_layer(rel, files):
    """Whether `rel` is a proofs file of a model in Spec and Proofs form:
    `<Model>/Proofs.lean` beside a `<Model>/Spec.lean` or a `<Model>/Spec/`
    directory, or `<Model>/Proofs/<X>.lean` beside `<Model>/Spec/<X>.lean` (or
    a single `<Model>/Spec.lean`). It proves the propositions its `Spec` file
    states and reads no Rust of its own: the entry, and the reading of the
    Rust it records, belong to the `Spec` file. `files` is every `.lean` path
    under MODEL_DIR, relative to it: a file named like a proofs layer with no
    such `Spec` beside it is a model file like any other and needs an entry."""
    parts = rel.split("/")
    if len(parts) >= 2 and parts[-1] == "Proofs.lean":
        model = "/".join(parts[:-1])
        return f"{model}/Spec.lean" in files or any(f.startswith(f"{model}/Spec/") for f in files)
    if len(parts) >= 3 and parts[-2] == "Proofs":
        model = "/".join(parts[:-2])
        return f"{model}/Spec/{parts[-1]}" in files or f"{model}/Spec.lean" in files
    return False


def is_model(rel, files):
    """Whether `rel`, a `.lean` path relative to MODEL_DIR, needs an entry;
    `files` is every `.lean` path there (`is_proofs_layer`)."""
    return rel not in SKIP_MODELS and not rel.startswith(SKIP_DIRS) and not is_proofs_layer(rel, files)


ENTRY_KEYS = {"lean", "rust", "read_against", "read_digest"}
ITEM_KINDS = r"(?:fn|struct|enum|trait|type|const|static|mod|union)"
IDENT = r"[A-Za-z_][A-Za-z0-9_]*"
# The digest scheme. Bump it whenever the normalisation or the digest layout
# changes (every read_digest changes with it), and record the new golden digest
# in GOLDEN_DIGESTS; the self-test fails until both are done.
DIGEST_VERSION = 2
DIGEST_RE = r"(?:v([1-9][0-9]*):)?sha256:[0-9a-f]{64}"  # no `vN:` is scheme 1
# The digest of the self-test's fixed input (`golden` in self_test) under each
# scheme.
GOLDEN_DIGESTS = {
    2: "v2:sha256:d1386f020d10fdf96245dc43dc9c90b67996f773f4916c26b03f43d42e807f2f",
}
# A keyword that starts an item (or a `let` statement) after attributes: what
# a `#[cfg(test)]` before it covers ends at its `;` or body, never at a comma.
ITEM_WORDS = {"fn", "struct", "enum", "impl", "mod", "const", "static", "type", "trait", "use",
              "union", "extern", "macro_rules", "let"}
# Of those, the ones that end only at a `;` (`const X: u32 = { 1 } + 2;`).
SEMI_WORDS = {"static", "type", "use", "let"}
BUMP_CMD = "python3 -I scripts/formal/check-model-drift.py --bump"

EXIT_OK, EXIT_DRIFT, EXIT_USAGE, EXIT_MANIFEST = 0, 1, 2, 3


class GitError(Exception):
    pass


# --------------------------------------------------------------------------
# sources: the working tree, or a commit


class Repo:
    def __init__(self, path):
        self.path = path
        self._blobs = {}
        self._trees = {}

    def run(self, *args, check=True):
        try:
            p = subprocess.run(["git", "-C", self.path, *args], capture_output=True)
        except OSError as e:
            raise GitError(f"git {' '.join(args)}: {e}") from e
        if check and p.returncode != 0:
            raise GitError(f"git {' '.join(args)}: {p.stderr.decode(errors='replace').strip()}")
        return p

    def commit(self, rev):
        """The full commit id `rev` names, or None (also when git is not
        available or the directory is not a repository)."""
        try:
            p = self.run("rev-parse", "--verify", "--quiet", "--end-of-options",
                         f"{rev}^{{commit}}", check=False)
        except GitError:
            return None
        return p.stdout.decode().strip() if p.returncode == 0 else None

    def show(self, rev, path):
        """The text of `path` at `rev`, or None when it does not exist there."""
        key = (rev, path)
        if key not in self._blobs:
            blob = self.tree(rev).get(path)
            if blob is None:
                self._blobs[key] = None
            else:
                p = self.run("cat-file", "blob", blob, check=False)
                self._blobs[key] = p.stdout.decode("utf-8", errors="replace") if p.returncode == 0 else None
        return self._blobs[key]

    def tree(self, rev):
        """{path: blob id} for every file at `rev`."""
        if rev not in self._trees:
            out = self.run("ls-tree", "-r", "-z", "--full-tree", rev).stdout.decode("utf-8", errors="replace")
            files = {}
            for rec in out.split("\0"):
                if not rec:
                    continue
                meta, _, path = rec.partition("\t")
                parts = meta.split()
                if len(parts) == 3 and parts[1] == "blob":
                    files[path] = parts[2]
            self._trees[rev] = files
        return self._trees[rev]


class WorkTree:
    """Files as they are on disk under `root`."""

    def __init__(self, root):
        self.root = root
        self.label = "the working tree"
        self._text = {}

    def read(self, path):
        if path not in self._text:
            try:
                with open(os.path.join(self.root, *path.split("/")), "rb") as f:
                    self._text[path] = f.read().decode("utf-8", errors="replace")
            except OSError:
                self._text[path] = None
        return self._text[path]

    def model_files(self):
        out = set()
        top = os.path.join(self.root, *MODEL_DIR.split("/"))
        for dirpath, dirnames, filenames in os.walk(top):
            dirnames[:] = sorted(d for d in dirnames if not d.startswith("."))
            rel = os.path.relpath(dirpath, self.root).replace(os.sep, "/")
            for f in filenames:
                if f.endswith(".lean"):
                    out.add(f"{rel}/{f}")
        files = {p[len(MODEL_DIR) + 1:] for p in out}
        return {p for p in out if is_model(p[len(MODEL_DIR) + 1:], files)}


class GitTree:
    """Files as they are in commit `rev`."""

    def __init__(self, repo, rev):
        self.repo, self.rev = repo, rev
        self.label = rev[:12]

    def read(self, path):
        return self.repo.show(self.rev, path)

    def model_files(self):
        prefix = MODEL_DIR + "/"
        lean = {p for p in self.repo.tree(self.rev) if p.startswith(prefix) and p.endswith(".lean")}
        files = {p[len(prefix):] for p in lean}
        return {p for p in lean if is_model(p[len(prefix):], files)}


# --------------------------------------------------------------------------
# Rust: comments out, then items


def lex(src):
    """Per-character class of `src`: 'c' code, 'm' comment, 's' string or char
    literal. Handles nested block comments, raw strings and lifetimes."""
    n = len(src)
    cls = ["c"] * n
    i = 0

    def mark(a, b, k):
        for j in range(a, min(b, n)):
            cls[j] = k

    def ident_before(k):
        return k > 0 and (src[k - 1].isalnum() or src[k - 1] == "_")

    while i < n:
        ch = src[i]
        if src.startswith("//", i):
            j = src.find("\n", i)
            j = n if j == -1 else j
            mark(i, j, "m")
            i = j
            continue
        if src.startswith("/*", i):
            depth, j = 0, i
            while j < n:
                if src.startswith("/*", j):
                    depth += 1
                    j += 2
                elif src.startswith("*/", j):
                    depth -= 1
                    j += 2
                    if depth == 0:
                        break
                else:
                    j += 1
            mark(i, j, "m")
            i = j
            continue
        m = re.match(r'(?:b|c)?r(#*)"', src[i:i + 300])
        if m and not ident_before(i):
            close = '"' + m.group(1)
            j = src.find(close, i + len(m.group(0)))
            j = n if j == -1 else j + len(close)
            mark(i, j, "s")
            i = j
            continue
        if ch == '"' or (ch in "bc" and src.startswith('"', i + 1) and not ident_before(i)):
            j = i + (1 if ch == '"' else 2)
            while j < n and src[j] != '"':
                j += 2 if src[j] == "\\" else 1
            j += 1
            mark(i, j, "s")
            i = j
            continue
        if ch == "'":
            if src.startswith("\\", i + 1):
                j = i + 3 if src.startswith("'", i + 2) else src.find("'", i + 2)
                j = n if j == -1 else j + 1
                mark(i, j, "s")
                i = j
                continue
            if i + 2 < n and src[i + 2] == "'":
                mark(i, i + 3, "s")
                i += 3
                continue
            if i + 1 < n and ord(src[i + 1]) > 127:
                k = src.find("'", i + 1)
                if k != -1 and k - i <= 3:
                    mark(i, k + 1, "s")
                    i = k + 1
                    continue
        i += 1
    return "".join(cls)


def collapse(text, cls):
    """`text` with each whitespace run outside string and char literals (per
    `cls`, its classes from `lex`) collapsed to one space, and stripped at
    either end where that end is not inside a literal; literals verbatim (a
    multi-line literal keeps the whitespace at its line ends)."""
    segs, pos = [], 0  # [text, is a literal]
    for m in re.finditer(r"s+", cls):
        segs.append([re.sub(r"\s+", " ", text[pos:m.start()]), False])
        segs.append([text[m.start():m.end()], True])
        pos = m.end()
    segs.append([re.sub(r"\s+", " ", text[pos:]), False])
    if not segs[0][1]:
        segs[0][0] = segs[0][0].lstrip()
    if not segs[-1][1]:
        segs[-1][0] = segs[-1][0].rstrip()
    return "".join(t for t, _ in segs)


def impl_self_type(header):
    """The self type's name in an `impl` header (the text between `impl` and
    its `{`), or None. Strips the impl's generics, its `where` clause and a
    trait (`Trait for`), keeping `for<'a>` binders out of the split."""
    header = re.sub(r"^\s*<", "<", header)
    if header.startswith("<"):  # impl generics
        depth, p = 0, 0
        for p, c in enumerate(header):
            if c == "<":
                depth += 1
            elif c == ">" and header[p - 1] != "-":
                depth -= 1
                if depth == 0:
                    break
        header = header[p + 1:]
    header = re.split(r"\bwhere\b", header)[0]
    # `Trait for Type`; a `for<'a>` higher-ranked binder is not that `for`.
    self_ty = re.split(r"\bfor\b(?!\s*<)", header)[-1].strip()
    tm = re.match(r"(?:&\s*(?:'\w+\s+)?(?:mut\s+)?)?(?:dyn\s+)?((?:\w+::)*)(\w+)", self_ty)
    return tm.group(2) if tm else None


class Rust:
    """One Rust source, with comments blanked (`code`) and with comments and
    literals blanked (`bare`, for brace matching), and `cls`, each character's
    class ('c' code, 'm' comment or test code, 's' literal). Offsets agree."""

    def __init__(self, src):
        cls = lex(src)
        self.code = "".join(c if k != "m" or c == "\n" else " " for c, k in zip(src, cls))
        self.bare = "".join(c if k == "c" or c == "\n" else " " for c, k in zip(src, cls))
        self.cls = cls
        # Test-only code (`#[cfg(test)]` items, typically `mod tests`) is not
        # what a model describes: blank it too, before any item is looked for.
        tests = []
        for m in re.finditer(r"#\[\s*cfg\s*\(\s*test\s*\)\s*\]", self.bare):
            word, at = self._head(m.end())
            tests.append((m.start(), self._end(at, self._mode(word, at, test=True))))
        for a, b in tests:
            self.code = self._blank(self.code, a, b)
            self.bare = self._blank(self.bare, a, b)
            self.cls = self.cls[:a] + "m" * (b - a) + self.cls[b:]

    @staticmethod
    def _blank(text, a, b):
        return text[:a] + re.sub(r"[^\n]", " ", text[a:b]) + text[b:]

    def norm(self, spans=None):
        """The code of `spans` (default: the whole file), whitespace collapsed
        outside literals."""
        if spans is None:
            spans = [(0, len(self.code))]
        text = "\n".join(self.code[a:b] for a, b in spans)
        cls = "c".join(self.cls[a:b] for a, b in spans)
        return collapse(text, cls)

    def line_of(self, offset):
        return self.bare.count("\n", 0, offset) + 1

    def lines(self, spans=None):
        """[(line number, the line's code with whitespace collapsed outside
        literals)] for every non-blank line of `spans` (default: the whole
        file)."""
        out = []
        for a, b in spans if spans is not None else [(0, len(self.code))]:
            first, pos = self.line_of(a), a
            for k, raw in enumerate(self.code[a:b].split("\n")):
                text = collapse(raw, self.cls[pos:pos + len(raw)])
                pos += len(raw) + 1
                if text:
                    out.append((first + k, text))
        return out

    def _close(self, i):
        """Offset just past the bracket that closes the one at `i`."""
        depth, n = 0, len(self.bare)
        while i < n:
            c = self.bare[i]
            if c in "([{":
                depth += 1
            elif c in ")]}":
                depth -= 1
                if depth == 0:
                    return i + 1
            i += 1
        return n

    def _head(self, i):
        """(word, offset) of what follows `i`, past whitespace, attributes,
        visibility (`pub`, `pub(crate)`) and qualifiers (`async`, `unsafe`,
        `default`, `safe`): the word (`macro_rules` for `macro_rules!`), or ""
        when no identifier is next."""
        bare, n = self.bare, len(self.bare)
        word_re = re.compile(IDENT)
        while True:
            while i < n and bare[i].isspace():
                i += 1
            if bare.startswith("#", i):
                j = i + 1 + (bare.startswith("!", i + 1))
                if bare.startswith("[", j):
                    i = self._close(j)
                    continue
            m = word_re.match(bare, i)
            if not m:
                return "", i
            w, k = m.group(0), m.end()
            while k < n and bare[k].isspace():
                k += 1
            if w == "pub":
                i = self._close(k) if bare.startswith("(", k) else m.end()
                continue
            if w in ("async", "unsafe", "default", "safe") and word_re.match(bare, k):
                i = k
                continue
            return w, m.start()

    def _mode(self, word, at, test=False):
        """How the item or element whose first word `word` is at `at` ends (for
        `_end`): "item" (its body's `}` or a `;`), "semi" (a `;` only), and for
        what a `#[cfg(test)]` covers when it is not an item, "field" (`name:
        ...,`, a field or a struct-expression field, even one named by a weak
        keyword such as `union`) or "comma" (a match arm, an argument, a
        statement, a tuple field, a `fn(..)` pointer type)."""
        ahead = self.bare[at:at + 200]
        if test and re.match(rf"{IDENT}\s*:(?!:)", ahead):
            return "field"
        if test and re.match(r"(?:extern\s+)?fn\s*\(", ahead):
            return "comma"
        if word == "const":
            m = re.match(rf"const\s+({IDENT})", self.bare[at:at + 200])
            return "item" if m and m.group(1) in ("fn", "unsafe", "async", "extern") else "semi"
        if word in SEMI_WORDS:
            return "semi"
        if word in ITEM_WORDS or not test:
            return "item"
        return "comma"

    def _end(self, start, mode="item", angles=True):
        """End of what starts at `start`, the end of the enclosing block at the
        latest. "item": the `}` that closes its body (the first `{` outside the
        header's angle brackets, so `impl Foo<{ N }> {` is one header) or a `;`
        at depth 0. "semi": a `;` at depth 0. "comma": a `,` or `;` at depth 0
        or a `}` back to depth 0. "field": as "comma", but a `,` inside a
        type's angle brackets does not end it.

        A `<` opens an angle bracket only right after an identifier or `::`
        and not before `=` or `<` (so `n <= LIMIT`, `x < y` and `1 << 3` do
        not). If the angle brackets are still open when the enclosing block
        or the file ends, the scan is redone without them ("field" as
        "comma"): when unsure, the span ends early, since test code left in
        only adds warnings while real code blanked would hide drift."""
        bare, n = self.bare, len(self.bare)
        depth, angle, i = 0, 0, start
        angles = angles and mode in ("item", "field")
        while i < n:
            c = bare[i]
            if c in "([{":
                depth += 1
            elif c in ")]}":
                depth -= 1
                if depth < 0:
                    break
                if c == "}" and depth == 0 and angle == 0 and mode != "semi":
                    return i + 1
            elif depth == 0:
                if c == ";":
                    return i + 1
                if c == "," and angle == 0 and mode in ("comma", "field"):
                    return i + 1
                if angles and c == "<":
                    if ((bare[i - 1:i].isalnum() or bare[i - 1:i] == "_" or bare[i - 2:i] == "::")
                            and bare[i + 1:i + 2] not in ("=", "<")):
                        angle += 1
                elif angles and c == ">" and angle > 0 and bare[i - 1] not in "-=":
                    angle -= 1
            i += 1
        if angle > 0:
            return self._end(start, "comma" if mode == "field" else mode, angles=False)
        return i

    def _with_attrs(self, start):
        """Move `start` back to its line's start (over `pub` and the like) and
        then over the outer attributes above it, however many lines each
        spans. Comments are blank in `bare`, so doc comments are skipped as
        whitespace."""
        bare = self.bare
        start = bare.rfind("\n", 0, start) + 1
        while True:
            j = start - 1
            while j >= 0 and bare[j].isspace():
                j -= 1
            if j < 0 or bare[j] != "]":
                return start
            # the `[` that opens this `]`
            depth, k = 0, j
            while k >= 0:
                if bare[k] in ")]}":
                    depth += 1
                elif bare[k] in "([{":
                    depth -= 1
                    if depth == 0:
                        break
                k -= 1
            if k <= 0 or bare[k] != "[" or bare[k - 1] != "#":
                return start
            hash_at = k - 1
            line = bare.rfind("\n", 0, hash_at) + 1
            start = line if not bare[line:hash_at].strip() else hash_at

    def _match(self, keyword_at):
        """(start with attributes, end, keyword offset) of the item whose
        keyword is at `keyword_at`."""
        word = re.match(IDENT, self.bare[keyword_at:keyword_at + 40])
        mode = self._mode(word.group(0) if word else "", keyword_at)
        return (self._with_attrs(keyword_at), self._end(keyword_at, mode), keyword_at)

    def _impls(self, name):
        """Matches of `impl` blocks whose self type is `name`, and of `trait name`."""
        out = []
        for m in re.finditer(r"\bimpl\b", self.bare):
            j = m.end()
            # header runs to the first `{` or `;` outside angle brackets
            k, angle = j, 0
            while k < len(self.bare) and not (angle == 0 and self.bare[k] in "{;"):
                if self.bare[k] == "<":
                    angle += 1
                elif self.bare[k] == ">" and self.bare[k - 1] != "-":
                    angle -= 1
                k += 1
            if impl_self_type(self.bare[j:k]) == name:
                out.append(self._match(m.start()))
        for m in re.finditer(rf"\btrait\s+{re.escape(name)}\b", self.bare):
            out.append(self._match(m.start()))
        return out

    def item_matches(self, item):
        """[(start, end, keyword offset)] for every match of `item`, sorted,
        one per span; empty when it is not found."""
        found = []
        if "::" in item:
            owner, name = item.rsplit("::", 1)
            owner = owner.rsplit("::", 1)[-1]
            for a, b, _ in self._impls(owner):
                for m in re.finditer(rf"\bfn\s+{re.escape(name)}\b", self.bare[a:b]):
                    found.append(self._match(a + m.start()))
        else:
            pat = rf"\b{ITEM_KINDS}\s+{re.escape(item)}\b|\bmacro_rules!\s*{re.escape(item)}\b"
            found = [self._match(m.start()) for m in re.finditer(pat, self.bare)]
            found += self._impls(item)
        spans = {}
        for a, b, kw in found:
            spans[(a, b)] = min(kw, spans.get((a, b), kw))
        return sorted((a, b, kw) for (a, b), kw in spans.items())

    def item_spans(self, item):
        """Every span `item` names; empty when it is not found."""
        return [(a, b) for a, b, _ in self.item_matches(item)]


def first_changed_line(old, old_spans, new, new_spans):
    """The first line of `new_spans` in `new` that differs from `old_spans` in
    `old`, comparing comment-free lines with whitespace collapsed; None when
    the lines agree or `new_spans` has none."""
    old_lines = [t for _, t in old.lines(old_spans)]
    new_lines = new.lines(new_spans)
    sm = difflib.SequenceMatcher(None, old_lines, [t for _, t in new_lines], autojunk=False)
    for tag, _i1, _i2, j1, _j2 in sm.get_opcodes():
        if tag != "equal" and new_lines:
            return new_lines[min(j1, len(new_lines) - 1)][0]
    return None


class Snapshot:
    """The Rust refs as one source (the working tree, or a commit) has them."""

    def __init__(self, source):
        self.source = source
        self._rust = {}

    def rust(self, path):
        """The parsed file, or None when it does not exist."""
        if path not in self._rust:
            text = self.source.read(path)
            self._rust[path] = None if text is None else Rust(text)
        return self._rust[path]

    def resolve(self, ref):
        """(Rust, spans) for `ref`: spans None for a whole file; None when the
        file or the item is not there."""
        path, item = split_ref(ref)
        r = self.rust(path)
        if r is None:
            return None
        if item is None:
            return r, None
        spans = r.item_spans(item)
        return (r, spans) if spans else None

    def code(self, ref):
        """`ref`'s normalised code, or None when it is not there."""
        got = self.resolve(ref)
        return None if got is None else got[0].norm(got[1])

    def keyword_line(self, ref):
        """The line of `ref`'s first item keyword; None for a whole file."""
        path, item = split_ref(ref)
        r = self.rust(path)
        if r is None or item is None:
            return None
        m = r.item_matches(item)
        return r.line_of(m[0][2]) if m else None


def fingerprint(refs, snap):
    """`read_digest` for `refs` as `snap` has them. Every ref must resolve."""
    h = hashlib.sha256(f"lambo-model-drift/{DIGEST_VERSION}".encode() + b"\0")
    for ref in sorted(refs):
        code = snap.code(ref)
        if code is None:
            raise ValueError(f"{ref} does not resolve")
        for part in (ref, code):
            b = part.encode("utf-8")
            h.update(len(b).to_bytes(8, "big"))
            h.update(b)
    return f"v{DIGEST_VERSION}:sha256:{h.hexdigest()}"


def digest_scheme(digest):
    """The scheme number of a well-formed `read_digest` (1 when it has no `vN:`)."""
    m = re.fullmatch(DIGEST_RE, digest)
    return int(m.group(1) or 1)


# --------------------------------------------------------------------------
# manifest


def split_ref(ref):
    path, _, item = ref.partition("#")
    return path, item or None


def model_name(lean):
    """`Canon` for `formal/LamboProofs/Canon.lean`: what `--bump` takes."""
    prefix = MODEL_DIR + "/"
    if lean.startswith(prefix) and lean.endswith(".lean"):
        return lean[len(prefix):-len(".lean")]
    return lean


def load_manifest(text, where):
    """Parse the manifest. Returns (entries by Lean path, problems); a problem
    is (Lean path or None, kind, message), kind one of "toml" (the file as a
    whole), "global", "entry", "ref", "digest", "scheme" (digests of an older
    digest scheme, one problem for all of them) and "scheme-newer" (of a newer
    one: this script is older than the manifest)."""
    problems = []
    try:
        data = tomllib.loads(text)
    except tomllib.TOMLDecodeError as e:
        return {}, [(None, "toml", f"{where}: not valid TOML: {e}")]
    extra = set(data) - {"model"}
    if extra:
        problems.append((None, "global", f"{where}: unknown top-level keys {sorted(extra)}"))
    entries = {}
    stale_scheme = {}  # {scheme: [lean]} for digests of another scheme
    models = data.get("model", [])
    if not isinstance(models, list):
        return {}, problems + [(None, "toml", f"{where}: `model` must be an array of tables ([[model]])")]
    for k, e in enumerate(models):
        label = f"{where}: [[model]] #{k + 1}"
        if not isinstance(e, dict):
            problems.append((None, "toml", f"{label} is not a table"))
            continue
        lean = e.get("lean")
        if not isinstance(lean, str) or not lean:
            problems.append((None, "toml", f"{label}: `lean` must be a path"))
            continue
        label = f"{where}: {lean}"
        unknown = set(e) - ENTRY_KEYS
        if unknown:
            problems.append((lean, "entry", f"{label}: unknown keys {sorted(unknown)}"))
        rust = e.get("rust")
        if not isinstance(rust, list) or not rust or not all(isinstance(r, str) and r for r in rust):
            problems.append((lean, "entry", f"{label}: `rust` must be a non-empty list of paths"))
            rust = []
        if len(set(rust)) != len(rust):
            problems.append((lean, "entry", f"{label}: `rust` lists a path twice"))
        ra = e.get("read_against")
        if ra is not None and (not isinstance(ra, str) or not re.fullmatch(r"[0-9a-f]{7,64}", ra)):
            problems.append((lean, "entry", f"{label}: `read_against`, when given, must be a commit "
                                            f"id (7 to 64 lowercase hex digits)"))
            ra = None
        digest = e.get("read_digest")
        if digest is None:
            problems.append((lean, "digest", f"{label}: no `read_digest`; re-read the model and run "
                                             f"`{BUMP_CMD} {model_name(lean)}`"))
        elif not isinstance(digest, str) or not re.fullmatch(DIGEST_RE, digest):
            problems.append((lean, "digest", f"{label}: `read_digest` must be `v{DIGEST_VERSION}:sha256:` "
                                             f"and 64 lowercase hex digits; re-read the model and "
                                             f"run `{BUMP_CMD} {model_name(lean)}`"))
            digest = None
        elif digest_scheme(digest) != DIGEST_VERSION:
            stale_scheme.setdefault(digest_scheme(digest), []).append(lean)
        if lean in entries:
            problems.append((lean, "entry", f"{label}: listed twice"))
        entries[lean] = {"lean": lean, "rust": list(rust), "read_against": ra, "read_digest": digest}
    for newer in (False, True):
        group = {k: v for k, v in stale_scheme.items() if (k > DIGEST_VERSION) == newer}
        if not group:
            continue
        n = sum(len(v) for v in group.values())
        schemes = ", ".join(f"v{k}" for k in sorted(group))
        head = (f"{where}: {plural(n, 'read_digest')} use{'s' if n == 1 else ''} scheme {schemes} and "
                f"this script computes v{DIGEST_VERSION}, so they cannot be compared "
                f"({', '.join(model_name(x) for v in group.values() for x in v)}).")
        if newer:
            problems.append((None, "scheme-newer",
                             f"{where}: this script is older than the manifest's digest scheme: {head[len(where) + 2:]} "
                             f"Run the check-model-drift.py from the same commit as {MANIFEST}; do not bump "
                             f"with this one."))
        else:
            problems.append((None, "scheme",
                             f"{where}: digest scheme changed: {head[len(where) + 2:]} In the change that "
                             f"updates the script, first run the previous script to confirm no model has "
                             f"drifted, then run `python3 -I scripts/formal/check-model-drift.py --bump-all`"))
    return entries, problems


def check_manifest(source):
    """(entries, problems, Snapshot of `source`) for the manifest `source` has."""
    snap = Snapshot(source)
    text = source.read(MANIFEST)
    if text is None:
        return {}, [(None, "toml", f"{MANIFEST} does not exist in {source.label}")], snap
    entries, problems = load_manifest(text, MANIFEST)
    models = source.model_files()
    for m in sorted(models - set(entries)):
        problems.append((None, "global", f"{MANIFEST}: no entry for {m}"))
    for lean, e in sorted(entries.items()):
        if lean not in models:
            if source.read(lean) is None:
                problems.append((lean, "entry", f"{MANIFEST}: {lean} does not exist"))
            else:
                problems.append((lean, "entry", f"{MANIFEST}: {lean} is not a model file under {MODEL_DIR}/"))
        for ref in e["rust"]:
            path, item = split_ref(ref)
            parts = path.split("/")
            if path.startswith("/") or "\\" in path or any(p in ("", ".", "..") for p in parts):
                problems.append((lean, "ref", f"{MANIFEST}: {lean}: {path} is not a relative path "
                                              f"inside the repository"))
                continue
            if not path.endswith(".rs"):
                problems.append((lean, "ref", f"{MANIFEST}: {lean}: {path} is not a Rust file"))
                continue
            if snap.rust(path) is None:
                problems.append((lean, "ref", f"{MANIFEST}: {lean}: {path} does not exist"))
                continue
            if item is not None:
                if not re.fullmatch(rf"{IDENT}(?:::{IDENT})*", item):
                    problems.append((lean, "ref", f"{MANIFEST}: {lean}: {ref}: `#{item}` is not a Rust name"))
                elif snap.resolve(ref) is None:
                    problems.append((lean, "ref", f"{MANIFEST}: {lean}: {ref}: no item `{item}` in {path}"))
    return entries, problems, snap


def _split_eol(line):
    for eol in ("\r\n", "\n", "\r"):
        if line.endswith(eol):
            return line[:-len(eol)], eol
    return line, ""


def _value(line):
    """The value of a one-line `key = value`, or None."""
    try:
        d = tomllib.loads(line)
    except tomllib.TOMLDecodeError:
        return None
    return next(iter(d.values())) if len(d) == 1 else None


def entry_lines(lines):
    """For each [[model]] table in `lines`, in order: {"lean": its value or
    None, and the index in `lines` of its "lean", "read_against" and
    "read_digest" lines (None when absent)}."""
    blocks, cur = [], None
    for i, raw in enumerate(lines):
        s = _split_eol(raw)[0].strip()
        if re.fullmatch(r"\[\[\s*model\s*\]\]\s*(?:#.*)?", s):
            cur = {"lean": None, "lean_line": None, "read_against": None, "read_digest": None}
            blocks.append(cur)
            continue
        if re.fullmatch(r"\[\[?\s*[^\[\]=\"']+\]\]?\s*(?:#.*)?", s):
            cur = None  # another table
            continue
        m = re.match(r"(lean|read_against|read_digest)\s*=", s)
        if cur is None or not m:
            continue
        key = m.group(1)
        if key == "lean":
            cur["lean"], cur["lean_line"] = _value(s), i
        else:
            cur[key] = i
    return blocks


def edit_digests(text, digests):
    """`text` with each {lean: digest} written into its entry's read_digest
    line (a new line after `read_against`, else after `lean`, when there is
    none), every other byte unchanged. Raises ValueError when it cannot."""
    lines = text.splitlines(keepends=True)
    nl = "\r\n" if "\r\n" in text else "\n"
    by_lean = {}
    for b in entry_lines(lines):
        if b["lean"] in digests:
            if b["lean"] in by_lean:
                raise ValueError(f"{b['lean']} has two [[model]] tables")
            by_lean[b["lean"]] = b
    missing = sorted(set(digests) - set(by_lean))
    if missing:
        raise ValueError(f"no `lean = ...` line found for {', '.join(missing)}")
    after = {}
    for lean, digest in digests.items():
        b = by_lean[lean]
        if b["read_digest"] is not None:
            i = b["read_digest"]
            body, eol = _split_eol(lines[i])
            m = re.fullmatch(r"""(\s*read_digest\s*=\s*)("[^"\\]*"|'[^']*')(.*)""", body)
            if not m:
                raise ValueError(f"cannot rewrite the read_digest line of {lean}: {body.strip()!r}")
            lines[i] = f'{m.group(1)}"{digest}"{m.group(3)}{eol}'
        else:
            i = b["read_against"] if b["read_against"] is not None else b["lean_line"]
            body, eol = _split_eol(lines[i])
            if not eol:  # the file's last line, with no newline: so is the new one
                lines[i] = body + nl
            indent = re.match(r"\s*", body).group(0)
            after[i] = f'{indent}read_digest = "{digest}"{eol}'
    out = []
    for i, line in enumerate(lines):
        out.append(line)
        if i in after:
            out.append(after[i])
    return "".join(out)


def uncommitted_refs(root, entries, targets, snap):
    """{Rust path: {model names}} for the refs of `targets` whose normalised
    code in the working tree (`snap`) differs from HEAD's, or that HEAD does
    not have; empty when `root` has no HEAD."""
    repo = Repo(root)
    head = repo.commit("HEAD")
    if head is None:
        return {}
    at_head = Snapshot(GitTree(repo, head))
    out = {}
    try:
        for lean in targets:
            for ref in entries[lean]["rust"]:
                if at_head.code(ref) != snap.code(ref):
                    out.setdefault(split_ref(ref)[0], set()).add(model_name(lean))
    except GitError:
        return {}
    return out


def bump(root, names, everything):
    """--bump / --bump-all: rewrite read_digest for the named models."""
    source = WorkTree(root)
    # A symlinked manifest is written through the link, which stays a link.
    path = os.path.realpath(os.path.join(root, *MANIFEST.split("/")))
    try:
        with open(path, "rb") as f:
            raw = f.read()
        text = raw.decode("utf-8")
    except (OSError, UnicodeDecodeError) as e:
        print(f"error: cannot read {MANIFEST}: {e}", file=sys.stderr)
        return EXIT_MANIFEST
    entries, problems, snap = check_manifest(source)
    # A digest of a newer scheme means this script is older than the manifest:
    # bumping with it would silently downgrade the digests.
    fatal = [msg for lean, kind, msg in problems if kind in ("toml", "scheme-newer")]
    if fatal:
        print(f"refused: {MANIFEST} is broken:")
        for msg in fatal:
            print(f"  {msg}")
        return EXIT_MANIFEST
    if everything:
        targets = sorted(entries)
    else:
        by_name = {}
        for lean in entries:
            for key in (lean, model_name(lean), model_name(lean) + ".lean"):
                by_name.setdefault(key, lean)
        unknown = [n for n in names if n not in by_name]
        if unknown:
            print(f"error: no model {', '.join(repr(n) for n in unknown)} in {MANIFEST}; models: "
                  f"{', '.join(sorted(model_name(lean) for lean in entries))}", file=sys.stderr)
            return EXIT_USAGE
        targets = sorted({by_name[n] for n in names})
    blocking = [msg for lean, kind, msg in problems if lean in targets and kind in ("entry", "ref")]
    if blocking:
        print(f"refused: nothing written; fix {MANIFEST} first:")
        for msg in blocking:
            print(f"  {msg}")
        return EXIT_MANIFEST
    digests = {lean: fingerprint(entries[lean]["rust"], snap) for lean in targets}
    try:
        new_text = edit_digests(text, digests)
        old_data, new_data = tomllib.loads(text), tomllib.loads(new_text)

        def without_digests(data):
            rest = {k: v for k, v in data.items() if k != "model"}
            return rest, [{k: v for k, v in e.items() if k != "read_digest"} for e in data.get("model", [])]

        if without_digests(old_data) != without_digests(new_data):
            raise ValueError("the edit changed more than read_digest")
        for old_e, new_e in zip(old_data.get("model", []), new_data.get("model", [])):
            want = digests.get(new_e.get("lean"), old_e.get("read_digest"))
            if new_e.get("read_digest") != want:
                raise ValueError(f"read_digest of {new_e.get('lean')} did not land")
    except (ValueError, tomllib.TOMLDecodeError) as e:
        print(f"refused: nothing written; cannot edit {MANIFEST} safely: {e}")
        return EXIT_MANIFEST
    changed = [lean for lean in targets if entries[lean]["read_digest"] != digests[lean]]
    if new_text != text:
        fd, tmp = tempfile.mkstemp(dir=os.path.dirname(path), prefix=".models.toml.")
        try:
            with os.fdopen(fd, "wb") as f:
                f.write(new_text.encode("utf-8"))
                f.flush()
                os.fsync(f.fileno())
            os.chmod(tmp, os.stat(path).st_mode & 0o7777)
            os.replace(tmp, path)
        except BaseException:
            if os.path.exists(tmp):
                os.unlink(tmp)
            raise
        try:  # make the rename itself durable; best effort
            dfd = os.open(os.path.dirname(path), os.O_RDONLY)
            try:
                os.fsync(dfd)
            finally:
                os.close(dfd)
        except OSError:
            pass
    for lean in targets:
        state = "bumped" if lean in changed else "already current"
        print(f"{model_name(lean)}: read_digest {digests[lean][:19]}... ({state})")
    dirty = uncommitted_refs(root, entries, targets, snap)
    if dirty:
        print("note: the code of these refs in the working tree differs from HEAD, so the digest "
              "records uncommitted edits; commit them together with the manifest:")
        for path, names in sorted(dirty.items()):
            print(f"  {path} ({', '.join(sorted(names))})")
    # A digest-scheme problem is judged on the manifest as written now.
    others = [msg for lean, kind, msg in problems if lean not in targets and kind != "scheme"]
    others += [msg for _, kind, msg in load_manifest(new_text, MANIFEST)[1] if kind == "scheme"]
    if others:
        print(f"note: {MANIFEST} has other problems the check will report:")
        for msg in others:
            print(f"  {msg}")
    if changed:
        print(f"{len(changed)} of {len(targets)} bumped in {MANIFEST}. Bump only models you have "
              f"re-read; update their read_against by hand if you keep it.")
    return EXIT_OK


# --------------------------------------------------------------------------
# drift


def explain(refs, snap, old):
    """[(ref, kind, line)] for the refs of a drifted model: with `old` (a
    Snapshot of a read_against whose fingerprint is the model's read_digest),
    the refs whose code differs there, kind "changed"; without, every ref,
    kind "unknown". `line` is the first changed line, else the item's keyword
    line, else None (a whole file)."""
    found = []
    for ref in refs:
        kw = snap.keyword_line(ref)
        if old is None:
            found.append((ref, "unknown", kw))
            continue
        was, now = old.resolve(ref), snap.resolve(ref)
        if was[0].norm(was[1]) != now[0].norm(now[1]):
            found.append((ref, "changed", first_changed_line(was[0], was[1], now[0], now[1]) or kw))
    return found


def find_drift(entries, snap, repo):
    """[(lean, entry, findings, read_against commit or None, why)] for every
    model whose fingerprint differs from its read_digest. The commit is given
    (and the findings narrowed to the refs that changed) only when the refs at
    read_against fingerprint to exactly read_digest; otherwise `why` says why
    every ref is listed."""
    drift = []
    for lean, e in sorted(entries.items()):
        if fingerprint(e["rust"], snap) == e["read_digest"]:
            continue
        ra = e["read_against"]
        commit = repo.commit(ra) if ra else None
        old, why = None, None
        if not ra:
            why = "the entry has no read_against"
        elif commit is None:
            why = f"read_against {ra} is not a commit in this clone"
        else:
            at_ra = Snapshot(GitTree(repo, commit))
            try:
                matches = fingerprint(e["rust"], at_ra) == e["read_digest"]
            except ValueError:  # a ref that does not resolve there
                matches = False
            if matches:
                old = at_ra
            else:
                why = (f"read_against {ra} is not the state read_digest was taken at (the refs "
                       f"there do not fingerprint to it: the ref list changed since the last bump, "
                       f"or the bump read another tree, such as uncommitted edits or a later commit)")
        found = explain(e["rust"], snap, old)
        if old is not None and not found:  # cannot happen short of a hash collision
            old, why = None, f"no ref differs from read_against {ra}"
            found = explain(e["rust"], snap, None)
        drift.append((lean, e, found, commit if old is not None else None, why))
    return drift


# --------------------------------------------------------------------------
# output


def esc_data(s):
    """A workflow command's message, escaped."""
    return s.replace("%", "%25").replace("\r", "%0D").replace("\n", "%0A")


def esc_prop(s):
    """A workflow command's property value, escaped."""
    return esc_data(s).replace(":", "%3A").replace(",", "%2C")


def gh(level, msg, file=None, line=None):
    if not os.environ.get("GITHUB_ACTIONS"):
        return
    props = []
    if file:
        props.append(f"file={esc_prop(file)}")
        if line:
            props.append(f"line={line}")
    props.append(f"title={esc_prop('Lean model drift')}")
    print(f"::{level} {','.join(props)}::{esc_data(msg)}")


def manifest_line_of(source, lean, key="lean_line"):
    """1-based line of `lean`'s entry in the manifest (its `key` line), or None."""
    text = source.read(MANIFEST) or ""
    for b in entry_lines(text.splitlines(keepends=True)):
        if b["lean"] == lean:
            i = b[key] if b[key] is not None else b["lean_line"]
            return None if i is None else i + 1
    return None


def plural(n, word):
    return f"{n} {word}" + ("" if n == 1 else "s")


def report(drift, entries, source, strict):
    level = "error" if strict else "warning"
    print(f"model drift in {source.label}: the Rust of {len(drift)} of {plural(len(entries), 'model')} "
          f"no longer matches the model's read_digest. Re-read each model against its Rust, update "
          f"it if needed, and run `{BUMP_CMD} MODEL`.")
    for lean, e, found, commit, why in drift:
        name, ra = model_name(lean), e["read_against"]
        todo = f"Re-read {lean} against it, update the model if needed, and run `{BUMP_CMD} {name}`."
        if commit is None:
            print(f"  {name} ({lean}): {why}, so which of its refs changed is not known; its refs:")
        else:
            print(f"  {name} ({lean}), read against {ra}:")
        for ref, kind, line in found:
            path, _ = split_ref(ref)
            at = f", line {line}" if line else ""
            if kind == "changed":
                hint = f"git diff {ra} -- {path}"
                print(f"    {ref}: changed since {ra}{at} ({hint})")
                msg = f"{ref} has changed since {lean} was read against {ra}. {todo} To see the change: {hint}"
            else:
                print(f"    {ref}{at}")
                msg = (f"{lean}'s Rust no longer matches its read_digest, and {ref} is one of its "
                       f"refs; {why}, so which of them changed is not known. Re-read {lean} against "
                       f"its Rust, update the model if needed, and run `{BUMP_CMD} {name}`.")
            gh(level, msg, path, line)


def main(argv):
    ap = argparse.ArgumentParser(
        prog="check-model-drift.py",
        description="Flag Lean models whose Rust changed since the model was last read.")
    ap.add_argument("--strict", action="store_true", help="exit 1 on drift (default: warn, exit 0)")
    ap.add_argument("--repo", default=".", help="repository root (default: the current directory)")
    ap.add_argument("--rev", default=None,
                    help="check this commit's tree instead of the working tree")
    ap.add_argument("--bump", nargs="+", metavar="MODEL",
                    help="rewrite read_digest for these models (after re-reading them)")
    ap.add_argument("--bump-all", action="store_true", help="rewrite read_digest for every model")
    ap.add_argument("--self-test", action="store_true", help="run the built-in tests and exit")
    args = ap.parse_args(argv)
    if args.self_test:
        return self_test()
    if args.bump or args.bump_all:
        if args.bump and args.bump_all:
            print("error: give --bump MODEL ... or --bump-all, not both", file=sys.stderr)
            return EXIT_USAGE
        if args.rev:
            print("error: --bump reads the working tree; it does not take --rev", file=sys.stderr)
            return EXIT_USAGE
        return bump(args.repo, args.bump or [], args.bump_all)

    repo = Repo(args.repo)
    try:
        if args.rev:
            commit = repo.commit(args.rev)
            if commit is None:
                print(f"error: --rev {args.rev!r} is not a commit in {args.repo}", file=sys.stderr)
                return EXIT_USAGE
            source = GitTree(repo, commit)
        else:
            source = WorkTree(args.repo)
        entries, problems, snap = check_manifest(source)
        if problems:
            print(f"{MANIFEST} is broken:")
            for lean, _, msg in problems:
                print(f"  {msg}")
                gh("error", msg, MANIFEST, manifest_line_of(source, lean) if lean else None)
            return EXIT_MANIFEST
        print(f"{MANIFEST}: {plural(len(entries), 'model')}, "
              f"{plural(sum(len(e['rust']) for e in entries.values()), 'Rust ref')}, all present in "
              f"{source.label}")
        drift = find_drift(entries, snap, repo)
    except GitError as e:
        print(f"error: {e}", file=sys.stderr)
        return EXIT_USAGE
    if not drift:
        print(f"no model drift in {source.label}: every model's Rust matches its read_digest")
        return EXIT_OK
    report(drift, entries, source, args.strict)
    return EXIT_DRIFT if args.strict else EXIT_OK


# --------------------------------------------------------------------------
# self-test


def self_test():
    import contextlib
    import io

    failures = []
    dp = f"v{DIGEST_VERSION}:sha256:"

    def expect(name, cond, detail=""):
        print(f"  {'ok  ' if cond else 'FAIL'} {name}")
        if not cond:
            failures.append(f"{name}: {detail}")

    def changed(old, new, item=None):
        ro, rn = Rust(old), Rust(new)
        if item is None:
            return ro.norm() != rn.norm()
        so = ro.item_spans(item)
        if not so:
            return None
        return ro.norm(so) != rn.norm(rn.item_spans(item))

    print("self-test: lexer")
    lexer = [
        ("`//` inside a string", 'fn a(){ let s="//"; let x=1; }', 'fn a(){ let s="//"; let x=2; }', True),
        ("raw byte string", 'fn a(){ let s=br##"a"#b/*"##; 1 }', 'fn a(){ let s=br##"a"#b/*"##; 2 }', True),
        ("char '\"' then a string", "fn a(){ let c='\"'; let s=\"//\"; 1 }",
         "fn a(){ let c='\"'; let s=\"//\"; 2 }", True),
        ("lifetimes", "fn a<'a>(x:&'a str){ let s=\"//\"; 1 }", "fn a<'a>(x:&'a str){ let s=\"//\"; 2 }", True),
        ("nested block comment", "/* a /* b */ c */ fn a(){1}", "/* a /* b */ c */ fn a(){2}", True),
        ("an identifier ending in r before a string is not a raw string",
         'fn a(){ m!(ar"a\\"//"); 1 }', 'fn a(){ m!(ar"a\\"//"); 2 }', True),
        ("raw identifier is not a raw string", 'fn a(){ let r#type=1; let s="//"; 1 }',
         'fn a(){ let r#type=1; let s="//"; 2 }', True),
        ("comment-only change", "fn a(){ 1 // x\n}", "fn a(){ 1 // y\n}", False),
        ("doc-only change", "/// a\nfn a(){1}", "/// b\nfn a(){1}", False),
        ("#[cfg(test)] module change", "#[cfg(test)]\nmod tests { fn t(){1} }\nfn r(){1}",
         "#[cfg(test)]\nmod tests { fn t(){2} }\nfn r(){1}", False),
        ("#[cfg(test)] field, then a real field", "struct S {\n #[cfg(test)]\n a: u32,\n b: u32 }",
         "struct S {\n #[cfg(test)]\n a: u32,\n b: u64 }", True),
        ("#[cfg(not(test))] is real code", "#[cfg(not(test))]\nfn r(){1}", "#[cfg(not(test))]\nfn r(){2}", True),
        ("whitespace between tokens", "fn a() -> u8 {\n    1\n}", "fn a()  ->\tu8 {  1 }", False),
        ("two spaces to one inside a string", 'fn a()->&str{ "a  b" }', 'fn a()->&str{ "a b" }', True),
        ("tab to space inside a string", 'fn a()->&str{ "a\tb" }', 'fn a()->&str{ "a b" }', True),
        ("newline to space inside a string", 'fn a()->&str{ "a\nb" }', 'fn a()->&str{ "a b" }', True),
        ("no-break space to space inside a string", 'fn a()->&str{ "a\u00a0b" }', 'fn a()->&str{ "a b" }', True),
        ("' ' to a tab char", "fn a()->char{ ' ' }", "fn a()->char{ '\t' }", True),
        ("#[cfg(test)] fn with a comma in its return type, then real code",
         "#[cfg(test)]\npub(crate) fn v(a: u8, b: u8) -> Result<A, B> {\n    t(a, b)\n}\nfn r(){1}",
         "#[cfg(test)]\npub(crate) fn v(a: u8, b: u8) -> Result<A, B> {\n    t(a, b)\n}\nfn r(){2}", True),
        ("... a change inside that fn is not",
         "#[cfg(test)]\npub(crate) fn v(a: u8, b: u8) -> Result<A, B> {\n    t(a, b)\n}\nfn r(){1}",
         "#[cfg(test)]\npub(crate) fn v(a: u8, b: u8) -> Result<A, B> {\n    t(b, a)\n}\nfn r(){1}", False),
        ("#[cfg(test)] field with a comma in its type, then a real field",
         "struct S {\n #[cfg(test)]\n a: HashMap<u8, u8>,\n b: u32 }",
         "struct S {\n #[cfg(test)]\n a: HashMap<u8, u8>,\n b: u64 }", True),
        ("#[cfg(test)] struct-expression field `n <= LIMIT`, then a real field",
         "fn f() -> S { S { #[cfg(test)] fast: n <= LIMIT, slow: 1 } }",
         "fn f() -> S { S { #[cfg(test)] fast: n <= LIMIT, slow: 2 } }", True),
        ("... `x < y`", "fn f() -> S { S { #[cfg(test)] a: x < y, b: 1 } }",
         "fn f() -> S { S { #[cfg(test)] a: x < y, b: 2 } }", True),
        ("... `1 << 3`", "fn f() -> S { S { #[cfg(test)] a: 1 << 3, b: 1 } }",
         "fn f() -> S { S { #[cfg(test)] a: 1 << 3, b: 2 } }", True),
        ("... `if x < y {..} else {..}`", "fn f() -> S { S { #[cfg(test)] a: if x < y { 1 } else { 2 }, b: 1 } }",
         "fn f() -> S { S { #[cfg(test)] a: if x < y { 1 } else { 2 }, b: 2 } }", True),
        ("... a closure `|x| x < 3`", "fn f() -> S { S { #[cfg(test)] a: |x| x < 3, b: 1 } }",
         "fn f() -> S { S { #[cfg(test)] a: |x| x < 3, b: 2 } }", True),
        ("... `(x as u8) < 3`", "fn f() -> S { S { #[cfg(test)] a: (x as u8) < 3, b: 1 } }",
         "fn f() -> S { S { #[cfg(test)] a: (x as u8) < 3, b: 2 } }", True),
        ("... `x < y`, then a real field holding `>`",
         "fn f() -> S { S { #[cfg(test)] a: x < y, b: y > z } }",
         "fn f() -> S { S { #[cfg(test)] a: x < y, b: y > w } }", True),
        ("... `x<y` with no spaces (angles unbalanced: ends at the comma)",
         "fn f() -> S { S { #[cfg(test)] a: x<y, b: 1 } }", "fn f() -> S { S { #[cfg(test)] a: x<y, b: 2 } }", True),
        ("#[cfg(test)] tuple field of a fn-pointer type, then a real field",
         "struct S(#[cfg(test)] fn(u8) -> u8, u32);", "struct S(#[cfg(test)] fn(u8) -> u8, u64);", True),
        ("... `unsafe fn(..)`", "struct S(#[cfg(test)] unsafe fn(u8), u32);",
         "struct S(#[cfg(test)] unsafe fn(u8), u64);", True),
        ("... `extern \"C\" fn(..)`", 'struct S(#[cfg(test)] extern "C" fn(u8), u32);',
         'struct S(#[cfg(test)] extern "C" fn(u8), u64);', True),
        ("#[cfg(test)] field named `union`, then a real field", "struct S {\n #[cfg(test)]\n union: u8,\n b: u32 }",
         "struct S {\n #[cfg(test)]\n union: u8,\n b: u64 }", True),
        ("... named `macro_rules`", "struct S {\n #[cfg(test)]\n macro_rules: u8,\n b: u32 }",
         "struct S {\n #[cfg(test)]\n macro_rules: u8,\n b: u64 }", True),
        ("#[cfg(test)] match arm is still cut at its comma",
         "fn f(x:u8)->u8{ match x {\n #[cfg(test)]\n 0 => 7,\n _ => 1 } }",
         "fn f(x:u8)->u8{ match x {\n #[cfg(test)]\n 0 => 7,\n _ => 2 } }", True),
    ]
    for name, old, new, want in lexer:
        expect(name, changed(old, new) is want)

    print("self-test: items")
    hrtb = ("impl Foo<u8> { fn apply(&self){0} }\nimpl<F> Foo<F>\nwhere\n    F: for<'a> Fn(&'a u8),\n"
            "{\n fn apply(&self){%d}\n}")
    expect("`where` on its own line, for<'a> bound", changed(hrtb % 1, hrtb % 2, "Foo::apply") is True)
    tr = "impl<T> Tr for Foo<T>\nwhere\n    T: for<'a> Fn(&'a u8),\n{\n fn apply(&self){%d}\n}"
    expect("... and with a trait", changed(tr % 1, tr % 2, "Foo::apply") is True)
    expect("impl dyn Trait",
           changed("impl dyn Tr { fn apply(&self){1} }", "impl dyn Tr { fn apply(&self){2} }",
                   "Tr::apply") is True)
    expect("impl dyn Trait + Send",
           changed("impl dyn Tr + Send { fn apply(&self){1} }",
                   "impl dyn Tr + Send { fn apply(&self){2} }", "Tr") is True)
    ml = '/// doc\n#[derive(\n    Debug,\n)]\n#[serde(\n  rename = "%s"\n)]\npub struct Foo { x: u8 }'
    expect("multi-line attribute", changed(ml % "a", ml % "b", "Foo") is True)
    expect("... and a change above it is not", changed("fn x(){1}\n" + ml % "a", "fn x(){2}\n" + ml % "a",
                                                       "Foo") is False)
    expect("a change outside the item is not", changed("fn a(){1}\nfn b(){1}", "fn a(){1}\nfn b(){2}",
                                                       "a") is False)
    expect("a const runs to its `;`, past a block",
           changed("const X: u32 = { 1 } + 2;", "const X: u32 = { 1 } + 3;", "X") is True)
    expect("a const-generic block does not end an impl header",
           changed("impl Foo<{ N }> { fn get(&self) -> u8 { 1 } }",
                   "impl Foo<{ N }> { fn get(&self) -> u8 { 2 } }", "Foo::get") is True)
    expect("an item only in #[cfg(test)] code is not found",
           changed("#[cfg(test)]\nfn a(){1}", "#[cfg(test)]\nfn a(){2}", "a") is None)
    nested = "fn a(){1}\nmod inner {\n    fn a(){%d}\n}\n#[cfg(test)]\nmod tests {\n    fn a(){%d}\n}\n"
    expect("a same-named item in a nested non-test module is part of the ref",
           changed(nested % (1, 1), nested % (2, 1), "a") is True)
    expect("... one in a #[cfg(test)] module is not", changed(nested % (1, 1), nested % (1, 2), "a") is False)
    src = "use x;\n\n/// doc\n#[derive(\n  Debug,\n)]\npub struct Foo { x: u8 }\n"
    r = Rust(src)
    expect("keyword line, not the first attribute's",
           r.line_of(r.item_matches("Foo")[0][2]) == 7, r.item_matches("Foo"))
    old = Rust("/// d\n#[inline]\nfn get() -> u8 {\n    let a = 1;\n    a\n}\n")
    new = Rust("/// d\n#[inline]\nfn get() -> u8 {\n    let a = 2;\n    a\n}\n")
    expect("first changed line", first_changed_line(old, old.item_spans("get"), new,
                                                    new.item_spans("get")) == 4)

    print("self-test: golden digest")
    golden = {
        "src/g.rs": ("//! golden\nuse std::fmt;\n\n/// doc\n#[derive(Debug)]\npub struct Foo<const N: usize> {\n"
                     "    a: u8,\n    #[cfg(test)]\n    t: HashMap<u8, u8>,\n    b: &'static str,\n}\n\n"
                     "impl Foo<{ 3 }> {\n    pub fn get(&self) -> &str { \"a  b\\t\" /* c */ }\n}\n\n"
                     "const X: u32 = { 1 } + 2;\n\n#[cfg(test)]\npub(crate) fn helper(a: u8, b: u8) -> "
                     "Result<u8, ()> {\n    Ok(a + b)\n}\n\n#[cfg(test)]\nmod tests {\n    fn t() {}\n}\n"
                     "\n#[cfg(test)]\nuse a::{b, c};\n\n#[cfg(test)]\nfn tested() -> &'static str {\n"
                     "    \"t  e  s  t\"\n}\n"),
        "src/h.rs": "pub const K: char = ' '; // space\n",
    }

    class Fixed:
        label = "the golden input"

        def read(self, path):
            return golden.get(path)

    got = fingerprint(["src/g.rs", "src/g.rs#Foo", "src/g.rs#Foo::get", "src/g.rs#X", "src/h.rs#K"],
                      Snapshot(Fixed()))
    want = GOLDEN_DIGESTS.get(DIGEST_VERSION)
    expect(f"the golden input's digest is GOLDEN_DIGESTS[{DIGEST_VERSION}]", got == want,
           f"got {got}, want {want}. If the normalisation or the digest layout changed on purpose, "
           f"bump DIGEST_VERSION and add the new golden digest; otherwise the change is a bug")

    print("self-test: workflow-command escaping")
    expect("property values", esc_prop("a%b,c:d\r\ne") == "a%25b%2Cc%3Ad%0D%0Ae", esc_prop("a%b,c:d\r\ne"))
    expect("message data keeps `,` and `:`", esc_data("50%, a:b\nc\r") == "50%25, a:b%0Ac%0D")

    with tempfile.TemporaryDirectory() as d:
        env = dict(os.environ, GIT_CONFIG_NOSYSTEM="1", GIT_CONFIG_GLOBAL=os.devnull,
                   GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@example.invalid",
                   GIT_COMMITTER_NAME="t", GIT_COMMITTER_EMAIL="t@example.invalid")
        env.pop("GITHUB_ACTIONS", None)

        def git(*a):
            return subprocess.run(["git", "-C", d, *a], check=True, capture_output=True,
                                  env=env).stdout.decode().strip()

        def write(path, text):
            p = os.path.join(d, path)
            os.makedirs(os.path.dirname(p), exist_ok=True)
            with open(p, "w", newline="") as f:
                f.write(text)

        def read(path):
            with open(os.path.join(d, path), newline="") as f:
                return f.read()

        def commit(msg):
            git("add", "-A")
            git("commit", "-q", "--allow-empty", "-m", msg)
            return git("rev-parse", "HEAD")

        def digests():
            data = tomllib.loads(read(MANIFEST))
            return {e["lean"]: e.get("read_digest") for e in data["model"]}

        def manifest(ra_a, ra_b, a_rust='"src/a.rs"', b_rust='"src/b.rs#keep"', keep=True,
                     a_lean="A"):
            """Write the manifest; with `keep`, each entry keeps the read_digest
            it has now (an edit that does not bump)."""
            dg = digests() if keep and os.path.exists(os.path.join(d, MANIFEST)) else {}
            a_dg = dg.get(f"{MODEL_DIR}/A.lean") or dg.get(f"{MODEL_DIR}/A2.lean")
            b_dg = dg.get(f"{MODEL_DIR}/B.lean")

            def line(v):
                return f'read_digest = "{v}"\n' if v else ""
            write(MANIFEST,
                  "# models\n\n"
                  f'[[model]]\nlean = "formal/LamboProofs/{a_lean}.lean"\n'
                  + (f'read_against = "{ra_a[:10]}"\n' if ra_a else "") + line(a_dg)
                  + f"rust = [{a_rust}]\n\n"
                  '[[model]]  # B\nlean = "formal/LamboProofs/B.lean"\n'
                  + (f'read_against = "{ra_b[:10]}"\n' if ra_b else "") + line(b_dg)
                  + f"rust = [\n  {b_rust},\n]\n")

        def run(*argv, gha=False, repo=None):
            out = io.StringIO()
            old = os.environ.pop("GITHUB_ACTIONS", None)
            if gha:
                os.environ["GITHUB_ACTIONS"] = "true"
            try:
                with contextlib.redirect_stdout(out), contextlib.redirect_stderr(out):
                    code = main(["--repo", repo or d, *argv])
            finally:
                os.environ.pop("GITHUB_ACTIONS", None)
                if old is not None:
                    os.environ["GITHUB_ACTIONS"] = old
            return code, out.getvalue()

        def only_digest_lines_differ(before, after, n):
            """`after` is `before` with exactly `n` read_digest lines changed."""
            a, b = before.splitlines(keepends=True), after.splitlines(keepends=True)
            diff = [(x, y) for x, y in zip(a, b) if x != y]
            return (len(a) == len(b) and len(diff) == n
                    and all(x.startswith("read_digest") and y.startswith("read_digest") for x, y in diff))

        def b_rs(keep_attr="#[inline]", keep_body="x + 1", other="2", s_keep="3"):
            return ("/// kept\n" + keep_attr + "\npub fn keep(x: u32) -> u32 {\n    let s = \"}\";\n    "
                    + keep_body + "\n}\n\npub fn other() -> u32 {\n    " + other + "\n}\n\n"
                    "pub struct S;\nimpl S {\n    pub fn keep(&self) -> u32 { " + s_keep + " }\n}\n")

        git("init", "-q", "-b", "main")
        write("formal/LamboProofs/A.lean", "-- model A\n")
        write("formal/LamboProofs/B.lean", "-- model B\n")
        write("formal/LamboProofs/Audit.lean", "-- audit\n")
        write("src/a.rs", "pub fn a() -> u32 {\n    1\n}\n")
        write("src/a2.rs", "pub fn a2() -> u32 {\n    1\n}\n")
        write("src/b.rs", b_rs())
        c0 = commit("code")
        manifest(c0, c0)
        commit("manifest, no digests yet")

        print("self-test: bootstrap and --bump")
        code, out = run()
        expect("no read_digest: exit 3", code == 3 and "no `read_digest`" in out, out)
        before = read(MANIFEST)
        code, out = run("--bump", "A", "Nope")
        expect("--bump of an unknown model: exit 2, nothing written",
               code == 2 and "no model 'Nope'" in out and read(MANIFEST) == before, out)
        code, out = run("--bump", "A", "--rev", "HEAD")
        expect("--bump with --rev: exit 2", code == 2, out)
        code, out = run("--bump-all")
        after = read(MANIFEST)
        inserted = [x for x in after.splitlines(keepends=True) if x not in before.splitlines(keepends=True)]
        expect("--bump-all inserts one read_digest line per entry, after read_against",
               code == 0 and len(inserted) == 2
               and all(x.startswith(f'read_digest = "{dp}') for x in inserted)
               and after.replace(inserted[0], "", 1).replace(inserted[1], "", 1) == before
               and f'read_against = "{c0[:10]}"\nread_digest' in after, out + after)
        code, out = run("--strict")
        expect("bootstrapped: clean", code == 0 and "no model drift" in out, out)
        code, out = run("--bump", "A")
        expect("bumping a current model: nothing to change", code == 0 and "already current" in out
               and read(MANIFEST) == after, out)
        c1 = commit("bump all")

        print("self-test: drift")
        write("src/a.rs", "pub fn a() -> u32 {\n    2\n}\n")
        c2 = commit("change a")
        code, out = run()
        expect("Rust changed: reported, exit 0", code == 0 and "model drift" in out
               and "src/a.rs: changed since" in out and "A (formal/LamboProofs/A.lean)" in out, out)
        code, out = run("--strict")
        expect("... exit 1 with --strict", code == 1, out)
        code, out = run(gha=True)
        expect("... ::warning on the Rust file at the changed line, with a git diff hint",
               f"::warning file=src/a.rs,line=2,title=Lean model drift::src/a.rs has changed since "
               f"formal/LamboProofs/A.lean was read against {c0[:10]}." in out
               and f"git diff {c0[:10]} -- src/a.rs" in out and f"--bump A`" in out, out)
        code, out = run("--strict", gha=True)
        expect("... ::error with --strict", "::error file=src/a.rs,line=2," in out, out)
        code, out = run("--rev", c1)
        expect("--rev at the bump commit: clean", code == 0 and f"no model drift in {c1[:12]}" in out, out)
        write("README", "x\n")
        commit("unrelated")
        code, out = run("--strict")
        expect("drift persists across a later unrelated commit", code == 1 and "src/a.rs" in out, out)
        write("formal/LamboProofs/A.lean", "-- model A, re-read\n")
        c4 = commit("touch model A")
        code, out = run("--strict")
        expect("editing the Lean file alone does not clear it", code == 1 and "src/a.rs" in out, out)
        before = read(MANIFEST)
        code, out = run("--bump", "formal/LamboProofs/A.lean")
        expect("--bump clears it, changing only A's read_digest line",
               code == 0 and only_digest_lines_differ(before, read(MANIFEST), 1)
               and digests()[f"{MODEL_DIR}/B.lean"] in before, out)
        code, out = run("--strict")
        expect("... clean after", code == 0 and "no model drift" in out, out)
        commit("bump A")

        print("self-test: comments, whitespace, items")
        write("src/a.rs", "// a comment\npub fn a()  ->  u32 {\n    /* still */ 2 // two\n}\n\n"
              "#[cfg(test)]\nmod tests {\n    #[test]\n    fn a_is_two() { assert_eq!(super::a(), 2); }\n}\n")
        commit("comment a, add a test module")
        code, out = run("--strict")
        expect("comment-, whitespace- and #[cfg(test)]-only changes: clean", code == 0, out)
        write("src/b.rs", b_rs(other="20"))
        commit("change other")
        code, out = run("--strict")
        expect("change outside src/b.rs#keep: clean", code == 0, out)
        write("src/b.rs", b_rs(keep_attr="#[inline(always)]", other="20"))
        c8 = commit("change keep's attribute")
        code, out = run("--strict", gha=True)
        expect("attribute of src/b.rs#keep changed: drift, annotation at the attribute (first changed line)",
               code == 1 and "::error file=src/b.rs,line=2," in out, out)
        manifest(c4, "deadbeef00")
        commit("B's read_against not in this clone")
        code, out = run("--strict", gha=True)
        expect("read_against not resolvable: every ref listed at its keyword line, no git diff hint",
               code == 1 and "::error file=src/b.rs,line=3," in out and "is not a commit in this clone" in out
               and "git diff deadbeef00" not in out, out)
        manifest(c4, None)
        commit("B without read_against")
        code, out = run("--strict")
        expect("no read_against: drift still reported", code == 1 and "has no read_against" in out, out)
        manifest(c4, c8)
        run("--bump", "B")
        manifest(c4, c8, b_rust='"src/b.rs#S::keep"')
        commit("B models S::keep, not yet bumped")
        code, out = run("--strict", gha=True)
        expect("ref re-pointed, code unchanged since read_against: drift; read_against is not the "
               "digest's state, so every ref is listed at its keyword line",
               code == 1 and "is not the state read_digest was taken at" in out
               and "::error file=src/b.rs,line=14," in out and "git diff" not in out, out)
        run("--bump", "B.lean")
        commit("bump B")
        write("src/b.rs", b_rs(keep_attr="#[inline(always)]", keep_body="x + 2", other="20"))
        commit("change free keep")
        code, out = run("--strict")
        expect("free fn keep changed, B models S::keep: clean", code == 0, out)
        write("src/b.rs", b_rs(keep_attr="#[inline(always)]", keep_body="x + 2", other="20", s_keep="4"))
        c11 = commit("change S::keep")
        code, out = run("--strict", gha=True)
        expect("S::keep changed: drift at the fn line", code == 1 and "src/b.rs#S::keep" in out
               and "::error file=src/b.rs,line=14," in out, out)
        manifest(c4, c11)
        run("--bump", "B")
        commit("B back to keep, read against c11")

        print("self-test: the ref list is part of the digest")
        A2 = '"src/a.rs", "src/a2.rs"'
        manifest(c4, c11, a_rust='"src/a.rs", "src/a2.rs"')
        run("--bump", "A")
        commit("A also models a2")
        code, out = run("--strict")
        expect("A with a2, bumped: clean", code == 0, out)
        manifest(c4, c11, a_rust='"src/a2.rs", "src/a.rs"')
        commit("reorder A's refs")
        code, out = run("--strict")
        expect("reordering refs: clean", code == 0, out)
        manifest(c4, c11)
        commit("drop a2 from A")
        code, out = run("--strict")
        expect("dropping a ref: drift", code == 1 and "A (formal/LamboProofs/A.lean)" in out, out)
        manifest(c4, c11, a_rust=A2, b_rust='"src/b.rs"')
        run("--bump", "B")
        commit("A with a2 again; B models all of src/b.rs")
        code, out = run("--strict")
        expect("... restoring it: clean", code == 0, out)
        manifest(c4, c11, a_rust=A2, b_rust='"src/b.rs#keep"')
        commit("narrow B to keep")
        code, out = run("--strict")
        expect("narrowing a whole file to an item: drift", code == 1 and "B (formal/LamboProofs/B.lean)" in out, out)
        run("--bump", "B")
        commit("bump B")
        git("mv", "formal/LamboProofs/A.lean", "formal/LamboProofs/A2.lean")
        manifest(c4, c11, a_rust=A2, a_lean="A2")
        commit("rename A.lean to A2.lean")
        code, out = run("--strict")
        expect("renaming a Lean file, same refs: clean", code == 0, out)
        git("mv", "formal/LamboProofs/A2.lean", "formal/LamboProofs/A.lean")
        manifest(c4, c11, a_rust=A2)
        c12 = commit("rename back")

        print("self-test: refs new since read_against")
        write("src/new.rs", "pub fn n() {}\n")
        manifest(c4, c11, a_rust=A2 + ', "src/new.rs"')
        commit("A also models a new file")
        code, out = run("--strict", gha=True)
        expect("a path that did not exist at read_against: drift, every ref listed, whole files "
               "annotated file-level",
               code == 1 and "is not the state read_digest was taken at" in out
               and "::error file=src/new.rs,title=" in out and "::error file=src/a.rs,title=" in out, out)
        write("src/b.rs", b_rs(keep_attr="#[inline(always)]", keep_body="x + 2", other="20", s_keep="4")
              + "pub fn fresh() {}\n")
        manifest(c4, c11, a_rust=A2, b_rust='"src/b.rs#keep", "src/b.rs#fresh"')
        commit("B also models a new item")
        code, out = run("--strict", gha=True)
        expect("an item that did not exist at read_against: drift, every ref at its keyword line",
               code == 1 and "is not the state read_digest was taken at" in out
               and "::error file=src/b.rs,line=16," in out and "::error file=src/b.rs,line=3," in out, out)
        git("checkout", "-q", c12, "--", ".")
        git("rm", "-q", "src/new.rs")
        commit("undo")
        code, out = run("--strict")
        expect("... back to clean", code == 0, out)

        print("self-test: squash merge")
        git("checkout", "-q", "-b", "pr")
        write("src/a2.rs", "pub fn a2() -> u32 {\n    2\n}\n")
        x = commit("change a2")
        manifest(x, c11, a_rust=A2)
        run("--bump", "A")
        commit("re-read A, bump, read_against on the branch")
        code, out = run("--strict")
        expect("on the branch: clean", code == 0, out)
        git("checkout", "-q", "main")
        git("merge", "-q", "--squash", "pr")
        commit("squash-merge pr")
        git("branch", "-q", "-D", "pr")
        git("reflog", "expire", "--expire=now", "--all")
        git("gc", "-q", "--prune=now")
        gone = subprocess.run(["git", "-C", d, "cat-file", "-e", f"{x}^{{commit}}"], env=env,
                              capture_output=True).returncode != 0
        code, out = run("--strict")
        expect("on main after the branch is deleted and collected: clean, exit 0",
               gone and code == 0 and "no model drift" in out, out)
        write("src/a2.rs", "pub fn a2() -> u32 {\n    3\n}\n")
        commit("change a2 on main")
        code, out = run("--strict")
        expect("... later drift: every ref listed, read_against not in this clone",
               code == 1 and f"read_against {x[:10]} is not a commit in this clone" in out
               and "src/a2.rs" in out and "src/a.rs" in out, out)
        git("checkout", "-q", "HEAD~1", "--", "src/a2.rs")
        commit("undo")

        def collected(rev):
            git("reflog", "expire", "--expire=now", "--all")
            git("gc", "-q", "--prune=now")
            return subprocess.run(["git", "-C", d, "cat-file", "-e", f"{rev}^{{commit}}"], env=env,
                                  capture_output=True).returncode != 0

        print("self-test: rebase merge")
        git("checkout", "-q", "-b", "pr")
        write("src/a.rs", "pub fn a() -> u32 {\n    5\n}\n")
        y = commit("change a")
        manifest(y, c11, a_rust=A2)
        run("--bump", "A")
        commit("re-read A, bump, read_against on the branch")
        git("checkout", "-q", "main")
        write("README", "main moves on\n")
        commit("main moves on")
        git("checkout", "-q", "pr")
        git("rebase", "-q", "main")
        git("checkout", "-q", "main")
        git("merge", "-q", "--ff-only", "pr")
        git("branch", "-q", "-D", "pr")
        code, out = run("--strict")
        expect("rebase-merged, the branch's original commits collected: clean, exit 0",
               collected(y) and code == 0 and "no model drift" in out, out)

        print("self-test: shallow clone")
        with tempfile.TemporaryDirectory() as sd:
            sh = os.path.join(sd, "clone")
            subprocess.run(["git", "clone", "-q", "--depth", "1", "file://" + d, sh], check=True,
                           capture_output=True, env=env)
            code, out = run("--strict", repo=sh)
            expect("a depth-1 clone: clean", code == 0 and "no model drift" in out, out)
            with open(os.path.join(sh, "src", "a.rs"), "w") as f:
                f.write("pub fn a() -> u32 {\n    6\n}\n")
            code, out = run("--strict", gha=True, repo=sh)
            expect("... a change there: drift, read_against not in the clone, every ref listed and "
                   "whole files annotated file-level",
                   code == 1 and "is not a commit in this clone" in out
                   and "::error file=src/a.rs,title=" in out and "::error file=src/a2.rs,title=" in out
                   and "git diff" not in out, out)

        print("self-test: read_against that is not the digest's state")
        pre = git("rev-parse", "HEAD")
        write("src/a.rs", "pub fn a() -> u32 {\n    6\n}\n")
        mid = commit("change a, not bumped")
        manifest(mid, c11, a_rust=A2)
        write("src/a2.rs", "pub fn a2() -> u32 {\n    6\n}\n")
        commit("read_against moved to mid by hand, without a bump; a2 changes")
        code, out = run("--strict", gha=True)
        expect("read_against resolves but its refs do not fingerprint to read_digest: every ref "
               "listed, not only the one that changed since read_against",
               code == 1 and f"read_against {mid[:10]} is not the state read_digest was taken at" in out
               and "::error file=src/a.rs,title=" in out and "::error file=src/a2.rs,title=" in out
               and "git diff" not in out, out)
        git("checkout", "-q", pre, "--", ".")
        commit("undo")

        print("self-test: --bump with uncommitted edits")
        head = git("rev-parse", "HEAD")
        manifest(head, c11, a_rust=A2)
        run("--bump", "A")
        commit("A read against head")
        write("src/a.rs", "pub fn a() -> u32 {\n    7\n}\n")
        code, out = run("--bump", "A")
        expect("--bump names the ref files whose code differs from HEAD",
               code == 0 and "differs from HEAD" in out and "  src/a.rs (A)" in out
               and "  src/a2.rs (A)" not in out, out)
        git("add", MANIFEST)
        git("commit", "-q", "-m", "commit only the manifest")
        git("checkout", "-q", "--", "src/a.rs")
        code, out = run("--strict")
        expect("... the manifest committed without the edit: drift, and read_against is not the "
               "digest's state (not 'older than its last bump')",
               code == 1 and f"read_against {head[:10]} is not the state read_digest was taken at" in out
               and "uncommitted edits" in out and "older than" not in out, out)
        run("--bump", "A")
        commit("re-bump A")
        code, out = run("--strict")
        expect("... re-bumped: clean", code == 0, out)

        print("self-test: escaping in annotations")
        odd = "src/we,ird:50%.rs"
        write(odd, "pub fn w() -> u32 { 1 }\n")
        manifest(c4, c11, a_rust=A2, b_rust=f'"src/b.rs#keep", "{odd}#w"')
        run("--bump", "B")
        commit("B models an oddly named file")
        write(odd, "pub fn w() -> u32 { 2 }\n")
        commit("change it")
        code, out = run(gha=True)
        expect("file= escapes `,` `:` `%`; the message escapes `%` only",
               "::warning file=src/we%2Cird%3A50%25.rs,line=1,title=Lean model drift::"
               "formal/LamboProofs/B.lean's Rust no longer matches its read_digest, and "
               "src/we,ird:50%25.rs#w is one of its refs" in out, out)
        manifest(c4, c11, a_rust=A2)
        git("rm", "-q", odd)
        run("--bump", "B")
        commit("drop it")

        print("self-test: digest scheme")
        clean = read(MANIFEST)
        write(MANIFEST, clean.replace(f'read_digest = "{dp}', 'read_digest = "sha256:'))
        commit("digests of scheme 1")
        code, out = run("--strict")
        expect("digests of another scheme: exit 3, one 'digest scheme changed' message, no drift",
               code == 3 and out.count("digest scheme changed") == 1 and "--bump-all" in out
               and "no longer matches" not in out, out)
        code, out = run("--bump", "A")
        expect("... --bump of one model notes the scheme change still open for the other",
               code == 0 and "digest scheme changed: 1 read_digest uses scheme v1" in out
               and "(B)" in out, out)
        code, out = run("--bump-all")
        expect("... --bump-all rewrites them to this scheme, and no longer notes it",
               code == 0 and read(MANIFEST) == clean and "digest scheme changed" not in out, out)
        commit("bump-all")
        write(MANIFEST, clean.replace(f'read_digest = "{dp}', f'read_digest = "v{DIGEST_VERSION + 1}:sha256:', 1))
        code, out = run("--strict")
        expect("a digest of a newer scheme: exit 3, the script is older, no --bump-all advice",
               code == 3 and "this script is older than the manifest's digest scheme" in out
               and "--bump-all" not in out, out)
        before = read(MANIFEST)
        code, out = run("--bump-all")
        expect("... --bump-all refuses rather than downgrade it", code == 3 and read(MANIFEST) == before, out)
        write(MANIFEST, clean)

        print("self-test: broken manifest")
        write(MANIFEST, clean.replace(f'read_digest = "{dp}', f'read_digest = "v{DIGEST_VERSION}:sha1:', 1))
        commit("bad digest format")
        code, out = run("--strict", gha=True)
        expect("read_digest malformed: exit 3, ::error on the manifest at the entry",
               code == 3 and "`read_digest` must be" in out
               and "::error file=formal/models.toml,line=4," in out, out)
        write(MANIFEST, clean.replace(f'read_against = "{c4[:10]}"', 'read_against = "HEAD"'))
        commit("read_against a ref name")
        code, out = run()
        expect("read_against not a hex id: exit 3", code == 3 and "read_against" in out, out)
        write(MANIFEST, clean.replace(f'read_against = "{c4[:10]}"', 'read_against = "deadbeef00"'))
        commit("read_against not a commit")
        code, out = run("--strict")
        expect("read_against well-formed but not a commit, no drift: clean", code == 0, out)
        write(MANIFEST, clean)
        write("formal/LamboProofs/Sub/C.lean", "-- model C, no entry\n")
        commit("add Sub/C")
        code, out = run()
        expect("model in a subdirectory without an entry: exit 3",
               code == 3 and "no entry for formal/LamboProofs/Sub/C.lean" in out, out)
        code, out = run("--bump", "B")
        expect("... --bump of another model still works", code == 0 and "note:" in out, out)
        git("rm", "-q", "-r", "formal/LamboProofs/Sub")
        write("formal/LamboProofs/Lib/L.lean", "-- shared lemmas, no entry\n")
        write("formal/LamboProofs/Drain/History.lean", "-- history layer, no entry\n")
        write("formal/LamboProofs/Drain/Proofs/Base.lean", "-- proofs layer, no entry\n")
        write("formal/LamboProofs/Score/Proofs.lean", "-- proofs layer, no entry\n")
        commit("add Lib/L, Drain/History and two files named like proofs layers")
        code, out = run()
        expect("a file named like a proofs layer, with no Spec beside it, needs an entry: exit 3",
               code == 3 and "no entry for formal/LamboProofs/Drain/Proofs/Base.lean" in out
               and "no entry for formal/LamboProofs/Score/Proofs.lean" in out
               and "Lib/L.lean" not in out and "History.lean" not in out, out)
        write("formal/LamboProofs/Drain/Spec/Base.lean", "-- spec layer, needs an entry\n")
        write("formal/LamboProofs/Score/Spec.lean", "-- spec layer, needs an entry\n")
        commit("add Drain/Spec/Base and Score/Spec")
        code, out = run()
        expect("a Spec file without an entry: exit 3, its proofs layer needs none",
               code == 3 and "no entry for formal/LamboProofs/Drain/Spec/Base.lean" in out
               and "no entry for formal/LamboProofs/Score/Spec.lean" in out
               and "/Proofs" not in out, out)
        write("formal/LamboProofs/Drain/Proofs/Other.lean", "-- no Spec/Other.lean beside it\n")
        commit("add Drain/Proofs/Other")
        code, out = run()
        expect("a Proofs/ file whose Spec/ file of the same name is missing needs an entry",
               code == 3 and "no entry for formal/LamboProofs/Drain/Proofs/Other.lean" in out
               and "Proofs/Base.lean" not in out, out)
        git("rm", "-q", "-r", "formal/LamboProofs/Lib", "formal/LamboProofs/Drain",
            "formal/LamboProofs/Score")
        manifest(c4, c11, a_rust=A2, b_rust='"src/b.rs#keep", "src/gone.rs"')
        commit("missing path")
        code, out = run()
        expect("missing Rust path: exit 3", code == 3 and "src/gone.rs does not exist" in out, out)
        before = read(MANIFEST)
        code, out = run("--bump", "B")
        expect("--bump refuses a model whose refs do not resolve, writes nothing",
               code == 3 and "refused" in out and read(MANIFEST) == before, out)
        manifest(c4, c11, a_rust=A2, b_rust='"src/b.rs#nothing"')
        commit("missing item")
        code, out = run()
        expect("missing item: exit 3", code == 3 and "no item `nothing`" in out, out)
        manifest(c4, c11, a_rust=A2, b_rust='"../outside.rs"')
        commit("path outside the repository")
        code, out = run()
        expect("path outside the repository: exit 3", code == 3 and "not a relative path" in out, out)

        print("self-test: --bump keeps the file byte-for-byte")
        write(MANIFEST, clean.replace("\n", "\r\n").replace(
            f'read_digest = "{dp}', f'read_digest   =  "{dp}0000', 1).replace(
            '[[model]]  # B', '[[model]]  # B, "quoted", lean = "x"'))
        before = read(MANIFEST)
        code, out = run("--bump", "A")
        after = read(MANIFEST)
        expect("CRLF, odd spacing and comments kept; only A's digest line changes",
               code == 0 and only_digest_lines_differ(before, after, 1)
               and f'read_digest   =  "{dp}' in after and "\r\n" in after
               and tomllib.loads(after)["model"][0]["read_digest"] != tomllib.loads(before)["model"][0][
                   "read_digest"], out)

        a_dg = tomllib.loads(clean)["model"][0]["read_digest"]
        text = (f'[[model]]\r\nlean = "formal/LamboProofs/A.lean"\r\nread_digest = "{a_dg}"\r\n'
                f'rust = ["src/a.rs", "src/a2.rs"]\r\n\r\n[[model]]\r\nrust = ["src/b.rs#keep"]\r\n'
                f'lean = "formal/LamboProofs/B.lean"')
        write(MANIFEST, text)
        code, out = run("--bump", "B")
        after = read(MANIFEST)
        expect("CRLF and no final newline, the line inserted after the last: CRLF kept, still no "
               "final newline",
               code == 0 and after.startswith(text + "\r\n")
               and after[len(text) + 2:].startswith(f'read_digest = "{dp}')
               and not after.endswith("\n") and "\n" not in after.replace("\r\n", ""), repr(after))

        print("self-test: --bump through a symlinked manifest")
        link = os.path.join(d, *MANIFEST.split("/"))
        real = os.path.join(d, "formal", "real-models.toml")
        write(MANIFEST, clean.replace(f'read_digest = "{dp}', f'read_digest = "{dp}0', 1))
        os.replace(link, real)
        os.symlink("real-models.toml", link)
        code, out = run("--bump", "A")
        expect("written through the link, which stays a link",
               code == 0 and os.path.islink(link) and read("formal/real-models.toml") == clean, out)

    if failures:
        print(f"self-test: {len(failures)} failed")
        for f in failures:
            print(f"--- {f}")
        return 1
    print("self-test: all passed")
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
