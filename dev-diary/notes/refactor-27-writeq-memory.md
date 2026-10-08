# #27: write queue and Memory split (decisions)

Refactor 4/5. Base: #26's head `d8fd22a` (main with #24, #25 and #16).
Decisions and why; the commits carry the mechanics.

## One pattern for both files: the struct stays, the impl blocks move

`writeq.rs` (3,622 lines) and `memory.rs` (3,492) each became a module root
that keeps its central struct (`WritePipeline`, `Memory`) and its
construction or accessors, with child modules holding `impl` blocks by
responsibility. Children can reach the parent's private fields, nothing outside
the module can, so no field widened past its module and every move is verbatim.
Items that were private to the old file became `pub(super)`, the same reach.
Public and crate paths are unchanged through re-exports; a compile probe of
every base `pub` and `pub(crate)` item found one dropped path
(`writeq::ConsumeStamp`), now restored.

| module | holds | lines |
|---|---|---|
| `writeq.rs` | module rule and invariants, `WritePipeline`, `spawn` | 276 |
| `writeq/receipts.rs` | `ReceiptId`, `ReceiptAnswer`, the receipt store, lookup / wait / piggyback | 854 |
| `writeq/calibration.rs` | the probe, the observed rate, `Calibration` (telemetry) | 693 |
| `writeq/counters.rs` | `WriteQueueCounters`, `ReplayBlockReason`, who moves which counter | 173 |
| `writeq/admission.rs` | bounds, `DropReason`, `Job`, `Lanes`, `admit` | 535 |
| `writeq/execution.rs` | `WriteCtx::run`, `mirror_concepts`, the lane worker and its settle | 594 |
| `writeq/drain.rs` | `quiesce`, `abort_workers`, `abort_all_sync` | 240 |
| `writeq/replay.rs` | the durable-intent replay | 444 |
| `memory.rs` | module invariants, `Memory` struct, accessors | 386 |
| `memory/types.rs` | result value types | 179 |
| `memory/builder.rs` | `MemoryBuilder`, `Attach`, the attach order, `AttachShutdown` | 786 |
| `memory/leases.rs` | heartbeat, `LeaseLostSignal`, `ACTIVE_SESSIONS` | 396 |
| `memory/gate.rs` | the writers gate | 98 |
| `memory/writes.rs` | metadata, derive, record_action (sync and async ack), demote, retract, soft locks | 806 |
| `memory/reads.rs` | recall, saints, stats, events, access hooks, `vector_candidates` | 271 |
| `memory/shutdown.rs` | `close`, `Drop`, `HandleCustody`, `TailCustody`, `final_flush` | 757 |

Order followed the issue: value types and construction first, replay and
shutdown last. A scripted comparison of every fn, struct, enum, const, static
and const-assert against the base: at the last move commit (`9e2146e`), 243
of 247 items are identical and the other four differ only by rustfmt
re-wrapping `admit`'s signature and `super::`-qualified doc links. At the
branch head, comments stripped, 235 of 247 are identical; the twelve others
are the vector seam (5), the `AttachShutdown` trait (2), the two fixes below,
the two asserts moved to MCP, and `admit`'s rustfmt wrap. Clippy's
`await_holding_lock` (which covers `parking_lot` guards) is clean under
`-D warnings` in every CI row, so no guard is held across an `.await`.

**`close()` stays one function.** The acceptance asks that shutdown
orchestration be understandable in one place; `memory/shutdown.rs` numbers its
stages in the module doc, which is also where #40's stage logging attaches.

## Where the three MCP couplings went

- `WRITE_QUEUE_DRAIN_BUDGET * 4 <= CLOSE_FLUSH_GRACE` now sits in
  `mcp/serve.rs`, and `MAX_CONCURRENT_RECEIPT_WAITS * 2 <= INFLIGHT_DEPTH_WARN`
  in `mcp/proxy.rs`. Why: the consumer depends on the core, not the reverse,
  and `mcp` is compiled in every build, so the asserts still run everywhere.
  Raising either writeq constant past its bound still fails the build
  (checked by mutation).
