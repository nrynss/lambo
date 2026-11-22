//! Property tests: `rank_members` against `formal/LamboProofs/Order.lean`,
//! and `emitted` against `formal/LamboProofs/TopK.lean`.

use std::cmp::Ordering;

use proptest::collection::vec;
use proptest::prelude::*;

use super::*;
use crate::test_util::props::{self, cid, key, origin};
use crate::types::{Concept, ConceptType};

/// A node in no graph (a stale durable-vector id).
fn phantom(n: u64) -> NodeId {
    NodeId(uuid::Uuid::from_u64_pair(9, n))
}

/// A score: a few repeated values (so ties are common), the IEEE specials
/// `total_cmp` orders, or any `f64`.
fn score() -> impl Strategy<Value = f64> {
    prop_oneof![
        Just(0.5),
        Just(0.0),
        Just(-0.0),
        Just(f64::NAN),
        Just(f64::INFINITY),
        Just(f64::NEG_INFINITY),
        any::<f64>(),
    ]
}

// ---------------------------------------------------------------------------
// rank_members (Order.lean)
// ---------------------------------------------------------------------------

/// One concept of a ranking case: its key and whether it is Canonical.
type RankConcept = (Option<String>, bool);

/// Concepts (any key, keys may collide), each with a score, plus the origin
/// interaction and a phantom id as keyless members, and a shuffle.
fn rank_case() -> impl Strategy<Value = (Vec<RankConcept>, Vec<f64>, Vec<usize>)> {
    (0usize..8).prop_flat_map(|n| {
        let members: Vec<usize> = (0..n + 2).collect();
        (
            vec((key(), any::<bool>()), n),
            vec(score(), n + 2),
            Just(members).prop_shuffle(),
        )
    })
}

/// The graph of a ranking case. A key-less concept is impossible in the
/// graph, so `None` keys become the empty key, and every concept is an
/// Observation so colliding keys insert.
fn rank_graph(concepts: &[RankConcept]) -> Graph {
    props::graph_of(concepts.iter().enumerate().map(|(i, (k, canonical))| {
        let status = if *canonical {
            CanonizationStatus::Canonical
        } else {
            CanonizationStatus::None
        };
        let mut c: Concept = props::concept(i as u64, "x", k.as_deref().unwrap_or(""), status);
        c.concept_type = ConceptType::Observation;
        c
    }))
}

/// Member `i` of a case with `n` concepts: a concept, the origin
/// interaction, or a phantom.
fn rank_member(i: usize, n: usize) -> NodeId {
    match i {
        i if i < n => cid(i as u64),
        i if i == n => origin(),
        _ => phantom(0),
    }
}

/// Lean `Order.rankMembers` (`formal/LamboProofs/Order.lean`), built
/// independently of `rank_cmp`: Canonical first, score descending by IEEE
/// total order (NaN and the infinities included), then Lean `tieBreak` as
/// [`props::spec_tie_key`] states it.
fn spec_order(graph: &Graph, a: &Scored<NodeId>, b: &Scored<NodeId>) -> Ordering {
    let canonical = |id| {
        matches!(graph.node(id), Some(Node::Concept(c))
            if c.canonization_status == CanonizationStatus::Canonical)
    };
    let tie = |id| {
        let key = match graph.node(id) {
            Some(Node::Concept(c)) => Some(c.canonical_key.as_str()),
            _ => None,
        };
        props::spec_tie_key(key, id)
    };
    // `f64::total_cmp` as an integer order on the bits: flip every bit but
    // the sign of a negative number, then compare as `i64`.
    let total = |x: f64| {
        let bits = x.to_bits() as i64;
        bits ^ (((bits >> 63) as u64) >> 1) as i64
    };
    canonical(b.item)
        .cmp(&canonical(a.item))
        .then_with(|| total(b.score).cmp(&total(a.score)))
        .then_with(|| tie(a.item).cmp(&tie(b.item)))
}

