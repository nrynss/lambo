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

use crate::embed::{EmbedError, Embedder};

pub mod citations;
pub mod dependents;
#[cfg(feature = "embed-fixture")]
pub mod dresscode;
pub mod fair_test;
pub mod supersession;
#[cfg(feature = "store-memory")]
mod vector_searchable;
#[cfg(feature = "store-memory")]
pub use vector_searchable::{GraphRanked, VectorSearchable};

/// Give a synthetic concept cosine 0.7 to a fixture query, while keeping
/// distinct concepts below the vector-dedupe threshold. Hash fixture vectors
/// model identity, not semantic relatedness; layer integration tests need
/// explicit relevance now that keywords cannot bypass the floor (#157).
#[cfg(feature = "embed-fixture")]
pub async fn seed_layer_match(memory: &crate::Memory, content: &str, query: &str) {
    let fixture = crate::embed::FixtureEmbedder::with_dimensions(memory.embedding_contract().dim)
        .expect("fixture width");
    let q = fixture.embed_sync(query);
    let mut orthogonal = fixture.embed_sync(content);
    let dot: f32 = q.iter().zip(&orthogonal).map(|(a, b)| a * b).sum();
    for (r, q) in orthogonal.iter_mut().zip(&q) {
        *r -= dot * q;
    }
    let norm = orthogonal.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(norm > 0.01, "use distinct synthetic content and query");
    let vector: Vec<f32> = q
        .iter()
        .zip(orthogonal)
        .map(|(q, r)| 0.7 * q + 0.51_f32.sqrt() * r / norm)
        .collect();
    {
        let mut graph = memory.graph().write();
        let updates = graph
            .concepts()
            .map(|c| {
                (
                    c.id,
                    if c.content == content {
                        vector.clone()
                    } else {
                        c.embedding
                            .clone()
                            .unwrap_or_else(|| fixture.embed_sync(&c.content))
                    },
                )
            })
            .collect();
        graph.seed_test_vectors(updates);
    }
    memory.settle_daemon().await;
}

/// The text role a delegating test embedder forwards (#22).
///
/// A wrapper that counts, gates, refuses or rewrites embeds keeps its
/// behaviour in one `embed_as(text, role)` and calls [`TextRole::embed`] on
/// its inner embedder where it used to call `embed`. Its `embed` passes
/// [`TextRole::Document`] and its `embed_query` [`TextRole::Query`], so the
/// wrapper does the same thing in both roles while the inner adapter sees the
/// role the caller asked for. (Implementing only `embed` would hand every
/// query to the inner adapter's *document* role: the trap the `Embedder`
/// docs describe.)
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TextRole {
    /// [`Embedder::embed`].
    Document,
    /// [`Embedder::embed_query`].
    Query,
}

impl TextRole {
    /// Embed `text` through `inner` in this role.
    pub async fn embed(self, inner: &dyn Embedder, text: &str) -> Result<Vec<f32>, EmbedError> {
        match self {
            Self::Document => inner.embed(text).await,
            Self::Query => inner.embed_query(text).await,
        }
    }
}

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

/// A gcloud configuration directory that does not exist, for tests that must see no
/// gcloud application-default credentials. See [`EnvGuard::clear_google_credentials`].
pub const NO_GCLOUD_CONFIG_DIR: &str = "/nonexistent/lambo-test-no-gcloud-config";

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

    /// Leave this process no Google credential to find (issue #7): both credential
    /// variables unset, and `CLOUDSDK_CONFIG` pointed at a directory that does not exist,
    /// so the chain cannot fall through to a developer's own `gcloud auth
    /// application-default login` file under the real `HOME`. Removing the two variables
    /// alone is no longer "no credential" on a machine that has run gcloud.
    pub fn clear_google_credentials(&self) {
        self.remove("GCP_LAMBO_CREDENTIALS");
        self.remove("GOOGLE_APPLICATION_CREDENTIALS");
        self.set("CLOUDSDK_CONFIG", NO_GCLOUD_CONFIG_DIR);
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

#[cfg(test)]
mod text_role_tests {
    use super::TextRole;
    use crate::embed::{EmbedError, Embedder};

    /// Answers `[1, 0]` as a document and `[0, 1]` as a query.
    struct TwoRoles;

    #[async_trait::async_trait]
    impl Embedder for TwoRoles {
        fn dimensions(&self) -> usize {
            2
        }
        async fn embed(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            Ok(vec![1.0, 0.0])
        }
        async fn embed_query(&self, _text: &str) -> Result<Vec<f32>, EmbedError> {
            Ok(vec![0.0, 1.0])
        }
    }

    /// #22: the helper every delegating test embedder forwards through
    /// reaches the role it names, so a wrapper never turns a query into a
    /// document embed.
    #[tokio::test]
    async fn each_role_reaches_its_own_method() {
        assert_eq!(
            TextRole::Document.embed(&TwoRoles, "x").await.unwrap(),
            vec![1.0, 0.0]
        );
        assert_eq!(
            TextRole::Query.embed(&TwoRoles, "x").await.unwrap(),
            vec![0.0, 1.0]
        );
    }
}

/// One raw HTTP/1.1 exchange: the full response as sent, minus the `date`
/// header (the only field that varies between two requests). The byte-level
/// comparison behind the uniform-404 claim (#32 PR 1 review L2), shared by
/// `surface::session`'s router test and the serve's own router (#32 PR 4
/// review L7).
pub async fn on_the_wire(addr: std::net::SocketAddr, method: &str, path: &str) -> String {
    on_the_wire_as(addr, method, path, None).await
}

/// [`on_the_wire`] presenting an `Authorization` header value, for a
/// surface that authenticates (#32 PR 5): the comparison is between two
/// requests from the same authenticated caller.
pub async fn on_the_wire_as(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    authorization: Option<&str>,
) -> String {
    on_the_wire_with(addr, method, path, "localhost", authorization, &[], "").await
}

/// [`on_the_wire_as`] with the `Host` header, further headers and a body
/// given (#32 PR 5 review M1 and L5): a request with a non-loopback `Host`,
/// an `Mcp-Session-Id` or a JSON-RPC body. With no extra header and an
/// empty body the request bytes are exactly [`on_the_wire_as`]'s.
pub async fn on_the_wire_with(
    addr: std::net::SocketAddr,
    method: &str,
    path: &str,
    host: &str,
    authorization: Option<&str>,
    headers: &[(&str, &str)],
    body: &str,
) -> String {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let mut stream = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let authorization = authorization
        .map(|value| format!("Authorization: {value}\r\n"))
        .unwrap_or_default();
    let extra: String = headers
        .iter()
        .map(|(name, value)| format!("{name}: {value}\r\n"))
        .collect();
    // Head and body in one write, so a refusal answered before the body is
    // read cannot race a second segment into a reset.
    let request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {host}\r\nConnection: close\r\n{authorization}{extra}Content-Length: {}\r\n\r\n{body}",
        body.len()
    );
    stream.write_all(request.as_bytes()).await.expect("write");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).await.expect("read");
    String::from_utf8(raw)
        .expect("utf-8 response")
        .split("\r\n")
        .filter(|line| !line.to_ascii_lowercase().starts_with("date:"))
        .collect::<Vec<_>>()
        .join("\r\n")
}

