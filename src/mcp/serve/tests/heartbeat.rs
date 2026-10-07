//! I1 / I2: ledger flags, the heartbeat timer and the refusal poller.

use super::*;

// -----------------------------------------------------------------------
// I1 / I2 — ledger flags and the heartbeat timer
// -----------------------------------------------------------------------

/// **I1.** Both ledger knobs are off in the default options: `--ledger` is
/// opt-in, and nobody who did not ask for it gets a writer thread.
#[test]
fn i1_the_ledger_is_off_in_the_default_serve_options() {
    let opts = ServeOptions::new("s", "a");
    assert!(opts.ledger.is_none(), "no ledger path by default");
    assert!(opts.ledger_heartbeat.is_none(), "no heartbeat by default");
    assert!(
        authorize_ledger(&opts).is_ok(),
        "the default options are a legal configuration"
    );
}

/// **I2.** `--ledger-heartbeat` without `--ledger` is refused at startup
/// with a message that names the fix, rather than accepted as a no-op that
/// writes heartbeats nowhere.
#[test]
fn i2_a_heartbeat_without_a_ledger_is_refused_and_says_why() {
    let mut opts = ServeOptions::new("s", "a");
    opts.ledger_heartbeat = Some(Duration::from_secs(60));
    let err = authorize_ledger(&opts).expect_err("must refuse");
    let msg = err.to_string();
    assert!(msg.contains("--ledger"), "names the missing flag: {msg}");
    assert!(
        msg.contains("60"),
        "quotes the interval it was given: {msg}"
    );

    // With a path, the same interval is fine.
    opts.ledger = Some(std::path::PathBuf::from("/tmp/nonexistent/calls.jsonl"));
    assert!(authorize_ledger(&opts).is_ok(), "a path makes it legal");

    // A ledger with no heartbeat is also fine — heartbeats are optional.
    opts.ledger_heartbeat = None;
    assert!(authorize_ledger(&opts).is_ok());
}

/// **I-R1-12.** A zero heartbeat interval is refused at the *library*
/// boundary, not only by the CLI.
///
/// `tokio::time::interval` panics on a zero period, so `serve()` used to hand
/// a non-CLI caller a heartbeat task that panicked on its first tick while
/// the CLI refused the same options with a different exit code from the
/// heartbeat-without-ledger case. One check, one path out, for both.
#[test]
fn i2_a_zero_heartbeat_interval_is_refused_at_the_library_boundary() {
    let mut opts = ServeOptions::new("s", "a");
    opts.ledger = Some(std::path::PathBuf::from("/tmp/nonexistent/calls.jsonl"));
    opts.ledger_heartbeat = Some(Duration::ZERO);
    let err = authorize_ledger(&opts).expect_err("a zero interval must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("at least 1 second"),
        "keeps the CLI's wording: {msg}"
    );
    assert!(
        matches!(err, LamboError::Config(_)),
        "a configuration error, so it exits the way the other one does: {err:?}"
    );

    // One second is the smallest legal interval.
    opts.ledger_heartbeat = Some(Duration::from_secs(1));
    assert!(authorize_ledger(&opts).is_ok());

    // And zero is refused with no ledger too — that arm reports the missing
    // flag first, which is the more useful of the two messages.
    opts.ledger = None;
    opts.ledger_heartbeat = Some(Duration::ZERO);
    assert!(authorize_ledger(&opts).is_err());
}

