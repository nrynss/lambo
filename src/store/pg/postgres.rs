//! PostgreSQL + pgvector dialect of the Postgres-wire-protocol family.
//!
//! Feature: `store-postgres` (same `sqlx` postgres driver as `store-cockroach`;
//! no second driver). Registered in [`crate::store::build_store`] for
//! [`crate::store::StoreKind::Postgres`].
//!
//! # B2: templated width, hnsw from init
//!
//! [`PostgresDialect::init_sql`] substitutes a configured width into
//! `migrations/postgres/001_init.sql` and creates the hnsw index in the same
//! init. It does **not** copy Cockroach SQL (`VECTOR(n)`, `CREATE VECTOR
//! INDEX`, `STRING`). Width data flow is the inverse of Cockroach: Cockroach
//! parses `VECTOR(n)` *out* of a static file; Postgres writes `vector(n)`
//! *in*.
//!
//! B3 ranking: `<=>` is pgvector cosine distance and
//! [`PostgresDialect::distance_to_score`] is `1 - d` (clamped). Copying
//! Cockroach's L2 formula here (or this formula onto Cockroach) ranks
//! wrongly without failing; the two dialects pin each other. H3 is this
//! row's live parity box.
//!
//! [`Dialect::vector_dim`] reads the width that `init_sql` will substitute
//! (config pin, else 1024). B4 probes live `vector(n)` at init and
//! preflight so reporting is not an echo of the same config value.

use std::borrow::Cow;

use super::{Dialect, PgStore};
use crate::store::{StoreConfig, StoreError};

/// Placeholder in `migrations/postgres/001_init.sql`. Not valid SQL, so
/// applying the file without substitution fails loudly.
const VECTOR_DIM_PLACEHOLDER: &str = "__LAMBO_VECTOR_DIM__";

const INIT_SQL_TEMPLATE: &str = include_str!("../../../migrations/postgres/001_init.sql");

/// pgvector hnsw on type `vector` accepts at most 2000 dimensions. 768 and
/// 1536 pass; Gemini 3072 does not. Enforced in Rust so `CREATE INDEX` is
/// never the discovery. The halfvec hatch (hnsw on `halfvec`, ceiling 4000)
/// is named in the error and is not implemented in B2.
const PGVECTOR_HNSW_VECTOR_MAX_DIM: usize = 2000;

/// Construction default when neither the operator pin nor the embedder width
/// is present. Matches the BGE demo `[embedder] dim`. Not B4's reporting
/// authority.
const DEFAULT_POSTGRES_VECTOR_DIM: usize = 1024;

/// PostgreSQL + pgvector, as a [`Dialect`] of the Postgres-wire-protocol family.
///
/// A zero-sized compile-time selector: it is never constructed, only named as
/// `PgStore<PostgresDialect>`.
pub struct PostgresDialect;

