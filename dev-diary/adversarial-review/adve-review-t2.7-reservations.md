# Adversarial Review: T2.7 — Soft-Lock Reservations

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after 2 review rounds               ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T2.7 — Reservations policy (spec §11)
**Scope:** `src/graph/reserve.rs`, one `pub mod reserve;` line in `src/graph/mod.rs`
**Implementer:** T27Reserve (commit `1e410d6`); remediation `04c5274`
**Reviewer:** ReviewT27Reserve (round 1), Review2T27Reserve (round 2)
**Gate at close:** `cargo test graph::` = 44 passed / 0 failed, 0 warnings.

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| R1 | P2 | `now + ttl` (reserve.rs:74) uses chrono 0.4.45's `Add<TimeDelta> for DateTime`, which PANICS on overflow; TTLs in the (~260k, ~292k]-year band pass `chrono::Duration::from_std` (TimeDelta::MAX > DateTime<Utc> range) and crash the process instead of the documented typed `ReserveError` | **Fixed** (`04c5274`): `now.checked_add_signed(ttl).ok_or_else(|| ReserveError{..})`; regression test `ttl_overflowing_datetime_is_typed_error_not_panic` (8.21e12 s) verified to panic on the OLD code and pass post-fix |
| R2 | SHOULD | Coverage gaps: takeover at exact expiry, extend on still-live lock, `active_reservation` on missing node, release-then-re-reserve, release of expired lock | **Fixed** (`04c5274`): all 5 tests added (6 with the regression test; 38 -> 44) |

## Round 2 — verified clean

Verdict ACCEPT, no findings. Verified: `expires_at` comes only from
`checked_add_signed` on the production TTL path (no panicking `Add`); all 6 new
tests exercise their named semantics with explicit timestamps; half-open boundary
(active iff `now < expires_at`) consistent across reserve/release/active_reservation;
pinned contract holds (no `Utc::now` anywhere in reserve.rs; missing node NotFound;
same-agent extend; cross-agent Conflict naming holder+expiry with lock untouched;
takeover expiry from takeover time; identity-only release; RAM-local — no mutation-log
writes); 44 passed / 0 failed, 0 warnings.

## Notable decisions recorded (handoff log)

- Expiry is half-open: `now == expires_at` is expired (a fully-elapsed TTL is dead).
- `release` ignores expiry — decided by agent identity alone (per the pinned contract).
- Reservations are RAM-local (no `Mutation` kind exists); they round-trip via
  `GraphSnapshot` only; storage stays on `Graph` (T2.1) — this module is policy only.
- Cut-order: whole feature = `reserve.rs` + one mod.rs line.
