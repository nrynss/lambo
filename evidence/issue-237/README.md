# #237: Elastic Jina embeddings v3 calibration (measured 10 Nov 2026)

Lambo 0.10.0, `kind = "elastic"` embedder pointed at an Elastic Cloud
Serverless Elasticsearch 9.7.0 cluster, inference id `.jina-embeddings-v3`,
dim 1024. The 8-concept live probe from the issue (Jina raw cosine ranking
the lesson first, fused recall burying it behind keyword-heavy neighbours)
is motivation only, not calibration data — it comes from one 8-concept
session, not the 100 concepts / 310 queries of the eval set. The measured
row below uses the same quantile-matching method as #87/#203/#221 on the
same #190 set (`fixtures/eval/`).

**Status: shipped.** The row is `ELASTIC_JINA_V3_1024` in `BUILTIN`
(`src/embed/calibration.rs`), derived via
`evidence/issue-87-calibration/derive.py` (dev split only; held-out
measured, never tuned on). Covered by `rows_match_the_committed_evidence`
plus the exact-contract
(`elastic_exact_jina_contract_is_calibrated_but_never_reference`) and
neighbour (`elastic_neighbouring_contracts_stay_uncalibrated`) tests:
`warn_if_uncalibrated` is silent for the exact
`elastic`/`.jina-embeddings-v3`/1024 contract, and `lambo stats` reports
`calibration=calibrated`.

## Contract to measure

The `elastic` alias builds the `elastic` adapter (`src/embed/elastic.rs`)
against the cluster's Inference API (`POST
/_inference/text_embedding/.jina-embeddings-v3`), with `input_type: INGEST`
for documents and `SEARCH` for queries (`send_input_type` defaults on):

| Contract | `model` | `dim` | How |
|---|---|---:|---|
| Elastic Jina v3 | `.jina-embeddings-v3` | 1024 | `LAMBO_EMBEDDER=elastic`, `LAMBO_LLAMA_MODEL=.jina-embeddings-v3` |

The reference leg is BGE-M3 (candle, CPU, 1024) — the committed
`evidence/issue-221/runs/bge-m3-candle-cpu-detail.json`, re-running BGE was
not needed.

## Method (replicates #87 / #203 / #221 exactly)

Needs the cluster URL and API key from the keyring (never print them, never
commit them). The eval set is 100 concepts / 310 queries / 403 pairs; each
embed is one Inference API call, so the run takes ~15 min wall.

```bash
# Smoke: the ignored live test proves the endpoint accepts input_type and
# returns 1024-wide float vectors (aborts the phase otherwise).
LAMBO_ELASTIC_EMBED_URL=<keyring url> \
LAMBO_ELASTIC_INFERENCE_ID=.jina-embeddings-v3 LAMBO_ELASTIC_EMBED_DIM=1024 \
LAMBO_ES_API_KEY=<keyring key> \
  cargo test --features embed-elastic --lib embed::elastic::tests::live \
  -- --ignored --nocapture

# Target: Elastic Jina v3 at 1024.
LAMBO_EMBEDDER=elastic LAMBO_EMBED_DIM=1024 \
  LAMBO_LLAMA_EMBED_URL=<keyring url> \
  LAMBO_LLAMA_MODEL=.jina-embeddings-v3 \
  LAMBO_EMBED_API_KEY_ENV=LAMBO_ES_API_KEY \
  LAMBO_ES_API_KEY=<keyring key> \
  LAMBO_EMBED_CONNECT_TIMEOUT_MS=20000 LAMBO_EMBED_TIMEOUT_MS=60000 \
  cargo run --release --example eval_measure --features embed-elastic -- \
  --cache /tmp/opencode/lambo-issue-237-elastic.jsonl \
  --out evidence/issue-237/runs/elastic-jina-v3-1024.json \
  --detail evidence/issue-237/runs/elastic-jina-v3-1024-detail.json

# Derive the row (dev split only; held-out is measured, never tuned on),
# exactly as evidence/issue-87-calibration/README.md describes:
python3 evidence/issue-87-calibration/derive.py \
  --reference evidence/issue-221/runs/bge-m3-candle-cpu-detail.json \
  --target evidence/issue-237/runs/elastic-jina-v3-1024-detail.json \
  --kind elastic --model '.jina-embeddings-v3' --dim 1024 \
  --framing self \
  --out evidence/issue-237/calibration-elastic-jina-v3-1024.json