/// **I2 acceptance.** The heartbeat interval actually fires, repeatedly, on
/// the interval it was given — asserted on a paused clock so the test pins
/// the *period* rather than racing a wall-clock sleep.
// Same gate the other Memory-building tests in this module use: the store
// and embedder it needs are feature-gated, and a bare `#[cfg(test)]` would
// not compile under `--no-default-features --features store-sqlite`.
#[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
#[tokio::test(start_paused = true)]
async fn i2_the_heartbeat_fires_on_its_interval() {
    let dir = crate::test_util::ScratchDir::new("lambo-i2-hb");
    let path = dir.join("calls.jsonl");
    let ledger = Ledger::open(&path);

    let mem = Arc::new(
        Memory::builder()
            .session("i2-heartbeat-timer")
            .agent("agent-a")
            .flush_interval(Duration::from_secs(3_600))
            .store(Arc::new(crate::store::MemoryStore::new()) as Arc<dyn crate::store::GraphStore>)
            .embedder(
                Arc::new(crate::embed::FixtureEmbedder::new()) as Arc<dyn crate::embed::Embedder>
            )
            .embedding_contract(crate::types::EmbeddingContract {
                kind: "fixture".into(),
                model: None,
                dim: 1024,
            })
            .build()
            .await
            .expect("build"),
    );
    let server = LamboServer::with_ledger(mem.clone(), Arc::clone(&ledger));

    let every = Duration::from_secs(30);
    let task = tokio::spawn(heartbeat_loop(server, Arc::clone(&ledger), every));

    // The writer is a real OS thread, so each step yields to it until the
    // count lands rather than assuming an instant write.
    async fn wait_for(ledger: &Ledger, want: u64) -> u64 {
        for _ in 0..2_000 {
            if ledger.counters().written() >= want {
                break;
            }
            tokio::task::yield_now().await;
            std::thread::sleep(Duration::from_millis(1));
        }
        ledger.counters().written()
    }

    // The first beat is immediate — it stamps the binary's identity at the
    // moment the session attached, so the first stretch of ledger is not
    // left unattributed.
    assert_eq!(wait_for(&ledger, 1).await, 1, "the first beat is immediate");

    // Advancing by less than the interval must NOT produce a beat.
    tokio::time::advance(every / 2).await;
    assert_eq!(
        ledger.counters().written(),
        1,
        "half an interval is not a beat"
    );

    // Crossing the interval produces exactly one more, twice over.
    tokio::time::advance(every).await;
    assert_eq!(wait_for(&ledger, 2).await, 2, "one beat per interval");
    tokio::time::advance(every).await;
    assert_eq!(wait_for(&ledger, 3).await, 3, "and again");

    task.abort();
    ledger.shutdown();

    // Every beat is a `stats` line carrying the sha.
    let text = std::fs::read_to_string(&path).expect("ledger file");
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    for line in lines {
        let v: serde_json::Value = serde_json::from_str(line).expect("parses");
        assert_eq!(v["kind"], serde_json::json!("stats"), "{line}");
        assert_eq!(
            v["git_sha"],
            serde_json::json!(crate::ledger::GIT_SHA),
            "an upgrade shows here as a sha change: {line}"
        );
        assert_eq!(
            v["version"],
            serde_json::json!(crate::ledger::VERSION),
            "{line}"
        );
    }
    mem.close().await.expect("close");
}

