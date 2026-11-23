#!/usr/bin/env python3
"""Check that a change to the Lean models restates nothing they prove.

  python3 -I scripts/formal/check-decl-equivalence.py [--base REF] [--renames FILE]
      [--deletions FILE]
      [--keep DIR] [--base-dump F] [--head-dump F]

Builds the models at REF (default `origin/main`, exported with `git archive`
into a temporary directory) and in the working tree, dumps every declaration
of every module under `formal/LamboProofs/` with `scripts/formal/DumpDecls.lean`
(a Lean program that walks the environment, so it reads what Lean elaborated,
not the source text), and compares the two by fully qualified name:

* every theorem at REF exists at HEAD with the same kind, universe parameters
  and type, as the kernel term (binder names included), so no theorem is
  weakened, strengthened, restated or deleted. Its proof may differ;
* every definition, inductive, constructor and structure keeps its type, and
  a definition its body, so the theorems still speak of the same things;
* a declaration may change name only when FILE lists it, one `OLD<TAB>NEW`
  per line (a line starting with `#` is a comment), as a helper moved to the shared library:
  NEW must exist at HEAD with OLD's kind and type, and several OLD names may
  map to one NEW (duplicates merged). Names in FILE rewrite references in the
  compared bodies too;
* a theorem may be missing at HEAD only when `--deletions FILE` lists it,
  one `NAME<TAB>REASON<TAB>KEPT` per line (a line starting with `#` is a
  comment; a reason may contain `#`, as in an issue number): NAME is a
  theorem at the base and not at HEAD, REASON says why it went, and KEPT is
  the theorem at HEAD that states the same thing or more (`-` when there is
  none, as for a dead helper). Only theorems can be listed: a definition's
  deletion would change what the others state;
* no declaration is added at HEAD other than a NEW of FILE;
* every theorem at HEAD rests on `propext`, `Quot.sound` and
  `Classical.choice` at most.

Which module a declaration lives in is reported (a summary of the moves), not
compared, so splitting a model file into several is allowed. A private name
is compared as its user-facing name (a declaration that goes private or
public fails: that changes its kernel name), and a binder name without its macro
scopes (a hygienic name records the module it was elaborated in). Only declarations written in the source
are compared; the constants Lean generates for them (`foo.proof_1`,
`foo.match_1`, `_auxLemma.N`, ...) are not, as their numbering follows the
file layout. Nor are the syntax extensions a file declares (a `local
notation`'s parser and unexpander, a `macro`'s rules: declarations whose type
is `Lean.Macro`, `Lean.ParserDescr` and the like), whose names embed the
module; they change no kernel term, and every statement that uses them is
compared as the kernel term it elaborated to.

Exit 0 when the only differences are the listed renames and deletions, 1
otherwise. Needs
`lake` (`~/.elan/bin` on PATH, or LAKE set) and git.
"""

import argparse
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile
from collections import Counter
from pathlib import Path

STANDARD_AXIOMS = {"propext", "Quot.sound", "Classical.choice"}
# The types of the declarations a syntax extension generates (`notation`,
# `infix`, `macro`, `syntax`, `macro_rules`, an unexpander or elaborator).
SYNTAX_TYPES = {
    "Lean.Macro", "Lean.ParserDescr", "Lean.TrailingParserDescr",
    "Lean.PrettyPrinter.Unexpander", "Lean.PrettyPrinter.Delaborator.Delab",
    "Lean.Elab.Term.TermElab", "Lean.Elab.Tactic.Tactic", "Lean.Elab.Command.CommandElab",
}
PRIVATE = re.compile(r"_private\.(?:[^.\s]+\.)*?0\.")


def lake():
    exe = os.environ.get("LAKE") or shutil.which("lake")
    if not exe:
        cand = Path.home() / ".elan" / "bin" / "lake"
        exe = str(cand) if cand.exists() else None
    if not exe:
        sys.exit("lake not found: put ~/.elan/bin on PATH or set LAKE")
    return exe


def modules(formal):
    root = Path(formal) / "LamboProofs"
    mods = []
    for p in sorted(root.rglob("*.lean")):
        rel = p.relative_to(formal).with_suffix("")
        if rel.parts == ("LamboProofs", "Audit"):
            continue
        mods.append(".".join(rel.parts))
    return mods


def dump(formal, script, out):
    """Build the models under `formal` and dump their declarations to `out`."""
    exe = lake()
    print(f"building {formal} ...", file=sys.stderr)
    subprocess.run([exe, "build", "LamboProofs"], cwd=formal, check=True,
                   stdout=subprocess.DEVNULL)
    subprocess.run([exe, "env", "lean", "--run", str(script), str(out), *modules(formal)],
                   cwd=formal, check=True)


