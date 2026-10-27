#!/usr/bin/env python3
"""#181: replay rule (a)'s same-content signal over the 2026-09-07 hitk lists.

The efficacy run's 990-concept store copy is not available, so this replays
what can be replayed from the published, redacted outputs
(nrynss/lambo-paper, evaluation/results/2026-09-07/):

* hitk/<question>.json: each question's top 20 (rank, type, score, node id,
  content cut to 120 characters);
* lambo-2026-09-07/exports/lambo/<question>.full.json and the vimanam run's
  exports: the top 5 with full content, for the questions it covers.

Only the same-content signal is replayed. The Semantic-edge and cosine
signals need the store's edges and vectors, which these files do not carry.
Content is compared as lambo normalizes it (NFC, format characters
stripped, lowercase, whitespace collapsed). Where only the 120-character cut
is known, equality of the cuts is a proxy, and a full export, when there is
one, decides. A redacted entry is never equal to anything.

Usage: python3 hitk_replay.py <lambo-paper>/evaluation/results/2026-09-07
Prints a table; writes nothing.
"""

import json
import sys
import unicodedata
from pathlib import Path


def normalized(text):
    text = "".join(c for c in text if unicodedata.category(c) != "Cf")
    text = unicodedata.normalize("NFC", text)
    return " ".join(text.lower().split())


def full_texts(root):
    """node_id -> full content, from every published full export."""
    out = {}
    for path in sorted(root.glob("*/exports/lambo/*.full.json")):
        for hit in json.loads(path.read_text())["structuredContent"]["hits"]:
            out[hit["node_id"]] = hit["content"]
    return out


def replay(hits, full, k=3):
    """Rule (a) with the same-content signal over the window of min(20, 4k)."""
    window = min(20, 4 * k)

    def key(h):
        if h["content"].startswith("[REDACTED"):
            return None
        text = full.get(h["node_id"], h["content"])
        return normalized(text)

    picked, skipped = [], []
    for i, h in enumerate(hits):
        if len(picked) >= k:
            break
        if i < window and key(h) is not None:
            twin = next((p for p in picked if key(hits[p]) == key(h)), None)
            if twin is not None:
                skipped.append((hits[twin]["rank"], h["rank"], h["node_id"] in full))
                continue
        picked.append(i)
    return [hits[i]["rank"] for i in picked], skipped


def main():
    root = Path(sys.argv[1])
    full = full_texts(root)
    for k in (3, 5):
        print(f"\ntop_k = {k} (window {min(20, 4 * k)})")
        table(root, full, k)


def table(root, full, k):
    print(f"{'question':28} {'before':16} {'after':16} skips (kept rank, skipped rank, text)")
    for path in sorted((root / "hitk").glob("*.json")):
        data = json.loads(path.read_text())
        hits = sorted(data["hits"], key=lambda h: h["rank"])
        after, skipped = replay(hits, full, k)
        before = [h["rank"] for h in hits[:k]]
        # Same-content pairs anywhere in the top 20, for context.
        seen, pairs = {}, []
        for h in hits:
            if h["content"].startswith("[REDACTED"):
                continue
            text = normalized(full.get(h["node_id"], h["content"]))
            if text in seen:
                pairs.append((seen[text], h["rank"]))
            else:
                seen[text] = h["rank"]
        print(
            f"{data['id']:28} {str(before):16} {str(after):16} "
            f"{[(a, b, 'full' if f else 'cut') for a, b, f in skipped]} "
            f"pairs in top 20: {pairs}"
        )


if __name__ == "__main__":
    main()
