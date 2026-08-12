# Adversarial Review: T4.2 — Hot list

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                                  ║
║  Disposition: ACCEPT                                             ║
║  Opened / Closed: 2026-08-12                                     ║
║  RECORD RECONSTRUCTED POST-HOC (XP-2 remediation, 2026-08-12)    ║
╚══════════════════════════════════════════════════════════════════╝
```

> **Reconstruction notice.** No review record was committed at the time. Rebuilt
> on 2026-08-12 from the PHASE-4 status line, the commit history, and the code,
> as remediation for **XP-2** of `adve-review-p4-daemon-opus.md`. Sections headed
> **Not recoverable** say what is lost rather than inventing review prose.

**Task:** T4.2 — bounded hot list (spec §9)
**Scope:** `src/daemon/hotlist.rs` (new, 629 lines), `src/daemon/mod.rs` (+1)
**Implementing commit:** `74c4d5c` — *"P4 T4.2: hot list (bounded PQ, revalidate,
condition payloads)"*
**Merged:** `518680f` (`task/p4-t4.2-hotlist` → `phase/p4-daemon`), rolled up in
the `c8f64f6` tier merge
**Status line** (PHASE-4-daemon.md, section *"T4.2 — Hot list"*): *"done
(2026-08-12, reviewed ACCEPT; merged c8f64f6)"* — no remediation round claimed
**Board line (`dev-diary/README.md`, commit `a5663cf`):** *"T4.1–T4.5 done
(scoring/skeleton, hot list, conflict, drift, GC — reviewed ACCEPT, merged
40fdaee + c8f64f6)"*

## What the review had in front of it

Reconstructed from the shipped module (12 tests at merge, counted in
`518680f:src/daemon/hotlist.rs`):

- A bounded priority queue at `hot_list_max = 1000`, ranked
  `(severity desc, recency desc, node id asc)`. `Condition::severity` assigns
  the order: Conflict > HighRiskModification > Drift > StaleSession. Eviction on
  overflow removes the **lowest** priority entry; recall consumes from the
  highest.
- One entry per `(node, condition)`: re-inserting a pair already present
  **refreshes** it (new payload, new predicate, new recency) instead of
  duplicating, so the bound caps distinct pairs rather than filling with
  duplicates of a persisting condition.
- `HotListPayload` — the per-condition renderable payload (conflict carries the
  agents + seconds-ago the §13 sentence needs).
- `HotList::revalidate(&Graph, node) -> bool`, the T5.3 seam. It takes `&Graph`
  explicitly rather than stashing a graph handle, so recall never takes a hidden
  lock on top of the one it already holds (§6.4 — the daemon's `RwLock<Graph>`
  is not reentrant).

## Verified clean (re-verified 2026-08-12 against the shipped code)

- The bound is enforced under overflow, and a refresh does not count as an
  overflow insert (there is a dedicated test for exactly that distinction).
- Priority ordering is total and deterministic: the node-id tie-break makes the
  order stable regardless of insertion interleaving.
- `revalidate` drops only the entries for the node it was asked about; a node
  with one surviving entry stays hot.
- No `Graph` handle is retained anywhere in the structure, so the §6.4
  lock-discipline argument holds structurally rather than by convention.

## Not recoverable

- **Any reviewer notes.** The status line records a clean ACCEPT with no
  remediation round, and the task branch has one commit, so there is nothing to
  reconstruct beyond the shipped artifact. Whether the round examined the
  predicate contract in any depth is unknown — see below.
- **Gate numbers at close.** Not captured in-repo.

## Findings reopened by the later tier review

The `revalidate` contract was the single largest gap the P4 tier review found,
and it lands squarely in T4.2's surface:

| ID | Issue |
|---|---|
| XP-3 (P1) | Predicates captured `now` by move at detection time, so a recency-bound condition re-validated `true` forever against an unchanged graph and served a frozen `seconds_ago` — the exact API T5.3 is told to call. The bool return also made a payload refresh inexpressible. |
| CONC-5 | Each predicate re-ran a **whole-graph** detection pass, so recall force-including ten hot nodes meant ten full scans under the graph lock |
| ALGO-2 | The conflict payload recorded *when* the newest write happened but not *who* made it, leaving the §13 attribution sentence underivable |

`hotlist.rs`'s own module docs at the time described "stale entries drop out
then, **not on a timer**" — which was the exact inverse of what the code did.
A review record with a stated scope would have made that contradiction a reopen
criterion; without one it survived to the tier review. That is XP-2's point.
