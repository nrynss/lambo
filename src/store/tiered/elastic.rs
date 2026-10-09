//! The Elasticsearch REST client behind the recall tier (#18).
//!
//! Plain HTTP over the crate's existing `reqwest` (no Elasticsearch crate:
//! the surface used is six endpoints). Index layout:
//!
//! * `{prefix}-v-{contract hash}` — one data index per embedding contract,
//!   created on first use with an explicit mapping: `embedding` is a
//!   `dense_vector` of the contract's width with cosine similarity (HNSW),
//!   `session_id` a keyword filter, `v` the document's external version.
//! * `{prefix}-meta` — one sync marker document per session. Its `_id` is
//!   the hex SHA-256 of the session id ([`marker_id`]), never the raw id: the
//!   URL layer drops `.` and `..` path segments and the engine refuses an
//!   `_id` over 512 bytes. The session id is stored in the document.
//!
//! Secrets: the API key comes from the environment variable `[recall]
//! api_key = { env = ... }` names, is sent only in the `Authorization`
//! header (marked sensitive), and appears in no error or log line. A URL that
//! carries credentials is refused at construction.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use async_trait::async_trait;
use parking_lot::Mutex;
use reqwest::header::{HeaderValue, AUTHORIZATION, CONTENT_TYPE};
use reqwest::{Method, StatusCode, Url};
use serde_json::{json, Value};

use super::index::{DeleteReport, DocOp, KnnHit, RecallIndex, SyncMarker};
use super::project::{contract_hash, validate_index_prefix};
use crate::store::RecallConfig;
use crate::types::{EmbeddingContract, NodeId, SessionId, StoreError};

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(5000);

/// The budget for a request whose work grows with the session: a refresh, a
/// delete-by-query over every session document, a count (#18 review M1). The
/// configured `timeout_ms` is meant for single-document writes and kNN; a
/// delete-by-query over a large session can take far longer, and timing it
/// out would leave the session stale forever while the engine keeps
/// deleting. The configured timeout still bounds connecting.
const MAINTENANCE_TIMEOUT: Duration = Duration::from_secs(300);

/// `num_candidates` floor for a kNN query (the issue's `max(limit * 4, 100)`).
const MIN_NUM_CANDIDATES: usize = 100;
/// Elasticsearch's ceiling for `num_candidates`.
const MAX_NUM_CANDIDATES: usize = 10_000;

pub(crate) struct ElasticRecall {
    client: reqwest::Client,
    base: Url,
    prefix: String,
    auth: Option<HeaderValue>,
    refresh: &'static str,
    /// The per-request timeout (`timeout_ms`).
    timeout: Duration,
    meta_ready: AtomicBool,
    indices_ready: Mutex<HashSet<String>>,
}

fn backend(what: &str, e: impl std::fmt::Display) -> StoreError {
    StoreError::Backend(format!("recall index: {what}: {e}"))
}

impl ElasticRecall {
    /// Build the client. No network I/O: the first request connects.
    pub(crate) fn new(cfg: &RecallConfig) -> Result<Self, StoreError> {
        validate_index_prefix(&cfg.index_prefix)?;
        let base = Url::parse(cfg.url.trim())
            .map_err(|e| StoreError::Backend(format!("recall.url is not a valid URL: {e}")))?;
        if !matches!(base.scheme(), "http" | "https") || base.cannot_be_a_base() {
            return Err(StoreError::Backend(
                "recall.url must be an http:// or https:// URL".into(),
            ));
        }
        if !base.username().is_empty() || base.password().is_some() {
            return Err(StoreError::Backend(
                "recall.url carries credentials; secrets are by reference only: put the API \
                 key in an environment variable and name it with recall.api_key = { env = \"...\" }"
                    .into(),
            ));
        }
        let auth = match &cfg.api_key {
            Some(secret) => {
                let mut value = HeaderValue::from_str(&format!("ApiKey {}", secret.resolve()?))
                    .map_err(|_| {
                        StoreError::Backend(format!(
                            "the API key in {} is not a valid header value",
                            secret.env
                        ))
                    })?;
                value.set_sensitive(true);
                Some(value)
            }
            None => None,
        };
        let timeout = cfg
            .timeout_ms
            .map(Duration::from_millis)
            .unwrap_or(DEFAULT_TIMEOUT);
        let client = reqwest::Client::builder()
            .connect_timeout(timeout)
            .timeout(timeout)
            .build()
            .map_err(|e| backend("build HTTP client", e))?;
        Ok(Self {
            client,
            base,
            prefix: cfg.index_prefix.clone(),
            auth,
            refresh: cfg.refresh.as_param(),
            timeout,
            meta_ready: AtomicBool::new(false),
            indices_ready: Mutex::new(HashSet::new()),
        })
    }

