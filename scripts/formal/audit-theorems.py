#!/usr/bin/env python3
"""Find the theorems in the Lean models that nothing references.

  python3 -I scripts/formal/audit-theorems.py [--dump FILE] [--out FILE] [--repo DIR]
  python3 -I scripts/formal/audit-theorems.py --duplicates [--dump FILE]
  python3 -I scripts/formal/audit-theorems.py --check [--dump FILE]

Reads a dump of the elaborated models (`scripts/formal/DumpDecls.lean`; built
here when `--dump` is not given) and, for every theorem declared in a model
file (as `count-theorems.py` counts them: the shared lemmas under `Lib/` are
not model files), records what references it:

* proof: another declaration's proof or definition uses it, through the
  constants Lean generated for it (real usage, from the environment, not
  name matching);
* named: another declaration's text names it (a lemma proved by `rfl` and
  handed to `simp` or `dsimp` is applied definitionally and leaves no trace
  in the proof term, so this is textual: its name, outside comments and
  outside its own declaration, in a model of its namespace, or qualified
  as `Model.name` in any model);
* example: an `example` in the models names it (examples are not kept in
  the environment, so this one is textual: an identifier in the example's
  text that is a theorem of the example's namespace);
* anchor: a docs page's `{/* lean: Model.name */}` anchor;
* ledger: a claims ledger under `formal/claims/` names it in backticks, in
  a line that names its model file (or any model, when the line names none);
* rust: a comment in `src/` or `tests/` names it as `Model.name`, or in
  backticks within ten lines of a mention of its model file;
* audit: `Audit.lean` prints its axioms.

A theorem with none of these is unreferenced. For each, the report gives a
class and a one-line reason, from what the sources say about it:

* (a) a standalone property worth keeping: its model's header or
  `formal/README.md` names it as a result, the CHANGELOG names it, its doc
  comment opens with a bold title (the models' mark for a result) or quotes
  the Rust or names a Rust test, or it is a closed statement about a
  concrete case;
* (b) a deletion candidate, only when it is shown to be one: a referenced
  theorem that stays states the same thing, or supersedes it (its name with
  a current-model suffix, `foo` and `fooW`, or the counterpart its doc
  comment names) and its doc comment does not state it as a result (a
  finding's bold title keeps it in (a)); or it is a dead helper, an
  undocumented `rfl` unfolding of a definition that nothing uses or names;
* (c) unclear: anything else, such as an undocumented lemma that may be a
  property in its own right (an asymmetry, an invariant) or a leftover.

Precedence: a same-statement twin (b); a result named in a header, the
README or the CHANGELOG, or a bold title (a); a superseding counterpart (b);
the other (a) criteria; a `@[simp]` lemma (c); a dead helper (b); the rest
(c). The classes are a first sort from those signals, for a person to
confirm.

`--check` exits 1 when a theorem is neither used by a proof (or named by
another declaration) nor carries a doc comment: the rule every theorem
follows (formal/README.md, "How the models are written"). CI runs it.

`--duplicates` lists theorems that state the same thing (the same `shape`:
binders named by depth, universe parameters by position) in more than one
declaration, for finding helpers to share in `Lib/`.
"""

import argparse
import importlib.util
import json
import re
import subprocess
import sys
from collections import Counter, defaultdict
from pathlib import Path

HERE = Path(__file__).resolve().parent
_spec = importlib.util.spec_from_file_location("count_theorems", HERE / "count-theorems.py")
ct = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(ct)

SIGNALS = ("proof", "named", "example", "anchor", "ledger", "rust", "audit")
ANCHOR = re.compile(r"\{/\* lean: (?P<body>[^*]*?) \*/\}")
TICKED = re.compile(r"`([A-Za-z_][\w'.]*)`")
QUALIFIED = re.compile(r"(?<![\w.])([A-Z][A-Za-z]*)\.([a-z_A-Z][\w']*)")
LEAN_FILE = re.compile(r"(?:LamboProofs/)?((?:[A-Z][A-Za-z]*/)?[A-Z][A-Za-z]*)\.lean")


