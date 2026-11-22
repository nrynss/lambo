//! Property tests: [`expand`] against `formal/LamboProofs/Expand.lean`.

use std::collections::{HashMap, HashSet, VecDeque};

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::test_util::props::{self, cid, origin};
use crate::types::Edge;

/// The edge types a random graph draws from: the five traversal types and
/// two concept-to-concept types `expand` must not follow.
const TYPES: [EdgeType; 7] = [
    EdgeType::Dependency,
    EdgeType::Causal,
    EdgeType::Hierarchical,
    EdgeType::CoOccurrence,
    EdgeType::Semantic,
    EdgeType::Contradicts,
    EdgeType::SuspectedSupersedes,
];

/// A node that is in no graph: a seed may name one (a stale vector id).
fn phantom() -> NodeId {
    cid(999)
}

/// The edge types `expand` follows (spec §8; `Expand.lean` leaves them
/// abstract in its neighbour function), spelled out rather than read from
/// `TRAVERSAL_ORDER`, so a change there is caught here.
fn traversable(ty: EdgeType) -> bool {
    matches!(
        ty,
        EdgeType::Dependency
            | EdgeType::Causal
            | EdgeType::Hierarchical
            | EdgeType::CoOccurrence
            | EdgeType::Semantic
    )
}

/// `(n, edges, seeds, depth, groups)`: see [`case`].
type Case = (
    u64,
    Vec<(u64, u64, usize)>,
    Vec<u64>,
    usize,
    Vec<Option<u8>>,
);

/// A random graph of `n` concepts, its edges, the seeds (indices: `< n` a
/// concept, `n` the origin interaction, `n + 1` a node not in the graph),
/// the depth, and each concept's chunk group (one of three, or none).
fn case() -> impl Strategy<Value = Case> {
    (1u64..=10).prop_flat_map(|n| {
        (
            Just(n),
            vec((0..n, 0..n, 0..TYPES.len()), 0..30),
            vec(0..n + 2, 0..6),
            0usize..5,
            vec(proptest::option::weighted(0.5, 0u8..3), n as usize),
        )
    })
}

/// Builds the graph and returns it with the edges it accepted, as
/// `(source, target, type)`. Rejected edges (an endpoint rule) are left out
/// of both, so the reference below sees exactly the graph `expand` sees.
fn build(
    n: u64,
    edges: &[(u64, u64, usize)],
    groups: &[Option<u8>],
) -> (Graph, Vec<(NodeId, NodeId, EdgeType)>) {
    let mut g = props::graph_of((0..n).map(|i| {
        let text = format!("c{i}");
        let mut c = props::concept(i, &text, &text, crate::types::CanonizationStatus::None);
        c.chunk_group_id = groups[i as usize].map(|k| format!("group-{k}"));
        c
    }));
    let mut accepted = Vec::new();
    for (k, &(s, t, ty)) in edges.iter().enumerate() {
        let (src, tgt, ty) = (cid(s), cid(t), TYPES[ty]);
        let edge = Edge {
            id: NodeId(uuid::Uuid::from_u64_pair(3, k as u64)),
            session_id: props::sid(),
            source: src,
            target: tgt,
            edge_type: ty,
            weight: 1.0,
            reinforcements: 1,
            created_at: props::t0(),
            last_reinforced: props::t0(),
            event_time: None,
        };
        if g.upsert_edge(edge).is_ok() {
            accepted.push((src, tgt, ty));
        }
    }
    (g, accepted)
}

/// The reference: every node within `depth` traversable edges of a seed,
/// by a plain BFS over the accepted edge list (not over the graph's
/// adjacency index, which `expand` reads).
fn within(edges: &[(NodeId, NodeId, EdgeType)], seeds: &[NodeId], depth: usize) -> HashSet<NodeId> {
    let mut out: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
    for &(s, t, ty) in edges {
        if traversable(ty) {
            out.entry(s).or_default().push(t);
        }
    }
    let mut dist: HashMap<NodeId, usize> = HashMap::new();
    let mut queue = VecDeque::new();
    for &s in seeds {
        if dist.insert(s, 0).is_none() {
            queue.push_back(s);
        }
    }
    while let Some(x) = queue.pop_front() {
        let d = dist[&x];
        if d == depth {
            continue;
        }
        for &y in out.get(&x).into_iter().flatten() {
            if let std::collections::hash_map::Entry::Vacant(e) = dist.entry(y) {
                e.insert(d + 1);
                queue.push_back(y);
            }
        }
    }
    dist.into_keys().collect()
}

proptest! {
    #![proptest_config(props::config())]

    /// `Expand.expand_nodup`, `Expand.expand_sound`, `Expand.expand_complete`
    /// and `Expand.expand_prefix` (`formal/LamboProofs/Expand.lean`), on
    /// random graphs, seeds and depths:
    ///
    /// * no node is recorded twice;
    /// * every member is within `depth` traversable edges of a seed, and
    ///   every such node is a member, so the members are exactly that set;
    /// * the output starts with the de-duplicated seeds, in input order
    ///   (Lean `discover [] seeds`).
    ///
    /// Seeds may name the origin interaction (only non-traversable `Derives`
    /// edges leave it) and a node in no graph.
    ///
    /// `siblings` is not in the Lean model; it is checked against spec §8
    /// directly: exactly the concepts that share a chunk group with a
    /// member and are not members themselves, id ascending.
    #[test]
    fn expand_is_exactly_the_bounded_neighbourhood((n, edges, seeds, depth, groups) in case()) {
        let (g, accepted) = build(n, &edges, &groups);
        let seeds: Vec<NodeId> = seeds
            .into_iter()
            .map(|i| match i {
                i if i < n => cid(i),
                i if i == n => origin(),
                _ => phantom(),
            })
            .collect();
        let input: Vec<Scored<NodeId>> = seeds.iter().map(|&s| Scored::new(s, 0.5)).collect();
        let out = expand(&g, input, depth);
        let got: Vec<NodeId> = out.required.iter().map(|s| s.item).collect();

        // expand_nodup
        let set: HashSet<NodeId> = got.iter().copied().collect();
        prop_assert_eq!(set.len(), got.len(), "a node recorded twice: {:?}", got);

        // expand_sound + expand_complete
        prop_assert_eq!(&set, &within(&accepted, &seeds, depth));

        // expand_prefix
        let mut seen = HashSet::new();
        let dedup: Vec<NodeId> = seeds.iter().copied().filter(|s| seen.insert(*s)).collect();
        prop_assert_eq!(&got[..dedup.len()], &dedup[..]);

        // Spec §8 sibling closure, from the drawn groups.
        let touched: HashSet<u8> = (0..n)
            .filter(|&i| set.contains(&cid(i)))
            .filter_map(|i| groups[i as usize])
            .collect();
        let mut want: Vec<NodeId> = (0..n)
            .filter(|&i| groups[i as usize].is_some_and(|k| touched.contains(&k)))
            .map(cid)
            .filter(|c| !set.contains(c))
            .collect();
        want.sort_by_key(|c| c.0);
        let siblings: Vec<NodeId> = out.siblings.iter().map(|s| s.item).collect();
        prop_assert_eq!(siblings, want, "siblings");
    }
}
