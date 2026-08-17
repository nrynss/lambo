# Adversarial Review - Hardening H2, round 1

- **Reviewer:** `h2_review_r1` (fresh independent reviewer; source read-only)
- **Date:** 2026-08-17
- **Scope:** implementation commit `2af8b86` against base `5962e80`, plus the
  full H2 specification in `dev-diary/notes/hardening-tasks.md` (authority for
  acceptance criteria, verification, and cut line)
- **Worktree:** `/home/nryn/work/lambo/worktrees/hardening-h2`
- **Verdict:** **CLEAN / APPROVE** - 0 P0, 0 P1, 0 P2, 1 P3 (report
  documentation only; non-blocking)

The patch stops shipping the self-contradicting pairing: `api_inspect` computes
`gate_progress` only when
`concept.canonization_status != CanonizationStatus::Canonical`, so a Canonical
hit keeps `status`/`blast_radius`/`dependents`/`truncated` at HTTP 200 with no
`gate_progress` key on the wire and runs neither gate-only store query. Every
acceptance criterion is met with store-surface (not JSON-only) proof, all
specified verification gates pass with the claimed counts, and no consumer of
the HTTP contract depended on a Canonical payload carrying gate bars
(`web/app.js:580` already guards `gp && d.status !== "Canonical"`). The single
finding is a stale line-reference pair in the implementer's report; the
committed patch itself is accurate.

## Findings

### H2-R1-1 (P3) - Implementation report's test line references are stale despite claiming a re-audit

- **Evidence:** The implementer's report states the canonical assertions are
  "now 2159-2195" and the cooldown regression "now 2389-2422+", claiming the
  refs were "re-audited against the current serve_web.rs (portal rebuild moved
  it)". The committed tree at `2af8b86` (worktree verified clean) disagrees:
  - The canonical gate-absence assertions live in
    `inspect_endpoint_reports_a_focus_and_its_structural_dependents` at
    `src/cli/serve_web.rs:2302-2334` (H2 key-absence assertion at
    `:2326-2331`).
  - The cooldown regression
    `inspect_surfaces_a_cooling_concepts_repromotion_cooldown` is at
    `src/cli/serve_web.rs:2623-2667`, not 2389-2422+.
  - `src/cli/serve_web.rs:2159-2195` today contains
    `recall_endpoint_returns_the_context_block_verbatim` and
    `events_endpoint_tails_the_canonization_feed` — unrelated recall/events
    tests, no gate assertions.
  The two production refs are approximately right (`InspectResponse` 516-533
  vs claimed 516-546; `api_inspect` 949-1035 vs claimed 946-1032), so the
  claim is not uniformly wrong — the test refs are.
- **Impact:** A remediation agent or future reviewer following the report's
  test refs to locate the canonical key-absence assertion or the cooldown
  regression lands in unrelated endpoint tests. The committed spec handoff
  paragraph deliberately carries no line numbers, so nothing in-repo is wrong;
  this affects only the report artifact.
- **Required remediation:** Correct the two test refs in the implementation
  report to the current tree: canonical assertions `src/cli/serve_web.rs:2302-2334`
  (absence assertion `:2326-2331`), cooldown regression
  `src/cli/serve_web.rs:2623-2667`. No code change.

## Positive observations

- The predicate keys on the concept's *current* status
  (`concept.canonization_status != CanonizationStatus::Canonical`,
  `src/cli/serve_web.rs:1002`), not `last_demotion_time` and not
  has-ever-been-Canonical — exactly the spec's key. Budget demotion resets
  status to `None`, so a cooling, recently demoted concept still gets the gate
  block; the retained cooldown regression (`:2623-2667`, status `None`,
  demoted 5 s ago, default 300 s cooldown) proves `in_cooldown: true` +
  `cooldown_until` survive.
- The key-absence assertion is on the parsed wire JSON with `Value::get`
  (`hit.get("gate_progress").is_none()`, `:2331`, `:2417`): a serialized
  `null` would read `Some(Null)` and fail the test, so a `skip_serializing_if`
  regression cannot hide behind indexing semantics.
- The query-count regression is a genuine store wrapper, not a bypass. The
  `Counting` struct (`:1517-1629`) implements the full `GraphStore` trait,
  increments `AtomicUsize` counters on `blast_radius`/`interaction_span`, and
  delegates everything else to the seeded `MemoryStore`. It is injected as
  **the** served store via
  `state_from_backends(backends_with_store(Box::new(counted)), ...)`
  (`:2408-2412`), `spawn` serves the production `router(state)` builder
  (`:2055`), and `AppState` holds exactly one store
  (`state.store()` -> `backends.store.as_ref()`, `:314-316`), so the router
  cannot accidentally use a different store. The response itself proves the
  wrapper is live in the request path: a `found: true` hit requires
  `load_session` (delegated through `Counting`) to succeed.
