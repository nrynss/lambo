# G3 — the graph Laplacian for expansion and blast radius

**Exploratory spike, 2026-08-21. Read-only. Not product code, not a workspace
member.** Produces measurements and a recommendation; changes no behaviour.

Spec: [`dev-diary/lambo-for-mooshik/G-recall-calibration.md` §G3](../../dev-diary/lambo-for-mooshik/G-recall-calibration.md).

**Verdict in one line: H1 falsified, H2 upheld, and the most valuable finding is
neither of them** — the dogfood graph connects *artifacts* and strands
*reasoning*, so 63% of `Logic` concepts sit off the giant component where no
expansion algorithm can reach them.

---

## What was measured, against what

| | |
| --- | --- |
| Snapshot | `~/lambo-dogfood/lambo-dev.db` copied to `snapshot.db`, opened `immutable=1`. The live store and the `lambo-dogfood` MCP session were never written or locked. |
| Queries | all **22** real `lambo_recall` calls in `~/lambo-dogfood/calls.jsonl`, with their recorded `top_k` |
| Graph | 386 concepts, 98 interactions, 1027 edges of which **528** are Concept↔Concept |
| Fidelity | the Python port of `normalize_tokens`/`canonical_key` passes `fixtures/canonicalization-cases.json` verbatim (`python3 lambo_graph.py`) |

### Two semantics that had to be got right first

1. **`expand.rs` is a membership gate, not a ranking.** `assemble.rs` scores
   every member `daemon*w_daemon + relevance*w_query`, and `relevance` is the
   *phase-1* score — exactly `0.0` for anything that entered by expansion. So
   fixed-depth expansion contributes no ordinal signal at all. The honest
   comparison is set-vs-set at equal budget.
2. **`blast_radius` is not a dependent count.** Per `src/store/sqlite.rs`
   (and its MemoryStore/Cockroach twins) it counts concepts with an aged inbound
   `{Dependency, Causal, Hierarchical}` edge **from** the node and **no** aged
   inbound structural edge from any other concept source — i.e. *exclusive*
   dependents, the ones orphaned if the node went away. It already encodes a
   crude sole-support notion, which changes H2's whole shape.

### Edge weighting, argued

All four Concept↔Concept types carry `weight` **exactly 0.5**, and
`reinforcements` is **1** for every edge in the graph. The stored weight and
reinforcement fields carry *zero* discriminating information here, so a
data-driven weighting is unavailable; any type weighting must be hand-chosen.
The `*_typed` variants therefore borrow `expand.rs`'s own priority tiers
(`Dependency = Causal = 1.0 > Hierarchical = 0.7 > CoOccurrence = 0.4`). It
made no material difference to any conclusion below.

`Derives` (Interaction→Concept) and `Temporal` (Interaction→Interaction) are
excluded from the diffusion for the reason `expand.rs` excludes them — they are
the bipartite provenance backbone, not semantic relations, and diffusing along
them makes every concept from one interaction a neighbour of every other. Step 4
measures what including them *would* do, because that turned out to be the most
interesting number in the spike.

---

## Sparsity: the graph is thin, and thin in a specific way

`python3 step1_census.py`

| | |
| --- | --- |
| concepts | 386 |
| Concept↔Concept edges | 528 (`Causal` 207, `CoOccurrence` 202, `Dependency` 94, `Hierarchical` 25, `Semantic` **0**) |
| edge density | 0.00703 of `n(n-1)/2` |
| mean undirected degree | 2.70 |
| **degree exactly 1** | **179 / 386 (46%)** |
| connected components (undirected, traversable) | **41**, largest 266 (69%), 11 isolated |
| concepts with an embedding | 22 / 386 |
| concepts with a stored `blast_radius` | **0 / 386** — no canonization has ever completed here |
| concepts with a `chunk_group_id` | 0 / 386 (so sibling force-inclusion is dead code on this graph) |

**The directed depth-2 ball is usually empty.** `expand.rs` follows
`out_neighbors_typed` only, and this graph is directed hub→sink: `record_action`
puts the out-edges on the *action* node, and the file/commit resources are
sinks. Per single seed:

