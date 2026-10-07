//! Vector and keyword query logic: scoring, ties, fetch growth, session
//! filtering and transaction retries.

use super::*;

#[test]
fn age_cutoff_computation() {
    let now = Utc.with_ymd_and_hms(2026, 8, 11, 12, 0, 0).unwrap();
    assert_eq!(cutoff(now, Duration::ZERO).unwrap(), now);
    assert_eq!(
        cutoff(now, Duration::from_secs(3600)).unwrap(),
        now - chrono::Duration::hours(1)
    );
    assert_eq!(
        cutoff(now, Duration::from_secs(90)).unwrap(),
        now - chrono::Duration::seconds(90)
    );
    // Out-of-range (chrono i64 seconds) -> typed error, not panic.
    assert!(cutoff(now, Duration::from_secs(u64::MAX)).is_err());
}

#[test]
fn distance_to_score_is_cosine() {
    use CockroachDialect as C;
    assert_eq!(C::distance_to_score(0.0), 1.0);
    assert!((C::distance_to_score(1.0) - 0.5).abs() < 1e-12);
    assert_eq!(C::distance_to_score(2.0), -1.0);
    assert_eq!(C::distance_to_score(3.0), -1.0, "clamped");
    assert!((C::distance_to_score(0.5) - 0.875).abs() < 1e-12);
    // B3 vice-versa pin: copying Postgres `1 - d` onto Cockroach goes red.
    // Postgres at d=1 is 0.0 and at d=0.5 is 0.5.
    assert_ne!(
        C::distance_to_score(1.0),
        0.0,
        "copied Postgres 1 - d onto Cockroach L2"
    );
    assert_ne!(
        C::distance_to_score(0.5),
        0.5,
        "copied Postgres 1 - d onto Cockroach L2"
    );
    assert!(
        C::forced_exact_scan_sql().is_none(),
        "Cockroach has no H3 forced-exact GUC"
    );
}

#[test]
fn session_filter_keeps_only_caller_and_preserves_order() {
    // Rows arrive from SQL in L2-distance-ascending order (score-descending).
    // Foreign-session rows must be dropped without reordering the survivors.
    let sid = SessionId::from("caller-session");
    let (a, b, c) = (NodeId::new(), NodeId::new(), NodeId::new());
    let (fx, fy) = (NodeId::new(), NodeId::new());
    let foreign = "other-session".to_string();
    let mine = sid.0.clone();
    // dist asc: a(0.0), fx(0.5), b(1.0), fy(1.5), c(2.0)
    let rows = vec![
        (a, 0.0, mine.clone(), "a".to_string()),
        (fx, 0.5, foreign.clone(), "fx".to_string()),
        (b, 1.0, mine.clone(), "b".to_string()),
        (fy, 1.5, foreign.clone(), "fy".to_string()),
        (c, 2.0, mine.clone(), "c".to_string()),
    ];
    let got = filter_session_rows::<CockroachDialect>(&sid, &rows);
    let items: Vec<_> = got.iter().map(|s| s.item).collect();
    assert_eq!(
        items,
        vec![a, b, c],
        "foreign rows dropped, order preserved"
    );
    // Scores follow distance_to_score: 0.0→1.0, 1.0→0.5, 2.0→-1.0 (descending).
    assert!((got[0].score - 1.0).abs() < 1e-12);
    assert!((got[1].score - 0.5).abs() < 1e-12);
    assert!((got[2].score - (-1.0)).abs() < 1e-12);
}

/// Issue #2: at an exact distance tie the pg-family order is canonical key
/// ascending, node id ascending only behind that. The keys here contradict
/// the id order on purpose, so the old id-first chain fails this test.
#[test]
fn vector_ties_are_ordered_by_canonical_key_or_trigger_exact_fallback() {
    let sid = SessionId::from("ties");
    let low = NodeId(Uuid::from_u64_pair(0, 1));
    let mid = NodeId(Uuid::from_u64_pair(0, 2));
    let high = NodeId(Uuid::from_u64_pair(0, 3));
    let rows_a = vec![
        (high, 0.25, sid.0.clone(), "alpha".to_string()),
        (low, 0.25, sid.0.clone(), "gamma".to_string()),
        (mid, 0.25, sid.0.clone(), "beta".to_string()),
    ];
    let mut rows_b = rows_a.clone();
    rows_b.reverse();
    let ids = |rows: &[(NodeId, f64, String, String)]| {
        filter_session_rows::<CockroachDialect>(&sid, rows)
            .into_iter()
            .map(|s| s.item)
            .collect::<Vec<_>>()
    };
    // "alpha" (high) < "beta" (mid) < "gamma" (low): key order, not id order.
    assert_eq!(ids(&rows_a), vec![high, mid, low]);
    assert_eq!(ids(&rows_b), vec![high, mid, low]);

    // More equal-distance rows than the fetch window: k+1 exposes that the
    // kth subset is arbitrary (the key tie-break cannot recover rows SQL
    // never returned), so the caller must use exact fallback.
    assert!(has_boundary_tie(&rows_a, 2));
    assert!(has_boundary_tie(&rows_b, 2));
    assert!(crdb_sql()
        .session_vector_candidates
        .contains("ORDER BY dist ASC, id ASC"));
    assert!(!has_boundary_tie(
        &[
            (low, 0.1, sid.0.clone(), "alpha".to_string()),
            (mid, 0.2, sid.0.clone(), "beta".to_string()),
            (high, 0.3, sid.0.clone(), "gamma".to_string()),
        ],
        2
    ));
}