/// **JE2E-1.** The holder's refusal poller must not re-read its whole
/// history every 500 ms, and its dedup set must not grow with uptime.
///
/// The founding scenario is a client that auto-respawns a losing serve, so
/// "one row per respawn, forever, re-read on every poll" is the shape that
/// matters. Three properties, asserted over the extracted bookkeeping
/// because the loop around it is an infinite `sleep`:
///
/// 1. the window advances with the newest row seen (quadratic re-reads gone);
/// 2. a re-delivered row is not logged twice (the dedup the overlap costs);
/// 3. the dedup set holds only the overlap window, never the history.
///
/// Since JE2E-R2-5 the read starts [`REFUSAL_OVERLAP_SECS`] *below* the
/// cursor, so re-delivery is the normal case rather than an edge; the last
/// section pins what that buys and what it still does not.
#[test]
fn the_refusal_pollers_window_advances_and_its_dedup_set_stays_bounded() {
    fn refusal(secs: i64, by: &str, holder: &str) -> crate::store::lease::LeaseRefusal {
        crate::store::lease::LeaseRefusal {
            session: crate::types::SessionId::new("s"),
            at: chrono::DateTime::from_timestamp(1_800_000_000 + secs, 0).expect("stamp"),
            refused_by: by.to_string(),
            current_holder: holder.to_string(),
        }
    }
    let start = chrono::DateTime::from_timestamp(1_800_000_000, 0).expect("stamp");
    let mut cursor = RefusalCursor::starting_at(start);

    // Poll 1: two refusals against us, one against a previous holder.
    let new = cursor.take_new(
        vec![
            refusal(1, "b@h#1", "me"),
            refusal(2, "c@h#1", "me"),
            refusal(3, "d@h#1", "someone-else"),
        ],
        "me",
    );
    assert_eq!(new.len(), 2, "both of ours are new: {new:?}");
    assert_eq!(
        cursor.since(),
        refusal(2 - REFUSAL_OVERLAP_SECS, "", "").at,
        "the window advances with OUR newest row — not past it, and not onto \
             another holder's — less the overlap it deliberately re-reads"
    );
    assert_eq!(
        cursor.seen.len(),
        2,
        "the set holds the overlap window, not the history: {:?}",
        cursor.seen
    );

    // Poll 2: the store re-delivers everything in the overlap. None of it
    // may be logged again.
    let new = cursor.take_new(
        vec![refusal(1, "b@h#1", "me"), refusal(2, "c@h#1", "me")],
        "me",
    );
    assert!(new.is_empty(), "re-delivered rows are not new: {new:?}");
    assert_eq!(
        cursor.since(),
        refusal(2 - REFUSAL_OVERLAP_SECS, "", "").at,
        "and the window holds"
    );

    // Poll 3: a hundred fresh respawns, each a new instant. Every one is
    // logged once, the window ends on the last, and the set is still one.
    let burst: Vec<_> = (3..103)
        .map(|n| refusal(n, &format!("r{n}@h#1"), "me"))
        .collect();
    let new = cursor.take_new(burst, "me");
    assert_eq!(new.len(), 100, "every respawn is recorded once");
    assert_eq!(
        cursor.since(),
        refusal(102 - REFUSAL_OVERLAP_SECS, "", "").at
    );
    assert!(
        cursor.seen.len() <= 2,
        "a hundred refusals later the dedup set still holds only the overlap window — \
             this is the unbounded-growth half of JE2E-1: {:?}",
        cursor.seen
    );

    // Two refusals stamped at the SAME store instant are both recorded, and
    // both are remembered — which is why the cursor lands on the newest row
    // rather than past it.
    let mut cursor = RefusalCursor::starting_at(start);
    let new = cursor.take_new(
        vec![refusal(5, "x@h#1", "me"), refusal(5, "y@h#1", "me")],
        "me",
    );
    assert_eq!(new.len(), 2, "one instant, two refusals, both recorded");
    assert_eq!(cursor.seen.len(), 2);
    assert!(
        cursor
            .take_new(vec![refusal(5, "y@h#1", "me")], "me")
            .is_empty(),
        "and neither is re-recorded on the next poll"
    );
}

