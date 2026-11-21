//! Shared backend-error classification (STORE-4, #301).
//!
//! Every retry decision on the write path — the adapters' transaction replay
//! and the flush loop's retry, dead-letter and degrade — is made from the
//! STRUCTURED error code, never from flattened message text. Each adapter's
//! codes are read by its own table: the driver error is told apart by type
//! (`SqliteError`, `PgDatabaseError`), so a five-digit SQLite extended code is
//! never read as a SQLSTATE, nor a numeric SQLSTATE as a SQLite code.
//!
//! * **Constraint violations are deterministic and terminal.** A unique-key
//!   collision can never succeed on replay; retrying it only blocks the
//!   queue (head-of-line). Postgres / CockroachDB SQLSTATE class `23` and
//!   SQLite `SQLITE_CONSTRAINT` (primary code 19, every extended variant).
//!   Both adapters map them to [`StoreError::Constraint`] and the flush loop
//!   dead-letters the batch (D5: drop-after-log, visible in
//!   `FlushStats::dead_lettered`).
//! * **Permanent errors are terminal too (#301).** Errors no replay can fix
//!   that are not constraint violations: SQLSTATE classes `0A` (feature not
//!   supported), `21` (cardinality violation), `22` (data exception), `3F`
//!   (invalid schema name), `42` (syntax error or access rule violation, such
//!   as `42P01` undefined table, `42703` undefined column, `42501`
//!   insufficient privilege), `44` (WITH CHECK OPTION violation) and `54`
//!   (program limit exceeded); SQLite primary codes `SQLITE_ERROR` (1, such as
//!   "no such table", except the internal `SQLITE_ERROR_RETRY`),
//!   `SQLITE_CORRUPT` (11), `SQLITE_TOOBIG` (18), `SQLITE_MISMATCH` (20),
//!   `SQLITE_RANGE` (25) and `SQLITE_NOTADB` (26). Both adapters map them to
//!   [`StoreError::Permanent`]; the flush loop does not retry it and degrades
//!   the session.
//! * **Everything else is retryable**, as it was before #301. That is the
//!   transients (SQLSTATE `40` serialization failure or deadlock, `08`
//!   connection exception, `53` insufficient resources, `55P03` lock not
//!   available, `57` operator intervention such as `57014` statement timeout
//!   and `57P01`..`57P03` server shutdown, `58` system error; SQLite
//!   `SQLITE_BUSY` and `SQLITE_LOCKED`), every code not in the lists above, and
//!   every error with no statement code (pool timeout, lost connection, TLS,
//!   protocol). They map to [`StoreError::Backend`]. The permanent lists are
//!   kept short on purpose: a permanent error misread as retryable costs
//!   retries and ends where it did before #301, retained until the backlog
//!   degrades the session, while a transient misread as permanent would
//!   degrade a session that would have recovered.
//!
//! Classification home: the flush loop reads retryability from
//! [`StoreError::is_retryable`] (types); the adapters classify the raw
//! [`sqlx::Error`] here before it is flattened, so no decision ever depends
//! on error text.
//!
//! This module compiles only when a sqlx-backed adapter is enabled
//! (`store-cockroach` / `store-postgres` / `store-sqlite`), which is when the
//! [`sqlx::Error::Database`] payload exists.

use std::borrow::Cow;

use crate::types::StoreError;

/// Structured class of a backend error, decided at the source so retry
/// decisions never depend on flattened message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Constraint violation (SQLSTATE `23xxx` / `SQLITE_CONSTRAINT`):
    /// deterministic — dead-letter, never retry.
    Constraint,
    /// Transient, or not known to be permanent: replaying / retrying is
    /// safe.
    Retryable,
    /// Permanent and not a constraint violation (#301): no replay can fix it
    /// — never retry; the flush loop degrades the session.
    Permanent,
}

/// Classify a raw `sqlx::Error`. Errors with no statement code (pool
/// timeout, I/O, TLS, protocol) are transport failures and retryable.
pub fn classify(e: &sqlx::Error) -> ErrorClass {
    match e {
        sqlx::Error::Database(db) => classify_database(db.as_ref()),
        _ => ErrorClass::Retryable,
    }
}

/// Classify a database error by its adapter's own code table. A driver error
/// type this build does not know (only the tests' stand-in) falls back to
/// reading the code's shape.
fn classify_database(db: &dyn sqlx::error::DatabaseError) -> ErrorClass {
    let code = db.code();
    let Some(code) = code.as_deref() else {
        return ErrorClass::Retryable;
    };
    #[cfg(feature = "store-sqlite")]
    if db.try_downcast_ref::<sqlx::sqlite::SqliteError>().is_some() {
        return classify_sqlite(code);
    }
    #[cfg(any(feature = "store-cockroach", feature = "store-postgres"))]
    if db
        .try_downcast_ref::<sqlx::postgres::PgDatabaseError>()
        .is_some()
    {
        return classify_sqlstate(code);
    }
    classify_by_shape(code)
}

