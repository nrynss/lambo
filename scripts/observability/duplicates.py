#!/usr/bin/env python3
"""DOGFOOD metric 3 — near-duplicate concepts above and below the merge threshold.

The question: **is canonicalization actually converging the vocabulary, or is the
merge threshold too high for a real embedder?**

Hybrid derive merges an incoming concept into an existing one at cosine >=
`semantic_match_threshold` (0.85, `src/graph/hybrid.rs`). G1 measured BGE-M3's
paraphrase band at 0.6889–0.8984 — straddling that threshold — and predicted
that a real embedder would therefore *create duplicates* where the
fixture-calibrated tests show convergence. This is the post-hoc scan that
catches them: a full pairwise cosine over the store's durable vectors, reusing
`evidence/mooshik-f-sqlite-bge/cosine_probe.py`'s decode of the SQLite `TEXT`
embedding codec.

Two populations, and the second is the interesting one:

  * **at or above the threshold** — pairs that *should* have merged and did not.
    Every one of these is a real defect: two concepts the write path was
    supposed to converge. (Ordering matters: a pair only gets the chance to merge
    if the older concept had an embedding when the newer one was written, so a
    session whose embedder came up late will show these legitimately.)
  * **in the band below the threshold** — pairs a human would call duplicates
    that the write path was never going to catch. These are the evidence for or
    against lowering the threshold, and G1's prediction stands or falls on how
    many there are.

Runs against the store, not the ledger: durable embeddings are what the merge
decision reads, and the ledger deliberately carries no vectors. Pass a ledger
too and the report cross-checks the pair count against the derives that created
them.

Usage:
    python3 scripts/observability/duplicates.py --store ~/lambo-dogfood/lambo.db
    python3 scripts/observability/duplicates.py --store lambo.db --band 0.60 --json
    python3 scripts/observability/duplicates.py --store lambo.db --ledger calls.jsonl
"""

from __future__ import annotations

import argparse
import itertools
import json
import math
import sqlite3
import sys
from typing import Any

import _ledger


def decode_vector(blob: Any) -> list[float] | None:
    """The SQLite adapter's embedding codec: a bracketed `TEXT` float list.

    Same decode `cosine_probe.py` uses, kept identical on purpose — two scripts
    that decode the same column two ways will eventually disagree.
    """
    if blob is None:
        return None
    text = blob.decode("utf-8") if isinstance(blob, bytes) else str(blob)
    text = text.strip()
    if not text.startswith("[") or not text.endswith("]"):
        return None
    body = text[1:-1].strip()
    if not body:
        return None
    try:
        return [float(x) for x in body.split(",")]
    except ValueError:
        return None


def cosine(a: list[float], b: list[float]) -> float:
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    if na == 0.0 or nb == 0.0:
        return 0.0
    return dot / (na * nb)


def load_concepts(store: str, session: str | None) -> tuple[list[tuple[str, str, list[float]]], dict[str, Any]]:
    con = sqlite3.connect(f"file:{store}?mode=ro", uri=True)
    try:
        sql = "SELECT id, content, embedding, canonization_status FROM concepts"
        params: tuple[Any, ...] = ()
        if session:
            sql += " WHERE session_id = ?"
            params = (session,)
        rows = con.execute(sql + " ORDER BY content", params).fetchall()
    except sqlite3.Error as exc:
        con.close()
        raise _ledger.LedgerError(
            f"{store} does not look like a provisioned lambo store: {exc}"
        ) from exc
    con.close()

    vectors: list[tuple[str, str, list[float]]] = []
    no_embedding = 0
    status = {}
    for node_id, content, blob, canon in rows:
        status[canon] = status.get(canon, 0) + 1
        vec = decode_vector(blob)
        if vec is None:
            no_embedding += 1
            continue
        vectors.append((node_id, content, vec))
    meta = {
        "store": store,
        "session": session,
        "concepts": len(rows),
        "with_embedding": len(vectors),
        "without_embedding": no_embedding,
        "by_canonization_status": status,
        "dim": len(vectors[0][2]) if vectors else None,
    }
    return vectors, meta


def analyse(
    vectors: list[tuple[str, str, list[float]]],
    threshold: float,
    band_floor: float,
    max_pairs: int,
) -> dict[str, Any]:
    n = len(vectors)
    total_pairs = n * (n - 1) // 2
    if total_pairs > max_pairs:
        raise _ledger.LedgerError(
            f"{n} concepts is {total_pairs} pairs, over the --max-pairs cap of "
            f"{max_pairs}. This scan is O(n^2) by design (it is the exact answer, "
            f"not an index probe); raise the cap deliberately or scope it with "
            f"--session."
        )
    above: list[dict[str, Any]] = []
    in_band: list[dict[str, Any]] = []
    scores: list[float] = []
    for (id_a, text_a, vec_a), (id_b, text_b, vec_b) in itertools.combinations(vectors, 2):
        if len(vec_a) != len(vec_b):
            continue  # mixed-width store: a contract change mid-session
        score = cosine(vec_a, vec_b)
        scores.append(score)
        row = {
            "cosine": round(score, 4),
            "a": text_a,
            "b": text_b,
            "a_id": id_a,
            "b_id": id_b,
        }
        if score >= threshold:
            above.append(row)
        elif score >= band_floor:
            in_band.append(row)
    above.sort(key=lambda r: -r["cosine"])
    in_band.sort(key=lambda r: -r["cosine"])
    return {
        "threshold": threshold,
        "band_floor": band_floor,
        "pairs_compared": len(scores),
        "at_or_above_threshold": above,
        "in_band_below_threshold": in_band,
        "max_cosine": round(max(scores), 4) if scores else None,
        "mean_cosine": round(sum(scores) / len(scores), 4) if scores else None,
    }


