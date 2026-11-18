#!/usr/bin/env python3
"""#254: would a layer floor that rises with the layer's size work?

Reads the committed `eval_measure --detail` runs over the #190 set
(`fixtures/eval/`) and, per embedding contract, measures a candidate
size-aware floor against the fixed floor. Standard library only; run from
anywhere in the repo:

    python3 evidence/issue-254/floor_rule.py

It writes `floor-rule.json` beside itself and prints the README's tables.

The candidate rule (arXiv:2610.34507, Proposition 1). A fixed floor `f0`
lets a share `p0` of single irrelevant (query, concept) pairs through. If a
layer of `N0` concepts was acceptable at `f0`, a layer of `N` concepts keeps
the same chance that some irrelevant concept clears the bar, for independent
pairs, when the per-pair share is

    p_N = 1 - (1 - p0) ** (N0 / N)

so the floor for size N is the `1 - p_N` quantile of the contract's null
distribution (the cosine of a query to a concept graded irrelevant). It is
`f0` for N <= N0. Everything is derived on the dev split; the held-out split
is only measured.

Two anchors `N0` are reported, because the recorded sizes differ:
`recorded` uses the size each floor's row records (12 for the BGE-M3
reference, 100 for the built-in rows), and `common12` uses 12 for every
contract (the rows' floors are quantile-matched to BGE-M3's 0.50, which was
chosen on a 12-concept layer).
"""

import json
import math
import pathlib
import random

HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parent.parent
FRAMING = "self"  # what derive.py derived every row on; "raw" is reported too
SIZES = [12, 32, 64, 100, 128, 256, 512, 1024, 2048]
DIRECT_SIZES = [12, 32, 64, 96]  # the labelled set has 100 concepts
DRAWS = 200
SEED = 254
# By-query bootstrap (resample whole queries with replacement): pairs that
# share a query are not independent, so the query is the unit. Its own seed,
# so the draws above are unchanged by it.
BOOTSTRAP_RESAMPLES = 1000
BOOTSTRAP_SEED = 2540

# name, detail run, fixed floor (src/embed/calibration.rs), recorded size.
CONTRACTS = [
    ("bge-m3 (reference)", "evidence/issue-221/runs/bge-m3-candle-cpu-detail.json", 0.50, 12),
    ("qwen3-embedding-4b 2560", "evidence/issue-221/runs/qwen3-4b-2560-detail.json", 0.59, 100),
    ("qwen3-embedding-4b 1024", "evidence/issue-221/runs/qwen3-4b-1024-detail.json", 0.60, 100),
    ("elastic jina-v3 1024", "evidence/issue-237/runs/elastic-jina-v3-1024-detail.json", 0.39, 100),
]


def quantile(sorted_values, p):
    """Nearest rank, as `evidence/issue-87-calibration/derive.py` computes it."""
    n = len(sorted_values)
    return sorted_values[min(n - 1, max(0, math.ceil(p * n) - 1))]


def per_pair_share(p0, n0, n):
    """Proposition 1: the per-pair share that keeps best-of-n at best-of-n0's rate."""
    return 1.0 - (1.0 - p0) ** (n0 / n)


def rule_floor(null_sorted, f0, n0, n):
    """The candidate floor for a layer of n concepts, and its per-pair share."""
    p0 = sum(x >= f0 for x in null_sorted) / len(null_sorted)
    if n <= n0:
        return f0, p0
    p_n = per_pair_share(p0, n0, n)
    return max(f0, quantile(null_sorted, 1.0 - p_n)), p_n


def share_at_or_above(values, floor):
    return sum(x >= floor for x in values) / len(values) if values else None