impl Dialect for PostgresDialect {
    fn init_sql(dim: usize) -> Result<Cow<'static, str>, StoreError> {
        refuse_over_hnsw_ceiling(dim)?;
        let hits = INIT_SQL_TEMPLATE.matches(VECTOR_DIM_PLACEHOLDER).count();
        if hits != 1 {
            return Err(StoreError::Invariant(format!(
                "migrations/postgres/001_init.sql must contain the width \
                 placeholder {VECTOR_DIM_PLACEHOLDER} exactly once, found {hits}"
            )));
        }
        let sql = INIT_SQL_TEMPLATE.replace(VECTOR_DIM_PLACEHOLDER, &dim.to_string());
        Ok(Cow::Owned(sql))
    }

    /// PostgreSQL's text type is `TEXT`. Not copied from Cockroach's `STRING`:
    /// if this token ever reached a statement against Cockroach it would fail
    /// loud rather than run the wrong dialect silently. B3's table is the
    /// authority for the spelling.
    const STRING_CAST: &'static str = "::TEXT";

    /// pgvector's dense-vector type is `vector`. Not Cockroach's `VECTOR`.
    const VECTOR_CAST: &'static str = "::vector";

    /// `<=>` is pgvector cosine **distance**. Not Cockroach's `<->` (L2). Using
    /// the Cockroach operator on Postgres would compile and rank by the wrong
    /// metric; using this operator on Cockroach fails at the first vector query.
    const DISTANCE_OP: &'static str = "<=>";

    /// `<=>` is pgvector cosine **distance** (`1 - cosine similarity`).
    /// Score `1 - d` is therefore cosine similarity, the scale
    /// `semantic_match_threshold` is written against.
    ///
    /// This identity does not need unit-norm embeddings: pgvector's cosine
    /// distance already divides by the product of the norms. Unit norm is
    /// still the `Embedder::embed` output contract (documented under F), and
    /// it is what makes Cockroach's L2 conversion `1 - d^2/2` equal this
    /// one. Copying that L2 formula onto this dialect, or this formula onto
    /// Cockroach, ranks wrongly without failing. `distance_to_score_is_one_minus_d`
    /// and Cockroach's `distance_to_score_is_cosine` pin the two formulas apart.
    ///
    /// The clamp absorbs float error at the ends rather than widening the
    /// range past `[-1, 1]`.
    fn distance_to_score(dist: f64) -> f64 {
        (1.0 - dist).clamp(-1.0, 1.0)
    }

    /// Width substituted *into* the DDL (the inverse of Cockroach's parse-out).
    ///
    /// Source: `[store] vector_dim` when set, otherwise the private
    /// `DEFAULT_POSTGRES_VECTOR_DIM` (1024). Named rather than linked: it is a
    /// private item, and a public doc comment linking to one is the single new
    /// warning the private-item doc gate exists to catch (E2E-F7).
    /// `build_store_with_vector_dim` copies
    /// the resolved embedder width into the pin slot when the pin is absent,
    /// so a process that configured `[embedder] dim = 768` inits at 768.
    /// B4 then checks this number against live `vector(n)` at init and attach.
    fn vector_dim(cfg: &StoreConfig) -> Result<usize, StoreError> {
        let dim = cfg.vector_dim.unwrap_or(DEFAULT_POSTGRES_VECTOR_DIM);
        refuse_over_hnsw_ceiling(dim)?;
        Ok(dim)
    }

    fn live_schema_vector_width_sql() -> Option<&'static str> {
        Some(
            "SELECT format_type(atttypid, atttypmod) \
             FROM pg_attribute \
             WHERE attrelid = 'concepts'::regclass \
               AND attname = 'embedding' \
               AND NOT attisdropped",
        )
    }

    const NAME: &'static str = "postgres";
    const STORE_TYPE_NAME: &'static str = "PostgresStore";
    const DSN_ENV: &'static str = "LAMBO_POSTGRES_DSN";
    const DSN_LABEL: &'static str = "Postgres DSN";
    // Cloud SQL IAM database auth: the shared-service-account path (`LAMBO_POSTGRES_IAM`).
    const SUPPORTS_CLOUD_SQL_IAM_AUTH: bool = true;

    fn post_init_statements() -> &'static [&'static str] {
        &[
            "ALTER TABLE session_leases \
             ADD COLUMN IF NOT EXISTS current_token BIGINT NOT NULL DEFAULT 0",
            "ALTER TABLE session_leases ADD COLUMN IF NOT EXISTS endpoint TEXT",
        ]
    }

    fn forced_exact_scan_sql() -> Option<&'static str> {
        Some("SET LOCAL enable_indexscan = off")
    }
}

fn refuse_over_hnsw_ceiling(dim: usize) -> Result<(), StoreError> {
    if dim == 0 {
        return Err(StoreError::Backend(
            "PostgresDialect vector width must be at least 1".into(),
        ));
    }
    if dim > PGVECTOR_HNSW_VECTOR_MAX_DIM {
        return Err(StoreError::Backend(format!(
            "PostgresDialect refuses dim {dim}: pgvector hnsw on type vector \
             supports at most {PGVECTOR_HNSW_VECTOR_MAX_DIM} dimensions \
             (768 and 1536 pass; Gemini 3072 does not). The halfvec hatch \
             (hnsw on halfvec, ceiling 4000) is not implemented. Set \
             store.vector_dim to {PGVECTOR_HNSW_VECTOR_MAX_DIM} or less. \
             Never let CREATE INDEX discover this."
        )));
    }
    Ok(())
}

/// The durable PostgreSQL adapter.
pub type PostgresStore = PgStore<PostgresDialect>;

// The live-test harness (DSN skip/require, DSN rewrite, the vector corpus and
// its EXPLAIN helpers) is test-only and shared with the SQLite H1/H3 parity
// tests, which import it from this module's path.
#[cfg(test)]
mod test_support;
#[cfg(test)]
pub(crate) use test_support::{corpus, dsn_for_database, postgres_dsn_or_skip};

#[cfg(test)]
mod tests;
