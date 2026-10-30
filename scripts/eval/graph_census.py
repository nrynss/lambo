#!/usr/bin/env python3
"""#175 topology census of one session in an offline SQLite copy (stdlib only).

No texts, node IDs, prompts, agents or local paths enter the aggregate report.
Opens the explicitly named snapshot immutable/read-only, never MCP or a writer.
This measures graph structure, not query relevance or retrieval quality.
"""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
import sqlite3

# src/recall/expand.rs::TRAVERSAL_ORDER. Only outgoing concept-to-concept edges.
TRAVERSAL_ORDER = ("Dependency", "Causal", "Hierarchical", "CoOccurrence", "Semantic")


def distribution(values):
    """Nearest-rank quantiles; empty populations have no numeric summary."""
    xs = sorted(values)
    if not xs:
        return dict(n=0, min=None, p50=None, p90=None, p99=None, max=None, mean=None, zero=0)
    return dict(n=len(xs), min=xs[0], p50=xs[math.ceil(.5 * len(xs)) - 1],
                p90=xs[math.ceil(.9 * len(xs)) - 1],
                p99=xs[math.ceil(.99 * len(xs)) - 1], max=xs[-1],
                mean=sum(xs) / len(xs), zero=xs.count(0))


def reachable(seed, adjacency, depth):
    """BFS required membership including seed; no chunk-group siblings."""
    seen, frontier = {seed}, {seed}
    for _ in range(depth):
        frontier = {v for u in frontier for v in adjacency.get(u, ())} - seen
        seen.update(frontier)
        if not frontier:
            break
    return seen


def components(nodes, adjacency):
    seen, groups = set(), []
    for root in sorted(nodes):
        if root in seen:
            continue
        group, stack = set(), [root]
        seen.add(root)
        while stack:
            u = stack.pop()
            group.add(u)
            for v in adjacency.get(u, ()):
                if v not in seen:
                    seen.add(v)
                    stack.append(v)
        groups.append(group)
    return sorted(groups, key=lambda g: (-len(g), min(g)))


def strong_component_sizes(nodes, outgoing, incoming):
    """Iterative Kosaraju: no recursion-depth limit, isolates included."""
    seen, finished = set(), []
    for root in sorted(nodes):
        if root in seen:
            continue
        seen.add(root)
        stack = [(root, iter(sorted(outgoing.get(root, ()))))]
        while stack:
            u, it = stack[-1]
            v = next(it, None)
            if v is None:
                finished.append(u)
                stack.pop()
            elif v not in seen:
                seen.add(v)
                stack.append((v, iter(sorted(outgoing.get(v, ())))))
    seen, sizes = set(), []
    for root in reversed(finished):
        if root in seen:
            continue
        count, stack = 0, [root]
        seen.add(root)
        while stack:
            u = stack.pop()
            count += 1
            for v in incoming.get(u, ()):
                if v not in seen:
                    seen.add(v)
                    stack.append(v)
        sizes.append(count)
    return sorted(sizes, reverse=True)


