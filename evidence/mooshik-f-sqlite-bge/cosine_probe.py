#!/usr/bin/env python3
"""Exact cosine between a recall query and the DURABLE stored vectors.

This recomputes, outside Lambo, the very number `rank_by_cosine` scores inside the
SQLite adapter: it reads `concepts.embedding` out of the database (the same TEXT
codec the adapter decodes) and embeds each query with the same llama.cpp server the
run used. `lambo recall`'s printed score passes through recall's leg-merge and
scaling, so it is not the raw similarity; this is.

Usage (from the repository root, llama-server on :8080):
    python3 evidence/mooshik-f-sqlite-bge/cosine_probe.py
"""

import json
import math
import sqlite3
import urllib.request

DB = "evidence/mooshik-f-sqlite-bge/f-bge.db"
URL = "http://127.0.0.1:8080/v1/embeddings"

QUERIES = [
    "database table for people signing up",
    "login token checking layer",
    "changes that do not break existing clients",
]


def embed(text: str) -> list[float]:
    body = json.dumps({"input": text, "model": "bge-m3"}).encode()
    req = urllib.request.Request(
        URL, data=body, headers={"Content-Type": "application/json"}
    )
    with urllib.request.urlopen(req, timeout=60) as r:
        return json.load(r)["data"][0]["embedding"]


def cosine(a: list[float], b: list[float]) -> float:
    dot = sum(x * y for x, y in zip(a, b))
    na = math.sqrt(sum(x * x for x in a))
    nb = math.sqrt(sum(x * x for x in b))
    return dot / (na * nb)


def main() -> None:
    con = sqlite3.connect(DB)
    rows = con.execute(
        "SELECT content, embedding FROM concepts "
        "WHERE embedding IS NOT NULL ORDER BY content"
    ).fetchall()
    stored = []
    for content, blob in rows:
        text = blob.decode("utf-8") if isinstance(blob, bytes) else blob
        vec = [float(x) for x in text.strip()[1:-1].split(",")]
        stored.append((content, vec))
    print(f"{len(stored)} durable vectors, width {len(stored[0][1])}")
    print(
        "\nCosine of the recall query against each DURABLE stored vector.\n"
        "Note what the stored vector is the embedding OF: hybrid embeds the\n"
        "context-framed string 'Concept: {content}' (no origin text on the CLI\n"
        "path), while recall embeds the query bare. This is that comparison."
    )
    for q in QUERIES:
        qv = embed(q)
        scores = sorted(
            ((cosine(qv, v), c) for c, v in stored), key=lambda t: -t[0]
        )
        print(f"\n  query: {q!r}")
        for s, c in scores:
            print(f"    {s:+.4f}  {c}")
        best, best_c = scores[0]
        margin = best - scores[1][0]
        print(f"    -> top: {best_c!r} (margin over 2nd: {margin:+.4f})")


if __name__ == "__main__":
    main()