def build_dump(repo, out):
    formal = repo / "formal"
    lake = "lake"
    for cand in (Path.home() / ".elan" / "bin" / "lake",):
        if cand.exists():
            lake = str(cand)
    subprocess.run([lake, "build", "LamboProofs"], cwd=formal, check=True, stdout=subprocess.DEVNULL)
    mods = []
    for p in sorted((formal / "LamboProofs").rglob("*.lean")):
        rel = p.relative_to(formal).with_suffix("")
        if rel.parts != ("LamboProofs", "Audit"):
            mods.append(".".join(rel.parts))
    subprocess.run([lake, "env", "lean", "--run", str(HERE / "DumpDecls.lean"), str(out), *mods],
                   cwd=formal, check=True)


def fnv1a64(data):
    """FNV-1a (64-bit) of bytes, as DumpDecls.lean writes it."""
    h = 14695981039346656037
    for b in data:
        h = ((h ^ b) * 1099511628211) & 0xFFFFFFFFFFFFFFFF
    return f"{h:016x}"


def module_path(repo, module):
    return repo / "formal" / Path(*module.split(".")).with_suffix(".lean")


def namespace_of(name):
    """`Drain` for `Lambo.Drain.foo` (the model namespace)."""
    parts = name.split(".")
    return parts[1] if len(parts) > 2 and parts[0] == "Lambo" else None


class Models:
    def __init__(self, repo, dump):
        self.repo = repo
        self.env = {}
        self.hashes = {}
        with open(dump, encoding="utf-8") as fh:
            for line in fh:
                d = json.loads(line)
                if d.get("record") == "module":
                    self.hashes[d["module"]] = d["fnv1a64"]
                    continue
                self.env[d["name"]] = d
        self.user = {n: d for n, d in self.env.items() if d["user"]}
        # The theorems count-theorems.py counts: a `theorem` line in a model file.
        files = {p.resolve() for p in ct.model_files(repo)}
        decl_lines = defaultdict(set)
        self.text = {}
        for p in files:
            src = p.read_text(encoding="utf-8")
            self.text[p] = src
            for i, line in enumerate(ct.strip_comments(src).split("\n"), 1):
                if ct.DECL.match(line):
                    decl_lines[p].add(i)
        self.theorems = {}
        for n, d in self.user.items():
            if d["kind"] != "theorem":
                continue
            p = module_path(repo, d["module"]).resolve()
            if p in files and d["line"] in decl_lines[p]:
                self.theorems[n] = d
        # A model file whose source no longer hashes to what the dump was made
        # from: the dump is older than the sources, and its lines (or more) are
        # wrong. A dump with no module records cannot be checked and is stale.
        self.stale = []
        for p in sorted(files):
            module = ".".join(p.relative_to((repo / "formal").resolve()).with_suffix("").parts)
            if self.hashes.get(module) != fnv1a64(p.read_bytes()):
                self.stale.append(p.relative_to(repo.resolve()).as_posix())
        # Nested declarations (structure fields) belong to their parent.
        spans = sorted(((d["span"][0], -d["span"][1], d["module"], n) for n, d in self.user.items()))
        self.parent = {}
        stack = []
        for s, ne, m, n in spans:
            e = -ne
            while stack and (stack[-1][2] != m or stack[-1][1] < s):
                stack.pop()
            if stack and stack[-1][0] <= s and e <= stack[-1][1]:
                self.parent[n] = stack[-1][3]
            stack.append((s, e, m, n))

    def owner(self, n):
        while n in self.parent:
            n = self.parent[n]
        return n

    def proof_users(self):
        """For each theorem, the user declarations whose type or value uses it."""
        users = defaultdict(set)
        memo = {}

        def reach(n):
            if n in memo:
                return memo[n]
            memo[n] = set()
            out, stack, seen = set(), list(self.env[n]["uses"]), set()
            while stack:
                u = stack.pop()
                if u in seen:
                    continue
                seen.add(u)
                if u in self.user:
                    out.add(u)
                elif u in self.env:
                    stack.extend(self.env[u]["uses"])
            memo[n] = out
            return out

        for n in self.user:
            o = self.owner(n)
            for u in reach(n):
                if u in self.theorems and u != o and u != n:
                    users[u].add(o)
        return users


