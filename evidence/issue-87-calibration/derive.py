#!/usr/bin/env python3
"""Derive per-contract calibration constants for #87 by quantile matching.

Inputs are two `eval_measure --detail` files over the same set: the
reference contract (BGE-M3) and the target contract. Every constant is
derived on the dev split only; the held-out split is measured, never tuned
on. Standard library only.

    python3 derive.py --reference bge-detail.json --target eg2-detail.json \
        --kind embeddinggemma2 --model '...' --dim 768 --out calibration.json

For a reference constant c and a null distribution F (dev split):
    q   = F_ref(c)            share of the reference null at or below c
    c_X = quantile_X(q)       nearest rank, as the canary computes quantiles
Nulls: query-to-irrelevant-concept cosine (grade 0) for the recent score and
the layer floor; related-but-distinct pair cosine for the merge threshold
(matched on the share merged, i.e. at or above the threshold) over every
labelled pair. #193's identifier guard refuses a pair whose texts name
different identifiers before any threshold applies, so the rates a writer
would actually see are reported separately, on the guard-eligible pairs.
"""

import argparse
import json
import math
import pathlib

HERE = pathlib.Path(__file__).resolve().parent
PAIRS = HERE.parent.parent / "fixtures" / "eval" / "pairs.json"

# BGE-M3's constants (src/recall/candidates.rs, src/recall/layered.rs,
# src/graph/hybrid.rs).
RECENT_SCORE = 0.35
LAYER_MIN_SIMILARITY = 0.50
MERGE_THRESHOLD = 0.85
SHIP_DECIMALS = 2


def quantile(values, p):
    v = sorted(values)
    return v[min(len(v) - 1, max(0, math.ceil(p * len(v)) - 1))]


def share_at_or_below(values, c):
    return sum(x <= c for x in values) / len(values)


def share_above(values, c):
    return sum(x > c for x in values) / len(values)


def share_at_or_above(values, c):
    return sum(x >= c for x in values) / len(values)


class Detail:
    def __init__(self, path, pair_split, framing):
        d = json.load(open(path))
        self.framing = framing
        self.rows = [r for r in d["query_concepts"] if r["framing"] == framing]
        self.pairs = [
            (r, pair_split[r["id"]])
            for r in d["pairs"]
            if r["framing"] == framing and not r.get("review")
        ]
        if any("same_identifiers" not in r for r, _ in self.pairs):
            raise SystemExit(f"{path}: pairs lack same_identifiers; rerun eval_measure")

    def top_irrelevant(self, split, k=5):
        """Cosines of the grade-0 concepts among each query's top k: the
        irrelevant hits a recall's vector leg actually returns."""
        by_query = {}
        for r in self.rows:
            if r["split"] == split:
                by_query.setdefault(r["query"], []).append(r)
        out = []
        for rows in by_query.values():
            rows.sort(key=lambda r: -r["cosine"])
            out.extend(r["cosine"] for r in rows[:k] if r["grade"] == 0)
        return out

    def query(self, split, grade):
        return [r["cosine"] for r in self.rows if r["split"] == split and r["grade"] == grade]

    def pair(self, split, label, mergeable_only=True):
        """Pair cosines. By default only the pairs a writer could merge:
        #193's identifier guard refuses the rest before any threshold."""
        return [
            r["cosine"]
            for r, s in self.pairs
            if s == split and r["label"] == label and (r["same_identifiers"] or not mergeable_only)
        ]


def r4(x):
    return round(x, 4)


def recall_constant(ref, tgt, c):
    q = share_at_or_below(ref.query("dev", 0), c)
    derived = quantile(tgt.query("dev", 0), q)
    shipped = round(derived, SHIP_DECIMALS)
    out = {"reference": c, "reference_null_quantile": r4(q), "derived": r4(derived), "shipped": shipped}
    for split in ("dev", "held_out"):
        rows = {}
        for name, d, x in (("reference", ref, c), ("target_before", tgt, c), ("target_after", tgt, shipped)):
            rows[name] = {
                "at": x,
                "irrelevant_above": r4(share_above(d.query(split, 0), x)),
                "top5_irrelevant_above": r4(share_above(d.top_irrelevant(split), x)),
                "true_hits_above": r4(share_above(d.query(split, 2), x)),
                "irrelevant_n": len(d.query(split, 0)),
                "true_hits_n": len(d.query(split, 2)),
                "top5_irrelevant_n": len(d.top_irrelevant(split)),
            }
        out[split] = rows
    return out


