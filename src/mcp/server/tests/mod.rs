//! Unit tests for the MCP tool server, grouped by subject.

use super::*;
use crate::canon::PromotionPolicy;
use crate::embed::{Embedder, FixtureEmbedder};
use crate::store::{GraphStore, MemoryStore};
use crate::types::EmbeddingContract;
use crate::Config;

// What the subject files used to reach through `use super::*` while every
// production item sat in `server.rs`: since #25 split it into submodules they
// are named here, so the subject files keep their single `use super::*`.
use super::params::MAX_AGENT_ID_CHARS;
use super::response::{conflict_err, contain_panic, redact_urls, tool_err};
use super::stats::{gc_stats_json, gc_summary_line};
use super::trace::{
    recall_facts, truncate_for_ledger, truncate_to, LEDGER_CONTENT_PREFIX, LEDGER_FOCUS_PREFIX,
    LEDGER_QUERY_PREFIX,
};
use crate::graph::action::Action;
use crate::graph::derive::ParentOf;
use crate::surface::focus::resolve_focus;
use crate::surface::limits::{clamp_cfg_default, MAX_ACTION_TARGETS, MAX_TOP_K};
use crate::types::{ConceptType, LamboError, NodeId};
use chrono::{DateTime, Utc};
use rmcp::model::ContentBlock;
use serde_json::json;
use std::time::Duration;

mod derive_image;
mod errors;
mod inspect;
mod ledger;
mod receipts;
mod schemas;
mod stats;
mod tools;

async fn server(session: &str) -> LamboServer {
    server_with_config(session, Config::default()).await
}

async fn server_with_config(session: &str, config: Config) -> LamboServer {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    server_with_parts(
        session,
        store,
        Arc::new(FixtureEmbedder::new()),
        fixture_contract(),
        config,
    )
    .await
}

fn fixture_contract() -> EmbeddingContract {
    EmbeddingContract {
        kind: "fixture".into(),
        model: None,
        dim: 1024,
    }
}

/// [`server_with_config`] over a chosen store, embedder and contract (#22).
async fn server_with_parts(
    session: &str,
    store: Arc<dyn GraphStore>,
    embedder: Arc<dyn Embedder>,
    contract: EmbeddingContract,
    config: Config,
) -> LamboServer {
    let mem = Memory::builder()
        .session(session)
        .agent("agent-a")
        .config(config)
        // Keep the background flush loop out of the assertions.
        .flush_interval(Duration::from_secs(3_600))
        .store(store)
        .embedder(embedder)
        .embedding_contract(contract)
        .build()
        .await
        .expect("build");
    LamboServer::new(Arc::new(mem))
}

/// The fixture embedder with its image modality hidden: the shape of every
/// text-only deployment (BGE-M3 over llama.cpp, candle, Gemini), whose
/// servers list exactly the seven spec tools (#22).
struct TextOnly(FixtureEmbedder);

#[async_trait::async_trait]
impl Embedder for TextOnly {
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.0.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, crate::embed::EmbedError> {
        self.0.embed_query(text).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.0.as_any()
    }
}

/// A server that can serve an image derive (#22): the fixture embedder, which
/// embeds images, over a `MemoryStore` that claims vector search, under the
/// default (`Hybrid`) strategy.
async fn image_server(session: &str, config: Config) -> LamboServer {
    server_with_parts(
        session,
        Arc::new(crate::test_util::VectorSearchable(Arc::new(
            MemoryStore::new(),
        ))),
        Arc::new(FixtureEmbedder::new()),
        fixture_contract(),
        config,
    )
    .await
}

/// A PNG the fixture embeds exactly as the text query `label`, base64-encoded
/// for the wire.
fn png_b64(label: &str) -> String {
    use base64::Engine as _;
    base64::engine::general_purpose::STANDARD.encode(crate::embed::png_with_label(label))
}

/// A server whose embedder embeds text only, over a plain `MemoryStore`.
async fn text_only_server(session: &str, config: Config) -> LamboServer {
    server_with_parts(
        session,
        Arc::new(MemoryStore::new()),
        Arc::new(TextOnly(FixtureEmbedder::new())),
        fixture_contract(),
        config,
    )
    .await
}

fn tools(s: &LamboServer) -> Vec<rmcp::model::Tool> {
    s.tool_router.list_all()
}

/// Drive a tool by its published name, from the JSON a client would send.
///
/// This deserializes through the real `Parameters<T>` types, so wire-shape
/// bugs are caught here. It deliberately does **not** go through
/// `ToolRouter::call`: building a `RequestContext` needs a live `Peer`, and
/// the protocol path (handshake, `tools/list`, `tools/call` dispatch) is
/// covered end-to-end by the real Claude Code client run captured in
/// `dev-diary/evidence/t8.2-mcp-client/`, which is stronger evidence than a
/// hand-built context would be.
/// Drive a tool and, for a J3 write, **wait for it to be applied** before
/// returning.
///
/// This is the default harness entry point because "call the tool and let
/// it finish" is what almost every test in this module means; an assertion
/// about the graph immediately after an asynchronous ack is a race, not a
/// test. Tests that are *about* the ack — its shape, a drop, a pending
/// receipt — use [`call_raw`] instead.
///
/// The wait goes through `pipeline().wait` rather than through the shipped
/// `lambo_stats` receipt surface **on purpose**: a second tool call here
/// would append a second ledger line, and several tests in this module
/// count lines per call. The shipped surface is exercised by
/// `waiting_on_a_receipt_through_lambo_stats_restores_read_your_writes`,
/// which is the test that owns that claim.
async fn call(s: &LamboServer, name: &str, args: serde_json::Value) -> CallToolResult {
    let agent_id = args
        .get("agent_id")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    let out = call_raw(s, name, args).await;
    let receipt = out
        .structured_content
        .as_ref()
        .and_then(|v| v.get("receipt"))
        .and_then(|v| v.as_str())
        .and_then(|r| r.parse::<crate::writeq::ReceiptId>().ok());
    if let Some(id) = receipt {
        let answer = s
            .mem
            .pipeline()
            .wait(
                &AgentId::new(&agent_id),
                id,
                crate::writeq::RECEIPT_WAIT_MAX,
            )
            .await;
        assert!(
            answer.is_settled(),
            "{name}'s receipt did not settle within RECEIPT_WAIT_MAX: {}",
            answer.tag()
        );
    }
    out
}