/// Map a write-path `sqlx::Error` into a typed [`StoreError`] (STORE-4,
/// #301).
///
/// Constraint violations become [`StoreError::Constraint`] carrying the
/// SQLSTATE / extended code; permanent errors become
/// [`StoreError::Permanent`] and the rest [`StoreError::Backend`], each with
/// the caller's `ctx`, which names the statement for the log. A permanent
/// error's message also carries its code, which the driver's message may not.
///
/// **Write paths only.** Constraints can only arise from write statements
/// (INSERT / UPDATE / DELETE / DDL), so this mapping is used exclusively on
/// the write path (`upsert_*`, `apply_canonization`, `delete_*`, seed / tx
/// bodies); read / load paths keep their existing plain `Backend` mapping.
pub fn map_write_err(e: sqlx::Error, ctx: impl FnOnce(&str) -> String) -> StoreError {
    match classify(&e) {
        ErrorClass::Constraint => StoreError::Constraint(error_code(&e)),
        ErrorClass::Permanent => {
            StoreError::Permanent(ctx(&format!("{e} (code {})", error_code(&e))))
        }
        ErrorClass::Retryable => StoreError::Backend(ctx(&e.to_string())),
    }
}

/// The SQLSTATE / extended code that classified the error (falls back to the
/// flattened message when the code is somehow absent).
fn error_code(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(db) => db
            .code()
            .map(Cow::into_owned)
            .unwrap_or_else(|| e.to_string()),
        _ => e.to_string(),
    }
}

/// `SQLITE_ERROR_RETRY`: SQLite's internal "prepare again", never a
/// statement's final answer; not permanent like the rest of `SQLITE_ERROR`.
const SQLITE_ERROR_RETRY: u32 = 513;

/// Classify a SQLite extended result code: its primary code is the low byte.
fn classify_sqlite(code: &str) -> ErrorClass {
    let Ok(n) = code.parse::<u32>() else {
        return ErrorClass::Retryable;
    };
    if n == SQLITE_ERROR_RETRY {
        return ErrorClass::Retryable;
    }
    match n & 0xFF {
        19 => ErrorClass::Constraint, // SQLITE_CONSTRAINT (all variants)
        // SQLITE_ERROR, CORRUPT, TOOBIG, MISMATCH, RANGE, NOTADB.
        1 | 11 | 18 | 20 | 25 | 26 => ErrorClass::Permanent,
        // SQLITE_BUSY (5) and SQLITE_LOCKED (6) among them.
        _ => ErrorClass::Retryable,
    }
}

/// Classify a Postgres / CockroachDB SQLSTATE by its two-character class.
fn classify_sqlstate(code: &str) -> ErrorClass {
    if code.len() != 5 {
        return ErrorClass::Retryable;
    }
    match code.get(..2) {
        Some("23") => ErrorClass::Constraint,
        Some("0A" | "21" | "22" | "3F" | "42" | "44" | "54") => ErrorClass::Permanent,
        // 40 (serialization, deadlock), 08 (connection), 53, 55, 57, 58 and
        // every class not listed.
        _ => ErrorClass::Retryable,
    }
}

/// The fallback for a driver error type this build cannot identify: read the
/// code's shape. A numeric code is read as SQLite first when its low byte is
/// `SQLITE_CONSTRAINT`, `SQLITE_BUSY` or `SQLITE_LOCKED`, so five-digit
/// constraint variants (e.g. 26643 `SQLITE_CONSTRAINT_ROWID`) are not taken
/// for SQLSTATEs; otherwise a five-character code is a SQLSTATE and any other
/// a SQLite code. Real driver errors never reach this: the per-adapter
/// tables decide them by type.
fn classify_by_shape(code: &str) -> ErrorClass {
    if let Ok(n) = code.parse::<u32>() {
        match n & 0xFF {
            19 => return ErrorClass::Constraint,
            5 | 6 => return ErrorClass::Retryable,
            _ => {}
        }
    }
    if code.len() == 5 {
        classify_sqlstate(code)
    } else {
        classify_sqlite(code)
    }
}

/// A `sqlx::Error::Database` with `code`, for tests: sqlx's real per-driver
/// error constructors are crate-private, so the classifier and the flush
/// loop's handling of what it returns are tested through this (the flush
/// tests need `store-memory`).
#[cfg(all(test, feature = "store-memory"))]
pub(crate) fn fake_db_error(code: &str) -> sqlx::Error {
    tests::db_err(code)
}

