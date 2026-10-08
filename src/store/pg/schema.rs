//! Schema authority for the Postgres-wire family: `init_schema` applies the
//! dialect's DDL (raw multi-statement, idempotent) and its post-init
//! convergence statements; `preflight_schema` diffs a live database against
//! the same DDL's tables and columns; both prove a templated vector width
//! against the live column where the dialect substitutes one (B4).

use sqlx::PgPool;

use super::codec::{backend, parse_pgvector_format_type};
use super::{Dialect, PgStore};
use crate::store::{
    columns_in_ddl, tables_in_ddl, unprovisioned_column_err, unprovisioned_store_err,
};
use crate::types::StoreError;

/// How many times `apply_schema` re-runs the DDL after losing a concurrent
/// first-time `CREATE ... IF NOT EXISTS` race. One retry suffices for two
/// racers; the bound covers a few more without masking a real fault.
const DDL_RACE_RETRIES: u32 = 3;

/// SQLSTATEs a concurrent `CREATE ... IF NOT EXISTS` loser can raise:
/// `unique_violation` on a system catalog index, `duplicate_table`, and
/// `duplicate_object`. Anything else is a real failure and is not retried.
fn is_concurrent_ddl_race_code(code: &str) -> bool {
    matches!(code, "23505" | "42P07" | "42710")
}

fn is_concurrent_ddl_race(e: &sqlx::Error) -> bool {
    e.as_database_error()
        .and_then(|d| d.code())
        .is_some_and(|code| is_concurrent_ddl_race_code(&code))
}

impl<D: Dialect> PgStore<D> {
    /// B4: dialects that substitute width into DDL must prove the live
    /// column matches construction dim. Cockroach skips this: its authority
    /// is the static file parsed at construction.
    pub(super) async fn assert_live_schema_width(&self, pool: &PgPool) -> Result<(), StoreError> {
        let Some(sql) = D::live_schema_vector_width_sql() else {
            return Ok(());
        };
        let formatted: Option<String> = sqlx::query_scalar(sql)
            .fetch_optional(pool)
            .await
            .map_err(backend)?;
        let Some(formatted) = formatted else {
            return Err(StoreError::Backend(format!(
                "{}: concepts.embedding is missing; store is unprovisioned \
                 or not a vector schema",
                D::NAME
            )));
        };
        let live = parse_pgvector_format_type(&formatted).ok_or_else(|| {
            StoreError::Backend(format!(
                "{}: concepts.embedding type {formatted:?} is not vector(n)",
                D::NAME
            ))
        })?;
        if live != self.vector_dim {
            return Err(StoreError::Backend(format!(
                "{}: live schema width is vector({live}) but this process \
                 constructed at dim {}. DDL outranks the pin for reporting; \
                 they must match on an initialized store. Re-init at the \
                 schema width, or migrate.",
                D::NAME,
                self.vector_dim
            )));
        }
        Ok(())
    }

    pub(super) async fn apply_schema(&self) -> Result<(), StoreError> {
        // Multi-statement DDL via the simple protocol (raw_sql); every statement is
        // `IF NOT EXISTS`, so a serial re-run is idempotent (T3.1 acceptance).
        // Concurrent first-time runs are not: two connections can both pass an
        // `IF NOT EXISTS` check and the loser fails on the catalog's unique
        // index (e.g. `pg_extension_name_index` for `CREATE EXTENSION`). The
        // simple-protocol batch runs as one implicit transaction, so the loser's
        // whole batch rolls back and a re-run sees the winner's objects and
        // no-ops. Retry only those race codes, a bounded number of times.
        let pool = &self.pool().await?;
        let mut attempt = 0;
        loop {
            match sqlx::raw_sql(self.ddl.as_ref()).execute(pool).await {
                Ok(_) => break,
                Err(e) if attempt < DDL_RACE_RETRIES && is_concurrent_ddl_race(&e) => {
                    attempt += 1;
                }
                Err(e) => return Err(backend(e)),
            }
        }

        // Post-DDL convergence ALTERs. Not folded into `init_sql`: that would
        // turn this from raw_sql + N query() calls into one raw_sql, which is
        // a Cockroach behaviour change B0 forbade and B2 does not make. The
        // statements themselves are not byte-identical (STRING vs TEXT, INT
        // vs BIGINT), so they live on the dialect.
        for stmt in D::post_init_statements() {
            sqlx::query(stmt).execute(pool).await.map_err(backend)?;
        }
        self.assert_live_schema_width(pool).await?;
        Ok(())
    }

    /// J3 F5 + J3-R2R-3. An `information_schema.tables` read in the
    /// connection's current schema, diffed against the table names in the DDL
    /// this build ships, then an `information_schema.columns` read per required
    /// table, diffed against the column set the same DDL declares. Cockroach is
    /// provisioned by `scripts/provision.sh`, not by `init_schema` on the attach
    /// path, so the same upgrade-without-reprovision hazard applies — and here
    /// every failed statement is also a round trip. The column half is the
    /// Cockroach-dialect side of J3-R2R-3 (source-correct here; live-cluster
    /// verification is the named follow-up the brief records).
    pub(super) async fn verify_schema(&self) -> Result<(), StoreError> {
        let pool = &self.pool().await?;
        let present: Vec<String> = sqlx::query_scalar(
            "SELECT table_name FROM information_schema.tables \
             WHERE table_schema = current_schema()",
        )
        .fetch_all(pool)
        .await
        .map_err(backend)?;
        let required = tables_in_ddl(self.ddl.as_ref());
        let missing_tables: Vec<&str> = required
            .into_iter()
            .filter(|t| !present.iter().any(|p| p == t))
            .collect();
        if !missing_tables.is_empty() {
            return Err(unprovisioned_store_err(D::NAME, &missing_tables));
        }
        let mut by_table: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for (table, col) in columns_in_ddl(self.ddl.as_ref()) {
            by_table.entry(table).or_default().push(col);
        }
        for (table, cols) in by_table {
            let present_cols: Vec<String> = sqlx::query_scalar(
                "SELECT column_name FROM information_schema.columns \
                 WHERE table_schema = current_schema() AND table_name = $1",
            )
            .bind(table)
            .fetch_all(pool)
            .await
            .map_err(backend)?;
            let missing: Vec<&str> = cols
                .iter()
                .copied()
                .filter(|c| !present_cols.iter().any(|p| p == c))
                .collect();
            if !missing.is_empty() {
                return Err(unprovisioned_column_err(D::NAME, table, &missing));
            }
        }
        self.assert_live_schema_width(pool).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::is_concurrent_ddl_race_code;

    #[test]
    fn only_the_concurrent_create_race_codes_are_retried() {
        // unique_violation (catalog index), duplicate_table, duplicate_object.
        for code in ["23505", "42P07", "42710"] {
            assert!(
                is_concurrent_ddl_race_code(code),
                "{code} should be retried"
            );
        }
        // syntax error, undefined object, insufficient privilege, admin shutdown.
        for code in ["42601", "42704", "42501", "57P01"] {
            assert!(
                !is_concurrent_ddl_race_code(code),
                "{code} must not be retried"
            );
        }
    }
}
