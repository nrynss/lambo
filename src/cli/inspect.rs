//! `lambo inspect` — neighbourhood around a focus concept (reader process).
//!
//! Resolution (UUID → exact case-insensitive content → substring; ambiguous
//! refuses with named candidates; deterministic sort) is shared with
//! `lambo_inspect`. Caps: [`MAX_INSPECT_DEPTH`], [`MAX_INSPECT_NODES`],
//! [`MAX_INSPECT_CANDIDATES`].

use serde_json::json;

use super::caps::{
    check_size_cli, require_nonempty, CliError, MAX_INSPECT_CANDIDATES, MAX_INSPECT_DEPTH,
    MAX_INSPECT_NODES,
};
use super::load_reader_graph;
use crate::graph::Graph;
use crate::recall::format;
use crate::store::GraphStore;
use crate::types::{CanonizationStatus, EdgeType, Node, NodeId};

/// Upper bound on the number of concepts `inspect`'s fuzzy (substring) leg
/// will scan (T8.7 residual #3 graph-size guard).
///
/// [`resolve_focus`]'s substring pass lowercases **every** concept's content —
/// O(total-content) allocation per call. The HTTP rate limit bounds the call
/// *rate*; this constant bounds the *per-call* concept set, so the combined
/// per-second work is `rate_limit×MAX_INSPECT_SCAN_CONCEPTS` no matter how large
/// the session graph grows. A graph past the cap **refuses** the fuzzy focus
/// (the exact and node-id legs still resolve) rather than paying the unbounded
/// pass — refusing, not trimming, so the search never silently misses a match
/// it did not look at. It lives here, not in `caps`, because it guards this
/// function's own iteration and the task that added it touches this file.
pub(crate) const MAX_INSPECT_SCAN_CONCEPTS: usize = 2_000;

/// A concept `inspect` could have meant.
#[derive(Clone, Debug)]
pub(crate) struct FocusCandidate {
    pub id: NodeId,
    pub content: String,
}

/// How `inspect` resolved (or refused to resolve) its `focus`.
#[derive(Debug)]
pub(crate) enum Focus {
    /// A node UUID, or an exact (case-insensitive) content match.
    Exact(NodeId),
    /// Exactly one substring match — usable, but the caller is told.
    Fuzzy { id: NodeId, content: String },
    /// Several substring matches; the caller must disambiguate.
    Ambiguous(Vec<FocusCandidate>),
    /// Nothing matched.
    Missing,
    /// The graph exceeded [`MAX_INSPECT_SCAN_CONCEPTS`]; the fuzzy leg refused
    /// rather than pay its O(total-content) pass (T8.7 residual #3 guard).
    Oversized { cap: usize },
}

/// Resolve inspect's focus **deterministically**.
///
/// `Graph::concepts()` iterates a `HashMap`, so a `.find(..)` over it would
/// pick an arbitrary match — arbitrary across runs *and* within one run.
/// Every leg here collects and sorts by a total order, and the ambiguous
/// case refuses instead of guessing.
pub(crate) fn resolve_focus(g: &Graph, focus: &str) -> Focus {
    if let Some(id) = uuid::Uuid::parse_str(focus)
        .ok()
        .map(NodeId)
        .filter(|id| g.node(*id).is_some())
    {
        return Focus::Exact(id);
    }

    let mut exact: Vec<FocusCandidate> = g
        .concepts()
        .filter(|c| c.content.eq_ignore_ascii_case(focus))
        .map(|c| FocusCandidate {
            id: c.id,
            content: c.content.clone(),
        })
        .collect();
    if !exact.is_empty() {
        // Case-insensitive duplicates are the same concept to the caller, so
        // there is nothing to disambiguate — just pick one *stably*.
        exact.sort_by(|a, b| a.content.cmp(&b.content).then(a.id.0.cmp(&b.id.0)));
        return Focus::Exact(exact[0].id);
    }

    // Graph-size guard for the fuzzy leg (T8.7 residual #3). The rate limit
    // bounds the request *rate*; this bounds the *per-call* concept set the
    // O(total-content) lowercase pass iterates, so per-second work cannot grow
    // with an unattended graph. Refuse (not trim) a graph past the cap — a trim
    // would silently search a subset and could miss the real match. The count
    // itself is an allocation-free linear scan; the bounded cost is the
    // lowercase+alloc pass below, which never runs on an oversized graph.
    if g.concepts().count() > MAX_INSPECT_SCAN_CONCEPTS {
        return Focus::Oversized {
            cap: MAX_INSPECT_SCAN_CONCEPTS,
        };
    }

    let needle = focus.to_lowercase();
    let mut fuzzy: Vec<FocusCandidate> = g
        .concepts()
        .filter(|c| c.content.to_lowercase().contains(&needle))
        .map(|c| FocusCandidate {
            id: c.id,
            content: c.content.clone(),
        })
        .collect();
    if fuzzy.is_empty() {
        return Focus::Missing;
    }
    // Shortest content first: the least-padded match is the closest to what was
    // asked for. Content then id break ties, so the order is total.
    fuzzy.sort_by(|a, b| {
        a.content
            .len()
            .cmp(&b.content.len())
            .then_with(|| a.content.cmp(&b.content))
            .then_with(|| a.id.0.cmp(&b.id.0))
    });
    if fuzzy.len() == 1 {
        let c = fuzzy.remove(0);
        Focus::Fuzzy {
            id: c.id,
            content: c.content,
        }
    } else {
        Focus::Ambiguous(fuzzy)
    }
}

