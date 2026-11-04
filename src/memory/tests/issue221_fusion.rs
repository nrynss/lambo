//! #221: the exact 8-concept session + query as a regression test, on
//! recorded live Qwen3-Embedding-4B vectors (no network), at both widths.
//!
//! The issue's session misranks under max-merge and ranks the lesson first
//! under the Qwen contract's RRF default. The vectors were recorded live
//! against Vultr Serverless Inference (`qwen3-embedding-4b`, 2560 dims full
//! width and 1024 Matryoshka truncation) by `examples/repro221.rs` and
//! `examples/record_1024_221.rs` (both since removed); the doc texts below
//! are verbatim from the reopen comment, the query is verbatim, and the
//! framing matches [`crate::graph::hybrid::context_text`] for a
//! single-concept derive (`"{content} — {content}"`). An unknown embed text
//! fails the derive loudly, so a framing change breaks this test instead of
//! silently re-measuring it.

use super::*;
use crate::embed::{EmbedError, Embedder};
use crate::recall::fusion::RecallFusion;
use crate::store::SqliteStore;
use crate::ConceptCitations;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

/// The 8 concept texts, verbatim from the issue, with their types: 1 is a
/// resource, 2 a constraint citing 1, 3 an observation citing 2, 4–8 plain
/// observations.
const CONCEPTS: &[(&str, ConceptType)] = &[
    ("pagination_changed", ConceptType::Resource),
    (
        "When a list endpoint starts paging, follow next_page_token until absent; every record imported exactly once. check: chk-paging-001",
        ConceptType::Constraint,
    ),
    (
        "run r-001: naive migration imported 100 of 2,431 records; fixed version passed",
        ConceptType::Observation,
    ),
    (
        "The consumer repo uses Python 3.12 and httpx.",
        ConceptType::Observation,
    ),
    (
        "Sandbox runs are capped at 120 seconds.",
        ConceptType::Observation,
    ),
    (
        "The demo API v1 returns all items in one response.",
        ConceptType::Observation,
    ),
    (
        "Caddy terminates TLS on the VM.",
        ConceptType::Observation,
    ),
    (
        "The web app front end is built with Chaaya.",
        ConceptType::Observation,
    ),
];

/// The recall query, verbatim from the issue.
const QUERY: &str = "the orders API now returns results one page at a time with a cursor; how should the importer handle it?";

/// The recorded fixture: the framed doc texts, their live vectors, and the
/// bare query with its live query vector.
#[derive(serde::Deserialize)]
struct RecordedSession {
    framed_texts: Vec<String>,
    doc_vectors: Vec<Vec<f32>>,
    query: String,
    query_vector: Vec<f32>,
}

fn recorded_json(dim: usize) -> &'static str {
    match dim {
        2560 => include_str!("../../../evidence/issue-221/repro-8concept-qwen3-4b-2560.json"),
        1024 => include_str!("../../../evidence/issue-221/repro-8concept-qwen3-4b-1024.json"),
        _ => panic!("no recorded Qwen fixture at {dim} dims"),
    }
}

fn recorded(dim: usize) -> RecordedSession {
    serde_json::from_str(recorded_json(dim)).expect("the recorded Qwen vectors parse")
}

/// The recorded vectors behind a fail-closed [`Embedder`]: only the 9
/// recorded texts answer; anything else is a test bug and fails the call.
struct RecordedQwen {
    dim: usize,
    docs: HashMap<String, Vec<f32>>,
    query_text: String,
    query_vector: Vec<f32>,
}

impl RecordedQwen {
    fn new(dim: usize, rec: &RecordedSession) -> Self {
        assert_eq!(
            rec.framed_texts.len(),
            rec.doc_vectors.len(),
            "one vector per framed text"
        );
        assert_eq!(
            rec.framed_texts.len(),
            CONCEPTS.len(),
            "the fixture covers exactly the 8 concepts"
        );
        for v in rec.doc_vectors.iter() {
            assert_eq!(v.len(), dim, "{dim}-wide Qwen vectors");
        }
        assert_eq!(rec.query_vector.len(), dim);
        Self {
            dim,
            docs: rec
                .framed_texts
                .iter()
                .zip(rec.doc_vectors.iter())
                .map(|(t, v)| (t.clone(), v.clone()))
                .collect(),
            query_text: rec.query.clone(),
            query_vector: rec.query_vector.clone(),
        }
    }
}

#[async_trait::async_trait]
impl Embedder for RecordedQwen {
    fn dimensions(&self) -> usize {
        self.dim
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.docs.get(text).cloned().ok_or_else(|| {
            EmbedError::Backend(format!("recorded Qwen has no doc vector for {text:?}"))
        })
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if text == self.query_text {
            Ok(self.query_vector.clone())
        } else {
            Err(EmbedError::Backend(format!(
                "recorded Qwen has no query vector for {text:?}"
            )))
        }
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        None
    }
}

fn qwen_contract(dim: usize) -> EmbeddingContract {
    EmbeddingContract {
        kind: "bge_m3".into(),
        model: Some("qwen3-embedding-4b".into()),
        dim,
    }
}