| mean | median | p90 | max | `|ball| == 1` (no expansion at all) |
| --- | --- | --- | --- | --- |
| 2.63 | **1** | 7 | 20 | **248 / 386 (64%)** |

Multi-seed real queries do expand (12–30 seeds → 20–69 members), so this is not
a claim that phase 2 never fires. It is a claim about where the headroom is.

---

## H1 — expansion as diffusion. **FALSIFIED.**

`python3 step2_diffusion.py && python3 step2b_cliff.py`

Equal-budget set comparison: take the fixed-depth expansion's non-seed members
`E_fixed`, take the top-`|E_fixed|` non-seed nodes by diffusion score, measure
the disagreement. Seeds are the ported phase-1 output for each real query.

| variant | mean Jaccard | set disagreements | same-set, different-order | mean τ vs BFS level |
| --- | --- | --- | --- | --- |
| `ppr_dir_a0.5` | **0.994** | **2 / 22 (9%)** | 18 | 0.300 |
| `ppr_dir_a0.3` | 0.985 | 4 / 22 (18%) | 16 | 0.277 |
| `ppr_dir_a0.15` | 0.970 | 7 / 22 (32%) | 13 | 0.261 |
| `heat_t0.5` (und.) | 0.642 | 19 / 22 (86%) | 0 | 0.270 |
| `heat_t1.0` (und.) | 0.590 | 19 / 22 (86%) | 0 | 0.228 |
| `heat_t2.0` (und.) | 0.520 | 19 / 22 (86%) | 0 | 0.120 |
| `ppr_und_a0.3` | 0.522 | 20 / 22 (91%) | 0 | 0.295 |
| **CONTROL: fixed-depth BFS, symmetrized** | **0.468** | **20 / 22 (91%)** | — | — |

`*_typed` variants land within ±0.03 Jaccard of their plain twins and change no
verdict; full table in `out/h1_summary.json`.

**The control is the whole result.** The undirected diffusions disagree with
fixed-depth on 19–20 of 22 queries — but so does *fixed-depth BFS run on the
symmetrized graph*, at a lower Jaccard still. That disagreement is the
**symmetrization**, not the diffusion. Hold directedness constant and diffusion
agrees with the depth cliff at Jaccard 0.994, disagreeing on the set in 2 of 22
queries. §G3's falsifier — "fewer than ~1 in 10 real recalls, or disagrees only
on ties" — is met on both clauses at once: 9% set disagreement, and the other
18 queries are the *same set* in a different order, where "order" is a quantity
`assemble.rs` discards.

**The cliff sits where diffusion would put it.** An *inversion* is a node at
directed distance ≥ 3 (excluded by the cliff) that diffusion scores above some
node at distance 1 or 2 (admitted):

| operator | queries with an inversion | total inversions | worst query |
| --- | --- | --- | --- |
| `ppr_dir_a0.5` | 2 / 22 | **3** | 2 |
| `ppr_dir_a0.3` | 4 / 22 | 8 | 3 |
| `ppr_dir_a0.15` | 7 / 22 | 14 | 3 |
| `heat_t1.0` (und.) | 19 / 22 | 627 | 129 |
| `ppr_und_a0.3` | 20 / 22 | 538 | 84 |

Three inversions across 22 real queries. The "hard depth cliff" the hypothesis
was built to soften is, at matched directedness, already within three nodes of
where continuous decay puts the boundary.

### The disagreements are hub bias, and they are regressions

The few real disagreements do not favour diffusion. At `alpha=0.15` the same
single concept — *"Two J3 defects were found by MEASURING THE SHIPPED BINARY…"*,
a high-degree distance-3 hub — is promoted on 6 of the 7 disagreeing queries,
and what it displaces is precise level-1 provenance:

