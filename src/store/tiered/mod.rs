//! `TieredStore`: a durable store with an Elasticsearch recall tier beside it
//! (#18, feature `recall-elastic`).
//!
//! The durable store (`primary`: SQLite, Postgres or Cockroach, or the
//! in-memory store in tests) stays the source of truth and keeps every
//! obligation that needs a transaction. The recall index serves one thing,
//! the vector leg of phase-1 recall, through the #26 seam
//! ([`VectorCandidateSource`]). Everything else is delegated unchanged.
//!
//! # Delegation
//!
//! `init_schema` (plus the index's marker index), `preflight_schema`,
//! `load_session`, `keyword_candidates`, the leases, `blast_radius`,
//! `interaction_span`, `record_canonization` and the flush stats go to the
//! primary. Fencing is therefore exactly the primary's: a stale-token flush
//! fails there and nothing is mirrored.
//!
//! # Mirroring
//!
//! After the primary commits a flush, the batch is projected
//! (`project::project`) and bulk-written to the index at the external version
//! `(fencing_token << 32) | flush_counter`, with the counter kept **per
//! session** and restarted for each new token (#32: fencing tokens are per
//! session, and this store holds no "current session"). A replayed or late
//! write older than what the index holds is refused by the engine as a
//! version conflict, which counts as success.
//!
//! A mirror failure never fails the flush (the primary is already durable).
//! It marks the session **stale**, counts the failure, keeps the last error
//! ([`TieredStore::tier_status`]) and logs it.
//!
//! # When the index is trusted
//!
//! Per session, the tier is `Unknown`, `InSync` or `Stale`. Only `InSync`
//! serves vector candidates from the index; the other two fall back to the
//! primary's own checked read (exact on SQLite, the database's ranking on the
//! pg family), or to no vector leg at all when the primary has none, so
//! recall degrades to its keyword and recent legs and never to wrong answers.
//!
//! `InSync` is established from a **sync marker** the index keeps per
//! session: the durable `mutation_epoch` it reflects. It is written after
//! every clean mirror and compared with the durable snapshot's epoch when the
//! session is loaded. A crash between the primary's commit and the mirror,
//! or a failed mirror before a restart, leaves the marker behind the durable
//! epoch, and the next load sees it.
//!
//! **Repair.** Only a process that holds the session's lease repairs: at
//! load (`Memory` acquires the lease before it loads), after a later flush
//! while the session is stale (at most once per [`REPAIR_BACKOFF`]), and from
//! `lambo recall-index backfill`. A repair re-indexes every stored vector from
//! the durable snapshot at a fresh version, then deletes every session
//! document older than that version (deleted nodes, an older contract's
//! index), then writes the marker. Readers never write to the index; a reader
//! that finds the marker behind serves from the primary.
//!
//! A repair runs as a background task, one at a time per session, off the
//! flush loop and the attach path (#18 review M1): requests while one runs
//! collapse into a single rerun, each pass reads its own durable snapshot
//! and repairs only when the marker is behind it (a marker ahead of a load
//! is re-checked, never repaired from: M4). Bounds: a flush waits at most
//! [`MIRROR_DEADLINE`] for its mirror, a repair pass runs at most
//! [`REPAIR_DEADLINE`], releasing the lease waits at most [`RELEASE_GRACE`]
//! for an in-flight repair, and the index client gives delete-by-query,
//! refresh and count a longer budget than single-document requests.
//! `lambo recall-index backfill` still rebuilds inline: it is the operator's
//! explicit command.
//!
//! # The embedding contract
//!
//! Each contract's vectors live in their own index, `{prefix}-v-{hash}`
//! (`project::contract_hash`). A checked read compares the expected contract
//! with the session's durable contract (cached from the last load or flush,
//! the one thing besides the counters this store remembers per session) and
//! then queries only the expected contract's index. If the durable contract
//! changes between the two, the answer is still in the expected space: stale,
//! never meaningless. That is the accepted divergence from the SQL adapters'
//! single-snapshot check, recorded in `dev-diary/notes/feature-18-elastic-tier.md`.
//!
//! # `exact_vector_scan` stays false (#8)
//!
//! A session holder over a store that declares an exact scan ranks in its own
//! graph and never asks the store. This store leaves the declaration at its
//! default `false`: its checked read is a tier, not unmodified delegation, so
//! a holder over `TieredStore(sqlite)` reaches the index for **recall**. The
//! note records why a holder does not switch to its graph for small sessions
//! either.
//!
//! **Derive is the exception** (#18 review M6, amending #8's "one
//! constructor, two callers"): the store declares `holder_derives_from_graph`,
//! so a holder's hybrid derive takes its semantic-merge candidates from its
//! in-memory graph (`VectorCandidates::for_holder_derive`). The index lags
//! (unflushed concepts, the refresh interval, everything while stale), and
//! derive's dedupe of a fact written seconds ago must not miss it and mint a
//! paraphrased duplicate; it also keeps index latency off the write path.

pub(crate) mod elastic;
pub(crate) mod index;
pub(crate) mod project;

#[cfg(all(test, feature = "store-memory"))]
mod fake;
#[cfg(all(test, feature = "store-memory"))]
mod tests;

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use self::index::{DocOp, KnnHit, RecallIndex, SyncMarker};
use self::project::{index_doc, mirror_version, project, sole_session};
use crate::store::lease::{LeaseHolder, LeaseInfo, LeaseOutcome, LeaseRefusal, LEASE_TTL};
use crate::store::vector_source::{ensure_is_an_embedding, rank_by_cosine, VectorCandidateSource};
use crate::store::{
    validate_vector_candidate_limit, Capabilities, EraseOutcome, GraphStore, RecallBackfillReport,
    SessionFlushStats,
};
use crate::types::{
    tie_break_by_key, CanonizationEvent, Concept, EmbeddingContract, GraphSnapshot,
    InteractionSpan, MutationBatch, NodeId, Scored, SessionId, StoreError,
};

/// The shortest interval between two repair attempts for one stale session
/// on the flush path. A repair reads the whole durable session, so while the
/// index is down it must not run on every flush.
pub(crate) const REPAIR_BACKOFF: Duration = Duration::from_secs(60);

/// Documents per bulk request during a repair.
const REPAIR_CHUNK: usize = 500;