#[cfg(test)]
mod tests {
    use super::*;
    use sqlx::error::DatabaseError;

    /// Minimal `DatabaseError` so the classifier can be tested without a
    /// live database (sqlx's real per-driver error constructors are
    /// crate-private).
    #[derive(Debug)]
    struct FakeDbError {
        code: String,
        message: String,
    }

    impl std::fmt::Display for FakeDbError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            write!(f, "{}", self.message)
        }
    }

    impl std::error::Error for FakeDbError {}

    impl DatabaseError for FakeDbError {
        fn message(&self) -> &str {
            &self.message
        }

        fn code(&self) -> Option<Cow<'_, str>> {
            Some(self.code.clone().into())
        }

        fn as_error(&self) -> &(dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn as_error_mut(&mut self) -> &mut (dyn std::error::Error + Send + Sync + 'static) {
            self
        }

        fn into_error(self: Box<Self>) -> Box<dyn std::error::Error + Send + Sync + 'static> {
            self
        }

        fn kind(&self) -> sqlx::error::ErrorKind {
            sqlx::error::ErrorKind::Other
        }
    }

    pub(super) fn db_err(code: &str) -> sqlx::Error {
        sqlx::Error::Database(Box::new(FakeDbError {
            code: code.into(),
            message: format!("db error (code {code})"),
        }))
    }

    #[test]
    fn postgres_sqlstate_classes() {
        // Constraint classes (23xxx) are terminal.
        for code in ["23505", "23503", "23514", "23000"] {
            assert_eq!(classify(&db_err(code)), ErrorClass::Constraint, "{code}");
        }
        // Transient: serialization (40xxx), connection (08xxx), shutdown.
        for code in [
            "40001", "40003", "40P01", "08006", "08P01", "57P01", "57P02", "57P03",
        ] {
            assert_eq!(classify(&db_err(code)), ErrorClass::Retryable, "{code}");
        }
        // Permanent, non-constraint (#301).
        assert_eq!(classify(&db_err("42P01")), ErrorClass::Permanent); // undefined table
        assert_eq!(classify(&db_err("22012")), ErrorClass::Permanent); // division by zero
                                                                       // A class not known to be permanent stays retryable (#301).
        assert_eq!(classify(&db_err("99999")), ErrorClass::Retryable);
    }
    #[test]
    fn numeric_sqlstates_survive_the_sqlite_check() {
        // In the shape fallback all-digit SQLSTATEs parse as u32 and hit the
        // SQLite check first, but only low-byte 19/5/6 classify there — these
        // must fall through to the SQLSTATE table unchanged.
        assert_eq!(classify(&db_err("40001")), ErrorClass::Retryable); // serialization failure
        assert_eq!(classify(&db_err("08006")), ErrorClass::Retryable); // connection failure
        assert_eq!(classify(&db_err("23505")), ErrorClass::Constraint); // unique_violation
    }

    #[test]
    fn sqlite_extended_codes() {
        // SQLITE_CONSTRAINT = 19 and its extended variants (19 | n<<8).
        assert_eq!(classify(&db_err("19")), ErrorClass::Constraint);
        assert_eq!(classify(&db_err("2067")), ErrorClass::Constraint); // UNIQUE
        assert_eq!(classify(&db_err("787")), ErrorClass::Constraint); // FOREIGNKEY
        assert_eq!(classify(&db_err("1555")), ErrorClass::Constraint); // PRIMARYKEY
        assert_eq!(classify(&db_err("1299")), ErrorClass::Constraint); // NOTNULL
                                                                       // Five-digit constraint variants must not be mistaken for SQLSTATEs.
        assert_eq!(classify(&db_err("26643")), ErrorClass::Constraint); // ROWID
        assert_eq!(classify(&db_err("28179")), ErrorClass::Constraint); // DATATYPE
        assert_eq!(classify(&db_err("29459")), ErrorClass::Constraint); // PINNED
        assert_eq!(classify(&db_err("29715")), ErrorClass::Constraint); // FUNCTION
                                                                        // SQLITE_BUSY = 5 and its extended variants.
        assert_eq!(classify(&db_err("5")), ErrorClass::Retryable);
        assert_eq!(classify(&db_err("261")), ErrorClass::Retryable); // BUSY_RECOVERY
        assert_eq!(classify(&db_err("517")), ErrorClass::Retryable); // BUSY_SNAPSHOT
                                                                     // SQLITE_ERROR is permanent (#301).
        assert_eq!(classify(&db_err("1")), ErrorClass::Permanent);
    }

    /// #301: each adapter's own table, which real driver errors reach by
    /// type. Codes the shape fallback would misread are decided right here:
    /// SQLSTATE 42501 (insufficient privilege) and 22021 (bad character)
    /// have low byte 5, `SQLITE_BUSY`'s.
    #[test]
    fn per_adapter_tables() {
        for code in ["42501", "22021", "42P01", "0A000", "54001", "22P05"] {
            assert_eq!(classify_sqlstate(code), ErrorClass::Permanent, "{code}");
        }
        for code in ["23505", "23P01"] {
            assert_eq!(classify_sqlstate(code), ErrorClass::Constraint, "{code}");
        }
        for code in [
            "40001", "40003", "08006", "53300", "55P03", "57014", "57P01", "58030", "XX000",
            "25006", "28000", "P0001", "", "4200",
        ] {
            assert_eq!(classify_sqlstate(code), ErrorClass::Retryable, "{code:?}");
        }
        // SQLITE_ERROR, CORRUPT, TOOBIG, MISMATCH, RANGE, NOTADB.
        for code in ["1", "11", "18", "20", "25", "26", "257"] {
            assert_eq!(classify_sqlite(code), ErrorClass::Permanent, "{code}");
        }
        for code in ["19", "2067", "26643"] {
            assert_eq!(classify_sqlite(code), ErrorClass::Constraint, "{code}");
        }
        // BUSY, LOCKED, IOERR, FULL, READONLY_RECOVERY, SCHEMA, ERROR_RETRY.
        for code in ["5", "6", "10", "13", "264", "17", "513", "x"] {
            assert_eq!(classify_sqlite(code), ErrorClass::Retryable, "{code}");
        }
    }

    /// #301: a permanent error that is not a constraint violation must not
    /// be retryable, and a transient must stay retryable. Lean:
    /// `Flush.permanent_not_retryable_fixed` (before the fix,
    /// `Flush.permanent_retryable_old`) and `Flush.transient_retryable_fixed`.
    #[test]
    fn permanent_errors_are_not_retryable_and_transients_are() {
        // Undefined table / column, syntax, bad values, unsupported feature,
        // cardinality, schema, WITH CHECK OPTION, program limit; SQLITE_ERROR.
        for code in [
            "42P01", "42703", "42601", "22P02", "22001", "0A000", "21000", "3F000", "44000",
            "54000", "1",
        ] {
            let err = map_write_err(db_err(code), |m| format!("upsert concept: {m}"));
            assert!(!err.is_retryable(), "{code} must not be retried: {err:?}");
            assert!(
                !matches!(err, StoreError::Constraint(_)),
                "{code} is not a constraint violation: {err:?}"
            );
            assert!(err.to_string().contains(code), "{code} missing: {err}");
            assert!(
                err.to_string().contains("upsert concept"),
                "context missing: {err}"
            );
        }
        // Serialization / deadlock, connection, statement timeout and
        // shutdown, too many connections, lock not available, system I/O,
        // internal, read-only transaction; SQLITE_BUSY, SQLITE_LOCKED,
        // SQLITE_IOERR.
        for code in [
            "40001", "40P01", "08006", "57014", "57P01", "53300", "55P03", "58030", "XX000",
            "25006", "5", "6", "261", "10",
        ] {
            let err = map_write_err(db_err(code), |m| format!("upsert concept: {m}"));
            assert!(err.is_retryable(), "{code} must stay retryable: {err:?}");
        }
        // No statement code: a pool timeout, a lost connection, a closed pool.
        for e in [
            sqlx::Error::PoolTimedOut,
            sqlx::Error::Io(std::io::Error::other("connection reset")),
            sqlx::Error::PoolClosed,
        ] {
            let err = map_write_err(e, |m| format!("upsert concept: {m}"));
            assert!(err.is_retryable(), "must stay retryable: {err:?}");
        }
    }

    #[test]
    fn map_write_err_produces_constraint_permanent_or_backend() {
        match map_write_err(db_err("23505"), |m| format!("upsert concept: {m}")) {
            StoreError::Constraint(code) => assert_eq!(code, "23505"),
            other => panic!("expected Constraint, got {other:?}"),
        }
        match map_write_err(db_err("42P01"), |m| format!("upsert concept: {m}")) {
            StoreError::Permanent(msg) => {
                assert!(msg.contains("upsert concept"), "context missing: {msg}");
                assert!(msg.contains("42P01"), "code missing: {msg}");
            }
            other => panic!("expected Permanent, got {other:?}"),
        }
        match map_write_err(db_err("40001"), |m| format!("upsert concept: {m}")) {
            StoreError::Backend(msg) => {
                assert!(msg.contains("upsert concept"), "context missing: {msg}");
                assert!(msg.contains("40001"), "code missing: {msg}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }
}
