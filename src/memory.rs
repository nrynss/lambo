//! `Memory` — the spec §6.1 library surface (T8.1).
//!
//! This is the assembly point: one [`Memory`] owns a session's in-RAM
//! [`Graph`], its [`InvertedIndex`], the resolved store + embedder, and the
//! **three** background tasks the session needs —
//!
//! | Task | Built by | What breaks without it |
//! |---|---|---|
//! | [`Daemon`] | [`Daemon::from_config`] | no scoring, no hot list, no conflict/drift/stale events |
//! | [`FlushTask`] | [`FlushTask::new`] | nothing is ever durable |
//! | [`CanonizationTask`] | [`CanonizationTask::from_daemon`] | **no node ever transitions** — the spec §13 demo is impossible |
//!
//! ## Lock discipline (spec §6.4, non-negotiable)
//!
//! The graph lock is **never** held across an `.await`. Every method here
//! takes the lock, works, releases, and only then does I/O. Where a method
//! needs both the graph and the index, it takes them in the order the daemon's
//! GC uses — **graph → index** (`daemon::run_loop`; taking them the other way
//! around would deadlock against a concurrent GC sync).
//!
//! ## The writers gate (COH-6 clause 14)
//!
//! `close()` stops the three background producers before it drains the log —
//! but the surface's **own** writers run on caller tasks it does not own, and
//! `derive` / `retract` cross `.await` points. Without a barrier a write that
//! passed `ensure_open` before the latch could append to the graph log *after*
//! the final drain: acknowledged to its caller, durable nowhere, and (for a
//! retraction) resurrected on the next attach.
//!
//! So every mutating method holds a **read permit** on `Memory::writers` for
//! its whole body, awaits included, and re-checks `closed` after acquiring it;
//! [`Memory::close`] latches `closed` and then takes the **write** side before
//! it stops anything. The two orders are the only two outcomes: an in-flight
//! write finishes and lands in the final batch, or a late write is refused with
//! the closed error. Nothing is acknowledged and lost.
//!
//! Read-only methods (`recall`, `stats`, `canonical_memories`, `events`) do
//! **not** take the gate — a long recall must not delay shutdown, and they are
//! refused after close by `ensure_open` as before.
//!
//! ## Inverted-index mirroring (the contract at `src/graph/mod.rs`)
//!
//! The graph is index-free by design and **the session owner MUST mirror every
//! concept write into the index**. `Memory` is that owner. Every write path
//! here — [`Memory::derive`], [`Memory::record_action`], [`Memory::demote`] —
//! calls `Memory::mirror_concepts` on the ids it created, and
//! [`Memory::retract`] calls `index.remove`. GC-driven removals are mirrored by
//! the daemon itself because [`MemoryBuilder::build`] hands it the index via
//! [`Daemon::with_index`].
//!
//! A forgotten mirror is **silent** staleness — recall returns stale keyword
//! candidates and nothing crashes. The contract is pinned by
//! `tests/p2_integration.rs::inverted_index_manual_sync_contract`.
//!
//! ## Interactions are server-stamped
//!
//! Every write opens a fresh [`Interaction`] whose `created_at` is taken here,
//! from the process clock — never from a caller. `derive` / `record_action` /
//! `demote` all take their logical timestamp from the interaction node, so a
//! caller-supplied timestamp would propagate to every concept and edge below it
//! and backdating by 61s would neuter the whole `canonization_edge_min_age`
//! inflation guard (P6 review F18). There is deliberately no API to pass one.
//!
//! **The clock behind that stamp is a crate-private seam** —
//! `MemoryBuilder::clock`, `pub(crate)` — and that is not a hole in the rule
//! above. The rule is about *callers*: no library method, no CLI flag and no
//! MCP tool argument accepts a timestamp, and every one of them still gets the
//! process clock. Swapping the clock is a decision the process makes about
//! itself at construction, once, for every write it will ever make; it cannot
//! be reached across the MCP boundary, cannot be set per call, and cannot
//! backdate one interaction relative to its neighbours. `lambo demo` is the
//! only user: it installs a monotone script clock so the OUTCOME block is
//! reproducible run to run (see `crate::cli::demo::script_clock`).

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, LazyLock};
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex as PlMutex, RwLock};
use tokio::sync::{broadcast, RwLock as AsyncRwLock, RwLockReadGuard as AsyncRwLockReadGuard};
use tokio::task::JoinHandle;

use crate::canon::CanonizationTask;
use crate::config::{Config, ScoringWeights};
use crate::daemon::{Clock, Daemon, RecallPipeline};
use crate::embed::Embedder;
use crate::graph::action::{record_action as graph_record_action, Action, ActionOutcome};
use crate::graph::canonical::{canonicalize, CanonicalizeResult};
use crate::graph::demote::demote as graph_demote;
use crate::graph::derive::{derive as graph_derive, DeriveOutcome, ParentOf};
use crate::graph::index::InvertedIndex;
use crate::graph::reserve::{release as graph_release, reserve as graph_reserve};
use crate::graph::{hybrid, Graph};
use crate::recall::cache::RecallCache;
use crate::recall::format;
use crate::resolve::{
    embedding_mismatch_error, session_embedding_compatibility, ResolvedBackends,
    SessionEmbeddingCompatibility,
};
use crate::store::flush::{
    panic_message, CatchUnwindPoll, FlushParams, FlushTask, FLUSH_ATTEMPT_TIMEOUT,
};
use crate::store::lease::{LeaseHolder, LeaseOutcome, LEASE_HEARTBEAT_INTERVAL, LEASE_TTL};
use crate::store::load::load_session_async;
use crate::store::{Capabilities, GraphStore};
use crate::types::{
    AgentId, CanonizationStatus, Concept, ConceptType, DaemonEvent, EmbeddingContract, Interaction,
    LamboError, MatchStrategy, MutationBatch, Node, NodeId, RecallQuery, RecallResult, Reservation,
    SessionId, StoreError,
};

// ---------------------------------------------------------------------------
// Value types
// ---------------------------------------------------------------------------

/// Bound on [`Memory::retract`]'s durable blast-radius query (R2-5).
///
/// It was the one store call on a user-facing path with no bound at all, and
/// the writers gate turned that into `close()`'s problem: `retract` holds a
/// read permit across this await, so `close()`'s step 0 waited on it — an
/// unresponsive backend made shutdown unbounded, defeating the point of the
/// `FLUSH_ATTEMPT_TIMEOUT` bound on step 4.
///
/// **The same 30s as [`hybrid::HYBRID_IO_TIMEOUT`]**, and defined from it so
/// there is one number: that constant bounds exactly this shape — the store I/O
/// of a `&self` write method that holds the gate — and `retract` earning its own
/// value would only invite the two to drift. Named for its own site because the
/// hybrid *derive* path is not the caller.
const RETRACT_IO_TIMEOUT: Duration = hybrid::HYBRID_IO_TIMEOUT;

/// Whether [`Memory::retract`] is allowed to mutate (spec §6.1, §13).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DryRun {
    /// Report the impact and change **nothing** — the spec §13 blast-radius
    /// story. Not a "mostly read-only" mode: no node, no edge, no index entry
    /// and no mutation-log record is touched.
    Yes,
    /// Report the impact **and** remove the node (plus its incident edges) from
    /// the graph and the inverted index.
    No,
}

impl DryRun {
    /// `true` for [`DryRun::Yes`].
    pub fn is_dry(self) -> bool {
        matches!(self, DryRun::Yes)
    }
}

/// What retracting a concept costs — the spec §13 blast-radius report.
///
/// ## Two radii, on purpose
///
/// [`Self::blast_radius`] is computed from the **in-RAM graph**, which spec
/// §2.1 makes the primary tier and which is what recall's `⚑ N nodes` warning
/// renders. [`Self::durable_blast_radius`] is `GraphStore::blast_radius` —
/// the same question asked of the durable store, which lags the graph by up to
/// one `backend_flush_interval` and answers `None` here when it cannot be
/// reached (or when the session has never been flushed).
///
/// They agree once the session is flushed. When they disagree the in-RAM one
/// is the truthful answer to "what breaks if I remove this **now**", so it is
/// the headline; the durable one is reported beside it rather than silently
/// reconciled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ImpactReport {
    /// The resolved concept.
    pub target: NodeId,
    /// Its content, as stored.
    pub content: String,
    /// Its canonization status — retracting a `Canonical` node is the loud case.
    pub canonization_status: CanonizationStatus,
    /// Concepts that would be orphaned, from the in-RAM graph (headline).
    pub blast_radius: u64,
    /// The same count from `GraphStore::blast_radius`; `None` when the store
    /// could not answer (see [`Self::warnings`]).
    pub durable_blast_radius: Option<u64>,
    /// Edges that would be deleted along with the node.
    pub incident_edges: usize,
    /// `true` when nothing was mutated.
    pub dry_run: bool,
    /// `true` when the node was actually removed from graph + index.
    pub removed: bool,
    /// Degradation notes (e.g. the store could not be reached). Never fatal.
    pub warnings: Vec<String>,
}

/// One canonical ("saint") memory — [`Memory::canonical_memories`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalMemory {
    /// The concept's node id.
    pub node_id: NodeId,
    /// Its text.
    pub content: String,
    /// How it is classified.
    pub concept_type: ConceptType,
    /// In-RAM blast radius (dependents), same source as recall's `⚑` warning.
    pub blast_radius: u64,
    /// When it was first written.
    pub created_at: DateTime<Utc>,
    /// How many times recall has returned it.
    pub access_count: i32,
}

/// Session health — spec §2.4 requires the durability loss bound to be
/// *observable*, so `flush_lag` and `log_depth` are the load-bearing fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MemoryStats {
    /// The session these figures are for.
    pub session: SessionId,
    /// The agent this writer runs as.
    pub agent: AgentId,
    /// Time since the last successful flush.
    pub flush_lag: Duration,
    /// Mutations sitting in the graph's write-behind log, awaiting drain.
    /// Read from the graph, so it is **always current**.
    pub log_depth: usize,
    /// The flush task's own not-yet-durable count: its pending batch plus the
    /// log length **as of its last poll** (it refreshes once per cycle, so
    /// between cycles this lags the log by up to `POLL_QUANTUM`).
    ///
    /// Neither field alone is the whole loss window, because the flush task's
    /// `pending` buffer is task-owned and has no accessor:
    /// `log_depth.max(flush_depth)` is the honest lower bound, and it is exact
    /// except while a retained batch and fresh writes coexist. Exposing
    /// `FlushTask::pending_len()` would make it exact — deliberately not done
    /// here, since T8.1's authorization over `src/store/flush.rs` covers only
    /// the stop channel.
    pub flush_depth: usize,
    /// Batches dropped as dead letters (deterministic constraint violations).
    pub dead_lettered: u64,
    /// `true` once the session degraded to `durability="none"` (spec §2.3).
    pub degraded: bool,
    /// Nodes in the session, interactions and concepts together.
    pub node_count: usize,
    /// Edges in the session.
    pub edge_count: usize,
    /// Concepts in the session.
    pub concept_count: usize,
    /// Concepts that have reached canonical status.
    pub canonical_count: usize,
    /// `MutationEpoch` — recall-cache key (spec §8).
    pub epoch: u64,
    /// Background daemon cycles completed since the session opened.
    pub daemon_cycles: u64,
    /// Canonization evaluation cycles completed.
    pub canonization_cycles: u64,
    /// Canonization cycles that failed. A non-zero count is worth investigating.
    pub canonization_failures: u64,
}

// ---------------------------------------------------------------------------
// Second-writer detection (T81-8)
// ---------------------------------------------------------------------------

/// Live [`Memory`] handles per session, process-wide.
///
/// Two `Memory`s on one session each spawn a full task trio over **divergent
/// in-RAM copies** and flush both copies into the same rows: the later flush
/// wins, the other handle's writes are overwritten, one side's GC deletes nodes
/// the other still holds — and neither side looks wrong from the inside. Cheap
/// to detect, so it is detected.
///
/// **Reported, not refused.** Spec §2.2 assigns single-writer enforcement to
/// deployment, and a process-global refusal would be both too strong and too
/// weak: too strong because `build()` would gain a new failure mode for
/// legitimate re-attaches (a leaked handle whose owner dropped the reference,
/// a tool that opens a read-mostly second view), and far too weak because the
/// collisions that actually corrupt a session come from *other processes and
/// hosts*, which no in-process registry can see. Inventing a policy here would
/// buy a false sense of protection; an ERROR line naming both agents buys T8.2
/// the diagnostic it will actually want.
static ACTIVE_SESSIONS: LazyLock<PlMutex<HashMap<SessionId, Vec<AgentId>>>> =
    LazyLock::new(|| PlMutex::new(HashMap::new()));

/// Record a handle; log loudly if the session already had one.
fn register_session(session: &SessionId, agent: &AgentId) {
    let mut active = ACTIVE_SESSIONS.lock();
    let agents = active.entry(session.clone()).or_default();
    if !agents.is_empty() {
        tracing::error!(
            session = %session,
            agent = %agent,
            existing = ?agents,
            handles = agents.len() + 1,
            "SecondSessionWriter: this process already holds a Memory handle for session \
             {session} (agents {agents:?}) and is opening another for {agent}. Spec §2.2 is one \
             writer per session: the two handles keep divergent in-RAM graphs and flush them into \
             the same rows, so the later flush silently overwrites the other's writes. Close one."
        );
    }
    agents.push(agent.clone());
}

/// Release a handle's registration.
///
/// Reached through [`Memory::unregister_once`], from whichever comes first: a
/// **successful** [`Memory::close`], or [`Drop`] (so a handle that is never
/// closed still releases when it goes).
///
/// **A successful close releases; a failed one does not** (R2-4). Close then
/// re-attach in the same process is the MCP server's ordinary shape — and this
/// crate's own reload test — so holding the slot until `Drop` fired a
/// `SecondSessionWriter` ERROR against a handle that had already made its tail
/// durable and stopped every task: a false alarm on the one path that is
/// certainly safe, which is how ops-level detectors get ignored. A **failed**
/// close is the opposite case and keeps its slot: that handle still holds an
/// undurable tail in its in-RAM graph and is documented as retryable, so a
/// second writer arriving on the session is still the divergence the detector
/// is for.
fn unregister_session(session: &SessionId, agent: &AgentId) {
    let mut active = ACTIVE_SESSIONS.lock();
    if let Some(agents) = active.get_mut(session) {
        if let Some(position) = agents.iter().position(|a| a == agent) {
            agents.remove(position);
        }
        if agents.is_empty() {
            active.remove(session);
        }
    }
}

/// Spawn the single-writer lease heartbeat (T8.6).
///
/// Refreshes the lease every [`LEASE_HEARTBEAT_INTERVAL`] (a third of the TTL),
/// so a live holder keeps the session and a crashed one's lease lapses within
/// one full TTL. The first tick is consumed immediately, so the first *refresh*
/// lands one interval after acquisition, not at once.
///
/// A refresh that comes back [`LeaseOutcome::Held`] means this handle **lost**
/// the session — its lease expired (a store outage starved the heartbeat past
/// the TTL) and another writer took over. On that transition the heartbeat
/// latches the shared `fence` (T86-2): the owning [`Memory`] then refuses every
/// further write and its write-behind flush loop stops and drops its pending
/// tail, so the two writers can no longer flush divergent graphs into one
/// session. The loss is still logged loudly and the fenced handle does not
/// self-destruct — the operator reconciles — but it is now a loud, *safe* stop
/// rather than silent split-brain corruption. Setting the fence is idempotent,
/// so the heartbeat may keep beating after it without changing anything.
fn spawn_lease_heartbeat(
    store: Arc<dyn GraphStore>,
    session: SessionId,
    holder: LeaseHolder,
    fence: Arc<AtomicBool>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(LEASE_HEARTBEAT_INTERVAL);
        // Skip the immediate first tick: refresh after one interval, not now.
        interval.tick().await;
        loop {
            interval.tick().await;
            match store.refresh_lease(&session, &holder, LEASE_TTL).await {
                Ok(LeaseOutcome::Acquired(_)) => {}
                Ok(LeaseOutcome::Held { current, .. }) => {
                    // T86-2: fence the writer. From here every write is refused
                    // and the flush loop stops overwriting the new holder's rows.
                    fence.store(true, Ordering::Release);
                    tracing::error!(
                        session = %session,
                        holder = %holder,
                        new_holder = %current.holder,
                        "single-writer lease LOST: this handle's lease expired (heartbeat starved \
                         past the TTL) and {} took the session. This handle is now FENCED — further \
                         writes are refused and its tail will NOT be flushed; an operator must \
                         reconcile.",
                        current.holder
                    );
                }
                Err(err) => {
                    // A transient store blip: log and keep beating. The lease has
                    // TTL slack for two missed refreshes before it can lapse.
                    tracing::warn!(
                        session = %session,
                        holder = %holder,
                        error = %err,
                        "single-writer lease heartbeat refresh failed; will retry next interval"
                    );
                }
            }
        }
    })
}

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// Builder for [`Memory`] — spec §6.1.
///
/// The named setters below (`match_strategy`, `flush_interval`,
/// `scoring_weights`) override the corresponding [`Config`] field and are
/// applied at `build()` time, so they commute with [`MemoryBuilder::config`].
/// Everything else the session needs already has a `Config` knob, so pass a
/// whole [`Config`] rather than looking for more setters. **No new knobs are
/// introduced by this type.**
#[derive(Default)]
pub struct MemoryBuilder {
    session: Option<SessionId>,
    agent: Option<AgentId>,
    store: Option<Arc<dyn GraphStore>>,
    embedder: Option<Arc<dyn Embedder>>,
    embedding: Option<EmbeddingContract>,
    allow_embedding_mismatch: bool,
    config: Config,
    // Held as overrides rather than written straight into `config`, so
    // `.config(..)` and the named setters commute — calling them in either
    // order gives the same session. They are applied in `build`.
    match_strategy: Option<MatchStrategy>,
    flush_interval: Option<Duration>,
    scoring_weights: Option<ScoringWeights>,
    // Crate-private, and not a knob: see `MemoryBuilder::clock`.
    clock: Option<Clock>,
}

impl MemoryBuilder {
    /// Session this process owns (spec §2.2 — one writer per session).
    pub fn session(mut self, session: impl Into<String>) -> Self {
        self.session = Some(SessionId::new(session));
        self
    }

    /// Agent id stamped on every interaction and concept this handle writes.
    pub fn agent(mut self, agent: impl Into<String>) -> Self {
        self.agent = Some(AgentId::new(agent));
        self
    }

    /// Durable store. Accepts the `Box<dyn GraphStore>` that
    /// [`ResolvedBackends`] carries, or an `Arc` you already share.
    pub fn store(mut self, store: impl Into<Arc<dyn GraphStore>>) -> Self {
        self.store = Some(store.into());
        self
    }

    /// Embedder. Accepts [`ResolvedBackends::embedder`] directly.
    pub fn embedder(mut self, embedder: impl Into<Arc<dyn Embedder>>) -> Self {
        self.embedder = Some(embedder.into());
        self
    }

    /// The live embedding space (stamped on a fresh session; checked against a
    /// loaded one — see [`MemoryBuilder::build`]).
    pub fn embedding_contract(mut self, contract: EmbeddingContract) -> Self {
        self.embedding = Some(contract);
        self
    }

    /// Explicitly allow a same-width stored/live embedding-contract mismatch.
    ///
    /// This is a dangerous migration escape hatch, not a compatibility mode.
    /// With vectors present, it only permits a same-kind model-identifier
    /// rename and the caller must know those identifiers denote the same
    /// vector space. Cross-kind migration requires the old vectors to have
    /// been atomically cleared/re-embedded first. Different dimensions remain
    /// a hard error.
    pub fn allow_embedding_mismatch(mut self, allow: bool) -> Self {
        self.allow_embedding_mismatch = allow;
        self
    }

    /// Level B: take store + embedder + contract from **one**
    /// `resolve_backends` / `resolve_from_config_path` call.
    ///
    /// This is the single-construction-site path (spec §3.4): prefer it over
    /// setting the three pieces separately, and never rebuild the store or
    /// embedder with a second config pass.
    ///
    /// **Config is deliberately NOT applied here.** This method forwards only
    /// the store/embedder/embedding — `backends.config` is consumed and
    /// dropped. A writer built from a resolved backend MUST also pass
    /// `.config(backends.config.clone())` (before or after — the two fields
    /// commute), as `open_writer` and `serve::build_memory` do; otherwise the
    /// `[daemon]` cadence overrides the resolver applied are silently lost and
    /// the session behaves as `Config::default()`.
    pub fn backends(mut self, backends: ResolvedBackends) -> Self {
        self.store = Some(Arc::from(backends.store));
        self.embedder = Some(Arc::from(backends.embedder));
        self.embedding = Some(backends.embedding);
        self.allow_embedding_mismatch = backends.allow_embedding_mismatch;
        self
    }

    /// `Canonical` (keyword-only) or `Hybrid` (keyword + vector merge).
    /// Dispatched on by [`Memory::derive`]. Overrides `Config::match_strategy`.
    pub fn match_strategy(mut self, strategy: MatchStrategy) -> Self {
        self.match_strategy = Some(strategy);
        self
    }

    /// `backend_flush_interval` (spec §2.4). Overrides the config value.
    pub fn flush_interval(mut self, interval: Duration) -> Self {
        self.flush_interval = Some(interval);
        self
    }

    /// Daemon scoring weights (spec §9; default 0.25 / 0.20 / 0.20 / 0.35).
    /// Overrides `Config::scoring`.
    pub fn scoring_weights(mut self, weights: ScoringWeights) -> Self {
        self.scoring_weights = Some(weights);
        self
    }

    /// The process clock every interaction this handle opens is stamped from.
    /// Defaults to [`Utc::now`].
    ///
    /// **`pub(crate)` on purpose, and it stays that way.** The invariant this
    /// crate enforces is that no *caller* supplies a timestamp (F18: the MCP
    /// params are `deny_unknown_fields`, `lambo_derive` refuses a `timestamp`
    /// argument by name, and no CLI verb takes one). That invariant is about
    /// the trust boundary, not about the identity of the function that reads
    /// the clock: a process may decide, once, at construction, what "now"
    /// means for every write it will make. It may not let a caller decide it
    /// per write, which is why this is a builder setter and not a `derive`
    /// argument, and why it is not reachable from outside the crate at all.
    ///
    /// The one caller is `lambo demo`, which needs the session's temporal
    /// extent to be a property of its script rather than of the scheduler —
    /// see [`crate::cli::demo::script_clock`].
    pub(crate) fn clock(mut self, clock: Clock) -> Self {
        self.clock = Some(clock);
        self
    }

    /// Base [`Config`] for every knob the named setters do not cover.
    ///
    /// Order-independent: `match_strategy` / `flush_interval` /
    /// `scoring_weights` are applied on top of this at `build()` time whether
    /// they were set before or after this call.
    pub fn config(mut self, config: Config) -> Self {
        self.config = config;
        self
    }

