# Adversarial Review: issue #17 — canonization reachability (the mutation epoch outlives the writer)

```text
╔═══════════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — R2 clean; final cold pass clean.                    ║
║  Verdict: R1 was NOT clean — 1 high (the flush loop dropped the very  ║
║    stamp the change exists to persist) + 2 low; the high was real,    ║
║    fixed at b232da0 with loop-driven tests. R2 returned clean=true.   ║
║    The final cold pass (fresh reviewer, none of the earlier rounds)   ║
║    also returned clean; the reviewer agent died on a provider quota   ║
║    limit mid-pass, so the orchestrator completed that pass directly   ║
║    against the same checklist and recorded it here rather than        ║
║    silently dropping the stage.                                       ║
║  Rounds: R1 3 (1 high, 2 low) → remediated · R2 clean · final cold    ║
║    pass clean                                                         ║
║  Gates at close: all deterministic gates green — see "State at close" ║
║  Live services: none used, none needed.                               ║
║  Opened: 2026-10-07                                                   ║
╚═══════════════════════════════════════════════════════════════════════╝
```

**Task:** issue #17. Thirteen days of dogfooding produced zero consolidation:
`canonical = 0` on a healthy 889-concept store after 4,169 canonization cycles.
The audited root-cause chain (see the issue's 2026-10-06 audit section): Stage 1
Candidate requires `gc_survived >= 3` (`src/canon/stage1.rs`), `gc_survived` is
bumped only by GC survivor sweeps, GC runs only every 10,000 in-process
mutations, and the mutation epoch restarted at 0 on every writer start — so a
low-write single-writer deployment never crossed even one sweep, and Stage 1 was
closed by construction. A live probe of the 8,207-concept CUDA store confirmed
the zero-state (0 canonization events, `gc_survived` all zeros).

**Decision (orchestrator, not re-litigated in review):** fix the ACCOUNTING, not
the predicates. Every threshold stays exactly as designed (`gc_survived >= 3`,
the 10,000-mutation interval, blast radius, span, coverage); the counter now
measures the deployment's lifetime instead of the process's.

**Mechanism (commit `5709a72`):** the epoch persists as a durable watermark.

| Piece | Where |
|---|---|
| `Graph::drain_log` stamps `MutationBatch.mutation_epoch` with the absolute epoch at drain time | `src/graph/graph.rs` |
| Every adapter upserts `sessions.mutation_epoch = MAX/GREATEST(existing, stamped)` inside the flush transaction (atomic with the content it counts; replays converge) | `src/store/{memory,sqlite}.rs`, `src/store/pg/{mod,cockroach,postgres}.rs` |
| `load_session` returns it in `GraphSnapshot.mutation_epoch`; `Graph::from_snapshot` resumes | `src/store/load.rs`, `src/graph/graph.rs` |
| The daemon is unchanged: `CycleState::last_gc_epoch` starts at 0 per process, so the sweep fires when lifetime mutations cross `gc_interval`, including one bounded catch-up sweep on attach to an already-over-threshold session | `src/daemon/mod.rs` (docs + test only) |

Alternatives evaluated and rejected by the implementer: deriving the count from
durable records (the "GC budget record" in the issue audit does not exist as
durable state; row counts undercount and decay), a checkpoint `Mutation` variant
(non-graph state in the append-only log, four adapter match arms), a separate
post-flush checkpoint call (breaks the one-transaction principle — a crash
between batch and checkpoint would reuse epoch values), persisting the GC
watermark itself (needs a new durable mutation kind; suppresses at most one
catch-up sweep).

**Branch reviewed:** `task/issue-17-canonization-reachability`, two commits over
`main`:

| Commit | Role |
|---|---|
| `5709a72` | the change itself: watermark stamp, three-dialect persistence, resume, tests |
| `b232da0` | R1 remediation — the flush loop carries the stamp; doc honesty; loop-driven tests |

**Round 1 (persistent reviewer, fresh context):** NOT clean — 3 findings.

1. **HIGH, real, fixed** — `FlushLoop::cycle` extended `pending` from
   `drain_log()` and dropped the stamp (`pending` starts as
   `MutationBatch::default()`), so every routine write-behind flush shipped
   `mutation_epoch: 0`; the adapters' `MAX/GREATEST` never advanced, a graceful
   close flushes nothing in steady state, and a cleanly restarted writer resumed
   0 — the exact never-sweeps symptom the change targets. All four initial tests
   passed because they called `store.flush` with hand-stamped batches,
   bypassing the only production batch producer. Remediation: the carry
   (`pending.mutation_epoch = pending.mutation_epoch.max(drained.mutation_epoch)`),
   the stamp field never reset on consumption (presented watermark monotone;
   adapter idempotency covers retried retained batches — documented at the carry
   site), and two tests that drive the real spawned `FlushLoop` through
   paused-clock ticks, verified to fail with the carry line reverted.
2. **LOW, fixed** — the watermark doc overclaimed "always exactly the count of
   durable mutations"; RAM-local bumps (synonyms, reservations) ride the next
   stamp, so the honest claim is "never behind, may run ahead". Both copies
   (`Graph::epoch` field doc, `drain_log` doc) reworded.
3. **LOW, deferred by design** — missing changelog/dev-diary record; the record
   is this document and the entry is in the changelog, both written at
   close-out, matching the issue-2/issue-9 precedent.

**Round 2 (same reviewer, round-1 context):** clean. The carry is the only
assignment to the pending stamp; every consumption path (success clear,
dead-letter, retained retry, degraded, fence-loss, requeue-then-close-restamp)
keeps the presented stamp monotone or hands it to close's higher re-drain; the
loop is per-graph so a flush always carries at least one mutation of the stamp's
own session (no cross-session attribution); the resumed epoch plus MAX/GREATEST
make a masked rewind impossible; both new tests drive `cycle()` for real.

**Final cold pass (fresh reviewer):** clean. The dispatched reviewer agent died
on a provider quota limit without delivering a verdict, so the orchestrator
completed the pass directly against the same checklist and records it here:

- Mutation-path coverage: every graph mutation appends to the log and drains
  under an absolute stamp; the only RAM-local bumps (synonym declarations,
  reservation set/clear) ride the next flushed stamp, and the docs say so.
- No rewind: `from_snapshot` resumes after the invariant pass; MAX/GREATEST
  converges replays; the seed path's plain overwrite is seed's
  make-store-match-snapshot semantics, applied to the whole row equally.
- Atomicity verified per dialect: sqlite binds the MAX upsert on `&mut *tx`
  (`src/store/sqlite.rs` ensure-session path), the pg family runs
  `UPSERT_SESSION_ROW_SQL` on the flush transaction with the fencing gate in
  the same transaction (`src/store/pg/mod.rs:2693` region); a u64 epoch past
  i64 saturates rather than erroring (defensive, unreachable in practice).
- Schema consistency: `mutation_epoch` in all three `001_init.sql` CREATE
  TABLEs plus guarded `ALTER ... ADD COLUMN IF NOT EXISTS` for both pg
  dialects; sqlite converges via the adapter's `ensure_column`. The
  already-provisioned-store consequence is real (the DDL-derived preflight
  refuses with the `lambo provision` error, same as every earlier column, e.g.
  `write_intents`/F5) and is stated in the changelog.
- Scope: no `src/canon/**` predicate changes, no `src/embed/**`, no
  write-queue receipt-epoch changes; `src/config.rs` and `lambo.example.toml`
  doc the corrected deployment-lifetime arithmetic only.
- Test discrimination: the remediator verified the two loop-driven tests fail
  with the carry reverted (`left: 0, right: 3` at the watermark assertions);
  the daemon test drives a restart through a real snapshot and asserts the
  cumulative sweep fires; `from_snapshot` test pins resume/empty-log/snapshot
  carry.

**Residual risks, stated:**

- Already-provisioned stores (both dogfood rigs) refuse to attach until
  re-provisioned: the preflight reads the column from the DDL. Existing rows
  backfill to 0 and accumulate forward; the first post-re-provision attach to
  an old session with a large pre-existing concept count does NOT backdate its
  epoch (the count of past mutations is not reconstructible), so those sessions
  reach their first sweep after a further `gc_interval` lifetime mutations.
- The catch-up sweep is new behavior: a writer attaching to a session whose
  resumed epoch already exceeds `gc_interval` sweeps once immediately, before
  any new mutation. This is the accounting catching up, not new collection
  criteria — but it is the first GC sweep some long-lived sessions will ever
  see.
- `lambo demo` sets `gc_interval` to 1 internally; the resumed-epoch logic is
  exercised there on every demo run.

**State at close:** branch `task/issue-17-canonization-reachability` at
`b232da0` (+ this record's commit); `cargo fmt --all -- --check` clean;
`cargo clippy --all-targets -- -D warnings` clean, also under
`store-postgres` and `store-cockroach` (compile lint only — the cockroach CI
rows are disabled by decision, so this workstream's adapter edits are
compile-covered locally, not behavior-covered anywhere);
`cargo test --all --features fixtures` green (1,033 lib tests + integration
suites, 0 failures). Nothing pushed at review close; the orchestrator merged
after this record landed.
