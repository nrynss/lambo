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

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use chrono::{DateTime, Utc};
use parking_lot::{Mutex as PlMutex, RwLock};
use tokio::sync::{broadcast, RwLock as AsyncRwLock, RwLockReadGuard as AsyncRwLockReadGuard};
use tokio::task::JoinHandle;

use crate::canon::CanonizationTask;
use crate::config::{Config, ScoringWeights};
use crate::daemon::access::AccessLedger;
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
use crate::store::lease::{LeaseHolder, LeaseOutcome, LEASE_TTL};
use crate::store::load::load_session_async;
use crate::store::{Capabilities, GraphStore};
use crate::types::{
    tie_break_by_key, AgentId, CanonizationStatus, Concept, ConceptType, DaemonEvent,
    EmbeddingContract, Interaction, LamboError, MatchStrategy, MutationBatch, Node, NodeId,
    RecallQuery, RecallResult, Reservation, SessionId, StoreError,
};
use crate::writeq::{Submitted, WriteCtx, WritePipeline};

mod types;

pub use types::{CanonicalMemory, DryRun, GcStats, GcSweepSummary, ImpactReport, MemoryStats};

mod leases;

pub(crate) use leases::LeaseLostSignal;
use leases::{register_session, spawn_lease_heartbeat};

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

// ---------------------------------------------------------------------------
// Builder
// ---------------------------------------------------------------------------

/// The clause [`Attach::Held`]'s message uses for a holder that still looks
/// live — named, not inlined, because `mcp::serve` has to be able to *correct*
/// it (J2-R2-3).
///
/// The refusal is composed here from the lease row alone, which is evidence up to
/// one `LEASE_HEARTBEAT_INTERVAL` old. `serve`'s election then probes the
/// holder's endpoint, and a refused connect is better evidence: it replaces this
/// clause rather than contradicting it a sentence later. Sharing the literal is
/// what keeps the two ends from drifting — a reword here that left `serve`
/// looking for the old text would silently turn the correction into a no-op.
pub(crate) const STILL_REFRESHING_CLAUSE: &str = "is still refreshing it";

/// What a session lease refusal tells the loser (J2).
///
/// The refusal has always carried this information; before J2 it was formatted
/// into a string and the structure was thrown away, which is why a losing
/// `serve` could only exit. `mcp::serve` needs the holder's *endpoint* to proxy
/// to it, and the host inside `holder` to know whether that endpoint means
/// anything on this machine.
#[derive(Clone)]
pub struct LeaseHeldElsewhere {
    /// The operator-facing refusal, byte-identical to the message
    /// [`MemoryBuilder::build`] returns as [`LamboError::Conflict`].
    pub message: String,
    /// The lease row as the store reported it — holder token, fencing token,
    /// timings, and the holder's published `endpoint`.
    pub current: crate::store::lease::LeaseInfo,
    /// How long the current holder has held it.
    pub age: Duration,
    /// The store this attach was refused against.
    ///
    /// Handed back so a caller that becomes a proxy can **re-read** the lease
    /// row on every reconnect attempt without opening a second connection —
    /// Level B's "one store per process" is preserved, and the proxy still runs
    /// no `store::load` and holds no in-RAM graph.
    pub store: Arc<dyn GraphStore>,
}

/// The outcome of an attach: this process owns the session, or someone else does.
///
/// **Why a type rather than a new [`LamboError`] variant.** J1-R2-2's lesson is
/// that a code path must never be selected by matching an error *variant* with
/// several producers. The other way to honour that is not to widen the error
/// enum at all: [`MemoryBuilder::build`] still returns exactly the
/// `LamboError::Conflict` it always did — same bytes, same `err_class`, same N4
/// treatment, same CLI text and exit code — and the one caller that needs the
/// structure asks for it by calling [`MemoryBuilder::build_attach`] instead.
/// Nothing downstream of `build` moved.
pub enum Attach {
    /// The lease is ours; the session is live. Boxed because a `Memory` handle
    /// is an order of magnitude larger than the refusal report, and every
    /// caller moves it straight out of the enum anyway.
    Attached(Box<Memory>),
    /// A live lease is held by another writer.
    Held(Box<LeaseHeldElsewhere>),
}

/// The named setters below (`match_strategy`, `flush_interval`,
/// `scoring_weights`) override the corresponding [`Config`] field and are
/// applied at `build()` time, so they commute with [`MemoryBuilder::config`].
/// Everything else the session needs already has a `Config` knob, so pass a
/// whole [`Config`] rather than looking for more setters. **No new knobs are
/// introduced by this type.**
/// `Clone` is deliberate and load-bearing (J2): `mcp::serve` retries the attach
/// when the session's lease is held by a holder that turns out to be
/// unreachable, and it must retry the *same* configuration — one store, one
/// embedder, one contract, all behind `Arc`s, so a clone is cheap and does not
/// violate Level B's single construction site.
#[derive(Clone, Default)]
pub struct MemoryBuilder {
    session: Option<SessionId>,
    agent: Option<AgentId>,
    store: Option<Arc<dyn GraphStore>>,
    embedder: Option<Arc<dyn Embedder>>,
    embedding: Option<EmbeddingContract>,
    allow_embedding_mismatch: bool,
    /// K2. Crate-internal attach mode for the re-embed migration: load the
    /// session WITHOUT the Level B contract check, because this caller is about
    /// to rewrite every vector into the live space atomically (see
    /// [`crate::graph::Graph::reembed_all`]). Set only by `lambo re-embed`;
    /// never relabels anything.
    reembed_mode: bool,
    config: Config,
    // Held as overrides rather than written straight into `config`, so
    // `.config(..)` and the named setters commute — calling them in either
    // order gives the same session. They are applied in `build`.
    match_strategy: Option<MatchStrategy>,
    flush_interval: Option<Duration>,
    scoring_weights: Option<ScoringWeights>,
    // Crate-private, and not a knob: see `MemoryBuilder::clock`.
    clock: Option<Clock>,
    endpoint: Option<String>,
    /// J4. An optional call ledger this process appends its own conflict and
    /// write-intent **completion** lines to (pre-lease startup, lease refusals,
    /// proxying/degraded, and durable-intent completion — see
    /// `dev-diary/lambo-for-mooshik/J-multi-client.md` §J4). `None` for every
    /// writer that is not a `serve` and for a `serve` run without `--ledger`.
    /// Set only by [`crate::mcp::serve`]; every ordinary writer keeps the
    /// default.
    ledger: Option<Arc<crate::ledger::Ledger>>,
    /// J6. The shutdown pre-arm a `serve` process wants installed the instant
    /// this builder takes the single-writer lease, closing the window between
    /// the acquire and the arming at `holder_shutdown` in which a SIGTERM had
    /// the default disposition and killed the process with `close()` un-run.
    ///
    /// Handed in already-constructed but **unarmed**: this builder decides
    /// *when* (the `LeaseOutcome::Acquired` arm below, and nowhere else), and
    /// `serve` decides *what*. `None` — every CLI writer verb, every library
    /// caller, every test — installs no handler at all, which is deliberate:
    /// a library `build()` must not change the calling process's signal
    /// disposition. See `crate::mcp::serve::EarlyShutdown`.
    early_shutdown: Option<crate::mcp::serve::EarlyShutdown>,
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

