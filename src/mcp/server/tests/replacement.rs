//! #198 part c on the MCP wire: a hybrid derive's settled receipt carries
//! `replacement_candidates` (on `lambo_stats`'s receipt and on the
//! piggybacked `write_receipts`) only when the write listed one, with the
//! id recall shows, the full node id, an excerpt, the cosine and the new
//! concept's id. A receipt that lists none has no such key.

use super::*;
use crate::recall::format::short_id;
use crate::types::{AgentId, MatchStrategy};

const OLD: &str = "the embedder listens on port 8080";
const REWORDED: &str = "after the clash the embedder moved to port 9090 behind the proxy";

/// Every text embeds to the same unit vector, so every existing concept is
/// a vector hit at cosine 1.0: the predicate decides.
struct Constant;

#[async_trait::async_trait]
impl Embedder for Constant {
    fn dimensions(&self) -> usize {
        1024
    }
    async fn embed(&self, _text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        let mut v = vec![0.0; 1024];
        v[0] = 1.0;
        Ok(v)
    }
}

async fn hybrid_server(session: &str) -> LamboServer {
    let mem = Memory::builder()
        .session(session)
        .agent("agent-a")
        .config(Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            ..Config::default()
        })
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        )) as Arc<dyn GraphStore>)
        .embedder(Arc::new(Constant) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        })
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    LamboServer::new(Arc::new(mem))
}

async fn ack(s: &LamboServer, content: &str) -> String {
    let out = call_raw(
        s,
        "lambo_derive",
        json!({"agent_id": "agent-a", "concepts": [{"content": content, "concept_type": "logic"}]}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    out.structured_content.unwrap()["receipt"]
        .as_str()
        .unwrap()
        .to_string()
}

async fn settled(s: &LamboServer, receipt: &str) -> serde_json::Value {
    let out = call_raw(
        s,
        "lambo_stats",
        json!({"agent_id": "agent-a", "receipt": receipt, "wait_ms": 30_000}),
    )
    .await;
    let r = out.structured_content.unwrap()["receipt"].clone();
    assert_eq!(r["state"], "applied", "{r}");
    r
}

#[tokio::test]
async fn the_settled_receipt_lists_replacement_candidates() {
    let s = hybrid_server("mcp-replacement").await;
    let first = settled(&s, &ack(&s, OLD).await).await;
    assert!(first.get("replacement_candidates").is_none(), "{first}");
    assert!(
        !first["detail"].as_str().unwrap().contains("replacement"),
        "{first}"
    );
    let old = first["created"][0].as_str().unwrap().to_string();

    let r = settled(&s, &ack(&s, REWORDED).await).await;
    let new = r["created"][0].as_str().unwrap().to_string();
    let short = |id: &str| short_id(crate::types::NodeId(id.parse().unwrap()));
    assert_eq!(
        r["replacement_candidates"],
        json!({
            "candidates": [{
                "id": short(&old),
                "node_id": old,
                "excerpt": OLD,
                "cosine": 1.0,
                "new_id": short(&new),
            }],
            "total": 1,
        }),
        "{r}"
    );
    let detail = r["detail"].as_str().unwrap();
    assert!(
        detail.contains(&format!(
            "; 1 replacement candidate(s): existing concepts this write may replace (nothing \
             recorded; cite one in against on a later call only if it really is replaced)\n  - \
             {} \"{OLD}\" (cosine 1.0000, for {})",
            short(&old),
            short(&new)
        )),
        "{detail}"
    );
    s.mem.close().await.unwrap();
}

/// The piggybacked receipt on the next tool response carries the same
/// field, and a receipt with none adds no `write_receipts` entry.
#[tokio::test]
async fn the_piggybacked_receipt_carries_them_too() {
    let s = hybrid_server("mcp-replacement-piggy").await;
    let agent = AgentId::new("agent-a");
    let first: crate::writeq::ReceiptId = ack(&s, OLD).await.parse().unwrap();
    s.mem
        .pipeline()
        .wait_for(&agent, None, first, Duration::from_secs(30))
        .await;
    let quiet = call_raw(&s, "lambo_saints", json!({"agent_id": "agent-a"})).await;
    let payload = quiet.structured_content.clone().unwrap();
    assert!(payload.get("write_receipts").is_none(), "{payload}");
    assert!(text_of(&quiet).contains("write receipts"));
    assert!(!text_of(&quiet).contains("replacement candidate"));

    let second: crate::writeq::ReceiptId = ack(&s, REWORDED).await.parse().unwrap();
    s.mem
        .pipeline()
        .wait_for(&agent, None, second, Duration::from_secs(30))
        .await;
    let out = call_raw(&s, "lambo_saints", json!({"agent_id": "agent-a"})).await;
    let payload = out.structured_content.as_ref().unwrap();
    let entry = &payload["write_receipts"][0];
    assert_eq!(entry["id"], second.to_string());
    assert_eq!(entry["state"], "applied");
    assert!(entry.get("stale_report").is_none(), "{entry}");
    assert_eq!(entry["replacement_candidates"]["total"], 1, "{entry}");
    assert_eq!(
        entry["replacement_candidates"]["candidates"][0]["excerpt"],
        OLD
    );
    assert!(text_of(&out).contains("1 replacement candidate(s)"));
    s.mem.close().await.unwrap();
}

/// M1 on the wire: a write backfilled with an `event_time` before an
/// existing concept lists it with `"newer": true`, and the receipt text
/// says it may replace this write and is not one to cite.
#[tokio::test]
async fn a_backfilled_write_marks_a_later_candidate_newer() {
    let s = hybrid_server("mcp-replacement-newer").await;
    let first = settled(&s, &ack(&s, OLD).await).await;
    let old = first["created"][0].as_str().unwrap().to_string();
    let out = call_raw(
        &s,
        "lambo_derive",
        json!({
            "agent_id": "agent-a",
            "event_time": "2020-01-01T00:00:00Z",
            "concepts": [{"content": REWORDED, "concept_type": "logic"}],
        }),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    let receipt = out.structured_content.unwrap()["receipt"]
        .as_str()
        .unwrap()
        .to_string();
    let r = settled(&s, &receipt).await;
    let row = &r["replacement_candidates"]["candidates"][0];
    assert_eq!(row["node_id"], old, "{r}");
    assert_eq!(row["newer"], true, "{r}");
    let detail = r["detail"].as_str().unwrap();
    assert!(
        detail.contains(
            "; newer: dated after this write, so it may replace this write, not the other way \
             round; do not cite it in against)"
        ),
        "{detail}"
    );
    s.mem.close().await.unwrap();
}
