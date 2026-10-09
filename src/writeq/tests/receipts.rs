//! Receipt ids, answers and the counters derived from them.

use super::*;

#[test]
fn a_receipt_id_round_trips_through_its_wire_form() {
    let id = ReceiptId {
        epoch: 0x0123_4567_89ab_cdef,
        issued_ms: 1_755_000_000_123,
        seq: 42,
    };
    let text = id.to_string();
    assert!(text.starts_with("lwr1."), "{text}");
    assert_eq!(text.parse::<ReceiptId>().unwrap(), id);
}

#[test]
fn a_malformed_receipt_id_is_a_parse_error_not_an_answer() {
    // Every one of these must fail to parse rather than resolve to some
    // other session's receipt: a lookup can only classify ids it can read.
    for bad in [
        "",
        "lwr1",
        "lwr2.0.0.0",
        "lwr1.0.0",
        "lwr1.0.0.0.0",
        "lwr1.zz.0.0",
        "not-a-receipt",
    ] {
        assert!(
            bad.parse::<ReceiptId>().is_err(),
            "{bad:?} parsed as a receipt id"
        );
    }
}

/// The `ledger_queued_lines` lesson, re-derived here: the gauge is correct
/// only because a refused admission never enters `accepted`. This is the
/// test the alternative formula fails.
#[test]
fn outstanding_excludes_refusals_because_they_never_reached_accepted() {
    let c = WriteQueueCounters::default();
    c.accepted.fetch_add(10, Ordering::Relaxed);
    c.applied.fetch_add(4, Ordering::Relaxed);
    c.failed.fetch_add(1, Ordering::Relaxed);
    c.abandoned.fetch_add(1, Ordering::Relaxed);
    c.dropped_queue_full.fetch_add(7, Ordering::Relaxed);
    c.dropped_queue_bytes.fetch_add(3, Ordering::Relaxed);

    assert_eq!(c.outstanding(), 5, "10 accepted - 4 applied - 1 failed");
    assert_eq!(c.dropped(), 10, "refusals are counted, just not subtracted");
    // The two formulas a future edit might reach for, and why each is
    // wrong. Written with `saturating_sub` because the first one *panics*
    // on subtract-with-overflow in a debug build otherwise — which is the
    // strongest form of the argument, and the reason `outstanding()` uses
    // saturating arithmetic rather than relying on the invariant.
    assert_ne!(
        c.accepted()
            .saturating_sub(c.applied())
            .saturating_sub(c.failed())
            .saturating_sub(c.dropped()),
        c.outstanding(),
        "subtracting refusals underflows the gauge — they were never accepted"
    );
    assert_ne!(
        c.accepted()
            .saturating_sub(c.applied())
            .saturating_sub(c.failed())
            .saturating_sub(c.abandoned()),
        c.outstanding(),
        "abandoned is a label on a subset of failed, not a fourth term"
    );
}

#[test]
fn abandoned_is_always_a_subset_of_failed() {
    let c = WriteQueueCounters::default();
    c.failed.fetch_add(3, Ordering::Relaxed);
    c.abandoned.fetch_add(3, Ordering::Relaxed);
    assert!(
        c.abandoned() <= c.failed(),
        "every abandoned job is settled failed, so the label can never exceed the class"
    );
}

#[test]
fn replay_blocked_names_the_reason_or_is_none() {
    // J3-R2R-8: the level `replay_owed` cannot tell draining from wedged;
    // `replay_blocked` defaults to `None` and records the class that ended
    // the last replay.
    let c = WriteQueueCounters::default();
    assert_eq!(c.replay_blocked(), ReplayBlockReason::None);
    c.set_replay_blocked(ReplayBlockReason::Embedder);
    assert_eq!(c.replay_blocked(), ReplayBlockReason::Embedder);
    c.set_replay_blocked(ReplayBlockReason::Other);
    assert_eq!(c.replay_blocked(), ReplayBlockReason::Other);
    c.set_replay_blocked(ReplayBlockReason::None);
    assert_eq!(c.replay_blocked(), ReplayBlockReason::None);
}