/// A batching test embedder over [`crate::embed::FixtureEmbedder`] (#125):
/// it counts single `embed` calls and `embed_batch` rounds separately, so a
/// test can prove that gathered writes cost fewer embedder rounds.
///
/// * A text containing [`BatchingEmbedder::REFUSE`] fails its own slot (and
///   its own single embed) as a permanent `Backend` refusal.
/// * With `fail_rounds` set, every `embed_batch` round fails every slot as
///   `Unavailable` (a whole-batch outage), while single embeds still work.
/// * With a `gate`, every round and every single embed first takes one
///   permit, so a test can hold the embedder while it queues writes.
#[cfg(feature = "embed-fixture")]
pub struct BatchingEmbedder {
    pub inner: crate::embed::FixtureEmbedder,
    pub capacity: usize,
    pub singles: Arc<std::sync::atomic::AtomicUsize>,
    pub rounds: Arc<std::sync::atomic::AtomicUsize>,
    pub round_sizes: Arc<PlMutex<Vec<usize>>>,
    pub fail_rounds: Arc<std::sync::atomic::AtomicBool>,
    pub gate: Option<Arc<tokio::sync::Semaphore>>,
}

#[cfg(feature = "embed-fixture")]
impl BatchingEmbedder {
    pub const REFUSE: &'static str = "REFUSE-ME";

    pub fn new(capacity: usize) -> Self {
        Self {
            inner: crate::embed::FixtureEmbedder::new(),
            capacity,
            singles: Arc::default(),
            rounds: Arc::default(),
            round_sizes: Arc::default(),
            fail_rounds: Arc::default(),
            gate: None,
        }
    }

    pub fn singles(&self) -> usize {
        self.singles.load(std::sync::atomic::Ordering::SeqCst)
    }

    pub fn rounds(&self) -> usize {
        self.rounds.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Every embedder call: rounds plus single embeds.
    pub fn calls(&self) -> usize {
        self.singles() + self.rounds()
    }

    async fn pass_gate(&self) {
        if let Some(gate) = &self.gate {
            gate.acquire()
                .await
                .expect("the gate outlives its holders")
                .forget();
        }
    }

    async fn one(&self, text: &str, role: TextRole) -> Result<Vec<f32>, EmbedError> {
        if text.contains(Self::REFUSE) {
            return Err(EmbedError::Backend("refused by the test embedder".into()));
        }
        role.embed(&self.inner, text).await
    }

    async fn embed_as(&self, text: &str, role: TextRole) -> Result<Vec<f32>, EmbedError> {
        self.singles
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.pass_gate().await;
        self.one(text, role).await
    }
}

#[cfg(feature = "embed-fixture")]
#[async_trait::async_trait]
impl Embedder for BatchingEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed_as(text, TextRole::Document).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.embed_as(text, TextRole::Query).await
    }
    async fn embed_batch(
        &self,
        texts: &[&str],
        deadline: tokio::time::Instant,
    ) -> Vec<Result<Vec<f32>, EmbedError>> {
        assert!(
            texts.len() <= self.capacity,
            "a caller sent {} texts to an embedder of capacity {}",
            texts.len(),
            self.capacity
        );
        self.rounds
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.round_sizes.lock().push(texts.len());
        if tokio::time::timeout_at(deadline, self.pass_gate())
            .await
            .is_err()
        {
            return texts
                .iter()
                .map(|_| Err(crate::embed::deadline_passed()))
                .collect();
        }
        if self.fail_rounds.load(std::sync::atomic::Ordering::SeqCst) {
            return texts
                .iter()
                .map(|_| Err(EmbedError::Unavailable("batch backend down".into())))
                .collect();
        }
        let mut out = Vec::with_capacity(texts.len());
        for text in texts {
            out.push(self.one(text, TextRole::Document).await);
        }
        out
    }
    fn batch_capacity(&self) -> usize {
        self.capacity
    }
    fn modalities(&self) -> crate::embed::Modalities {
        self.inner.modalities()
    }
    async fn embed_image(
        &self,
        image: crate::embed::ImageInput<'_>,
    ) -> Result<Vec<f32>, EmbedError> {
        self.inner.embed_image(image).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}