    /// Attach WITHOUT the Level B contract check (`pub(crate)`, K2).
    ///
    /// A normal attach refuses a stored/live contract mismatch because two
    /// model spaces in one session is exactly the corruption Level B exists to
    /// prevent. The re-embed migration needs the OPPOSITE: it must attach a
    /// session that carries an old contract and old-space vectors, because it
    /// is about to rewrite every one of those vectors into the live space
    /// atomically ([`crate::graph::Graph::reembed_all`]) and swap the contract
    /// in the same flushed batch. Skipping the check here loads the session
    /// as-is — old contract, old vectors — and nothing is relabelled.
    ///
    /// Only `lambo re-embed` sets this; it is deliberately unreachable from
    /// outside the crate, so no ordinary caller can bypass the mismatch refusal.
    pub(crate) fn reembed_mode(mut self, on: bool) -> Self {
        self.reembed_mode = on;
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
    /// commute), as `open_writer` and `serve`'s `serve_builder` do; otherwise the
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
    /// Selects the recall matching scope, whether a derive embeds, **and** the
    /// call-time validation rule set (see [`MatchStrategy`] in `types`, which
    /// documents all three consequences). Dispatched on by [`Memory::derive`].
    /// Overrides `Config::match_strategy`.
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

    /// Where this writer can be reached, published into the lease row's
    /// `endpoint` column when the lease is acquired (J2).
    ///
    /// Set by [`crate::mcp::serve`](mod@crate::mcp::serve) and by nothing else. A `serve` process is
    /// the only writer another process can forward MCP tool calls to, so it is
    /// the only one whose address is worth recording; unset — every CLI writer
    /// verb, every library caller, every test — leaves the column NULL, which a
    /// refused `serve` reads as "no hub here" and reports honestly instead of
    /// dialling nothing. See `store::lease::LeaseHolder::endpoint`.
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Attach a call ledger (J4) for this serve's own conflict / completion
    /// lines. `None` (the default) writes nothing.
    pub fn ledger(mut self, ledger: Option<Arc<crate::ledger::Ledger>>) -> Self {
        self.ledger = ledger;
        self
    }

    /// Arm this handle the instant the single-writer lease is acquired (J6).
    ///
    /// Crate-private, and set by `crate::mcp::serve` alone. The handle
    /// arrives unarmed and is armed from exactly one place — the
    /// `LeaseOutcome::Acquired` arm of [`MemoryBuilder::build_attach`] — so a
    /// build that never takes the lease never installs a signal handler. That
    /// is what keeps the startup election killable, and what makes "a proxy
    /// never arms" the same statement as "a proxy never takes the lease".
    pub(crate) fn early_shutdown(mut self, early: crate::mcp::serve::EarlyShutdown) -> Self {
        self.early_shutdown = Some(early);
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
        match self.build_attach().await? {
            Attach::Attached(mem) => Ok(*mem),
            // The exact error this function returned before J2 — see `Attach`.
            Attach::Held(held) => Err(LamboError::Conflict(held.message)),
        }
    }

    /// [`MemoryBuilder::build`], reporting a lease refusal as data rather than
    /// as an error (J2). See [`Attach`] for why this is a second method rather
    /// than a change to `build`'s error type.
    pub async fn build_attach(self) -> Result<Attach, LamboError> {
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

        // (0a) Schema preflight (J3 round-1 F5), BEFORE the lease: `init_schema`
        // runs only from `lambo provision`, never here, so a store provisioned
        // by an older build is missing whatever tables this build's DDL added.
        // Measured on such a store: the session attaches, every write is acked,
        // and NOTHING becomes durable — a write's mutations and its
        // `PutWriteIntent` share one flush transaction, so one missing table
        // rolls each batch back whole and the operator learns only from the
        // failed final flush at close. Refusing here is honest and actionable;
        // acking into a void is neither. Ordered before the acquire so the
        // refusal has no lease to release.
        store.preflight_schema().await.map_err(LamboError::Store)?;

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
        let lease_holder = match &self.endpoint {
            // J2: reachability is published by the same acquire that takes the
            // lease, so a live row always names the current holder's address —
            // there is no window in which a refused racer can read a leased row
            // with no endpoint in it.
            Some(endpoint) => LeaseHolder::for_this_process(&agent).reachable_at(endpoint.clone()),
            None => LeaseHolder::for_this_process(&agent),
        };
        let lease_token = match store
            .acquire_lease(&session, &lease_holder, LEASE_TTL)
            .await
            .map_err(LamboError::Store)?
        {
            // Capture the monotonic fencing token (GitHub issue #1) the store
            // minted for this holder. A refresh PRESERVES it, so the value is
            // stable for the handle's life; every durable write (flush + canon)
            // presents it and the store rejects a stale one after a takeover.
            LeaseOutcome::Acquired(info) => {
                // J6 — the pre-arm, HERE, the first statement after the lease
                // is ours. Everything from this point on holds something a
                // signal must not be allowed to destroy: the lease itself, and
                // shortly the write-behind tail. Before this, the whole span
                // down to `holder_shutdown` in `serve` ran under the default
                // disposition, so a SIGTERM in it killed the process with
                // `close()` un-run — CI run 32710994512.
                //
                // It cannot move above the acquire: the election loop that
                // calls this may legitimately run for `ELECTION_BUDGET` (20s),
                // and a registration nothing polls is deafness for exactly as
                // long as nothing polls it (J2-R1-7). Below the acquire, that
                // loop is over by construction.
                //
                // Synchronous and non-blocking: it installs the handlers and
                // returns, adding no `await` to the acquire it follows.
                if let Some(early) = &self.early_shutdown {
                    early.arm();
                }
                info.token
            }
            LeaseOutcome::Held { current, age } => {
                // Fail closed, naming the current holder and its age. Reported
                // as data (J2) so `mcp::serve` can proxy to the holder; the
                // message is byte-identical to what `build` has always returned,
                // and `build` still returns it as `LamboError::Conflict`.
                let message = format!(
                    "session {session} is already held by another writer ({}) — it acquired the \
                     single-writer lease {}s ago and {STILL_REFRESHING_CLAUSE}. Refusing to open \
                     a second writer. If that holder is wedged, an operator can force a takeover \
                     (see the single-writer lease note in docs/reference/cli.mdx)",
                    current.holder,
                    age.as_secs(),
                );
                return Ok(Attach::Held(Box::new(LeaseHeldElsewhere {
                    message,
                    current,
                    age,
                    store,
                })));
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
            let write_intents = loaded.write_intents;
            let mut graph = loaded.graph;

            // (2) Level B / STORE-1 — the model-mixing refusal's second half.
            // `None` on a fresh session is not a mismatch — it is an unstamped
            // space, so stamp it.
            //
            // K2: reembed_mode (the `lambo re-embed` migration) skips this
            // entire check and loads the session AS-IS — old contract, old
            // vectors. That is safe precisely because that caller immediately
            // rewrites every concept vector into the live space and swaps the
            // contract in one ordered batch (`Graph::reembed_all`), which the
            // final flush persists transactionally; nothing is ever relabelled.
            // Without the skip, build_attach would refuse the very attach the
            // migration exists to perform (cross-kind + vectors present).
            if !self.reembed_mode {
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
            }
            Ok::<_, LamboError>((existing, graph, loaded.index, write_intents))
        };
        // J6 — the one unbounded `await` under the pre-arm, and therefore the
        // one place the pre-arm could have become the immunity J2-R1-7
        // rejected. The startup load reads the whole durable session back; on a
        // large session or a wedged store that is not quick, and a recorded
        // signal that nobody acts on until it finishes is deafness with no
        // bound at all — worse than the 20 seconds that ruling refused.
        //
        // So the load is RACED against the record. A signal here abandons it
        // and falls into the startup-error path immediately below, which
        // already releases the freshly-acquired lease — strictly better than
        // the bare kill it replaces, which left the row to lapse at
        // `LEASE_TTL` and wedged the session for that long.
        //
        // Everything under the pre-arm after this point is synchronous (the
        // three task spawns, the attach log, the write pipeline, the `Memory`
        // construction, the return through `resolve_role`), so this is the last
        // place a signal can be parked across. The pre-arm covers no unbounded
        // wait.
        let startup = match &self.early_shutdown {
            Some(early) => {
                tokio::pin!(startup);
                tokio::select! {
                    // Bias toward the load: if it is already done, take that
                    // answer rather than a signal delivered in the same poll.
                    biased;
                    loaded = &mut startup => loaded,
                    () = early.fired() => Err(LamboError::Config(format!(
                        "session {session}: a shutdown signal arrived while the session was \
                         still loading, before it was ever attached — the startup load was \
                         abandoned and the single-writer lease released. Nothing was written \
                         and nothing was lost; start again when you are ready."
                    ))),
                }
            }
            None => startup.await,
        };
        let (existing, graph, index, write_intents) = match startup {
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
        // Issue #30: the read paths note accesses here; the daemon cycle
        // applies them through the graph's write path, and `close` applies
        // the remainder before its final drain.
        let accesses = Arc::new(AccessLedger::new());
        let daemon = Daemon::from_config(graph.clone(), &config)
            .with_index(index.clone())
            .with_access_ledger(accesses.clone());
        // CONC-3: subscribe BEFORE spawn or the warm-up condition set is lost.
        let startup_events = daemon.events();

        // Single-writer-lease fence (T86-2): shared by the flush loop and the
        // lease heartbeat. Latched `true` the instant the heartbeat detects the
        // lease was lost; from then the flush loop stops (and drops its tail)
        // and the write gate refuses. Created before both consumers.
        let lease_lost = Arc::new(AtomicBool::new(false));
        // JE2E-4: the wake-up and the winner's id, latched beside the fence.
        let lease_lost_signal = Arc::new(LeaseLostSignal::default());

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
            lease_lost_signal.clone(),
        );

        tracing::info!(
            session = %session,
            agent = %agent,
            existing,
            match_strategy = ?config.match_strategy,
            // Beside `match_strategy` for the same reason it is here: both are
            // enum-valued `Config` knobs a process file or an environment
            // variable can select, and an operator reading one line must be
            // able to see which value actually won. `promotion_policy`
            // especially — a `lambo.toml` saying `Solo` under a stale
            // `LAMBO_PROMOTION_POLICY=Swarm` is a correct, silent override
            // whose only other evidence is days of absent canonization events.
            promotion_policy = %config.promotion_policy,
            embedder = %embedding.kind,
            dim = embedding.dim,
            "Memory session attached (daemon + flush + canonization running)"
        );
        // J3's background write pipeline. Built here, with `Arc` clones of the
        // shared state its workers need and deliberately NOT a handle on the
        // `Memory` being constructed — see `WriteCtx`. `spawn` also starts the
        // calibration probe that measures this deployment's embedder; it is
        // spawned rather than awaited, so it costs this startup nothing.
        let pipeline = Arc::new(WritePipeline::spawn(
            WriteCtx {
                session: session.clone(),
                graph: graph.clone(),
                index: index.clone(),
                store: store.clone(),
                embedder: embedder.clone(),
                embedding: embedding.clone(),
                match_strategy: config.match_strategy,
                max_cooccurrence_per_derive: config.max_cooccurrence_per_derive,
                semantic_match_threshold: config.semantic_match_threshold,
                daemon_wake: daemon.waker(),
                lease_lost: lease_lost.clone(),
                ledger: self.ledger,
            },
            clock.clone(),
        ));

        // J3 durable intents: replay whatever a previous process acked and
        // could not apply — one at a time, in admission order, concurrent with
        // (never ahead of) this session's own calls. Also seeds the
        // cross-restart receipt answers, so those ids answer
        // `pending`/`applied_after_restart`/`failed` instead of
        // `restart_lost`.
        pipeline.spawn_replay(write_intents);

        // Last, so nothing registers for a `build()` that failed. Released by
        // a successful `close()` (R2-4) or, failing that, by `Drop` (T81-8).
        register_session(&session, &agent);

        Ok(Attach::Attached(Box::new(Memory {
            pipeline,
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
            accesses,
            writers: AsyncRwLock::new(()),
            close_state: tokio::sync::Mutex::new(false),
            closed: AtomicBool::new(false),
            registered: AtomicBool::new(true),
            lease_holder,
            lease_token,
            lease_released: AtomicBool::new(false),
            lease_lost,
            lease_lost_signal,
            clock,
        })))
    }
}

// ---------------------------------------------------------------------------
// Memory
// ---------------------------------------------------------------------------

/// An attached session: graph + index + store + embedder, three background
/// tasks, and (since J3) the asynchronous write pipeline.
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
///     event_time: None,
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
    /// Read accesses noted by [`Memory::recall`] and the MCP inspect focus,
    /// not yet applied to the graph (issue #30). Shared with the daemon, whose
    /// cycle applies them; [`Memory::close`] applies the remainder in the
    /// final drain's write section. A leaf lock: never held while another lock
    /// is taken. See [`crate::daemon::access`] for what counts and why.
    accesses: Arc<AccessLedger>,
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
    /// The serve-facing half of that fence (JE2E-4): a wake-up and the winner's
    /// id. See [`LeaseLostSignal`], and [`Memory::lease_lost_latched`] for what
    /// waits on it.
    lease_lost_signal: Arc<LeaseLostSignal>,
    /// **J3's background write pipeline** — bounded per-agent FIFO lanes and
    /// the receipt store their outcomes land in. Always present: there is no
    /// "async writes off" mode, because the two tools that use it
    /// (`lambo_derive`, `lambo_record_action`) are the two whose result does
    /// not gate the caller's next action.
    ///
    /// The synchronous [`Memory::derive`] / [`Memory::record_action`] surface
    /// does **not** go through it and is unchanged. That is deliberate rather
    /// than transitional: an owner that derives and then asserts — every
    /// embedded user, `lambo demo`, and most of this file's own tests — depends
    /// on read-your-writes, and silently removing it from the existing surface
    /// would be a far larger change than J3 is. The async path is additive.
    ///
    /// `Arc` because the workers, the receipt waiters and the MCP server all
    /// hold it, and because [`Memory::close`] must reach it before it takes the
    /// writers gate.
    pipeline: Arc<WritePipeline>,
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

    /// This handle's **default** agent id: the one stamped on writes made
    /// through the plain write methods ([`Memory::derive`],
    /// [`Memory::record_action`], [`Memory::reserve`], [`Memory::release`],
    /// [`Memory::demote`]), and the id this handle registered in
    /// `ACTIVE_SESSIONS` and holds the single-writer lease under.
    ///
    /// It is **not** "the only agent this handle can write as". Since J1 every
    /// write method has an `_as` twin taking the acting agent per call
    /// ([`Memory::derive_as`] and friends), so one handle serves many agents —
    /// which is what lets one `lambo serve` process host several MCP clients
    /// without falsifying any of their identities. Callers wanting "who wrote
    /// this" must read the interaction's `agent_id`, not this accessor.
    ///
    /// Process-level identity (lease holder, `ACTIVE_SESSIONS` key, heartbeat
    /// lines) is still exactly this one id: per-call ids name *writers*, never
    /// lease holders.
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

    /// The resolved embedder (single construction site — do not build
    /// another). `lambo serve`'s keep-warm task (issue #13) touches the very
    /// instance this session embeds with.
    pub(crate) fn embedder(&self) -> &Arc<dyn Embedder> {
        &self.embedder
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

    /// Record one explicit human confirmation of a concept (C2, spec §3.2's
    /// "Human Confirmed" term — the solo score's `Human Confirmed × 4.0`).
    ///
    /// ## What "human confirmed" means operationally
    ///
    /// A **deliberate human verdict on one concept**, carried by a dedicated
    /// verb rather than inferred: no agent write path (`derive`,
    /// `record_action`, recall) reaches this counter, so agent activity cannot
    /// inflate the heaviest term in the solo formula. Each call bumps the
    /// concept's `human_confirmed` count by one and appends an `UpsertNode`
    /// mutation, so the value is durable on every store adapter. Surfacing the
    /// verb on MCP/CLI is deliberately deferred: unlike `event_time`, whose MCP
    /// wire form now has a historical-ingest consumer, `confirm_human` still
    /// needs a consumer-designed wire contract. The in-process API remains the
    /// contract future confirm tooling will call.
    ///
    /// A missing id or a non-concept node is an error — a confirmation that
    /// cannot be applied must fail loudly, never silently vanish.
    pub fn confirm_human(&self, node: NodeId) -> Result<i32, LamboError> {
        let _writing = self.begin_write_sync()?;
        let confirmed = self.graph.write().confirm_human(node)?;
        self.daemon.wake();
        Ok(confirmed)
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
        self.derive_as(&self.agent, concepts, parent_of).await
    }

    /// [`Memory::derive`] on behalf of `agent` (J1).
    ///
    /// The acting agent lands on the interaction this call opens and on every
    /// `Provenance` edge below it, so "who derived this" survives into the
    /// graph rather than being flattened to the handle's own id. Everything
    /// else — validation, canonicalization, the write-behind log, the
    /// single-writer lease and its fencing token — is unchanged and still
    /// process-wide: this parameter names the *writer*, not a second session.
    ///
    /// Identity is whatever the caller passed. Over MCP that is caller-asserted
    /// and unauthenticated (see `lambo_reserve`'s tool doc), which is exactly
    /// the trust level lambo's soft locks already assume.
    pub async fn derive_as(
        &self,
        agent: &AgentId,
        concepts: &[(&str, ConceptType)],
        parent_of: &ParentOf<'_>,
    ) -> Result<DeriveOutcome, LamboError> {
        self.derive_for_ingest_as(agent, None, concepts, parent_of)
            .await
    }

    /// [`Memory::derive`] for a fact whose about-time the caller knows (D).
    ///
    /// This is the historical-corpus entry point: `event_time` — a commit
    /// date, a transcript timestamp — is carried on the interaction this call
    /// opens and inherited by every concept and edge derived under it. Flush
    /// time stays process-stamped exactly as in [`Memory::derive`]; F18's
    /// server-authority rule is about *observed-at* claims, not about-time,
    /// which no store-side clock could know. Canonization's age floors,
    /// coverage bar and session separation then measure the replayed history
    /// on its own timeline; see `crate::canon::event_time`.
    ///
    /// Passing `None` is exactly [`Memory::derive`]: the fallback rule makes
    /// the interaction behave as if D never happened.
    pub async fn derive_for_ingest(
        &self,
        event_time: DateTime<Utc>,
        concepts: &[(&str, ConceptType)],
        parent_of: &ParentOf<'_>,
    ) -> Result<DeriveOutcome, LamboError> {
        self.derive_for_ingest_as(&self.agent, Some(event_time), concepts, parent_of)
            .await
    }

    /// [`Memory::derive_for_ingest`] on behalf of `agent` (J1), with an
    /// explicit `Option` so an ingester can mix timestamped turns with live
    /// ones through one seam.
    pub async fn derive_for_ingest_as(
        &self,
        agent: &AgentId,
        event_time: Option<DateTime<Utc>>,
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
        let interaction = self.begin_interaction_full(agent, Some(prompt), event_time)?;

        let outcome = match self.config.match_strategy {
            MatchStrategy::Hybrid => {
                hybrid::derive(
                    self.graph.clone(),
                    self.store.as_ref(),
                    self.embedder.as_ref(),
                    &self.embedding,
                    interaction,
                    agent,
                    concepts,
                    parent_of,
                    self.config.max_cooccurrence_per_derive,
                    self.config.semantic_match_threshold,
                    // The synchronous path has no durable intent to consume —
                    // the caller holds the outcome directly (J3).
                    None,
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
                    agent,
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
        self.record_action_as(&self.agent, action)
    }

    /// [`Memory::record_action`] on behalf of `agent` (J1). See
    /// [`Memory::derive_as`] for what the per-call id does and does not change.
    ///
    /// The action's own `event_time` (D) — when it carries one — stamps the
    /// interaction opened for this call, and through it every edge the call
    /// creates. Flush time is still process-stamped here, never caller-set.
    pub fn record_action_as(
        &self,
        agent: &AgentId,
        action: &Action<'_>,
    ) -> Result<ActionOutcome, LamboError> {
        let _writing = self.begin_write_sync()?;
        let interaction =
            self.begin_interaction_full(agent, Some(action.action.to_string()), action.event_time)?;
        let outcome = {
            let mut g = self.graph.write();
            graph_record_action(&mut g, interaction, agent, action)?
        };

        // The action node may be pre-existing (already indexed) — mirroring it
        // anyway is idempotent and covers the case where it is not.
        let mut touched = outcome.created.clone();
        touched.push(outcome.action_node);
        self.mirror_concepts(&touched);
        self.daemon.wake();
        Ok(outcome)
    }

    /// [`Memory::record_action`] with an embedder hop, so the concepts it
    /// creates are findable by semantic recall and not only by keyword.
    ///
    /// **Async because embedding is I/O**, which is precisely why
    /// [`Memory::record_action`] never did it: that entry point is
    /// synchronous, and a sync signature has nowhere to put a model call. The
    /// cost of the omission was measured on the dogfood session 2026-09-01 —
    /// 555 of 946 concepts with no vector, every one of them from an action —
    /// so the sync path is now the deliberate keyword-only choice rather than
    /// the default one. Callers holding a runtime should prefer this.
    ///
    /// Under [`MatchStrategy::Canonical`] this is exactly
    /// [`Memory::record_action_as`]: that strategy has no vector leg, and
    /// embedding here would stamp a contract on a session that asked for none.
    /// Under `Hybrid` on a store without `VECTOR_SEARCH` nothing is embedded
    /// either, the same degrade hybrid `derive` makes.
    ///
    /// An embedder failure fails the call with **nothing written** (J3-R3-1's
    /// rule, see [`crate::graph::action::embed_action_contents`]).
    pub async fn record_action_embedded_as(
        &self,
        agent: &AgentId,
        action: &Action<'_>,
    ) -> Result<ActionOutcome, LamboError> {
        if self.config.match_strategy == MatchStrategy::Canonical {
            return self.record_action_as(agent, action);
        }
        // Held across the embed await below, so a concurrent `close()` either
        // waits for this whole call or refuses it (T81-1), matching `derive`.
        let _writing = self.begin_write().await?;
        {
            let g = self.graph.read();
            crate::graph::action::validate(&g, action)?;
        }
        // Off-lock: real model calls. Skipped when the store cannot search
        // vectors, as hybrid `derive` skips them: the write is keyword-only.
        let embeddings = if self
            .store
            .capabilities()
            .contains(Capabilities::VECTOR_SEARCH)
        {
            crate::graph::action::embed_action_contents(self.embedder.as_ref(), action).await?
        } else {
            crate::graph::action::ActionEmbeddings::new()
        };
        let interaction =
            self.begin_interaction_full(agent, Some(action.action.to_string()), action.event_time)?;
        let outcome = {
            let mut g = self.graph.write();
            if !embeddings.is_empty() {
                g.stamp_embedding(self.embedding.clone())?;
            }
            crate::graph::action::record_action_with_embeddings(
                &mut g,
                interaction,
                agent,
                action,
                &embeddings,
            )?
        };
        let mut touched = outcome.created.clone();
        touched.push(outcome.action_node);
        self.mirror_concepts(&touched);
        self.daemon.wake();
        Ok(outcome)
    }

    /// The J3 write pipeline and its receipt store.
    ///
    /// The MCP server needs it for the two delivery surfaces the pipeline
    /// deliberately does not own — the piggyback on the next tool response and
    /// the fetch-by-id tool — and `lambo_stats` needs its counters.
    pub fn pipeline(&self) -> &Arc<WritePipeline> {
        &self.pipeline
    }

    /// [`Memory::derive_as`] **acknowledged before the embedder** (J3).
    ///
    /// What stays synchronous, and why each part does:
    ///
    /// * The **writers gate**, so a concurrent `close()` cannot slip between
    ///   the checks below and the enqueue.
    /// * The **validation pre-pass**, so the errors a caller can actually fix
    ///   still arrive at call time rather than on a receipt. It is **the
    ///   pre-pass the session's `match_strategy` actually uses**, and the two
    ///   are not the same set of rules:
    ///   * `Hybrid` (the default — see `config.rs`): `hybrid::validate_limits`
    ///     then `hybrid::validate_graph_inputs` and
    ///     `hybrid::validate_embed_budget`, which is deliberately the
    ///     **smaller** set. It omits the repeated-`Observation` and
    ///     single-`Hierarchical`-parent rejections, because hybrid's own write
    ///     path does not enforce them and validation that disagrees with the
    ///     write is worse than none (defect 3 in §J3 Status).
    ///   * `Canonical`: `hybrid::validate_limits` then
    ///     [`crate::graph::derive::validate`], the read-only half of the
    ///     synchronous path — the same checks against the same graph in the
    ///     same order.
    ///
    ///   Under `Hybrid`, then, five error classes move from call time to the
    ///   receipt, and all five need the embedder or the store: embedder
    ///   failure, an embedder dim/contract mismatch,
    ///   [`crate::graph::hybrid::HYBRID_IO_TIMEOUT`] expiry,
    ///   `MAX_HYBRID_REPLANS` exhaustion, and store errors from the vector
    ///   candidate check. Nothing a *caller* could act on moved, and no rule
    ///   was removed from the write path: `validate_graph_inputs` still runs
    ///   inside `derive_planned`'s phase 1. Under `Canonical` nothing moves.
    ///   (The bullet this replaces claimed "the same checks, run against the
    ///   same graph, in the same order" for every strategy, three lines above
    ///   the comment correcting it — J3-R1-4.)
    /// * The **interaction**, which pins this write's place in the `Temporal`
    ///   chain at submission time. That is why the chain cannot be corrupted by
    ///   an out-of-order drain: the drain no longer decides the order. Scoped to
    ///   *sequential* calls from one agent — for two this agent has in flight at
    ///   once, this line and the queue's own enqueue are separate critical
    ///   sections and can disagree (J3-R1-10, and see `writeq`'s §Ordering).
    ///
    /// What moves off the call path is the embedder wait — 22 to 25 ms of a
    /// warm 27 ms `derive` — not the 0.4 ms round trip, which is not worth
    /// removing.
    ///
    /// D's optional **event time** rides the same seam as on
    /// [`Memory::record_action_async_as`]: the interaction is opened
    /// synchronously at submit (before the job is queued), so the parameter is
    /// stamped straight into that interaction via `begin_interaction_full` and
    /// every edge the queued derive later creates inherits it. `None` is a live
    /// fact (fallback rule: about-time = created-at).
    ///
    /// Returns the receipt. **A refused admission is not an `Err`**: the
    /// receipt carries [`crate::writeq::ReceiptAnswer::Dropped`] and the drop
    /// is counted in `lambo_stats`. An `Err` here means the call was rejected
    /// before a receipt existed — a closed or fenced session, or input the
    /// pre-pass refused.
    pub async fn derive_async_as(
        &self,
        agent: &AgentId,
        concepts: &[(&str, ConceptType)],
        parent_of: &ParentOf<'_>,
        event_time: Option<DateTime<Utc>>,
    ) -> Result<Submitted, LamboError> {
        let _writing = self.begin_write().await?;
        // The pre-pass, on the call path. The background path re-runs its own
        // planning validation; this one exists so the common errors do not have
        // to be collected from a receipt.
        //
        // **The pre-pass must be the one the strategy actually uses.** Hybrid's
        // and the synchronous path's are different sets of rules — hybrid omits
        // the repeated-`Observation` and single-`Hierarchical`-parent
        // rejections — so running the wrong one here would refuse writes the
        // background path would have accepted, and validation that disagrees
        // with the write is worse than none.
        hybrid::validate_limits(concepts, parent_of, self.config.semantic_match_threshold)?;
        {
            let g = self.graph.read();
            match self.config.match_strategy {
                MatchStrategy::Hybrid => {
                    hybrid::validate_graph_inputs(&g, parent_of)?;
                    // The embed budget too: an over-budget call is refused
                    // here, not after the ack as a timeout at apply.
                    hybrid::validate_embed_budget(
                        &g,
                        concepts,
                        parent_of,
                        self.store
                            .capabilities()
                            .contains(Capabilities::VECTOR_SEARCH),
                    )?;
                }
                MatchStrategy::Canonical => {
                    crate::graph::derive::validate(&g, concepts, parent_of)?
                }
            }
        }
        let prompt = concepts
            .iter()
            .map(|(content, _)| *content)
            .collect::<Vec<_>>()
            .join("; ");
        let interaction = self.begin_interaction_full(agent, Some(prompt), event_time)?;
        Ok(self
            .pipeline
            .submit_derive(
                agent.clone(),
                interaction,
                concepts
                    .iter()
                    .map(|(c, t)| ((*c).to_string(), *t))
                    .collect(),
                parent_of
                    .pairs()
                    .iter()
                    .map(|(a, b)| ((*a).to_string(), (*b).to_string()))
                    .collect(),
            )
            .await)
    }

    /// [`Memory::record_action_as`] acknowledged before the graph write (J3).
    ///
    /// `record_action` has no embedder hop of its own, so what asynchrony buys
    /// here is not latency but **ordering with `derive`**: both tools feed one
    /// per-agent lane, so an agent that records an action and then derives from
    /// it gets them applied in that order. Routing only `derive` through the
    /// queue would have let a later synchronous `record_action` overtake an
    /// earlier queued `derive` on the same agent's chain.
    ///
    /// The interaction — and with it the action's optional D `event_time` — is
    /// pinned at submit time exactly as in [`Memory::record_action_as`].
    ///
    /// See [`Memory::derive_async_as`] for what stays on the call path.
    pub async fn record_action_async_as(
        &self,
        agent: &AgentId,
        action: &Action<'_>,
    ) -> Result<Submitted, LamboError> {
        let _writing = self.begin_write().await?;
        {
            let g = self.graph.read();
            crate::graph::action::validate(&g, action)?;
        }
        let interaction =
            self.begin_interaction_full(agent, Some(action.action.to_string()), action.event_time)?;
        Ok(self
            .pipeline
            .submit_action(
                agent.clone(),
                interaction,
                action.action.to_string(),
                action.produces.iter().map(|s| (*s).to_string()).collect(),
                action.modifies.iter().map(|s| (*s).to_string()).collect(),
                action.depends_on.iter().map(|s| (*s).to_string()).collect(),
            )
            .await)
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
    /// (spec §11). Cross-agent contention returns [`LamboError::SoftLock`].
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
        self.reserve_as(&self.agent, node, ttl)
    }

    /// [`Memory::reserve`] on behalf of `agent` (J1) — the call that makes soft
    /// locks work for more than one client of one process.
    ///
    /// Contention is now genuine: two distinct ids reserving one node produce a
    /// [`LamboError::SoftLock`] for the second, and [`Memory::release_as`]
    /// refuses an id that does not hold the lock. Two callers passing the *same*
    /// id share one lock and can release each other's — cooperative by design,
    /// and the MCP layer says so in the tool description. Nothing here
    /// authenticates `agent`; a soft lock never did.
    pub fn reserve_as(
        &self,
        agent: &AgentId,
        node: NodeId,
        ttl: Duration,
    ) -> Result<Reservation, LamboError> {
        let _writing = self.begin_write_sync()?;
        let mut g = self.graph.write();
        graph_reserve(&mut g, node, agent, ttl, Utc::now())
    }

    /// Release this agent's soft lock on `node` — the pair of
    /// [`Memory::reserve`]. A non-owner gets [`LamboError::SoftLock`].
    pub fn release(&self, node: NodeId) -> Result<(), LamboError> {
        self.release_as(&self.agent, node)
    }

    /// [`Memory::release`] on behalf of `agent` (J1). A caller that does not
    /// hold the lock under this id gets [`LamboError::SoftLock`] and the lock
    /// stands — which is what stops one client dropping another's lock.
    ///
    /// The gate comes first, so a fenced handle fails here with a
    /// [`LamboError::Conflict`] — a *different* variant, deliberately, because
    /// its message is operator-only and `mcp::server` must be able to tell the
    /// two apart without reading either (J1-R2-2).
    pub fn release_as(&self, agent: &AgentId, node: NodeId) -> Result<(), LamboError> {
        let _writing = self.begin_write_sync()?;
        let mut g = self.graph.write();
        graph_release(&mut g, node, agent)
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
        self.recall_detailed(query).await.map(Into::into)
    }

    /// [`Memory::recall`], keeping the H3 presentation model of the SAME
    /// execution instead of the flattened projection.
    ///
    /// `recall` is now a projection of this, so there is exactly one recall
    /// implementation on the `Memory` surface — no second execution, and no way
    /// for the detailed and flattened views to describe different runs.
    ///
    /// The MCP `lambo_recall` handler calls this so the I1 call ledger can
    /// record final **and** per-leg scores plus the typed warning kinds that
    /// actually rendered. Its response to the client is built from the
    /// projection and is byte-identical to what `recall` produced before.
    pub(crate) async fn recall_detailed(
        &self,
        query: RecallQuery,
    ) -> Result<crate::recall::detail::DetailedRecall, LamboError> {
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
            .recall_detailed(
                &self.session,
                query,
                self.store.as_ref(),
                embedding.as_deref().map(|vector| (vector, &self.embedding)),
                self.config.recall_weights,
                &mut cache,
            )
            .await;
        drop(cache);

        // Issue #30: every hit returned to the caller is an access — cache-
        // served or not, inside the token budget or not (the wire carries every
        // hit's content). Noted after the pipeline released the graph lock;
        // non-concept hits are skipped when the daemon applies the batch.
        self.note_accesses(result.hits.iter().map(|h| h.node_id));

        warnings.append(&mut result.warnings);
        result.warnings = warnings;
        Ok(result)
    }

    /// Note that a read returned `ids` to a caller (issue #30). Cheap and
    /// lock-light: one leaf-mutex section, no graph lock, no I/O. The counts
    /// reach the graph — and, through the normal flush, the store — on the
    /// daemon's next cycle, without advancing the mutation epoch.
    ///
    /// Stamped from the wall clock, deliberately **not** from `self.clock`:
    /// that seam is the interaction stamp, and `lambo demo`'s script clock
    /// advances one step per call, so reading it here would shift every later
    /// interaction of the script by each recall it retried. Scoring takes the
    /// later of `created_at` and `last_accessed` as the last touch, so a read
    /// stamped before a scripted `created_at` cannot age a concept.
    ///
    /// A read that completes after [`Memory::close`] took the ledger is not
    /// counted: the ledger is closed then and drops it (see
    /// [`AccessLedger::close`]). "A clean close loses nothing" means nothing
    /// noted before that point.
    pub(crate) fn note_accesses(&self, ids: impl IntoIterator<Item = NodeId>) {
        self.accesses.record(ids, Utc::now());
    }

    /// The canonical ("saints") memories — spec §10's `Canonical` nodes.
    ///
    /// A graph scan, deliberately: canonization status lives on the concept and
    /// no store query for it exists or is needed. The graph is the primary tier
    /// (spec §2.1), so it is also the freshest answer.
    ///
    /// Ordered blast-radius descending, then oldest first, then the issue-2
    /// tie-break (canonical key ascending, node id ascending behind it):
    /// total and deterministic, and stable across runs because the tie is
    /// decided by the persisted key, not the per-run id.
    pub fn canonical_memories(&self) -> Vec<CanonicalMemory> {
        let g = self.graph.read();
        let radii = format::blast_radii(&g);
        let mut out: Vec<CanonicalMemory> = g
            .concepts()
            .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
            .map(|c| CanonicalMemory {
                node_id: c.id,
                canonical_key: c.canonical_key.clone(),
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
                .then(tie_break_by_key(
                    Some(&a.canonical_key),
                    &a.node_id,
                    Some(&b.canonical_key),
                    &b.node_id,
                ))
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
        let embedded_concepts = g.concepts().filter(|c| c.embedding.is_some()).count();
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
            embedded_concepts,
            epoch: g.epoch(),
            daemon_cycles: self.daemon.cycles(),
            canonization_cycles: self.canon.cycles(),
            canonization_failures: self.canon.failures(),
        };
        drop(g);
        stats
    }

    /// GC's sweep accounting (issue #29): the durable mark from the graph and
    /// the last sweep this process ran. Read-only; see [`GcStats`].
    pub fn gc_stats(&self) -> GcStats {
        let mark = self.graph.read().gc_mark();
        GcStats {
            last_gc_at: mark.last_gc_at,
            last_gc_epoch: mark.last_gc_epoch,
            last_sweep: self.daemon.last_gc().map(|o| GcSweepSummary {
                trigger: o.trigger,
                collected: o.concepts_collected.len(),
                deferred: o.collections_deferred,
                collection_cap: o.collection_cap,
                cap_bound: o.cap_bound(),
                resources_spared_by_dependents: o.resources_spared_by_dependents,
                survivors_deferred: o.survivors_pending.len(),
            }),
        }
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
    ///    `closed` so new calls are refused, drain J3's asynchronous write
    ///    queue (see the note at the end of this step), take the write side of
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
    ///
    ///    **J3's write pipeline is drained inside this step, and BEFORE the
    ///    gate is taken** (`crate::writeq::WritePipeline::quiesce`). The
    ///    order is forced, not chosen: the gate's write side is held for the
    ///    rest of this method, so a background worker that had to pass through
    ///    the gate could never finish and a `close` waiting for it would
    ///    deadlock. The workers therefore never touch the gate — latching
    ///    `closed` is what stops new jobs, and the quiesce is what makes
    ///    "nothing new lands after the drain" true of the workers. Bounded by
    ///    [`crate::writeq::WRITE_QUEUE_DRAIN_BUDGET`]; anything still
    ///    outstanding is abandoned (aborted **and joined**), its receipt
    ///    settled `failed`, and counted in `lambo_stats`'
    ///    `write_queue_abandoned`.
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
    /// one [`crate::writeq::WRITE_QUEUE_DRAIN_BUDGET`] for the write-queue
    /// quiesce inside step 0, plus one `FLUSH_ATTEMPT_TIMEOUT` for step 4. That
    /// quiesce budget is *carved out of*
    /// the window `serve` gives `close` rather than added to it (see that
    /// constant), so this does not move the number an operator has sized a
    /// supervisor timeout against.
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

        // 0a — J3's background write queue, drained BEFORE the writers gate is
        // taken. The order is forced, not chosen: the gate's write side is held
        // for the rest of this method, so a worker that had to pass through the
        // gate could never finish and a close waiting for it would deadlock.
        // The workers therefore do not use the gate; latching `closed` above is
        // what stops new jobs (the enqueue path is a gated write), and this
        // quiesce is what makes "nothing new lands after the drain" true of the
        // workers. Bounded by `WRITE_QUEUE_DRAIN_BUDGET`; anything left over is
        // abandoned with an honest receipt rather than waited for.
        self.pipeline.abort_probe();
        // The intent replay is stopped — aborted AND joined — before the
        // quiesce and therefore well before the final drain: an aborted task
        // can still finish a synchronous stretch that appends to the log until
        // the join returns (R3-1). Whatever it had not yet consumed stays
        // durable for the next serve.
        self.pipeline.stop_replay().await;
        self.pipeline.quiesce().await;

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
        // Accesses noted since the daemon's last cycle (it is stopped now) are
        // applied in the same section, and every access the flush had not yet
        // taken from the graph's dirty set rides the tail after the log, so a
        // clean close loses none (issue #30). The ledger is taken before the
        // graph lock: it stays a leaf. `close` (not `take`) shuts it in the
        // same critical section, so a recall still in flight that finishes
        // after this point is dropped explicitly instead of noting into a
        // ledger nothing will apply again.
        let accesses = self.accesses.close();
        let batch = {
            let mut g = self.graph.write();
            g.record_accesses(&accesses);
            let mut batch = g.drain_log();
            batch.mutations.extend(g.drain_accesses(usize::MAX));
            batch
        };
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
        self.begin_interaction_full(&self.agent, prompt, None)
    }

    /// The one interaction-opening seam, with D's optional **event time**.
    ///
    /// F18's rule guards flush time only: `created_at` remains process-stamped
    /// no matter what arrives here. `event_time` is a different concept — the
    /// instant the fact is *about* (a commit date, transcript timestamp), not
    /// an observation claim about the present — and it is stored verbatim as
    /// [`Interaction::event_time`] (`None` = live fact, fallback rule). Every
    /// edge the write creates inherits this interaction's about-time at
    /// creation, so one parameter stamps the whole turn.
    fn begin_interaction_full(
        &self,
        agent: &AgentId,
        prompt: Option<String>,
        event_time: Option<DateTime<Utc>>,
    ) -> Result<NodeId, LamboError> {
        let id = NodeId::new();
        let created_at = (self.clock)();
        let mut g = self.graph.write();
        let previous_id = g.temporal_chain().last().copied();
        g.insert_interaction(Interaction {
            id,
            session_id: self.session.clone(),
            agent_id: agent.clone(),
            prompt_text: prompt,
            previous_id,
            created_at,
            event_time,
        })?;
        Ok(id)
    }

    /// Mirror concept writes into the inverted index (the `src/graph/mod.rs`
    /// contract).
    ///
    /// The body lives in [`crate::writeq::mirror_concepts`] — where the lock
    /// order that makes it safe is documented — because J3's background workers
    /// hold `Arc` clones of the graph and index rather than a `Memory`, and two
    /// copies of a lock-order rule is two chances to get it wrong.
    fn mirror_concepts(&self, ids: &[NodeId]) {
        crate::writeq::mirror_concepts(&self.graph, &self.index, ids);
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

    /// Issue #30 test hook: read accesses noted but not yet applied by the
    /// daemon. Lets a test assert the premise "the daemon has NOT applied
    /// these" right before a `close`, so the test exercises close's own apply
    /// rather than racing the daemon. Test-only, gated like the hook above.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(crate) fn unapplied_accesses(&self) -> usize {
        self.accesses.pending()
    }

    /// Issue #30 test hook: wait until the daemon has rescored the current
    /// epoch and has no cycle left to run (a wake that arrived mid-cycle leaves
    /// one stored permit, so "settled" means the cycle count stopped moving).
    /// With a long `daemon_tick_interval` nothing runs a cycle after this
    /// until something wakes the daemon again — and reads never do.
    #[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
    pub(crate) async fn settle_daemon(&self) {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let cycles = self.daemon.cycles();
                tokio::time::sleep(Duration::from_millis(20)).await;
                if self.daemon.cycles() == cycles
                    && self.daemon.scores().epoch == self.graph.read().epoch()
                {
                    return;
                }
                if self.daemon.scores().epoch != self.graph.read().epoch() {
                    self.daemon.wake();
                }
            }
        })
        .await
        .expect("the daemon settles within 10 s");
    }

    fn closed_error(&self) -> LamboError {
        LamboError::Config(format!("session {} is closed", self.session))
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
        // J3: the background write workers and the calibration probe hold `Arc`
        // clones of the graph, so a dropped handle's workers would keep writing
        // into a graph nobody will flush. Aborted without a join and without
        // settling their receipts — `Drop` cannot await, and a process that is
        // going away has nobody to answer. This is the same shape as the tail
        // this method warns about: a handle dropped without a clean close loses
        // its un-applied writes, which is exactly what the receipt for one says
        // when the next process is asked (`restart_lost`).
        self.pipeline.abort_all_sync();
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
mod tests;
