#!/usr/bin/env python3
"""Check that a change to the Lean models restates nothing they prove.

  python3 -I scripts/formal/check-decl-equivalence.py [--base REF] [--renames FILE]
      [--deletions FILE] [--spec-form | --only-form-changes]
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
  none, as for a dead helper). A definition (or other non-theorem) can be
  listed only when no declaration left at HEAD mentions it in its type or
  value, so its deletion changes nothing the others state;
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

`--spec-form` is for a change that puts a model into Spec and Proofs form
(formal/README.md, "Reading a Spec file"): the statement of `theorem foo`
moves into a definition `Spec.foo : Prop` and the theorem becomes `theorem foo
: Spec.foo`, so its type is no longer the old term. With the flag, a theorem
whose type differs passes when it changed form and still states the same
thing:

* its type at HEAD is exactly the definition `Lambo.<Model>.Spec.<name>`, for
  the theorem `Lambo.<Model>.<name>`, of type `Prop`;
* that definition's body, with the proofs Lean abstracted out of it
  (`foo._proof_1`) put back, is the base's statement, as the kernel term
  (`DumpDecls.lean` does the unfolding in Lean, and has the kernel confirm
  that `Spec.foo` and the body are definitionally equal). So `Spec.foo`
  unfolds to the old statement by `rfl`: nothing was weakened, no hypothesis
  added, no conclusion dropped. The printed term names a bound variable only
  where something refers to it, so a hypothesis written `(h : P)` before and
  `P →` now compares equal;
* the definition `Spec.foo` may be added at HEAD, but only as the statement
  of the theorem of its name.

A theorem that leaves Spec form is checked the same way round, and one in
Spec form on both sides is compared as before: its type is the same constant,
and that definition's body must not change. Everything else is as without
the flag.

`--only-form-changes` runs that one check and nothing else, for CI on every
pull request: for each theorem declared on both sides whose statement is in
Spec form on one side and spelled out on the other, the two statements must
be the same, and so must every definition under `LamboProofs` the statement
reaches, transitively through definition bodies (its kind, type and body):
this mode compares no definition by itself, and a statement that prints as
before says something else once a definition it mentions changed. A theorem
added, deleted or restated without changing form is not its business (a
model change may do all three), so it passes when no theorem changed form.

The base is built from scratch unless the working tree has a `formal/.lake`:
then the base starts from a copy of it and Lake rebuilds only the modules
whose sources differ.

Exit 0 when the only differences are the listed renames and deletions (and,
with `--spec-form`, the theorems restated in Spec form), 1 otherwise. Needs
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


def mentioned_by(dump, name):
    """The declarations of `dump` whose type or value mentions `name`."""
    pat = re.compile(r"(?<![\w.'])" + re.escape(name) + r"(?![\w'])")
    return sorted(n for n, d in dump.all.items()
                  if pat.search(d["type"] or "") or pat.search(str(d["value"] or "")))


def spec_name(name):
    """The Spec proposition of a theorem: `Lambo.Drain.Spec.foo` for
    `Lambo.Drain.foo` (`Spec` goes after the model's namespace)."""
    parts = name.split(".")
    return ".".join(parts[:2] + ["Spec"] + parts[2:]) if len(parts) > 2 else None


def statement(dump, name, d):
    """What a theorem states, as the kernel term, and the error if it is in
    Spec form but not well formed: its `Spec` definition's body when its type
    is that definition, its type otherwise."""
    if not d.get("spec"):
        return dump.norm(d["type"]), None
    if d["spec"] != spec_name(name):
        return None, (f"{name}: stated as {d['spec']}, but its Spec proposition must be "
                      f"{spec_name(name)}")
    if not d.get("spec_rfl"):
        return None, f"{name}: the kernel does not accept {d['spec']} as its own body"
    return dump.norm(d["stmt"]), None


# A constant's name in a printed kernel term: the text between separators,
# with a universe instantiation (`Eq.{1}` prints as `Eq.` then `{1}`) cut off.
NAME_TOKEN = re.compile(r"[^\s()\[\]{},:@]+")


def reached_definitions(dump, roots):
    """The constants of `dump` other than theorems that `roots` reach, through
    the bodies of definitions: what a statement mentioning `roots` means. A
    theorem is not followed (its value is a proof, not a meaning), and a
    constant outside the dump (Lean's own library) ends the walk."""
    seen, stack = set(), list(roots)
    while stack:
        n = stack.pop()
        if n in seen:
            continue
        d = dump.all.get(n)
        if d is None or d["kind"] == "theorem":
            continue
        seen.add(n)
        stack.extend(d["uses"] or [])
    return seen


def statement_roots(dump, stmt, extra=()):
    """The constants of `dump` that a normalised statement names, and `extra`."""
    roots = {tok.rstrip(".") for tok in NAME_TOKEN.findall(stmt or "")}
    return {n for n in roots | set(extra) if n in dump.all}


def changed_definitions(bd, hd, bstmt, hstmt, h):
    """The definitions a restated theorem's statement reaches, transitively
    through definition bodies, that differ between base and HEAD: missing on
    one side, or of another kind, type or body. The two statements can print
    alike while a definition they mention changed underneath, so the same
    statement would no longer say the same thing. HEAD's Spec proposition
    also contributes the constants its body uses."""
    prop = hd.all.get(h.get("spec") or "")
    base = reached_definitions(bd, statement_roots(bd, bstmt))
    head = reached_definitions(hd, statement_roots(hd, hstmt, (prop or {}).get("uses") or ()))
    out = []
    for n in sorted({bd.renames.get(x, x) for x in base} | head):
        old = next((x for x, y in bd.renames.items() if y == n), n)
        b, a = bd.all.get(old), hd.all.get(n)
        if (b or a) and not (b or a)["user"]:
            # A generated constant (a matcher, an equation lemma) is compared
            # through the user declaration it belongs to.
            continue
        if b is None or a is None:
            out.append(f"{n} (declared at {'HEAD' if b is None else 'the base'} only)")
        elif b["kind"] != a["kind"]:
            out.append(f"{n} (kind changed)")
        elif bd.norm(b["type"]) != hd.norm(a["type"]):
            out.append(f"{n} (type changed)")
        elif bd.norm(b["value"]) != hd.norm(a["value"]):
            out.append(f"{n} (body changed)")
    return out


def form_change(bd, hd, name, new, b, h):
    """For a theorem in Spec form on one side only: None when both sides state
    the same thing, else the error. The statements must print alike, and
    every definition they reach must be unchanged."""
    bs, berr = statement(bd, name, b)
    hs, herr = statement(hd, new, h)
    if berr or herr:
        return berr or herr
    if bs != hs:
        which = h.get("spec") or b.get("spec")
        return (f"{name}: statement changed: {which} does not unfold to the "
                f"{'base' if h.get('spec') else 'HEAD'} statement\n{first_difference(bs, hs)}")
    changed = changed_definitions(bd, hd, bs, hs, h if h.get("spec") else b if b.get("spec") else h)
    if changed:
        return (f"{name}: statement changed: it prints as before, but a definition it reaches "
                f"changed: {', '.join(changed)}")
    return None


def compare_forms(bd, hd):
    """`--only-form-changes`: the theorems declared on both sides whose
    statement is in Spec form on one side only."""
    errors, restated = [], []
    for name, b in sorted(bd.user.items()):
        h = hd.user.get(name)
        if h is None or b["kind"] != "theorem" or h["kind"] != "theorem":
            continue
        if bool(b.get("spec")) == bool(h.get("spec")):
            continue
        err = form_change(bd, hd, name, name, b, h)
        if err:
            errors.append(err)
        else:
            restated.append(name)
    return errors, restated


def compare(bd, hd, renames, deletions=None, spec_form=False, restated=None):
    base, head = bd.user, hd.user
    deletions = deletions or {}
    errors, notes = [], []
    restated = [] if restated is None else restated
    targets = set(renames.values())
    for name, (reason, kept) in sorted(deletions.items()):
        b = base.get(name)
        if b is None:
            errors.append(f"{name}: listed as deleted, but not declared at the base")
        elif b["kind"] != "theorem":
            users = mentioned_by(hd, name)
            if users:
                errors.append(f"{name}: listed as deleted, but a {b['kind']} that {users[0]} at HEAD "
                              f"still mentions: only an unused definition can be deleted")
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
            changed_form = (b["kind"] == h["kind"] == "theorem"
                            and bool(b.get("spec")) != bool(h.get("spec")))
            if spec_form and changed_form:
                err = form_change(bd, hd, name, new, b, h)
                if err:
                    errors.append(err)
                else:
                    restated.append(new)
            else:
                errors.append(f"{name}: statement changed\n{first_difference(bt, ht)}"
                              + ("\n    (it changed form: --spec-form compares the statement behind "
                                 "a Spec proposition)" if changed_form else ""))
        if b["kind"] != "theorem" and bd.norm(b["value"]) != hd.norm(h["value"]):
            errors.append(f"{name}: definition body changed")
        if b["private"] != h["private"]:
            # A private name is `_private.<module>.0.<name>` in the kernel, so
            # going private or public renames the declaration.
            errors.append(f"{name}: {'private' if b['private'] else 'public'} at the base, "
                          f"{'private' if h['private'] else 'public'} at HEAD")
    # A Spec proposition may be added only as the statement of the theorem of
    # its name, restated from the base.
    stated = {head[n]["spec"] for n in restated if head[n].get("spec")}
    for name in sorted(set(head) - set(base)):
        if name in targets:
            continue
        h = head[name]
        if spec_form and name in stated and h["kind"] == "def" and h["type"] == "Prop":
            continue
        errors.append(f"{name}: {h['kind']} added at HEAD (not a listed move"
                      + (", nor the Spec proposition of a restated theorem)" if spec_form else ")"))
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
    form = ap.add_mutually_exclusive_group()
    form.add_argument("--spec-form", action="store_true",
                      help="accept a theorem restated as `theorem foo : Spec.foo` when Spec.foo unfolds "
                           "to its old statement, and the Spec propositions added for them")
    form.add_argument("--only-form-changes", action="store_true",
                      help="check only the theorems whose statement moved into or out of Spec form")
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
            built = repo / "formal" / ".lake"
            if built.is_dir():
                # Start the base from the working tree's build: Lake rebuilds
                # only the modules whose sources differ.
                shutil.copytree(built, tree / "formal" / ".lake", symlinks=True)
            dump(tree / "formal", script, base_dump.resolve())
        if not args.head_dump:
            dump(repo / "formal", script, head_dump.resolve())
        renames = read_renames(args.renames)
        bd, hd = Dump(base_dump, renames), Dump(head_dump, {})
        base, head = bd.user, hd.user
        if args.only_form_changes:
            errors, restated = compare_forms(bd, hd)
            print(f"base {args.base}: {len(restated) + len(errors)} theorem(s) changed form")
            for n in restated:
                print(f"  restated  {n}")
            if errors:
                print(f"{len(errors)} difference(s):", file=sys.stderr)
                for e in errors:
                    print(f"  {e}", file=sys.stderr)
                return 1
            print("equivalent: every theorem that moved into or out of Spec form states what it stated"
                  if restated else "no theorem changed form")
            return 0
        deletions = read_deletions(args.deletions)
        restated = []
        errors, notes = compare(bd, hd, renames, deletions, spec_form=args.spec_form, restated=restated)
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
        if restated:
            by_model = Counter(".".join(n.split(".")[:2]) for n in restated)
            print(f"  restated {len(restated)} in Spec form, each unfolding to its old statement: "
                  + ", ".join(f"{k} {v}" for k, v in sorted(by_model.items())))
        for n in notes:
            print(f"  note: {n}")
        if errors:
            print(f"{len(errors)} difference(s):", file=sys.stderr)
            for e in errors:
                print(f"  {e}", file=sys.stderr)
            return 1
        print("equivalent: every statement and definition unchanged"
              + (" but the listed moves and deletions" if renames or deletions else "")
              + (", the restated theorems as their Spec propositions unfold" if restated else "")
              + "; theorems rest on standard axioms only")
        return 0
    finally:
        if not args.keep:
            shutil.rmtree(work, ignore_errors=True)


if __name__ == "__main__":
    sys.exit(main())
