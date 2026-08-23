# H2 — the Cockroach live leg of cross-store recall parity

Closes H2's Done-when box (`dev-diary/lambo-for-mooshik/H-cross-store-parity.md`): the H1
harness's corpus, probe × limit grid and v1 report shape, run **live** against a real
CockroachDB cluster through `CockroachStore` — with the §H2 attribution asserted, not just
recorded: score skew zero on the shared `1 − d²/2 ≡ cosine` scale where exact-scan applies,
ANN divergence measured against the C-SPANN envelope.

## What produced this directory

`store::cockroach::h2_cockroach_parity::h2_live_cockroach_recall_parity`
(`src/store/cockroach.rs`). `#[ignore]`d like every live cockroach test — a run without
`LAMBO_COCKROACH_DSN` reports it as **ignored**, never skip-as-green; with
`LAMBO_REQUIRE_LIVE=1` a missing DSN is a hard failure. Regenerate:

```sh
set -a; . ./.env; set +a   # provides LAMBO_COCKROACH_DSN (never printed)
LAMBO_REQUIRE_LIVE=1 LAMBO_H2_EMIT_EVIDENCE=1 \
  cargo test --features store-cockroach,store-sqlite,fixtures \
  --lib store::cockroach::h2_cockroach_parity -- --ignored --nocapture
```

Files: `report.json` (v1 report — same serde shape as H1's, rows added, no new fields) and
`run-20260823-live.txt` (the full `--nocapture` log of this run).

## This run

- Run date: 2026-08-23 (08:48 IST)
- Commit at capture time: `fe4d63d8e65a9a30ed394fb299629c509223aa88`
- Cluster: CockroachDB CCL v26.2.5 (serverless/basic, `LOCALITY REGIONAL BY TABLE IN PRIMARY REGION`)
- Beam size: default **64** (`LAMBO_VECTOR_BEAM_SIZE` unset) — exactly the beam the C-SPANN
  published figure (0.99 recall@50) refers to
- Index parameters (from `SHOW CREATE TABLE concepts`, full DDL in the run log):
  `VECTOR INDEX concepts_embedding_idx (embedding vector_l2_ops) WHERE embedding IS NOT NULL`
  — partial, L2 metric, on `concepts.embedding VECTOR(1024)`. The production query was
  camera-proofed live this run: plain `EXPLAIN` shows
  `• vector search / table: concepts@concepts_embedding_idx (partial index)` and no
  `FULL SCAN`, so the cockroach adapter's `index_present: true` is measured, not assumed.
- Corpus: both committed fixture graphs (`session-rest-api`: 22 concepts, `session-drift`:
  9 concepts), each re-homed into a per-run unique session scope (fresh node ids under
  `…-h2-<run-uuid>` sessions — nothing pre-existing touched, nothing dropped), stamped with
  a dim-1024 fixture contract, vectors = `synthetic_unit_vector` (same construction as H1).
- Grid: 4 probe shapes (`stored-itself`, `negated`, `midpoint`, `off-axis`) × 5 limits
  (1, 3, 5, pool, pool+7) × 3 adapter pairs × 2 fixtures = **120 pairs**
  (80 `AnnEnvelope`, 40 `ExactMustMatch`).

## The numbers

| Measure | ExactMustMatch (memory-oracle vs sqlite) | AnnEnvelope (cockroach vs each exact adapter) |
| --- | --- | --- |
| pairs | 40 | 80 |
| candidate jaccard | 1.0 every row | **min 1.0** — every row perfect |
| rank prefix | full | displaced ranks across all rows: **0** |
| max score diff | 0.0 (bit-for-bit equal, asserted) | **max 5.36e-6** over all 80 rows |
| `exact_match` | true on all 40 rows | false only by float noise in scores |

Attribution, per the spec:

1. **Skew is zero on the shared scale.** The worst ANN-vs-exact score difference is
   5.36e-6 ≈ 5 ulps of an f32 accumulation over 1024 dims — pure round-trip noise from
   computing f32 cosine directly vs f32 L2 + `distance_to_score(1 − d²/2)`. No row shows
   systematic skew (the H3-named conversion bug class would sit ≥ ~0.01); the assertion
   bound is 1e-4.
2. **ANN divergence sits inside — in fact below — the envelope.** C-SPANN publishes 0.99
   recall@50 at beam 64, i.e. jaccard ≥ 0.98 at equal-size top-k sets. Measured:
   **jaccard 1.0 on all 80 ANN cells and zero rank displacement** — on a ≤22-row corpus the
   C-SPANN index answered every probe exactly. Recall@k = 1.000 for k ∈ {1,3,5,pool,pool+7}
   against the exact-cosine oracle. (The 16 rows where `rank_prefix_match < limit` are
   truncation at pool size, not divergence: prefix equals the pool size there.)

## The quarantine-history measurement (the question F could only reason about)

Same history of writes into both adapters: stamp contract A (dim 1024), write 9 vectors,
recall (both answer 5 candidates), then restamp contract B (dim 4, different model).

| Adapter | mechanism | observable recall after restamp |
| --- | --- | --- |
| SQLite | width-change restamp-quarantine: NULLs all 9/9 concept vectors | answers the dim-4 question **EMPTY** (valid stamp, zero vectors) |
| Cockroach | NULL-only quarantine (unstamped→stamped transition) + `VECTOR(1024)` DDL width enforcement | **refuses** the dim-4 question fail-closed |

Cross-space recall delivered: **0 candidates on both adapters.** The mechanisms differ
exactly as documented; the *observable recall behaviour* matches — no vector written under
contract A is ever returned under contract B, on either backend. That settles F's box-5
residue ("whether the two produce the same observable behaviour") with a measurement
instead of reasoning.

## Report schema

Identical v1 shape to `evidence/mooshik-h1-cross-store-parity/report.json` — field-by-field
table in that README. This file adds only ROWS: one new `adapters` entry (`"cockroach"`,
`scan: "Ann"`, `index_present: true`) plus `"sqlite"` as a second exact-scan adapter when
compiled in, and the corresponding `pairs`. `git_rev` is null by design (the harness never
shells out); the capture rev is recorded above.

## Design note

The H2 harness is a deliberate twin of H1's module, not a shared extraction — see the
module doc on `store::cockroach::h2_cockroach_parity` for why that follows this repo's
recorded precedent (private test infra is reimplemented per adapter, F's `cosine_oracle`,
H1's `MemoryOracleStore`). Comparability is carried by the versioned serde shape, not by a
shared type.
