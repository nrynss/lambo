//! Read projections: what the portal derives from the durable snapshot and
//! the reader graph. The hop-1 structural dependents and the structural edge
//! filter (`/api/inspect`, `/api/graph`), the ordered canonization feed and
//! its cursor (`/api/events`, `/api/pulse`), and the count-based stats with
//! their freshness fingerprint. Focus resolution itself is shared
//! (`crate::surface::focus`); these projections are the portal's own (#25).

use std::collections::{HashMap, HashSet};

use super::dto::{EventsPayload, InspectDependent, StatsRead, WebEvent, WebStats, WRITER_ONLY};
use super::state::AppState;
use super::views::{SessionView, ViewCounts};
use crate::cli::caps::{CliError, MAX_INSPECT_NODES};
use crate::graph::Graph;
use crate::store::SessionFlushStats;
use crate::types::{
    tie_break_by_key, CanonizationEvent, CanonizationStatus, Concept, EdgeType, Node, NodeId,
};

/// The structural edge types the page may show. Mirrors
/// `STRUCTURAL_EDGE_IN` in `src/store/sqlite/structural.rs`: blast radius,
/// interaction span and this page all exclude `CoOccurrence`/`Semantic`.
pub(super) fn is_structural(ty: EdgeType) -> bool {
    matches!(
        ty,
        EdgeType::Dependency | EdgeType::Causal | EdgeType::Hierarchical
    )
}

/// Hop-1 structural neighbours of `node`, bounded to [`MAX_INSPECT_NODES`]
/// with the bound reported rather than cut silently.
pub(super) fn structural_dependents(g: &Graph, node: NodeId) -> (Vec<InspectDependent>, bool) {
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

pub(super) fn structural_rank(ty: EdgeType) -> u8 {
    match ty {
        EdgeType::Causal => 0,
        EdgeType::Dependency => 1,
        EdgeType::Hierarchical => 2,
        _ => 3,
    }
}

pub(super) fn status_str(s: CanonizationStatus) -> &'static str {
    match s {
        CanonizationStatus::None => "None",
        CanonizationStatus::Candidate => "Candidate",
        CanonizationStatus::Venerable => "Venerable",
        CanonizationStatus::Canonical => "Canonical",
    }
}

/// The whole ordered canonization feed, `seq` from 0, for `events` naming
/// `concepts`: a total order that depends neither on which adapter produced
/// them nor on which ids a run minted. Same-instant events are routine (one
/// eval cycle stamps the cycle's `now` on every event it emits — a Stage-3
/// batch or a multi-demotion cycle), and they order by the moved concept's
/// canonical key, then the event id (issue #2, remediation round 3 — the bare
/// event id was run-minted, which made `seq` and the cursor built on it
/// per-run arbitrary). The id residual remains only for events whose node is
/// absent from `concepts`.
///
/// The portal builds it from a loaded graph, which keeps every concept and
/// canonization event of the snapshot it was built from, so the feed needs no
/// second load (#4 PR 1). [`slice_events`] pages it at the poll cursor.
pub(super) fn ordered_events<'a>(
    concepts: impl Iterator<Item = &'a Concept>,
    events: &[CanonizationEvent],
) -> Vec<WebEvent> {
    let mut content: HashMap<NodeId, &str> = HashMap::new();
    let mut key_of: HashMap<NodeId, &str> = HashMap::new();
    for c in concepts {
        content.insert(c.id, c.content.as_str());
        key_of.insert(c.id, c.canonical_key.as_str());
    }

    let mut ordered: Vec<&CanonizationEvent> = events.iter().collect();
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

    ordered
        .iter()
        .enumerate()
        .map(|(seq, ev)| WebEvent {
            seq,
            occurred_at: ev.occurred_at.to_rfc3339(),
            node_id: ev.node_id.0.to_string(),
            content: content.get(&ev.node_id).map(|s| (*s).to_string()),
            from_status: status_str(ev.from_status),
            to_status: status_str(ev.to_status),
            blast_radius: ev.blast_radius,
        })
        .collect()
}

/// The page of an ordered feed at or after the cursor `since`.
pub(super) fn slice_events(all: &[WebEvent], since: usize) -> EventsPayload {
    let total = all.len();
    let start = since.min(total);
    EventsPayload {
        total,
        since: start,
        events: all[start..].to_vec(),
    }
}

pub(super) fn stats_from(
    state: &AppState,
    view: &SessionView,
    flush: Option<SessionFlushStats>,
) -> WebStats {
    let ViewCounts {
        nodes,
        edges,
        concepts,
        canonical,
    } = view.counts;
    let event_total = view.event_total();

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

/// The event feed and the stats, from the session's current view (one load
/// per TTL, #4 PR 1).
///
/// The view carries the counts, the embedding contract and the feed; only the
/// writer-published flush stats are read per request, so `flush_lag_ms` is as
/// fresh as before (design section 7, #16).
pub(super) async fn read_feed_and_stats(
    state: &AppState,
    since: usize,
) -> Result<(EventsPayload, StatsRead), CliError> {
    let view = state.view().await?;
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
    Ok((
        view.events_since(since),
        StatsRead {
            stats: stats_from(state, &view, flush),
            embedding_status: view.embedding.clone(),
        },
    ))
}
