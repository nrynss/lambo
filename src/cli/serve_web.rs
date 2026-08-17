//! `lambo serve-web` — the T8.5 demo window: a read-only page onto one session.
//!
//! # What it is
//!
//! A single axum server that renders three things and nothing else: the T5.3
//! **recall context block verbatim**, the T6.4 **canonization event feed**, and
//! durable session counts. It is a window onto the product's real output, not a
//! product — no framework, no build step, no client state beyond a poll cursor.
//!
//! # Read-only, by construction
//!
//! **Auth, mirroring T8.7's fail-closed rule.** Two consequences this module
//! is built around:
//!
//! 1. **This app must stay read-only.** Every route is registered with
//!    `routing::get` and every handler reads. There is deliberately no
//!    `derive` / `record_action` / `reserve` path reachable from the browser:
//!    a write surface is a stranger with a pen in your session's memory.
//!    `read_only_router_has_no_mutating_route` and
//!    `the_module_registers_only_get_routes` fail the build's test gate if a
//!    later edit adds one.
//! 2. **Loopback is unauthenticated by default; anywhere else fails closed.**
//!    Reading still leaks the whole session to whoever can reach the port, so
//!    `--bind` defaults to loopback and needs **no token** — a judge's browser
//!    just works. A non-loopback bind (LAN or public) is refused at startup
//!    unless a bearer token is configured (`LAMBO_AUTH_TOKEN` env or
//!    `--auth-token`); when a token is set, every request must send
//!    `Authorization: Bearer <token>` (mirrors `crate::mcp::serve`'s
//!    `authorize_bind`). The surface stays read-only either way, and a
//!    token-protected bind should still sit behind a private network or an
//!    authenticating proxy.
//!
//! # Reader, not writer (spec §2.2)
//!
//! This process is a **reader**: it never constructs a [`Memory`], never takes
//! the T8.6 writer lease, and never spawns GC — same discipline as
//! [`crate::cli::recall`] and [`crate::cli::stats`]. Recall reuses
//! `cli::recall::run` outright, so the page cannot drift from what the CLI and
//! MCP surfaces return.
//!
//! The honest cost of least privilege, stated on the page rather than papered
//! over:
//!
//! * **The live feed is a store poll, not the daemon broadcast.**
//!   [`Memory::events`] is an in-process `broadcast` owned by the writer, and a
//!   separate reader process cannot subscribe to it. The feed instead tails
//!   `GraphSnapshot::canonization_events`, which is the same audit trail the
//!   writer durably records — one hop behind the broadcast (bounded by the
//!   writer's flush interval) and a *superset* across writer restarts. Taking
//!   the broadcast would mean becoming the writer, which would mean holding the
//!   lease, which would mean this page could not run beside a live `lambo serve`.
//! * **`flush_lag` / `log_depth` are reported as `n/a` only when no writer has
//!   published them yet.** The writer's `FlushTask` publishes its flush stats
//!   into the shared store after each cycle (T85-3), and this reader fetches
//!   them — so a live writer shows real numbers. When no writer has published
//!   yet (or the store doesn't support it), the page reports `n/a`. A reader
//!   that fabricated `0` would be claiming a durability bound it cannot see
//!   (same call [`crate::cli::stats`] makes); a published value is a real
//!   measurement this reader *can* see.
//! * **Graph `epoch` is not surfaced at all.** `Graph::from_snapshot` starts a
//!   loaded graph at epoch 0, so a reader's epoch is always 0 — a number that
//!   looks live and is not.
//!
//! What *is* live: node / edge / concept / canonical counts, the canonization
//! feed, and `durable_change_age_ms` (how long since this reader last observed
//! the durable snapshot change) — all of which move during a demo scenario.
//!
//! # Deployment (P9 target: AWS)
//!
//! * **Self-contained binary.** `web/index.html`, `web/app.css` and `web/app.js`
//!   are `include_str!`-embedded. No CDN, no webfont, no asset directory to
//!   ship; the page renders on a host with zero egress.
//! * **Polling, not SSE.** The page polls `/api/pulse` every 1.5 s. Beyond
//!   there being no `Stream` implementation in the dependency set to hand
//!   `axum::response::Sse`, a short poll survives ALB/CloudFront idle timeouts
//!   and connection recycling, which a long-lived SSE channel does not.
//! * **`GET /healthz`** answers ALB / ECS health checks without touching the
//!   store, so a slow database degrades the page instead of failing the target.
//! * **No secrets in the page.** `/api/session` reports store and embedder
//!   *kind* only — never the DSN, the SQLite path, or the embedder URL.
//!   `session_info_never_leaks_the_dsn_path_or_embedder_url` pins that.
//!
//! [`Memory`]: crate::memory::Memory
//! [`Memory::events`]: crate::memory::Memory::events

use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::{Duration, Instant};

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde::{Deserialize, Serialize};

use super::caps::{check_size_cli, require_nonempty, CliError, MAX_INSPECT_NODES};
use super::load_reader_graph_with_contract;
use crate::canon::{gate_progress, GateProgress};
use crate::cli::inspect::{resolve_focus, Focus};
use crate::graph::Graph;
use crate::mcp::AUTH_TOKEN_ENV;
use crate::recall::format::blast_radii;
use crate::resolve::ResolvedBackends;
use crate::store::{Capabilities, GraphStore, SessionFlushStats, StoreKind};
use crate::types::{
    CanonizationStatus, ConceptType, EdgeType, GraphSnapshot, Node, NodeId, SessionId, StoreError,
};

// ---------------------------------------------------------------------------
// Embedded assets — the whole client, compiled into the binary (P9/AWS).
// ---------------------------------------------------------------------------

const INDEX_HTML: &str = include_str!("../../web/index.html");
const APP_CSS: &str = include_str!("../../web/app.css");
const APP_JS: &str = include_str!("../../web/app.js");

/// How often the page re-reads `/api/pulse`. Served to the client so the
/// interval has exactly one definition.
const POLL_INTERVAL: Duration = Duration::from_millis(1_500);

/// Cap on how long a graceful shutdown may take before the process stops
/// waiting for in-flight connections.
///
/// A reader holds no writer lease and no un-flushed tail, so an abandoned
/// shutdown loses **nothing** — the bound exists purely so Ctrl-C always exits
/// rather than blocking behind a client that will not let go.
const SHUTDOWN_GRACE: Duration = Duration::from_secs(5);

/// Upper bound on the concepts `/api/graph` returns. The tree view marks
/// load-bearing nodes, and the exhibit stays well under it; the cap exists so
/// a pathological session cannot balloon the payload. When it is hit the
/// handler says so with `"truncated": true` rather than cutting silently.
const MAX_GRAPH_NODES: usize = 4_096;

/// Upper bound on the structural edges `/api/graph` returns. Same rationale
/// as [`MAX_GRAPH_NODES`], and surfaced the same way.
const MAX_GRAPH_EDGES: usize = 16_384;

// ---------------------------------------------------------------------------
// Args
// ---------------------------------------------------------------------------

/// `lambo serve-web` arguments, mirroring `lambo serve`'s bind/port conventions.
#[derive(Debug, Clone)]
pub struct Args {
    /// Session to open a window onto. Read as a reader; never written.
    pub session: String,
    /// TCP port to listen on.
    pub port: u16,
    /// Bind address. Loopback by default — no token required. A non-loopback
    /// bind requires a token (see `authorize_bind_web`).
    pub bind: IpAddr,
    /// Optional bearer token required on every request. Prefer the
    /// [`AUTH_TOKEN_ENV`] env var, which overrides this flag — a token in argv
    /// is visible in `ps` and shell history. Mandatory on any non-loopback bind.
    pub auth_token: Option<AuthToken>,
}

// ---------------------------------------------------------------------------
// Auth (mirrors T8.7's fail-closed bearer posture in `crate::mcp::serve`)
// ---------------------------------------------------------------------------

/// A bearer token that cannot be printed.
///
/// Mirrors `mcp::serve::SecretToken`: a redacting [`Debug`] makes "never
/// logged" a property of the type, and rejecting empty/whitespace tokens makes
/// a set-but-empty [`AUTH_TOKEN_ENV`] a usage error rather than a silent
/// authenticate-everything.
#[derive(Clone, PartialEq, Eq)]
pub struct AuthToken(String);

impl AuthToken {
    /// Reject empty and whitespace-only tokens (fail closed, not silently).
    fn new(raw: impl Into<String>) -> Result<Self, String> {
        let raw = raw.into();
        if raw.trim().is_empty() {
            return Err(
                "auth token is empty — pass a non-empty secret, or omit it entirely to \
                 run unauthenticated on loopback"
                    .into(),
            );
        }
        Ok(Self(raw))
    }

    fn as_bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

impl std::fmt::Debug for AuthToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AuthToken(<redacted>)")
    }
}

impl std::str::FromStr for AuthToken {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Self::new(s)
    }
}

/// Compare `presented` against `expected` without an early exit and with a
/// loop count independent of the presented input's length.
///
/// Mirrors `mcp::serve`'s `tokens_match`: the accumulate-then-test shape
/// keeps the time independent of where the first differing byte falls, and
/// [`std::hint::black_box`] stops the optimiser from proving the accumulator
/// can be short-circuited. The loop runs over **`expected`** (the secret,
/// whose length is fixed per deployment) and every byte of `expected` is
/// consumed on every call — the number of iterations depends only on the
/// secret, never on `presented`'s length, so the input cannot leak its length
/// through the loop count. A length change is folded into `diff` via the XOR
/// below, so a truncated or padded `presented` is still refused.
fn tokens_match(presented: &[u8], expected: &[u8]) -> bool {
    if expected.is_empty() {
        // Unreachable via `AuthToken::new`, which rejects empty tokens; a
        // belt-and-braces guard so the `%` below cannot divide by zero.
        return false;
    }
    let mut diff = (presented.len() ^ expected.len()) as u64;
    for (i, exp_byte) in expected.iter().enumerate() {
        let presented_byte = if presented.is_empty() {
            0
        } else {
            presented[i % presented.len()]
        };
        diff |= u64::from(exp_byte ^ presented_byte);
    }
    std::hint::black_box(diff) == 0
}

