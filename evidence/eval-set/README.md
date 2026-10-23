# Evaluation set baselines (#190)

Runs of `examples/eval_measure` over the set in `fixtures/eval/` (v0: agent
labels, pending owner adjudication; see `fixtures/eval/README.md`). Items
awaiting owner review (5 pairs, 0 queries) are left out of every number.

## Runs

| File | Embedder | Where |
|---|---|---|
| `runs/bge-m3-candle-cpu.json` | candle BGE-M3, CPU, 1024 dims, `BAAI/bge-m3@5617a9f…`, weights sha256 `68440cc1b73b…` | Linux box, 8 vCPU, no accelerator; 2026-10-23; one cold run (no cached vectors), so `embed_secs` is the export's cold embed time |

## What the baseline says

Two framings: **raw** (the texts as given, which is what `[embedder]
related_pairs` embeds) and **self** (`"{t} — {t}"`, what a one-concept
`lambo_derive` stores). Queries are always bare, as `lambo_recall` embeds
them. Cosine quantiles are nearest rank, as the canary computes them.

### Pairs (#87, #176, #181, #198)

| Framing | Label | n | p05 | p50 | p95 | p99 |
|---|---|---:|---:|---:|---:|---:|
| raw | paraphrase | 108 | 0.838 | 0.924 | 0.970 | 0.984 |
| raw | related_distinct | 170 | 0.549 | 0.837 | 0.980 | 0.989 |
| raw | unrelated | 120 | 0.342 | 0.453 | 0.534 | 0.542 |
| self | paraphrase | 108 | 0.834 | 0.920 | 0.972 | 0.985 |
| self | related_distinct | 170 | 0.562 | 0.823 | 0.975 | 0.991 |
| self | unrelated | 120 | 0.345 | 0.461 | 0.526 | 0.538 |

- **#87.** `related_pairs` pointed at `fixtures/eval/related_pairs.json`
  (170 pairs) derives a threshold of **0.989** (p99). It meets the count
  rule and clears the canary's random-pair floor (p99 0.524)
  (`threshold_rules_met`). Whether the canary gets to apply it depends on
  embedding the file inside its timeout, which on this box it sometimes
  does not (`within_canary_timeout`; see below). At 0.989 only 1 of 108
  paraphrases (0.9%) would still merge: on this set, the p99 rule turns
  semantic merging almost off.
- **The default 0.85** merges 35% (raw) / 43% (self) of the related-distinct
  pairs and keeps 91% / 87% of the paraphrases.
- **Without the identifier family** (the pairs #193's guard refuses before
  any threshold), the related-distinct p99 drops to 0.955 (raw) / 0.960
  (self); 22% / 12% of paraphrases sit above it, and 21% / 22% of the
  remaining related-distinct pairs are at or above 0.85.
- Share at or above 0.85 by family (raw): identifier 58%, quantity 67%,
  repository 25%, correction 18%, polarity 14%, topic-neighbour 12%,
  exception 8%; paraphrase rewording 93%, terse 70%.

### Canary timing (#191)

Embedding the 170-pair export (274 distinct texts) cold on this CPU box
took **62.1 s** in the committed run; three earlier cold runs of the same
export took 59.3 s, 59.2 s and 65.3 s, against `CANARY_EMBED_TIMEOUT` of 60 s. On CPU,
a related_pairs file of this size straddles the canary's budget. The
owner's Metal rig will be faster, but this is the case #191's backoff is
for.

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

## Adding a run

```text
LAMBO_EMBEDDER=candle LAMBO_EMBED_DEVICE=cpu \
  cargo run --release --features embed-candle --example eval_measure -- \
  --cache /tmp/vectors.jsonl --out evidence/eval-set/runs/<name>.json
# with a warm --cache the export's embed_secs is null; for a cold timing
# of the export alone, add --export-only and leave out --cache.
```

The run file records only the fields that name the model (kind, dim,
model, device, repo, revision, weights file, the candle identity with any
local weights directory replaced by `<local>`): no URL, credential or path.
`--cache` holds vectors keyed by hashes and `--detail` per-item cosines and
rankings; neither belongs in the repository.