#[test]
fn grow_retry_is_final_when_satisfied_exhausted_or_capped() {
    // Satisfied: enough in-session hits -> final, no retry.
    assert_eq!(next_fetch_k(3, true, 30, 3), None);
    // Exhausted: no k+1 lookahead -> no more global rows exist.
    assert_eq!(next_fetch_k(1, false, 30, 3), None);
    // Capped: k already at the cap -> never grow past it.
    assert_eq!(next_fetch_k(0, true, VECTOR_FETCH_CAP, 5), None);
    // Page full + under-delivered + room to grow -> double (capped at VECTOR_FETCH_CAP).
    assert_eq!(
        next_fetch_k(1, true, 30, 5),
        Some(60),
        "grow retry doubles k"
    );
    let near_cap = VECTOR_FETCH_CAP / 2;
    assert_eq!(
        next_fetch_k(1, true, near_cap, 5),
        Some(VECTOR_FETCH_CAP),
        "growth clamps at the cap"
    );
}

#[test]
fn cap_crowd_out_uses_exact_session_fallback() {
    // Adversarial distribution: 2,048 closer foreign-session rows put the
    // caller's nearest concept at global rank 2,049. The capped fast path
    // must not silently return empty; it switches to the exact session query.
    let caller = SessionId::from("caller");
    let mut globally_ranked: Vec<(NodeId, f64, String, String)> = (0..VECTOR_FETCH_CAP)
        .map(|rank| {
            (
                NodeId::new(),
                rank as f64 / 10_000.0,
                "foreign".to_string(),
                format!("foreign-{rank}"),
            )
        })
        .collect();
    globally_ranked.push((NodeId::new(), 0.3, caller.0.clone(), "local".to_string()));
    let capped_page = &globally_ranked[..VECTOR_FETCH_CAP];
    let local = filter_session_rows::<CockroachDialect>(&caller, capped_page);
    assert!(local.is_empty(), "local row is exactly global rank 2,049");
    assert!(needs_session_fallback(
        local.len(),
        true,
        VECTOR_FETCH_CAP,
        1
    ));
    let crdb = crdb_sql();
    assert!(crdb
        .session_vector_candidates
        .contains("WHERE session_id = $2"));
    assert!(crdb
        .session_vector_candidates
        .contains("ORDER BY dist ASC, id ASC"));

    assert!(!needs_session_fallback(1, true, VECTOR_FETCH_CAP, 1));
    assert!(!needs_session_fallback(0, false, VECTOR_FETCH_CAP, 1));
}

#[test]
fn initial_fetch_k_is_floor_but_capped_at_same_bound_as_growth() {
    // The BASE global fetch must be floored at the multiplier (a non-trivial
    // query always pulls some headroom) and CAPPED at VECTOR_FETCH_CAP — the
    // same worst-case bound the growth step enforces. The public caller validates
    // `limit`; this pure helper still saturates defensively. `limit == 0` is
    // short-circuited by the caller, so 0 maps to
    // the floor here.
    assert_eq!(
        initial_fetch_k(0),
        VECTOR_FETCH_MULTIPLIER,
        "0 floors at multiplier"
    );
    assert_eq!(
        initial_fetch_k(1),
        VECTOR_FETCH_MULTIPLIER,
        "floor at multiplier"
    );
    assert_eq!(initial_fetch_k(5), 50);
    assert_eq!(initial_fetch_k(7), 70);
    let over = VECTOR_FETCH_CAP / VECTOR_FETCH_MULTIPLIER + 1;
    assert_eq!(
        initial_fetch_k(over),
        VECTOR_FETCH_CAP,
        "limit just over cap clamps"
    );
    assert_eq!(
        initial_fetch_k(usize::MAX),
        VECTOR_FETCH_CAP,
        "defensive saturation clamps to the cap"
    );
}