# A generated constant: a matcher, an abstracted proof, or any name with a
# component Lean starts with `_` (`_auxLemma.3`, `_sparseCasesOn_1`, ...).
AUX = re.compile(r"[^\s()\[\]{},:]+?\.(?:match_\d+|proof_\d+|_[A-Za-z][\w']*)(?:\.[\w']+)*(?![\w'])")


class Dump:
    """One dump: the declarations written in the source (`user`), and every
    constant, so that a reference to a generated one can be compared by what
    it is rather than by its name (Lean reuses an identical matcher it has
    already made in the same module, so `foo.match_1` in one layout can be
    `bar.match_1` in another)."""

    def __init__(self, path, renames):
        self.user, self.all = {}, {}
        with open(path, encoding="utf-8") as fh:
            for line in fh:
                d = json.loads(line)
                if "record" in d:
                    continue
                self.all[d["name"]] = d
                if d["user"] and d["type"] not in SYNTAX_TYPES:
                    self.user[d["name"]] = d
        self.renames = renames
        self._aux = {}

    def aux(self, name):
        if name not in self._aux:
            self._aux[name] = "aux"
            d = self.all.get(name)
            body = name if d is None else self.norm(d["type"]) + "|" + str(self.norm(d["value"]))
            self._aux[name] = "aux#" + hashlib.sha256(body.encode()).hexdigest()[:16]
        return self._aux[name]

    def norm(self, s):
        if s is None:
            return None
        s = PRIVATE.sub("", s)
        s = AUX.sub(lambda m: self.aux(PRIVATE.sub("", m.group(0))), s)
        if self.renames:
            s = RENAME_RE(self.renames).sub(lambda m: self.renames[m.group(0)], s)
        return s


_rename_cache = {}


def RENAME_RE(renames):
    key = id(renames)
    if key not in _rename_cache:
        alts = sorted(renames, key=len, reverse=True)
        _rename_cache[key] = re.compile(
            r"(?<![\w.'])(?:" + "|".join(re.escape(a) for a in alts) + r")(?![\w'])"
        )
    return _rename_cache[key]


def read_renames(path):
    renames = {}
    if not path:
        return renames
    for raw in open(path, encoding="utf-8"):
        line = "" if raw.lstrip().startswith("#") else raw.strip()
        if not line:
            continue
        old, new = line.split("\t") if "\t" in line else line.split()
        renames[old.strip()] = new.strip()
    return renames


def first_difference(a, b, context=80):
    """Where two normalised strings first differ, as two short lines."""
    i = 0
    while i < min(len(a), len(b)) and a[i] == b[i]:
        i += 1
    lo = max(0, i - context)
    return f"    base: ...{a[lo:i + context]}\n    head: ...{b[lo:i + context]}"


def read_deletions(path):
    """NAME -> (REASON, KEPT or None)."""
    deletions = {}
    if not path:
        return deletions
    for raw in open(path, encoding="utf-8"):
        line = raw.rstrip("\n")
        if not line.strip() or line.lstrip().startswith("#"):
            continue
        parts = [x.strip() for x in line.split("\t")]
        if len(parts) != 3 or not parts[0] or not parts[1] or not parts[2]:
            raise SystemExit(f"{path}: want NAME<TAB>REASON<TAB>KEPT, got {raw!r}")
        deletions[parts[0]] = (parts[1], None if parts[2] == "-" else parts[2])
    return deletions