def ledger_cross_check(ledger: _ledger.Ledger) -> dict[str, Any]:
    created = matched = merged = 0
    for call in ledger.sorted_calls():
        if call.get("tool") != "lambo_derive" or not _ledger.succeeded(call):
            continue
        created += call.get("created") or 0
        matched += call.get("matched") or 0
        merged += call.get("semantic_merged") or 0
    return {"ledger_created": created, "ledger_matched": matched, "ledger_semantic_merged": merged}


def render(meta: dict[str, Any], data: dict[str, Any], cross: dict[str, Any] | None) -> list[str]:
    out = [
        f"== metric 3 — near-duplicate concepts ==",
        f"   store:    {meta['store']}"
        + (f"  (session {meta['session']})" if meta["session"] else "  (all sessions)"),
        f"   concepts: {meta['concepts']} total, {meta['with_embedding']} with a durable "
        f"embedding (dim {meta['dim']}), {meta['without_embedding']} without",
        f"   status:   {meta['by_canonization_status']}",
        f"   pairs:    {data['pairs_compared']} compared, "
        f"merge threshold {data['threshold']:g}, band floor {data['band_floor']:g}",
    ]
    if meta["without_embedding"]:
        out.append(
            "   note:     concepts without an embedding CANNOT be scanned. They are "
            "invisible to this metric, not proven distinct."
        )
    if data["max_cosine"] is None:
        out += ["", "Fewer than two embedded concepts — nothing to compare."]
        return out
    out.append(
        f"   cosine:   max {data['max_cosine']:.4f}, mean {data['mean_cosine']:.4f}"
    )

    above = data["at_or_above_threshold"]
    out += [
        "",
        f"AT OR ABOVE the merge threshold ({data['threshold']:g}): {len(above)} pair(s)",
    ]
    if not above:
        out.append(
            "   none — every pair the write path was supposed to converge did converge"
        )
    else:
        out.append(
            "   Each of these is a pair hybrid derive should have merged. Check the "
            "ordering caveat first: a pair cannot merge if the older concept had no "
            "embedding when the newer one was written."
        )
    for row in above[:25]:
        out.append(f"   {row['cosine']:.4f}  {row['a']!r}")
        out.append(f"           {row['b']!r}")
    if len(above) > 25:
        out.append(f"   … and {len(above) - 25} more")

    band = data["in_band_below_threshold"]
    out += [
        "",
        f"IN THE BAND [{data['band_floor']:g}, {data['threshold']:g}): {len(band)} pair(s)",
        "   G1 measured BGE-M3 paraphrases at 0.6889-0.8984, straddling the 0.85",
        "   threshold, and predicted duplicates would land here. Read these as a",
        "   human: how many are genuine duplicates decides whether the threshold",
        "   should come down.",
    ]
    for row in band[:25]:
        out.append(f"   {row['cosine']:.4f}  {row['a']!r}")
        out.append(f"           {row['b']!r}")
    if len(band) > 25:
        out.append(f"   … and {len(band) - 25} more")

    if cross:
        out += [
            "",
            "Ledger cross-check:",
            f"   derives created {cross['ledger_created']}, matched "
            f"{cross['ledger_matched']}, semantically merged "
            f"{cross['ledger_semantic_merged']}",
            "   `matched` is exact-key convergence and `semantic_merged` is the",
            "   similarity path. A high pair count above with a zero",
            "   `semantic_merged` means the vector merge never fired at all.",
        ]
    return out


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(
        description=__doc__ or "",
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--store", required=True, help="SQLite lambo store to scan")
    parser.add_argument("--session", help="scope the scan to one session id")
    parser.add_argument(
        "--ledger",
        nargs="*",
        default=[],
        help="optional ledger file(s) to cross-check the derive counts against",
    )
    parser.add_argument(
        "--threshold",
        type=float,
        default=_ledger.MERGE_THRESHOLD,
        help="hybrid semantic_match_threshold in force (default: %(default)s)",
    )
    parser.add_argument(
        "--band",
        type=float,
        default=0.65,
        help="floor of the below-threshold band to report (default: %(default)s, "
        "just under G1's measured paraphrase minimum of 0.6889)",
    )
    parser.add_argument(
        "--max-pairs",
        type=int,
        default=2_000_000,
        help="refuse an O(n^2) scan larger than this (default: %(default)s)",
    )
    parser.add_argument("--json", action="store_true", help="emit JSON instead of text")
    args = parser.parse_args(argv)

    try:
        vectors, meta = load_concepts(args.store, args.session)
        data = analyse(vectors, args.threshold, args.band, args.max_pairs)
    except _ledger.LedgerError as exc:
        print(f"duplicates.py: {exc}", file=sys.stderr)
        return 1

    cross = None
    if args.ledger:
        cross = ledger_cross_check(_ledger.load(args.ledger))
        data.update(cross)

    if args.json:
        json.dump({"report": "metric 3 — near-duplicate concepts", **meta, **data},
                  sys.stdout, indent=2)
        sys.stdout.write("\n")
    else:
        print("\n".join(render(meta, data, cross)))
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
