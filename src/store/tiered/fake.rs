//! In-process [`RecallIndex`] for tests: Elasticsearch's external-version
//! rules, its near-real-time search, and switchable faults. No network, no
//! container.
//!
//! # What the fake models (and why)
//!
//! The engine keeps two views of every index, and the recall tier's
//! correctness depends on the difference:
//!
//! * **Realtime** (`docs`): what a versioned write or a get-by-id sees. External
//!   versions are checked here, so a replay or a late write loses at once.
//! * **Searchable** (`searchable`): what a search, a kNN query, a count or a
//!   delete-by-query sees, as of the last **refresh**. A write made with
//!   `refresh=false` stays invisible to all four until the next refresh.
//!
//! `FakeIndex::new()` refreshes after every write (an index configured with
//! `refresh = "wait_for"`), so a test that is not about visibility need not
//! think about it. `FakeIndex::lagging()` leaves writes pending until
//! [`FakeIndex::refresh_now`] (the default `refresh = "false"` deployment).
//!
//! **Delete-by-query** follows the engine: it scrolls the *searchable*
//! snapshot, deletes each hit only if the realtime version still equals the
//! snapshot's (a document rewritten since is a **version conflict**, skipped
//! and counted under `conflicts=proceed`), then refreshes (`refresh=true` on
//! the request refreshes after the deletes, not before). A pending document
//! is therefore never deleted, and becomes visible afterwards.
//! [`FakeIndex::conflict_next_delete`] injects the race the engine reports as
//! a conflict: a mirror write landing on a matched document between the
//! snapshot and its delete.
//!
//! **Scores.** By default kNN is exact cosine. [`FakeIndex::quantized`] scores
//! against int8 scalar-quantized document vectors and passes the result
//! through the engine's `f32` `(1 + cos) / 2` score, which is what a
//! `dense_vector` gets by default on 8.14 and later (`int8_hnsw`). The stored
//! float vector is still returned from `_source`.
//!
//! **Latency.** `delay_*` stall a call (a slow cluster); `hold_deletes` parks
//! every delete-by-query until released (a long-running delete).

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;

use super::index::{DocOp, IndexDoc, KnnHit, RecallIndex, SyncMarker};
use super::project::contract_hash;
use crate::types::{EmbeddingContract, NodeId, SessionId, StoreError};

/// One stored document or delete tombstone, with its version.
#[derive(Clone, Debug)]
pub(crate) struct Stored {
    pub version: u64,
    pub doc: Option<IndexDoc>,
}

type Key = (String, String);

#[derive(Default)]
pub(crate) struct FakeIndex {
    /// Realtime view: `(index, node id)` -> stored.
    pub docs: Mutex<BTreeMap<Key, Stored>>,
    /// What search sees, as of the last refresh.
    pub searchable: Mutex<BTreeMap<Key, Stored>>,
    pub markers: Mutex<HashMap<String, (u64, SyncMarker)>>,
    pub indices: Mutex<HashSet<String>>,
    /// Writes stay invisible to search until [`Self::refresh_now`].
    pub lag: AtomicBool,
    /// kNN scores come from int8-quantized vectors.
    pub quantize: AtomicBool,
    /// Every call fails while set (an unreachable cluster).
    pub down: AtomicBool,
    /// Only kNN fails while set (a query-side outage).
    pub knn_down: AtomicBool,
    /// The next delete-by-query finds this many of its matched documents
    /// rewritten under it (a concurrent mirror): version conflicts.
    pub conflicts_next_delete: AtomicUsize,
    /// Milliseconds every bulk / kNN call stalls before answering.
    pub delay_bulk_ms: AtomicU64,
    pub delay_knn_ms: AtomicU64,
    /// Parks every delete-by-query until a permit is added.
    pub delete_gate: Mutex<Option<std::sync::Arc<tokio::sync::Semaphore>>>,
    pub knn_calls: AtomicUsize,
    pub bulk_calls: AtomicUsize,
    pub delete_calls: AtomicUsize,
    /// Version conflicts the last delete-by-query skipped.
    pub last_delete_conflicts: AtomicU64,
}