- The non-Canonical fixture exercises all three statuses (Candidate, Venerable,
  status-None) in one test (`:2339-2393`) and asserts identical gate shape and
  thresholds (bars 3.0 / 5.0 / 3.0 / 0.3, `strictly_above` on blast radius,
  `in_cooldown: false`). Focus resolution is exact: `resolve_focus`
  (`src/cli/inspect.rs:72-85`) resolves case-insensitive exact content before
  the fuzzy substring leg, so "candidate concept" cannot bleed into the other
  two concepts.
- Misses are unchanged (`InspectResponse::missing`, `:538-548`: 200 +
  `found: false`, gate key skipped), and the gate-read-failure degradation arm
  is textually identical to base (`Err(e) => warn + None`), so a failed gate
  read on a non-Canonical hit still omits the block without failing the
  endpoint.
- Scope is exactly the two files claimed: `src/cli/serve_web.rs` and the H2
  spec handoff paragraph. CLI (`src/cli/inspect.rs` has no `GateProgress`
  reference) and MCP (`gate_progress` appears nowhere in `src/mcp`) surfaces
  are untouched; no public API or MCP wire change.
- Cross-boundary: the only in-repo HTTP consumer,
  `web/app.js:renderGates` (`web/app.js:578-582`), already computes
  `applicable = gp && d.status !== "Canonical"` and returns early when not
  applicable, so a Canonical payload without the key renders exactly as
  before (pre-existing guard, unchanged by the patch).
- The implementer's verification-gate counts all reproduce exactly (see table).

## Commands and results

| Command/check | Result |
|---|---|
| `git diff --stat 5962e80..2af8b86` | 2 files: `src/cli/serve_web.rs` (+315/-30), `dev-diary/notes/hardening-tasks.md` (+36); scope confirmed |
| `cargo test --lib inspect_endpoint_reports_a_focus_and_its_structural_dependents` | pass (1 passed) |
| `cargo test --lib inspect_keeps_the_gate_block_for_every_non_canonical_status` | pass (1 passed) |
| `cargo test --lib inspect_canonical_hit_runs_neither_gate_only_store_query` | pass (1 passed) |
| `cargo test --lib inspect_surfaces_a_cooling_concepts_repromotion_cooldown` | pass (1 passed) |
| `cargo test` | pass: library 699 passed / 1 ignored; every binary, integration and doc harness passed |
| `cargo test --no-default-features --features store-memory,embed-fixture` | pass: library 686 passed; every enabled harness passed |
| `cargo test --all-features` | pass: library 829 passed / 8 ignored (live Cockroach legs, `LAMBO_COCKROACH_DSN` unset); every enabled harness passed |
| `cargo clippy --all-targets -- -D warnings` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass |
| `cargo fmt --all -- --check` | pass |
| `git diff --check 5962e80..2af8b86` | pass |
| `git status --short` at HEAD `2af8b86` | clean |

No live CockroachDB leg was run because `LAMBO_COCKROACH_DSN` is not set; the
all-features suite reported its live legs as ignored, not passed. No Cockroach
code is touched by this patch (store queries are trait-surface calls only).

## Verdict

**CLEAN / APPROVE.** All five acceptance criteria hold with store-surface
evidence, the predicate keys on current status as specified, the cut line
(no transactional snapshot) is respected, and every verification gate passes
with the implementer's claimed counts. The single P3 finding is confined to
the implementation report's line references and does not affect the patch,
the wire contract, or any consumer.

## Remediation disposition

- **Remediation agent:** `H2RemediationR1`
- **Remediation commit:** `0000000000000000000000000000000000000000` (filled after commit)
- **Disposition:** H2-R1-1 (P3) ACCEPTED with documentation; no code
  change. The original CLEAN / APPROVE verdict above is unchanged, and no
  round-2 review is required for a docs-only disposition.

### H2-R1-1 (P3) - accepted (documentation only)

The finding concerns stale test line references in the implementer's final
report only; the committed spec handoff paragraph carries no line numbers,
so nothing in-repo was wrong. The correct references are already recorded
in the finding body of this record: the canonical gate-absence assertions
at `src/cli/serve_web.rs:2302-2334` (H2 key-absence assertion at
`:2326-2331`) and the cooldown regression
`inspect_surfaces_a_cooling_concepts_repromotion_cooldown` at
`src/cli/serve_web.rs:2623-2667`. No code change was required or
performed. The review verdict remains CLEAN / APPROVE; no round-2 review
is required for a docs-only disposition.
