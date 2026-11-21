//! Shared backend-error classification (#301).
//!
//! Every retry decision on the write path — the adapters' transaction replay
//! and the flush loop's retry, dead-letter and degrade — is made from the
//! STRUCTURED error code, never from flattened message text. Each adapter's
//! codes are read by its own table: the driver error is told apart by type
//! (`SqliteError`, `PgDatabaseError`), so a five-digit SQLite extended code is
//! never read as a SQLSTATE, nor a numeric SQLSTATE as a SQLite code. A
//! database error neither type identifies is retryable (in tests only, a
//! stand-in error type is read by the code's shape instead).
//!
//! * **Constraint violations are deterministic and terminal.** A unique-key
//!   collision can never succeed on replay; retrying it only blocks the
//!   queue (head-of-line). Postgres / CockroachDB SQLSTATE class `23` and
//!   SQLite `SQLITE_CONSTRAINT` (primary code 19, every extended variant).
//!   Both adapters map them to [`StoreError::Constraint`] and the flush loop
//!   dead-letters the batch (drop-after-log, visible in
//!   `FlushStats::dead_lettered`).
//! * **A value the store rejects is dead-lettered the same way (#301).** An
//!   error about a value in the batch, which no replay of that batch can fix
//!   but which says nothing about the next batch: SQLSTATE classes `21`
//!   (cardinality violation), `22` (data exception, such as `22021` a
//!   character the encoding cannot hold) and `44` (WITH CHECK OPTION
//!   violation), and `54000` (program limit exceeded, such as a row too
//!   big); SQLite `SQLITE_TOOBIG` (18) and `SQLITE_MISMATCH` (20). Both
//!   adapters map them to [`StoreError::Constraint`] too, with their code, so
//!   the flush loop drops that batch, counts it in `dead_lettered`, and the
//!   session goes on.
//! * **A permanent error about the store degrades (#301).** An error no
//!   replay can fix that is about the store, not the batch, so every later
//!   batch would fail the same way: SQLSTATE classes `42` (syntax error or
//!   access rule violation, such as `42P01` undefined table, `42703`
//!   undefined column, `42501` insufficient privilege) and `3F` (invalid
//!   schema name), and class `54` other than `54000` (`54001` statement too
//!   complex and up); SQLite primary codes `SQLITE_ERROR` (1, such as "no
//!   such table", except the internal `SQLITE_ERROR_RETRY`), `SQLITE_CORRUPT`
//!   (11), `SQLITE_RANGE` (25) and `SQLITE_NOTADB` (26). Both adapters map
//!   them to [`StoreError::Permanent`]; the flush loop does not retry it and
//!   degrades the session.
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
//!   degrade a session that would have recovered. That is why `0A000`
//!   (feature not supported) stays retryable: Postgres also returns it for
//!   "cached plan must not change result type", which clears once the
//!   connection re-prepares the statement.
//!
//! Only statements mapped with [`map_write_err`] are classified. Reads the
//! adapters make inside the flush transaction (the fence's lease-token read,
//! the lookup of a deleted row's session) keep their plain `Backend`
//! mapping, so a missing `session_leases` table, say, is retried, retained,
//! and degrades the session on `backend_log_max`, as before #301.
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
    /// A value the batch holds that the store rejects (#301): deterministic
    /// for that batch — dead-letter it like a constraint violation.
    ValueRejected,
    /// Permanent, and about the store rather than the batch (#301): no
    /// replay can fix it — never retry; the flush loop degrades the session.
    StorePermanent,
    /// Transient, or not known to be permanent: replaying / retrying is
    /// safe.
    Retryable,
}

/// Classify a raw `sqlx::Error`. Errors with no statement code (pool
/// timeout, I/O, TLS, protocol) are transport failures and retryable.
pub fn classify(e: &sqlx::Error) -> ErrorClass {
    match e {
        sqlx::Error::Database(db) => classify_database(db.as_ref()),
        _ => ErrorClass::Retryable,
    }
}

/// Classify a database error by its adapter's own code table, chosen by the
/// driver error's type.
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
    unidentified(code)
}