/// Does an `Authorization` header carry the expected bearer token?
///
/// Scheme matched case-insensitively (RFC 7235 §2.1); the credential compared
/// byte-for-byte in constant time. Mirrors `mcp::serve::bearer_ok`.
fn bearer_ok(header: Option<&str>, expected: &AuthToken) -> bool {
    let Some(raw) = header else {
        return false;
    };
    let raw = raw.trim();
    let Some((scheme, credential)) = raw.split_once(' ') else {
        return false;
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return false;
    }
    tokens_match(credential.trim().as_bytes(), expected.as_bytes())
}

/// Resolve the effective token from the flag and the environment (env wins).
///
/// Mirrors `mcp::serve::resolve_auth_token`: a set-but-empty env var is an
/// error rather than a silent fallback to the flag.
fn resolve_auth_token(flag: Option<AuthToken>) -> Result<Option<AuthToken>, CliError> {
    match std::env::var(AUTH_TOKEN_ENV).ok() {
        Some(raw) => AuthToken::new(raw)
            .map(Some)
            .map_err(|e| CliError::Usage(format!("{AUTH_TOKEN_ENV}: {e}"))),
        None => Ok(flag),
    }
}

/// Fail closed when a non-loopback bind has no token.
///
/// Mirrors `mcp::serve::authorize_bind`. serve-web is a *reader* — it never
/// takes the writer lease, so exposure is read-only — but the whole session
/// is still readable, so a token-less bind to the world is not a configuration
/// worth starting.
fn authorize_bind_web(bind: IpAddr, token: Option<&AuthToken>) -> Result<(), CliError> {
    if bind.is_loopback() || token.is_some() {
        return Ok(());
    }
    Err(CliError::Usage(format!(
        "refusing to start: --bind {bind} exposes an unauthenticated read-only session beyond \
         loopback. Set {AUTH_TOKEN_ENV} (or pass --auth-token) to require \
         'Authorization: Bearer <token>' on every request, or bind 127.0.0.1 and reach it \
         through a tunnel or an authenticating proxy."
    )))
}

// ---------------------------------------------------------------------------
// State
// ---------------------------------------------------------------------------

/// When this reader last saw the durable snapshot *change*.
struct Freshness {
    fingerprint: u64,
    observed_at: Instant,
}

struct AppState {
    session: SessionId,
    backends: ResolvedBackends,
    /// True when `--bind` reaches beyond loopback. A non-loopback bind always
    /// carries a token (see [`authorize_bind_web`]).
    exposed: bool,
    /// Optional bearer token. When set, every route requires it.
    auth: Option<AuthToken>,
    freshness: Mutex<Freshness>,
}

impl AppState {
    fn store(&self) -> &dyn GraphStore {
        self.backends.store.as_ref()
    }

    /// Record the durable state's count fingerprint; return how long the
    /// current one has been standing.
    ///
    /// Counts only: two different graphs with identical counts read as
    /// "unchanged". That is the right trade for a freshness indicator — it is a
    /// hint about writer activity, not a consistency claim.
    fn observe(&self, fingerprint: u64) -> Duration {
        let mut f = self.freshness.lock();
        if f.fingerprint != fingerprint {
            f.fingerprint = fingerprint;
            f.observed_at = Instant::now();
        }
        f.observed_at.elapsed()
    }
}

// ---------------------------------------------------------------------------
// Wire types
// ---------------------------------------------------------------------------

/// Session identity and backend *kinds*.
///
/// Deliberately kind-only: `StoreConfig::dsn`, `StoreConfig::path` and
/// `EmbedderConfig::llama_url` are credentials or internal topology and never
/// appear here. `StoreConfig`'s own `Debug` redacts the DSN for the same reason.
#[derive(Debug, Serialize)]
struct SessionInfo {
    session: String,
    store: String,
    embedder: String,
    embedding_dim: usize,
    vector_search: bool,
    /// Always `"reader"` — this process holds no writer lease.
    mode: &'static str,
    /// Always `true`. The router registers `GET` routes only.
    read_only: bool,
    /// The in-RAM store is per-process: a reader cannot see another process's
    /// writes through it. Surfaced so the page can say so instead of looking broken.
    store_is_process_local: bool,
    /// `--bind` reaches beyond loopback. Such a bind always requires a bearer
    /// token; the page can only reach this surface through an authenticated
    /// proxy or a client that sends the token.
    exposed_beyond_loopback: bool,
    poll_interval_ms: u64,
    version: &'static str,
}

/// One canonization transition, as the writer durably recorded it.
#[derive(Debug, Serialize)]
struct WebEvent {
    /// Position in the session's ordered event list — the poll cursor.
    seq: usize,
    occurred_at: String,
    node_id: String,
    /// `None` when the concept is no longer in the snapshot (GC'd since).
    content: Option<String>,
    from_status: &'static str,
    to_status: &'static str,
    blast_radius: Option<i32>,
}

#[derive(Debug, Serialize)]
struct EventsPayload {
    /// Every transition recorded for the session.
    total: usize,
    /// The cursor this response answered.
    since: usize,
    events: Vec<WebEvent>,
}

#[derive(Debug, Serialize)]
struct WebStats {
    session: String,
    nodes: usize,
    edges: usize,
    concepts: usize,
    canonical: usize,
    canonization_events: usize,
    /// `Some` when a writer has published flush stats into the shared store
    /// (T85-3); `null` (rendered `n/a`) when no writer has yet, or the store
    /// doesn't support it — never a fabricated `0`.
    flush_lag_ms: Option<u64>,
    /// Same: writer-published log depth, or `null`/`n/a` when absent.
    log_depth: Option<usize>,
    /// How long the durable counts above have been unchanged, as seen here.
    durable_change_age_ms: u64,
    mode: &'static str,
    writer_only: &'static str,
}

#[derive(Debug, Serialize)]
struct Pulse {
    stats: WebStats,
    events: EventsPayload,
}

#[derive(Debug, Serialize)]
struct RecallResponse {
    session: String,
    query: String,
    /// The T5.3 context block **verbatim** — canonical markers, `⚑` warnings
    /// and conflict lines exactly as an agent would receive them.
    context: String,
    elapsed_ms: u64,
}

#[derive(Debug, Deserialize)]
struct RecallParams {
    q: Option<String>,
    top_k: Option<usize>,
    max_tokens: Option<usize>,
    traversal_depth: Option<usize>,
}

#[derive(Debug, Deserialize)]
struct SinceParams {
    since: Option<usize>,
}

const WRITER_ONLY: &str = "flush_lag / log_depth / daemon_cycles live in the writer process; \
                           this is a lease-free reader and cannot observe them";
// ---------------------------------------------------------------------------
// /api/inspect structure
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct InspectParams {
    focus: String,
    /// Accepted for CLI parity but deliberately ignored (treated as 1): the
    /// page needs hop 1 only, per the /api/inspect contract.
    #[allow(dead_code)]
    depth: Option<usize>,
}

/// One hop-1 structural neighbour of the focus — a thing the focus stands
/// behind, or a thing behind it. Structural edges only
/// (`Dependency`/`Causal`/`Hierarchical`), which is what keeps the false
/// `CoOccurrence` edge off the page (T7).
#[derive(Debug, Serialize)]
struct InspectDependent {
    content: String,
    concept_type: ConceptType,
    edge: String,
}

#[derive(Debug, Serialize)]
struct InspectResponse {
    focus: String,
    found: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    status: Option<&'static str>,
    blast_radius: u64,
    dependents: Vec<InspectDependent>,
    /// `true` when the dependents array hit the bound; always present (a miss
    /// already says so via `found: false`).
    truncated: bool,
    /// T11: which gates this concept meets, additive beside status/radius.
    #[serde(skip_serializing_if = "Option::is_none")]
    gate_progress: Option<GateProgress>,
}

impl InspectResponse {
    /// A miss is a 200 with `found: false` — never a non-2xx (the page says
    /// "nothing depends on this" without rendering an error).
    fn missing(focus: String) -> Self {
        Self {
            focus,
            found: false,
            status: None,
            blast_radius: 0,
            dependents: Vec::new(),
            truncated: false,
            gate_progress: None,
        }
    }
}

/// The structural edge types the page may show. Mirrors
/// `STRUCTURAL_EDGE_IN` in `src/store/sqlite.rs`: blast radius,
/// interaction span and this page all exclude `CoOccurrence`/`Semantic`.
fn is_structural(ty: EdgeType) -> bool {
    matches!(
        ty,
        EdgeType::Dependency | EdgeType::Causal | EdgeType::Hierarchical
    )
}

/// Hop-1 structural neighbours of `node`, bounded to [`MAX_INSPECT_NODES`]
/// with the bound reported rather than cut silently.
fn structural_dependents(g: &Graph, node: NodeId) -> (Vec<InspectDependent>, bool) {
    let mut deps: Vec<InspectDependent> = Vec::new();
    let mut seen: HashSet<NodeId> = HashSet::new();
    let mut truncated = false;
    // incident_edges returns id-ascending (deterministic); the first
    // structural edge naming a neighbour decides its edge label.
    for edge in g.incident_edges(node) {
        if !is_structural(edge.edge_type) {
            continue;
        }
        let other = if edge.source == node {
            edge.target
        } else {
            edge.source
        };
        if !seen.insert(other) {
            continue;
        }
        // Only a structural, unique Concept neighbour counts toward the bound:
        // a CoOccurrence/duplicate/interaction incident edge must not set
        // `truncated` when the structural list is actually complete.
        let Some(Node::Concept(c)) = g.node(other) else {
            continue;
        };
        if deps.len() >= MAX_INSPECT_NODES {
            truncated = true;
            break;
        }
        deps.push(InspectDependent {
            content: c.content.clone(),
            concept_type: c.concept_type,
            edge: format!("{:?}", edge.edge_type),
        });
    }
    (deps, truncated)
}

#[derive(Debug, Serialize)]
struct GraphEdge {
    parent: String,
    child: String,
    edge: String,
}

#[derive(Debug, Serialize)]
struct GraphNode {
    content: String,
    concept_type: ConceptType,
    status: &'static str,
    /// The live dependent count (same helper `/api/inspect` uses), so the
    /// tree marks load-bearing Candidates/Venerables — not just promoted
    /// Canonicals whose frozen `blast_radius` column happens to be `Some`.
    blast_radius: u64,
}

#[derive(Debug, Serialize)]
struct GraphResponse {
    session: String,
    nodes: Vec<GraphNode>,
    edges: Vec<GraphEdge>,
    truncated: bool,
}

