//! Test-only helpers shared across modules.
//!
//! Keep env mutation behind a single lock so store/embed/main tests do not race
//! under `cargo test` parallelism.

#![cfg(test)]

use std::sync::{Mutex, MutexGuard, OnceLock};

/// Global mutex for any test that sets/removes process environment variables.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