    /// Load the session and start all three background tasks.
    ///
    /// 1. `load_session` — a missing session is a first use, not an error.
    /// 2. **Level B contract check**: if the loaded session carries an
    ///    [`EmbeddingContract`], [`session_embedding_compatibility`]
    ///    refuses a kind / model / dim mismatch (the model-mixing refusal —
    ///    STORE-1). A fresh session is stamped with the live contract instead.
    /// 3. Spawn the daemon, the flush task and the **canonization task**.
    ///
    /// The daemon's first cycle *is* the spec §2.5 warm-up rescore, so
    /// [`Memory::events`]'s first receiver is subscribed **before** `spawn`
    /// (CONC-3: `broadcast` delivers only what is sent after subscription, and
    /// a resumed session publishes its whole restored condition set on that
    /// first cycle).
    pub async fn build(self) -> Result<Memory, LamboError> {
        let session = self.session.ok_or_else(|| {
            LamboError::Config("Memory::builder: .session(..) is required".into())
        })?;
        let agent = self
            .agent
            .ok_or_else(|| LamboError::Config("Memory::builder: .agent(..) is required".into()))?;
        let store = self.store.ok_or_else(|| {
            LamboError::Config(
                "Memory::builder: .store(..) or .backends(..) is required (Level B: resolve once)"
                    .into(),
            )
        })?;
        let embedder = self.embedder.ok_or_else(|| {
            LamboError::Config(
                "Memory::builder: .embedder(..) or .backends(..) is required (Level B: resolve once)"
                    .into(),
            )
        })?;
        let embedding = self.embedding.ok_or_else(|| {
            LamboError::Config(
                "Memory::builder: .embedding_contract(..) or .backends(..) is required — a session \
                 without a stamped embedding space cannot refuse a model swap"
                    .into(),
            )
        })?;
        // Named setters win over the base config, whatever order they came in.
        let mut config = self.config;
        if let Some(strategy) = self.match_strategy {
            config.match_strategy = strategy;
        }
        if let Some(interval) = self.flush_interval {
            config.backend_flush_interval = interval;
        }
        if let Some(weights) = self.scoring_weights {
            config.scoring = weights;
        }
        let config = config;
        // Fail closed before any side effect: a zero cadence would panic
        // tokio::interval inside the spawned tasks, so validate the merged
        // config (named setters included) before the lease is even acquired.
        config.validate()?;
        let clock = self.clock.unwrap_or_else(|| Arc::new(Utc::now));

        // (0) Single-writer lease (spec §2.2, T8.6) — the store-enforced gate,
        // taken BEFORE the startup load (T86-1). Claiming the session first means
        // a losing racer gets the honest, named refusal below rather than an
        // opaque `database is locked`: the load path opens a `BEGIN IMMEDIATE`
        // write transaction, so two simultaneous `serve` startups used to contend
        // on the SQLite write lock during the load — before either reached the
        // lease — and the loser died on the lock, never producing the designed
        // "held by <holder>, run this override" message. Nothing in the load
        // depends on the lease and nothing in the acquire depends on the load, so
        // the order is free to fix.
        //
        // **Acquiring the lease says NOTHING about durability.** If the previous
        // holder crashed, its lease expired but its write-behind tail died with
        // it (in-RAM log, no WAL) — this is exactly why the `load_session_async`
        // below runs unconditionally and replays whatever WAS made durable. The
        // lease is a concurrency gate, not a completeness guarantee; the startup
        // load is what makes the new holder correct.
        //
        // Every ordinary startup error after acquisition explicitly releases
        // this holder-scoped lease below. A process crash can still only be
        // recovered by TTL, but a clean refusal must never look like a crash to
        // the next invocation.
        let lease_holder = LeaseHolder::for_this_process(&agent);
        let lease_token = match store
            .acquire_lease(&session, &lease_holder, LEASE_TTL)
            .await
            .map_err(LamboError::Store)?
        {
            // Capture the monotonic fencing token (GitHub issue #1) the store
            // minted for this holder. A refresh PRESERVES it, so the value is
            // stable for the handle's life; every durable write (flush + canon)
            // presents it and the store rejects a stale one after a takeover.
            LeaseOutcome::Acquired(info) => info.token,
            LeaseOutcome::Held { current, age } => {
                // Fail closed, naming the current holder and its age.
                return Err(LamboError::Conflict(format!(
                    "session {session} is already held by another writer ({}) — it acquired the \
                     single-writer lease {}s ago and is still refreshing it. Refusing to open a \
                     second writer. If that holder is wedged, an operator can force a takeover \
                     (see the single-writer lease note in docs/reference/cli.mdx)",
                    current.holder,
                    age.as_secs(),
                )));
            }
        };

        // (1) Startup load (spec §2.5). The async core, not the sync wrapper:
        // `store::load::load_session` parks a worker thread and joins it, which
        // would block a runtime worker from inside this async fn. The lease is
        // already ours (step 0), so this load is the winner replaying durable
        // state — never a loser contending on the store's write lock.
        let startup = async {
            let loaded = load_session_async(store.as_ref(), &session).await?;
            let existing = !loaded.graph.is_empty();
            let mut graph = loaded.graph;

            // (2) Level B / STORE-1 — the model-mixing refusal's second half.
            // `None` on a fresh session is not a mismatch — it is an unstamped
            // space, so stamp it.
            match session_embedding_compatibility(graph.embedding(), &embedding) {
                SessionEmbeddingCompatibility::Unrecorded => {
                    graph.stamp_embedding(embedding.clone())?;
                }
                SessionEmbeddingCompatibility::Compatible => {}
                SessionEmbeddingCompatibility::Mismatch { stored, live } => {
                    if !self.allow_embedding_mismatch || stored.dim != live.dim {
                        return Err(embedding_mismatch_error(&stored, &live));
                    }
                    tracing::warn!(
                        session = %session,
                        stored_kind = %stored.kind,
                        stored_model = ?stored.model,
                        live_kind = %live.kind,
                        live_model = ?live.model,
                        dim = live.dim,
                        "operator allowed an embedding contract mismatch; relabeling the session's \
                         existing vectors with the configured live contract"
                    );
                    graph.replace_embedding_with_operator_override(live)?;
                    // E2E-1: the override relabel must be durable BEFORE the
                    // first write — the checked candidate read compares the
                    // *durable* contract against the expected one, so a
                    // write-behind relabel (flush at interval / close) would
                    // refuse the very first hybrid write on a vector-capable
                    // store (live-reproduced on Cockroach: the documented
                    // `--allow-embedding-mismatch` workflow failed its first
                    // run and only succeeded on the second). Flush the queued
                    // `SetEmbedding` synchronously here, armored exactly like
                    // the close-time final flush; it stays an ordered durable
                    // mutation (later writes append after it in the log). A
                    // failed relabel flush refuses the attach: the writer
                    // would otherwise hit the same E2E-1 refusal on its first
                    // write, and the startup-error path below releases the
                    // freshly acquired lease.
                    let relabel = graph.drain_log();
                    if !relabel.mutations.is_empty() {
                        final_flush(store.as_ref(), &relabel, Some(lease_token))
                            .await
                            .map_err(|e| {
                                LamboError::Store(StoreError::Backend(format!(
                                    "session {session}: the operator override relabel could not \
                                     be made durable before the first write: {e}"
                                )))
                            })?;
                    }
                }
            }
            Ok::<_, LamboError>((existing, graph, loaded.index))
        }
        .await;
        let (existing, graph, index) = match startup {
            Ok(startup) => startup,
            Err(startup_error) => {
                if let Err(release_error) = store.release_lease(&session, &lease_holder).await {
                    tracing::warn!(
                        session = %session,
                        holder = %lease_holder,
                        error = %release_error,
                        "could not release writer lease after startup refusal; it will lapse at TTL"
                    );
                }
                return Err(startup_error);
            }
        };

        let graph = Arc::new(RwLock::new(graph));
        let index = Arc::new(RwLock::new(index));

        // (3) Daemon first — the canonization task borrows its score table and
        // event sender, so it must exist before `CanonizationTask::from_daemon`.
        let daemon = Daemon::from_config(graph.clone(), &config).with_index(index.clone());
        // CONC-3: subscribe BEFORE spawn or the warm-up condition set is lost.
        let startup_events = daemon.events();

        // Single-writer-lease fence (T86-2): shared by the flush loop and the
        // lease heartbeat. Latched `true` the instant the heartbeat detects the
        // lease was lost; from then the flush loop stops (and drops its tail)
        // and the write gate refuses. Created before both consumers.
        let lease_lost = Arc::new(AtomicBool::new(false));

        let flush = FlushTask::new(
            graph.clone(),
            store.clone(),
            FlushParams {
                interval: config.backend_flush_interval,
                max_batch: config.backend_flush_max_batch,
                retries: config.backend_flush_retries,
                log_max: config.backend_log_max,
            },
        )
        .with_fence(lease_lost.clone())
        .with_token(lease_token);
        let canon = CanonizationTask::from_daemon(graph.clone(), store.clone(), &daemon, &config)
            .with_token(lease_token);

        // Each `spawn` panics if called twice — each is called exactly once,
        // here, and nowhere else in this type.
        let daemon_handle = daemon.spawn();
        let flush_handle = flush.spawn();
        let canon_handle = canon.spawn();
        // Heartbeat: refresh the lease at a fraction of its TTL so a live holder
        // keeps the session and a crashed one's lease lapses (T8.6).
        let heartbeat_handle = spawn_lease_heartbeat(
            store.clone(),
            session.clone(),
            lease_holder.clone(),
            lease_lost.clone(),
        );

        tracing::info!(
            session = %session,
            agent = %agent,
            existing,
            match_strategy = ?config.match_strategy,
            embedder = %embedding.kind,
            dim = embedding.dim,
            "Memory session attached (daemon + flush + canonization running)"
        );
        // Last, so nothing registers for a `build()` that failed. Released by
        // a successful `close()` (R2-4) or, failing that, by `Drop` (T81-8).
        register_session(&session, &agent);

        Ok(Memory {
            session,
            agent,
            config,
            graph,
            index,
            store,
            embedder,
            embedding,
            daemon,
            flush,
            canon,
            daemon_handle: PlMutex::new(Some(daemon_handle)),
            flush_handle: PlMutex::new(Some(flush_handle)),
            canon_handle: PlMutex::new(Some(canon_handle)),
            heartbeat_handle: PlMutex::new(Some(heartbeat_handle)),
            startup_events: PlMutex::new(Some(startup_events)),
            recall_cache: tokio::sync::Mutex::new(RecallCache::new()),
            writers: AsyncRwLock::new(()),
            close_state: tokio::sync::Mutex::new(false),
            closed: AtomicBool::new(false),
            registered: AtomicBool::new(true),
            lease_holder,
            lease_token,
            lease_released: AtomicBool::new(false),
            lease_lost,
            clock,
        })
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// An attached session: graph + index + store + embedder + three tasks.
///
/// One process owns one session (spec §2.2). Every method takes `&self`, so a
/// `Memory` behind an `Arc` serves concurrent MCP tool calls; each call carries
/// its own `agent_id` through the writes it makes.
///
/// # Example (spec §6.1)
///
/// ```
/// # #[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
/// # async fn spec_6_1() -> Result<(), lambo::LamboError> {
/// use std::sync::Arc;
/// use std::time::Duration;
///
/// use lambo::embed::FixtureEmbedder;
/// use lambo::graph::action::Action;
/// use lambo::graph::derive::ParentOf;
/// use lambo::memory::{DryRun, Memory};
/// use lambo::{
///     ConceptType, Embedder, EmbeddingContract, GraphStore, MatchStrategy, MemoryStore,
///     RecallQuery, ScoringWeights,
/// };
///
/// // Level B: resolve once, then hand into Memory. (`resolve_from_config_path(None)?`
/// // is the production path; the doc-test builds the same three pieces by hand so it
/// // needs no lambo.toml and no network.)
/// let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
/// let embedder: Arc<dyn Embedder> = Arc::new(FixtureEmbedder::new());
/// let contract = EmbeddingContract { kind: "fixture".into(), model: None, dim: 1024 };
///
/// let mem = Memory::builder()
///     .session("project-doom")
///     .agent("agent-A")
///     .store(store.clone())
///     .embedder(embedder)
///     .embedding_contract(contract)
///     .match_strategy(MatchStrategy::Canonical)
///     .flush_interval(Duration::from_millis(20))
///     .scoring_weights(ScoringWeights::default())   // 0.25 / 0.20 / 0.20 / 0.35
///     .build().await?;
///
/// mem.set_root_goal(&["doom-style FPS", "3D renderer"])?;
/// mem.declare_synonym("register_user", "create_user")?;
///
/// // `derive` is async (hybrid matching is async; one shape for both strategies).
/// mem.derive(&[
///     ("user schema", ConceptType::Entity),
///     ("must stay backward compatible", ConceptType::Constraint),
/// ], &ParentOf::none()).await?;
///
/// mem.record_action(&Action {
///     action: "created migrations/003.sql",
///     produces: &["migrations/003.sql"],
///     depends_on: &["user schema"],
///     modifies: &[],
/// })?;
///
/// mem.demote("The caching layer was the bottleneck.", "chunk-1")?;
///
/// let result = mem.recall(RecallQuery {
///     query: "update user schema".into(),
///     top_k: 5,
///     max_tokens: 500,
///     traversal_depth: 2,
/// }).await?;
/// let _ = result.context;
///
/// let impact = mem.retract("user schema", DryRun::Yes).await?;
/// assert!(impact.dry_run && !impact.removed);
///
/// let node = impact.target;
/// let _reservation = mem.reserve(node, Duration::from_secs(30))?;
///
/// let _saints = mem.canonical_memories();
/// let stats = mem.stats();
/// assert!(stats.node_count > 0);
///
/// // close() drains and flushes the tail: everything written above is durable
/// // afterwards, even though the flush interval may never have elapsed.
/// mem.close().await?;
/// let snap = store.load_session(&lambo::SessionId::new("project-doom")).await.unwrap();
/// assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
/// # Ok(())
/// # }
/// # #[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
/// # tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap()
/// #     .block_on(spec_6_1()).unwrap();
/// ```
pub struct Memory {
    session: SessionId,
    agent: AgentId,
    config: Config,
    graph: Arc<RwLock<Graph>>,
    index: Arc<RwLock<InvertedIndex>>,
    store: Arc<dyn GraphStore>,
    embedder: Arc<dyn Embedder>,
    embedding: EmbeddingContract,
    daemon: Daemon,
    flush: FlushTask,
    canon: CanonizationTask,
    /// `Option` + `Mutex` so [`Memory::close`] can take ownership through
    /// `&self` and [`Drop`] can abort whatever `close` did not.
    daemon_handle: PlMutex<Option<JoinHandle<()>>>,
    flush_handle: PlMutex<Option<JoinHandle<()>>>,
    canon_handle: PlMutex<Option<JoinHandle<()>>>,
    /// The single-writer lease heartbeat (T8.6). Aborted by `close()` before the
    /// lease is released, and by `Drop` so a leaked handle stops squatting the
    /// lease (its row then lapses at TTL rather than being kept alive forever).
    /// Unlike the three producers it never touches the graph or the tail, so a
    /// bare `abort()` is enough — no `HandleCustody` reap is needed.
    heartbeat_handle: PlMutex<Option<JoinHandle<()>>>,
    /// The receiver subscribed before `Daemon::spawn`; handed to the first
    /// [`Memory::events`] caller so the warm-up condition set is not lost.
    startup_events: PlMutex<Option<broadcast::Receiver<DaemonEvent>>>,
    /// Session-scoped recall cache (spec §8's key carries no session id, so the
    /// owner holds one per session). A `tokio` mutex, not `parking_lot`:
    /// `Daemon::recall` needs `&mut` across its `.await`s. This is not the
    /// graph lock — holding it across an await is fine and only serializes
    /// concurrent recalls on this handle.
    recall_cache: tokio::sync::Mutex<RecallCache<RecallPipeline>>,
    /// **Writers gate** (T81-1, COH-6 clause 14). Mutating methods hold the
    /// READ side for their whole body — `.await`s included — and re-check
    /// `closed` once they have it; [`Memory::close`] takes the WRITE side
    /// before stopping the tasks, so it cannot drain past an in-flight write.
    ///
    /// A `tokio` RwLock, deliberately not `parking_lot`: `derive` and `retract`
    /// hold this across `.await` (that is the entire point), which a
    /// `parking_lot` guard may never do. It is **not** the graph lock and the
    /// §6.4 rule is untouched — the graph lock is still taken, used and
    /// released inside these methods without ever crossing an await.
    writers: AsyncRwLock<()>,
    /// Serializes `close()` bodies and holds its one-shot success flag: `true`
    /// once a close has actually made the tail durable.
    ///
    /// A second **concurrent** caller parks here until the first finishes
    /// rather than returning an early `Ok` over an in-flight final flush
    /// (T81-6), and a **failed** close leaves it `false` so the tail — pushed
    /// back to the front of the log — can be retried (T81-5).
    close_state: tokio::sync::Mutex<bool>,
    closed: AtomicBool,
    /// `true` while this handle holds a slot in [`ACTIVE_SESSIONS`]. A
    /// **successful** `close()` releases it (R2-4) and clears this, so [`Drop`]
    /// does not release a second time and take a *different* handle's slot with
    /// it — the registry keys on session + agent id, which a re-attach reuses.
    registered: AtomicBool,
    /// This handle's single-writer lease identity (agent + pid + host), reused
    /// by the heartbeat and by release (T8.6).
    lease_holder: LeaseHolder,
    /// Monotonic fencing token (GitHub issue #1) this handle presents on every
    /// durable write (via the FlushTask and the canon task). Minted by the
    /// store at takeover; a refresh PRESERVES it, so it is stable for the
    /// handle's life. The store rejects a stale/missing one after a takeover —
    /// the hard store-side fence, independent of the cooperative `lease_lost`.
    lease_token: u64,
    /// `true` once this handle has released its lease. A **successful** `close()`
    /// releases (a graceful close hands off rather than waiting out the TTL); a
    /// failed close keeps the lease for a retry and lets it lapse at TTL if none
    /// comes. `Drop` never releases — a handle dropped without a clean close is
    /// the crash-shaped path, where expiry is the correct release mechanism (and
    /// `Drop` cannot `await` the store anyway); it only aborts the heartbeat so
    /// the lease is actually free to lapse.
    lease_released: AtomicBool,
    /// **Single-writer-lease fence** (T86-2). Latched `true` by the heartbeat
    /// the instant it observes the lease was LOST — this handle's lease expired
    /// (a store outage starved the beat past the TTL) and another writer took
    /// the session. Once set, the write gate ([`Memory::begin_write`] /
    /// [`Memory::begin_write_sync`]) refuses every mutation, the write-behind
    /// flush loop stops and drops its tail (`FlushTask::with_fence`), and
    /// [`Memory::close`] refuses to flush or release. It turns the split-brain
    /// where two writers flush divergent graphs into one session into a loud,
    /// safe stop. Shared (`Arc`) with the heartbeat and the flush task.
    lease_lost: Arc<AtomicBool>,
    /// Where [`Memory::begin_interaction`] takes its stamp. [`Utc::now`] unless
    /// the *process* replaced it at construction ([`MemoryBuilder::clock`],
    /// crate-private) — never something a caller can reach or vary per write.
    clock: Clock,
}

impl Memory {
    /// Start building a session.
    pub fn builder() -> MemoryBuilder {
        MemoryBuilder::default()
    }

    /// The session this handle owns.
    pub fn session(&self) -> &SessionId {
        &self.session
    }

    /// The agent id stamped on this handle's writes.
    pub fn agent(&self) -> &AgentId {
        &self.agent
    }

    /// The resolved config (read-only; every knob was fixed at build time).
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// The session's live embedding space.
    pub fn embedding_contract(&self) -> &EmbeddingContract {
        &self.embedding
    }

    /// The shared graph, for readers that need more than the methods here
    /// (`lambo inspect`, the MCP inspect tool). **Read-only in spirit**: a
    /// caller that mutates through this handle bypasses the index mirroring
    /// contract and will silently stale recall.
    pub fn graph(&self) -> &Arc<RwLock<Graph>> {
        &self.graph
    }

    /// The session's inverted index (same caveat as [`Memory::graph`]).
    pub fn index(&self) -> &Arc<RwLock<InvertedIndex>> {
        &self.index
    }

    /// The resolved store (single construction site — do not build another).
    pub fn store(&self) -> &Arc<dyn GraphStore> {
        &self.store
    }

    // -----------------------------------------------------------------------
    // Session metadata
    // -----------------------------------------------------------------------

    /// Declare the session's root goal (spec §9 drift anchor). Concepts the
    /// goal names are promoted to `Venerable` through the audited transition
    /// path, so the promotion is durable.
    pub fn set_root_goal(&self, goals: &[&str]) -> Result<(), LamboError> {
        let _writing = self.begin_write_sync()?;
        let value = serde_json::to_value(goals)
            .map_err(|e| LamboError::Config(format!("set_root_goal: {e}")))?;
        self.graph.write().set_root_goal(Some(value));
        self.daemon.wake();
        Ok(())
    }

    /// Map `source` onto `canonical` for canonicalization (spec §7.1 step 4).
    ///
    /// # Not durable (pinned upstream contract S5)
    ///
    /// Synonyms are **RAM-local for this handle's lifetime**. There is no
    /// `Mutation` kind for them by pinned S5 design, so no flush — not even
    /// [`Memory::close`]'s final one — writes them, and `load_session` cannot
    /// restore them: after a reattach the map is empty again.
    ///
    /// The consequence is not cosmetic. A synonym is what makes
    /// `register_user` resolve onto the existing `create_user` concept; once
    /// it is gone the same phrase **creates a duplicate concept** instead of
    /// matching, and [`Memory::retract`]'s resolution loses the alias too. A
    /// caller that needs the mapping across restarts must re-declare it on
    /// every attach (do it right after `build()`, before the first
    /// [`Memory::derive`]).
    pub fn declare_synonym(&self, source: &str, canonical: &str) -> Result<(), LamboError> {
        let _writing = self.begin_write_sync()?;
        self.graph.write().declare_synonym(source, canonical);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Write path
    // -----------------------------------------------------------------------

    /// Derive concepts from a fresh interaction (spec §7) — **async**.
    ///
    /// Async because `MatchStrategy::Hybrid` dispatches to
    /// [`crate::graph::hybrid::derive`], which embeds and queries the store.
    /// One shape serves both strategies rather than two divergent signatures;
    /// the `Canonical` arm does no I/O and never awaits.
    ///
    /// Mirrors every created **and** matched concept into the inverted index
    /// (`index.add` is idempotent per node id, so re-mirroring a matched
    /// concept is a cheap re-index, not a duplicate posting).
    ///
    /// A failure after the interaction was opened leaves that interaction in
    /// the graph — interactions are append-only in v0.1 (spec §9) and an empty
    /// one is harmless. `derive` itself is validate-then-mutate, so no partial
    /// concept write can survive an error.
    pub async fn derive(
        &self,
        concepts: &[(&str, ConceptType)],
        parent_of: &ParentOf<'_>,
    ) -> Result<DeriveOutcome, LamboError> {
        // Held across every await below, so a concurrent `close()` either
        // waits for this whole derive or refuses it (T81-1).
        let _writing = self.begin_write().await?;
        let prompt = concepts
            .iter()
            .map(|(content, _)| *content)
            .collect::<Vec<_>>()
            .join("; ");
        let interaction = self.begin_interaction(Some(prompt))?;

        let outcome = match self.config.match_strategy {
            MatchStrategy::Hybrid => {
                hybrid::derive(
                    self.graph.clone(),
                    self.store.as_ref(),
                    self.embedder.as_ref(),
                    &self.embedding,
                    interaction,
                    &self.agent,
                    concepts,
                    parent_of,
                    self.config.max_cooccurrence_per_derive,
                    self.config.semantic_match_threshold,
                )
                .await?
            }
            MatchStrategy::Canonical => {
                // Short critical section; the guard dies with this block, well
                // before the mirroring below. No `.await` inside it (§6.4).
                let mut g = self.graph.write();
                graph_derive(
                    &mut g,
                    interaction,
                    &self.agent,
                    concepts,
                    parent_of,
                    self.config.max_cooccurrence_per_derive,
                )?
            }
        };

        let mut touched = outcome.created.clone();
        touched.extend(outcome.matched.iter().copied());
        self.mirror_concepts(&touched);
        self.daemon.wake();
        Ok(outcome)
    }

    /// Record an agent action (spec §7): a `Resource` concept plus `Causal` /
    /// `Dependency` edges, on a fresh interaction.
    ///
    /// Synchronous — unlike `derive` there is no hybrid twin and no I/O.
    pub fn record_action(&self, action: &Action<'_>) -> Result<ActionOutcome, LamboError> {
        let _writing = self.begin_write_sync()?;
        let interaction = self.begin_interaction(Some(action.action.to_string()))?;
        let outcome = {
            let mut g = self.graph.write();
            graph_record_action(&mut g, interaction, &self.agent, action)?
        };

        // The action node may be pre-existing (already indexed) — mirroring it
        // anyway is idempotent and covers the case where it is not.
        let mut touched = outcome.created.clone();
        touched.push(outcome.action_node);
        self.mirror_concepts(&touched);
        self.daemon.wake();
        Ok(outcome)
    }

    /// Context-overflow demotion (spec §7): one `Observation` concept per
    /// sentence of `chunk`, all sharing `chunk_group_id` for T5.2 sibling
    /// co-retrieval.
    ///
    /// The interaction opened here carries **no** `prompt_text`: the chunk is
    /// being demoted precisely because it overflowed the context window, and
    /// copying it onto the interaction node would put it straight back into
    /// recall's recent-interactions leg.
    ///
    /// An empty or whitespace-only chunk is a no-op — not even an interaction
    /// is opened.
    pub fn demote(&self, chunk: &str, chunk_group_id: &str) -> Result<Vec<NodeId>, LamboError> {
        let _writing = self.begin_write_sync()?;
        if chunk.trim().is_empty() {
            return Ok(Vec::new());
        }
        let interaction = self.begin_interaction(None)?;
        let created = {
            let mut g = self.graph.write();
            graph_demote(&mut g, interaction, &self.agent, chunk, chunk_group_id)?
        };
        // Observations are concepts: the mod.rs contract names `demote`
        // explicitly, and missing it is the classic silent-staleness bug.
        self.mirror_concepts(&created);
        self.daemon.wake();
        Ok(created)
    }

    /// Blast-radius report for `target`, optionally removing it (spec §6.1,
    /// §13) — **async** because the durable radius is a store query.
    ///
    /// `target` is resolved through the canonicalization pipeline (so a synonym
    /// or a differently-cased phrase finds the same concept), falling back to
    /// an exact `content` match — which is how a demoted `Observation`, skipped
    /// by canonicalization's match step, is reachable.
    ///
    /// [`DryRun::Yes`] mutates **nothing**. [`DryRun::No`] removes the node and
    /// every incident edge from the graph and drops it from the inverted index
    /// in the same critical section, so no reader can observe the node gone
    /// from one and present in the other.
    ///
    /// The report is **measured before the removal**, under a read lock that is
    /// released for the durable-radius store query: with a concurrent writer on
    /// another task, `blast_radius` / `incident_edges` describe the graph as of
    /// the measurement, not as of the removal (an edge added in between is
    /// destroyed but uncounted). Report accuracy only — the removal itself is
    /// atomic under one write lock.
    ///
    /// ## The durable-radius query is bounded (R2-5)
    ///
    /// That store call gets `RETRACT_IO_TIMEOUT`, and a timeout **fails the
    /// whole retraction** — nothing is removed, since the await precedes every
    /// mutation. Note the asymmetry with the arm above it, which is deliberate:
    /// a store *error* is an answer, and the commonest one ("no such session
    /// yet") is what a never-flushed session gives, so it degrades to a warning
    /// and an in-RAM-only count. A store that never answers is a different
    /// animal — the report's durable half cannot be honestly filled in, and
    /// `retract` holds the writers gate across this await, so an unbounded wait
    /// here is also an unbounded `close()` (its step 0 waits for exactly this
    /// permit).
    ///
    /// **This includes a dry run** (R3-3). [`DryRun::Yes`] mutates nothing, so
    /// nothing is at stake in *proceeding* — it could have degraded to the
    /// warning path like the error arm does. It does not, for three reasons.
    /// The asymmetry above is a judgement about the **store** ("an error is an
    /// answer, a hang is not"), and what this call was going to do next cannot
    /// change what the store said. A dry run is the *preview* an operator
    /// authorises the real retraction from, so quietly returning a report whose
    /// durable half is missing is least defensible exactly when the backend is
    /// wedged. And the two calls are meant to be read together: an operator who
    /// gets `Ok` from `DryRun::Yes` and, a second later, a timeout error from
    /// `DryRun::No` has been told two different things about one store. So a
    /// dry run against an unresponsive backend **errors**, having (as always)
    /// mutated nothing.
    pub async fn retract(&self, target: &str, dry_run: DryRun) -> Result<ImpactReport, LamboError> {
        // The gate spans the store call below, so a `close()` racing a live
        // retraction waits for it rather than draining past its removal —
        // which would acknowledge a retraction that resurrects on reattach
        // (T81-1). A DryRun::Yes retract takes the gate too: whether it will
        // mutate is known here, but the store call is the same, and holding a
        // shared read permit costs concurrent writers nothing.
        let _writing = self.begin_write().await?;

        // Resolve + measure under ONE read lock; released before the store call.
        let (node, content, canonization_status, blast_radius, incident_edges) = {
            let g = self.graph.read();
            let node = resolve_concept(&g, target)?;
            let concept = match g.node(node) {
                Some(Node::Concept(c)) => c,
                _ => {
                    return Err(LamboError::Store(StoreError::NotFound(format!(
                        "retract: {target:?} did not resolve to a concept"
                    ))))
                }
            };
            (
                node,
                concept.content.clone(),
                concept.canonization_status,
                format::blast_radius(&g, node),
                g.incident_edges(node).len(),
            )
        };

        // Durable radius — no lock held (spec §6.4), and bounded (R2-5).
        let mut warnings = Vec::new();
        let durable = tokio::time::timeout(
            RETRACT_IO_TIMEOUT,
            self.store
                .blast_radius(&self.session, node, Duration::ZERO, Utc::now()),
        )
        .await;
        let durable_blast_radius = match durable {
            Ok(Ok(count)) => Some(count),
            Ok(Err(err)) => {
                // Not fatal: the graph is the primary tier and already answered.
                // A never-flushed session legitimately lands here.
                warnings.push(format!(
                    "durable blast radius unavailable ({err}); reporting the in-RAM count only"
                ));
                None
            }
            Err(_elapsed) => {
                // Fatal, unlike the error arm above — see the rustdoc: an error
                // is an answer ("no such session yet"), a hang is not, and this
                // one holds the writers gate open behind it. Fatal for a DRY
                // RUN too (R3-3): a dry run is the preview the real retraction
                // is authorised from, so it must not be the one call that
                // quietly reports less about a wedged store.
                //
                // Nothing has been mutated at this point: every graph write is
                // below, so the retraction is refused whole rather than left
                // half-done.
                return Err(LamboError::Store(StoreError::Backend(format!(
                    "retract: durable blast-radius query timed out after {RETRACT_IO_TIMEOUT:?}; \
                     nothing was removed"
                ))));
            }
        };

        let removed = if dry_run.is_dry() {
            false
        } else {
            // graph -> index, the daemon GC's order. Both guards die here.
            let mut g = self.graph.write();
            g.remove_node(node)?;
            self.index.write().remove(node);
            drop(g);
            self.daemon.wake();
            true
        };

        Ok(ImpactReport {
            target: node,
            content,
            canonization_status,
            blast_radius,
            durable_blast_radius,
            incident_edges,
            dry_run: dry_run.is_dry(),
            removed,
            warnings,
        })
    }

    /// Acquire or extend a soft lock on `node` for this handle's agent
    /// (spec §11). Cross-agent contention returns [`LamboError::Conflict`].
    ///
    /// # Not durable (pinned upstream contract S5)
    ///
    /// Reservations live in RAM only: like synonyms they have no `Mutation`
    /// kind, so no flush — [`Memory::close`]'s final one included — persists
    /// them and no reattach restores them. A restart releases every soft lock
    /// in the session; a caller that reattaches must re-`reserve` anything it
    /// still holds, and must not read "no reservation" after a restart as
    /// "nobody else was working on this".
    pub fn reserve(&self, node: NodeId, ttl: Duration) -> Result<Reservation, LamboError> {
        let _writing = self.begin_write_sync()?;
        let mut g = self.graph.write();
        graph_reserve(&mut g, node, &self.agent, ttl, Utc::now())
    }

    /// Release this agent's soft lock on `node` — the pair of
    /// [`Memory::reserve`]. A non-owner gets [`LamboError::Conflict`].
    pub fn release(&self, node: NodeId) -> Result<(), LamboError> {
        let _writing = self.begin_write_sync()?;
        let mut g = self.graph.write();
        graph_release(&mut g, node, &self.agent)
    }

    // -----------------------------------------------------------------------
    // Read path
    // -----------------------------------------------------------------------

    /// Three-phase recall (spec §8), rendered as the T5.3 context block.
    ///
    /// The query is embedded first — **before** any lock — and only when the
    /// store actually claims `VECTOR_SEARCH`; otherwise the vector leg would be
    /// refused anyway and the embed call would be wasted latency. An embed
    /// failure degrades to the keyword + recent legs with a warning on the
    /// result rather than failing the read.
    pub async fn recall(&self, query: RecallQuery) -> Result<RecallResult, LamboError> {
        self.ensure_open()?;

        let mut warnings = Vec::new();
        let embedding = if self
            .store
            .capabilities()
            .contains(Capabilities::VECTOR_SEARCH)
        {
            match self.embedder.embed(&query.query).await {
                Ok(vector) => Some(vector),
                Err(err) => {
                    warnings.push(format!(
                        "recall: query embedding failed ({err}); vector leg skipped"
                    ));
                    None
                }
            }
        } else {
            None
        };

        // The recall cache is `&mut` across `Daemon::recall`'s awaits. This is
        // NOT the graph lock — `Daemon::recall` takes and releases that itself,
        // after its own store I/O.
        let mut cache = self.recall_cache.lock().await;
        let mut result = self
            .daemon
            .recall(
                &self.session,
                query,
                self.store.as_ref(),
                embedding.as_deref().map(|vector| (vector, &self.embedding)),
                self.config.recall_weights,
                &mut cache,
            )
            .await;
        drop(cache);

        warnings.append(&mut result.warnings);
        result.warnings = warnings;
        Ok(result)
    }

    /// The canonical ("saints") memories — spec §10's `Canonical` nodes.
    ///
    /// A graph scan, deliberately: canonization status lives on the concept and
    /// no store query for it exists or is needed. The graph is the primary tier
    /// (spec §2.1), so it is also the freshest answer.
    ///
    /// Ordered blast-radius descending, then oldest first, then by id — total
    /// and deterministic, so `lambo saints` output is stable across runs.
    pub fn canonical_memories(&self) -> Vec<CanonicalMemory> {
        let g = self.graph.read();
        let radii = format::blast_radii(&g);
        let mut out: Vec<CanonicalMemory> = g
            .concepts()
            .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
            .map(|c| CanonicalMemory {
                node_id: c.id,
                content: c.content.clone(),
                concept_type: c.concept_type,
                blast_radius: radii.get(&c.id).copied().unwrap_or(0),
                created_at: c.created_at,
                access_count: c.access_count,
            })
            .collect();
        drop(g);
        out.sort_by(|a, b| {
            b.blast_radius
                .cmp(&a.blast_radius)
                .then(a.created_at.cmp(&b.created_at))
                .then(a.node_id.0.cmp(&b.node_id.0))
        });
        out
    }

    /// Session health. `flush_lag`, `log_depth` and `flush_depth` are the spec
    /// §2.4 observable durability bound — the loss window on a writer crash.
    pub fn stats(&self) -> MemoryStats {
        let flush = self.flush.stats();
        let g = self.graph.read();
        let concept_count = g.concepts().count();
        let canonical_count = g
            .concepts()
            .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
            .count();
        let stats = MemoryStats {
            session: self.session.clone(),
            agent: self.agent.clone(),
            flush_lag: flush.lag,
            log_depth: g.log_len(),
            flush_depth: flush.depth,
            dead_lettered: flush.dead_lettered,
            degraded: self.flush.degraded(),
            node_count: g.node_count(),
            edge_count: g.edge_count(),
            concept_count,
            canonical_count,
            epoch: g.epoch(),
            daemon_cycles: self.daemon.cycles(),
            canonization_cycles: self.canon.cycles(),
            canonization_failures: self.canon.failures(),
        };
        drop(g);
        stats
    }

    /// Subscribe to `Conflict` / `Drift` / `Stale` / `HighRisk` / `Canonized`
    /// events (spec §6.1 — events replace callbacks).
    ///
    /// The **first** call returns the receiver subscribed before the daemon was
    /// spawned, so it sees the spec §2.5 warm-up cycle's condition set — on a
    /// resumed session that is the whole restored set, including the demo's
    /// planted `Conflict`, and a receiver created afterwards would miss it
    /// (CONC-3: emission is on transition, so nothing re-publishes for a late
    /// subscriber). Later calls get a fresh subscription from the daemon.
    ///
    /// A dropped receiver is not an error; a lagging one misses messages and
    /// re-syncs rather than blocking the daemon.
    pub fn events(&self) -> broadcast::Receiver<DaemonEvent> {
        if let Some(rx) = self.startup_events.lock().take() {
            return rx;
        }
        self.daemon.events()
    }

    // -----------------------------------------------------------------------
    // Shutdown
    // -----------------------------------------------------------------------

    /// Final flush + clean shutdown of all three tasks (spec §6.1).
    ///
    /// Idempotent **after success**: once a close has made the tail durable
    /// every later call is `Ok(())` and does nothing.
    ///
    /// ## Concurrent and repeated calls
    ///
    /// The body is serialized. A second caller that arrives while a close is in
    /// flight **parks until it finishes** and then returns its outcome — it
    /// never gets an early `Ok` over an in-flight final flush (which, if it
    /// gated process exit, would let runtime teardown cancel that flush and
    /// lose the tail).
    ///
    /// ## Retry after failure
    ///
    /// A close that fails is **retryable, and says so by staying failed**: the
    /// drained batch goes back to the front of the graph log (where the next
    /// `drain_log` finds it, in order), the failure is returned, and the
    /// success flag is *not* set. Call `close()` again — after the store
    /// recovers — and the same tail is flushed. Repeated calls keep returning
    /// the failure for as long as the tail is undurable; `Ok(())` from
    /// `close()` always means "the tail is written".
    ///
    /// The session is closed to writers from the first call regardless: the
    /// background tasks are stopped and every mutating method is refused, so a
    /// retry re-attempts exactly the same tail rather than a growing one.
    ///
    /// ## Bounding this against the lease — caller's contract (T86-4)
    ///
    /// `close()` aborts the lease heartbeat **first** (right after latching
    /// `closed`), because the success paths below release the lease explicitly
    /// and it must not keep being refreshed underneath that release. From that
    /// moment the lease is no longer refreshed, so it stays valid only for its
    /// **remaining TTL** — at worst one [`LEASE_HEARTBEAT_INTERVAL`] short of a
    /// full [`LEASE_TTL`] (≈30s) if the last beat landed just before close.
    ///
    /// This method is otherwise **unbounded**: the step-2 flush-task join and the
    /// step-4 final flush each have their own internal timeout ladders, but their
    /// composition has no single wall-clock cap. A `close()` whose final flush
    /// runs longer than that remaining validity therefore lets the lease
    /// **expire while this handle is still flushing its tail**, which can admit a
    /// second writer mid-flush — the exact window the lease exists to close.
    ///
    /// **The `serve` path avoids this by bounding `close()` below the TTL.**
    /// [`crate::mcp::serve`](mod@crate::mcp::serve) caps its close at `CLOSE_GRACE` (10s) inside a
    /// `SHUTDOWN_BUDGET` (15s), and a build-time assertion pins
    /// `LEASE_TTL (45s) > SHUTDOWN_BUDGET`, so the lease is provably still valid
    /// when `release` lands. **A direct library caller gets no such bound** and
    /// **MUST** cap `close()` — e.g. under [`tokio::time::timeout`] — below the
    /// lease's remaining validity, exactly as `serve` does, if two processes may
    /// contend on the session. (Reordering the heartbeat to keep refreshing
    /// across the flush was considered and rejected: it entangles the heartbeat's
    /// lost-lease fence with the mid-close release/flush ordering, and this
    /// close body's ordering is load-bearing for the R2-1/R3-1 cancellation and
    /// custody invariants above. Bounding at the call site is the smaller-risk
    /// contract, and in-repo `serve` is the only production caller.)
    ///
    /// ## Cancellation
    ///
    /// **Dropping this future never destroys the tail** (R2-1). A caller that
    /// wraps `close()` in a [`timeout`](tokio::time::timeout) or drops it out of
    /// a `select!` leaves the drained mutations back at the front of the graph
    /// log — the state a *failed* close leaves, and retryable the same way. The
    /// session stays closed to writers, `succeeded` stays unset, and the next
    /// `close()` re-drains and re-flushes exactly that tail. A cancelled close
    /// can therefore never be followed by an `Ok(())` that did not write it.
    ///
    /// Cancelling mid-flush may still have let the store apply the batch: the
    /// retry replays it, which the `src/graph/mod.rs` replay contract makes
    /// idempotent (the failure path has always made the same bet).
    ///
    /// **Nor does it strand a background task** (R3-1). Cancellation lands on
    /// whichever `.await` this future is parked on, and the longest of those is
    /// the step-2 join — the one an external `timeout` almost always fires in.
    /// A dropped `JoinHandle` *detaches* its task rather than stopping it, so
    /// that used to leave a live flush loop still holding the tail in its own
    /// `pending` buffer while the retry — finding an empty slot and an empty log
    /// — took the shortcut below and returned `Ok(())`. Every handle therefore
    /// travels in a `HandleCustody` guard that returns it to its slot unless
    /// the join actually completed, so a retry re-joins the same task and picks
    /// up the tail it requeues. The invariant that falls out of it —
    /// no success is ever latched over an un-joined flush task — is asserted in
    /// `Memory::latch_success`.
    ///
    /// ## The drain (COH-6)
    ///
    /// `FlushTask` owns its `pending` buffer, so a hard
    /// [`JoinHandle::abort`](tokio::task::JoinHandle::abort) on it would drop
    /// every mutation drained from the log but not yet durable — above all a
    /// batch RETAINED after a failed flush, which sits at the front of that
    /// buffer. So:
    ///
    /// 0. **Shut the writers up — the surface's own, then the tasks'.** Latch
    ///    `closed` so new calls are refused, take the write side of
    ///    `Memory::writers`, which waits out every write already in flight on
    ///    a caller task (T81-1), and only then stop the two mutation producers,
    ///    canonization first and the daemon second. It takes both halves for
    ///    "nothing new lands after the drain" to be true of the *surface* and
    ///    not just of the tasks. `abort()` is safe for both tasks: neither
    ///    holds a `parking_lot` guard across an `.await`, and the write-behind
    ///    log carries any canonization hop whose phase-4 record was cancelled.
    ///    Both are then **joined**, not merely aborted: tokio cancels a running
    ///    task at its next `.await`, so until the join returns an aborted
    ///    producer can still finish a synchronous stretch — and append to the
    ///    log (R3-1).
    /// 1. [`FlushTask::stop`] — the loop finishes its current `cycle()` (an
    ///    in-flight flush and its retry/backoff complete; a post-retry
    ///    `RETAINED_BACKOFF` hold is *not* waited out), re-appends `pending` to
    ///    the **front** of the graph log, and exits.
    /// 2. Await its handle — the task is gone and can no longer take the graph
    ///    lock, so step 3 races nothing. The handle is held in a
    ///    `HandleCustody` guard for the whole of that await, so a cancelled
    ///    join returns it to its slot instead of detaching the task (R3-1).
    /// 3. Take the graph lock, `drain_log()`, release. The batch is handed
    ///    straight to a `TailCustody` guard, which returns it to the log if
    ///    this future is dropped before step 4 makes it durable (R2-1).
    /// 4. `store.flush(&batch)` directly, with **no lock held**, armored like
    ///    every background attempt is: a `FLUSH_ATTEMPT_TIMEOUT` bound
    ///    (STORE-2) and panic containment, so a hung or panicking adapter
    ///    yields an error instead of wedging or unwinding out of `close`.
    ///    Its result is this method's result; on failure the batch is returned
    ///    to the log (see *Retry after failure*).
    ///
    /// A retained batch is therefore flushed or surfaced — never silently lost.
    ///
    /// ## How long it can take
    ///
    /// Bounded by the flush loop's current cycle (worst case
    /// `FLUSH_ATTEMPT_TIMEOUT × (retries + 1)`), plus the slowest write in
    /// flight at step 0 — the gate waits for it rather than losing it — plus
    /// one `FLUSH_ATTEMPT_TIMEOUT` for step 4.
    ///
    /// Step 0 is itself bounded now (R2-5): every store call a gated write can
    /// be parked in has a timeout — `RETRACT_IO_TIMEOUT` for `retract`'s
    /// durable radius, [`hybrid::HYBRID_IO_TIMEOUT`] over hybrid `derive`'s
    /// whole embed/query phase. A caller-supplied **embedder** is the one
    /// remaining way to stretch it: `Embedder` carries no bound of its own, so
    /// an adapter that never returns still parks a permit indefinitely. An
    /// owner that needs a hard wall-clock cap on `close()` should wrap it in a
    /// `timeout` — which is safe: a dropped `close()` leaves the tail on the
    /// log for the retry (see *Cancellation*).
    ///
    /// ## When it does not flush
    ///
    /// A session that degraded to `durability="none"` (spec §2.3) stopped all
    /// store I/O by design. `close` does not quietly resurrect it: it skips the
    /// final flush and returns an error saying the tail was not written, rather
    /// than reporting a durability it did not deliver. The tail stays in the
    /// log (so `stats().log_depth` keeps telling the truth) and every later
    /// `close()` returns the same error — a degraded session has no path back
    /// to a durable tail, and saying `Ok` would be a lie.
    ///
    /// **A degraded session errors even when its log is empty** (R2-3). While
    /// degraded the flush task keeps draining the log and dropping what it
    /// drained (STORE-3), so an empty log is that mode's steady state — the
    /// tail was dead-lettered, not written — and an `Ok(())` there would be the
    /// same lie by a quieter route. `degraded()` is therefore checked before
    /// the empty-log shortcut, not after it.
    pub async fn close(&self) -> Result<(), LamboError> {
        // T81-6: one close body at a time. A concurrent second caller parks
        // here and, when it gets in, either sees the success flag or re-runs
        // the (idempotent) shutdown — never an early `Ok` over an in-flight
        // final flush.
        let mut succeeded = self.close_state.lock().await;
        if *succeeded {
            return Ok(());
        }

        // 0 — the writers gate (T81-1). Latch first so new writes are refused,
        // then take the write side: it is granted only once every write that
        // slipped in before the latch has finished, so nothing this session
        // acknowledged can still be on its way to the log. Held for the rest of
        // `close` — the drain below must be the last word on the log.
        self.closed.store(true, Ordering::Release);
        let _quiesced = self.writers.write().await;

        // Stop the lease heartbeat before anything else in the shutdown: from
        // here the lease is released explicitly on the success paths below, so
        // it must not keep being refreshed. Aborting is synchronous and the task
        // touches neither the graph nor the tail, so no custody/join is needed.
        self.abort_heartbeat();

        // T86-2: a fenced handle lost its lease — another writer owns the
        // session now. The final flush this close would otherwise do is exactly
        // the split-brain write the lease exists to prevent, so refuse it: stop
        // the tasks, DROP the tail (it dies with this handle as it would on a
        // crash), and do NOT release the lease (it is not ours to release). Fail
        // closed with the honest refusal rather than a lying `Ok` over a tail we
        // may never make durable. `succeeded` stays false; a retried close hits
        // this same branch (the handles are already reaped) and errors again.
        if self.lease_lost() {
            for slot in [&self.canon_handle, &self.daemon_handle, &self.flush_handle] {
                if let Some(handle) = slot.lock().take() {
                    handle.abort();
                }
            }
            let undrained = self.graph.read().log_len();
            tracing::error!(
                session = %self.session,
                mutations = undrained,
                "close: this handle lost its single-writer lease; refusing to flush the tail \
                 ({undrained} mutations discarded) and NOT releasing the lease — another writer \
                 owns the session"
            );
            return Err(self.lease_lost_error());
        }

        // ...and the two mutation producers off, before the drain. Every
        // handle travels in a `HandleCustody` guard: cancelled on a join, this
        // future must hand the handle back to its slot rather than detach a
        // task that is still able to write (R3-1). `abort()` is synchronous, so
        // it cannot be skipped by a cancellation — but only the join proves the
        // task has actually stopped.
        //
        // Coverage note (R4-3): only the flush handle's custody is pinned by a
        // test. The canon/daemon detach window (an aborted task finishing a
        // synchronous stretch that appends to the log) is real — probed
        // directly in review — but too narrow to exercise deterministically:
        // neither loop has a long synchronous stretch to park in. Custody is
        // applied uniformly anyway because the hazard class is identical and
        // reasoning per-handle about window width is exactly the mistake R3-1
        // caught. Same class of documented blind spot as `begin_write_sync`'s
        // re-check (R2-6) and the flush select's `biased;` (T81-4).
        let mut canon = HandleCustody::take(&self.canon_handle);
        canon.abort();
        let _ = canon.join().await;
        drop(canon);

        let mut daemon = HandleCustody::take(&self.daemon_handle);
        daemon.abort();
        let _ = daemon.join().await;
        drop(daemon);

        // 1 — graceful stop; the loop returns custody of `pending`.
        self.flush.stop();

        // 2 — join. After this the flush task cannot touch the graph. This is
        // the long await (the whole of `close`'s "worst case ≈ 2 minutes") and
        // so the one an external timeout fires in: dropping the handle here
        // used to leave a zombie flush task holding the tail in its own
        // `pending`, invisible to the retry, to `Drop`'s warning and to the log
        // (R3-1). Custody keeps it re-joinable instead.
        let mut flush = HandleCustody::take(&self.flush_handle);
        if let Some(Err(err)) = flush.join().await {
            if !err.is_cancelled() {
                tracing::warn!(error = %err, "flush task did not stop cleanly");
            }
        }
        drop(flush);

        // 3 — final drain. Short critical section, guard dies with the block.
        let batch = { self.graph.write().drain_log() };
        // Custody of the drained tail passes to `TailCustody` immediately:
        // from here until it is durable those mutations exist nowhere else,
        // and `close()` is a future its caller may drop (R2-1).
        let mut tail = TailCustody::new(&self.graph, batch);

        // A degraded session errors **before** the empty-log shortcut (R2-3).
        // While degraded the flush task keeps draining the log and DROPPING
        // each batch (STORE-3), so an empty log is the *normal* degraded
        // state, not evidence that anything was written. Checked second, the
        // shortcut turned exactly that state into `Ok(())` — a durability
        // claim over a tail the session had already dead-lettered.
        if self.flush.degraded() {
            let count = tail.len();
            tracing::error!(
                mutations = count,
                session = %self.session,
                "close: session is degraded (durability=\"none\"); the tail was NOT written \
                 ({count} mutations still in the log)",
            );
            // `tail`'s `Drop` puts the batch back on the log: the mutations
            // are no more durable for having been drained, and leaving them
            // there keeps `stats().log_depth` honest about what was lost
            // (T81-5). `succeeded` stays false, so no later `close()` can
            // report `Ok` over this tail.
            let detail = if count == 0 {
                "the log is empty because degraded mode drops what it drains, not because the \
                 tail was written"
                    .to_string()
            } else {
                format!("{count} tail mutations were not flushed")
            };
            return Err(LamboError::Store(StoreError::Backend(format!(
                "close: session {} degraded to durability=\"none\"; {detail}",
                self.session
            ))));
        }

        if tail.is_empty() {
            // Graceful close: hand off the lease now rather than waiting out the
            // TTL, so the next writer takes the session immediately (T8.6).
            self.release_lease_once().await;
            self.latch_success(&mut succeeded);
            return Ok(());
        }

        // 4 — the final flush, no lock held, armored (T81-2). The result is
        // bound out of the `match` scrutinee so the borrow of `tail` ends
        // here rather than spanning the arms.
        let count = tail.len();
        let flushed = final_flush(self.store.as_ref(), tail.batch(), Some(self.lease_token)).await;
        match flushed {
            Ok(()) => {
                // Custody ends: the tail is durable, so it must NOT go back
                // on the log. Nothing awaits between here and the return, so
                // no cancellation can land in this window.
                tail.durable();
                tracing::info!(
                    mutations = count,
                    session = %self.session,
                    "Memory session closed (tail flushed)"
                );
                // Tail is durable: release the lease so the handoff is clean
                // (T8.6). A failed flush (the `Err` arm below) deliberately does
                // NOT release — it keeps the lease for a retry and lets it lapse
                // at TTL if none comes.
                self.release_lease_once().await;
                self.latch_success(&mut succeeded);
                Ok(())
            }
            Err(err) => {
                // T81-5: the batch is NOT lost with the error. `tail`'s `Drop`
                // puts it back at the FRONT of the log — `push_front_log`'s
                // documented purpose — so a retried `close()` (or an owner
                // that fixes the store first) drains and flushes exactly this
                // tail, in order.
                drop(tail);
                tracing::error!(
                    error = %err,
                    mutations = count,
                    session = %self.session,
                    "close: final flush failed; {count} tail mutations returned to the graph log \
                     — retry close() once the store is healthy",
                );
                Err(LamboError::Store(err))
            }
        }
    }

    // -----------------------------------------------------------------------
    // Internals
    // -----------------------------------------------------------------------

    /// Open a fresh interaction at the tail of the temporal chain.
    ///
    /// `created_at` is stamped **here**, from the process clock — never from a
    /// caller (P6 review F18: every concept and edge below this interaction
    /// inherits the timestamp, and backdating by 61s would neuter the
    /// `canonization_edge_min_age` inflation guard). `self.clock` *is* that
    /// process clock: [`Utc::now`] everywhere except `lambo demo`, which pins
    /// it at construction (see [`MemoryBuilder::clock`]).
    ///
    /// Reading the chain tail and inserting happen under one write lock, so two
    /// concurrent writers cannot both claim the same predecessor.
    fn begin_interaction(&self, prompt: Option<String>) -> Result<NodeId, LamboError> {
        let id = NodeId::new();
        let created_at = (self.clock)();
        let mut g = self.graph.write();
        let previous_id = g.temporal_chain().last().copied();
        g.insert_interaction(Interaction {
            id,
            session_id: self.session.clone(),
            agent_id: self.agent.clone(),
            prompt_text: prompt,
            previous_id,
            created_at,
        })?;
        Ok(id)
    }

    /// Mirror concept writes into the inverted index (the `src/graph/mod.rs`
    /// contract). Ids that are not concepts are skipped.
    ///
    /// Lock order is **graph read → index write**, matching the daemon's GC
    /// sync; taking them the other way round would deadlock against it. Both
    /// guards are held together on purpose so a concurrent recall — which reads
    /// (graph, index) as a pair — sees an atomic publication.
    fn mirror_concepts(&self, ids: &[NodeId]) {
        if ids.is_empty() {
            return;
        }
        let g = self.graph.read();
        let mut index = self.index.write();
        for &id in ids {
            if let Some(Node::Concept(concept)) = g.node(id) {
                index.add(concept);
            }
        }
    }

    /// Give up this handle's [`ACTIVE_SESSIONS`] slot, at most once (R2-4).
    ///
    /// Called by a successful `close()` and by [`Drop`], whichever comes first.
    /// The flag is what makes it safe for both to call it: the registry keys on
    /// session + agent id, so a second release would evict whichever *other*
    /// handle had re-attached under the same ids in between — turning the
    /// second-writer detector into a source of the very blind spot it exists to
    /// remove.
    fn unregister_once(&self) {
        if self.registered.swap(false, Ordering::AcqRel) {
            unregister_session(&self.session, &self.agent);
        }
    }

    /// Abort the lease heartbeat, if it is still running (T8.6). Synchronous, so
    /// both `close()` and `Drop` can call it. Idempotent — the second caller
    /// finds the slot already `None`.
    fn abort_heartbeat(&self) {
        if let Some(handle) = self.heartbeat_handle.lock().take() {
            handle.abort();
        }
    }

    /// Release this handle's single-writer lease, at most once (T8.6).
    ///
    /// Called only from the **success** paths of `close()`: a graceful close
    /// hands the session off immediately rather than waiting out the TTL. Guarded
    /// by `lease_released` so a retried `close()` does not release twice — the
    /// release is holder-scoped in the store anyway, but the flag also skips a
    /// redundant round-trip. `Drop` deliberately does not call this (see the
    /// field docs): a handle abandoned without a clean close lets its lease lapse
    /// at TTL, the crash-shaped path.
    async fn release_lease_once(&self) {
        if self.lease_released.swap(true, Ordering::AcqRel) {
            return;
        }
        if let Err(err) = self
            .store
            .release_lease(&self.session, &self.lease_holder)
            .await
        {
            // Non-fatal: the lease lapses at TTL even if the explicit release
            // could not reach the store. A failed release must not turn a
            // durable close into an error.
            tracing::warn!(
                session = %self.session,
                holder = %self.lease_holder,
                error = %err,
                "could not release the single-writer lease on close; it will lapse at TTL instead"
            );
        }
    }

    /// Release the single-writer lease after a `close()` that was **abandoned**
    /// (L82-1).
    ///
    /// `close()` releases on its own success paths, and deliberately does not on
    /// a failed final flush — that failure keeps the lease for a retried close
    /// and lets it lapse at TTL if none comes. Neither covers the case the live
    /// review hit: `serve` bounds `close()` with a deadline, and a close that
    /// blows the deadline is *dropped*, so it never reaches either path. The
    /// process then exits with the lease row still present, and the session is
    /// wedged for the rest of the TTL — a second, avoidable failure stacked on
    /// top of the lost tail.
    ///
    /// Releasing here is sound because the release is not a durability claim.
    /// It says "this process is gone", which is true — `serve` calls this
    /// immediately before exiting. The tail is lost either way; keeping the
    /// lease does not make it less lost, it only makes the next writer wait 45 s
    /// to find that out.
    ///
    /// **Except when this handle was fenced.** A lost lease belongs to another
    /// writer, and `release_lease` is holder-scoped precisely so a straggler
    /// cannot evict it — but skipping the call entirely also skips the log line
    /// that would confuse an operator reading it. `close()`'s fenced branch has
    /// the same rule and the same reason.
    ///
    /// Best-effort by construction: `release_lease_once` already downgrades a
    /// store error to a warning, and the caller bounds this with its own
    /// deadline.
    pub async fn release_lease_after_abandoned_close(&self) {
        if self.lease_lost() {
            tracing::debug!(
                session = %self.session,
                "not releasing the single-writer lease after an abandoned close: this handle was \
                 fenced, so the lease belongs to another writer"
            );
            return;
        }
        // The heartbeat must not outlive the release and re-acquire what we just
        // gave up. `close()` aborts it first thing, but an abandoned close may
        // have been dropped before reaching that line.
        self.abort_heartbeat();
        self.release_lease_once().await;
    }

    /// The one place `close()` latches success and gives up its registry slot.
    ///
    /// Both success paths — the empty-log shortcut and a completed step-4 flush
    /// — go through here so the R3-1 invariant is asserted once for both: **no
    /// `close()` may report success while a flush `JoinHandle` is still parked
    /// in its slot un-joined.** A parked handle means a live flush task, and a
    /// live flush task may hold the tail in its own `pending` buffer, where an
    /// empty log looks exactly like a written one.
    ///
    /// `HandleCustody` is what *guarantees* it, and the guarantee is a
    /// two-line argument: the slot is emptied only into a custody guard, and
    /// that guard hands the handle back unless the join returned. So `None` at
    /// step 3 means "reaped", never "detached" — the state that made the
    /// shortcut a lie. The assertion is the pin on that reasoning rather than a
    /// second mechanism, hence `debug_assert!` — and it is a *pin only* (R4-2):
    /// a neutered guard leaves the slot `None`, the very state this asserts,
    /// so the assertion cannot fire on the regression that matters. Detachment
    /// is undetectable from here by construction; the enforcement is
    /// `HandleCustody` and the R3-1 regression test's durability assertion,
    /// not this line.
    fn latch_success(&self, succeeded: &mut bool) {
        debug_assert!(
            self.flush_handle.lock().is_none(),
            "close() latched success with an un-joined flush task still in its slot: the tail may \
             be sitting in that task's pending buffer (R3-1)"
        );
        *succeeded = true;
        self.unregister_once();
    }

    fn ensure_open(&self) -> Result<(), LamboError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(self.closed_error());
        }
        Ok(())
    }

    /// Test hook (T86-2): latch the lease-lost fence exactly as
    /// [`spawn_lease_heartbeat`] does when a refresh comes back
    /// [`LeaseOutcome::Held`]. The real heartbeat only fires on its
    /// [`LEASE_HEARTBEAT_INTERVAL`] (15s), so a test drives the fence directly
    /// after arranging a real store-level takeover.
    ///
    /// Gate matches the sole caller's tests module (not bare `test`): under
    /// `--no-default-features` feature combos that module is compiled out and
    /// a bare `#[cfg(test)]` method becomes a dead-code error under
    /// `-D warnings` (CI feature-matrix).
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    fn simulate_lease_loss(&self) {
        self.lease_lost.store(true, Ordering::Release);
    }

    fn closed_error(&self) -> LamboError {
        LamboError::Config(format!("session {} is closed", self.session))
    }

    /// `true` once the heartbeat latched a lost lease (T86-2).
    fn lease_lost(&self) -> bool {
        self.lease_lost.load(Ordering::Acquire)
    }

    /// The honest refusal a fenced handle returns (T86-2): another writer owns
    /// the session now, so this process is no longer the writer and refuses to
    /// touch the graph. A `Conflict`, the same class as the build-time
    /// single-writer refusal.
    fn lease_lost_error(&self) -> LamboError {
        LamboError::Conflict(format!(
            "session {} lost its single-writer lease: this process's lease expired (the store was \
             unreachable past the {}s TTL) and another writer took the session. This handle is no \
             longer the writer and refuses further writes — its tail will not be flushed. Spec \
             §2.2 is one writer per session; an operator must reconcile and, if needed, force a \
             takeover: {}",
            self.session,
            LEASE_TTL.as_secs(),
            crate::store::lease::OPERATOR_OVERRIDE,
        ))
    }

    /// Enter the writers gate from an **async** method (T81-1).
    ///
    /// [`Memory::ensure_open`] first, so a write against an already-closed
    /// session is refused without queueing behind `close()`'s write side; then
    /// the read permit; then the check **again**, because `close()` may have
    /// latched `closed` while this call waited for the permit.
    ///
    /// The second check is what makes the gate airtight. If it sees `closed`
    /// as open, the latch had not happened yet, so `close()`'s later
    /// `writers.write()` must wait for the permit this returns — the write
    /// completes and its mutations are in `close()`'s final batch. If it sees
    /// `closed`, the write is refused and never touched the graph.
    async fn begin_write(&self) -> Result<AsyncRwLockReadGuard<'_, ()>, LamboError> {
        self.ensure_open()?;
        // T86-2: a fenced handle (lost its lease) refuses before it touches the
        // gate — the strongest refusal, checked first.
        if self.lease_lost() {
            return Err(self.lease_lost_error());
        }
        let permit = self.writers.read().await;
        self.ensure_open()?;
        if self.lease_lost() {
            return Err(self.lease_lost_error());
        }
        Ok(permit)
    }