fn structural_rank(ty: EdgeType) -> u8 {
    match ty {
        EdgeType::Causal => 0,
        EdgeType::Dependency => 1,
        EdgeType::Hierarchical => 2,
        _ => 3,
    }
}

// ---------------------------------------------------------------------------
// Reads
// ---------------------------------------------------------------------------

/// Raw snapshot for the event tail. A missing session is a first use — an
/// empty session, not an error (same rule as `store::load::load_session_async`).
async fn load_snapshot(
    store: &dyn GraphStore,
    session: &SessionId,
) -> Result<GraphSnapshot, CliError> {
    match store.load_session(session).await {
        Ok(snap) => Ok(snap),
        Err(StoreError::SessionNotFound(_)) => Ok(GraphSnapshot {
            session_id: session.clone(),
            ..GraphSnapshot::default()
        }),
        Err(e) => Err(CliError::Runtime(e.to_string())),
    }
}

fn status_str(s: CanonizationStatus) -> &'static str {
    match s {
        CanonizationStatus::None => "None",
        CanonizationStatus::Candidate => "Candidate",
        CanonizationStatus::Venerable => "Venerable",
        CanonizationStatus::Canonical => "Canonical",
    }
}

/// Canonization events at or after `since`, in a total order that does not
/// depend on which adapter produced them.
fn events_from(snap: &GraphSnapshot, since: usize) -> EventsPayload {
    let content: HashMap<NodeId, &str> = snap
        .concepts
        .iter()
        .map(|c| (c.id, c.content.as_str()))
        .collect();

    let mut ordered: Vec<&crate::types::CanonizationEvent> =
        snap.canonization_events.iter().collect();
    // SQLite orders by (occurred_at, id) on load and MemoryStore by insertion;
    // sorting here makes `seq` mean the same thing on every backend, which is
    // what lets the page use it as a cursor.
    ordered.sort_by(|a, b| a.occurred_at.cmp(&b.occurred_at).then(a.id.0.cmp(&b.id.0)));

    let total = ordered.len();
    let start = since.min(total);
    let events = ordered[start..]
        .iter()
        .enumerate()
        .map(|(offset, ev)| WebEvent {
            seq: start + offset,
            occurred_at: ev.occurred_at.to_rfc3339(),
            node_id: ev.node_id.0.to_string(),
            content: content.get(&ev.node_id).map(|s| (*s).to_string()),
            from_status: status_str(ev.from_status),
            to_status: status_str(ev.to_status),
            blast_radius: ev.blast_radius,
        })
        .collect();

    EventsPayload {
        total,
        since: start,
        events,
    }
}

fn stats_from(
    state: &AppState,
    g: &Graph,
    event_total: usize,
    flush: Option<SessionFlushStats>,
) -> WebStats {
    let concepts = g.concepts().count();
    let canonical = g
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .count();
    let nodes = g.node_count();
    let edges = g.edge_count();

    let mut fingerprint = 0u64;
    for part in [nodes, edges, concepts, canonical, event_total] {
        // FNV-1a over the counts: cheap, stable, and only ever compared to
        // itself (never persisted, never a key).
        fingerprint = (fingerprint ^ part as u64).wrapping_mul(0x100_0000_01b3);
    }

    // T85-3: a writer that has published flush stats into the shared store is
    // visible to this reader, so render the real numbers. When the store
    // returns `None` (no writer yet, or store doesn't support it) we keep the
    // honest `n/a` + `writer_only` tooltip — never a fabricated `0`.
    let (flush_lag_ms, log_depth) = match flush {
        Some(s) => (Some(s.flush_lag_ms), Some(s.log_depth as usize)),
        None => (None, None),
    };

    WebStats {
        session: state.session.as_str().to_string(),
        nodes,
        edges,
        concepts,
        canonical,
        canonization_events: event_total,
        flush_lag_ms,
        log_depth,
        durable_change_age_ms: state.observe(fingerprint).as_millis() as u64,
        mode: "reader",
        writer_only: WRITER_ONLY,
    }
}

async fn read_stats(state: &AppState, event_total: usize) -> Result<WebStats, CliError> {
    let loaded = load_reader_graph_with_contract(
        state.store(),
        state.session.as_str(),
        Some(&state.backends.embedding),
    )
    .await?;
    // T85-3: fetch the writer-published flush stats from the shared store when
    // available. A read failure degrades to `n/a` (None) rather than failing
    // the whole stats endpoint — the session/counts payload is the load-bearing
    // part, and a transient stats read must not take the page down.
    let flush = match state.store().read_flush_stats(&state.session).await {
        Ok(f) => f,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "read_flush_stats failed; reporting n/a for flush_lag/log_depth"
            );
            None
        }
    };
    // Scoped so the (`!Send`) read guard provably never spans an await.
    let stats = {
        let g = loaded.graph.read();
        stats_from(state, &g, event_total, flush)
    };
    Ok(stats)
}

async fn read_events(state: &AppState, since: usize) -> Result<EventsPayload, CliError> {
    let snap = load_snapshot(state.store(), &state.session).await?;
    Ok(events_from(&snap, since))
}

// ---------------------------------------------------------------------------
// Handlers — all GET, all read.
// ---------------------------------------------------------------------------

fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// JSON with `no-store`: session memory must never be served from a cache.
fn json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

fn fail(err: CliError) -> Response {
    let status = match &err {
        // A bad query string is the caller's fault; a store that will not
        // answer is upstream's.
        CliError::Usage(_) => StatusCode::BAD_REQUEST,
        CliError::Runtime(_) => StatusCode::BAD_GATEWAY,
    };
    json(status, serde_json::json!({ "error": err.to_string() }))
}

async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

async fn stylesheet() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
}

async fn script() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

/// Liveness for an ALB / ECS target group. Deliberately does **not** touch the
/// store: a slow database should degrade the page, not fail the health check
/// and take the task out of rotation.
async fn healthz() -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "ok").into_response()
}

async fn api_session(State(state): State<Arc<AppState>>) -> Response {
    json(
        StatusCode::OK,
        SessionInfo {
            session: state.session.as_str().to_string(),
            store: state.backends.store_cfg.kind.to_string(),
            embedder: state.backends.embedder_cfg.kind.to_string(),
            embedding_dim: state.backends.embedding.dim,
            vector_search: state
                .backends
                .store
                .capabilities()
                .contains(Capabilities::VECTOR_SEARCH),
            mode: "reader",
            read_only: true,
            store_is_process_local: state.backends.store_cfg.kind == StoreKind::Memory,
            exposed_beyond_loopback: state.exposed,
            poll_interval_ms: POLL_INTERVAL.as_millis() as u64,
            version: env!("CARGO_PKG_VERSION"),
        },
    )
}

async fn api_events(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SinceParams>,
) -> Response {
    match read_events(&state, params.since.unwrap_or(0)).await {
        Ok(payload) => json(StatusCode::OK, payload),
        Err(e) => fail(e),
    }
}

async fn api_stats(State(state): State<Arc<AppState>>) -> Response {
    // `usize::MAX` asks for the count without the rows.
    let total = match read_events(&state, usize::MAX).await {
        Ok(p) => p.total,
        Err(e) => return fail(e),
    };
    match read_stats(&state, total).await {
        Ok(stats) => json(StatusCode::OK, stats),
        Err(e) => fail(e),
    }
}

/// Stats + the event tail in one round trip — what the page actually polls.
async fn api_pulse(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SinceParams>,
) -> Response {
    let events = match read_events(&state, params.since.unwrap_or(0)).await {
        Ok(p) => p,
        Err(e) => return fail(e),
    };
    match read_stats(&state, events.total).await {
        Ok(stats) => json(StatusCode::OK, Pulse { stats, events }),
        Err(e) => fail(e),
    }
}

/// Recall, straight through [`crate::cli::recall::run`].
///
/// Reusing the CLI reader verbatim is the point: the page cannot show a
/// prettier or staler context block than the one an agent receives, because it
/// is running the same code with the same validators and the same caps.
async fn api_recall(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RecallParams>,
) -> Response {
    let query = params.q.unwrap_or_default();
    let started = Instant::now();
    let result = super::recall::run(
        &state.backends,
        state.session.as_str(),
        query.trim(),
        params.top_k,
        params.max_tokens,
        params.traversal_depth,
    )
    .await;

    match result {
        Ok(context) => json(
            StatusCode::OK,
            RecallResponse {
                session: state.session.as_str().to_string(),
                query,
                context,
                elapsed_ms: started.elapsed().as_millis() as u64,
            },
        ),
        Err(e) => fail(e),
    }
}
/// Who stands behind a focus, structurally — `/api/inspect`'s answer to
/// "what depends on this". Read-only: loads the graph as a reader and never
/// takes the writer lease. `depth` is accepted for CLI parity and treated as
/// 1 (the page needs hop 1 only).
async fn api_inspect(
    State(state): State<Arc<AppState>>,
    Query(params): Query<InspectParams>,
) -> Response {
    if params.focus.trim().is_empty() {
        // A blank focus is a miss, not an error — the page says "nothing
        // depends on this" without rendering an error (contract #2).
        return json(StatusCode::OK, InspectResponse::missing(params.focus));
    }
    let loaded = match load_reader_graph_with_contract(
        state.store(),
        state.session.as_str(),
        Some(&state.backends.embedding),
    )
    .await
    {
        Ok(l) => l,
        Err(e) => return fail(e),
    };
    // Scoped so the (`!Send`) read guard provably never spans the await for
    // the gate-progress query below.
    let found = {
        let g = loaded.graph.read();
        match resolve_focus(&g, params.focus.trim()) {
            Focus::Exact(id) | Focus::Fuzzy { id, .. } => match g.node(id) {
                Some(Node::Concept(c)) => {
                    let blast = blast_radii(&g).get(&id).copied().unwrap_or(0);
                    let (dependents, truncated) = structural_dependents(&g, id);
                    Some((c.clone(), blast, dependents, truncated))
                }
                _ => None,
            },
            // Ambiguous / missing / oversized all read as a miss (200, not
            // an error): there is no single canonical concept to describe.
            Focus::Ambiguous(_) | Focus::Missing | Focus::Oversized { .. } => None,
        }
    };
    let resp = match found {
        Some((concept, blast, dependents, truncated)) => {
            // T11: surface the concept's gate progress by re-running the
            // evaluation's own queries (`blast_radius` + `interaction_span`,
            // both with the eval's min_edge_age, plus the re-promotion
            // cooldown) against the store, with the concept's persisted
            // gc_survived. This is surfacing — the same numbers the eval
            // reaches — not a shadow calculation. A read failure degrades
            // this additive payload to null rather than failing the endpoint
            // the page loads on.
            let gate_progress = match gate_progress(
                state.store(),
                &state.session,
                &concept,
                state.backends.config.canonization_edge_min_age,
                state.backends.config.canonization_repromotion_cooldown,
                chrono::Utc::now(),
            )
            .await
            {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "gate_progress for /api/inspect failed; omitted");
                    None
                }
            };
            InspectResponse {
                focus: params.focus,
                found: true,
                status: Some(status_str(concept.canonization_status)),
                blast_radius: blast,
                dependents,
                truncated,
                gate_progress,
            }
        }
        None => InspectResponse::missing(params.focus),
    };
    json(StatusCode::OK, resp)
}

