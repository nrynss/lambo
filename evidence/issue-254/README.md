# A layer floor that rises with the layer's size

[#254](https://github.com/nrynss/lambo/issues/254) showed that a layer's
fixed cosine floor admits a hit for more unrelated queries as the layer
grows ([`evidence/layer-floor-size/`](../layer-floor-size/README.md)). Its
option 2 is a default floor that rises with the layer's size, so that the
admission rate for unrelated queries stays where it was at the size the
floor was measured on. This measures whether such a floor meets the issue's
acceptance criteria.

**Result: it does not, and no size-aware floor ships.** A floor derived
from each contract's measured null distribution does hold admission roughly
level on the labelled set, as far as that set can test (96 concepts). It
does so by rejecting hits that are labelled relevant: under BGE-M3 the fixed
0.50 floor already rejects 17% of the held-out grade-2 pairs, and the rule
rejects 33% at 32 concepts, 45% at 100, 76% at 1,024 and 83% at 2,048. On
real queries against an unrelated stand-in layer the same floor does not
hold admission level either: it pushes it towards zero. What ships instead
is a warning when a layer on its default floor holds more concepts than the
floor was measured on (option 1).

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

`N0` is the size each floor records (`layer_floor_measured_concepts`): 12
for the BGE-M3 reference, 100 for the built-in rows. Those two are not
consistent with each other: the rows' floors are quantile-matched to
BGE-M3's 0.50, which was chosen on a 12-concept layer. So
[`floor-rule.json`](floor-rule.json) also carries every row at `N0 = 12`
(`common12`). Under BGE-M3 the two anchors are the same.

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

The rule does not hold the rate level here. It falls from 15 to 37% (A) and
4 to 18% (B) at 8 concepts to 0 to 3% at 1,024 and above. The cause is that
the rule's null is the labelled set's, where a query and an irrelevant
concept are still about the same project: 3.05% of those pairs reach 0.50,
against 2.3% (A) and 0.77% (B) of cross-project pairs
(`evidence/layer-floor-size/null-pairs.json`). A floor scaled from the
in-project null rises faster than an unrelated layer needs.

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

This is not acceptable at any size above the measured one. The first step,
to 32 concepts, already doubles the held-out loss (17% to 33%), and at the
sizes where the fixed floor has stopped discriminating (a few hundred
concepts) the rule rejects about half of the relevant pairs or more. The
reason is in the two distributions: the largest irrelevant cosine on dev is
0.711 (0.749 held-out), and the median grade-2 cosine is 0.61 (0.58
held-out). They overlap, so no
absolute floor separates them, and a floor high enough to keep a large
layer quiet is above most true hits.

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

Cells are "uncorrected / corrected" again. B's uncorrected floor rests on 2
of 53 queries (its 3.8% at 8 concepts) and is not stable. Even the lowest
figure at 2,048 concepts loses 45% of the relevant pairs. Such a floor also
could not ship: it is fitted to one operator's private queries, not derived
from committed data.

## 3. Goldens

None moved. No floor, threshold, score or ranking weight changes in this
branch, so every recall golden and test is byte-identical. The only code
change is the warning (below) and the `lambo stats` line, which gains text
only for a calibrated session holding more concepts than the measured size.

## What ships

`lambo serve` logs one line when it attaches a layer's session, if the
layer is on its default floor and holds more concepts than that floor was
measured on (`layer_floor_size_warning` in `src/embed/calibration.rs`), and
`lambo stats` states the same comparison for a session. A floor set with
`[[serve.layer]] min_similarity` is the operator's own and is not warned
of. The floor itself is unchanged.

Per-query standardisation
([#256](https://github.com/nrynss/lambo/issues/256), with abstention in
[#174](https://github.com/nrynss/lambo/issues/174)) compares a hit with
that query's own score distribution over the layer instead of an absolute
bar. This evidence does not test it. It does show why an absolute bar, at
any height, cannot do the job.

## Limits

- **The labelled set stops at 100 concepts.** Admission on it is measured
  to 96. Everything above that is either the rule's own arithmetic or the
  real-query check.
- **Tail resolution.** The dev null has 23,707 pairs from 239 queries and
  100 concepts. The floor at 2,048 concepts rests on 5 of them and the one
  at 1,024 on 9, so those two floors are uncertain by a few hundredths; at
  256 it rests on 35. No tail is fitted and nothing is extrapolated past
  2,048. The conclusion does not depend on the last rows: the loss is
  already 33% at 32 concepts, where the floor rests on 275 pairs.
- **Null pairs are not independent.** They share queries and concepts, so
  the effective sample is smaller than 23,707, and Proposition 1's
  independence does not hold exactly.
- **The labelled null is in-project.** Every concept and most queries are
  about one project, so it is heavier-tailed than a cross-project null.
  The set has no cross-project text (its README, "Known biases").
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
library; its random draws are seeded. `real_queries.py` needs the private
database (table `e2`, built as `evidence/layer-floor-size/method.sql` step 3
describes) and the `duckdb` CLI, and reads it read-only:

```sh
python3 evidence/issue-254/real_queries.py --db <analysis.duckdb> \
    --project-a <session> --project-b <session>
```
