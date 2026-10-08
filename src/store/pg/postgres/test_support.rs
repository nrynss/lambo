//! Test harness for the PostgreSQL dialect, shared by `postgres::tests` and
//! the SQLite H1/H3 cross-store parity tests: the live-DSN skip/require
//! contract, a DSN database-path rewrite, and the deterministic vector corpus
//! with its EXPLAIN helpers. Test-only; re-exported from `postgres` at its
//! original paths (`crate::store::pg::postgres::{postgres_dsn_or_skip,
//! dsn_for_database, corpus}`).

// `corpus` names the adapter as `super::PostgresStore`, as it did when it
// was declared in `postgres.rs`.
use super::PostgresStore;

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
