//! #198 on the MCP wire: `lambo_recall`'s structured hits carry
//! `possibly_superseded_by` on the older hit of a suspected pair whenever
//! both are shown, whatever the line gate says; the gate decides only the
//! prose line. A recall with no pair has no such key.

use super::*;
use crate::recall::suspected::SuspectedLine;
use crate::types::MatchStrategy;

const OLD: &str = "the embedder listens on port 8080";
const NEW: &str = "the embedder listens on port 8081";

/// Every text embeds to the same unit vector, so every pair is a vector
/// hit at cosine 1.0: the guard and the trigger decide.
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

async fn hybrid_server(session: &str, kind: &str, line: SuspectedLine) -> LamboServer {
    let mem = Memory::builder()
        .session(session)
        .agent("agent-a")
        .config(Config {
            daemon_tick_interval: Duration::from_secs(3_600),
            suspected_supersession_line: line,
            ..Config::default()
        })
        .flush_interval(Duration::from_secs(3_600))
        .store(Arc::new(crate::test_util::GraphRanked(
            crate::test_util::VectorSearchable(Arc::new(MemoryStore::new())),
        )) as Arc<dyn GraphStore>)
        .embedder(Arc::new(Constant) as Arc<dyn Embedder>)
        .embedding_contract(EmbeddingContract {
            kind: kind.into(),
            model: None,
            dim: 1024,
        })
        .match_strategy(MatchStrategy::Hybrid)
        .build()
        .await
        .expect("build");
    LamboServer::new(Arc::new(mem))
}

async fn derive_one(s: &LamboServer, content: &str) -> String {
    let created = derive_created(
        s,
        "agent-a",
        json!([{"content": content, "concept_type": "logic"}]),
    )
    .await;
    assert_eq!(created.len(), 1, "{created:?}");
    created[0].clone()
}

async fn recall(s: &LamboServer) -> CallToolResult {
    let out = call(
        s,
        "lambo_recall",
        json!({"agent_id": "agent-a", "query": "embedder listens"}),
    )
    .await;
    assert_eq!(out.is_error, Some(false), "{out:?}");
    out
}

fn hit<'a>(payload: &'a serde_json::Value, id: &str) -> &'a serde_json::Value {
    payload["hits"]
        .as_array()
        .expect("hits")
        .iter()
        .find(|h| h["node_id"] == json!(id))
        .unwrap_or_else(|| panic!("no hit {id}: {payload}"))
}

/// Gate on (a validated contract) and gate off (an unvalidated one, as
/// EmbeddingGemma 2 is): the field is on the older hit either way; the
/// line is in the text only when the gate is on.
#[tokio::test]
async fn recall_carries_the_field_whatever_the_gate() {
    for (kind, shown) in [("bge_m3", true), ("fixture", false)] {
        let s = hybrid_server(
            &format!("mcp-suspected-{kind}"),
            kind,
            SuspectedLine::Validated,
        )
        .await;
        let old = derive_one(&s, OLD).await;
        tokio::time::sleep(Duration::from_millis(5)).await;
        let new = derive_one(&s, NEW).await;
        let out = recall(&s).await;
        let payload = out.structured_content.as_ref().expect("payload");
        assert_eq!(
            hit(payload, &old)["possibly_superseded_by"],
            json!(new),
            "{kind}: {payload}"
        );
        assert!(
            hit(payload, &new).get("possibly_superseded_by").is_none(),
            "{kind}: {payload}"
        );
        let text = text_of(&out);
        assert_eq!(
            text.contains("Possibly superseded"),
            shown,
            "{kind}: {text}"
        );
    }
}

/// No pair: the hit has exactly the keys it had before #198.
#[tokio::test]
async fn recall_without_a_pair_has_no_field() {
    let s = hybrid_server("mcp-suspected-none", "bge_m3", SuspectedLine::On).await;
    derive_one(&s, OLD).await;
    let out = recall(&s).await;
    let payload = out.structured_content.as_ref().expect("payload");
    let hits = payload["hits"].as_array().expect("hits");
    assert!(!hits.is_empty(), "{payload}");
    for h in hits {
        let mut keys: Vec<&str> = h.as_object().unwrap().keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(
            keys,
            vec![
                "blast_radius",
                "concept_type",
                "content",
                "is_canonical",
                "node_id",
                "score"
            ],
            "{payload}"
        );
    }
    assert!(!text_of(&out).contains("Possibly superseded"));
}