/// Derive and return the node ids it created, read off the **receipt**
/// through the shipped `lambo_stats` surface.
///
/// Before J3 the ids were in `lambo_derive`'s own `structuredContent`.
/// They cannot be: an ack issued before the write has no ids to report. So
/// this is the shape a real agent now uses when it needs a node id — derive,
/// then wait on the receipt — and every test that used to read
/// `derived["created"][0]` goes through it.
async fn derive_created(
    s: &LamboServer,
    agent_id: &str,
    concepts: serde_json::Value,
) -> Vec<String> {
    let ack = call_raw(
        s,
        "lambo_derive",
        serde_json::json!({"agent_id": agent_id, "concepts": concepts}),
    )
    .await;
    assert_eq!(ack.is_error, Some(false), "derive failed: {ack:?}");
    let receipt = ack.structured_content.as_ref().expect("ack payload")["receipt"]
        .as_str()
        .expect("ack carries a receipt id")
        .to_string();
    let waited = call_raw(
        s,
        "lambo_stats",
        serde_json::json!({
            "agent_id": agent_id,
            "receipt": receipt,
            "wait_ms": crate::writeq::RECEIPT_WAIT_MAX.as_millis() as u64,
        }),
    )
    .await;
    let payload = waited.structured_content.expect("stats payload");
    assert_eq!(
        payload["receipt"]["state"].as_str(),
        Some("applied"),
        "receipt did not apply: {}",
        payload["receipt"]
    );
    payload["receipt"]["created"]
        .as_array()
        .expect("an applied derive receipt lists what it created")
        .iter()
        .map(|v| v.as_str().expect("node id string").to_string())
        .collect()
}

/// The raw tool call: no receipt wait, so a J3 ack is observed as the ack
/// it is.
async fn call_raw(s: &LamboServer, name: &str, args: serde_json::Value) -> CallToolResult {
    fn parse<T: serde::de::DeserializeOwned>(v: serde_json::Value) -> Parameters<T> {
        Parameters(serde_json::from_value(v).expect("tool params deserialize"))
    }
    match name {
        "lambo_recall" => s.lambo_recall(parse(args)).await,
        "lambo_derive" => s.lambo_derive(parse(args)).await,
        "lambo_record_action" => s.lambo_record_action(parse(args)).await,
        "lambo_reserve" => s.lambo_reserve(parse(args)).await,
        "lambo_inspect" => s.lambo_inspect(parse(args)).await,
        "lambo_saints" => s.lambo_saints(parse(args)).await,
        "lambo_stats" => s.lambo_stats(parse(args)).await,
        "lambo_derive_image" => s.lambo_derive_image(parse(args)).await,
        other => panic!("unknown tool {other}"),
    }
}

/// Pull the concatenated text content out of a result — what an MCP client
/// actually feeds the model.
fn text_of(out: &CallToolResult) -> String {
    out.content
        .iter()
        .filter_map(|c| match c {
            ContentBlock::Text(t) => Some(t.text.clone()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

// =======================================================================
// I1 / I2 — the serve call ledger
// =======================================================================

/// A scratch directory outside the repo, unique per test.
fn ledger_dir(tag: &str) -> crate::test_util::ScratchDir {
    crate::test_util::ScratchDir::new(&format!("lambo-i1-{tag}"))
}

/// The same [`server`] fixture, with a ledger attached.
async fn server_with_ledger(session: &str, path: &std::path::Path) -> LamboServer {
    let plain = server(session).await;
    LamboServer::with_ledger(Arc::clone(plain.memory()), Ledger::open(path))
}

/// Wait until the writer thread has caught up, then parse the file.
///
/// The writer is a real OS thread by design (it must not sit on a Tokio
/// worker), so tests wait on `written` rather than sleeping a guessed
/// interval.
fn read_ledger(ledger: &Ledger, expect_lines: u64) -> Vec<serde_json::Value> {
    let deadline = Instant::now() + Duration::from_secs(10);
    while ledger.counters().written() < expect_lines && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert_eq!(
        ledger.counters().written(),
        expect_lines,
        "expected {expect_lines} written lines, dropped={}",
        ledger.counters().dropped()
    );
    let text = std::fs::read_to_string(ledger.path()).expect("ledger file");
    text.lines()
        .map(|l| {
            serde_json::from_str(l).unwrap_or_else(|e| panic!("line does not parse: {e}: {l}"))
        })
        .collect()
}
