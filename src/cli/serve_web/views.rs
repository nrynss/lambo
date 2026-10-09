//! Per-session reader views (#4 PR 1, design section 5).
//!
//! A [`SessionView`] is ONE store load of a session, turned into everything
//! the data routes read: the reader graph and index, the ordered canonization
//! feed, the counts and the embedding status. It is immutable once built and
//! shared by `Arc`, so every request and every open tab reads the same load
//! until it is older than the TTL.
//!
//! [`ViewCache`] holds one slot per served session (one today; the allowlist
//! in PR 2) and bounds the work:
//!
//! * **TTL.** A request finding a view younger than `view_ttl_ms` uses it. An
//!   older one is reloaded on the requesting task, and the request waits.
//! * **Single-flight.** Concurrent requests for one stale session share one
//!   load: they queue on the slot's gate, and a request that arrived before
//!   a load finished takes that load's outcome (view or error) instead of
//!   loading again. With a TTL of 0 that is still one load per burst.
//! * **Load concurrency.** A semaphore bounds simultaneous loads across
//!   sessions (1 on SQLite, whose pool is one connection).
//! * **LRU.** At most `max_loaded_sessions` views are held; the least
//!   recently used is dropped beyond that. A request already holding it keeps
//!   its `Arc`. Eviction drops the view only: the slot's freshness tracker and
//!   query-embedding cache stay, so a reloaded session does not report "just
//!   changed" and does not re-embed a repeated query.
//! * **Recall concurrency.** A second semaphore bounds simultaneous recalls
//!   (embed and pipeline work). A recall that waits [`RECALL_PERMIT_WAIT`]
//!   for a permit is answered 503 with `Retry-After: 1`, after the session
//!   was resolved, so it is no oracle (design 5.3).
//! * **Query embeddings (#14).** Each slot keeps its own query-embedding
//!   LRU (128 entries or 1 MiB), so a repeated recall query skips the embed.
//!   One per session, never process-wide (design 5.4).
//! * **Failure is not cached.** A failed load is answered to the requests
//!   that joined it, and the next request retries, behind the semaphore.
//! * **No background task.** Nothing refreshes a session nobody is viewing.
//!
//! The embedding status is computed per view, so contract, trust and the
//! mismatch warning are per session by construction (design 5.2).

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use parking_lot::Mutex;
use tokio::sync::{Mutex as AsyncMutex, Semaphore, SemaphorePermit};
use tokio::time::Instant;

use super::dto::{EmbeddingStatus, EventsPayload, WebEvent};
use super::projections::{ordered_events, slice_events};
use crate::cli::caps::CliError;
use crate::cli::{load_reader_graph, LoadedReader};
use crate::config::WebConfig;
use crate::recall::query_cache::QueryEmbeddingCache;
use crate::store::{GraphStore, StoreKind};
use crate::types::{CanonizationStatus, EmbeddingContract, SessionId};

/// How long a recall waits for a [`ViewBounds::recall_concurrency`] permit
/// before the portal answers 503 (design 5.3).
pub(super) const RECALL_PERMIT_WAIT: Duration = Duration::from_secs(2);

/// The cache's bounds, defaults applied and the SQLite rule enforced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct ViewBounds {
    pub(super) ttl: Duration,
    pub(super) max_loaded_sessions: usize,
    pub(super) load_concurrency: usize,
    /// Simultaneous recalls, process-wide (embed and pipeline work).
    pub(super) recall_concurrency: usize,
}

impl ViewBounds {
    /// `[web]` for a store of `kind`. SQLite loads one at a time whatever
    /// `load_concurrency` says: the portal's pool is one connection, and a
    /// second permit would only queue inside the pool while holding it.
    pub(super) fn resolve(web: &WebConfig, kind: StoreKind) -> Self {
        Self {
            ttl: web.view_ttl(),
            max_loaded_sessions: web.max_loaded_sessions(),
            load_concurrency: if kind == StoreKind::Sqlite {
                1
            } else {
                web.load_concurrency()
            },
            recall_concurrency: web.recall_concurrency(),
        }
    }
}

/// The counts `/api/stats` reports, taken once per load.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct ViewCounts {
    pub(super) nodes: usize,
    pub(super) edges: usize,
    pub(super) concepts: usize,
    pub(super) canonical: usize,
}

/// One load of one session, and everything derived from it.
pub(super) struct SessionView {
    /// The reader graph and index, from the load.
    pub(super) reader: LoadedReader,
    /// The whole ordered canonization feed, `seq` from 0.
    events: Vec<WebEvent>,
    pub(super) counts: ViewCounts,
    /// Stored versus configured embedding contract, for THIS session.
    pub(super) embedding: EmbeddingStatus,
    loaded_at: Instant,
}

