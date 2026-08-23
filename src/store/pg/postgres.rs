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

/// Live-test DSN from `LAMBO_POSTGRES_DSN`. Same skip/require contract as
/// the Cockroach live tests: unset without `LAMBO_REQUIRE_LIVE` prints a
/// skip notice; `LAMBO_REQUIRE_LIVE` panics rather than skip-as-green.
#[cfg(test)]
pub(crate) fn postgres_dsn_or_skip(test: &str) -> Option<String> {
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

/// Rewrite the database path of a Postgres DSN, preserving the query string.
#[cfg(test)]
pub(crate) fn dsn_for_database(base: &str, database: &str) -> String {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::lease::{LeaseHolder, LeaseOutcome};
    use crate::store::vector::encode_vector;
    use crate::store::{build_store, GraphStore, StoreConfig, StoreKind};
    use crate::types::{
        AgentId, Concept, ConceptType, EmbeddingContract, Interaction, Mutation, MutationBatch,
        Node, NodeId, SessionId,
    };
    use chrono::{TimeZone, Utc};
    use sqlx::Row;

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
    fn distance_to_score_is_one_minus_d() {
        use PostgresDialect as P;
        // `<=>` returns cosine distance d = 1 - cosine; score 1 - d = cosine.
        assert_eq!(P::distance_to_score(0.0), 1.0);
        assert_eq!(P::distance_to_score(1.0), 0.0);
        assert_eq!(P::distance_to_score(0.5), 0.5);
        assert_eq!(P::distance_to_score(2.0), -1.0);
        assert_eq!(P::distance_to_score(3.0), -1.0, "clamped");
        // Copying Cockroach `1 - d^2/2` onto Postgres goes red: that formula
        // at d=0.5 is 0.875 and at d=1 is 0.5.
        let cockroach_formula = |d: f64| (1.0 - 0.5 * d * d).clamp(-1.0, 1.0);
        assert_ne!(
            P::distance_to_score(0.5),
            cockroach_formula(0.5),
            "copied Cockroach 1 - d^2/2 onto Postgres cosine distance"
        );
        assert_ne!(
            P::distance_to_score(1.0),
            cockroach_formula(1.0),
            "copied Cockroach 1 - d^2/2 onto Postgres cosine distance"
        );
        assert_eq!(P::DISTANCE_OP, "<=>");
        assert_eq!(
            P::forced_exact_scan_sql(),
            Some("SET LOCAL enable_indexscan = off")
        );
    }

    #[test]
    fn recall_sql_pairs_cosine_operator_with_text_and_vector_casts() {
        let store = PostgresStore::new(cfg_with_dim(Some(8))).expect("construct");
        let global = store.vector_candidates_sql();
        let session = store.session_vector_candidates_sql();
        assert!(
            global.contains("<=>"),
            "recall must order by pgvector cosine distance: {global}"
        );
        assert!(
            !global.contains("<->"),
            "must not copy Cockroach L2 into the recall query: {global}"
        );
        assert!(
            global.contains("::vector"),
            "probe must use pgvector's vector cast: {global}"
        );
        assert!(
            !global.contains("::VECTOR"),
            "must not copy Cockroach VECTOR cast: {global}"
        );
        assert!(
            global.contains("::TEXT"),
            "id columns must use TEXT: {global}"
        );
        assert!(
            !global.contains("::STRING"),
            "must not copy Cockroach STRING cast: {global}"
        );
        assert!(session.contains("<=>"), "{session}");
        assert!(!session.contains("<->"), "{session}");
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

    #[test]
    fn with_forced_exact_scan_is_off_by_default() {
        let store = PostgresStore::new(cfg_with_dim(Some(8))).expect("construct");
        assert!(
            !store.forced_exact_scan(),
            "production construction must not force a seq scan"
        );
        let exact = store.with_forced_exact_scan();
        assert!(exact.forced_exact_scan());
    }

    /// B3-R1-1: the camera-proof must share the production SET LOCAL
    /// execute and must not inject the GUC as extra_set on the exact lane.
    #[test]
    fn explain_vector_candidates_uses_store_forced_exact_scan() {
        let camera = include_str!("postgres.rs");
        let base = include_str!("mod.rs");
        assert!(
            camera.contains("store.issue_forced_exact_scan(&mut tx)"),
            "camera-proof must issue the GUC via the production helper"
        );
        assert!(
            base.contains("self.issue_forced_exact_scan(&mut tx)"),
            "vector_candidates_checked must issue the GUC via the shared helper"
        );
        assert!(
            camera.contains("explain_vector_candidates(&exact, None)"),
            "exact-lane EXPLAIN must not pass the GUC as extra_set"
        );
        assert!(
            !camera.contains(
                "explain_vector_candidates(&exact, Some(\"SET LOCAL enable_indexscan = off\"))"
            ),
            "exact-lane EXPLAIN must not inject the GUC as extra_set"
        );
        assert!(
            camera.contains("store.forced_exact_scan()"),
            "camera-proof rustdoc/comments must name the flag the helper reads"
        );
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

    async fn unique_live_store(test: &str, dim: usize) -> Option<PostgresStore> {
        let admin_dsn = postgres_dsn_or_skip(test)?;
        let admin = sqlx::PgPool::connect(&admin_dsn)
            .await
            .unwrap_or_else(|e| panic!("{test}: connect admin DSN: {e}"));
        let db = format!("lambo_b3_{}", uuid::Uuid::new_v4().simple());
        sqlx::query(&format!("CREATE DATABASE {db}"))
            .execute(&admin)
            .await
            .unwrap_or_else(|e| panic!("{test}: create {db}: {e}"));
        let dsn = dsn_for_database(&admin_dsn, &db);
        let store = PostgresStore::new(StoreConfig {
            kind: StoreKind::Postgres,
            dsn: Some(dsn),
            path: None,
            vector_dim: Some(dim),
        })
        .unwrap_or_else(|e| panic!("{test}: construct: {e}"));
        store
            .init_schema()
            .await
            .unwrap_or_else(|e| panic!("{test}: init_schema: {e}"));
        Some(store)
    }

    /// Camera-proof EXPLAIN of the production `vector_candidates` SQL.
    ///
    /// `extra_set` is the inverse GUC that proves hnsw *can* be used
    /// (`enable_seqscan = off`). It is not the forced-exact path. When
    /// `store.forced_exact_scan()` is set, the helper issues
    /// `Dialect::forced_exact_scan_sql` through the same
    /// `PgStore::issue_forced_exact_scan` production search uses. The
    /// exact-lane caller must pass `extra_set = None` (B3-R1-1).
    async fn explain_vector_candidates(store: &PostgresStore, extra_set: Option<&str>) -> String {
        let pool = store.pool().await.expect("pool");
        let dim = store.vector_dimensions().expect("dim");
        let probe = encode_vector(&vec![0.0; dim]).expect("encode probe");
        let sql = store.vector_candidates_sql();
        let mut tx = pool.begin().await.expect("begin explain");
        if let Some(set) = extra_set {
            sqlx::query(set)
                .execute(&mut *tx)
                .await
                .unwrap_or_else(|e| panic!("SET {set}: {e}"));
        }
        store
            .issue_forced_exact_scan(&mut tx)
            .await
            .unwrap_or_else(|e| panic!("forced-exact GUC: {e}"));
        let rows = sqlx::query(&format!("EXPLAIN {sql}"))
            .bind(&probe)
            .bind(5i64)
            .fetch_all(&mut *tx)
            .await
            .unwrap_or_else(|e| panic!("EXPLAIN: {e}"));
        tx.commit().await.expect("commit explain");
        rows.iter()
            .map(|r| r.try_get::<String, usize>(0).expect("explain col"))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Camera-proof: the production recall query can use the hnsw index
    /// created at init. Small tables may prefer a seq scan, so the proof
    /// also EXPLAINs with `enable_seqscan = off` (the inverse of H3's
    /// forced-exact GUC) and asserts `concepts_embedding_idx`.
    #[tokio::test]
    #[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
    async fn explain_recall_uses_hnsw() {
        let Some(store) = unique_live_store("explain_recall_uses_hnsw", 8).await else {
            return;
        };
        let natural = explain_vector_candidates(&store, None).await;
        eprintln!("B3 EXPLAIN (planner's choice):\n{natural}");
        let forced_hnsw =
            explain_vector_candidates(&store, Some("SET LOCAL enable_seqscan = off")).await;
        eprintln!("B3 EXPLAIN (enable_seqscan = off):\n{forced_hnsw}");
        assert!(
            forced_hnsw.contains("concepts_embedding_idx"),
            "recall must be able to use concepts_embedding_idx, got:\n{forced_hnsw}"
        );
        assert!(
            forced_hnsw.to_ascii_lowercase().contains("hnsw")
                || forced_hnsw.contains("concepts_embedding_idx"),
            "forced-index plan must name hnsw or the embedding index, got:\n{forced_hnsw}"
        );

        // Must not pass the GUC as extra_set: the helper reads
        // store.forced_exact_scan() and issues D::forced_exact_scan_sql
        // through the production helper. Passing extra_set here would
        // leave the flag dead (B3-R1-1).
        let exact = store.with_forced_exact_scan();
        let forced_exact = explain_vector_candidates(&exact, None).await;
        eprintln!("B3 EXPLAIN (forced-exact enable_indexscan = off):\n{forced_exact}");
        assert!(
            !forced_exact.contains("concepts_embedding_idx"),
            "forced-exact lane must not use the hnsw index, got:\n{forced_exact}"
        );
    }

    /// Preservation: fencing StaleWrite and idempotent upserts still hold
    /// on the Postgres dialect. Shared PgStore code, proven on this dialect
    /// so B3 does not silently drop them.
    #[tokio::test]
    #[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
    async fn fencing_refuses_stale_write_and_upserts_replay() {
        let Some(store) =
            unique_live_store("fencing_refuses_stale_write_and_upserts_replay", 8).await
        else {
            return;
        };
        let sid = SessionId::from("b3-fence");
        let holder = LeaseHolder {
            agent: AgentId::from("b3"),
            pid: 1,
            host: "test".into(),
            endpoint: None,
        };
        let outcome = store
            .acquire_lease(&sid, &holder, std::time::Duration::from_secs(45))
            .await
            .expect("acquire");
        let LeaseOutcome::Acquired(info) = outcome else {
            panic!("expected Acquired, got {outcome:?}");
        };
        let token = info.token;
        assert!(token > 0, "takeover must mint a fencing token");

        let origin = NodeId::new();
        let concept_id = NodeId::new();
        let ts = Utc.with_ymd_and_hms(2026, 8, 23, 12, 0, 0).unwrap();
        let batch = MutationBatch {
            mutations: vec![
                Mutation::UpsertNode {
                    node: Node::Interaction(Interaction {
                        event_time: None,
                        id: origin,
                        session_id: sid.clone(),
                        agent_id: AgentId::from("b3"),
                        prompt_text: Some("p".into()),
                        previous_id: None,
                        created_at: ts,
                    }),
                },
                Mutation::UpsertNode {
                    node: Node::Concept(Concept {
                        id: concept_id,
                        session_id: sid.clone(),
                        content: "c".into(),
                        canonical_key: "c".into(),
                        concept_type: ConceptType::Entity,
                        origin_interaction: origin,
                        origin_agent: AgentId::from("b3"),
                        created_at: ts,
                        access_count: 0,
                        last_accessed: None,
                        gc_survived: 0,
                        canonization_status: crate::types::CanonizationStatus::None,
                        blast_radius: None,
                        last_demotion_time: None,
                        embedding: None,
                        human_confirmed: 0,
                        chunk_group_id: None,
                    }),
                },
            ],
        };
        store
            .flush(&batch, Some(token))
            .await
            .expect("flush with token");
        store
            .flush(&batch, Some(token))
            .await
            .expect("replayed flush must converge");
        let stale = store.flush(&batch, Some(token.saturating_sub(1))).await;
        assert!(
            matches!(stale, Err(crate::store::StoreError::StaleWrite(_))),
            "stale token must be StaleWrite, got {stale:?}"
        );
        let loaded = store.load_session(&sid).await.expect("load");
        assert_eq!(loaded.concepts.len(), 1, "upsert replay must not duplicate");
        assert_eq!(loaded.concepts[0].id, concept_id);
        assert!(
            loaded.created_at.is_some(),
            "Postgres created_at defaults to now(), the documented divergence from SQLite"
        );

        let contract = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("b3-null".into()),
            dim: 8,
        };
        store
            .flush(
                &MutationBatch {
                    mutations: vec![Mutation::SetEmbedding {
                        session_id: sid.clone(),
                        embedding: Some(contract.clone()),
                    }],
                },
                Some(token),
            )
            .await
            .expect("stamp contract");
        let probe = vec![1.0f32, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0, 0.0];
        let hits = store
            .vector_candidates_checked(&sid, &probe, &contract, 5)
            .await
            .expect("NULL-only quarantine: no vector, empty not error");
        assert!(
            hits.is_empty(),
            "concepts with NULL embeddings must not be candidates, got {hits:?}"
        );
    }
}
