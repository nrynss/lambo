//! Test-only helpers shared across modules.
//!
//! Keep env mutation behind a single lock so store/embed/main tests do not race
//! under `cargo test` parallelism, and keep every tracing-capture site in the
//! binary on one subscriber-installation path (R3-2).

#![cfg(test)]

use std::io;
use std::sync::{Arc, LazyLock, Mutex, MutexGuard, OnceLock};

use parking_lot::Mutex as PlMutex;
use tracing_subscriber::fmt::MakeWriter;

/// Global mutex for any test that sets/removes process environment variables.
pub fn env_lock() -> MutexGuard<'static, ()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Tracing capture
// ---------------------------------------------------------------------------

/// A dispatcher registered for the whole test binary and default nowhere, held
/// alive only to keep `tracing`'s global caches honest.
///
/// Callsite interest and the global max level are cached **globally**, and
/// `tracing_core` rebuilds them from *the calling thread's* default subscriber
/// whenever only one dispatcher is registered (`Rebuilder::JustOne` ->
/// `get_default`). These tests run in parallel, each installing its own
/// `set_default` subscriber at its own max level and dropping it again — so a
/// rebuild triggered from a thread whose subscriber is ERROR-only pins the
/// global max level at ERROR, and another thread's WARN event is discarded
/// before any subscriber sees it. Measured at roughly one suite run in twenty,
/// as the R2-2 drop-warning assertion failing against a buffer that was missing
/// an event the code had definitely emitted.
///
/// A second live registrant keeps the registry past that one-dispatcher
/// shortcut, so every rebuild takes the max over *all* live dispatchers.
/// `NoSubscriber` gives no level hint — which counts as TRACE — and claims no
/// callsite (`Interest::never`), so it raises the ceiling without capturing
/// anything or changing what any subscriber receives.
///
/// **It has to be forced before every capture registration in the binary, not
/// just some** (R3-2). The floor works by being registered *first*: it is the
/// registration of a capturing subscriber that triggers the rebuild, and a
/// rebuild only takes the max over dispatchers that are already live. One site
/// forcing it does not protect a sibling site that races ahead of it in another
/// thread — and the eight sites that did not force it were in this same test
/// binary, sharing the same global caches. So the floor lives here, private,
/// and the only ways to install a subscriber are the two functions below, which
/// force it. Adding a capture site cannot forget it.
static TRACE_FLOOR: LazyLock<tracing::Dispatch> =
    LazyLock::new(|| tracing::Dispatch::new(tracing::subscriber::NoSubscriber::default()));

/// Capturing writer behind [`capture_logs`].
#[derive(Clone)]
struct BufWriter(Arc<PlMutex<Vec<u8>>>);

impl io::Write for BufWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for BufWriter {
    type Writer = BufWriter;
    fn make_writer(&'a self) -> Self::Writer {
        self.clone()
    }
}

/// Everything the capturing subscriber has written so far.
///
/// Cloneable and `Send`, so a test may hand it to a spawned task; reading it is
/// a snapshot, not a drain.
#[derive(Clone)]
pub struct CapturedLogs(Arc<PlMutex<Vec<u8>>>);

impl CapturedLogs {
    /// The whole buffer as text (lossy — these are formatted log lines).
    pub fn contents(&self) -> String {
        String::from_utf8_lossy(&self.0.lock()).into_owned()
    }

    /// The captured lines, blank ones dropped — for the sites that assert on
    /// how *many* events were emitted.
    pub fn lines(&self) -> Vec<String> {
        self.contents()
            .lines()
            .filter(|l| !l.trim().is_empty())
            .map(str::to_owned)
            .collect()
    }

    /// `true` if any captured line contains `needle`.
    pub fn contains(&self, needle: &str) -> bool {
        self.contents().contains(needle)
    }
}

/// Capture this thread's tracing output at `level`, for as long as the returned
/// guard lives.
///
/// Forces [`TRACE_FLOOR`] first, so this subscriber's own registration is the
/// rebuild that re-evaluates every callsite with the floor included.
pub fn capture_logs(level: tracing::Level) -> (CapturedLogs, tracing::subscriber::DefaultGuard) {
    LazyLock::force(&TRACE_FLOOR);
    let buf = Arc::new(PlMutex::new(Vec::new()));
    let subscriber = tracing_subscriber::fmt()
        .with_writer(BufWriter(buf.clone()))
        .with_max_level(level)
        .finish();
    let guard = tracing::subscriber::set_default(subscriber);
    (CapturedLogs(buf), guard)
}

/// Install a thread-local default that registers tracing callsites as
/// `always`-interested while dropping every event.
///
/// `tracing` caches each callsite's `Interest` process-wide at first
/// registration; with no default subscriber that interest is `never`, so a
/// callsite first reached by a test that installs nothing becomes permanently
/// disabled for *every* test — including the one that asserts on it through a
/// capturing subscriber. The case that bit was `store::flush`'s shared
/// `BackendFlushFailed` warn callsite in `cycle`, asserted by
/// `degrades_past_log_max_and_stops_flushing`. Any test that can reach such a
/// callsite without wanting its output must install this guard so the callsite
/// can never be poisoned. `TRACE` keeps the filter from returning `never`; the
/// sink writer keeps the events silent.
pub fn quiet_logs() -> tracing::subscriber::DefaultGuard {
    LazyLock::force(&TRACE_FLOOR);
    tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_max_level(tracing::Level::TRACE)
            .with_writer(io::sink)
            .finish(),
    )
}
