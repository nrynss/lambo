# Adversarial Review: T8 — P8 full end-to-end

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLEAN — full P8 surface exercised end to end             ║
║  Verdict: CLEAN (0 P1 / 0 P2 / 0 P3)                              ║
║  Scope:   phase/p8-surface @ 2963ca7 (clean)                      ║
║  Opened:  2026-08-15                                               ║
╚══════════════════════════════════════════════════════════════════╝
```

**Task:** P8 — assemble the library into `lambo`, expose it over MCP + CLI,
script the two-agent demo, and serve the read-only web window.
**Method:** full gate block; build the ship profile; drive a real MCP stdio
session; exercise the CLI read + write verbs against sqlite; run the demo twice
and diff; probe serve-web live; verify the cross-cutting invariants. The review
was split across a first-pass agent (which completed the gate block + surfaces
and then halted) and the orchestrator (which finished the invariant checks).

## Gates

All 9 rows green: fmt clean; clippy -D warnings x3 clean; `cargo test` and
`cargo test --features store-sqlite` 0 failed; the two minimal --no-run rows
with `RUSTFLAGS=-D warnings` clean; `cargo check --no-default-features` clean.
`cargo build --features demo,store-sqlite,store-memory` and the
`store-cockroach` compile both succeed.

## Surfaces (verified live)

- **MCP stdio**: `initialize` + `tools/list` returns exactly the 7 tools
  (recall, derive, record_action, reserve, inspect, saints, stats) with no
  internal-note leakage; derive/recall/stats/saints return sensible responses;
  a client-supplied timestamp field is refused (`f18_no_tool_schema_accepts_a_client_timestamp`).
- **CLI verbs**: read verbs (recall, saints, inspect, stats) run against a
  provisioned sqlite session; write verbs (derive, record-action,
  reserve/release) acquire the lease and fail closed naming the holder while a
  `serve` holds the session.
- **Demo**: `rest-api` runs twice with byte-identical OUTCOME blocks (12
  interactions, 27 concepts, 114 edges, user schema Canonical blast 9, 5
  canonization_events).
- **serve-web**: read-only reader; `/api/recall` returns the T5.3 context block
  verbatim; `/api/stats` shows real flush numbers (or honest n/a); POST
  `/api/pulse` -> 405; a non-loopback bind with no token refuses to start.

## Invariants (verified)

- **F18**: no tool schema accepts a client timestamp; created_at is
  server-stamped. Tests pass.
- **Single-writer lease**: memory/sqlite/cockroach atomic acquire; cross-process
  refusal; release on close; readers never take the lease. Tests + live conformance pass.
- **Inverted-index mirroring**: derive/record_action/demote mirror the index
  (p2_integration contract test passes).
- **Embedding contract**: mixed-model session attach is refused
  (`session_contract_rejects_model_space_mix`, `embedding_contract_cannot_change_while_vectors_remain`).
- **Level-B fail-closed** (live): uncompiled kind (`sqlite` on a non-sqlite
  binary) -> hard error; unknown TOML key (`knd`) -> hard error; Cockroach
  without DSN -> hard error; embedder dim 7 vs store width 1024 -> hard error
  ("embedder dim 7 is incompatible with store vector width 1024"). A memory
  store correctly accepts any embedder dim (no authoritative vector width).
- **retract DryRun**: reports blast radius and mutates nothing
  (`retract_dry_run_reports_impact_and_mutates_nothing`).

## Verdict

**CLEAN.** No P1/P2/P3 findings. The assembled P8 surface, its invariants, and
its evidence hold end to end. T8.9 (release) and the merge to main remain the
only unfinished phase items and are out of this review's scope.