def lean_files_text(m):
    return {p: t for p, t in m.text.items()}


def example_refs(m):
    refs = defaultdict(set)
    for p, src in m.text.items():
        code = ct.strip_comments(src).split("\n")
        ns = None
        i = 0
        while i < len(code):
            line = code[i]
            if mm := re.match(r"namespace Lambo\.(\w+)", line):
                ns = ns or mm.group(1)
            if line.startswith("example"):
                j = i + 1
                while j < len(code) and (code[j].startswith((" ", "\t")) or code[j] == ""):
                    j += 1
                body = "\n".join(code[i:j])
                for tok in set(re.findall(r"[A-Za-z_][\w'.]*", body)):
                    for cand in (f"Lambo.{ns}.{tok}", f"Lambo.{tok}"):
                        if cand in m.theorems:
                            refs[cand].add(f"example in {p.name}")
                i = j
                continue
            i += 1
    return refs


def anchor_refs(m):
    refs = defaultdict(set)
    pages = list((m.repo / "site/src/content/docs").glob("*.mdx")) + list((m.repo / "docs/reference").glob("*.mdx"))
    for page in pages:
        for a in ANCHOR.finditer(page.read_text(encoding="utf-8")):
            for ref in a.group("body").split(","):
                name = "Lambo." + ref.strip()
                if name in m.theorems:
                    refs[name].add(page.relative_to(m.repo).as_posix())
    return refs


def by_short(m):
    short = defaultdict(set)
    for n in m.theorems:
        short[n.split(".")[-1]].add(n)
    return short


def models_named(line):
    """Model namespaces a line names through a `.lean` path (`Drain/Close.lean` is Drain)."""
    out = set()
    for f in LEAN_FILE.findall(line):
        out.add(f.split("/")[0])
    return out


def ledger_refs(m):
    refs = defaultdict(set)
    short = by_short(m)
    for f in sorted((m.repo / "formal" / "claims").glob("*.md")):
        for line in f.read_text(encoding="utf-8").splitlines():
            named = models_named(line)
            for tok in TICKED.findall(line):
                for n in short.get(tok.split(".")[-1], ()):
                    if tok.count(".") and "Lambo." + tok != n and not n.endswith("." + tok):
                        continue
                    if not named or namespace_of(n) in named:
                        refs[n].add(f.relative_to(m.repo).as_posix())
    return refs


def rust_refs(m):
    refs = defaultdict(set)
    short = by_short(m)
    files = []
    for d in ("src", "tests"):
        files += sorted((m.repo / d).rglob("*.rs"))
    for f in files:
        lines = f.read_text(encoding="utf-8", errors="replace").splitlines()
        rel = f.relative_to(m.repo).as_posix()
        for i, line in enumerate(lines):
            if "//" not in line:
                continue
            comment = line[line.index("//"):]
            for model, name in QUALIFIED.findall(comment):
                n = f"Lambo.{model}.{name}"
                if n in m.theorems:
                    refs[n].add(rel)
            ticks = TICKED.findall(comment)
            if not ticks:
                continue
            window = " ".join(lines[max(0, i - 10):i + 11])
            named = models_named(window)
            for tok in ticks:
                for n in short.get(tok, ()):
                    if namespace_of(n) in named:
                        refs[n].add(rel)
    return refs


