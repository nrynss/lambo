# A layer floor that rises with the layer's size

[#254](https://github.com/nrynss/lambo/issues/254) showed that a layer's
fixed cosine floor admits a hit for more unrelated queries as the layer
grows ([`evidence/layer-floor-size/`](../layer-floor-size/README.md)). Its
option 2 is a default floor that rises with the layer's size, so that the
admission rate for unrelated queries stays where it was at the size the
floor was chosen on. This measures whether such a floor meets the issue's
acceptance criteria.

**Result: it does not, and no size-aware floor ships.** A floor derived
from each contract's measured null distribution does hold admission roughly
level on the labelled set, as far as that set can test (96 concepts). It
does so by rejecting hits that are labelled relevant. Under BGE-M3 the fixed
0.50 floor already rejects 15% of the dev grade-2 pairs and 17% of the
held-out ones. The rule rejects 37% (dev) and 45% (held-out) at 100
concepts, 71% and 76% at 1,024, and 76% and 83% at 2,048. At 32 concepts the
two splits disagree about the first step (20% dev, 33% held-out, with
overlapping intervals), so the conclusion rests on 100 concepts and above.
On real queries against an unrelated stand-in layer the same floor does not
hold admission level either: it pushes it towards zero. What ships instead
is a warning when a layer on its default floor holds more concepts with
vectors than the 12-concept layer that floor was chosen on (option 1).

- **When:** 2026-11-18.
- **Labelled data:** the #190 set (`fixtures/eval/`: 100 concepts, 368
  queries, 239 dev and 129 held-out), as measured by the committed
  `eval_measure --detail` runs: BGE-M3 on candle, CPU
  (`evidence/issue-221/runs/bge-m3-candle-cpu-detail.json`, the reference
  run), Qwen3 Embedding 4B at 2560 and 1024 dims (`evidence/issue-221/runs/`)
  and Elastic Jina v3 (`evidence/issue-237/runs/`). Self framing, as every
  calibration row was derived on.
- **Real queries:** the private analysis database behind
  `evidence/layer-floor-size/` (Apple M3 Pro, lambo 0.12.0, candle BGE-M3 on
  Metal): the logged `lambo_recall` queries of project A (132) and project B
  (53), each scored against concepts drawn from the other, unrelated project
  sessions.

These files hold aggregates only: no query text, no concept text, no
session name.

## The candidate rule

A fixed floor `f0` lets a share `p0` of single irrelevant (query, concept)
pairs through. arXiv:2610.34507, Proposition 1: if best-of-`N0` at `f0` was
acceptable, best-of-`N` keeps the same rate, for independent pairs, when the
per-pair share is `p_N = 1 - (1 - p0)^(N0 / N)`. The floor for a layer of
`N` concepts is then the `1 - p_N` quantile of the contract's null
distribution: the cosine of a query to a concept graded irrelevant, on the
dev split (23,707 pairs per contract). At `N <= N0` it is `f0`, so a layer
at or below the measured size would behave exactly as now.

No functional form is fitted. The floor at each size is an empirical
quantile, and the table says how many null pairs sit at or above it, which
is how far into the tail the data resolves.

Two anchors `N0` are measured. `recorded` is the size each floor's record
names (`layer_floor_measured_concepts`): 12 for the BGE-M3 reference, 100
for the built-in rows. Those are not the same kind of number. The 12 is the
layer BGE-M3's 0.50 was chosen on. The 100 is the corpus on which a row's
floor was quantile-matched to that 0.50: it says where the match was made,
not that the floor was checked on a layer of 100. So every floor's bar
traces to the 12-concept layer, and [`floor-rule.json`](floor-rule.json)
also carries every row at `N0 = 12` (`common12`), which is the anchor the
shipped warning uses for every contract. Under BGE-M3 the two anchors are
the same.

## 1. Admission for queries with nothing relevant in the layer

### Labelled set (up to 96 concepts)

Each held-out query is scored against random layers drawn from the concepts
graded irrelevant to it (200 draws per query and size, fixed seed). This is
measured directly, with no independence assumption. The set has 100
concepts, so it cannot answer for a layer above about 96.

BGE-M3, share of (query, layer) draws whose best cosine clears the floor:

| Queries | Layer size | Fixed 0.50 | Rule |
|---|---:|---:|---:|
| no-answer, held-out (32) | 12 | 24.0% | 24.0% |
| | 32 | 44.5% | 30.8% |
| | 64 | 58.5% | 30.5% |
| | 96 | 65.2% | 30.6% |
| all, held-out (129) | 12 | 27.0% | 27.0% |
| | 32 | 49.0% | 25.0% |
| | 64 | 65.2% | 23.0% |
| | 96 | 73.1% | 19.6% |

On dev, where the floors were derived, the rule gives 25.6%, 26.1%, 21.0%
and 23.8% for the no-answer queries (74) at the four sizes. So within the
set the rule keeps admission near its 12-concept level (24 to 31% held-out,
against 24 to 65% for the fixed floor), though it is not exact: pairs that
share a query are not independent, which is the assumption Proposition 1
makes.

The other contracts behave alike when anchored at 12 (`common12`): at 96
concepts the held-out no-answer share is 33.6% (Qwen 2560), 36.6% (Qwen
1024) and 39.7% (Jina), against 80.3%, 77.2% and 74.4% for their fixed
floors. Anchored at their recorded 100, the rule cannot differ from the
fixed floor at any size the set can build.

"No-answer" here means near-domain: all 100 concepts are about this
project, and most no-answer queries are too. It is not the cross-project
case a shared layer faces. That is the second check.

### Real queries, unrelated stand-in layer (8 to 2,048 concepts)

File: [`real-queries.json`](real-queries.json), from
[`real_queries.py`](real_queries.py). The rule's floors are the BGE-M3 ones
above, derived on the labelled set, not on these queries.

| Layer size | Rule floor | A fixed | A rule | B fixed | B rule |
|---:|---:|---:|---:|---:|---:|
| 8 | 0.5000 | 15.2% / 37.4% | 15.2% / 37.4% | 3.8% / 17.6% | 3.8% / 17.6% |
| 32 | 0.5333 | 34.1% / 63.9% | 15.2% / 24.0% | 15.1% / 33.3% | 3.8% / 9.4% |
| 64 | 0.5584 | 47.0% / 80.3% | 10.6% / 16.2% | 22.6% / 44.6% | 0.0% / 2.5% |
| 128 | 0.5813 | 59.9% / 88.1% | 5.3% / 7.1% | 41.5% / 55.4% | 0.0% / 0.0% |
| 256 | 0.6010 | 81.1% / 94.7% | 3.0% / 5.3% | 66.0% / 76.1% | 9.4% / 4.4% |
| 512 | 0.6174 | 88.6% / 97.2% | 1.5% / 4.5% | 73.6% / 85.5% | 5.7% / 7.5% |
| 1,024 | 0.6487 | 91.7% / 98.7% | 0.0% / 1.5% | 79.2% / 88.0% | 1.9% / 3.1% |
| 2,048 | 0.6673 | 96.2% / 99.2% | 1.5% / 1.3% | 83.0% / 96.9% | 0.0% / 0.0% |