impl SessionView {
    fn build(reader: LoadedReader, configured: &EmbeddingContract) -> Self {
        let (events, counts, embedding) = {
            let g = reader.graph.read();
            let counts = ViewCounts {
                nodes: g.node_count(),
                edges: g.edge_count(),
                concepts: g.concepts().count(),
                canonical: g
                    .concepts()
                    .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
                    .count(),
            };
            (
                ordered_events(g.concepts(), g.canonization_events()),
                counts,
                EmbeddingStatus::inspect(g.embedding(), configured),
            )
        };
        Self {
            reader,
            events,
            counts,
            embedding,
            loaded_at: Instant::now(),
        }
    }

    /// The feed at or after the cursor `since`.
    pub(super) fn events_since(&self, since: usize) -> EventsPayload {
        slice_events(&self.events, since)
    }

    /// Every transition recorded for the session.
    pub(super) fn event_total(&self) -> usize {
        self.events.len()
    }

    fn fresh(&self, ttl: Duration) -> bool {
        self.loaded_at.elapsed() < ttl
    }

    /// A rough size of the view: every concept's vector at the configured
    /// width plus its text. Logged, never enforced (design Q17).
    fn estimated_bytes(&self, dim: usize) -> usize {
        let g = self.reader.graph.read();
        g.concepts()
            .map(|c| dim * std::mem::size_of::<f32>() + c.content.len())
            .sum()
    }
}

/// When this reader last saw a session's durable snapshot *change*.
struct Freshness {
    fingerprint: u64,
    observed_at: std::time::Instant,
}

/// A load's failure, kept only for the requests that joined that load.
#[derive(Clone)]
enum LoadFailure {
    Usage(String),
    Runtime(String),
}

impl LoadFailure {
    fn of(err: &CliError) -> Self {
        match err {
            CliError::Usage(m) => Self::Usage(m.clone()),
            CliError::Runtime(m) => Self::Runtime(m.clone()),
        }
    }

    fn to_error(&self) -> CliError {
        match self {
            Self::Usage(m) => CliError::Usage(m.clone()),
            Self::Runtime(m) => CliError::Runtime(m.clone()),
        }
    }
}

#[derive(Default)]
struct SlotState {
    /// The current view, if one is loaded and not evicted.
    ready: Option<Arc<SessionView>>,
    /// Loads finished (successful or not). A request that saw a smaller
    /// number on arrival joined a load that finished since.
    completed: u64,
    /// The outcome of the last finished load, when it failed.
    failure: Option<LoadFailure>,
    /// The once-per-session mismatch warning has been printed.
    warned_mismatch: bool,
}

/// One served session's slot. Never removed: eviction clears `ready` only.
struct Slot {
    /// Single-flight: one loader at a time; joiners wait here.
    gate: AsyncMutex<()>,
    state: Mutex<SlotState>,
    /// LRU stamp from [`ViewCache::tick`].
    last_used: AtomicU64,
    freshness: Mutex<Freshness>,
    /// #14's query-embedding LRU, ONE PER SESSION: a text-keyed cache shared
    /// across sessions is a cross-tenant timing oracle (#32 decision 13). An
    /// empty one allocates nothing, so a served session nobody recalls on
    /// costs nothing here.
    queries: Mutex<QueryEmbeddingCache>,
}

impl Slot {
    fn new() -> Self {
        Self {
            gate: AsyncMutex::new(()),
            state: Mutex::new(SlotState::default()),
            last_used: AtomicU64::new(0),
            freshness: Mutex::new(Freshness {
                fingerprint: 0,
                observed_at: std::time::Instant::now(),
            }),
            queries: Mutex::new(QueryEmbeddingCache::new()),
        }
    }
}

/// The portal's bounded cache of per-session views. See the module docs.
pub(super) struct ViewCache {
    bounds: ViewBounds,
    slots: HashMap<SessionId, Slot>,
    loads: Semaphore,
    recalls: Semaphore,
    tick: AtomicU64,
}

impl ViewCache {
    /// A cache serving exactly `sessions`. Nothing is loaded until a request
    /// asks (design Q15).
    pub(super) fn new(sessions: impl IntoIterator<Item = SessionId>, bounds: ViewBounds) -> Self {
        Self {
            slots: sessions.into_iter().map(|s| (s, Slot::new())).collect(),
            loads: Semaphore::new(bounds.load_concurrency),
            recalls: Semaphore::new(bounds.recall_concurrency),
            tick: AtomicU64::new(0),
            bounds,
        }
    }

    pub(super) fn bounds(&self) -> &ViewBounds {
        &self.bounds
    }

    fn slot(&self, session: &SessionId) -> Result<&Slot, CliError> {
        // Unreachable through the router: a request names a served session
        // or is refused before it gets here.
        self.slots.get(session).ok_or_else(|| {
            CliError::Runtime(format!("serve-web: session '{session}' is not served here"))
        })
    }

