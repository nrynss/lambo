//! J2-R2-2 / J2-R2-3: the election budget and the refusal it composes.

use super::*;

// -----------------------------------------------------------------------
// J2-R2-2 / J2-R2-3 — the election's arithmetic and the refusal it composes
// -----------------------------------------------------------------------

/// J2-R2-2: the claim [`ELECTION_BUDGET`]'s docstring makes, asserted.
///
/// The docstring used to say the wait "still succeeds" in "the majority of
/// real cases" because "a lease expires uniformly somewhere inside its TTL".
/// It does not expire uniformly: every refresh sets `expires_at` to
/// `now + LEASE_TTL` and refreshes come every `LEASE_HEARTBEAT_INTERVAL`, so
/// an abrupt death leaves at least `LEASE_TTL - LEASE_HEARTBEAT_INTERVAL`.
/// This test is what makes that arithmetic falsifiable instead of asserted in
/// prose — moving any of the three constants into a shape where a prompt
/// restart could be waited out turns it red.
#[test]
fn an_abrupt_holder_death_outlasts_the_election_budget() {
    // The whole reachable range after an abrupt death, both ends.
    let least = lease::LEASE_TTL - lease::LEASE_HEARTBEAT_INTERVAL;
    assert_eq!(least, Duration::from_secs(30), "the floor moved");
    for lapses_in in [least, lease::LEASE_TTL] {
        assert!(
            !waiting_fits(lapses_in, ELECTION_BUDGET),
            "a client starting promptly after an abrupt holder death must be refused, not \
                 waited out: {lapses_in:?} of lease against a {ELECTION_BUDGET:?} budget"
        );
    }
    // And the window that IS waited out, at its exact boundary: a client
    // arriving late enough that only the slack separates the lapse from the
    // budget.
    let widest = ELECTION_BUDGET - ELECTION_SLACK;
    assert!(
        waiting_fits(widest, ELECTION_BUDGET),
        "the largest lapse the budget can absorb must still be waited out"
    );
    assert!(
        !waiting_fits(widest + Duration::from_secs(1), ELECTION_BUDGET),
        "one second past it must refuse"
    );
    // The floor is above the widest waitable lapse — which is the whole
    // finding in one line.
    assert!(
        least > widest,
        "if this ever inverts, the docstring's 'a prompt start is refused by design' is \
             no longer true and must be rewritten with it"
    );
}

/// J2-R2-3: the refusal must not tell an operator that a dead holder "is
/// still refreshing" its lease.
///
/// The false clause came *first* in the composed paragraph and the probe's
/// contradiction second, so the opening sentence sent an operator looking for
/// a process that no longer exists.
#[test]
fn a_dead_holders_refusal_does_not_claim_it_is_still_refreshing() {
    let held = format!(
        "session s is already held by another writer (a@h#1) — it acquired the \
             single-writer lease 4s ago and {}. Refusing to open a second writer.",
        crate::memory::STILL_REFRESHING_CLAUSE
    );
    let refused = format!("{ENDPOINT_NOT_ACCEPTING} (Connection refused (os error 61))");
    let corrected = correct_the_refresh_claim(&held, &refused);
    assert!(
        !corrected.contains(crate::memory::STILL_REFRESHING_CLAUSE),
        "the probe is the better evidence and the claim must go: {corrected}"
    );
    assert!(
        corrected.contains("most likely died"),
        "and it must be replaced by what the probe actually found: {corrected}"
    );
    // J2-R1-9's rule, applied to the new literal: a continuation that lost
    // its `\\` shows up as a double space, and a phrase that spans one shows
    // up nowhere. Assert both.
    assert!(
        !corrected.contains("  "),
        "a broken string continuation leaves a double space: {corrected}"
    );
    assert!(
        corrected.contains("endpoint is not answering"),
        "the phrase spanning the continuation must survive it: {corrected}"
    );
    // Narrow on purpose: every other refusal is a LIVE holder this process
    // merely cannot forward to, and the clause is true for it.
    for other in [
        "That holder published no endpoint, so there is nothing to forward tool calls to",
        "That holder is on another host (a@elsewhere#2)",
        "the holder's endpoint is not safe to dial (a directory check failed)",
    ] {
        assert!(
            correct_the_refresh_claim(&held, other)
                .contains(crate::memory::STILL_REFRESHING_CLAUSE),
            "a live-but-unforwardable holder IS still refreshing its lease: {other}"
        );
    }
}
