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
