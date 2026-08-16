# Adversarial Review: Remediation T3 — Round 3 (final clearance, worktree `remed-T3`)

```text
╔════════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — Round 3 final clearance review                     ║
║  Scope:  Confirm the ONLY change since Round-2 APPROVE is the new    ║
║          over-cap edge-bound truncation test                          ║
║          `graph_truncates_and_reports_at_the_edges_bound` (plus its  ║
║          seed helper `seed_many_structural_edges`), that it is test-  ║
║          only and non-vacuous, that no other T3 change regressed,     ║
║          and that the worktree is integration-clean.                  ║
║  Branch: remed-T3 (worktree /home/nryn/work/worktrees/remed-T3,    ║
║          detached HEAD @ f158720, 5-file working-tree diff + new      ║
║          src/canon/gate.rs — unchanged set from Round 2).             ║
║  Date:   2026-08-17                                                   ║
║  Reviewer: T3ReviewR3 (read-only)                                     ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 0 P3 / 0 nits in the worktree.      ║
║          Round-2's N2 (edge-bound truncation untested) is closed      ║
║          in-tree by the new test; Round-2's N1 (one contract doc      ║
║          sentence) is deliberately NOT in this worktree — it is       ║
║          Main's integration-time doc edit. Worktree is clean.         ║
╚════════════════════════════════════════════════════════════════════╝
```

## Grounding

Read-only. Re-read `adve-review-remed-T3round2.md` (the prior APPROVE with the
2 nits), the full `git diff HEAD` impact, and the two new test-module items on
disk: `seed_many_structural_edges` (`serve_web.rs:1670`) and
`graph_truncates_and_reports_at_the_edges_bound` (`serve_web.rs:2221-2244`).
Ran the new test.

## Verify 1 — the only change since Round-2 APPROVE is the new edge-bound test

The working-tree file set is byte-identical in shape to Round 2: canonical
files are unchanged (`git diff --numstat` canon = 6 insertions / 4 deletions:
mod 2/0, stage1 1/1, stage2 2/2, stage3 1/1 — the exact `pub(super)` threshold
exposure Round 2 reviewed), `src/canon/gate.rs` is still untracked (created in
Round 1, present in Round 2), and `serve_web.rs` grew to +842/-10 from Round 2's
+779/-13.

Every `serve_web.rs` diff hunk falls in one of two buckets:
- Hunks before the `mod tests` boundary (`:1274`, right-side lines 83, 97, 130,
  206, 225, 432, 856, 1066) — the production changes Round 2 already reviewed
  (status/contract, gate threading, `/api/graph` + `/api/inspect`, `blast_radius`
  u64 live change).
- Hunks at/after `:1274` — all inside `mod tests`. The **only** new test-module
  *content* introduced since Round 2 is the helper + test below. No new
  production symbol, no new route, no changed signature.

The two new items are both inside `mod tests` (`mod tests` opens at `:1271`):
- **`seed_many_structural_edges(session, concepts)`** (`:1670-1705`) — seeds one
  `Interaction` root, `concepts` `Concept`s (each with the §5.7 required
  `Derives` edge from the root), then a full DAG of `EdgeType::Dependency`
  structural edges with strictly increasing `i < j` source→dst ordering, keeping
  the structural edges acyclic (the `/api/graph` builder rejects cycles). Test
  helper only.
- **`graph_truncates_and_reports_at_the_edges_bound`** (`:2221-2244`) — picks
  `concepts = 182`, so the helper produces `182 × 181 / 2 = 16 471`
  `Dependency` edges **> `MAX_GRAPH_EDGES` (16 384)**, while `182 <
  `MAX_GRAPH_NODES` (4 096)` keeps the node side untruncated — so the edge
  branch fires in isolation (a regression that silently dropped
  `MAX_GRAPH_EDGES` alone would fail). Asserts `truncated == true`,
  `edges.len() == MAX_GRAPH_EDGES`, and `nodes.len() == concepts` (182,
  untruncated). This is the exact N2 fix Round 2 requested.

**Non-vacuous:** 16 471 > 16 384 by 87 edges; over-seeded, not boundary-luck.
**Test-only:** both functions live in the `#[cfg(test)]` module; zero production
behavior touched. This is confirmed read-only-diff (no production hunk changed).

**Passes:** `cargo test --all-features graph_truncates_and_reports_at_the_edges_bound`
→ `test cli::serve_web::tests::graph_truncates_and_reports_at_the_edges_bound ...
ok`; the enclosing lib target ran `820 filtered out, 0 failed`.

## Verify 2 — no other T3 change regressed, nothing new

Re-verified against Round 2's reviewed/approved surface:
- `/api/inspect` (Part A): structural-only dependents bound at
  `MAX_INSPECT_NODES` (`:528`), `truncated: bool` plain (`R1-N1`), depth-ignored
  test (`inspect_ignores_the_depth_parameter`), dependents-bound cap test — all
  present, unchanged.
