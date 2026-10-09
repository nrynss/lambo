//! `TieredStore`: a durable store with an Elasticsearch recall tier beside it
//! (#18, feature `recall-elastic`).
//!
//! The durable store (`primary`: SQLite, Postgres or Cockroach, or the
//! in-memory store in tests) stays the source of truth and keeps every
//! obligation that needs a transaction. The recall index serves one thing,
//! the vector leg of phase-1 recall, through the #26 seam
//! ([`VectorCandidateSource`]). Everything else is delegated unchanged.
//!
//! # Delegation
//!
//! `init_schema` (plus the index's marker index), `preflight_schema`,
//! `load_session`, `keyword_candidates`, the leases, `blast_radius`,
//! `interaction_span`, `record_canonization` and the flush stats go to the
//! primary. Fencing is therefore exactly the primary's: a stale-token flush
//! fails there and nothing is mirrored.
//!
//! # Mirroring
//!
//! After the primary commits a flush, the batch is projected
//! (`project::project`) and bulk-written to the index at the external version
//! `(fencing_token << 32) | flush_counter`, with the counter kept **per
//! session** and restarted for each new token (#32: fencing tokens are per
//! session, and this store holds no "current session"). A replayed or late
//! write older than what the index holds is refused by the engine as a
//! version conflict, which counts as success.
//!
//! A mirror failure never fails the flush (the primary is already durable).
//! It marks the session **stale**, counts the failure, keeps the last error
//! ([`TieredStore::tier_status`]) and logs it.
//!
//! # When the index is trusted
//!
//! Per session, the tier is `Unknown`, `InSync` or `Stale`. Only `InSync`
//! serves vector candidates from the index; the other two fall back to the
//! primary's own checked read (exact on SQLite, the database's ranking on the
//! pg family), or to no vector leg at all when the primary has none, so
//! recall degrades to its keyword and recent legs and never to wrong answers.
//!
//! `InSync` is established from a **sync marker** the index keeps per
//! session: the durable `mutation_epoch` it reflects. It is written after
//! every clean mirror and compared with the durable snapshot's epoch when the
//! session is loaded. A crash between the primary's commit and the mirror,
//! or a failed mirror before a restart, leaves the marker behind the durable
//! epoch, and the next load sees it.
//!
//! **Repair.** Only a process that holds the session's lease repairs: at
//! load (`Memory` acquires the lease before it loads), after a later flush
//! while the session is stale (at most once per [`REPAIR_BACKOFF`]), and from
//! `lambo recall-index backfill`. A repair re-indexes every stored vector from
//! the durable snapshot at a fresh version, then deletes every session
//! document older than that version (deleted nodes, an older contract's
//! index), then writes the marker. Readers never write to the index; a reader
//! that finds the marker behind serves from the primary.
//!
//! # The embedding contract
//!
//! Each contract's vectors live in their own index, `{prefix}-v-{hash}`
//! (`project::contract_hash`). A checked read compares the expected contract
//! with the session's durable contract (cached from the last load or flush,
//! the one thing besides the counters this store remembers per session) and
//! then queries only the expected contract's index. If the durable contract
//! changes between the two, the answer is still in the expected space: stale,
//! never meaningless. That is the accepted divergence from the SQL adapters'
//! single-snapshot check, recorded in `dev-diary/notes/feature-18-elastic-tier.md`.
//!
//! # `exact_vector_scan` stays false (#8)
//!
//! A session holder over a store that declares an exact scan ranks in its own
//! graph and never asks the store. This store leaves the declaration at its
//! default `false`: its checked read is a tier, not unmodified delegation, so
//! a holder over `TieredStore(sqlite)` reaches the index. The note records why
//! a holder does not switch to its graph for small sessions either.

pub(crate) mod elastic;
pub(crate) mod index;
pub(crate) mod project;

#[cfg(test)]
mod fake;
#[cfg(all(test, feature = "store-memory"))]
mod tests;

use std::collections::HashMap;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use parking_lot::Mutex;

