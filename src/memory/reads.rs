//! Read operations: recall, saints, stats, GC accounting and events, plus the
//! read-access ledger (#30).
//!
//! Reads never take the writers gate (a long recall must not delay
//! shutdown); they are refused after close by `ensure_open`. Recall's order
//! is: embed the query (only when the vector leg can run), take the recall
//! cache, run the daemon's three-phase recall (which takes and releases the
//! graph lock itself, after its own store I/O), then note every returned hit
//! as an access. The access note comes after the pipeline returns and is a
//! leaf lock.

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
use std::time::Duration;

use chrono::Utc;
use tokio::sync::broadcast;

use super::{CanonicalMemory, GcStats, GcSweepSummary, Memory, MemoryStats};
use crate::recall::candidates;
use crate::recall::format;
use crate::store::vector_source::VectorCandidates;
use crate::types::{
    tie_break_by_key, CanonizationStatus, DaemonEvent, LamboError, NodeId, RecallQuery,
    RecallResult,
};

impl Memory {
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

        // The one source this recall's vector leg reaches candidates through
        // (#27); the query embed is its own step, skipped when the leg cannot
        // run (#14 moves the cache check ahead of it).
        let vectors = self.vector_candidates();
        let mut warnings = Vec::new();
        let embedding =
            match candidates::embed_query(vectors, self.embedder.as_ref(), &query.query).await {
                Ok(vector) => vector,
                Err(warning) => {
                    warnings.push(warning);
                    None
                }
            };

        // The recall cache is `&mut` across `Daemon::recall`'s awaits. This is
        // NOT the graph lock — `Daemon::recall` takes and releases that itself,
        // after its own store I/O.
        let mut cache = self.recall_cache.lock().await;
        let mut result = self
            .daemon
            .recall_with(
                &self.session,
                query,
                vectors,
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

    /// The vector-candidate source this session's recall and hybrid derive
    /// are given (#27's caller-side seam). Today the durable store, exactly as
    /// before; #8 returns a graph-backed source from here, and the write
    /// queue's twin is `WriteCtx::vector_candidates`.
    pub(crate) fn vector_candidates(&self) -> VectorCandidates<'_> {
        VectorCandidates::from_store(self.store.as_ref())
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
}
