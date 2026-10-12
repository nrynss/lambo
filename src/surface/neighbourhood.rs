//! The `inspect` neighbourhood projection, shared by `lambo inspect` and
//! `lambo_inspect`: the focus's blast radius and the dependents it counts
//! (#36), then a bounded BFS around the focus, rendered as text and as
//! structured JSON. Caps: [`MAX_INSPECT_NODES`] (a total across every
//! hop); the depth bound ([`super::limits::MAX_INSPECT_DEPTH`]) is each
//! caller's door check.

use serde_json::json;

use super::limits::MAX_INSPECT_NODES;
use crate::graph::Graph;
use crate::recall::format;
use crate::types::{CanonizationStatus, EdgeType, Node, NodeId};

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
    // #36: name the dependents the count is made of, so a warning says what
    // rests on the focus instead of leaving the caller to read edge
    // directions off the hops below (which list neighbours both ways).
    let (dependents, dependents_total) = dependents_listing(g, target);
    let dependents_truncated = dependents_total > dependents.len();
    if !dependents.is_empty() {
        // The header carries the true total; the rows stop at the cap and the
        // truncation row says so.
        text.push_str(&format!("dependents ({dependents_total}):\n"));
        for d in &dependents {
            text.push_str(&format!("  {} ({:?})\n", label(d.id), d.edge_type));
        }
        if dependents_truncated {
            text.push_str(&format!(
                "  (truncated at {MAX_INSPECT_NODES} dependents)\n"
            ));
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

    let dependents_json: Vec<serde_json::Value> = dependents
        .iter()
        .map(|d| {
            json!({
                "node_id": d.id.0.to_string(),
                "label": label(d.id),
                "edge_type": format!("{:?}", d.edge_type),
            })
        })
        .collect();
    let structured = json!({
        "node_id": target.0.to_string(),
        "label": label(target),
        "blast_radius": radii.get(&target).copied().unwrap_or(0),
        "dependents": dependents_json,
        "dependents_truncated": dependents_truncated,
        "levels": levels,
    });
    (text, structured)
}

/// One dependent of the inspect focus and the edge kind that declares it.
struct Dependent {
    id: NodeId,
    edge_type: EdgeType,
}

/// The focus's dependents (#36, [`format::dependents_of`]): the concepts
/// that depend on it (`Dependency` edges into it) and its `parent_of`
/// children, labelled with the declaring edge kind (`Dependency` when both
/// kinds declare the same dependent). Ordered by label then node id, capped
/// at [`MAX_INSPECT_NODES`]; returns the capped rows and the true total.
fn dependents_listing(g: &Graph, target: NodeId) -> (Vec<Dependent>, usize) {
    let dependency_sources: std::collections::HashSet<NodeId> = g
        .incident_edges(target)
        .into_iter()
        .filter(|e| e.edge_type == EdgeType::Dependency && e.target == target)
        .map(|e| e.source)
        .collect();
    let mut out: Vec<Dependent> = format::dependents_of(g, target)
        .into_iter()
        .map(|(id, _)| Dependent {
            id,
            edge_type: if dependency_sources.contains(&id) {
                EdgeType::Dependency
            } else {
                EdgeType::Hierarchical
            },
        })
        .collect();
    let content = |id: NodeId| match g.node(id) {
        Some(Node::Concept(c)) => c.content.as_str(),
        _ => "",
    };
    out.sort_by(|a, b| content(a.id).cmp(content(b.id)).then(a.id.0.cmp(&b.id.0)));
    let total = out.len();
    out.truncate(MAX_INSPECT_NODES);
    (out, total)
}

#[cfg(test)]
mod tests {
    use super::render_neighbourhood;
    use crate::surface::limits::MAX_INSPECT_NODES;
    use crate::test_util::dependents as case;

    /// #36: inspect names the dependents its blast radius counts, in text
    /// and JSON, with the declaring edge kind; a parent, a Causal producer,
    /// an association neighbour and a contradiction (#19) are not listed.
    /// The inspect text the cloudops guard parses
    /// (`scripts/cloudops/_lambo.py::parse_dependents`), pinned byte for byte
    /// in `fixtures/inspect-dependents-golden.txt`. The Python self-test reads
    /// the same file, so the parser is tested against real renderer output.
    /// Regenerate with `LAMBO_UPDATE_GOLDEN=1` after a deliberate change.
    #[test]
    fn inspect_text_matches_the_golden_the_cloudops_guard_parses() {
        let g = case::graph();
        let (text, _) = render_neighbourhood(&g, case::concept_id("X"), 1);
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/fixtures/inspect-dependents-golden.txt"
        );
        if std::env::var_os("LAMBO_UPDATE_GOLDEN").is_some() {
            std::fs::write(path, &text).expect("write golden");
        }
        let golden = std::fs::read_to_string(path).expect("golden present");
        assert_eq!(text, golden, "inspect text drifted from {path}");
    }

    #[test]
    fn inspect_lists_the_dependents_it_counts() {
        let g = case::graph();
        let (text, json) = render_neighbourhood(&g, case::concept_id("X"), 1);
        assert!(text.contains("blast radius: 8\n"), "{text}");
        assert!(text.contains("dependents (8):\n"), "{text}");
        for (name, kind) in [
            ("A1", "Dependency"),
            ("A2", "Dependency"),
            ("Z", "Dependency"),
            ("W", "Dependency"),
            ("V", "Dependency"),
            ("E", "Dependency"),
            ("C1", "Hierarchical"),
            ("C2", "Hierarchical"),
        ] {
            assert!(
                text.contains(&format!("  concept {name} ["))
                    && text.contains(&format!("({kind})")),
                "{name}: {text}"
            );
        }
        let deps = json["dependents"].as_array().expect("dependents");
        assert_eq!(deps.len(), 8, "{json}");
        assert_eq!(json["dependents_truncated"], false);
        let labels: Vec<&str> = deps.iter().map(|d| d["label"].as_str().unwrap()).collect();
        for absent in [
            "concept P [",
            "concept K [",
            "concept N [",
            "concept D [",
            "concept Q [",
        ] {
            assert!(
                !labels.iter().any(|l| l.starts_with(absent)),
                "{absent}: {labels:?}"
            );
        }
        let c1 = deps
            .iter()
            .find(|d| d["label"].as_str().unwrap().starts_with("concept C1 ["))
            .unwrap();
        assert_eq!(c1["edge_type"], "Hierarchical");
        assert_eq!(
            c1["node_id"],
            case::concept_id("C1").0.to_string(),
            "{json}"
        );

        // Past the cap the header keeps the true total, the rows stop at
        // the cap and the truncation row says so.
        let mut g = case::graph();
        let x = case::concept_id("X");
        let origin = g
            .interactions()
            .next()
            .map(|i| i.id)
            .expect("an interaction");
        let extra = MAX_INSPECT_NODES + 3;
        for k in 0..extra {
            let mut c = match g.node(case::concept_id("A1")) {
                Some(crate::types::Node::Concept(c)) => c.clone(),
                _ => unreachable!(),
            };
            c.id = crate::types::NodeId::new();
            c.content = format!("zz extra dependent {k:04}");
            c.canonical_key = c.content.clone();
            c.origin_interaction = origin;
            let id = c.id;
            g.insert_concept(c, origin).unwrap();
            g.upsert_edge(crate::types::Edge {
                event_time: None,
                id: crate::types::NodeId::new(),
                session_id: case::session(),
                source: id,
                target: x,
                edge_type: crate::types::EdgeType::Dependency,
                weight: 1.0,
                reinforcements: 1,
                created_at: case::now(),
                last_reinforced: case::now(),
            })
            .unwrap();
        }
        let total = 8 + extra;
        let (text, json) = render_neighbourhood(&g, x, 0);
        assert!(text.contains(&format!("dependents ({total}):\n")), "{text}");
        assert!(
            text.contains(&format!("(truncated at {MAX_INSPECT_NODES} dependents)")),
            "{text}"
        );
        assert_eq!(
            json["dependents"].as_array().unwrap().len(),
            MAX_INSPECT_NODES
        );
        assert_eq!(json["dependents_truncated"], true);
        assert_eq!(json["blast_radius"], total as u64);

        // A node nothing depends on lists no dependents.
        let g = case::graph();
        let (text, json) = render_neighbourhood(&g, case::concept_id("K"), 1);
        assert!(!text.contains("dependents ("), "{text}");
        assert!(json["dependents"].as_array().unwrap().is_empty());
    }
}
