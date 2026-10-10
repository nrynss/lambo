//! Construction and attach: [`MemoryBuilder`] and the [`Attach`] outcome.
//!
//! `build_attach` is the **only** place a `Memory` is assembled, and its order
//! is load-bearing:
//!
//! 1. validate the merged config, then the schema preflight — before the lease,
//!    so a refusal has nothing to release;
//! 2. acquire the single-writer lease (a held lease is returned as
//!    [`Attach::Held`], data rather than an error); arm the serve's early
//!    shutdown in the `Acquired` arm and nowhere else (J6);
//! 3. the startup load and the Level B embedding-contract check, raced against
//!    that early shutdown; any error from here releases the fresh lease;
//! 4. spawn the daemon (its first events receiver subscribed before `spawn`),
//!    the flush task and the canonization task, then the lease heartbeat;
//! 5. build the write pipeline (which spawns its calibration probe) and spawn
//!    the durable-intent replay;
//! 6. register the handle in the second-writer registry, last, so a failed
//!    build registers nothing.
//!
//! Everything after step 3 is synchronous, so nothing under the early-shutdown
//! arm waits unboundedly. One builder builds one session; a process hosting
//! many sessions (#32) clones the builder (cheap: `Arc`s) and attaches each.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use parking_lot::{Mutex as PlMutex, RwLock};
use tokio::sync::RwLock as AsyncRwLock;

use super::{final_flush, register_session, spawn_lease_heartbeat, LeaseLostSignal, Memory};
use crate::canon::CanonizationTask;
use crate::config::{Config, ScoringWeights};
use crate::daemon::access::AccessLedger;
use crate::daemon::{Clock, Daemon};
use crate::embed::Embedder;
use crate::recall::cache::RecallCache;
use crate::recall::query_cache::QueryEmbeddingCache;
use crate::resolve::{
    embedding_mismatch_error, session_embedding_compatibility, ResolvedBackends,
    SessionEmbeddingCompatibility,
};
use crate::store::flush::{FlushParams, FlushTask};
use crate::store::lease::{LeaseHolder, LeaseOutcome, LEASE_TTL};
use crate::store::load::load_session_async;
use crate::store::GraphStore;
use crate::types::{AgentId, EmbeddingContract, LamboError, MatchStrategy, SessionId, StoreError};
use crate::writeq::{EmbedderCalibration, WriteCtx, WritePipeline};

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

/// The shutdown pre-arm a serving process hands [`MemoryBuilder::build_attach`]
/// (J6), as the builder needs it: armed at lease acquisition, and raced
/// against the startup load.
///
/// A trait so `memory` does not depend on the serving layer: `serve`
/// implements it (`crate::mcp::serve::EarlyShutdown`), and a process hosting
/// many sessions (#32) can hand every attach the same process-wide handle.
pub(crate) trait AttachShutdown: Send + Sync {
    /// Install the signal handling. Called once, from the
    /// `LeaseOutcome::Acquired` arm and nowhere else; synchronous and
    /// non-blocking, so it adds no `await` to the acquire it follows.
    fn arm(&self);

