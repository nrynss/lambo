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
        Focus::Missing => Err(CliError::Runtime(format!(
            "inspect: no concept matching '{}' in session '{}'",
            focus, session
        ))),
    }
}
