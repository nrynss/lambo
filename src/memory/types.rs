//! The value types `Memory`'s operations return: retraction reports, saints,
//! session health and GC accounting. Plain data, no behaviour beyond
//! [`DryRun::is_dry`]; re-exported at `crate::memory` and the crate root.

use std::time::Duration;

use chrono::{DateTime, Utc};

use crate::types::{AgentId, CanonizationStatus, ConceptType, NodeId, SessionId};

/// Whether [`Memory::retract`](super::Memory::retract) is allowed to mutate (spec §6.1, §13).
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

/// One canonical ("saint") memory — [`Memory::canonical_memories`](super::Memory::canonical_memories).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanonicalMemory {
    /// The concept's node id.
    pub node_id: NodeId,
    /// The concept's canonical key: the stable tie order for equal blast
    /// radius and age, ahead of the per-run node id (issue #2).
    pub canonical_key: String,
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
    /// Time since the store was last caught up: the drain the last successful
    /// flush covered, or the last flush poll that found nothing pending (#16
    /// §3). At most one poll interval while nothing is pending; it grows while
    /// anything waits to be flushed, and past the flush interval only while
    /// the store is not taking writes. Dropped mutations (dead letters, a
    /// fenced or degraded session) are not in it; see
    /// `crate::store::flush::FlushStats::lag`.
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
    /// Concepts carrying a vector in this session's stamped embedding space
    /// (K2). Applied != embedded at session scale: canonization status says
    /// nothing about whether a vector exists, so before K2 nothing answered
    /// "how many of these concepts actually carry a vector?" — the gap behind
    /// the 92/100 dogfood damage (8 concepts with NULL embeddings ranked
    /// keyword-only and nobody could see it). `embedded_concepts ==
    /// concept_count` is the healthy state; anything less is NULL rows.
    pub embedded_concepts: usize,
    /// `MutationEpoch` — recall-cache key (spec §8).
    pub epoch: u64,
    /// Background daemon cycles completed since the session opened.
    pub daemon_cycles: u64,
    /// Canonization evaluation cycles completed.
    pub canonization_cycles: u64,
    /// Canonization cycles that failed. A non-zero count is worth investigating.
    pub canonization_failures: u64,
}

/// GC's sweep accounting for `lambo_stats` (issue #29): read-side only.
///
/// `last_gc_at` / `last_gc_epoch` are the session's durable sweep mark (they
/// survive a writer restart). `last_sweep` is the most recent sweep **this
/// process** ran, so it is `None` after a restart until the next sweep even
/// when `last_gc_at` is set.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcStats {
    /// When the last sweep ran, or when a never-swept session's clock was
    /// anchored. `None` until either happens.
    pub last_gc_at: Option<chrono::DateTime<chrono::Utc>>,
    /// The epoch GC's next interval and idle floor are measured from.
    pub last_gc_epoch: u64,
    /// The last sweep this process ran, if any.
    pub last_sweep: Option<GcSweepSummary>,
}

/// One sweep's headline numbers (issue #29), from
/// [`crate::daemon::gc::GcOutcome`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GcSweepSummary {
    /// Why it ran; `None` only for a sweep not started by the daemon.
    pub trigger: Option<crate::daemon::gc::GcTrigger>,
    /// Concepts collected (steps 2 and 3).
    pub collected: usize,
    /// Candidates the per-sweep cap held back.
    pub deferred: usize,
    /// The cap it ran under.
    pub collection_cap: usize,
    /// Did the cap hold anything back?
    pub cap_bound: bool,
    /// Resources under their bar kept only because they have dependents.
    pub resources_spared_by_dependents: usize,
    /// Survivor bumps still waiting to be drained from that sweep **at the
    /// time it ran** (not live).
    pub survivors_deferred: usize,
}
