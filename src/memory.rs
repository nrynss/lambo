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

use parking_lot::{Mutex as PlMutex, RwLock};
use tokio::sync::{broadcast, RwLock as AsyncRwLock};
use tokio::task::JoinHandle;

use crate::canon::CanonizationTask;
use crate::config::Config;
use crate::daemon::access::AccessLedger;
use crate::daemon::{Clock, Daemon, RecallPipeline};
use crate::embed::Embedder;
use crate::graph::index::InvertedIndex;
use crate::graph::Graph;
use crate::recall::cache::RecallCache;
use crate::store::flush::FlushTask;
use crate::store::lease::LeaseHolder;
use crate::store::GraphStore;
use crate::types::{AgentId, DaemonEvent, EmbeddingContract, SessionId};
use crate::writeq::WritePipeline;

mod types;

pub use types::{CanonicalMemory, DryRun, GcStats, GcSweepSummary, ImpactReport, MemoryStats};

mod leases;

pub(crate) use leases::LeaseLostSignal;
use leases::{register_session, spawn_lease_heartbeat};

mod builder;

pub(crate) use builder::STILL_REFRESHING_CLAUSE;
pub use builder::{Attach, LeaseHeldElsewhere, MemoryBuilder};

mod gate;
mod reads;
mod shutdown;
mod writes;

use shutdown::final_flush;

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

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests;