Each cell is "uncorrected / corrected": two sensitivity estimates of the
share of queries admitting a hit, not an interval (see
[Limits](#limits)).

The rule does not hold the rate level here. Under the rule, each estimate
separately:

| | 8 concepts | 1,024 concepts | 2,048 concepts |
|---|---:|---:|---:|
| A, uncorrected | 15.2% | 0.0% | 1.5% |
| A, corrected | 37.4% | 1.5% | 1.3% |
| B, uncorrected | 3.8% | 1.9% | 0.0% |
| B, corrected | 17.6% | 3.1% | 0.0% |

**Why it over-corrects: the two nulls fall off at different rates above
0.50.** The rule's floors are quantiles of the labelled set's null. The
share of single pairs at or above 0.50 does not explain the result: it is
3.05% on the labelled dev null, and on the cross-project pairs 2.30% (A) and
0.77% (B) uncorrected but 5.65% (A) and 2.09% (B) corrected
(`evidence/layer-floor-size/null-pairs.json`). Project A's corrected null is
heavier at 0.50 than the labelled one, and the rule still drives A's
corrected admission to 1.5% at 1,024 concepts. What differs is the tail
above 0.50, measured here at each of the rule's floors (`null_tail` in
[`real-queries.json`](real-queries.json)):

| Rule floor for | Floor | Labelled dev null | A, uncorrected | A, corrected | B, uncorrected | B, corrected |
|---:|---:|---:|---:|---:|---:|---:|
| 12 | 0.5000 | 3.054% | 2.301% | 5.652% | 0.768% | 2.087% |
| 32 | 0.5333 | 1.160% | 0.413% | 1.079% | 0.140% | 0.357% |
| 64 | 0.5584 | 0.586% | 0.104% | 0.264% | 0.031% | 0.089% |
| 128 | 0.5813 | 0.291% | 0.033% | 0.070% | 0.010% | 0.020% |
| 256 | 0.6010 | 0.148% | 0.010% | 0.023% | 0.004% | 0.008% |
| 1,024 | 0.6487 | 0.038% | 0.001% | 0.002% | 0.000% | 0.001% |

Share of single (query, concept) pairs at or above the floor: 23,707
labelled dev pairs, 499,620 cross-project pairs for A and 249,524 for B. At
0.5333 the labelled null keeps 38% of the share it has at 0.50; the
cross-project pairs keep 17 to 19%, in both projects and under both
estimates. At 0.5813 it is 10% against 1.0 to 1.4%. So a floor that follows
the labelled null's quantiles climbs past the cross-project tail, and the
gap widens with size. The last rows rest on few pairs (at 0.6487: 9 labelled
pairs, 3 and 9 for A, 1 and 3 for B).

This says how the over-correction arises, not why the tails differ. The
labelled null is in-project (a query and an irrelevant concept are about the
same project), but the two populations also differ in who wrote the text, in
how the queries were embedded and in size, and nothing here separates those.
The correction was fitted on logged top hits, not on pairs in this tail.

## 2. Hits labelled relevant

A layer hit is admitted on its own cosine, so whether a relevant hit
survives depends only on the floor, not on the layer's size. For each floor
the rule gives, this counts the #190 set's grade-2 pairs (the concept
answers the query) below it. File: [`floor-rule.json`](floor-rule.json).

BGE-M3 (`N0 = 12`):

| Layer size | Floor | Dev null pairs at or above | Grade-2 lost, dev | Grade-2 lost, held-out | Held-out queries with no relevant hit admitted |
|---:|---:|---:|---:|---:|---:|
| 12 (fixed floor) | 0.5000 | 724 | 25/169 (15%) | 17/98 (17%) | 17/97 (18%) |
| 32 | 0.5333 | 275 | 34/169 (20%) | 32/98 (33%) | 31/97 (32%) |
| 64 | 0.5584 | 139 | 52/169 (31%) | 38/98 (39%) | 37/97 (38%) |
| 100 | 0.5724 | 89 | 62/169 (37%) | 44/98 (45%) | 43/97 (44%) |
| 128 | 0.5813 | 69 | 67/169 (40%) | 49/98 (50%) | 48/97 (49%) |
| 256 | 0.6010 | 35 | 79/169 (47%) | 57/98 (58%) | 56/97 (58%) |
| 512 | 0.6174 | 18 | 98/169 (58%) | 63/98 (64%) | 62/97 (64%) |
| 1,024 | 0.6487 | 9 | 120/169 (71%) | 74/98 (76%) | 73/97 (75%) |
| 2,048 | 0.6673 | 5 | 129/169 (76%) | 81/98 (83%) | 80/97 (82%) |

The last column counts answerable queries whose best relevant concept
(grade 1 or 2) is below the floor, so the layer would return nothing
relevant for them.

Sampling uncertainty, from a by-query bootstrap (1,000 resamples of whole
queries, seeded; `bootstrap_common12` in the JSON). The interval on a loss
resamples that split's queries with the floor held at its point estimate;
the interval on a floor resamples the dev queries behind the null. Neither
covers the labels or the embedder run:

| Layer size | Floor, 95% | Grade-2 lost, dev, 95% | Grade-2 lost, held-out, 95% |
|---:|---:|---:|---:|
| 12 (fixed floor) | 0.500 (not resampled) | 10 to 21% | 11 to 26% |
| 32 | 0.530 to 0.538 | 14 to 27% | 24 to 42% |
| 100 | 0.565 to 0.581 | 29 to 44% | 36 to 55% |
| 256 | 0.595 to 0.609 | 39 to 54% | 48 to 67% |
| 1,024 | 0.619 to 0.667 | 64 to 78% | 67 to 84% |
| 2,048 | 0.646 to 0.693 | 69 to 83% | 75 to 91% |

