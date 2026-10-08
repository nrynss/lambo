//! Issue #30: recorded accesses and their draining.

use super::*;

// -----------------------------------------------------------------------
// Issue #30 — read accesses
// -----------------------------------------------------------------------

#[test]
fn record_accesses_updates_count_and_last_accessed_without_bumping_the_epoch() {
    let (mut g, iid, cid) = small_graph();
    g.drain_log();
    let epoch = g.epoch();

    let applied = g.record_accesses(&[(cid, 3, ts(10)), (iid, 5, ts(10))]);
    assert_eq!(
        applied, 1,
        "the interaction id carries no counter and is skipped"
    );
    let Some(Node::Concept(c)) = g.node(cid) else {
        panic!("concept missing")
    };
    assert_eq!(c.access_count, 3);
    assert_eq!(c.last_accessed, Some(ts(10)));
    assert_eq!(
        g.epoch(),
        epoch,
        "an access must not advance the mutation epoch"
    );

    // Nothing reaches the log; the concept is access-dirty, and the drain
    // the flush calls yields one narrow RecordAccess carrying its absolute
    // values (not a full-row UpsertNode).
    assert_eq!(g.log_len(), 0, "a read never appends to the log");
    assert_eq!(g.drain_log().mutation_epoch, epoch);
    assert_eq!(g.pending_accesses(), 1);
    assert_eq!(
        g.drain_accesses(usize::MAX),
        [Mutation::RecordAccess {
            session_id: g.session_id().clone(),
            id: cid,
            access_count: 3,
            last_accessed: ts(10),
        }]
    );
}

/// The dirty set is per concept, not per read: any number of applies
/// before a drain yield one update per concept with the latest values; a
/// limited drain takes the lowest ids and leaves the rest dirty; a removed
/// concept leaves the set.
#[test]
fn drain_accesses_is_one_update_per_concept_and_honours_the_limit() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    let ids: Vec<NodeId> = (1..=3)
        .map(|k| {
            let c = concept(k, iid, &format!("c{k}"));
            let id = c.id;
            g.insert_concept(c, iid).unwrap();
            id
        })
        .collect();
    g.drain_log();
    for round in 0..100 {
        g.record_accesses(&[
            (ids[0], 1, ts(round)),
            (ids[1], 1, ts(round)),
            (ids[2], 1, ts(round)),
        ]);
    }
    assert_eq!(g.pending_accesses(), 3, "bounded by concepts, not reads");
    assert_eq!(g.log_len(), 0);

    let mut sorted = ids.clone();
    sorted.sort_unstable_by_key(|id| id.0);
    let first = g.drain_accesses(2);
    let drained: Vec<(NodeId, i32)> = first
        .iter()
        .map(|m| match m {
            Mutation::RecordAccess {
                id, access_count, ..
            } => (*id, *access_count),
            other => panic!("{other:?}"),
        })
        .collect();
    assert_eq!(drained, [(sorted[0], 100), (sorted[1], 100)]);
    assert_eq!(g.pending_accesses(), 1);

    g.remove_node(sorted[2]).unwrap();
    assert_eq!(g.pending_accesses(), 0, "a removed concept leaves the set");
    assert!(g.drain_accesses(usize::MAX).is_empty());
}

#[test]
fn record_accesses_accumulates_keeps_the_latest_instant_and_saturates() {
    let (mut g, _iid, cid) = small_graph();
    g.record_accesses(&[(cid, 2, ts(20))]);
    // An older instant arriving later (a slow caller) never moves the
    // timestamp backwards.
    g.record_accesses(&[(cid, 1, ts(5))]);
    let Some(Node::Concept(c)) = g.node(cid) else {
        panic!()
    };
    assert_eq!(c.access_count, 3);
    assert_eq!(c.last_accessed, Some(ts(20)));

    g.record_accesses(&[(cid, u32::MAX, ts(21))]);
    g.record_accesses(&[(cid, u32::MAX, ts(22))]);
    let Some(Node::Concept(c)) = g.node(cid) else {
        panic!()
    };
    assert_eq!(c.access_count, i32::MAX, "saturating, never wraps negative");
}

#[test]
fn record_accesses_skips_missing_ids_and_zero_counts() {
    let (mut g, _iid, cid) = small_graph();
    g.drain_log();
    let applied = g.record_accesses(&[(uid(999), 4, ts(1)), (cid, 0, ts(1))]);
    assert_eq!(applied, 0);
    assert_eq!(g.log_len(), 0, "nothing to persist");
    let Some(Node::Concept(c)) = g.node(cid) else {
        panic!()
    };
    assert_eq!((c.access_count, c.last_accessed), (0, None));
}

#[test]
fn accesses_survive_a_snapshot_round_trip() {
    let (mut g, _iid, cid) = small_graph();
    g.record_accesses(&[(cid, 7, ts(30))]);
    let back = Graph::from_snapshot(g.snapshot()).unwrap();
    let Some(Node::Concept(c)) = back.node(cid) else {
        panic!()
    };
    assert_eq!((c.access_count, c.last_accessed), (7, Some(ts(30))));
    assert_eq!(back.epoch(), g.epoch());
}