impl FakeIndex {
    /// Writes are searchable at once (`refresh = "wait_for"`).
    pub(crate) fn new() -> Self {
        Self::default()
    }

    /// Writes wait for a refresh (`refresh = "false"`, the default).
    pub(crate) fn lagging() -> Self {
        let f = Self::default();
        f.lag.store(true, Ordering::SeqCst);
        f
    }

    /// kNN scores from int8-quantized document vectors.
    pub(crate) fn quantized(self) -> Self {
        self.quantize.store(true, Ordering::SeqCst);
        self
    }

    fn check_up(&self) -> Result<(), StoreError> {
        if self.down.load(Ordering::SeqCst) {
            Err(StoreError::Backend(
                "recall index: connection refused".into(),
            ))
        } else {
            Ok(())
        }
    }

    pub(crate) fn set_down(&self, down: bool) {
        self.down.store(down, Ordering::SeqCst);
    }

    /// Make every pending write searchable (the engine's periodic refresh).
    pub(crate) fn refresh_now(&self) {
        let docs = self.docs.lock().clone();
        *self.searchable.lock() = docs;
    }

    fn after_write(&self) {
        if !self.lag.load(Ordering::SeqCst) {
            self.refresh_now();
        }
    }

    async fn pass_delete_gate(&self) {
        let gate = self.delete_gate.lock().clone();
        if let Some(gate) = gate {
            let _ = gate.acquire().await;
        }
    }

    async fn stall(ms: &AtomicU64) {
        let ms = ms.load(Ordering::SeqCst);
        if ms > 0 {
            tokio::time::sleep(Duration::from_millis(ms)).await;
        }
    }

    /// Documents the index holds for `session` (realtime, pending included),
    /// by node id.
    pub(crate) fn live(&self, session: &SessionId) -> BTreeMap<String, (String, IndexDoc)> {
        Self::session_docs(&self.docs.lock(), session)
    }

    /// Documents a search finds for `session` right now, by node id.
    pub(crate) fn searchable(&self, session: &SessionId) -> BTreeMap<String, (String, IndexDoc)> {
        Self::session_docs(&self.searchable.lock(), session)
    }

    fn session_docs(
        view: &BTreeMap<Key, Stored>,
        session: &SessionId,
    ) -> BTreeMap<String, (String, IndexDoc)> {
        view.iter()
            .filter_map(|((index, id), s)| {
                s.doc
                    .as_ref()
                    .filter(|d| d.session_id == session.0)
                    .map(|d| (id.clone(), (index.clone(), d.clone())))
            })
            .collect()
    }

    pub(crate) fn marker(&self, session: &SessionId) -> Option<u64> {
        self.markers
            .lock()
            .get(&session.0)
            .map(|(_, m)| m.synced_epoch)
    }

    /// Plant a document directly (a stale hit the durable store no longer
    /// has). Searchable at once.
    pub(crate) fn plant(&self, contract: &EmbeddingContract, doc: IndexDoc) {
        let index = self.index_name(contract);
        self.indices.lock().insert(index.clone());
        let stored = Stored {
            version: doc.v,
            doc: Some(doc.clone()),
        };
        self.docs
            .lock()
            .insert((index.clone(), doc.node_id.clone()), stored.clone());
        self.searchable.lock().insert((index, doc.node_id), stored);
    }

    /// Apply one versioned write; `false` when it lost to a newer version.
    fn apply(&self, key: Key, version: Option<u64>, doc: Option<IndexDoc>) -> bool {
        let mut docs = self.docs.lock();
        let current = docs.get(&key).map(|s| s.version);
        let next = match (version, current) {
            (Some(v), Some(cur)) if v <= cur => return false,
            (Some(v), _) => v,
            (None, cur) => cur.unwrap_or(0) + 1,
        };
        docs.insert(key, Stored { version: next, doc });
        true
    }

