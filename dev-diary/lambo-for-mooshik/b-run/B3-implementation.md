# B3: Postgres dialect surface (casts, `<=>`, `1 - d`), implementation record

**Phase:** B3 (workstream B, `lambo-for-mooshik`).
**Baseline:** B2 closed (round 2 APPROVE, zero residue).
**Tree left dirty and uncommitted**, per the run protocol. Did not merge to
`lambo-for-mooshik`. Did not touch `.env`.

---

## 1. Conversion reasoning

pgvector `<=>` is cosine **distance**: `d = 1 - cosine_similarity`.
`PostgresDialect::distance_to_score` is therefore `1 - d`, clamped to
`[-1, 1]`. That is cosine similarity, the scale `semantic_match_threshold`
is written against.

The clamp absorbs float error at the ends. It does not hide a copied
Cockroach formula: at `d = 0.5` Postgres is `0.5` and Cockroach
`1 - d^2/2` is `0.875`; at `d = 1` Postgres is `0.0` and Cockroach is `0.5`.

Cockroach `<->` is L2. For **unit-norm** embeddings (the `Embedder::embed`
output contract under F), `d_L2^2 = 2 - 2·cosine`, so `1 - d_L2^2/2` equals
cosine. That identity is why the two dialects can agree on scores at all.
It does **not** make the formulas interchangeable:

* Copying `1 - d^2/2` onto Postgres leaves `<=>` returning cosine distance
  and the conversion squaring it. Ranking looks like a model-quality
  problem. No query fails.
* Copying `1 - d` onto Cockroach does the same in the other direction.

pgvector's cosine distance already divides by the product of the norms, so
the Postgres identity does not need unit-norm. Unit-norm is still required
for Cockroach equivalence, and it is still the embedder contract.

Tokens were already B1's PostgreSQL spellings (`::TEXT`, `::vector`, `<=>`).
B3 implements the conversion those tokens must pair with. Cockroach stays
`::STRING`, `::VECTOR`, `<->`, `1 - d^2/2`.

---

## 2. What was built

* `PostgresDialect::distance_to_score = (1 - d).clamp(-1, 1)`. The B2
  `unimplemented!` / `should_panic` backstop is gone.
* `Dialect::forced_exact_scan_sql` (default none). Postgres returns
  `SET LOCAL enable_indexscan = off`. Issued after the contract/PK read and
  before the vector query, inside the existing search transaction, when
  `PgStore::with_forced_exact_scan` is set. Production construction leaves
  the flag off. Not a B3 ranking row: operator and conversion are unchanged.
* H3 extends H1 `build_adapters` (in `src/store/sqlite.rs`). When a DSN is
  offered it appends `postgres-hnsw` (Ann, index allowed) and
  `postgres-exact` (Exact, forced seq scan). The pairwise loop, measures,
  and `ParityReport` v1 schema are unchanged. H1 with `postgres_dsn = None`
  is still sqlite + memory-oracle, 40 synthetic pairs, bit-for-bit.
* Live EXPLAIN of the **production** `vector_candidates` SQL (camera-proof,
  not a lookalike): `Index Scan using concepts_embedding_idx` with
  `Order By: (embedding <=> $1::vector)`. Forced-exact EXPLAIN is
  `Seq Scan` + `Sort`, no embedding index.
* Live preservation on this dialect: fencing `StaleWrite`, idempotent
  upsert replay, `created_at: Some(_)` (the documented Postgres vs SQLite
  divergence), NULL-only quarantine (a concept with no vector is not a
  candidate).
* `postgres-live` CI: keeps the B2 two-width init job; adds EXPLAIN+fencing
  and H3 (`store-postgres,store-sqlite,fixtures`). Image remains the pinned
  digest, never `:latest`.

B4 is not closed. `vector_dimensions()` still echoes the construction dim.

---

## 3. Conversion pins (mutation-proven, restored)

| Mutation | Test | Result |
| --- | --- | --- |
| Postgres body `(1 - d).clamp` → Cockroach `(1 - 0.5*d*d).clamp` | `distance_to_score_is_one_minus_d` | **RED** at `d=1`: left `0.5`, right `0.0` |
| Cockroach body `(1 - 0.5*d*d).clamp` → Postgres `(1 - d).clamp` | `distance_to_score_is_cosine` | **RED** at `d=1`: expected `0.5`, got `0.0` |

Composed recall SQL is pinned separately:
`recall_sql_pairs_cosine_operator_with_text_and_vector_casts` requires
`<=>`, `::vector`, `::TEXT` and rejects `<->`, `::VECTOR`, `::STRING`.
B0 `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` still
holds for Cockroach.

---

## 4. H3 result (measured live)

Pinned image, `docker inspect` Image:
`sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.

DSN: `postgres://lambo:lambo@127.0.0.1:5432/lambo?sslmode=disable`.
Container started for the live tests, then removed. Port 5432 was free.

