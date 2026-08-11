# Adversarial Review: T2.3 — derive()

```text
╔══════════════════════════════════════════════════════════╗
║  STATUS: CLOSED                                          ║
║  Disposition: ACCEPT after 2 review rounds               ║
║  Opened: 2026-08-11                                      ║
║  Closed: 2026-08-11                                      ║
╚══════════════════════════════════════════════════════════╝
```

**Task:** T2.3 — `derive()` write path (spec §6.1, §7)
**Scope:** `src/graph/derive.rs`, one `pub mod derive;` line in `src/graph/mod.rs`
**Implementer:** T23Derive (`5cb16a8`); remediation `a3be3c4`
**Reviewer:** ReviewT23Derive (round 1), Review2T23Derive (round 2)
**Gate at close:** `cargo test graph::` = 80 passed / 0 failed, 0 warnings.

## Round 1 — findings

| # | Severity | Finding | Disposition |
|---|----------|---------|-------------|
| F1 | P2 | Within-call Derives self-reinforcement on canonical-key collisions: two contents canonicalizing to the same PRE-EXISTING concept both took the Matched branch; `created_this_call` covered only created nodes, so the second content re-entered the write path and bumped the just-created Derives edge (0.9→1.9, reinforced=1) — contradicting "one call never self-reinforces" | **Fixed** (`a3be3c4`): guard renamed `written_this_call`, covers created AND matched nodes; collision → exactly one Derives write, reinforced=0, one `outcome.matched` entry per colliding content. Regression test `derive_key_collision_on_preexisting_concept_does_not_self_reinforce` demonstrated failing pre-fix (reinforced 1) |
| F2 | P2 | parent_of dedup keyed on RAW strings: two pairs resolving to the same node pair double-wrote the Hierarchical natural key (0.5→1.5) and re-triggered F1's Derives double-write for the shared parent | **Fixed** (`a3be3c4`): dedup keyed on RESOLVED `(parent_node, child_node)`; shared parent covered by the F1 guard. Regression test `derive_parent_of_colliding_pairs_write_one_hierarchical_edge` demonstrated failing pre-fix (reinforced 2) |

## Round 2 — verified clean

Verdict ACCEPT, no findings. Verified: both fixes close the findings (guards + counting
match the module doc); all 13 original tests unchanged and passing; re-derive
semantics (no duplicates, CoOccurrence reinforced, well-ordered mutations), match-reuse,
CoOccurrence cap, parent_of creation, NotFound errors, reflexive parent_of rejection,
drained-batch ordering contract, daemon seam documented. Change set = derive.rs +
mod.rs line + handoff only.

## Notable decisions recorded (handoff log)

- Step-4 Derives realized via `insert_concept` for both created and matched concepts
  (matched get an idempotent node re-upsert — the only public path emitting the
  required UpsertNode, preserving §2.4 batch ordering); created concepts are never
  double-written within a call.
- Edge weights: Derives 0.9 (Graph convention), CoOccurrence/Hierarchical 0.5
  (module constants); timestamps derive from `interaction.created_at` (no clock —
  deterministic).
- parent_of-only contents created as Entity (`PARENT_OF_CONCEPT_TYPE`); reflexive
  parent_of rejected (StoreError::Invariant, Hierarchical self-loop = cycle).
- Daemon notify: documented seam only (T4 owns the channel) — no stubs.
