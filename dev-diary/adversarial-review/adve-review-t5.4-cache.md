# Adversarial Review: T5.4 — Recall cache

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                                  ║
║  Disposition: ACCEPT (2 P3 doc-accuracy findings; no             ║
║                remediation round — non-blocking)                 ║
║  Opened / Closed: 2026-08-12                                     ║
╚══════════════════════════════════════════════════════════════════╝
```

**Task:** T5.4 — recall cache (handoff PHASE-5-recall.md T5.4, spec §8)
**Scope:** `src/recall/cache.rs` (new), `src/recall/mod.rs` (+1 additive line)
**Implementing commit:** `f319b6b` — *"feat(recall): T5.4 bounded LRU recall cache keyed by (query_hash, top_k, traversal_depth, mutation_epoch)"*
**Merged:** `53979b2` (`task/p5-t5.4-cache` → `phase/p5-recall`)
**Status line (PHASE-5-recall.md):** *"done (2026-08-12, reviewed ACCEPT; merged 53979b2)"*

## Verdict

ACCEPT. 8/8 module tests green; `RUSTFLAGS="-D warnings"` clean (forced
recompile); scope exact; `CacheKey` exactly `(query_hash, top_k,
traversal_depth, mutation_epoch)` matching spec §8; epoch invalidation only, no
generation counters.

## Findings (both P3, doc-only, recorded per closure — not remediated)

- **T54-1** — `src/recall/cache.rs:14-15`: doc claims inserts "amortize to O(1)"
  because eviction runs only when full. At steady-state capacity, every
  fresh-key insert runs the O(capacity) eviction scan — the amortization claim
  is misleading. Harmless in practice (capacity 128). Fix: reword (e.g.
  "inserts are O(1) until full and O(capacity) after; capacity is small").
- **T54-2** — `src/recall/cache.rs:78`: "Not `Sync`-friendly by design" is
  type-system-inaccurate — every field (`u64`, `usize`, `HashMap<CacheKey,
  (RecallResult, u64)>`) is `Sync`, so the struct is auto-`Send` + `Sync`. The
  real constraint is that every operation takes `&mut self`. Fix: reword to
  "all operations take `&mut self` (no interior synchronization); wrap in a
  lock at the call site."

## Verified

- hit/miss/evict (capacity+1)/epoch-invalidation tests all pass; a re-touched
  entry survives eviction; distinct queries map to distinct keys; insert
  overwrites; `clear` empties.
- Determinism caveat for `DefaultHasher` documented (cache.rs:57-63) and
  cross-checked against the `crate::embed::fixture` precedent.
- "Any graph mutation bumps `Graph::epoch`" verified: `epoch += 1` in
  `push_mutation` (graph/graph.rs:1180) with regression test
  `epoch_bumps_per_mutation_not_per_read`.
- `RecallResult` derives `Serialize`/`Deserialize` (types/mod.rs:451).
- Bounded capacity with `with_capacity` assert > 0; `len`/`capacity`/`clear`
  accessors.
- Plain struct, no interior mutability; tests in-module per repo convention;
  no em/en dashes in cache.rs.
