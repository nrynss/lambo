//! The recall index seam: what `TieredStore` needs from a search engine.
//!
//! One narrow async trait, [`RecallIndex`], with the Elasticsearch REST client
//! (`elastic::ElasticRecall`) as the production implementation and an
//! in-process fake for tests. `TieredStore` owns all the policy (what to
//! mirror, when the index is trusted, when to fall back); an implementation
//! only moves documents and answers kNN queries.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use crate::types::{EmbeddingContract, NodeId, SessionId, StoreError};

/// One concept as the recall index stores it.
///
/// Only what the vector leg and the tie-break need, plus the text for a
/// future hybrid leg. Canonization state is deliberately absent: an
/// `UpsertNode` carries a snapshot of the concept that may be stale on those
/// columns (R2-1), so a mirrored copy of them would be wrong in exactly the
/// cases the durable store goes out of its way to get right.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub(crate) struct IndexDoc {
    pub session_id: String,
    pub node_id: String,
    /// The issue-2 tie-break key, returned with every hit.
    pub canonical_key: String,
    pub content: String,
    pub concept_type: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    pub embedding: Vec<f32>,
    /// The external version this document was written at (0 for an unleased
    /// write). Stored as a field as well so a reconcile can drop every
    /// session document older than the one it just wrote.
    pub v: u64,
}

/// One write against a data index.
///
/// Both variants carry the external version the write is made at, so a
/// replayed or late write that is older than what the index holds is refused
/// by the engine (a version conflict, counted as success). `None` is an
/// unleased write, which takes the engine's own last-write-wins versioning.
#[derive(Clone, Debug, PartialEq)]
pub(crate) enum DocOp {
    Index {
        contract: EmbeddingContract,
        id: NodeId,
        version: Option<u64>,
        doc: IndexDoc,
    },
    Delete {
        contract: EmbeddingContract,
        id: NodeId,
        version: Option<u64>,
    },
}

impl DocOp {
    pub(crate) fn id(&self) -> NodeId {
        match self {
            Self::Index { id, .. } | Self::Delete { id, .. } => *id,
        }
    }

    pub(crate) fn contract(&self) -> &EmbeddingContract {
        match self {
            Self::Index { contract, .. } | Self::Delete { contract, .. } => contract,
        }
    }
}

/// One kNN hit, already mapped to cosine similarity.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct KnnHit {
    pub id: NodeId,
    /// Cosine similarity in `[-1, 1]`, the scale every other vector source
    /// returns (`rank_by_cosine`).
    pub cosine: f64,
    pub canonical_key: String,
}

/// What one delete-by-query did. The engine deletes only what its search
/// snapshot shows and skips (under `conflicts=proceed`) every matched
/// document rewritten between that snapshot and its delete, so a caller that
/// needs the documents gone must look at `version_conflicts`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct DeleteReport {
    pub deleted: u64,
    pub version_conflicts: u64,
}

/// The per-session sync marker: the durable mutation epoch the index is known
/// to reflect. Written after a clean mirror or reconcile, compared with the
/// durable snapshot's `mutation_epoch` when a session is loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct SyncMarker {
    pub synced_epoch: u64,
}

#[async_trait]
pub(crate) trait RecallIndex: Send + Sync {
    /// The data index name a contract's vectors live in. Pure.
    fn index_name(&self, contract: &EmbeddingContract) -> String;

    /// Create the shared marker index if it is missing (idempotent).
    async fn provision(&self) -> Result<(), StoreError>;

    /// Create `contract`'s data index with its explicit mapping if it is
    /// missing (idempotent).
    async fn ensure_index(&self, contract: &EmbeddingContract) -> Result<(), StoreError>;

    /// Apply `ops`. A version conflict (the index already holds a newer
    /// write) and a delete of an absent document are success; any other
    /// per-item failure fails the call.
    async fn bulk(&self, ops: &[DocOp]) -> Result<(), StoreError>;

    /// Make every write so far visible to search, count and
    /// delete-by-query in every data index. Search is near-real-time: a
    /// write made with `refresh=false` is invisible to all three until the
    /// next refresh, so a delete-by-query that must catch it refreshes first.
    async fn refresh(&self) -> Result<(), StoreError>;

    /// Delete documents by node id from every data index. Used only for a
    /// delete-only batch that cannot be attributed to a session (node ids are
    /// globally unique, so no session filter is needed to delete them).
    /// Deletes only what is searchable: refresh first.
    async fn delete_ids(&self, ids: &[NodeId]) -> Result<DeleteReport, StoreError>;

    /// Delete the session's documents from every data index, all of them
    /// (`below = None`) or only those written at a version below `below`.
    /// Deletes only what is searchable: refresh first.
    async fn delete_session_docs(
        &self,
        session: &SessionId,
        below: Option<u64>,
    ) -> Result<DeleteReport, StoreError>;

    /// How many of the session's documents a search finds in every data
    /// index (as of the last refresh).
    async fn count_session_docs(&self, session: &SessionId) -> Result<u64, StoreError>;

    /// The `k` nearest session documents in `contract`'s index. A missing
    /// index is an empty answer, not an error.
    async fn knn(
        &self,
        contract: &EmbeddingContract,
        session: &SessionId,
        probe: &[f32],
        k: usize,
    ) -> Result<Vec<KnnHit>, StoreError>;

    async fn read_marker(&self, session: &SessionId) -> Result<Option<SyncMarker>, StoreError>;

    /// Write the marker at `version` (external; a conflict is success).
    async fn write_marker(
        &self,
        session: &SessionId,
        marker: SyncMarker,
        version: Option<u64>,
    ) -> Result<(), StoreError>;

    /// Remove the marker (absent is success).
    async fn delete_marker(&self, session: &SessionId) -> Result<(), StoreError>;
}
