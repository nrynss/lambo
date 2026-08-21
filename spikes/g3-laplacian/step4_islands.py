"""G3 step 4 — the islands, and whether provenance edges join them.

The orchestrator's own derived memory says this graph is island-heavy. This
quantifies it and answers the dogfood question directly: do the islands connect
through the `Derives` / `Temporal` provenance backbone that `expand.rs`
deliberately excludes?

If they do, that is a finding about the SHAPE of lambo's memory — the semantic
relations the agents declare are sparse, and what actually holds the graph
together is the record_action / interaction spine.
"""

import json
from collections import Counter, defaultdict
from pathlib import Path

from lambo_graph import STRUCTURAL, TRAVERSAL_ORDER, LamboGraph

HERE = Path(__file__).resolve().parent
OUT = HERE / "out"


def comps(nodes, adj):
    seen, out = set(), []
    for n in nodes:
        if n in seen:
            continue
        st, cur = [n], []
        seen.add(n)
        while st:
            x = st.pop()
            cur.append(x)
            for y in adj.get(x, ()):
                if y not in seen:
                    seen.add(y)
                    st.append(y)
        out.append(sorted(cur))
    out.sort(key=lambda c: (-len(c), c[0]))
    return out


def main():
    g = LamboGraph()
    print("=== G3 step 4 — islands and the provenance backbone ===\n")

    concept_ids = set(g.concepts)
    views = {}

    def add(name, edge_pred, via_interactions=False):
        adj = defaultdict(set)
        for e in g.edges:
            if not edge_pred(e):
                continue
            s, t = e["source"], e["target"]
            if s == t:
                continue
            adj[s].add(t)
            adj[t].add(s)
        if via_interactions:
            # Project the bipartite graph onto concepts: two concepts are
            # adjacent if they share an interaction (a Derives co-parent) or if
            # their origin interactions are Temporal-adjacent.
            by_int = defaultdict(list)
            for e in g.edges:
                if e["type"] == "Derives" and e["target"] in concept_ids:
                    by_int[e["source"]].append(e["target"])
            proj = defaultdict(set)
            for _i, cs in by_int.items():
                for a in cs:
                    for b in cs:
                        if a != b:
                            proj[a].add(b)
            for k, v in proj.items():
                adj[k] |= v
        cadj = {k: {x for x in v if x in concept_ids} for k, v in adj.items()}
        cs = comps(g.nodes, cadj)
        views[name] = cs
        iso = sum(1 for c in cs if len(c) == 1)
        print(
            f"{name:<44} components {len(cs):>4}  largest {len(cs[0]):>4} "
            f"({len(cs[0])/len(g.nodes):>4.0%})  isolated {iso:>4}"
        )
        return cs

    print(f"{'view':<44} {'':>15} {'':>13} {'':>8}")
    add("structural (Dep/Causal/Hier)", lambda e: e["type"] in STRUCTURAL)
    add("traversable (the 5 recall types)", lambda e: e["type"] in TRAVERSAL_ORDER)
    add(
        "traversable + Derives co-parent projection",
        lambda e: e["type"] in TRAVERSAL_ORDER,
        via_interactions=True,
    )
    add(
        "everything incl. Derives/Temporal spine",
        lambda e: True,
        via_interactions=True,
    )
    print()

    trav = views["traversable (the 5 recall types)"]
    prov = views["traversable + Derives co-parent projection"]
    print(
        f"Adding the Derives co-parent projection collapses "
        f"{len(trav)} islands -> {len(prov)}, and the largest component grows "
        f"{len(trav[0])} -> {len(prov[0])} concepts."
    )
    print()

    # --- who lives on an island? ---------------------------------------
    small = [c for c in trav if len(c) <= 2]
    print(f"the {len(small)} smallest traversable islands ({sum(len(c) for c in small)} concepts):")
    typ = Counter()
    for c in small:
        for x in c:
            typ[g.concepts[x]["type"]] += 1
    print(f"  concept types on those islands: {dict(typ.most_common())}")
    allty = Counter(c["type"] for c in g.concepts.values())
    print(f"  vs the whole corpus:            {dict(allty.most_common())}")
    print()
    print("  a sample of island concepts:")
    for c in small[:10]:
        for x in c[:1]:
            print(f"    [{g.concepts[x]['type']:<10}] {g.snippet(x, 74)}")
    print()

    # --- what the big component is made of ------------------------------
    big = set(trav[0])
    print(f"the giant component ({len(big)} concepts) by type:")
    bt = Counter(g.concepts[x]["type"] for x in big)
    for t, n in bt.most_common():
        print(f"    {t:<12} {n:>4} / {allty[t]:<4} ({n/allty[t]:>4.0%} of that type)")
    print()

    # --- which edge type carries the connectivity? ----------------------
    print("connectivity contribution, one type at a time (undirected):")
    for t in ("Dependency", "Causal", "CoOccurrence", "Hierarchical", "Derives", "Temporal"):
        adj = defaultdict(set)
        for e in g.edges:
            if e["type"] == t and e["source"] != e["target"]:
                if e["source"] in concept_ids and e["target"] in concept_ids:
                    adj[e["source"]].add(e["target"])
                    adj[e["target"]].add(e["source"])
        cs = comps(g.nodes, adj)
        touched = len({x for v in adj.values() for x in v} | set(adj))
        print(
            f"  {t:<14} concept-concept edges "
            f"{sum(1 for e in g.edges if e['type']==t and e['source'] in concept_ids and e['target'] in concept_ids):>4}"
            f"   concepts touched {touched:>4}   components {len(cs):>4}"
            f"   largest {len(cs[0]):>4}"
        )

    (OUT / "islands.json").write_text(
        json.dumps(
            {
                k: dict(n_components=len(v), sizes=[len(c) for c in v])
                for k, v in views.items()
            },
            indent=1,
        )
    )
    print(f"\nwrote {OUT/'islands.json'}")


if __name__ == "__main__":
    main()
