# Adversarial Review: T5.1 — Phase-1 candidates

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                                  ║
║  Disposition: ACCEPT (no findings, no remediation round)         ║
║  Opened / Closed: 2026-08-12                                     ║
╚══════════════════════════════════════════════════════════════════╝
```

**Task:** T5.1 — phase-1 candidates (handoff PHASE-5-recall.md T5.1, spec §8)
**Scope:** `src/recall/candidates.rs` (new), `src/recall/mod.rs` (+1 additive line)
**Implementing commit:** `8b013a4` — *"feat(recall): T5.1 phase-1 candidates - keyword + recent-interactions + capability-gated vector union"*
**Merged:** `e230f71` (`task/p5-t5.1-candidates` → `phase/p5-recall`)
**Status line (PHASE-5-recall.md):** *"done (2026-08-12, reviewed ACCEPT; merged e230f71)"*

## Verdict

ACCEPT — 0 findings. Every review criterion verified against source; 9/9 module
tests green under `RUSTFLAGS="-D warnings"`; `cargo check --all-targets` clean.

## Verified

- **Lock discipline.** `gather` (async; takes only `&dyn GraphStore` + session/embedding/limit) is the sole I/O channel and produces `Phase1Input`; `candidates` (sync; takes `&Graph` + `&InvertedIndex` + `Phase1Input`) has no store access — the API shape makes it type-impossible for an async store call to happen while a graph lock could be held. Module doc mandates gather-before-lock.
- **Capability gate.** `gather_without_capability_zero_store_calls_one_log` asserts 0 async store calls + exactly 1 log line naming `VECTOR_SEARCH`; `gather_with_capability_but_no_embedding_skips_vector` asserts 0 calls + 1 line on the no-embedding path. The `SpyVectorStore` double counts calls and panics on any unexpected async trait method. MemoryStore hard-codes `Capabilities::empty()` (src/store/memory.rs:201), so the fake is the right VECTOR_SEARCH double — no store edit needed.
- **Golden exactness.** `golden_keyword_leg_exact_within_union` green. Fixture facts independently confirmed: 3 most recent interactions by `created_at` (09:45/09:50/09:55Z) own exactly concepts 1009-1012; fixture carries zero embeddings; goldens are `pagination -> [1008]`, `create -> [1002]`; concepts 1009-1012 ("api docs", "caching layer", "load testing", "api layer") match neither query. Index-side `recall_phase1_keyword_goldens_pass` (src/graph/index.rs:357) untouched and still green.
- **Merge rule.** Per-node max-merge (keyword inserted, recent/vector via `and_modify(max)`); BM25 kept over a lower vector score (asserted `score > 0.9` where the store returned 0.9); final sort `total_cmp` score-desc then node-id asc (total order — deterministic regardless of HashMap/leg order); `RECENT_SCORE = 0.5` flat and documented with rationale.
- **Recent selection.** `created_at` desc then id asc, take 3. `recent_ties_break_by_node_id` plants i1/i2/i3 all at `created_at = 60` with i4 at 120 — id-asc tie-break selects {c1, c2, c4} (output `[c1, c2, c4]`); id-desc would give {c2, c3, c4} — genuinely discriminating. `planted_graph`'s chain tail i4 is the OLDEST, so a chain-order implementation would select {c4, c3, c2} vs created_at {c1, c2, c3} — also discriminating.
- **Scope hygiene.** Diff touches exactly `src/recall/candidates.rs` (+707) and `src/recall/mod.rs` (+1 additive `pub mod candidates;`); worktree porcelain clean; single commit on fork point 73aa894.
- **Quality.** No unwrap/panic/TODO in production code; `StoreError` propagated via `?`; tracing target `lambo::recall` follows repo convention.