    /// The engine's delete-by-query: scroll the searchable snapshot, delete
    /// each match whose realtime version is unchanged, count the rest as
    /// conflicts, then refresh. Returns `(deleted, conflicts)`.
    async fn delete_by_query(&self, matches: impl Fn(&Key, &IndexDoc) -> bool) -> (u64, u64) {
        self.delete_calls.fetch_add(1, Ordering::SeqCst);
        self.pass_delete_gate().await;
        let snapshot: Vec<(Key, u64)> = self
            .searchable
            .lock()
            .iter()
            .filter_map(|(k, s)| {
                s.doc
                    .as_ref()
                    .filter(|d| matches(k, d))
                    .map(|_| (k.clone(), s.version))
            })
            .collect();
        let mut inject = self.conflicts_next_delete.swap(0, Ordering::SeqCst);
        let (mut deleted, mut conflicts) = (0u64, 0u64);
        {
            let mut docs = self.docs.lock();
            for (key, seen) in snapshot {
                let Some(current) = docs.get_mut(&key) else {
                    conflicts += 1;
                    continue;
                };
                if inject > 0 {
                    // A mirror rewrites the document between the scroll and
                    // the delete: same content, the next version.
                    inject -= 1;
                    current.version += 1;
                }
                if current.version == seen && current.doc.is_some() {
                    current.doc = None;
                    current.version += 1;
                    deleted += 1;
                } else {
                    conflicts += 1;
                }
            }
        }
        self.last_delete_conflicts
            .store(conflicts, Ordering::SeqCst);
        // `refresh=true` refreshes after the deletes.
        self.refresh_now();
        (deleted, conflicts)
    }

    /// The engine's score for `doc` against `probe`, mapped back to cosine.
    fn score(&self, probe: &[f32], doc: &[f32]) -> f64 {
        if !self.quantize.load(Ordering::SeqCst) {
            return f64::from(crate::embed::cosine(probe, doc));
        }
        let max = doc
            .iter()
            .fold(0f32, |m, x| m.max(x.abs()))
            .max(f32::MIN_POSITIVE);
        let q: Vec<f32> = doc
            .iter()
            .map(|x| (x / max * 127.0).round() * max / 127.0)
            .collect();
        let cos = crate::embed::cosine(probe, &q);
        // `_score` is an f32 of (1 + cos) / 2.
        let score = (1.0 + cos) / 2.0;
        2.0 * f64::from(score) - 1.0
    }
}

#[async_trait]
impl RecallIndex for FakeIndex {
    fn index_name(&self, contract: &EmbeddingContract) -> String {
        format!("test-v-{}", contract_hash(contract))
    }

    async fn provision(&self) -> Result<(), StoreError> {
        self.check_up()
    }

    async fn ensure_index(&self, contract: &EmbeddingContract) -> Result<(), StoreError> {
        self.check_up()?;
        self.indices.lock().insert(self.index_name(contract));
        Ok(())
    }

    async fn bulk(&self, ops: &[DocOp]) -> Result<(), StoreError> {
        self.check_up()?;
        Self::stall(&self.delay_bulk_ms).await;
        self.bulk_calls.fetch_add(1, Ordering::SeqCst);
        for op in ops {
            let index = self.index_name(op.contract());
            if !self.indices.lock().contains(&index) {
                return Err(StoreError::Backend(format!("no such index {index}")));
            }
            let key = (index, op.id().0.to_string());
            match op {
                DocOp::Index { version, doc, .. } => {
                    self.apply(key, *version, Some(doc.clone()));
                }
                DocOp::Delete { version, .. } => {
                    self.apply(key, *version, None);
                }
            }
        }
        self.after_write();
        Ok(())
    }