/// The longest a flush waits for its mirror (#18 review M1). Past it the
/// flush returns, the session goes stale, and a background repair catches
/// the index up: the primary already holds the batch.
pub(crate) const MIRROR_DEADLINE: Duration = Duration::from_secs(15);

/// The longest one background repair may run before it is abandoned and the
/// session marked stale (the backoff then spaces the next attempt).
pub(crate) const REPAIR_DEADLINE: Duration = Duration::from_secs(600);

/// The most sessions whose tier state one store keeps (#18 review L5). A
/// long-lived reader (serve-web) touches many sessions; past this, entries
/// for sessions this store neither holds nor is repairing are evicted,
/// least recently used first. An evicted reader entry costs one durable load
/// to re-check on its next read.
pub(crate) const MAX_TRACKED_SESSIONS: usize = 4096;

/// Consecutive failed or timed-out index reads that open the read breaker
/// (#18 review M2).
pub(crate) const BREAKER_THRESHOLD: u32 = 3;

/// How long an open breaker sends every vector read straight to the durable
/// store before one read probes the index again.
pub(crate) const BREAKER_COOLDOWN: Duration = Duration::from_secs(30);

/// The longest one index read may take, connecting included, before it
/// counts as a failure and the read is served from the durable store.
pub(crate) const READ_DEADLINE: Duration = Duration::from_secs(5);

/// How long releasing a lease waits for this store's in-flight repair of the
/// session before abandoning it (the next holder repairs at load).
pub(crate) const RELEASE_GRACE: Duration = Duration::from_secs(10);

/// Extra neighbours fetched beyond `limit`, so the exact re-rank decides the
/// last places rather than the engine's approximate order (M3).
pub(crate) const KNN_OVERFETCH: usize = 16;

/// Delete-by-query passes before a sweep gives up on documents that keep
/// being rewritten under it (version conflicts).
const SWEEP_ATTEMPTS: usize = 3;

/// Sweep-and-count passes an erase makes before it reports failure.
const ERASE_ATTEMPTS: usize = 3;

/// What a sweep deletes.
#[derive(Clone, Copy)]
enum Sweep<'a> {
    /// The session's documents, all or those below a version.
    Session(&'a SessionId, Option<u64>),
    /// Documents by node id, in every data index.
    Ids(&'a [NodeId]),
}

/// Whether the index can be trusted for one session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TierSync {
    /// Not checked yet in this process, or the check could not reach the index.
    #[default]
    Unknown,
    /// The index reflects the durable state as of the last clean mirror.
    InSync,
    /// A mirror failed or the marker is behind: repair before trusting it.
    Stale,
}

/// Per-session tier state. Keyed by session in [`TieredStore`]; nothing here
/// is process-wide.
#[derive(Debug, Default)]
struct SessionTier {
    /// The fencing token `counter` belongs to.
    token: u64,
    /// The last flush counter used under `token`.
    counter: u32,
    sync: TierSync,
    /// The session's durable contract, when known (`Some(None)`: none).
    contract: Option<Option<EmbeddingContract>>,
    /// The token of a lease this store acquired for the session, if held.
    held: Option<u64>,
    mirror_failures: u64,
    last_error: Option<String>,
    next_repair: Option<Instant>,
    /// When a read may next re-check an `Unknown` session (a check reads the
    /// whole durable session, so an unreachable index must not cost one per
    /// recall).
    next_check: Option<Instant>,
    /// A repair of this session is running (single-flight, M4).
    repairing: bool,
    /// Another repair was asked for while one ran: run once more after it.
    repair_again: bool,
    /// The background task running the repair (M1).
    repair_task: Option<tokio::task::JoinHandle<()>>,
    /// Last time this entry was used, for eviction (L5).
    last_used: Option<Instant>,
}

impl SessionTier {
    /// Whether evicting this entry loses nothing that matters: a held
    /// session's flush counter must survive (a restarted counter would
    /// version writes below ones already in the index), and a running
    /// repair owns its entry.
    fn evictable(&self) -> bool {
        self.held.is_none() && !self.repairing
    }
}

/// How the index's sync marker compares with a durable snapshot.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Marker {
    /// The marker is the snapshot's epoch (or nothing was ever indexable).
    InSync,
    /// Behind the snapshot: a mirror was lost. A holder repairs.
    Behind,
    /// Ahead of the snapshot: the load raced a later mirrored flush.
    /// Re-checked, never repaired from.
    Ahead,
    /// The index could not be asked.
    Unreachable,
}

impl Marker {
    fn sync(self) -> TierSync {
        match self {
            Self::InSync => TierSync::InSync,
            Self::Behind => TierSync::Stale,
            Self::Ahead | Self::Unreachable => TierSync::Unknown,
        }
    }
}

/// The read-side circuit breaker (M2). One per tier: every session's reads
/// go to the same cluster, so a cluster that is down or blackholed for one
/// is down for all.
#[derive(Debug, Default)]
struct Breaker {
    /// Index reads that failed or timed out in a row.
    failures: u32,
    /// While set and in the future, reads skip the index. Once it passes,
    /// one read probes the index and pushes it forward again, so readers
    /// arriving during the probe still skip.
    open_until: Option<Instant>,
}

/// The tier's view of one session, for tests. Production reports the same
/// facts through `lambo::recall_tier` warnings.
#[cfg(all(test, feature = "store-memory"))]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TierStatus {
    pub sync: TierSync,
    pub token: u64,
    pub flush_counter: u32,
    pub mirror_failures: u64,
    pub last_error: Option<String>,
}

/// A durable store with a recall index beside it. See the module docs.
///
/// A handle on shared [`Tier`] state, so work the tier runs off the caller's
/// path can hold the state it needs.
pub(crate) struct TieredStore {
    tier: Arc<Tier>,
}

/// The tier's state and policy, shared by the store handle.
pub(crate) struct Tier {
    primary: Box<dyn GraphStore>,
    recall: Box<dyn RecallIndex>,
    /// The process's configured embedder width, reported when the primary
    /// persists no vectors of its own (so `VECTOR_SEARCH` keeps its width).
    vector_dim: Option<usize>,
    sessions: Mutex<HashMap<SessionId, SessionTier>>,
    repair_backoff: Duration,
    mirror_deadline: Duration,
    repair_deadline: Duration,
    release_grace: Duration,
    max_sessions: usize,
    breaker: Mutex<Breaker>,
    breaker_threshold: u32,
    breaker_cooldown: Duration,
    read_deadline: Duration,
}

