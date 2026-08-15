# Adversarial review: store-side monotonic fencing tokens (GitHub issue #1)

Branch: task/fencing (worktree /home/nryn/work/lambo/worktrees/task-fencing). Review only — no edits.

## Verdict
CLEAN with 2 informational P3 notes (no safety impact). The central property holds: a write
presenting a stale or missing fencing token is rejected (StoreError::StaleWrite) at BOTH durable
gates (flush + record_canonization) on all three backends; a valid current token is the only way
past.

## Verified (the 8 review points)
1. Stale-token rejection at both gates, all backends:
   - MemoryStore::flush gate store/memory.rs:467-480; record_canonization gate :735-746. Both
     under the inner WRITE lock, atomic with the write.
   - SqliteStore::flush gate store/sqlite.rs:695-714; record_canonization gate :1040-1060 — both
     inside the SAME transaction as the write (rolls back on `?`).
   - CockroachStore::flush gate store/cockroach.rs:1901-1920; record_canonization gate :2259-2281 —
     same in-txn design. Compile-verified (demo/ship build + clippy -D warnings); live tests are
     `#[ignore]`d on LAMBO_COCKROACH_DSN with an honesty gate, so this env verifies by compile +
     reviewable SQL, matching the two runnable backends.
   - Shared predicate store/lease.rs `lease_permits_write(current, presented)`: current==0 (no
     lease) → allow; current>0 → require Some(token) with token>=current.
   - StoreError::StaleWrite added (store/mod.rs, types/mod.rs:742), non-retryable
     (types/mod.rs:758).
2. Token lifecycle — monotonic, no clock tie (bump is integer `current_token + 1`, never wall clock):
   - MemoryStore::acquire_lease store/memory.rs:335-382: own-lease refresh preserves
     (line 349), expired different-holder takeover bumps (362), fresh mints 1 (374).
   - SqliteStore / CockroachStore do it in one atomic upsert (sqlite.rs:1103-1119,
     cockroach.rs:1323-1336): `CURRENT_TOKEN = CASE WHEN <same holder> THEN current ELSE current+1`.
3. Threading — Memory::build captures token from acquire_lease (memory.rs:551-572), overrides
   FlushTask::with_token (618) and CanonizationTask::with_token (620). FlushTask presents it on
   every store.flush (flush.rs:612); canon loop presents it on every record_canonization
   (task.rs:253). final_flush/close passes Some(lease_token) (memory.rs:1665, 2256). No production
   flush/record_canonization path omits the token.
4. seed() fixture bypass — MemoryStore::seed (memory.rs:103-109) writes inner directly (no gate);
   Sqlite/Cockroach seed also direct. Confirmed by seed_still_works_without_a_token test + fixtures.
5. No production path passes None where a token is required: all ~127 swept test call-sites are in
   `mod tests` blocks (test doubles forwarding `_token`/`token`, or unleased test MemoryStores).
   Production durable writers = FlushTask, final_flush, Evaluator.record — all carry the token. The
   reverted sock.flush() false-positive is not present. serve_web.rs:1136 promote() is a test
   (mod tests line 912), unleased MemoryStore → None passes by design.
6. clippy allow — Evaluator::eval_cycle gained `token` (8 params incl. self → over clippy's 7),
   `#[allow(clippy::too_many_arguments)]` mirrors the existing free-function eval_cycle() allow
   (eval.rs:295, 680). Justified, not hiding a smell.
7. Pinning tests (store/memory.rs:1812, 1848, 1892, 1918) all pass and genuinely pin (removing the
   gate makes `.unwrap_err()`/token-equality asserts fail):
   - a_stale_token_is_rejected_by_flush_and_record_canonization
   - a_takeover_bumps_the_token_and_the_new_holder_writes
   - a_refresh_preserves_the_token_and_the_holder_still_writes
   - seed_still_works_without_a_token
8. Gate block run in-worktree (see gates_run).

## Findings
- P3 (info) src/store/flush.rs:533-573 + types/mod.rs:752-758: doc comment says a StaleWrite is
  treated "like a constraint," but `FlushLoop::cycle` only dead-letters `Constraint`; a StaleWrite
  hits the generic arm (RETAINED_BACKOFF + retain). No safety impact — `flush_with_retry` returns
  immediately (non-retryable), and the lease_lost fence latch breaks the loop (and drops pending)
  within one heartbeat after any real takeover. Minor doc/behavior mismatch; not a fence bypass.
- P3 (info) src/canon/eval.rs:687-696: the free-function `eval_cycle` presents None (documented
  "test/utility form"). Not reachable from any production path in this crate (only re-exported); a
  hypothetical library consumer calling it against a leased real store would get StaleWrite, i.e.
  fail closed. Informational only.

## gates_run (in worktree /home/nryn/work/lambo/worktrees/task-fencing)
- cargo clippy --all-targets -- -D warnings  → clean
- cargo clippy --all-targets --features store-sqlite -- -D warnings → clean
- cargo clippy --features demo --all-targets -- -D warnings → clean (incl. cockroach)
- cargo test → 718 passed, 0 failed (4 fencing pinning tests pass; seed_still incl., fixtures default)
- cargo test --features store-sqlite → 766 passed, 0 failed
- cargo build --features demo → ok; cargo test --features demo --no-run → compiles (cockroach)
- Cockroach live conformance tests are #[ignore]d; run only with --ignored + LAMBO_COCKROACH_DSN