    /// `{meta}/_doc/{marker id}`.
    fn marker_url(&self, session: &SessionId, query: &[(&str, &str)]) -> Result<Url, StoreError> {
        let meta = self.meta_index();
        let id = marker_id(session);
        self.url(&[&meta, "_doc", &id], query)
    }

    fn meta_index(&self) -> String {
        format!("{}-meta", self.prefix)
    }

    fn data_pattern(&self) -> String {
        format!("{}-v-*", self.prefix)
    }

    /// `base/segment/segment…`, each segment percent-encoded.
    fn url(&self, segments: &[&str], query: &[(&str, &str)]) -> Result<Url, StoreError> {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .map_err(|()| backend("build URL", "base URL cannot take a path"))?
            .pop_if_empty()
            .extend(segments);
        if !query.is_empty() {
            let mut pairs = url.query_pairs_mut();
            for (k, v) in query {
                pairs.append_pair(k, v);
            }
        }
        Ok(url)
    }

    /// Send one request; the body comes back as JSON (`Null` when empty).
    async fn send(
        &self,
        what: &str,
        method: Method,
        url: Url,
        body: Option<Body>,
    ) -> Result<(StatusCode, Value), StoreError> {
        self.send_within(what, method, url, body, None).await
    }

    /// [`Self::send`] with the maintenance budget instead of the per-request
    /// timeout.
    async fn send_maintenance(
        &self,
        what: &str,
        method: Method,
        url: Url,
        body: Option<Body>,
    ) -> Result<(StatusCode, Value), StoreError> {
        let budget = MAINTENANCE_TIMEOUT.max(self.timeout);
        self.send_within(what, method, url, body, Some(budget))
            .await
    }

    async fn send_within(
        &self,
        what: &str,
        method: Method,
        url: Url,
        body: Option<Body>,
        timeout: Option<Duration>,
    ) -> Result<(StatusCode, Value), StoreError> {
        let mut req = self.client.request(method, url);
        if let Some(timeout) = timeout {
            req = req.timeout(timeout);
        }
        if let Some(auth) = &self.auth {
            req = req.header(AUTHORIZATION, auth.clone());
        }
        req = match body {
            Some(Body::Json(v)) => req.json(&v),
            Some(Body::NdJson(text)) => req.header(CONTENT_TYPE, "application/x-ndjson").body(text),
            None => req,
        };
        let resp = req.send().await.map_err(|e| backend(what, e))?;
        let status = resp.status();
        let bytes = resp.bytes().await.map_err(|e| backend(what, e))?;
        let value = if bytes.is_empty() {
            Value::Null
        } else {
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        };
        Ok((status, value))
    }

    fn error_type(body: &Value) -> &str {
        body["error"]["type"].as_str().unwrap_or("")
    }

    fn fail(what: &str, status: StatusCode, body: &Value) -> StoreError {
        let reason = body["error"]["reason"].as_str().unwrap_or("");
        backend(
            what,
            format!("HTTP {status} {} {reason}", Self::error_type(body)),
        )
    }

    /// Create `index` with `body` unless it exists.
    async fn create_index(&self, index: &str, body: Value) -> Result<(), StoreError> {
        let url = self.url(&[index], &[])?;
        let (status, resp) = self
            .send("create index", Method::PUT, url, Some(Body::Json(body)))
            .await?;
        if status.is_success() || Self::error_type(&resp) == "resource_already_exists_exception" {
            Ok(())
        } else {
            Err(Self::fail("create index", status, &resp))
        }
    }

    fn bulk_body(&self, ops: &[DocOp]) -> Result<String, StoreError> {
        let mut out = String::new();
        for op in ops {
            let index = self.index_name(op.contract());
            let id = op.id().0.to_string();
            let (action, version, doc) = match op {
                DocOp::Index { version, doc, .. } => ("index", *version, Some(doc)),
                DocOp::Delete { version, .. } => ("delete", *version, None),
            };
            let mut meta = json!({ "_index": index, "_id": id });
            if let Some(v) = version {
                meta["version"] = json!(v);
                meta["version_type"] = json!("external");
            }
            out.push_str(&json!({ action: meta }).to_string());
            out.push('\n');
            if let Some(doc) = doc {
                out.push_str(
                    &serde_json::to_string(doc).map_err(|e| backend("encode document", e))?,
                );
                out.push('\n');
            }
        }
        Ok(out)
    }

