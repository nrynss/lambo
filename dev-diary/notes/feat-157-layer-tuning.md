# #157: general-layer admission and measured defaults

Retain weight **0.50**, raise `min_similarity` from **0.40 to 0.50**, and
require **absolute vector cosine** for ordinary layer hits. Keywords no
longer bypass that floor. Compatible anchored dependency answers still
pass. Weight and floor remain configurable through `[[serve.layer]]`; the
primary session has no layer floor. No live configuration or writer was
changed.

The issue originally limited the implementation to default changes. The
owner explicitly expanded it to test an absolute-relevance admission rule
after the defaults-only sweep demonstrated the tradeoff below. The merge
still interleaves admitted hits by `weight / original rank`, with existing
dedupe, forced-hit and token-budget rules.

## Method and disclosure

Measured 2026-10-13 from main `0ff27103`, with a fresh SQLite online backup
of the dogfood rig, then an isolated scratch copy. Current main required a
schema migration; `lambo provision` migrated only that copy. The existing
reviewed labels cover 6,791 records; the fresh backup added 795 records
classified provisionally for calibration. Sixteen protected-origin
placements were corrected using origin metadata after the split's hard
provenance guard refused an origin replica. Nothing waives that guard.
There are 787 low-confidence records: this is **not a reviewed cut-over
plan**. Existing audited general provenance still requires owner review.

BGE-M3, candle on Metal/Accelerate, 1024 dimensions, Hugging Face revision
`5617a9f61b028005a4858fdac845db406aefb181`. `lambo session verify` measured
SQLite-holder behaviour, with top_k=5, max_tokens=500, traversal_depth=2,
and fixed clock `2026-10-13T12:41:49.477Z`. Source and split inputs are
read-only during replay. The general layer contains 12 concepts.

The original 37 project queries cover six consuming sessions. For positive
probes, the original six general-rule queries were also asked from each of
those sessions (36 probes). A matching rule means the first origin returned
by the unlayered general answer to that same query; it is a retrieval
reference, not independent human relevance annotation. Probes are repeated
across sessions, not 36 independent topics.

Every verify invocation used `--aggregate-only endor`. No Endor queries,
text, or concepts were used as probes or relevance examples. No private
query strings, concept contents, origin/target ids, or raw reports are
published. Committed CSVs contain aggregates only:

- [Full 30-configuration sweep](../../evidence/157-layer-tuning/sweep.csv).
- [Baseline and selected per-project counts](../../evidence/157-layer-tuning/projects.csv).

## Results

Project recall@5 is the verifier's micro retention of in-scope pre-split
hits, not a human-judged relevance score. General share counts only the
first five ranks: forced hits beyond five are reported separately. Lost
and gained use all returned ranks, as the verifier defines them.

| Admission | Weight | Floor | Project recall@5 | Queries with general at k=5 | General share at k=5 | Lost / gained | Matching rule at k=5 | Matching rule in context |
|---|---:|---:|---:|---:|---:|---:|---:|---:|
| Keyword bypass (old) | .50 | .40 | .87 | 36/37 | 19.46% | 13 / 133 | 35/36 | 22/36 |
| Keyword bypass | .50 | .50 | .89 | 26/37 | 14.05% | 11 / 121 | 35/36 | 22/36 |
| Keyword bypass | .20 | .50 | .91 | 0/37 | 0% | 8 / 118 | 0/36 | 0/36 |
| Absolute vector | .50 | .45 | .89 | 16/37 | 8.65% | 11 / 117 | 35/36 | 22/36 |
| **Absolute vector (selected)** | **.50** | **.50** | **.91** | **3/37** | **1.62%** | **9 / 112** | **35/36** | **22/36** |
| Absolute vector | .33 | .50 | .91 | 2/37 | 1.08% | 9 / 112 | 35/36 | 18/36 |
| Absolute vector | .50 | .55 | .91 | 0/37 | 0% | 9 / 110 | 17/36 | 13/36 |
| Absolute vector | .50 | .60 | .91 | 0/37 | 0% | 9 / 110 | 6/36 | 5/36 |

