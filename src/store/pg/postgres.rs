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

/// A deterministic corpus of unit vectors, and the rows to hold them.
///
/// # Why the live proofs need this at all (E2E-F3 / E2E-F4)
///
/// Both the `EXPLAIN` box and H3's hnsw envelope used to be measured on a
/// corpus of 0, 9 or 22 rows. On a table that small the planner prefers a
/// sequential scan whatever the index offers, and hnsw returns the exact
/// answer because it visits everything, so neither instrument could move: the
/// `EXPLAIN` capture proved only that the index is *usable* under
/// `enable_seqscan = off`, and the envelope was a comparison of two identical
/// answers. A corpus above the planner's crossover is what makes both of them
/// measurements.
#[cfg(test)]
pub(crate) mod corpus {
    use super::PostgresStore;
    use sqlx::Row;

    /// Rows above which the planner picks `concepts_embedding_idx` over a
    /// sequential scan for the production recall query.
    ///
    /// Measured on the pinned `pgvector/pgvector:pg17` digest at dim 8: 100
    /// rows still plans a `Seq Scan`, 500 rows plans an
    /// `Index Scan using concepts_embedding_idx`. Callers seed a multiple of
    /// this so the assertion is not a coin flip on a slightly different
    /// `ANALYZE`.
    pub(crate) const PLANNER_CROSSOVER_ROWS: usize = 500;

    /// Deterministic unit vectors: a corpus that reproduces run to run, so an
    /// envelope printed as evidence is a number someone else can obtain again.
    ///
    /// `splitmix64` seeded from the row index, not a stateful RNG, so row `i`
    /// is the same vector regardless of what ran between two calls.
    pub(crate) fn unit_vector(i: usize, dim: usize) -> Vec<f32> {
        let mut state = (i as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) ^ 0x2545_f491_4f6c_dd1d;
        let mut next = || {
            state = state.wrapping_add(0x9e37_79b9_7f4a_7c15);
            let mut z = state;
            z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
            z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
            z ^= z >> 31;
            (z >> 11) as f64 / (1u64 << 52) as f64 * 2.0 - 1.0
        };
        let mut v: Vec<f32> = (0..dim).map(|_| next() as f32).collect();
        let norm = v.iter().map(|x| x * x).sum::<f32>().sqrt();
        // A zero vector is a `NaN` distance under `<=>`. The generator cannot
        // produce one at any realistic dim; fall back rather than divide by it.
        if norm == 0.0 {
            v[0] = 1.0;
        } else {
            for x in &mut v {
                *x /= norm;
            }
        }
        v
    }

