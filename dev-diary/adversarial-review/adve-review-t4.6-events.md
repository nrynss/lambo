# Adversarial Review: T4.6 — Event channel + daemon-loop wiring

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                                  ║
║  Disposition: ACCEPT (after 1 remediation round + a final pass)   ║
║  Opened / Closed: 2026-08-12                                     ║
║  RECORD RECONSTRUCTED POST-HOC (XP-2 remediation, 2026-08-12)    ║
╚══════════════════════════════════════════════════════════════════╝
```

> **Reconstruction notice.** No review record was committed at the time. Rebuilt
> on 2026-08-12 from the PHASE-4 status line and Handoff Log, the commit history,
> and the code, as remediation for **XP-2** of
> `adve-review-p4-daemon-opus.md`. This is the **best-attested** of the six
> rounds: the three redesigns it forced are described in the Handoff Log and in
> `mod.rs`'s own comments, which reference them by number ("T4.6 finding 1/2/3").
> Sections headed **Not recoverable** say what is still lost.

**Task:** T4.6 — the §6.1 broadcast event channel + threading it through the
daemon loop
**Scope:** `src/daemon/events.rs` (new, 827 lines), `src/daemon/mod.rs`
(+788/−52 — shared with T4.1, coordinated), `src/daemon/hotlist.rs` (+55/−…),
`src/daemon/conflict.rs` (+31/−…), `src/daemon/drift.rs` (+10/−…)
**Implementing commit:** `8bcb816` — *"P4 T4.6: event channel + daemon-loop
wiring (spec §6.1/§9)"*
**Merged:** `4331d9b` (`task/p4-t4.6-events` → `phase/p4-daemon`)
**Final review pass:** `cd9340e` — *"P4 close: exit-criterion 1a coverage +
Handoff Log (final review remediation)"*
**Status line** (PHASE-4-daemon.md, section *"T4.6 — Event channel"*): *"done
(2026-08-12, reviewed ACCEPT after 1 remediation round; merged 8bcb816)"*

## Round 1 — three loop redesigns (attested)

The Handoff Log records this verbatim under "What surprised": *"The T4.6 review
forced **three loop redesigns**."* All three are also annotated in `mod.rs` by
number, so the findings are recoverable in substance even though the reviewer's
prose is not.

**Finding 1 — epoch-gating detection would have killed idle staleness.**
The T4.1 skeleton gated the whole cycle on `Graph::epoch()` changing. But a
session with *no* mutations must still age into `Stale`: an untouched concept
crosses the stale window purely because time passed (spec §9's background-daemon
semantics). Redesign: only the **rescore** is epoch-gated; all four detectors run
on **every** cycle. A `Clock` seam (`Daemon::with_clock`) was added so a test can
advance time without waiting on the wall clock, and
`stale_fires_for_idle_session_after_window_elapses` is the regression.

**Finding 2 — captured-`now` predicates left ghost hot-list entries.**
The per-entry re-validation closure froze `now` at detection, so re-checking it
re-evaluated the same instant forever: a `HighRisk` entry whose 30s window had
elapsed re-validated `true` indefinitely. Redesign: the loop stopped evaluating
predicates per cycle and instead keeps the hot list **equal to the cycle's fresh
`(condition, node)` set** (`HotList::retain_conditions`), which also removed an
`O(hot_len × whole-graph)` scan per cycle.

This fix was **scoped to the loop only**. The broken predicate stayed public for
recall — a decision the tier review later recorded as knowing, and reopened as
XP-3 (see below).

**Finding 3 — level-triggered emission would flood the channel.**
Publishing every detected condition every cycle would put one duplicate per cycle
per persisting condition into a 256-slot ring. Redesign: **emit-on-transition** —
an event fires when a pair *enters* the detected set; exit just stops emitting
(the §6.1 enum has no resolved variant and was not touched).

## Final pass — `cd9340e` (attested by its own commit body)

A closing review round found the exit-criterion-1a coverage incomplete and
produced:

- `loop_emits_high_risk_for_fresh_write_to_canonical_node` — the entered-gated
  HighRisk emit path and the `events::high_risk_event` mapper had **no**
  loop-level test.
- `loop_emits_stale_from_rest_api_fixture_after_writes_age_out` — staleness had
  only a synthetic-clock test; this one is fixture-driven (2h rebase).
- Exit criteria ticked with per-kind test-name annotations, including the honest
  note that `Canonized`'s coverage is a **seam round-trip**, not a loop emission,
  because the emit site is P6.
- The Handoff Log filled in: module map, the three redesigns, the v0.6.0
  interpretation seams, the `sync_index` no-production-caller note, and the
  planted-fixture semantics.

## Verified clean (re-verified 2026-08-12 against the shipped code)

- `emit` is genuinely fire-and-forget: `Sender::send` errs only with zero
  receivers, which the code swallows deliberately; a lagging receiver never makes
  `send` fail. The loop cannot block on a consumer.
- The slow-consumer contract is tested at small capacity
  (`lagged_receiver_does_not_block_the_loop`) and shows `Lagged`, not a hang.
- The "no queue bound" promise of §6.1 is kept in behavior, not memory, and the
  module says so.
- Lock order is uniformly graph → hot, and no `.await` sits inside a guard.

## Not recoverable

- **The reviewer's prose and finding severities.** Only the three redesigns'
  *substance* survives, via the Handoff Log and the numbered `mod.rs` comments.
- **Whether the round considered the P6 sender seam reachable.** `emit_canonized`
  shipped as `pub(crate)` with `#[allow(dead_code)]` and a doc comment calling it
  "a deliberate, documented seam". The tier review's verdict on that
  (`#[allow(dead_code)]` was the compiler agreeing there was no caller) is
  XP-4 below.