impl std::ops::Deref for TieredStore {
    type Target = Tier;

    fn deref(&self) -> &Tier {
        &self.tier
    }
}

impl TieredStore {
    pub(crate) fn new(
        primary: Box<dyn GraphStore>,
        recall: Box<dyn RecallIndex>,
        vector_dim: Option<usize>,
    ) -> Self {
        Self {
            tier: Arc::new(Tier {
                primary,
                recall,
                vector_dim,
                sessions: Mutex::new(HashMap::new()),
                repair_backoff: REPAIR_BACKOFF,
                mirror_deadline: MIRROR_DEADLINE,
                repair_deadline: REPAIR_DEADLINE,
                release_grace: RELEASE_GRACE,
                max_sessions: MAX_TRACKED_SESSIONS,
                breaker: Mutex::new(Breaker::default()),
                breaker_threshold: BREAKER_THRESHOLD,
                breaker_cooldown: BREAKER_COOLDOWN,
                read_deadline: READ_DEADLINE,
            }),
        }
    }

    /// Mutate the tier's settings before the store is shared.
    #[cfg(all(test, feature = "store-memory"))]
    fn tier_mut(&mut self) -> &mut Tier {
        Arc::get_mut(&mut self.tier).expect("configured before the store is shared")
    }

    #[cfg(all(test, feature = "store-memory"))]
    pub(crate) fn with_repair_backoff(mut self, backoff: Duration) -> Self {
        self.tier_mut().repair_backoff = backoff;
        self
    }

    #[cfg(all(test, feature = "store-memory"))]
    pub(crate) fn with_deadlines(mut self, mirror: Duration, repair: Duration) -> Self {
        let tier = self.tier_mut();
        tier.mirror_deadline = mirror;
        tier.repair_deadline = repair;
        self
    }

    #[cfg(all(test, feature = "store-memory"))]
    pub(crate) fn with_read_breaker(
        mut self,
        threshold: u32,
        cooldown: Duration,
        read_deadline: Duration,
    ) -> Self {
        let tier = self.tier_mut();
        tier.breaker_threshold = threshold;
        tier.breaker_cooldown = cooldown;
        tier.read_deadline = read_deadline;
        self
    }

    #[cfg(all(test, feature = "store-memory"))]
    pub(crate) fn with_max_sessions(mut self, max: usize) -> Self {
        self.tier_mut().max_sessions = max;
        self
    }

    #[cfg(all(test, feature = "store-memory"))]
    pub(crate) fn with_release_grace(mut self, grace: Duration) -> Self {
        self.tier_mut().release_grace = grace;
        self
    }
}

impl Tier {
    /// The tier's view of `session`.
    #[cfg(all(test, feature = "store-memory"))]
    pub(crate) fn tier_status(&self, session: &SessionId) -> TierStatus {
        let sessions = self.sessions.lock();
        let st = sessions.get(session);
        TierStatus {
            sync: st.map(|s| s.sync).unwrap_or_default(),
            token: st.map_or(0, |s| s.token),
            flush_counter: st.map_or(0, |s| s.counter),
            mirror_failures: st.map_or(0, |s| s.mirror_failures),
            last_error: st.and_then(|s| s.last_error.clone()),
        }
    }

    /// Wait until no repair is running for any session.
    #[cfg(all(test, feature = "store-memory"))]
    pub(crate) async fn repairs_settled(&self) {
        loop {
            let tasks: Vec<_> = self
                .sessions
                .lock()
                .values_mut()
                .filter_map(|st| st.repair_task.take())
                .collect();
            if tasks.is_empty() {
                return;
            }
            for task in tasks {
                let _ = task.await;
            }
        }
    }

    /// Sessions whose tier state this store keeps.
    #[cfg(all(test, feature = "store-memory"))]
    pub(crate) fn tracked_sessions(&self) -> Vec<SessionId> {
        self.sessions.lock().keys().cloned().collect()
    }

    /// Run `f` on the session's state. Never held across an `.await`.
    ///
    /// Creating an entry past [`MAX_TRACKED_SESSIONS`] first evicts the
    /// least recently used evictable entries, down to three quarters of the
    /// bound (so the scan is amortised over many inserts).
    fn with_state<R>(&self, session: &SessionId, f: impl FnOnce(&mut SessionTier) -> R) -> R {
        let mut sessions = self.sessions.lock();
        if !sessions.contains_key(session) && sessions.len() >= self.max_sessions {
            let keep = self.max_sessions - self.max_sessions / 4;
            let mut idle: Vec<(Option<Instant>, SessionId)> = sessions
                .iter()
                .filter(|(_, st)| st.evictable())
                .map(|(sid, st)| (st.last_used, sid.clone()))
                .collect();
            idle.sort_by_key(|(used, _)| *used);
            let excess = (sessions.len() + 1).saturating_sub(keep);
            for (_, sid) in idle.into_iter().take(excess) {
                sessions.remove(&sid);
            }
        }
        let st = sessions.entry(session.clone()).or_default();
        st.last_used = Some(Instant::now());
        f(st)
    }

    /// Forget the session's state: this store no longer holds it and runs
    /// no repair for it (L5). The next read re-checks it.
    fn evict(&self, session: &SessionId) {
        let mut sessions = self.sessions.lock();
        if sessions.get(session).is_some_and(SessionTier::evictable) {
            sessions.remove(session);
        }
    }

    /// The next external version for a write to `session` under `token`.
    /// Only leased writes are mirrored (L4), so there always is a token.
    fn next_version(&self, session: &SessionId, token: u64) -> Result<u64, String> {
        self.with_state(session, |st| {
            if st.token != token {
                st.token = token;
                st.counter = 0;
            }
            let counter = st.counter.checked_add(1).ok_or_else(|| {
                format!(
                    "flush counter for session {session} under token {token} is exhausted; \
                     refusing to wrap the mirror version"
                )
            })?;
            let version = mirror_version(token, counter).ok_or_else(|| {
                format!("fencing token {token} is too large to version a recall-index write")
            })?;
            st.counter = counter;
            Ok(version)
        })
    }

