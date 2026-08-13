# Adversarial Review: T8.1 — `Memory` builder & assembly (fable, deep)

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN                                                    ║
║  Verdict: REQUEST CHANGES — 1 P1 / 2 P2 / 6 P3                   ║
║  Opened: 2026-08-13                                              ║
╚══════════════════════════════════════════════════════════════════╝
```

**Task:** T8.1 — `Memory` builder & assembly (spec §6.1 surface; PHASE-8-surface.md)
**Branch reviewed:** `phase/p8-surface` @ `512760d`; implementing commit `86f32b9`
**Scope:** `src/memory.rs` (new) + every seam it wires (FlushTask stop/COH-6 drain, CanonizationTask, daemon, STORE-1 attach check, index mirroring, cut list)
**Method:** single deep fable reviewer; clause-by-clause COH-6 audit; lock-discipline trace of every public method; 3 mutation experiments + 1 deterministic race probe (all reverted, tree verified clean).

**Gates at review (clean tree):** fmt clean; `-D warnings` check clean ×3 feature sets; `cargo test` 528 lib / 5 integration / 1 doc, 0 failed; `--features store-sqlite` 568 lib, 0 failed.

---

## Findings

### T81-1 (P1) — A write racing `close()` is acknowledged, then silently lost; retraction resurrects on reload — CONFIRMED (demonstrated)
`src/memory.rs:1108-1116` (`ensure_open`), `:998-1057` (`close`), `:738-803` (`retract`), `:626-675` (`derive`).
`Memory`'s own rustdoc (memory.rs:425-427) advertises `&self` methods behind an `Arc` serving concurrent MCP tool calls — exactly T8.2's shape. `ensure_open` is a check-at-entry `AtomicBool` load with no writer barrier; `close()` stops the three background producers but not the API's own writers. Any write that passes `ensure_open` and then crosses an await (`retract`'s `blast_radius`, hybrid `derive`'s embed/store calls, or any sync write interleaved on another worker) can append mutations to the graph log **after** step-3 `drain_log()`. The flush task is gone; the mutations sit in the log forever. Both sides report success.
**Demonstrated deterministically:** a delegating store parks `blast_radius` on a `Notify`; `retract("victim", DryRun::No)` passes `ensure_open`, parks; `close()` completes `Ok`; the retract resumes, returns `Ok(removed: true)`; post-close `log_len() > 0` and `load_session` still contains "victim" — an acknowledged retraction that resurrects on reattach. Defeats COH-6's own intent: "no new mutations land after the drain" was enforced for the 3 tasks but not the surface's writers (COH-6 clause 14 FAIL).
**Fix shape:** writers gate (writes take a read permit, `close()` takes the write side before draining), or an enforced quiesce in `close()`.

### T81-2 (P2) — `close()`'s final flush has no timeout and no panic containment — CONFIRMED (traced)
`src/memory.rs:1050` vs `src/store/flush.rs:79` (`FLUSH_ATTEMPT_TIMEOUT = 30s`), `:522` (`CatchUnwindPoll`), `:542` (timeout).
The background flush path armors every store attempt twice (STORE-2 timeout + panic containment). `close()`'s hand-rolled step-4 flush has neither: a hung store hangs `close()` forever (the Handoff Log's "worst case ≈ 2 minutes" bounds only the join, not step 4), and a panicking adapter unwinds out of `close()` after `closed` latched and the log drained — making the tail unrecoverable (compounds T81-5). STORE-2's rationale applies verbatim.

### T81-3 (P2) — COH-6 "requeue to the FRONT, chronological" is unpinned: a push-back mutant survives the full suite — CONFIRMED (mutation)
`src/graph/graph.rs:1090` (`push_front_log`), test `src/memory.rs:1526-1554`.
Mutant `splice(0..0, …)` → `extend(…)`: **528/528 pass**. The test asserts only combined batch *length*, never order; `push_front_log` has no unit test. A real regression can put an edge upsert ahead of its endpoint's `UpsertNode` in the final batch, violating the in-order-replay premise (graph/mod.rs) — a conforming SQL adapter may fail the whole final transaction, turning `close()` into an error and (T81-5) losing the tail. Fix: assert mutation *sequence* + a direct order test.

### T81-4 (P3) — `biased;`/stop-first is correct but untestable-as-written and untested — CONFIRMED (mutation)
`src/store/flush.rs:362-370`. Mutant removing `biased;`: 528/528 pass. Implementation matches COH-6 exactly by inspection; the regression failure mode is a probabilistic shutdown hang. Known blind spot: pinning needs loom-style or paused-time engineering; a long-in-flight-flush + stop + join-with-timeout test would give partial coverage.

### T81-5 (P3) — A failed `close()` permanently drops the drained tail; a second `close()` returns `Ok` — CONFIRMED (traced)
`src/memory.rs:999-1001`, `:1029`, `:1050`. On step-4 failure the error is surfaced (COH-6-compliant) but `batch` is a dropped local, the log is empty, `closed` is latched: no retry possible, and a second `close()` contradicts the first with `Ok(())`. Cheap fix: `push_front_log(batch)` on failure and/or un-latch for retry; at minimum document one-shot semantics.

### T81-6 (P3) — Concurrent second `close()` returns `Ok` before the first finishes — CONFIRMED (traced)
`src/memory.rs:999-1001`. Two racing shutdown paths: caller #2 gets `Ok` while #1's final flush is in flight; if #2 gates process exit, runtime teardown cancels #1's flush — tail lost despite an `Ok`. Fix: second caller awaits the first's completion.

### T81-7 (P3) — `declare_synonym`/reservations silently non-durable through `close()`; surface docs don't disclose — CONFIRMED (traced to pinned upstream contract S5)
`src/memory.rs:601-605`; `graph.rs:677-684`; `types/mod.rs:271-340`; `store/load.rs:355-389`. Synonyms have no `Mutation` kind by pinned S5 design; after reattach, a synonym-matched derive creates a duplicate concept. Not a T8.1 bug — the T8.1 action is rustdoc disclosure on `declare_synonym`/`reserve` + a Handoff note for T8.2 (MCP clients will assume durability).

### T81-8 (P3) — No second-writer guard, even in-process — PLAUSIBLE (spec assigns single-writer to deployment)
`src/memory.rs:305`. Two same-process `build()`s for one session each spawn a full task trio against divergent RAM copies. Spec §2.2 frames this as deployment's job; a process-global session registry (or loud log) would be cheap insurance for T8.2.

### T81-9 (P3) — `retract` measures impact and acts under different lock acquisitions — CONFIRMED (traced)
`src/memory.rs:742-760` vs `:780-790`, separated by the `blast_radius` await. Reported impact can be stale vs what removal destroys. Report-accuracy only; one-line doc note.

---

## COH-6 compliance (clause by clause)

Clauses 1–13 and 15 **PASS** (Notify stop; `biased;` stop-first; latching; requeue-to-front via `splice(0..0)`; RETAINED_BACKOFF not waited out; canonization stopped before drain; daemon stopped before drain; abort()-safety; join-before-drain; no await under guard; final flush lock-free; error surfaced; retained batch never silently lost — mutant skipping `requeue_pending` fails exactly its 2 tests; doc-test asserts post-close durability). Caveats: clause 2 untested (T81-4), clause 4 order-unpinned (T81-3), clause 11 unarmored (T81-2), clause 12 one-shot (T81-5).
Clause 14 ("nothing new lands after the drain") **FAIL** → T81-1: enforced for the 3 tasks only, not the surface's own writers.

## Verified holds (attacked, did not break)

Single construction site; load → STORE-1 check (kind/model/dim each rejected, tested) → daemon → flush → canonization, each spawned exactly once, handles retained, no half-built leak, no task sees a half-loaded graph. Lock discipline: no await under any parking_lot guard anywhere in memory.rs; every graph+index pair is graph→index matching the daemon GC; recall embeds/gathers before any lock. Index mirroring on derive/record_action/demote + removal on retract, pinned by test. Retract resolution cannot cross-kind misresolve (GRAPH-1 + partial-UNIQUE); `remove_node` cleans edges (§2.4 order), temporal chain, and reservations; DryRun::Yes fully inert (no mutation/epoch/index/wake/event). Retract-on-Canonical permitted and loudly reported (eviction immunity is a GC concept — reasonable). `canonical_memories` comparator total and as claimed. `stats` exposes flush lag + log depth (phase-doc MUST met). `events()` pre-spawn receiver (CONC-3) preserved. F18 holds at this surface: interactions server-stamped in `begin_interaction`. Cut list clean repo-wide (`release()` is `reserve`'s necessary §11 pair — acceptable). Double-close idempotent-`Ok`; method-after-close refused; drop-without-close aborts tasks and warns. Done-when met: §6.1-mirroring doc-test on MemoryStore, post-close durability asserted, attach-mismatch rejection tested.

Rulings on the implementer's 6 self-flags: #1 escalated → T81-2; #2/#3/#4/#5 accepted as documented trade-offs (#4: T8.2 must call `events()` once at startup); #6 (degraded-close branch untested) stands — fold into remediation if cheap.

## Disposition

Pending remediation. Suggested order: T81-1 (writers gate — the only P1), T81-2 (reuse the flush path's timeout + CatchUnwindPoll armor on step 4), T81-3 (sequence assertion + `push_front_log` order test), then the P3 wave (T81-5/6 close-retry semantics, T81-7 rustdoc disclosure + T8.2 handoff note, T81-4 partial coverage test, T81-8 optional registry, T81-9 doc note).

Reopen criteria: regression in the COH-6 compliance table, the "Verified holds" list, or the drain/requeue tests.
