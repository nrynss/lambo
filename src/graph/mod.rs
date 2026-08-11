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
//! upserts (spec §2.4). Because the log is chronological and an edge can only be
//! written once its endpoints exist, any edge referencing nodes that appear earlier
//! in the *same* drain batch is always safe.
//!
//! ## Weight dynamics (v0.6.0 §5.4 semantics)
//!
//! Duplicate natural-key edge writes reinforce: `weight` bumps by
//! [`REINFORCE_BUMP`] (capped at [`MAX_EDGE_WEIGHT`]), `reinforcements += 1`, and
//! `last_reinforced` moves to the write time. **Recall never reinforces** — the read
//! path does not call edge writes. Decay of `CoOccurrence`/`Semantic` edges is the
//! daemon GC's job (T4.x), not this module's.
//!
//! The exact v0.6.0 constants are not in-repo; the bump/cap below are the v0.1
//! decision (see Handoff Log T2.1).

pub mod graph;

pub use graph::{Graph, MAX_EDGE_WEIGHT, REINFORCE_BUMP};
