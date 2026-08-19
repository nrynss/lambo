#!/usr/bin/env python3
"""DOGFOOD metric 4 — real recall scores against G1's measured bands.

The question: **do the constants still mean what G1 measured them to mean?**
G1 measured BGE-M3's score bands on a synthetic corpus
(`evidence/mooshik-g-recall-calibration/measurement.txt`) and G2 lowered
`RECENT_SCORE` from 0.5 to 0.35 on the strength of it. That was 24 pairs. This
script asks the same question of every recall a real session actually served.

What it reads, and why the ledger had to grow a field for it: recall's phase-1
merge folds the keyword / recency / vector legs by `max`, so a merged score is
**lossy** — a 0.35 is either the recency floor or a genuine weak cosine, and the
two mean opposite things. I1 carries the per-leg numbers (`legs.bm25`,
`legs.recent`, `legs.vector_cosine`) so this script can tell them apart. A hit
with an EMPTY `legs` object was not a phase-1 candidate at all: it came in
through phase-2 traversal expansion and has no leg score by construction, so it
is counted separately rather than folded in.

Everything banded here is `legs.vector_cosine` — the RAW cosine the store
returned — never the hit's `score`, which is the final ranking value after
phase-3 assembly applies the daemon score table and `RecallWeights` and is on a
different scale entirely. Banding `score` against G1's cosines would be
comparing two different quantities.

Three things are flagged:

  1. **Floor masking** — a hit whose vector cosine is present but BELOW the
     recency floor, so `max` discarded the semantic signal. This is the exact
     failure G2 lowered the floor to fix; a recurrence means the floor needs
     lowering again, or per-embedder calibration (G2 option 3).
  2. **Band drift** — the observed vector-cosine distribution against G1's
     measured `true_recall` band (0.4599–0.6577). A live distribution centred
     well outside it means G1's corpus was not representative and the constants
     rest on the wrong numbers.
  3. **Unranked recalls** — recalls that returned hits but no vector leg at all,
     which means the whole session was keyword+recency only. Usually a store
     without VECTOR_SEARCH or a failed query embedding; either way the score
     bands say nothing about that stretch.

Usage:
    python3 scripts/observability/score_bands.py ~/lambo-dogfood/calls.jsonl
    python3 scripts/observability/score_bands.py --floor 0.5 --json calls.jsonl
"""

from __future__ import annotations

import statistics
import sys
from typing import Any

import _ledger

#: G1's measured bands, verbatim from
#: `evidence/mooshik-g-recall-calibration/measurement.txt`. Cosines of
#: `Concept: <text>` pairs / bare queries under bge-m3-q8_0.
G1_BANDS = {
    "true_recall": {"n": 6, "min": 0.4599, "max": 0.6577, "mean": 0.5826},
    "paraphrase_merge": {"n": 10, "min": 0.6889, "max": 0.8984, "mean": 0.7992},
    "related_distinct": {"n": 10, "min": 0.6348, "max": 0.8913, "mean": 0.7683},
    "unrelated": {"n": 8, "min": 0.4999, "max": 0.6249, "mean": 0.5759},
}


