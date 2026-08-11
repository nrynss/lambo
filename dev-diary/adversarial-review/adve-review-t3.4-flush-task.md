# Adversarial Review: T3.4 — Write-behind flush task

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after 2 review rounds + polish      ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T3.4 — Write-behind flush task (spec §2.4–2.5)
**Scope:** `src/store/flush.rs`, one `pub mod flush;` line in `src/store/mod.rs`
**Implementer:** T34Flush (`055f257`); remediation `c0ac96d` (F1–F4) + `7066f16` (panic-containment tests)
**Reviewer:** ReviewT34Flush (round 1), Review2T34Flush (round 2)

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| F1 | SHOULD | `spawn(self)`→`spawn(&self)` deviation (kept so stats() stays reachable) leaves a double-spawn path: two loops with independent pending buffers could persist out of order | **Fixed** (`c0ac96d`): `started: AtomicBool` in Shared; spawn check-and-set; second call PANICS ("spawn called twice — exactly one loop may run"); doc says spawn exactly once |
| F2 | INFO | Unused `iid2` binding → test-build warning (the "0 warnings" claim was false) | **Fixed** (`c0ac96d`): renamed `_iid2`; 0 warnings |
| F3 | INFO | A panicking store backend aborts the loop silently (degraded() stays false) — worse than the designed degradation | **Fixed** (`c0ac96d`): `catch_unwind` containment (CatchUnwindPoll, std-only) routes panics into the retain/backoff/degrade path with a BackendFlushPanic warn carrying the payload |
| F4 | INFO | `lag` docstring promised 0 until first success but `last_success` was initialized at construction | **Fixed** (`c0ac96d`): initialized in `spawn` |

**Also fixed by the remediator (not a review finding):** ~30% suite flake — tracing caches callsite Interest process-wide at first registration; two subscriber-less tests raced and one registered `NoSubscriber`, permanently disabling the shared warn callsite. Fixed via `keep_callsites_enabled()` (silent TRACE-level default in the two warn-emitting no-subscriber tests). Verified sound against tracing-core 0.1.x and stable across 10/10 runs.

## Round 2 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| R1 | P3 | The new panic-containment path had zero test coverage (the module's most severe failure mode) | **Fixed** (`7066f16`): `panicking_backend_is_contained_batch_retained_then_lands` (loop survives 2 panics, batch retained + lands whole, BackendFlushPanic ×2 with payload) + `repeated_panics_lead_to_degrade_past_log_max` (persistent panics → designed degrade) + `PanicStore` mock |

## Verified at close

Lock discipline: write lock held only for `drain_log`, never across an `.await` (the only awaits are store.flush and backoff sleeps, lock-free). Bounded doubling backoff (100/200/400ms capped 10s, attempts = retries+1); batch retained on exhaustion, flushed ahead of new drains in one ordered batch; degradation past log_max terminal + documented + pinned (FlushDegraded ERROR, flush I/O frozen, depth keeps tracking); stats() thread-safe (atomics + parking_lot Mutex), accurate across success/failure/idle; max_batch is a TRIGGER not a cap (oversize burst delivered whole); 8 tests fully deterministic under tokio pause/advance; change set = flush.rs + mod.rs line + handoff only. `cargo test --lib` 210 passed / 0 failed / 0 warnings (incl. 2 new panic tests).

## Notable decisions recorded (handoff)

- `spawn(&self)` (not the pinned `spawn(self)` — the pinned form consumes the only stats()/degraded() handle); single-spawn enforced at runtime with a panic.
- Degradation is terminal for the session (spec: durability="none").
- Panic containment depends on `panic=unwind` (holds today; would no-op under a future `panic=abort` profile — noted in handoff).