- **Gate numbers at close.** This record used to quote "328 passed / 0 failed /
  3 ignored" as one figure from PHASE-4's exit criteria. It is a splice: the
  pass/fail pair is the exit criteria's, but the **"3 ignored" is the P4 tier
  review's** (`adve-review-p4-daemon-opus.md`, Gates table). Both were measured
  after this task merged, so neither is this round's own gate output — which is
  what is actually unrecoverable.

## Findings reopened by the later tier review

| ID | Issue |
|---|---|
| XP-3 (P1) | Finding 2's fix was loop-only. The frozen-`now` predicate stayed public for recall, so T5.3 calling `revalidate` five minutes later gets `true` and renders a frozen `seconds_ago`. |
| CONC-2 (+ALGO-8) (P1) | Finding 3's fix created a new failure mode: `prev_conditions` updates whether or not any consumer received the event, so an event evicted from the 256-slot ring while its condition still holds is **never** re-emitted. Combined with per-concept `Stale` (one event per concept in a single warm-up burst — 22 on the fixture, ~4,000 at scale) this can permanently lose the demo's `Conflict`, which is emitted *before* the Stale burst in the same cycle. |
| XP-4 (+ALGO-7) (P1) | `emit_canonized` needs the broadcast `Sender` and `Daemon` exposed only a `Receiver` — no public path to the sender existed anywhere in `src/`, so P6's documented seam was unreachable. The exit-criterion test builds a *local* channel: it proves the variant serializes, not that the seam is callable. |
| CONC-3 | `subscribe()` after `spawn()` misses the warm-up cycle's entire condition set, and all eight loop tests subscribe before spawn, so the suite could not see it |
| CONC-4 | flush.rs's panic containment precedent was not adopted for the loop; config-reachable `expect()` on `ChronoDuration::from_std` |
| XP-6 (+CONC-7) | Zero daemon tests used `start_paused`; the negative assertions were wall-clock sleeps that pass vacuously when a cycle overruns |

The pattern is worth recording for future rounds: findings 2 and 3 were both
*correct diagnoses with incomplete fixes*, and in both cases the incompleteness
was visible in the code (a public API left on the broken path; a transition
recorded without regard to delivery). A committed record with reopen criteria is
what turns "fixed for the loop" into a tracked residual instead of a silent one.