def analyse(ledger: _ledger.Ledger, floor: float) -> dict[str, Any]:
    vector: list[float] = []
    bm25: list[float] = []
    masked: list[dict[str, Any]] = []
    expansion_hits = 0
    recalls = 0
    recalls_with_vector = 0
    recalls_without_hits = 0

    for call in ledger.sorted_calls():
        if call.get("tool") != _ledger.RECALL_TOOL or not _ledger.succeeded(call):
            continue
        recalls += 1
        hits = call.get("hits") or []
        if not hits:
            recalls_without_hits += 1
            continue
        saw_vector = False
        for hit in hits:
            legs = hit.get("legs") or {}
            if not legs:
                expansion_hits += 1
                continue
            cosine = legs.get("vector_cosine")
            if isinstance(cosine, (int, float)):
                vector.append(float(cosine))
                saw_vector = True
                recent = legs.get("recent")
                if isinstance(recent, (int, float)) and cosine < recent:
                    # `max` kept the flat floor and threw away a real semantic
                    # signal. Whether the hit was TRUE is a human call — the
                    # content is here so it can be made.
                    masked.append(
                        {
                            "ts": call["ts"],
                            "agent_id": call.get("agent_id"),
                            "query": call.get("query"),
                            "content": hit.get("content"),
                            "vector_cosine": cosine,
                            "recent": recent,
                            "final_score": hit.get("score"),
                            "included_in_context": hit.get("included_in_context"),
                        }
                    )
            score = legs.get("bm25")
            if isinstance(score, (int, float)):
                bm25.append(float(score))
        if saw_vector:
            recalls_with_vector += 1

    def summarise(values: list[float]) -> dict[str, Any] | None:
        if not values:
            return None
        return {
            "n": len(values),
            "min": round(min(values), 4),
            "max": round(max(values), 4),
            "mean": round(statistics.fmean(values), 4),
            "median": round(statistics.median(values), 4),
            "p10": round(quantile(values, 0.10), 4),
            "p90": round(quantile(values, 0.90), 4),
        }

    observed = summarise(vector)
    band = G1_BANDS["true_recall"]
    drift = None
    if observed:
        drift = {
            "below_g1_true_min": sum(1 for v in vector if v < band["min"]),
            "above_g1_true_max": sum(1 for v in vector if v > band["max"]),
            "inside_g1_true_band": sum(1 for v in vector if band["min"] <= v <= band["max"]),
            "mean_delta_vs_g1": round(observed["mean"] - band["mean"], 4),
        }

    return {
        "floor": floor,
        "g1_bands": G1_BANDS,
        "recalls": recalls,
        "recalls_with_a_vector_leg": recalls_with_vector,
        "recalls_that_returned_no_hits": recalls_without_hits,
        "expansion_only_hits": expansion_hits,
        "vector_cosine": observed,
        "bm25": summarise(bm25),
        "band_drift": drift,
        "floor_masked": masked,
        "histogram": histogram(vector),
    }


def quantile(values: list[float], q: float) -> float:
    ordered = sorted(values)
    if len(ordered) == 1:
        return ordered[0]
    pos = q * (len(ordered) - 1)
    low = int(pos)
    high = min(low + 1, len(ordered) - 1)
    return ordered[low] + (ordered[high] - ordered[low]) * (pos - low)


def histogram(values: list[float], width: float = 0.05) -> list[dict[str, Any]]:
    """Fixed-width buckets from 0 to 1, so two runs are directly comparable."""
    if not values:
        return []
    counts: dict[int, int] = {}
    for v in values:
        idx = min(int(v / width), int(1 / width) - 1)
        counts[idx] = counts.get(idx, 0) + 1
    return [
        {"low": round(i * width, 2), "high": round((i + 1) * width, 2), "n": counts[i]}
        for i in sorted(counts)
    ]