/// The session's structural skeleton, for the tree view. Read-only: no writer
/// lease. Ships only `Dependency`/`Causal`/`Hierarchical` edges — the false
/// `CoOccurrence` edge stays out of the visible claim.
async fn api_graph(State(state): State<Arc<AppState>>) -> Response {
    let loaded = match load_reader_graph_with_contract(
        state.store(),
        state.session.as_str(),
        Some(&state.backends.embedding),
    )
    .await
    {
        Ok(l) => l,
        Err(e) => return fail(e),
    };
    let (nodes, edges, truncated) = {
        let g = loaded.graph.read();
        // One in-memory pass for every node's dependent count, matching
        // /api/inspect's live semantics (not the frozen concepts-row column,
        // which is `None` until promotion), so the tree marks load-bearing
        // Candidates/Venerables and the two endpoints agree.
        let radii = blast_radii(&g);
        let mut nodes: Vec<GraphNode> = g
            .concepts()
            .map(|c| GraphNode {
                content: c.content.clone(),
                concept_type: c.concept_type,
                status: status_str(c.canonization_status),
                blast_radius: radii.get(&c.id).copied().unwrap_or(0),
            })
            .collect();
        nodes.sort_by(|a, b| {
            a.content
                .cmp(&b.content)
                .then_with(|| a.status.cmp(b.status))
        });
        let nodes_trunc = nodes.len() > MAX_GRAPH_NODES;
        nodes.truncate(MAX_GRAPH_NODES);

        // Structural edges only, both endpoints concepts, ordered like the
        // reference SQL so the payload is deterministic.
        let mut raw: Vec<(u8, String, String, String)> = g
            .edges()
            .filter(|e| is_structural(e.edge_type))
            .filter_map(|e| {
                let (Some(Node::Concept(s)), Some(Node::Concept(t))) =
                    (g.node(e.source), g.node(e.target))
                else {
                    return None;
                };
                Some((
                    structural_rank(e.edge_type),
                    s.content.clone(),
                    t.content.clone(),
                    format!("{:?}", e.edge_type),
                ))
            })
            .collect();
        raw.sort_by(|a, b| {
            a.0.cmp(&b.0)
                .then_with(|| a.1.cmp(&b.1))
                .then_with(|| a.2.cmp(&b.2))
        });
        let edges_trunc = raw.len() > MAX_GRAPH_EDGES;
        let edges: Vec<GraphEdge> = raw
            .into_iter()
            .take(MAX_GRAPH_EDGES)
            .map(|(_, parent, child, edge)| GraphEdge {
                parent,
                child,
                edge,
            })
            .collect();

        (nodes, edges, nodes_trunc || edges_trunc)
    };
    json(
        StatusCode::OK,
        GraphResponse {
            session: state.session.as_str().to_string(),
            nodes,
            edges,
            truncated,
        },
    )
}

// ---------------------------------------------------------------------------
// Router
// ---------------------------------------------------------------------------

/// Bearer gate applied when a token is configured.
///
/// When [`AppState::auth`] is `Some`, every request — static asset, health
/// check, or API — must carry `Authorization: Bearer <token>`. When it is
/// `None` (the loopback default) this is a pure pass-through, so a judge's
/// browser needs no credentials. Mirrors `mcp::serve`'s `guard_request`, minus
/// the transport-specific rate/session guards this read-only process does not
/// have.
async fn require_auth(
    axum::extract::State(state): axum::extract::State<Arc<AppState>>,
    req: axum::extract::Request,
    next: middleware::Next,
) -> Response {
    if let Some(expected) = &state.auth {
        let presented = req
            .headers()
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok());
        if !bearer_ok(presented, expected) {
            // Deliberately terse and identical for "no header" and "wrong
            // token": the difference is not the caller's business, and the
            // token itself is never echoed.
            return (
                StatusCode::UNAUTHORIZED,
                [(header::WWW_AUTHENTICATE, "Bearer")],
                "unauthorized: this endpoint requires 'Authorization: Bearer <token>'\n",
            )
                .into_response();
        }
    }
    next.run(req).await
}

/// Every route, `GET`-only.
///
/// Adding a mutating method here is what `read_only_router_has_no_mutating_route`
/// exists to catch: this server is read-only, so a write route is a stranger
/// with a pen. A new path must also be added to the tests' `ROUTES` list, which
/// `routes_constant_covers_every_registered_route` enforces. The `require_auth`
/// layer sits over the whole router and enforces the bearer token whenever one
/// is configured.
fn router(state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(index))
        .route("/app.css", get(stylesheet))
        .route("/app.js", get(script))
        .route("/healthz", get(healthz))
        .route("/api/session", get(api_session))
        .route("/api/inspect", get(api_inspect))
        .route("/api/graph", get(api_graph))
        .route("/api/recall", get(api_recall))
        .route("/api/events", get(api_events))
        .route("/api/stats", get(api_stats))
        .route("/api/pulse", get(api_pulse))
        .layer(middleware::from_fn_with_state(state.clone(), require_auth))
        .with_state(state)
}

// ---------------------------------------------------------------------------
// Process
// ---------------------------------------------------------------------------

/// SIGINT / SIGTERM, registered **eagerly** so a signal arriving during startup
/// is not missed (same discipline as `lambo serve`).
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let int = signal(SignalKind::interrupt());
        let term = signal(SignalKind::terminate());
        async move {
            match (int, term) {
                (Ok(mut int), Ok(mut term)) => {
                    tokio::select! {
                        _ = int.recv() => {}
                        _ = term.recv() => {}
                    }
                }
                (Ok(mut int), Err(_)) => {
                    let _ = int.recv().await;
                }
                (Err(_), Ok(mut term)) => {
                    let _ = term.recv().await;
                }
                (Err(_), Err(_)) => {
                    let _ = tokio::signal::ctrl_c().await;
                }
            }
        }
    }
    #[cfg(not(unix))]
    {
        async {
            let _ = tokio::signal::ctrl_c().await;
        }
    }
}

/// Serve the read-only session window until SIGINT / SIGTERM.
pub async fn run(backends: ResolvedBackends, args: Args) -> Result<String, CliError> {
    require_nonempty("session", &args.session)?;
    check_size_cli("session", &args.session)?;

    // Env beats flag (mirrors `mcp::serve`). A set-but-empty LAMBO_AUTH_TOKEN
    // is a usage error, not a silent fallback to the flag.
    let auth = match resolve_auth_token(args.auth_token) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("lambo serve-web: {e}");
            return Err(e);
        }
    };
    // Fail closed: a non-loopback bind with no token is a config error, not a
    // warning (same posture as `mcp::serve::authorize_bind`).
    authorize_bind_web(args.bind, auth.as_ref())?;

    // T1 part 2 #1 / T1b-R1-5: fail fast at startup on an embedder-contract
    // mismatch instead of serving a half-broken page with 502s on
    // /api/stats, /api/pulse and /api/recall. The read-only load checks the
    // session's stored contract against the live embedder; a fresh or absent
    // session has no stored contract and loads fine, so this only refuses on a
    // genuine mismatch — the loud, singular failure naming the mismatched
    // model (kind/model/dim). Read-only: no writer, no lease, nothing stamped.
    // This is intentionally all-or-nothing: a contract mismatch means the
    // session's stored vectors are unusable, so serve-web refuses to start
    // rather than serving only the structural, embedder-free surfaces. If a
    // structural-only view of a mismatched session is ever needed, gate only
    // the stats/pulse/recall endpoints instead.
    // The one-time load here is deliberate redundancy: it fails before the
    // server binds, so a mismatch is caught with a single loud startup error
    // even though the first request would reload the session.
    if let Err(e) = load_reader_graph_with_contract(
        backends.store.as_ref(),
        &args.session,
        Some(&backends.embedding),
    )
    .await
    {
        eprintln!(
            "lambo serve-web: refusing to start — the live embedder does not match this \
             session's stored vectors:\n{e}"
        );
        return Err(e);
    }

    let exposed = !args.bind.is_loopback();
    let state = Arc::new(AppState {
        session: SessionId::new(args.session.as_str()),
        backends,
        exposed,
        auth,
        freshness: Mutex::new(Freshness {
            fingerprint: 0,
            observed_at: Instant::now(),
        }),
    });

    let addr = SocketAddr::new(args.bind, args.port);
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .map_err(|e| CliError::Runtime(format!("bind {addr}: {e}")))?;
    let local = listener
        .local_addr()
        .map_err(|e| CliError::Runtime(format!("local_addr: {e}")))?;

    println!(
        "lambo serve-web: read-only window on session '{}' at http://{local}/",
        args.session
    );
    println!("lambo serve-web: reader process — no writer lease, no write routes");
    // A non-loopback bind always carries a token (`authorize_bind_web`), so
    // the two branches below are exhaustive: token configured, or loopback.
    if state.auth.is_some() {
        eprintln!(
            "⚑ lambo serve-web: authentication is ON — every request must send \
             'Authorization: Bearer <token>' (from {AUTH_TOKEN_ENV} or --auth-token)."
        );
    } else {
        eprintln!(
            "⚑ lambo serve-web: bound to {} — no auth token configured, so the surface is \
             unauthenticated. Anyone who can reach this port can read the whole session; keep \
             it on a private network or behind an authenticating proxy.",
            args.bind
        );
    }
    if state.backends.store_cfg.kind == StoreKind::Memory {
        eprintln!(
            "⚑ lambo serve-web: the 'memory' store is process-local — this reader has its own \
             empty copy and cannot see another process's writes. Use sqlite or cockroach to \
             watch a live session."
        );
    }

    serve_bounded(listener, router(state), shutdown_signal(), SHUTDOWN_GRACE).await
}