    /// Delete what `query` matches in every data index.
    ///
    /// The engine deletes from its last-refresh search snapshot, so the
    /// caller refreshes first ([`RecallIndex::refresh`]); `refresh=true` here
    /// only makes the deletes themselves visible afterwards. A document
    /// rewritten between the snapshot and its delete is skipped and counted
    /// in `version_conflicts`, which is returned for the caller to act on.
    async fn delete_by_query(&self, what: &str, query: Value) -> Result<DeleteReport, StoreError> {
        let pattern = self.data_pattern();
        let url = self.url(
            &[&pattern, "_delete_by_query"],
            &[
                ("conflicts", "proceed"),
                ("refresh", "true"),
                ("allow_no_indices", "true"),
                ("ignore_unavailable", "true"),
            ],
        )?;
        let (status, resp) = self
            .send_maintenance(
                what,
                Method::POST,
                url,
                Some(Body::Json(json!({ "query": query }))),
            )
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(DeleteReport::default());
        }
        if !status.is_success() {
            return Err(Self::fail(what, status, &resp));
        }
        if let Some(failures) = resp["failures"].as_array()
            && !failures.is_empty()
        {
            return Err(backend(
                what,
                format!("{} documents failed to delete", failures.len()),
            ));
        }
        Ok(DeleteReport {
            deleted: resp["deleted"].as_u64().unwrap_or(0),
            version_conflicts: resp["version_conflicts"].as_u64().unwrap_or(0),
        })
    }

    /// The session filter every session-scoped query uses.
    fn session_filter(session: &SessionId) -> Value {
        json!({ "term": { "session_id": session.0 } })
    }
}