def merge_constant(ref, tgt, c):
    # Derived on every labelled pair: the scale map is a property of the
    # embedder, and the identifier guard (#193) removes the same pairs under
    # both contracts, leaving too few on the dev split (6 related-distinct)
    # to match on. What a writer would actually merge is reported beside it
    # ("writer": the guard-eligible pairs only).
    m = share_at_or_above(ref.pair("dev", "related_distinct", mergeable_only=False), c)
    derived = quantile(tgt.pair("dev", "related_distinct", mergeable_only=False), 1 - m)
    shipped = round(derived, SHIP_DECIMALS)
    out = {"reference": c, "reference_related_distinct_merged": r4(m), "derived": r4(derived), "shipped": shipped}
    for view, only in (("cosine_only", False), ("writer", True)):
        out[view] = {}
        for split in ("dev", "held_out"):
            rows = {}
            for name, d, x in (("reference", ref, c), ("target_before", tgt, c), ("target_after", tgt, shipped)):
                rbd = d.pair(split, "related_distinct", mergeable_only=only)
                para = d.pair(split, "paraphrase", mergeable_only=only)
                rows[name] = {
                    "at": x,
                    "related_distinct_merged": r4(share_at_or_above(rbd, x)),
                    "paraphrase_merged": r4(share_at_or_above(para, x)),
                    "related_distinct_n": len(rbd),
                    "paraphrase_n": len(para),
                }
            out[view][split] = rows
    # The related_pairs rule (#176) embeds every labelled pair, guard ignored.
    p99 = quantile(tgt.pair("dev", "related_distinct", mergeable_only=False), 0.99)
    out["p99_rule"] = {
        "value": r4(p99),
        "held_out_related_distinct_merged": r4(
            share_at_or_above(tgt.pair("held_out", "related_distinct", mergeable_only=False), p99)
        ),
        "held_out_paraphrase_merged": r4(
            share_at_or_above(tgt.pair("held_out", "paraphrase", mergeable_only=False), p99)
        ),
    }
    return out


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--reference", required=True)
    ap.add_argument("--target", required=True)
    ap.add_argument("--kind", required=True)
    ap.add_argument("--model", required=True)
    ap.add_argument("--dim", type=int, required=True)
    ap.add_argument("--framing", default="self")
    ap.add_argument("--out", required=True)
    a = ap.parse_args()
    split = {p["id"]: p["split"] for p in json.load(open(PAIRS))}
    result = {"framing": a.framing, "contracts": []}
    for framing in sorted({a.framing, "raw", "self"}):
        ref, tgt = Detail(a.reference, split, framing), Detail(a.target, split, framing)
        row = {
            "kind": a.kind,
            "model": a.model,
            "dim": a.dim,
            "framing": framing,
            "recent_score": recall_constant(ref, tgt, RECENT_SCORE),
            "layer_min_similarity": recall_constant(ref, tgt, LAYER_MIN_SIMILARITY),
            "merge_threshold": merge_constant(ref, tgt, MERGE_THRESHOLD),
            "default_threshold_merges": r4(
                share_at_or_above(
                    tgt.pair("dev", "related_distinct", mergeable_only=False), MERGE_THRESHOLD
                )
            ),
        }
        if framing == a.framing:
            result["contracts"].append(row)
        else:
            result.setdefault("sensitivity", []).append(row)
    pathlib.Path(a.out).write_text(json.dumps(result, indent=2) + "\n")
    print(json.dumps(result["contracts"][0], indent=2))


if __name__ == "__main__":
    main()
