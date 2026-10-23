#!/usr/bin/env python3
"""#190 owner tool: list recall queries from a `lambo serve --ledger` file as
candidate no-answer queries.

A ledger records no relevance judgement, so a "miss" cannot be detected; this
lists every distinct recall query with its best vector cosine and the heads of
its top hits, weakest first, as a sheet for the owner to label
(`no_answer` when the store held no answer at the time, `answerable`
otherwise, `skip`), plus `private` for any query that must not be published.
Fold the sheet in with `fold_owner_labels.py`.

    python3 scripts/eval/ledger_misses.py --ledger calls.jsonl --out misses.json
"""
from __future__ import annotations

import argparse
import json
import sys


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--ledger", required=True, nargs="+")
    ap.add_argument("--out", required=True)
    ap.add_argument("--heads", type=int, default=3)
    a = ap.parse_args()
    seen: dict[str, dict] = {}
    for path in a.ledger:
        with open(path) as f:
            for line in f:
                try:
                    e = json.loads(line)
                except json.JSONDecodeError:
                    continue
                if e.get("kind") != "call" or e.get("tool") != "lambo_recall" or not e.get("query"):
                    continue
                q = e["query"].strip()
                hits = e.get("hits") or []
                cos = [h.get("legs", {}).get("vector_cosine") for h in hits]
                best = max((c for c in cos if c is not None), default=None)
                row = seen.setdefault(q, {"query": q, "times": 0, "best_vector_cosine": best,
                                          "hit_count": e.get("hit_count"),
                                          "top_hits": [(h.get("content") or "")[:120] for h in hits[: a.heads]],
                                          "label": None, "private": None, "note": None})
                row["times"] += 1
                # The weakest view of a repeated query is kept: a query that
                # once found nothing close is the no-answer candidate.
                if best is None or (row["best_vector_cosine"] is not None and best < row["best_vector_cosine"]):
                    row["best_vector_cosine"] = best
                    row["hit_count"] = e.get("hit_count")
                    row["top_hits"] = [(h.get("content") or "")[:120] for h in hits[: a.heads]]
    rows = sorted(seen.values(), key=lambda r: (r["best_vector_cosine"] is not None, r["best_vector_cosine"] or 0.0))
    with open(a.out, "w") as f:
        json.dump(rows, f, indent=1, ensure_ascii=False)
    print(f"{len(rows)} distinct recall queries")
    return 0


if __name__ == "__main__":
    sys.exit(main())
