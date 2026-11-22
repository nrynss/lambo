#!/usr/bin/env python3
"""Aggregate of the recall bodies that probe_recall.py saved, without content.

For each of the 20 queries (by index, never by text) and each round: whether
the first eight hits are the same ids in the same order on every serve, the
largest score difference among those eight, what each serve returned beyond
the eighth hit (id prefix and score), and how many warning lines the response
carried, counted as "Session inactive" or other. Reads the --save file of
probe_recall.py; prints JSON.

Usage:
    python3 probe_top8.py bodies.json > probe-top8.json
"""
import json
import sys


def warning_counts(body):
    """Counts only. A warning line can quote memory text, so none is copied."""
    lines = [str(w) for w in (body.get("warnings") or [])]
    inactive = sum(1 for w in lines if w.startswith("Session inactive"))
    return {"session_inactive": inactive, "other": len(lines) - inactive}


def main():
    with open(sys.argv[1]) as f:
        bodies = json.load(f)
    sides = list(bodies)
    queries = list(bodies[sides[0]])
    out = {"sides": sides, "queries": len(queries), "rounds": len(bodies[sides[0]][queries[0]]),
           "per_query": []}
    all_equal = True
    max_delta = 0.0
    for qi, q in enumerate(queries):
        rounds = []
        for r in range(out["rounds"]):
            hits = {s: bodies[s][q][r]["body"].get("hits", []) for s in sides}
            top = {s: [(h["node_id"], h.get("score", 0.0)) for h in hits[s][:8]] for s in sides}
            ids_equal = all([i for i, _ in top[s]] == [i for i, _ in top[sides[0]]] for s in sides)
            delta = 0.0
            if ids_equal:
                for k in range(len(top[sides[0]])):
                    scores = [top[s][k][1] for s in sides]
                    delta = max(delta, max(scores) - min(scores))
            all_equal = all_equal and ids_equal
            max_delta = max(max_delta, delta)
            rounds.append({
                "round": r,
                "hits": {s: len(hits[s]) for s in sides},
                "errors": {s: bool(bodies[s][q][r]["err"]) for s in sides},
                "top8_ids_equal_in_order": ids_equal,
                "top8_max_score_delta": round(delta, 6),
                "beyond_top8": {s: [{"id_prefix": h["node_id"][:8], "score": round(h.get("score", 0.0), 4)}
                                    for h in hits[s][8:]] for s in sides},
                "warnings": {s: warning_counts(bodies[s][q][r]["body"]) for s in sides},
            })
        out["per_query"].append({"query_index": qi, "rounds": rounds})
    out["top8_ids_equal_in_order_everywhere"] = all_equal
    out["top8_max_score_delta_everywhere"] = round(max_delta, 6)
    json.dump(out, sys.stdout, indent=2)
    sys.stdout.write("\n")


if __name__ == "__main__":
    main()
