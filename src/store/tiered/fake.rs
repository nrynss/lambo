//! In-process [`RecallIndex`] for tests: Elasticsearch's external-version
//! rules, exact kNN, and switchable faults. No network, no container.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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

#[derive(Default)]
pub(crate) struct FakeIndex {
    /// `(index, node id)` -> stored.
    pub docs: Mutex<BTreeMap<(String, String), Stored>>,
    pub markers: Mutex<HashMap<String, (u64, SyncMarker)>>,
    pub indices: Mutex<HashSet<String>>,
    /// Every call fails while set (an unreachable cluster).
    pub down: AtomicBool,
    /// Only kNN fails while set (a query-side outage).
    pub knn_down: AtomicBool,
    pub knn_calls: AtomicUsize,
    pub bulk_calls: AtomicUsize,
}

impl FakeIndex {
    pub(crate) fn new() -> Self {
        Self::default()
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

    /// Live documents for `session`, by node id.
    pub(crate) fn live(&self, session: &SessionId) -> BTreeMap<String, (String, IndexDoc)> {
        self.docs
            .lock()
            .iter()
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

    /// Plant a document directly (a stale hit the durable store no longer has).
    pub(crate) fn plant(&self, contract: &EmbeddingContract, doc: IndexDoc) {
        let index = self.index_name(contract);
        self.indices.lock().insert(index.clone());
        self.docs.lock().insert(
            (index, doc.node_id.clone()),
            Stored {
                version: doc.v,
                doc: Some(doc),
            },
        );
    }

    /// Apply one versioned write; `false` when it lost to a newer version.
    fn apply(&self, key: (String, String), version: Option<u64>, doc: Option<IndexDoc>) -> bool {
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
        Ok(())
    }

    async fn delete_ids(&self, ids: &[NodeId]) -> Result<(), StoreError> {
        self.check_up()?;
        let wanted: HashSet<String> = ids.iter().map(|i| i.0.to_string()).collect();
        for ((_, id), s) in self.docs.lock().iter_mut() {
            if wanted.contains(id) && s.doc.is_some() {
                s.doc = None;
                s.version += 1;
            }
        }
        Ok(())
    }

    async fn delete_session_docs(
        &self,
        session: &SessionId,
        below: Option<u64>,
    ) -> Result<(), StoreError> {
        self.check_up()?;
        for s in self.docs.lock().values_mut() {
            if let Some(doc) = &s.doc
                && doc.session_id == session.0
                && below.is_none_or(|b| doc.v < b)
            {
                s.doc = None;
                s.version += 1;
            }
        }
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
        if self.knn_down.load(Ordering::SeqCst) {
            return Err(StoreError::Backend("recall index: search timed out".into()));
        }
        let index = self.index_name(contract);
        let mut hits: Vec<KnnHit> = self
            .docs
            .lock()
            .iter()
            .filter(|((i, _), _)| *i == index)
            .filter_map(|(_, s)| s.doc.as_ref())
            .filter(|d| d.session_id == session.0)
            .map(|d| KnnHit {
                id: NodeId(uuid::Uuid::parse_str(&d.node_id).expect("uuid")),
                cosine: f64::from(crate::embed::cosine(probe, &d.embedding)),
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
