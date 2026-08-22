# H1 — cross-store recall parity, cluster-free half

Closes H1's Done-when box (`dev-diary/lambo-for-mooshik/H-cross-store-parity.md`): the harness
runs, against SQLite and an in-process "memory oracle" (`MemoryStore` plus exact cosine), the
same probe × limit grid F's agreement matrix uses — and asserts, not just reports, that the two
exact-scan adapters agree bit-for-bit on every candidate, every rank, and every score.

## What produced this file

`store::sqlite::tests::h1_cross_store_parity::h1_sqlite_and_memory_oracle_agree_exactly`
(`src/store/sqlite.rs`). Not `#[ignore]`d — it needs no DSN and runs in normal CI (the same
`--features store-sqlite,fixtures` row F's own matrix runs in).

Regenerate:

```sh
LAMBO_H1_EMIT_EVIDENCE=1 cargo test --features store-sqlite,embed-fixture,fixtures \
  --lib store::sqlite::tests::h1_cross_store_parity -- --nocapture
```

Without `LAMBO_H1_EMIT_EVIDENCE`, the test still runs and asserts — it just skips the write, so a
normal `cargo test` never touches the working tree.

## Two legs, one grid

1. **Required — synthetic vectors on both committed fixture graphs**
   (`session-rest-api`, `session-drift`): `synthetic_unit_vector` and the same probe shapes as
   F's matrix (`stored-itself`, `negated`, `midpoint`, `off-axis`) × 5 limits. This is the
   evidence the Done-when box needs.
2. **Optional — the real BGE-M3 corpus** (`evidence/mooshik-f-sqlite-bge/f-bge.db`): the same
   grid, seeded from real embedder output instead of synthetic vectors, so cross-store agreement
   is also checked on vectors an actual model produced. Best-effort: skips cleanly (with a
   message, not a failure) if the corpus is missing or its schema has drifted since it was
   captured. This run's corpus was readable and contributed 20 of the 60 pairs below.

Both legs run through the *same* pairwise grid runner (`run_fixture_grid`), so both get the same
three measures and the same exact-agreement assertion.

## Report schema (`schema_version: 1`)

| Field | Meaning |
| --- | --- |
| `schema_version` | Bumped only for a breaking change (field removed/retyped/repurposed). Additive fields do not bump it. |
| `harness.git_rev` | Populated by whatever captures the report, e.g. `git rev-parse HEAD` — the harness itself never shells out. `null` here. |
| `harness.features` | Cargo features active when this report was generated. |
| `harness.fixtures` | Fixture / corpus labels this run actually exercised. |
| `adapters[].name` | Free-text adapter name (`"sqlite"`, `"memory-oracle"`, …) — never an enum tied to one backend. |
| `adapters[].scan` | `"Exact"` (full/linear scan) or `"Ann"` (approximate index). Both H1 adapters are `Exact`. |
| `adapters[].index_present` | Whether an index served the answer in this run, vs. a full/forced-exact scan. Always `false` here: SQLite and the memory oracle are both linear scans by construction. |
| `pairs[].fixture` / `.probe` / `.limit` | Which fixture, which of the four probe shapes, and which limit this row measures. |
| `pairs[].adapter_a` / `.adapter_b` | The two adapters compared (order matches insertion order into `build_adapters`). |
| `pairs[].attribution` | `"ExactMustMatch"` (both sides exact-scan — any disagreement is adapter skew, asserted) or `"AnnEnvelope"` (at least one side is ANN — divergence is only reported, against a stated envelope). Every row here is `ExactMustMatch`. |
| `pairs[].candidate_jaccard` | Jaccard similarity of the two top-k id sets. `1.0` on every row here. |
| `pairs[].rank_prefix_match` | Length of the longest shared ordered prefix. Equals `limit` (or the pool size, once truncation clips it) on every row here. |
| `pairs[].displacement` | Per shared id, `{id, rank_a, rank_b}` for every id whose rank differs between the two adapters. Empty on every row here. |
| `pairs[].max_score_diff` | Largest absolute score difference over ids present in both answers. `0.0` on every row here — both adapters compute cosine via the identical `crate::embed::cosine` function on identical decoded `f32` vectors, so scores are bit-identical, not just close. |
| `pairs[].exact_match` | `true` iff the two `Vec<Scored<NodeId>>` answers are bit-for-bit equal. The field `ExactMustMatch` rows are asserted against. `true` on every row here — a `false` row would have failed the test, not been recorded quietly. |

**Why this shape survives a third adapter (H2's live Cockroach, H3's pgvector) untouched:**
nothing above names a specific backend in a field. H2/H3 add ROWS — new `adapters` entries, new
`pairs` entries (sqlite-vs-cockroach, memory-oracle-vs-cockroach, …), most of them
`AnnEnvelope` — not new fields. `index_present` is already a plain `bool` any backend's own
index-detection probe (Cockroach's `EXPLAIN`, pgvector's `SET LOCAL enable_indexscan = off`
forced-exact lane) can set the same way. And every score above is already on the shared
`1 − d²/2 ≡ cosine` scale before it reaches this report — that conversion happens inside each
adapter's own `vector_candidates_checked` (SQLite/memory-oracle return `cosine` directly;
Cockroach's `distance_to_score` is `1 − d²/2` on the same output type), so the report never
needs a per-adapter conversion field either.

## This run's numbers

60 pairs total: 2 fixtures × 4 probes × 5 limits (synthetic) + 1 real-embedder corpus × 4
probes × 5 limits, all against the one adapter pair H1 has (sqlite, memory-oracle). Every row:
`attribution: "ExactMustMatch"`, `exact_match: true`, `candidate_jaccard: 1.0`,
`max_score_diff: 0.0`, `displacement: []`. Zero divergence, as the attribution rule requires for
two exact-scan adapters — any row otherwise would have been a failing assertion, not a quietly
recorded number.

## Mutation checks performed against this test (perturb, confirm red, revert)

1. **Reversed sort direction** in the memory oracle's tie-break comparator — turned every
   `ExactMustMatch` pair red (jaccard 0, wrong candidates entirely).
2. **1% order-preserving score skew** (`* 0.99`) — isolated the score-agreement measure: jaccard
   and rank prefix stayed perfect, `max_score_diff` went non-zero, `exact_match` went red.
3. **Off-by-one truncation** (`limit.saturating_sub(1)`) — isolated candidate-set-size
   disagreement: jaccard dropped to 0 at `limit = 1`.
4. **Dropped one limit from the synthetic leg's grid** — caught a real bug in the harness
   itself: the matrix-dimensions assertion was originally `report.pairs.len() >= 40`, which
   stayed green because the optional real-embedder leg's extra pairs papered over the drop.
   Fixed to `assert_eq!` on the synthetic leg's own pair count, checked before the optional leg
   runs.

All four reverted after confirming red.