/// The exact issue session under `fusion` at `dim`: derive the 8 concepts in
/// order (2 cites 1, 3 cites 2), await one daemon cycle so recall runs
/// blended, and recall the issue query at top_k 4. Returns the ranked
/// contents.
async fn recall_issue_session(fusion: RecallFusion, dim: usize) -> Vec<String> {
    let dir = crate::test_util::ScratchDir::new("lambo-221");
    let path = dir.join("issue221.sqlite");
    let path = path.to_str().expect("scratch path").to_string();
    let store = Arc::new(SqliteStore::connect(&path).expect("connect")) as Arc<dyn GraphStore>;
    store.init_schema().await.expect("init_schema");
    let rec = recorded(dim);
    let embedder = Arc::new(RecordedQwen::new(dim, &rec)) as Arc<dyn Embedder>;
    let config = crate::Config {
        recall_fusion: fusion,
        ..crate::Config::default()
    };
    let mem = Memory::builder()
        .session("issue-221-repro")
        .agent("kalari")
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(embedder)
        .embedding_contract(qwen_contract(dim))
        .match_strategy(MatchStrategy::Hybrid)
        .config(config)
        .build()
        .await
        .expect("build");
    let agent = crate::types::AgentId::from("kalari");
    for (n, (content, kind)) in CONCEPTS.iter().enumerate() {
        let cites = match n {
            1 => vec![ConceptCitations {
                content: content.to_string(),
                depends_on: vec![CONCEPTS[0].0.to_string()],
                against: vec![],
            }],
            2 => vec![ConceptCitations {
                content: content.to_string(),
                depends_on: vec![CONCEPTS[1].0.to_string()],
                against: vec![],
            }],
            _ => vec![],
        };
        if cites.is_empty() {
            mem.derive(&[(*content, *kind)], &ParentOf::none()).await
        } else {
            mem.derive_cited_as(&agent, &[(*content, *kind)], &ParentOf::none(), &cites)
                .await
        }
        .unwrap_or_else(|e| panic!("deriving concept {n}: {e}"));
        // Distinct `created_at`s, so the recent leg's three newest
        // interactions are the last three writes every run rather than a tie
        // broken by random interaction ids.
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    let waited = std::time::Instant::now();
    while mem.stats().daemon_cycles == 0 {
        assert!(
            waited.elapsed() < Duration::from_secs(60),
            "the daemon's warm-up cycle never ran"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let result = mem
        .recall(RecallQuery {
            query: QUERY.into(),
            top_k: 4,
            max_tokens: 100_000,
            traversal_depth: mem.config().default_traversal_depth,
        })
        .await
        .expect("recall");
    mem.close().await.expect("close");
    assert_eq!(result.hits.len(), 4, "top_k 4 on an 8-concept session");
    result.hits.into_iter().map(|h| h.content).collect()
}

/// The fixture's geometry is the live one: the lesson is the vector leg's
/// top hit, ahead of the demo-API observation, and every recorded cosine
/// sits in Qwen's narrow band (all below 0.75, the lesson above 0.70 at full
/// width; the 1024 truncation keeps the order with the lesson above 0.65).
fn assert_live_geometry(dim: usize) {
    let rec = recorded(dim);
    assert_eq!(rec.query, QUERY, "the fixture records the issue query");
    let cos = |i: usize| crate::embed::cosine(&rec.query_vector, &rec.doc_vectors[i]) as f64;
    let lesson = cos(1);
    let floor = if dim == 2560 { 0.70 } else { 0.65 };
    assert!(
        (floor..0.80).contains(&lesson),
        "{dim}: lesson cosine {lesson}, want the live value"
    );
    assert!(
        lesson > cos(5),
        "{dim}: lesson outscores the demo-API observation"
    );
    assert!(cos(5) > cos(2), "{dim}: demo-API outscores r-001");
    for (i, (content, _)) in CONCEPTS.iter().enumerate() {
        let framed = format!("{content} — {content}");
        assert_eq!(
            rec.framed_texts[i], framed,
            "{dim}: concept {i} is framed as a single-concept derive stores it"
        );
        assert!(
            cos(i) < 0.80,
            "{dim}: concept {i} cosine {} sits in Qwen's narrow band",
            cos(i)
        );
    }
}

#[test]
fn the_2560_vectors_keep_the_live_geometry() {
    assert_live_geometry(2560);
}

#[test]
fn the_1024_vectors_keep_the_live_geometry() {
    assert_live_geometry(1024);
}

/// Under max-merge the fixture reproduces the reported misrank: the
/// demo-API observation (BM25 5.38 on the shared API/returns/one) outranks
/// the lesson (BM25 2.53), so the vector leg's top hit lands second.
async fn assert_max_misranks(dim: usize) {
    let ranked = recall_issue_session(RecallFusion::default(), dim).await;
    assert_eq!(
        ranked[0], CONCEPTS[5].0,
        "{dim}: demo-API observation first under max"
    );
    assert_eq!(ranked[1], CONCEPTS[1].0, "{dim}: lesson second under max");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_merge_reproduces_the_issue_misrank_2560() {
    assert_max_misranks(2560).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn max_merge_reproduces_the_issue_misrank_1024() {
    assert_max_misranks(1024).await;
}

/// Under the Qwen contract's RRF default the lesson ranks first: RRF compares
/// the legs' ranks instead of their raw scales, so the vector leg's top hit
/// counts again and the daemon blend puts it ahead of the keyword winner.
async fn assert_rrf_default_ranks_lesson_first(dim: usize) {
    use crate::embed::calibration::{default_fusion, CalibrationMode};
    let fusion = default_fusion(CalibrationMode::Builtin, &qwen_contract(dim));
    assert_eq!(fusion.kind, crate::recall::fusion::FusionKind::Rrf);
    let ranked = recall_issue_session(fusion, dim).await;
    assert_eq!(
        ranked[0], CONCEPTS[1].0,
        "{dim}: lesson first under the RRF default"
    );
    assert_eq!(
        ranked[1], CONCEPTS[5].0,
        "{dim}: demo-API observation second"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_qwen_rrf_default_ranks_the_lesson_first_2560() {
    assert_rrf_default_ranks_lesson_first(2560).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn the_qwen_rrf_default_ranks_the_lesson_first_1024() {
    assert_rrf_default_ranks_lesson_first(1024).await;
}