    async fn delete_ids(&self, ids: &[NodeId]) -> Result<(), StoreError> {
        self.check_up()?;
        let wanted: HashSet<String> = ids.iter().map(|i| i.0.to_string()).collect();
        self.delete_by_query(|(_, id), _| wanted.contains(id)).await;
        Ok(())
    }

    async fn delete_session_docs(
        &self,
        session: &SessionId,
        below: Option<u64>,
    ) -> Result<(), StoreError> {
        self.check_up()?;
        self.delete_by_query(|_, doc| {
            doc.session_id == session.0 && below.is_none_or(|b| doc.v < b)
        })
        .await;
        Ok(())
    }

    async fn knn(
        &self,
        contract: &EmbeddingContract,
        session: &SessionId,
        probe: &[f32],
        k: usize,
    ) -> Result<Vec<KnnHit>, StoreError> {
        self.check_up()?;
        self.knn_calls.fetch_add(1, Ordering::SeqCst);
        Self::stall(&self.delay_knn_ms).await;
        if self.knn_down.load(Ordering::SeqCst) {
            return Err(StoreError::Backend("recall index: search timed out".into()));
        }
        let index = self.index_name(contract);
        let mut hits: Vec<KnnHit> = self
            .searchable
            .lock()
            .iter()
            .filter(|((i, _), _)| *i == index)
            .filter_map(|(_, s)| s.doc.as_ref())
            .filter(|d| d.session_id == session.0)
            .map(|d| KnnHit {
                id: NodeId(uuid::Uuid::parse_str(&d.node_id).expect("uuid")),
                cosine: self.score(probe, &d.embedding),
                canonical_key: d.canonical_key.clone(),
            })
            .collect();
        hits.sort_by(|a, b| b.cosine.total_cmp(&a.cosine));
        hits.truncate(k);
        Ok(hits)
    }

    async fn read_marker(&self, session: &SessionId) -> Result<Option<SyncMarker>, StoreError> {
        self.check_up()?;
        Ok(self.markers.lock().get(&session.0).map(|(_, m)| *m))
    }

    async fn write_marker(
        &self,
        session: &SessionId,
        marker: SyncMarker,
        version: Option<u64>,
    ) -> Result<(), StoreError> {
        self.check_up()?;
        let mut markers = self.markers.lock();
        let current = markers.get(&session.0).map(|(v, _)| *v);
        let next = match (version, current) {
            (Some(v), Some(cur)) if v <= cur => return Ok(()),
            (Some(v), _) => v,
            (None, cur) => cur.unwrap_or(0) + 1,
        };
        markers.insert(session.0.clone(), (next, marker));
        Ok(())
    }

    async fn delete_marker(&self, session: &SessionId) -> Result<(), StoreError> {
        self.check_up()?;
        self.markers.lock().remove(&session.0);
        Ok(())
    }
}

/// Lets a test keep a handle on the fake after handing the store a box.
#[async_trait]
impl RecallIndex for std::sync::Arc<FakeIndex> {
    fn index_name(&self, contract: &EmbeddingContract) -> String {
        (**self).index_name(contract)
    }
    async fn provision(&self) -> Result<(), StoreError> {
        (**self).provision().await
    }
    async fn ensure_index(&self, contract: &EmbeddingContract) -> Result<(), StoreError> {
        (**self).ensure_index(contract).await
    }
    async fn bulk(&self, ops: &[DocOp]) -> Result<(), StoreError> {
        (**self).bulk(ops).await
    }
    async fn delete_ids(&self, ids: &[NodeId]) -> Result<(), StoreError> {
        (**self).delete_ids(ids).await
    }
    async fn delete_session_docs(
        &self,
        session: &SessionId,
        below: Option<u64>,
    ) -> Result<(), StoreError> {
        (**self).delete_session_docs(session, below).await
    }
    async fn knn(
        &self,
        contract: &EmbeddingContract,
        session: &SessionId,
        probe: &[f32],
        k: usize,
    ) -> Result<Vec<KnnHit>, StoreError> {
        (**self).knn(contract, session, probe, k).await
    }
    async fn read_marker(&self, session: &SessionId) -> Result<Option<SyncMarker>, StoreError> {
        (**self).read_marker(session).await
    }
    async fn write_marker(
        &self,
        session: &SessionId,
        marker: SyncMarker,
        version: Option<u64>,
    ) -> Result<(), StoreError> {
        (**self).write_marker(session, marker, version).await
    }
    async fn delete_marker(&self, session: &SessionId) -> Result<(), StoreError> {
        (**self).delete_marker(session).await
    }
}

