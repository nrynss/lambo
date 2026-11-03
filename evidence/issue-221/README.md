# #221: Qwen3 Embedding 4B calibration (measured 4 Nov 2026)

Lambo 0.8.0, `kind = "openai"` embedder pointed at Vultr Serverless
Inference, model `qwen3-embedding-4b`, dim 2560. SQLite store,
promotion_policy Solo, one session, 10 concepts. A reworded paging query
ranked the lesson 2nd (fused 1.87) behind an unrelated observation (3.42),
while Qwen's raw cosine ranked the lesson first (0.659 vs 0.549 for a
related evidence concept and 0.526 for the unrelated observation).

Cause: no calibration row exists for this contract, so recall falls back
to the BGE-M3-shaped uncalibrated defaults. Qwen's query-to-concept
similarities live in a narrow band (observed 0.26–0.66 on that session),
where BGE-M3-shaped floors make the vector leg too weak and the keyword
leg wins. A measured row — the same quantile-matching method as #87/#203
on the same #190 set (`fixtures/eval/`) — would re-seat the constants on
Qwen's scale.

**Status: measured 4 Nov 2026 (keyed Vultr run).** Rows for both
contracts are in `src/embed/calibration.rs` (`BUILTIN`), with evidence in
`calibration-qwen3-4b-2560.json` / `calibration-qwen3-4b-1024.json`
(derived via `evidence/issue-87-calibration/derive.py`, dev split only;
held-out measured, never tuned on). The anecdotal numbers below remain
motivation only, not calibration data — they come from one 10-concept
session, not the 100 concepts / 310 queries of the eval set.
`qwen3_embedding_4b_rows_match_their_evidence`
(`src/embed/calibration_tests.rs`) pins this state: it fails if either row
leaves `lookup`, and `rows_match_the_committed_evidence` requires each row
to agree with its committed evidence file.

## Contracts to measure

The `openai` alias builds the `bge_m3` adapter, so both contracts have
`kind = "bge_m3"` with the model stamped from config:

| Contract | `model` | `dim` | How |
|---|---|---:|---|
| Qwen full width | `qwen3-embedding-4b` | 2560 | no `dimensions` (adapter default) |
| Qwen Matryoshka 1024 | `qwen3-embedding-4b` | 1024 | `[embedder] dimensions = 1024` (ask 3) |

The two widths are different spaces: each needs its own row (a truncation
is not a rescaling). The reference leg is BGE-M3 (candle, CPU, 1024), the
same reference `evidence/issue-87-calibration/` used.

## Method (replicates #87 / #203 exactly)

Needs `VULTR_INFERENCE_API_KEY` in the environment (never print it, never
commit it). Small token budget: the eval set is 100 concepts / 310 queries
/ 403 pairs; the Vultr endpoint takes up to 2048 inputs per request and
bills $0.05/1M tokens. Qwen's context is 32768 tokens; over-length input
is a 400, which the adapter classes as a content refusal (not retryable).

```bash
# Reference (BGE-M3, candle CPU) — reproduces evidence/issue-87-calibration/runs/.
LAMBO_EMBEDDER=candle LAMBO_EMBED_DEVICE=cpu \
  cargo run --release --example eval_measure --features embed-candle -- \
  --out evidence/issue-221/runs/bge-m3-candle-cpu.json \
  --detail evidence/issue-221/runs/bge-m3-candle-cpu-detail.json

# Target: Qwen3 Embedding 4B at full width (2560).
LAMBO_EMBEDDER=openai LAMBO_EMBED_DIM=2560 \
  LAMBO_LLAMA_EMBED_URL=https://api.vultrinference.com \
  LAMBO_LLAMA_MODEL=qwen3-embedding-4b \
  LAMBO_EMBED_API_KEY_ENV=VULTR_INFERENCE_API_KEY \
  cargo run --release --example eval_measure --features embed-bge -- \
  --out evidence/issue-221/runs/qwen3-4b-2560.json \
  --detail evidence/issue-221/runs/qwen3-4b-2560-detail.json

# Target: Qwen3 Embedding 4B truncated to 1024 (Matryoshka).
LAMBO_EMBEDDER=openai LAMBO_EMBED_DIM=1024 LAMBO_EMBED_DIMENSIONS=1024 \
  LAMBO_LLAMA_EMBED_URL=https://api.vultrinference.com \
  LAMBO_LLAMA_MODEL=qwen3-embedding-4b \
  LAMBO_EMBED_API_KEY_ENV=VULTR_INFERENCE_API_KEY \
  cargo run --release --example eval_measure --features embed-bge -- \
  --out evidence/issue-221/runs/qwen3-4b-1024.json \
  --detail evidence/issue-221/runs/qwen3-4b-1024-detail.json

# Derive each row (dev split only; held-out is measured, never tuned on),
# exactly as evidence/issue-87-calibration/README.md describes:
python3 evidence/issue-87-calibration/derive.py \
  --reference evidence/issue-221/runs/bge-m3-candle-cpu-detail.json \
  --target evidence/issue-221/runs/qwen3-4b-2560-detail.json \
  --kind bge_m3 --model 'qwen3-embedding-4b' --dim 2560 \
  --out evidence/issue-221/calibration-qwen3-4b-2560.json
python3 evidence/issue-87-calibration/derive.py \
  --reference evidence/issue-221/runs/bge-m3-candle-cpu-detail.json \
  --target evidence/issue-221/runs/qwen3-4b-1024-detail.json \
  --kind bge_m3 --model 'qwen3-embedding-4b' --dim 1024 \
  --out evidence/issue-221/calibration-qwen3-4b-1024.json
```

Each row was then shipped in `src/embed/calibration.rs` (`BUILTIN`)
with `evidence = "evidence/issue-221/"`, evidence kept beside it, and
`qwen3_embedding_4b_row_is_pending_measurement` replaced by
`qwen3_embedding_4b_rows_match_their_evidence` (a
`rows_match_the_committed_evidence`-style agreement test).

## Anecdote (motivation only — NOT calibration data)

From the day-one Kalari session (10 concepts, reworded paging query; the
deterministic `lambo_inspect` path from the change kind is unaffected):

- Qwen raw cosine: lesson 0.659, related evidence 0.549, unrelated
  "demo API v1 returns all items" observation 0.526; observed band
  0.26–0.66 (BGE-M3's constants assume a wider spread with floor 0.50).
- Fused recall: lesson 1.87 (2nd) behind the unrelated observation 3.42 —
  the keyword leg outscored the weakened vector leg.

## What ask 2 and ask 3 changed meanwhile

- Ask 2: `lambo serve` (and every full resolve) logs a warning when the
  resolved contract has no calibration row
  (`calibration::warn_if_uncalibrated`, called from
  `resolve::resolve_backends`); `lambo stats` prints a `calibration=…`
  line from the stored contract. Qwen operators see both until the row
  above lands.
- Ask 3: `[embedder] dimensions` (+ `LAMBO_EMBED_DIMENSIONS`) is sent as
  the OpenAI `dimensions` field when set, and must equal `dim`; unset
  sends nothing, so the llama.cpp BGE-M3 path is byte-identical. This is
  what makes the 1024 row measurable at all (Vultr serves Qwen at 2560
  unless asked).
