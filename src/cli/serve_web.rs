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
//! `cli::recall::run_detailed` outright (the H3 single-execution seam — CLI
//! string, `hits` and `response_annotations` come from one run), so the page
//! cannot drift from what the CLI and MCP surfaces return.
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
use super::load_reader_graph;
use crate::canon::{gate_progress, GateProgress, PromotionPolicy};
use crate::cli::inspect::{resolve_focus, Focus};
use crate::graph::Graph;
use crate::mcp::AUTH_TOKEN_ENV;
use crate::recall::format::blast_radii;
use crate::resolve::{
    embedding_mismatch_error, session_embedding_compatibility, ResolvedBackends,
    SessionEmbeddingCompatibility,
};
use crate::store::{Capabilities, GraphStore, SessionFlushStats, StoreKind};
use crate::types::{
    tie_break_by_key, CanonizationStatus, ConceptType, EdgeType, EmbeddingContract, GraphSnapshot,
    Node, NodeId, SessionId, StoreError,
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
/// Mirrors `mcp::serve::authorize_bind` — the rule, not its J2 section: a
/// reader takes no lease and binds no session endpoint, so the pre-lease
/// ordering argument that section restates has no counterpart here.
/// serve-web is a *reader* — it never
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

/// Session identity, backend kinds, and embedding-space compatibility.
///
/// Backend connectivity remains deliberately hidden: `StoreConfig::dsn`,
/// `StoreConfig::path` and `EmbedderConfig::llama_url` are credentials or
/// internal topology and never appear here. H1 intentionally includes the
/// stored and configured model identifiers because a mismatch warning that
/// cannot name the two spaces is not actionable. `StoreConfig`'s own `Debug`
/// redacts the DSN for the same reason.
#[derive(Clone, Debug, Serialize)]
struct SessionInfo {
    session: String,
    store: String,
    embedder: String,
    embedding_dim: usize,
    vector_search: bool,
    /// Stored-vs-configured embedding identity. A mismatch leaves structural
    /// routes available but makes vector recall fail closed.
    embedding_contract: EmbeddingStatus,
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

#[derive(Clone, Debug, Serialize)]
struct EmbeddingStatus {
    /// `unrecorded`, `compatible`, or `mismatch`.
    status: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    stored: Option<EmbeddingContract>,
    configured: EmbeddingContract,
    #[serde(skip_serializing_if = "Option::is_none")]
    message: Option<String>,
}

impl EmbeddingStatus {
    fn inspect(stored: Option<&EmbeddingContract>, configured: &EmbeddingContract) -> Self {
        match session_embedding_compatibility(stored, configured) {
            SessionEmbeddingCompatibility::Unrecorded => Self {
                status: "unrecorded",
                stored: None,
                configured: configured.clone(),
                message: None,
            },
            SessionEmbeddingCompatibility::Compatible => Self {
                status: "compatible",
                stored: stored.cloned(),
                configured: configured.clone(),
                message: None,
            },
            SessionEmbeddingCompatibility::Mismatch { stored, live } => Self {
                status: "mismatch",
                message: Some(embedding_mismatch_error(&stored, &live).to_string()),
                stored: Some(stored),
                configured: live,
            },
        }
    }
    /// E2E-8: only a `compatible` contract means the vector leg can actually
    /// return candidates. `unrecorded` (legacy) sessions had their vectors
    /// quarantined at load and the checked read returns an empty pool for an
    /// unstamped durable contract — the flag must not say the leg is on when
    /// it returns nothing. `mismatch` is refused by the checked read. The
    /// `status` field itself keeps its `unrecorded|compatible|mismatch`
    /// semantics (H1's banner logic depends on it); only this derived flag
    /// tightens.
    fn vector_search_trusted(&self) -> bool {
        self.status == "compatible"
    }
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
    embedding_contract: EmbeddingStatus,
    vector_search: bool,
}

struct StatsRead {
    stats: WebStats,
    embedding_status: EmbeddingStatus,
}

#[derive(Debug, Serialize)]
struct RecallResponse {
    session: String,
    query: String,
    /// The T5.3 context block **verbatim** — canonical markers, `⚑` warnings
    /// and conflict lines exactly as an agent would receive them. Byte-equal
    /// to `lambo recall` for the same execution (H3: both project from the
    /// same `run_detailed` call).
    context: String,
    elapsed_ms: u64,
    /// H3: every ranked hit, with full status, `included_in_context` and the
    /// hit's typed annotations.
    hits: Vec<crate::recall::detail::DetailedHit>,
    /// H3: response-global explanations (`traversal`, `vector_degraded`) in
    /// producer order.
    response_annotations: Vec<crate::recall::detail::Annotation>,
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
    /// C2: the promotion policy **this reader process resolved**, always
    /// present, including on a miss (it is a property of the process, not of
    /// the concept).
    ///
    /// It is here because `serve-web` is a lease-free reader that runs its own
    /// `resolve_for_command`, and there is no channel — no lease-row column, no
    /// session-endpoint field — carrying the writer's policy to it. So a
    /// `lambo serve` started from a systemd unit with
    /// `Environment=LAMBO_PROMOTION_POLICY=Solo` and a `lambo serve-web` opened
    /// by hand in a shell that does not export it are two processes with two
    /// answers, and the page would silently render the reader's. Naming the
    /// resolution the numbers came from is what makes that mismatch *visible*
    /// instead of a wrong gate count; closing it needs the two processes to
    /// resolve the same config, which is an operator requirement the docs state
    /// rather than something this payload can enforce.
    promotion_policy: PromotionPolicy,
    /// T11: how close this concept is to canonization under
    /// `promotion_policy`, additive beside status/radius.
    ///
    /// Absent in exactly two situations, and `gate_progress_omitted` says which
    /// — a client must never have to guess. Under `Solo` it is **present**,
    /// carrying the policy label and the cooldown with swarm's four gates left
    /// out (see [`GateProgress`]); an absent block is never a policy statement.
    #[serde(skip_serializing_if = "Option::is_none")]
    gate_progress: Option<GateProgress>,
    /// Why `gate_progress` is absent, when it is and the focus was found.
    ///
    /// `"already_canonical"` — H2: a promoted fact has no promotion gates left
    /// to explain, and the block's aged-basis figures contradicted the live
    /// status/blast_radius. `"unavailable"` — the store read behind the gates
    /// failed; this additive payload degrades to a labelled absence rather than
    /// failing the endpoint the page loads on.
    ///
    /// The label exists because those two used to be one indistinguishable
    /// null, and the page rendered "no gates" identically for a canonized
    /// concept and for a broken store query.
    #[serde(skip_serializing_if = "Option::is_none")]
    gate_progress_omitted: Option<&'static str>,
}

impl InspectResponse {
    /// A miss is a 200 with `found: false` — never a non-2xx (the page says
    /// "nothing depends on this" without rendering an error).
    ///
    /// `gate_progress_omitted` stays `None` here: `found: false` already
    /// explains the absence, and there is no concept whose gates went missing.
    fn missing(focus: String, promotion_policy: PromotionPolicy) -> Self {
        Self {
            focus,
            found: false,
            status: None,
            blast_radius: 0,
            dependents: Vec::new(),
            truncated: false,
            promotion_policy,
            gate_progress: None,
            gate_progress_omitted: None,
        }
    }
}

/// The structural edge types the page may show. Mirrors
/// `STRUCTURAL_EDGE_IN` in `src/store/sqlite/structural.rs`: blast radius,
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

/// Canonization events at or after `since`, in a total order that depends
/// neither on which adapter produced them nor on which ids a run minted:
/// same-instant events are routine (one eval cycle stamps the cycle's `now`
/// on every event it emits — a Stage-3 batch or a multi-demotion cycle), and
/// they order by the moved concept's canonical key, then the event id
/// (issue #2, remediation round 3 — the bare event id was run-minted, which
/// made `seq` and the cursor built on it per-run arbitrary). The id residual
/// remains only for events whose node is absent from this snapshot.
fn events_from(snap: &GraphSnapshot, since: usize) -> EventsPayload {
    let content: HashMap<NodeId, &str> = snap
        .concepts
        .iter()
        .map(|c| (c.id, c.content.as_str()))
        .collect();
    let key_of: HashMap<NodeId, &str> = snap
        .concepts
        .iter()
        .map(|c| (c.id, c.canonical_key.as_str()))
        .collect();

    let mut ordered: Vec<&crate::types::CanonizationEvent> =
        snap.canonization_events.iter().collect();
    // SQLite orders by (occurred_at, id) on load and MemoryStore by insertion;
    // sorting here makes `seq` mean the same thing on every backend, which is
    // what lets the page use it as a cursor. The lookup only runs on exact
    // occurred_at ties.
    ordered.sort_by(|a, b| {
        a.occurred_at.cmp(&b.occurred_at).then_with(|| {
            tie_break_by_key(
                key_of.get(&a.node_id).copied(),
                &a.node_id,
                key_of.get(&b.node_id).copied(),
                &b.node_id,
            )
        })
    });

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

async fn read_stats(state: &AppState, event_total: usize) -> Result<StatsRead, CliError> {
    let loaded = load_reader_graph(state.store(), state.session.as_str()).await?;
    let embedding_status = {
        let g = loaded.graph.read();
        EmbeddingStatus::inspect(g.embedding(), &state.backends.embedding)
    };
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
    Ok(StatsRead {
        stats,
        embedding_status,
    })
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
    let loaded = match load_reader_graph(state.store(), state.session.as_str()).await {
        Ok(loaded) => loaded,
        Err(err) => return fail(err),
    };
    let embedding_status = {
        let graph = loaded.graph.read();
        EmbeddingStatus::inspect(graph.embedding(), &state.backends.embedding)
    };
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
                .contains(Capabilities::VECTOR_SEARCH)
                && embedding_status.vector_search_trusted(),
            embedding_contract: embedding_status,
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
        Ok(read) => json(StatusCode::OK, read.stats),
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
        Ok(read) => {
            let vector_search = state
                .store()
                .capabilities()
                .contains(Capabilities::VECTOR_SEARCH)
                && read.embedding_status.vector_search_trusted();
            json(
                StatusCode::OK,
                Pulse {
                    stats: read.stats,
                    events,
                    embedding_contract: read.embedding_status,
                    vector_search,
                },
            )
        }
        Err(e) => fail(e),
    }
}

/// Recall, straight through [`crate::cli::recall::run_detailed`].
///
/// Reusing the CLI reader verbatim is the point: the page cannot show a
/// prettier or staler context block than the one an agent receives, because it
/// is running the same code with the same validators and the same caps. H3:
/// the payload's `context` and its structured `hits` /
/// `response_annotations` all come from that ONE execution.
async fn api_recall(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RecallParams>,
) -> Response {
    let query = params.q.unwrap_or_default();
    let started = Instant::now();
    let result = super::recall::run_detailed(
        &state.backends,
        state.session.as_str(),
        query.trim(),
        params.top_k,
        params.max_tokens,
        params.traversal_depth,
    )
    .await;

    match result {
        Ok(cli) => json(
            StatusCode::OK,
            RecallResponse {
                session: state.session.as_str().to_string(),
                query,
                context: cli.context,
                elapsed_ms: started.elapsed().as_millis() as u64,
                hits: cli.hits,
                response_annotations: cli.response_annotations,
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
        return json(
            StatusCode::OK,
            InspectResponse::missing(params.focus, state.backends.config.promotion_policy),
        );
    }
    let loaded = match load_reader_graph(state.store(), state.session.as_str()).await {
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
            Focus::Ambiguous { .. } | Focus::Missing { .. } | Focus::Oversized { .. } => None,
        }
    };
    let resp = match found {
        Some((concept, blast, dependents, truncated)) => {
            // T11 surfaces the concept's gate progress by re-running the
            // evaluation's own queries (`blast_radius` + `interaction_span`,
            // both with the eval's min_edge_age, plus the re-promotion
            // cooldown) against the store, with the concept's persisted
            // gc_survived. This is surfacing — the same numbers the eval
            // reaches — not a shadow calculation.
            //
            // H2: those gates deliberately read against connections older
            // than `canonization_edge_min_age`, so on a young session a
            // Canonical concept can read zero bars beside a live radius of
            // nine. Pairing a Canonical status with gate figures saying it
            // does not qualify was a self-contradicting payload; the fix is
            // to stop shipping the pairing. A Canonical concept has no
            // promotion left to explain, so skip the gate block entirely —
            // and with it the two store queries that exist only to build it.
            // Keyed on the concept's CURRENT status, not `last_demotion_time`
            // and not has-ever-been-Canonical: budget demotion resets status
            // to `None`, and a cooling concept's progress is genuinely
            // useful. A read failure still degrades this additive payload to
            // null rather than failing the endpoint the page loads on.
            //
            // C2: the live `promotion_policy` goes in too — both on the block
            // (so a gate count is attributable to a policy) and on the
            // response itself (so a miss and an omitted block still say which
            // policy this reader resolved). Under `Solo` the block is still
            // shipped: `gate_progress` leaves swarm's four gates out and keeps
            // the cooldown, which is policy-independent and, on the
            // score-admitted hop, the only reason a banded-through Venerable
            // still is not Canonical. Suppressing the whole block under `Solo`
            // would take that one true fact with it and leave a stall with no
            // account at all.
            //
            // Every absence is labelled. `gate_progress: null` used to mean
            // three unrelated things at once; it now means two, and
            // `gate_progress_omitted` names which.
            let (gate_progress, gate_progress_omitted) =
                if concept.canonization_status == CanonizationStatus::Canonical {
                    (None, Some("already_canonical"))
                } else {
                    match gate_progress(
                        state.store(),
                        &state.session,
                        &concept,
                        state.backends.config.promotion_policy,
                        state.backends.config.canonization_edge_min_age,
                        state.backends.config.canonization_repromotion_cooldown,
                        chrono::Utc::now(),
                    )
                    .await
                    {
                        Ok(p) => (Some(p), None),
                        Err(e) => {
                            tracing::warn!(
                                error = %e,
                                "gate_progress for /api/inspect failed; omitted"
                            );
                            (None, Some("unavailable"))
                        }
                    }
                };
            InspectResponse {
                focus: params.focus,
                found: true,
                status: Some(status_str(concept.canonization_status)),
                blast_radius: blast,
                dependents,
                truncated,
                promotion_policy: state.backends.config.promotion_policy,
                gate_progress,
                gate_progress_omitted,
            }
        }
        None => InspectResponse::missing(params.focus, state.backends.config.promotion_policy),
    };
    json(StatusCode::OK, resp)
}

/// The session's structural skeleton, for the tree view. Read-only: no writer
/// lease. Ships only `Dependency`/`Causal`/`Hierarchical` edges — the false
/// `CoOccurrence` edge stays out of the visible claim.
async fn api_graph(State(state): State<Arc<AppState>>) -> Response {
    let loaded = match load_reader_graph(state.store(), state.session.as_str()).await {
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

    // H1 reader policy: keep the structural portal available, but never let a
    // model mismatch look healthy. `/api/session` carries the stored and live
    // contracts plus a loud message, the page renders it as a banner, and the
    // recall route remains fail-closed through `cli::recall`. Structural
    // stats/graph/inspect deliberately load without an embedder contract.
    let startup = load_reader_graph(backends.store.as_ref(), &args.session).await?;
    let embedding_status = {
        let graph = startup.graph.read();
        EmbeddingStatus::inspect(graph.embedding(), &backends.embedding)
    };
    if embedding_status.status == "mismatch" {
        eprintln!(
            "lambo serve-web: WARNING — vector recall is disabled for this session: {}",
            embedding_status
                .message
                .as_deref()
                .unwrap_or("stored and configured embedding contracts differ")
        );
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
mod tests;