def named_refs(m):
    refs = defaultdict(set)
    code = {}
    ns_of_file = {}
    for p, src in m.text.items():
        code[p] = ct.strip_comments(src).split("\n")
        mm = re.search(r"^namespace Lambo\.(\w+)", src, re.M)
        ns_of_file[p] = mm.group(1) if mm else None
    lib = (m.repo / "formal" / "LamboProofs" / "Lib")
    for p in sorted(lib.rglob("*.lean")) if lib.exists() else []:
        code[p.resolve()] = ct.strip_comments(p.read_text(encoding="utf-8")).split("\n")
        ns_of_file[p.resolve()] = "Lib"
    words = {}
    for p, lines in code.items():
        for i, line in enumerate(lines, 1):
            for tok in re.findall(r"[A-Za-z_][\w'.]*", line):
                words.setdefault(tok, []).append((p, i))
    for n, d in m.theorems.items():
        short, ns = n.split(".")[-1], namespace_of(n)
        home = module_path(m.repo, d["module"]).resolve()
        s, e = d["span"]
        for tok in (short, f"{ns}.{short}", f"Lambo.{ns}.{short}"):
            for p, i in words.get(tok, ()):
                if p == home and s <= i <= e:
                    continue
                if tok == short and ns_of_file.get(p) != ns:
                    continue
                refs[n].add(p.name)
    return refs


def audit_refs(m):
    refs = defaultdict(set)
    audit = m.repo / "formal" / "LamboProofs" / "Audit.lean"
    for mm in re.finditer(r"^#print axioms (\S+)", audit.read_text(encoding="utf-8"), re.M):
        n = mm.group(1)
        n = n if n.startswith("Lambo.") else "Lambo." + n
        if n in m.theorems:
            refs[n].add("Audit.lean")
    return refs


def header_of(src):
    """A model file's header: its comments before `namespace`."""
    cut = src.find("\nnamespace ")
    return src if cut < 0 else src[:cut]


def doc_comment(src_lines, d):
    """The theorem's own doc comment, if its span starts with one."""
    s, line = d["span"][0], d["line"]
    text = "\n".join(src_lines[s - 1:line - 1])
    return text if text.lstrip().startswith("/--") else ""


SUPERSEDING = ("W", "D", "Fixed", "_fixed", "Q", "R")
COUNTERPART = re.compile(r"(?:[Cc]urrent counterparts?|[Ss]uperseded by|[Rr]eplaced by)[^`]{0,40}`([A-Za-z_][\w'.]*)`")
RFL = re.compile(r":=\s*(?:by\s+)?rfl\s*$")


def local_name(n):
    """`n` below its model namespace: `Cmd.mem_all` for `Lambo.Resolve.Cmd.mem_all`."""
    ns = namespace_of(n)
    return n.removeprefix(f"Lambo.{ns}.") if ns else n


def counterpart(m, n, doc, referenced):
    """A referenced theorem that supersedes `n`: its name with a current-model
    suffix (`foo` and `fooW`), or one its doc comment names as its current
    counterpart."""
    for suf in SUPERSEDING:
        if n + suf in m.theorems and referenced(n + suf):
            return n + suf
    ns = namespace_of(n)
    for tok in COUNTERPART.findall(doc):
        for cand in (f"Lambo.{ns}.{tok}", f"Lambo.{tok}"):
            if cand in m.theorems and cand != n:
                return cand
    return None


