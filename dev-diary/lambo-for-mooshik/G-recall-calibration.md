# G — Recall score calibration for real embedders

**Goal:** recall and hybrid-merge thresholds that mean something under a real embedder's
score distribution, not just the fixture's.

**Where this came from:** F's evidence capture (`evidence/mooshik-f-sqlite-bge/`), the
first run of the full recall stack under BGE-M3. The vector leg ranked every query
correctly; one correct ranking was then erased by the blend. This is not an F defect —
F's layer behaved exactly as specified — it is the first calibration signal from a real
embedder, and the reason the rig was stood up.

---

## The problem, measured

Recall's candidate gather gives every recent-interaction member a flat
[`RECENT_SCORE`] = 0.5 (`src/recall/candidates.rs:64`) and folds legs by `max`
(`candidates.rs:156`, `:212`). The comment at `candidates.rs:62` states the assumption:
vector and BM25 scores are *"typically > 0.5 for a match worth returning."*

That was true of the fixture embedder, which has no middle band: near pairs ≥ 0.85,
everything else near-orthogonal. BGE-M3 puts genuine cross-vocabulary matches at roughly
0.35–0.65. From the committed evidence, three zero-vocabulary-overlap queries:

| Query → concept | Cosine | Cleared the 0.5 floor? |
| --- | --- | --- |
| "database table for people signing up" → user schema | 0.5706 | yes, surfaced |
| "login token checking layer" → auth middleware | 0.5823 | yes, surfaced |
| "changes that do not break existing clients" → backward compat | 0.3991 (margin +0.0476) | **no — `max(0.5, 0.399)` tied all three concepts** |

The failure is a missed lift, never a wrong answer: a correct-but-weak hit loses its
ordering signal, it does not rank below a wrong one because of the floor. It bites
hardest in young sessions where everything is recent. Under D's event-time clock a
bootstrapped corpus makes "recent" a small slice, which shrinks but does not remove the
band.