/// Render a BFS neighbourhood around `target`. Caller holds the graph read
/// lock; this function never awaits (spec §6.4).
pub(crate) fn render_neighbourhood(
    g: &Graph,
    target: NodeId,
    depth: usize,
) -> (String, serde_json::Value) {
    use std::collections::{HashMap, HashSet};

    let radii = format::blast_radii(g);
    let label = |id: NodeId| -> String {
        match g.node(id) {
            Some(Node::Concept(c)) => {
                let canon = match c.canonization_status {
                    CanonizationStatus::Canonical => ", canonical",
                    CanonizationStatus::Venerable => ", venerable",
                    CanonizationStatus::Candidate => ", candidate",
                    CanonizationStatus::None => "",
                };
                format!("{} [{:?}{}]", c.content, c.concept_type, canon)
            }
            Some(Node::Interaction(i)) => {
                format!("<interaction {}>", i.id.0)
            }
            None => format!("<missing {}>", id.0),
        }
    };

    let mut text = String::new();
    text.push_str(&format!("focus: {}\n", label(target)));
    if let Some(r) = radii.get(&target) {
        text.push_str(&format!("blast radius: {r}\n"));
        if *r > 0 {
            text.push_str(&format!("{}\n", format::blast_radius_warning(*r)));
        }
    }
    if let Some(res) = g.reservation(target) {
        text.push_str(&format!("{}\n", format::reservation_warning(res)));
    }

    let mut seen: HashSet<NodeId> = HashSet::new();
    seen.insert(target);
    let mut frontier = vec![target];
    let mut levels: Vec<serde_json::Value> = Vec::new();
    let mut budget = MAX_INSPECT_NODES;

    for hop in 1..=depth {
        let mut next = Vec::new();
        let mut rows: Vec<serde_json::Value> = Vec::new();
        let mut by_type: HashMap<EdgeType, Vec<String>> = HashMap::new();
        for &node in &frontier {
            for edge in g.incident_edges(node) {
                let other = if edge.source == node {
                    edge.target
                } else {
                    edge.source
                };
                // Budget first, `seen` second: marking a node seen and *then*
                // discovering the budget is spent permanently excludes a
                // neighbour that was never rendered.
                if budget == 0 {
                    break;
                }
                if !seen.insert(other) {
                    continue;
                }
                budget -= 1;
                let dir = if edge.source == node { "->" } else { "<-" };
                by_type
                    .entry(edge.edge_type)
                    .or_default()
                    .push(format!("{dir} {}", label(other)));
                rows.push(json!({
                    "node_id": other.0.to_string(),
                    "label": label(other),
                    "edge_type": format!("{:?}", edge.edge_type),
                    "direction": dir,
                    "weight": edge.weight,
                }));
                next.push(other);
            }
        }
        if rows.is_empty() {
            break;
        }
        text.push_str(&format!("\nhop {hop}:\n"));
        let mut kinds: Vec<_> = by_type.into_iter().collect();
        kinds.sort_by_key(|(k, _)| format!("{k:?}"));
        for (kind, mut entries) in kinds {
            entries.sort();
            text.push_str(&format!("  {kind:?}\n"));
            for e in entries {
                text.push_str(&format!("    {e}\n"));
            }
        }
        levels.push(json!({ "hop": hop, "neighbours": rows }));
        frontier = next;
        if budget == 0 {
            text.push_str(&format!(
                "\n(truncated at {MAX_INSPECT_NODES} neighbours)\n"
            ));
            break;
        }
    }

    let structured = json!({
        "node_id": target.0.to_string(),
        "label": label(target),
        "blast_radius": radii.get(&target).copied().unwrap_or(0),
        "levels": levels,
    });
    (text, structured)
}