    /// Enter the writers gate from a **synchronous** method.
    ///
    /// `try_read` rather than `read().await`: these methods cannot await, and
    /// `blocking_read` on a runtime worker would be worse than the race it
    /// fixes. The only thing that holds the write side is `close()`, so a
    /// failed `try_read` means exactly "a close is in progress" and maps to the
    /// closed error. The post-acquire re-check is the same barrier as
    /// [`Memory::begin_write`]'s — and since a sync method never awaits, the
    /// permit is held continuously from the check to the last mutation, so
    /// `close()` cannot drain past it.
    ///
    /// **Coverage note (R2-6).** The `try_read` refusal is pinned by
    /// `a_sync_write_is_refused_while_the_gate_is_taken`; the re-check itself is
    /// not, and cannot honestly be. Its window — `try_read` *succeeding* after
    /// `close()` latched but before `close()` requests the write side — is one
    /// instruction wide and needs true parallelism to enter, so no
    /// deterministic single-threaded interleaving reaches it and a probabilistic
    /// hammer would not fail reliably either. It is kept because it costs an
    /// atomic load and closes the same hole `begin_write`'s does — where the
    /// window is wide enough to construct, and *is* constructed, by
    /// `a_write_that_takes_the_gate_after_close_latched_is_refused`. Same
    /// blind-spot class as T81-4's `biased;`.
    fn begin_write_sync(&self) -> Result<AsyncRwLockReadGuard<'_, ()>, LamboError> {
        self.ensure_open()?;
        // T86-2: fenced handles refuse before touching the gate (see `begin_write`).
        if self.lease_lost() {
            return Err(self.lease_lost_error());
        }
        let permit = self.writers.try_read().map_err(|_| self.closed_error())?;
        self.ensure_open()?;
        if self.lease_lost() {
            return Err(self.lease_lost_error());
        }
        Ok(permit)
    }
}

