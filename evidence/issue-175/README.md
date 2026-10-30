# #175: current graph census and benchmark preparation

First milestone of the [graph-relevance experiment](https://github.com/nrynss/lambo/issues/175#issuecomment-6199960984), measured **2026-10-30** on the local Apple M3 Pro rig. This is a topology census. No propagation arm, query-relevance grade, retrieval-quality result or latency comparison is reported yet.

## Frozen input and scope

- Engine semantics: `a913a9a079362902486a3f7bd550bf5e954b38e2` (`main`, v0.8.0 plus re-pin evidence).
- Source: the **currently configured serving store**, backed up through SQLite's online backup API from a `mode=ro` connection. The older top-level `lambo-dev.db` is the pre-split archive and was not the input.
- Snapshot SHA-256: `5499e0890c480304398a0ffcd80bbfba64f6aba758e11a2d5214d6a4f08db258`; 94,760,960 bytes. It is a standalone copy with no WAL/journal, passed `quick_check`, and its hash stayed unchanged through the census. The original writer stayed running.
- Selection: **session `lambo` only**. No shared layers or other project sessions enter the census. The complete raw backup, manifest, case sheet and key stay outside Git in a private local research directory.
- Embedding contract: Candle BGE-M3, 1024 dimensions, revision `5617a9f61b028005a4858fdac845db406aefb181`, safetensors SHA prefix `68440cc1b73b`. All 2,421 concepts have a non-null stored embedding; this census does not decode or validate those vectors.

The committed [`census-2026-10-30.json`](census-2026-10-30.json) contains aggregates, contract metadata and the snapshot hash. It contains no concept text, prompt, node ID, agent identity or local path.

## What graph was measured

Traversal follows `src/recall/expand.rs::TRAVERSAL_ORDER`: outgoing concept-to-concept Dependency, Causal, Hierarchical, CoOccurrence and Semantic edges. It does not reverse edges or traverse Derives, Temporal or Contradicts. Reachability includes all BFS-required graph members before selection/token fitting, excludes the seed, and excludes chunk-group siblings. The snapshot has no self-loops or dangling/cross-session edge endpoints; every concept's origin interaction is present.

Parallel edge types to the same target count once for outgoing-neighbour degree and reachable membership. All edge rows are also reported separately. Quantiles are nearest rank. Hubs are the top 1% by distinct incident neighbours, including ties at the cutoff (16 neighbours, 25 concepts); this is an explicit topology diagnostic, not a claim about useful hubs or the daemon's complete score.

| Quantity | Value |
|---|---:|
| Concepts / interactions | 2,421 / 900 |
| All edge rows | 6,272 |
| Concept-to-concept edge rows | 2,931 |
| Recall-traversable edge rows / distinct directed pairs | 2,930 / 2,921 |
| Weak components / isolated concepts | 595 / 290 |
| Largest weak component | 1,096 (45.3%) |
| Strong components / largest | 2,408 / 6 |
| Zero outgoing neighbours | 1,600 (66.1%) |
| Seeds with additions unique to hop 2 | 113 (4.7%) |
| Seeds with different nonzero sender fan-outs within two hops | 266 (11.0%) |
| Seeds with multiple paths to a new second-hop node | 10 |
| Seeds reaching an incident hub within two hops | 123 |

Single-seed reachable **other** concepts:

| Depth | Mean | Median | p90 | p99 | Maximum | No expansion |
|---|---:|---:|---:|---:|---:|---:|
| 1 | 1.207 | 0 | 4 | 14 | 23 | 1,600 |
| 2 | 1.323 | 0 | 5 | 14 | 23 | 1,600 |
| 3 | 1.397 | 0 | 5 | 15 | 29 | 1,600 |

These are **single-concept seeds**, not production phase-1 query seed sets. Real queries can have several direct matches, which can reach more nodes or compete for the same budget. The census therefore cannot establish the share of real recalls affected by #175.

### The connectivity limitation is concentrated in reasoning

| Type | Concepts | In largest weak component | Zero outgoing neighbours | Seeds gaining hop-2 nodes |
|---|---:|---:|---:|---:|
| Resource | 1,025 | 798 | 737 | 33 |
| Entity | 262 | 217 | 250 | 6 |
| Logic | 697 | 34 | 354 | 45 |
| Constraint | 241 | 35 | 128 | 14 |
| Observation | 196 | 12 | 131 | 15 |

The giant component contains most file/entity records and only a small share of reasoning records. Ranking can improve choices among reachable evidence; it cannot surface disconnected facts through propagation. This is consistent with G3's old connectivity concern, but the snapshots differ in date and session scope, so these figures are not a controlled before/after comparison.

## Decision and next milestones

**Proceed to query/label preparation, with targeted strata and representative controls.** The graph has concrete two-hop and varying-fanout opportunities, but they are a minority. These counts do not show that any reached concept is useful, nor that normalized propagation beats the zero-relevance arm.

`scripts/eval/graph_cases.py` prepared **100 distinct seed cases**, 20 each from these topology strata:

| Stratum | Eligible seeds before deduplication | Selected |
|---|---:|---:|
| Has second-hop additions | 113 | 20 |
| Different sender fan-outs | 266 | 20 |
| Reaches an incident hub | 123 | 20 |
| No outgoing expansion | 1,600 | 20 |
| Text contains an identifier/path/issue-like token | 2,103 | 20 |

Selection is deterministic (seed 175), uses no propagation outcomes, and avoids repeated seeds across strata. Candidate texts are shuffled, with topology details and graph IDs retained only in a separate private key. The sheet has **no authored questions or relevance labels yet**: query, topic/split, grades, labeller/date and publication decisions are unset. It is not a set of 100 evaluated queries. Identifier-bearing text does not by itself make a good identifier question, and hub-reaching does not imply a relevant hub.

The next steps are:

1. Author genuine questions from the texts, covering the agreed multi-hop, low-degree detail, relevant-hub, exact-identifier and sparse families. Add separately authored unanswerable controls. Grade text usefulness without knowing an arm's selection. Group by topic/connected context before separating dev and held-out; do not treat overlapping seed neighbourhoods as independent topics.
2. Pool the actual phase-1 and expanded candidates for those questions before finalizing labels. A topology neighbourhood alone is not the full relevance universe. Record query settings, seeds/scores, BFS membership, daemon scores, clock and embedding contract. Keep stratified diagnostics separate from an estimate over representative traffic.
3. Add the separate graph-only relevance input to production assembly, with warm/cold regression checks that preserve phase-1 provenance. Compare the agreed three arms on **fixed candidates and direction**. Changing only `ExpandedSet` scores currently has no effect.
4. Tune gain/damping only on dev, freeze settings, then evaluate rendered answer-facet coverage, precision/recall and useful graph-only evidence, hub share and p50/p99 latency on held-out queries. Preserve exact structural dispatch and required warnings. Publish negative outcomes; changing the production default remains gated on the issue's adoption criteria.

No new propagation algorithm, production configuration, default, graph edge or label was introduced in this milestone.

## Reproducing

Take a consistent standalone backup first; never copy only the database file of a WAL-mode writer. Keep raw inputs outside the repository. The census requires an explicit session and rejects an input with a WAL/journal beside it.

```sh
python3 scripts/eval/graph_census.py \
  --store /private/snapshot.db --session lambo \
  --revision a913a9a079362902486a3f7bd550bf5e954b38e2 \
  --out census.json
python3 scripts/eval/graph_cases.py \
  --store /private/snapshot.db --session lambo \
  --out /private/case-sheet.json --key /private/case-key.json
python3 -m unittest discover -s scripts/eval -p 'test_graph_*.py'
```

The case tool refuses outputs inside its repository, creates private files with mode 0600, and refuses overwrites. It never infers the largest session or labels a graph neighbour relevant.

Validation: 10 stdlib tests cover direction/depth, cycles, isolates, duplicate typed edges, session isolation, invalid endpoints, immutable input bytes, exclusive outputs, sidecar refusal, deterministic sampling and unset labels. SCC sizes are checked against mutual reachability on every directed three-node graph. The traversal-type list is checked against production. An independent recursive SQL census on the frozen input matched all 2,421 seeds: 1,600 empty expansions, 113 seeds with hop-2 additions, 3,203 total depth-2 graph-only members and 2,921 depth-1 members.