`h3_postgres_recall_parity` **passed** (0.97s). 240 pairs, 4 adapters,
dim 8, both fixture graphs. Report:
`evidence/mooshik-h3-postgres-parity/report.json`.

Forced-exact vs sqlite / memory-oracle: **zero adapter skew**. Jaccard 1.0,
empty displacement, identical id order. Max score diff **1.21e-7** (f32
round-trip through pgvector) against bound `1e-4`. Bit-for-bit `exact_match`
is 2/40: the other 38 differ in ulps only, which is why ExactMustMatch
against postgres-exact is id-and-order plus the score bound, not `==` on
`f64`. sqlite vs memory-oracle remains bit-for-bit (40/40).

hnsw envelope vs forced-exact: **zero divergence** at this corpus
(session-rest-api 22 concepts, session-drift 9). min_jaccard 1.0,
max_score_diff 0, no displacement, 40/40 bit-for-bit between the two
Postgres lanes. EXPLAIN showed the production query using hnsw; at this n
the index agrees with the seq scan. That is the measured envelope, not a
claim about larger graphs.

---

## 5. What was not built (on purpose)

* Live-schema `vector_dimensions()` (B4).
* halfvec path.
* ANN knobs (`hnsw.ef_search`, `m`, `ef_construction`).
* `store-postgres` in `ship` / `demo`.
* Park-and-fail-over (B-wide, FUTURE.md).
* A second H3 harness (H2's twin pattern). H3 extends H1 `build_adapters`.

---

## 6. Gates

| Gate | Claimed | Measured |
| --- | --- | --- |
| `cargo fmt --all -- --check` | pass | **pass** (rc 0) |
| `cargo clippy --all-targets -- -D warnings` | pass | **pass** (rc 0) |
| `cargo clippy --all-targets --features store-cockroach -- -D warnings` | pass | **pass** (rc 0) |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass (extra) | **pass** (rc 0) |
| `cargo test --features store-cockroach` | 944 listed (B2 close; no new tests on this set) | **pass**. `-- --list` **944**. Summed `test result`: 940 passed / 4 ignored. Lib: 928 passed / 2 ignored. |
| `cargo test --no-default-features --features store-cockroach` | 603 listed | **pass**. `-- --list` **603**. 603 passed / 0 ignored. Lib: 594 passed. |
| `cargo test --features store-cockroach,fixtures` | 1012 listed | **pass**. `-- --list` **1012**. 1000 passed / 12 ignored. Lib: 987 passed / 10 ignored. |
| `cargo test --no-default-features --features store-postgres` | compile + unit; live ignored | **pass**. `-- --list` **591** (+4 vs B2-R2 587: two unit, two ignored live). 588 passed / 3 ignored. Lib: 579 passed / 3 ignored (`init_schema_at_two_widths_creates_hnsw`, `explain_recall_uses_hnsw`, `fencing_refuses_stale_write_and_upserts_replay`). |
| B0 composed-SQL pin | still holds for Cockroach | **pass** `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` |
| H1 lock | `h1_cross_store_parity` green. sqlite.rs may change only by extending `build_adapters` | **pass**. `h1_sqlite_and_memory_oracle_agree_exactly` still 60 pairs, 2 adapters. sqlite.rs changed only in the H1 harness module (no SQLite vector behaviour). |
| H3 live | forced-exact zero adapter skew; hnsw envelope stated | **pass**. See §4. Digest `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`. |
| Live EXPLAIN hnsw | production recall uses the index | **pass**. `Index Scan using concepts_embedding_idx` / `embedding <=> …::vector`. |
| Live fencing / upsert / NULL quarantine | preserved on this dialect | **pass** `fencing_refuses_stale_write_and_upserts_replay` |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | no new pg/postgres warnings | **54** warnings (B0 baseline 54). None naming `PostgresDialect` / `distance_to_score` after dropping a rustdoc link to a `cfg(test)` method. |

---

## 7. Files touched

* `src/store/pg/postgres.rs`: `distance_to_score`, forced-exact SQL, conversion
  and composed-SQL pins, live EXPLAIN and fencing tests
* `src/store/pg/dialect.rs`: `forced_exact_scan_sql` (default none); module
  doc: B3 fills ranking
* `src/store/pg/mod.rs`: `force_exact_scan` flag, `with_forced_exact_scan`,
  SET LOCAL inside `vector_candidates_checked` after the contract read
* `src/store/pg/cockroach.rs`: vice-versa pin on `distance_to_score_is_cosine`
  (no new listed test)
* `src/store/sqlite.rs`: H1 `build_adapters` grows postgres lanes when a DSN
  is offered; `h3_postgres_recall_parity`. SQLite scan/score path unchanged
* `.github/workflows/ci.yml`: postgres-live H3 + EXPLAIN/fencing steps
* `CHANGELOG.md`: ranking conversion note
* `evidence/mooshik-h3-postgres-parity/`: H3 report
* `dev-diary/lambo-for-mooshik/b-run/B3-implementation.md`: this file
