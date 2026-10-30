#!/usr/bin/env python3
"""#175: audit labelled graph-only headroom before comparing propagation arms.

Pool with the production runner first. This tool classifies where *labelled*
facets can be found; it never infers usefulness from a graph edge. Review direct
alternatives before freezing labels. Public output contains only aggregates.
"""

from __future__ import annotations

import argparse
from collections import Counter
import json
from pathlib import Path

from graph_cases import write_private
from graph_compare import digest, validate


def screen(labels, pool):
    queries = validate(labels)
    rows = []
    seen = set()
    for frame in pool["queries"]:
        qid = frame["id"]
        if qid in seen or qid not in queries:
            raise ValueError("duplicate or unknown pooled query")
        seen.add(qid)
        query = queries[qid]
        facets = {f["id"] for f in query["facets"]}
        direct_ids = {c["item"] for c in frame["phase1"]}
        bfs_ids = set(frame["bfs_ids"])
        if not direct_ids <= bfs_ids:
            raise ValueError("BFS pool omits a direct seed")
        direct_facets, graph_facets = set(), set()
        for positive in query["known_relevant"]:
            node = positive["node"]
            if node in direct_ids:
                direct_facets.update(positive["facets"])
            elif node in bfs_ids:
                graph_facets.update(positive["facets"])
        graph_only = graph_facets - direct_facets
        if (
            "graph_only_facets" in query
            and set(query["graph_only_facets"]) != graph_only
        ):
            raise ValueError("declared graph-only facets differ from production pool")
        unreachable = facets - direct_facets - graph_facets
        if query["kind"] == "no_answer":
            category = "no_answer_control"
        elif unreachable:
            category = "unreachable_known_facets"
        elif graph_only:
            category = "graph_only_known_facets"
        else:
            category = "all_known_facets_direct"
        rows.append(
            dict(
                id=qid,
                topic=query["topic"],
                category=category,
                facets=len(facets),
                direct_facets=sorted(direct_facets),
                graph_only_facets=sorted(graph_only),
                unreachable_facets=sorted(unreachable),
            )
        )
    if seen != set(queries):
        raise ValueError("pool does not contain the full authored question set")
    return rows


def aggregate(rows):
    return dict(
        questions=len(rows),
        categories=dict(sorted(Counter(r["category"] for r in rows).items())),
        known_facets=sum(r["facets"] for r in rows),
        direct_known_facets=sum(len(r["direct_facets"]) for r in rows),
        graph_only_known_facets=sum(len(r["graph_only_facets"]) for r in rows),
        unreachable_known_facets=sum(len(r["unreachable_facets"]) for r in rows),
        graph_only_topics=len({r["topic"] for r in rows if r["graph_only_facets"]}),
    )


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--labels", type=Path, required=True)
    parser.add_argument("--pool", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument(
        "--key", type=Path, help="optional private per-query headroom key"
    )
    args = parser.parse_args()
    if args.key and args.key.resolve().is_relative_to(
        Path(__file__).resolve().parents[2]
    ):
        parser.error("per-query key must stay outside the repository")
    labels = json.loads(args.labels.read_text())
    pool = json.loads(args.pool.read_text())
    if pool.get("dataset_sha256") != digest(args.labels):
        raise ValueError("pool labels hash differs")
    if pool.get("snapshot_sha256") != labels.get("snapshot_sha256"):
        raise ValueError("pool snapshot differs")
    rows = screen(labels, pool)
    report = dict(
        labels_sha256=digest(args.labels),
        pool_sha256=digest(args.pool),
        snapshot_sha256=labels["snapshot_sha256"],
        **aggregate(rows),
        scope="Known labels only; an edge is not a relevance label. "
        "Screening enriches graph-only cases and does not estimate traffic prevalence.",
    )
    if args.key:
        write_private(args.key, rows)
    with args.out.open("x") as output:
        json.dump(report, output, indent=2, sort_keys=True)
        output.write("\n")
    print(json.dumps(aggregate(rows), sort_keys=True))


if __name__ == "__main__":
    main()