    fn mark_stale(&self, session: &SessionId, error: &str) {
        tracing::warn!(
            target: "lambo::recall_tier",
            session = %session,
            "recall index is stale for this session; vector recall falls back to the durable \
             store until it is repaired: {error}"
        );
        self.with_state(session, |st| {
            st.sync = TierSync::Stale;
            st.mirror_failures += 1;
            st.last_error = Some(error.to_owned());
        });
    }

    /// The session's durable contract: cached, or read once from the primary.
    async fn durable_contract(
        &self,
        session: &SessionId,
    ) -> Result<Option<EmbeddingContract>, StoreError> {
        if let Some(known) = self.with_state(session, |st| st.contract.clone()) {
            return Ok(known);
        }
        let contract = match self.primary.load_session(session).await {
            Ok(snap) => snap.embedding,
            Err(StoreError::SessionNotFound(_)) => None,
            Err(e) => return Err(e),
        };
        self.with_state(session, |st| st.contract = Some(contract.clone()));
        Ok(contract)
    }

    /// Delete `what` from the index so that nothing it matched survives.
    ///
    /// Every pass refreshes first: delete-by-query deletes from the engine's
    /// last-refresh search snapshot, so a document mirrored with
    /// `refresh=false` inside the last refresh interval would otherwise be
    /// missed (and then exposed by the delete's own refresh). A document
    /// rewritten between the snapshot and its delete is a version conflict
    /// the engine skips; the pass is retried, and conflicts that persist
    /// through [`SWEEP_ATTEMPTS`] passes are an error, never a silent
    /// success.
    async fn sweep(&self, what: Sweep<'_>) -> Result<(), StoreError> {
        let mut conflicts = 0;
        for _ in 0..SWEEP_ATTEMPTS {
            self.recall.refresh().await?;
            let report = match what {
                Sweep::Session(session, below) => {
                    self.recall.delete_session_docs(session, below).await?
                }
                Sweep::Ids(ids) => self.recall.delete_ids(ids).await?,
            };
            conflicts = report.version_conflicts;
            if conflicts == 0 {
                return Ok(());
            }
        }
        Err(StoreError::Backend(format!(
            "recall index: {conflicts} documents were still being rewritten after \
             {SWEEP_ATTEMPTS} delete passes (version conflicts)"
        )))
    }

    /// Compare the index's marker with the durable epoch of a snapshot.
    async fn check_marker(&self, session: &SessionId, snap: Option<&GraphSnapshot>) -> Marker {
        let epoch = snap.map_or(0, |s| s.mutation_epoch);
        let indexable = snap.is_some_and(|s| {
            s.embedding
                .as_ref()
                .is_some_and(|c| s.concepts.iter().any(|k| index_doc(k, c, None).is_some()))
        });
        match self.recall.read_marker(session).await {
            Ok(Some(m)) if m.synced_epoch == epoch => Marker::InSync,
            Ok(Some(m)) if m.synced_epoch > epoch => Marker::Ahead,
            // Nothing was ever mirrored and there is nothing to mirror.
            Ok(None) if !indexable => Marker::InSync,
            Ok(_) => Marker::Behind,
            Err(e) => {
                tracing::warn!(
                    target: "lambo::recall_tier",
                    session = %session,
                    "recall index unreachable while checking its sync marker: {e}"
                );
                Marker::Unreachable
            }
        }
    }

    /// The durable snapshot, `None` for a session the store does not have.
    async fn load_durable(&self, session: &SessionId) -> Result<Option<GraphSnapshot>, StoreError> {
        match self.primary.load_session(session).await {
            Ok(snap) => {
                self.with_state(session, |st| st.contract = Some(snap.embedding.clone()));
                Ok(Some(snap))
            }
            Err(StoreError::SessionNotFound(_)) => {
                self.with_state(session, |st| st.contract = Some(None));
                Ok(None)
            }
            Err(e) => Err(e),
        }
    }

    /// A marker ahead of the snapshot this process loaded is not staleness:
    /// the load raced a flush that committed and mirrored a later epoch.
    /// Re-check against a fresh load; never repair from the older snapshot
    /// (#18 review M4). Still ahead after the re-load: leave the session
    /// `Unknown` (reads fall back) and re-check after the backoff.
    async fn recheck_ahead(&self, session: &SessionId) -> Marker {
        let fresh = match self.load_durable(session).await {
            Ok(snap) => snap,
            Err(_) => return Marker::Unreachable,
        };
        let marker = self.check_marker(session, fresh.as_ref()).await;
        if marker == Marker::Ahead {
            tracing::warn!(
                target: "lambo::recall_tier",
                session = %session,
                "recall index marker is ahead of the durable session after a re-load; \
                 not repairing from it, reads fall back until it is re-checked"
            );
        }
        marker
    }

    /// Settle the session's state after a durable load: cache the contract,
    /// check the marker, and repair if this process holds the lease and the
    /// marker is behind.
    async fn settle_after_load(
        self: &Arc<Self>,
        session: &SessionId,
        snap: Option<&GraphSnapshot>,
    ) {
        let held = self.with_state(session, |st| {
            st.contract = Some(snap.and_then(|s| s.embedding.clone()));
            st.held
        });
        let mut marker = self.check_marker(session, snap).await;
        if marker == Marker::Ahead {
            marker = self.recheck_ahead(session).await;
        }
        match (marker, held) {
            (Marker::Behind, Some(_)) => self.request_repair(session),
            (marker, _) => self.with_state(session, |st| {
                // A repair in flight owns the state until it finishes.
                if !st.repairing {
                    st.sync = marker.sync();
                    if marker == Marker::Ahead {
                        st.next_check = Some(Instant::now() + self.repair_backoff);
                    }
                }
            }),
        }
    }

