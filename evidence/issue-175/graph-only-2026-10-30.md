# #175: fresh graph-only evidence check, 2026-10-30

**Result: neither tested propagation arm recovered a held-out missing answer facet, and both displaced known direct support. Keep production behavior unchanged.** This follow-up fixes the first pilot's absence of labelled graph-only answers: six fresh challenge questions have seven known supporting facets absent from direct retrieval but reachable through the existing directed graph.

The held-out set has **two challenge questions with three missing facets**, plus 30 direct-answer questions and two synthetic no-answer controls. This is a small mechanism check, not a population effectiveness estimate.

| Arm | Dev-selected damping / gain | Held-out missing facets rendered | Held-out direct-control facet coverage |
|---|---:|---:|---:|
| A: zero graph relevance | — / 0 | 0/3 | 56.7% |
| B: damped propagation | 0.75 / 32 | 0/3 | 26.7% |
| C: sender-degree normalization | 0.5 / 16 | 0/3 | 53.3% |

Coverage counts only fully rendered known support. Direct-control coverage is the mean fraction of known facets covered per question; it is not the fraction of questions answered correctly. Other candidates remain unjudged. Machine-readable data, all 49 development settings, paired topic intervals and input/source fingerprints are in [`graph-only-2026-10-30.json`](graph-only-2026-10-30.json).

## What was changed in the evidence

The [first pilot](pilot-2026-10-30.md) had 80 answerable questions whose known support was already in phase 1. Its low-gain selected arms tied baseline; it could not test recovery of missing evidence.

This run uses the same frozen Lambo-only snapshot and real Candle Metal retrieval, with **fresh questions and previously unlabelled support for the missing facets**. Snapshot SHA-256 is `5499e0890c480304398a0ffcd80bbfba64f6aba758e11a2d5214d6a4f08db258`; clock is `2026-10-30T09:14:26Z`. The Rust experiment implementation is exactly commit `88216d2080cc3e629097a0948e29a173341686f0`. No graph edge, candidate generator, fusion rule, direct relevance or runtime setting changed.

1. From substantive existing links, author 60 relational questions with expected facts before pooling retrieval. Four sampled source pairs were rejected before authoring because their association did not yield a coherent question. Of the 60, two lacked the exact nominated supporting record in phase 1. Reviewing alternatives showed that one already had equivalent direct support.
2. Before seeing any propagation outcome, author a second batch of 24 multi-facet questions from different anchors/targets. This batch responds to low baseline headroom, not to any arm's wins or losses. No question wording was rewritten after its retrieval screen. Keep both batches in the denominator.
3. Audit every labelled facet/source mapping, credit exact duplicates and reviewed equivalent answers, then freeze all labels and semantic topic groups. Nine questions provisionally missed a nominated support record; three ceased being graph-only questions after reviewing complete direct alternatives. One additional facet also had a direct equivalent. **Final result: six graph-only questions, seven missing facets, and 78 direct-answer controls among 84 authored answerable questions.**
4. Add four explicitly synthetic absent-answer controls. Verify the complete frozen dataset again with production pooling and an identical 88-query vector cache. No graph-only facet is inferred merely because a node is a neighbour.

The frozen query/label file hashes to `aeef24b71b7cdd5a227486bc3c3687408bfae657cb7ca0a88d1f086336affd9c`. Raw texts, questions, node IDs, qrels, vectors, paths and per-query results remain outside Git. The aggregate report fingerprints the private audit, pools, settings and runs.

### Headroom and split

| Split | Graph-only questions / missing facets | Direct-answer questions | No-answer controls |
|---|---:|---:|---:|
| Development | 4 / 4 | 48 | 2 |
| Held-out | 2 / 3 | 30 | 2 |

There are four challenge topic groups: write-queue/accounting and citation/canonization on development; caller identity and proxy recovery on held-out. Related queue questions stay together. No semantic topic or known positive node crosses the split; the full set has 14 development and 11 held-out groups including control topics. These grouping checks do not make the shared graph neighbourhoods independent.

Every missing facet has a private shortest-path witness from an actual phase-1 seed. **All seven witnesses are one hop.** They are absent from the direct seed set and present in BFS-required membership; siblings/forced companions alone do not qualify. This run supplies real missing-evidence headroom, but no two-hop-only answer case. The source-informed sample and baseline screening strongly enrich graph opportunities: 6/84 is a screening yield for these authored questions, not production prevalence.

The new `graph_screen.py` checks labelled headroom against production pools, including equivalent direct supports. It refuses a frozen `graph_only_facets` declaration that disagrees with the actual pool. It emits aggregate counts and an optional private per-query key; it does not generate relevance labels.

## Controlled run and selection

