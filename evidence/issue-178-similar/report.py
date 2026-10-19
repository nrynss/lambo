#!/usr/bin/env python3
"""Tabulate the EVAL lines of `similar_precision_report` against labelled-pairs.json.

    cargo test --release --features store-sqlite,fixtures --lib similar_precision_report \
        -- --ignored --nocapture | python3 evidence/issue-178-similar/report.py
"""
import json, os, sys
from collections import Counter, defaultdict

here = os.path.dirname(os.path.abspath(__file__))
rows = {r["id"]: r for r in json.load(open(os.path.join(here, "labelled-pairs.json")))}
evals = [json.loads(l.split("EVAL ", 1)[1]) for l in sys.stdin if "EVAL " in l]
pairs = [e for e in evals if e["mode"] == "pair"]

def guard_of(g):
    hit = [k for k, v in g.items() if v and k not in ("too_short",)]
    return ",".join(hit) or ("too_short" if g.get("too_short") else "no candidate (no shared LSH band, cosine below the floor)")

print("## Each labelled pair on its own (default parameters, cosine 0.85)\n")
for src in ("dogfood-i3", "synthetic"):
    tp = fp = fn = tn = 0
    fam = defaultdict(Counter)
    for e in pairs:
        r = rows[e["id"]]
        if r["source"] != src:
            continue
        dup = r["label"] == "duplicate"
        s = e["suggested"]
        tp += dup and s; fp += (not dup) and s; fn += dup and not s; tn += (not dup) and not s
        fam[(r["label"], r["family"])]["suggested" if s else "not suggested"] += 1
        if not s:
            fam[(r["label"], r["family"])]["guard: " + guard_of(e["guarded"])] += 1
    prec = tp / (tp + fp) if tp + fp else float("nan")
    rec = tp / (tp + fn) if tp + fn else float("nan")
    print(f"### {src}: {tp + fp + fn + tn} pairs\n")
    print(f"suggested {tp + fp}: {tp} duplicates, {fp} distinct. **Precision {prec:.3f}**, recall {rec:.3f}.\n")
    print("| label | family | outcome |")
    print("|---|---|---|")
    for (lab, f), c in sorted(fam.items()):
        print(f"| {lab} | {f} | " + ", ".join(f"{k} {v}" for k, v in sorted(c.items())) + " |")
    print()

pos = {frozenset((r["a"], r["b"])) for r in rows.values() if r["source"] == "synthetic" and r["label"] == "duplicate"}
neg = {frozenset((r["a"], r["b"])) for r in rows.values() if r["source"] == "synthetic" and r["label"] == "distinct"}
corpus = [e for e in evals if e["mode"] == "corpus"]
summary = next(e for e in evals if e["mode"] == "corpus-summary")
# Two variants of one base phrase are duplicates of each other too.
base = {}
for r in rows.values():
    if r["source"] == "synthetic" and r["label"] == "duplicate":
        base[r["a"]] = r["a"]
        base[r["b"]] = r["a"]
def verdict(e):
    if base.get(e["a"]) is not None and base.get(e["a"]) == base.get(e["b"]):
        return "duplicate (variants of one phrase)"
    if frozenset((e["a"], e["b"])) in neg:
        return "distinct (labelled)"
    return "distinct (unlabelled)"
c = Counter(verdict(e) for e in corpus)
dups = sum(v for k, v in c.items() if k.startswith("duplicate"))
print(f"## The synthetic texts as one store ({summary['concepts']} concepts, text source)\n")
print(f"{len(corpus)} suggestions from {summary['lexical_candidates']} LSH candidates: " + ", ".join(f"{k} {v}" for k, v in sorted(c.items())) + f". **Precision {dups / len(corpus):.3f}.**\n")
for e in corpus:
    if not verdict(e).startswith("duplicate"):
        print(f"- {verdict(e)}: {e['a']!r} / {e['b']!r} (jaccard {e['jaccard']:.3f})")
print(f"\nguarded: {json.dumps(summary['guarded'])}")
