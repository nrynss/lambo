# Adversarial Review: T4.4 — Drift detection

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

**Task:** T4.4 — drift detection (spec §9, §7.7) + `set_root_goal`
auto-`Venerable`
**Scope:** `src/daemon/drift.rs` (new, 572 lines), `src/graph/graph.rs` (+237 —
`set_root_goal` through the T2.1 mutation path), `src/daemon/mod.rs` (+2)
**Implementing commit:** `c7726e8` — *"P4 T4.4: drift detection (BFS to root
goal) + set_root_goal auto-Venerable"*
**Merged:** `587f843` (`task/p4-t4.4-drift` → `phase/p4-daemon`), rolled up in
`c8f64f6`
**Status line** (PHASE-4-daemon.md, section *"T4.4 — Drift detection"*): *"done
(2026-08-12, reviewed ACCEPT; merged c8f64f6)"* — no remediation round claimed

## What the review had in front of it

Reconstructed from the shipped module (12 tests at merge). `drift.rs`'s header
carries an unusually explicit "Interpretation notes (T4.4 design decisions)"
block, which is the best surviving evidence of what the round reasoned about:

- **Root goal nodes.** Spec §9's "root goal nodes are automatically `Venerable`"
  is implemented in `Graph::set_root_goal`, which promotes the matching concept
  through the T2.1 mutation path (audit row + `Mutation::CanonizationTransition`)
  rather than flipping a field — so the promotion is durable and visible to the
  §10 state machine. The §10 machine has no `Venerable → Venerable` or
  `Canonical → Venerable` edge, so an already-protected goal is left alone.
- **Traversal set and direction.** `Causal`/`Dependency`/`Hierarchical` only
  (spec §9), treated as **undirected** — argued from the fixture, whose chain is
  directed *away* from the goal, so an orientation-sensitive walk would report
  "no path" for exactly the node the fixture plants as drifted.
- **Distance = hop count.** Spec §9 says "weighted shortest path … warn beyond
  `drift_threshold=5` hops"; the threshold is denominated in hops and the
  fixture's chain is unit-weight, so the operative metric is an unweighted
  multi-source BFS. "Beyond" is strict: `dist > threshold` fires,
  `dist == threshold` does not.
- **Cycle safety (G6).** The P2 grok review's binding note — multi-hop
  `Hierarchical` cycles are writable — is honored with a visited set, and the
  module says so.
- **"… or no path."** Read as *out of scope*: a concept with no traversable
  route to any goal emitted no hit. The note argues this from the fixture's
  isolated pair being planted as GC food and from the acceptance wording
  ("exactly one Drift event, for the planted node").
- **Determinism.** Goal seeds sorted by id, neighbor lists id-ascending, output
  sorted by node id.

## Verified clean (re-verified 2026-08-12 against the shipped code)

- The BFS is cycle-safe by construction (visited set), so G6 is honored without
  assuming anything from `assert_invariants`.
- Reachability and distance come from the same pass, so the reported hop count is
  genuinely the shortest path.
- The threshold comparison is strict, matching "beyond".
- `set_root_goal`'s promotion goes through `apply_canonization_transition`, so it
  is gated by the §10 legality check and lands in the audit trail — not a field
  flip. This is a genuinely good decision and survives unchanged.

## Not recoverable

- **Any reviewer notes**, including whether the "no path = out of scope" reading
  was raised as a spec deviation and accepted, or simply not questioned. The
  module note reads as the implementer's argument rather than a review outcome,
  and there is no second commit on the task branch to diff.
- **Gate numbers at close.** Not captured in-repo.

## Findings reopened by the later tier review

| ID | Issue |
|---|---|
| ALGO-5 (+XP-9) | Spec §9 warns beyond the threshold **or on no path**. The out-of-scope reading meant the maximally drifted case — a concept with no structural connection to the goal at all — was the one case that never warned. The phase doc also still said "weighted" for an unweighted BFS. |
| ALGO-6 | Root-goal matching accepted only a bare JSON string. Spec §6.1's own `root_goal` example is a **list**, and `as_str()` on an array is `None`, so an array goal silently disabled drift, auto-`Venerable`, and GC's root-goal exclusion. T1.4 explicitly carried this shape decision to P4; P4 froze it without recording the decision. |
| ALGO-12 | `set_root_goal` promoted the *first* `HashMap` match (nondeterministic under multiple matches) and stamped `occurred_at` with `Utc::now()` in an otherwise logical-time write path |
| CONC-5 | The hot-list predicate re-ran the whole-graph `detect` pass |

ALGO-5's disposition (remediated in Wave 6 of this branch) also settles the
fixture question the module note leaned on: the isolated pair is GC's step-3 food
**and** unreachable from the goal, so it fires Drift once before GC's interval
collects it. Both readings of the fixture were consistent with the generator
comment; only one is consistent with spec §9.
