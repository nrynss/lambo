//! Unit tests for the read-only web portal, grouped by subject.

use super::*;
use super::{auth::*, projections::*, routes::*};
use crate::cli::caps::{ConceptKind, MAX_INSPECT_NODES};
use crate::embed::{EmbedderConfig, EmbedderKind, FixtureEmbedder};
use crate::store::{Capabilities, GraphStore, StoreConfig, StoreKind};
use crate::surface::bearer::tokens_match;
use crate::types::{
    AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType,
    EmbeddingContract, GraphSnapshot, Interaction, Mutation, MutationBatch, Node, NodeId, Scored,
    SessionId, StoreError,
};
use crate::MemoryStore;
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use std::net::{Ipv4Addr, SocketAddr};
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Every production source file of the portal, for the source-scan tests
/// (`routes::the_module_registers_only_get_routes`,
/// `routes::routes_constant_covers_every_registered_route`,
/// `auth::the_portal_uses_the_shared_bearer_check`).
///
/// The scans used to read `serve_web.rs` alone. #28 split it, and a scan left
/// pointing at one file stays green while scanning less, so the list is the
/// scans' single input and `routes::the_source_scans_cover_every_production_file`
/// fails when a file under `src/cli/serve_web/`, at any depth outside
/// `tests/`, is missing from it.
const PRODUCTION_SOURCES: &[(&str, &str)] = &[
    (
        "serve_web.rs",
        include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/cli/serve_web.rs")),
    ),
    (
        "serve_web/auth.rs",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/cli/serve_web/auth.rs"
        )),
    ),
    (
        "serve_web/dto.rs",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/cli/serve_web/dto.rs"
        )),
    ),
    (
        "serve_web/projections.rs",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/cli/serve_web/projections.rs"
        )),
    ),
    (
        "serve_web/routes.rs",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/cli/serve_web/routes.rs"
        )),
    ),
    (
        "serve_web/state.rs",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/cli/serve_web/state.rs"
        )),
    ),
    (
        "serve_web/views.rs",
        include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/src/cli/serve_web/views.rs"
        )),
    ),
];

/// The production text of every portal source: each file up to its
/// `#[cfg(all(test` line (only the root has one), concatenated.
fn production_source() -> String {
    PRODUCTION_SOURCES
        .iter()
        .map(|(_, src)| src.split("#[cfg(all(test").next().unwrap_or(src))
        .collect::<Vec<_>>()
        .join("\n")
}

/// The file that holds `fn router(`. Exactly one must.
fn router_source() -> &'static str {
    let holders: Vec<&(&str, &str)> = PRODUCTION_SOURCES
        .iter()
        .filter(|(_, src)| src.contains("fn router("))
        .collect();
    assert_eq!(
        holders.len(),
        1,
        "exactly one portal source must define fn router(, found {:?}",
        holders.iter().map(|(name, _)| *name).collect::<Vec<_>>()
    );
    holders[0].1
}

mod auth;
mod feeds;
mod graph;
mod inspect;
mod recall;
mod routes;
mod session;
mod shutdown;
mod views;

/// Every path `router` answers. The read-only method sweep iterates this,
/// and `routes_constant_covers_every_registered_route` proves it is not
/// missing one — so a new route cannot be added without being checked.
const ROUTES: &[&str] = &[
    "/",
    "/app.css",
    "/app.js",
    "/healthz",
    "/api/session",
    "/api/inspect",
    "/api/graph",
    "/api/recall",
    "/api/events",
    "/api/stats",
    "/api/pulse",
];

/// `Arc<MemoryStore>` as a `GraphStore`, so the seeding CLI writes and the
/// web reader share one in-RAM store the way two processes share a file.
#[derive(Clone)]
struct Shared(Arc<MemoryStore>);

#[async_trait]
impl GraphStore for Shared {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.0.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities()
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.0.vector_dimensions()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.0.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.0.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.vector_candidates(session, embedding, limit).await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.0.blast_radius(session, node, min_edge_age, now).await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.0.interaction_span(session, node, min_age, now).await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.0.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        self.0.acquire_lease(session, holder, ttl).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        self.0.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
    ) -> Result<(), StoreError> {
        self.0.release_lease(session, holder).await
    }
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &crate::store::SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.0.write_flush_stats(session, stats).await
    }
    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::SessionFlushStats>, StoreError> {
        self.0.read_flush_stats(session).await
    }
}

