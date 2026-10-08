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

/// Take the process-environment lock: the only way a lib test may mutate the
/// environment.
///
/// Every lib test that sets or removes a variable does it through the returned
/// [`EnvGuard`], so mutations are serialised behind one global mutex, and each
/// variable the guard touched is put back to its value from before the first
/// touch when the guard drops (on the panic path too). A test that only needs
/// a quiet environment for a read can hold the guard without mutating
/// anything.
pub fn env_lock() -> EnvGuard {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = LOCK
        .get_or_init(|| Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    EnvGuard {
        _lock: lock,
        saved: std::cell::RefCell::new(Vec::new()),
    }
}

/// Proof that the caller holds the process-environment lock, and the scope of
/// its mutations. See [`env_lock`].
pub struct EnvGuard {
    _lock: MutexGuard<'static, ()>,
    /// First-touch values, in touch order: `None` means "was unset".
    saved: std::cell::RefCell<Vec<(std::ffi::OsString, Option<std::ffi::OsString>)>>,
}

impl EnvGuard {
    /// Set `key` to `value` until this guard drops.
    pub fn set(&self, key: impl AsRef<std::ffi::OsStr>, value: impl AsRef<std::ffi::OsStr>) {
        let key = key.as_ref();
        self.remember(key);
        // SAFETY: see `mutate`.
        unsafe { Self::mutate(key, Some(value.as_ref())) }
    }

    /// Unset `key` until this guard drops.
    pub fn remove(&self, key: impl AsRef<std::ffi::OsStr>) {
        let key = key.as_ref();
        self.remember(key);
        // SAFETY: see `mutate`.
        unsafe { Self::mutate(key, None) }
    }

    fn remember(&self, key: &std::ffi::OsStr) {
        let mut saved = self.saved.borrow_mut();
        if !saved.iter().any(|(k, _)| k == key) {
            saved.push((key.to_owned(), std::env::var_os(key)));
        }
    }

    /// The single place lib tests call `set_var` / `remove_var`.
    ///
    /// # Safety
    ///
    /// Only called through `&self`, and an `EnvGuard` exists only while its
    /// holder owns the global env mutex, so no two lib tests mutate the
    /// environment at once and no env-reading test that also holds the lock
    /// runs concurrently with a mutation. `std::env::var`/`var_os` readers
    /// elsewhere are synchronised with these calls by std's own environment
    /// lock. What the mutex cannot exclude is a thread reading the
    /// environment through libc directly (`getenv` inside a C library) while
    /// another test mutates it (libc may reallocate `environ` on any `setenv`).
    /// That residual exposure is the one every `cargo test` binary that sets
    /// a variable carries; the lock removes the test-against-test races. Callers must
    /// hold the env mutex: only `EnvGuard` methods and its `Drop` call this.
    unsafe fn mutate(key: &std::ffi::OsStr, value: Option<&std::ffi::OsStr>) {
        match value {
            Some(v) => unsafe { std::env::set_var(key, v) },
            None => unsafe { std::env::remove_var(key) },
        }
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        // Restore in reverse touch order while the mutex is still held (the
        // `_lock` field drops after this body runs).
        for (key, old) in self.saved.get_mut().drain(..).rev() {
            // SAFETY: see `mutate`; the lock is held for the whole body.
            unsafe { Self::mutate(&key, old.as_deref()) }
        }
    }
}

/// Run an async cleanup to completion from a synchronous `Drop`, whatever
/// runtime the test is on: a fresh thread owns its own current-thread runtime,
/// so this neither needs nor disturbs the caller's (blocking a worker of the
/// test's own runtime would deadlock a `current_thread` one). Cleanup is best
/// effort by design: it runs on the panic path too, so it must not panic.
pub fn run_blocking<F>(cleanup: F)
where
    F: std::future::Future<Output = ()> + Send + 'static,
{
    let joined = std::thread::spawn(move || {
        match tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
        {
            Ok(rt) => rt.block_on(cleanup),
            Err(e) => eprintln!("test cleanup: no runtime: {e}"),
        }
    })
    .join();
    if joined.is_err() {
        eprintln!("test cleanup: the cleanup thread panicked");
    }
}

// ---------------------------------------------------------------------------
// Scratch directories
// ---------------------------------------------------------------------------

/// A unique scratch directory under `std::env::temp_dir()`, removed on drop so a
/// test that panics part-way leaves nothing under `$TMPDIR`. Derefs to
/// [`std::path::Path`]. Not for socket paths: `temp_dir()` is 46 bytes on macOS,
/// so tests that bind or dial an endpoint use [`ScratchDir::short`].
pub struct ScratchDir {
    path: std::path::PathBuf,
}

impl ScratchDir {
    /// Create `<temp_dir>/<prefix>-<pid>-<uuid>`.
    pub fn new(prefix: &str) -> Self {
        let path = std::env::temp_dir().join(format!(
            "{prefix}-{}-{}",
            std::process::id(),
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&path)
            .unwrap_or_else(|e| panic!("scratch dir {}: {e}", path.display()));
        Self { path }
    }

    /// Create `/tmp/lb<tag><pid>_<n>`: a scratch directory short enough to hold
    /// a unix socket address (`SUN_PATH_MAX`), for the tests that bind or dial
    /// an endpoint inside it. [`ScratchDir::new`] cannot serve those: macOS's
    /// `temp_dir()` is 46 bytes before anything is joined to it.
    ///
    /// A process-wide counter, not a clock fragment, so two calls in one process
    /// never collide and the name stays short. Created exclusively (0700 on
    /// unix): an existing directory (a recycled pid's leftover) is not ours, so
    /// the next number is taken rather than sharing it.
    #[cfg(unix)]
    pub fn short(tag: &str) -> Self {
        use std::os::unix::fs::DirBuilderExt;
        use std::sync::atomic::{AtomicU32, Ordering};
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let pid = std::process::id();
        loop {
            let n = NEXT.fetch_add(1, Ordering::Relaxed);
            let path = std::path::PathBuf::from(format!("/tmp/lb{tag}{pid}_{n}"));
            match std::fs::DirBuilder::new().mode(0o700).create(&path) {
                Ok(()) => return Self { path },
                Err(e) if e.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(e) => panic!("short scratch dir {}: {e}", path.display()),
            }
        }
    }
}

impl std::ops::Deref for ScratchDir {
    type Target = std::path::Path;
    fn deref(&self) -> &std::path::Path {
        &self.path
    }
}

impl AsRef<std::path::Path> for ScratchDir {
    fn as_ref(&self) -> &std::path::Path {
        &self.path
    }
}

impl Drop for ScratchDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
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

#[cfg(test)]
mod run_blocking_tests {
    use super::run_blocking;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    /// The cleanup runs to completion from a synchronous context on a
    /// current-thread runtime (what `#[tokio::test]` is), which a plain
    /// `block_on` on the caller's own thread could not do.
    #[tokio::test]
    async fn runs_an_async_cleanup_from_inside_a_current_thread_runtime() {
        let done = Arc::new(AtomicBool::new(false));
        let flag = done.clone();
        run_blocking(async move {
            tokio::task::yield_now().await;
            flag.store(true, Ordering::SeqCst);
        });
        assert!(done.load(Ordering::SeqCst));
    }
}

#[cfg(test)]
mod env_guard_tests {
    use super::env_lock;

    /// Every variable an `EnvGuard` touched is back to its first-touch state
    /// once the guard drops: a set variable that was unset is unset again,
    /// and a removed variable that was set comes back with its old value,
    /// however many times either was changed in between. The "was set" half
    /// uses `CARGO_PKG_NAME`, which Cargo exports to every test binary it
    /// runs and nothing in the crate reads at run time.
    #[test]
    fn restores_every_touched_variable_on_drop() {
        const UNSET: &str = "LAMBO_TEST_ENV_GUARD_WAS_UNSET";
        const SET: &str = "CARGO_PKG_NAME";
        let original = std::env::var_os(SET).expect("cargo test exports CARGO_PKG_NAME");
        {
            let env = env_lock();
            assert_eq!(std::env::var_os(UNSET), None);
            env.set(UNSET, "a");
            env.set(UNSET, "b");
            env.remove(SET);
            env.set(SET, "changed");
            assert_eq!(std::env::var(UNSET).as_deref(), Ok("b"));
            assert_eq!(std::env::var(SET).as_deref(), Ok("changed"));
        }
        let _env = env_lock();
        assert_eq!(std::env::var_os(UNSET), None);
        assert_eq!(std::env::var_os(SET), Some(original));
    }
}