/// `axum::serve` under a shutdown signal, with the grace window applied to the
/// **drain only**.
///
/// The bound belongs after the signal, never around the running server:
/// wrapping the whole server in the timeout kills it at the deadline whether or
/// not anyone asked it to stop — which is exactly what the first cut of this
/// function did, and what `the_grace_window_bounds_the_drain_not_the_server`
/// now fails on. `grace` is injectable so that test runs in milliseconds.
async fn serve_bounded(
    listener: tokio::net::TcpListener,
    app: Router,
    shutdown: impl std::future::Future<Output = ()>,
    grace: Duration,
) -> Result<String, CliError> {
    // axum's graceful shutdown takes a future; this oneshot is how the signal
    // arm below reaches it.
    let (graceful_tx, graceful_rx) = tokio::sync::oneshot::channel::<()>();
    let server = axum::serve(listener, app).with_graceful_shutdown(async move {
        let _ = graceful_rx.await;
    });
    // `WithGracefulShutdown` is `IntoFuture`, not `Future`.
    let mut running = std::pin::pin!(async move { server.await });

    tokio::select! {
        // If the server is already done, take that answer over a signal that
        // landed in the same poll.
        biased;
        r = &mut running => {
            return r
                .map(|()| STOPPED.to_string())
                .map_err(|e| CliError::Runtime(format!("serve: {e}")));
        }
        () = shutdown => {}
    }
    let _ = graceful_tx.send(());

    // A reader holds no writer lease and no un-flushed tail, so abandoning the
    // drain loses nothing; the bound only guarantees Ctrl-C actually exits.
    match tokio::time::timeout(grace, &mut running).await {
        Ok(Ok(())) => Ok(STOPPED.to_string()),
        Ok(Err(e)) => Err(CliError::Runtime(format!("serve: {e}"))),
        Err(_) => Ok(format!(
            "{STOPPED} (connections dropped at the grace deadline)"
        )),
    }
}

