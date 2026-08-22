#!/usr/bin/env python3
"""Pairwise cosine between two independently-produced vector sets.

Cosine is computed by its definition, `dot(a,b) / (|a| |b|)`, applied
symmetrically to both sides. That is not a fix-up: it is the metric, and it is
the same arithmetic `evidence/mooshik-f-sqlite-bge/cosine_probe.py` uses. No
vector is re-normalized, truncated, or re-pooled on either side before the
comparison — doing so to both sides is precisely what would void the parity
gate.

Usage:
    python3 evidence/mooshik-k1-metal/compare.py \
        --a evidence/mooshik-k1-metal/vectors-candle-cpu-f32.jsonl \
        --b evidence/mooshik-k1-metal/vectors-llama-q8.jsonl \
        --corpus evidence/mooshik-k1-metal/corpus.jsonl \
        --label candle-cpu-f32__vs__llama-q8 \
        --out-json evidence/mooshik-k1-metal/parity-candle-cpu-f32.json \
        --out-csv  evidence/mooshik-k1-metal/parity-candle-cpu-f32.csv
"""

import argparse
import csv
import gzip
import json
import math
import statistics


def _open(path):
    """The committed vector captures are gzipped (they are ~1.8 MB each raw);
    accept either form so a re-run needs no unpacking step."""
    if str(path).endswith(".gz"):
        return gzip.open(path, "rt", encoding="utf-8")
    return open(path, encoding="utf-8")


def load(path):
    out = {}
    with _open(path) as fh:
        for line in fh:
            if not line.strip():
                continue
            rec = json.loads(line)
            out[rec["id"]] = rec
    return out


def cosine(a, b):
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    return dot / (na * nb)


def pct(values, p):
    if not values:
        return None
    s = sorted(values)
    k = (len(s) - 1) * p / 100.0
    lo, hi = math.floor(k), math.ceil(k)
    if lo == hi:
        return s[int(k)]
    return s[lo] + (s[hi] - s[lo]) * (k - lo)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--a", required=True, help="side A vectors (the spike)")
    ap.add_argument("--b", required=True, help="side B vectors (the reference)")
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--label", required=True)
    ap.add_argument("--out-json", required=True)
    ap.add_argument("--out-csv", required=True)
    ap.add_argument("--gate", type=float, default=0.99)
    args = ap.parse_args()

    corpus = {json.loads(l)["id"]: json.loads(l) for l in open(args.corpus, encoding="utf-8") if l.strip()}
    A, B = load(args.a), load(args.b)

    rows = []
    skipped = []
    for cid, item in corpus.items():
        ra, rb = A.get(cid), B.get(cid)
        if ra is None or rb is None or "vector" not in (ra or {}) or "vector" not in (rb or {}):
            skipped.append({"id": cid, "reason": (rb or {}).get("error") or (ra or {}).get("error") or "missing"})
            continue
        va, vb = ra["vector"], rb["vector"]
        if len(va) != len(vb):
            skipped.append({"id": cid, "reason": f"dim {len(va)} vs {len(vb)}"})
            continue
        rows.append(
            {
                "id": cid,
                "group": item.get("group", ""),
                "lang": item.get("lang", ""),
                "bytes": item["bytes"],
                "tokens": ra.get("tokens", ""),
                "cosine": cosine(va, vb),
                "norm_a": math.sqrt(sum(x * x for x in va)),
                "norm_b": math.sqrt(sum(x * x for x in vb)),
            }
        )

    cosines = [r["cosine"] for r in rows]
    rows_sorted = sorted(rows, key=lambda r: r["cosine"])

    def by(key):
        buckets = {}
        for r in rows:
            buckets.setdefault(r[key] or "-", []).append(r["cosine"])
        return {
            k: {"n": len(v), "median": statistics.median(v), "min": min(v)}
            for k, v in sorted(buckets.items(), key=lambda kv: str(kv[0]))
        }

    summary = {
        "label": args.label,
        "n": len(rows),
        "skipped": skipped,
        "gate": args.gate,
        "median": statistics.median(cosines) if cosines else None,
        "mean": statistics.fmean(cosines) if cosines else None,
        "min": min(cosines) if cosines else None,
        "max": max(cosines) if cosines else None,
        "p01": pct(cosines, 1),
        "p05": pct(cosines, 5),
        "p25": pct(cosines, 25),
        "p75": pct(cosines, 75),
        "p95": pct(cosines, 95),
        "below_gate": [
            {"id": r["id"], "bytes": r["bytes"], "cosine": r["cosine"]}
            for r in rows_sorted
            if r["cosine"] < args.gate
        ],
        "worst_10": [
            {"id": r["id"], "bytes": r["bytes"], "cosine": r["cosine"]} for r in rows_sorted[:10]
        ],
        "by_group": by("group"),
        "by_lang": by("lang"),
        "by_bytes": {
            str(k): v
            for k, v in sorted(
                {
                    r["bytes"]: None for r in rows
                }.items()
            )
        },
        "verdict": (
            "PASS" if cosines and statistics.median(cosines) >= args.gate else "FAIL"
        ),
    }
    # by_bytes needs real aggregation, done here to keep the dict ordered by size
    buckets = {}
    for r in rows:
        buckets.setdefault(r["bytes"], []).append(r["cosine"])
    summary["by_bytes"] = {
        str(k): {"n": len(v), "median": statistics.median(v), "min": min(v)}
        for k, v in sorted(buckets.items())
    }

    with open(args.out_json, "w", encoding="utf-8") as f:
        json.dump(summary, f, indent=2)
        f.write("\n")

    with open(args.out_csv, "w", newline="", encoding="utf-8") as f:
        w = csv.DictWriter(
            f, fieldnames=["id", "group", "lang", "bytes", "tokens", "cosine", "norm_a", "norm_b"]
        )
        w.writeheader()
        for r in sorted(rows, key=lambda r: r["id"]):
            w.writerow(r)

    print(f"=== {args.label} ===")
    print(f"  n={summary['n']}  skipped={len(skipped)}")
    print(
        f"  median={summary['median']:.6f}  min={summary['min']:.6f}  "
        f"max={summary['max']:.6f}"
    )
    print(f"  p01={summary['p01']:.6f}  p05={summary['p05']:.6f}  p95={summary['p95']:.6f}")
    print(f"  below gate {args.gate}: {len(summary['below_gate'])}")
    for r in summary["worst_10"][:5]:
        print(f"    worst: {r['id']:<24} {r['bytes']:>6} B  {r['cosine']:.6f}")
    print(f"  VERDICT: {summary['verdict']}")


if __name__ == "__main__":
    main()