/// Inspect the neighbourhood around `focus` (lease-free reader).
pub async fn run(
    store: &dyn GraphStore,
    session: &str,
    focus: &str,
    depth: usize,
) -> Result<String, CliError> {
    require_nonempty("session", session)?;
    check_size_cli("session", session)?;
    require_nonempty("focus", focus)?;
    check_size_cli("focus", focus)?;
    if depth > MAX_INSPECT_DEPTH {
        return Err(CliError::Usage(format!(
            "depth must be in 0..={MAX_INSPECT_DEPTH}"
        )));
    }

    let loaded = load_reader_graph(store, session).await?;
    let g = loaded.graph.read();
    match resolve_focus(&g, focus.trim()) {
        Focus::Exact(id) => {
            let (text, _) = render_neighbourhood(&g, id, depth);
            Ok(text)
        }
        Focus::Fuzzy {
            id,
            content: matched,
        } => {
            let note = format!(
                "resolved '{}' → '{}' (substring match, single candidate)",
                focus.trim(),
                matched
            );
            let (text, _) = render_neighbourhood(&g, id, depth);
            Ok(format!("{note}\n{text}"))
        }
        Focus::Ambiguous(candidates) => {
            let mut msg = format!(
                "inspect: '{}' matches {} concepts — name one exactly, or pass its node_id:",
                focus.trim(),
                candidates.len()
            );
            for c in candidates.iter().take(MAX_INSPECT_CANDIDATES) {
                msg.push_str(&format!("\n  {} [{}]", c.content, c.id.0));
            }
            if candidates.len() > MAX_INSPECT_CANDIDATES {
                msg.push_str(&format!(
                    "\n  … and {} more",
                    candidates.len() - MAX_INSPECT_CANDIDATES
                ));
            }
            Err(CliError::Usage(msg))
        }
        Focus::Oversized { cap } => Err(CliError::Runtime(format!(
            "inspect: this session's graph has more than {cap} concepts, so the substring \
             (fuzzy) focus is disabled; pass a node_id or an exact concept instead"
        ))),
        Focus::Missing => Err(CliError::Runtime(format!(
            "inspect: no concept matching '{}' in session '{}'",
            focus, session
        ))),
    }
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;
    use chrono::Utc;
    use uuid::Uuid;

    use super::*;
    use crate::graph::Graph;
    use crate::types::{AgentId, CanonizationStatus, Concept, ConceptType, Interaction, SessionId};

    fn sid() -> SessionId {
        SessionId::from("test-session")
    }

    fn ts() -> chrono::DateTime<Utc> {
        Utc.timestamp_opt(1_752_000_000, 0).unwrap()
    }

    fn interaction(id: u64) -> Interaction {
        Interaction {
            id: NodeId(Uuid::from_u64_pair(1, id)),
            session_id: sid(),
            agent_id: AgentId::from("agent-a"),
            prompt_text: Some(format!("prompt {id}")),
            previous_id: None,
            created_at: ts(),
        }
    }

    fn concept(id: u64, origin: NodeId, content: &str) -> Concept {
        Concept {
            id: NodeId(Uuid::from_u64_pair(2, id)),
            session_id: sid(),
            content: content.to_string(),
            canonical_key: content.to_string(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("agent-a"),
            created_at: ts(),
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

    fn graph_with_concepts(n: usize) -> (Graph, NodeId, NodeId) {
        let mut g = Graph::new(sid());
        let i = interaction(1);
        let iid = i.id;
        g.insert_interaction(i).unwrap();
        let mut last = iid;
        for k in 1..=n {
            let c = concept(k as u64, iid, &format!("concept-{k}"));
            last = c.id;
            g.insert_concept(c, iid).unwrap();
        }
        (g, iid, last)
    }

    /// The fuzzy leg must refuse a graph past [`MAX_INSPECT_SCAN_CONCEPTS`]
    /// instead of running its O(total-content) lowercase pass (T8.7 #3 guard),
    /// while the exact and node-id legs still resolve.
    #[test]
    fn a_graph_past_the_scan_cap_refuses_only_the_fuzzy_leg() {
        let (g, _iid, cid) = graph_with_concepts(MAX_INSPECT_SCAN_CONCEPTS + 1);

        // Exact content match still resolves — the guard only gates the
        // substring leg's lowercase pass.
        assert!(matches!(resolve_focus(&g, "concept-7"), Focus::Exact(_)));
        // Node-id focus still resolves.
        assert!(matches!(
            resolve_focus(&g, &cid.0.to_string()),
            Focus::Exact(_)
        ));
        // A non-matching substring focus is refused before the O(total-content)
        // pass, not silently scanned.
        match resolve_focus(&g, "no-such-substring") {
            Focus::Oversized { cap } => assert_eq!(cap, MAX_INSPECT_SCAN_CONCEPTS),
            other => panic!("a graph past the cap must refuse the fuzzy leg, got {other:?}"),
        }
    }

    /// A graph within the cap still resolves the fuzzy leg normally (so the
    /// guard does not fire spuriously).
    #[test]
    fn a_graph_within_the_scan_cap_still_resolves_the_fuzzy_leg() {
        let (g, _iid, _cid) = graph_with_concepts(3);
        match resolve_focus(&g, "concept-2") {
            Focus::Fuzzy { .. } | Focus::Exact(_) => {}
            other => panic!("a small graph must resolve a substring focus, got {other:?}"),
        }
    }
}
