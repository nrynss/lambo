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

## Results (self framing; dev tuned on, held-out measured)

Shipped rows (raw-framing sensitivity in parentheses — the two framings
agree within 0.02; #87 agreed within 0.008):

| Contract | Recent | Layer floor | Merge | Default-threshold merges |
|---|---|---|---|---|
| Qwen 2560 | **0.41** (0.39) | **0.59** (0.57) | **0.89** (0.89) | 0.70 |
| Qwen 1024 | **0.42** (0.40) | **0.60** (0.59) | **0.90** (0.90) | 0.71 |

### Recent leg (applies by default)

| Split | Run | Recent | All irrelevant above | Top-5 irrelevant above | True hits above |
|---|---|---:|---:|---:|---:|
| dev | BGE-M3 | 0.35 | 54.8% | 98.9% | 100% |
| dev | Qwen 2560 before | 0.35 | 81.6% | 99.1% | 100% |
| dev | Qwen 2560 after | 0.41 | 54.1% | 98.3% | 99.4% (1 of 169 below) |
| held-out | BGE-M3 | 0.35 | 55.2% | 97.6% | 100% |
| held-out | Qwen 2560 before | 0.35 | 84.5% | 98.2% | 100% |
| held-out | Qwen 2560 after | 0.41 | 58.2% | 98.0% | 100% |
| dev | Qwen 1024 before | 0.35 | 83.3% | 99.2% | 100% |
| dev | Qwen 1024 after | 0.42 | 54.0% | 98.5% | 99.4% (1 of 169 below) |
| held-out | Qwen 1024 before | 0.35 | 86.1% | 98.2% | 100% |
| held-out | Qwen 1024 after | 0.42 | 58.2% | 98.2% | 100% |

### Layer floor (the default for an unset floor)

| Split | Run | Floor | All irrelevant above | True hits above |
|---|---|---:|---:|---:|
| held-out | BGE-M3 | 0.50 | 3.0% | 82.7% |
| held-out | Qwen 2560 at default | 0.50 | 18.1% | 100% |
| held-out | Qwen 2560 calibrated | 0.59 | 3.3% | 79.6% |

Qwen 1024 calibrates to 0.60 with the same shape: held-out irrelevant
above 3.3% (BGE-M3 3.0%), true hits above 79.6% (BGE-M3 82.7%).

### Merge threshold (the default)

By cosine (every pair):

| Split | Run | Threshold | Related-distinct merged | Paraphrases merged |
|---|---|---:|---:|---:|
| dev | BGE-M3 | 0.85 | 61.5% | 86.6% |
| dev | Qwen 2560 before | 0.85 | 70.3% | 93.9% |
| dev | Qwen 2560 after | 0.89 | 62.1% | 78.1% |
| held-out | BGE-M3 | 0.85 | 63.2% | 83.3% |
| held-out | Qwen 2560 before | 0.85 | 71.1% | 86.1% |
| held-out | Qwen 2560 after | 0.89 | 61.4% | 69.4% |
| dev | Qwen 1024 after | 0.90 | 61.0% | 74.4% |
| held-out | Qwen 1024 after | 0.90 | 60.5% | 69.4% |

### Retrieval (unchanged: constants do not reorder vector hits)

| Run | Framing | hit@1 | nDCG@10 |
|---|---|---:|---:|
| BGE-M3 | raw / self | 0.775 / 0.763 | 0.866 / 0.855 |
| Qwen 2560 | raw / self | 0.721 / 0.702 | 0.856 / 0.840 |
| Qwen 1024 | raw / self | 0.699 / 0.714 | 0.844 / 0.842 |

## Live recall probe (proxy session, daemon path)

The original 10-concept session's exact texts are not in the repo, so the
anecdote above cannot be replayed verbatim. As a proxy, a fresh SQLite
session under the Qwen 2560 contract (`kind = "openai"`,
`model = "qwen3-embedding-4b"`, Solo) was written with 11 concepts in the
anecdote's shape — a paging lesson, a related paging evidence, the
unrelated "demo API v1 returns all items" observation, and fillers — and
recalled through `lambo serve` (the daemon path, where the calibrated
recent score and layer floor apply) with `calibration = "builtin"` vs
`"off"`:

- Lesson-seeking query ("how should the list endpoint return results when
  there are too many for one response"): lesson 1st at 3.37, runner-up
  1.84, unrelated observation 3rd at 1.40 (builtin). The anecdote's
  failure mode — the unrelated observation ahead of the lesson — does not
  occur.
- The row is live in the daemon: off vs builtin fused scores differ
  (e.g. 3.38 → 3.37, 1.41 → 1.40 on that query).

(`lambo recall`, the lease-free reader, keeps the flat 0.35 recent score
on purpose — the rows were measured on the daemon's text-recall blend —
so a CLI before/after cannot discriminate; the probe above goes through
the daemon.)

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

## Fusion follow-up (reopened #221, measured 4 Nov 2026)

The 0.9.0 rows above did not move the reopened case: with
`calibration = "builtin"` and `"off"` the fused scores are identical to two
decimals (2.92 / 1.77 / 1.18 / 0.83), at 1024 too, and after Solo promotion.
The exact 8-concept session is now recorded
(`repro-8concept-qwen3-4b-2560.json`: the 8 framed doc texts, their live
2560-dim vectors, the bare query and its live query vector), so the anecdote
is replayable offline, and the two mysteries from the reopen comment are
answered below with score-breakdown evidence.

### M1: why builtin vs off is identical — the vector leg never wins the max

Phase-1 relevance under the default fusion is
`max(keyword BM25, recent flat score, vector cosine)`
(`LegScores::merged`, `src/recall/candidates.rs`; the final is
`0.5 × daemon + 0.5 × relevance`, `src/recall/assemble.rs`, default
`RecallWeights` in `src/config.rs`). Live leg values on the exact session:

| Concept | BM25 | vector cosine | max (= relevance) | final |
|---|---|---:|---:|---:|
| demo API v1 … | 5.3753 | 0.5815 | 5.3753 | 2.9207 |
| lesson (paging) | 2.5265 | 0.7274 | 2.5265 | 1.7569 |
| Chaaya … | 1.8883 | 0.3158 | 1.8883 | 1.1775 |
| run r-001 … | 1.0634 | 0.5680 | 1.0634 | 0.8324 |

Every shown hit's BM25 is above 1.06, while every calibrated constant is at
most 0.90 (recent 0.41, layer floor 0.59, merge threshold 0.89 — and the
latter two do not even enter phase 1). So `max` picks the BM25 leg in both
runs and the calibration cannot move a rank or a shown score: builtin vs off
finals are identical to 4 decimals (2.9207 / 1.7569 / 1.1775 / 0.8324),
reproduced here through `Memory` on SQLite under both widths.

The dropout hypotheses from the reopen comment resolve as follows: **floor
exclusion, no** — recall's vector leg has no cosine floor
(`rank_by_cosine`, `src/store/vector_source.rs`, is top-limit only; the
layer floor is layered-recall-only and the merge threshold is write-path
only); **candidate cap, no** — all 8 concepts are returned by the leg;
**cache, no** — the query cache serves the same vector; **weight, in
effect yes** — under max there is no weight, only raw scales, and BM25's
scale (unbounded, 1.06–5.38 here) always beats cosine (≤ 1, 0.26–0.73 here).
The vector leg is present in every shown hit's provenance and contributes
to none of their fused values.

### M2: the keyword/body split, at the token level

Query stems (`normalize_tokens`): order api now return result one page time
with cursor; how should import handl it? — plus `order`-family forms.

| Concept | shared stems | BM25 | cosine (vector rank) | vector share of fused |
|---|---|---:|---:|---:|
| demo API v1 … | api, return, one | 5.3753 | 0.5815 (2nd) | 11% |
| lesson (paging) | page (via `next_page_token`), import | 2.5265 | 0.7274 (1st) | 29% |
| Chaaya … | with | 1.8883 | 0.3158 (6th) | 17% |
| run r-001 … | import | 1.0634 | 0.5680 (3rd) | 53% |

Splitting the finals (`final = 0.5 × daemon + 0.5 × relevance`) gives daemon
scores demo 0.47 / lesson 0.99 / Chaaya 0.47 / r-001 0.60: the daemon ranks
the lesson first (it has the dependent), but the query-relevance leg —
keyword-decided under max — flips it. Qwen's raw cosine ranks the lesson
first (0.7274), then the demo observation (0.5815), then r-001 (0.5680).

### Measured blend: RRF at #173's defaults, Qwen-scoped

`examples/eval_fusion` on the #190 set (`fixtures/eval/`), Qwen3-Embedding-4B
at 2560, ten variants on `dev` (100 concepts derived once; the embedding
cache is local-only, not committed) — `max`, RRF at #173's defaults, a k
sweep (2, 5), single-axis leg-weight moves (`keyword=0.5`, `vector=0.5`,
`vector=2`, `recent=0`), and daemon-share moves (`wd=0`, `wd=0.3`); a
sweep of single axes, not a grid:

| variant | dev nDCG@10 | dev hit@1 | vocab-mismatch (30) | exact-id (18) |
|---|---|---|---|---|
| `max` | 0.7905 | 0.679 | 0.446 | 0.923 |
| `rrf` (k=3, 1/1/0.3) | 0.8521 | 0.715 | 0.699 | 0.859 |
| `rrf:k=2` | 0.8546 | 0.715 | 0.706 | 0.867 |
| `rrf:k=5` | 0.8386 | 0.685 | 0.677 | 0.858 |
| `rrf:keyword=0.5` | 0.8411 | 0.685 | 0.743 | 0.756 |
| `rrf:vector=2` | 0.8600 | 0.727 | 0.784 | 0.780 |
| `rrf:vector=0.5` | 0.8160 | 0.667 | 0.584 | 0.886 |
| `rrf:recent=0` | 0.8634 | 0.733 | 0.710 | 0.863 |
| `rrf:wd=0` | 0.8789 | 0.758 | 0.743 | 0.887 |
| `rrf:wd=0.3` | 0.8726 | 0.751 | 0.718 | 0.879 |

Held-out confirmation (`max` vs `rrf` only, decided on dev):

| split | max nDCG@10 | rrf nDCG@10 | vocab-mismatch | exact-id | contrast |
|---|---|---|---|---|---|
| dev | 0.7905 | 0.8521 (+0.062) | 0.446 → 0.699 | 0.923 → 0.859 | 0.911 → 0.896 |
| held_out | 0.8403 | 0.8685 (+0.028) | 0.672 → 0.802 | 0.948 → 0.881 | 0.912 → 0.854 |

The shape matches #173's BGE-M3/EG2 result (big vocabulary-mismatch gain,
small exact-identifier/contrast losses on small classes), and the exact #221
case is a vocabulary-mismatch query: RRF fixes it where no max-side constant
can. `wd=0` scores higher on dev but does NOT fix the exact case (relevance
alone still ranks the demo observation first: 0.913 vs 0.783), and `recent=0`
is the set artefact #173 names (the recent leg always holds the corpus's last
three concepts). `wd=0.3` does fix the exact case — lesson first at 0.845 vs
demo 0.749 on the recorded 8-concept session — and beats the shipped default
on dev (0.8726 vs 0.8521). It is still not shipped: it introduces a
non-default daemon share, and the shipped default stays RRF at #173's
existing constants (k = 3, 1.0/1.0/0.3, daemon 0.5). No new numbers — the
shipped default is RRF at the existing defaults, applied per-contract (see
below).