- `/api/graph` (Part B): node cap `MAX_GRAPH_NODES = 4_096` (`:137`), edge cap
  `MAX_GRAPH_EDGES = 16_384` (`:141`), `edges_trunc = raw.len() > MAX_GRAPH_EDGES`
  (`:997`) + `.take(MAX_GRAPH_EDGES)` (`:1000`) — production unchanged; now
  directly exercised by the new test.
- Gate (Part C + T11): `gate_progress` blast-radius aged via
  `store.blast_radius(...min_edge_age, now)` (`gate.rs:131-133`), cooldown mirror
  (`in_cooldown`, `cooldown_until`), `pub(super)` single-sourced stage thresholds
  — unchanged.
- Comparator (Part D): `tokens_match` unchanged; the reworded comment (`R1-N2`)
  and depth test (`R1-N3`) present.
- Read-only/no-writer-lease: no new route; the five changed-file set is
  identical to Round 2, so the no-writer-lease/invariant tests remain valid.
  Successfully compiled and tested (target built clean; the single test ran and
  the binary linked with all-features).

No new regression was introduced — the only textual delta from the approved
Round-2 state is the two test-module additions.

## Verify 3 — the single outstanding item is N1, and it is NOT a worktree concern

Round 2 closed with two nits:
- **T3-R2-N2** (edge-bound truncation untested) — **CLOSED in-tree** by the new
  `graph_truncates_and_reports_at_the_edges_bound` test (verified, passes).
- **T3-R2-N1** (worth one doc sentence in the written contract clarifying that
  the top-level `/api/inspect` `blast_radius` is the live dependent count while
  `gate_progress.blast_radius` is the engine's aged gate evidence) — a **doc
  sentence in the task contract**, not a worktree source defect.

Statement for the record: **N1 is explicitly deferred to Main and is NOT to be
landed in this worktree.** It is Main's integration-time edit to the written
contract (the `## T3`/`## T11` doc sentence in `dev-diary/notes/
remediation-tasks.md`), to be applied at merge — outside the scope of this
read-only worktree. It does not gate the worktree.

## Verify 4 — no P1 / P2 / P3 / nit remains in the worktree

- No P1, no P2, no P3: the five P3s and three nits from Round 1 were all
  independently re-verified delivered in Round 2 (APPROVE), and Round 3 confirms
  the working-tree file set is unchanged apart from the N2 test addition.
- N2: closed in-tree (this review).
- N1: pending-as-doc at integration (Main), explicitly out of the worktree.
- No new finding introduced by the test addition (test-only, read-only, passes).

## Summary

Round 3 confirms the T3 worktree is **integration-clean**. The only change since
Round-2 APPROVE is the added over-cap edge-bound truncation test
`graph_truncates_and_reports_at_the_edges_bound` plus its `seed_many_structural_edges`
helper — both inside the `#[cfg(test)]` module, non-vacuous (16 471 structural
`Dependency` edges > `MAX_GRAPH_EDGES` 16 384; 182 nodes < `MAX_GRAPH_NODES`
4 096), asserting `truncated == true`, `edges.len() == 16 384`, and
`nodes.len() == 182` (untouched by the node bound), and passing under
all-features. No production behavior changed; the canonical files, `gate.rs`,
and all Round-2-reviewed production hunks are byte-unchanged. Round-2's N2 is
now closed in-tree; Round-2's N1 remains the single outstanding item, deferred
as a task-doc contract sentence to Main's integration-time edit (explicitly not
a worktree concern). No P1/P2/P3/nit remains in the worktree. **APPROVE.**

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": [], "nits": [] }, "summary": "Round-3 final clearance: the only change since Round-2 APPROVE is the new test graph_truncates_and_reports_at_the_edges_bound (+ helper seed_many_structural_edges) in the serve_web test module. Confirmed test-only (both inside #[cfg(test)]), non-vacuous (16471 Dependency edges > MAX_GRAPH_EDGES 16384, 182 nodes < MAX_GRAPH_NODES 4096, asserts truncated==true + edges.len()==16384 + nodes.len()==182), and passing (cargo test --all-features graph_truncates_and_reports_at_the_edges_bound -> ok). Canonical files and gate.rs are byte-unchanged from Round 2; no production hunk changed; no new regression. Round-2 N2 (edge-bound truncation untested) is closed in-tree by this test; Round-2 N1 (one contract doc sentence on the two blast_radius keys) is the single outstanding item, explicitly deferred to Main as an integration-time doc edit and intentionally NOT in this worktree. No P1/P2/P3/nit remains in the worktree. Worktree is clean for integration." }
```