impl std::fmt::Debug for Memory {
    /// Identity and liveness only — the store, embedder and task handles have
    /// no useful debug form, and the graph must not be formatted under a lock.
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Memory")
            .field("session", &self.session)
            .field("agent", &self.agent)
            .field("match_strategy", &self.config.match_strategy)
            .field("embedding", &self.embedding)
            .field("closed", &self.closed.load(Ordering::Acquire))
            .finish_non_exhaustive()
    }
}

impl Drop for Memory {
    /// Abort any task [`Memory::close`] did not stop, and say so if a tail dies
    /// with this handle.
    ///
    /// This is the leak guard, not the shutdown path: dropping without `close`
    /// abandons the tail (see `close`'s drain), so it warns. After a successful
    /// `close` every handle is already `None` and this is a no-op.
    ///
    /// **Two ways to lose a tail, not one** (R2-2, amended by R3-1/R4-1).
    /// Keying the warning on task handles still being `Some` catches the
    /// never-closed handle but is blind to the *closed-and-failed* one: a
    /// `close()` that **failed** has reaped all three handles, so `leaked` is
    /// false — while the mutations it kept are sitting in the log, about to be
    /// dropped in silence. Precisely the case `close`'s "retry after failure"
    /// contract asks the owner to act on, so it must not go out quietly. The
    /// log is therefore checked too, whatever the handles say.
    ///
    /// A **cancelled** `close()` is the third shape (R4-1): `HandleCustody`
    /// has put the handles *back*, so `leaked` is true and the first branch
    /// fires — but its count can understate the loss, because a tail drained
    /// by the flush task before the cancellation lives in that task's
    /// `pending`, not in the log this counts. The first message says so
    /// rather than pretending the log count is the whole story.
    fn drop(&mut self) {
        // No-op if a successful `close()` already released the slot (R2-4).
        self.unregister_once();
        // Stop the lease heartbeat so a leaked handle stops refreshing — its
        // lease then lapses at TTL and the session becomes takeable (T8.6). The
        // lease itself is NOT released here: `Drop` cannot `await` the store, and
        // a handle dropped without a clean close is the crash-shaped path where
        // expiry is the right release. A successful `close()` already released it.
        self.abort_heartbeat();
        let mut leaked = false;
        for handle in [&self.daemon_handle, &self.flush_handle, &self.canon_handle] {
            if let Some(handle) = handle.lock().take() {
                handle.abort();
                leaked = true;
            }
        }
        let undrained = self.graph.read().log_len();
        if leaked {
            tracing::warn!(
                session = %self.session,
                mutations = undrained,
                "Memory dropped with live background tasks (never closed, or a close() was \
                 cancelled): tasks aborted and {undrained} un-flushed mutations in the log were \
                 discarded — after a cancelled close(), mutations held in the flush task's \
                 buffer are lost as well and are not in this count"
            );
        } else if undrained > 0 {
            tracing::warn!(
                session = %self.session,
                mutations = undrained,
                "Memory dropped after a close() that did not finish: {undrained} un-flushed \
                 mutations were discarded. close() returned an error (or was cancelled) and \
                 kept that tail in the log for a retry that never came."
            );
        }
    }
}

/// Custody of a background task's [`JoinHandle`] while `close()` stops and
/// reaps it — R3-1.
///
/// `close()` used to lift each handle out of its slot (`slot.lock().take()`)
/// and then `await` it as a bare local. That await is the long one — the flush
/// join is what `close`'s "worst case ≈ 2 minutes" measures, and an external
/// [`timeout`](tokio::time::timeout) around `close()` is the posture its own
/// docs invite. Dropping the future there dropped the local `JoinHandle`, which
/// **detaches** the task rather than stopping it: the flush loop kept running,
/// kept its `pending` buffer — which holds the tail, the log having already
/// been drained into it — and kept writing the session through its own `Arc`s.
/// The slot was left `None`, so the retried `close()` skipped the join, drained
/// an empty log, took the empty-log shortcut and returned `Ok(())` over a tail
/// that was neither durable nor anywhere [`Drop`]'s R2-2 warning could see it
/// (the log was empty because the zombie held the batch). COH-6 clause 13 — "a
/// retained batch is never silently lost" — by the same route.
///
/// So a handle is never a bare local either. This guard owns it from the take
/// until [`HandleCustody::join`] sees the task actually finish, and its `Drop`
/// returns an un-reaped handle to its slot. A `JoinHandle` whose poll was
/// cancelled is re-awaitable, so the retry re-joins *that* task, waits out its
/// in-flight attempt and collects its `requeue_pending` (COH-6): the tail is
/// back on the log before step 3 drains, and the empty-log shortcut is never
/// reached with a live flush task behind it.
///
/// **All three handles, not only the flush one.** The daemon and canonization
/// handles are `abort()`ed before their join, and `abort()` is a synchronous
/// fire — cancellation cannot land between the take and the abort, because
/// there is no await between them. What the abort does *not* buy is that the
/// task has stopped: tokio cancels an already-running task at its next
/// `.await`, so an aborted producer can still finish a synchronous stretch, and
/// that stretch can append to the graph log. Only the join proves it is over.
/// Detached at its join, such a task is left running while the retry goes
/// straight to the drain — the same `Ok(())`-over-a-lost-mutation shape as the
/// flush case, through a narrower window. Same guard, same reason.
///
/// Like `TailCustody`, the `parking_lot` guard is taken for one statement and
/// never across an `.await` (§6.4): `join` holds nothing while it waits.
///
/// **Composition with `TailCustody`.** `close()` drops each of these
/// explicitly once its join has returned, so at most one custody guard is ever
/// live and the two never overlap: a cancellation at step 2 restores a handle
/// and no tail exists yet; a cancellation at step 4 restores the tail and every
/// handle is already reaped. Both orders end the same way — every guard is a
/// local declared *after* `_quiesced` and the `close_state` guard, so both run
/// before the retry can enter `close()` at all. R2-1's rule ("the tail is back
/// on the log before `close_state` releases") is unchanged, and R3-1's is its
/// twin one step earlier.
struct HandleCustody<'a> {
    slot: &'a PlMutex<Option<JoinHandle<()>>>,
    /// `None` once [`HandleCustody::join`] has reaped the task — that is what
    /// tells `Drop` there is nothing to hand back.
    handle: Option<JoinHandle<()>>,
}

impl<'a> HandleCustody<'a> {
    /// Lift the handle out of `slot`. The slot stays empty only for as long as
    /// this guard lives.
    fn take(slot: &'a PlMutex<Option<JoinHandle<()>>>) -> Self {
        let handle = slot.lock().take();
        Self { slot, handle }
    }

    /// Signal cancellation. Synchronous, so no cancellation of `close()` can
    /// land between this and the [`HandleCustody::join`] that follows it.
    fn abort(&self) {
        if let Some(handle) = &self.handle {
            handle.abort();
        }
    }

    /// Wait for the task to finish; `None` if the slot was already empty (an
    /// earlier `close()` reaped it).
    ///
    /// Custody ends only when the join **returns**. Cancelled mid-poll, the
    /// handle is still owned here and `Drop` puts it back.
    async fn join(&mut self) -> Option<Result<(), tokio::task::JoinError>> {
        let outcome = self.handle.as_mut()?.await;
        self.handle = None;
        Some(outcome)
    }
}

impl Drop for HandleCustody<'_> {
    fn drop(&mut self) {
        if let Some(handle) = self.handle.take() {
            *self.slot.lock() = Some(handle);
        }
    }
}

/// Custody of the tail between `close()`'s drain (step 3) and the moment it is
/// durable (step 4) — R2-1.
///
/// Between those two points the mutations exist **only** as a local inside
/// `close()`: they are out of the graph log and the flush task that owned the
/// other copy has already been joined. `close()` is an ordinary future, so a
/// caller that wraps it in `tokio::time::timeout` or drops it out of a
/// `select!` — the posture `close`'s own "How long it can take" section invites,
/// and which this crate's own shutdown test uses — destroys that local mid-flush.
/// The tail then existed nowhere: the log was empty, so the *next* `close()`
/// drained nothing, took the empty-log shortcut and returned `Ok(())` over
/// mutations nobody ever wrote.
///
/// So the batch is never a bare local. This guard owns it from the drain until
/// [`TailCustody::durable`] is called, and its `Drop` — which runs on
/// cancellation exactly as it runs on the error path — hands it back to the
/// front of the log. Cancel a `close()` and the tail is where it started, for
/// the retry (or for `Drop`'s R2-2 warning) to find.
///
/// `Drop` is synchronous and takes the `parking_lot` write lock for one
/// statement, never across an `.await` (§6.4). Re-appending a batch whose flush
/// may have partly landed is the same bet the failure path already makes: a
/// mutation batch is replayed, and replay is idempotent by the `src/graph/mod.rs`
/// contract.
struct TailCustody<'a> {
    graph: &'a RwLock<Graph>,
    batch: MutationBatch,
    /// Set by [`TailCustody::durable`]; suppresses the hand-back.
    durable: bool,
}

impl<'a> TailCustody<'a> {
    fn new(graph: &'a RwLock<Graph>, batch: MutationBatch) -> Self {
        Self {
            graph,
            batch,
            durable: false,
        }
    }

    fn batch(&self) -> &MutationBatch {
        &self.batch
    }

    fn len(&self) -> usize {
        self.batch.len()
    }

    fn is_empty(&self) -> bool {
        self.batch.is_empty()
    }

    /// The store took it: end custody, so `Drop` does not put a durable batch
    /// back on the log (which would flush it twice and leave `log_depth`
    /// claiming an undurable tail).
    fn durable(&mut self) {
        self.durable = true;
    }
}

impl Drop for TailCustody<'_> {
    fn drop(&mut self) {
        if self.durable {
            return;
        }
        // `push_front_log` is a no-op on an empty batch, so the empty-log and
        // degraded-with-empty-log paths cost nothing here.
        self.graph
            .write()
            .push_front_log(std::mem::take(&mut self.batch.mutations));
    }
}