    /// pgvector's text input form for one vector.
    fn vector_literal(v: &[f32]) -> String {
        let mut out = String::with_capacity(v.len() * 12 + 2);
        out.push('[');
        for (i, x) in v.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            out.push_str(&x.to_string());
        }
        out.push(']');
        out
    }

    const SESSION_SQL: &str = "INSERT INTO sessions (session_id, root_goal, created_at, embedding_kind, embedding_model, embedding_dim) VALUES ($1, '{}'::jsonb, now(), $3, $4, $2) ON CONFLICT (session_id) DO NOTHING";

    const INTERACTION_SQL: &str = "INSERT INTO interactions (id, session_id, agent_id, prompt_text, created_at) VALUES ($1, $2, 'corpus', 'seed', now())";

    const CONCEPT_BATCH_SQL: &str = "INSERT INTO concepts (id, session_id, content, canonical_key, concept_type, origin_interaction, origin_agent, created_at, embedding) SELECT u.id, $4, 'corpus', u.key, 'Observation', $5, 'corpus', now(), u.emb::vector FROM UNNEST($1::uuid[], $2::text[], $3::text[]) AS u(id, key, emb)";

    /// Seed `rows` deterministic unit vectors into a live store's `concepts`
    /// table under `session`, returning `(id, vector)` so a caller can compute
    /// the exact answer for itself.
    ///
    /// Written with `UNNEST` batches rather than the `flush()` path on purpose:
    /// the subject of these proofs is the **recall query's plan and answer**,
    /// and the write path has its own tests. Seeding thousands of rows through
    /// `flush()` would put minutes of unrelated work in front of a measurement
    /// that does not depend on it.
    ///
    /// The session's embedding contract is written in the shape
    /// `vector_candidates_checked` reads back, so a caller can measure through
    /// the production entry point rather than raw SQL.
    pub(crate) async fn seed(
        store: &PostgresStore,
        session: &str,
        contract: &crate::types::EmbeddingContract,
        rows: usize,
    ) -> Vec<(uuid::Uuid, Vec<f32>)> {
        let dim = contract.dim;
        let pool = &store.pool().await.expect("corpus: pool");
        sqlx::query(SESSION_SQL)
            .bind(session)
            .bind(dim as i64)
            .bind(&contract.kind)
            .bind(contract.model.as_deref())
            .execute(pool)
            .await
            .expect("corpus: session row");
        let origin = uuid::Uuid::new_v4();
        sqlx::query(INTERACTION_SQL)
            .bind(origin)
            .bind(session)
            .execute(pool)
            .await
            .expect("corpus: interaction row");

        let corpus: Vec<(uuid::Uuid, Vec<f32>)> = (0..rows)
            .map(|i| (uuid::Uuid::new_v4(), unit_vector(i, dim)))
            .collect();
        for (chunk_index, chunk) in corpus.chunks(500).enumerate() {
            let base = chunk_index * 500;
            let ids: Vec<uuid::Uuid> = chunk.iter().map(|(id, _)| *id).collect();
            let keys: Vec<String> = (0..chunk.len()).map(|i| format!("k{}", base + i)).collect();
            let embeddings: Vec<String> = chunk.iter().map(|(_, v)| vector_literal(v)).collect();
            sqlx::query(CONCEPT_BATCH_SQL)
                .bind(&ids)
                .bind(&keys)
                .bind(&embeddings)
                .bind(session)
                .bind(origin)
                .execute(pool)
                .await
                .expect("corpus: concept batch");
        }
        // Without this the planner costs against a stale `pg_class` estimate
        // and keeps choosing a sequential scan however many rows are really
        // there, which would make the plan assertion a test of autovacuum's
        // schedule.
        sqlx::query("ANALYZE concepts")
            .execute(pool)
            .await
            .expect("corpus: ANALYZE");
        corpus
    }

    /// EXPLAIN the production recall SQL on `store`, through the same
    /// forced-exact path production search uses, and report whether the
    /// planner's **natural** choice named `concepts_embedding_idx`.
    ///
    /// This is what turns H3's `index_present` from a hardcoded literal into a
    /// measurement: with `forced_exact_scan_sql()` returning `None`, the
    /// "exact" lane's plan names the index and the probe says so.
    /// Only the H3 parity harness calls this, and that harness lives behind
    /// `store-sqlite` because it compares the two engines against SQLite and
    /// the memory oracle. Under `store-postgres` alone it is genuinely
    /// uncalled, so the allow is scoped to exactly that combination rather
    /// than blanket-silencing dead code on every build.
    #[cfg_attr(not(feature = "store-sqlite"), allow(dead_code))]
    pub(crate) async fn index_present(store: &PostgresStore, probe: &[f32], limit: i64) -> bool {
        plan(store, probe, limit)
            .await
            .contains("concepts_embedding_idx")
    }

    /// A plan with its vector literal elided, for printing.
    ///
    /// A dim-768 probe renders as roughly ten kilobytes of floats inside the
    /// `Order By` line, twice per capture. Useful in none of the cases where
    /// someone reads this output.
    pub(crate) fn elide_vector_literals(plan: &str) -> String {
        let mut out = String::with_capacity(plan.len());
        let mut rest = plan;
        while let Some(open) = rest.find("'[") {
            out.push_str(&rest[..open]);
            let after = &rest[open + 2..];
            match after.find("]'") {
                Some(close) => {
                    let inner = &after[..close];
                    let dims = inner.split(',').count();
                    out.push_str(&format!("'[<{dims} floats>]'"));
                    rest = &after[close + 2..];
                }
                None => {
                    out.push_str(&rest[open..]);
                    return out;
                }
            }
        }
        out.push_str(rest);
        out
    }

    /// The natural plan text for the production recall SQL, with no GUC beyond
    /// the store's own forced-exact setting.
    pub(crate) async fn plan(store: &PostgresStore, probe: &[f32], limit: i64) -> String {
        let pool = &store.pool().await.expect("plan: pool");
        let encoded = crate::store::vector::encode_vector(probe).expect("plan: encode probe");
        let sql = store.vector_candidates_sql();
        let mut tx = pool.begin().await.expect("plan: begin");
        store
            .issue_forced_exact_scan(&mut tx)
            .await
            .expect("plan: forced-exact GUC");
        let rows = sqlx::query(&format!("EXPLAIN {sql}"))
            .bind(&encoded)
            .bind(limit)
            .fetch_all(&mut *tx)
            .await
            .expect("plan: EXPLAIN");
        tx.commit().await.expect("plan: commit");
        rows.iter()
            .map(|r| r.try_get::<String, usize>(0).expect("plan: col"))
            .collect::<Vec<_>>()
            .join("\n")
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

    /// E2E-F2: the variable the missing-DSN error tells an operator to set is
    /// the variable configuration resolution actually reads for this kind. It
    /// used to be neither: `LAMBO_POSTGRES_DSN` was printed here and read
    /// nowhere, while `LAMBO_COCKROACH_DSN` selected the database.
    #[test]
    fn dsn_env_named_in_errors_is_the_one_config_reads() {
        assert_eq!(
            PostgresDialect::DSN_ENV,
            crate::store::POSTGRES_DSN_ENV,
            "the dialect's operator-facing DSN variable and the one \
             StoreConfig::dsn_from_env_for_kind consults must be one string"
        );
        assert_eq!(
            StoreKind::Postgres.dsn_env(),
            Some(PostgresDialect::DSN_ENV),
            "kind -> env var mapping must agree with the dialect"
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
            assert_eq!(
                crate::store::pg::parse_pgvector_format_type(&formatted),
                Some(dim),
                "parser must agree with live format_type at dim {dim}"
            );
            assert_eq!(
                store.vector_dimensions(),
                Some(dim),
                "reporting must match live schema at dim {dim}"
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

    #[test]
    fn parse_pgvector_format_type_reads_vector_n() {
        use crate::store::pg::parse_pgvector_format_type as parse;
        assert_eq!(parse("vector(768)"), Some(768));
        assert_eq!(parse("vector(1536)"), Some(1536));
        assert_eq!(parse("  vector(8)  "), Some(8));
        assert_eq!(
            parse("VECTOR(768)"),
            None,
            "Cockroach spelling is not pgvector"
        );
        assert_eq!(parse("vector"), None);
        assert_eq!(parse("text"), None);
        assert_eq!(parse("vector(x)"), None);
        assert!(PostgresDialect::live_schema_vector_width_sql().is_some());
    }

    #[tokio::test]
    #[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
    async fn live_schema_width_refuses_a_config_that_disagrees() {
        let Some(store768) =
            unique_live_store("live_schema_width_refuses_a_config_that_disagrees", 768).await
        else {
            return;
        };
        assert_eq!(store768.vector_dimensions(), Some(768));
        let dsn = store768.dsn().to_string();
        let wrong = PostgresStore::new(StoreConfig {
            kind: StoreKind::Postgres,
            dsn: Some(dsn),
            path: None,
            vector_dim: Some(1536),
        })
        .expect("construction is I/O-free; mismatch is on attach");
        assert_eq!(wrong.vector_dimensions(), Some(1536));
        let err = wrong
            .preflight_schema()
            .await
            .expect_err("1536 process against vector(768) must fail")
            .to_string();
        assert!(err.contains("768"), "{err}");
        assert!(err.contains("1536"), "{err}");
        assert!(
            err.to_ascii_lowercase().contains("live schema") || err.contains("vector(768)"),
            "must name the live schema, not only the pin: {err}"
        );
    }

    /// Shared-SA live proof: with `LAMBO_POSTGRES_IAM=1`, `GCP_LAMBO_CREDENTIALS` and a
    /// Cloud SQL IAM DSN (database user = the service account, NO password in the URL),
    /// the store's `pool()` mints the SA access token and authenticates to Cloud SQL as
    /// the service account. Run:
    ///
    ///   LAMBO_POSTGRES_IAM=1 \
    ///   GCP_LAMBO_CREDENTIALS=/path/sa.json \
    ///   LAMBO_POSTGRES_DSN='postgresql://cachy-nryn%40mooshik.iam@HOST:5432/lambo?sslmode=require' \
    ///   cargo test --features store-postgres,fixtures --lib \
    ///     store::pg::postgres::tests::iam_auth_connects_as_service_account -- --ignored
    #[ignore = "live: LAMBO_POSTGRES_IAM + GCP_LAMBO_CREDENTIALS + LAMBO_POSTGRES_DSN (Cloud SQL IAM user)"]
    #[tokio::test]
    async fn iam_auth_connects_as_service_account() {
        let Some(dsn) = postgres_dsn_or_skip("iam_auth_connects_as_service_account") else {
            return;
        };
        if std::env::var_os("LAMBO_POSTGRES_IAM").is_none() {
            eprintln!("skipped: LAMBO_POSTGRES_IAM must be set");
            return;
        }
        let store = PostgresStore::new(StoreConfig {
            kind: StoreKind::Postgres,
            dsn: Some(dsn),
            path: None,
            vector_dim: Some(8),
        })
        .expect("construct");
        let pool = &store.pool().await.expect("pool");
        let user: String = sqlx::query_scalar("SELECT current_user")
            .fetch_one(pool)
            .await
            .expect("query current_user");
        eprintln!("IAM authenticated to Postgres as: {user}");
        assert!(
            user.contains('@') && !user.is_empty(),
            "expected an IAM service-account login, got {user:?}"
        );
    }

    /// The IAM token is a password with an expiry, so the pool that carries it has one too.
    ///
    /// Offline, and it never connects: `connect_lazy_with` opens nothing, so a DSN pointing
    /// at a host that does not exist still exercises the whole mint-and-rotate path. The
    /// mock OAuth endpoint's hit count is the pin. One mint for the first pool, still one
    /// while the token is live, and a second the moment it lapses. Before this, the token
    /// was minted once at pool creation and a `serve` outlived it.
    /// The IAM token is a password with an expiry, so the pool that carries it has one too.
    ///
    /// Offline, and it never connects: `connect_lazy_with` opens nothing, so a DSN pointing
    /// at a host that does not exist still exercises the whole mint-and-rotate path. The
    /// mock OAuth endpoint's hit count is the pin. One mint for the first pool, still one
    /// while the token is live, and a second the moment it lapses. Before this, the token
    /// was minted once at pool creation and a `serve` outlived it.
    ///
    /// The env lock is held only across the **synchronous** construction (spec §6.4: no
    /// lock across an await), which is exactly the window in which the opt-in is read.
    #[tokio::test]
    async fn the_iam_pool_is_rebuilt_when_its_token_expires() {
        use httpmock::prelude::*;
        let server = MockServer::start();
        // `expires_in` 61s leaves a 1s TTL after the token source's 60s refresh margin.
        let mint = server.mock(|when, then| {
            when.method(POST).path("/token");
            then.status(200)
                .json_body(serde_json::json!({ "access_token": "tok", "expires_in": 61 }));
        });
        let dir = std::env::temp_dir().join(format!("lambo-iam-rotate-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("scratch dir");
        let creds = dir.join("sa.json");
        std::fs::write(
            &creds,
            serde_json::json!({
                "type": "service_account",
                "client_email": "sa@example.com",
                "private_key": crate::gcp_auth::TEST_RSA_PRIVATE_KEY_PEM,
                "project_id": "mooshik",
                "token_uri": format!("{}/token", server.base_url()),
            })
            .to_string(),
        )
        .expect("write credentials");

        let store = with_iam_env(Some(&creds), || {
            PostgresStore::new(StoreConfig {
                kind: StoreKind::Postgres,
                dsn: Some("postgres://cachy-nryn%40mooshik.iam@127.0.0.1:1/lambo".into()),
                path: None,
                vector_dim: Some(8),
            })
            .expect("construct")
        });

        store.pool().await.expect("first pool");
        store.pool().await.expect("cached pool");
        mint.assert_hits(1);

        tokio::time::sleep(std::time::Duration::from_millis(1200)).await;
        store.pool().await.expect("rotated pool");
        mint.assert_hits(2);

        std::fs::remove_file(&creds).ok();
    }

    /// Opting in without a credential file is named, not a panic and not a silent
    /// password-path fallback: a deployment that thinks it is authenticating as the shared
    /// service account must never quietly authenticate as something else.
    #[tokio::test]
    async fn iam_without_credentials_fails_closed_naming_the_variables() {
        let store = with_iam_env(None, || {
            PostgresStore::new(StoreConfig {
                kind: StoreKind::Postgres,
                dsn: Some("postgres://u@127.0.0.1:1/lambo".into()),
                path: None,
                vector_dim: Some(8),
            })
            .expect("construct")
        });
        let err = store.pool().await.expect_err("must refuse").to_string();
        assert!(err.contains("LAMBO_POSTGRES_IAM"), "{err}");
        assert!(err.contains("GCP_LAMBO_CREDENTIALS"), "{err}");
    }

    /// Run `build` with the IAM opt-in set (and `credentials` pointing where the caller
    /// says, or nowhere), restoring the environment before returning. Synchronous by
    /// design: the store reads the opt-in during construction, so the lock never has to
    /// span an await.
    fn with_iam_env<T>(credentials: Option<&std::path::Path>, build: impl FnOnce() -> T) -> T {
        let _g = crate::test_util::env_lock();
        let prev_iam = std::env::var_os("LAMBO_POSTGRES_IAM");
        let prev_gcp = std::env::var_os("GCP_LAMBO_CREDENTIALS");
        let prev_adc = std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS");
        std::env::set_var("LAMBO_POSTGRES_IAM", "1");
        match credentials {
            Some(path) => std::env::set_var("GCP_LAMBO_CREDENTIALS", path),
            None => std::env::remove_var("GCP_LAMBO_CREDENTIALS"),
        }
        std::env::remove_var("GOOGLE_APPLICATION_CREDENTIALS");
        let built = build();
        match prev_iam {
            Some(v) => std::env::set_var("LAMBO_POSTGRES_IAM", v),
            None => std::env::remove_var("LAMBO_POSTGRES_IAM"),
        }
        match prev_gcp {
            Some(v) => std::env::set_var("GCP_LAMBO_CREDENTIALS", v),
            None => std::env::remove_var("GCP_LAMBO_CREDENTIALS"),
        }
        if let Some(v) = prev_adc {
            std::env::set_var("GOOGLE_APPLICATION_CREDENTIALS", v);
        }
        built
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
    async fn explain_vector_candidates(
        store: &PostgresStore,
        probe: &[f32],
        extra_set: Option<&str>,
    ) -> String {
        let pool = &store.pool().await.expect("pool");
        let probe = encode_vector(probe).expect("encode probe");
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

    /// **Acceptance (B's Done-when box 4)**: an `EXPLAIN` capture proves the
    /// hnsw index is actually used by the recall query.
    ///
    /// # What this used to prove, and why that was nothing (E2E-F4)
    ///
    /// The previous version EXPLAINed on the table `init_schema` had just
    /// created: **zero rows**, with a probe of `vec![0.0; dim]`. On pgvector a
    /// zero vector gives `NaN` for `<=>` against every row, and the only
    /// load-bearing assertion was taken under `SET LOCAL enable_seqscan = off`.
    /// That GUC penalises the *alternative*, so it establishes that the index
    /// is **usable**, not that the planner would choose it: a query that had
    /// degraded to a sequential scan in production passed the test unchanged.
    /// On an empty table there is nothing for the planner to have an opinion
    /// about in the first place.
    ///
    /// # What it proves now
    ///
    /// 1. The corpus is seeded past the measured planner crossover, with real
    ///    non-zero unit vectors, and `ANALYZE`d.
    /// 2. The probe is a vector from the corpus, so `<=>` is finite and the
    ///    query has a real answer. Asserted, not assumed.
    /// 3. The **natural** plan (no GUC at all) names
    ///    `concepts_embedding_idx`. Remove the seeding and this fails: at 0 and
    ///    at 100 rows the same query plans a `Seq Scan`.
    /// 4. The forced-exact lane, through the production
    ///    `issue_forced_exact_scan` path, plans a `Seq Scan` and does **not**
    ///    name the index. Return `None` from `forced_exact_scan_sql()` and this
    ///    fails, because the planner then picks the index for that lane too.
    ///
    /// The `enable_seqscan = off` capture is kept, printed, and no longer
    /// asserted on: it is a diagnostic for a failure, not the proof.
    #[tokio::test]
    #[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
    async fn explain_recall_uses_hnsw() {
        const DIM: usize = 8;
        let rows = corpus::PLANNER_CROSSOVER_ROWS * 4;
        let Some(store) = unique_live_store("explain_recall_uses_hnsw", DIM).await else {
            return;
        };
        let contract = crate::types::EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: DIM,
        };
        let seeded = corpus::seed(&store, "b3-explain", &contract, rows).await;
        let probe = seeded[7].1.clone();

        // The probe is answerable: finite distances, a full result set. The old
        // zero probe produced `NaN` on every row, so nothing downstream of the
        // plan meant anything either.
        let pool = &store.pool().await.expect("pool");
        let encoded = encode_vector(&probe).expect("encode probe");
        let dists: Vec<f64> = sqlx::query(store.vector_candidates_sql())
            .bind(&encoded)
            .bind(5i64)
            .fetch_all(pool)
            .await
            .expect("recall rows")
            .iter()
            .map(|r| r.try_get::<f64, _>("dist").expect("dist"))
            .collect();
        assert_eq!(
            dists.len(),
            5,
            "the corpus must answer the probe: {dists:?}"
        );
        assert!(
            dists.iter().all(|d| d.is_finite()),
            "a NaN distance means the probe was degenerate, not that recall works: {dists:?}"
        );

        let natural = corpus::plan(&store, &probe, 5).await;
        eprintln!(
            "B3 EXPLAIN at {rows} rows (planner's choice):\n{}",
            corpus::elide_vector_literals(&natural)
        );
        assert!(
            natural.contains("concepts_embedding_idx"),
            "the planner must CHOOSE concepts_embedding_idx for the production recall \
             query at {rows} rows, with no GUC helping it. Got:\n{natural}"
        );
        assert!(
            !natural.contains("Seq Scan on concepts"),
            "a natural plan that still scans concepts sequentially is the failure this \
             box exists to catch. Got:\n{natural}"
        );

        // Diagnostic only: what the index can do when the alternative is
        // penalised. Kept because it separates "the planner declined the index"
        // from "the index cannot serve this query" when the assertion above
        // fails.
        let forced_hnsw =
            explain_vector_candidates(&store, &probe, Some("SET LOCAL enable_seqscan = off")).await;
        eprintln!(
            "B3 EXPLAIN (diagnostic, enable_seqscan = off):\n{}",
            corpus::elide_vector_literals(&forced_hnsw)
        );

        // Must not pass the GUC as extra_set: the helper reads
        // store.forced_exact_scan() and issues D::forced_exact_scan_sql
        // through the production helper. Passing extra_set here would
        // leave the flag dead (B3-R1-1).
        let exact = store.with_forced_exact_scan();
        let forced_exact = corpus::plan(&exact, &probe, 5).await;
        eprintln!(
            "B3 EXPLAIN (forced-exact enable_indexscan = off):\n{}",
            corpus::elide_vector_literals(&forced_exact)
        );
        assert!(
            !forced_exact.contains("concepts_embedding_idx"),
            "forced-exact lane must not use the hnsw index, got:\n{forced_exact}"
        );
        assert!(
            forced_exact.contains("Seq Scan on concepts"),
            "at {rows} rows the forced-exact lane must actually be a sequential scan, not \
             merely 'not the index': on an empty table both lanes are seq scans and this \
             assertion is free. Got:\n{forced_exact}"
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