#[test]
fn tx_retryable_is_structured_not_substring() {
    // STORE-4: the tx-replay decision matches the TYPED error — never
    // message text. Constraint violations (SQLSTATE 23xxx) are
    // deterministic: never replayed, dead-lettered upstream. Typed
    // variants are permanent. A Backend error may be a transient
    // (serialization conflict, connection exception, server shutdown);
    // the replay is bounded by TX_RETRY_ATTEMPTS + backoff, so a
    // non-constraint Backend (e.g. a schema bug) at worst re-runs the tx
    // body a bounded number of times before surfacing.
    assert!(!tx_retryable(&StoreError::Constraint("23505".into())));
    assert!(!tx_retryable(&StoreError::SessionNotFound("x".into())));
    assert!(!tx_retryable(&StoreError::Capability("nope".into())));
    assert!(!tx_retryable(&StoreError::NotFound("nope".into())));
    assert!(!tx_retryable(&StoreError::Invariant("nope".into())));
    assert!(tx_retryable(&StoreError::Backend(
        "restart transaction: TransactionRetryWithProtoRefreshError: TransactionRetryError: \
             retry txn (RETRY_SERIALIZABLE - failed preemptive refresh...)"
            .into(),
    )));
    assert!(tx_retryable(&StoreError::Backend(
        "db error: SQLSTATE 40001".into()
    )));
    assert!(tx_retryable(&StoreError::Backend(
        "relation \"concepts\" does not exist".into()
    )));
}

#[tokio::test]
async fn checked_vector_transaction_retries_backend_but_not_contract_mismatch() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    // Models the complete checked candidate transaction closure: the first
    // serializable attempt is aborted with SQLSTATE 40001 and the whole body
    // is invoked again, not resumed after the failed statement.
    let attempts = AtomicUsize::new(0);
    let value = tx_retry(|| {
        let attempt = attempts.fetch_add(1, Ordering::SeqCst);
        async move {
            if attempt == 0 {
                Err(StoreError::Backend("db error: SQLSTATE 40001".into()))
            } else {
                Ok(42usize)
            }
        }
    })
    .await
    .unwrap();
    assert_eq!(value, 42);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);

    // The checked read maps a durable/query contract mismatch to Invariant,
    // so it is deterministic and returned on the first attempt.
    let mismatch_attempts = AtomicUsize::new(0);
    let err = tx_retry(|| {
        mismatch_attempts.fetch_add(1, Ordering::SeqCst);
        async {
            Err::<(), _>(StoreError::Invariant(
                "vector candidate lookup refused after embedding contract changed".into(),
            ))
        }
    })
    .await
    .unwrap_err();
    assert!(matches!(err, StoreError::Invariant(_)));
    assert_eq!(mismatch_attempts.load(Ordering::SeqCst), 1);
}

#[test]
fn keyword_score_folds_case_like_memory_store() {
    // Regression (P3 review R1): the SQL predicate lowercases the columns, so the
    // score must fold the row text the same way — a mixed-case row ("Register
    // User") selected by token "register" scores its hits, not 0.0.
    assert_eq!(
        score_keyword_hits("Register User", "Register User", &["register".into()]),
        1,
        "mixed-case content + key must still count the lowercase token"
    );
    assert_eq!(
        score_keyword_hits(
            "Register User",
            "Register User",
            &["register".into(), "user".into()]
        ),
        2
    );
    assert_eq!(
        score_keyword_hits("Register User", "Register User", &["schema".into()]),
        0
    );
    assert_eq!(
        score_keyword_hits("register user", "register user", &["register".into()]),
        1
    );
    // Key-only hit still counts (SQL predicate is content OR canonical_key).
    assert_eq!(
        score_keyword_hits("Foo", "register user", &["register".into()]),
        1
    );
    // Tokens are pre-normalized lowercase; an uppercase token matches nothing.
    assert_eq!(
        score_keyword_hits("register user", "register user", &["Register".into()]),
        0
    );
    // Empty rows/tokens (post-normalization) contribute nothing.
    assert_eq!(score_keyword_hits("", "", &["a".into()]), 0);
}

#[test]
fn normalize_tokens_matches_memory_store() {
    let tokens = vec!["  Schema ".to_string(), "".to_string(), "  ".to_string()];
    assert_eq!(
        CockroachStore::normalize_tokens(&tokens),
        vec!["schema".to_string()]
    );
    assert!(CockroachStore::normalize_tokens(&[]).is_empty());
}
