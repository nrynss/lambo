//! Schema authority for the SQLite adapter: the embedded T3.1 DDL, its
//! idempotent application with guarded column convergence (`init_schema`), and
//! the read-only preflight that diffs a live database against the same DDL
//! (`preflight_schema`). `migrations/sqlite/001_init.sql` is the only source
//! of table and column names; nothing here re-spells them.

use sqlx::SqlitePool;

use super::codec::db_err;
use super::SqliteStore;
use crate::store::{
    columns_in_ddl, tables_in_ddl, unprovisioned_column_err, unprovisioned_store_err,
};
use crate::types::StoreError;

/// T3.1 DDL — embedded and executed verbatim by [`SqliteStore::init_schema`](crate::store::GraphStore::init_schema),
/// and read for its table names by [`SqliteStore::preflight_schema`](crate::store::GraphStore::preflight_schema) (J3 F5).
/// Idempotent by construction (`IF NOT EXISTS` everywhere).
pub(super) const INIT_SQL: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/migrations/sqlite/001_init.sql"
));

/// Idempotent post-T3.1 column convergence: SQLite has no
/// `ADD COLUMN IF NOT EXISTS`, so check `pragma_table_info` first and ALTER
/// only when the column is absent. Safe to call on every `init_schema` (fresh
/// databases already carry the columns from the DDL — no-op).
pub(super) async fn ensure_column(
    pool: &SqlitePool,
    table: &str,
    column: &str,
    alter_ddl: &str,
) -> Result<(), StoreError> {
    let present: Option<String> =
        sqlx::query_scalar("SELECT name FROM pragma_table_info(?) WHERE name = ?")
            .bind(table)
            .bind(column)
            .fetch_optional(pool)
            .await
            .map_err(|e| db_err(&format!("init_schema: inspect {table}.{column}"), e))?;
    if present.is_none() {
        sqlx::query(alter_ddl)
            .execute(pool)
            .await
            .map_err(|e| db_err(&format!("init_schema: add {table}.{column}"), e))?;
    }
    Ok(())
}