class Run:
    def __init__(self, path, framing):
        d = json.loads((ROOT / path).read_text())
        self.kind = {q["id"]: q["kind"] for q in d["queries"] if q["framing"] == framing}
        self.rows = [r for r in d["query_concepts"] if r["framing"] == framing]

    def cosines(self, split, grade):
        return [r["cosine"] for r in self.rows if r["split"] == split and r["grade"] == grade]

    def cosines_by_query(self, split, grade):
        by = {}
        for r in self.rows:
            if r["split"] == split and r["grade"] == grade:
                by.setdefault(r["query"], []).append(r["cosine"])
        return by

    def best_relevant(self, split):
        """Each answerable query's best cosine to a concept graded 1 or 2."""
        best = {}
        for r in self.rows:
            if r["split"] == split and r["grade"] >= 1:
                best[r["query"]] = max(best.get(r["query"], 0.0), r["cosine"])
        return list(best.values())

    def irrelevant_by_query(self, split, kind=None):
        by = {}
        for r in self.rows:
            if r["split"] == split and r["grade"] == 0 and kind in (None, self.kind[r["query"]]):
                by.setdefault(r["query"], []).append(r["cosine"])
        return by


def direct_admission(by_query, n, floors, rng):
    """Share of (query, random n-concept layer of irrelevant concepts) whose
    best cosine clears each floor. Measured, not modelled: no independence
    assumption."""
    hits = [0] * len(floors)
    total = 0
    for cosines in by_query.values():
        if len(cosines) < n:
            continue
        for _ in range(DRAWS):
            best = max(rng.sample(cosines, n))
            total += 1
            for i, f in enumerate(floors):
                hits[i] += best >= f
    return [h / total for h in hits], total // DRAWS


def interval95(values):
    """The 2.5th and 97.5th percentiles (nearest rank) of bootstrap values."""
    v = sorted(values)
    return [quantile(v, 0.025), quantile(v, 0.975)]


def bootstrap(run, f0, n0, floors, rng):
    """By-query bootstrap, 95% percentile intervals, for anchor `n0`.

    Two questions, answered separately. `floor`: how far the rule's floor at
    each size moves when the dev queries behind the null are resampled (the
    fixed floor is not resampled, so it has no interval). `grade2_lost`: how
    far the share of grade-2 pairs below the point-estimate floor moves when
    that split's queries are resampled, with the floor held where the full
    dev null puts it. Neither covers the other's uncertainty, and neither
    covers the labels, the embedder run or the set's size.
    """
    null = list(run.cosines_by_query("dev", 0).values())
    floor_draws = {n: [] for n in SIZES}
    for _ in range(BOOTSTRAP_RESAMPLES):
        resampled = sorted(x for cosines in rng.choices(null, k=len(null)) for x in cosines)
        for n in SIZES:
            floor_draws[n].append(rule_floor(resampled, f0, n0, n)[0])
    lost_draws = {split: {n: [] for n in SIZES} for split in ("dev", "held_out")}
    fixed_draws = {split: [] for split in ("dev", "held_out")}
    for split in ("dev", "held_out"):
        g2 = list(run.cosines_by_query(split, 2).values())
        for _ in range(BOOTSTRAP_RESAMPLES):
            resampled = [x for cosines in rng.choices(g2, k=len(g2)) for x in cosines]
            fixed_draws[split].append(sum(x < f0 for x in resampled) / len(resampled))
            for n in SIZES:
                lost_draws[split][n].append(
                    sum(x < floors[n] for x in resampled) / len(resampled)
                )
    return {
        "unit": "query",
        "anchor_n0": n0,
        "resamples": BOOTSTRAP_RESAMPLES,
        "seed": BOOTSTRAP_SEED,
        "interval": "95% percentile",
        "dev_null_queries": len(null),
        "grade2_queries": {
            split: len(run.cosines_by_query(split, 2)) for split in ("dev", "held_out")
        },
        "grade2_lost_share_at_fixed": {
            split: [round(x, 4) for x in interval95(fixed_draws[split])]
            for split in ("dev", "held_out")
        },
        "sizes": [
            {
                "layer_concepts": n,
                "floor_point": round(floors[n], 4),
                "floor": [round(x, 4) for x in interval95(floor_draws[n])],
                "grade2_lost_share": {
                    split: [round(x, 4) for x in interval95(lost_draws[split][n])]
                    for split in ("dev", "held_out")
                },
            }
            for n in SIZES
        ],
    }