/// Bit-exact identity of a ranked list (`f64` has no `Eq`; NaN must compare).
fn bits(list: &[Scored<NodeId>]) -> Vec<(NodeId, u64)> {
    list.iter().map(|s| (s.item, s.score.to_bits())).collect()
}

proptest! {
    #![proptest_config(props::config())]

    /// `Order.rankMembers_lawful`, `Order.rankMembers_separating`,
    /// `Order.rankMembers_canonical_first` and `Order.sorted_unique`
    /// (`formal/LamboProofs/Order.lean`), on random mixed keyed and keyless
    /// members (concepts, an interaction, an id in no graph) with random
    /// scores, NaN and infinities included:
    ///
    /// * `rank_cmp`, the comparator `rank_members` sorts by, is lawful (swap,
    ///   `Less` transitive) and separating;
    /// * `rank_members` gives the same list for any permutation of its input,
    ///   and that list is the one Lean `rankMembers` gives ([`spec_order`]:
    ///   Canonical first, score descending, keyless last, tie-form key
    ///   ascending, id ascending), strictly sorted, every Canonical member
    ///   ahead of every other one.
    #[test]
    fn rank_members_is_a_lawful_deterministic_order((concepts, scores, shuffle) in rank_case()) {
        let n = concepts.len();
        let g = rank_graph(&concepts);
        let members: Vec<Scored<NodeId>> = (0..n + 2)
            .map(|i| Scored::new(rank_member(i, n), scores[i]))
            .collect();

        for a in &members {
            for b in &members {
                let ab = rank_cmp(&g, a, b);
                prop_assert_eq!(ab, rank_cmp(&g, b, a).reverse(), "swap");
                if ab == Ordering::Equal {
                    prop_assert_eq!(a.item, b.item, "separating");
                }
                for c in &members {
                    if ab == Ordering::Less && rank_cmp(&g, b, c) == Ordering::Less {
                        prop_assert_eq!(rank_cmp(&g, a, c), Ordering::Less, "lt_trans");
                    }
                }
            }
        }

        let mut x = members.clone();
        let mut y: Vec<Scored<NodeId>> = shuffle.iter().map(|&i| members[i].clone()).collect();
        let mut z = members.clone();
        rank_members(&g, &mut x);
        rank_members(&g, &mut y);
        z.sort_by(|a, b| spec_order(&g, a, b));
        prop_assert_eq!(bits(&x), bits(&y), "sorted_unique");
        prop_assert_eq!(bits(&x), bits(&z), "not Lean rankMembers' order");
        for (i, a) in x.iter().enumerate() {
            for b in &x[i + 1..] {
                prop_assert_eq!(spec_order(&g, a, b), Ordering::Less, "strictly sorted");
            }
        }
        let first_plain = x
            .iter()
            .position(|s| !matches!(g.node(s.item), Some(Node::Concept(c))
                if c.canonization_status == CanonizationStatus::Canonical))
            .unwrap_or(x.len());
        for s in &x[first_plain..] {
            prop_assert!(!matches!(g.node(s.item), Some(Node::Concept(c))
                if c.canonization_status == CanonizationStatus::Canonical), "canonical_first");
        }
    }
}

// ---------------------------------------------------------------------------
// emitted (TopK.lean)
// ---------------------------------------------------------------------------

/// Concept contents: a small pool, so the same-content signal of #181 fires.
const CONTENTS: [&str; 4] = ["alpha beta", "Alpha  beta", "gamma", "delta 7"];

/// One ranked entry: which member (`< n` a concept, `n` the origin
/// interaction, more a phantom), whether it is forced, whether it would
/// carry a warning line of its own.
type Entry = (usize, bool, bool);