def render(data: dict[str, Any]) -> list[str]:
    band = data["g1_bands"]["true_recall"]
    out = [
        f"recalls: {data['recalls']} "
        f"({data['recalls_with_a_vector_leg']} with a vector leg, "
        f"{data['recalls_that_returned_no_hits']} returned no hits)",
        f"recency floor in force: {data['floor']:g}   "
        f"merge threshold: {_ledger.MERGE_THRESHOLD:g}",
    ]
    if data["expansion_only_hits"]:
        out.append(
            f"{data['expansion_only_hits']} hit(s) had no phase-1 legs at all — they "
            "entered through traversal expansion and carry no score to band"
        )

    observed = data["vector_cosine"]
    if not observed:
        out += [
            "",
            "NO VECTOR-LEG SCORES IN THIS LEDGER. Either the store does not "
            "advertise VECTOR_SEARCH, or every query embedding failed. The score "
            "bands say nothing about this stretch — that is the finding.",
        ]
        return out

    out += [
        "",
        "Vector cosine, observed vs G1's measured bands:",
        f"   observed   n={observed['n']:<5} min={observed['min']:.4f} "
        f"p10={observed['p10']:.4f} median={observed['median']:.4f} "
        f"p90={observed['p90']:.4f} max={observed['max']:.4f} mean={observed['mean']:.4f}",
    ]
    for name, b in data["g1_bands"].items():
        out.append(
            f"   G1 {name:<17} n={b['n']:<5} min={b['min']:.4f} "
            f"max={b['max']:.4f} mean={b['mean']:.4f}"
        )
    d = data["band_drift"]
    out += [
        "",
        f"Against G1's true_recall band [{band['min']:.4f}, {band['max']:.4f}]: "
        f"{d['inside_g1_true_band']} inside, {d['below_g1_true_min']} below, "
        f"{d['above_g1_true_max']} above; mean delta {d['mean_delta_vs_g1']:+.4f}",
    ]
    if abs(d["mean_delta_vs_g1"]) > 0.10:
        out.append(
            "   ^ the live mean is more than 0.10 off G1's. G1's 24-pair corpus is "
            "not representative of this traffic, and both constants rest on it — "
            "re-measure before trusting the floor or the merge threshold."
        )

    if observed["min"] < data["floor"]:
        out.append(
            f"   ^ the weakest observed cosine ({observed['min']:.4f}) is BELOW the "
            f"recency floor ({data['floor']:g}): scores in that band are maskable."
        )

    out += ["", "Distribution (0.05 buckets):"]
    peak = max(b["n"] for b in data["histogram"])
    for b in data["histogram"]:
        bar = "#" * max(1, round(40 * b["n"] / peak))
        marker = ""
        if b["low"] <= data["floor"] < b["high"]:
            marker = "  <- recency floor"
        if b["low"] <= _ledger.MERGE_THRESHOLD < b["high"]:
            marker += "  <- merge threshold"
        out.append(f"   {b['low']:.2f}-{b['high']:.2f} {b['n']:>5} {bar}{marker}")

    masked = data["floor_masked"]
    out += ["", f"Floor masking (metric 4's recurrence check): {len(masked)} occurrence(s)"]
    if not masked:
        out.append(
            "   none — no hit had a real cosine discarded by the flat recency score. "
            "G2's lowered floor is holding on this traffic."
        )
    else:
        out.append(
            "   Each row is a semantic signal the `max` merge threw away. Judge "
            "whether the hit was TRUE; a recurring true hit here means the floor "
            "needs lowering again (G2 option 1) or per-embedder calibration (option 3)."
        )
        for m in masked[:20]:
            out.append(
                f"   {m['ts']}  cosine={m['vector_cosine']:.4f} < floor={m['recent']:.4f} "
                f"-> final={m['final_score']:.4f}"
            )
            out.append(f"       query:   {m['query']!r}")
            out.append(f"       concept: {m['content']!r}")
        if len(masked) > 20:
            out.append(f"   … and {len(masked) - 20} more")

    bm25 = data["bm25"]
    if bm25:
        out += [
            "",
            f"BM25 leg for reference: n={bm25['n']} min={bm25['min']:.4f} "
            f"median={bm25['median']:.4f} max={bm25['max']:.4f} "
            "(a different scale from cosine — never banded against it)",
        ]
    return out


def main(argv: list[str]) -> int:
    parser = _ledger.base_parser(__doc__ or "")
    parser.add_argument(
        "--floor",
        type=float,
        default=_ledger.RECENT_FLOOR,
        help="RECENT_SCORE the serve was built with (default: %(default)s)",
    )
    args = parser.parse_args(argv)
    ledger = _ledger.load(args.ledger)
    data = analyse(ledger, args.floor)
    _ledger.emit(
        "metric 4 — recall score bands", ledger, render(data), data, args.json
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