def measure(name, path, f0, recorded, framing):
    run = Run(path, framing)
    null_dev = sorted(run.cosines("dev", 0))
    out = {
        "contract": name,
        "run": path,
        "framing": framing,
        "fixed_floor": f0,
        "recorded_measured_concepts": recorded,
        "dev_null_pairs": len(null_dev),
        "dev_null_share_at_or_above_fixed_floor": round(share_at_or_above(null_dev, f0), 5),
        "dev_null_max": null_dev[-1],
        "anchors": {},
        "direct_best_of_n": [],
    }
    anchors = {"recorded": recorded, "common12": 12}
    for label, n0 in anchors.items():
        rows = []
        for n in SIZES:
            floor, p_n = rule_floor(null_dev, f0, n0, n)
            row = {
                "layer_concepts": n,
                "per_pair_share_target": round(p_n, 6),
                "floor": round(floor, 4),
                # How far into the tail the data resolves: the floor at this
                # size rests on this many dev null pairs.
                "dev_null_pairs_at_or_above": sum(x >= floor for x in null_dev),
            }
            for split in ("dev", "held_out"):
                g2, g1 = run.cosines(split, 2), run.cosines(split, 1)
                best = run.best_relevant(split)
                row[split] = {
                    "grade2_pairs": len(g2),
                    "grade2_lost": sum(x < floor for x in g2),
                    "grade2_lost_at_fixed": sum(x < f0 for x in g2),
                    "grade1_pairs": len(g1),
                    "grade1_lost": sum(x < floor for x in g1),
                    "grade1_lost_at_fixed": sum(x < f0 for x in g1),
                    "answerable_queries": len(best),
                    "queries_with_no_relevant_hit_admitted": sum(x < floor for x in best),
                    "queries_with_no_relevant_hit_admitted_at_fixed": sum(x < f0 for x in best),
                    "null_share_at_or_above": round(
                        share_at_or_above(run.cosines(split, 0), floor), 6
                    ),
                }
            rows.append(row)
        out["anchors"][label] = {"n0": n0, "sizes": rows}

    # Directly measured admission for layers the labelled set can build: each
    # query against random layers of concepts graded irrelevant to it.
    rng = random.Random(SEED)
    for split in ("dev", "held_out"):
        for population, kind in (("no_answer", "no_answer"), ("all", None)):
            by_query = run.irrelevant_by_query(split, kind)
            for n in DIRECT_SIZES:
                floors = [f0] + [rule_floor(null_dev, f0, n0, n)[0] for n0 in anchors.values()]
                shares, queries = direct_admission(by_query, n, floors, rng)
                out["direct_best_of_n"].append(
                    {
                        "split": split,
                        "queries": population,
                        "queries_n": queries,
                        "layer_concepts": n,
                        "admit_fixed": round(shares[0], 4),
                        "admit_rule_recorded": round(shares[1], 4),
                        "admit_rule_common12": round(shares[2], 4),
                    }
                )
    # Sampling uncertainty, for the anchor every floor shares (12; module
    # docstring). Its own generator, after everything else.
    floors12 = {n: rule_floor(null_dev, f0, 12, n)[0] for n in SIZES}
    out["bootstrap_common12"] = bootstrap(
        run, f0, 12, floors12, random.Random(BOOTSTRAP_SEED)
    )
    return out


def pct(a, b):
    return f"{100 * a / b:.0f}%" if b else "n/a"


def span(pair):
    return f"{100 * pair[0]:.0f} to {100 * pair[1]:.0f}%"