/// `n` concepts' contents, the ranked entries (distinct members, any
/// order), `top_k`, and the selection: none, or `similar_signals` off/on.
///
/// Drawn so the #181 window binds. Once `top_k` ordinary hits are taken
/// nothing more is checked, so a skip past `window(top_k)` needs a forced
/// member ahead and then more than `window(top_k)` (4 for `top_k = 1`)
/// redundant, unwarned ordinary members in a row. So: contents mostly one
/// text, few warnings (a warned member is never skipped), `top_k` mostly 1,
/// long lists and a selection more often than not, and in most cases the
/// first concept with that text moved to the front and forced.
fn emit_case() -> impl Strategy<Value = (Vec<usize>, Vec<Entry>, usize, Option<bool>)> {
    prop_oneof![1 => 0usize..16, 2 => 8usize..16]
        .prop_flat_map(|n| {
            (
                vec(
                    prop_oneof![16 => Just(0usize), 1 => Just(1), 1 => Just(2), 1 => Just(3)],
                    n,
                ),
                proptest::sample::subsequence((0..n + 3).collect::<Vec<_>>(), 0..=n + 3)
                    .prop_shuffle()
                    .prop_flat_map(|ids| {
                        let k = ids.len();
                        (
                            Just(ids),
                            vec(
                                (proptest::bool::weighted(0.3), proptest::bool::weighted(0.1)),
                                k,
                            ),
                        )
                    })
                    .prop_map(|(ids, flags)| {
                        ids.into_iter()
                            .zip(flags)
                            .map(|(i, (f, w))| (i, f, w))
                            .collect::<Vec<Entry>>()
                    }),
                prop_oneof![1 => 0usize..6, 4 => Just(1usize), 1 => Just(2usize)],
                proptest::option::weighted(0.9, any::<bool>()),
                proptest::bool::weighted(0.8),
            )
        })
        .prop_map(|(contents, mut entries, top_k, selection, lead)| {
            let n = contents.len();
            if lead
                && let Some(at) = entries
                    .iter()
                    .position(|&(i, _, _)| i < n && contents[i] == 0)
            {
                let (i, _, w) = entries.remove(at);
                entries.insert(0, (i, true, w));
            }
            (contents, entries, top_k, selection)
        })
}