def classify(m, n, d, src_lines, header, readme, changelog, doc_mentions, twins=(),
             referenced=lambda x: True):
    """(class, reason) for an unreferenced theorem; see the module doc."""
    short, local = n.split(".")[-1], local_name(n)
    if twins:
        return "b", f"states the same as `{local_name(twins[0])}`, which stays and is referenced"
    path = module_path(m.repo, d["module"])
    rel = path.relative_to(m.repo / "formal" / "LamboProofs").as_posix()
    doc = doc_comment(src_lines, d)
    ticks = {f"`{short}`", f"`{local}`"}
    if any(t in header for t in ticks):
        return "a", f"named among the results in the header of `{rel}`"
    if any(t in readme for t in ticks) or f"`{namespace_of(n)}.{local}`" in readme:
        return "a", "named as a result in `formal/README.md`"
    if any(t in changelog for t in ticks):
        return "a", "named in `CHANGELOG.md` as what a change proved"
    if doc.lstrip().startswith("/-- **"):
        title = re.sub(r"\s+", " ", doc.lstrip()[4:].split("**")[1]).strip()
        return "a", f"its doc comment states it as a result: \"{title[:90]}\""
    sup = counterpart(m, n, doc, referenced)
    if sup:
        return "b", f"superseded by `{local_name(sup)}`, its counterpart over the current model, which stays"
    if "∀" not in d["type"] and "forall" not in d["type"] and "->" not in d["type"]:
        return "a", "a closed statement about a concrete case (a worked example)"
    if doc and ("Rust test" in doc or re.search(r'"[^"]{12,}"', doc)):
        return "a", "its doc comment ties it to the Rust (quotes the source or names a Rust test)"
    above = src_lines[d["line"] - 2] if d["line"] >= 2 else ""
    if "@[simp" in src_lines[d["line"] - 1] or (above.startswith("@[") and "simp" in above):
        return "c", "a `@[simp]` lemma: `simp` may use it without naming it, so it may not be unused"
    body = "\n".join(src_lines[d["line"] - 1:d["span"][1]])
    span = d["span"][1] - d["span"][0] + 1
    if not doc and RFL.search(body):
        return "b", "a dead helper: an undocumented `rfl` unfolding of a definition that nothing uses or names"
    mentions = sorted(doc_mentions.get(short, set()) - {n})
    if mentions:
        names = ", ".join(f"`{local_name(x)}`" for x in mentions[:2])
        return "c", f"documented; only the doc comment of {names} mentions it"
    if doc:
        first = re.sub(r"\s+", " ", doc.lstrip()[3:].split("-/")[0]).strip()
        return "c", f"documented but cited nowhere: \"{first[:90]}\""
    return "c", f"undocumented {span}-line lemma, a property in its own right or a leftover"


def check(m):
    """The models' rule: every theorem is used by a proof (or named by another
    declaration, as an `rfl` lemma handed to `simp` is), or its doc comment says
    what it guarantees about the Rust. Only the doc comment's presence can be
    checked; what it says is the reviewer's."""
    used = set(m.proof_users())
    used |= set(named_refs(m))
    bad = []
    for n, d in m.theorems.items():
        if n in used:
            continue
        p = module_path(m.repo, d["module"]).resolve()
        if not doc_comment(m.text[p].split("\n"), d):
            bad.append((p.relative_to(m.repo.resolve()).as_posix(), d["line"], local_name(n)))
    for path, line, name in sorted(bad):
        print(f"{path}:{line}: {name} is used by no proof and has no doc comment stating its Rust guarantee",
              file=sys.stderr)
    if bad:
        print(f"{len(bad)} theorem(s) neither used nor documented (formal/README.md, \"How the models are "
              f"written\")", file=sys.stderr)
        return 1
    print(f"every theorem of {len(m.theorems)} is used by a proof or documented")
    return 0


