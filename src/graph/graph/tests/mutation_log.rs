//! The mutation epoch and log: GC watermarks, draining and ordering.

use super::*;

/// A graph with a few writes behind it (epoch > 0).
fn graph_with_writes() -> Graph {
    let mut g = Graph::new(sid());
    let mut prev = None;
    for i in 0..3 {
        let ix = interaction(9_000 + i, prev, i as i64);
        prev = Some(ix.id);
        g.insert_interaction(ix).unwrap();
    }
    assert!(g.epoch() >= 3);
    g
}

/// Issue #29 item 2: exempting exactly the bumps a writer appended moves
/// the watermark to the epoch, so those writes are invisible to GC's
/// measure and the next real write is visible again.
#[test]
fn exempting_appended_bumps_moves_the_watermark_up_to_the_epoch() {
    let mut g = graph_with_writes();
    let e = g.epoch();
    g.record_gc_sweep(e - 2, ts(0));
    g.exempt_from_gc_measure(2);
    assert_eq!(g.gc_mark().last_gc_epoch, e);
    let tail = g.temporal_chain().last().copied();
    g.insert_interaction(interaction(1, tail, 99)).unwrap();
    assert!(
        g.epoch() > g.gc_mark().last_gc_epoch,
        "the next write counts"
    );
}

/// Issue #29 item 2: an over-large exemption cannot move the watermark
/// past the epoch (which would hide the next writes from GC's measure and
/// suppress sweeps). Debug builds treat it as the caller bug it is; release
/// builds clamp. Either way the mark never ends up ahead of the epoch.
#[test]
#[cfg_attr(debug_assertions, should_panic(expected = "past epoch"))]
fn exempt_from_gc_measure_never_moves_the_watermark_past_the_epoch() {
    let mut g = graph_with_writes();
    let e = g.epoch();
    g.record_gc_sweep(e - 1, ts(0));
    g.exempt_from_gc_measure(10);
    assert_eq!(g.gc_mark().last_gc_epoch, e, "clamped to the epoch");
}

/// Issue #29 review: the writer-side reset flag never reaches a snapshot
/// and never survives `from_snapshot`, while `drain_log` still carries it
/// (the flush path is the one place it is meant to travel).
#[test]
fn the_gc_reset_flag_never_leaves_or_enters_through_a_snapshot() {
    let mut g = graph_with_writes();
    g.reanchor_gc_clock(ts(5));
    assert!(g.gc_mark().last_gc_at_reset);
    let snap = g.snapshot();
    assert!(!snap.gc_mark.last_gc_at_reset, "snapshot is a stored view");
    assert_eq!(snap.gc_mark.last_gc_at, Some(ts(5)));
    assert!(
        g.drain_log().gc_mark.last_gc_at_reset,
        "drain still carries"
    );

    let flagged = GraphSnapshot {
        gc_mark: GcMark {
            last_gc_epoch: 3,
            last_gc_at: Some(ts(7)),
            last_gc_at_reset: true,
        },
        ..snap
    };
    let resumed = Graph::from_snapshot(flagged).unwrap();
    assert!(!resumed.gc_mark().last_gc_at_reset);
    assert_eq!(resumed.gc_mark().last_gc_at, Some(ts(7)));
    assert_eq!(resumed.gc_mark().last_gc_epoch, 3);
}

#[test]
fn epoch_bumps_per_mutation_not_per_read() {
    let (mut g, iid, cid) = small_graph();
    let e0 = g.epoch();
    assert!(e0 > 0);
    // Reads do not bump.
    let _ = g.node(cid);
    let _ = g.out_neighbors(iid);
    assert_eq!(g.epoch(), e0);
    // Seed the edge's target concept *before* the drain so the post-drain
    // section contains exactly the one edge write. The edge must be
    // type-legal (GRAPH-2 rejects type-invalid endpoints at the write gate —
    // a `Semantic` edge from an interaction was never legal per spec §5).
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();
    // drain does not reset; anchor on the post-drain epoch so the next
    // write is isolated from insert_concept's own bumps.
    let e_before = g.epoch();
    let _ = g.drain_log();
    assert_eq!(g.epoch(), e_before);
    // Next write bumps again.
    g.upsert_edge(edge(1, cid, c2id, EdgeType::Semantic, 0.5))
        .unwrap();
    assert!(g.epoch() > e_before);
}

