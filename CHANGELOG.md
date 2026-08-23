# Changelog

## Unreleased (0.3.0)

### Breaking

- `store.kind = "postgres"` and `"pg"` now select `StoreKind::Postgres`, not
  CockroachDB. `"cockroach"` and `"crdb"` remain Cockroach. A leftover
  `kind = "postgres"` pointed at a Cockroach cluster fails at provision
  (`CREATE EXTENSION vector`) or first vector query (`<=>`) rather than
  silently ranking with Cockroach SQL.

### Added

- `StoreKind::Postgres` and Cargo feature `store-postgres` (same sqlx postgres
  driver as `store-cockroach`; no second driver). B2 lands templated-width
  pgvector DDL with an hnsw index from init. Dimensions above 2000 are
  refused at init, naming the pgvector hnsw ceiling and the unimplemented
  `halfvec` hatch. `lambo provision` for `kind = "postgres"` runs
  `init_schema` (not `scripts/provision.sh`).
- DSN identity normalisation beside `store_identity`: two spellings of one
  database (`postgres://u@host/db` and `postgres://u@host:5432/db`) derive one
  session endpoint. Password is stripped from the identity so it never reaches
  the filesystem or the lease row.
- `store_is_shareable(Postgres) = true`: a networked store another process can
  open, ruled in the exhaustive match, not defaulted.
- Postgres ranking: pgvector `<=>` (cosine distance) converts with `1 - d`.
  Cockroach stays `<->` L2 and `1 - d^2/2`. Copying either formula onto the
  other dialect is pinned to fail.

### Notes

- B0's extraction already made `crate::store::pg::{PgStore, Dialect}` and
  `crate::store::pg::cockroach::CockroachDialect` public crate API. 0.3.0 is
  the first release that carries them.
