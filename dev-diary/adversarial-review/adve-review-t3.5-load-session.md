# Adversarial Review: T3.5 — load_session() / startup

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after 2 review rounds               ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T3.5 — `load_session()` / startup (spec §2.5)
**Scope:** `src/store/load.rs`, one `pub mod load;` line in `src/store/mod.rs`
**Implementer:** T35Load (`8a0bab4`); remediation `1c7a834`
**Reviewer:** ReviewT35Load (round 1), Review2T35Load (round 2)

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| R1 | INFO | Module doc falsely claimed T4.1's daemon skeleton "already lists" the warm-up rescore wake source and named non-existent `src/daemon/events.rs` | **Fixed** (`1c7a834`): reworded to planned-state (intended wake source for T4.1, transport = T4.6 event channel; only `DaemonEvent` in types exists today); handoff aligned |
| R2 | INFO | `block_on` discarded the worker-thread panic payload (generic Backend message) | **Fixed** (`1c7a834`): map_err downcasts `Box<dyn Any>` (&str → String → fallback) and surfaces the detail |

## Round 2 — verified clean

Verdict ACCEPT, no findings. Verified: sync `load_session(&dyn GraphStore, &SessionId)` shape (pinned contract); sync-over-async bridge sound — `thread::scope` + fresh current-thread runtime, panic-free from both `#[tokio::test]` and runtime-less `#[test]` contexts (both tested); SessionNotFound → fresh empty session; corrupted snapshots (dangling previous_id, concept without Derives — injected through the store's own flush path) → typed `StoreError::Invariant`, never a panic; index rebuilt from the same snapshot with reference-agreement tests; no `Utc::now`; S5 acceptance split exactly as pinned (flush round-trip asserts all mutation-carried state deep-equal with synonyms/reservations asserted EMPTY + contract documented; full-snapshot round-trip via `MemoryStore::seed` preserves reservation + synonym). Change set = load.rs + mod.rs line + handoff only.

## Notable decisions recorded (handoff)

- Sync-over-async bridge: `Handle::block_on`/`Runtime::block_on` panic inside tokio tasks, so the store future runs on a private worker thread with a fresh current-thread runtime; thread-per-call is an acceptable, documented tradeoff for a startup-only path.
- S5 durability contract: synonyms/reservations survive ONLY full-snapshot load (MemoryStore::seed / store snapshot), never the mutation log — flush→load yields them empty.
- Round-trip test is generic vs `&dyn GraphStore` — swaps to SqliteStore unchanged once T3.3 lands.
- Present-but-empty snapshot loads as Invariant (single temporal-chain head required); duplicate natural keys in a corrupted snapshot silently reinforce (record_edge) — both graph-tier behaviors, documented not changed.