/// [`Shared`] that also claims `VECTOR_SEARCH`, so a failing embedder
/// actually runs on the reader path (a plain `MemoryStore` claims no
/// capabilities, and the embed would be skipped entirely).
#[derive(Clone)]
struct VectorSearch(Shared);

#[async_trait]
impl GraphStore for VectorSearch {
    async fn init_schema(&self) -> Result<(), StoreError> {
        self.0.init_schema().await
    }
    fn capabilities(&self) -> Capabilities {
        self.0.capabilities() | Capabilities::VECTOR_SEARCH
    }
    fn vector_dimensions(&self) -> Option<usize> {
        self.0.vector_dimensions()
    }
    async fn flush(&self, batch: &MutationBatch, token: Option<u64>) -> Result<(), StoreError> {
        self.0.flush(batch, token).await
    }
    async fn load_session(&self, session: &SessionId) -> Result<GraphSnapshot, StoreError> {
        self.0.load_session(session).await
    }
    async fn keyword_candidates(
        &self,
        session: &SessionId,
        tokens: &[String],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.keyword_candidates(session, tokens, limit).await
    }
    async fn vector_candidates(
        &self,
        session: &SessionId,
        embedding: &[f32],
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0.vector_candidates(session, embedding, limit).await
    }
    async fn vector_candidates_checked(
        &self,
        session: &SessionId,
        embedding: &[f32],
        expected_contract: &EmbeddingContract,
        limit: usize,
    ) -> Result<Vec<Scored<NodeId>>, StoreError> {
        self.0
            .vector_candidates_checked(session, embedding, expected_contract, limit)
            .await
    }
    async fn blast_radius(
        &self,
        session: &SessionId,
        node: NodeId,
        min_edge_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<u64, StoreError> {
        self.0.blast_radius(session, node, min_edge_age, now).await
    }
    async fn interaction_span(
        &self,
        session: &SessionId,
        node: NodeId,
        min_age: Duration,
        now: DateTime<Utc>,
    ) -> Result<crate::types::InteractionSpan, StoreError> {
        self.0.interaction_span(session, node, min_age, now).await
    }
    async fn record_canonization(
        &self,
        event: &CanonizationEvent,
        token: Option<u64>,
    ) -> Result<(), StoreError> {
        self.0.record_canonization(event, token).await
    }
    async fn acquire_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        self.0.acquire_lease(session, holder, ttl).await
    }
    async fn refresh_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
        ttl: Duration,
    ) -> Result<crate::store::lease::LeaseOutcome, StoreError> {
        self.0.refresh_lease(session, holder, ttl).await
    }
    async fn release_lease(
        &self,
        session: &SessionId,
        holder: &crate::store::lease::LeaseHolder,
    ) -> Result<(), StoreError> {
        self.0.release_lease(session, holder).await
    }
    async fn write_flush_stats(
        &self,
        session: &SessionId,
        stats: &crate::store::SessionFlushStats,
    ) -> Result<(), StoreError> {
        self.0.write_flush_stats(session, stats).await
    }
    async fn read_flush_stats(
        &self,
        session: &SessionId,
    ) -> Result<Option<crate::store::SessionFlushStats>, StoreError> {
        self.0.read_flush_stats(session).await
    }
}

fn backends_on(store: Arc<MemoryStore>) -> ResolvedBackends {
    backends_with_store(Box::new(Shared(store)))
}

/// [`backends_on`] for a pre-wrapped store, so the query-count regression
/// can serve a counting wrapper without duplicating the embedder config.
fn backends_with_store(store: Box<dyn GraphStore>) -> ResolvedBackends {
    ResolvedBackends {
        store,
        embedder: Box::new(FixtureEmbedder::new()),
        store_cfg: StoreConfig {
            kind: StoreKind::Memory,
            dsn: None,
            path: None,
            vector_dim: None,
        },
        embedder_cfg: EmbedderConfig {
            kind: EmbedderKind::Fixture,
            dim: 1024,
            llama_url: None,
            llama_model: None,
            ..Default::default()
        },
        embedding: EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        },
        allow_embedding_mismatch: false,
        config: crate::Config::default(),
    }
}

fn state_on(store: Arc<MemoryStore>, session: &str) -> Arc<AppState> {
    state_with_auth(store, session, None)
}