proptest! {
    #![proptest_config(props::config())]

    /// `TopK.emit_sublist`, `TopK.forced_mem_emit`, `TopK.emit_free_le_topK`,
    /// `TopK.emit_all_concepts` and `TopK.emit_free_eq_take`, and with the
    /// #181 redundancy skip `TopK.emitR_sublist`, `TopK.forced_mem_emitR`,
    /// `TopK.emitR_free_le_topK`, `TopK.emitR_free_count`,
    /// `TopK.skipsR_spec`, `TopK.skipsR_kept_emitted`,
    /// `TopK.skipsR_kept_before`, `TopK.skipsR_not_emitted` and
    /// `TopK.skipsR_length_le` (`formal/LamboProofs/TopK.lean`):
    ///
    /// * the output is a subsequence of `ranked` and holds only concepts;
    /// * every forced concept is emitted, at any `top_k`, never skipped;
    /// * at most `top_k` non-forced hits; without the skip they are exactly
    ///   the first `top_k` ordinary concepts, with it there are
    ///   `min(top_k, ordinary - skipped)`;
    /// * a skipped member is an ordinary concept with no warning of its own,
    ///   redundant (by `Redundancy::signal`) with a hit emitted ahead of it,
    ///   is not itself emitted, and at most `window(top_k)` are skipped;
    /// * `TopK.emitR_eq_emit_of_skipsR_nil`: when nothing is skipped the hits
    ///   are exactly the ones `emitted` gives without a selection.
    #[test]
    fn emitted_keeps_forced_caps_free_and_keeps_order(
        (contents, entries, top_k, selection) in emit_case()
    ) {
        let n = contents.len();
        let g = props::graph_of((0..n).map(|i| {
            props::concept(i as u64, CONTENTS[contents[i]], &format!("k{i}"), CanonizationStatus::None)
        }));
        let id = |i: usize| match i {
            i if i < n => cid(i as u64),
            i if i == n => origin(),
            i => phantom(i as u64),
        };
        let ranked: Vec<Scored<NodeId>> =
            entries.iter().map(|&(i, _, _)| Scored::new(id(i), 0.5)).collect();
        let forced_set: HashSet<NodeId> =
            entries.iter().filter(|e| e.1).map(|&(i, _, _)| id(i)).collect();
        let warn_set: HashSet<NodeId> =
            entries.iter().filter(|e| e.2).map(|&(i, _, _)| id(i)).collect();
        let forced = |x: NodeId| forced_set.contains(&x);
        let is_concept = |x: NodeId| matches!(g.node(x), Some(Node::Concept(_)));

        let redundancy = selection.map(|similar| Redundancy { threshold: 0.85, similar });
        let mut warns = |x: NodeId| warn_set.contains(&x);
        let mut sel = redundancy.as_ref().map(|r| Selection {
            redundancy: r,
            warns: &mut warns,
            verdicts: HashMap::new(),
        });
        let emission = emitted(&g, &ranked, forced, top_k, sel.as_mut());
        let out: Vec<NodeId> = emission.hits.iter().map(|s| s.item).collect();
        let ranked_ids: Vec<NodeId> = ranked.iter().map(|s| s.item).collect();
        let pos = |x: NodeId| ranked_ids.iter().position(|&r| r == x).unwrap();

        // emit_sublist / emitR_sublist (ids are distinct, so strictly
        // increasing positions is the subsequence test)
        let positions: Vec<usize> = out.iter().map(|&x| pos(x)).collect();
        prop_assert!(positions.windows(2).all(|w| w[0] < w[1]), "not a subsequence: {:?}", positions);
        // emit_all_concepts
        prop_assert!(out.iter().all(|&x| is_concept(x)));
        // forced_mem_emit / forced_mem_emitR
        for &x in &ranked_ids {
            if is_concept(x) && forced(x) {
                prop_assert!(out.contains(&x), "forced {} cut", x);
            }
        }
        // emit_free_le_topK / emitR_free_le_topK
        let free: Vec<NodeId> = out.iter().copied().filter(|&x| !forced(x)).collect();
        prop_assert!(free.len() <= top_k);

        let ordinary: Vec<NodeId> =
            ranked_ids.iter().copied().filter(|&x| is_concept(x) && !forced(x)).collect();
        let skips: Vec<(NodeId, NodeId)> = emission
            .skipped
            .iter()
            .flat_map(|(&kept, list)| list.iter().map(move |s| (kept, s.node_id)))
            .collect();
        match &redundancy {
            None => {
                // emit_free_eq_take
                prop_assert!(skips.is_empty());
                let want: Vec<NodeId> = ordinary.iter().copied().take(top_k).collect();
                prop_assert_eq!(free, want);
            }
            Some(r) => {
                // emitR_free_count
                prop_assert_eq!(free.len(), top_k.min(ordinary.len() - skips.len()));
                // skipsR_length_le
                prop_assert!(skips.len() <= redundancy::window(top_k));
                let concept = |x: NodeId| match g.node(x) {
                    Some(Node::Concept(c)) => c,
                    _ => unreachable!("checked is_concept"),
                };
                for &(kept, x) in &skips {
                    // skipsR_spec
                    prop_assert!(is_concept(x) && !forced(x) && !warn_set.contains(&x));
                    let (kc, xc) = (concept(kept), concept(x));
                    let (kn, xn) = (
                        redundancy::normalized_content(&kc.content),
                        redundancy::normalized_content(&xc.content),
                    );
                    prop_assert!(r.signal(&g, (kc, &kn), (xc, &xn)).is_some(), "skip without a signal");
                    // skipsR_kept_emitted, skipsR_kept_before, skipsR_not_emitted
                    prop_assert!(out.contains(&kept));
                    prop_assert!(pos(kept) < pos(x));
                    prop_assert!(!out.contains(&x));
                }
                // emitR_eq_emit_of_skipsR_nil
                if skips.is_empty() {
                    let plain = emitted(&g, &ranked, forced, top_k, None);
                    let plain: Vec<NodeId> = plain.hits.iter().map(|s| s.item).collect();
                    prop_assert_eq!(&out, &plain, "nothing skipped, yet the hits differ");
                }
            }
        }
    }
}
