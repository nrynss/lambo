# Evaluation set baselines (#190)

Runs of `examples/eval_measure` over the set in `fixtures/eval/` (v0: agent
labels plus the owner's rulings on the five review items; see
`fixtures/eval/README.md`). No item awaits owner review.

## Runs

| File | Embedder | Where |
|---|---|---|
| `runs/bge-m3-candle-cpu.json` | candle BGE-M3, CPU, 1024 dims, `BAAI/bge-m3@5617a9f…`, weights sha256 `68440cc1b73b…` | Linux box, 8 vCPU, no accelerator; 2026-10-23; one cold run (no cached vectors); `embed_secs` times the export as the canary embeds it |

## What the baseline says

Two framings: **raw** (the texts as given, which is what `[embedder]
related_pairs` embeds) and **self** (`"{t} — {t}"`, what a one-concept
`lambo_derive` stores). Queries are always bare, as `lambo_recall` embeds
them. Cosine quantiles are nearest rank, as the canary computes them.

### Pairs (#87, #176, #181, #198)

| Framing | Label | n | p05 | p50 | p95 | p99 |
|---|---|---:|---:|---:|---:|---:|
| raw | paraphrase | 110 | 0.838 | 0.924 | 0.972 | 0.984 |
| raw | related_distinct | 173 | 0.549 | 0.837 | 0.980 | 0.989 |
| raw | unrelated | 120 | 0.342 | 0.453 | 0.534 | 0.542 |
| self | paraphrase | 110 | 0.834 | 0.921 | 0.972 | 0.985 |
| self | related_distinct | 173 | 0.562 | 0.823 | 0.975 | 0.991 |
| self | unrelated | 120 | 0.345 | 0.461 | 0.526 | 0.538 |

- **#87.** `related_pairs` pointed at `fixtures/eval/related_pairs.json`
  (173 pairs) derives a threshold of **0.989** (p99). It meets the count
  rule and clears the canary's random-pair floor (p99 0.524)
  (`threshold_rules_met`). Whether the canary gets to apply it depends on
  embedding the file inside its timeout, which on this box it does
  not (`within_canary_timeout`; see below). At 0.989 only 1 of 110
  paraphrases (0.9%) would still merge: on this set, the p99 rule turns
  semantic merging almost off.
- **The default 0.85** merges 35% (raw) / 43% (self) of the related-distinct
  pairs and keeps 91% / 87% of the paraphrases.
- **Without the identifier family** (the pairs #193's guard refuses before
  any threshold), the related-distinct p99 drops to 0.955 (raw) / 0.960
  (self); 23% / 13% of paraphrases sit above it, and 21% / 22% of the
  remaining related-distinct pairs are at or above 0.85.
- Share at or above 0.85 by family (raw): identifier 58%, quantity 67%,
  repository 25%, correction 17%, polarity 14%, topic-neighbour 11%,
  exception 8%; paraphrase rewording 93%, terse 70%.

### Canary timing (#191)

`eval_measure` embeds the export as the canary's `embed_all` does: every
pair's `a` then `b` in file order, duplicates included (**346 texts** for
the 173 pairs), in batches of the embedder's capacity, timed as one pass
and never from the cache. That took **62.6 s** in the committed run (68.0 s and 70.2 s for
the earlier 172- and 170-pair exports),
against `CANARY_EMBED_TIMEOUT` of 60 s (`within_canary_timeout: false`).
Four earlier runs that embedded only the 274 distinct texts took 59.3 s,
59.2 s, 65.3 s and 62.1 s, so even the deduplicated workload straddles
the budget. On CPU, a related_pairs file of this size overruns the canary;
the owner's Metal rig will be faster, but this is the case #191's backoff
is for.

### Retrieval over the 100-concept corpus (#173)

Grade-2 hit and graded nDCG, answerable queries only:

| Framing | Slice | n | hit@1 | hit@5 | hit@10 | MRR | nDCG@10 |
|---|---|---:|---:|---:|---:|---:|---:|
| self | all | 204 | 0.779 | 0.941 | 0.966 | 0.852 | 0.876 |
| self | dev | 125 | 0.808 | 0.952 | 0.960 | 0.871 | 0.893 |
| self | held_out | 79 | 0.734 | 0.924 | 0.975 | 0.823 | 0.850 |
| self | vocabulary-mismatch | 12 | 0.833 | 0.917 | 1.000 | 0.887 | 0.907 |
| self | exact-identifier | 29 | 0.793 | 0.966 | 1.000 | 0.863 | 0.889 |
| raw | all | 204 | 0.794 | 0.956 | 0.976 | 0.859 | 0.883 |
| raw | held_out | 79 | 0.760 | 0.949 | 0.975 | 0.831 | 0.857 |

True-recall band (bare query against each grade-2 concept, self-framed):
p05 0.456, p50 0.606, p95 0.726 (n 209).

### Abstention (#174)

Top-1 cosine, self-framed corpus: answerable p50 0.612 (dev) / 0.597
(held-out); no-answer p50 0.532 / 0.542. A split-conformal top-1 gate
tuned on dev answerable at α = 0.10 gives τ = 0.532; on held-out it keeps
81% of answerable queries (raw: 85%) and abstains on 41% of no-answer
queries (13 of 32). A cosine-only gate separates the two poorly on this
corpus, which is near-domain by construction; #174's held-out slice is 32
no-answer queries, short of its 100 until the owner adds ledger misses.

## The #198 pairs (2026-10-27)

The set gained 102 pairs for #198 after the run above (see
`fixtures/eval/README.md`, "Added for #198"): 41 `supersedes`, 53
`distinct_entity` in the suspected-supersession trigger's regime and 8
`same-event` paraphrases, all refused by the identifier guard and typed
with `concept_types`. The committed run measured the 403-pair set. The
#198 pairs are kept out of the export (owner decision, 2026-10-27), so it
is still the 173 pairs the run's canary timing and threshold are for:
the identifier guard refuses every one of them before any threshold
applies, so they barely move the threshold the export derives (BGE-M3 p99
0.98921 with them out, 0.98934 with them in) while they push the canary's
embed of the export from 62.6 s to 87.6 s, past its 60 s timeout. A rerun's pair tables will lean further toward the
identifier family, since they cover every pair in `pairs.json`. No run here measures the
trigger: tuning its masked token Jaccard and reporting its precision per
embedder are #198's own work.

## Adding a run

```text
LAMBO_EMBEDDER=candle LAMBO_EMBED_DEVICE=cpu \
  cargo run --release --features embed-candle --example eval_measure -- \
  --cache /tmp/vectors.jsonl --out evidence/eval-set/runs/<name>.json
# the export is always embedded afresh for its timing, cache or not;
# --export-only stops after it.
```

The run file records only the fields that name the model (kind, dim,
model, device, repo, revision, weights file, the candle identity with any
local weights directory replaced by `<local>`): no URL, credential or path.
`--cache` holds vectors keyed by hashes and `--detail` per-item cosines and
rankings; neither belongs in the repository.