impl SqliteStore {
    pub(super) async fn apply_schema(&self) -> Result<(), StoreError> {
        // The T3.1 DDL is idempotent (every statement IF NOT EXISTS); the
        // SQLite driver executes multi-statement strings statement-by-statement
        // and aborts on the first error.
        sqlx::query(INIT_SQL)
            .execute(self.pool())
            .await
            .map_err(|e| db_err("init_schema (migrations/sqlite/001_init.sql)", e))?;

        // Post-T3.1 columns (P3 wave 2 remediation): fresh databases carry them
        // inline from the DDL above; pre-existing databases converge here.
        // SQLite has no `ADD COLUMN IF NOT EXISTS` (verified: 3.53.4 rejects
        // the syntax), so each column is inspected via `pragma_table_info` and
        // a plain ALTER is issued only when it is missing — making the whole
        // init idempotent on any database state. See the migration header.
        ensure_column(
            self.pool(),
            "concepts",
            "chunk_group_id",
            "ALTER TABLE concepts ADD COLUMN chunk_group_id TEXT",
        )
        .await?;
        // C2 (SoloPolicy): the explicit human-confirmation count. Existing
        // databases converge here; fresh ones carry the column inline from the
        // DDL and this is a no-op.
        ensure_column(
            self.pool(),
            "concepts",
            "human_confirmed",
            "ALTER TABLE concepts ADD COLUMN human_confirmed INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // #22: a supplied vector's provenance (compact JSON). Nullable with
        // no default: existing rows read NULL, which is the truth for every
        // concept written before #22 (each was embedded from its content).
        ensure_column(
            self.pool(),
            "concepts",
            "embedding_source",
            "ALTER TABLE concepts ADD COLUMN embedding_source TEXT",
        )
        .await?;
        // D (about-time): the nullable about-time of interactions and edges
        // (NULL = live fact, fallback created_at; an edge inherits it from the
        // writing interaction). Existing pre-D databases converge here; fresh
        // ones carry the columns inline from the DDL and these are no-ops.
        ensure_column(
            self.pool(),
            "interactions",
            "event_time",
            "ALTER TABLE interactions ADD COLUMN event_time TEXT",
        )
        .await?;
        ensure_column(
            self.pool(),
            "edges",
            "event_time",
            "ALTER TABLE edges ADD COLUMN event_time TEXT",
        )
        .await?;
        ensure_column(
            self.pool(),
            "sessions",
            "embedding_kind",
            "ALTER TABLE sessions ADD COLUMN embedding_kind TEXT",
        )
        .await?;
        ensure_column(
            self.pool(),
            "sessions",
            "embedding_model",
            "ALTER TABLE sessions ADD COLUMN embedding_model TEXT",
        )
        .await?;
        ensure_column(
            self.pool(),
            "sessions",
            "embedding_dim",
            "ALTER TABLE sessions ADD COLUMN embedding_dim INTEGER",
        )
        .await?;
        ensure_column(
            self.pool(),
            "canonization_events",
            "last_demotion_time",
            "ALTER TABLE canonization_events ADD COLUMN last_demotion_time TEXT",
        )
        .await?;
        ensure_column(
            self.pool(),
            "session_leases",
            "current_token",
            "ALTER TABLE session_leases ADD COLUMN current_token INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // J2. Additive and nullable, so an ALREADY-PROVISIONED store (the
        // dogfood rig's `lambo-dev.db` among them) converges here on the next
        // attach without a re-provision: existing rows get NULL, which reads as
        // "this holder published no endpoint" — exactly what a pre-J2 holder
        // did. No default, deliberately: a fabricated address would be worse
        // than an honest absence.
        ensure_column(
            self.pool(),
            "session_leases",
            "endpoint",
            "ALTER TABLE session_leases ADD COLUMN endpoint TEXT",
        )
        .await?;
        // Issue #17: the durable mutation counter. NOT NULL DEFAULT 0, so the
        // ALTER backfills existing rows and the accounting accumulates forward
        // from a session's first post-upgrade flush.
        ensure_column(
            self.pool(),
            "sessions",
            "mutation_epoch",
            "ALTER TABLE sessions ADD COLUMN mutation_epoch INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        // Issue #29: GC's sweep accounting. The epoch backfills 0 (never
        // swept) and the time NULL, which the daemon treats as "anchor the
        // `gc_max_interval` clock on first attach", never "sweep now".
        ensure_column(
            self.pool(),
            "sessions",
            "last_gc_epoch",
            "ALTER TABLE sessions ADD COLUMN last_gc_epoch INTEGER NOT NULL DEFAULT 0",
        )
        .await?;
        ensure_column(
            self.pool(),
            "sessions",
            "last_gc_at",
            "ALTER TABLE sessions ADD COLUMN last_gc_at TEXT",
        )
        .await?;
        Ok(())
    }

    /// J3 F5 + J3-R2R-3. A `sqlite_master` read, diffed against the **table**
    /// names in the DDL this build ships, then a `pragma_table_info` read per
    /// required table, diffed against the **column** set the same DDL declares.
    /// Cheap (no DDL, no write) and run before the lease is taken, so a refusal
    /// leaves no lease to release. The column half matters because a missing
    /// *column* is F5's exact consequence — attaches, acks, and loses
    /// everything, loud only at close — and the table check cannot see it
    /// (J3-R2R-3 measured it at the same magnitude as a missing table).
    pub(super) async fn verify_schema(&self) -> Result<(), StoreError> {
        let present: Vec<String> =
            sqlx::query_scalar("SELECT name FROM sqlite_master WHERE type = 'table'")
                .fetch_all(self.pool())
                .await
                .map_err(|e| db_err("preflight_schema: list tables", e))?;
        let required = tables_in_ddl(INIT_SQL);
        let missing_tables: Vec<&str> = required
            .into_iter()
            .filter(|t| !present.iter().any(|p| p == t))
            .collect();
        if !missing_tables.is_empty() {
            return Err(unprovisioned_store_err("sqlite", &missing_tables));
        }
        // Column preflight (J3-R2R-3): every required (table, column) from the
        // same DDL source, diffed per table against `pragma_table_info`.
        let mut by_table: std::collections::BTreeMap<&str, Vec<&str>> = Default::default();
        for (table, col) in columns_in_ddl(INIT_SQL) {
            by_table.entry(table).or_default().push(col);
        }
        for (table, cols) in by_table {
            let present_cols: Vec<String> =
                sqlx::query_scalar("SELECT name FROM pragma_table_info(?)")
                    .bind(table)
                    .fetch_all(self.pool())
                    .await
                    .map_err(|e| db_err("preflight_schema: list columns", e))?;
            let missing: Vec<&str> = cols
                .iter()
                .copied()
                .filter(|c| !present_cols.iter().any(|p| p == c))
                .collect();
            if !missing.is_empty() {
                return Err(unprovisioned_column_err("sqlite", table, &missing));
            }
        }
        Ok(())
    }
}
