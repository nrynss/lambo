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
//! `distance_to_score` remains B3: a guessed formula would silently mis-rank.
//! [`Dialect::vector_dim`] reads the width that `init_sql` will substitute
//! (config pin, else 1024). [`GraphStore::vector_dimensions`] reporting from
//! the live schema is B4; this file does not close B4.

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

    /// B3 owns this conversion. A stub formula would silently mis-rank, which
    /// is worse than refusing: it would read as a finished dialect. Init and
    /// construction no longer fail closed, so this is the remaining backstop
    /// on the ranking path.
    fn distance_to_score(_dist: f64) -> f64 {
        unimplemented!(
            "PostgresDialect::distance_to_score is B3: <=> is cosine distance \
             and the score is 1 - d; a stub formula would silently mis-rank"
        )
    }

    /// Width substituted *into* the DDL (the inverse of Cockroach's parse-out).
    ///
    /// Source: `[store] vector_dim` when set, otherwise
    /// [`DEFAULT_POSTGRES_VECTOR_DIM`]. `build_store_with_vector_dim` copies
    /// the resolved embedder width into the pin slot when the pin is absent,
    /// so a process that configured `[embedder] dim = 768` inits at 768.
    /// Live-schema reporting is B4.
    fn vector_dim(cfg: &StoreConfig) -> Result<usize, StoreError> {
        let dim = cfg.vector_dim.unwrap_or(DEFAULT_POSTGRES_VECTOR_DIM);
        refuse_over_hnsw_ceiling(dim)?;
        Ok(dim)
    }

    const NAME: &'static str = "postgres";
    const STORE_TYPE_NAME: &'static str = "PostgresStore";
    const DSN_ENV: &'static str = "LAMBO_POSTGRES_DSN";
    const DSN_LABEL: &'static str = "Postgres DSN";

    fn post_init_statements() -> &'static [&'static str] {
        &[
            "ALTER TABLE session_leases \
             ADD COLUMN IF NOT EXISTS current_token BIGINT NOT NULL DEFAULT 0",
            "ALTER TABLE session_leases ADD COLUMN IF NOT EXISTS endpoint TEXT",
        ]
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{build_store, GraphStore, StoreConfig, StoreKind};

    fn cfg_with_dim(dim: Option<usize>) -> StoreConfig {
        StoreConfig {
            kind: StoreKind::Postgres,
            dsn: Some("postgres://u@localhost/lambo".into()),
            path: None,
            vector_dim: dim,
        }
    }

    #[test]
    fn dialect_tokens_are_not_cockroach_sql() {
        assert_ne!(
            PostgresDialect::STRING_CAST,
            "::STRING",
            "copying Cockroach STRING would make a leftover kind=postgres \
             pointed at Cockroach run instead of failing loud"
        );
        assert_ne!(PostgresDialect::VECTOR_CAST, "::VECTOR");
        assert_ne!(
            PostgresDialect::DISTANCE_OP,
            "<->",
            "copying Cockroach L2 would silently mis-rank on pgvector"
        );
        assert_eq!(PostgresDialect::NAME, "postgres");
        assert!(
            PostgresDialect::post_init_statements()
                .iter()
                .any(|s| s.contains("endpoint TEXT")),
            "Postgres converge must use TEXT, not STRING"
        );
        assert!(
            PostgresDialect::post_init_statements()
                .iter()
                .all(|s| !s.contains("STRING")),
            "STRING is Cockroach: {stmts:?}",
            stmts = PostgresDialect::post_init_statements()
        );
    }

    #[test]
    #[should_panic(expected = "B3")]
    fn distance_to_score_does_not_guess_a_formula() {
        let _ = PostgresDialect::distance_to_score(0.5);
    }

    #[test]
    fn init_sql_templates_width_and_creates_hnsw() {
        for dim in [768, 1024, 1536, 2000] {
            let sql = PostgresDialect::init_sql(dim).expect("init_sql");
            assert!(
                !sql.contains(VECTOR_DIM_PLACEHOLDER),
                "placeholder must be substituted at dim {dim}"
            );
            assert!(
                sql.contains(&format!("vector({dim})")),
                "templated width missing at dim {dim}: {sql}"
            );
            assert!(
                !sql.contains("embedding VECTOR(") && !sql.contains("VECTOR INDEX"),
                "must not copy Cockroach VECTOR() at dim {dim}"
            );
            assert!(
                sql.contains("USING hnsw"),
                "hnsw must be present from init at dim {dim}"
            );
            assert!(
                sql.contains("vector_cosine_ops"),
                "hnsw opclass must match <=> at dim {dim}"
            );
            assert!(
                !sql.to_ascii_lowercase().contains("using ivfflat"),
                "ivfflat is rejected at dim {dim}"
            );
            assert!(
                !sql.contains("CREATE VECTOR INDEX"),
                "must not copy Cockroach CREATE VECTOR INDEX at dim {dim}"
            );
            assert!(
                sql.contains("CREATE EXTENSION IF NOT EXISTS vector"),
                "pgvector extension missing at dim {dim}"
            );
        }
    }

    #[test]
    fn template_contains_the_placeholder_exactly_once() {
        assert_eq!(INIT_SQL_TEMPLATE.matches(VECTOR_DIM_PLACEHOLDER).count(), 1);
    }

    #[test]
    fn dim_above_hnsw_ceiling_is_refused_naming_halfvec() {
        for dim in [2001, 3072] {
            let err = PostgresDialect::init_sql(dim).unwrap_err().to_string();
            assert!(err.contains("2000"), "{err}");
            assert!(err.contains("halfvec"), "{err}");
            assert!(err.contains(&dim.to_string()), "{err}");
            assert!(
                err.to_ascii_lowercase().contains("create index")
                    || err.contains("Never let CREATE INDEX"),
                "must say CREATE INDEX is not the discovery: {err}"
            );
            let err = PostgresDialect::vector_dim(&cfg_with_dim(Some(dim)))
                .unwrap_err()
                .to_string();
            assert!(err.contains("2000"), "{err}");
            assert!(err.contains("halfvec"), "{err}");
        }
        let err = PostgresDialect::init_sql(0).unwrap_err().to_string();
        assert!(err.contains("at least 1"), "{err}");
    }

    #[test]
    fn vector_dim_reads_config_and_defaults_to_1024() {
        assert_eq!(
            PostgresDialect::vector_dim(&cfg_with_dim(None)).unwrap(),
            1024
        );
        assert_eq!(
            PostgresDialect::vector_dim(&cfg_with_dim(Some(768))).unwrap(),
            768
        );
        assert_eq!(
            PostgresDialect::vector_dim(&cfg_with_dim(Some(1536))).unwrap(),
            1536
        );
    }

    #[test]
    fn postgres_store_new_constructs_with_a_dsn() {
        let store = PostgresStore::new(cfg_with_dim(Some(768))).expect("construct");
        assert_eq!(store.vector_dimensions(), Some(768));
        let store = PostgresStore::new(cfg_with_dim(None)).expect("construct default dim");
        assert_eq!(store.vector_dimensions(), Some(1024));
    }

    #[test]
    fn postgres_store_new_names_postgres_on_a_missing_dsn() {
        let err = match PostgresStore::new(StoreConfig {
            kind: StoreKind::Postgres,
            dsn: None,
            path: None,
            vector_dim: None,
        }) {
            Ok(_) => panic!("missing DSN must fail"),
            Err(e) => e.to_string(),
        };
        assert!(err.contains("PostgresStore"), "{err}");
        assert!(err.contains("LAMBO_POSTGRES_DSN"), "{err}");
        assert!(
            !err.contains("CockroachStore"),
            "must not launder the Cockroach DSN miss: {err}"
        );
    }

    #[test]
    fn build_store_constructs_a_working_adapter() {
        let s = build_store(cfg_with_dim(Some(1536))).expect("build_store postgres");
        assert!(s
            .capabilities()
            .contains(crate::store::Capabilities::VECTOR_SEARCH));
        assert_eq!(s.vector_dimensions(), Some(1536));
        assert!(StoreKind::Postgres.is_compiled());
        assert!(
            StoreKind::Postgres.is_ready(),
            "is_ready is true once B2 lands a working dialect"
        );
    }

    #[test]
    fn connect_options_do_not_set_cockroach_beam_size() {
        let opts =
            PostgresStore::connect_options("postgres://u@localhost/lambo").expect("parse DSN");
        let debug = format!("{opts:?}");
        assert!(
            !debug.contains("vector_search_beam_size"),
            "Cockroach C-SPANN dial must not be sent to Postgres: {debug}"
        );
        assert!(
            debug.contains("statement_timeout") || debug.contains("20s"),
            "shared statement_timeout must still be applied: {debug}"
        );
    }

    /// Live: `LAMBO_POSTGRES_DSN` against the pinned
    /// `pgvector/pgvector:pg17` digest
    /// `sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.
    fn postgres_dsn_or_skip(test: &str) -> Option<String> {
        match std::env::var("LAMBO_POSTGRES_DSN") {
            Ok(v) if !v.trim().is_empty() => Some(v),
            _ => {
                if std::env::var_os("LAMBO_REQUIRE_LIVE").is_some() {
                    panic!("{test}: LAMBO_POSTGRES_DSN is unset but LAMBO_REQUIRE_LIVE is set");
                }
                eprintln!("{test}: skipped (no LAMBO_POSTGRES_DSN)");
                None
            }
        }
    }

    fn dsn_for_database(base: &str, database: &str) -> String {
        let (stem, query) = match base.split_once('?') {
            Some((s, q)) => (s, Some(q)),
            None => (base, None),
        };
        let Some((prefix, _)) = stem.rsplit_once('/') else {
            panic!("DSN has no database path: {base}");
        };
        match query {
            Some(q) => format!("{prefix}/{database}?{q}"),
            None => format!("{prefix}/{database}"),
        }
    }

    #[tokio::test]
    #[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
    async fn init_schema_at_two_widths_creates_hnsw() {
        let Some(admin_dsn) = postgres_dsn_or_skip("init_schema_at_two_widths_creates_hnsw") else {
            return;
        };
        let admin = sqlx::PgPool::connect(&admin_dsn)
            .await
            .unwrap_or_else(|e| panic!("connect admin DSN: {e}"));
        for dim in [768_usize, 1536] {
            let db = format!("lambo_b2_w{dim}");
            sqlx::query(&format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)"))
                .execute(&admin)
                .await
                .unwrap_or_else(|e| panic!("drop {db}: {e}"));
            sqlx::query(&format!("CREATE DATABASE {db}"))
                .execute(&admin)
                .await
                .unwrap_or_else(|e| panic!("create {db}: {e}"));
            let dsn = dsn_for_database(&admin_dsn, &db);
            let store = PostgresStore::new(StoreConfig {
                kind: StoreKind::Postgres,
                dsn: Some(dsn.clone()),
                path: None,
                vector_dim: Some(dim),
            })
            .unwrap_or_else(|e| panic!("construct dim {dim}: {e}"));
            store
                .init_schema()
                .await
                .unwrap_or_else(|e| panic!("init_schema dim {dim}: {e}"));
            store
                .preflight_schema()
                .await
                .unwrap_or_else(|e| panic!("preflight dim {dim}: {e}"));

            let pool = sqlx::PgPool::connect(&dsn)
                .await
                .unwrap_or_else(|e| panic!("connect {db}: {e}"));
            let amname: String = sqlx::query_scalar(
                "SELECT am.amname \
                 FROM pg_class t \
                 JOIN pg_index i ON i.indrelid = t.oid \
                 JOIN pg_class idx ON idx.oid = i.indexrelid \
                 JOIN pg_am am ON am.oid = idx.relam \
                 WHERE t.relname = 'concepts' \
                   AND idx.relname = 'concepts_embedding_idx'",
            )
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("index lookup dim {dim}: {e}"));
            assert_eq!(amname, "hnsw", "hnsw missing from init at dim {dim}");

            let formatted: String = sqlx::query_scalar(
                "SELECT format_type(atttypid, atttypmod) \
                 FROM pg_attribute \
                 WHERE attrelid = 'concepts'::regclass \
                   AND attname = 'embedding'",
            )
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("typmod dim {dim}: {e}"));
            assert_eq!(
                formatted,
                format!("vector({dim})"),
                "live width at dim {dim}"
            );

            let endpoint_type: String = sqlx::query_scalar(
                "SELECT data_type FROM information_schema.columns \
                 WHERE table_schema = current_schema() \
                   AND table_name = 'session_leases' \
                   AND column_name = 'endpoint'",
            )
            .fetch_one(&pool)
            .await
            .unwrap_or_else(|e| panic!("endpoint type dim {dim}: {e}"));
            assert_eq!(
                endpoint_type, "text",
                "endpoint must be TEXT not STRING at dim {dim}"
            );
        }
    }
}
