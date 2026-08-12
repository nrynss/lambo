# Adversarial Review: T4.3 — Conflict detection ★ (demo trigger)

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

**Task:** T4.3 — conflict detection (spec §9), the demo trigger
**Scope:** `src/daemon/conflict.rs` (new, 536 lines), `src/daemon/hotlist.rs`
(rebased onto T4.2's), `src/daemon/mod.rs` (+3/−1)
**Implementing commit:** `7ca55bc` — *"P4 T4.3: conflict detection (spec §9,
window+agent attribution, hot-list wiring)"*
**Merged:** `805586f` (`task/p4-t4.3-conflict` → `phase/p4-daemon`), rolled up in
`c8f64f6`
**Status line (PHASE-4-daemon.md:83):** *"done (2026-08-12, reviewed ACCEPT;
merged c8f64f6)"* — no remediation round claimed

## What the review had in front of it

Reconstructed from the shipped module (9 tests at merge):

- Spec §9's rule: ≥2 active agents holding edges to the same node, at least one
  `Causal`/`Dependency` write inside `conflict_recency_window = 30s`.
- `conflict_at(graph, node, window, now)` as the per-node primitive, with
  `detect` folding it over the temporal chain and the concept set so both use
  identical logic. `now` is a parameter, so fixtures and tests mock the clock.
- Agent attribution: an edge was attributed to its **source node's** agent — an
  `Interaction`'s `agent_id` or a `Concept`'s `origin_agent`. "Agent X has an
  edge to node N" meant X was the source-agent of at least one edge incident to
  N. This rule is documented at length in the module header, which is itself
  evidence the round engaged with it.
- `insert_conflicts` refreshes one hot-list entry per hit, carrying the agents +
  `seconds_ago` T5.3 renders.
- Fixture coverage: the planted conflict in `session-rest-api` (the caching
  layer, contested by agent-a's `Derives` and agent-b's `Dependency`) fires;
  single-agent and stale-window cases do not, under a mocked clock.

## Verified clean (re-verified 2026-08-12 against the shipped code)

- The window predicate is closed on both ends and rejects future-dated edges, so
  a mocked clock cannot manufacture a hit.
- Hits are returned in deterministic node-id order and the agent list is sorted
  by agent id.
- The detector is pure: no locks, no I/O, no hot-list mutation in `detect`
  itself.
- `insert_conflicts` dedups through `HotList::insert`'s `(node, condition)` key,
  so re-running the detector refreshes rather than duplicating.

## Not recoverable

- **Any reviewer notes**, and in particular whether the source-agent attribution
  rule was *challenged* or merely *documented*. The task branch has one commit
  and the status line claims no remediation round, so there is no
  pre-remediation state to diff. The module docs argue the rule carefully, which
  suggests it was considered; that it was considered and still wrong is the
  finding below.
- **Gate numbers at close.** Not captured in-repo.

## Findings reopened by the later tier review

Three of the tier review's findings are T4.3's, and two of them break the demo
sentence this task exists to feed:

| ID | Issue |
|---|---|
| ALGO-3 | Source-agent attribution mis-credits cross-agent writes under canonical concept reuse: `record_action` resolves an existing concept as the source of a new edge, so the edge is attributed to that concept's *original* author. On the demo's shape the two-agent set collapses to one and the conflict silently does not fire. |
| ALGO-2 | The payload records when the newest qualifying write happened but not who made it, so §13's "Agent A wrote to it eleven seconds ago" is underivable — and the naive guess (first agent alphabetically) is provably wrong on the shipped fixture, where the newest write is agent-b's. |
| XP-3 (P1) | `insert_conflicts`'s predicate captured `now` by move, so the entry re-validated true forever and served a frozen `seconds_ago` |

The fixture test passed throughout, because the fixture's contested node is
reached by an `Interaction`-sourced `Derives` edge (attributed correctly by rule
1) plus one concept→concept edge whose source concept happens to belong to the
other agent. The mis-attribution only bites when `record_action` reuses a
canonical concept — the path the demo actually takes. A green fixture test was
not evidence the rule was right.