The defaults-only grid was weights [.50, .33, .25, .20] x floors
[.40, .50, .60, .70, .80]. With weight >= .25, floors >= .50 plateaued
at 26/37 project queries: keyword evidence bypassed even a .80 floor.
Weight .20 suppressed every positive probe as well. The absolute-vector
grid was weights [.50, .33] x floors [.40, .45, .50, .55, .60].

At the selected operating point, general contributes one top-five hit in
three queries and none in 34; mean 0.081 hits/query, down from 0.973.
All-rank general hits fall 56 -> 4 (one selected hit is past rank five).
The selected three ordinary general hits replace three primary slots
relative to the low-weight control. Their admission reflects cosine >= .50,
not a claim that those three were independently judged relevant.
Foreign-primary hits assigned to Endor remain zero throughout the sweep.

Per-project recall@5 (baseline -> selected): ajilamu .741 -> .852,
general-usage 1 -> 1, lambo .871 -> .903, lambo-paper 1 -> .889,
rustydocs .941 -> .941, vimanam .933 -> 1. The lambo-paper regression is
one general-assigned hit rejected by the stricter cutoff. The other eight
selected losses are primary hits (ajilamu four, lambo three, rustydocs one).
They are not all attributable to general displacement: the low-weight
control also loses primary hits after splitting.

## Why this operating point

The absolute .50 floor substantially reduces general intrusion while
retaining the baseline positive-probe results. Lower weight buys little
additional exclusion and worsens rendered visibility. Raising the floor
to .55 removes all ordinary general hits on the project set but loses many
rule probes. This is a measured tradeoff, not proof of perfect relevance
or a universal cosine calibration. #87 still owns per-embedder calibration.

The 500-token budget already hides 14/36 matching-rule probes in the old
baseline and the selected setting; returned hits alone would obscure that.
The setting preserves, rather than solves, this budget limitation. The
small general layer, repeated positive queries, provisional fresh labels,
and the one rejected general hit limit the evidence. Review the delta and
that loss before cut-over; rerun with the owner's final labels and serving
budget. The live rig remains untouched.

## Reproduction

Keep all inputs and reports in a private directory. On a fresh completed
backup copy, classify with the reviewed labels plus explicitly identified
delta labels, then split. Use the intended serving TOML with SQLite store
kind, matching BGE-M3 contract, pinned project and general sessions, and
`layers = ["general"]`. For every weight/floor pair, write a separate config
and run (paths below are placeholders):

```sh
lambo --config private/variant.toml session verify \
  --session lambo-dev --database private/source.db \
  --plan private/plan.json --split-database private/split.db \
  --queries private/project-and-rule-probes.json \
  --aggregate-only endor --top-k 5 --max-tokens 500 \
  --traversal-depth 2 --at '<fixed source clock>' \
  --report private/variant-report.json
```

The keyword-bypass baseline uses main `0ff27103`; the absolute-vector trial
uses this change. A configuration cannot restore the old keyword bypass.
Aggregate project rows separately from appended positive probes. Count
`rank <= 5` for recall@5 and general share, all ranks for lost/gained, and
`included_in_context` for rendered rule visibility. Keep raw reports private.

## Validation

- `cargo test --features store-sqlite --all-targets`: 2,492 passed,
  zero failed, seven ignored.
- `cargo clippy --features store-sqlite --all-targets -- -D warnings`,
  `cargo check --no-default-features --features store-sqlite,embed-fixture`,
  formatting, documentation mirrors and diff whitespace checks passed.
- Synthetic coverage checks the inclusive cosine boundary, keyword-only
  and weak-keyword rejection (including a forced warning), custom floors,
  unchanged primary keyword recall, anchored traversal, dedupe, access
  attribution, the single query embed, MCP serving and replay/holder parity.
- Public aggregate files were checked against the private source ids and
  query strings; neither is present.
- Mutation checks: restoring the keyword bypass fails the absolute-floor
  regression; restoring default 0.40 fails the tuned-defaults regression.
  Restored implementation: all 23 layered merge tests pass.
