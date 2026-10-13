//! Manual checks against a disposable, loopback Elasticsearch cluster.
//! Ordinary runs report ignored; explicitly requested runs require the URL.
//! Each test owns a UUID index prefix and a temporary SQLite primary.

use super::*;
use crate::embed::{EmbedderConfig, EmbedderKind, FixtureEmbedder, FAR, NEAR_A, NEAR_B};
use crate::store::tiered::elastic::ElasticRecall;
use crate::store::{RecallConfig, RecallKind, RecallRefresh, SqliteStore};
use serde_json::Value;

const DIM: usize = 1024;

fn live_contract() -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: DIM,
    }
}

struct LiveRig {
    cfg: RecallConfig,
    dir: std::path::PathBuf,
    client: reqwest::Client,
}

impl LiveRig {
    fn new() -> Self {
        let url = std::env::var("LAMBO_ELASTIC_URL").expect(
            "live test explicitly requested: set LAMBO_ELASTIC_URL to the disposable localhost Elasticsearch cluster",
        );
        let parsed = reqwest::Url::parse(&url).expect("valid LAMBO_ELASTIC_URL");
        assert!(
            matches!(
                parsed.host_str(),
                Some("127.0.0.1" | "localhost" | "[::1]" | "::1")
            ),
            "these destructive synthetic tests require a loopback cluster"
        );
        assert_eq!(parsed.scheme(), "http");
        assert!(parsed.username().is_empty());
        assert!(parsed.password().is_none());
        assert!(parsed.query().is_none());
        assert!(parsed.fragment().is_none());
        let dir =
            std::env::temp_dir().join(format!("lambo-elastic-live-{}", NodeId::new().0.simple()));
        std::fs::create_dir(&dir).expect("create live SQLite temp directory");
        Self {
            cfg: RecallConfig {
                kind: RecallKind::Elastic,
                url: url.trim_end_matches('/').to_string(),
                api_key: None,
                index_prefix: format!("lambo-live-{}", NodeId::new().0.simple()),
                refresh: RecallRefresh::True,
                timeout_ms: Some(5_000),
            },
            dir,
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(10))
                .build()
                .unwrap(),
        }
    }

    fn primary(&self) -> SqliteStore {
        SqliteStore::connect(self.dir.join("primary.db").to_str().unwrap())
            .unwrap()
            .with_vector_dim(DIM)
            .unwrap()
    }

    fn index(&self) -> ElasticRecall {
        ElasticRecall::new(&self.cfg).unwrap()
    }

    fn store(&self) -> TieredStore {
        self.store_with(&self.cfg)
    }

    fn store_with(&self, cfg: &RecallConfig) -> TieredStore {
        TieredStore::new(
            Box::new(self.primary()),
            Box::new(ElasticRecall::new(cfg).unwrap()),
            Some(DIM),
        )
    }

    async fn get(&self, path: &str) -> Value {
        self.client
            .get(format!("{}/{}", self.cfg.url, path))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn cleanup(&self) -> Result<(), String> {
        let mut failure = None;
        // Delete explicit owned names, never a cluster-wide wildcard.
        for name in [
            self.index().index_name(&live_contract()),
            format!("{}-meta", self.cfg.index_prefix),
        ] {
            match self
                .client
                .delete(format!("{}/{}", self.cfg.url, name))
                .send()
                .await
            {
                Ok(response)
                    if response.status().is_success()
                        || response.status() == reqwest::StatusCode::NOT_FOUND => {}
                Ok(response) => {
                    failure.get_or_insert_with(|| {
                        format!("cleanup of owned index {name}: {}", response.status())
                    });
                }
                Err(error) => {
                    failure
                        .get_or_insert_with(|| format!("cleanup of owned index {name}: {error}"));
                }
            }
        }
        if let Err(error) = std::fs::remove_dir_all(&self.dir) {
            failure.get_or_insert_with(|| format!("remove live SQLite temp directory: {error}"));
        }
        failure.map_or(Ok(()), Err)
    }
}

/// Keep the rig alive outside the assertion task so cleanup also runs on panic.
async fn with_live<F, Fut>(check: F)
where
    F: FnOnce(Arc<LiveRig>) -> Fut,
    Fut: std::future::Future<Output = ()> + Send + 'static,
{
    let rig = Arc::new(LiveRig::new());
    let result = tokio::spawn(check(Arc::clone(&rig))).await;
    let cleanup = rig.cleanup().await;
    if let Err(error) = result {
        if let Err(cleanup_error) = cleanup {
            eprintln!("{cleanup_error}");
        }
        std::panic::resume_unwind(error.into_panic());
    }
    cleanup.unwrap_or_else(|error| panic!("{error}"));
}