/// Issue #17: the mutation epoch is durable accounting, not process state.
/// `from_snapshot` resumes it, so a writer restart neither resets GC's
/// `gc_interval` measure (which gates every Swarm Stage 1 promotion
/// through `gc_survived`) nor rewinds the epoch scale the recall cache
/// keys on. Pre-fix, a restarted writer's epoch began at 0 and a
/// low-write deployment never crossed the interval in any single process.
#[test]
fn from_snapshot_resumes_the_mutation_epoch() {
    let (mut g, iid, _cid) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    g.insert_concept(c2, iid).unwrap();
    let epoch_1 = g.epoch();
    assert!(epoch_1 > 0, "test premise: the session has taken mutations");

    // "Restart": re-materialize from the snapshot the way a store load
    // does. The counter resumes exactly where writer 1 left it.
    let mut writer2 = Graph::from_snapshot(g.snapshot()).unwrap();
    assert_eq!(writer2.epoch(), epoch_1, "restart must resume, not reset");
    assert_eq!(writer2.log_len(), 0, "resuming counts nothing new");
    assert_eq!(
        writer2.snapshot().mutation_epoch,
        epoch_1,
        "the snapshot carries the accounting forward"
    );

    // The drained batch stamps the absolute watermark the store persists.
    let batch = writer2.drain_log();
    assert!(batch.is_empty());
    assert_eq!(batch.mutation_epoch, epoch_1);

    // The resumed counter keeps rising, strictly above the pre-restart
    // value — so an epoch-keyed cache entry from before the restart can
    // never collide with post-restart content.
    let c3 = concept(3, iid, "caching layer");
    writer2.insert_concept(c3, iid).unwrap();
    assert!(writer2.epoch() > epoch_1);
}

#[test]
fn drain_log_clears_and_orders_writes() {
    let (mut g, iid, cid) = small_graph();
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();
    g.upsert_edge(edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5))
        .unwrap();
    g.remove_node(c2id).unwrap();

    let batch = g.drain_log();
    assert_eq!(g.log_len(), 0);
    assert!(g.drain_log().is_empty());

    // Ordering contract: every edge's endpoints were upserted earlier in the
    // same batch; deletions follow upserts; node deletions follow their
    // incident edge deletions.
    let mut seen_nodes: HashSet<NodeId> = HashSet::new();
    let mut seen_edges: HashSet<NodeId> = HashSet::new();
    let mut saw_delete = false;
    for m in &batch.mutations {
        match m {
            Mutation::UpsertNode { node } => {
                assert!(!saw_delete, "node upsert after deletion: {m:?}");
                seen_nodes.insert(node.id());
            }
            Mutation::UpsertEdge { edge } => {
                assert!(!saw_delete, "edge upsert after deletion: {m:?}");
                assert!(seen_nodes.contains(&edge.source), "{m:?}");
                assert!(seen_nodes.contains(&edge.target), "{m:?}");
                seen_edges.insert(edge.id);
            }
            Mutation::DeleteEdge { id } => {
                saw_delete = true;
                assert!(seen_edges.contains(id) || seen_nodes.contains(id), "{m:?}");
            }
            Mutation::DeleteNode { id } => {
                assert!(saw_delete, "DeleteNode must follow incident DeleteEdges");
                assert!(seen_nodes.contains(id), "{m:?}");
            }
            // Neither carries graph topology, so neither participates in
            // the endpoint-ordering contract.
            Mutation::CanonizationTransition { .. }
            | Mutation::SetRootGoal { .. }
            | Mutation::SetEmbedding { .. }
            | Mutation::PutWriteIntent { .. }
            | Mutation::ConsumeWriteIntent { .. }
            | Mutation::RecordAccess { .. } => {}
        }
    }
    assert!(saw_delete);
}

/// The requeue half of COH-6 (T81-3): a batch the flush task hands back on
/// stop goes to the **front** of the log — in its original order, ahead of
/// everything written while it sat in the task's pending buffer — and does
/// not bump the epoch.
///
/// Order is the load-bearing part. Appending instead of prepending puts an
/// edge upsert ahead of the `UpsertNode` for one of its endpoints, which
/// breaks the `src/graph/mod.rs` "replay in order, never re-sort" premise a
/// conforming SQL adapter relies on (it would fail the whole final
/// transaction, i.e. lose the tail).
#[test]
fn push_front_log_prepends_the_returned_batch_in_order() {
    let (mut g, iid, cid) = small_graph();

    // What the flush task drained and failed to persist.
    let retained = g.drain_log().mutations;
    assert!(retained.len() >= 2, "need a multi-mutation batch");
    assert_eq!(g.log_len(), 0);

    // Writes that landed while the batch was retained in the task.
    let c2 = concept(2, iid, "auth middleware");
    let c2id = c2.id;
    g.insert_concept(c2, iid).unwrap();
    g.upsert_edge(edge(1, cid, c2id, EdgeType::CoOccurrence, 0.5))
        .unwrap();
    let fresh_len = g.log_len();
    assert!(fresh_len >= 2);
    let epoch_before = g.epoch();

    g.push_front_log(retained.clone());
    assert_eq!(
        g.epoch(),
        epoch_before,
        "requeueing re-counts nothing: the epoch must not move"
    );

    let combined = g.drain_log().mutations;
    assert_eq!(combined.len(), retained.len() + fresh_len);
    assert_eq!(
        &combined[..retained.len()],
        &retained[..],
        "the returned batch must come FIRST, in its original order"
    );

    // The premise that ordering serves: no edge before its endpoints.
    let mut seen: HashSet<NodeId> = HashSet::new();
    for m in &combined {
        match m {
            Mutation::UpsertNode { node } => {
                seen.insert(node.id());
            }
            Mutation::UpsertEdge { edge } => {
                assert!(seen.contains(&edge.source), "endpoint missing: {m:?}");
                assert!(seen.contains(&edge.target), "endpoint missing: {m:?}");
            }
            _ => {}
        }
    }

    // Empty is a no-op, not a panic.
    g.push_front_log(Vec::new());
    assert_eq!(g.log_len(), 0);
}

