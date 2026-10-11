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

/// J2-R2-3 / #118: the refusal must not tell an operator that a dead holder
/// "is still refreshing" its lease, and a refused endpoint must be named as
/// the likely death it is.
#[test]
fn a_dead_holders_refusal_does_not_claim_it_is_still_refreshing() {
    let session = crate::types::SessionId::new("s");
    let now = chrono::Utc::now();
    let current = lease::LeaseInfo {
        // Another host, so the local pid check cannot fire and this pins the
        // probe's branch alone.
        holder: "a@not-this-host.invalid#1".into(),
        token: 1,
        acquired_at: now - chrono::Duration::seconds(4),
        expires_at: now + chrono::Duration::seconds(41),
        endpoint: None,
    };
    let age = Duration::from_secs(4);
    let held = crate::memory::LeaseHeldElsewhere {
        message: crate::memory::lease_held_message(&session, &current, age, false),
        current,
        age,
        store: Arc::new(crate::store::MemoryStore::new()),
    };
    assert!(
        !held.message.contains("still refreshing"),
        "the row alone must not claim liveness: {}",
        held.message
    );
    let refused = format!("{ENDPOINT_NOT_ACCEPTING} (Connection refused (os error 61))");
    let corrected = refusal_with_probe(&held, &session, &refused);
    assert!(
        corrected.contains("most likely died"),
        "the probe's conclusion must be in the refusal: {corrected}"
    );
    assert!(
        corrected.contains("endpoint is not answering") && corrected.contains("expires in 41s"),
        "with the row's facts beside it: {corrected}"
    );
    assert!(
        !corrected.contains("  "),
        "a broken string continuation leaves a double space: {corrected}"
    );
    // Narrow on purpose: every other outcome is a holder this process merely
    // cannot forward to, and the row's facts stand unchanged.
    for other in [
        "That holder published no endpoint, so there is nothing to forward tool calls to",
        "That holder is on another host (a@elsewhere#2)",
        "the holder's endpoint is not safe to dial (a directory check failed)",
    ] {
        assert_eq!(
            refusal_with_probe(&held, &session, other),
            held.message,
            "an unforwardable holder keeps the row-only refusal: {other}"
        );
    }
}