def main(argv=None):
    ap = argparse.ArgumentParser(description=__doc__.split("\n")[0])
    ap.add_argument("--repo", default=".", help="repository root (default: .)")
    ap.add_argument("--dump", help="a DumpDecls.lean dump (built when absent)")
    ap.add_argument("--out", help="write the markdown report here (default: stdout)")
    ap.add_argument("--duplicates", action="store_true", help="list theorems stated more than once")
    ap.add_argument("--check", action="store_true",
                    help="exit 1 if a theorem is neither used by a proof nor carries a doc comment")
    ap.add_argument("--deleted", default="formal/audit/deleted-theorems.tsv",
                    help="the theorems deleted on purpose (NAME<TAB>REASON<TAB>KEPT), listed in the report")
    args = ap.parse_args(argv)
    repo = Path(args.repo).resolve()
    dump = Path(args.dump) if args.dump else repo / "formal" / ".lake" / "decls.jsonl"
    if not args.dump:
        build_dump(repo, dump)
    m = Models(repo, dump)
    if m.stale:
        print(f"{dump}: {m.stale[0]} ({len(m.stale)} model files in all) is not the source the dump "
              f"was made from; the dump is older than the sources, rebuild it (omit --dump)", file=sys.stderr)
        return 1

    if args.duplicates:
        groups = defaultdict(list)
        for n, d in m.user.items():
            if d["kind"] == "theorem":
                groups[d["shape"]].append(n)
        dups = [v for v in groups.values() if len(v) > 1]
        for v in sorted(dups):
            print("  ".join(sorted(v)))
        print(f"{len(dups)} statement(s) proved more than once", file=sys.stderr)
        return 0

    if args.check:
        return check(m)

    refs = {k: defaultdict(set) for k in SIGNALS}
    for n, us in m.proof_users().items():
        refs["proof"][n] |= us
    for k, f in (("named", named_refs), ("example", example_refs), ("anchor", anchor_refs), ("ledger", ledger_refs),
                 ("rust", rust_refs), ("audit", audit_refs)):
        for n, v in f(m).items():
            refs[k][n] |= v

    readme = (repo / "formal" / "README.md").read_text(encoding="utf-8")
    changelog = (repo / "CHANGELOG.md").read_text(encoding="utf-8") if (repo / "CHANGELOG.md").exists() else ""
    # Theorem names that other theorems' doc comments mention.
    doc_mentions = defaultdict(set)
    for n, d in m.theorems.items():
        lines = m.text[module_path(repo, d["module"]).resolve()].split("\n")
        for tok in TICKED.findall(doc_comment(lines, d)):
            doc_mentions[tok.split(".")[-1]].add(n)

    shapes = defaultdict(list)
    for n, d in m.theorems.items():
        shapes[d["shape"]].append(n)
    per_file = defaultdict(list)
    counts = Counter()
    for n, d in m.theorems.items():
        hit = [k for k in SIGNALS if refs[k].get(n)]
        rel = module_path(repo, d["module"]).relative_to(repo / "formal" / "LamboProofs").as_posix()
        counts[(rel, "total")] += 1
        for k in hit:
            counts[(rel, k)] += 1
        if hit:
            continue
        p = module_path(repo, d["module"]).resolve()
        src = m.text[p]
        twins = sorted(x for x in shapes[d["shape"]] if x != n and any(refs[k].get(x) for k in SIGNALS))
        cls, why = classify(m, n, d, src.split("\n"), header_of(src), readme, changelog, doc_mentions, twins,
                            referenced=lambda x: any(refs[k].get(x) for k in SIGNALS))
        per_file[rel].append((d["line"], local_name(n), cls, why))
        counts[(rel, cls)] += 1

    files = sorted({rel for rel, _ in counts})
    out = []
    w = out.append
    total = Counter()
    for (rel, k), v in counts.items():
        total[k] += v
    w("# Unreferenced theorems in the Lean models\n")
    deleted = []
    dpath = repo / args.deleted
    if dpath.exists():
        for raw in dpath.read_text(encoding="utf-8").splitlines():
            line = raw.split("#", 1)[0]
            if line.strip():
                name, reason, kept = [x.strip() for x in line.split("\t")]
                deleted.append((name, reason, kept))
    w(f"Generated by `python3 -I scripts/formal/audit-theorems.py --out formal/audit/unreferenced-theorems.md` "
      f"from the models as built.\n")
    if deleted:
        w(f"{len(deleted)} theorems this audit found unreferenced were deleted in the change that added it; "
          f"`{args.deleted}` lists them with their reasons (and `check-decl-equivalence.py --deletions` "
          f"reads it), and so does the section \"Deleted\" at the end.\n")
    w("A theorem counts as referenced when another declaration's proof or definition uses it (read from "
      "Lean's environment, through the constants Lean generates), an `example` names it, a docs page's "
      "`lean:` anchor cites it, a claims ledger under `formal/claims/` names it with its model file, a "
      "comment in `src/` or `tests/` names it (as `Model.name`, or in backticks near a mention of its "
      "model file), or `Audit.lean` prints its axioms. The rest are listed below, each with a class:\n")
    w("* **(a)** a standalone property worth keeping, ideally cited from a test, doc or ledger. Criteria: "
      "its file's header, `formal/README.md` or the CHANGELOG names it as a result; its doc comment opens "
      "with a bold title (the models' mark for a result), quotes the Rust or names a Rust test; or it is a "
      "closed statement about a concrete case.")
    w("* **(b)** a deletion candidate, only when shown to be one. Criteria: a referenced theorem that stays "
      "states the same thing, or supersedes it (its name with a current-model suffix such as `fooW`, or "
      "the current counterpart its doc comment names) and it is not stated as a result (a bold-titled doc "
      "comment, such as a finding's, keeps it in (a)); or it is a dead helper, an undocumented `rfl` "
      "unfolding of a definition that nothing uses or names.")
    w("* **(c)** unclear: anything else, such as an undocumented lemma that may be a property in its own "
      "right (an asymmetry, an invariant) or a leftover, or a `@[simp]` lemma `simp` may use unnamed.\n")
    w("Precedence: a same-statement twin (b); a result named in a header, the README or the CHANGELOG, "
      "or a bold title (a); a superseding counterpart (b); the other (a) criteria; a `@[simp]` lemma (c); "
      "a dead helper (b); the rest (c). The result is a first sort for the owner to confirm. `Drain/History.lean` describes no current Rust "
      "and is kept as the evidence for its findings. Names are given below their model's namespace "
      "(`Cmd.mem_all` is `Lambo.Resolve.Cmd.mem_all`).\n")
    w("## Summary\n")
    w(f"{total['total']} theorems in {len(files)} model files; {total['a'] + total['b'] + total['c']} "
      f"unreferenced: {total['a']} (a), {total['b']} (b), {total['c']} (c).\n")
    w("| File | Theorems | Used by a proof | Unreferenced | (a) | (b) | (c) |")
    w("| --- | ---: | ---: | ---: | ---: | ---: | ---: |")
    for rel in files:
        un = counts[(rel, "a")] + counts[(rel, "b")] + counts[(rel, "c")]
        w(f"| `{rel}` | {counts[(rel, 'total')]} | {counts[(rel, 'proof')]} | {un} | "
          f"{counts[(rel, 'a')]} | {counts[(rel, 'b')]} | {counts[(rel, 'c')]} |")
    w(f"| **Total** | {total['total']} | {total['proof']} | {total['a'] + total['b'] + total['c']} | "
      f"{total['a']} | {total['b']} | {total['c']} |\n")
    w("Other references, theorems with at least one: " + ", ".join(
        f"{k} {total[k]}" for k in SIGNALS if k != "proof") + ".\n")
    dups = sorted(sorted(v) for v in shapes.values() if len(v) > 1)
    if dups:
        w("## Statements proved twice\n")
        w("Theorems with the same statement (up to binder names) under two names; one of each pair could "
          "go, its citations moved to the other.\n")
        for v in dups:
            w("- " + ", ".join(f"`{n.removeprefix('Lambo.')}`" + (" (unreferenced)" if not any(
                refs[k].get(n) for k in SIGNALS) else "") for n in v) + ".")
        w("")
    for rel in files:
        rows = sorted(per_file.get(rel, []))
        if not rows:
            continue
        w(f"## `{rel}`\n")
        for line, short, cls, why in rows:
            w(f"- ({cls}) `{short}` (line {line}): {why}.")
        w("")
    if deleted:
        w("## Deleted\n")
        w("Each was referenced by nothing (no proof in the dump, no other declaration naming it, no "
          "example, docs anchor, claims ledger row, Rust comment, `Audit.lean` line, oracle or README) "
          "when it was deleted.\n")
        for name, reason, kept in deleted:
            w(f"- `{name.removeprefix('Lambo.')}`: {reason}"
              + (f"; kept: `{kept.removeprefix('Lambo.')}`" if kept != "-" else "") + ".")
        w("")
    text = "\n".join(out).rstrip("\n") + "\n"
    if args.out:
        Path(args.out).write_text(text, encoding="utf-8")
    else:
        sys.stdout.write(text)
    return 0


if __name__ == "__main__":
    sys.exit(main())