struct Fixture {
    origin: NodeId,
    vector_ids: Vec<NodeId>,
    vectorless: NodeId,
}

fn fixture_batch(sid: &SessionId, epoch: u64) -> (Fixture, MutationBatch) {
    let embedder = FixtureEmbedder::new();
    let origin = NodeId::new();
    let vectorless = NodeId::new();
    let mut mutations = vec![
        set_contract(sid, Some(live_contract())),
        interaction(sid, origin),
    ];
    let mut vector_ids = Vec::new();
    for text in [NEAR_A, NEAR_B, FAR] {
        let id = NodeId::new();
        vector_ids.push(id);
        mutations.push(upsert(concept(
            sid,
            id,
            origin,
            text,
            Some(embedder.embed_sync(text)),
        )));
        let now = Utc::now();
        mutations.push(Mutation::UpsertEdge {
            edge: crate::types::Edge {
                event_time: None,
                id: NodeId::new(),
                session_id: sid.clone(),
                source: origin,
                target: id,
                edge_type: crate::types::EdgeType::Derives,
                weight: 1.0,
                reinforcements: 1,
                created_at: now,
                last_reinforced: now,
            },
        });
    }
    mutations.push(upsert(concept(
        sid,
        vectorless,
        origin,
        "synthetic vectorless fact",
        None,
    )));
    let now = Utc::now();
    mutations.push(Mutation::UpsertEdge {
        edge: crate::types::Edge {
            event_time: None,
            id: NodeId::new(),
            session_id: sid.clone(),
            source: origin,
            target: vectorless,
            edge_type: crate::types::EdgeType::Derives,
            weight: 1.0,
            reinforcements: 1,
            created_at: now,
            last_reinforced: now,
        },
    });
    (
        Fixture {
            origin,
            vector_ids,
            vectorless,
        },
        batch(epoch, mutations),
    )
}

fn backends(store: Box<dyn GraphStore>) -> crate::resolve::ResolvedBackends {
    crate::resolve::ResolvedBackends {
        store,
        embedder: Box::new(FixtureEmbedder::new()),
        store_cfg: Default::default(),
        embedder_cfg: EmbedderConfig {
            kind: EmbedderKind::Fixture,
            dim: DIM,
            ..Default::default()
        },
        embedding: live_contract(),
        allow_embedding_mismatch: false,
        config: crate::Config::default(),
    }
}