/// `close()`'s step-4 store attempt, armored exactly like a background one
/// (T81-2).
///
/// The flush loop protects every `store.flush` twice — `FLUSH_ATTEMPT_TIMEOUT`
/// (STORE-2) and [`CatchUnwindPoll`] — and both rationales apply verbatim to the
/// final flush, which runs against the same caller-supplied adapter:
///
/// * **Timeout.** Without it a hung store hangs `close()` forever, and the
///   handle's tail is stuck behind a call that will never return. The same
///   constant, not a close-specific one: this is one `store.flush` attempt on
///   the same store, so the bound STORE-2 chose for an attempt is the bound
///   here. `close` makes exactly one attempt (no retry ladder), so 30s is the
///   whole of it.
/// * **Panic containment.** Without it a panicking adapter unwinds out of
///   `close` *after* `closed` latched and the log drained — the tail would be
///   unrecoverable even for a caller that catches the panic. Contained, it is
///   an ordinary error and the caller's batch goes back on the log.
///
/// Dropping the timed-out future is safe for the same reason it is in the loop:
/// the adapter only borrows `&MutationBatch`, which the caller still owns.
async fn final_flush(
    store: &dyn GraphStore,
    batch: &MutationBatch,
    token: Option<u64>,
) -> Result<(), StoreError> {
    let attempt = async {
        match CatchUnwindPoll(async { store.flush(batch, token).await }).await {
            Ok(result) => result,
            Err(payload) => {
                let message = panic_message(&payload);
                tracing::error!(
                    panic = %message,
                    "close: store.flush panicked during the final flush; treating it as a failed \
                     flush (the tail returns to the graph log)"
                );
                Err(StoreError::Backend(format!(
                    "close: store flush panicked: {message}"
                )))
            }
        }
    };
    match tokio::time::timeout(FLUSH_ATTEMPT_TIMEOUT, attempt).await {
        Ok(result) => result,
        Err(_elapsed) => Err(StoreError::Backend(format!(
            "close: store flush timed out after {FLUSH_ATTEMPT_TIMEOUT:?}"
        ))),
    }
}