/// **JE2E-R2-5.** A refusal whose row becomes *visible* after the cursor has
/// already passed its stamp must still be logged.
///
/// `refused_at` is the store's clock at statement time; visibility is
/// commit-ordered. On Cockroach a slow-committing INSERT (push-with-refresh
/// keeps the evaluated `now()`) therefore surfaces a row stamped *earlier*
/// than one that committed before it. A cursor that only moved forward
/// excluded that row with `refused_at >= since` **forever** — the fix for
/// JE2E-1's unbounded re-read traded an unbounded window for a permanent
/// skip, which is a worse defect on a rarer path.
///
/// Both halves are asserted here, because the overlap is a *bound*, not a
/// cure: a row late by less than the overlap is recovered, and one late by
/// more is not. The second assertion is the residual the docstring states,
/// pinned so it cannot quietly become something else.
#[test]
fn a_late_committing_refusal_below_the_cursor_is_still_logged() {
    // Stamps in MILLISECONDS, and the lateness below is an absolute figure
    // rather than one derived from `REFUSAL_OVERLAP_SECS`. That is
    // deliberate: a test whose input moves with the constant it is testing
    // cannot see the constant change — shrinking the overlap to zero would
    // shrink the "late" row's lateness to zero with it and stay green. The
    // fixture has to oppose the constant, not track it.
    fn refusal(millis: i64, by: &str) -> crate::store::lease::LeaseRefusal {
        crate::store::lease::LeaseRefusal {
            session: crate::types::SessionId::new("s"),
            at: chrono::DateTime::from_timestamp_millis(1_800_000_000_000 + millis).expect("stamp"),
            refused_by: by.to_string(),
            current_holder: "me".to_string(),
        }
    }
    // 500 ms of commit lag sits inside a one-second overlap and outside a
    // zero-second one, which is what makes the mutation visible.
    const LATE_MS: i64 = 500;
    // Compile-time, so shrinking the overlap fails the BUILD rather than
    // letting this test quietly become vacuous. (With this guard bypassed,
    // the behavioural assertion below fires on its own — verified.)
    const _: () = assert!(
        REFUSAL_OVERLAP_SECS * 1_000 > LATE_MS,
        "this test's lateness must sit INSIDE the overlap, or its assertions are about \
             nothing"
    );
    let start = chrono::DateTime::from_timestamp_millis(1_800_000_000_000).expect("stamp");

    // One poll, modelled the way the loop actually runs it: the STORE
    // filters by `refused_at >= since` and hands over what is left. The
    // filter is the half that skips, so a test calling `take_new` directly
    // with rows the store would never return proves nothing about the
    // window — it would assert the dedup, twice.
    fn poll(
        cursor: &mut RefusalCursor,
        rows: &[crate::store::lease::LeaseRefusal],
    ) -> Vec<crate::store::lease::LeaseRefusal> {
        let since = cursor.since();
        let delivered: Vec<_> = rows.iter().filter(|r| r.at >= since).cloned().collect();
        cursor.take_new(delivered, "me")
    }

    let mut cursor = RefusalCursor::starting_at(start);

    // L2 commits fast and is seen; the cursor advances onto its stamp (10).
    assert_eq!(
        poll(&mut cursor, &[refusal(10_000, "L2@h#1")]).len(),
        1,
        "the fast committer is logged"
    );

    // L1 started earlier — stamped 10 - overlap — and only now becomes
    // visible, BELOW the advanced cursor. Under a forward-only cursor the
    // store's own predicate excludes it for the rest of this process's life;
    // the overlap is what keeps it inside the read.
    let late = refusal(10_000 - LATE_MS, "L1@h#1");
    let new = poll(&mut cursor, &[refusal(10_000, "L2@h#1"), late]);
    assert_eq!(
        new.len(),
        1,
        "the late committer is logged and the re-delivered one is not: {new:?}"
    );
    assert_eq!(new[0].refused_by, "L1@h#1");

    // And the residual, pinned at its own magnitude: later than the overlap
    // is still never logged, because the store never delivers it. This is
    // the sentence the docstring owes a reader — "older rows cannot be
    // RE-logged" was true and incomplete; they can also be never logged.
    let mut cursor = RefusalCursor::starting_at(start);
    poll(&mut cursor, &[refusal(10_000, "L2@h#1")]);
    let too_late = refusal(10_000 - REFUSAL_OVERLAP_SECS * 1_000 - 1, "L0@h#1");
    assert!(
        too_late.at < cursor.since(),
        "a row this old is outside the read window by construction"
    );
    assert!(
        poll(&mut cursor, &[too_late]).is_empty(),
        "beyond the overlap the skip is permanent — bounded by the constant, not cured"
    );
}