    /// Ask for a repair of the session from a durable snapshot read by the
    /// repair itself, never one a caller loaded earlier (M4), and only when
    /// the marker is behind it.
    ///
    /// The repair runs as a background task (#18 review M1), off the flush
    /// loop and the attach path: the marker makes deferring it safe, since
    /// reads fall back until it lands. One runs at a time per session; a
    /// request while one runs makes it run once more afterwards, which picks
    /// up whatever committed meanwhile. It uses the lease token held when it
    /// starts each pass and stops once the lease is gone. Each pass is
    /// bounded by the repair deadline; a failure marks the session stale and
    /// starts the backoff, and drops any queued rerun (the backoff covers it).
    fn request_repair(self: &Arc<Self>, session: &SessionId) {
        let start = self.with_state(session, |st| {
            if st.repairing {
                st.repair_again = true;
                false
            } else {
                st.repairing = true;
                st.repair_again = false;
                true
            }
        });
        if !start {
            return;
        }
        let tier = Arc::clone(self);
        let sid = session.clone();
        let task = tokio::spawn(async move { tier.run_repairs(&sid).await });
        self.with_state(session, |st| st.repair_task = Some(task));
    }

    async fn run_repairs(&self, session: &SessionId) {
        // Stops once the lease is gone.
        while let Some(token) = self.with_state(session, |st| st.held) {
            let result =
                match tokio::time::timeout(self.repair_deadline, self.repair_once(session, token))
                    .await
                {
                    Ok(result) => result,
                    Err(_) => Err(StoreError::Backend(format!(
                        "repair did not finish within its {}s deadline",
                        self.repair_deadline.as_secs_f64()
                    ))),
                };
            if let Err(e) = result {
                self.mark_stale(session, &format!("repair failed: {e}"));
                self.with_state(session, |st| {
                    st.next_repair = Some(Instant::now() + self.repair_backoff);
                    st.repair_again = false;
                });
                break;
            }
            let again = self.with_state(session, |st| std::mem::take(&mut st.repair_again));
            if !again {
                break;
            }
        }
        self.with_state(session, |st| st.repairing = false);
    }

    async fn repair_once(&self, session: &SessionId, token: u64) -> Result<(), StoreError> {
        let snap = self.load_durable(session).await?;
        let mut marker = self.check_marker(session, snap.as_ref()).await;
        if marker == Marker::Ahead {
            marker = self.recheck_ahead(session).await;
        }
        match marker {
            Marker::Behind => {}
            Marker::Unreachable => {
                return Err(StoreError::Backend(
                    "recall index unreachable while checking its sync marker".into(),
                ));
            }
            Marker::InSync | Marker::Ahead => {
                self.with_state(session, |st| {
                    st.sync = marker.sync();
                    st.next_repair = None;
                    if marker == Marker::Ahead {
                        st.next_check = Some(Instant::now() + self.repair_backoff);
                    }
                });
                return Ok(());
            }
        }
        let (contract, concepts, epoch) = match &snap {
            Some(s) => (
                s.embedding.as_ref(),
                s.concepts.as_slice(),
                s.mutation_epoch,
            ),
            None => (None, &[][..], 0),
        };
        self.reconcile(session, contract, concepts, epoch, token)
            .await
            .map(|_| ())
    }

    /// Rebuild the session's index documents from durable state, at a fresh
    /// version under `token`. See "Repair" in the module docs.
    async fn reconcile(
        &self,
        session: &SessionId,
        contract: Option<&EmbeddingContract>,
        concepts: &[Concept],
        epoch: u64,
        token: u64,
    ) -> Result<RecallBackfillReport, StoreError> {
        let version = Some(
            self.next_version(session, token)
                .map_err(StoreError::Backend)?,
        );
        // Reads fall back while documents are being replaced.
        self.with_state(session, |st| {
            if st.sync == TierSync::InSync {
                st.sync = TierSync::Unknown;
            }
        });
        let mut indexed = 0u64;
        let mut index = None;
        if let Some(contract) = contract {
            self.recall.ensure_index(contract).await?;
            index = Some(self.recall.index_name(contract));
            let ops: Vec<DocOp> = concepts
                .iter()
                .filter(|c| &c.session_id == session)
                .filter_map(|c| {
                    index_doc(c, contract, version).map(|doc| DocOp::Index {
                        contract: contract.clone(),
                        id: c.id,
                        version,
                        doc,
                    })
                })
                .collect();
            for chunk in ops.chunks(REPAIR_CHUNK) {
                self.recall.bulk(chunk).await?;
            }
            indexed = ops.len() as u64;
        }
        self.sweep(Sweep::Session(session, version)).await?;
        self.recall
            .write_marker(
                session,
                SyncMarker {
                    synced_epoch: epoch,
                },
                version,
            )
            .await?;
        self.with_state(session, |st| {
            st.sync = TierSync::InSync;
            st.next_repair = None;
        });
        Ok(RecallBackfillReport {
            session: session.clone(),
            indexed,
            index,
            mutation_epoch: epoch,
        })
    }

    /// Repair from the durable store, unless a recent attempt failed.
    fn repair_if_due(self: &Arc<Self>, session: &SessionId) {
        let due = self.with_state(session, |st| {
            st.next_repair.is_none_or(|at| Instant::now() >= at)
        });
        if due {
            self.request_repair(session);
        }
    }

    /// Mirror a committed batch. Never fails the flush.
    async fn mirror(self: &Arc<Self>, batch: &MutationBatch, token: Option<u64>) {
        match sole_session(&batch.mutations) {
            Ok(Some(session)) => self.mirror_session(&session, batch, token).await,
            Ok(None) => self.mirror_unnamed(batch, token).await,
            Err(_) => {
                // The graph drains one session per batch. A hand-built batch
                // over several cannot be attributed (its deletes name no
                // session), so it is not mirrored: each session it names goes
                // stale and is repaired rather than trusted.
                for sid in crate::store::batch::batch_session_ids(&batch.mutations) {
                    let session = SessionId::new(sid);
                    self.mark_stale(
                        &session,
                        "a batch spanning several sessions cannot be attributed; repairing",
                    );
                }
            }
        }
    }