#[test]
fn mutation_log_is_chronological_across_interleaved_writes() {
    // Adve-review T2.1 M2: §2.4's phase grouping holds *within* a logical
    // write, not across the batch. A node upsert may legally follow a
    // DeleteNode in the same drained batch (create -> delete -> create within
    // one flush interval); adapters replay in order and never re-sort.
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let i1_id = i1.id;
    g.insert_interaction(i1).unwrap();
    let c1 = concept(1, i1_id, "first");
    let c1_id = c1.id;
    g.insert_concept(c1, i1_id).unwrap(); // UpsertNode(c1), UpsertEdge(derives)
    g.remove_node(c1_id).unwrap(); // DeleteEdge(derives), DeleteNode(c1)
    let c2 = concept(2, i1_id, "second");
    g.insert_concept(c2, i1_id).unwrap(); // UpsertNode(c2), UpsertEdge(derives2)

    let batch = g.drain_log();
    let kinds: Vec<&str> = batch
        .mutations
        .iter()
        .map(|m| match m {
            Mutation::UpsertNode { .. } => "upsert_node",
            Mutation::UpsertEdge { .. } => "upsert_edge",
            Mutation::DeleteNode { .. } => "delete_node",
            Mutation::DeleteEdge { .. } => "delete_edge",
            Mutation::CanonizationTransition { .. } => "transition",
            Mutation::SetRootGoal { .. } => "set_root_goal",
            Mutation::SetEmbedding { .. } => "set_embedding",
            Mutation::PutWriteIntent { .. } => "put_write_intent",
            Mutation::ConsumeWriteIntent { .. } => "consume_write_intent",
            Mutation::RecordAccess { .. } => "record_access",
        })
        .collect();
    let expected = [
        "upsert_node", // i1
        "upsert_node", // c1
        "upsert_edge", // derives c1
        "delete_edge", // derives c1
        "delete_node", // c1
        "upsert_node", // c2 — legally AFTER a DeleteNode
        "upsert_edge", // derives c2
    ];
    assert_eq!(kinds, expected);

    // Chronological replay is always safe: every edge references a node
    // upserted earlier in the same batch.
    let mut nodes: HashSet<NodeId> = HashSet::new();
    for m in &batch.mutations {
        match m {
            Mutation::UpsertNode { node } => {
                nodes.insert(node.id());
            }
            Mutation::UpsertEdge { edge } => {
                assert!(nodes.contains(&edge.source), "{m:?}");
                assert!(nodes.contains(&edge.target), "{m:?}");
            }
            _ => {}
        }
    }
}

#[test]
fn bump_gc_survived_increments_and_emits_upserts() {
    let mut g = Graph::new(sid());
    let i1 = interaction(1, None, 0);
    let iid = i1.id;
    g.insert_interaction(i1).unwrap();
    let c1 = concept(1, iid, "survivor one");
    let c1id = c1.id;
    let c2 = concept(2, iid, "survivor two");
    let c2id = c2.id;
    g.insert_concept(c1, iid).unwrap();
    g.insert_concept(c2, iid).unwrap();
    // Discard the insert mutations so the count below isolates the bumps.
    g.drain_log();
    let epoch_before = g.epoch();
    let bumped = g.bump_gc_survived(&[c1id, c2id]);

    assert_eq!(bumped, 2);
    let c1 = match g.node(c1id).unwrap() {
        Node::Concept(c) => c,
        _ => unreachable!(),
    };
    assert_eq!(c1.gc_survived, 1);
    // Every bump emits an UpsertNode so the durable store mirrors it.
    assert!(g.epoch() > epoch_before);
    let batch = g.drain_log();
    let upserts = batch
        .mutations
        .iter()
        .filter(|m| {
            matches!(
                m,
                Mutation::UpsertNode {
                    node: Node::Concept(_)
                }
            )
        })
        .count();
    assert_eq!(upserts, 2);
    // Missing ids are skipped, not fatal.
    assert_eq!(g.bump_gc_survived(&[NodeId::nil()]), 0);
}
