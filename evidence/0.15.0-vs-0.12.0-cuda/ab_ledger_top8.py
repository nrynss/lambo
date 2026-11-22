#!/usr/bin/env python3
"""Per-query aggregate of the sequential A/B, from the two serves' own ledgers.

The A/B harness keeps only the aggregate `recall_topk_jaccard`. Each A/B serve ran
with --ledger, and a recall ledger line lists the hits it returned. This
pairs the two sides' `top_k = 8` recalls in call order (10 rounds of 20
queries; the warm-up recalls use the default top_k and are skipped) and
prints, per round and query index: hits per side, whether the first eight are
the same ids in the same order, the largest score difference among them, what
each side returned beyond the eighth (id prefix and score), and each side's
`hot_warning` flag and `warning_count`. No query text and no concept text.

Usage:
    python3 ab_ledger_top8.py A=serve-ledger.jsonl B=serve-ledger.jsonl > ab-ledger-top8.json
"""
import json
import sys


def recalls(path):
    out = []
    with open(path) as f:
        for line in f:
            try:
                o = json.loads(line)
            except ValueError:
                continue
            if o.get("tool") == "lambo_recall" and o.get("top_k") == 8:
                out.append(o)
    return out


def main():
    (a, pa), (b, pb) = (spec.split("=", 1) for spec in sys.argv[1:3])
    ra, rb = recalls(pa), recalls(pb)
    n = min(len(ra), len(rb))
    queries = 20
    rows = []
    first_round_overlap = []
    all_equal = True
    max_delta = 0.0
    for i in range(n):
        ha, hb = ra[i]["hits"], rb[i]["hits"]
        ia, ib = [h["node_id"] for h in ha[:8]], [h["node_id"] for h in hb[:8]]
        equal = ia == ib
        delta = max((abs(x["score"] - y["score"]) for x, y in zip(ha[:8], hb[:8])), default=0.0) if equal else None
        all_equal = all_equal and equal
        if delta:
            max_delta = max(max_delta, delta)
        sa, sb = {h["node_id"] for h in ha}, {h["node_id"] for h in hb}
        if i < queries:
            first_round_overlap.append(len(sa & sb) / len(sa | sb) if (sa or sb) else 1.0)
        beyond = lambda hs: [{"id_prefix": h["node_id"][:8], "score": round(h["score"], 4)} for h in hs[8:]]
        rows.append({
            "round": i // queries, "query_index": i % queries,
            "hits": {a: len(ha), b: len(hb)},
            "top8_ids_equal_in_order": equal,
            "top8_max_score_delta": None if delta is None else round(delta, 6),
            "beyond_top8": {a: beyond(ha), b: beyond(hb)},
            "hot_warning": {a: bool(ra[i].get("hot_warning")), b: bool(rb[i].get("hot_warning"))},
            "warning_count": {a: ra[i].get("warning_count"), b: rb[i].get("warning_count")},
        })
    json.dump({
        "sides": [a, b], "top_k_8_recalls": {a: len(ra), b: len(rb)}, "paired": n,
        "top8_ids_equal_in_order_everywhere": all_equal,
        "top8_max_score_delta_everywhere": round(max_delta, 6),
        "first_round_set_overlap_mean": round(sum(first_round_overlap) / len(first_round_overlap), 4),
        "first_round_queries_with_overlap_below_1": [i for i, v in enumerate(first_round_overlap) if v < 1.0],
        "per_call": rows,
    }, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
