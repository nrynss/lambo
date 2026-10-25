# Per-contract score calibration (#87)

Lambo's absolute score constants were set on BGE-M3: the recent leg's flat
`RECENT_SCORE` (0.35), the layered-recall floor `min_similarity` (0.50) and
the merge threshold `semantic_match_threshold` (0.85). This directory
measures them under EmbeddingGemma 2 and derives that contract's values.
The method is in `src/embed/calibration.rs`; `derive.py` reproduces
`calibration.json` from two `eval_measure --detail` files.

## Runs

| File | Embedder | Where |
|---|---|---|
| `runs/bge-m3-candle-cpu.json` | candle BGE-M3, CPU, 1024 dims, `BAAI/bge-m3@5617a9f…`, weights sha256 `68440cc1b73b…` (the reference) | Linux box, 8 vCPU, no accelerator |
| `runs/eg2-q8-768-llamacpp-b11517-cpu.json` | EmbeddingGemma 2, contract `embeddinggemma2` / `ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2` / 768, served by llama.cpp b11517 (`8a1a9b512`, ubuntu-x64 release, CPU, text only, `--embeddings --pooling mean`); `embeddinggemma-2-Q8_0.gguf` sha256 `2188ac1d…40b09135`, as in `evidence/issue-22-eg2/versions.txt` | same box |

The set is #190's (`fixtures/eval/`): 100 concepts, 310 queries (204
answerable, 106 no-answer), 403 labelled pairs, each item on a dev or
held-out split. Queries are bare; concepts and pairs are measured in both
framings (raw, and self `"{t} — {t}"`, what a one-concept derive stores).

## Method

Quantile matching against BGE-M3, **dev split only**. For a BGE-M3 constant
`c` and a null distribution `F`: `q = F_bge(c)`, then `c_EG2 =
quantile_EG2(q)` (nearest rank). Each constant keeps the rank position it
was chosen for; BGE-M3 maps to itself, so no BGE-M3 ranking moves. The
held-out split is measured with the shipped (two-decimal) values, never
tuned on.

| Constant | Null | BGE-M3 | q (dev) | EG2 derived (self / raw) | Shipped |
|---|---|---:|---:|---:|---:|
| recent score | query to irrelevant concept (grade 0) | 0.35 | 0.482 | 0.630 / 0.627 | **0.63** |
| layer floor | query to irrelevant concept (grade 0) | 0.50 | 0.967 | 0.720 / 0.718 | **0.72** (recorded only) |
| merge threshold | related-but-distinct pair, share merged | 0.85 | 0.439 merged | 0.926 / 0.934 | **0.93** (opt-in) |

The two framings agree to within 0.008. Vector cosines are not rescaled: a
`(cos − floor) / (1 − floor)` map would move every reported cosine, the
layer floors, dedupe and the BGE-M3 goldens. Under BGE-M3 (floor ≈ 0.49) it
would also push the median true hit (0.61) below 0.35.

## Before and after (self framing; n per split in `calibration.json`)

### Recent leg (applies by default)

"Above" is the share of vector scores that outrank a recent-leg member.
"Top-5 irrelevant" counts only the irrelevant concepts among each query's
five best, which are the irrelevant hits a recall's vector leg actually
returns.

| Split | Run | Recent | All irrelevant above | Top-5 irrelevant above | True hits above |
|---|---|---:|---:|---:|---:|
| dev | BGE-M3 | 0.35 | 51.8% | 98.7% | 100% |
| dev | EG2 before | 0.35 | 100% | 100% | 100% |
| dev | EG2 after | 0.63 | 51.8% | 97.1% | 100% |
| held-out | BGE-M3 | 0.35 | 52.6% | 97.3% | 100% |
| held-out | EG2 before | 0.35 | 100% | 100% | 100% |
| held-out | EG2 after | 0.63 | 56.4% | 96.8% | 100% |

The lowest true hit is 0.353 under BGE-M3 and 0.657 under EG2, so both
constants sit below every true hit on the set.