    /// The view of `session`: the current one while it is younger than the
    /// TTL, otherwise a fresh load, single-flight and bounded. `configured`
    /// is the live embedder's contract, for the view's embedding status.
    pub(super) async fn view(
        &self,
        store: &dyn GraphStore,
        configured: &EmbeddingContract,
        session: &SessionId,
    ) -> Result<Arc<SessionView>, CliError> {
        let slot = self.slot(session)?;
        slot.last_used.store(
            self.tick.fetch_add(1, Ordering::Relaxed) + 1,
            Ordering::Relaxed,
        );

        let arrived = {
            let state = slot.state.lock();
            if let Some(view) = &state.ready
                && view.fresh(self.bounds.ttl)
            {
                return Ok(view.clone());
            }
            state.completed
        };

        let _gate = slot.gate.lock().await;
        {
            let state = slot.state.lock();
            // A load finished while this request queued: take its outcome
            // rather than loading again (single-flight, TTL 0 included).
            if state.completed > arrived {
                match (&state.failure, &state.ready) {
                    (Some(failure), _) => return Err(failure.to_error()),
                    (None, Some(view)) => return Ok(view.clone()),
                    // Another session's load evicted this view between its
                    // load finishing and this request taking the gate (more
                    // active sessions than `max_loaded_sessions`). Not an
                    // error: load it again below, still under the gate.
                    (None, None) => {}
                }
            }
            if let Some(view) = &state.ready
                && view.fresh(self.bounds.ttl)
            {
                return Ok(view.clone());
            }
        }

        let loaded = {
            // The semaphore is never closed, so `acquire` cannot fail.
            let _permit = self
                .loads
                .acquire()
                .await
                .map_err(|e| CliError::Runtime(format!("serve-web: load bound: {e}")))?;
            load_reader_graph(store, session.as_str()).await
        };
        let outcome = loaded.map(|reader| Arc::new(SessionView::build(reader, configured)));

        let warn = {
            let mut state = slot.state.lock();
            state.completed += 1;
            match &outcome {
                Ok(view) => {
                    state.ready = Some(view.clone());
                    state.failure = None;
                    let warn = view.embedding.status == "mismatch" && !state.warned_mismatch;
                    state.warned_mismatch |= warn;
                    warn
                }
                Err(err) => {
                    state.failure = Some(LoadFailure::of(err));
                    false
                }
            }
        };
        let view = outcome?;
        if warn {
            // The startup warning of the single-session portal, moved to the
            // session's first load (design 5.1): an operator log, not a
            // response.
            eprintln!(
                "lambo serve-web: WARNING — vector recall is disabled for session '{session}': {}",
                view.embedding
                    .message
                    .as_deref()
                    .unwrap_or("stored and configured embedding contracts differ")
            );
        }
        tracing::debug!(
            session = %session,
            concepts = view.counts.concepts,
            estimated_bytes = view.estimated_bytes(configured.dim),
            "serve-web: loaded a session view"
        );
        self.evict_beyond_cap(session);
        Ok(view)
    }

    /// Drop least-recently-used views until at most `max_loaded_sessions`
    /// are held, never `keep` (the one just loaded).
    fn evict_beyond_cap(&self, keep: &SessionId) {
        loop {
            let mut loaded = 0;
            let mut oldest: Option<(&Slot, u64)> = None;
            for (id, slot) in &self.slots {
                if slot.state.lock().ready.is_none() {
                    continue;
                }
                loaded += 1;
                let used = slot.last_used.load(Ordering::Relaxed);
                if id != keep && oldest.is_none_or(|(_, u)| used < u) {
                    oldest = Some((slot, used));
                }
            }
            match oldest {
                Some((slot, _)) if loaded > self.bounds.max_loaded_sessions => {
                    slot.state.lock().ready = None;
                }
                _ => return,
            }
        }
    }

    /// Record `session`'s count fingerprint; return how long the current one
    /// has been standing.
    ///
    /// Counts only: two different graphs with identical counts read as
    /// "unchanged". That is the right trade for a freshness indicator — it is a
    /// hint about writer activity, not a consistency claim.
    pub(super) fn observe(&self, session: &SessionId, fingerprint: u64) -> Duration {
        let Ok(slot) = self.slot(session) else {
            return Duration::ZERO;
        };
        let mut f = slot.freshness.lock();
        if f.fingerprint != fingerprint {
            f.fingerprint = fingerprint;
            f.observed_at = std::time::Instant::now();
        }
        f.observed_at.elapsed()
    }

    /// `session`'s query-embedding cache (#14), one per session.
    pub(super) fn queries(&self, session: &SessionId) -> Option<&Mutex<QueryEmbeddingCache>> {
        self.slots.get(session).map(|slot| &slot.queries)
    }

    /// A recall permit, or `None` when every one stayed taken for
    /// [`RECALL_PERMIT_WAIT`] (the caller answers 503).
    pub(super) async fn recall_permit(&self) -> Option<SemaphorePermit<'_>> {
        tokio::time::timeout(RECALL_PERMIT_WAIT, self.recalls.acquire())
            .await
            .ok()?
            .ok()
    }

    /// Whether `session` has a view held right now (tests).
    #[cfg(test)]
    #[cfg_attr(
        not(all(feature = "store-memory", feature = "embed-fixture")),
        allow(dead_code)
    )]
    pub(super) fn is_loaded(&self, session: &SessionId) -> bool {
        self.slots
            .get(session)
            .is_some_and(|slot| slot.state.lock().ready.is_some())
    }
}