The first step, to 32 concepts, is not settled by this set. Held-out loses
33% against 17% at the fixed floor, but 15 of the 98 held-out pairs happen
to lie between 0.50 and 0.5333; on dev it is 9 of 169, a loss of 20% against
15%, and each split's interval at 32 overlaps its own interval at the fixed
floor. From 100 concepts the loss is clear on both splits: 37% (dev) and
45% (held-out) at 100, against 15% and 17%, and it rises with every step.
The rule rejects half or more of the grade-2 pairs from 128 concepts on
held-out and from 512 on dev. That is where the fixed floor has stopped
discriminating (a few hundred concepts), so the rule fails where it is
needed.

The reason is in the two distributions: the largest irrelevant cosine on
dev is 0.711 (0.749 held-out), and the median grade-2 cosine is 0.61 (0.58
held-out). They overlap, so on this set no absolute floor separates
relevant from irrelevant pairs, and a floor high enough to keep a large
layer quiet is above most of the hits labelled relevant. The population
matters: these grade-2 pairs are a query against a concept of its own
project, not a project's query against a shared layer's rule, which was not
measured ([Limits](#limits)).

The other contracts, held-out grade-2 pairs lost (of 98):

| Contract | Fixed floor | Lost at fixed | Rule at 1,024, `N0` = 100 | Rule at 2,048, `N0` = 100 | Rule at 100, `N0` = 12 | Rule at 1,024, `N0` = 12 |
|---|---:|---:|---:|---:|---:|---:|
| Qwen3 Embedding 4B, 2560 | 0.59 | 20 (20%) | 59 (60%) | 64 (65%) | 56 (57%) | 82 (84%) |
| Qwen3 Embedding 4B, 1024 | 0.60 | 20 (20%) | 56 (57%) | 64 (65%) | 54 (55%) | 82 (84%) |
| Elastic Jina v3, 1024 | 0.39 | 18 (18%) | 63 (64%) | 72 (73%) | 52 (53%) | 78 (80%) |

The gentler anchor (100) only delays the loss.

**A milder floor fitted to the real queries does not rescue it.** For each
size, [`real-queries.json`](real-queries.json) gives the floor that would
hold admission at its 8-concept level on those same queries (an in-sample
quantile, so the best case for any floor that depends on size alone), and
the held-out grade-2 share that floor loses:

| Layer size | A: floor | A: grade-2 lost | B: floor | B: grade-2 lost |
|---:|---:|---:|---:|---:|
| 32 | 0.538 / 0.519 | 34% / 24% | 0.555 / 0.515 | 38% / 22% |
| 128 | 0.552 / 0.542 | 38% / 35% | 0.555 / 0.538 | 38% / 34% |
| 512 | 0.575 / 0.562 | 46% / 41% | 0.647 / 0.574 | 76% / 46% |
| 2,048 | 0.594 / 0.573 | 55% / 45% | 0.647 / 0.611 | 76% / 62% |

Cells are "uncorrected / corrected" again. This table is not anchored like
the rule's: it holds admission at its 8-concept level, the smallest layer
the real-query data has, where the rule holds it at the 12-concept level.
Admission at 8 is lower than at 12, so these floors are somewhat higher, and
their losses somewhat larger, than a 12-anchored fit would give; the data
has no 12-concept column to say by how much. B's uncorrected floor rests on
2 of 53 queries (its 3.8% at 8 concepts) and is not stable. Even the lowest
figure at 2,048 concepts loses 45% of the relevant pairs. Such a floor also
could not ship: it is fitted to one operator's private queries, not derived
from committed data.

## 3. Goldens

None moved. No floor, threshold, score or ranking weight changes in this
branch, so every recall golden and test is byte-identical. The only code
change is the warning (below) and the wording of the `lambo stats`
`calibration=` line for a calibrated contract.

## What ships

`lambo serve` logs one line when it has attached a layer's session, if the
layer is on its default floor and holds more concepts with vectors than the
12-concept layer that floor was chosen on (`layer_floor_size_warning` in
`src/embed/calibration.rs`). The line gives both sizes and their ratio. It
does not offer a floor to set, because this evidence finds none: it says
that raising the floor rejects hits labelled relevant and that the layer's
weight still limits what an admitted hit displaces. The size is read once,
at attach. A floor set with `[[serve.layer]] min_similarity` is the
operator's own choice of that trade and is not warned of. The floor itself
is unchanged.

`lambo stats` (the CLI) states two facts on its `calibration=` line: the
size the contract's layer floor was chosen on, and how many concepts with
vectors the session holds. It does not warn, because a reader cannot know
whether the session is served as a layer or with which floor. The MCP
`lambo_stats` tool is unchanged.

Per-query standardisation is option 3 of
[#254](https://github.com/nrynss/lambo/issues/254) and part of the direction
of [#174](https://github.com/nrynss/lambo/issues/174) (abstention): compare
a hit with that query's own score distribution over the layer instead of an
absolute bar. [#256](https://github.com/nrynss/lambo/issues/256) is
narrower: it reports a confidence and the top per-leg scores on every
recall, with no threshold. This evidence tests neither. What it shows is
that on the #190 set no absolute bar separates grade-2 pairs from irrelevant
ones at a height that also keeps a large layer quiet. Those grade-2 pairs
are a query against its own project's concepts; whether a shared layer's
hits score like them was not measured, so for real layer hits this is an
expectation, not a result.

## Limits

- **The labelled set stops at 100 concepts.** Admission on it is measured
  to 96. Everything above that is either the rule's own arithmetic or the
  real-query check.
- **Tail resolution.** The dev null has 23,707 pairs from 239 queries and
  100 concepts. The floor at 2,048 concepts rests on 5 of them and the one
  at 1,024 on 9; at 256 it rests on 35. The by-query bootstrap puts the
  1,024 floor at 0.619 to 0.667 (point 0.649) and the 2,048 floor at 0.646
  to 0.693 (point 0.667), against 0.565 to 0.581 at 100. No tail is fitted
  and nothing is extrapolated past 2,048. The conclusion does not depend on
  the last rows: at 100 concepts, where the floor rests on 89 pairs, the
  loss is 37% (dev) and 45% (held-out).
- **Null pairs are not independent.** They share queries and concepts, so
  the effective sample is smaller than 23,707, and Proposition 1's
  independence does not hold exactly.
- **The labelled null is in-project.** Every concept and most queries are
  about one project, and the set has no cross-project text (its README,
  "Known biases"). Above 0.50 its tail falls off more slowly than the
  cross-project pairs of projects A and B do (section 1). That is two
  projects of one operator, with approximate query vectors; it is not shown
  that being in-project is the reason.
- **Relevance labels have one rater.** The set is v0: labelled by agents,
  with the owner's adjudication on flagged items, and no double-labelled
  overlap. Relevance was pooled once against BGE-M3, which favours it
  slightly.
- **Grade-2 pairs are query-to-own-project concepts**, not
  query-to-shared-rule pairs. Whether a real general rule scores like them
  against a project query was not measured.
- **Self framing.** Recall embeds a query bare; the rows were derived on
  the self framing and so is this. Under raw framing the BGE-M3 rule loses
  12, 30, 38, 76 and 85 of the 98 held-out grade-2 pairs at 12, 32, 100,
  1,024 and 2,048 concepts (`sensitivity_raw_framing` in the JSON).
- **Real-query vectors are approximate.** They were embedded through a
  derive and carry its framing; on logged pairs the recomputed cosine runs
  about 0.013 below the server's. "Uncorrected" uses it as it is and
  "corrected" applies a linear fit made on logged top hits, which does not
  bound the error for unrelated pairs near the floor. The true share could
  lie outside both figures. "Uncorrected" is one random order of the
  stand-in layer; "corrected" pools three.
- **The stand-in layer is not a real layer.** It is a random draw of one
  operator's other projects, so some matches are relevant and a curated
  shared layer would differ. There are no relevance labels on the real
  queries.
- **EmbeddingGemma 2 is not covered.** Its row's detail run is not
  committed (`evidence/issue-87-calibration/runs/` holds summaries only),
  so its null tail could not be read here.
- **Vector leg only**, one machine per run, one run per contract. No
  difference here is claimed to be within or beyond run-to-run variation.

## Reproducing

```sh
python3 evidence/issue-254/floor_rule.py          # writes floor-rule.json, prints the tables
python3 -m unittest discover -s evidence/issue-254 -p 'test_*.py'
```

`floor_rule.py` needs only the committed run files and the standard
library; its random draws and its bootstrap are seeded, and it takes about
40 seconds. `real_queries.py` needs the private database (tables `e2`, `q`
and `pool`, built as `evidence/layer-floor-size/method.sql` describes) and
the `duckdb` CLI, and reads it read-only:

```sh
python3 evidence/issue-254/real_queries.py --db <analysis.duckdb> \
    --project-a <session> --project-b <session>
```
