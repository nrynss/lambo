# Fixed cosine thresholds against a growing store

Two of Lambo's fixed cosine thresholds are applied to the best match over a
store that grows: the layer admission floor (0.50 for BGE-M3) and the
semantic merge threshold (0.85). arXiv:2610.34507, raised on
[#174](https://github.com/nrynss/lambo/issues/174), shows that a fixed
threshold on a best-of-N score fires more often as N grows. This measures
whether that happens here.

**Result:** it does for the layer floor
([#254](https://github.com/nrynss/lambo/issues/254)). It was not shown for
the merge threshold, where the first measurement was misleading and the
corrected one shows no clear growth.

- **When:** 2026-11-18.
- **Machine and build:** Apple M3 Pro, lambo 0.12.0 release asset, candle
  BGE-M3 on Metal.
- **Store:** a backup of the Mac dogfood store (7,259 concepts in 8
  sessions), and for the merge side also the archived pre-split store (one
  session of 5,599 concepts). All vectors are the ones the store holds.
- **Queries:** 314 `lambo_recall` calls from the serve ledger, 31 October
  to 18 November 2026 (309 distinct texts).

These files hold aggregates only, with no memory content. Sessions are
labelled: **A** is this project's session, **A-archived** is the same
project in the archived single-session store, and **B** is another project.

## 1. Layer floor

**Question:** how often does a query admit a hit from a layer that holds
nothing relevant to it, as the layer grows?

**Method:** the queries of project A (132) and project B (53) are each
scored against concepts drawn at random from the other, unrelated project
sessions of the same store, as a stand-in for a layer of size N. Three
random orders per project; a layer of size N is the first N concepts of an
order. A query admits when its best cosine over the layer is at or above the
floor, which is the layer's admission rule (`src/recall/layered.rs`).

File: [`layer-admission.json`](layer-admission.json).

| Layer size | A, uncorrected | A, corrected | B, uncorrected | B, corrected |
|---:|---:|---:|---:|---:|
| 8 | 15.2% | 37.4% | 3.8% | 17.6% |
| 64 | 47.0% | 80.3% | 22.6% | 44.7% |
| 256 | 81.1% | 94.7% | 66.0% | 76.1% |
| 1,024 | 91.7% | 98.7% | 79.2% | 88.1% |
| 2,048 | 96.2% | 99.2% | 83.0% | 96.9% |

Share of queries admitting at least one hit at the 0.50 floor. "Corrected"
and "uncorrected" are two estimates, explained under
[Query vectors](#query-vectors), and neither is a bound on the true share.
The uncorrected column is one random order; the corrected one is the mean of
three.

At a floor of 0.55 the corrected shares are 22.0% (A) and 3.1% (B) at 64
concepts and 60.6% and 45.9% at 1,024. A higher fixed floor delays the
effect and does not remove it.

The 0.50 floor itself was chosen against a small layer: #157's sweep ran on
a `general` layer of 12 concepts
(`dev-diary/notes/feat-157-layer-tuning.md`).

**Why:** for one unrelated query and concept, the cosine has a median of
0.405 (A) and 0.375 (B), and 0.50 is near its 98th to 99th percentile: 2.3%
(A) and 0.77% (B) of pairs reach it uncorrected
([`null-pairs.json`](null-pairs.json)). A bar that one pair in a hundred
clears is cleared by some pair in most layers of a few hundred concepts.

**On the live rig:** the shared layer holds 35 concepts. In the same ledger
it contributed a hit that survived into the final result for 16 of 132
recalls in project A (12%) and 21 of 53 in project B (40%). That counts
hits that survived the layer weight (0.5) and the `top_k` cut, so it is not
the admission rate, and some of those hits are relevant.

## 2. Merge threshold

**Question:** does the share of new facts whose best existing match clears
0.85 grow with the size of the session?

Only fact-type concepts are counted (Logic, Observation, Constraint), since
those are what a derive writes. Matches within the same derive call are
excluded: a merge is tested against earlier calls, and concepts of one call
are near-duplicates of each other for the reason given under
[Same-call siblings](#same-call-siblings).

### First attempt: random subsets (misleading)

File: [`merge-random-subsample.json`](merge-random-subsample.json).

Each fact's best cosine to the concepts of other calls in a random subset of
its session. The share at or above 0.85 rises from 0.0 to 0.2% at 64
concepts to 6.0 to 6.8% at the full session, in all three sessions.

This does not describe what a writer faces. A new fact's near neighbours are
recent concepts from the same piece of work, and a random subset removes
most of them at small sizes. The growth is the subset filling back in.

### Corrected: arrival order

File: [`merge-arrival-order.json`](merge-arrival-order.json).

Each fact's best cosine to concepts of earlier calls only, grouped by how
many concepts its session held when it was written. A fact with no earlier
concept from another call has no match and counts as below the threshold
(18 of the 4,434 facts across the three sessions).

| Written when the session held | A | A-archived | B |
|---|---:|---:|---:|
| 1 to 500 | 1.9% | 1.9% | 4.4% |
| 501 to 1,000 | 1.4% | 0.5% | 2.5% |
| 1,001 to 2,000 | 4.6% | 4.1% | 3.1% |
| 2,001 to 4,000 | 3.9% | 1.9% | |
| over 4,000 | | 4.2% | |

Share of facts whose best earlier match is at or above 0.85. Each cell has
189 to 840 facts.

The share moves between 0.5% and 4.6% with no consistent direction: it is
higher later in A, mixed in A-archived, and highest at the start in B. These sessions do not
show the threshold firing more often as the session grows, up to 5,599
concepts. They do not rule out a slow rise either.

### What the matches above 0.85 are

In session A, 43 facts have a best earlier match at or above 0.85 (0.850 to
0.899). One reader (the agent that ran this, reading both texts cut to 230
characters) judged about 16 to restate the same fact and about 27 to be
related but distinct; several of the 27 are revisions that replace the older
statement. 33 of the 43 pairs have a `Semantic` edge in the store. This is a
single quick reading, not a labelled set, and no text is included here. The
overlap of paraphrases and related-but-distinct pairs at 0.85 is already
recorded in `evidence/mooshik-g-recall-calibration/`.

## Notes on the method

### Query vectors

Recall embeds a query bare. A concept is embedded with its call's text
(`context_text` in `src/graph/hybrid.rs`). No command prints a bare query
vector, so the queries were embedded by deriving each as a one-concept call
on a scratch serve ([`embed_queries.py`](embed_queries.py)), which adds that
framing.

File: [`query-vector-check.json`](query-vector-check.json). On the 627
(query, concept) pairs for which the ledger logged the server's own cosine,
the recomputed cosine is lower by 0.013 on average, with correlation 0.96.

- **Uncorrected** figures use the recomputed cosine as it is.
- **Corrected** figures apply the linear fit from those pairs
  (0.0438 + 0.9504 × cosine, RMSE 0.0195).

These are two sensitivity estimates, not an interval. The check and the fit
come from logged top hits, selected for a high server cosine. They say
nothing direct about how the framing moves the cosine of an unrelated pair
near the floor, which is what decides admission. The true share could lie
below both figures or above both. What both estimates share is the shape:
admission rises steeply with layer size. Bare query vectors, which
[#258](https://github.com/nrynss/lambo/issues/258) would provide, are needed
to settle the level.

The merge side needs no correction: it compares stored vectors with stored
vectors, as the writer does.

### Same-call siblings

Because each concept's vector carries its whole call's text, concepts
written in one derive call are close to each other. Within a session the
median nearest-neighbour cosine over all concepts is 0.84 to 0.91; with
same-call concepts excluded it is 0.78 to 0.80.

### Other limits

- The "unrelated" sessions are one operator's other projects, so some
  cross-project matches are relevant, and layer admission is overstated by
  that much.
- A real shared layer is curated, not a random draw of another project's
  concepts.
- Only the vector leg is measured. Keywords do not bypass the layer floor.
- One store, one embedder, one machine.

## Reproducing

[`export_vecs.py`](export_vecs.py) writes a store's vectors to JSONL.
[`embed_queries.py`](embed_queries.py) embeds query texts through a scratch
serve; run it only against a scratch store, since it writes.
[`method.sql`](method.sql) gives the DuckDB queries with placeholder session
names.
