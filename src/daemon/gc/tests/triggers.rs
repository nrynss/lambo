//! When a sweep is due: the GC clock, the catch-up anchor and the
//! mutation and time triggers.

use super::*;

// ---- the trigger -------------------------------------------------

/// Issue #29 item 3: only a stored time beyond the tolerance counts as
/// "in the future"; an unset mark never does.
#[test]
fn gc_clock_ahead_needs_more_than_the_tolerance() {
    let now = ts(0);
    assert!(!gc_clock_ahead(mark(0, None), now));
    assert!(!gc_clock_ahead(mark(0, Some(now)), now));
    assert!(!gc_clock_ahead(
        mark(0, Some(now - ChronoDuration::days(3))),
        now
    ));
    assert!(!gc_clock_ahead(
        mark(0, Some(now + GC_CLOCK_SKEW_TOLERANCE)),
        now
    ));
    assert!(gc_clock_ahead(
        mark(
            0,
            Some(now + GC_CLOCK_SKEW_TOLERANCE + ChronoDuration::seconds(1))
        ),
        now
    ));
    assert!(gc_clock_ahead(
        mark(0, Some(now + ChronoDuration::days(365))),
        now
    ));
}

fn mark(epoch: u64, at: Option<DateTime<Utc>>) -> crate::types::GcMark {
    crate::types::GcMark {
        last_gc_epoch: epoch,
        last_gc_at: at,
        last_gc_at_reset: false,
    }
}

const DAY: std::time::Duration = std::time::Duration::from_secs(86_400);

/// A never-swept, anchored session (mark epoch 0) measures the floor
/// against its whole lifetime: with at least `gc_idle_floor` mutations it
/// time-sweeps once a full interval after the anchor with no write in
/// between; below the floor it never does. The sweep then moves the mark,
/// so it is once, not daily.
#[test]
fn never_swept_session_over_the_floor_catches_up_once_after_the_anchor() {
    let anchor = ts(0);
    let lifetime = 500u64;
    assert_eq!(
        sweep_due(
            lifetime,
            mark(0, Some(anchor)),
            anchor + ChronoDuration::hours(23),
            10_000,
            DAY,
            100
        ),
        None,
        "not before the interval"
    );
    assert_eq!(
        sweep_due(
            lifetime,
            mark(0, Some(anchor)),
            anchor + ChronoDuration::hours(24),
            10_000,
            DAY,
            100
        ),
        Some(GcTrigger::Elapsed),
        "an idle never-swept session over the floor sweeps one interval after the anchor"
    );
    assert_eq!(
        sweep_due(
            99,
            mark(0, Some(anchor)),
            anchor + ChronoDuration::days(30),
            10_000,
            DAY,
            100
        ),
        None,
        "below the floor it never time-sweeps"
    );
    // After that sweep the mark is (epoch, now): idle again, nothing due.
    let after = anchor + ChronoDuration::hours(24);
    assert_eq!(
        sweep_due(
            lifetime,
            mark(lifetime, Some(after)),
            after + ChronoDuration::days(30),
            10_000,
            DAY,
            100
        ),
        None
    );
}

#[test]
fn sweep_due_mutation_trigger_is_unchanged_and_ungated() {
    // gc_interval mutations since the mark: due, with or without a clock,
    // and regardless of the floor.
    assert_eq!(
        sweep_due(10_000, mark(0, None), ts(0), 10_000, DAY, 100),
        Some(GcTrigger::Mutations)
    );
    assert_eq!(
        sweep_due(10_050, mark(50, Some(ts(0))), ts(1), 10_000, DAY, 20_000),
        Some(GcTrigger::Mutations)
    );
    assert_eq!(
        sweep_due(9_999, mark(0, Some(ts(0))), ts(1), 10_000, DAY, 100_000),
        None
    );
}

#[test]
fn sweep_due_time_trigger_needs_the_interval_and_the_floor() {
    let at = ts(0);
    let day_later = at + ChronoDuration::days(1);
    // Elapsed and over the floor.
    assert_eq!(
        sweep_due(600, mark(500, Some(at)), day_later, 10_000, DAY, 100),
        Some(GcTrigger::Elapsed)
    );
    // Elapsed, one mutation short of the floor: an idle session does not
    // sweep on time alone.
    assert_eq!(
        sweep_due(599, mark(500, Some(at)), day_later, 10_000, DAY, 100),
        None
    );
    // Over the floor, one second short of the interval.
    assert_eq!(
        sweep_due(
            600,
            mark(500, Some(at)),
            day_later - ChronoDuration::seconds(1),
            10_000,
            DAY,
            100
        ),
        None
    );
    // Never anchored: the time trigger cannot fire.
    assert_eq!(
        sweep_due(600, mark(500, None), day_later, 10_000, DAY, 100),
        None
    );
    // Clock behind the mark: not elapsed.
    assert_eq!(
        sweep_due(
            600,
            mark(500, Some(at)),
            at - ChronoDuration::days(3),
            10_000,
            DAY,
            100
        ),
        None
    );
    // Thirty days down: due once — and once the sweep is recorded, not
    // again (no backlog).
    let late = at + ChronoDuration::days(30);
    assert_eq!(
        sweep_due(600, mark(500, Some(at)), late, 10_000, DAY, 100),
        Some(GcTrigger::Elapsed)
    );
    assert_eq!(
        sweep_due(700, mark(700, Some(late)), late, 10_000, DAY, 100),
        None
    );
}
