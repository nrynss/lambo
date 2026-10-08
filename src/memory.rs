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
use crate::store::flush::{panic_message, CatchUnwindPoll, FlushTask, FLUSH_ATTEMPT_TIMEOUT};
use crate::store::lease::LeaseHolder;
use crate::store::GraphStore;
use crate::types::{
    AgentId, DaemonEvent, EmbeddingContract, LamboError, MutationBatch, SessionId, StoreError,
};
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
mod writes;

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

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests;