/// The fake's own model, pinned so a test that relies on it cannot pass
/// against a fake that quietly stopped modelling the engine.
#[cfg(test)]
mod model {
    use super::*;

    fn contract() -> EmbeddingContract {
        EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 2,
        }
    }

    fn doc(sid: &str, id: NodeId, v: u64, e: [f32; 2]) -> IndexDoc {
        IndexDoc {
            session_id: sid.into(),
            node_id: id.0.to_string(),
            canonical_key: "k".into(),
            content: "c".into(),
            concept_type: "entity".into(),
            created_at: chrono::Utc::now(),
            embedding: e.to_vec(),
            v,
        }
    }

    fn index(id: NodeId, v: u64) -> DocOp {
        DocOp::Index {
            contract: contract(),
            id,
            version: Some(v),
            doc: doc("s", id, v, [1.0, 0.0]),
        }
    }

    #[tokio::test]
    async fn a_lagging_write_is_invisible_to_search_and_to_delete_by_query() {
        let f = FakeIndex::lagging();
        let sid = SessionId::new("s");
        f.ensure_index(&contract()).await.unwrap();
        let a = NodeId::new();
        f.bulk(&[index(a, 5)]).await.unwrap();
        assert_eq!(f.live(&sid).len(), 1, "held in realtime");
        assert!(f.searchable(&sid).is_empty(), "not yet searchable");
        assert!(f
            .knn(&contract(), &sid, &[1.0, 0.0], 5)
            .await
            .unwrap()
            .is_empty());
        f.delete_session_docs(&sid, None).await.unwrap();
        assert_eq!(
            f.searchable(&sid).len(),
            1,
            "delete-by-query missed the pending doc and its refresh exposed it"
        );
    }

    #[tokio::test]
    async fn a_document_rewritten_under_a_delete_by_query_is_a_conflict() {
        let f = FakeIndex::new();
        let sid = SessionId::new("s");
        f.ensure_index(&contract()).await.unwrap();
        f.bulk(&[index(NodeId::new(), 5), index(NodeId::new(), 5)])
            .await
            .unwrap();
        f.conflicts_next_delete.store(1, Ordering::SeqCst);
        f.delete_session_docs(&sid, None).await.unwrap();
        assert_eq!(f.last_delete_conflicts.load(Ordering::SeqCst), 1);
        assert_eq!(f.searchable(&sid).len(), 1, "the conflicting doc survives");
    }

    #[tokio::test]
    async fn quantized_scores_are_close_but_not_exact() {
        let f = FakeIndex::new().quantized();
        let sid = SessionId::new("s");
        f.ensure_index(&contract()).await.unwrap();
        let a = NodeId::new();
        let e = [0.8f32, 0.123_456_7];
        f.bulk(&[DocOp::Index {
            contract: contract(),
            id: a,
            version: Some(1),
            doc: doc("s", a, 1, e),
        }])
        .await
        .unwrap();
        let probe = [0.3f32, 0.9];
        let hit = &f.knn(&contract(), &sid, &probe, 1).await.unwrap()[0];
        let exact = f64::from(crate::embed::cosine(&probe, &e));
        let err = (hit.cosine - exact).abs();
        assert!(err > 1e-6 && err < 1e-2, "quantization noise {err}");
    }
}
