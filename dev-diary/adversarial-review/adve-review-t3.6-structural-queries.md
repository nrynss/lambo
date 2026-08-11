# Adversarial Review: T3.6 — Structural Queries, both dialects ★

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after 2 review rounds               ║
║  Opened: 2026-08-11 | Closed: 2026-08-11                ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T3.6 — Structural queries (spec §4.1 + errata; never-cut — backs canonization Stages 2–3 and the `⚑ Load-bearing pillar` warning)
**Scope:** `blast_radius` + `interaction_span` inside cockroach.rs / sqlite.rs (the three-way agreement proof)
**Implementer:** T36Structural (`492086d`); remediation `dde550f`
**Reviewer:** ReviewT36Structural (round 1), Review2T36Structural (round 2)

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| F1 | P2 | The span's both-timestamp age gates (e.created_at AND i.created_at — spec errata) were never behaviorally discriminated: every span test collapsed to an all-old origin set, so a spec-literal regression (dropping either gate) would pass the sqlite suite (no text-level lock either) | **Fixed** (`dde550f`): fresh edge's source now carries a DISTINCT origin i2 (span distinct 2→1 across min-age 0→1h); i-gate probe adds an aged edge with fresh origin i3 (span distinct 1→0); matrix loop covers 8 nodes × 2 ages × 2 queries with exact MemoryStore equality + oracle-independent anchors; sqlite text-level lock `structural_span_sql_gates_both_timestamps` asserts both clauses on the runtime const. **Discriminating power demonstrated**: dropping either gate alone produces the divergent value at the 3600s cell exactly as predicted |
| F2 | P3 | Cockroach's matrix never verified its assertion count (sqlite locks 180) — a future narrowing would silently shrink coverage | **Fixed** (`dde550f`): cockroach matrix returns its count, accumulated over both fixtures, asserted == 180 ("matrix dimensions drifted") — 45 nodes × 2 ages × 2 queries |

## Round 2 — verified clean

Verdict ACCEPT, no findings. Verified: exact three-way agreement per adapter (both 180 locks pass), §4.1 errata Derives probes, F1 single-point coverage (1.0 / distinct ≥ 1), aged-vs-fresh edge cases, both-timestamp span gating now behaviorally locked, memory.rs (oracle) untouched, INTERACTION_SPAN_SQL const extraction byte-identical to prior SQL, deterministic fixtures. Own runs: sqlite 232 / 1 ignored; cockroach live 232 / 1 ignored, conformance_suite 48s, zero SKIP; zero warnings.

## Notable decisions recorded (handoff)

- Three-way agreement is the abstraction's proof: both SQL dialects answer blast_radius + interaction_span EXACTLY like MemoryStore's naive scan (180 exact-equality assertions per adapter).
- Span gates BOTH e.created_at and i.created_at (spec §4.1 second errata, pinned); blast_radius excludes provenance Derives/Temporal (first errata) — a spec-literal "simplification" of either now fails the matrix.
- F1 single-point rule: extent ≤ 0 with distinct ≥ 1 → coverage 1.0, three-store consistent.