const STOPPED: &str = "lambo serve-web: stopped";

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(all(test, feature = "store-memory", feature = "embed-fixture"))]
mod tests {
    use super::*;
    use crate::cli::caps::ConceptKind;
    use crate::embed::{EmbedderConfig, EmbedderKind, FixtureEmbedder};
    use crate::store::{StoreConfig, StoreKind};
    use crate::types::{
        AgentId, CanonizationEvent, CanonizationStatus, Concept, ConceptType, Edge, EdgeType,
        EmbeddingContract, Interaction, Mutation, MutationBatch, Node, NodeId, Scored, SessionId,
    };
    use crate::MemoryStore;
    use async_trait::async_trait;
    use chrono::{DateTime, Utc};
    use std::net::{Ipv4Addr, SocketAddr};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

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
        ResolvedBackends {
            store: Box::new(Shared(store)),
            embedder: Box::new(FixtureEmbedder::new()),
            store_cfg: StoreConfig {
                kind: StoreKind::Memory,
                dsn: None,
                path: None,
            },
            embedder_cfg: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
            },
            embedding: EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            },
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
        Arc::new(AppState {
            session: SessionId::new(session),
            backends: backends_on(store),
            exposed: auth.is_some(),
            auth,
            freshness: Mutex::new(Freshness {
                fingerprint: 0,
                observed_at: Instant::now(),
            }),
        })
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
    /// exactly as the canonization task does.
    async fn promote(store: &Arc<MemoryStore>, session: &str, content: &str) {
        let sid = SessionId::new(session);
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
                    None,
                )
                .await
                .expect("record canonization");
        }
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

    /// A session of `n` plain concepts (and one interaction to root them),
    /// with no canonization — for driving `/api/graph` past its node bound.
    async fn seed_many_concepts(session: &str, n: usize) -> Arc<MemoryStore> {
        let store = Arc::new(MemoryStore::new());
        let sid = SessionId::new(session);
        let iid = NodeId::new();
        let now = Utc::now();
        let mut batch = MutationBatch::new();
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(Interaction {
                id: iid,
                session_id: sid.clone(),
                agent_id: AgentId::from("agent-a"),
                prompt_text: Some("root".to_string()),
                previous_id: None,
                created_at: now,
            }),
        });
        for i in 0..n {
            let cid = NodeId::new();
            batch.push(Mutation::UpsertNode {
                node: Node::Concept(concept(
                    sid.clone(),
                    cid,
                    iid,
                    &format!("concept {i:05}"),
                    now,
                )),
            });
            // §5.7: every concept must have a Derives edge from an interaction.
            batch.push(Mutation::UpsertEdge {
                edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
            });
        }
        store.flush(&batch, None).await.expect("seed many");
        store
    }

    /// A session of `concepts` plain concepts with a Dependency edge between
    /// *every increasing* pair (`source_i -> source_j` for `i < j`) — exactly
    /// `concepts * (concepts - 1) / 2` structural edges, all acyclic — for
    /// driving `/api/graph` past its edge bound without crossing the node
    /// bound. The `i < j` ordering keeps the structural edges a DAG (the graph
    /// builder rejects Dependency cycles), and every natural key
    /// (source, target, Dependency) is distinct, so MemoryStore (which dedupes
    /// edges by that triple) keeps them all.
    async fn seed_many_structural_edges(session: &str, concepts: usize) -> Arc<MemoryStore> {
        let store = Arc::new(MemoryStore::new());
        let sid = SessionId::new(session);
        let iid = NodeId::new();
        let now = Utc::now();
        let ids: Vec<NodeId> = (0..concepts).map(|_| NodeId::new()).collect();
        let mut batch = MutationBatch::new();
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(Interaction {
                id: iid,
                session_id: sid.clone(),
                agent_id: AgentId::from("agent-a"),
                prompt_text: Some("root".to_string()),
                previous_id: None,
                created_at: now,
            }),
        });
        for (i, &cid) in ids.iter().enumerate() {
            batch.push(Mutation::UpsertNode {
                node: Node::Concept(concept(
                    sid.clone(),
                    cid,
                    iid,
                    &format!("concept {i:05}"),
                    now,
                )),
            });
            // §5.7: every concept must have a Derives edge from an interaction.
            batch.push(Mutation::UpsertEdge {
                edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
            });
        }
        for (i, &src) in ids.iter().enumerate() {
            for &dst in ids.iter().skip(i + 1) {
                batch.push(Mutation::UpsertEdge {
                    edge: edge(
                        NodeId::new(),
                        sid.clone(),
                        src,
                        dst,
                        EdgeType::Dependency,
                        now,
                    ),
                });
            }
        }
        store
            .flush(&batch, None)
            .await
            .expect("seed many structural edges");
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

    /// Like [`request`], but with an optional `Authorization` header so the
    /// auth gate can be exercised.
    async fn request_authed(
        addr: SocketAddr,
        method: &str,
        path: &str,
        authorization: Option<&str>,
    ) -> HttpResponse {
        let mut sock = tokio::net::TcpStream::connect(addr).await.expect("connect");
        let auth = authorization
            .map(|a| format!("Authorization: {a}\r\n"))
            .unwrap_or_default();
        let req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {addr}\r\nAccept: application/json\r\n{auth}Connection: close\r\n\r\n"
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

    // ---- (a) the router serves the page and the JSON endpoints ----------

    #[tokio::test]
    async fn serves_the_page_and_its_embedded_assets() {
        let store = Arc::new(MemoryStore::new());
        let (addr, handle) = spawn(state_on(store, "t85-assets")).await;

        let page = request(addr, "GET", "/").await;
        assert_eq!(page.status, 200);
        assert!(
            page.headers
                .to_lowercase()
                .contains("content-type: text/html"),
            "{}",
            page.headers
        );
        assert!(page.body.contains("<title>Lambo"), "{}", page.body);
        assert!(
            page.body.contains("/app.css") && page.body.contains("/app.js"),
            "the page must reference the embedded assets, not a CDN"
        );

        let css = request(addr, "GET", "/app.css").await;
        assert_eq!(css.status, 200);
        assert!(css.body.contains("--bg:"), "stylesheet body: {}", css.body);

        let js = request(addr, "GET", "/app.js").await;
        assert_eq!(js.status, 200);
        assert!(js.body.contains("/api/pulse"), "script body: {}", js.body);

        let health = request(addr, "GET", "/healthz").await;
        assert_eq!(health.status, 200);
        assert_eq!(health.body, "ok");

        handle.abort();
    }

    /// No asset may reach off-host: a stripped AWS task has no egress, and the
    /// demo URL must not depend on a third party being up.
    #[test]
    fn embedded_assets_reference_no_external_origin() {
        for (name, body) in [
            ("index.html", INDEX_HTML),
            ("app.css", APP_CSS),
            ("app.js", APP_JS),
        ] {
            for needle in ["http://", "https://", "//cdn", "@import url("] {
                assert!(
                    !body.contains(needle),
                    "{name} references an external origin ('{needle}') — assets must be self-contained"
                );
            }
        }
    }

    // ---- (b) endpoints against a seeded session ------------------------

    #[tokio::test]
    async fn recall_endpoint_returns_the_context_block_verbatim() {
        let store = seed("t85-recall").await;
        let state = state_on(store.clone(), "t85-recall");
        let (addr, handle) = spawn(state).await;

        let body = get_json(addr, "/api/recall?q=update%20user%20schema").await;
        let context = body["context"].as_str().expect("context string");

        // Same reader path the CLI runs — the page must not be able to show a
        // different answer than `lambo recall`.
        let expected = crate::cli::recall::run(
            &backends_on(store),
            "t85-recall",
            "update user schema",
            None,
            None,
            None,
        )
        .await
        .expect("cli recall");

        assert!(
            context.contains("user schema"),
            "recall must name the seeded concept: {context}"
        );
        assert!(
            context.contains(", canonical]"),
            "the canonical marker must survive to the page verbatim: {context}"
        );
        assert!(
            context.contains('⚑'),
            "the ⚑ blast-radius warning must survive to the page verbatim: {context}"
        );
        // Same hit set, same order. Scores carry a recency term that ticks
        // between the two calls, so the comparison is on the concept lines
        // rather than the floating-point suffix.
        let hits = |s: &str| -> Vec<String> {
            s.lines()
                .filter(|l| !l.trim().is_empty() && !l.starts_with('⚑') && !l.contains(" ago"))
                .filter_map(|l| l.split(" [").next().map(|c| c.trim().to_string()))
                .filter(|c| !c.is_empty())
                .collect()
        };
        assert_eq!(
            hits(context),
            hits(&expected),
            "the page must render the CLI's context block, not a reformatted one\
             \npage:\n{context}\ncli:\n{expected}"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn recall_endpoint_rejects_a_missing_query_without_touching_the_store() {
        let store = seed("t85-recall-usage").await;
        let (addr, handle) = spawn(state_on(store, "t85-recall-usage")).await;

        let r = request(addr, "GET", "/api/recall").await;
        assert_eq!(r.status, 400, "{}", r.body);
        assert!(r.body.contains("error"), "{}", r.body);

        let blank = request(addr, "GET", "/api/recall?q=%20%20").await;
        assert_eq!(blank.status, 400, "{}", blank.body);

        handle.abort();
    }

    #[tokio::test]
    async fn events_endpoint_tails_the_canonization_feed() {
        let store = seed("t85-events").await;
        let (addr, handle) = spawn(state_on(store.clone(), "t85-events")).await;

        let all = get_json(addr, "/api/events").await;
        assert_eq!(all["total"], 3, "three audited hops were seeded: {all}");
        let events = all["events"].as_array().expect("events array");
        assert_eq!(events.len(), 3);
        assert_eq!(events[0]["from_status"], "None");
        assert_eq!(events[0]["to_status"], "Candidate");
        assert_eq!(events[2]["to_status"], "Canonical");
        assert_eq!(
            events[2]["content"], "user schema",
            "the feed must name the concept, not just its uuid: {all}"
        );
        assert_eq!(events[0]["seq"], 0);
        assert_eq!(events[2]["seq"], 2);

        // The cursor the page polls with: nothing new since the last read.
        let caught_up = get_json(addr, "/api/events?since=3").await;
        assert_eq!(caught_up["total"], 3);
        assert!(caught_up["events"].as_array().expect("array").is_empty());

        // A new transition appends, and only the new one comes back.
        promote(&store, "t85-events", "auth middleware").await;
        let tail = get_json(addr, "/api/events?since=3").await;
        assert_eq!(tail["total"], 6, "{tail}");
        let tail_events = tail["events"].as_array().expect("array");
        assert_eq!(tail_events.len(), 3);
        assert_eq!(tail_events[0]["content"], "auth middleware");
        assert_eq!(tail_events[0]["seq"], 3);

        handle.abort();
    }

    #[tokio::test]
    async fn stats_endpoint_counts_the_session_and_refuses_to_fake_writer_fields() {
        let store = seed("t85-stats").await;
        let (addr, handle) = spawn(state_on(store, "t85-stats")).await;

        let stats = get_json(addr, "/api/stats").await;
        assert!(stats["nodes"].as_u64().expect("nodes") >= 3, "{stats}");
        assert!(stats["edges"].as_u64().expect("edges") >= 1, "{stats}");
        assert_eq!(stats["concepts"], 3, "{stats}"); // user schema, auth middleware, create user
        assert_eq!(stats["canonical"], 1, "{stats}");
        assert_eq!(stats["canonization_events"], 3, "{stats}");
        assert_eq!(stats["mode"], "reader");

        // The load-bearing honesty: a reader must say n/a, never 0.
        assert!(
            stats["flush_lag_ms"].is_null(),
            "a reader cannot observe flush lag; reporting a number would be a lie: {stats}"
        );
        assert!(stats["log_depth"].is_null(), "{stats}");
        assert!(
            stats["writer_only"]
                .as_str()
                .expect("writer_only")
                .contains("reader"),
            "{stats}"
        );

        handle.abort();
    }

    #[tokio::test]
    async fn stats_endpoint_renders_writer_published_flush_stats() {
        // T85-3: once a writer's FlushTask has published flush stats into the
        // shared store, a reader must render the real numbers (not n/a).
        let store = seed("t85-stats-live").await;
        let sid = SessionId::from("t85-stats-live");
        store
            .write_flush_stats(
                &sid,
                &crate::store::SessionFlushStats {
                    flush_lag_ms: 12,
                    log_depth: 3,
                },
            )
            .await
            .unwrap();
        let (addr, handle) = spawn(state_on(store, "t85-stats-live")).await;

        let stats = get_json(addr, "/api/stats").await;
        assert_eq!(stats["flush_lag_ms"], 12, "{stats}");
        assert_eq!(stats["log_depth"], 3, "{stats}");
        assert_eq!(stats["mode"], "reader");

        handle.abort();
    }

    #[tokio::test]
    async fn pulse_returns_stats_and_events_in_one_round_trip() {
        let store = seed("t85-pulse").await;
        let (addr, handle) = spawn(state_on(store, "t85-pulse")).await;

        let pulse = get_json(addr, "/api/pulse?since=0").await;
        assert_eq!(pulse["events"]["total"], 3, "{pulse}");
        assert_eq!(pulse["stats"]["canonical"], 1, "{pulse}");
        assert_eq!(
            pulse["stats"]["canonization_events"], pulse["events"]["total"],
            "stats and the feed must agree within one response: {pulse}"
        );

        handle.abort();
    }

    // ---- /api/inspect ---------------------------------------------------

    /// Focus on a real concept: the page gets its status, its blast radius,
    /// its structural dependents (never a `CoOccurrence` edge), and — the T11
    /// add-on — its canonization gate progress.
    #[tokio::test]
    async fn inspect_endpoint_reports_a_focus_and_its_structural_dependents() {
        let store = seed("t93-inspect").await;
        let (addr, handle) = spawn(state_on(store, "t93-inspect")).await;

        let hit = get_json(addr, "/api/inspect?focus=user%20schema").await;
        assert_eq!(hit["found"], true, "{hit}");
        assert_eq!(hit["status"], "Canonical", "{hit}");
        assert!(hit["blast_radius"].as_u64().is_some(), "{hit}");

        // Structural edges only: the false CoOccurrence edge (T7) must never
        // appear on the page.
        let deps = hit["dependents"].as_array().expect("dependents");
        for dep in deps {
            let edge = dep["edge"].as_str().expect("edge");
            assert!(
                matches!(edge, "Dependency" | "Causal" | "Hierarchical"),
                "non-structural edge '{edge}' leaked onto the inspect page: {hit}"
            );
            assert!(dep["content"].as_str().is_some(), "{hit}");
            assert!(dep["concept_type"].as_str().is_some(), "{hit}");
        }

        // T11: the payload explains the concept with its gate bars.
        let gp = &hit["gate_progress"];
        assert_eq!(gp["gc_survived"]["bar"], 3.0, "{hit}");
        assert_eq!(gp["blast_radius"]["bar"], 5.0, "{hit}");
        assert!(gp["blast_radius"]["strictly_above"] == true, "{hit}");
        assert_eq!(gp["distinct_interactions"]["bar"], 3.0, "{hit}");
        assert!(gp["coverage"]["bar"].as_f64().is_some(), "{hit}");

        handle.abort();
    }

    /// A miss is a `200` with `found: false` — never a non-2xx — and carries
    /// the empty dependents shape from the contract.
    #[tokio::test]
    async fn inspect_endpoint_miss_is_a_200_with_found_false() {
        let store = seed("t93-inspect-miss").await;
        let (addr, handle) = spawn(state_on(store, "t93-inspect-miss")).await;

        let r = request(addr, "GET", "/api/inspect?focus=no-such-concept").await;
        assert_eq!(
            r.status, 200,
            "a miss must be 200, never non-2xx: {}",
            r.body
        );
        let v: serde_json::Value = serde_json::from_str(&r.body).unwrap();
        assert_eq!(v["found"], false, "{v}");
        assert_eq!(v["blast_radius"], 0, "{v}");
        assert!(v["dependents"].as_array().unwrap().is_empty(), "{v}");
        // status / gate_progress are omitted on a miss (no concept to explain).
        assert!(v["status"].is_null(), "{v}");
        assert!(v["gate_progress"].is_null(), "{v}");

        // A blank focus is the same miss, not an error.
        let blank = request(addr, "GET", "/api/inspect?focus=").await;
        assert_eq!(blank.status, 200, "blank focus must be a 200 miss");

        handle.abort();
    }

    // ---- /api/graph -----------------------------------------------------

    /// The tree view ships the structural skeleton: concepts with their row
    /// status + blast radius, and structural edges only.
    #[tokio::test]
    async fn graph_endpoint_returns_the_structural_skeleton() {
        let store = seed("t93-graph").await;
        let (addr, handle) = spawn(state_on(store, "t93-graph")).await;

        let g = get_json(addr, "/api/graph").await;
        assert_eq!(g["session"], "t93-graph", "{g}");
        assert_eq!(g["truncated"], false, "{g}");

        let nodes = g["nodes"].as_array().expect("nodes");
        assert!(!nodes.is_empty(), "{g}");
        for n in nodes {
            assert!(n["content"].as_str().is_some(), "{g}");
            assert!(n["concept_type"].as_str().is_some(), "{g}");
            assert!(n["status"].as_str().is_some(), "{g}");
            assert!(n["blast_radius"].as_i64().is_some(), "{g}");
        }
        assert!(
            nodes
                .iter()
                .any(|n| n["content"] == "user schema" && n["status"] == "Canonical"),
            "the canonical concept with its row status must be a node: {g}"
        );

        let edges = g["edges"].as_array().expect("edges");
        for e in edges {
            let ty = e["edge"].as_str().expect("edge");
            assert!(
                matches!(ty, "Dependency" | "Causal" | "Hierarchical"),
                "non-structural edge '{ty}' in the tree: {g}"
            );
            assert!(e["parent"].as_str().is_some(), "{g}");
            assert!(e["child"].as_str().is_some(), "{g}");
        }

        handle.abort();
    }

    /// A non-canonical (here status `None`) node with dependents must report a
    /// nonzero `blast_radius` on BOTH `/api/graph` and `/api/inspect` — the
    /// live dependent count from the same helper, not the frozen
    /// concepts-row column (which is `None` until promotion), so the tree
    /// foregrounds load-bearing Candidates/Venerables (T3-R1-2).
    #[tokio::test]
    async fn graph_and_inspect_agree_on_a_live_blast_radius_for_non_canonical() {
        // The seed's `create user` action is never promoted (status `None`)
        // but stands behind a dependent, so format::blast_radii counts it.
        let store = seed("t93-graph-live").await;
        let (addr, handle) = spawn(state_on(store, "t93-graph-live")).await;

        let g = get_json(addr, "/api/graph").await;
        let pillar = g["nodes"]
            .as_array()
            .expect("nodes")
            .iter()
            .find(|n| n["content"] == "create user")
            .expect("create user node");
        assert_ne!(pillar["status"], "Canonical", "{g}");
        let graph_radius = pillar["blast_radius"].as_u64().expect("radius");
        assert!(
            graph_radius >= 1,
            "a non-canonical node with a dependent must report a nonzero live radius: {g}"
        );

        let hit = get_json(addr, "/api/inspect?focus=create%20user").await;
        assert_eq!(hit["found"], true, "{hit}");
        assert_eq!(
            hit["blast_radius"].as_u64().unwrap(),
            graph_radius,
            "/api/inspect and /api/graph must report the same live blast radius: {hit} {g}"
        );
        handle.abort();
    }

    /// Drive `/api/inspect` past `MAX_INSPECT_NODES` structural dependents:
    /// `truncated` is true and the payload pins at the cap. This also pins the
    /// T3-R1-3 fix — a non-structural/duplicate incident edge alone must not
    /// set `truncated` when the structural list is complete.
    #[tokio::test]
    async fn inspect_truncates_and_reports_at_the_dependents_bound() {
        let n = MAX_INSPECT_NODES + 1;
        let store = seed_chain_around("t93-inspect-cap", "hub", n).await;
        let (addr, handle) = spawn(state_on(store, "t93-inspect-cap")).await;

        let hit = get_json(addr, "/api/inspect?focus=hub").await;
        assert_eq!(hit["found"], true, "{hit}");
        assert_eq!(hit["truncated"], true, "{hit}");
        assert_eq!(
            hit["dependents"].as_array().expect("dependents").len(),
            MAX_INSPECT_NODES,
            "at the bound the payload must be pinned at the cap: {hit}"
        );
        handle.abort();
    }

    /// Drive `/api/graph` past `MAX_GRAPH_NODES` concepts: `truncated` is true
    /// and the payload pins at the cap (T3-R1-5).
    #[tokio::test]
    async fn graph_truncates_and_reports_at_the_nodes_bound() {
        let store = seed_many_concepts("t93-graph-cap", MAX_GRAPH_NODES + 1).await;
        let (addr, handle) = spawn(state_on(store, "t93-graph-cap")).await;

        let g = get_json(addr, "/api/graph").await;
        assert_eq!(g["truncated"], true, "{g}");
        assert_eq!(
            g["nodes"].as_array().expect("nodes").len(),
            MAX_GRAPH_NODES,
            "at the bound the payload must be pinned at the cap: {g}"
        );
        handle.abort();
    }

    /// Drive `/api/graph` past `MAX_GRAPH_EDGES` structural edges: `truncated`
    /// is true and the payload pins at the edge cap, while the node count
    /// stays under its own bound so the edge branch fires in isolation.
    #[tokio::test]
    async fn graph_truncates_and_reports_at_the_edges_bound() {
        // 182 concepts => 182 * 181 / 2 = 16471 Dependency edges >
        // MAX_GRAPH_EDGES (16384), but 182 concepts < MAX_GRAPH_NODES (4096)
        // keeps the node side untruncated. The i < j ordering keeps the
        // structural edges a DAG (the graph builder rejects cycles).
        let concepts = 182;
        let store = seed_many_structural_edges("t93-graph-edge-cap", concepts).await;
        let (addr, handle) = spawn(state_on(store, "t93-graph-edge-cap")).await;

        let g = get_json(addr, "/api/graph").await;
        assert_eq!(g["truncated"], true, "{g}");
        assert_eq!(
            g["edges"].as_array().expect("edges").len(),
            MAX_GRAPH_EDGES,
            "at the edge bound the payload must be pinned at the cap: {g}"
        );
        assert_eq!(
            g["nodes"].as_array().expect("nodes").len(),
            concepts,
            "nodes stay under their own bound and must not be cut: {g}"
        );
        handle.abort();
    }

    /// The `depth` query parameter is accepted for CLI parity but deliberately
    /// treated as 1: any depth value returns the same hop-1 shape, never
    /// rejected (T3-R1-N3).
    #[tokio::test]
    async fn inspect_ignores_the_depth_parameter() {
        let store = seed("t93-inspect-depth").await;
        let (addr, handle) = spawn(state_on(store, "t93-inspect-depth")).await;

        let d1 = get_json(addr, "/api/inspect?focus=user%20schema&depth=1").await;
        let d3 = get_json(addr, "/api/inspect?focus=user%20schema&depth=3").await;
        assert_eq!(d1["found"], true, "{d1}");
        assert_eq!(d3["found"], true, "{d3}");
        assert_eq!(
            d1["dependents"], d3["dependents"],
            "depth must be ignored (treated as 1), not change the shape: {d1} {d3}"
        );
        handle.abort();
    }

    /// A concept demoted within the re-promotion cooldown must surface
    /// `in_cooldown: true` + `cooldown_until` in its gate progress — the
    /// fifth (non-threshold) reason a Venerable that clears all four gates
    /// still does not promote (T3-R1-4).
    #[tokio::test]
    async fn inspect_surfaces_a_cooling_concepts_repromotion_cooldown() {
        let store = Arc::new(MemoryStore::new());
        let sid = SessionId::new("t93-cooldown");
        let iid = NodeId::new();
        let cid = NodeId::new();
        let now = Utc::now();
        let mut batch = MutationBatch::new();
        batch.push(Mutation::UpsertNode {
            node: Node::Interaction(Interaction {
                id: iid,
                session_id: sid.clone(),
                agent_id: AgentId::from("agent-a"),
                prompt_text: Some("hot".to_string()),
                previous_id: None,
                created_at: now,
            }),
        });
        // Demoted 5s ago: inside the default 300s cooldown.
        let mut c = concept(sid.clone(), cid, iid, "hot", now);
        c.last_demotion_time = Some(now - chrono::Duration::seconds(5));
        batch.push(Mutation::UpsertNode {
            node: Node::Concept(c),
        });
        // §5.7: every concept must have a Derives edge from an interaction.
        batch.push(Mutation::UpsertEdge {
            edge: edge(NodeId::new(), sid.clone(), iid, cid, EdgeType::Derives, now),
        });
        store.flush(&batch, None).await.expect("seed cooldown");
        let (addr, handle) = spawn(state_on(store, "t93-cooldown")).await;

        let hit = get_json(addr, "/api/inspect?focus=hot").await;
        assert_eq!(hit["found"], true, "{hit}");
        let gp = &hit["gate_progress"];
        assert_eq!(gp["in_cooldown"], true, "{hit}");
        assert!(
            gp["cooldown_until"].is_string(),
            "a cooling concept must carry cooldown_until: {hit}"
        );
        handle.abort();
    }

    #[tokio::test]
    async fn an_unwritten_session_is_an_empty_window_not_an_error() {
        let store = Arc::new(MemoryStore::new());
        let (addr, handle) = spawn(state_on(store, "t85-never-written")).await;

        let pulse = get_json(addr, "/api/pulse").await;
        assert_eq!(pulse["stats"]["nodes"], 0, "{pulse}");
        assert_eq!(pulse["events"]["total"], 0, "{pulse}");

        handle.abort();
    }

    // ---- (c) read-only guarantee ---------------------------------------

    #[tokio::test]
    async fn read_only_router_has_no_mutating_route() {
        let store = seed("t85-readonly").await;
        let (addr, handle) = spawn(state_on(store, "t85-readonly")).await;

        for path in ROUTES {
            for method in ["POST", "PUT", "PATCH", "DELETE"] {
                let r = request(addr, method, path).await;
                assert_eq!(
                    r.status, 405,
                    "{method} {path} must be Method Not Allowed — this is a read-only window \
                     and a mutating route here would be a stranger with a pen. Got {} / {}",
                    r.status, r.body
                );
            }
        }

        handle.abort();
    }

    /// Source-level backstop for the behavioural test above: catches a mutating
    /// route registered on a path that was also left out of [`ROUTES`].
    #[test]
    fn the_module_registers_only_get_routes() {
        let src = include_str!("serve_web.rs");
        let prod = src.split("#[cfg(all(test").next().unwrap_or(src);
        for banned in [
            "routing::post",
            "routing::put",
            "routing::patch",
            "routing::delete",
            "post(",
            "put(",
            "patch(",
            "delete(",
        ] {
            assert!(
                !prod.contains(banned),
                "serve_web registers '{banned}' — the demo app must stay read-only on every \
                 bind, token-protected or not"
            );
        }
        // A reader never opens a writer, never takes the lease, never spawns GC.
        for banned in [
            "Memory::builder",
            "open_writer",
            "acquire_lease",
            ".spawn()",
        ] {
            assert!(
                !prod.contains(banned),
                "serve_web contains '{banned}' — serve-web is a lease-free reader (spec §2.2)"
            );
        }
    }

    /// Every route the router answers is listed in [`ROUTES`], so the method
    /// sweep above cannot silently miss one.
    #[test]
    fn routes_constant_covers_every_registered_route() {
        let src = include_str!("serve_web.rs");
        let body = src
            .split("fn router(")
            .nth(1)
            .and_then(|s| s.split("\n}\n").next())
            .expect("router body");
        let registered: Vec<String> = body
            .match_indices(".route(\"")
            .filter_map(|(i, _)| {
                let rest = &body[i + ".route(\"".len()..];
                rest.find('"').map(|end| rest[..end].to_string())
            })
            .collect();
        assert!(!registered.is_empty(), "parsed no routes from the router");
        for path in &registered {
            assert!(
                ROUTES.contains(&path.as_str()),
                "route '{path}' is not in ROUTES — the read-only method sweep would skip it"
            );
        }
        assert_eq!(registered.len(), ROUTES.len(), "ROUTES has stale entries");
    }

    // ---- auth: fail-closed non-loopback bind (mirrors T8.7) -------------

    /// A non-loopback bind without a token is a startup refusal; loopback
    /// stays optional-auth. Mirrors `mcp::serve`'s `authorize_bind` test.
    #[test]
    fn authorize_bind_web_fails_closed_off_loopback() {
        let public: IpAddr = "0.0.0.0".parse().unwrap();
        let any_v6: IpAddr = "0:0:0:0:0:0:0:0".parse().unwrap();
        let lan: IpAddr = "192.168.1.10".parse().unwrap();
        let loopback: IpAddr = "127.0.0.1".parse().unwrap();
        let loopback_v6: IpAddr = "::1".parse().unwrap();
        let loopback_subnet: IpAddr = "127.9.9.9".parse().unwrap();

        for bind in [public, any_v6, lan] {
            let err =
                authorize_bind_web(bind, None).expect_err("{bind} without a token must not start");
            let msg = err.to_string();
            assert!(msg.contains("refusing to start"), "{msg}");

            let token = AuthToken::new("s3cret").expect("valid");
            authorize_bind_web(bind, Some(&token)).expect("a token satisfies the rule");
        }

        for bind in [loopback, loopback_v6, loopback_subnet] {
            authorize_bind_web(bind, None).expect("loopback stays optional-auth");
        }
    }

    /// AuthToken rejects empty/whitespace tokens, so a set-but-empty
    /// LAMBO_AUTH_TOKEN is a usage error rather than authenticate-everything.
    #[test]
    fn an_empty_auth_token_is_refused() {
        assert!(AuthToken::new("").is_err());
        assert!(AuthToken::new("   ").is_err());
        assert!(AuthToken::new("s3cret").is_ok());
    }

    /// The `Authorization` header is parsed strictly: scheme case-insensitive,
    /// credential exact.
    #[test]
    fn bearer_header_is_parsed_strictly() {
        let expected = AuthToken::new("s3cret").expect("valid");
        assert!(bearer_ok(Some("Bearer s3cret"), &expected));
        assert!(bearer_ok(Some("bearer s3cret"), &expected), "RFC 7235 §2.1");
        assert!(bearer_ok(Some("  Bearer s3cret  "), &expected));
        assert!(!bearer_ok(None, &expected), "a missing header is a refusal");
        assert!(!bearer_ok(Some("Bearer wrong"), &expected));
        assert!(!bearer_ok(Some("Basic s3cret"), &expected), "wrong scheme");
        assert!(!bearer_ok(Some("s3cret"), &expected), "no scheme at all");
        assert!(!bearer_ok(Some("Bearer"), &expected));
        assert!(!bearer_ok(Some(""), &expected));
    }

    /// The comparator performs a full scan over the secret's length on every
    /// call — there is no early return on a mismatch, and the loop count does
    /// not depend on the presented input's length. Boolean results alone
    /// cannot tell a short-circuited compare from a full scan, so this pins
    /// the full-scan *semantics* (every position is compared, a late-only
    /// difference is caught, exact input matches, and a length change is a
    /// refusal even when the shorter input is an exact prefix — the case a
    /// naive `zip`-style short-circuit would wrongly accept) and the fixed
    /// loop bound is a property of the code, guarded by `black_box`.
    #[test]
    fn tokens_match_scans_the_full_length_without_short_circuiting() {
        let token = b"s3cret".as_slice();

        // Exact match across the whole length -> true.
        assert!(tokens_match(b"s3cret", token), "correct token matches");

        // A difference at a single position is caught wherever it falls:
        // first, middle, or last. (These alone do not prove a full scan — a
        // short-circuit catches a first divergence at any depth too.)
        assert!(!tokens_match(b"x3cret", token), "first byte differs");
        assert!(!tokens_match(b"s3xret", token), "middle byte differs");
        assert!(!tokens_match(b"s3crex", token), "last byte differs");

        // The genuine non-short-circuit guard is the truncated-prefix refusal
        // below: a naive `zip`-style compare would stop at the common 4 bytes,
        // find them equal, and wrongly accept `"s3cr"`. Requiring `false` here
        // proves the accumulator folds the length difference and the scan
        // commits no early-return on the secret bytes.

        // A longer presented value whose prefix matches must refuse: the scan
        // and the length fold both see the trailing bytes.
        assert!(
            !tokens_match(b"s3cret-extra", token),
            "padded token refuses"
        );

        // A shorter presented value that is an exact prefix must refuse: a
        // naive full-scan-equal-length or zip-short-circuit implementation
        // would accept the common prefix and return true.
        assert!(!tokens_match(b"s3cr", token), "truncated prefix refuses");

        // Empty presented input is refused.
        assert!(!tokens_match(b"", token));
    }

    /// When a token is configured, every route — API, asset, healthz — refuses
    /// a request without it and serves one that carries it. This is the
    /// middleware wiring that the `authorize_bind_web` unit test only proves is
    /// *called*.
    #[tokio::test]
    async fn a_configured_token_is_required_on_every_route() {
        let store = seed("t85-auth").await;
        let (addr, handle) = spawn(state_with_auth(
            store,
            "t85-auth",
            Some(AuthToken::new("s3cret").unwrap()),
        ))
        .await;

        // No token and a wrong token are refused identically (terse 401).
        for path in ROUTES {
            let r = request_authed(addr, "GET", path, None).await;
            assert_eq!(r.status, 401, "GET {path} without a token must be 401");
        }
        let r = request_authed(addr, "GET", "/api/session", Some("Bearer wrong")).await;
        assert_eq!(r.status, 401, "a wrong token must be refused");

        // The correct bearer token is accepted on the data API and the page.
        let r = request_authed(addr, "GET", "/api/session", Some("Bearer s3cret")).await;
        assert_eq!(r.status, 200, "the correct token must be served");
        let info: serde_json::Value = serde_json::from_str(&r.body).expect("json");
        assert_eq!(info["read_only"], true);
        assert_eq!(info["exposed_beyond_loopback"], true);
        let r = request_authed(addr, "GET", "/", Some("Bearer s3cret")).await;
        assert_eq!(r.status, 200, "the correct token must serve the page");

        handle.abort();
    }

    // ---- no secrets in the page ----------------------------------------

    #[tokio::test]
    async fn session_info_never_leaks_the_dsn_path_or_embedder_url() {
        let store = Arc::new(MemoryStore::new());
        let mut backends = backends_on(store);
        backends.store_cfg.dsn = Some("postgresql://demo:hunter2@crdb.internal:26257/lambo".into());
        backends.store_cfg.path = Some("/var/lib/lambo/private.sqlite".into());
        backends.embedder_cfg.llama_url = Some("http://embed.internal:8080".into());

        let state = Arc::new(AppState {
            session: SessionId::new("t85-secrets"),
            backends,
            exposed: false,
            auth: None,
            freshness: Mutex::new(Freshness {
                fingerprint: 0,
                observed_at: Instant::now(),
            }),
        });
        let (addr, handle) = spawn(state).await;

        let raw = request(addr, "GET", "/api/session").await;
        assert_eq!(raw.status, 200);
        for secret in [
            "hunter2",
            "crdb.internal",
            "postgresql://",
            "/var/lib/lambo",
            "embed.internal",
        ] {
            assert!(
                !raw.body.contains(secret),
                "/api/session leaked '{secret}': {}",
                raw.body
            );
        }

        let info: serde_json::Value = serde_json::from_str(&raw.body).expect("json");
        assert_eq!(info["store"], "memory");
        assert_eq!(info["embedder"], "fixture");
        assert_eq!(info["read_only"], true);
        assert_eq!(info["mode"], "reader");

        handle.abort();
    }

    #[tokio::test]
    async fn api_responses_are_not_cacheable() {
        let store = seed("t85-cache").await;
        let (addr, handle) = spawn(state_on(store, "t85-cache")).await;

        for path in ["/api/session", "/api/stats", "/api/events", "/api/pulse"] {
            let r = request(addr, "GET", path).await;
            assert!(
                r.headers.to_lowercase().contains("cache-control: no-store"),
                "{path} must not be cacheable — it is session memory: {}",
                r.headers
            );
        }

        handle.abort();
    }

    // ---- shutdown -------------------------------------------------------

    /// The grace window bounds the **drain**, not the server.
    ///
    /// Regression: the first cut wrapped the whole `axum::serve` future in
    /// `timeout(SHUTDOWN_GRACE, ..)`, so the process served happily for five
    /// seconds and then exited 0 on its own — which is how it was caught, by
    /// running it. The unit tests could not see it because they call
    /// `axum::serve` directly.
    #[tokio::test]
    async fn the_grace_window_bounds_the_drain_not_the_server() {
        let store = Arc::new(MemoryStore::new());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind");
        let addr = listener.local_addr().expect("addr");
        let grace = Duration::from_millis(40);

        let server = tokio::spawn(async move {
            serve_bounded(
                listener,
                router(state_on(store, "t85-grace")),
                std::future::pending::<()>(), // no signal, ever
                grace,
            )
            .await
        });

        // Well past the grace window with no shutdown signal: still serving.
        tokio::time::sleep(grace * 6).await;
        assert!(!server.is_finished(), "the server exited without a signal");
        let alive = request(addr, "GET", "/healthz").await;
        assert_eq!(
            alive.status, 200,
            "the grace window must bound the post-signal drain, not the server's lifetime"
        );

        server.abort();
    }

    #[tokio::test]
    async fn a_shutdown_signal_stops_the_server_within_the_grace_window() {
        let store = Arc::new(MemoryStore::new());
        let listener = tokio::net::TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .expect("bind");

        let out = tokio::time::timeout(
            Duration::from_secs(5),
            serve_bounded(
                listener,
                router(state_on(store, "t85-signal")),
                std::future::ready(()), // signal already pending
                Duration::from_millis(50),
            ),
        )
        .await
        .expect("a signalled server must return, not hang")
        .expect("clean stop");
        assert!(out.starts_with(STOPPED), "{out}");
    }

    // ---- freshness ------------------------------------------------------

    #[test]
    fn durable_change_age_resets_only_when_the_counts_move() {
        let state = state_on(Arc::new(MemoryStore::new()), "t85-freshness");
        let first = state.observe(7);
        std::thread::sleep(Duration::from_millis(12));
        let same = state.observe(7);
        assert!(
            same > first,
            "an unchanged snapshot must keep ageing: {first:?} -> {same:?}"
        );
        let moved = state.observe(8);
        assert!(
            moved < same,
            "a changed snapshot must reset the age: {same:?} -> {moved:?}"
        );
    }
}
