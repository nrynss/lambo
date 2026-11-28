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

/// A write intent of `iid` with receipt `receipt`.
fn intent_of(iid: NodeId, receipt: &str) -> crate::types::WriteIntent {
    crate::types::WriteIntent {
        session_id: sid(),
        receipt: receipt.into(),
        agent: crate::types::AgentId::from("agent-a"),
        interaction: iid,
        lane_seq: 1,
        issued_ms: 0,
        payload: crate::types::WriteIntentPayload::Derive {
            concepts: vec![],
            pairs: vec![],
            receipt_destination: None,
        },
        created_at: ts(0),
        outcome: None,
    }
}

fn applied() -> crate::types::WriteIntentOutcome {
    crate::types::WriteIntentOutcome {
        tag: "applied".into(),
        summary: "applied".into(),
        consumed_at: ts(1),
    }
}

/// A lane worker's consume: the consume and the consumed-unsettled record in
/// one hold, as `WorkerCommit::at_commit` and the failure consume do.
fn worker_consume(g: &mut Graph, receipt: &str) {
    g.consume_write_intent(receipt.into(), applied());
    g.mark_consumed_unsettled(receipt);
}

/// #345 review and re-review: a dead-lettered receipt is kept until its
/// write settles, not until its consume. A worker can panic between the two
/// (the close then sets the write aside), so a consumed, unsettled write
/// stays in the set; its settle forgets it, so the set does not grow for the
/// life of the process.
///
/// Mirrors `Drain.settlePrune_exact`, `Drain.settlePrune_settled_forgotten`.
#[test]
fn a_settled_write_leaves_the_dead_lettered_intents() {
    let (mut g, iid, _cid) = small_graph();
    g.record_write_intent(intent_of(iid, "lwr1.1.1.1"));
    g.record_write_intent(intent_of(iid, "lwr1.1.1.2"));
    let dropped = g.drain_log();
    g.note_dead_lettered_intents(&dropped.mutations);
    assert!(g.intent_dead_lettered("lwr1.1.1.1"));
    assert!(g.intent_dead_lettered("lwr1.1.1.2"));

    worker_consume(&mut g, "lwr1.1.1.1");
    assert!(
        g.intent_dead_lettered("lwr1.1.1.1"),
        "consumed but not settled: a panic here leaves it for the close to set aside"
    );
    g.settled_write("lwr1.1.1.1");
    assert!(
        !g.intent_dead_lettered("lwr1.1.1.1"),
        "a settled write is never set aside: its receipt is forgotten"
    );
    assert!(
        g.intent_dead_lettered("lwr1.1.1.2"),
        "an unsettled dead-lettered intent stays"
    );
}

/// #345 review and re-review: a put whose write was consumed and settled
/// before the dead letter (its consume in the dropped batch, or still in the
/// log behind it) is not noted. One consumed but not settled (its worker
/// panicked after the commit) is noted, consume in the batch or in the log.
///
/// Mirrors `Drain.keptAtConsume_vouches_after_panic` (both cases) and
/// `Drain.settlePrune_exact`.
#[test]
fn only_a_put_settled_before_the_dead_letter_is_skipped() {
    let (mut g, iid, _cid) = small_graph();
    for r in [
        "lwr1.2.1.1",
        "lwr1.2.1.2",
        "lwr1.2.1.3",
        "lwr1.2.1.4",
        "lwr1.2.1.5",
    ] {
        g.record_write_intent(intent_of(iid, r));
    }
    // Consumed inside the batch the dead letter drops: .1 settled, .3 not.
    worker_consume(&mut g, "lwr1.2.1.1");
    g.settled_write("lwr1.2.1.1");
    worker_consume(&mut g, "lwr1.2.1.3");
    let dropped = g.drain_log();
    // Consumed after the drain, before the dead letter: .2 settled, .4 not.
    worker_consume(&mut g, "lwr1.2.1.2");
    g.settled_write("lwr1.2.1.2");
    worker_consume(&mut g, "lwr1.2.1.4");
    g.note_dead_lettered_intents(&dropped.mutations);
    assert!(!g.intent_dead_lettered("lwr1.2.1.1"));
    assert!(!g.intent_dead_lettered("lwr1.2.1.2"));
    assert!(
        g.intent_dead_lettered("lwr1.2.1.3"),
        "consumed in the batch, not settled"
    );
    assert!(
        g.intent_dead_lettered("lwr1.2.1.4"),
        "consumed in the log, not settled"
    );
    assert!(g.intent_dead_lettered("lwr1.2.1.5"), "never consumed");
}
