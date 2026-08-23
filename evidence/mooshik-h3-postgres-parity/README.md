# H3: the Postgres leg of cross-store recall parity

Closes B3's parity box via the H1 harness (`dev-diary/lambo-for-mooshik/H-cross-store-parity.md`):
the same probe × limit grid and v1 report shape, with postgres-hnsw and postgres-exact
appended to `build_adapters`. Not a second harness.

## What produced this directory

`store::sqlite::tests::h1_cross_store_parity::h3_postgres_recall_parity`
(`src/store/sqlite.rs`). `#[ignore]`d. A run without `LAMBO_POSTGRES_DSN` reports ignored,
never skip-as-green; `LAMBO_REQUIRE_LIVE=1` panics on a missing DSN. Regenerate:

```sh
LAMBO_POSTGRES_DSN='postgres://lambo:lambo@127.0.0.1:5432/lambo?sslmode=disable' \
LAMBO_REQUIRE_LIVE=1 LAMBO_H3_EMIT_EVIDENCE=1 \
  cargo test --no-default-features --features store-postgres,store-sqlite,fixtures \
  --lib store::sqlite::tests::h1_cross_store_parity::h3_postgres_recall_parity \
  -- --ignored --nocapture --exact
```

Image: `pgvector/pgvector:pg17@sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.

## This run

- Run date: 2026-08-23
- Image digest: `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`
- Corpus: H1 synthetic leg, dim 8, both committed fixture graphs
- Adapters: sqlite, memory-oracle, postgres-hnsw, postgres-exact
- 240 pairs (2 fixtures × 4 probes × 5 limits × C(4,2)=6)

EXPLAIN of the production `vector_candidates` SQL (not a lookalike):

```
Index Scan using concepts_embedding_idx on concepts
  Order By: (embedding <=> '[0,0,0,0,0,0,0,0]'::vector)
```

Forced-exact (`SET LOCAL enable_indexscan = off`): `Seq Scan` + `Sort`, no
`concepts_embedding_idx`.

## Numbers

| Pair | cells | min jaccard | max score diff | displacement |
| --- | ---: | ---: | ---: | ---: |
| sqlite vs memory-oracle | 40 | 1.0 | 0 | empty |
| postgres-exact vs sqlite | 40 | 1.0 | 1.21e-7 | empty |
| postgres-exact vs memory-oracle | 40 | 1.0 | 1.21e-7 | empty |
| postgres-hnsw vs postgres-exact | 40 | 1.0 | 0 | empty |
| postgres-hnsw vs sqlite | 40 | 1.0 | 1.21e-7 | empty |

Forced-exact adapter skew: **zero** (jaccard 1, no rank displacement, score
within 1e-4; measured max 1.21e-7, f32 round-trip through pgvector).
hnsw envelope vs forced-exact: **zero divergence** at this corpus size
(9 and 22 vectors). The index is used; at this n it agrees with the seq scan.
A conversion mix-up (`1 - d^2/2` on `<=>`) would land near 0.375 at cosine 0.5,
orders of magnitude above the bound.

Score bound used: `H3_SCORE_SKEW_EPSILON = 1e-4` (same as H2).
