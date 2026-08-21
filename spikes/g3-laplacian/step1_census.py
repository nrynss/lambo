"""G3 step 1 — the sparsity picture, and validation of the ported phase-1.

Writes `out/census.json` and prints the tables that go into the README.
Deterministic: no randomness anywhere.
"""

import json
from collections import Counter, defaultdict
from pathlib import Path

import numpy as np

from lambo_graph import STRUCTURAL, TRAVERSAL_ORDER, LamboGraph

HERE = Path(__file__).resolve().parent
OUT = HERE / "out"
OUT.mkdir(exist_ok=True)


def components(nodes, adj):
    """Connected components of the UNDIRECTED view. Deterministic order."""
    seen = set()
    comps = []
    for n in nodes:
        if n in seen:
            continue
        stack = [n]
        seen.add(n)
        comp = []
        while stack:
            x = stack.pop()
            comp.append(x)
            for y in adj.get(x, ()):
                if y not in seen:
                    seen.add(y)
                    stack.append(y)
        comps.append(sorted(comp))
    comps.sort(key=lambda c: (-len(c), c[0]))
    return comps


def main():
    g = LamboGraph()
    rep = {"session": g.session}
    print(f"=== G3 census — session {g.session} ===\n")

    n = len(g.nodes)
    rep["concepts"] = n
    rep["interactions"] = len(g.interactions)
    rep["edges_total"] = len(g.edges)
    rep["edges_concept_concept"] = len(g.cc_edges)
    rep["edge_types_all"] = dict(Counter(e["type"] for e in g.edges))
    rep["edge_types_cc"] = dict(Counter(e["type"] for e in g.cc_edges))
    rep["concept_types"] = dict(Counter(c["type"] for c in g.concepts.values()))
    rep["with_embedding"] = sum(c["has_embedding"] for c in g.concepts.values())
    rep["stored_blast_non_null"] = sum(
        c["stored_blast"] is not None for c in g.concepts.values()
    )
    rep["chunk_grouped"] = sum(
        c["chunk_group"] is not None for c in g.concepts.values()
    )

    print(f"concepts                {n}")
    print(f"interactions            {len(g.interactions)}")
    print(f"edges (all)             {len(g.edges)}   {rep['edge_types_all']}")
    print(f"edges (concept-concept) {len(g.cc_edges)}   {rep['edge_types_cc']}")
    print(f"concepts w/ embedding   {rep['with_embedding']}/{n}")
    print(f"stored blast_radius     {rep['stored_blast_non_null']}/{n} non-null")
    print(f"chunk_group_id set      {rep['chunk_grouped']}/{n}")
    print()

    # ---- adjacency views ------------------------------------------------
    # Recall-traversable (all five types, DIRECTED as expand.rs uses them) and
    # its undirected closure (what diffusion sees).
    trav = [e for e in g.cc_edges if e["type"] in TRAVERSAL_ORDER]
    struct = [e for e in g.cc_edges if e["type"] in STRUCTURAL]

    und = defaultdict(set)
    for e in trav:
        if e["source"] != e["target"]:
            und[e["source"]].add(e["target"])
            und[e["target"]].add(e["source"])
    outdeg = Counter()
    indeg = Counter()
    for e in trav:
        outdeg[e["source"]] += 1
        indeg[e["target"]] += 1

    # ---- density --------------------------------------------------------
    uniq_und = sum(len(v) for v in und.values()) // 2
    dens = uniq_und / (n * (n - 1) / 2)
    rep["traversable_edges"] = len(trav)
    rep["undirected_distinct_pairs"] = uniq_und
    rep["edge_density"] = dens
    rep["mean_undirected_degree"] = 2 * uniq_und / n
    print(f"traversable (5-type) edges          {len(trav)}")
    print(f"distinct undirected pairs           {uniq_und}")
    print(f"edge density (of n(n-1)/2)          {dens:.5f}")
    print(f"mean undirected degree              {2*uniq_und/n:.2f}")
    print()

    # ---- components -----------------------------------------------------
    comps = components(g.nodes, und)
    sizes = [len(c) for c in comps]
    isolated = sum(1 for s in sizes if s == 1)
    rep["components"] = len(comps)
    rep["component_sizes"] = sizes
    rep["isolated_nodes"] = isolated
    rep["largest_component"] = sizes[0]
    print(f"connected components (undirected)   {len(comps)}")
    print(f"  largest                           {sizes[0]} ({sizes[0]/n:.0%} of nodes)")
    print(f"  size distribution                 {dict(Counter(sizes))}")
    print(f"  isolated (degree-0) concepts      {isolated} ({isolated/n:.0%})")
    print()

    # ---- degree distribution -------------------------------------------
    und_deg = Counter({x: len(und[x]) for x in g.nodes})
    hist = Counter(len(und.get(x, ())) for x in g.nodes)
    rep["undirected_degree_hist"] = {str(k): v for k, v in sorted(hist.items())}
    print("undirected degree histogram (traversable edges):")
    for d in sorted(hist):
        print(f"  deg {d:>3}: {hist[d]:>4}  {'#' * min(60, hist[d])}")
    print()

    top = sorted(g.nodes, key=lambda x: (-len(und.get(x, ())), x))[:10]
    rep["top_degree"] = [
        {"node": x, "deg": len(und.get(x, ())), "content": g.snippet(x, 60)}
        for x in top
    ]
    print("highest-degree concepts:")
    for x in top:
        print(f"  deg {len(und.get(x,())):>3}  {g.snippet(x, 78)}")
    print()

    # ---- what the depth-2 ball actually costs ---------------------------
    ball = {}
    for x in g.nodes:
        e2 = g.expand([x], depth=2)
        ball[x] = len(e2)
    bs = np.array(sorted(ball.values()))
    rep["depth2_ball"] = dict(
        mean=float(bs.mean()),
        median=float(np.median(bs)),
        p90=float(np.percentile(bs, 90)),
        max=int(bs.max()),
        eq1=int((bs == 1).sum()),
    )
    print("out-directed depth-2 ball size per seed (expand.rs semantics):")
    print(
        f"  mean {bs.mean():.2f}  median {np.median(bs):.0f}  p90 "
        f"{np.percentile(bs, 90):.0f}  max {bs.max()}  "
        f"|ball|==1 (no expansion at all): {(bs==1).sum()}/{n}"
    )
    print()

    # ---- structural (blast) sub-view ------------------------------------
    sund = defaultdict(set)
    for e in struct:
        if e["source"] != e["target"]:
            sund[e["source"]].add(e["target"])
            sund[e["target"]].add(e["source"])
    scomps = components(g.nodes, sund)
    rep["structural_edges"] = len(struct)
    rep["structural_components"] = len(scomps)
    rep["structural_largest"] = len(scomps[0])
    rep["structural_isolated"] = sum(1 for c in scomps if len(c) == 1)
    print(f"structural ({'/'.join(STRUCTURAL)}) edges  {len(struct)}")
    print(
        f"  components {len(scomps)}  largest {len(scomps[0])}  "
        f"isolated {rep['structural_isolated']}"
    )

    blasts = {}
    for x in g.nodes:
        cnt, excl = g.blast_radius(x)
        if cnt:
            blasts[x] = (cnt, excl)
    rep["nodes_with_blast_gt0"] = len(blasts)
    rep["blast_hist"] = {
        str(k): v for k, v in sorted(Counter(c for c, _ in blasts.values()).items())
    }
    allsd = {x: g.all_structural_dependents(x) for x in g.nodes}
    rep["nodes_with_any_structural_dependent"] = sum(1 for v in allsd.values() if v)
    print(
        f"  concepts with >=1 structural dependent      "
        f"{rep['nodes_with_any_structural_dependent']}"
    )
    print(f"  concepts with blast_radius > 0 (EXCLUSIVE)  {len(blasts)}")
    print(f"  blast_radius histogram                      {rep['blast_hist']}")
    print()

    # ---- edge weights: is a type-weighting even available? --------------
    rep["weight_by_type"] = {
        t: sorted({e["weight"] for e in g.edges if e["type"] == t})
        for t in sorted({e["type"] for e in g.edges})
    }
    rep["reinforcements_max"] = max(e["reinforcements"] for e in g.edges)
    print("edge weight values by type (all edges):", rep["weight_by_type"])
    print("max reinforcements across all edges:", rep["reinforcements_max"])

    (OUT / "census.json").write_text(json.dumps(rep, indent=1, sort_keys=True))
    print(f"\nwrote {OUT / 'census.json'}")


if __name__ == "__main__":
    main()