    /// A batch that names no session: a delete-only batch (a GC sweep), or
    /// one that touches nothing the index holds.
    async fn mirror_unnamed(self: &Arc<Self>, batch: &MutationBatch, token: Option<u64>) {
        let deleted: Vec<NodeId> = crate::store::batch::batch_deleted_ids(&batch.mutations).0;
        if deleted.is_empty() {
            return;
        }
        // Attribute through the lease this store holds under the same token.
        let owner = token.and_then(|t| {
            let sessions = self.sessions.lock();
            let mut owners = sessions
                .iter()
                .filter(|(_, st)| st.held == Some(t))
                .map(|(s, _)| s.clone());
            match (owners.next(), owners.next()) {
                (Some(one), None) => Some(one),
                _ => None,
            }
        });
        if let Some(session) = owner {
            self.mirror_session(&session, batch, token).await;
            return;
        }
        // Unattributable: delete by id everywhere (always safe, the nodes are
        // durably gone) and leave every marker where it is, so the owning
        // session's next load sees its marker behind and repairs.
        let swept = tokio::time::timeout(self.mirror_deadline, self.sweep(Sweep::Ids(&deleted)))
            .await
            .unwrap_or_else(|_| Err(self.past_deadline()));
        if let Err(e) = swept {
            let sessions: Vec<SessionId> = self.sessions.lock().keys().cloned().collect();
            for session in sessions {
                self.mark_stale(&session, &format!("unattributed delete failed: {e}"));
            }
        }
    }

    async fn mirror_session(
        self: &Arc<Self>,
        session: &SessionId,
        batch: &MutationBatch,
        token: Option<u64>,
    ) {
        let Some(token) = token else {
            self.mark_unleased(session);
            return;
        };
        let sync = self.with_state(session, |st| st.sync);
        if sync != TierSync::InSync {
            // The durable snapshot already holds this batch, so a repair
            // covers it; mirroring it on its own first would be redundant.
            self.repair_if_due(session);
            return;
        }
        let version = match self.next_version(session, token) {
            Ok(v) => Some(v),
            Err(e) => {
                self.mark_stale(session, &e);
                return;
            }
        };
        self.mirror_ops(session, batch, version).await;
    }

    /// An unleased write (the seed and fixture path) is not mirrored (#18
    /// review L4). The engine would take it with internal versioning, which
    /// bumps an externally versioned document to `V + 1`, exactly the next
    /// leased write's `(T << 32) | (c + 1)`; that write's conflict counts as
    /// success and a stale document would be served while in sync. So the
    /// index only ever sees leased, externally versioned writes (plus
    /// delete-by-query, whose tombstones are never rewritten): the session
    /// goes stale, reads fall back, and the next holder load or
    /// `lambo recall-index backfill` repairs it. Not counted as a mirror
    /// failure.
    fn mark_unleased(&self, session: &SessionId) {
        tracing::debug!(
            target: "lambo::recall_tier",
            session = %session,
            "unleased write not mirrored; the recall index catches up at the next holder \
             load or recall-index backfill"
        );
        self.with_state(session, |st| {
            st.sync = TierSync::Stale;
            // The batch may have switched the contract; read it again.
            st.contract = None;
            st.last_error = Some(
                "unleased write not mirrored: repaired at the next holder load or \
                 recall-index backfill"
                    .into(),
            );
        });
    }

    /// Project and write at `version`; advance the marker only when
    /// everything landed.
    async fn mirror_ops(&self, session: &SessionId, batch: &MutationBatch, version: Option<u64>) {
        let before = match self.durable_contract(session).await {
            Ok(c) => c,
            Err(e) => {
                self.mark_stale(session, &format!("durable contract unavailable: {e}"));
                return;
            }
        };
        let projection = project(session, &batch.mutations, before, version);
        self.with_state(session, |st| {
            st.contract = Some(projection.contract_after.clone());
        });
        let written = async {
            for contract in &projection.contracts {
                self.recall.ensure_index(contract).await?;
            }
            if !projection.ops.is_empty() {
                self.recall.bulk(&projection.ops).await?;
            }
            self.recall
                .write_marker(
                    session,
                    SyncMarker {
                        synced_epoch: batch.mutation_epoch,
                    },
                    version,
                )
                .await?;
            Ok::<(), StoreError>(())
        };
        let written = tokio::time::timeout(self.mirror_deadline, written)
            .await
            .unwrap_or_else(|_| Err(self.past_deadline()));
        if let Err(e) = written {
            self.mark_stale(session, &format!("mirror failed: {e}"));
        }
    }

    fn past_deadline(&self) -> StoreError {
        StoreError::Backend(format!(
            "recall index mirror did not finish within its {}s deadline; the batch is \
             durable and a background repair catches the index up",
            self.mirror_deadline.as_secs_f64()
        ))
    }

    /// Whether a read may ask the index now. An open breaker answers no
    /// until its cool-down passes; then this read is the probe, and the
    /// cool-down restarts so concurrent readers keep skipping until the
    /// probe's outcome closes or re-opens it.
    fn breaker_admits(&self) -> bool {
        let mut b = self.breaker.lock();
        match b.open_until {
            Some(until) if Instant::now() < until => false,
            Some(_) => {
                b.open_until = Some(Instant::now() + self.breaker_cooldown);
                true
            }
            None => true,
        }
    }

    fn breaker_success(&self) {
        let mut b = self.breaker.lock();
        if b.open_until.is_some() {
            tracing::info!(
                target: "lambo::recall_tier",
                "recall index answered again; vector reads use it again"
            );
        }
        *b = Breaker::default();
    }

    fn breaker_failure(&self) {
        let mut b = self.breaker.lock();
        b.failures = b.failures.saturating_add(1);
        if b.failures >= self.breaker_threshold {
            if b.open_until.is_none() {
                tracing::warn!(
                    target: "lambo::recall_tier",
                    failures = b.failures,
                    "recall index reads keep failing; serving vector reads from the durable \
                     store for {}s before probing it again",
                    self.breaker_cooldown.as_secs_f64()
                );
            }
            b.open_until = Some(Instant::now() + self.breaker_cooldown);
        }
    }

    /// The primary's own checked read, or no vector leg when it has none.
    async fn fallback(
        &self,
        session: &SessionId,
        probe: &[f32],
        expected: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        if self
            .primary
            .capabilities()
            .contains(Capabilities::VECTOR_SEARCH)
        {
            self.primary
                .vector_candidates_checked(session, probe, expected, limit)
                .await
        } else {
            tracing::debug!(
                target: "lambo::recall_tier",
                session = %session,
                "recall index not in sync and the durable store has no vector search; \
                 vector leg returns nothing (keyword and recent legs still run)"
            );
            Ok(Vec::new())
        }
    }

