//! Every route, `GET`-only, and its handler. [`router`] is the whole
//! surface; `ROUTES` in the tests must list every path it registers.

use std::sync::Arc;
use std::time::Instant;

use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::middleware;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use serde::Serialize;

use super::auth::require_auth;
use super::dto::{
    GraphEdge, GraphNode, GraphResponse, InspectParams, InspectResponse, Pulse, RecallParams,
    RecallResponse, SessionInfo, SinceParams,
};
use super::projections::{
    is_structural, read_feed_and_stats, status_str, structural_dependents, structural_rank,
};
use super::state::AppState;
use super::views::RECALL_PERMIT_WAIT;
use super::{APP_CSS, APP_JS, INDEX_HTML, POLL_INTERVAL};
use crate::canon::gate_progress;
use crate::cli::caps::CliError;
use crate::cli::recall::RecallRequest;
use crate::recall::format::blast_radii;
use crate::store::{Capabilities, StoreKind};
use crate::surface::focus::{resolve_focus, Focus};
use crate::types::{CanonizationStatus, Node};

/// Upper bound on the concepts `/api/graph` returns. The tree view marks
/// load-bearing nodes, and the exhibit stays well under it; the cap exists so
/// a pathological session cannot balloon the payload. When it is hit the
/// handler says so with `"truncated": true` rather than cutting silently.
pub(super) const MAX_GRAPH_NODES: usize = 4_096;

/// Upper bound on the structural edges `/api/graph` returns. Same rationale
/// as [`MAX_GRAPH_NODES`], and surfaced the same way.
pub(super) const MAX_GRAPH_EDGES: usize = 16_384;

pub(super) fn asset(content_type: &'static str, body: &'static str) -> Response {
    ([(header::CONTENT_TYPE, content_type)], body).into_response()
}

/// JSON with `no-store`: session memory must never be served from a cache.
pub(super) fn json<T: Serialize>(status: StatusCode, body: T) -> Response {
    (status, [(header::CACHE_CONTROL, "no-store")], Json(body)).into_response()
}

/// The answer to a recall that found every [`ViewBounds::recall_concurrency`]
/// permit taken for [`RECALL_PERMIT_WAIT`]: 503, `Retry-After: 1`, no-store.
///
/// [`ViewBounds::recall_concurrency`]: super::views::ViewBounds::recall_concurrency
pub(super) fn recall_busy() -> Response {
    let mut response = json(
        StatusCode::SERVICE_UNAVAILABLE,
        serde_json::json!({
            "error": format!(
                "recall: every recall slot stayed busy for {} s; retry shortly",
                RECALL_PERMIT_WAIT.as_secs()
            )
        }),
    );
    response
        .headers_mut()
        .insert(header::RETRY_AFTER, header::HeaderValue::from_static("1"));
    response
}

pub(super) fn fail(err: CliError) -> Response {
    let status = match &err {
        // A bad query string is the caller's fault; a store that will not
        // answer is upstream's.
        CliError::Usage(_) => StatusCode::BAD_REQUEST,
        CliError::Runtime(_) => StatusCode::BAD_GATEWAY,
    };
    json(status, serde_json::json!({ "error": err.to_string() }))
}

pub(super) async fn index() -> Response {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

pub(super) async fn stylesheet() -> Response {
    asset("text/css; charset=utf-8", APP_CSS)
}

pub(super) async fn script() -> Response {
    asset("text/javascript; charset=utf-8", APP_JS)
}

/// Liveness for an ALB / ECS target group. Deliberately does **not** touch the
/// store: a slow database should degrade the page, not fail the health check
/// and take the task out of rotation.
pub(super) async fn healthz() -> Response {
    ([(header::CONTENT_TYPE, "text/plain; charset=utf-8")], "ok").into_response()
}

pub(super) async fn api_session(State(state): State<Arc<AppState>>) -> Response {
    let view = match state.view().await {
        Ok(view) => view,
        Err(err) => return fail(err),
    };
    let embedding_status = view.embedding.clone();
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

pub(super) async fn api_events(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SinceParams>,
) -> Response {
    match state.view().await {
        Ok(view) => json(StatusCode::OK, view.events_since(params.since.unwrap_or(0))),
        Err(e) => fail(e),
    }
}

pub(super) async fn api_stats(State(state): State<Arc<AppState>>) -> Response {
    // `usize::MAX` asks for the count without the rows.
    match read_feed_and_stats(&state, usize::MAX).await {
        Ok((_, read)) => json(StatusCode::OK, read.stats),
        Err(e) => fail(e),
    }
}

/// Stats + the event tail in one round trip — what the page actually polls.
pub(super) async fn api_pulse(
    State(state): State<Arc<AppState>>,
    Query(params): Query<SinceParams>,
) -> Response {
    match read_feed_and_stats(&state, params.since.unwrap_or(0)).await {
        Ok((events, read)) => {
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

/// Recall, through [`crate::cli::recall::run_detailed_on`] on the session's
/// view.
///
/// Running the CLI reader's own pipeline is the point: the page cannot show a
/// prettier context block than the one an agent receives, because it is
/// running the same code with the same validators and the same caps; it can be
/// at most one view TTL older than a fresh `lambo recall`. H3: the payload's
/// `context` and its structured `hits` / `response_annotations` all come from
/// that ONE execution. The arguments are validated before the view is
/// touched, so a bad query costs no store call.
pub(super) async fn api_recall(
    State(state): State<Arc<AppState>>,
    Query(params): Query<RecallParams>,
) -> Response {
    let query = params.q.unwrap_or_default();
    let started = Instant::now();
    let request = match RecallRequest::validate(
        state.session.as_str(),
        query.trim(),
        params.top_k,
        params.max_tokens,
        params.traversal_depth,
    ) {
        Ok(request) => request,
        Err(e) => return fail(e),
    };
    // Bound concurrent recalls process-wide (design 5.3). Held until the
    // response is built.
    let Some(_permit) = state.views.recall_permit().await else {
        return recall_busy();
    };
    let result = match state.view().await {
        Ok(view) => super::recall::run_detailed_on(&state.backends, &view.reader, &request).await,
        Err(e) => Err(e),
    };

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
pub(super) async fn api_inspect(
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
    let view = match state.view().await {
        Ok(view) => view,
        Err(e) => return fail(e),
    };
    // Scoped so the (`!Send`) read guard provably never spans the await for
    // the gate-progress query below.
    let found = {
        let g = view.reader.graph.read();
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
pub(super) async fn api_graph(State(state): State<Arc<AppState>>) -> Response {
    let view = match state.view().await {
        Ok(view) => view,
        Err(e) => return fail(e),
    };
    let (nodes, edges, truncated) = {
        let g = view.reader.graph.read();
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

/// Every route, `GET`-only.
///
/// Adding a mutating method here is what `read_only_router_has_no_mutating_route`
/// exists to catch: this server is read-only, so a write route is a stranger
/// with a pen. A new path must also be added to the tests' `ROUTES` list, which
/// `routes_constant_covers_every_registered_route` enforces. The `require_auth`
/// layer sits over the whole router and enforces the bearer token whenever one
/// is configured.
pub(super) fn router(state: Arc<AppState>) -> Router {
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