def print_tables(result):
    for c in result["contracts"]:
        print(f"\n### {c['contract']} (fixed floor {c['fixed_floor']}, {c['framing']} framing)")
        print(
            f"\ndev null: {c['dev_null_pairs']} pairs, "
            f"{100 * c['dev_null_share_at_or_above_fixed_floor']:.2f}% at or above the fixed "
            f"floor, max {c['dev_null_max']}"
        )
        for label, anchor in c["anchors"].items():
            if label == "common12" and anchor["n0"] == c["recorded_measured_concepts"]:
                print("\nanchor `common12` is the same as `recorded` for this contract")
                continue
            print(f"\nanchor `{label}` (N0 = {anchor['n0']})\n")
            print(
                "| Layer size | Floor | Dev null pairs at or above | Grade-2 lost, dev | "
                "Grade-2 lost, held-out | Queries with no relevant hit, held-out |"
            )
            print("|---:|---:|---:|---:|---:|---:|")
            for r in anchor["sizes"]:
                d, h = r["dev"], r["held_out"]
                print(
                    f"| {r['layer_concepts']:,} | {r['floor']:.4f} | "
                    f"{r['dev_null_pairs_at_or_above']} | "
                    f"{d['grade2_lost']}/{d['grade2_pairs']} ({pct(d['grade2_lost'], d['grade2_pairs'])}) | "
                    f"{h['grade2_lost']}/{h['grade2_pairs']} ({pct(h['grade2_lost'], h['grade2_pairs'])}) | "
                    f"{h['queries_with_no_relevant_hit_admitted']}/{h['answerable_queries']} "
                    f"({pct(h['queries_with_no_relevant_hit_admitted'], h['answerable_queries'])}) |"
                )
        b = c["bootstrap_common12"]
        fixed = b["grade2_lost_share_at_fixed"]
        print(
            f"\nby-query bootstrap, N0 = {b['anchor_n0']}, {b['resamples']} resamples, "
            f"95% percentile intervals\n"
        )
        print(
            f"grade-2 lost at the fixed floor: dev {span(fixed['dev'])}, "
            f"held-out {span(fixed['held_out'])}\n"
        )
        print("| Layer size | Floor | Floor, 95% | Grade-2 lost, dev, 95% | Grade-2 lost, held-out, 95% |")
        print("|---:|---:|---:|---:|---:|")
        for r in b["sizes"]:
            lost = r["grade2_lost_share"]
            print(
                f"| {r['layer_concepts']:,} | {r['floor_point']:.4f} | "
                f"{r['floor'][0]:.3f} to {r['floor'][1]:.3f} | {span(lost['dev'])} | "
                f"{span(lost['held_out'])} |"
            )
        print("\ndirect best-of-N, held-out queries, layers of irrelevant concepts\n")
        print("| Queries | Layer size | Fixed | Rule, recorded | Rule, common12 |")
        print("|---|---:|---:|---:|---:|")
        for r in c["direct_best_of_n"]:
            if r["split"] != "held_out":
                continue
            print(
                f"| {r['queries']} ({r['queries_n']}) | {r['layer_concepts']} | "
                f"{100 * r['admit_fixed']:.1f}% | {100 * r['admit_rule_recorded']:.1f}% | "
                f"{100 * r['admit_rule_common12']:.1f}% |"
            )


def main():
    result = {
        "issue": 254,
        "set": "fixtures/eval (the #190 set: 100 concepts, 368 queries)",
        "derived_on": "dev",
        "sizes": SIZES,
        "direct_sizes": DIRECT_SIZES,
        "direct_draws_per_query": DRAWS,
        "seed": SEED,
        "bootstrap_resamples": BOOTSTRAP_RESAMPLES,
        "bootstrap_seed": BOOTSTRAP_SEED,
        "contracts": [measure(n, p, f, r, FRAMING) for n, p, f, r in CONTRACTS],
        "sensitivity_raw_framing": [measure(n, p, f, r, "raw") for n, p, f, r in CONTRACTS[:1]],
    }
    (HERE / "floor-rule.json").write_text(json.dumps(result, indent=1) + "\n")
    print_tables(result)


if __name__ == "__main__":
    main()
