# Adversarial Review: T5.3 — Phase-3 scoring, assembly & context format

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                                  ║
║  Disposition: ACCEPT (3 P3 findings recorded; no remediation     ║
║                round — non-blocking)                             ║
║  Opened / Closed: 2026-08-12                                     ║
╚══════════════════════════════════════════════════════════════════╝
```

**Task:** T5.3 — phase-3 scoring, hot-list force-include, assembly, context format
(handoff PHASE-5-recall.md T5.3, spec §8; the ★★ product task)
**Scope:** `src/recall/assemble.rs` (new, 874 lines), `src/recall/format.rs` (new, ~510),
`fixtures/recall-context-golden.txt` (new, byte-exact golden), `src/recall/mod.rs` (+2 additive lines)
**Implementing commit:** `6512a1b` — *"feat(recall): T5.3 phase-3 scoring, hot-list force-include, assembly + context format"*
**Merged:** `e5dde1a` (`task/p5-t5.3-assemble` → `phase/p5-recall`)
**Status line (PHASE-5-recall.md):** *"done (2026-08-12, reviewed ACCEPT; merged e5dde1a)"*

## Verdict

ACCEPT. 44/44 `recall::` tests green; full lib 407/0 under `-D warnings`; no-default
`--lib` and `--tests` compile clean; fmt clean. Golden test is byte-exact and
wall-clock-free (planted fixed `now`).

## Findings (all P3, recorded per closure — not remediated)

- **T53-1** — em dashes (U+2014) 21× in doc prose across assemble.rs/format.rs, outside the
  exempt spec-verbatim ⚑ template. The repo's own text style uses em dashes (hotlist.rs:27,
  graph.rs:66, candidates.rs:7, config.rs, daemon/*), so this matches the codebase — P3.
  *Fix (optional):* en dash/colon/period in non-template prose; keep the ⚑ template and golden
  fixture verbatim.
- **T53-2** — assemble.rs:180 `.expect("canonical hits carry a blast radius")` in the shipped
  path. Provably unreachable: `hit.blast_radius` is constructed `Some(...)` iff `canonical`
  (lines 167-171) and the expect is guarded by the same `canonical` in the same iteration;
  consistent with the repo's production-expect convention (daemon/mod.rs:247).
  *Fix (optional):* `if let Some(radius) = ...` inside the `canonical` branch.
- **T53-3** — `format::blast_radius` is O(concepts × edges) per canonical hit
  (outer `graph.concepts()`, inner `graph.edges()`); correctness unaffected (per-concept
  aggregation, HashMap order cannot change the result; confirmed by repeated runs). Trivial
  at demo scale (C=22, E=49). *Fix (optional):* bucket inbound structural edges by target once.
  Skipped — session-graph scale is bounded.

## Verified

- **Scoring:** `final = w_daemon·daemon + w_query·relevance` (RecallWeights default 0.5/0.5);
  relevance = phase-1 score for candidates (BM25/max-merged), 0.0 for BFS-reached and
  chunk-sibling members; daemon lookup missing → 0.0; weights sanitized (NaN/negative/inf →
  0.0); ties by node id asc. Tests pass, including planted-weight mixing and tie-break.
- **Hot-list force-include:** `assemble` calls `HotList::revalidate(graph, id, now)` with the
  caller's `now`; lapsed entries dropped (removed from list), live entries carry the freshly
  rebuilt payload — test asserts "11 seconds ago" (not the 999 stale sentinel) and asserts the
  lapsed entry left the list. Writer rendered from the payload's `writer` field, never
  `agents[0]` (ALGO-2; agent-b rendered as writer in the discriminating test).
- **Assembly:** top_k respected (hot force-include beyond); truncation keeps the longest
  whole-block score-ordered prefix, never splits a block, honors a custom `token_fn`, keeps
  hits + warnings for truncated blocks; `max_tokens = 0` → empty context, full hits.
- **Context format:** `[canonical]` marker only for Canonical; ⚑ line byte-exact (em dash +
  glyph pinned); conflict line writer + seconds-ago; reservations rendered when
  `active_reservation` and absent when expired; warnings accumulated.
- **Blast radius:** spec §4.1 + 2026-08-11 errata: count concepts c ≠ node with ≥1 inbound
  structural edge (Dependency/Causal/Hierarchical) from node and no inbound structural edge
  from any other concept; 1-hop, no recursion, Derives/Temporal excluded; computed over the
  in-RAM graph. Independent review confirmed the count is genuinely 8 on
  session-rest-api.json (see Handoff Log for the 8-vs-9 reconciliation).
- **Golden:** "update user schema" (top_k=5, max_tokens=500, depth=2) on session-rest-api via
  the real merged candidates+expand+rescore pipeline with a planted T4.3-shaped Conflict entry
  (write 11s before a fixed `now`) renders the committed golden byte-for-byte.

## Wave-barrier gate note (integrator)

The CI clippy gate caught `clippy::too_many_arguments` on `assemble` (9/7) — fixed with an
`#[allow]` + justification following the repo precedent (derive.rs:392, cockroach.rs:1945),
and the full no-default TEST matrix (run only at the barrier, not by `cargo check`-based
reviews) caught two T5.1 fixture-dependent tests running un-gated under no-default features —
fixed by gating the tests and the `load_rest_api_fixture` helper (`629a61c`). Barrier gates:
fmt clean; clippy `--all-targets -D warnings` clean; default 412/0; sqlite 444/0;
sqlite-minimal 340/0; cockroach 335/0; minimal + demo checks clean.