#[test]
fn every_answer_has_a_distinct_tag_and_none_of_them_is_unknown() {
    let answers = [
        ReceiptAnswer::Pending,
        ReceiptAnswer::PendingReplay,
        ReceiptAnswer::Applied(AppliedSummary {
            kind: WriteKind::Derive,
            summary: "x".into(),
            created: Vec::new(),
            matched: Vec::new(),
            created_count: 0,
            matched_count: 0,
            semantic_merged: Some(0),
            reinforced: Some(0),
            edges: None,
            embedded: Some(0),
        }),
        ReceiptAnswer::Failed("x".into()),
        ReceiptAnswer::AppliedAfterRestart("x".into()),
        ReceiptAnswer::IntentRecorded,
        ReceiptAnswer::Dropped("x".into()),
        ReceiptAnswer::Expired,
        ReceiptAnswer::RestartLost,
        ReceiptAnswer::NeverIssued,
        ReceiptAnswer::Forbidden,
    ];
    // J3-R2R-4: `ordinal` has no `_` arm, so it is exhaustive by
    // construction — if a twelfth variant is ever added, this compiles
    // only when it is named. Asserting eleven distinct ordinals (and
    // eleven distinct tags) turns that mechanical exhaustiveness into the
    // proof that `ReceiptAnswer`'s count stays real.
    let ords: Vec<usize> = answers.iter().map(ReceiptAnswer::ordinal).collect();
    let mut ords_sorted = ords.clone();
    ords_sorted.sort_unstable();
    ords_sorted.dedup();
    assert_eq!(
        ords_sorted.len(),
        11,
        "ordinals must be eleven and distinct: {ords:?}"
    );
    assert_eq!(answers.len(), 11, "the taxonomy holds eleven answers");
    let mut tags: Vec<&str> = answers.iter().map(ReceiptAnswer::tag).collect();
    tags.sort_unstable();
    let mut deduped = tags.clone();
    deduped.dedup();
    assert_eq!(tags, deduped, "two answers share a tag: {tags:?}");
    for a in &answers {
        assert_ne!(a.tag(), "unknown");
        assert!(!a.describe().is_empty());
    }
    // §J3: expired must not read as unknown, and restart-lost must not
    // either. The distinguishing words are in the prose the model reads.
    assert!(ReceiptAnswer::Expired.describe().contains("expired"));
    assert!(ReceiptAnswer::RestartLost
        .describe()
        .contains("different serve process"));
    // Kept word-for-word consistent with the proxy's HUB_LOST_CODE
    // (-32002) wording, which says the same thing about the same hazard.
    assert!(ReceiptAnswer::RestartLost.describe().contains("UNKNOWN"));
    assert!(ReceiptAnswer::RestartLost
        .describe()
        .contains("Recall before re-deriving"));
}

#[test]
fn only_the_two_pendings_are_unsettled() {
    assert!(!ReceiptAnswer::Pending.is_settled());
    assert!(
        !ReceiptAnswer::PendingReplay.is_settled(),
        "PendingReplay is owed a replay and must stay unsettled"
    );
    for a in [
        ReceiptAnswer::Failed("x".into()),
        ReceiptAnswer::AppliedAfterRestart("x".into()),
        // Terminal FOR THIS PROCESS: the next process's answer for the
        // same id is applied_after_restart or failed, but nothing in this
        // one can change it again.
        ReceiptAnswer::IntentRecorded,
        ReceiptAnswer::Dropped("x".into()),
        ReceiptAnswer::Expired,
        ReceiptAnswer::RestartLost,
        ReceiptAnswer::NeverIssued,
        ReceiptAnswer::Forbidden,
    ] {
        assert!(a.is_settled(), "{} should be terminal", a.tag());
    }
}

/// The apply-latency window (#11): nearest-rank percentiles over the most
/// recent [`APPLY_LATENCY_WINDOW`] applied writes, oldest evicted first.
#[test]
fn the_apply_latency_window_reports_recent_percentiles() {
    let mut window = ApplyLatency::default();
    assert!(window.summary().is_none(), "no applied write, no figure");
    for ms in 1..=10u64 {
        window.record(Duration::from_millis(ms));
    }
    let s = window.summary().expect("ten samples");
    assert_eq!(s.samples, 10);
    assert_eq!(s.p50, Duration::from_millis(5));
    assert_eq!(s.p90, Duration::from_millis(9));
    assert_eq!(s.max, Duration::from_millis(10));

    // A full window of fast writes pushes the slow ones out.
    for _ in 0..APPLY_LATENCY_WINDOW {
        window.record(Duration::from_millis(2));
    }
    let s = window.summary().expect("full window");
    assert_eq!(s.samples, APPLY_LATENCY_WINDOW);
    assert_eq!(
        s.max,
        Duration::from_millis(2),
        "the window is recent, not lifetime"
    );

    // One sample is every percentile.
    let mut one = ApplyLatency::default();
    one.record(Duration::from_millis(7));
    let s = one.summary().expect("one sample");
    assert_eq!(
        (s.p50, s.p90, s.max),
        (
            Duration::from_millis(7),
            Duration::from_millis(7),
            Duration::from_millis(7)
        )
    );
}
