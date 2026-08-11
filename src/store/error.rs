//! Shared backend-error classification (STORE-4).
//!
//! Every retry decision on the write path — the adapters' transaction replay
//! and the flush loop's retry / dead-letter — is made from the STRUCTURED
//! error code, never from flattened message text:
//!
//! * **Constraint violations are deterministic and terminal.** A unique-key
//!   collision can never succeed on replay; retrying it only blocks the
//!   queue (head-of-line). Both adapters map them to
//!   [`StoreError::Constraint`] and the flush loop dead-letters the batch
//!   (D5: drop-after-log, visible in `FlushStats::dead_lettered`).
//! * **Transients are retryable.** Cockroach serialization conflicts
//!   (SQLSTATE `40xxx`), connection exceptions (`08xxx`) and server-shutdown
//!   states (`57P01`..`57P03`), and SQLite `SQLITE_BUSY` (extended codes
//!   whose primary byte is 5) are safe to replay.
//! * **Everything else is permanent.**
//!
//! Classification home: the flush loop reads retryability from
//! [`StoreError::is_retryable`] (types); the adapters classify the raw
//! [`sqlx::Error`] here before it is flattened, so no decision ever depends
//! on error text.
//!
//! This module compiles only when a sqlx-backed adapter is enabled
//! (`store-cockroach` / `store-sqlite`), which is when the
//! [`sqlx::Error::Database`] payload exists.

use std::borrow::Cow;

use crate::types::StoreError;

/// Structured class of a backend error, decided at the source so retry
/// decisions never depend on flattened message text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ErrorClass {
    /// Constraint violation (Postgres SQLSTATE `23xxx` / SQLite
    /// `SQLITE_CONSTRAINT`): deterministic — dead-letter, never retry.
    Constraint,
    /// Transient (serialization conflict, connection exception, busy):
    /// replaying / retrying is safe.
    Retryable,
    /// Permanent and not a constraint — not worth retrying.
    Other,
}

/// Classify a raw `sqlx::Error`. Non-database errors (pool acquire, decode)
/// have no statement code and are permanent by default.
pub fn classify(e: &sqlx::Error) -> ErrorClass {
    match e {
        sqlx::Error::Database(db) => classify_code(db.code()),
        _ => ErrorClass::Other,
    }
}

/// Map a write-path `sqlx::Error` into a typed [`StoreError`] (STORE-4).
///
/// Constraint violations become [`StoreError::Constraint`] carrying the
/// SQLSTATE / extended code; everything else becomes [`StoreError::Backend`]
/// with the caller's `ctx`, which names the statement for the log.
///
/// **Write paths only.** Constraints can only arise from write statements
/// (INSERT / UPDATE / DELETE / DDL), so this mapping is used exclusively on
/// the write path (`upsert_*`, `apply_canonization`, `delete_*`, seed / tx
/// bodies); read / load paths keep their existing plain `Backend` mapping.
pub fn map_write_err(e: sqlx::Error, ctx: impl FnOnce(&str) -> String) -> StoreError {
    match classify(&e) {
        ErrorClass::Constraint => StoreError::Constraint(constraint_code(&e)),
        _ => StoreError::Backend(ctx(&e.to_string())),
    }
}

/// The SQLSTATE / extended code that identified a constraint violation
/// (falls back to the flattened message when the code is somehow absent).
fn constraint_code(e: &sqlx::Error) -> String {
    match e {
        sqlx::Error::Database(db) => db
            .code()
            .map(Cow::into_owned)
            .unwrap_or_else(|| e.to_string()),
        _ => e.to_string(),
    }
}

/// Classify a database error code. Postgres / Cockroach SQLSTATEs are five
/// characters beginning with two digits; SQLite exposes numeric extended
/// result codes whose primary code is the low byte. The SQLite shape is
/// checked FIRST: five-digit constraint variants (e.g. 26643
/// SQLITE_CONSTRAINT_ROWID) satisfy the SQLSTATE shape and must not be
/// misread as Postgres codes.
fn classify_code(code: Option<Cow<'_, str>>) -> ErrorClass {
    let Some(code) = code else {
        return ErrorClass::Other;
    };
    if let Ok(n) = code.parse::<u32>() {
        // SQLite extended result code: primary code is the low byte.
        match n & 0xFF {
            19 => return ErrorClass::Constraint, // SQLITE_CONSTRAINT (all variants)
            5 => return ErrorClass::Retryable,   // SQLITE_BUSY (incl. SNAPSHOT/RECOVERY)
            _ => {}
        }
    }
    if code.len() == 5 && code.as_bytes()[..2].iter().all(u8::is_ascii_digit) {
        // Postgres / Cockroach SQLSTATE classes.
        if code.starts_with("23") {
            ErrorClass::Constraint
        } else if code.starts_with("40") || code.starts_with("08") {
            // 40xxx: serialization / deadlock / rollback; 08xxx: connection.
            ErrorClass::Retryable
        } else if matches!(code.as_ref(), "57P01" | "57P02" | "57P03") {
            // Server shutting down / cannot connect now — transient.
            ErrorClass::Retryable
        } else {
            ErrorClass::Other
        }
    } else {
        ErrorClass::Other
    }
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

    fn db_err(code: &str) -> sqlx::Error {
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
        // Permanent, non-constraint.
        assert_eq!(classify(&db_err("42P01")), ErrorClass::Other); // undefined table
        assert_eq!(classify(&db_err("22012")), ErrorClass::Other); // division by zero
        assert_eq!(classify(&db_err("99999")), ErrorClass::Other);
    }
    #[test]
    fn numeric_sqlstates_survive_the_sqlite_check() {
        // All-digit SQLSTATEs parse as u32 and hit the SQLite branch first,
        // but only low-byte 19/5 classify there — these must fall through to
        // the SQLSTATE branch unchanged.
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
                                                                     // Anything else is permanent.
        assert_eq!(classify(&db_err("1")), ErrorClass::Other); // SQLITE_ERROR
    }

    #[test]
    fn map_write_err_produces_constraint_or_backend() {
        match map_write_err(db_err("23505"), |m| format!("upsert concept: {m}")) {
            StoreError::Constraint(code) => assert_eq!(code, "23505"),
            other => panic!("expected Constraint, got {other:?}"),
        }
        match map_write_err(db_err("42P01"), |m| format!("upsert concept: {m}")) {
            StoreError::Backend(msg) => {
                assert!(msg.contains("upsert concept"), "context missing: {msg}");
                assert!(msg.contains("42P01"), "code missing: {msg}");
            }
            other => panic!("expected Backend, got {other:?}"),
        }
    }
}
