//! #60: the graph's record of what the durable store has not seen yet.

use super::*;
use crate::types::EmbeddingContract;

fn contract() -> EmbeddingContract {
    EmbeddingContract {
        kind: "unflushed-test".into(),
        model: None,
        dim: 4,
    }
}

/// A write is unflushed from the moment it is appended until a flush whose
/// stamp covers it commits; a write after the drain keeps its entry through
/// the earlier batch's commit.
#[test]
fn a_write_stays_unflushed_until_a_batch_that_covers_it_commits() {
    let (mut g, iid, cid) = small_graph();
    assert!(
        g.is_unflushed(&cid),
        "an inserted concept is not durable yet"
    );
    assert!(!g.is_unflushed(&iid), "only concepts are tracked");

    let first = g.drain_log();
    // A second concept lands after the drain: its epoch is above the stamp.
    let later = concept(2, iid, "billing schema");
    let later_id = later.id;
    g.insert_concept(later, iid).unwrap();

    g.mark_durable_through(first.mutation_epoch);
    assert!(!g.is_unflushed(&cid), "the committed batch carried it");
    assert!(
        g.is_unflushed(&later_id),
        "written after the drain, so not in the committed batch"
    );
    assert_eq!(g.unflushed_len(), 1);

    let second = g.drain_log();
    g.mark_durable_through(second.mutation_epoch);
    assert_eq!(g.unflushed_len(), 0);
}

/// A re-upsert after the drain re-arms an entry the earlier commit would
/// otherwise have cleared: the newer write is not in that batch.
#[test]
fn a_rewrite_after_the_drain_outlives_the_earlier_commit() {
    let (mut g, _iid, cid) = small_graph();
    let batch = g.drain_log();
    g.confirm_human(cid).unwrap();
    g.mark_durable_through(batch.mutation_epoch);
    assert!(g.is_unflushed(&cid));
}

/// A delete is tracked until it is durable (the database still returns the
/// row until then), and a removed concept is not left behind afterwards.
#[test]
fn a_delete_is_unflushed_until_its_batch_commits() {
    let (mut g, _iid, cid) = small_graph();
    let loaded = g.drain_log();
    g.mark_durable_through(loaded.mutation_epoch);
    assert!(!g.is_unflushed(&cid));

    g.remove_node(cid).unwrap();
    assert!(g.is_unflushed(&cid), "the store still holds the row");
    let batch = g.drain_log();
    g.mark_durable_through(batch.mutation_epoch);
    assert_eq!(g.unflushed_ids().count(), 0);
}

/// A batch the flush dropped never reached the store, so a later commit
/// with a higher stamp must not clear it; a later write of the same concept
/// replaces the pin and is durable once its own batch commits.
#[test]
fn a_dropped_batch_stays_unflushed_through_later_commits() {
    let (mut g, iid, cid) = small_graph();
    let dropped = g.drain_log();
    g.pin_unflushed(&dropped.mutations);

    let other = concept(2, iid, "billing schema");
    g.insert_concept(other, iid).unwrap();
    let committed = g.drain_log();
    assert!(committed.mutation_epoch > dropped.mutation_epoch);
    g.mark_durable_through(committed.mutation_epoch);
    assert!(g.is_unflushed(&cid), "a dropped write is never durable");
    assert_eq!(g.unflushed_len(), 1);

    g.confirm_human(cid).unwrap();
    let rewrite = g.drain_log();
    g.mark_durable_through(rewrite.mutation_epoch);
    assert!(!g.is_unflushed(&cid), "the rewrite carried the whole row");
}

/// The contract is tracked the same way: unflushed from the stamp until the
/// batch that carries it commits.
#[test]
fn the_embedding_contract_is_unflushed_until_its_batch_commits() {
    let mut g = Graph::new(sid());
    assert!(!g.contract_unflushed());
    g.stamp_embedding(contract()).unwrap();
    assert!(g.contract_unflushed());
    let batch = g.drain_log();
    g.pin_unflushed(&batch.mutations);
    g.mark_durable_through(u64::MAX - 1);
    assert!(g.contract_unflushed(), "a dropped stamp stays unflushed");

    let mut g = Graph::new(sid());
    g.stamp_embedding(contract()).unwrap();
    let batch = g.drain_log();
    g.mark_durable_through(batch.mutation_epoch);
    assert!(!g.contract_unflushed());
}

/// A loaded session starts with nothing unflushed: what it loaded is durable.
#[test]
fn a_loaded_graph_has_nothing_unflushed() {
    let (g, _iid, _cid) = small_graph();
    let loaded = Graph::from_snapshot(g.snapshot()).unwrap();
    assert_eq!(loaded.unflushed_len(), 0);
    assert!(!loaded.contract_unflushed());
}