| query | PPR promotes (distance ≥ 3) | PPR drops (BFS level) |
| --- | --- | --- |
| `J3 round 1 findings projection bound` | the "MEASURING THE SHIPPED BINARY" hub, +2 more | `206f977 feat(serve): J1 per-call agent identity` (1), `docs/reference/mcp.mdx` (1), `evidence/mcp-client-stdio/README.md` (1) |
| `J3 probe representative ceilings observed` | J4-ledger requirement, receipt taxonomy, rejected write-intent queue | `src/memory.rs` (1), `528ade6 J3 docs and register sweep` (1), `scripts/observability/README.md` (1) |
| `J3 projection bound remediation lanes EWMA` | the same hub | `dev-diary/lambo-for-mooshik/J-multi-client.md` (1) |
| `per-call agent identity J1 attribution reserve` | the same hub | `mcp::server::tests::two_agents_through_one_server_hold_distinct_locks` (1) |

This is textbook PageRank popularity bias. On a memory graph whose level-1
dependents *are* the precise provenance an agent asked for, mass-based ranking
prefers the globally popular concept over the answer. A human judge reading the
table above sides with fixed-depth.

### What H1 did surface: a cheaper question

Phase 2 follows **out-edges only**. Symmetrizing the *same* BFS — one function
call, no Laplacian — multiplies the expansion 2–4× on every real query:

| seeds | out-edges (today) | symmetrized | query |
| --- | --- | --- | --- |
| 12 | 20 | 67 | `J multi-client survivability proxy lease agent identity as` |
| 30 | 43 | 101 | `J2 proxy losing serve endpoint lease` |
| 30 | 46 | 127 | `J2 round 1 remediation in-flight endpoint TMPDIR` |
| 12 | 22 | 80 | `J2 round 2 remediation DIAL_BUDGET operator ruling` |
| 5 | **1** | **19** | `J3 round 2 probe text representative` |
| 5 | 2 | 2 | `J3 durable intents redesign` |

Whether that is *better* is untested here and is a different hypothesis — it
needs a relevance judgement, not a Laplacian. It is the biggest single
behavioural lever in phase 2, and it is far cheaper to try than diffusion.

---

## H2 — blast radius as effective resistance. **UPHELD.**

`python3 step3_resistance.py && python3 step3b_control.py`

`R_eff(u,v) = L†_uu + L†_vv − 2·L†_uv` on the symmetrized structural graph,
computed **per connected component** (a disconnected Laplacian's pseudoinverse
yields finite but meaningless cross-component values; those are reported as
infinite). Low resistance = many independent paths = deeply load-bearing.
Per-node aggregate: `conductance = Σ_dependents 1/R_eff`.

**First, a structural fact that nearly killed H2 outright.** Of the 159
`(node, dependent)` pairs `blast_radius` actually counts, **156 (98.1%) sit at
`R_eff = 1/w = 2.0` exactly** — a single edge, no multiplicity. That is not a
coincidence: a dependent with path multiplicity has an inbound structural edge
from another source, which is exactly what the exclusivity filter excludes. So
resistance is *constant* over the set `blast_radius` looks at and cannot reorder
it. **The discriminating resistance lives entirely outside that set.**

Over all 325 structural `(node, dependent)` pairs it discriminates well — 118
distinct values, and half the pairs show multiplicity:

| `R_eff` band | pairs | share |
| --- | --- | --- |
| `R < 0.6` (deep, many independent paths) | 41 | 12.6% |
| `0.6 ≤ R < 1.2` | 67 | 20.6% |
| `1.2 ≤ R < 2.0` | 52 | 16.0% |
| `R = 2.0` (single edge, chain) | 165 | 50.8% |

### The confound control

