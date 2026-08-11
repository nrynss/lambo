//! In-RAM graph core (T2.1) — the primary tier of the write path.
//!
//! Per spec §2.1 the in-memory graph is primary; the durable store is synchronised
//! behind it. This module owns the graph structure, the spec §5.7 invariants, the
//! write-behind mutation log (T3.4's input), and the `MutationEpoch` counter.
//!
//! ## Lock discipline (spec §6.4, non-negotiable)
//!
//! [`Graph`] is a plain `&mut self` struct — it owns **no lock**. The owner (the
//! `Memory` type, T2.3+) wraps it in `Arc<RwLock<Graph>>` (`parking_lot`) and **never
//! holds the lock across an `.await`**. Take, work, release, then do I/O.
//!
//! ## Mutation log contract
//!
//! Every write appends its [`Mutation`]s to an ordered log drained by
//! [`Graph::drain_log`]. Ordering is guaranteed: within one logical write, node
//! mutations precede the edge mutations that reference them, and deletions follow
//! upserts (spec §2.4).
//!
//! **`drain_log()` returns chronological write order.** Spec §2.4's phase grouping
//! (node upserts, then edge upserts, then deletions, then canonization transitions)
//! applies *within* a single logical write, **not** across the batch: a node
//! upsert may legally follow a `DeleteNode` in the same drained batch (create ->
//! delete -> create within one flush interval). Store adapters (T3.4+) MUST replay
//! batches in order and MUST NOT re-sort them. Because the log is chronological
//! and an edge can only be written once its endpoints exist, **in-order replay
//! is always safe**: an edge's endpoints were upserted earlier in the same
//! batch **or in an earlier batch of the same session** — e.g. `record_action`
//! after a drain produces edge-only batches whose endpoints were committed by
//! the previous flush (adve-review GRAPH-10). Adapters MUST tolerate edge rows
//! referencing nodes already present from an earlier flush (the idempotent
//! upsert contract makes that free).
//!
//! ## Weight dynamics (v0.6.0 §5.4 semantics)
//!
//! Duplicate natural-key edge writes reinforce: `weight` bumps by
//! [`REINFORCE_BUMP`] (capped at [`MAX_EDGE_WEIGHT`]), `reinforcements += 1`, and
//! `last_reinforced` moves to the write time. **Recall never reinforces** — the read
//! path does not call edge writes. Decay of `CoOccurrence`/`Semantic` edges is the
//! daemon GC's job (T4.x), not this module's.
//!
//! ## InvertedIndex ownership (P3 contract — muse-spark M3)
//!
//! [`InvertedIndex`] (T2.6) is a **separate structure owned by the session owner**
//! (P3's `Memory`, the `Arc<RwLock<Graph>>` holder) — the graph module itself is
//! index-free and no write API touches it. The owner MUST mirror every concept
//! write into the index: `index.add` on every concept create/update (including
//! `derive`, `record_action`, `demote` creations) and `index.remove` on
//! `remove_node`. A forgotten mirror is silent staleness (recall returns stale
//! keyword candidates), so P3 must not rely on memory — the sync contract is
//! tested in `tests/p2_integration.rs::inverted_index_manual_sync_contract`.
//!
//! The exact v0.6.0 constants are not in-repo; the bump/cap below are the v0.1
//! decision (see Handoff Log T2.1).

// The inner module shares the parent's name (`graph::graph`) by design — T2.1
// owns `src/graph/graph.rs` and phase docs reference that path.
#[allow(clippy::module_inception)]
pub mod graph;

// Canonicalization pipeline (T2.2) — spec §7.1 steps 1–5.
pub mod canonical;
// T2.3 — `derive()`, the primary write path (spec §7; see `derive.rs`).
pub mod derive;
// T2.5 — context-overflow demotion: Observation nodes per sentence (spec §7).
pub mod demote;
// Inverted index + BM25 (T2.6) — spec §8 phase-1 keyword source.
pub mod index;
// T2.7 — spec §11 soft-lock reservation policy (see `reserve.rs`). Kept a single
// additive module: cutting the feature is deleting this line + `reserve.rs`.
pub mod reserve;
// T2.4 — spec §7 record_action + write-time Causal/Dependency cycle check (see `action.rs`).
pub mod action;

pub use graph::{Graph, MAX_EDGE_WEIGHT, REINFORCE_BUMP};