- `MemoryBuilder` held `mcp::serve::EarlyShutdown`. It now holds
  `Arc<dyn AttachShutdown>`, a crate-private trait with the two operations
  `build_attach` uses (`arm` in the `Acquired` arm, `fired` raced against the
  startup load); `EarlyShutdown` implements it by forwarding. Behaviour is the
  same. Why a trait rather than moving the type: the signal handling is the
  serving process's business, #28 can now reorganise it without touching
  `memory`, and #32 can hand every attach one process-wide handle.

## The caller-side vector seam (#8, #14)

`store::vector_source::VectorCandidates<'a>` is what recall's vector leg and
hybrid derive's semantic match are given. It answers `available()` (the
`VECTOR_SEARCH` bit, no I/O) and `checked(...)` (the checked read, capability
refusal included). Its one variant today, `Store`, reproduces exactly what the
callers did when they called the store.

- Recall: `recall::candidates::gather_from` and `Daemon::recall_with`.
  `gather`, `Daemon::recall` and `Daemon::recall_detailed` keep their
  signatures and wrap the store.
- Derive: `graph::hybrid::derive_with`; public `hybrid::derive` wraps the store.
- The query embed is its own step, `recall::candidates::embed_query`, shared by
  `Memory::recall_detailed` and `lambo recall`.
- The source is chosen in two places only: `Memory::vector_candidates` and
  `WriteCtx::vector_candidates` (the write queue holds no `Memory`). The
  embed-budget pre-pass and both `record_action` embed gates ask the source too.

#8 adds a graph-backed variant (a `VectorCandidateSource` over the in-memory
graph, reusing `rank_by_cosine`) and returns it from those two methods. #14
moves the cache check ahead of `embed_query`. Why an enum: the store path stays
statically dispatched, so the seam costs nothing measurable today (A/B in the
PR).

## `graph/graph.rs`: split later, not here

2,153 lines, about 29% doc comments, one struct whose private state every
mutation must keep consistent. Its impl clusters match its test subjects
(snapshot, transitions, embeddings, mutation log, accesses, root goal,
invariants), so it can take the same struct-in-root split. Nothing in the
Memory/queue boundaries requires it, and #8 does not need it unless #8 keeps a
vector matrix inside `Graph`. A follow-up issue body is drafted with concrete
boundaries; the orchestrator files it.

## Defects fixed on the way (own commits)

- `WritePipeline::abort_workers` aborted and joined lane workers one at a time.
  A join is pending while its worker is mid-poll on another thread; a `close()`
  cancelled there dropped the later handles un-aborted (still applying jobs),
  and a retried close found none to join. Now every worker is aborted first and
  un-joined handles return to `Lanes::workers` (`WorkerCustody`).
- `WritePipeline::stop_replay` dropped the aborted replay handle on
  cancellation, so a retry could drain the log while the replay finished a
  synchronous stretch. Now `ReplayCustody` returns it to its slot.

- (Review L3, remediation) A close retried after one cancelled inside
  `abort_workers` ran `quiesce` first, which saw `outstanding() > 0` (an
  aborted worker never runs its `running -= 1`; only the post-join reset
  does) and waited the whole `WRITE_QUEUE_DRAIN_BUDGET` for a settle nobody
  would send. `abort_workers` now sets `Lanes::workers_aborted` before its
  first await, and `quiesce` waits on `Lanes::drainable()`, which is zero
  once that is set. Pinned by
  `a_quiesce_after_a_cancelled_abort_does_not_wait_out_the_budget`.

The first two are the R3-1 hazard `HandleCustody` already closes for the
daemon, flush and canonization tasks. Durability was not at risk (every job
is a durable intent). Note for tests: tokio completes the join of an aborted *parked* task
at once, so only a task running on another thread opens the window; the
regression tests use a multi-thread runtime and an embedder that blocks its
thread.

## Stale lifecycle docs corrected