    /// Establish the session's state for a read that arrives before any load
    /// through this store (a reader that never loaded the session).
    ///
    /// A check that cannot settle it (the index or the primary unreachable)
    /// is not repeated by the next read: the session stays `Unknown`, reads
    /// fall back, and the next check waits out the repair backoff.
    async fn sync_for_read(self: &Arc<Self>, session: &SessionId) -> TierSync {
        let (sync, due) = self.with_state(session, |st| {
            (st.sync, st.next_check.is_none_or(|at| Instant::now() >= at))
        });
        if sync != TierSync::Unknown || !due {
            return sync;
        }
        match self.primary.load_session(session).await {
            Ok(snap) => self.settle_after_load(session, Some(&snap)).await,
            Err(StoreError::SessionNotFound(_)) => self.settle_after_load(session, None).await,
            Err(_) => {}
        }
        self.with_state(session, |st| {
            if st.sync == TierSync::Unknown {
                st.next_check = Some(Instant::now() + self.repair_backoff);
            }
            st.sync
        })
    }
}

#[async_trait]
impl VectorCandidateSource for TieredStore {
    /// The index's answer when the session is in sync, the primary's
    /// otherwise. Same contract as every other source: limit checks, the
    /// probe check, an empty answer for an unknown session or one with no
    /// contract, an `Invariant` refusal for a contract mismatch.
    async fn checked_vector_candidates(
        &self,
        session: &SessionId,
        probe: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        validate_vector_candidate_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        ensure_is_an_embedding(probe)?;
        if self.tier.sync_for_read(session).await != TierSync::InSync {
            return self
                .fallback(session, probe, expected_contract, limit)
                .await;
        }
        let durable = self.with_state(session, |st| st.contract.clone().flatten());
        let Some(durable) = durable else {
            return Ok(Vec::new());
        };
        durable
            .ensure_compatible(expected_contract)
            .map_err(|err| {
                StoreError::Invariant(format!(
                    "vector candidate lookup refused after embedding contract changed: {err}"
                ))
            })?;
        if probe.len() != durable.dim {
            return Err(StoreError::Invariant(format!(
                "query embedding has {} dimensions but session {} stores vectors of {}",
                probe.len(),
                session.0,
                durable.dim
            )));
        }
        // Only the expected contract's index is queried: whatever happened to
        // the durable contract since the check, these vectors are in the
        // caller's space.
        if !self.breaker_admits() {
            return self
                .fallback(session, probe, expected_contract, limit)
                .await;
        }
        let fetch = limit.saturating_add(KNN_OVERFETCH);
        let read = tokio::time::timeout(
            self.read_deadline,
            self.recall.knn(expected_contract, session, probe, fetch),
        )
        .await
        .unwrap_or_else(|_| {
            Err(StoreError::Backend(format!(
                "recall index read did not answer within {}s",
                self.read_deadline.as_secs_f64()
            )))
        });
        let hits = match read {
            Ok(hits) => {
                self.breaker_success();
                hits
            }
            Err(e) => {
                self.breaker_failure();
                tracing::warn!(
                    target: "lambo::recall_tier",
                    session = %session,
                    "recall index query failed, serving this read from the durable store: {e}"
                );
                return self
                    .fallback(session, probe, expected_contract, limit)
                    .await;
            }
        };
        Ok(rerank(probe, &hits, limit))
    }
}

/// Rank the index's hits exactly (#18 review M3).
///
/// The engine only chooses the pool: its scores may come from quantized
/// vectors and are `f32` whatever the mapping, so each hit is re-scored from
/// its stored vector with [`rank_by_cosine`], the scorer every exact source
/// uses (same cosine, same issue-2 tie-break). A hit whose vector did not
/// come back, or came back at another width, keeps the engine's score.
fn rerank(probe: &[f32], hits: &[KnnHit], limit: usize) -> Vec<Scored<NodeId>> {
    let exact = |h: &KnnHit| {
        h.embedding
            .as_deref()
            .filter(|e| e.len() == probe.len())
            .is_some()
    };
    if hits.iter().all(exact) {
        return rank_by_cosine(
            probe,
            hits.iter().map(|h| {
                (
                    h.id,
                    h.embedding.as_deref().unwrap_or_default(),
                    h.canonical_key.as_str(),
                )
            }),
            limit,
        );
    }
    tracing::debug!(
        target: "lambo::recall_tier",
        "recall index returned hits without their stored vectors; ranking those by the \
         engine's score"
    );
    let mut ranked: Vec<(Scored<NodeId>, &str)> = hits
        .iter()
        .map(|h| {
            let score = match h.embedding.as_deref().filter(|e| e.len() == probe.len()) {
                Some(e) => f64::from(crate::embed::cosine(probe, e)),
                None => h.cosine,
            };
            (Scored::new(h.id, score), h.canonical_key.as_str())
        })
        .collect();
    ranked.sort_by(|(a, a_key), (b, b_key)| {
        b.score
            .total_cmp(&a.score)
            .then_with(|| tie_break_by_key(Some(a_key), &a.item, Some(b_key), &b.item))
    });
    ranked.truncate(limit);
    ranked.into_iter().map(|(s, _)| s).collect()
}

