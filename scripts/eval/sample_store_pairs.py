#!/usr/bin/env python3
"""#190 owner tool: draw candidate concept pairs from a dogfood store COPY for
labelling, stratified by cosine, without showing the cosines.

Writes a labelling sheet (pairs with empty labels, texts only) and a separate
key file (the cosines and node ids), so the labeller judges the texts blind,
as the labelling protocol in fixtures/eval/README.md asks. Never run it
against a live writer's file.

    python3 scripts/eval/sample_store_pairs.py --store /path/to/copy.db \
        --session lambo-dev --out sheet.json --key key.json

Then fill every row's `label` (related_distinct | paraphrase | unrelated |
skip), `family`, `relation` (identifier and correction families) and
`private` (true if either text must not be published), and fold the sheet in
with `fold_owner_labels.py`.
"""
from __future__ import annotations

import argparse
import itertools
import json
import random
import sys

import _store

BINS = [0.55, 0.65, 0.75, 0.80, 0.85, 1.0001]


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--store", required=True)
    ap.add_argument("--session")
    ap.add_argument("--per-bin", type=int, default=60)
    ap.add_argument("--seed", type=int, default=190)
    ap.add_argument("--max-concepts", type=int, default=4000, help="O(n^2) scan cap")
    ap.add_argument("--out", required=True)
    ap.add_argument("--key", required=True)
    a = ap.parse_args()

    rows = [(i, c, v) for i, c, v in _store.concepts(a.store, a.session) if v is not None]
    if len(rows) > a.max_concepts:
        print(f"{len(rows)} embedded concepts is over --max-concepts {a.max_concepts}", file=sys.stderr)
        return 2
    bins: dict[int, list[tuple[float, str, str, str, str]]] = {i: [] for i in range(len(BINS) - 1)}
    for (ia, ca, va), (ib, cb, vb) in itertools.combinations(rows, 2):
        if len(va) != len(vb) or ca == cb:
            continue
        c = _store.cosine(va, vb)
        for k in range(len(BINS) - 1):
            if BINS[k] <= c < BINS[k + 1]:
                bins[k].append((c, ia, ca, ib, cb))
                break
    rng = random.Random(a.seed)
    sheet, key = [], []
    for k, items in bins.items():
        for c, ia, ca, ib, cb in rng.sample(items, min(a.per_bin, len(items))):
            pid = f"store-{len(sheet) + 1:04d}"
            a_text, b_text = (ca, cb) if rng.random() < 0.5 else (cb, ca)
            sheet.append({"id": pid, "a": a_text, "b": b_text, "label": None, "family": None,
                          "relation": None, "private": None, "note": None})
            key.append({"id": pid, "cosine": round(c, 4), "bin": [BINS[k], BINS[k + 1]],
                        "node_ids": [ia, ib]})
    rng.shuffle(sheet)
    with open(a.out, "w") as f:
        json.dump(sheet, f, indent=1, ensure_ascii=False)
    with open(a.key, "w") as f:
        json.dump({"store_sha256": _store.sha256(a.store), "session": a.session, "seed": a.seed,
                   "embedded_concepts": len(rows), "pairs": key}, f, indent=1)
    print(f"{len(sheet)} pairs from {len(rows)} embedded concepts; per bin "
          + ", ".join(f"[{BINS[k]}, {BINS[k + 1]}): {len(v)}" for k, v in bins.items()))
    return 0


if __name__ == "__main__":
    sys.exit(main())