`close()` said a job left after the quiesce is "abandoned ... settled failed
... `write_queue_abandoned`" (since durable intents it is deferred:
`intent_durable`, `write_queue_deferred`); that the heartbeat is aborted
"first (right after latching closed)" (it is aborted once close holds the
writers gate, after the replay stop, the queue drain and the gate wait, so
it keeps refreshing for an unbounded stretch, not "at most the drain
budget"); and that it shuts down "all three tasks".
`WriteQueueCounters::abandoned` claimed close-budget jobs; `quiesce` said
admission "promised" its budget; `derive_async_as` still had the pre-J3-R3-5
"22 to 25 ms". Each corrected line says what it used to claim.

## Relocated review chronology

The module headers below are the base's (`d8fd22a`), kept here verbatim
because the code now carries only the rules. The J3 history they cite is in
`dev-diary/lambo-for-mooshik/J3-durability-redesign.md` and the J3 review
files.

### `src/writeq.rs` module doc at `d8fd22a`

```text
//! Asynchronous write pipeline and write receipts (J3).
//!
//! # The rule
//!
//! A write may be acknowledged **before** it has been applied only when its
//! result does not gate the caller's next action. `derive` and `record_action`
//! qualify: a warm `derive` is 27 ms of which 22 to 27 ms is the embedding call
//! (`dev-diary/lambo-for-mooshik/J-multi-client.md` §Measurements; J3-R3-5
//! corrected this line's earlier "22 to 25 ms" misquote of that section),
//! durability
//! was *already* asynchronous (the write-behind log returns long before
//! anything reaches disk), and neither outcome is something the agent branches
//! on. **`reserve` never qualifies** — its result *is* the caller's next
//! action, and an asynchronous reservation has two agents editing while each
//! believes it holds the lock.
//!
//! # Shape
//!
//! 1. **The synchronous part stays on the call path.** Validation resolves
//!    against the graph, and the interaction node is opened here too (see
//!    *Ordering* below). What moves off the call path is the embedder wait, not
//!    the round trip: the round trip is 0.31 to 0.48 ms on the rig and is not
//!    worth removing.
//! 2. **Embed, canonicalize and insert in the background**, through the
//!    ordinary [`crate::graph::hybrid::derive`] /
//!    [`crate::graph::action::record_action`] path. Dedup is therefore
//!    unaffected: embedding still precedes insertion, so the vector is present
//!    when matching happens.
//! 3. **The ack carries a [`ReceiptId`]**, against which the outcome is stored.
//!    Receipts are delivered two ways — piggybacked on that agent's next tool
//!    response, and fetched by id — and the fetch doubles as **opt-in
//!    synchrony**: an agent that needs its write applied waits on the receipt,
//!    which restores read-your-writes on demand without charging every agent
//!    for it. There is no `await` flag and no MCP notification (a notification
//!    lands in a client log rather than in the model's context, which is the
//!    exact failure workstream J exists to fix).
//!
//! # Ordering
//!
//! **Scope first, because the strong sentence used to come first and its
//! retraction came nine lines later** (J3-R2-6): everything in this section is
//! a claim about **one agent's writes sent one after another**. Two calls one
//! agent has in flight *simultaneously* are outside it, for the reason spelled
//! out below.
//!
//! The interaction is opened **synchronously, on the call path**, before the
//! job is queued. `begin_interaction_full` takes the graph write
//! lock only briefly and never awaits, so this is cheap — and for a sequential
//! caller it makes submission order *be* `Temporal`-chain order by
//! construction. That is strictly stronger than ordering the drain: the chain
//! no longer depends on drain order at all, so an out-of-order drain cannot
//! corrupt it. Since J1 the chain is session-wide (see
//! `Memory::begin_interaction_full`), so "one agent's writes apply in submission
//! order" is read off the chain by filtering it on `agent_id`.
//!
//! Per-agent FIFO is **still** enforced in the drain, for a second reason:
//! insertion order decides which of two identical concepts is `created` and
//! which is `matched`, and that distinction is reported in the receipt. Each
//! agent gets its own lane with a single consumer, so a lane drains in
//! submission order; lanes run concurrently, because interleaving *across*
//! agents is fine.
//!
//! **The scope of both promises, stated once more where the mechanism is**
//! (J3-R1-10): one agent's *sequential* submissions. The chain position is pinned
//! by `begin_interaction_full` and the
//! lane position by the `lanes.lock()` inside `WritePipeline::admit`, and
//! those are two critical sections with no ordering between them across
//! threads. So for two `lambo_derive` calls one agent has in flight *at the
//! same time*, the chain order and the drain order can disagree — task A can
//! open its interaction first and enqueue second. The consequence is confined
//! to created/matched attribution between those two calls, and a caller that
//! fires two writes concurrently has asserted no order for them to keep;
//! closing the window would mean opening the interaction under the lane lock,
//! which nests the graph write lock inside it. What must not happen is claiming
//! more than that, which the first version of this section did.
//!
//! # Backpressure — fairness and memory, never durability (the J3 redesign)
//!
//! Three review rounds produced five falsified estimator axes — width, warmth,
//! length, failure shape, concurrency scaling — every one a P1, because the
//! durability invariant ("no acked write is silently abandoned") was **coupled
//! to an estimator's correctness**: a clean close had a deadline and the
//! deadline's arithmetic rested on a measured rate. The series does not
//! converge; an estimator is wrong in as many ways as the workload has
//! covariates (`dev-diary/lambo-for-mooshik/J3-durability-redesign.md`).
//!
//! **Durable intents cut the coupling.** Every accepted job is recorded as a
//! [`crate::types::Mutation::PutWriteIntent`] at admission, so at a clean
//! close acked ⇒ (applied ∨ durable intent) **by construction** — the next
//! serve replays the remainder. Being wrong about the drain now costs a
//! deferral or a refusal, never a loss.
//!
//! Admission therefore guards only what admission can honestly guard:
//! **memory** (the aggregate bound [`WRITE_QUEUE_MAX`], derived from the
//! receipt store's cap, and the byte cap [`WRITE_QUEUE_MAX_BYTES`]) and
//! **fairness** (the per-lane bound [`WRITE_QUEUE_LANE_MAX`], one agent's
//! share of the queue). Both are static and generous, derived at their
//! constants from structural facts — not from a rate, because J3's five axes
//! are what happens when a rate is asked to carry an invariant.
//!
//! The probe and the observed rate survive as **telemetry**: the probe still
//! measures two input sizes and publishes the slower
//! ([`Calibration::probe_serial_items_per_sec`]), real write service times
//! still take over after [`OBSERVED_MIN_SAMPLES`] completed writes, and the
//! ratio between them ([`Calibration::probe_optimism`]) remains the payload's
//! self-diagnosing comparison (J3-R2-4). None of it sizes a bound any more.
//! The drop policy is fixed regardless — bound, drop, log once, count in
//! `lambo_stats`.
//!
//! # Accounting (the `ledger_queued_lines` lesson, re-derived)
//!
//! This module keeps its **own** counters and never touches
//! [`crate::ledger::LedgerCounters`], so the ledger's
//! `accepted − written − write_failed` keeps its exclusivity argument intact:
//! no new class enters the ledger's `accepted`. The queue mirrors that
//! discipline deliberately — a queue-full or byte-cap reject never enters
//! [`WriteQueueCounters::accepted`], so
//! `outstanding = accepted − applied − failed − deferred` is one expression
//! serving both the live gauge and the shutdown count, and cannot drift between
//! them. `abandoned` is a **label on a subset of `failed`**, not a fourth term:
//! an abandoned job is settled `failed`, and counting it twice is exactly the
//! mistake `adve-review-mooshik-I-round3.md`'s flip D maps. `deferred` **is** a
//! term — a close-deferred job settled `intent_durable` left this process's
//! custody without being applied or failed — and this line omitted it (J3
//! round-1 N5). The drift was inside the section whose whole thesis is that
//! there must be **one** expression, which is the reminder that a thesis does
//! not enforce itself: [`WriteQueueCounters::outstanding`] is the authority and
//! this sentence is a copy of it.
```

### `src/memory.rs` module doc at `d8fd22a`

```text
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
//! Every write opens a fresh [`Interaction`](crate::types::Interaction) whose `created_at` is taken here,
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
```