#[async_trait]
impl GraphStore for TieredStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.primary.init_schema().await?;
        self.recall.provision().await
    }

    /// The tier adds vector search whatever the primary has: when the index
    /// cannot answer, the primary's own read (or an empty vector leg) does.
    fn capabilities(&self) -> Capabilities {
        self.primary.capabilities() | Capabilities::VECTOR_SEARCH
    }

    async fn preflight_schema(&self) -> Result<(), StoreError> {
        // The index is not a durable obligation: an unreachable index must
        // not refuse an attach, it only sends vector reads to the primary.
        self.primary.preflight_schema().await
    }

    fn vector_dimensions(&self) -> Option<usize> {
        self.primary.vector_dimensions().or(self.vector_dim)
    }

    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        // Fencing is the primary's: on error nothing is mirrored.
        self.primary.flush(batch, token).await?;
        self.tier.mirror(batch, token).await;
        Ok(())
    }

    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        match self.primary.load_session(session).await {
            Ok(snap) => {
                self.tier.settle_after_load(session, Some(&snap)).await;
                Ok(snap)
            }
            Err(StoreError::SessionNotFound(s)) => {
                self.tier.settle_after_load(session, None).await;
                Err(StoreError::SessionNotFound(s))
            }
            Err(e) => Err(e),
        }
    }

    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.primary
            .keyword_candidates(session, tokens, limit)
            .await
    }

    /// The frozen unchecked surface stays the primary's: it cannot bind a
    /// contract, so it gets no index to choose between.
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        if self
            .primary
            .capabilities()
            .contains(Capabilities::VECTOR_SEARCH)
        {
            self.primary
                .vector_candidates(session, embedding, limit)
                .await
        } else {
            Err(StoreError::Capability(
                "the unchecked vector_candidates surface is the durable store's, which has no \
                 vector search; use vector_candidates_checked"
                    .into(),
            ))
        }
    }

    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.checked_vector_candidates(session, embedding, expected_contract, limit)
            .await
    }

    // `exact_vector_scan` deliberately keeps its default `false` (#8): see
    // the module docs.

    /// A holder's hybrid derive ranks in its graph, not in this lagging
    /// tier (#18 amending #8): see the module docs.
    fn holder_derives_from_graph(&self) -> bool {
        true
    }

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.primary
            .blast_radius(session, node, min_edge_age, now)
            .await
    }

    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<InteractionSpan, StoreError> {
        self.primary
            .interaction_span(session, node, min_age, now)
            .await
    }

    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.primary.record_canonization(event, token).await
    }

    /// The durable erase first; only once it committed, every document and
    /// the marker for the session leave the index. An index failure after a
    /// committed durable erase is an error, so a deletion fan-out never marks
    /// the session done while its vectors are still searchable. Rerunning is
    /// safe: the durable erase reports `already_absent` and the index cleanup
    /// is retried.
    ///
    /// "Gone" is checked, not assumed (#18 review H1): each pass sweeps the
    /// session (refresh, delete-by-query, retry on conflicts), refreshes, and
    /// counts what a search still finds. Only a count of zero lets the marker
    /// go and the erase report done.
    async fn erase_session(
        &self,
        session: &SessionId,
        eraser: &LeaseHolder,
    ) -> Result<EraseOutcome, StoreError> {
        let outcome = self.primary.erase_session(session, eraser).await?;
        if let EraseOutcome::Erased(_) = &outcome {
            let cleaned = async {
                let mut left = 0;
                for _ in 0..ERASE_ATTEMPTS {
                    self.sweep(Sweep::Session(session, None)).await?;
                    self.recall.refresh().await?;
                    left = self.recall.count_session_docs(session).await?;
                    if left == 0 {
                        return self.recall.delete_marker(session).await;
                    }
                }
                Err(StoreError::Backend(format!(
                    "recall index: {left} documents of the session are still searchable after \
                     {ERASE_ATTEMPTS} delete passes"
                )))
            }
            .await;
            if let Err(e) = cleaned {
                return Err(StoreError::Backend(format!(
                    "session {session} was erased from the durable store, but removing its \
                     documents from the recall index failed: {e}. Run erase-session again: the \
                     durable erase is idempotent and the recall index cleanup is retried"
                )));
            }
            self.sessions.lock().remove(session);
        }
        Ok(outcome)
    }

    async fn backfill_recall_index(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<Option<RecallBackfillReport>, StoreError> {
        let token = match self.acquire_lease(session, holder, LEASE_TTL).await? {
            LeaseOutcome::Acquired(info) => info.token,
            LeaseOutcome::Held { current, age } => {
                return Err(StoreError::Backend(format!(
                    "session {session} is held by a live writer ({}, holding the lease for \
                     {}s); nothing was rebuilt. A holder repairs its own recall index when it \
                     loads the session and after a failed mirror; stop it to rebuild from here",
                    current.holder,
                    age.as_secs()
                )));
            }
        };
        let result = match self.primary.load_session(session).await {
            Ok(snap) => {
                self.with_state(session, |st| st.contract = Some(snap.embedding.clone()));
                self.reconcile(
                    session,
                    snap.embedding.as_ref(),
                    &snap.concepts,
                    snap.mutation_epoch,
                    token,
                )
                .await
            }
            Err(StoreError::SessionNotFound(_)) => {
                self.reconcile(session, None, &[], 0, token).await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = self.release_lease(session, holder).await {
            tracing::warn!(
                target: "lambo::recall_tier",
                session = %session,
                "backfill could not release its lease (it expires on its own): {e}"
            );
        }
        result.map(Some)
    }

    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        let outcome = self.primary.acquire_lease(session, holder, ttl).await?;
        self.note_lease(session, &outcome);
        Ok(outcome)
    }

    async fn read_lease(&self, session: &SessionId) -> Result<Option<LeaseInfo>, StoreError> {
        self.primary.read_lease(session).await
    }

    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        let outcome = self.primary.refresh_lease(session, holder, ttl).await?;
        self.note_lease(session, &outcome);
        Ok(outcome)
    }

    /// A repair this store has in flight for the session is given
    /// [`RELEASE_GRACE`] to finish (a short-lived holder would otherwise
    /// release before its attach-time repair lands) and abandoned after it;
    /// the next holder repairs at load.
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        let task = self.with_state(session, |st| st.repair_task.take());
        if let Some(mut task) = task
            && tokio::time::timeout(self.release_grace, &mut task)
                .await
                .is_err()
        {
            task.abort();
            self.with_state(session, |st| {
                st.repairing = false;
                st.repair_again = false;
            });
        }
        self.primary.release_lease(session, holder).await?;
        self.with_state(session, |st| st.held = None);
        self.evict(session);
        Ok(())
    }

    async fn record_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        self.primary
            .record_lease_refusal(session, refused_by, current_holder)
            .await
    }

    async fn pending_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<LeaseRefusal>, StoreError> {
        self.primary.pending_lease_refusals(session, since).await
    }

    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.primary.write_flush_stats(session, stats).await
    }

    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        self.primary.read_flush_stats(session).await
    }
}

impl Tier {
    /// Remember whether this store holds the session's lease, and under which
    /// token: only a holder repairs the index.
    fn note_lease(&self, session: &SessionId, outcome: &LeaseOutcome) {
        let held = match outcome {
            LeaseOutcome::Acquired(info) => Some(info.token),
            LeaseOutcome::Held { .. } => None,
        };
        self.with_state(session, |st| st.held = held);
    }
}
