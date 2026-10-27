#!/usr/bin/env python3
"""Print the #181 evidence tables from an eval_redundancy run.

Usage: python3 summarise.py runs/bge-m3-candle-cpu.json

MMR's lambda is chosen here, on `dev` only: the one with the best mean dev
facet coverage over every condition, k and budget (ties to the larger
lambda, the one nearer `current`). Held-out numbers are then read for that
lambda; nothing is chosen on them.
"""

import json
import sys
from collections import defaultdict

CONDITIONS = ["clean", "dup", "contrast", "contrast-declared"]
SPLITS = ["dev", "held_out"]
KS = [3, 5]
BUDGETS = [250, 500]
FAMILIES = ["polarity", "exception", "correction", "repository", "identifier", "quantity",
            "topic-neighbour"]


def main():
    run = json.load(open(sys.argv[1]))
    cond = run["conditions"]
    m = lambda c, s, k, b, arm: cond[c]["metrics"][f"{s}/k{k}/b{b}/{arm}"]

    lambdas = sorted({a.split("@")[1] for a in arm_names(cond) if a.startswith("mmr@")})
    def dev_score(arm):
        xs = [m(c, "dev", k, b, arm)["facet_coverage"]
              for c in CONDITIONS for k in KS for b in BUDGETS]
        return sum(xs) / len(xs)
    scored = [(dev_score(f"mmr@{l}"), float(l)) for l in lambdas]
    best = max(scored)[1]
    mmr = f"mmr@{best:.1f}"
    shipped = run["shipped"]
    arms = ["current", "content", shipped, mmr, "fl"]

    print("## Validation (offline rule (a) against the pipeline, top_k 20)\n")
    print("| condition | concepts | Semantic edges | Contradicts edges | queries | agree | with a skip |")
    print("|---|---:|---:|---:|---:|---:|---:|")
    for c in CONDITIONS:
        v = cond[c]["validation"]
        print(f"| {c} | {cond[c]['concepts']} | {cond[c]['semantic_edges']} | "
              f"{cond[c]['contradicts_edges']} | {v['compared']} | {v['agree']} | {v['with_skip']} |")

    print(f"\n## MMR lambda, chosen on dev: {best:.1f}\n")
    print("| lambda | mean dev facet coverage |")
    print("|---:|---:|")
    for s, l in sorted(scored, key=lambda x: x[1]):
        print(f"| {l:.1f} | {s:.4f} |")

    for b in BUDGETS:
        print(f"\n## Facet coverage in the rendered context, budget {b} tokens\n")
        print("Mean share of each answerable query's labelled facets covered by the rendered "
              "context; dev / held-out. In brackets: distinct memories rendered per query.\n")
        header = "| condition | k | " + " | ".join(arms) + " |"
        print(header)
        print("|---|---:|" + "---:|" * len(arms))
        for c in CONDITIONS:
            for k in KS:
                cells = []
                for arm in arms:
                    d, h = m(c, "dev", k, b, arm), m(c, "held_out", k, b, arm)
                    cells.append(f"{d['facet_coverage']:.3f} / {h['facet_coverage']:.3f} "
                                 f"[{d['distinct_memories_rendered']:.2f} / "
                                 f"{h['distinct_memories_rendered']:.2f}]")
                print(f"| {c} | {k} | " + " | ".join(cells) + " |")

    print("\n## Queries whose rendered context changed (budget 500), dev / held-out\n")
    print("| condition | k | " + " | ".join(arms[1:]) + " |")
    print("|---|---:|" + "---:|" * (len(arms) - 1))
    for c in CONDITIONS:
        for k in KS:
            cells = [f"{m(c, 'dev', k, 500, a)['queries_changed']} / "
                     f"{m(c, 'held_out', k, 500, a)['queries_changed']}" for a in arms[1:]]
            print(f"| {c} | {k} | " + " | ".join(cells) + " |")

    print("\n## Rule (a) threshold sweep (facet coverage, budget 500, dev / held-out)\n")
    sweep = sorted(a for a in arm_names(cond) if a.startswith("a@"))
    print("| condition | k | content | " + " | ".join(sweep) + " |")
    print("|---|---:|---:|" + "---:|" * len(sweep))
    for c in ["dup", "contrast"]:
        for k in KS:
            cells = [f"{m(c, 'dev', k, 500, a)['facet_coverage']:.3f} / "
                     f"{m(c, 'held_out', k, 500, a)['facet_coverage']:.3f}"
                     for a in ["content"] + sweep]
            print(f"| {c} | {k} | " + " | ".join(cells) + " |")

    print("\n## False suppression by contrast family (budget 500)\n")
    print("Windows where `current` renders both a corpus concept and its labelled "
          "related-but-distinct partner, and the arm renders only one of them: "
          "suppressed / both rendered by `current`, dev | held-out.\n")
    for c in ["contrast", "contrast-declared"]:
        print(f"\n### {c}\n")
        print("| family | k | " + " | ".join(arms[1:]) + " |")
        print("|---|---:|" + "---:|" * (len(arms) - 1))
        for fam in FAMILIES:
            for k in KS:
                cells = []
                for arm in arms[1:]:
                    parts = []
                    for s in SPLITS:
                        x = cond[c]["suppression"].get(f"{s}/k{k}/b500/{arm}/{fam}")
                        parts.append(f"{x['suppressed']}/{x['both_in_current']}" if x else "0/0")
                    cells.append(" \\| ".join(parts))
                print(f"| {fam} | {k} | " + " | ".join(cells) + " |")

    print(f"\n## What {shipped} skipped, by signal (both splits)\n")
    print("`copy`: an exact copy or labelled rewording of the kept concept's fact. "
          "`partner`: a labelled related-but-distinct partner (a false suppression).\n")
    print("| condition | k | joined | signal | skips |")
    print("|---|---:|---|---|---:|")
    for c in ["dup", "contrast", "contrast-declared"]:
        agg = defaultdict(int)
        for key, n in cond[c]["skips"].items():
            split, k, arm, what, signal = key.split("/")
            if arm == shipped:
                agg[(k, what, signal)] += n
        for (k, what, signal), n in sorted(agg.items()):
            print(f"| {c} | {k[1:]} | {what} | {signal} | {n} |")
        if not agg:
            print(f"| {c} | 3, 5 | none | none | 0 |")


def arm_names(cond):
    names = set()
    for c in cond.values():
        for key in c["metrics"]:
            parts = key.split("/")
            if len(parts) == 4:
                names.add(parts[3])
    return names


if __name__ == "__main__":
    main()