use self::index::{DocOp, RecallIndex, SyncMarker};
use self::project::{index_doc, mirror_version, project, sole_session};
use crate::store::lease::{LeaseHolder, LeaseInfo, LeaseOutcome, LeaseRefusal, LEASE_TTL};
use crate::store::vector_source::{ensure_is_an_embedding, VectorCandidateSource};
use crate::store::{
    validate_vector_candidate_limit, Capabilities, EraseOutcome, GraphStore, RecallBackfillReport,
    SessionFlushStats,
};
use crate::types::{
    tie_break_by_key, CanonizationEvent, Concept, EmbeddingContract, GraphSnapshot,
    InteractionSpan, MutationBatch, NodeId, Scored, SessionId, StoreError,
};

/// The shortest interval between two repair attempts for one stale session
/// on the flush path. A repair reads the whole durable session, so while the
/// index is down it must not run on every flush.
pub(crate) const REPAIR_BACKOFF: Duration = Duration::from_secs(60);

/// Documents per bulk request during a repair.
const REPAIR_CHUNK: usize = 500;

/// Whether the index can be trusted for one session.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) enum TierSync {
    /// Not checked yet in this process, or the check could not reach the index.
    #[default]
    Unknown,
    /// The index reflects the durable state as of the last clean mirror.
    InSync,
    /// A mirror failed or the marker is behind: repair before trusting it.
    Stale,
}

/// Per-session tier state. Keyed by session in [`TieredStore`]; nothing here
/// is process-wide.
#[derive(Debug, Default)]
struct SessionTier {
    /// The fencing token `counter` belongs to.
    token: u64,
    /// The last flush counter used under `token`.
    counter: u32,
    sync: TierSync,
    /// The session's durable contract, when known (`Some(None)`: none).
    contract: Option<Option<EmbeddingContract>>,
    /// The token of a lease this store acquired for the session, if held.
    held: Option<u64>,
    mirror_failures: u64,
    last_error: Option<String>,
    next_repair: Option<Instant>,
}

/// The tier's view of one session, for tests. Production reports the same
/// facts through `lambo::recall_tier` warnings.
#[cfg(test)]
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TierStatus {
    pub sync: TierSync,
    pub token: u64,
    pub flush_counter: u32,
    pub mirror_failures: u64,
    pub last_error: Option<String>,
}

/// A durable store with a recall index beside it. See the module docs.
pub(crate) struct TieredStore {
    primary: Box<dyn GraphStore>,
    recall: Box<dyn RecallIndex>,
    /// The process's configured embedder width, reported when the primary
    /// persists no vectors of its own (so `VECTOR_SEARCH` keeps its width).
    vector_dim: Option<usize>,
    sessions: Mutex<HashMap<SessionId, SessionTier>>,
    repair_backoff: Duration,
}

