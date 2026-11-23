#!/usr/bin/env python3
"""Check that a change to the Lean models restates nothing they prove.

  python3 -I scripts/formal/check-decl-equivalence.py [--base REF] [--renames FILE]
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
  per line (`#` starts a comment), as a helper moved to the shared library:
  NEW must exist at HEAD with OLD's kind and type, and several OLD names may
  map to one NEW (duplicates merged). Names in FILE rewrite references in the
  compared bodies too;
* no declaration is added at HEAD other than a NEW of FILE;
* every theorem at HEAD rests on `propext`, `Quot.sound` and
  `Classical.choice` at most.

Which module a declaration lives in is reported (a summary of the moves), not
compared, so splitting a model file into several is allowed. A private name
is compared as its user-facing name, and a binder name without its macro
scopes (a hygienic name records the module it was elaborated in). Only declarations written in the source
are compared; the constants Lean generates for them (`foo.proof_1`,
`foo.match_1`, `_auxLemma.N`, ...) are not, as their numbering follows the
file layout.

Exit 0 when the only differences are the listed renames, 1 otherwise. Needs
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
        for line in open(path, encoding="utf-8"):
            d = json.loads(line)
            self.all[d["name"]] = d
            if d["user"]:
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
        line = raw.split("#", 1)[0].strip()
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


def compare(bd, hd, renames):
    base, head = bd.user, hd.user
    errors, notes = [], []
    targets = set(renames.values())
    for name, b in sorted(base.items()):
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
            notes.append(f"{name}: private {b['private']} -> {h['private']}")
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
        errors, notes = compare(bd, hd, renames)
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
        for n in notes:
            print(f"  note: {n}")
        if errors:
            print(f"{len(errors)} difference(s):", file=sys.stderr)
            for e in errors:
                print(f"  {e}", file=sys.stderr)
            return 1
        print("equivalent: every statement and definition unchanged; "
              "theorems rest on standard axioms only")
        return 0
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