/// Resolve a caller-supplied string to a concept id.
///
/// Canonicalization first (so synonyms and casing work), then an exact
/// `content` match — the fallback is what makes demoted `Observation`s
/// reachable, since canonicalization's match step skips them by design.
/// The fallback picks the lowest id among equal matches so the choice is
/// deterministic rather than `HashMap`-iteration dependent.
fn resolve_concept(graph: &Graph, target: &str) -> Result<NodeId, LamboError> {
    if let CanonicalizeResult::Matched { node, .. } = canonicalize(target, graph)? {
        return Ok(node);
    }
    let exact: Option<&Concept> = graph
        .concepts()
        .filter(|c| c.content == target)
        .min_by_key(|c| c.id.0);
    match exact {
        Some(c) => Ok(c.id),
        None => Err(LamboError::Store(StoreError::NotFound(format!(
            "no concept matching {target:?} in session {}",
            graph.session_id()
        )))),
    }
}

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests {
    use super::*;
    use crate::embed::FixtureEmbedder;
    use crate::store::MemoryStore;
    use crate::test_util::capture_logs;
    use crate::types::{
        CanonizationEvent, GraphSnapshot, InteractionSpan, Mutation, MutationBatch, Scored,
        StoreError,
    };
    use async_trait::async_trait;
    use std::collections::HashSet;
    use std::sync::atomic::AtomicUsize;

    fn contract(kind: &str, dim: usize) -> EmbeddingContract {
        EmbeddingContract {
            kind: kind.into(),
            model: None,
            dim,
        }
    }

    async fn memory_on(store: Arc<dyn GraphStore>, session: &str) -> Memory {
        Memory::builder()
            .session(session)
            .agent("agent-a")
            // A long flush interval keeps the background loop out of every
            // assertion: `close()` is what must make the tail durable.
            .flush_interval(Duration::from_secs(3_600))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .expect("build")
    }

    /// `GraphStore` that fails the first `fail_next` flushes (or every flush,
    /// with `fail_next = usize::MAX`), so a batch is RETAINED inside the flush
    /// task's pending buffer. Records every batch length it was handed.
    struct FlakyStore {
        inner: Arc<dyn GraphStore>,
        fail_remaining: AtomicUsize,
        /// Every batch it was handed, whole: the T81-3 assertions need the
        /// mutation **sequence**, not just the lengths.
        batches: PlMutex<Vec<MutationBatch>>,
    }

    impl FlakyStore {
        fn new(inner: Arc<dyn GraphStore>, fail_next: usize) -> Self {
            Self {
                inner,
                fail_remaining: AtomicUsize::new(fail_next),
                batches: PlMutex::new(Vec::new()),
            }
        }

        fn batches(&self) -> Vec<MutationBatch> {
            self.batches.lock().clone()
        }

        fn batch_lens(&self) -> Vec<usize> {
            self.batches.lock().iter().map(|b| b.len()).collect()
        }
    }

    #[async_trait]
    impl GraphStore for FlakyStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            self.batches.lock().push(batch.clone());
            let should_fail = self
                .fail_remaining
                .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |n| {
                    (n > 0).then(|| n - 1)
                })
                .is_ok();
            if should_fail {
                Err(StoreError::Backend("simulated outage".into()))
            } else {
                self.inner.flush(batch, token).await
            }
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }
        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates(session, embedding, limit)
                .await
        }
        async fn vector_candidates_checked(
            &self,
            session: &SessionId,
            embedding: &[f32],
            expected_contract: &EmbeddingContract,
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates_checked(session, embedding, expected_contract, limit)
                .await
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.inner
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
            self.inner
                .interaction_span(session, node, min_age, now)
                .await
        }
        async fn record_canonization(
            &self,
            event: &CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.inner.record_canonization(event, token).await
        }
    }

    /// How a store charges for a flush (L82-1).
    #[derive(Clone, Copy, Debug, PartialEq)]
    enum CostModel {
        /// What both SQL adapters did before L82-1: one statement, and so one
        /// network round-trip, per mutation in the batch.
        PerMutation,
        /// What they do now: one statement per planned [`FlushStep`].
        PerPlannedStatement,
    }

    /// A store that charges network latency for a flush, so a test can ask
    /// whether a tail drains inside `close()`'s window (L82-1).
    ///
    /// The `+ 2` on every count is `BEGIN` and `COMMIT`, which both adapters pay
    /// once per flush regardless of the batch.
    struct RoundTripStore {
        inner: Arc<dyn GraphStore>,
        model: CostModel,
        /// Per-statement round-trip. The live cluster (CockroachDB serverless,
        /// GCP asia-south1) measured 10–30 ms.
        rtt: Duration,
        round_trips: AtomicUsize,
        released: AtomicUsize,
    }

    impl RoundTripStore {
        fn new(inner: Arc<dyn GraphStore>, model: CostModel, rtt: Duration) -> Self {
            Self {
                inner,
                model,
                rtt,
                round_trips: AtomicUsize::new(0),
                released: AtomicUsize::new(0),
            }
        }

        fn round_trips(&self) -> usize {
            self.round_trips.load(Ordering::SeqCst)
        }

        fn releases(&self) -> usize {
            self.released.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl GraphStore for RoundTripStore {
        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            let statements = match self.model {
                CostModel::PerMutation => batch.mutations.len(),
                CostModel::PerPlannedStatement => crate::store::batch::planned_statements(
                    &batch.mutations,
                    crate::store::batch::BulkLimits {
                        interactions: 1,
                        concepts: 256,
                        edges: 512,
                    },
                ),
            } + 2;
            self.round_trips.fetch_add(statements, Ordering::SeqCst);
            tokio::time::sleep(self.rtt * u32::try_from(statements).unwrap()).await;
            self.inner.flush(batch, token).await
        }
        async fn release_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
        ) -> Result<(), StoreError> {
            self.released.fetch_add(1, Ordering::SeqCst);
            self.inner.release_lease(session, holder).await
        }
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }
        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates(session, embedding, limit)
                .await
        }
        async fn vector_candidates_checked(
            &self,
            session: &SessionId,
            embedding: &[f32],
            expected_contract: &EmbeddingContract,
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates_checked(session, embedding, expected_contract, limit)
                .await
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.inner
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
            self.inner
                .interaction_span(session, node, min_age, now)
                .await
        }
        async fn record_canonization(
            &self,
            event: &CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.inner.record_canonization(event, token).await
        }
    }

    /// Four `record_action` calls at the 64-target fan-out cap — the live L82-1
    /// repro's burst — left un-flushed in the log.
    async fn at_cap_burst(mem: &Memory) {
        for call in 0..4 {
            let produces: Vec<String> = (0..32).map(|n| format!("artifact {call}-{n}")).collect();
            let depends_on: Vec<String> =
                (0..32).map(|n| format!("dependency {call}-{n}")).collect();
            let produces_refs: Vec<&str> = produces.iter().map(String::as_str).collect();
            let depends_refs: Vec<&str> = depends_on.iter().map(String::as_str).collect();
            mem.record_action(&Action {
                action: &format!("burst action {call}"),
                produces: &produces_refs,
                modifies: &[],
                depends_on: &depends_refs,
            })
            .expect("at-cap record_action");
        }
    }

    /// **L82-1, end to end.** A burst left un-flushed at SIGTERM must drain
    /// inside `close()`'s window against a store that charges a real cluster's
    /// per-statement latency.
    ///
    /// Both halves run so the test *shows* the finding as well as the fix. With
    /// the pre-L82-1 cost model — one round-trip per mutation, which is what
    /// both adapters' `for m in &batch.mutations` loop bought — the close blows
    /// `CLOSE_FLUSH_GRACE` exactly as the live run did. With the planned-
    /// statement model it finishes in a fraction of it.
    ///
    /// The cost model is `store::batch`'s own plan, so this test pins the
    /// *arithmetic*, not the adapters' use of it: `store::batch`'s tests pin
    /// that the plan is small, and `cockroach::sql_shape_is_a_multi_row_upsert`
    /// pins that one planned step really is one statement. The three together
    /// are what close the loop — no local test can reach a cluster.
    ///
    /// Time is paused, so the sleeps are simulated and the test is instant.
    #[tokio::test(start_paused = true)]
    async fn an_at_cap_burst_drains_within_the_close_window() {
        let rtt = Duration::from_millis(30);

        // The regression, reproduced: per-mutation round-trips cannot drain.
        let slow = Arc::new(RoundTripStore::new(
            Arc::new(MemoryStore::new()),
            CostModel::PerMutation,
            rtt,
        ));
        let mem = memory_on(slow.clone(), "l82-1-per-mutation").await;
        at_cap_burst(&mem).await;
        let undrained = mem.stats().log_depth;
        assert!(
            undrained >= 700,
            "the burst must leave a realistic tail, got {undrained}"
        );
        assert!(
            tokio::time::timeout(crate::mcp::serve::CLOSE_FLUSH_GRACE, mem.close())
                .await
                .is_err(),
            "per-mutation round-trips must NOT fit the close window — if this stops timing out \
             the cost model no longer reflects what the pre-L82-1 adapters did, and the other \
             half of this test proves nothing"
        );

        // The fix: the same tail, planned into statements.
        let fast = Arc::new(RoundTripStore::new(
            Arc::new(MemoryStore::new()),
            CostModel::PerPlannedStatement,
            rtt,
        ));
        let mem = memory_on(fast.clone(), "l82-1-planned").await;
        at_cap_burst(&mem).await;
        let start = tokio::time::Instant::now();
        tokio::time::timeout(crate::mcp::serve::CLOSE_FLUSH_GRACE, mem.close())
            .await
            .expect("close must fit the grace window")
            .expect("close must succeed");
        let elapsed = start.elapsed();
        assert!(
            elapsed < Duration::from_secs(1),
            "the tail drained in {elapsed:?}; it must be a handful of round-trips, not a \
             close call against the window"
        );
        assert!(
            fast.round_trips() < 40,
            "the whole session cost {} round-trips — the burst must plan into statements, not \
             rows (L82-1)",
            fast.round_trips()
        );
    }

    /// **L82-1, the second failure.** `serve` bounds `close()` and *drops* it on
    /// timeout, so the release on close's success path never runs — the live run
    /// exited with a stale lease row and wedged the session for the whole
    /// `LEASE_TTL` on top of losing the tail.
    ///
    /// Releasing is honest here: it asserts this process is gone, which it is.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_close_still_releases_the_lease() {
        let store = Arc::new(RoundTripStore::new(
            Arc::new(MemoryStore::new()),
            CostModel::PerMutation,
            Duration::from_millis(30),
        ));
        let mem = memory_on(store.clone(), "l82-1-stale-lease").await;
        at_cap_burst(&mem).await;

        assert!(
            tokio::time::timeout(crate::mcp::serve::CLOSE_FLUSH_GRACE, mem.close())
                .await
                .is_err(),
            "this test needs the close to be abandoned"
        );
        assert_eq!(
            store.releases(),
            0,
            "an abandoned close cannot have released on its own — that is the bug"
        );

        mem.release_lease_after_abandoned_close().await;
        assert_eq!(
            store.releases(),
            1,
            "the lease must be released on the way out even though the tail was lost"
        );

        // Idempotent: `serve` may reach this after a close that already
        // released, and a second holder-scoped DELETE is a wasted round-trip at
        // best and a race at worst.
        mem.release_lease_after_abandoned_close().await;
        assert_eq!(store.releases(), 1, "the release must happen at most once");
    }

    /// **L82-1, the wiring.** The two halves above are the pieces; this is
    /// `serve`'s actual shutdown path putting them together.
    ///
    /// It drives `close_bounded_until` — the real body of the bounded close,
    /// with only the re-armed signal substituted (`shutdown_signal()` installs
    /// process-wide SIGINT/SIGTERM handlers, which a test must not do to the
    /// whole binary). A slow store makes the close blow its window, and the
    /// lease must be gone anyway.
    #[tokio::test(start_paused = true)]
    async fn an_abandoned_close_releases_the_lease_through_serve() {
        let store = Arc::new(RoundTripStore::new(
            Arc::new(MemoryStore::new()),
            CostModel::PerMutation,
            Duration::from_millis(30),
        ));
        let mem = memory_on(store.clone(), "l82-1-serve-wiring").await;
        at_cap_burst(&mem).await;

        let err = crate::mcp::serve::close_bounded_until(&mem, std::future::pending())
            .await
            .expect_err("the close must be abandoned for this test to mean anything");
        assert!(
            err.to_string().contains("not durable"),
            "the error must still say the tail was lost: {err}"
        );
        assert_eq!(
            store.releases(),
            1,
            "serve must release the lease on the abandoned-close path — leaving it stale wedges \
             the session for the whole LEASE_TTL (L82-1)"
        );
    }

    /// The happy path must not gain a second release: `close()` already handed
    /// the lease off, and a redundant holder-scoped DELETE is a wasted
    /// round-trip on the way out.
    #[tokio::test(start_paused = true)]
    async fn a_close_that_finishes_releases_exactly_once_through_serve() {
        let store = Arc::new(RoundTripStore::new(
            Arc::new(MemoryStore::new()),
            CostModel::PerPlannedStatement,
            Duration::from_millis(30),
        ));
        let mem = memory_on(store.clone(), "l82-1-serve-happy").await;
        at_cap_burst(&mem).await;

        crate::mcp::serve::close_bounded_until(&mem, std::future::pending())
            .await
            .expect("the burst must drain inside the window now");
        assert_eq!(store.releases(), 1, "released once, by close() itself");
    }

    /// A fenced handle must NOT release: the lease belongs to whoever took the
    /// session over, and `close()`'s own fenced branch has the same rule.
    #[tokio::test(start_paused = true)]
    async fn a_fenced_handle_does_not_release_on_an_abandoned_close() {
        let store = Arc::new(RoundTripStore::new(
            Arc::new(MemoryStore::new()),
            CostModel::PerPlannedStatement,
            Duration::from_millis(1),
        ));
        let mem = memory_on(store.clone(), "l82-1-fenced").await;
        mem.simulate_lease_loss();

        mem.release_lease_after_abandoned_close().await;
        assert_eq!(
            store.releases(),
            0,
            "a fenced handle must not evict the writer that took the session over"
        );
    }

    /// How [`AdverseStore::flush`] misbehaves — the two failure modes the
    /// background flush path is armored against (STORE-2 timeout + panic
    /// containment) plus a plain delay for the concurrency tests.
    #[derive(Clone, Copy, Debug)]
    enum FlushBehaviour {
        /// Never returns: the hung backend `FLUSH_ATTEMPT_TIMEOUT` exists for.
        Hang,
        /// Hangs the **first** flush and delegates every later one: a store
        /// that is unresponsive when the caller gives up on `close()` and
        /// healthy when it retries (R2-1).
        HangOnce,
        /// Unwinds inside the flush: the panicking adapter `CatchUnwindPoll`
        /// exists for.
        Panic,
        /// Succeeds, slowly.
        Delay(Duration),
    }

    /// Store double for `close()`'s step-4 armor (T81-2) and for the concurrent
    /// -close test (T81-6). Everything except `flush` delegates.
    struct AdverseStore {
        inner: Arc<dyn GraphStore>,
        behaviour: FlushBehaviour,
        flush_calls: AtomicUsize,
        flush_completed: AtomicBool,
        /// Armed for [`FlushBehaviour::HangOnce`]; disarmed by the first flush.
        hang_armed: AtomicBool,
    }

    impl AdverseStore {
        fn new(inner: Arc<dyn GraphStore>, behaviour: FlushBehaviour) -> Self {
            Self {
                inner,
                behaviour,
                flush_calls: AtomicUsize::new(0),
                flush_completed: AtomicBool::new(false),
                hang_armed: AtomicBool::new(true),
            }
        }

        fn flush_calls(&self) -> usize {
            self.flush_calls.load(Ordering::SeqCst)
        }

        /// `true` once a `flush` has actually returned from the backend.
        fn flush_completed(&self) -> bool {
            self.flush_completed.load(Ordering::SeqCst)
        }
    }

    #[async_trait]
    impl GraphStore for AdverseStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities()
        }
        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            self.flush_calls.fetch_add(1, Ordering::SeqCst);
            match self.behaviour {
                FlushBehaviour::Hang => std::future::pending::<()>().await,
                FlushBehaviour::HangOnce => {
                    if self.hang_armed.swap(false, Ordering::SeqCst) {
                        std::future::pending::<()>().await
                    }
                }
                FlushBehaviour::Panic => panic!("store adapter exploded mid-flush"),
                FlushBehaviour::Delay(d) => tokio::time::sleep(d).await,
            }
            let result = self.inner.flush(batch, token).await;
            self.flush_completed.store(true, Ordering::SeqCst);
            result
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }
        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates(session, embedding, limit)
                .await
        }
        async fn vector_candidates_checked(
            &self,
            session: &SessionId,
            embedding: &[f32],
            expected_contract: &EmbeddingContract,
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates_checked(session, embedding, expected_contract, limit)
                .await
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.inner
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
            self.inner
                .interaction_span(session, node, min_age, now)
                .await
        }
        async fn record_canonization(
            &self,
            event: &CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.inner.record_canonization(event, token).await
        }
    }

    /// Which store call [`ParkingStore`] suspends (once) until released.
    #[derive(Clone, Copy, PartialEq, Eq, Debug)]
    enum ParkPoint {
        /// `retract`'s durable-radius query — the await inside a **write**,
        /// which is where the T81-1 race lives.
        BlastRadius,
        /// `recall`'s vector leg — the await inside a **read**, to show that
        /// `close()` does not wait for readers.
        VectorCandidates,
    }

    /// Delegating store that parks the **first** call to one chosen method on a
    /// `Notify` and reports (on another `Notify`) that it got there. The
    /// reviewer's deterministic race probe, kept as a fixture.
    struct ParkingStore {
        inner: Arc<dyn GraphStore>,
        park_on: ParkPoint,
        armed: AtomicBool,
        entered: Arc<tokio::sync::Notify>,
        release: Arc<tokio::sync::Notify>,
    }

    impl ParkingStore {
        fn new(inner: Arc<dyn GraphStore>, park_on: ParkPoint) -> Self {
            Self {
                inner,
                park_on,
                armed: AtomicBool::new(true),
                entered: Arc::new(tokio::sync::Notify::new()),
                release: Arc::new(tokio::sync::Notify::new()),
            }
        }

        /// Notified once the parked call is suspended. `notify_one` latches, so
        /// awaiting this after the fact still works.
        fn entered(&self) -> Arc<tokio::sync::Notify> {
            self.entered.clone()
        }

        fn release(&self) {
            self.release.notify_one();
        }

        async fn park(&self, point: ParkPoint) {
            if point != self.park_on || !self.armed.swap(false, Ordering::SeqCst) {
                return;
            }
            self.entered.notify_one();
            self.release.notified().await;
        }
    }

    #[async_trait]
    impl GraphStore for ParkingStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            // Claimed so `recall` actually takes its vector leg (MemoryStore
            // itself has none); the leg then fails and recall degrades, which
            // is fine — the park is what the test needs.
            match self.park_on {
                ParkPoint::VectorCandidates => {
                    self.inner.capabilities() | Capabilities::VECTOR_SEARCH
                }
                ParkPoint::BlastRadius => self.inner.capabilities(),
            }
        }
        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            self.inner.flush(batch, token).await
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }
        async fn vector_candidates(
            &self,
            session: &SessionId,
            embedding: &[f32],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner
                .vector_candidates(session, embedding, limit)
                .await
        }
        async fn vector_candidates_checked(
            &self,
            session: &SessionId,
            embedding: &[f32],
            expected_contract: &EmbeddingContract,
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.park(ParkPoint::VectorCandidates).await;
            self.inner
                .vector_candidates_checked(session, embedding, expected_contract, limit)
                .await
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.park(ParkPoint::BlastRadius).await;
            self.inner
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
            self.inner
                .interaction_span(session, node, min_age, now)
                .await
        }
        async fn record_canonization(
            &self,
            event: &CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.inner.record_canonization(event, token).await
        }
    }

    /// `MemoryStore` plus a **real** vector leg: exact cosine over the
    /// embeddings that actually survived the write-behind flush. It is the
    /// in-process stand-in for Cockroach's `concepts_embedding_idx` (the only
    /// adapter that advertises `VECTOR_SEARCH`), which is what lets a default
    /// `cargo test` assert L82-4 end to end — derive → flush → vector recall on
    /// organically-derived data — with no live cluster. Every answer it gives
    /// is recorded so a test can prove the vector leg *fired* rather than
    /// inferring it from a rank.
    struct VectorSearchStore {
        inner: Arc<dyn GraphStore>,
        answers: PlMutex<Vec<Vec<Scored<NodeId>>>>,
    }

    impl VectorSearchStore {
        fn new(inner: Arc<dyn GraphStore>) -> Self {
            Self {
                inner,
                answers: PlMutex::new(Vec::new()),
            }
        }

        /// Every `vector_candidates` answer, in call order.
        fn answers(&self) -> Vec<Vec<Scored<NodeId>>> {
            self.answers.lock().clone()
        }
    }

    #[async_trait]
    impl GraphStore for VectorSearchStore {
        async fn init_schema(&self) -> Result<(), StoreError> {
            self.inner.init_schema().await
        }
        fn capabilities(&self) -> Capabilities {
            self.inner.capabilities() | Capabilities::VECTOR_SEARCH
        }
        fn vector_dimensions(&self) -> Option<usize> {
            Some(1024)
        }
        async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
            self.inner.flush(batch, token).await
        }
        async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
            self.inner.load_session(session).await
        }
        async fn keyword_candidates(
            &self,
            session: &SessionId,
            tokens: &[String],
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            self.inner.keyword_candidates(session, tokens, limit).await
        }
        async fn vector_candidates(
            &self,
            _session: &SessionId,
            _embedding: &[f32],
            _limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            panic!("VectorSearchStore: unchecked vector lookup")
        }
        async fn vector_candidates_checked(
            &self,
            session: &SessionId,
            embedding: &[f32],
            expected_contract: &EmbeddingContract,
            limit: usize,
        ) -> Result<Vec<Scored<NodeId>>, StoreError> {
            crate::store::validate_vector_candidate_limit(limit)?;
            // A session with nothing flushed yet is an EMPTY candidate pool, not
            // an error — the shape Cockroach returns for an unstamped session
            // before the first commit.
            let snapshot = match self.inner.load_session(session).await {
                Ok(snapshot) => snapshot,
                Err(StoreError::SessionNotFound(_)) => {
                    self.answers.lock().push(Vec::new());
                    return Ok(Vec::new());
                }
                Err(e) => return Err(e),
            };
            // H1/E2E-1: like Cockroach's checked read, bind the query's
            // expected contract to the DURABLE contract and refuse a change —
            // never silently answer with vectors the caller cannot interpret.
            // (Legacy unstamped vectors are quarantined at load, so an
            // unrecorded durable contract is an empty pool.)
            match &snapshot.embedding {
                None => {
                    self.answers.lock().push(Vec::new());
                    return Ok(Vec::new());
                }
                Some(durable) if durable == expected_contract => {}
                Some(durable) => {
                    return Err(StoreError::Invariant(format!(
                        "vector candidate lookup refused after embedding contract changed: \
                         vectors were written by kind={} model={:?} dim={}, but the live/attached \
                         embedder is kind={} model={:?} dim={} — re-embed or start a new session",
                        durable.kind,
                        durable.model,
                        durable.dim,
                        expected_contract.kind,
                        expected_contract.model,
                        expected_contract.dim,
                    )));
                }
            }
            let mut scored: Vec<Scored<NodeId>> = snapshot
                .concepts
                .iter()
                .filter_map(|c| {
                    let vector = c.embedding.as_ref()?;
                    Some(Scored::new(
                        c.id,
                        f64::from(crate::embed::cosine(embedding, vector)),
                    ))
                })
                .collect();
            // Same ordering contract as the real adapter: best first, ties
            // broken by the smaller UUID so the answer is deterministic.
            scored.sort_by(|a, b| {
                b.score
                    .total_cmp(&a.score)
                    .then_with(|| a.item.0.cmp(&b.item.0))
            });
            scored.truncate(limit);
            self.answers.lock().push(scored.clone());
            Ok(scored)
        }
        async fn blast_radius(
            &self,
            session: &SessionId,
            node: NodeId,
            min_edge_age: Duration,
            now: DateTime<Utc>,
        ) -> Result<u64, StoreError> {
            self.inner
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
            self.inner
                .interaction_span(session, node, min_age, now)
                .await
        }
        async fn record_canonization(
            &self,
            event: &CanonizationEvent,
            token: Option<u64>,
        ) -> Result<(), StoreError> {
            self.inner.record_canonization(event, token).await
        }
        async fn acquire_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
            ttl: Duration,
        ) -> Result<LeaseOutcome, StoreError> {
            self.inner.acquire_lease(session, holder, ttl).await
        }
        async fn refresh_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
            ttl: Duration,
        ) -> Result<LeaseOutcome, StoreError> {
            self.inner.refresh_lease(session, holder, ttl).await
        }
        async fn release_lease(
            &self,
            session: &SessionId,
            holder: &LeaseHolder,
        ) -> Result<(), StoreError> {
            self.inner.release_lease(session, holder).await
        }
    }

    /// `FixtureEmbedder` is hash-seeded per exact phrase, which cannot model the
    /// one thing an organic vector-recall test needs: hybrid embeds a concept
    /// **with** its origin context (`"register user — <prompt>"`, the PHASE-7
    /// calibration rule) while recall embeds the **bare** query
    /// (`"create account"`), and a real semantic embedder still scores those two
    /// as near. This wrapper reduces the context framing back to the concept
    /// label before delegating — behaviour BGE-M3 has for free and a hash
    /// fixture cannot. Nothing else about the embedding path is altered.
    #[derive(Debug)]
    struct ContextTolerantEmbedder(FixtureEmbedder);

    #[async_trait]
    impl Embedder for ContextTolerantEmbedder {
        fn dimensions(&self) -> usize {
            self.0.dimensions()
        }
        async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
            let label = text
                .strip_prefix("Concept: ")
                .unwrap_or(text)
                .split(" — ")
                .next()
                .unwrap_or(text);
            self.0.embed(label).await
        }
    }

    // -- build / Level B ----------------------------------------------------

    #[tokio::test]
    async fn build_requires_session_agent_store_embedder_and_contract() {
        let err = Memory::builder().build().await.unwrap_err();
        assert!(err.to_string().contains("session"), "{err}");

        let err = Memory::builder().session("s").build().await.unwrap_err();
        assert!(err.to_string().contains("agent"), "{err}");

        let err = Memory::builder()
            .session("s")
            .agent("a")
            .build()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("store"), "{err}");

        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let err = Memory::builder()
            .session("s")
            .agent("a")
            .store(store.clone())
            .build()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("embedder"), "{err}");

        let err = Memory::builder()
            .session("s")
            .agent("a")
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .build()
            .await
            .unwrap_err();
        assert!(err.to_string().contains("embedding_contract"), "{err}");
    }

    #[tokio::test]
    async fn fresh_session_is_stamped_with_the_live_embedding_contract() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "stamp-me").await;
        let stamped = mem.graph().read().embedding().cloned();
        assert_eq!(stamped, Some(contract("fixture", 1024)));
        mem.close().await.unwrap();
    }

    /// STORE-1 / Level B: attaching with a different embedder kind, model or
    /// dim than the session was written with must refuse, not mix vector spaces.
    #[tokio::test]
    async fn session_attach_rejects_embedder_kind_model_and_dim_mismatch() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());

        // Write a session stamped `fixture` / dim 1024.
        let mem = memory_on(store.clone(), "contracted").await;
        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        mem.close().await.unwrap();

        let attach = |c: EmbeddingContract| {
            let store = store.clone();
            async move {
                Memory::builder()
                    .session("contracted")
                    .agent("agent-b")
                    .store(store)
                    .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                    .embedding_contract(c)
                    .build()
                    .await
            }
        };

        let err = attach(contract("bge_m3", 1024)).await.unwrap_err();
        assert!(err.to_string().contains("kind"), "{err}");

        let err = attach(contract("fixture", 512)).await.unwrap_err();
        assert!(err.to_string().contains("dim"), "{err}");

        let err = attach(EmbeddingContract {
            kind: "fixture".into(),
            model: Some("other.gguf".into()),
            dim: 1024,
        })
        .await
        .unwrap_err();
        assert!(err.to_string().contains("model"), "{err}");

        // The matching contract still attaches.
        let ok = attach(contract("fixture", 1024)).await.unwrap();
        ok.close().await.unwrap();
    }

    #[tokio::test]
    async fn h1_same_width_model_change_refuses_by_default_and_explicit_override_persists() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let old = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-model-v1".into()),
            dim: 1024,
        };
        let live = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-model-renamed".into()),
            dim: 1024,
        };

        let first = Memory::builder()
            .session("h1-model-rename")
            .agent("operator")
            .store(store.clone())
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(old)
            .flush_interval(Duration::from_secs(3_600))
            .build()
            .await
            .unwrap();
        first.close().await.unwrap();

        let attach = |allow| {
            let store = store.clone();
            let live = live.clone();
            async move {
                Memory::builder()
                    .session("h1-model-rename")
                    .agent("operator")
                    .store(store)
                    .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                    .embedding_contract(live)
                    .allow_embedding_mismatch(allow)
                    .flush_interval(Duration::from_secs(3_600))
                    .build()
                    .await
            }
        };

        let err = attach(false).await.unwrap_err();
        let text = err.to_string();
        assert!(text.contains("fixture-model-v1"), "{text}");
        assert!(text.contains("fixture-model-renamed"), "{text}");
        assert!(text.contains("--allow-embedding-mismatch"), "{text}");

        let migrated = attach(true).await.unwrap();
        assert_eq!(migrated.embedding_contract(), &live);
        migrated.close().await.unwrap();
        let stored = store
            .load_session(&SessionId::new("h1-model-rename"))
            .await
            .unwrap();
        assert_eq!(stored.embedding, Some(live));
    }

    fn live_handles(session: &str) -> usize {
        ACTIVE_SESSIONS
            .lock()
            .get(&SessionId::new(session))
            .map(|agents| agents.len())
            .unwrap_or(0)
    }

    /// T81-8, retargeted for T8.6: the in-process `ACTIVE_SESSIONS` advisory
    /// still reports a second same-process handle loudly, with both agent ids,
    /// and releases the registration on drop — including a handle that was never
    /// closed.
    ///
    /// **Two separate stores on one logical session, on purpose.** Post-T8.6 the
    /// store lease *refuses* a second writer that shares a store (see
    /// `a_second_writer_sharing_a_store_is_refused_by_the_lease`). The advisory
    /// log's remaining domain is the collision the per-store lease cannot see:
    /// two writers that opened *different* store handles onto the same session
    /// (different `MemoryStore` instances here; different processes/hosts in
    /// production). Each acquires its own store's free lease, so both builds
    /// succeed — and the process-global registry is the only thing that catches
    /// them. That is exactly why the advisory log is kept, not replaced.
    #[tokio::test]
    async fn a_second_handle_on_one_session_is_reported_loudly() {
        let (logs, _guard) = capture_logs(tracing::Level::ERROR);

        // Distinct stores: the per-store lease cannot see across them, so this
        // isolates the process-global advisory (which can).
        let store_a: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store_b: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let first = memory_on(store_a, "one-writer").await;
        assert_eq!(live_handles("one-writer"), 1);
        assert!(
            !logs.contains("SecondSessionWriter"),
            "the first handle is not a collision"
        );

        let second = Memory::builder()
            .session("one-writer")
            .agent("agent-b")
            .flush_interval(Duration::from_secs(3_600))
            .store(store_b)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .unwrap();
        assert_eq!(live_handles("one-writer"), 2, "reported, not refused");

        let logged = logs.contents();
        assert!(logged.contains("SecondSessionWriter"), "{logged}");
        assert!(logged.contains("one-writer"), "{logged}");
        assert!(
            logged.contains("agent-a") && logged.contains("agent-b"),
            "{logged}"
        );

        first.close().await.unwrap();
        assert_eq!(
            live_handles("one-writer"),
            1,
            "a successful close releases its slot without waiting for Drop (R2-4)"
        );
        drop(first);
        assert_eq!(live_handles("one-writer"), 1);
        // Dropped without close(): the registration is still released.
        drop(second);
        assert_eq!(live_handles("one-writer"), 0);
    }

    /// T8.6: a second writer that **shares a store** with the first is now
    /// refused by the store-enforced single-writer lease — the promotion from
    /// advisory to enforced. The refusal names the current holder and its age,
    /// and points at the operator override.
    #[tokio::test]
    async fn a_second_writer_sharing_a_store_is_refused_by_the_lease() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let first = memory_on(store.clone(), "leased").await;

        // Same store, same session, different agent → distinct holder token →
        // the live lease is held by `first`, so this build fails closed.
        let err = Memory::builder()
            .session("leased")
            .agent("agent-b")
            .flush_interval(Duration::from_secs(3_600))
            .store(store.clone())
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .expect_err("a second writer on a shared store must be refused by the lease");
        // Fail closed: refused, and it stays a LamboError::Conflict.
        let LamboError::Conflict(msg) = err else {
            panic!("a shared-store second writer must fail closed as a Conflict, got: {err:?}");
        };
        assert!(msg.contains("single-writer"), "lease still enforced: {msg}");
        // Names the current holder and its age, so an operator can tell who to evict.
        assert!(msg.contains("agent-a"), "names the current holder: {msg}");
        assert!(msg.contains("s ago"), "names the holder's age: {msg}");
        // Surfaces the operator-takeover pointer — deliberately NOT the raw
        // `session_leases` SQL constant, which is no longer part of the
        // user-facing message (that string is intentionally not emitted).
        assert!(
            msg.contains("operator can force a takeover"),
            "surfaces the operator-takeover path: {msg}"
        );
        assert!(
            msg.contains("docs/reference/cli.mdx"),
            "points at the single-writer lease note: {msg}"
        );

        // After a clean close the lease is released, so a new writer attaches.
        first.close().await.unwrap();
        let second = memory_on(store, "leased").await;
        second.close().await.unwrap();
    }

    /// A degenerate cadence must fail `build()` with a `Config` error BEFORE
    /// the single-writer lease is ever acquired, AND without leaking a held
    /// lease. `validate()` runs at the runtime entry (memory.rs build) on the
    /// merged config ahead of `store.acquire_lease`, so a zero cadence can
    /// neither reach a spawned `tokio::interval` nor leave a lease held. The
    /// follow-up build on the same store/session/agent with a valid config
    /// must therefore succeed: a validate-after-lease regression would have
    /// leaked a held lease and wedged it (`acquire_lease` -> `Held`).
    #[tokio::test]
    async fn build_rejects_zero_cadence_before_acquiring_the_lease() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let bad = Config {
            gc_interval: 0,
            ..Config::default()
        };
        let err = Memory::builder()
            .session("bad-cadence")
            .agent("agent-a")
            .config(bad)
            .store(store.clone())
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .expect_err("a zero cadence must fail build()");
        // A Config error — not Conflict/Store/Held — is consistent with validate()
        // rejecting before the lease; the second build below is what proves no
        // lease was leaked.
        assert!(
            matches!(&err, LamboError::Config(_)),
            "must fail closed as a Config error before the lease, got: {err:?}"
        );

        // Follow-up build on the same store/session/agent with a VALID config.
        // If validate()-after-lease had leaked a held lease for this session
        // ("bad-cadence"), acquire_lease would return Held and wedge this
        // build — so its success genuinely proves no lease was leaked.
        let second = Memory::builder()
            .session("bad-cadence")
            .agent("agent-a")
            .config(Config::default())
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .expect("a valid second build must succeed; a leaked lease would wedge it");
        second.close().await.unwrap();
    }

    /// T86-2: a holder that LOSES its lease is FENCED. After the store starves
    /// its heartbeat past the TTL and another writer takes over, this handle must
    /// refuse every further write and must NOT flush or release — otherwise the
    /// two writers flush divergent graphs into one session (the exact split-brain
    /// the lease prevents). Simulates the outage-plus-takeover the heartbeat sees.
    #[tokio::test]
    async fn a_lost_lease_fences_the_writer_and_stops_the_flush() {
        let store = Arc::new(MemoryStore::new());
        let session = SessionId::new("fenced");
        let first = Memory::builder()
            .session("fenced")
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .store(store.clone() as Arc<dyn GraphStore>)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .expect("build");

        // A tail exists but nothing is durable yet (hour-long flush interval).
        first
            .derive(&[("before loss", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        assert!(
            store.load_session(&session).await.is_err(),
            "nothing flushed yet"
        );

        // Store outage past the TTL, then a DIFFERENT holder takes the session
        // over — exactly what the heartbeat's next refresh observes as `Held`.
        store.force_expire_lease(&session);
        let taker = LeaseHolder::for_this_process(&AgentId::new("agent-b"));
        let outcome = store
            .acquire_lease(&session, &taker, LEASE_TTL)
            .await
            .unwrap();
        assert!(
            outcome.is_acquired(),
            "agent-b takes over the expired lease"
        );

        // The heartbeat would latch the fence on its next tick; drive it directly.
        first.simulate_lease_loss();

        // 1. Every further write is refused with the honest lease-lost message.
        let derive_err = first
            .derive(&[("after loss", ConceptType::Entity)], &ParentOf::none())
            .await
            .expect_err("a fenced handle must refuse derive");
        let msg = derive_err.to_string();
        assert!(msg.contains("lost its single-writer lease"), "{msg}");
        assert!(msg.contains("no longer the writer"), "{msg}");

        let action_err = first
            .record_action(&Action {
                action: "write after loss",
                produces: &["x"],
                modifies: &[],
                depends_on: &[],
            })
            .expect_err("a fenced handle must refuse record_action");
        assert!(action_err
            .to_string()
            .contains("lost its single-writer lease"));

        let reserve_err = first
            .reserve(NodeId::new(), Duration::from_secs(60))
            .expect_err("a fenced handle must refuse reserve");
        assert!(reserve_err
            .to_string()
            .contains("lost its single-writer lease"));

        // 2. close() refuses to flush (no overwrite) and does not release.
        let close_err = first
            .close()
            .await
            .expect_err("a fenced close must not flush or release");
        assert!(close_err
            .to_string()
            .contains("lost its single-writer lease"));

        // No flush ever landed: the store has no concepts from `first`.
        assert!(
            store.load_session(&session).await.is_err(),
            "a fenced handle must never persist its tail — the new holder owns the session"
        );

        // The takeover holder still owns the lease: the fenced close did NOT
        // release it (a stale release would evict the new writer). A third,
        // distinct holder is therefore refused.
        let third = store
            .acquire_lease(
                &session,
                &LeaseHolder::for_this_process(&AgentId::new("agent-c")),
                LEASE_TTL,
            )
            .await
            .unwrap();
        assert!(
            !third.is_acquired(),
            "agent-b's lease is intact — the fenced handle must not release it"
        );

        drop(first);
    }

    /// T8.6: exactly one holder and one honest refusal, in-process — the memory
    /// backend's cross-"process" analogue done as two `build`s on one store.
    #[tokio::test]
    async fn one_store_two_builds_yield_one_holder_and_one_refusal() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());

        let a = Memory::builder()
            .session("dup")
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .store(store.clone())
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await;
        let b = Memory::builder()
            .session("dup")
            .agent("agent-b")
            .flush_interval(Duration::from_secs(3_600))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await;

        // Exactly one wins.
        assert!(
            a.is_ok() ^ b.is_ok(),
            "exactly one of the two builds must acquire the lease"
        );
        if let Ok(m) = a {
            m.close().await.unwrap();
        }
        if let Ok(m) = b {
            m.close().await.unwrap();
        }
    }

    /// R2-4: close-then-reattach in one process — the MCP server's ordinary
    /// shape, and this crate's own reload test — must be silent.
    ///
    /// `close()` used to leave the registration in place until `Drop`, so
    /// rebuilding the session while the closed handle was still in scope fired
    /// the ops-level `SecondSessionWriter` ERROR against a handle that had
    /// already flushed its tail and stopped every task. A detector that cries
    /// wolf on the one sequence that is certainly safe is a detector that gets
    /// filtered out.
    #[tokio::test]
    async fn a_closed_handle_does_not_collide_with_a_reattach() {
        let (logs, _guard) = capture_logs(tracing::Level::ERROR);

        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store.clone(), "reattach").await;
        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        mem.close().await.unwrap();
        assert_eq!(live_handles("reattach"), 0);

        // The closed handle is deliberately still in scope: the owner holds it
        // (for `stats`, for a retry) while re-attaching.
        let reattached = memory_on(store, "reattach").await;
        let logged = logs.contents();
        assert!(
            !logged.contains("SecondSessionWriter"),
            "a re-attach after a successful close is not a second writer: {logged}"
        );
        assert_eq!(live_handles("reattach"), 1);

        // ...and the closed handle's `Drop` must not release the *new* one's
        // slot: the registry keys on session + agent id, which the re-attach
        // reuses.
        drop(mem);
        assert_eq!(
            live_handles("reattach"),
            1,
            "Drop after an already-released close must not evict the live handle"
        );

        reattached.close().await.unwrap();
        assert_eq!(live_handles("reattach"), 0);
    }

    /// R2-2: dropping a handle whose `close()` **failed** must not be silent.
    ///
    /// The leak guard keyed on task handles still being `Some`; a failed close
    /// has already taken all three, so `leaked` was false and `Drop` said
    /// nothing — while the tail that same close deliberately kept in the log
    /// (T81-5, for the retry it documents) went out with the handle. The exact
    /// case an owner most needs told, lost most quietly.
    ///
    /// The failed close also keeps its registration, which is the other half of
    /// the R2-4 policy: this handle still holds an undurable tail.
    #[tokio::test]
    async fn dropping_a_handle_whose_close_failed_warns_about_the_kept_tail() {
        let (logs, _guard) = capture_logs(tracing::Level::WARN);

        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store: Arc<dyn GraphStore> = Arc::new(FlakyStore::new(inner, usize::MAX));
        let mem = memory_on(store, "drop-after-failed-close").await;
        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();

        let err = mem.close().await.unwrap_err();
        assert!(err.to_string().contains("simulated outage"), "{err}");
        let kept = mem.graph().read().log_len();
        assert!(kept > 0, "the failed close kept the tail for a retry");
        assert_eq!(
            live_handles("drop-after-failed-close"),
            1,
            "a failed close keeps its registration — the tail is still undurable"
        );
        assert!(!logs.contains("dropped"), "nothing dropped yet");

        drop(mem);
        assert_eq!(live_handles("drop-after-failed-close"), 0);

        let logged = logs.contents();
        assert!(
            logged.contains("un-flushed"),
            "dropping a handle that still holds an un-flushed tail must warn: {logged}"
        );
        assert!(
            logged.contains("close() that did not finish"),
            "the warning must name the cause — a failed or cancelled close, not a \
             forgotten one: {logged}"
        );
        assert!(logged.contains(&kept.to_string()), "{logged}");
    }

    // -- close / drain ------------------------------------------------------

    #[tokio::test]
    async fn close_flushes_the_tail() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store.clone(), "tail").await;

        mem.derive(
            &[
                ("user schema", ConceptType::Entity),
                ("must stay backward compatible", ConceptType::Constraint),
            ],
            &ParentOf::none(),
        )
        .await
        .unwrap();
        mem.record_action(&Action {
            action: "created migrations/003.sql",
            produces: &["migrations/003.sql"],
            modifies: &[],
            depends_on: &["user schema"],
        })
        .unwrap();

        // Nothing durable yet — the flush interval is an hour.
        assert!(store.load_session(&SessionId::new("tail")).await.is_err());
        assert!(mem.stats().log_depth > 0);

        mem.close().await.unwrap();

        let snap = store.load_session(&SessionId::new("tail")).await.unwrap();
        assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
        assert!(snap
            .concepts
            .iter()
            .any(|c| c.content == "created migrations/003.sql"));
        assert_eq!(snap.interactions.len(), 2, "one interaction per write");
    }

    /// A session whose flush task is guaranteed to have RETAINED a batch: one
    /// attempt per cycle (`retries = 0`), that attempt fails, and the resulting
    /// `RETAINED_BACKOFF` hold (10s) keeps any further store call out of the
    /// short virtual-time windows these tests drive. Returns the retained count.
    async fn retained_batch_session(session: &str, store: Arc<dyn GraphStore>) -> (Memory, usize) {
        let mem = Memory::builder()
            .config(Config {
                backend_flush_retries: 0,
                ..Config::default()
            })
            .session(session)
            .agent("agent-a")
            .flush_interval(Duration::from_millis(100))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .unwrap();

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        let retained = mem.graph().read().log_len();
        assert!(retained > 0);

        // One cycle: drain the log, attempt once, fail, retain + hold.
        for _ in 0..10 {
            tokio::time::advance(Duration::from_millis(150)).await;
            tokio::task::yield_now().await;
            if mem.graph().read().log_len() == 0 && mem.stats().flush_depth > 0 {
                break;
            }
        }
        assert_eq!(
            mem.graph().read().log_len(),
            0,
            "the flush task drained the log into its pending buffer"
        );
        assert_eq!(
            mem.stats().flush_depth,
            retained,
            "the batch is retained in the task — not durable, and invisible to drain_log"
        );
        (mem, retained)
    }

    /// The retained-batch case the COH-6 design exists for: the flush task
    /// drained the log and failed to persist it, so those mutations live only
    /// in the task's `pending` buffer. `close()` must get them back (via the
    /// stop signal's push-front) and make them durable — a hard abort would
    /// drop them with the task.
    #[tokio::test(start_paused = true)]
    async fn close_flushes_a_batch_retained_after_a_failed_flush() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        // Exactly one failing flush; every later attempt (close's) succeeds.
        let store: Arc<dyn GraphStore> = Arc::new(FlakyStore::new(inner.clone(), 1));
        let (mem, _retained) = retained_batch_session("retained", store).await;

        assert!(
            inner
                .load_session(&SessionId::new("retained"))
                .await
                .is_err(),
            "nothing durable yet"
        );

        mem.close().await.unwrap();

        let snap = inner
            .load_session(&SessionId::new("retained"))
            .await
            .unwrap();
        assert!(
            snap.concepts.iter().any(|c| c.content == "user schema"),
            "a retained batch must be flushed by close(), never dropped"
        );
    }

    /// Pins the push-**front** mechanism itself: returned custody lands at the
    /// FRONT of the graph log, so `close()`'s final batch carries the retained
    /// mutations *and* everything written after them, as one chronological
    /// batch. With a permanently failing store the attempt must also surface as
    /// `close()`'s error rather than being swallowed.
    #[tokio::test(start_paused = true)]
    async fn close_returns_the_retained_batch_to_the_front_of_the_log() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let flaky = Arc::new(FlakyStore::new(inner, usize::MAX)); // fails forever
        let store: Arc<dyn GraphStore> = flaky.clone();
        let (mem, retained) = retained_batch_session("push-front", store).await;

        // Fresh writes land in the log *after* the retained batch was drained.
        mem.record_action(&Action {
            action: "wrote docs/api.md",
            produces: &["docs/api.md"],
            modifies: &[],
            depends_on: &[],
        })
        .unwrap();
        let fresh = mem.graph().read().log_len();
        assert!(fresh > 0);

        let err = mem.close().await.unwrap_err();
        assert!(err.to_string().contains("simulated outage"), "{err}");

        let batches = flaky.batch_lens();
        assert_eq!(
            *batches.last().unwrap(),
            retained + fresh,
            "close() must flush the retained batch AND the later writes as one \
             chronological batch (push-front), not just the later writes: {batches:?}"
        );

        // ...and in that ORDER (T81-3). Length alone let a push-BACK mutant
        // (`splice(0..0, ..)` -> `extend(..)`) survive the whole suite, while
        // it puts an edge upsert ahead of its endpoint's UpsertNode and so
        // fails a conforming adapter's in-order replay.
        let flushed = flaky.batches();
        let first_attempt = flushed.first().unwrap(); // what the task retained
        let final_batch = flushed.last().unwrap();
        assert_eq!(
            first_attempt.len(),
            retained,
            "the failed attempt is exactly the retained batch"
        );
        assert_eq!(
            &final_batch.mutations[..retained],
            &first_attempt.mutations[..],
            "the retained mutations must lead the final batch, in their original \
             order, with the later writes behind them"
        );

        // The premise that order serves (`src/graph/mod.rs`: replay in order,
        // never re-sort): no edge upsert before the nodes it points at.
        let mut seen: HashSet<NodeId> = HashSet::new();
        for m in &final_batch.mutations {
            match m {
                Mutation::UpsertNode { node } => {
                    seen.insert(node.id());
                }
                Mutation::UpsertEdge { edge } => {
                    assert!(
                        seen.contains(&edge.source) && seen.contains(&edge.target),
                        "edge upsert precedes an endpoint — the batch is not replayable \
                         in order: {m:?}"
                    );
                }
                _ => {}
            }
        }
    }

    // -- close(): armor, retry, concurrency (T81-2 / T81-5 / T81-6) ---------

    /// T81-2, timeout arm. A hung backend used to hang `close()` forever —
    /// the background path bounds every attempt with `FLUSH_ATTEMPT_TIMEOUT`
    /// and step 4 now does too. T81-5: the batch survives the timeout.
    #[tokio::test(start_paused = true)]
    async fn close_bounds_a_hanging_store_and_keeps_the_tail() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store: Arc<dyn GraphStore> = Arc::new(AdverseStore::new(inner, FlushBehaviour::Hang));
        let mem = memory_on(store, "hang").await;

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        let depth = mem.stats().log_depth;
        assert!(depth > 0);

        let err = mem.close().await.unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            mem.graph().read().log_len() >= depth,
            "a timed-out final flush must return the tail to the log, not drop it"
        );
    }

    /// T81-2, panic arm. A panicking adapter used to unwind out of `close()`
    /// **after** the log was drained — the tail was unrecoverable even for a
    /// caller that caught the panic.
    #[tokio::test]
    async fn close_contains_a_panicking_store_and_keeps_the_tail() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let store: Arc<dyn GraphStore> = Arc::new(AdverseStore::new(inner, FlushBehaviour::Panic));
        let mem = memory_on(store, "panic").await;

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        let depth = mem.stats().log_depth;

        let err = mem.close().await.unwrap_err();
        assert!(err.to_string().contains("panicked"), "{err}");
        assert!(
            mem.graph().read().log_len() >= depth,
            "a panicking final flush must return the tail to the log, not drop it"
        );
    }

    /// T81-5: a failed `close()` is retryable and says so. The first close
    /// fails (store outage), the tail goes back on the log rather than
    /// vanishing with the error, and a second close — after the store
    /// recovered — makes exactly that tail durable. `Ok(())` from `close()`
    /// always means "the tail is written".
    #[tokio::test]
    async fn a_failed_close_keeps_the_tail_and_a_later_close_flushes_it() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        // Exactly one failing flush — and with an hour-long interval, close's
        // is the only flush there is.
        let store: Arc<dyn GraphStore> = Arc::new(FlakyStore::new(inner.clone(), 1));
        let mem = memory_on(store, "close-retry").await;

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        let depth = mem.stats().log_depth;

        let err = mem.close().await.unwrap_err();
        assert!(err.to_string().contains("simulated outage"), "{err}");
        assert!(
            mem.graph().read().log_len() >= depth,
            "a failed close must not drop the drained tail"
        );
        assert!(
            inner
                .load_session(&SessionId::new("close-retry"))
                .await
                .is_err(),
            "nothing durable yet"
        );

        // The store recovered: the retry flushes the tail it kept.
        mem.close().await.unwrap();
        let snap = inner
            .load_session(&SessionId::new("close-retry"))
            .await
            .unwrap();
        assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
        assert_eq!(mem.graph().read().log_len(), 0);

        // Durable now, so it is idempotent again.
        mem.close().await.unwrap();
    }

    /// R2-1: a **cancelled** `close()` must not destroy the tail.
    ///
    /// `close()` is an ordinary future, and its own rustdoc invites an external
    /// bound ("a caller-supplied store can make step 0 arbitrarily long") —
    /// `close_completes_when_stop_lands_during_a_long_flush` above wraps it in
    /// exactly such a `timeout`. Dropped between the drain and the flush, the
    /// batch used to die with the local: the log was empty, the flush task was
    /// already joined, and the **second** `close()` drained nothing, latched
    /// success and returned `Ok(())` — the documented "Ok means the tail is
    /// durable" invariant, violated silently. (The reviewer's probe then found
    /// `load_session` returning `SessionNotFound`.)
    ///
    /// With `TailCustody` the drop hands the batch back to the front of the
    /// log, so the retry has something to flush — and the empty-log shortcut,
    /// which is what turned the loss into an `Ok`, is never reached with a
    /// taken-and-lost tail behind it.
    #[tokio::test(start_paused = true)]
    async fn a_cancelled_close_returns_the_tail_to_the_log_for_the_retry() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let adverse = Arc::new(AdverseStore::new(inner.clone(), FlushBehaviour::HangOnce));
        let store: Arc<dyn GraphStore> = adverse.clone();
        let mem = memory_on(store, "cancelled-close").await;

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        let depth = mem.stats().log_depth;
        assert!(depth > 0);

        // The caller gives up while the store is hung — well inside step 4's
        // own FLUSH_ATTEMPT_TIMEOUT, so this is a dropped future, not an
        // internally-bounded failure.
        let outcome = tokio::time::timeout(Duration::from_secs(5), mem.close()).await;
        assert!(
            outcome.is_err(),
            "the hung store must outlast the caller's patience"
        );
        assert_eq!(adverse.flush_calls(), 1, "the final flush was in flight");

        assert!(
            mem.graph().read().log_len() >= depth,
            "a cancelled close() must return the drained tail to the log, not drop it with \
             the future"
        );
        assert!(
            inner
                .load_session(&SessionId::new("cancelled-close"))
                .await
                .is_err(),
            "nothing durable yet"
        );

        // The store recovered. The retry must find and flush that tail — the
        // bug returned `Ok(())` here having written nothing at all.
        mem.close().await.unwrap();
        let snap = inner
            .load_session(&SessionId::new("cancelled-close"))
            .await
            .unwrap();
        assert!(
            snap.concepts.iter().any(|c| c.content == "user schema"),
            "Ok(()) from close() must mean the tail is durable"
        );
        assert_eq!(mem.graph().read().log_len(), 0);
    }

    /// R3-1: a `close()` cancelled at the **step-2 join** must not detach the
    /// flush task.
    ///
    /// R2-1 covered the drain-to-flush window; this is the window before it,
    /// and the likelier one — the join is the long await (`close`'s own "worst
    /// case ≈ 2 minutes" is mostly this), so an external `timeout` fires here.
    /// The handle was lifted out of its slot *before* that await, so dropping
    /// the future dropped the local: the flush task was detached, not stopped —
    /// still running, still holding the whole tail in its `pending` (it had
    /// already drained the log into it), still writing through its own `Arc`s.
    /// The retry then found `flush_handle == None`, skipped the join, drained an
    /// empty log, took the empty-log shortcut, latched success, released the
    /// [`ACTIVE_SESSIONS`] slot and returned `Ok(())` over a tail that was not
    /// durable. `Drop`'s R2-2 warning was blind to it for the same reason the
    /// shortcut was: the log really was empty.
    ///
    /// With `HandleCustody` the cancelled join hands the handle back, so the
    /// retry re-joins that same task — a cancelled poll leaves a `JoinHandle`
    /// re-awaitable — and the tail is written before any `Ok`.
    #[tokio::test(start_paused = true)]
    async fn a_close_cancelled_at_the_flush_join_keeps_the_handle_and_reaps_the_task() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let adverse = Arc::new(AdverseStore::new(inner.clone(), FlushBehaviour::HangOnce));
        let store: Arc<dyn GraphStore> = adverse.clone();
        let mem = Memory::builder()
            .session("cancelled-join")
            .agent("agent-a")
            // Short enough that the background loop is mid-attempt when the
            // caller closes: the cancellation must land on step 2, not step 4.
            .flush_interval(Duration::from_secs(1))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .unwrap();

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();

        // Walk the clock to the first tick: the task's attempt is now hung
        // inside the store, with the tail in its `pending` buffer.
        for _ in 0..40 {
            tokio::time::advance(Duration::from_millis(200)).await;
            tokio::task::yield_now().await;
            if adverse.flush_calls() > 0 {
                break;
            }
        }
        assert_eq!(adverse.flush_calls(), 1, "a flush must be in flight");
        assert!(!adverse.flush_completed(), "and still in flight");
        assert_eq!(
            mem.graph().read().log_len(),
            0,
            "the tail is in the task's pending buffer now, not in the log — which is what \
             makes a detached task invisible to everything downstream"
        );

        // The caller gives up well inside the task's own FLUSH_ATTEMPT_TIMEOUT,
        // so `close()` is dropped parked on the join.
        let outcome = tokio::time::timeout(Duration::from_secs(5), mem.close()).await;
        assert!(
            outcome.is_err(),
            "the hung attempt must outlast the caller's patience"
        );
        assert_eq!(adverse.flush_calls(), 1, "step 4 was never reached");
        assert!(
            mem.flush_handle.lock().is_some(),
            "a cancelled step-2 join must return the JoinHandle to its slot: detached, the task \
             runs on and the retry skips the join entirely (R3-1)"
        );
        assert!(
            inner
                .load_session(&SessionId::new("cancelled-join"))
                .await
                .is_err(),
            "nothing durable yet"
        );

        // The store is healthy for the task's next attempt. The retry re-joins
        // that same task, so it waits out the attempt (and gets the tail with
        // it) instead of blessing an empty log.
        tokio::time::timeout(Duration::from_secs(300), mem.close())
            .await
            .expect("the retry must re-join the flush task, not hang")
            .expect("close");

        let snap = inner
            .load_session(&SessionId::new("cancelled-join"))
            .await
            .expect("Ok(()) from close() must mean the tail is durable");
        assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
        assert_eq!(mem.graph().read().log_len(), 0);

        // ...and no zombie behind that `Ok`. Detached, the task's hung attempt
        // times out at FLUSH_ATTEMPT_TIMEOUT and it goes right on flushing —
        // after `close()` returned, which is how the reviewer's probe caught it.
        assert!(
            mem.flush_handle.lock().is_none(),
            "a reaped task leaves its slot empty"
        );
        let after_ok = adverse.flush_calls();
        tokio::time::advance(FLUSH_ATTEMPT_TIMEOUT * 4).await;
        tokio::task::yield_now().await;
        assert_eq!(
            adverse.flush_calls(),
            after_ok,
            "a store call after close() returned Ok means the flush task was never stopped"
        );
    }

    /// T81-6: a second **concurrent** `close()` must not report `Ok` over an
    /// in-flight final flush (a caller gating process exit on that `Ok` would
    /// let runtime teardown cancel the flush and lose the tail). Both callers
    /// must observe the completed flush.
    #[tokio::test(start_paused = true)]
    async fn concurrent_closes_both_wait_for_the_one_final_flush() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let adverse = Arc::new(AdverseStore::new(
            inner.clone(),
            FlushBehaviour::Delay(Duration::from_secs(2)),
        ));
        let store: Arc<dyn GraphStore> = adverse.clone();
        let mem = Arc::new(memory_on(store, "concurrent-close").await);

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();

        let closer = |mem: Arc<Memory>, adverse: Arc<AdverseStore>| {
            tokio::spawn(async move {
                let result = mem.close().await;
                (result, adverse.flush_completed())
            })
        };
        let first = closer(mem.clone(), adverse.clone());
        let second = closer(mem.clone(), adverse.clone());

        let (first_result, first_saw_flush) = first.await.unwrap();
        let (second_result, second_saw_flush) = second.await.unwrap();
        first_result.expect("first close");
        second_result.expect("second close");
        assert!(
            first_saw_flush && second_saw_flush,
            "no close() may return before the final flush completed"
        );
        assert_eq!(
            adverse.flush_calls(),
            1,
            "the tail is flushed once, not once per caller"
        );

        let snap = inner
            .load_session(&SessionId::new("concurrent-close"))
            .await
            .unwrap();
        assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
    }

    /// Partial coverage for COH-6 clause 2 — `biased;` + stop-first in the
    /// flush loop's `select!` (T81-4).
    ///
    /// The regression that clause guards is a **lost stop permit**: an unbiased
    /// `select!` polls in a random start order, so a concurrently ready
    /// `interval.tick()` can be polled first, consume-and-drop the stored
    /// permit, and `close()`'s join then hangs forever. The tick is
    /// concurrently ready exactly when a flush outlasts the interval — which is
    /// what this builds: a 5s flush against a 1s interval, `stop()` arriving
    /// while that flush is in flight.
    ///
    /// It cannot *prove* the ordering (a lost permit under an unbiased select
    /// is probabilistic — pinning it needs loom); what it does pin is that this
    /// shutdown shape terminates. A hang fails the test instead of wedging it,
    /// because the join is wrapped in a timeout on the (paused) clock.
    #[tokio::test(start_paused = true)]
    async fn close_completes_when_stop_lands_during_a_long_flush() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let adverse = Arc::new(AdverseStore::new(
            inner.clone(),
            FlushBehaviour::Delay(Duration::from_secs(5)),
        ));
        let store: Arc<dyn GraphStore> = adverse.clone();
        let mem = Memory::builder()
            .session("slow-flush")
            .agent("agent-a")
            .flush_interval(Duration::from_secs(1))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .unwrap();

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();

        // Walk the clock to the first tick and stop as soon as the flush has
        // started — not far enough for it to finish.
        for _ in 0..40 {
            tokio::time::advance(Duration::from_millis(200)).await;
            tokio::task::yield_now().await;
            if adverse.flush_calls() > 0 {
                break;
            }
        }
        assert_eq!(adverse.flush_calls(), 1, "a flush must be in flight");
        assert!(!adverse.flush_completed(), "and still in flight");

        // The join must finish. A lost stop permit would spin the loop until
        // this (virtual) deadline instead.
        tokio::time::timeout(Duration::from_secs(300), mem.close())
            .await
            .expect("close() must not hang when stop lands during an in-flight flush")
            .expect("close");
        assert!(
            adverse.flush_completed(),
            "the in-flight flush ran to completion before the loop exited"
        );

        let snap = inner
            .load_session(&SessionId::new("slow-flush"))
            .await
            .unwrap();
        assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
    }

    /// A session that has degraded to `durability="none"` on backlog (STORE-3):
    /// one mutation of headroom, so the first cycle's drain is already past
    /// `backend_log_max`. Returns it degraded, with its log already emptied by
    /// that cycle's drain.
    async fn degraded_session(session: &str) -> Memory {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = Memory::builder()
            .config(Config {
                backend_flush_retries: 0,
                backend_log_max: 1,
                ..Config::default()
            })
            .session(session)
            .agent("agent-a")
            .flush_interval(Duration::from_millis(100))
            .store(store)
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .unwrap();

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        for _ in 0..20 {
            tokio::time::advance(Duration::from_millis(150)).await;
            tokio::task::yield_now().await;
            if mem.stats().degraded {
                break;
            }
        }
        assert!(mem.stats().degraded, "the session must have degraded");
        mem
    }

    /// The degraded-close branch (implementer self-flag #6, ruled in scope).
    /// A session past `backend_log_max` stopped all store I/O by design;
    /// `close()` must say the tail was not written instead of reporting a
    /// durability it did not deliver — and must keep saying it.
    #[tokio::test(start_paused = true)]
    async fn close_refuses_to_claim_durability_for_a_degraded_session() {
        let mem = degraded_session("degraded").await;

        // A write after degradation is still in the log when close drains it.
        mem.record_action(&Action {
            action: "wrote docs/api.md",
            produces: &["docs/api.md"],
            modifies: &[],
            depends_on: &[],
        })
        .unwrap();
        assert!(mem.stats().log_depth > 0);

        let err = mem.close().await.unwrap_err();
        assert!(err.to_string().contains("degraded"), "{err}");
        assert!(
            mem.graph().read().log_len() > 0,
            "the un-written tail stays visible in log_depth"
        );
        // No `Ok` over an undurable tail, ever.
        let again = mem.close().await.unwrap_err();
        assert!(again.to_string().contains("degraded"), "{again}");
    }

    /// R2-3: the same must hold with an **empty** log — the case the ordering
    /// bug made `Ok(())`.
    ///
    /// Degraded mode keeps draining the log and DROPPING what it drained
    /// (STORE-3, spec §2.3 "none = pure RAM"), so an empty log is that mode's
    /// *steady state*: it means the tail was dead-lettered, not written. With
    /// the empty-log shortcut ahead of the degraded check, a caller who closed
    /// a moment after the drain got `Ok(())` — a durability claim over
    /// mutations the session had already thrown away, and one that contradicted
    /// the very next `close()` if a single write had landed in between.
    #[tokio::test(start_paused = true)]
    async fn close_refuses_a_degraded_session_even_when_its_log_is_empty() {
        let mem = degraded_session("degraded-empty").await;

        // Write, then let a degraded cycle drain-and-drop it: log empty, tail
        // gone. Exactly the state the shortcut used to bless.
        mem.record_action(&Action {
            action: "wrote docs/api.md",
            produces: &["docs/api.md"],
            modifies: &[],
            depends_on: &[],
        })
        .unwrap();
        assert!(mem.stats().log_depth > 0);
        for _ in 0..20 {
            tokio::time::advance(Duration::from_millis(150)).await;
            tokio::task::yield_now().await;
            if mem.stats().log_depth == 0 {
                break;
            }
        }
        assert_eq!(
            mem.stats().log_depth,
            0,
            "degraded draining drops what it drained (STORE-3)"
        );
        assert_eq!(mem.stats().flush_depth, 0, "and retains nothing");

        let err = mem.close().await.unwrap_err();
        assert!(err.to_string().contains("degraded"), "{err}");
        assert!(
            err.to_string().contains("not because the tail was written"),
            "the error must say why an empty log is not durability: {err}"
        );
        // Still no `Ok`, however often it is asked.
        let again = mem.close().await.unwrap_err();
        assert!(again.to_string().contains("degraded"), "{again}");
    }

    // -- the writers gate (T81-1) -------------------------------------------

    /// The P1 race, as the reviewer demonstrated it, now as a regression test.
    ///
    /// A store double parks `retract`'s `blast_radius` call, so the retraction
    /// is suspended *mid-write* — past `ensure_open`, before its mutation.
    /// `close()` then starts. Without the writers gate `close()` completed
    /// `Ok`, the retract resumed, reported `removed: true`, and its `DeleteNode`
    /// sat in the log forever: an acknowledged retraction that resurrects on
    /// reattach. With the gate there are only two legal outcomes and both are
    /// asserted — `close()` waits and the removal is durable, or the retract is
    /// refused with the closed error and nothing was acknowledged.
    #[tokio::test]
    async fn a_write_in_flight_when_close_starts_is_never_acknowledged_then_lost() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let parking = Arc::new(ParkingStore::new(inner.clone(), ParkPoint::BlastRadius));
        let store: Arc<dyn GraphStore> = parking.clone();
        let mem = Arc::new(memory_on(store, "write-vs-close").await);

        mem.derive(
            &[
                ("victim", ConceptType::Entity),
                ("survivor", ConceptType::Entity),
            ],
            &ParentOf::none(),
        )
        .await
        .unwrap();

        let entered = parking.entered();
        let retracting = tokio::spawn({
            let mem = mem.clone();
            async move { mem.retract("victim", DryRun::No).await }
        });
        entered.notified().await; // the retract is parked, holding a permit

        let mut closing = tokio::spawn({
            let mem = mem.clone();
            async move { mem.close().await }
        });
        // A real chance to run to completion — the whole shutdown is
        // sub-millisecond here — so this fails loudly if `close()` ever stops
        // waiting for in-flight writers (it is what the P1 bug did).
        assert!(
            tokio::time::timeout(Duration::from_millis(500), &mut closing)
                .await
                .is_err(),
            "close() must not drain while a write is in flight"
        );

        parking.release();
        let report = retracting.await.unwrap();
        closing.await.unwrap().expect("close");

        assert_eq!(
            mem.graph().read().log_len(),
            0,
            "no mutation may be left in the log after close()"
        );
        let snap = inner
            .load_session(&SessionId::new("write-vs-close"))
            .await
            .unwrap();
        assert!(snap.concepts.iter().any(|c| c.content == "survivor"));
        match report {
            Ok(report) => {
                assert!(report.removed);
                assert!(
                    !snap.concepts.iter().any(|c| c.content == "victim"),
                    "an acknowledged retraction must not resurrect on reattach"
                );
            }
            Err(err) => {
                assert!(err.to_string().contains("closed"), "{err}");
                assert!(
                    snap.concepts.iter().any(|c| c.content == "victim"),
                    "a refused retraction must not have removed anything"
                );
            }
        }
    }

    /// R2-6: the **post-acquire** `closed` re-check in `begin_write` — the
    /// second half of the gate, and the half a mutant could delete and still
    /// pass 539/539.
    ///
    /// The window it covers is real but two instructions wide: a writer loads
    /// `closed` as open, `close()` latches it and asks for the write side, and
    /// only then does the writer ask for its read permit — which now queues.
    /// Without the re-check that write wakes up *after* `close()` has drained,
    /// flushed and returned, and appends to a log nobody will ever flush again:
    /// T81-1 exactly, by the one route the gate itself opens.
    ///
    /// Held open deterministically here by taking the gate's write side in the
    /// test, which is precisely the state `close()` is in between its latch and
    /// its own acquisition. The tokio `RwLock` is FIFO-fair, so the queued
    /// derive is granted its permit the moment the test's guard drops — with
    /// `closed` already latched. Either order of the two waiters gives the same
    /// verdict: a write that takes the gate after the latch is refused, never
    /// silently appended.
    #[tokio::test]
    async fn a_write_that_takes_the_gate_after_close_latched_is_refused() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = Arc::new(memory_on(inner.clone(), "late-permit").await);
        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        let baseline = mem.graph().read().log_len();

        // The gate is busy but the session is still OPEN: a writer arriving now
        // passes the entry check and queues for its permit.
        let gate = mem.writers.write().await;

        let deriving = tokio::spawn({
            let mem = mem.clone();
            async move {
                mem.derive(&[("late concept", ConceptType::Entity)], &ParentOf::none())
                    .await
            }
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            mem.graph().read().log_len(),
            baseline,
            "the derive must be queued for the permit, not past it"
        );

        let closing = tokio::spawn({
            let mem = mem.clone();
            async move { mem.close().await }
        });
        for _ in 0..8 {
            tokio::task::yield_now().await;
        }
        assert!(
            mem.closed.load(Ordering::Acquire),
            "close() must have latched before the permit is handed over — that is \
             the window under test"
        );

        drop(gate);
        let derived = deriving.await.unwrap();
        closing.await.unwrap().expect("close");

        let err = derived.expect_err(
            "a write granted the gate after close() latched must be refused by the \
             post-acquire re-check, not mutate",
        );
        assert!(err.to_string().contains("closed"), "{err}");
        assert_eq!(
            mem.graph().read().log_len(),
            0,
            "and it must not have logged a mutation"
        );
        let snap = inner
            .load_session(&SessionId::new("late-permit"))
            .await
            .unwrap();
        assert!(snap.concepts.iter().any(|c| c.content == "user schema"));
        assert!(
            !snap.concepts.iter().any(|c| c.content == "late concept"),
            "a refused write must reach neither the log nor the store"
        );
    }

    /// The sync arm of the same barrier (`begin_write_sync`): `try_read` fails
    /// while `close()` holds — or is queued for — the write side, and that maps
    /// to the closed error rather than to a mutation that would race the drain.
    ///
    /// Its own post-acquire re-check covers a window one instruction wide
    /// (`try_read` succeeding between the latch and `close()`'s request) which
    /// no single-threaded interleaving can construct; see `begin_write_sync`'s
    /// rustdoc. This pins the arm that is reachable.
    #[tokio::test]
    async fn a_sync_write_is_refused_while_the_gate_is_taken() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "sync-gate").await;
        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        let baseline = mem.graph().read().log_len();

        let gate = mem.writers.write().await;
        let err = mem
            .record_action(&Action {
                action: "late",
                produces: &[],
                modifies: &[],
                depends_on: &[],
            })
            .unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
        assert_eq!(
            mem.graph().read().log_len(),
            baseline,
            "a refused sync write must not have logged a mutation"
        );

        drop(gate);
        mem.close().await.unwrap();
    }

    /// The other half of the gate: **readers do not take it**, so a long recall
    /// cannot hold shutdown hostage. The store parks recall's vector leg; the
    /// close must still finish (a gated reader would deadlock it). Reads after
    /// close stay refused by `ensure_open`, exactly as before.
    #[tokio::test]
    async fn close_does_not_wait_for_an_in_flight_read() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let parking = Arc::new(ParkingStore::new(inner, ParkPoint::VectorCandidates));
        let store: Arc<dyn GraphStore> = parking.clone();
        // Canonical matching, so the one parked call is recall's — hybrid
        // `derive` would otherwise consume the park on its own vector leg.
        let mem = Arc::new(
            Memory::builder()
                .session("read-vs-close")
                .agent("agent-a")
                .flush_interval(Duration::from_secs(3_600))
                .match_strategy(MatchStrategy::Canonical)
                .store(store)
                .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                .embedding_contract(contract("fixture", 1024))
                .build()
                .await
                .unwrap(),
        );

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();

        let entered = parking.entered();
        let recalling = tokio::spawn({
            let mem = mem.clone();
            async move {
                mem.recall(RecallQuery {
                    query: "user schema".into(),
                    top_k: 5,
                    max_tokens: 500,
                    traversal_depth: 2,
                })
                .await
            }
        });
        entered.notified().await; // the recall is parked inside the store

        tokio::time::timeout(Duration::from_secs(10), mem.close())
            .await
            .expect("close() must not wait for readers")
            .expect("close");

        parking.release();
        recalling
            .await
            .unwrap()
            .expect("the parked recall still returns");

        // Reads after close are refused by `ensure_open`, as before.
        let err = mem
            .recall(RecallQuery {
                query: "user schema".into(),
                top_k: 5,
                max_tokens: 500,
                traversal_depth: 2,
            })
            .await
            .unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
    }

    /// Every mutating method — async and sync — refuses after close. The sync
    /// ones go through the gate's non-blocking arm, so this also pins that a
    /// closed session's `try_read` path returns the closed error rather than
    /// mutating.
    #[tokio::test]
    async fn every_mutating_method_is_refused_after_close() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "refuse-all").await;
        let out = mem
            .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        let node = out.created[0];
        mem.close().await.unwrap();

        let closed = |err: LamboError| assert!(err.to_string().contains("closed"), "{err}");
        closed(mem.set_root_goal(&["late"]).unwrap_err());
        closed(mem.declare_synonym("a", "b").unwrap_err());
        closed(
            mem.derive(&[("late", ConceptType::Entity)], &ParentOf::none())
                .await
                .unwrap_err(),
        );
        closed(
            mem.record_action(&Action {
                action: "late",
                produces: &[],
                modifies: &[],
                depends_on: &[],
            })
            .unwrap_err(),
        );
        closed(mem.demote("late chunk.", "chunk-late").unwrap_err());
        closed(mem.retract("user schema", DryRun::No).await.unwrap_err());
        closed(mem.reserve(node, Duration::from_secs(30)).unwrap_err());
        closed(mem.release(node).unwrap_err());

        assert_eq!(
            mem.graph().read().log_len(),
            0,
            "a refused write must not have logged a mutation"
        );
    }

    #[tokio::test]
    async fn close_is_idempotent_and_the_session_refuses_later_writes() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "closed").await;
        mem.close().await.unwrap();
        mem.close().await.unwrap();

        let err = mem
            .derive(&[("late", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap_err();
        assert!(err.to_string().contains("closed"), "{err}");
    }

    // -- index mirroring ----------------------------------------------------

    /// The `src/graph/mod.rs` contract, at the `Memory` level: every concept
    /// create — `derive`, `record_action` AND `demote` — must be searchable,
    /// and a retraction must stop being searchable.
    #[tokio::test]
    async fn every_write_path_mirrors_the_inverted_index() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "mirror").await;

        let out = mem
            .derive(
                &[
                    ("pagination", ConceptType::Entity),
                    ("rate limiter", ConceptType::Entity),
                ],
                &ParentOf::none(),
            )
            .await
            .unwrap();
        assert_eq!(out.created.len(), 2);
        assert!(!mem.index().read().search("pagination", 10).is_empty());
        assert!(!mem.index().read().search("limiter", 10).is_empty());

        mem.record_action(&Action {
            action: "wrote docs/api.md",
            produces: &["docs/api.md"],
            modifies: &[],
            depends_on: &["pagination"],
        })
        .unwrap();
        assert!(
            !mem.index().read().search("docs/api.md", 10).is_empty(),
            "record_action creations must be mirrored"
        );

        let obs = mem
            .demote("The caching layer was the bottleneck.", "chunk-9")
            .unwrap();
        assert_eq!(obs.len(), 1);
        assert!(
            !mem.index().read().search("caching", 10).is_empty(),
            "demote creations must be mirrored"
        );

        // Retraction removes the posting too.
        mem.retract("rate limiter", DryRun::No).await.unwrap();
        assert!(
            mem.index().read().search("limiter", 10).is_empty(),
            "stale posting after retract"
        );

        mem.close().await.unwrap();
    }

    // -- retract ------------------------------------------------------------

    #[tokio::test]
    async fn retract_dry_run_reports_impact_and_mutates_nothing() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "retract-dry").await;

        mem.derive(
            &[("stale dependency", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
        mem.record_action(&Action {
            action: "wired the client",
            produces: &[],
            modifies: &[],
            depends_on: &["stale dependency"],
        })
        .unwrap();

        let before_nodes = mem.stats().node_count;
        let before_edges = mem.stats().edge_count;
        let before_epoch = mem.stats().epoch;

        let report = mem.retract("stale dependency", DryRun::Yes).await.unwrap();
        assert!(report.dry_run);
        assert!(!report.removed);
        assert_eq!(report.content, "stale dependency");
        assert!(report.incident_edges > 0);

        let after = mem.stats();
        assert_eq!(after.node_count, before_nodes, "dry run must not remove");
        assert_eq!(after.edge_count, before_edges);
        assert_eq!(after.epoch, before_epoch, "dry run must not log a mutation");
        assert!(
            !mem.index().read().search("stale", 10).is_empty(),
            "dry run must not touch the index"
        );

        mem.close().await.unwrap();
    }

    #[tokio::test]
    async fn retract_removes_the_node_and_its_edges() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "retract-live").await;

        mem.derive(
            &[
                ("stale dependency", ConceptType::Entity),
                ("http client", ConceptType::Entity),
            ],
            &ParentOf::none(),
        )
        .await
        .unwrap();
        let before = mem.stats();

        let report = mem.retract("stale dependency", DryRun::No).await.unwrap();
        assert!(report.removed);
        assert!(!report.dry_run);

        let after = mem.stats();
        assert_eq!(after.node_count, before.node_count - 1);
        assert!(after.edge_count < before.edge_count);
        assert!(mem.graph().read().node(report.target).is_none());

        mem.close().await.unwrap();
    }

    /// R2-5: `retract`'s durable-radius query was the only store call on a
    /// user-facing path with no bound, and the writers gate made it `close()`'s
    /// problem — `retract` holds its permit across that await, so step 0 waited
    /// on the hung backend for as long as it cared to hang.
    ///
    /// Bounded, it fails at `RETRACT_IO_TIMEOUT` having mutated nothing (the
    /// await precedes every graph write), and the session still closes.
    #[tokio::test(start_paused = true)]
    async fn retract_bounds_a_hanging_durable_radius_query() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        // Parked and never released: the backend that never answers.
        let parking = Arc::new(ParkingStore::new(inner, ParkPoint::BlastRadius));
        let store: Arc<dyn GraphStore> = parking.clone();
        let mem = memory_on(store, "retract-hang").await;

        mem.derive(
            &[("stale dependency", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
        let before = mem.stats();

        // The outer bound is the test's safety net, an order of magnitude past
        // the real one: an unbounded `retract` fails this assertion instead of
        // wedging the suite on a store that never answers.
        let started = tokio::time::Instant::now();
        let err = tokio::time::timeout(
            RETRACT_IO_TIMEOUT * 10,
            mem.retract("stale dependency", DryRun::No),
        )
        .await
        .expect("retract must bound its own durable-radius query")
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");
        assert!(
            started.elapsed() >= RETRACT_IO_TIMEOUT,
            "the bound must be the timeout, not something shorter"
        );

        // Nothing half-done: the removal is below the await, so a refused
        // retraction leaves the node, its edges and its index posting.
        let after = mem.stats();
        assert_eq!(after.node_count, before.node_count);
        assert_eq!(after.edge_count, before.edge_count);
        assert_eq!(after.epoch, before.epoch, "no mutation was logged");
        assert!(!mem.index().read().search("stale", 10).is_empty());

        // ...and the writers gate is free again, so shutdown is bounded too.
        tokio::time::timeout(Duration::from_secs(60), mem.close())
            .await
            .expect("close() must not wait on the hung query")
            .expect("close");
    }

    /// R3-3: the bound applies to [`DryRun::Yes`] as well, and the rustdoc now
    /// says so.
    ///
    /// A dry run mutates nothing, so it *could* have degraded to the warning
    /// path the store-error arm uses — the reason it does not is that the
    /// asymmetry is a judgement about the store, not about this call, and a dry
    /// run is the preview the real retraction gets authorised from. Whichever
    /// way that decision goes it must be pinned, because the two arms of the
    /// same `match` now differ on it.
    #[tokio::test(start_paused = true)]
    async fn retract_bounds_a_hanging_query_for_a_dry_run_too() {
        let inner: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let parking = Arc::new(ParkingStore::new(inner, ParkPoint::BlastRadius));
        let store: Arc<dyn GraphStore> = parking.clone();
        let mem = memory_on(store, "retract-hang-dry").await;

        mem.derive(
            &[("stale dependency", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();
        let before = mem.stats();

        let err = tokio::time::timeout(
            RETRACT_IO_TIMEOUT * 10,
            mem.retract("stale dependency", DryRun::Yes),
        )
        .await
        .expect("a dry run must bound its durable-radius query too")
        .unwrap_err();
        assert!(err.to_string().contains("timed out"), "{err}");

        // Inert as ever — the failure changes nothing about that.
        let after = mem.stats();
        assert_eq!(after.node_count, before.node_count);
        assert_eq!(after.edge_count, before.edge_count);
        assert_eq!(after.epoch, before.epoch, "no mutation was logged");

        tokio::time::timeout(Duration::from_secs(60), mem.close())
            .await
            .expect("close() must not wait on the hung query")
            .expect("close");
    }

    #[tokio::test]
    async fn retract_reports_an_unknown_target_as_not_found() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "retract-missing").await;
        let err = mem.retract("nothing here", DryRun::Yes).await.unwrap_err();
        assert!(err.to_string().contains("no concept matching"), "{err}");
        mem.close().await.unwrap();
    }

    /// The store answers once the session is flushed; before that the report
    /// degrades to the in-RAM radius with a warning instead of failing.
    #[tokio::test]
    async fn retract_reports_the_durable_radius_when_the_store_can_answer() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store.clone(), "retract-durable").await;

        mem.derive(
            &[("stale dependency", ConceptType::Entity)],
            &ParentOf::none(),
        )
        .await
        .unwrap();

        let unflushed = mem.retract("stale dependency", DryRun::Yes).await.unwrap();
        assert!(unflushed.durable_blast_radius.is_none());
        assert_eq!(unflushed.warnings.len(), 1);

        mem.close().await.unwrap();

        // Re-attach over the now-durable session and ask again.
        let mem = memory_on(store, "retract-durable").await;
        let flushed = mem.retract("stale dependency", DryRun::Yes).await.unwrap();
        assert_eq!(flushed.durable_blast_radius, Some(flushed.blast_radius));
        assert!(flushed.warnings.is_empty());
        mem.close().await.unwrap();
    }

    // -- canonical_memories / stats / events --------------------------------

    #[tokio::test]
    async fn canonical_memories_lists_only_canonical_concepts() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "saints").await;

        mem.derive(
            &[
                ("user schema", ConceptType::Entity),
                ("auth middleware", ConceptType::Entity),
            ],
            &ParentOf::none(),
        )
        .await
        .unwrap();
        assert!(mem.canonical_memories().is_empty());

        // Walk `user schema` up the spec §10 state machine through the audited
        // transition path (the same one the canonization task uses).
        let target = {
            let g = mem.graph().read();
            let found = g
                .concepts()
                .find(|c| c.content == "user schema")
                .map(|c| c.id);
            found.unwrap()
        };
        for (from, to) in [
            (CanonizationStatus::None, CanonizationStatus::Candidate),
            (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
            (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
        ] {
            let event = CanonizationEvent {
                id: NodeId::new(),
                session_id: SessionId::new("saints"),
                node_id: target,
                from_status: from,
                to_status: to,
                blast_radius: Some(0),
                occurred_at: Utc::now(),
                last_demotion_time: None,
            };
            mem.graph()
                .write()
                .apply_canonization_transition(event)
                .unwrap();
        }

        let saints = mem.canonical_memories();
        assert_eq!(saints.len(), 1);
        assert_eq!(saints[0].content, "user schema");
        assert_eq!(saints[0].node_id, target);
        assert_eq!(mem.stats().canonical_count, 1);

        mem.close().await.unwrap();
    }

    #[tokio::test]
    async fn stats_expose_flush_lag_and_log_depth() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "stats").await;

        // A fresh session already carries exactly one mutation: `build()`
        // stamped the embedding contract, and that stamp is durable state.
        let baseline = mem.stats().log_depth;
        assert_eq!(baseline, 1, "the embedding-contract stamp");

        mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();

        let stats = mem.stats();
        assert!(stats.log_depth > baseline);
        assert!(stats.log_depth > 0, "unflushed mutations must be visible");
        assert!(!stats.degraded);
        assert_eq!(stats.dead_lettered, 0);
        assert_eq!(stats.session, SessionId::new("stats"));
        assert_eq!(stats.concept_count, 1);

        mem.close().await.unwrap();
        // The graph log is empty after the final drain.
        assert_eq!(mem.graph().read().log_len(), 0);
    }

    #[tokio::test]
    async fn events_hands_out_the_pre_spawn_receiver_first() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "events").await;
        // Two subscriptions, both usable; the first is the pre-spawn one.
        let _first = mem.events();
        let _second = mem.events();
        mem.close().await.unwrap();
    }

    // -- recall / synonyms / reservations -----------------------------------

    #[tokio::test]
    async fn recall_returns_a_context_block_for_a_derived_concept() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "recall").await;

        mem.derive(
            &[
                ("user schema", ConceptType::Entity),
                ("auth middleware", ConceptType::Entity),
            ],
            &ParentOf::none(),
        )
        .await
        .unwrap();

        let result = mem
            .recall(RecallQuery {
                query: "update user schema".into(),
                top_k: 5,
                max_tokens: 500,
                traversal_depth: 2,
            })
            .await
            .unwrap();

        assert!(
            result.hits.iter().any(|h| h.content == "user schema"),
            "keyword leg must find the derived concept: {result:?}"
        );
        assert!(result.context.contains("user schema"));

        mem.close().await.unwrap();
    }

    /// `MatchStrategy::Hybrid` routes `derive` through the async
    /// `graph::hybrid::derive` twin. Against a store without `VECTOR_SEARCH`
    /// (MemoryStore) the hybrid step is skipped and the outcome is identical to
    /// the canonical path — what matters here is that the dispatch compiles,
    /// runs, holds no lock across its awaits, and still mirrors the index.
    #[tokio::test]
    async fn hybrid_strategy_derives_and_mirrors_the_index() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = Memory::builder()
            .session("hybrid")
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .match_strategy(MatchStrategy::Hybrid)
            .store(store.clone())
            .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
            .embedding_contract(contract("fixture", 1024))
            .build()
            .await
            .unwrap();

        let out = mem
            .derive(
                &[
                    ("user schema", ConceptType::Entity),
                    ("auth middleware", ConceptType::Entity),
                ],
                &ParentOf::none(),
            )
            .await
            .unwrap();
        assert_eq!(out.created.len(), 2);
        assert!(!mem.index().read().search("schema", 10).is_empty());

        // A second derive of the same content matches rather than duplicating.
        let again = mem
            .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        assert!(again.created.is_empty());
        assert_eq!(again.matched.len(), 1);

        mem.close().await.unwrap();
        let snap = store.load_session(&SessionId::new("hybrid")).await.unwrap();
        assert_eq!(snap.concepts.len(), 2);
    }

    /// **L82-4 end to end.** A concept created by the ordinary derive surface
    /// persists its embedding, the vector survives the write-behind flush and a
    /// session reload, and recall's vector leg finds it. This is the path that
    /// stored `embedding IS NULL` for all 13 organic concepts the live-Cockroach
    /// review measured — recall was keyword/recency only on everything the
    /// product itself wrote (`adve-review-t8.2-t8.3-live.md`, L82-4).
    #[tokio::test]
    async fn organic_derive_persists_a_vector_that_recall_finds() {
        use crate::embed::{NEAR_A, NEAR_B};

        let store = Arc::new(VectorSearchStore::new(
            Arc::new(MemoryStore::new()) as Arc<dyn GraphStore>
        ));
        let session = SessionId::new("organic-vectors");
        let open = |store: Arc<VectorSearchStore>| async move {
            Memory::builder()
                .session("organic-vectors")
                .agent("agent-a")
                .flush_interval(Duration::from_secs(3_600))
                .match_strategy(MatchStrategy::Hybrid)
                .store(store as Arc<dyn GraphStore>)
                .embedder(
                    Arc::new(ContextTolerantEmbedder(FixtureEmbedder::new())) as Arc<dyn Embedder>
                )
                .embedding_contract(contract("fixture", 1024))
                .build()
                .await
                .expect("build")
        };

        let mem = open(store.clone()).await;
        let out = mem
            .derive(&[(NEAR_A, ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        assert_eq!(out.created.len(), 1);
        let organic = out.created[0];
        assert!(
            out.semantic_merged.is_empty(),
            "empty vector pool — nothing to merge with, so the concept is Fresh"
        );
        // close() drains the tail: the vector has to be durable, not RAM-only.
        mem.close().await.unwrap();

        // The reviewer's `SELECT embedding IS NOT NULL`, in process.
        let snapshot = store.load_session(&session).await.unwrap();
        let stored = snapshot
            .concepts
            .iter()
            .find(|c| c.id == organic)
            .expect("the derived concept is durable");
        assert_eq!(
            stored.embedding.as_ref().map(Vec::len),
            Some(1024),
            "an organically-derived concept persists its vector (L82-4)"
        );
        assert_eq!(
            store.answers(),
            vec![Vec::new()],
            "the derive's own gather queried an empty pool and merged nothing"
        );

        // Reopen (proving the vector round-trips through load_session) and
        // recall with text that shares NO token with the stored concept
        // ("create account" vs "register user") but is near it in the embedding
        // space — the keyword leg cannot score it, only the vector leg can.
        let reopened = open(store.clone()).await;
        let result = reopened
            .recall(RecallQuery {
                query: NEAR_B.into(),
                top_k: 5,
                max_tokens: 500,
                traversal_depth: 1,
            })
            .await
            .unwrap();

        let answers = store.answers();
        assert_eq!(answers.len(), 2, "recall issued exactly one vector query");
        let scored = answers[1]
            .iter()
            .find(|s| s.item == organic)
            .expect("the vector leg returned the organically-derived concept");
        assert!(
            scored.score >= 0.85,
            "organic vector scored by real similarity, got {}",
            scored.score
        );
        assert!(
            result.hits.iter().any(|h| h.node_id == organic),
            "the vector-leg candidate reaches the assembled result: {result:?}"
        );
        assert!(
            !result
                .warnings
                .iter()
                .any(|w| w.contains("vector leg skipped")),
            "the vector leg must not degrade here: {:?}",
            result.warnings
        );

        reopened.close().await.unwrap();
    }

    /// **E2E-1 regression.** The `--allow-embedding-mismatch` relabel must be
    /// durable BEFORE the first write on a `VECTOR_SEARCH`-capable store: the
    /// checked candidate read compares the *durable* contract against the
    /// expected one, so a write-behind relabel (flush at interval / close)
    /// made the documented override workflow refuse its FIRST hybrid write
    /// with `Invariant` (live-reproduced on Cockroach; the identical
    /// invocation only succeeded on the second run, once `close()` had landed
    /// the relabel). The sqlite/memory override tests cannot catch this —
    /// neither store advertises `VECTOR_SEARCH`, so the hybrid checked read
    /// is never reached; [`VectorSearchStore`] does advertise it and enforces
    /// the contract like Cockroach.
    #[tokio::test]
    async fn h1_override_relabel_is_durable_before_the_first_hybrid_write() {
        let store = Arc::new(VectorSearchStore::new(
            Arc::new(MemoryStore::new()) as Arc<dyn GraphStore>
        ));
        let old = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-model-v1".into()),
            dim: 1024,
        };
        let live = EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-model-renamed".into()),
            dim: 1024,
        };
        let open = |store: Arc<VectorSearchStore>, contract: EmbeddingContract, allow: bool| {
            let store = store.clone();
            async move {
                Memory::builder()
                    .session("e2e1-override-first-write")
                    .agent("operator")
                    .flush_interval(Duration::from_secs(3_600))
                    .match_strategy(MatchStrategy::Hybrid)
                    .store(store as Arc<dyn GraphStore>)
                    .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
                    .embedding_contract(contract)
                    .allow_embedding_mismatch(allow)
                    .build()
                    .await
                    .expect("attach")
            }
        };

        // Writer 1: contract A; the derive writes a real vector through the
        // checked read (empty pool on the fresh, unstamped session).
        let first = open(store.clone(), old.clone(), false).await;
        first
            .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        first.close().await.unwrap();

        // Writer 2: same kind, same width, renamed model, explicit override.
        // The FIRST write must now succeed — the relabel was flushed to the
        // store at attach, so the checked read sees the new durable contract.
        let second = open(store.clone(), live.clone(), true).await;
        second
            .derive(
                &[("auth middleware", ConceptType::Entity)],
                &ParentOf::none(),
            )
            .await
            .unwrap_or_else(|err| {
                panic!("override relabel must be durable before the first hybrid write: {err}")
            });
        second.close().await.unwrap();

        // The durable contract is the relabeled one, not the original.
        let snapshot = store
            .load_session(&SessionId::new("e2e1-override-first-write"))
            .await
            .unwrap();
        assert_eq!(snapshot.embedding, Some(live));
    }

    #[tokio::test]
    async fn declared_synonyms_merge_on_derive() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "synonyms").await;

        mem.declare_synonym("register_user", "create_user").unwrap();
        let first = mem
            .derive(&[("create_user", ConceptType::Logic)], &ParentOf::none())
            .await
            .unwrap();
        let second = mem
            .derive(&[("register_user", ConceptType::Logic)], &ParentOf::none())
            .await
            .unwrap();

        assert_eq!(first.created.len(), 1);
        assert!(
            second.created.is_empty(),
            "the synonym must match, not create"
        );
        assert_eq!(second.matched, first.created);

        mem.close().await.unwrap();
    }

    #[tokio::test]
    async fn reserve_and_release_round_trip() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "reserve").await;

        let out = mem
            .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        let node = out.created[0];

        let reservation = mem.reserve(node, Duration::from_secs(30)).unwrap();
        assert_eq!(reservation.node_id, node);
        assert_eq!(reservation.agent_id, AgentId::new("agent-a"));
        assert!(mem.graph().read().reservation(node).is_some());

        mem.release(node).unwrap();
        assert!(mem.graph().read().reservation(node).is_none());

        mem.close().await.unwrap();
    }

    #[tokio::test]
    async fn root_goal_promotes_matching_concepts_to_venerable() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
        let mem = memory_on(store, "goal").await;

        mem.derive(&[("3D renderer", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        mem.set_root_goal(&["doom-style FPS", "3D renderer"])
            .unwrap();

        let status = {
            let g = mem.graph().read();
            let found = g
                .concepts()
                .find(|c| c.content == "3D renderer")
                .map(|c| c.canonization_status);
            found.unwrap()
        };
        assert_eq!(status, CanonizationStatus::Venerable);

        mem.close().await.unwrap();
    }

    // -- reload -------------------------------------------------------------

    #[tokio::test]
    async fn a_closed_session_reloads_with_its_graph_and_index() {
        let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());

        let mem = memory_on(store.clone(), "reload").await;
        mem.derive(
            &[
                ("user schema", ConceptType::Entity),
                ("auth middleware", ConceptType::Entity),
            ],
            &ParentOf::none(),
        )
        .await
        .unwrap();
        mem.demote("The caching layer was the bottleneck.", "chunk-1")
            .unwrap();
        mem.close().await.unwrap();

        let reloaded = memory_on(store, "reload").await;
        assert_eq!(reloaded.stats().concept_count, 3);
        assert!(
            !reloaded.index().read().search("caching", 10).is_empty(),
            "load_session rebuilds the index from the same snapshot"
        );
        // A re-derive of an existing concept matches instead of creating.
        let out = reloaded
            .derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
            .await
            .unwrap();
        assert!(out.created.is_empty());
        assert_eq!(out.matched.len(), 1);

        reloaded.close().await.unwrap();
    }
}