fn state_with_auth(
    store: Arc<MemoryStore>,
    session: &str,
    auth: Option<AuthToken>,
) -> Arc<AppState> {
    state_from_backends(backends_on(store), session, auth)
}

fn state_from_backends(
    backends: ResolvedBackends,
    session: &str,
    auth: Option<AuthToken>,
) -> Arc<AppState> {
    state_with_web(
        backends,
        session,
        auth,
        &crate::config::WebConfig::default(),
    )
}

/// [`state_from_backends`] with explicit `[web]` bounds.
fn state_with_web(
    backends: ResolvedBackends,
    session: &str,
    auth: Option<AuthToken>,
    web: &crate::config::WebConfig,
) -> Arc<AppState> {
    let exposed = auth.is_some();
    Arc::new(AppState::new(
        SessionId::new(session),
        backends,
        exposed,
        auth,
        web,
    ))
}

/// `[web] view_ttl_ms = 0`: every request after a write sees it. For the
/// tests that write between two requests and assert the second sees the
/// write, a property of the store read, not of the view TTL (which
/// `views::a_write_is_served_after_the_ttl_and_not_before` pins).
fn web_ttl_zero() -> crate::config::WebConfig {
    crate::config::WebConfig {
        view_ttl_ms: Some(0),
        ..Default::default()
    }
}

/// A session with real content: two concepts in a hierarchy, an action, and
/// an audited promotion of "user schema" all the way to Canonical.
async fn seed(session: &str) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    crate::cli::derive::run(
        backends_on(store.clone()),
        crate::cli::derive::Args {
            session: session.into(),
            agent: "agent-a".into(),
            content: "user schema".into(),
            kind: ConceptKind::Entity,
            parent_of: vec!["auth middleware:user schema".into()],
            concept: vec!["auth middleware:entity".into()],
        },
    )
    .await
    .expect("derive");

    crate::cli::record_action::run(
        backends_on(store.clone()),
        crate::cli::record_action::Args {
            session: session.into(),
            agent: "agent-a".into(),
            action: "create user".into(),
            produces: vec!["user schema".into()],
            modifies: vec![],
            depends_on: vec!["auth middleware".into()],
        },
    )
    .await
    .expect("record-action");

    promote(&store, session, "user schema").await;
    store
}

/// Walk a concept through the audited transition path, recording each hop
/// exactly as the canonization task does: under a lease of its own, since the
/// session has been leased (by the derive above) and a lease's token is
/// never reset, so an unleased write is refused.
async fn promote(store: &Arc<MemoryStore>, session: &str, content: &str) {
    let sid = SessionId::new(session);
    let canon = crate::store::lease::LeaseHolder {
        endpoint: None,
        agent: AgentId::from("canon-fixture"),
        pid: 1,
        host: "test".into(),
    };
    let crate::store::lease::LeaseOutcome::Acquired(lease) = store
        .acquire_lease(&sid, &canon, std::time::Duration::from_secs(60))
        .await
        .expect("acquire")
    else {
        panic!("the fixture must take the released session");
    };
    let snap = store.load_session(&sid).await.expect("snapshot");
    let node = snap
        .concepts
        .iter()
        .find(|c| c.content == content)
        .map(|c| c.id)
        .unwrap_or_else(|| panic!("{content} must exist"));

    for (from, to) in [
        (CanonizationStatus::None, CanonizationStatus::Candidate),
        (CanonizationStatus::Candidate, CanonizationStatus::Venerable),
        (CanonizationStatus::Venerable, CanonizationStatus::Canonical),
    ] {
        store
            .record_canonization(
                &CanonizationEvent {
                    id: NodeId::new(),
                    session_id: sid.clone(),
                    node_id: node,
                    from_status: from,
                    to_status: to,
                    blast_radius: Some(1),
                    occurred_at: Utc::now(),
                    last_demotion_time: None,
                },
                Some(lease.token),
            )
            .await
            .expect("record canonization");
    }
    store.release_lease(&sid, &canon).await.expect("release");
}

fn concept(
    sid: SessionId,
    id: NodeId,
    origin: NodeId,
    content: &str,
    created: DateTime<Utc>,
) -> Concept {
    Concept {
        id,
        session_id: sid,
        content: content.to_string(),
        canonical_key: content.to_string(),
        concept_type: ConceptType::Entity,
        origin_interaction: origin,
        origin_agent: AgentId::from("agent-a"),
        created_at: created,
        access_count: 0,
        last_accessed: None,
        gc_survived: 0,
        canonization_status: CanonizationStatus::None,
        blast_radius: None,
        last_demotion_time: None,
        embedding: None,
        human_confirmed: 0,
        embedding_source: None,
        chunk_group_id: None,
    }
}