/// A database error of a type neither adapter table claims. Only a sqlx
/// driver this crate does not use could produce one: retryable, the class
/// that changes nothing from before #301.
#[cfg(not(test))]
fn unidentified(_code: &str) -> ErrorClass {
    ErrorClass::Retryable
}

/// In tests the classifier is driven by a stand-in error type (sqlx's real
/// constructors are crate-private), read by the code's shape.
#[cfg(test)]
fn unidentified(code: &str) -> ErrorClass {
    classify_by_shape(code)
}

/// Map a write-path `sqlx::Error` into a typed [`StoreError`] (#301).
///
/// Constraint violations and rejected values become
/// [`StoreError::Constraint`] carrying the SQLSTATE / extended code;
/// permanent errors about the store become [`StoreError::Permanent`] and the
/// rest [`StoreError::Backend`], each with the caller's `ctx`, which names
/// the statement for the log. A permanent error's message also carries its
/// code, which the driver's message may not.
///
/// **Write paths only.** Constraints can only arise from write statements
/// (INSERT / UPDATE / DELETE / DDL), so this mapping is used exclusively on
/// the write path (`upsert_*`, `apply_canonization`, `delete_*`, seed / tx
/// bodies); read / load paths keep their existing plain `Backend` mapping.
pub fn map_write_err(e: sqlx::Error, ctx: impl FnOnce(&str) -> String) -> StoreError {
    match classify(&e) {
        ErrorClass::Constraint | ErrorClass::ValueRejected => {
            StoreError::Constraint(error_code(&e))
        }
        ErrorClass::StorePermanent => {
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
#[cfg(any(test, feature = "store-sqlite"))]
const SQLITE_ERROR_RETRY: u32 = 513;

/// Classify a SQLite extended result code: its primary code is the low byte.
#[cfg(any(test, feature = "store-sqlite"))]
fn classify_sqlite(code: &str) -> ErrorClass {
    let Ok(n) = code.parse::<u32>() else {
        return ErrorClass::Retryable;
    };
    if n == SQLITE_ERROR_RETRY {
        return ErrorClass::Retryable;
    }
    match n & 0xFF {
        19 => ErrorClass::Constraint, // SQLITE_CONSTRAINT (all variants)
        // SQLITE_TOOBIG, SQLITE_MISMATCH: the value, not the store.
        18 | 20 => ErrorClass::ValueRejected,
        // SQLITE_ERROR, CORRUPT, RANGE, NOTADB.
        1 | 11 | 25 | 26 => ErrorClass::StorePermanent,
        // SQLITE_BUSY (5) and SQLITE_LOCKED (6) among them.
        _ => ErrorClass::Retryable,
    }
}

/// Classify a Postgres / CockroachDB SQLSTATE by its two-character class
/// (and `54000` by itself).
#[cfg(any(test, feature = "store-cockroach", feature = "store-postgres"))]
fn classify_sqlstate(code: &str) -> ErrorClass {
    if code.len() != 5 {
        return ErrorClass::Retryable;
    }
    if code == "54000" {
        return ErrorClass::ValueRejected; // program_limit_exceeded: a row too big
    }
    match code.get(..2) {
        Some("23") => ErrorClass::Constraint,
        Some("21" | "22" | "44") => ErrorClass::ValueRejected,
        Some("42" | "3F" | "54") => ErrorClass::StorePermanent,
        // 0A (feature not supported, see the module docs), 40 (serialization,
        // deadlock), 08 (connection), 53, 55, 57, 58 and every class not
        // listed.
        _ => ErrorClass::Retryable,
    }
}

/// The tests' reading of a stand-in error's code by its shape. A numeric
/// code is read as SQLite first when its low byte is `SQLITE_CONSTRAINT`,
/// `SQLITE_BUSY` or `SQLITE_LOCKED`, so five-digit constraint variants (e.g.
/// 26643 `SQLITE_CONSTRAINT_ROWID`) are not taken for SQLSTATEs; otherwise a
/// five-character code is a SQLSTATE and any other a SQLite code. Real
/// driver errors never reach this: the per-adapter tables decide them by
/// type.
#[cfg(test)]
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
        // Permanent and about the store, or about a value (#301).
        assert_eq!(classify(&db_err("42P01")), ErrorClass::StorePermanent); // undefined table
        assert_eq!(classify(&db_err("22012")), ErrorClass::ValueRejected); // division by zero
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
                                                                     // SQLITE_ERROR is permanent and about the store (#301).
        assert_eq!(classify(&db_err("1")), ErrorClass::StorePermanent);
    }

    /// #301: each adapter's own table, which real driver errors reach by
    /// type. Codes the shape fallback would misread are decided right here:
    /// SQLSTATE 42501 (insufficient privilege) and 22021 (bad character)
    /// have low byte 5, `SQLITE_BUSY`'s.
    #[test]
    fn per_adapter_tables() {
        // About the store: undefined table, insufficient privilege, invalid
        // schema, statement too complex.
        for code in ["42501", "42P01", "3F000", "54001", "54011"] {
            assert_eq!(
                classify_sqlstate(code),
                ErrorClass::StorePermanent,
                "{code}"
            );
        }
        // About a value: bad character, untranslatable character, division
        // by zero, cardinality, WITH CHECK OPTION, a row too big.
        for code in ["22021", "22P05", "22012", "21000", "44000", "54000"] {
            assert_eq!(classify_sqlstate(code), ErrorClass::ValueRejected, "{code}");
        }
        for code in ["23505", "23P01"] {
            assert_eq!(classify_sqlstate(code), ErrorClass::Constraint, "{code}");
        }
        for code in [
            "40001", "40003", "08006", "53300", "55P03", "57014", "57P01", "58030", "XX000",
            "25006", "28000", "P0001", "0A000", "", "4200",
        ] {
            assert_eq!(classify_sqlstate(code), ErrorClass::Retryable, "{code:?}");
        }
        // SQLITE_ERROR (and ERROR_MISSING_COLLSEQ), CORRUPT, RANGE, NOTADB.
        for code in ["1", "11", "25", "26", "257"] {
            assert_eq!(classify_sqlite(code), ErrorClass::StorePermanent, "{code}");
        }
        // SQLITE_TOOBIG, SQLITE_MISMATCH.
        for code in ["18", "20"] {
            assert_eq!(classify_sqlite(code), ErrorClass::ValueRejected, "{code}");
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
    /// be retryable, and a transient must stay retryable. A permanent error
    /// about the store is `Permanent` (the flush loop degrades); one about a
    /// value is a `Constraint` (the loop dead-letters). Lean:
    /// `Flush.store_permanent_not_retryable`, `Flush.value_rejected_not_retryable`
    /// (before the fix, `Flush.permanent_retryable_old`) and
    /// `Flush.transient_retryable_split`.
    #[test]
    fn permanent_errors_are_not_retryable_and_transients_are() {
        // Undefined table / column, syntax, schema, statement too complex;
        // SQLITE_ERROR.
        for code in ["42P01", "42703", "42601", "3F000", "54001", "1"] {
            let err = map_write_err(db_err(code), |m| format!("upsert concept: {m}"));
            assert!(!err.is_retryable(), "{code} must not be retried: {err:?}");
            assert!(
                matches!(err, StoreError::Permanent(_)),
                "{code} is about the store: {err:?}"
            );
            assert!(err.to_string().contains(code), "{code} missing: {err}");
            assert!(
                err.to_string().contains("upsert concept"),
                "context missing: {err}"
            );
        }
        // Bad values, cardinality, WITH CHECK OPTION, a row too big:
        // dead-lettered like a constraint violation, with the code.
        for code in ["22P02", "22001", "21000", "44000", "54000"] {
            match map_write_err(db_err(code), |m| format!("upsert concept: {m}")) {
                StoreError::Constraint(c) => assert_eq!(c, code),
                other => panic!("{code} is about a value: {other:?}"),
            }
        }
        // Serialization / deadlock, connection, statement timeout and
        // shutdown, too many connections, lock not available, system I/O,
        // internal, read-only transaction; SQLITE_BUSY, SQLITE_LOCKED,
        // SQLITE_IOERR.
        for code in [
            "40001", "40P01", "08006", "57014", "57P01", "53300", "55P03", "58030", "XX000",
            "25006", "0A000", "5", "6", "261", "10",
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