def census(con, session):
    meta = con.execute(
        "SELECT embedding_kind,embedding_model,embedding_dim,mutation_epoch "
        "FROM sessions WHERE session_id=?", (session,)).fetchone()
    if meta is None:
        raise ValueError("selected session does not exist")
    rows = con.execute(
        "SELECT id,concept_type,embedding IS NOT NULL,chunk_group_id,"
        "origin_interaction,created_at FROM concepts WHERE session_id=? ORDER BY id",
        (session,)).fetchall()
    if not rows:
        raise ValueError("selected session has no concepts")
    nodes = {r[0] for r in rows}
    kinds = {r[0]: r[1] for r in rows}
    interactions = {r[0] for r in con.execute(
        "SELECT id FROM interactions WHERE session_id=?", (session,))}
    edges = con.execute(
        "SELECT source,target,edge_type,weight,reinforcements "
        "FROM edges WHERE session_id=? ORDER BY id", (session,)).fetchall()
    endpoints = nodes | interactions
    missing = [e for e in edges if e[0] not in endpoints or e[1] not in endpoints]
    cc = [e for e in edges if e[0] in nodes and e[1] in nodes]
    trav = [e for e in cc if e[2] in TRAVERSAL_ORDER]
    malformed = [e for e in edges if e[2] in TRAVERSAL_ORDER
                 and (e[0] not in nodes or e[1] not in nodes)]
    if missing or malformed:
        raise ValueError("dangling/cross-session endpoints or traversable edges outside concept graph")

    outgoing, incoming, undirected = (defaultdict(set) for _ in range(3))
    for u, v, *_ in trav:
        outgoing[u].add(v)
        incoming[v].add(u)
        if u != v:
            undirected[u].add(v)
            undirected[v].add(u)
    weak = components(nodes, undirected)
    strong = strong_component_sizes(nodes, outgoing, incoming)
    incident = {u: len(outgoing[u] | incoming[u]) for u in nodes}
    cutoff = sorted(incident.values(), reverse=True)[math.ceil(len(nodes) * .01) - 1]
    hubs = {u for u in nodes if incident[u] >= max(1, cutoff)}
    d1, d2, d3, second = {}, {}, {}, {}
    hub_reaching = fan_sensitive = alternate_paths = 0
    for u in nodes:
        one = reachable(u, outgoing, 1)
        two = reachable(u, outgoing, 2)
        three = reachable(u, outgoing, 3)
        d1[u], d2[u], d3[u] = len(one) - 1, len(two) - 1, len(three) - 1
        second[u] = len(two - one)
        hub_reaching += bool((two - {u}) & hubs)
        senders = {u} | (outgoing[u] - {u})
        fan_sensitive += len({len(outgoing[v]) for v in senders if outgoing[v]}) > 1
        paths = Counter(w for v in outgoing[u] for w in outgoing[v] if w not in one)
        alternate_paths += any(count > 1 for count in paths.values())
    groups = Counter(r[3] for r in rows if r[3] is not None)
    by_type = {}
    for kind in sorted(set(kinds.values())):
        subset = {u for u in nodes if kinds[u] == kind}
        by_type[kind] = dict(
            concepts=len(subset), isolated=sum(not undirected[u] for u in subset),
            in_largest_weak_component=len(subset & weak[0]),
            zero_outdegree=sum(not outgoing[u] for u in subset),
            depth2_graph_only=distribution(d2[u] for u in subset),
            seeds_with_second_hop_additions=sum(second[u] > 0 for u in subset))
    return dict(
        schema_version=1, session=session,
        scope="single-session concept graph; no layers; no phase-1 retrieval",
        semantics=dict(directed=True, traversal_order=list(TRAVERSAL_ORDER),
                       degree="distinct traversable outgoing concept neighbours",
                       reachability="graph-only members; seed and siblings excluded",
                       quantiles="nearest rank"),
        embedding_contract=dict(zip(("kind", "model", "dim"), meta[:3])),
        mutation_epoch=meta[3], concepts=len(nodes), interactions=len(interactions),
        concept_types=dict(sorted(Counter(kinds.values()).items())),
        created_at_range=[min(r[5] for r in rows), max(r[5] for r in rows)],
        embedded_concepts=sum(r[2] for r in rows),
        quality=dict(dangling_edge_endpoints=len(missing), malformed_traversable_edges=len(malformed),
                     missing_origin_interactions=sum(r[4] not in interactions for r in rows),
                     self_loops=sum(e[0] == e[1] for e in trav)),
        edges=dict(all=len(edges), concept_to_concept=len(cc),
                   all_by_type=dict(sorted(Counter(e[2] for e in edges).items())),
                   concept_by_type=dict(sorted(Counter(e[2] for e in cc).items())),
                   traversable_rows=len(trav), distinct_directed_pairs=sum(map(len, outgoing.values())),
                   distinct_undirected_pairs=sum(map(len, undirected.values())) // 2),
        degree=dict(out=distribution(len(outgoing[u]) for u in nodes),
                    **{"in": distribution(len(incoming[u]) for u in nodes)},
                    undirected=distribution(len(undirected[u]) for u in nodes)),
        components=dict(weak_count=len(weak), weak_largest_10=[len(c) for c in weak[:10]],
                        weak_size_histogram=dict(sorted(Counter(len(c) for c in weak).items())),
                        isolates=sum(len(c) == 1 for c in weak),
                        strong_count=len(strong), strong_largest_10=strong[:10],
                        strong_size_histogram=dict(sorted(Counter(strong).items()))),
        single_seed=dict(depth1_graph_only=distribution(d1.values()),
                         depth2_graph_only=distribution(d2.values()),
                         depth3_graph_only=distribution(d3.values()),
                         second_hop_only=distribution(second.values()),
                         seeds_with_second_hop_additions=sum(v > 0 for v in second.values()),
                         seeds_with_multiple_second_hop_paths=alternate_paths,
                         seeds_with_different_sender_fanouts=fan_sensitive,
                         seeds_reaching_incident_hubs_at_depth2=hub_reaching),
        hubs=dict(definition="top 1% by distinct incident neighbours, including cutoff ties",
                  incident_cutoff=cutoff, count=len(hubs),
                  outdegree=distribution(len(outgoing[u]) for u in hubs),
                  indegree=distribution(len(incoming[u]) for u in hubs)),
        siblings=dict(grouped_concepts=sum(groups.values()), groups=len(groups),
                      group_sizes=distribution(groups.values())),
        edge_weights={t: dict(weights=distribution(e[3] for e in trav if e[2] == t),
                              reinforcements=distribution(e[4] for e in trav if e[2] == t))
                      for t in TRAVERSAL_ORDER}, by_concept_type=by_type)


def file_hash(path):
    with path.open("rb") as f:
        return hashlib.file_digest(f, "sha256").hexdigest()


def main():
    ap = argparse.ArgumentParser(description=__doc__)
    ap.add_argument("--store", required=True, type=Path, help="offline standalone SQLite copy")
    ap.add_argument("--session", required=True, help="explicit session; never infer largest")
    ap.add_argument("--revision", required=True, help="engine revision whose semantics were checked")
    ap.add_argument("--out", required=True, type=Path)
    a = ap.parse_args()
    path = a.store.resolve(strict=True)
    # immutable ignores WAL: refuse a possibly incomplete copy, rather than hiding it.
    if Path(str(path) + "-wal").exists() or Path(str(path) + "-journal").exists():
        ap.error("store must be a standalone offline copy with no WAL/journal")
    before = file_hash(path)
    con = sqlite3.connect(path.as_uri() + "?mode=ro&immutable=1", uri=True)
    try:
        if con.execute("PRAGMA quick_check").fetchone()[0] != "ok":
            raise ValueError("snapshot quick_check failed")
        report = census(con, a.session)
    finally:
        con.close()
    if before != file_hash(path):
        raise ValueError("snapshot changed during census")
    report["provenance"] = dict(snapshot_sha256=before, snapshot_bytes=path.stat().st_size,
                                engine_revision=a.revision, quick_check="ok")
    with a.out.open("x") as f:
        json.dump(report, f, indent=2, sort_keys=True, allow_nan=False)
        f.write("\n")
    print(json.dumps({k: report[k] for k in ("concepts", "edges", "single_seed")}))


if __name__ == "__main__":
    main()