fn edge(
    id: NodeId,
    sid: SessionId,
    source: NodeId,
    target: NodeId,
    edge_type: EdgeType,
    created: DateTime<Utc>,
) -> Edge {
    Edge {
        event_time: None,
        id,
        session_id: sid,
        source,
        target,
        edge_type,
        weight: 1.0,
        reinforcements: 1,
        created_at: created,
        last_reinforced: created,
    }
}

/// A session shaped like `focus` standing behind `dependents` leaves,
/// each a structural (Dependency) edge `focus -> dep_i`, plus one
/// interaction to root the concepts on. No canonization runs, so
/// `focus` stays status `None` — a load-bearing non-canonical node.
async fn seed_chain_around(session: &str, focus: &str, dependents: usize) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::new());
    let sid = SessionId::new(session);
    let iid = NodeId::new();
    let focus_id = NodeId::new();
    let now = Utc::now();
    let mut batch = MutationBatch::new();
    batch.push(Mutation::UpsertNode {
        node: Node::Interaction(Interaction {
            event_time: None,
            id: iid,
            session_id: sid.clone(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(focus.to_string()),
            previous_id: None,
            created_at: now,
        }),
    });
    batch.push(Mutation::UpsertNode {
        node: Node::Concept(concept(sid.clone(), focus_id, iid, focus, now)),
    });
    // §5.7: every concept must have a Derives edge from an interaction.
    batch.push(Mutation::UpsertEdge {
        edge: edge(
            NodeId::new(),
            sid.clone(),
            iid,
            focus_id,
            EdgeType::Derives,
            now,
        ),
    });
    for i in 0..dependents {
        let cid = NodeId::new();
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(concept(sid.clone(), cid, iid, &format!("dep{i}"), now)),
        });
        batch.push(Mutation::UpsertEdge {
            edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
        });
        batch.push(Mutation::UpsertEdge {
            edge: edge(
                NodeId::new(),
                sid.clone(),
                focus_id,
                cid,
                EdgeType::Dependency,
                now,
            ),
        });
    }
    store.flush(&batch, None).await.expect("seed chain");
    store
}

// ---- a minimal HTTP/1.1 client -------------------------------------
//
// Dependency-free on purpose: `reqwest` is gated behind `embed-bge`, and
// these tests must run under any feature combination that builds the web
// module. One request, `Connection: close`, read to EOF.

struct HttpResponse {
    status: u16,
    headers: String,
    body: String,
}

async fn request(addr: SocketAddr, method: &str, path: &str) -> HttpResponse {
    let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
    let req = format!(
        "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\nConnection: close\r\n\r\n"
    );
    sock.write_all(req.as_bytes()).await.expect("write");
    sock.flush().await.expect("flush");
    let mut raw = Vec::new();
    sock.read_to_end(&mut raw).await.expect("read");
    let raw = String::from_utf8_lossy(&raw).into_owned();

    let (head, body) = raw.split_once("\r\n\r\n").unwrap_or((raw.as_str(), ""));
    let status = head
        .lines()
        .next()
        .and_then(|l| l.split_whitespace().nth(1))
        .and_then(|c| c.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("no status line in: {head}"));

    // `Connection: close` means no chunked framing from hyper, so the body
    // is the bytes after the header block, verbatim.
    HttpResponse {
        status,
        headers: head.to_string(),
        body: body.to_string(),
    }
}

async fn get_json(addr: SocketAddr, path: &str) -> serde_json::Value {
    let r = request(addr, "GET", path).await;
    assert_eq!(r.status, 200, "GET {path} -> {}\n{}", r.status, r.body);
    serde_json::from_str(&r.body)
        .unwrap_or_else(|e| panic!("GET {path} body is not JSON ({e}): {}", r.body))
}

/// Bind an ephemeral port and serve `state` until the guard is dropped.
async fn spawn(state: Arc<AppState>) -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
        .await
        .expect("bind ephemeral");
    let addr = listener.local_addr().expect("addr");
    let app = router(state);
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, handle)
}