**Trade-off.** 0.35 was chosen to sit *below every true hit* (its own docs:
"only its rank position relative to real matches"). It was never above
irrelevant hits: under BGE-M3 too, 97–99% of the irrelevant hits a recall
returns outrank it. No value puts the recent leg above irrelevant hits
without also putting it above true ones. Dev split, EG2:

| Recent | Top-5 irrelevant above | All irrelevant above | True hits above |
|---:|---:|---:|---:|
| 0.63 (shipped) | 97.1% | 51.8% | 100% |
| 0.66 | 93.4% | 25.9% | 100% |
| 0.69 | 83.0% | 10.5% | 97.7% |
| 0.71 | 61.3% | 4.9% | 96.1% |
| 0.72 | 49.5% | 3.2% | 95.3% |
| 0.74 | 27.8% | 1.4% | 89.1% |

(BGE-M3 for comparison: 0.45 gives 81.1% / 11.0% / 96.9%, and 0.50 gives
48.8% / 3.3% / 89.1%.)

### Merge threshold (opt-in: `[embedder] calibrated_merge_threshold = true`)

| Split | Run | Threshold | Related-distinct merged | Paraphrases merged |
|---|---|---:|---:|---:|
| dev | BGE-M3 | 0.85 | 43.9% | 87.0% |
| dev | EG2 before | 0.85 | 78.1% | 100% |
| dev | EG2 after | 0.93 | 43.0% | 83.1% |
| held-out | BGE-M3 | 0.85 | 40.7% | 87.9% |
| held-out | EG2 before | 0.85 | 76.3% | 100% |
| held-out | EG2 after | 0.93 | 45.8% | 87.9% |
| held-out | EG2, p99 rule (0.997, via `related_pairs`) | 0.997 | 0% | 0% |

The issue expected 0.85 to leave EG2 paraphrases unmerged, on 8 pairs. On
these 403 pairs it does the opposite: it merges every paraphrase and three
quarters of the related-but-distinct pairs, so 0.80 would be worse. The
calibrated 0.93 brings EG2 back to BGE-M3's rate. That rate is itself high
(BGE-M3 merges about 41–44% of related-but-distinct pairs at 0.85), but
changing it is a decision about BGE-M3's default, not this calibration.

### Layer floor (recorded, not applied)

| Split | Run | Floor | All irrelevant above | True hits above |
|---|---|---:|---:|---:|
| held-out | BGE-M3 | 0.50 | 3.2% | 85.0% |
| held-out | EG2 at default | 0.50 | 99.9% | 100% |
| held-out | EG2 calibrated | 0.72 | 3.1% | 92.5% |

### Retrieval (unchanged: constants do not reorder vector hits)

| Run | Framing | hit@1 | nDCG@10 |
|---|---|---:|---:|
| BGE-M3 | raw / self | 0.794 / 0.779 | 0.883 / 0.876 |
| EG2 | raw / self | 0.775 / 0.799 | 0.888 / 0.897 |

## Not measured

Image embeddings (the set is text only), EG2 at 512/256/128 dims, other
quantisations, a llama.cpp build other than b11517, and BGE-M3 served by
llama.cpp (`bge_m3`). A contract without a row keeps the BGE-M3 constants.

## Reproduce

```
LAMBO_EMBEDDER=candle LAMBO_EMBED_DEVICE=cpu \
  cargo run --release --example eval_measure --features embed-candle -- \
  --out bge.json --detail bge-detail.json
LAMBO_EMBEDDER=eg2 LAMBO_EMBED_DIM=768 LAMBO_LLAMA_EMBED_URL=http://127.0.0.1:8080 \
  LAMBO_LLAMA_MODEL=ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0 \
  cargo run --release --example eval_measure --features embed-eg2 -- \
  --out eg2.json --detail eg2-detail.json
python3 evidence/issue-87-calibration/derive.py --reference bge-detail.json \
  --target eg2-detail.json --kind embeddinggemma2 \
  --model 'ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2' \
  --dim 768 --out calibration.json
```

The detail files (about 10 MB each) are not committed; they hold set ids
and cosines only.