#[tokio::test]
#[ignore = "live: requires LAMBO_ELASTIC_URL"]
async fn elastic_live_mapping_and_write_through_projection() {
    with_live(|rig| async move {
        let store = rig.store();
        store.init_schema().await.unwrap();
        let sid = SessionId::new("synthetic-projection");
        let writer = holder("live-writer");
        let token = attach(&store, &sid, &writer).await;
        let (fixture, mutations) = fixture_batch(&sid, 1);
        store.flush(&mutations, Some(token)).await.unwrap();
        settle(&store).await;
        assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
        let index = rig.index();
        let name = index.index_name(&live_contract());
        let mapping = rig.get(&format!("{name}/_mapping")).await;
        let properties = &mapping[&name]["mappings"]["properties"];
        assert_eq!(mapping[&name]["mappings"]["dynamic"], "strict");
        assert_eq!(properties["embedding"]["type"], "dense_vector");
        assert_eq!(properties["embedding"]["dims"], DIM);
        assert_eq!(properties["embedding"]["similarity"], "cosine");
        assert_eq!(properties["embedding"]["index_options"]["type"], "hnsw");
        assert_eq!(properties["session_id"]["type"], "keyword");
        assert_eq!(index.count_session_docs(&sid).await.unwrap(), 3);
        let doc = rig
            .get(&format!("{name}/_doc/{}", fixture.vector_ids[0].0))
            .await;
        let source = &doc["_source"];
        assert_eq!(source["session_id"], sid.0);
        assert_eq!(source["node_id"], fixture.vector_ids[0].0.to_string());
        assert_eq!(source["content"], NEAR_A);
        assert_eq!(source["canonical_key"], NEAR_A);
        assert_eq!(source["concept_type"], "Entity");
        assert_eq!(source["embedding"].as_array().unwrap().len(), DIM);
        assert_eq!(source["v"], mirror_version(token, 1).unwrap());
        assert_eq!(doc["_version"], source["v"]);
        let vectorless = rig
            .client
            .get(format!(
                "{}/{name}/_doc/{}",
                rig.cfg.url, fixture.vectorless.0
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(vectorless.status(), reqwest::StatusCode::NOT_FOUND);
        assert_eq!(
            index.read_marker(&sid).await.unwrap().unwrap().synced_epoch,
            1
        );
        store.release_lease(&sid, &writer).await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "live: requires LAMBO_ELASTIC_URL"]
async fn elastic_live_recall_matches_sqlite_fixture() {
    with_live(|rig| async move {
        use std::sync::atomic::Ordering;
        let sid = SessionId::new("synthetic-parity");
        let writer = holder("live-writer");
        let exact_fallback = Arc::new(ExactVectors::default());
        let primary: Arc<dyn GraphStore> = Arc::new(rig.primary());
        let shared = Shared::with_exact_vectors(Arc::clone(&primary), Arc::clone(&exact_fallback));
        let store = TieredStore::new(Box::new(shared), Box::new(rig.index()), Some(DIM));
        store.init_schema().await.unwrap();
        let token = attach(&store, &sid, &writer).await;
        let (_, mutations) = fixture_batch(&sid, 1);
        store.flush(&mutations, Some(token)).await.unwrap();
        settle(&store).await;
        assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
        let probe = FixtureEmbedder::new().embed_sync(NEAR_A);
        let exact = primary
            .vector_candidates_checked(&sid, &probe, &live_contract(), 3)
            .await
            .unwrap();
        let indexed = rig
            .index()
            .knn(&live_contract(), &sid, &probe, 3)
            .await
            .unwrap();
        assert_eq!(indexed.len(), 3, "real index must contain the fixture");
        // Any primary fallback now fails and increments a counter.
        exact_fallback.fail.store(true, Ordering::SeqCst);
        let tiered = store
            .vector_candidates_checked(&sid, &probe, &live_contract(), 3)
            .await
            .unwrap();
        assert_eq!(ids(&tiered), ids(&exact));
        for (a, b) in tiered.iter().zip(&exact) {
            assert!((a.score - b.score).abs() < 1e-6);
        }
        assert_eq!(exact_fallback.calls.load(Ordering::SeqCst), 0);
        store.release_lease(&sid, &writer).await.unwrap();
        let tiered = backends(Box::new(store));
        let sqlite = backends(Box::new(rig.primary()));
        let actual =
            crate::cli::recall::run_detailed(&tiered, &sid.0, NEAR_A, Some(3), None, Some(0))
                .await
                .unwrap();
        let expected =
            crate::cli::recall::run_detailed(&sqlite, &sid.0, NEAR_A, Some(3), None, Some(0))
                .await
                .unwrap();
        assert!(!actual.hits.is_empty());
        assert_eq!(
            actual
                .hits
                .iter()
                .map(|h| (h.node_id, &h.content))
                .collect::<Vec<_>>(),
            expected
                .hits
                .iter()
                .map(|h| (h.node_id, &h.content))
                .collect::<Vec<_>>()
        );
        for (a, b) in actual.hits.iter().zip(&expected.hits) {
            assert!((a.score - b.score).abs() < 1e-6);
        }
        assert_eq!(actual.context, expected.context);
        assert_eq!(exact_fallback.calls.load(Ordering::SeqCst), 0);
    })
    .await;
}

#[tokio::test]
#[ignore = "live: requires LAMBO_ELASTIC_URL"]
async fn elastic_live_delete_and_erase_preserve_other_session() {
    with_live(|rig| async move {
        let store = rig.store();
        store.init_schema().await.unwrap();
        let target = SessionId::new("synthetic-target");
        let other = SessionId::new("synthetic-other");
        let writer = holder("live-writer");
        let target_token = attach(&store, &target, &writer).await;
        let other_token = attach(&store, &other, &writer).await;
        let (fixture, mutations) = fixture_batch(&target, 1);
        store.flush(&mutations, Some(target_token)).await.unwrap();
        let (_, mutations) = fixture_batch(&other, 1);
        store.flush(&mutations, Some(other_token)).await.unwrap();
        settle(&store).await;
        let index = rig.index();
        assert_eq!(index.count_session_docs(&target).await.unwrap(), 3);
        assert_eq!(index.count_session_docs(&other).await.unwrap(), 3);
        store
            .flush(
                &batch(
                    2,
                    vec![Mutation::DeleteNode {
                        id: fixture.vector_ids[0],
                    }],
                ),
                Some(target_token),
            )
            .await
            .unwrap();
        index.refresh().await.unwrap();
        assert_eq!(index.count_session_docs(&target).await.unwrap(), 2);
        let name = index.index_name(&live_contract());
        let deleted = rig
            .client
            .get(format!(
                "{}/{name}/_doc/{}",
                rig.cfg.url, fixture.vector_ids[0].0
            ))
            .send()
            .await
            .unwrap();
        assert_eq!(deleted.status(), reqwest::StatusCode::NOT_FOUND);
        store.release_lease(&target, &writer).await.unwrap();
        let erased = store
            .erase_session(&target, &holder("live-eraser"))
            .await
            .unwrap();
        assert!(matches!(erased, EraseOutcome::Erased(_)));
        assert_eq!(index.count_session_docs(&target).await.unwrap(), 0);
        assert!(index.read_marker(&target).await.unwrap().is_none());
        assert_eq!(index.count_session_docs(&other).await.unwrap(), 3);
        assert!(matches!(
            rig.primary().load_session(&target).await,
            Err(StoreError::SessionNotFound(_))
        ));
        let again = store
            .erase_session(&target, &holder("live-eraser"))
            .await
            .unwrap();
        assert!(matches!(again, EraseOutcome::Erased(_)));
        store.release_lease(&other, &writer).await.unwrap();
    })
    .await;
}

#[tokio::test]
#[ignore = "live: requires LAMBO_ELASTIC_URL"]
async fn elastic_live_outage_fallback_then_restart_and_resume() {
    with_live(|rig| async move {
        let sid = SessionId::new("synthetic-restart");
        let writer = holder("live-writer");
        let store = rig.store();
        store.init_schema().await.unwrap();
        let token = attach(&store, &sid, &writer).await;
        let (fixture, mutations) = fixture_batch(&sid, 1);
        store.flush(&mutations, Some(token)).await.unwrap();
        settle(&store).await;
        assert_eq!(store.tier_status(&sid).sync, TierSync::InSync);
        store.release_lease(&sid, &writer).await.unwrap();
        drop(store);
        // Keep an owned loopback port bound without serving HTTP, forcing a
        // real client timeout without stopping the shared cluster.
        let stalled = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut down_cfg = rig.cfg.clone();
        down_cfg.url = format!("http://{}", stalled.local_addr().unwrap());
        down_cfg.timeout_ms = Some(200);
        let down = rig.store_with(&down_cfg);
        let token = attach(&down, &sid, &writer).await;
        settle(&down).await;
        let changed = concept(
            &sid,
            fixture.vector_ids[0],
            fixture.origin,
            "synthetic resumed fact",
            Some(FixtureEmbedder::new().embed_sync(NEAR_A)),
        );
        down.flush(&batch(2, vec![upsert(changed)]), Some(token))
            .await
            .unwrap();
        settle(&down).await;
        assert_eq!(down.tier_status(&sid).sync, TierSync::Stale);
        assert!(down.tier_status(&sid).mirror_failures > 0);
        let durable = rig.primary().load_session(&sid).await.unwrap();
        assert_eq!(durable.mutation_epoch, 2);
        assert!(durable
            .concepts
            .iter()
            .any(|c| c.content == "synthetic resumed fact"));
        let probe = FixtureEmbedder::new().embed_sync(NEAR_A);
        let fallback = down
            .vector_candidates_checked(&sid, &probe, &live_contract(), 3)
            .await
            .unwrap();
        let exact = rig
            .primary()
            .vector_candidates_checked(&sid, &probe, &live_contract(), 3)
            .await
            .unwrap();
        assert_eq!(ids(&fallback), ids(&exact));
        for (a, b) in fallback.iter().zip(&exact) {
            assert!((a.score - b.score).abs() < 1e-6);
        }
        down.release_lease(&sid, &writer).await.unwrap();
        drop(down);
        drop(stalled);
        // New client and primary handles resume only persisted state.
        let resumed = rig.store();
        let token = attach(&resumed, &sid, &writer).await;
        settle(&resumed).await;
        assert_eq!(resumed.tier_status(&sid).sync, TierSync::InSync);
        let index = rig.index();
        assert_eq!(
            index.read_marker(&sid).await.unwrap().unwrap().synced_epoch,
            2
        );
        let name = index.index_name(&live_contract());
        let repaired = rig
            .get(&format!("{name}/_doc/{}", fixture.vector_ids[0].0))
            .await;
        assert_eq!(repaired["_source"]["content"], "synthetic resumed fact");
        let fresh_id = NodeId::new();
        resumed
            .flush(
                &batch(
                    3,
                    vec![upsert(concept(
                        &sid,
                        fresh_id,
                        fixture.origin,
                        "synthetic write after restart",
                        Some(FixtureEmbedder::new().embed_sync(FAR)),
                    ))],
                ),
                Some(token),
            )
            .await
            .unwrap();
        settle(&resumed).await;
        assert_eq!(index.count_session_docs(&sid).await.unwrap(), 4);
        assert_eq!(
            index.read_marker(&sid).await.unwrap().unwrap().synced_epoch,
            3
        );
        resumed.release_lease(&sid, &writer).await.unwrap();
    })
    .await;
}