impl TieredStore {
    pub(crate) fn new(
        primary: Box<dyn GraphStore>,
        recall: Box<dyn RecallIndex>,
        vector_dim: Option<usize>,
    ) -> Self {
        Self {
            primary,
            recall,
            vector_dim,
            sessions: Mutex::new(HashMap::new()),
            repair_backoff: REPAIR_BACKOFF,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_repair_backoff(mut self, backoff: Duration) -> Self {
        self.repair_backoff = backoff;
        self
    }

    /// The tier's view of `session`.
    #[cfg(test)]
    pub(crate) fn tier_status(&self, session: &SessionId) -> TierStatus {
        let sessions = self.sessions.lock();
        let st = sessions.get(session);
        TierStatus {
            sync: st.map(|s| s.sync).unwrap_or_default(),
            token: st.map_or(0, |s| s.token),
            flush_counter: st.map_or(0, |s| s.counter),
            mirror_failures: st.map_or(0, |s| s.mirror_failures),
            last_error: st.and_then(|s| s.last_error.clone()),
        }
    }

    /// Run `f` on the session's state. Never held across an `.await`.
    fn with_state<R>(&self, session: &SessionId, f: impl FnOnce(&mut SessionTier) -> R) -> R {
        f(self.sessions.lock().entry(session.clone()).or_default())
    }

    /// The next external version for a write to `session` under `token`
    /// (`None` for an unleased write, which takes last-write-wins).
    fn next_version(&self, session: &SessionId, token: Option<u64>) -> Result<Option<u64>, String> {
        let Some(token) = token else {
            return Ok(None);
        };
        self.with_state(session, |st| {
            if st.token != token {
                st.token = token;
                st.counter = 0;
            }
            let counter = st.counter.checked_add(1).ok_or_else(|| {
                format!(
                    "flush counter for session {session} under token {token} is exhausted; \
                     refusing to wrap the mirror version"
                )
            })?;
            let version = mirror_version(token, counter).ok_or_else(|| {
                format!("fencing token {token} is too large to version a recall-index write")
            })?;
            st.counter = counter;
            Ok(Some(version))
        })
    }

    fn mark_stale(&self, session: &SessionId, error: &str) {
        tracing::warn!(
            target: "lambo::recall_tier",
            session = %session,
            "recall index is stale for this session; vector recall falls back to the durable \
             store until it is repaired: {error}"
        );
        self.with_state(session, |st| {
            st.sync = TierSync::Stale;
            st.mirror_failures += 1;
            st.last_error = Some(error.to_owned());
        });
    }

    /// The session's durable contract: cached, or read once from the primary.
    async fn durable_contract(
        &self,
        session: &SessionId,
    ) -> Result<Option<EmbeddingContract>, StoreError> {
        if let Some(known) = self.with_state(session, |st| st.contract.clone()) {
            return Ok(known);
        }
        let contract = match self.primary.load_session(session).await {
            Ok(snap) => snap.embedding,
            Err(StoreError::SessionNotFound(_)) => None,
            Err(e) => return Err(e),
        };
        self.with_state(session, |st| st.contract = Some(contract.clone()));
        Ok(contract)
    }

    /// Compare the index's marker with the durable epoch.
    async fn check_marker(&self, session: &SessionId, epoch: u64, indexable: bool) -> TierSync {
        match self.recall.read_marker(session).await {
            Ok(Some(m)) if m.synced_epoch == epoch => TierSync::InSync,
            // Nothing was ever mirrored and there is nothing to mirror.
            Ok(None) if !indexable => TierSync::InSync,
            Ok(_) => TierSync::Stale,
            Err(e) => {
                tracing::warn!(
                    target: "lambo::recall_tier",
                    session = %session,
                    "recall index unreachable while checking its sync marker: {e}"
                );
                TierSync::Unknown
            }
        }
    }

    /// Settle the session's state after a durable load: cache the contract,
    /// check the marker, and repair if this process holds the lease.
    async fn settle_after_load(&self, session: &SessionId, snap: Option<&GraphSnapshot>) {
        let contract = snap.and_then(|s| s.embedding.clone());
        let epoch = snap.map_or(0, |s| s.mutation_epoch);
        let concepts = snap.map_or(&[][..], |s| s.concepts.as_slice());
        let indexable = contract
            .as_ref()
            .is_some_and(|c| concepts.iter().any(|k| index_doc(k, c, None).is_some()));
        let held = self.with_state(session, |st| {
            st.contract = Some(contract.clone());
            st.held
        });
        let sync = self.check_marker(session, epoch, indexable).await;
        if sync == TierSync::InSync {
            self.with_state(session, |st| st.sync = TierSync::InSync);
            return;
        }
        match held {
            Some(token) => {
                if let Err(e) = self
                    .reconcile(session, contract.as_ref(), concepts, epoch, token)
                    .await
                {
                    self.mark_stale(session, &format!("repair at load failed: {e}"));
                    self.with_state(session, |st| {
                        st.next_repair = Some(Instant::now() + self.repair_backoff);
                    });
                }
            }
            None => self.with_state(session, |st| st.sync = sync),
        }
    }

    /// Rebuild the session's index documents from durable state, at a fresh
    /// version under `token`. See "Repair" in the module docs.
    async fn reconcile(
        &self,
        session: &SessionId,
        contract: Option<&EmbeddingContract>,
        concepts: &[Concept],
        epoch: u64,
        token: u64,
    ) -> Result<RecallBackfillReport, StoreError> {
        let version = self
            .next_version(session, Some(token))
            .map_err(StoreError::Backend)?;
        // Reads fall back while documents are being replaced.
        self.with_state(session, |st| {
            if st.sync == TierSync::InSync {
                st.sync = TierSync::Unknown;
            }
        });
        let mut indexed = 0u64;
        let mut index = None;
        if let Some(contract) = contract {
            self.recall.ensure_index(contract).await?;
            index = Some(self.recall.index_name(contract));
            let ops: Vec<DocOp> = concepts
                .iter()
                .filter(|c| &c.session_id == session)
                .filter_map(|c| {
                    index_doc(c, contract, version).map(|doc| DocOp::Index {
                        contract: contract.clone(),
                        id: c.id,
                        version,
                        doc,
                    })
                })
                .collect();
            for chunk in ops.chunks(REPAIR_CHUNK) {
                self.recall.bulk(chunk).await?;
            }
            indexed = ops.len() as u64;
        }
        self.recall.delete_session_docs(session, version).await?;
        self.recall
            .write_marker(
                session,
                SyncMarker {
                    synced_epoch: epoch,
                },
                version,
            )
            .await?;
        self.with_state(session, |st| {
            st.sync = TierSync::InSync;
            st.next_repair = None;
        });
        Ok(RecallBackfillReport {
            session: session.clone(),
            indexed,
            index,
            mutation_epoch: epoch,
        })
    }

    /// Repair from the durable store, unless a recent attempt failed.
    async fn repair_if_due(&self, session: &SessionId, token: u64) {
        let due = self.with_state(session, |st| {
            st.next_repair.is_none_or(|at| Instant::now() >= at)
        });
        if !due {
            return;
        }
        let result = match self.primary.load_session(session).await {
            Ok(snap) => {
                self.with_state(session, |st| st.contract = Some(snap.embedding.clone()));
                self.reconcile(
                    session,
                    snap.embedding.as_ref(),
                    &snap.concepts,
                    snap.mutation_epoch,
                    token,
                )
                .await
                .map(|_| ())
            }
            Err(StoreError::SessionNotFound(_)) => self
                .reconcile(session, None, &[], 0, token)
                .await
                .map(|_| ()),
            Err(e) => Err(e),
        };
        if let Err(e) = result {
            self.mark_stale(session, &format!("repair after flush failed: {e}"));
            self.with_state(session, |st| {
                st.next_repair = Some(Instant::now() + self.repair_backoff);
            });
        }
    }

    /// Mirror a committed batch. Never fails the flush.
    async fn mirror(&self, batch: &MutationBatch, token: Option<u64>) {
        match sole_session(&batch.mutations) {
            Ok(Some(session)) => self.mirror_session(&session, batch, token).await,
            Ok(None) => self.mirror_unnamed(batch, token).await,
            Err(_) => {
                // The graph drains one session per batch; a hand-built batch
                // over several is mirrored per session, without the deletes
                // (which name no session), so each session goes stale and is
                // repaired rather than trusted.
                for sid in crate::store::batch::batch_session_ids(&batch.mutations) {
                    let session = SessionId::new(sid);
                    self.mark_stale(
                        &session,
                        "a batch spanning several sessions cannot be attributed; repairing",
                    );
                }
            }
        }
    }

    /// A batch that names no session: a delete-only batch (a GC sweep), or
    /// one that touches nothing the index holds.
    async fn mirror_unnamed(&self, batch: &MutationBatch, token: Option<u64>) {
        let deleted: Vec<NodeId> = crate::store::batch::batch_deleted_ids(&batch.mutations).0;
        if deleted.is_empty() {
            return;
        }
        // Attribute through the lease this store holds under the same token.
        let owner = token.and_then(|t| {
            let sessions = self.sessions.lock();
            let mut owners = sessions
                .iter()
                .filter(|(_, st)| st.held == Some(t))
                .map(|(s, _)| s.clone());
            match (owners.next(), owners.next()) {
                (Some(one), None) => Some(one),
                _ => None,
            }
        });
        if let Some(session) = owner {
            self.mirror_session(&session, batch, token).await;
            return;
        }
        // Unattributable: delete by id everywhere (always safe, the nodes are
        // durably gone) and leave every marker where it is, so the owning
        // session's next load sees its marker behind and repairs.
        if let Err(e) = self.recall.delete_ids(&deleted).await {
            let sessions: Vec<SessionId> = self.sessions.lock().keys().cloned().collect();
            for session in sessions {
                self.mark_stale(&session, &format!("unattributed delete failed: {e}"));
            }
        }
    }

    async fn mirror_session(&self, session: &SessionId, batch: &MutationBatch, token: Option<u64>) {
        let sync = self.with_state(session, |st| st.sync);
        if sync != TierSync::InSync {
            match token {
                // The durable snapshot already holds this batch, so a repair
                // covers it; mirroring it on its own first would be redundant.
                Some(t) => self.repair_if_due(session, t).await,
                None => self.mirror_ops(session, batch, None, false).await,
            }
            return;
        }
        let version = match self.next_version(session, token) {
            Ok(v) => v,
            Err(e) => {
                self.mark_stale(session, &e);
                return;
            }
        };
        self.mirror_ops(session, batch, version, true).await;
    }

    /// Project and write; advance the marker only when `advance_marker` and
    /// everything landed.
    async fn mirror_ops(
        &self,
        session: &SessionId,
        batch: &MutationBatch,
        version: Option<u64>,
        advance_marker: bool,
    ) {
        let before = match self.durable_contract(session).await {
            Ok(c) => c,
            Err(e) => {
                self.mark_stale(session, &format!("durable contract unavailable: {e}"));
                return;
            }
        };
        let projection = project(session, &batch.mutations, before, version);
        self.with_state(session, |st| {
            st.contract = Some(projection.contract_after.clone());
        });
        let written = async {
            for contract in &projection.contracts {
                self.recall.ensure_index(contract).await?;
            }
            if !projection.ops.is_empty() {
                self.recall.bulk(&projection.ops).await?;
            }
            if advance_marker {
                self.recall
                    .write_marker(
                        session,
                        SyncMarker {
                            synced_epoch: batch.mutation_epoch,
                        },
                        version,
                    )
                    .await?;
            }
            Ok::<(), StoreError>(())
        }
        .await;
        if let Err(e) = written {
            self.mark_stale(session, &format!("mirror failed: {e}"));
        }
    }

    /// The primary's own checked read, or no vector leg when it has none.
    async fn fallback(
        &self,
        session: &SessionId,
        probe: &[f32],
        expected: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        if self
            .primary
            .capabilities()
            .contains(Capabilities::VECTOR_SEARCH)
        {
            self.primary
                .vector_candidates_checked(session, probe, expected, limit)
                .await
        } else {
            tracing::debug!(
                target: "lambo::recall_tier",
                session = %session,
                "recall index not in sync and the durable store has no vector search; \
                 vector leg returns nothing (keyword and recent legs still run)"
            );
            Ok(Vec::new())
        }
    }

    /// Establish the session's state for a read that arrives before any load
    /// through this store (a reader that never loaded the session).
    async fn sync_for_read(&self, session: &SessionId) -> TierSync {
        let sync = self.with_state(session, |st| st.sync);
        if sync != TierSync::Unknown {
            return sync;
        }
        match self.primary.load_session(session).await {
            Ok(snap) => self.settle_after_load(session, Some(&snap)).await,
            Err(StoreError::SessionNotFound(_)) => self.settle_after_load(session, None).await,
            Err(_) => return TierSync::Unknown,
        }
        self.with_state(session, |st| st.sync)
    }
}

#[async_trait]
impl VectorCandidateSource for TieredStore {
    /// The index's answer when the session is in sync, the primary's
    /// otherwise. Same contract as every other source: limit checks, the
    /// probe check, an empty answer for an unknown session or one with no
    /// contract, an `Invariant` refusal for a contract mismatch.
    async fn checked_vector_candidates(
        &self,
        session: &SessionId,
        probe: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        validate_vector_candidate_limit(limit)?;
        if limit == 0 {
            return Ok(Vec::new());
        }
        ensure_is_an_embedding(probe)?;
        if self.sync_for_read(session).await != TierSync::InSync {
            return self
                .fallback(session, probe, expected_contract, limit)
                .await;
        }
        let durable = self.with_state(session, |st| st.contract.clone().flatten());
        let Some(durable) = durable else {
            return Ok(Vec::new());
        };
        durable
            .ensure_compatible(expected_contract)
            .map_err(|err| {
                StoreError::Invariant(format!(
                    "vector candidate lookup refused after embedding contract changed: {err}"
                ))
            })?;
        if probe.len() != durable.dim {
            return Err(StoreError::Invariant(format!(
                "query embedding has {} dimensions but session {} stores vectors of {}",
                probe.len(),
                session.0,
                durable.dim
            )));
        }
        // Only the expected contract's index is queried: whatever happened to
        // the durable contract since the check, these vectors are in the
        // caller's space.
        let hits = match self
            .recall
            .knn(expected_contract, session, probe, limit)
            .await
        {
            Ok(hits) => hits,
            Err(e) => {
                tracing::warn!(
                    target: "lambo::recall_tier",
                    session = %session,
                    "recall index query failed, serving this read from the durable store: {e}"
                );
                return self
                    .fallback(session, probe, expected_contract, limit)
                    .await;
            }
        };
        let mut ranked: Vec<(Scored<NodeId>, String)> = hits
            .into_iter()
            .map(|h| (Scored::new(h.id, h.cosine), h.canonical_key))
            .collect();
        ranked.sort_by(|(a, a_key), (b, b_key)| {
            b.score
                .total_cmp(&a.score)
                .then_with(|| tie_break_by_key(Some(a_key), &a.item, Some(b_key), &b.item))
        });
        ranked.truncate(limit);
        Ok(ranked.into_iter().map(|(s, _)| s).collect())
    }
}

#[async_trait]
impl GraphStore for TieredStore {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.primary.init_schema().await?;
        self.recall.provision().await
    }

    /// The tier adds vector search whatever the primary has: when the index
    /// cannot answer, the primary's own read (or an empty vector leg) does.
    fn capabilities(&self) -> Capabilities {
        self.primary.capabilities() | Capabilities::VECTOR_SEARCH
    }

    async fn preflight_schema(&self) -> Result<(), StoreError> {
        // The index is not a durable obligation: an unreachable index must
        // not refuse an attach, it only sends vector reads to the primary.
        self.primary.preflight_schema().await
    }

    fn vector_dimensions(&self) -> Option<usize> {
        self.primary.vector_dimensions().or(self.vector_dim)
    }

    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        // Fencing is the primary's: on error nothing is mirrored.
        self.primary.flush(batch, token).await?;
        self.mirror(batch, token).await;
        Ok(())
    }

    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        match self.primary.load_session(session).await {
            Ok(snap) => {
                self.settle_after_load(session, Some(&snap)).await;
                Ok(snap)
            }
            Err(StoreError::SessionNotFound(s)) => {
                self.settle_after_load(session, None).await;
                Err(StoreError::SessionNotFound(s))
            }
            Err(e) => Err(e),
        }
    }

    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.primary
            .keyword_candidates(session, tokens, limit)
            .await
    }

    /// The frozen unchecked surface stays the primary's: it cannot bind a
    /// contract, so it gets no index to choose between.
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        if self
            .primary
            .capabilities()
            .contains(Capabilities::VECTOR_SEARCH)
        {
            self.primary
                .vector_candidates(session, embedding, limit)
                .await
        } else {
            Err(StoreError::Capability(
                "the unchecked vector_candidates surface is the durable store's, which has no \
                 vector search; use vector_candidates_checked"
                    .into(),
            ))
        }
    }

    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.checked_vector_candidates(session, embedding, expected_contract, limit)
            .await
    }

    // `exact_vector_scan` deliberately keeps its default `false` (#8): see
    // the module docs.

    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.primary
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
        self.primary
            .interaction_span(session, node, min_age, now)
            .await
    }

    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.primary.record_canonization(event, token).await
    }

    /// The durable erase first; only once it committed, every document and
    /// the marker for the session leave the index. An index failure after a
    /// committed durable erase is an error, so a deletion fan-out never marks
    /// the session done while its vectors are still searchable. Rerunning is
    /// safe: the durable erase reports `already_absent` and the index cleanup
    /// is retried.
    async fn erase_session(
        &self,
        session: &SessionId,
        eraser: &LeaseHolder,
    ) -> Result<EraseOutcome, StoreError> {
        let outcome = self.primary.erase_session(session, eraser).await?;
        if let EraseOutcome::Erased(_) = &outcome {
            let cleaned = async {
                self.recall.delete_session_docs(session, None).await?;
                self.recall.delete_marker(session).await
            }
            .await;
            if let Err(e) = cleaned {
                return Err(StoreError::Backend(format!(
                    "session {session} was erased from the durable store, but removing its \
                     documents from the recall index failed: {e}. Run erase-session again: the \
                     durable erase is idempotent and the recall index cleanup is retried"
                )));
            }
            self.sessions.lock().remove(session);
        }
        Ok(outcome)
    }

    async fn backfill_recall_index(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<Option<RecallBackfillReport>, StoreError> {
        let token = match self.acquire_lease(session, holder, LEASE_TTL).await? {
            LeaseOutcome::Acquired(info) => info.token,
            LeaseOutcome::Held { current, age } => {
                return Err(StoreError::Backend(format!(
                    "session {session} is held by a live writer ({}, holding the lease for \
                     {}s); nothing was rebuilt. A holder repairs its own recall index when it \
                     loads the session and after a failed mirror; stop it to rebuild from here",
                    current.holder,
                    age.as_secs()
                )));
            }
        };
        let result = match self.primary.load_session(session).await {
            Ok(snap) => {
                self.with_state(session, |st| st.contract = Some(snap.embedding.clone()));
                self.reconcile(
                    session,
                    snap.embedding.as_ref(),
                    &snap.concepts,
                    snap.mutation_epoch,
                    token,
                )
                .await
            }
            Err(StoreError::SessionNotFound(_)) => {
                self.reconcile(session, None, &[], 0, token).await
            }
            Err(e) => Err(e),
        };
        if let Err(e) = self.release_lease(session, holder).await {
            tracing::warn!(
                target: "lambo::recall_tier",
                session = %session,
                "backfill could not release its lease (it expires on its own): {e}"
            );
        }
        result.map(Some)
    }

    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        let outcome = self.primary.acquire_lease(session, holder, ttl).await?;
        self.note_lease(session, &outcome);
        Ok(outcome)
    }

    async fn read_lease(&self, session: &SessionId) -> Result<Option<LeaseInfo>, StoreError> {
        self.primary.read_lease(session).await
    }

    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
        ttl: Duration,
    ) -> Result<LeaseOutcome, StoreError> {
        let outcome = self.primary.refresh_lease(session, holder, ttl).await?;
        self.note_lease(session, &outcome);
        Ok(outcome)
    }

    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &LeaseHolder,
    ) -> Result<(), StoreError> {
        self.primary.release_lease(session, holder).await?;
        self.with_state(session, |st| st.held = None);
        Ok(())
    }

    async fn record_lease_refusal(
        &self,
        session: &SessionId,
        refused_by: &str,
        current_holder: &str,
    ) -> Result<(), StoreError> {
        self.primary
            .record_lease_refusal(session, refused_by, current_holder)
            .await
    }

    async fn pending_lease_refusals(
        &self,
        session: &SessionId,
        since: DateTime<Utc>,
    ) -> Result<Vec<LeaseRefusal>, StoreError> {
        self.primary.pending_lease_refusals(session, since).await
    }

    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.primary.write_flush_stats(session, stats).await
    }

    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<SessionFlushStats>, StoreError> {
        self.primary.read_flush_stats(session).await
    }
}

impl TieredStore {
    /// Remember whether this store holds the session's lease, and under which
    /// token: only a holder repairs the index.
    fn note_lease(&self, session: &SessionId, outcome: &LeaseOutcome) {
        let held = match outcome {
            LeaseOutcome::Acquired(info) => Some(info.token),
            LeaseOutcome::Held { .. } => None,
        };
        self.with_state(session, |st| st.held = held);
    }
}