Files: `fusion-qwen3-4b-2560-dev.json` (+ `.queries.jsonl`) is the dev
sweep, `fusion-qwen3-4b-2560-heldout.json` (+ `.queries.jsonl`) the held-out
confirmation, `repro-8concept-qwen3-4b-2560.json` and
`repro-8concept-qwen3-4b-1024.json` the recorded live vectors (full width
and Matryoshka truncation) for the offline regression tests. The held-out
run's stderr log is not kept: the example prints only the dev split to
stderr, so the file was a copy of the dev log's numbers, not held-out
evidence (the held-out numbers live in the JSON).

### Shipped fix

A file with no `[fusion]` table on a Qwen3-Embedding-4B contract under
`builtin` calibration now recalls with RRF at #173's defaults; every other
contract keeps `max`, and an explicit `[fusion]` table or
`calibration = "off"` keeps its say (`default_fusion` in
`src/embed/calibration.rs`, applied by `apply_contract_fusion_default` in
`src/resolve.rs` for both `resolve_backends` and `resolve_verify_io`, with a
one-line log). BGE-M3 and EmbeddingGemma 2 behavior is unchanged by
construction: their contracts resolve to `RecallFusion::default()` exactly
as before.

Before/after on the exact session (top-4), both widths, daemon path
(both rows pinned offline in `issue221_fusion` on the recorded vectors):

| Run | 1st | 2nd | 3rd | 4th |
|---|---|---|---|---|
| 2560 + 1024, max (before) | demo API v1 … 2.92 | lesson 1.76 | Chaaya 1.18 | r-001 0.83 |
| 2560, RRF default (after) | lesson 0.885 | demo API v1 … 0.668 | r-001 0.570 | pagination_changed 0.474 |
| 1024, RRF default (after) | lesson 0.885 | demo API v1 … 0.668 | r-001 0.570 | pagination_changed 0.474 |

Regression tests (offline, on the recorded vectors at both widths):
`memory::tests::issue221_fusion` — each fixture keeps the live geometry, max
reproduces the misrank, the RRF default ranks the lesson first;
`calibration::default_fusion` matrix (Qwen both widths → RRF, EG2/BGE-M3/
unmeasured → max, off → max); `resolve` scoping (unset → RRF on Qwen, stays
max on BGE-M3; explicit table and off win).
