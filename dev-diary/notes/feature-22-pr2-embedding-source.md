# #22 PR 2: persist a concept's embedding source (decisions)

Base: main `b3477db` (includes PR 1). Design of record:
`lambo-handoff-2026-10-08/design/22-DESIGN.md` (approved 2026-10-09), sections
4.1, 4.3, 4.5, 10, 11 and the PR 2 row of section 12. This is the second of five
PRs. It adds the field and the column on every store. No write path sets the
field yet, so behaviour does not change.

## What changed

| piece | where |
|---|---|
| `Concept.embedding_source`, `EmbeddingSource`, `SourceModality`, `VectorOrigin`, `ImageMimeWire`, the column codec `to_column` / `from_column` | `src/types/mod.rs`, re-exported from `src/lib.rs` |
| `embedding_source: None` in every `Concept` literal (mechanical; all build new concepts) | 44 files |
| SQLite column: DDL, guarded `ensure_column` ALTER, bind, positional load (`try_get(17)`), concept chunk 58 to 55 | `migrations/sqlite/001_init.sql`, `src/store/sqlite/{schema,write_rows,session_load,persistence}.rs` |
| Postgres and Cockroach column: DDL plus `ADD COLUMN IF NOT EXISTS`, upsert column list and conflict update, select, decode by name | `migrations/{postgres,cockroach}/001_init.sql`, `src/store/pg/{sql,codec}.rs` |
| `CONCEPT_COLUMNS` 17 to 18 | `src/store/batch.rs` |
| shared round-trip check | `src/store/embedding_source_testkit.rs` |
| erase census plants a source | `src/store/erase.rs` (testkit) |

## Decisions

**Compact JSON in one nullable text column** (design §4.3). `TEXT` on SQLite
and Postgres, `STRING` on Cockroach. A pre-#22 row reads `NULL`, which is true:
every concept written before #22 was embedded from its own content.

**An unreadable value fails the load.** `EmbeddingSource` is
`deny_unknown_fields`, and a value that does not decode (bad JSON, an unknown
key, an unknown modality or MIME such as a newer build would write) is
`StoreError::Invariant` naming the concept. Reading it as `None` would make an
image concept look text-embedded, and `lambo re-embed` would then replace its
image vector with a vector of its caption without any error. This matches
design R5: a downgrade is loud.

**A separate `ImageMimeWire`** (design §4.3 names it). Its serde spelling is the
MIME string (`"image/png"`), with `From` conversions both ways to
`embed::ImageMime`. The embed API stays free of serde.

**The column rides the conflict update.** It describes `embedding`, which the
whole-record upsert already rewrites, so an upsert that nulls an image vector
*and* sets its source to `None` (PR 3's `re-embed --drop-image-vectors`)
clears the column. The #30 narrow `RecordAccess` update writes two columns and
never touches it. The shared check proves both on every adapter.

**The embedding quarantine keeps the source** (review L1, decided). The
quarantine (`UPDATE concepts SET embedding = NULL`, run on a first contract
stamp on every adapter and on a width restamp on SQLite; the Memory adapter's
`SetEmbedding` matches) nulls the vector only. A quarantined image concept is
still an image concept, and keeping its source stops a later
`re-embed --missing-only` from giving it a vector of its caption. So a concept
can load with `embedding_source = Some(..)` and `embedding = None`; PR 3 must
treat that as "image vector missing", never as a text concept. The shared
check restamps the contract and asserts the vectors are gone and the sources
kept, on every adapter; a SQLite test covers the width restamp.

**SQLite concept chunk 58 to 55.** 58 × 18 = 1044 breaches the conservative
999-bind ceiling the R1-4 const-assert guards; 55 × 18 = 990. The pg family's
256-row chunk is 4,608 binds, far under 65,535.

**Upgrade needs `lambo provision`.** Attach runs only the column preflight
(J3-R2R-3), so an existing store is refused by name until `init_schema` adds
the column. This is the same path `human_confirmed` took. The dogfood rig pins
`lambo-0.3.0`, so it is unaffected until a re-pin, which must provision first.

## Tests

| test | store | runs in |
|---|---|---|
| `types::tests::embedding_source_*`, `concept_embedding_source_is_serde_default_and_skipped_when_none`, `image_mime_wire_converts_both_ways` | none (serde) | every row |
| `store::memory::tests::embedding_source_survives_the_flush_load_round_trip` | Memory | every row with `store-memory` |
| `store::sqlite::tests::persistence::embedding_source_survives_the_flush_load_round_trip`, `an_unreadable_embedding_source_fails_the_load` | SQLite | CI |
| `store::sqlite::tests::schema::init_schema_converges_a_pre_22_store_without_embedding_source` (preflight refuses by name, `init_schema` converges, old row reads `None`) | SQLite | CI |
| `store::pg::cockroach::tests::sql_shapes::embedding_source_rides_the_concept_upsert_and_select_shape`, plus the re-pinned `b0_composed_sql_is_byte_identical_to_the_pre_carve_constants` and `sql_shape_is_a_multi_row_upsert` | Cockroach SQL text | `store-cockroach` builds only: no CI row runs them, so run `cargo test --features store-cockroach,fixtures --lib store::pg::cockroach` after any concept column change |
| `store::pg::embedding_source_live::postgres_round_trips_the_embedding_source` | live Postgres, `#[ignore]` | not in `ci.yml` yet: the `postgres-live` job runs tests by name, and adding the step is a workflow edit for the owner (diff below) |
| `conformance_suite` → `check_embedding_source_survives_flush_load` | live Cockroach | by hand only: `cockroach-live` is disabled (`if: false`, 2026-10-06) |
| the #23 erase tests on SQLite, Memory and Postgres | all | the planted vectored concept now carries a source; the census still ends at zero rows |

No Postgres convergence test drops the column on the shared CI database (a
`DROP COLUMN` would race the other live tests in the step). `ADD COLUMN IF NOT
EXISTS` runs on every `init_schema` against an existing database, where it is a
no-op.

## Fixture JSON and wire shapes

Unchanged. No fixture, golden or MCP schema file is touched: the field is
serde-defaulted and skipped when `None`, and every concept a test or a fixture
writes has `None`.

## Impact on in-flight work

- **#18 Elastic tier (`feat/18-elastic-tier`).** `store::tiered::project::index_doc`
  copies named fields into an `IndexDoc`; it does not project a concept
  exhaustively, so `embedding_source` needs no ignore list. On rebase the
  branch's two `Concept` literals (`src/store/tiered/project.rs` and
  `src/store/tiered/tests.rs`) need `embedding_source: None` to compile.
- **#23 erase.** No code change (design §4.5): the column is on the concept row.

## Not in this PR

- Setting the field: `SuppliedVector`, `derive_image_as`, merge exclusion and
  the `re-embed` rules are PR 3.
- MCP, CLI and stats surface: PR 4. The EmbeddingGemma 2 adapter: PR 5.

## The `postgres-live` step to add

```yaml
      # #22 PR 2: a concept's embedding_source on live PostgreSQL.
      - name: Live Postgres embedding source round trip
        run: |
          set -o pipefail
          cargo test --no-default-features --features store-postgres --lib \
            -- --ignored --nocapture --exact \
            store::pg::embedding_source_live::postgres_round_trips_the_embedding_source \
            2>&1 | tee postgres-embedding-source.log
          grep -q 'test store::pg::embedding_source_live::postgres_round_trips_the_embedding_source ... ok' postgres-embedding-source.log
```