def compare(bd, hd, renames, deletions=None):
    base, head = bd.user, hd.user
    deletions = deletions or {}
    errors, notes = [], []
    targets = set(renames.values())
    for name, (reason, kept) in sorted(deletions.items()):
        b = base.get(name)
        if b is None:
            errors.append(f"{name}: listed as deleted, but not declared at the base")
        elif b["kind"] != "theorem":
            errors.append(f"{name}: listed as deleted, but a {b['kind']}: only theorems can be deleted")
        if name in head:
            errors.append(f"{name}: listed as deleted, but still declared")
        if kept is not None and kept not in head:
            errors.append(f"{name}: listed as deleted for {kept}, which is not declared at HEAD")
    for name, b in sorted(base.items()):
        if name in deletions:
            continue
        new = renames.get(name, name)
        h = head.get(new)
        if name in renames and name in head:
            errors.append(f"{name}: listed as moved to {new}, but still declared")
            continue
        if h is None:
            errors.append(f"{name}: {b['kind']} at the base, not declared at HEAD"
                          + (f" (listed as moved to {new})" if new != name else ""))
            continue
        if b["kind"] != h["kind"]:
            errors.append(f"{name}: kind {b['kind']} -> {h['kind']}")
        if b["levels"] != h["levels"]:
            errors.append(f"{name}: universe parameters {b['levels']} -> {h['levels']}")
        bt, ht = bd.norm(b["type"]), hd.norm(h["type"])
        if bt != ht:
            errors.append(f"{name}: statement changed\n{first_difference(bt, ht)}")
        if b["kind"] != "theorem" and bd.norm(b["value"]) != hd.norm(h["value"]):
            errors.append(f"{name}: definition body changed")
        if b["private"] != h["private"]:
            # A private name is `_private.<module>.0.<name>` in the kernel, so
            # going private or public renames the declaration.
            errors.append(f"{name}: {'private' if b['private'] else 'public'} at the base, "
                          f"{'private' if h['private'] else 'public'} at HEAD")
    for name in sorted(set(head) - set(base)):
        if name not in targets:
            errors.append(f"{name}: {head[name]['kind']} added at HEAD (not a listed move)")
    for old, new in renames.items():
        if old not in base:
            errors.append(f"{old}: listed as moved, but not declared at the base")
    for name, h in sorted(head.items()):
        if h["kind"] == "theorem":
            bad = set(h["axioms"] or []) - STANDARD_AXIOMS
            if bad:
                errors.append(f"{name}: rests on {sorted(bad)}")
    return errors, notes


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--repo", default=".", help="repository root (default: .)")
    ap.add_argument("--base", default="origin/main", help="git ref to compare against")
    ap.add_argument("--renames", help="OLD<TAB>NEW lines: helpers moved to the shared library")
    ap.add_argument("--deletions", help="NAME<TAB>REASON<TAB>KEPT lines: theorems deleted on purpose")
    ap.add_argument("--keep", help="write the two dumps into this directory")
    ap.add_argument("--base-dump", help="reuse a dump of the base instead of building it")
    ap.add_argument("--head-dump", help="reuse a dump of HEAD instead of building it")
    args = ap.parse_args(argv)
    repo = Path(args.repo).resolve()
    script = repo / "scripts" / "formal" / "DumpDecls.lean"
    work = Path(args.keep) if args.keep else Path(tempfile.mkdtemp(prefix="decl-eq-"))
    work.mkdir(parents=True, exist_ok=True)
    try:
        base_dump = Path(args.base_dump) if args.base_dump else work / "base.jsonl"
        head_dump = Path(args.head_dump) if args.head_dump else work / "head.jsonl"
        if not args.base_dump:
            tree = work / "base-tree"
            if tree.exists():
                shutil.rmtree(tree)
            tree.mkdir()
            arch = subprocess.run(["git", "archive", "--format=tar", args.base, "formal"],
                                  cwd=repo, check=True, capture_output=True).stdout
            subprocess.run(["tar", "-x", "-C", str(tree)], input=arch, check=True)
            dump(tree / "formal", script, base_dump.resolve())
        if not args.head_dump:
            dump(repo / "formal", script, head_dump.resolve())
        renames = read_renames(args.renames)
        bd, hd = Dump(base_dump, renames), Dump(head_dump, {})
        base, head = bd.user, hd.user
        deletions = read_deletions(args.deletions)
        errors, notes = compare(bd, hd, renames, deletions)
        kinds = Counter(d["kind"] for d in base.values())
        moved = Counter((b["module"], head[renames.get(n, n)]["module"])
                        for n, b in base.items()
                        if renames.get(n, n) in head and head[renames.get(n, n)]["module"] != b["module"])
        print(f"base {args.base}: {len(base)} declarations "
              f"({kinds['theorem']} theorems); HEAD: {len(head)}")
        for (a, b), k in sorted(moved.items()):
            print(f"  moved {k:4d}  {a} -> {b}")
        if renames:
            print(f"  renamed {len(renames)} (moved helpers):")
            for old, new in sorted(renames.items()):
                print(f"    {old} -> {new}")
        if deletions:
            print(f"  deleted {len(deletions)}:")
            for name, (reason, kept) in sorted(deletions.items()):
                print(f"    {name}: {reason}" + (f" (kept: {kept})" if kept else ""))
        for n in notes:
            print(f"  note: {n}")
        if errors:
            print(f"{len(errors)} difference(s):", file=sys.stderr)
            for e in errors:
                print(f"  {e}", file=sys.stderr)
            return 1
        print("equivalent: every statement and definition unchanged"
              + (" but the listed moves and deletions" if renames or deletions else "")
              + "; theorems rest on standard axioms only")
        return 0
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