**The same fixture geometry calibrated a second constant.** Hybrid derive merges an
incoming concept into an existing one at `semantic_match_threshold` = 0.85 default
(`src/graph/hybrid.rs:100`, "calibrated against" the fixture's near-pair floor). Under
BGE-M3, real paraphrases live well below 0.85, so a real embedder will *create
duplicates* where the fixture-tested path shows convergence — the same miscalibration on
the write side. G owns both or it will be re-discovered.

---

## G1 — Measure

No code changes. Against the local rig (llama-server + `bge-m3-q8_0.gguf` on
`127.0.0.1:8080`, the F evidence config `evidence/mooshik-f-sqlite-bge/lambo.bge-sqlite.toml`):

* Score distributions for true matches, paraphrases, related-but-distinct, and unrelated
  pairs under BGE-M3 — enough pairs to place the floor and the merge threshold inside a
  measured band, not a guessed one.
* Where the fixture's near/far contract (0.85 / far) sits relative to that band, so every
  fixture-calibrated test's assumption is stated once.
* The interaction cases: a true hit under the floor (the committed miss reproduces it), a
  paraphrase under the merge threshold (expect duplicate creation — capture it).

Output: a measurement note in this folder, in the shape of the F evidence README. The
numbers decide G2; G2 does not start on intuition.

**Depends on:** F (needs the SQLite vector leg; landed at `9fabff3`).

---

## G2 — Fix

Options, to be chosen by G1's numbers rather than in advance:

1. **Lower `RECENT_SCORE`** below the measured bottom of the true-match band. Smallest
   change; keeps `max` blending and its "a leg can only help" property.
2. **Blend additively** (or weighted) instead of by `max`, so a recency floor and a
   semantic score compose instead of masking each other. Bigger ordering change; every
   recall test and the demo's printed scores move.
3. **Per-embedder-kind calibration** — the contract already carries `kind`; constants
   could resolve per kind (fixture keeps today's values so no test churn; `bge_m3` and
   future `gemini` get measured ones). Most honest; most machinery.

Same decision for `semantic_match_threshold`, which need not pick the same option.

Constraints, whatever is chosen:

* Recall ordering is a global behaviour: before/after must be shown on the committed
  evidence queries **and** the fixture-based suite must keep passing without weakening
  any assertion. A changed constant that silently flips a fixture test proves the test
  was asserting the constant, not the behaviour — fix the test to assert the behaviour.
* The precision bias of hybrid merge is deliberate (`hybrid.rs:140`, "biased toward
  precision"). G2 may move the threshold; it may not silently trade the bias away —
  if recall@merge rises at a precision cost, the cost is measured and written down.
* SoloPolicy (C2) consumes recall behaviour downstream. If C lands first, re-run its
  boundary tests; if G lands first, note the constants C should calibrate against.

**Depends on:** G1.

---

## Done when

- [x] A measurement note records BGE-M3 score distributions for the four pair classes,
      with the floor and merge threshold placed against them
- [x] The committed miss (`0.3991` under a `0.5` floor) surfaces in recall after G2,
      shown on the same committed queries
- [x] Duplicate-creation under the merge threshold is measured before and after
- [x] Fixture-based tests pass unweakened, or any test that asserted a constant now
      asserts the behaviour, with the change explained
- [x] The chosen option and the rejected ones are recorded here with the numbers that
      decided it

## G1/G2 outcome (2026-08-19)

Evidence: [`evidence/mooshik-g-recall-calibration/`](../../evidence/mooshik-g-recall-calibration/).

`RECENT_SCORE` is now **0.35**. The deciding low watermark is the F durable-vector
miss at **0.3991**, not the fresh corpus's 0.4599 minimum. This retains max-merge's
monotonicity while surfacing the same query first after G2; additive blending was
rejected because it changes all phase-1 ordering to solve a floor problem.

`semantic_match_threshold` stays **0.85**. This is a calibration decision: the
measured paraphrase and related-distinct bands overlap. Lowering to 0.84 would rescue
only `user schema` / `user data model` (0.8495), while newly accepting three recorded
related-distinct pairs at 0.8069, 0.8152, and 0.8340. It also cannot cure the existing
0.8913 `reset password` / `forgot password` overlap. Under-merging is intentionally
safer, so the 0.8230 `register user` / `create account` pair creates duplicates both
before and after G2. Per-embedder constants cannot solve that score-only overlap.

The fixture suite retains its semantic behavior. The former recent-floor assumption
is now tested as behavior: the actual 0.3991 vector lift outranks unrelated recent
concepts, rather than asserting that recency is exactly 0.50. C2/SoloPolicy did not
land in this worktree, so there were no boundary tests to re-run; C should calibrate
against the 0.35 floor.

## G3 — Exploratory: the graph Laplacian for expansion and blast radius

**Status: EXPLORATORY, unscheduled** (added 2026-08-21, operator decision, from the J3
pause's math discussion). A spike, not a commitment: it produces measurements and a
recommendation, never a merged behaviour change on its own. Runs whenever an agent has
idle capacity after J closes; it blocks nothing and nothing blocks it — the diffusion
half works on edges alone, so it does not even wait for the embedding backfill.

**The two hypotheses, stated so the spike can falsify them:**

1. **Recall phase-2 expansion as diffusion, not fixed-depth traversal.** Today phase 2
   expands hits by graph traversal with a hard depth cliff (`traversal_depth`, default 2):
   a node at depth 2 counts fully, a node at depth 3 not at all, and one path counts the
   same as five. The principled alternative is diffusion over the concept graph's
   Laplacian — heat-kernel weighting or personalized PageRank seeded at the phase-1 hits —
   where relevance decays *continuously* with distance and *accumulates* over path
   multiplicity. Hypothesis: on the dogfood graph, diffusion ranks the genuinely related
   neighbourhood above the incidentally adjacent one in cases where fixed-depth cannot
   distinguish them. Falsifier: if diffusion's ranking disagrees with fixed-depth on
   fewer than ~1 in 10 real recalls, or disagrees only on ties, the cliff is harmless at
   session scale and the idea is shelved with numbers.
2. **Blast radius as effective resistance, not a count.** `blast_radius` today counts
   dependents; it cannot tell a dependent connected by five independent paths (deeply
   load-bearing) from one connected by a single chain (fragile linkage, cheap to verify by
   hand). Effective resistance between two nodes — computed from the Laplacian
   pseudoinverse — is exactly the quantity that knows the difference. Hypothesis: on the
   dogfood graph, resistance-ranked dependents reorder the warning-worthy set vs the
   count-ranked ones in a way a human judge endorses. Falsifier: if the two rankings agree
   on the dogfood graph's real cases, the count is sufficient at this scale.

**Method (all read-only against a copy of `~/lambo-dogfood/lambo-dev.db`):** export the
node/edge set; compute both scores offline (a ~200-line Python spike beside the
observability kit, NOT product code — n≈124 nodes, so dense pseudoinverse is trivial);
replay the ledger's real recall queries through both rankings; table the disagreements;
have the operator judge a sample. Costs to respect from the start: the graph mutates under
the daemon (spike works on a snapshot; product code would need incremental updates —
Laplacian solvers at session scale are cheap, but say so with a measured number); recall's
§6.4 lock discipline (diffusion must be computable outside the graph lock or precomputed);
and determinism (issue #2's lesson — no randomized solvers without pinned seeds).

**Relationship to the rest of the map:** independent of G1/G2's constants (this is about
*which nodes*, not *what score floor*). Feeds C's eviction-resistance thinking if
resistance proves informative (a concept's resistance profile is a canonization signal).
Post-Mooshik it is the natural growth path for recall at autobiography scale, where a
fixed depth-2 ball around every hit stops being cheap or meaningful.

**Done when (as an exploration):** the disagreement tables exist with real dogfood
queries; each hypothesis is upheld or falsified with numbers; the recommendation
(adopt / adapt / shelve) is recorded here with the evidence, and if "adopt", the product
design questions (incremental maintenance, lock discipline, determinism) each have a
measured answer rather than an assumption.

## G3 spike outcome (2026-08-21)

Spike: [`spikes/g3-laplacian/`](../../spikes/g3-laplacian/) — read-only, plain Python,
not a workspace member. Snapshot of `~/lambo-dogfood/lambo-dev.db` opened `immutable=1`;
the live store and the MCP session were never written or locked. All **22** real
`lambo_recall` calls in the dogfood ledger were replayed. The graph had grown well past
§G3's estimate: **386 concepts, 528 Concept↔Concept edges** (`Causal` 207,
`CoOccurrence` 202, `Dependency` 94, `Hierarchical` 25, `Semantic` 0). The ported
tokenizer passes `fixtures/canonicalization-cases.json` verbatim, so the replayed phase 1
is faithful rather than approximate.

Two semantics had to be pinned before any comparison was meaningful, and both reshape the
hypotheses:

* **`expand.rs` is a membership gate, not a ranking.** `assemble.rs` scores members
  `daemon×w_daemon + relevance×w_query`, and `relevance` is the *phase-1* score — exactly
  `0.0` for anything that arrived by expansion. Fixed-depth contributes no ordinal signal,
  so the honest comparison is set-vs-set at equal budget.
* **`blast_radius` is not a dependent count.** It counts *exclusive* dependents — those
  with no aged inbound structural edge from any other source. It already encodes a crude
  sole-support notion, which is what H2 had to be tested against.

### H1 — expansion as diffusion: **FALSIFIED, shelved with numbers**

| variant | mean Jaccard vs fixed-depth | set disagreements | same set, different order |
| --- | --- | --- | --- |
| `ppr_dir` α=0.5 (matched directedness) | **0.994** | **2 / 22 (9%)** | 18 |
| `ppr_dir` α=0.3 | 0.985 | 4 / 22 (18%) | 16 |
| `ppr_dir` α=0.15 | 0.970 | 7 / 22 (32%) | 13 |
| heat kernel t=0.5 / 1.0 / 2.0 (undirected) | 0.642 / 0.590 / 0.520 | 19 / 22 (86%) | 0 |
| `ppr_und` α=0.3 | 0.522 | 20 / 22 (91%) | 0 |
| **CONTROL: fixed-depth BFS, symmetrized** | **0.468** | **20 / 22 (91%)** | — |

The control carries the verdict. The undirected diffusions disagree on 19–20 of 22
queries — but so does plain fixed-depth BFS run on the symmetrized graph, at a lower
Jaccard still. **That disagreement is the symmetrization, not the diffusion.** Hold
directedness constant and diffusion reproduces the depth-2 cliff at Jaccard 0.994. Both
clauses of §G3's falsifier are met at once: 9% set disagreement (under ~1 in 10), and the
remaining 18 queries are the *same set* differing only in an order `assemble.rs` discards.

The cliff also sits where continuous decay would put it. Counting *inversions* — a node
at directed distance ≥3 scored above an admitted distance-1-or-2 node — gives **3 across
all 22 queries** at α=0.5 (8 at α=0.3, 14 at α=0.15), against 627 for the undirected heat
kernel. Type-weighted variants (`Dependency`=`Causal` 1.0 > `Hierarchical` 0.7 >
`CoOccurrence` 0.4, the only weighting available — every stored edge weight is 0.5 and
every `reinforcements` is 1, so the fields carry no information) land within ±0.03 Jaccard
and change no verdict.

Worse, the few genuine disagreements favour fixed-depth. At α=0.15 one high-degree
distance-3 hub is promoted on 6 of the 7 disagreeing queries, displacing precise level-1
provenance: `src/memory.rs`, `docs/reference/mcp.mdx`, `J-multi-client.md`, commit
`206f977`. That is PageRank popularity bias, and on a graph whose level-1 dependents *are*
the provenance the agent asked for, it is a regression.

### H2 — blast radius as effective resistance: **UPHELD, pending operator judgement**

`R_eff = L†_uu + L†_vv − 2L†_uv` on the symmetrized structural graph, pseudoinverse taken
**per connected component** (cross-component resistance is infinite, and a whole-graph
pinv would silently return a finite wrong number).

One structural fact nearly killed H2: of the 159 `(node, dependent)` pairs `blast_radius`
counts, **156 (98.1%) sit at `R_eff = 1/w = 2.0` exactly** — no multiplicity. That is
forced, not incidental: a dependent with multiple independent paths has another inbound
structural source, which is precisely what the exclusivity filter excludes. Resistance is
constant over the set the count looks at and cannot reorder it. Over all 325 structural
pairs it discriminates well (118 distinct values; 41 pairs at `R < 0.6`, 165 at `R = 2.0`).

The reordering therefore had to be attributed, since conductance sums over *all*
dependents while `blast_radius` filters to exclusive ones. At k=9, the set clearing the
Stage-3 bar `blast_radius > 5`:

| comparison | overlap | Jaccard |
| --- | --- | --- |
| `blast > 5` vs `count_all` top-9 — CONTROL | 7 / 9 | 0.636 |
| `blast > 5` vs `conductance` top-9 — H2 | 4 / 9 | 0.286 |
| `count_all` top-9 vs `conductance` top-9 — **H2 net** | 6 / 9 | **0.500** |

Kendall τ over the 65 concepts with a structural dependent: `count_exclusive`/`count_all`
+0.568, `count_exclusive`/`conductance` +0.436, `count_all`/`conductance` **+0.840**. H2
survives its control: at τ 0.840 (not 1.0) and 5 of the top 14 moving ≥3 places, the
resistance is neither redundant with the plain count nor a restatement of it. Three cases
carry it:

* **`Implemented J3 (writes acknowledged before the embedder)`** — `blast_radius` **4**,
  *below* the Stage-3 bar, so it warns nobody today; yet 13 structural dependents and the
  2nd-highest conductance, with `J-multi-client.md` (R=0.4222), `src/mcp/server.rs`
  (R=0.4726) and `src/memory.rs` (R=0.4951) the three most deeply-supported pairs in the
  graph. A load-bearing pillar the count structurally cannot see — every dependent is
  *also* depended on elsewhere, which is exactly why it is load-bearing.
* **`Stood up the dogfood rig`** — `blast_radius` **6**, warns today, conductance **3.00**,
  all six dependents pendant at `R = 2.0`. §G3's "fragile linkage, cheap to verify by
  hand", verbatim.
* **`Committed J2 stage 1 as 7f51bb6`** — the graph's *highest* blast radius (10) and top
  warning today, falls to conductance rank 11. Breadth of fan-out, not depth of support.

The falsifier does not trip. The one thing the spike cannot supply is §G3's own criterion
— "in a way a human judge endorses" — so the table in the spike README is the artefact to
judge.

### The side-finding worth more than either hypothesis

The graph **connects artifacts and strands reasoning**, and the provenance spine does not
fix it: adding the `Derives` co-parent projection collapses 41 traversable islands to only
**40** (giant component 266 → 271). So the orchestrator's islands do *not* join through
`record_action` provenance — they join through `Causal` (207 edges, largest single-type
component 135), which is already traversable. The giant component holds **92%** of
`Resource` and **85%** of `Entity` concepts but only 49% of `Constraint`, **37%** of
`Logic` and **30%** of `Observation`.

The mechanism is the two write APIs. `lambo_record_action` writes `Causal`/`Dependency`
edges to shared file and commit resources, which become hubs — that web *is* the giant
component. `lambo_derive` writes only **intra-call** `CoOccurrence` cliques, capped by
`max_cooccurrence_per_derive` (hence `CoOccurrence`'s largest component being 11), plus
`Hierarchical` edges from the rarely-used `parent_of` (25 in the whole graph). Derived
decisions, lessons and constraints therefore arrive edge-less and strand. **No phase-2
expansion algorithm can reach them from an artifact hit, because there is no path** — which
is also why diffusion cannot beat fixed-depth here: there is nothing to diffuse to.

Related sparsity numbers: density 0.00703, mean undirected degree 2.70, 179 of 386
concepts at degree exactly 1, 22/386 with an embedding, **0/386** with a stored
`blast_radius` (no canonization has ever completed on this store, so the pillar warning
has never actually fired). A single-seed directed depth-2 ball has median size **1**, and
**248 of 386** concepts expand to nothing at all — `expand.rs` follows out-edges only and
the graph is directed hub→sink.

### Recommendation: shelve H1, adapt H2, fix connectivity first

1. **Shelve H1.** Do not replace fixed-depth traversal with diffusion. The
   principled-looking win is a direction artefact; the honest version costs a solve per
   query to reproduce a cliff already in the right place. Revisit only if mean degree
   rises well above 2.70 — the cliff bites when the depth-2 ball is *large*, and its
   median here is 1.
2. **Take the cheaper question H1 surfaced.** Should phase 2 follow in-edges as well as
   out-edges? Symmetrizing the same BFS multiplies expansion 2–4× on every real query
   (20→67, 43→101, 1→19) for one function call and no new math. Whether that is *better*
   needs a relevance judgement, not a Laplacian — a G-shaped task, unmeasured here.
3. **Adapt H2, do not adopt it wholesale.** What generalizes is not "use effective
   resistance" but **the exclusivity filter is hiding load-bearing pillars**: a concept
   whose dependents each have other support is *more* load-bearing, and today it scores
   lower. Cheapest first: report `count_all` beside the exclusive count (τ 0.568 against
   it, no new math); or adopt conductance, which adds a separable signal on top (τ 0.840).
   Either way `MIN_BLAST_RADIUS = 5` needs recalibrating — it is tuned to the exclusive
   count's scale.
4. **Fix connectivity before either.** 63% of `Logic` and 70% of `Observation` concepts
   are unreachable from the artifact web. That is a `lambo_derive` API question — derived
   concepts have no way to declare a non-hierarchical edge — not a graph-algorithm one,
   and it outranks any traversal change.

### Product design questions, measured (they bear on H2's option 2)

* **Incremental maintenance — not needed at this scale.** One PPR seed-vector solve is
  **0.51 ms** at n=386, so there is no precompute to invalidate. H2's per-component
  pseudoinverses cost **3.02 ms for all 12 components together** (largest, n=221: 2.45 ms).
  Whole-graph dense `pinv` is O(n³) and would be untenable past n≈3 000 (3.85 s at 3 000,
  173 s at 10 000), but resistance is only ever wanted inside a component, and the
  structural graph is fragmented 144 ways. **Unmeasured:** whether a component-local
  pseudoinverse can be updated incrementally on edge insert rather than recomputed — moot
  at 3 ms, live if one giant structural component ever forms.
* **Lock discipline — satisfiable, no precomputed cache required.** Both operators are
  pure functions of the `(source, target, edge_type)` triples: no content, no embeddings,
  no scores. That is 528 triples (~21 KiB) here, so the graph lock is held for the copy
  only and all algebra runs outside it. §6.4 is satisfied.
* **Determinism — satisfied by construction.** Two independent runs of the PPR operator,
  heat kernel and pseudoinverse hash bit-identically
  (`e4aac064a469963e5f7ae315a21d91f04a7055505c05e5e9dbd143cf36289eb2`). Every solver is a
  direct dense factorization (LAPACK `gesv`/`gesdd`, Padé `expm`) — no iteration cap, no
  tolerance, no RNG, so issue #2's lesson has nothing to pin. A sparse iterative solver
  would give this guarantee up, which is the real argument for the dense path while n
  stays small.

**Confidence.** H1's falsification **high** — the control isolates the cause, the effect
size is large (0.994 vs 0.468), and it holds across three α, three temperatures and both
weightings. H2's upholding **medium** — clean control and understood mechanism, but the
warning set is n=9, no concept in the snapshot is canonical so the warning never fires
today, and the human-judge clause is by construction outside the spike. The islands
finding **high** — it is a census, not an inference.

**Feeds C** (per §G3's own note): resistance *is* informative, so a concept's conductance
profile is a live canonization/eviction-resistance signal — but C should read the
exclusivity caveat above before using `blast_radius` as a proxy for load-bearing-ness.