Conductance sums over *all* structural dependents while `blast_radius` filters
to exclusive ones, so the reordering has two candidate causes: the resistance
(H2's claim), or merely dropping the exclusivity filter (no Laplacian needed).
At `k = 9`, the set that clears the Stage-3 bar `blast_radius > 5`:

| comparison | overlap | Jaccard |
| --- | --- | --- |
| `blast > 5` vs `count_all` top-9 — **CONTROL** | 7 / 9 | 0.636 |
| `blast > 5` vs `conductance` top-9 — H2 | 4 / 9 | 0.286 |
| `count_all` top-9 vs `conductance` top-9 — **H2 net** | 6 / 9 | **0.500** |

Kendall τ over all 65 concepts with a structural dependent: `count_exclusive` vs
`count_all` **+0.568**, `count_exclusive` vs `conductance` **+0.436**,
`count_all` vs `conductance` **+0.840**. H2 survives: at τ 0.840 and Jaccard
0.500 the resistance is neither redundant with the plain count nor a restatement
of it — 5 of the top 14 move ≥ 3 places.

### The reordering, for a human judge

`*` marks a concept that warns today (`blast_radius > 5`).

| `blast` | all deps | conductance | concept |
| --- | --- | --- | --- |
| 7 `*` | 18 | **17.85** | Committed the J2 round-2 review remediation on wt/j2 as 72050b7 + 6442d95 |
| 4 | 13 | **17.52** | Implemented J3 (writes acknowledged before the embedder) on wt/j3, 427fabf..528ade6 |
| 7 `*` | 15 | 16.16 | Committed the J1 round-2 remediation as f083a5a on wt/j1 |
| 7 `*` | 13 | 12.47 | Committed the J1 round-1 remediation as 4a9c6a2 on wt/j1 |
| 2 | 8 | 12.01 | Committed cab9881 (J3-R1-4 pre-pass docstring; J3-R1-5 drop-class tests) |
| 5 | 12 | 11.73 | Committed J2 stages 3+4 as 275b418 on wt/j2 |
| 6 `*` | 12 | 11.49 | Landed J1 (per-call agent identity) as 206f977 |
| 3 | 9 | 11.32 | Committed a573e64 on wt/j2 — J2-L1 and J2-L2, the live two-client probe |
| 0 | 7 | 10.29 | Closed J2 round-1 adversarial review on wt/j2 (19f51d3..40791e2) |
| **10 `*`** | 15 | **9.85** | Committed J2 stage 1 as 7f51bb6 on wt/j2 — `session_leases` gains `endpoint` |
| 6 `*` | 10 | 7.38 | Committed J3 stage 1 on wt/j3 as 427fabf — `src/writeq.rs` |
| 6 `*` | 10 | 6.99 | Committed J2 stage 2 as 8e64fc8 on wt/j2 — `src/mcp/endpoint.rs` |
| 7 `*` | 9 | 4.88 | Closed the J2 round-2 adversarial review on wt/j2 as 920e096 |
| **6 `*`** | 6 | **3.00** | Stood up the dogfood rig: pinned binary lambo-3039b82, SQLite, BGE-M3 |

Three cases carry the argument:

* **`Implemented J3 (writes acknowledged before the embedder)`** — `blast_radius`
  **4**, below the Stage-3 bar, so it warns *nobody* today. Yet 13 structural
  dependents and the 2nd-highest conductance. Its dependents
  `dev-diary/lambo-for-mooshik/J-multi-client.md` (R=0.4222),
  `src/mcp/server.rs` (R=0.4726) and `src/memory.rs` (R=0.4951) are the three
  most deeply-supported pairs in the whole graph. This is a load-bearing pillar
  the count structurally cannot see, because every one of its dependents is
  *also* depended on by something else — which is precisely why it is
  load-bearing.
* **`Stood up the dogfood rig`** — `blast_radius` **6**, warns today, but
  conductance **3.00** with all six dependents pendant at `R = 2.0`. §G3's
  "fragile linkage, cheap to verify by hand", verbatim.
* **`Committed J2 stage 1 as 7f51bb6`** — the **highest** blast radius in the
  graph (10) and today's top warning, falls to conductance rank 11. Breadth of
  fan-out, not depth of support.

The falsifier ("if the two rankings agree, the count is sufficient") does not
trip. **The operator's endorsement of the reordering is the one thing this spike
cannot supply itself** — the table above is the artefact to judge.

---

## Islands: the finding worth more than either hypothesis

`python3 step4_islands.py`

| view | components | largest | isolated |
| --- | --- | --- | --- |
| structural (`Dep`/`Causal`/`Hier`) | 144 | 221 (57%) | 132 |
| traversable (the 5 recall types) | **41** | 266 (69%) | 11 |
| traversable + `Derives` co-parent projection | 40 | 271 (70%) | 11 |
| everything incl. the `Derives`/`Temporal` spine | 40 | 271 (70%) | 11 |

**The provenance spine does not join the islands: 41 → 40.** So the answer to
"do the orchestrator's islands connect through `record_action` edges" is *no* —
not through `Derives`. They connect through **`Causal`**, which `record_action`
writes and which is already in the traversable set:

| type | cc edges | concepts touched | components | largest |
| --- | --- | --- | --- | --- |
| `Causal` | 207 | 170 | 227 | **135** |
| `CoOccurrence` | 202 | 156 | 268 | 11 |
| `Dependency` | 94 | 117 | 293 | 34 |
| `Hierarchical` | 25 | 41 | 361 | 6 |

And here is the shape of the problem. The giant component, by concept type:

| type | in giant | total | share of that type |
| --- | --- | --- | --- |
| `Resource` | 160 | 173 | **92%** |
| `Entity` | 39 | 46 | **85%** |
| `Constraint` | 29 | 59 | 49% |
| `Logic` | 29 | 78 | **37%** |
| `Observation` | 9 | 30 | **30%** |

**The graph connects artifacts and strands reasoning.** The mechanism is in the
two write APIs:

* `lambo_record_action` writes `Causal` (produces/modifies) and `Dependency`
  (depends_on) edges from the action node to file and commit resources. Many
  actions touch the same files, so those resources become shared hubs and the
  giant component is the artifact web.
* `lambo_derive` writes only **intra-call** `CoOccurrence` cliques — pairwise
  among one call's concepts, capped by `max_cooccurrence_per_derive` (hence
  `CoOccurrence`'s largest component being 11) — plus `Hierarchical` edges from
  the rarely-used `parent_of` (25 edges in the entire graph). A derived concept
  joins the giant component only if it happens to canonicalize onto a node
  `record_action` also touched.

So the decisions, lessons and constraints that agents *derive* — the content a
future agent most needs — arrive edge-less and strand. **No phase-2 expansion
algorithm can reach them from an artifact hit, because there is no path.** This
also explains why diffusion cannot beat fixed-depth here: there is nothing to
diffuse *to*. Fix connectivity before fixing the traversal.

This is a data-poverty finding, but it is *not* the "insufficient density to
discriminate, re-run later" kind for H1 — H1's falsifier tripped on a real,
controlled measurement (Jaccard 0.994 at matched directedness), not on noise.
It is the reason the ceiling is where it is.

---

## Product-design questions, measured

`python3 step5_cost.py` — see `out/cost.json`. Single-threaded dense
numpy/scipy on this host. These are the numbers §G3 asks for "rather than an
assumption". They are answered for the record even though H1 is shelved,
because H2's recommendation needs the same three answers.

**Incremental maintenance — MEASURED, and the question dissolves at this scale.**

| operation, n = 386 | time |
| --- | --- |
| full n×n PPR operator (dense solve) | 2.3 ms |
| heat kernel `exp(−tL)` (dense `expm`) | 6.3 ms |
| Laplacian pseudoinverse (SVD) | 8.6 ms |
| apply a **precomputed** operator (matvec) | 7.0 µs |
| **one seed-vector solve, no precompute** | **0.51 ms** |

The last row is the one that matters: a single PPR solve for one query's seed
vector costs 0.5 ms, so there is **no precompute to invalidate** and incremental
maintenance is not needed at session scale — the daemon can recompute from the
current edge set per query and never hold stale state. Scaling, on synthetic
graphs at the same mean degree 2.70 (RNG seeded `20260821`):

| n | edges | one solve | full operator | pseudoinverse |
| --- | --- | --- | --- | --- |
| 386 | 521 | 0.5 ms | 1.6 ms | 17.6 ms |
| 1 000 | 1 350 | 4.8 ms | 19.2 ms | 126 ms |
| 3 000 | 4 050 | 128 ms | 504 ms | 3.85 s |
| 10 000 | 13 500 | 4.80 s | 16.3 s | **173 s** |

Dense `pinv` — which is what H2 needs — is O(n³) and becomes untenable past
n ≈ 3 000. **For H2 that is fine and does not need fixing:** resistance is only
ever wanted between a node and its own structural dependents, so the
pseudoinverse is taken **per connected component**, and this graph's structural
components are 144 with a largest of 221 (sizes 221, 7, 5, 3, 3, 3, 2, 2, …;
only 12 have more than one node). Measured: **all 12 component pseudoinverses,
matrix construction included, take 3.02 ms**, of which the n=221 component is
2.45 ms. It stays cheap as long as the structural graph stays fragmented —
which §"Islands" says it emphatically does. If a future graph ever has one
giant structural component of 10 000 nodes, this becomes a real cost and would
need a sparse/approximate resistance estimator, at which point determinism
returns as an open question. **Unmeasured:** whether a component-local
pseudoinverse can be updated incrementally on an edge insert rather than
recomputed. Not needed at 3 ms.

**Lock discipline — MEASURED, satisfiable.** Both operators are pure functions
of the `(source, target, edge_type)` triples: no content, no embeddings, no
scores. The edge list here is 528 triples (~21 KiB), so the graph lock is held
only for that copy, and all algebra (0.5 ms per solve, 3 ms per component
`pinv`) runs outside it. Recall §6.4 is satisfied without a precomputed cache.
For reference, reading the *whole* graph out of SQLite — far more than the
operators need — takes 132 ms.

**Determinism — MEASURED, satisfied by construction.** Two independent runs
building the PPR operator, the heat kernel and the pseudoinverse hash
bit-identically: `e4aac064a469963e5f7ae315a21d91f04a7055505c05e5e9dbd143cf36289eb2`.
Every solver is a direct dense factorization (LAPACK `gesv`/`gesdd`, Padé
`expm`) — no iterative solver, no tolerance, no iteration cap, no RNG, so
issue #2's lesson has nothing to pin. **This guarantee is exactly what a sparse
iterative solver would give up**, which is the real reason to prefer the dense
path while n stays small.

---

## Recommendation

| hypothesis | outcome |
| --- | --- |
| **H1** — expansion as diffusion | **FALSIFIED. Shelve with numbers.** At matched directedness, diffusion agrees with the depth-2 cliff at Jaccard 0.994, disagrees on the set in 2/22 real queries (9%, meeting the falsifier), and the few disagreements are PageRank hub bias that *degrades* provenance precision. Three cliff inversions across 22 queries. |
| **H2** — blast radius as effective resistance | **UPHELD, pending operator judgement.** Survives its confound control (τ 0.840 vs `count_all`, warning-set Jaccard 0.500, 5 of 14 moving ≥3 places), and the reordering has a clear reading: it finds a load-bearing pillar the exclusivity filter structurally cannot see, and demotes a broad-but-shallow fan-out that warns today. |

**Recommendation: shelve H1, adapt H2, and fix graph connectivity first.**

1. **Shelve H1.** Do not replace fixed-depth traversal with diffusion. The
   principled-looking win is a direction artefact, and the honest version of the
   idea costs a dense solve per query to reproduce a cliff that is already in
   the right place. Revisit only if the graph's mean degree rises well above
   2.70 — the depth cliff bites when the depth-2 ball is *large*, and here its
   median is 1.
2. **Take the cheap question H1 surfaced instead:** should phase 2 follow
   in-edges? One function call, 2–4× the expansion, no new math. Needs a
   relevance judgement, which is a G-shaped task.
3. **Adapt H2, do not adopt it wholesale.** The finding that generalizes is not
   "use effective resistance", it is **`blast_radius`'s exclusivity filter is
   hiding load-bearing pillars** — a concept whose dependents each have other
   support is *more* load-bearing, not less, and today it scores lower. Two
   options, cheapest first:
   * report `count_all` alongside the exclusive count (τ 0.568 against it, no
     new math, control shows it explains part of the reordering); or
   * the conductance score, which adds a real and separable signal on top
     (τ 0.840, not 1.0) at the cost of a Laplacian pseudoinverse.
   Either way the *warning threshold* would need recalibrating: `MIN_BLAST_RADIUS
   = 5` is tuned to the exclusive count's scale.
4. **Fix connectivity before either.** 63% of `Logic` and 70% of `Observation`
   concepts are unreachable from the artifact web. That is worth more than any
   traversal change, and it is a `lambo_derive` API question (derived concepts
   have no way to declare a non-hierarchical edge), not a graph-algorithm one.

**Confidence.** H1's falsification: **high** — the control isolates the cause,
the effect size is large (0.994 vs 0.468), and it holds across three α, three
temperatures and both weightings. H2's upholding: **medium** — the control is
clean and the mechanism is understood, but n=9 in the warning set is small, no
concept in this snapshot is canonical so the warning never actually fires today,
and "a human judge endorses it" is by construction not something the spike can
certify. The islands finding: **high** — it is a census, not an inference.

---

## Files

| file | what |
| --- | --- |
| `lambo_graph.py` | snapshot loader + ported tokenizer/BM25/phase-1/phase-2/`blast_radius`; `python3 lambo_graph.py` self-checks the tokenizer against the pinned fixture |
| `step1_census.py` | sparsity, degrees, components, depth-2 ball → `out/census.json` |
| `step2_diffusion.py` | H1 equal-budget disagreement tables → `out/h1_rows.json`, `out/h1_summary.json` |
| `step2b_cliff.py` | H1 cliff inversions + readable disagreements → `out/h1_cliff.json` |
| `step3_resistance.py` | H2 resistance/conductance rankings → `out/h2.json` |
| `step3b_control.py` | H2 confound control → `out/h2_control.json` |
| `step4_islands.py` | islands and the provenance spine → `out/islands.json` |
| `step5_cost.py` | cost, lock discipline, determinism → `out/cost.json` |
| `snapshot.db` | read-only copy of the dogfood store (+ `-wal`/`-shm`) — **not committed** |
| `calls.jsonl` | copy of the dogfood call ledger, source of the 22 real queries — **not committed** |

Run in order; steps 2b/3b read the JSON their predecessors write.

### Reproducing

The two data copies and `out/h1_rows.json` (~1 MB of per-query node-id lists) are
gitignored: they are snapshots of `~/lambo-dogfood/`, which is not repo state. To
re-run, re-take them first:

```sh
cd spikes/g3-laplacian
cp ~/lambo-dogfood/lambo-dev.db     snapshot.db
cp ~/lambo-dogfood/lambo-dev.db-wal snapshot.db-wal   # keeps un-checkpointed writes
cp ~/lambo-dogfood/lambo-dev.db-shm snapshot.db-shm
cp ~/lambo-dogfood/calls.jsonl      calls.jsonl
for s in lambo_graph step1_census step2_diffusion step2b_cliff \
         step3_resistance step3b_control step4_islands step5_cost; do
  python3 $s.py
done
```

`step5_cost.py` takes a few minutes (the n=10 000 dense pseudoinverse alone is
~173 s); every other step is seconds. Numbers here were taken against the
snapshot at 2026-08-21 09:49 — a later snapshot will differ, since the graph
grows with every dogfood session.

Requires `numpy`, `scipy` and `nltk` (only for the Snowball English stemmer,
which is what `rust-stemmers`' `Algorithm::English` is; the fixture check
verifies the match).

**Determinism.** Every solver is a direct dense factorization (LAPACK
`gesv`/`gesdd`, Padé `expm`). No iterative solver, no tolerance, no RNG — the
one `default_rng` in the tree is `step5_cost.py`'s synthetic scaling graph,
seeded `20260821`. Issue #2's lesson is satisfied by construction rather than by
pinning.