```

Endpoint: Elastic Cloud Serverless, Elasticsearch 9.7.0, inference id
`.jina-embeddings-v3`, measured 10 Nov 2026. The run stamps
`kind = "Elastic"`, `identity = ".jina-embeddings-v3"`, `dimensions = 1024`,
`send_input_type = true`.

## Results (self framing; dev tuned on, held-out measured)

Derived row (raw-framing sensitivity in parentheses — the two framings agree
within 0.02 on recent/floor, 0.02 on merge; #221 agreed within 0.02):

| Contract | Recent | Layer floor | Merge | Default-threshold merges |
|---|---|---|---|---|
| Elastic Jina v3 1024 | **0.13** (0.13) | **0.39** (0.39) | **0.86** (0.88) | 0.64 |

Jina's query-to-concept null sits far lower than BGE-M3's (dev irrelevant
p95 0.35 vs BGE-M3's wider spread), so the quantile match pulls both recall
constants down while the merge threshold stays near the default.

INVARIANT (shipped row): recent < layer_floor < merge_threshold —
0.13 < 0.39 < 0.86 holds.

### Recent leg (applies by default)

| Split | Run | Recent | All irrelevant above | Top-5 irrelevant above | True hits above |
|---|---|---:|---:|---:|---:|
| dev | BGE-M3 | 0.35 | 54.8% | 98.9% | 100% |
| dev | Jina v3 before | 0.35 | 5.2% | 64.1% | 84.6% |
| dev | Jina v3 after | 0.13 | 55.5% | 98.6% | 100% |
| held-out | BGE-M3 | 0.35 | 55.2% | 97.6% | 100% |
| held-out | Jina v3 before | 0.35 | 4.7% | 64.9% | 87.8% |
| held-out | Jina v3 after | 0.13 | 55.5% | 98.2% | 100% |

### Layer floor (the default for an unset floor)

| Split | Run | Floor | All irrelevant above | True hits above |
|---|---|---:|---:|---:|
| held-out | BGE-M3 | 0.50 | 3.0% | 82.7% |
| held-out | Jina v3 at default | 0.50 | 0.4% | 49.0% |
| held-out | Jina v3 calibrated | 0.39 | 2.7% | 81.6% |

### Merge threshold (the default)

By cosine (every pair):

| Split | Run | Threshold | Related-distinct merged | Paraphrases merged |
|---|---|---:|---:|---:|
| dev | BGE-M3 | 0.85 | 61.5% | 86.6% |
| dev | Jina v3 before | 0.85 | 63.6% | 74.4% |
| dev | Jina v3 after | 0.86 | 61.0% | 67.1% |
| held-out | BGE-M3 | 0.85 | 63.2% | 83.3% |
| held-out | Jina v3 before | 0.85 | 57.9% | 66.7% |
| held-out | Jina v3 after | 0.86 | 55.3% | 63.9% |

Writer view (guard-eligible pairs only, dev/held-out): after 0.86,
related-distinct merged 16.7% (1 of 6) / 38.5% (5 of 13), paraphrases merged
81.1% / 71.4% — the dev related-distinct class has only 6 guard-eligible
pairs, hence the step. The #176 related-pairs rule p99 is 0.9992
(held-out related-distinct merged 2.6%, paraphrases 0.0%).

### Retrieval (unchanged: constants do not reorder vector hits)

| Run | Framing | hit@1 | nDCG@10 |
|---|---|---:|---:|
| BGE-M3 | raw / self | 0.775 / 0.763 | 0.866 / 0.855 |
| Jina v3 1024 | raw / self | 0.702 / 0.683 | 0.831 / 0.811 |

### Raw/self sensitivity

| Framing | Recent | Layer floor | Merge |
|---|---|---|---|
| self (shipped basis) | 0.13 | 0.39 | 0.86 |
| raw | 0.13 | 0.39 | 0.88 |

## Anecdote (motivation only — NOT calibration data)

From the issue's 8-concept live probe (Elastic Jina v3, single session):

- Jina raw cosine ranked the lesson first; fused recall under uncalibrated
  defaults did not surface it — the keyword leg outscored the weakened
  vector leg, the same shape as #221's Qwen anecdote.
- Whether calibration alone fixes that session (a max-fusion scale question
  like #221/M1, where the vector leg never wins the max) is NOT answered by
  the numbers above and needs the fusion follow-up, not a constants row.