    /// Resolves once a shutdown has been requested, **immediately** if one
    /// already was.
    fn fired(&self) -> Pin<Box<dyn Future<Output = ()> + Send + '_>>;
}

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
    pub(super) session: Option<SessionId>,
    pub(super) agent: Option<AgentId>,
    pub(super) store: Option<Arc<dyn GraphStore>>,
    pub(super) embedder: Option<Arc<dyn Embedder>>,
    pub(super) embedding: Option<EmbeddingContract>,
    pub(super) allow_embedding_mismatch: bool,
    /// K2. Crate-internal attach mode for the re-embed migration: load the
    /// session WITHOUT the Level B contract check, because this caller is about
    /// to rewrite every vector into the live space atomically (see
    /// [`crate::graph::Graph::reembed_all`]). Set only by `lambo re-embed`;
    /// never relabels anything.
    pub(super) reembed_mode: bool,
    pub(super) config: Config,
    // Held as overrides rather than written straight into `config`, so
    // `.config(..)` and the named setters commute — calling them in either
    // order gives the same session. They are applied in `build`.
    pub(super) match_strategy: Option<MatchStrategy>,
    pub(super) flush_interval: Option<Duration>,
    pub(super) scoring_weights: Option<ScoringWeights>,
    // Crate-private, and not a knob: see `MemoryBuilder::clock`.
    pub(super) clock: Option<Clock>,
    pub(super) endpoint: Option<String>,
    /// J4. An optional call ledger this process appends its own conflict and
    /// write-intent **completion** lines to (pre-lease startup, lease refusals,
    /// proxying/degraded, and durable-intent completion — see
    /// `dev-diary/lambo-for-mooshik/J-multi-client.md` §J4). `None` for every
    /// writer that is not a `serve` and for a `serve` run without `--ledger`.
    /// Set only by [`crate::mcp::serve()`]; every ordinary writer keeps the
    /// default.
    pub(super) ledger: Option<Arc<crate::ledger::Ledger>>,
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
    /// disposition. See [`AttachShutdown`]; `serve`'s implementation is
    /// `crate::mcp::serve::EarlyShutdown`.
    pub(super) early_shutdown: Option<Arc<dyn AttachShutdown>>,
    /// #32 PR 3. The process-wide write-queue calibration this session's
    /// pipeline reads its probe from. `None` (the default) spawns a probe of
    /// the pipeline's own, as every session did before.
    pub(super) calibration: Option<EmbedderCalibration>,
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
    pub(crate) fn early_shutdown(mut self, early: impl AttachShutdown + 'static) -> Self {
        self.early_shutdown = Some(Arc::new(early));
        self
    }

    /// Share the write queue's embedder calibration probe with every other
    /// session in this process (#32 PR 3, design decision 14).
    ///
    /// The probe measures the embedder, so builders over one shared embedder
    /// that are handed clones of one [`EmbedderCalibration`] probe it once:
    /// the first build spawns the probe and every later one fires no probe
    /// embed. Each session's observed rate stays its own. The calibration's
    /// owner aborts the probe ([`EmbedderCalibration::abort`]); a session's
    /// close does not, since other sessions read it.
    ///
    /// Closing every session does not stop a probe still running, and the
    /// probe holds the embedder until it ends (bounded by
    /// [`PROBE_WARMUP_BUDGET`](crate::writeq::PROBE_WARMUP_BUDGET) plus
    /// [`PROBE_BUDGET`](crate::writeq::PROBE_BUDGET)); call
    /// [`EmbedderCalibration::abort`] when the last session over an embedder
    /// goes if a caller keeps the calibration.
    ///
    /// Unset, each build spawns a probe of its own, aborted at its close,
    /// which is what a single-session library caller wants.
    pub fn calibration(mut self, calibration: EmbedderCalibration) -> Self {
        self.calibration = Some(calibration);
        self
    }

    /// The embedder this builder will hand every session it builds, once
    /// [`MemoryBuilder::embedder`] or [`MemoryBuilder::backends`] set one.
    ///
    /// For `serve`'s process-wide tasks (#32 PR 4): the #13 keep-warm
    /// touches the one shared embedder, so it takes it from the template
    /// builder rather than from whichever session attached first.
    pub(crate) fn shared_embedder(&self) -> Option<Arc<dyn Embedder>> {
        self.embedder.clone()
    }

    /// The store this builder will hand every session it builds, once
    /// [`MemoryBuilder::store`] or [`MemoryBuilder::backends`] set one.
    ///
    /// For `serve`'s session registry (#32 PR 4 review L8): a background
    /// attach abandoned at shutdown releases the lease it may have taken
    /// through the same store.
    pub(crate) fn shared_store(&self) -> Option<Arc<dyn GraphStore>> {
        self.store.clone()
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
            // #23: the session was erased. Its lease row is the tombstone, which
            // no acquire can take over, so this is not a writer to proxy to or
            // wait out: refuse with the store's stable erased error, and do not
            // report it as `Held` (that would send `serve` dialling a holder
            // that does not exist and record a lease refusal against an erased
            // session).
            LeaseOutcome::Held { current, .. } if crate::store::erase::is_tombstone(&current) => {
                return Err(LamboError::Store(
                    crate::store::erase::erased_session_error(session.as_str()),
                ));
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
                            // #60: the relabel is durable; keep the graph's
                            // unflushed set exact.
                            graph.mark_durable_through(relabel.mutation_epoch);
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
        // #23 review L2: a background write refused because the session was
        // erased latches the fence and the wake-up at once, with the
        // tombstone as the winner, exactly as the heartbeat would on its next
        // beat — so `erased()` holds and reads of the deleted data stop now.
        let erased_latch: crate::store::erase::ErasedLatch = {
            let (fence, signal) = (lease_lost.clone(), lease_lost_signal.clone());
            Arc::new(move || {
                fence.store(true, std::sync::atomic::Ordering::Release);
                signal.latch(crate::store::erase::ERASED_HOLDER);
            })
        };
        let flush = flush.with_erased_latch(erased_latch.clone());
        let canon = CanonizationTask::from_daemon(graph.clone(), store.clone(), &daemon, &config)
            .with_token(lease_token)
            .with_erased_latch(erased_latch);

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
        // calibration probe that measures this deployment's embedder, unless a
        // shared calibration already has one for it (#32 PR 3); it is spawned
        // rather than awaited, so it costs this startup nothing.
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
                // #32 decision 15: completion lines name this session.
                ledger: self.ledger.map(|l| l.for_session(&session.0)),
            },
            clock.clone(),
            self.calibration.as_ref(),
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
            query_embeddings: PlMutex::new(QueryEmbeddingCache::new()),
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