/// The marker document's `_id` for `session`: hex SHA-256 of the session id.
pub(crate) fn marker_id(session: &SessionId) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(session.0.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

/// The stored vector from a hit's `_source`, when the engine returned one
/// (a cluster that excludes vectors from `_source` returns none).
fn stored_vector(v: &Value) -> Option<Vec<f32>> {
    v.as_array()?
        .iter()
        .map(|x| x.as_f64().map(|f| f as f32))
        .collect()
}

enum Body {
    Json(Value),
    NdJson(String),
}

#[async_trait]
impl RecallIndex for ElasticRecall {
    fn index_name(&self, contract: &EmbeddingContract) -> String {
        format!("{}-v-{}", self.prefix, contract_hash(contract))
    }

    async fn provision(&self) -> Result<(), StoreError> {
        if self.meta_ready.load(Ordering::Acquire) {
            return Ok(());
        }
        self.create_index(
            &self.meta_index(),
            json!({
                "mappings": {
                    "dynamic": "strict",
                    "properties": {
                        "synced_epoch": { "type": "long" },
                        "session_id": { "type": "keyword" }
                    }
                }
            }),
        )
        .await?;
        self.meta_ready.store(true, Ordering::Release);
        Ok(())
    }

    async fn ensure_index(&self, contract: &EmbeddingContract) -> Result<(), StoreError> {
        let name = self.index_name(contract);
        if self.indices_ready.lock().contains(&name) {
            return Ok(());
        }
        self.create_index(
            &name,
            json!({
                "mappings": {
                    "dynamic": "strict",
                    "_meta": {
                        "lambo_contract": {
                            "kind": contract.kind,
                            "model": contract.model,
                            "dim": contract.dim,
                        }
                    },
                    "properties": {
                        "session_id": { "type": "keyword" },
                        "node_id": { "type": "keyword" },
                        "canonical_key": { "type": "keyword" },
                        "content": { "type": "text" },
                        "concept_type": { "type": "keyword" },
                        "created_at": { "type": "date" },
                        "v": { "type": "long" },
                        "embedding": {
                            "type": "dense_vector",
                            "dims": contract.dim,
                            "index": true,
                            "similarity": "cosine",
                            // Pinned (#18 review M3): plain float HNSW. Left
                            // unset, 8.14+ defaults to int8_hnsw and 9.1+ to
                            // bbq_hnsw at 384+ dims, both quantized.
                            "index_options": { "type": "hnsw" }
                        }
                    }
                }
            }),
        )
        .await?;
        self.indices_ready.lock().insert(name);
        Ok(())
    }

    async fn bulk(&self, ops: &[DocOp]) -> Result<(), StoreError> {
        if ops.is_empty() {
            return Ok(());
        }
        let body = self.bulk_body(ops)?;
        let url = self.url(&["_bulk"], &[("refresh", self.refresh)])?;
        let (status, resp) = self
            .send("bulk write", Method::POST, url, Some(Body::NdJson(body)))
            .await?;
        if !status.is_success() {
            return Err(Self::fail("bulk write", status, &resp));
        }
        if resp["errors"].as_bool() != Some(true) {
            return Ok(());
        }
        let items = resp["items"].as_array().cloned().unwrap_or_default();
        for item in &items {
            let Some((action, result)) = item.as_object().and_then(|o| o.iter().next()) else {
                continue;
            };
            let code = result["status"].as_u64().unwrap_or(0);
            let accepted = (200..300).contains(&code)
                // A newer write already holds the document: the index is ahead.
                || code == 409
                // Deleting what is not there (replay, or never indexed).
                || (action == "delete" && code == 404);
            if !accepted {
                return Err(backend(
                    "bulk write",
                    format!(
                        "{action} {} failed: HTTP {code} {}",
                        result["_id"].as_str().unwrap_or("?"),
                        result["error"]["type"].as_str().unwrap_or("")
                    ),
                ));
            }
        }
        Ok(())
    }

    async fn refresh(&self) -> Result<(), StoreError> {
        let pattern = self.data_pattern();
        let url = self.url(
            &[&pattern, "_refresh"],
            &[("allow_no_indices", "true"), ("ignore_unavailable", "true")],
        )?;
        let (status, resp) = self
            .send_maintenance("refresh", Method::POST, url, None)
            .await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Self::fail("refresh", status, &resp))
        }
    }

    async fn delete_ids(&self, ids: &[NodeId]) -> Result<DeleteReport, StoreError> {
        if ids.is_empty() {
            return Ok(DeleteReport::default());
        }
        let values: Vec<String> = ids.iter().map(|id| id.0.to_string()).collect();
        self.delete_by_query("delete by id", json!({ "ids": { "values": values } }))
            .await
    }

    async fn delete_session_docs(
        &self,
        session: &SessionId,
        below: Option<u64>,
    ) -> Result<DeleteReport, StoreError> {
        let mut filter = vec![Self::session_filter(session)];
        if let Some(below) = below {
            filter.push(json!({ "range": { "v": { "lt": below } } }));
        }
        self.delete_by_query(
            "delete session documents",
            json!({ "bool": { "filter": filter } }),
        )
        .await
    }

    async fn count_session_docs(&self, session: &SessionId) -> Result<u64, StoreError> {
        let pattern = self.data_pattern();
        let url = self.url(
            &[&pattern, "_count"],
            &[("allow_no_indices", "true"), ("ignore_unavailable", "true")],
        )?;
        let (status, resp) = self
            .send_maintenance(
                "count session documents",
                Method::POST,
                url,
                Some(Body::Json(
                    json!({ "query": Self::session_filter(session) }),
                )),
            )
            .await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(0);
        }
        if !status.is_success() {
            return Err(Self::fail("count session documents", status, &resp));
        }
        resp["count"]
            .as_u64()
            .ok_or_else(|| backend("count session documents", "response without a count"))
    }

    async fn knn(
        &self,
        contract: &EmbeddingContract,
        session: &SessionId,
        probe: &[f32],
        k: usize,
    ) -> Result<Vec<KnnHit>, StoreError> {
        let index = self.index_name(contract);
        let url = self.url(&[&index, "_search"], &[])?;
        let num_candidates = (k * 4).clamp(MIN_NUM_CANDIDATES, MAX_NUM_CANDIDATES).max(k);
        let body = json!({
            "knn": {
                "field": "embedding",
                "query_vector": probe,
                "k": k,
                "num_candidates": num_candidates,
                "filter": Self::session_filter(session)
            },
            "size": k,
            // The stored vector rides along so the tier re-ranks exactly.
            "_source": ["canonical_key", "embedding"]
        });
        let (status, resp) = self
            .send("knn search", Method::POST, url, Some(Body::Json(body)))
            .await?;
        if status == StatusCode::NOT_FOUND && Self::error_type(&resp) == "index_not_found_exception"
        {
            // No vector of this contract was ever mirrored.
            return Ok(Vec::new());
        }
        if !status.is_success() {
            return Err(Self::fail("knn search", status, &resp));
        }
        let hits = resp["hits"]["hits"].as_array().cloned().unwrap_or_default();
        hits.iter()
            .map(|h| {
                let id = h["_id"]
                    .as_str()
                    .and_then(|s| uuid::Uuid::parse_str(s).ok())
                    .ok_or_else(|| backend("knn search", "hit without a node id"))?;
                let score = h["_score"]
                    .as_f64()
                    .ok_or_else(|| backend("knn search", "hit without a score"))?;
                Ok(KnnHit {
                    id: NodeId(id),
                    // `cosine` similarity scores `(1 + cos) / 2`; map back to
                    // the cosine every other vector source returns.
                    cosine: 2.0 * score - 1.0,
                    canonical_key: h["_source"]["canonical_key"]
                        .as_str()
                        .unwrap_or_default()
                        .to_owned(),
                    embedding: stored_vector(&h["_source"]["embedding"]),
                })
            })
            .collect()
    }

    async fn read_marker(&self, session: &SessionId) -> Result<Option<SyncMarker>, StoreError> {
        let url = self.marker_url(session, &[])?;
        let (status, resp) = self.send("read marker", Method::GET, url, None).await?;
        if status == StatusCode::NOT_FOUND {
            return Ok(None);
        }
        if !status.is_success() {
            return Err(Self::fail("read marker", status, &resp));
        }
        if resp["found"].as_bool() != Some(true) {
            return Ok(None);
        }
        serde_json::from_value(resp["_source"].clone())
            .map(Some)
            .map_err(|e| backend("read marker", e))
    }

    async fn write_marker(
        &self,
        session: &SessionId,
        marker: SyncMarker,
        version: Option<u64>,
    ) -> Result<(), StoreError> {
        self.provision().await?;
        let version_text = version.map(|v| v.to_string());
        let mut query = vec![("refresh", self.refresh)];
        if let Some(v) = &version_text {
            query.push(("version", v.as_str()));
            query.push(("version_type", "external"));
        }
        let url = self.marker_url(session, &query)?;
        let mut body = serde_json::to_value(marker).map_err(|e| backend("encode marker", e))?;
        body["session_id"] = json!(session.0);
        let (status, resp) = self
            .send("write marker", Method::PUT, url, Some(Body::Json(body)))
            .await?;
        if status.is_success() || status == StatusCode::CONFLICT {
            Ok(())
        } else {
            Err(Self::fail("write marker", status, &resp))
        }
    }

    async fn delete_marker(&self, session: &SessionId) -> Result<(), StoreError> {
        let url = self.marker_url(session, &[("refresh", "true")])?;
        let (status, resp) = self
            .send("delete marker", Method::DELETE, url, None)
            .await?;
        if status.is_success() || status == StatusCode::NOT_FOUND {
            Ok(())
        } else {
            Err(Self::fail("delete marker", status, &resp))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::{RecallKind, RecallRefresh, SecretRef};
    use httpmock::prelude::*;

    fn cfg(url: &str) -> RecallConfig {
        RecallConfig {
            kind: RecallKind::Elastic,
            url: url.into(),
            api_key: None,
            index_prefix: "lambo".into(),
            refresh: RecallRefresh::False,
            timeout_ms: Some(2000),
        }
    }

    fn contract() -> EmbeddingContract {
        EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 2,
        }
    }

    fn doc(id: NodeId, v: u64) -> super::super::index::IndexDoc {
        super::super::index::IndexDoc {
            session_id: "s".into(),
            node_id: id.0.to_string(),
            canonical_key: "k".into(),
            content: "c".into(),
            concept_type: "entity".into(),
            created_at: chrono::Utc::now(),
            embedding: vec![1.0, 0.0],
            v,
        }
    }

    #[test]
    fn construction_refuses_credentials_bad_urls_and_bad_prefixes() {
        let err = ElasticRecall::new(&cfg("https://user:pw@es.example.com"))
            .err()
            .unwrap();
        assert!(err.to_string().contains("by reference only"), "{err}");
        assert!(!err.to_string().contains("pw"), "never echo a credential");
        assert!(ElasticRecall::new(&cfg("ftp://es.example.com")).is_err());
        assert!(ElasticRecall::new(&cfg("not a url")).is_err());
        let mut bad = cfg("http://127.0.0.1:9200");
        bad.index_prefix = "Upper".into();
        assert!(ElasticRecall::new(&bad).is_err());
        let mut missing = cfg("http://127.0.0.1:9200");
        missing.api_key = Some(SecretRef {
            env: "LAMBO_TEST_RECALL_UNSET_VAR".into(),
        });
        let _env = crate::test_util::env_lock();
        let err = ElasticRecall::new(&missing).err().unwrap();
        assert!(
            err.to_string().contains("LAMBO_TEST_RECALL_UNSET_VAR"),
            "{err}"
        );
    }

    #[tokio::test]
    async fn the_api_key_rides_the_authorization_header() {
        let server = MockServer::start_async().await;
        let mock = server
            .mock_async(|when, then| {
                when.method(GET)
                    .path(format!(
                        "/lambo-meta/_doc/{}",
                        marker_id(&SessionId::new("s"))
                    ))
                    .header("authorization", "ApiKey fake");
                then.status(404).json_body(json!({ "found": false }));
            })
            .await;
        let mut c = cfg(&server.base_url());
        c.api_key = Some(SecretRef {
            env: "LAMBO_TEST_RECALL_AUTH".into(),
        });
        let index = {
            let env = crate::test_util::env_lock();
            env.set("LAMBO_TEST_RECALL_AUTH", "fake");
            ElasticRecall::new(&c).unwrap()
        };
        assert_eq!(index.read_marker(&SessionId::new("s")).await.unwrap(), None);
        mock.assert_async().await;
    }

    #[tokio::test]
    async fn bulk_writes_ndjson_at_external_versions_and_accepts_conflicts() {
        let server = MockServer::start_async().await;
        let index = ElasticRecall::new(&cfg(&server.base_url())).unwrap();
        let (a, b) = (NodeId::new(), NodeId::new());
        let name = index.index_name(&contract());
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/_bulk")
                    .query_param("refresh", "false")
                    .header("content-type", "application/x-ndjson")
                    .body_contains("\"version_type\":\"external\"")
                    .body_contains(format!("\"_index\":\"{name}\""))
                    .body_contains("\"version\":4294967297");
                then.status(200).json_body(json!({
                    "errors": true,
                    "items": [
                        { "index": { "_id": a.0.to_string(), "status": 409,
                                     "error": { "type": "version_conflict_engine_exception" } } },
                        { "delete": { "_id": b.0.to_string(), "status": 404, "result": "not_found" } }
                    ]
                }));
            })
            .await;
        let v = (1u64 << 32) | 1;
        let version = Some(v);
        index
            .bulk(&[
                DocOp::Index {
                    contract: contract(),
                    id: a,
                    version,
                    doc: doc(a, v),
                },
                DocOp::Delete {
                    contract: contract(),
                    id: b,
                    version,
                },
            ])
            .await
            .expect("a conflict and an absent delete are success");
        mock.assert_async().await;
        let body = index
            .bulk_body(&[DocOp::Delete {
                contract: contract(),
                id: b,
                version: None,
            }])
            .unwrap();
        assert_eq!(body.lines().count(), 1, "a delete has no source line");
        assert!(
            !body.contains("version"),
            "an unleased write is unversioned"
        );
    }

    #[tokio::test]
    async fn a_rejected_bulk_item_fails_the_write() {
        let server = MockServer::start_async().await;
        let index = ElasticRecall::new(&cfg(&server.base_url())).unwrap();
        let a = NodeId::new();
        server
            .mock_async(|when, then| {
                when.method(POST).path("/_bulk");
                then.status(200).json_body(json!({
                    "errors": true,
                    "items": [ { "index": { "_id": a.0.to_string(), "status": 400,
                                            "error": { "type": "mapper_parsing_exception" } } } ]
                }));
            })
            .await;
        let err = index
            .bulk(&[DocOp::Index {
                contract: contract(),
                id: a,
                version: Some(5),
                doc: doc(a, 5),
            }])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("mapper_parsing_exception"),
            "{err}"
        );
        assert!(err.to_string().contains(&a.0.to_string()), "{err}");
    }

    #[tokio::test]
    async fn knn_filters_by_session_and_maps_scores_to_cosine() {
        let server = MockServer::start_async().await;
        let index = ElasticRecall::new(&cfg(&server.base_url())).unwrap();
        let name = index.index_name(&contract());
        let hit = NodeId::new();
        let mock = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path(format!("/{name}/_search"))
                    .json_body_partial(
                        json!({
                            "knn": {
                                "field": "embedding",
                                "k": 3,
                                "num_candidates": 100,
                                "filter": { "term": { "session_id": "s" } }
                            },
                            "size": 3,
                            "_source": ["canonical_key", "embedding"]
                        })
                        .to_string(),
                    );
                then.status(200).json_body(json!({
                    "hits": { "hits": [
                        { "_id": hit.0.to_string(), "_score": 0.75,
                          "_source": { "canonical_key": "key", "embedding": [0.5, 0.25] } }
                    ] }
                }));
            })
            .await;
        let hits = index
            .knn(&contract(), &SessionId::new("s"), &[1.0, 0.0], 3)
            .await
            .unwrap();
        mock.assert_async().await;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].id, hit);
        assert!((hits[0].cosine - 0.5).abs() < 1e-12, "(1 + cos) / 2 = 0.75");
        assert_eq!(hits[0].canonical_key, "key");
        assert_eq!(
            hits[0].embedding,
            Some(vec![0.5, 0.25]),
            "the stored vector"
        );
    }

    #[tokio::test]
    async fn knn_on_a_missing_index_is_empty_and_other_errors_are_errors() {
        let server = MockServer::start_async().await;
        let index = ElasticRecall::new(&cfg(&server.base_url())).unwrap();
        let name = index.index_name(&contract());
        let missing = server
            .mock_async(|when, then| {
                when.method(POST).path(format!("/{name}/_search"));
                then.status(404)
                    .json_body(json!({ "error": { "type": "index_not_found_exception" } }));
            })
            .await;
        let hits = index
            .knn(&contract(), &SessionId::new("s"), &[1.0, 0.0], 3)
            .await
            .unwrap();
        assert!(hits.is_empty());
        missing.delete_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path(format!("/{name}/_search"));
                then.status(503)
                    .json_body(json!({ "error": { "type": "cluster_block_exception" } }));
            })
            .await;
        let err = index
            .knn(&contract(), &SessionId::new("s"), &[1.0, 0.0], 3)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("503"), "{err}");
    }

    #[tokio::test]
    async fn markers_are_versioned_and_keyed_by_the_hashed_session_id() {
        let server = MockServer::start_async().await;
        let path = format!("/lambo-meta/_doc/{}", marker_id(&SessionId::new("a/b c")));
        let index = ElasticRecall::new(&cfg(&server.base_url())).unwrap();
        let create = server
            .mock_async(|when, then| {
                when.method(PUT).path("/lambo-meta");
                then.status(400)
                    .json_body(json!({ "error": { "type": "resource_already_exists_exception" } }));
            })
            .await;
        let put = server
            .mock_async(|when, then| {
                when.method(PUT)
                    .path(&path)
                    .query_param("version", "9")
                    .query_param("version_type", "external")
                    .json_body(json!({ "synced_epoch": 3, "session_id": "a/b c" }));
                then.status(409)
                    .json_body(json!({ "error": { "type": "version_conflict_engine_exception" } }));
            })
            .await;
        let sid = SessionId::new("a/b c");
        index
            .write_marker(&sid, SyncMarker { synced_epoch: 3 }, Some(9))
            .await
            .expect("a newer marker already there is success");
        index
            .write_marker(&sid, SyncMarker { synced_epoch: 3 }, Some(9))
            .await
            .unwrap();
        put.assert_hits_async(2).await;
        create.assert_hits_async(1).await;

        let get = server
            .mock_async(|when, then| {
                when.method(GET).path(&path);
                then.status(200)
                    .json_body(json!({ "found": true, "_source": { "synced_epoch": 3 } }));
            })
            .await;
        assert_eq!(
            index.read_marker(&sid).await.unwrap(),
            Some(SyncMarker { synced_epoch: 3 })
        );
        get.assert_async().await;
    }

    /// L1: the marker's `_id` is the SHA-256 of the session id, so a session
    /// id the URL layer would rewrite (`.` and `..` are dropped as path
    /// segments) or one past the engine's 512-byte `_id` limit still has a
    /// marker of its own. The session id rides in the body.
    #[tokio::test]
    async fn the_marker_id_is_the_hashed_session_id() {
        use sha2::{Digest, Sha256};
        let server = MockServer::start_async().await;
        let index = ElasticRecall::new(&cfg(&server.base_url())).unwrap();
        server
            .mock_async(|when, then| {
                when.method(PUT).path("/lambo-meta");
                then.status(200).json_body(json!({ "acknowledged": true }));
            })
            .await;
        let long = "x".repeat(600);
        for raw in ["..", ".", long.as_str()] {
            let hex: String = Sha256::digest(raw.as_bytes())
                .iter()
                .map(|b| format!("{b:02x}"))
                .collect();
            let put = server
                .mock_async(|when, then| {
                    when.method(PUT)
                        .path(format!("/lambo-meta/_doc/{hex}"))
                        .json_body(json!({ "synced_epoch": 4, "session_id": raw }));
                    then.status(201).json_body(json!({ "result": "created" }));
                })
                .await;
            let get = server
                .mock_async(|when, then| {
                    when.method(GET).path(format!("/lambo-meta/_doc/{hex}"));
                    then.status(200).json_body(json!({
                        "found": true,
                        "_source": { "synced_epoch": 4, "session_id": raw }
                    }));
                })
                .await;
            let sid = SessionId::new(raw);
            index
                .write_marker(&sid, SyncMarker { synced_epoch: 4 }, Some(1))
                .await
                .unwrap();
            assert_eq!(
                index.read_marker(&sid).await.unwrap(),
                Some(SyncMarker { synced_epoch: 4 })
            );
            put.assert_async().await;
            get.assert_async().await;
        }
    }

    #[tokio::test]
    async fn session_deletes_go_by_query_and_report_failures() {
        let server = MockServer::start_async().await;
        let index = ElasticRecall::new(&cfg(&server.base_url())).unwrap();
        let ok = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/lambo-v-*/_delete_by_query")
                    .query_param("conflicts", "proceed")
                    .json_body_partial(
                        json!({ "query": { "bool": { "filter": [
                            { "term": { "session_id": "s" } },
                            { "range": { "v": { "lt": 12 } } }
                        ] } } })
                        .to_string(),
                    );
                then.status(200)
                    .json_body(json!({ "deleted": 2, "version_conflicts": 1, "failures": [] }));
            })
            .await;
        let report = index
            .delete_session_docs(&SessionId::new("s"), Some(12))
            .await
            .unwrap();
        assert_eq!(
            report,
            DeleteReport {
                deleted: 2,
                version_conflicts: 1
            },
            "conflicts are reported, not swallowed"
        );
        ok.assert_async().await;
        ok.delete_async().await;
        server
            .mock_async(|when, then| {
                when.method(POST).path("/lambo-v-*/_delete_by_query");
                then.status(200)
                    .json_body(json!({ "deleted": 0, "failures": [ { "cause": {} } ] }));
            })
            .await;
        let err = index
            .delete_session_docs(&SessionId::new("s"), None)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("failed to delete"), "{err}");
    }

    /// H1: the refresh a sweep makes first, and the count an erase confirms
    /// with, both cover every data index and tolerate there being none.
    #[tokio::test]
    async fn refresh_and_count_cover_every_data_index() {
        let server = MockServer::start_async().await;
        let index = ElasticRecall::new(&cfg(&server.base_url())).unwrap();
        let refresh = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/lambo-v-*/_refresh")
                    .query_param("allow_no_indices", "true")
                    .query_param("ignore_unavailable", "true");
                then.status(200).json_body(json!({ "_shards": {} }));
            })
            .await;
        index.refresh().await.unwrap();
        refresh.assert_async().await;

        let count = server
            .mock_async(|when, then| {
                when.method(POST)
                    .path("/lambo-v-*/_count")
                    .query_param("allow_no_indices", "true")
                    .json_body(json!({ "query": { "term": { "session_id": "s" } } }));
                then.status(200).json_body(json!({ "count": 3 }));
            })
            .await;
        assert_eq!(
            index
                .count_session_docs(&SessionId::new("s"))
                .await
                .unwrap(),
            3
        );
        count.assert_async().await;
    }

    /// M1: a delete-by-query (and the refresh and count around it) runs on
    /// the maintenance budget, not the per-request timeout that bounds
    /// single-document writes and kNN.
    #[tokio::test]
    async fn maintenance_requests_outlive_the_per_request_timeout() {
        let server = MockServer::start_async().await;
        let mut c = cfg(&server.base_url());
        c.timeout_ms = Some(100);
        let index = ElasticRecall::new(&c).unwrap();
        server
            .mock_async(|when, then| {
                when.method(POST).path("/lambo-v-*/_delete_by_query");
                then.status(200)
                    .delay(Duration::from_millis(400))
                    .json_body(json!({ "deleted": 1, "version_conflicts": 0, "failures": [] }));
            })
            .await;
        server
            .mock_async(|when, then| {
                when.method(GET).path(format!(
                    "/lambo-meta/_doc/{}",
                    marker_id(&SessionId::new("s"))
                ));
                then.status(404)
                    .delay(Duration::from_millis(400))
                    .json_body(json!({ "found": false }));
            })
            .await;
        let sid = SessionId::new("s");
        let report = index.delete_session_docs(&sid, None).await.unwrap();
        assert_eq!(report.deleted, 1);
        assert!(
            index.read_marker(&sid).await.is_err(),
            "an ordinary request still times out"
        );
    }

    #[tokio::test]
    async fn an_index_is_created_once_with_its_contract_mapping() {
        let server = MockServer::start_async().await;
        let index = ElasticRecall::new(&cfg(&server.base_url())).unwrap();
        let name = index.index_name(&contract());
        let create = server
            .mock_async(|when, then| {
                when.method(PUT).path(format!("/{name}")).json_body_partial(
                    json!({ "mappings": { "properties": {
                        "embedding": { "type": "dense_vector", "dims": 2,
                                       "index": true, "similarity": "cosine",
                                       "index_options": { "type": "hnsw" } },
                        "session_id": { "type": "keyword" }
                    } } })
                    .to_string(),
                );
                then.status(200).json_body(json!({ "acknowledged": true }));
            })
            .await;
        index.ensure_index(&contract()).await.unwrap();
        index.ensure_index(&contract()).await.unwrap();
        create.assert_hits_async(1).await;
    }

    #[tokio::test]
    async fn an_unreachable_cluster_is_an_error_not_a_hang() {
        // A port nothing listens on: connection refused, well inside the timeout.
        let index = ElasticRecall::new(&cfg("http://127.0.0.1:1")).unwrap();
        let err = index.read_marker(&SessionId::new("s")).await.unwrap_err();
        assert!(err.to_string().contains("recall index"), "{err}");
    }
}