The three arms use the existing directed concept traversal and unchanged BFS membership, phase-1 inputs, daemon scores, max fusion, warning/companion handling and token fitting. Each query has depth 2, top-k 10 and a 1,000-token context budget. Propagation remains the test-only two-hop implementation from the first pilot: nonnegative sum-one seeds, deterministic accumulation, unique traversable neighbours, then per-arm/query graph-only max normalization and gain. C divides each sender's contribution by its full traversable outgoing degree.

The **same 49-setting development grid** was fixed before this run: zero, plus B/C at damping `{0.25, 0.5, 0.75}` and gain `{0.25, 0.5, 1, 2, 4, 8, 16, 32}`. The predeclared primary objective is missing-facet coverage on development challenge questions. Ties favor overall known-support preservation, overall known hit@10, lower gain, then damping nearest 0.5. This deliberately tests whether any recovery is possible while exposing its cost to direct answers; it is not the issue's adoption objective.

The immutable development selection was written **before constructing and running held-out settings**. Held-out ran only A and the two selected configurations. There was no held-out parameter search, label change or query rewrite.

| Arm | Development missing facets rendered | Development direct-control facet coverage |
|---|---:|---:|
| A | 0/4 | 64.6% |
| Selected B | 1/4 | 39.6% |
| Selected C | 1/4 | 58.3% |

B and C recovered different development facets. That shows the separate relevance map can surface a labelled graph-only fact through the actual renderer. Each gain came with loss of known direct support; neither recovered a held-out missing facet.

### Where held-out recovery failed

The known supports for the three missing facets were absent from the selected hit list in all held-out arms. **They failed ranking before token fitting**; truncation was not the cause for these supports. Propagation did render other graph-only records in the challenge questions (nine with B, three with C), but none carried a labelled missing facet. Additional known records rendered sometimes repeated an already-direct facet rather than supply the missing one.

Across all 32 held-out answerable questions, macro known-facet coverage is 54.7% for A, 26.6% for B and 51.6% for C. Census-defined hub share is 0%, 6.0% and 0% respectively. Reduced hub share alone does not establish useful evidence recovery. Both synthetic held-out no-answer controls still receive context in all arms; no abstention claim follows.

Paired bootstrap results use semantic topic clusters, 2,000 draws and seed 175, separately for missing-facet recovery, direct controls and overall known coverage. The missing-facet delta is observed zero for every held-out question, so its finite-sample interval is `[0, 0]`. With **only two held-out challenge topics**, this says nothing about a tight population bound. Complete precision/recall, final-answer accuracy and end-to-end release p50/p99 remain unmeasured. Single debug component timings are not adoption latency evidence.

## Decision and remaining scope

The fresh set removes the first pilot's zero-headroom limitation and produces a negative held-out result for these tested settings. Sender-degree normalization preserves direct support better than unnormalized damping here, but it supplies no held-out missing-facet recovery and still lowers known coverage. **Do not expose or enable a runtime/default propagation setting from this evidence.**

Keep #175 open for review of the negative result and any further experiment proposal. Six single-agent challenge questions across four groups, all with one-hop support, do not settle propagation generally. Any larger evaluation needs independently reviewed real graph-only questions, two-hop-only and relevant-hub cases, complete candidate judgements and latency measurements. These inspected held-out questions cannot be reused as fresh confirmation. No default flip, merge or new graph construction is part of this run.

## Reproduction and validation

Use the private frozen inputs and operator command from the [first report](pilot-2026-10-30.md#reproducing-privately). For the new pre-arm audit and objective:

```sh
python3 scripts/eval/graph_screen.py \
  --labels /private/research/queries-frozen.json \
  --pool /private/research/pool-frozen.json \
  --out /private/research/screen.json --key /private/research/screen-key.json

python3 scripts/eval/graph_compare.py \
  --labels /private/research/queries-dev.json --run /private/research/dev-run.json \
  --split dev --out /private/research/dev-summary.json \
  --select /private/research/selection.json \
  --selection-family graph_only_challenge \
  --selection-metric known_graph_only_facet_coverage
```

Freeze the selected configurations, then run and score held-out without `--select`. The scanner requires exact label/snapshot hashes and complete query membership; the scorer refuses held-out selection and verifies identical query sets across settings. Private keys must remain outside the repository and refuse overwrites.

Validation: **27 Python tests** pass, including equivalent direct support, real BFS bounds, declared-facet consistency, focus metrics, conservative tie-breaking, split/hash checks and private-key refusal. Real Metal operator runs verify production-frame parity for all 88 pooled questions, 49 settings on 54 development questions, and three settings on 34 held-out questions. Snapshot bytes, input labels and graph serialization are unchanged. Rust source and structural/context goldens are unchanged from the already validated 2,903-test pilot commit; that commit's CI completed successfully.
