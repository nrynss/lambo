# dev-diary — phase handoffs

Work breakdown for **Lambo v0.1-hackathon** (see [`../lambo-hackathon-spec-v0.1.md`](../lambo-hackathon-spec-v0.1.md), "the spec").
Each file here is a **handoff**: enough context for an agent starting cold to do the work
without reading the conversation that produced it. The spec is frozen; where a phase doc and
the spec disagree, the spec wins and the disagreement goes in the Handoff Log.

**Deadline:** Tue Aug 18 2026, 5:00 pm ET (2:30 am IST Aug 19). Today's calendar is in §Calendar below.

---

## The one rule that makes the swarm work

> **Phase-level `requires` is advisory. Task-level `requires` is binding.**

Do not wait for a phase to finish. If `T5.1` requires only `T2.6`, start when `T2.6` lands —
not when P2 closes. Phases are for reading; tasks are for scheduling.

The corollary: **`MemoryStore` + fixtures are the unblocker.** `T1.2` ships a complete
in-RAM `GraphStore` (including both structural queries, computed naively) and `T1.4` commits
fixture graphs as JSON. Any task marked `fixture-ok` starts against those immediately and
swaps to live adapters later. The daemon, recall, and canonization tracks never wait for
CockroachDB.

**Packaging (Level B):** adapters are feature-gated and config-selected
([`notes/level-b-pluggability.md`](notes/level-b-pluggability.md)). Default features keep
memory + BGE + fixtures; Cockroach/Bedrock compile only when their features are enabled.

---

## Phase graph

| Phase | Doc | Requires | Runs parallel with | Blocks |
|---|---|---|---|---|
| **P0** Ground & spike | [PHASE-0-ground.md](PHASE-0-ground.md) | — | (T0.1‖T0.2‖T0.4 internally) | everything |
| **P1** Contracts & fixtures (+ Level B packaging) | [PHASE-1-contracts.md](PHASE-1-contracts.md) | T0.1 | — | everything after |
| **P2** Graph core | [PHASE-2-graph-core.md](PHASE-2-graph-core.md) | P1 | P3 P4 P5 P6 P7 | P8 |
| **P3** Stores | [PHASE-3-stores.md](PHASE-3-stores.md) | P1 · T0.3 | P2 P4 P5 P6 P7 | P6 (live queries) P8 |
| **P4** Daemon | [PHASE-4-daemon.md](PHASE-4-daemon.md) | P1 | P2 P3 P5 P6 P7 | P6 (gc_survived) P8 |
| **P5** Recall | [PHASE-5-recall.md](PHASE-5-recall.md) | P1 · soft P2 | P2 P3 P4 P6 P7 | P8 |
| **P6** Canonization | [PHASE-6-canonization.md](PHASE-6-canonization.md) | P1 · T4.5 | P2 P3 P5 P7 | P8 (demo story) |
| **P7** Embeddings | [PHASE-7-embeddings.md](PHASE-7-embeddings.md) | P1 · T0.3 T0.4 | P2 P3 P4 P5 P6 | — (degradable) |
| **P8** Surface | [PHASE-8-surface.md](PHASE-8-surface.md) | P2 P4 P5 · soft P3 P6 | (T8.2‖T8.3‖T8.5 internally) | P9 |
| **P9** Ship | [PHASE-9-ship.md](PHASE-9-ship.md) | P8 | — | submission |

```text
  P0 ──▶ P1 ──┬──▶ P2 graph core ──┐
              ├──▶ P3 stores ──────┤
              ├──▶ P4 daemon ──────┼──▶ P8 surface ──▶ P9 ship
              ├──▶ P5 recall ──────┤        ▲
              ├──▶ P6 canonization ┘        │
              └──▶ P7 embeddings ───────────┘  (degradable: keyword-only fallback)
                   ▲
             serial │ six tracks wide
```

**P0 and P1 are the only serial neck.** They are deliberately small. Everything after is
five-to-six agents wide. P7 is the only track whose total failure does not sink the ship —
the spec's capability gating (§3.2) means keyword-only is a lawful degraded mode.

**The critical demo path** (what must exist for the video, spec §13):
`derive`/`record_action` → daemon conflict detection → GC `gc_survived` → canonization all
three stages → recall context format with `[canonical]` + blast-radius warning → MCP serve →
`canonization_events` visible through the CockroachDB MCP server. Protect this chain over
everything else.

---

## Task pattern

Every task in every phase doc uses this shape:

```markdown
### T4.3 — Conflict detection
requires:   T1.1, T4.2
fixture-ok: yes
owns:       src/daemon/conflict.rs
status:     not-started        # not-started | claimed:<agent> | done
```

- **requires** — binding. Task ids only, may cross phases.
- **fixture-ok** — `yes` means it can start against `MemoryStore` + `fixtures/` before its
  real upstream exists.
- **owns** — paths this task writes. **No two concurrent tasks may own the same path.**
- **status** — claim a task by editing this line before starting.

---

## Conventions for agents

1. **Claim before you work.** Set `status: claimed:<your-id>` in the phase doc. If it is
   already claimed, pick another task.
2. **Stay inside `owns`.** `Cargo.toml` is the shared exception: additive dependency edits
   are allowed without a claim, but announce them in your Handoff Log entry. `src/lib.rs`
   module declarations likewise. Anything else outside your paths: write it in the Handoff
   Log and flag it — do not reach across.
3. **Contracts are frozen after P1.** Changing anything in `src/types.rs`, the `GraphStore`
   trait, or the `Embedder` trait requires updating `fixtures/` and `MemoryStore` in the
   same commit and a Handoff Log entry naming every dependent task. Assume other agents are
   mid-flight against the old shape.
4. **Never hold the graph lock across an `.await`** (spec §6.4). Take, work, release,
   then do I/O. Reviews reject violations on sight.
5. **Adapters own their SQL** (spec §3.2). Runtime `sqlx::query`, no compile-time macros,
   no dialect leaking above the `GraphStore` trait. If an adapter can't answer natively it
   computes the answer — callers never learn which.
6. **The cut order is law** (spec §14). If the schedule slips, cut in this order:
   Lambda sweep → ccloud scripting → SQLite adapter → reservations → drift detection.
   **Never cut:** canonization, blast radius, the recall context format. If your task is
   ahead of an uncut task on the critical demo path, help there instead.
7. **Fill the Handoff Log** at the bottom of your phase doc when done: what exists now, what
   surprised you, what the next agent should not re-derive. This folder is the diary — an
   undocumented finish is an unfinished task.
8. **Adversarial review before a phase closes.** Findings go in
   `dev-diary/adversarial-review/adve-review-<phase|task>.md`; screenshots and captured
   output in `dev-diary/evidence/`.
9. **All code written during the submission period** (Aug 8 onward). The v0.6.0 spec is
   prior design, credited in the README, never pasted in as code.

---

## Calendar

Spec §14 build plan, mapped to what actually remains. **The plan dates below are the plan —
they do not move.** As of Thu Aug 13 08:20 ET we are running ahead of them (P2–P7 all merged
to `main` by Thursday morning, against a plan that had the tracks still converging Friday);
that shows up as margin against each date, not as an earlier delivery date.

| Date | Plan | Actual |
|---|---|---|
| Sun Aug 9 | Spike day per spec — **not evidenced in repo; P0 is now first order of business** | P0 deferred; absorbed into Aug 10–11 without cost |
| Mon–Tue Aug 10–11 | Spec says paused for Native Builder / DataHub. Any Lambo time goes to **P0 + P1 only** — they are the neck | ✅ P0 (GO on Rust) + P1 complete, **plus P2 and P3 merged early** |
| Wed Aug 12 | P2/P3/P4/P5/P6/P7 launch wide | ✅ P4 + P5 merged — ahead of a day that only planned to *launch* the tracks |
| Thu Aug 13 | Tracks continue; P3 live adapters land; P6 swaps to live structural queries | ✅ **P6 (1dfffbc) and P7 (9b0c603) merged before 08:20 ET.** P3 live adapters landed Aug 11 |
| Fri Aug 14 | Tracks converge; P8 starts as soon as T2.x + T5.3 allow | P8 is unblocked early — every hard require is already on `main` |
| Sat Aug 15 | P8 complete: serve, demo scenario reproducible end-to-end | — |
| Sun Aug 16 | Full-system adversarial review; buffer | — |
| Mon Aug 17 | P9: video, README, diagram, Devpost draft | — |
| Tue Aug 18 | Submit **before** 5:00 pm ET | **Delivery date. T-5d 8h as of this update** |

**Decision gate (spec §14) — CLOSED, GO on Rust.** T0.3 proved `sqlx` × CockroachDB
`VECTOR` works; the Python fallback is off the table and T7.3 has since shipped live
`vector_candidates` against the cluster.

**Cut order (spec §14) — nothing is cut.** The schedule has not slipped, so the cut list
(Lambda sweep → ccloud scripting → SQLite adapter → reservations → drift) stays untouched;
SQLite, reservations, and drift all shipped. The one open task, **T7.1 Titan, is gated on
an external AWS authorization, not on our schedule** — its adapter PR goes up regardless
and the access request is being pursued with AWS. BGE-M3 remains the default dense path,
so no deliverable depends on that gate clearing.

---

## Status board

Keep this current; it is the only global view.

| Phase | Tasks done | Status |
|---|---|---|
| P0 | 3 / 4 | **GO on Rust** — T0.1–T0.3 done; T0.4 blocked on Bedrock use-case form (crate ready, not counted done) |
| P1 | 5 / 5 | **COMPLETE** — T1.1–T1.5 (incl. T1.4 fixtures + T1.5 Level B packaging): P2–P7 unblocked |
| P2 | 7 / 7 | **DONE + MERGED TO MAIN (2026-08-11, commit 9f9d2cb)** — graph core, canonicalization, derive, record_action, demote, BM25 index, reservations; exit criteria [x] (rebuild test); reviews CLOSED (per-task ACCEPT x2, muse-spark M1-M4, grok G1-G7); cross-phase contracts written into P3/P4/P5 docs |
| P3 | 6 / 6 | **DONE + MERGED TO MAIN (2026-08-11, commit 4c816a2)** — DDL, CockroachStore + live conformance, SqliteStore, flush, load_session, structural-query three-way gate; exit criteria [x]; all reviews CLOSED (per-task ×2, gemini36flash + opus46 partials) |
| P4 | 6 / 6 | **DONE + MERGED TO MAIN (2026-08-12, commit 73aa894)** — T4.1–T4.6 (scoring, hot list, conflict, drift, GC, events); adversarial review CLOSED ACCEPT after 2 remediation rounds; exit criteria [x]; live-Cockroach verification 2026-08-12 (SET_ROOT_GOAL residual closed) |
| P5 | 4 / 4 | **DONE + MERGED TO MAIN (2026-08-12)** — T5.1–T5.4 + entry `Daemon::recall`; reviews CLOSED (internal R1+R2, GPT5.6sol 4 P1 + 4 P2 remediated, independent deep adversarial ACCEPT); exit criteria [x] |
| P6 | 4 / 4 | **DONE + MERGED TO MAIN (2026-08-13, commit 1dfffbc)** — T6.1–T6.4 (Candidate, Venerable, Canonical, eval loop/budget/demotion/audit); exit criteria [x] all four; reviews CLOSED (fable ×5 — 19 findings remediated, R2 round, R3 CLEAN); live-Cockroach canonization progression test committed (d816d28). Carryover is P8-owned, not P6 debt: F18 server-side `created_at` → T8.2; R3-1 `seed()` divergence → T8.4; F13/R3-4 eval-batch query volume → T8.2/T8.4 (all three already written into PHASE-8) |
| P7 | 4 / 5 | **MERGED TO MAIN (2026-08-13, commit 9b0c603)** — T7.0 BGE-M3/llama.cpp (default path), T7.2 hybrid matching, T7.3 live `vector_candidates` on Cockroach all done, and **T7.4 camera-proof remediation DONE (2026-08-13)** — the §12.1 vector-index proof, previously marked "open", was fixed with a **partial** index on the canonical name (root cause: `vector_explain_camera_proof` asserts `"vector search"` against `EXPLAIN (OPT, VERBOSE)`, which emits `"vector-search"`; PHASE-7-embeddings.md status `done`, camera-proof GREEN, suite 5/5). **T7.1 Titan is the other open task and is not blocked on us:** AWS account is not yet authorized for Bedrock — a PR adding the `embed-bedrock` adapter goes up regardless, and the access request is being pursued with AWS. The default dense path is BGE-M3, so the ship does not wait on it (spec §12.2 keeps Titan as the swap-in). Two integration items live downstream: T8.1 wires hybrid into live sessions, and ship needs an index-favorable camera-proof `EXPLAIN` |
| P8 | 8 / 9 | **ESSENTIALLY COMPLETE (2026-08-15) — pending T8.9 + merge.** T8.1–T8.8 all DONE/CLEAN and live-verified (T8.2 MCP, T8.3 CLI, T8.4 demo ×2, T8.5 serve-web, T8.6 lease incl. the live leg, T8.7 hardening, T8.8 docs — see [PHASE-8-surface.md §Handoff Log](PHASE-8-surface.md)). Every [exit criterion](PHASE-8-surface.md) is met **except** the concurrency-on-MBP leg (T8.2 N1/N2), which has no evidence capture recorded. **T8.9 (release process & binary distribution) is the only P8 task not started.** Runs SERIAL on one branch (decision 2026-08-13) with a task → adve-review → remediation → review loop |
| P9 | 0 / 5 | blocked on P8 |

---

## Git workflow (parallel build) — star model

Each **phase** is a `phase/<slug>` branch; each **task** is a worktree on a
`task/<…>` branch. `main` is the single integrator.

> **P8 is an explicit exception (decided 2026-08-13).** The convergence phase runs
> **serial on a single branch with no task worktrees**: its tasks share `src/main.rs`
> and `src/cli/`, so running them wide buys hours and costs conflicts in the one file
> the demo depends on. P8 also adds a mandatory per-task agent loop (task →
> adversarial review → remediation → review, repeat to CLEAN, hard stop after each
> agent, orchestrator commits). The rules below still describe P0–P7 and any future
> wide phase — see [PHASE-8 §Execution protocol](PHASE-8-surface.md) for what P8 does
> instead.

**Topology (star):** merge only `worker → phase → main`. **Never** merge one
phase branch into another — cross-phase dependencies are resolved through `main`.

**Dependency rule (task-level `requires` is binding):**
- A task starts only when its `requires` are **on `main`**.
- Any completed task lands on `main` the moment it merges (`task → phase → main`),
  regardless of which phase it is in — so a cross-phase dep (T5.3 needs T2.6) is
  satisfied when T2.6 reaches `main`.
- Before branching **and** before merging back, **rebase the phase branch onto
  `main`** to pull in prerequisites from any phase. Example:
  ```
  T2.6 done → task/p2-t2.6 → phase/p2 → main            # T2.6 on main
  T5.3 needs T2.6: rebase phase/p5 onto main → branch task/p5-t5.3 …
  ```
- Merge order therefore *is* the parallelism: `worker → phase` is internal to the
  phase's parallel tasks; `phase → main` respects the phase graph (P2‖P3‖…‖P7 wide,
  P8 after P2/P4/P5, P9 after P8).

**Claim → worktree flow:**
1. Set `status: claimed:<id>` and the worktree/branch names in the phase doc.
2. Rebase the phase branch onto `main`: `git rebase main phase/<slug>`.
3. Add the worktree: `git worktree add worktrees/<wt-name> -b <branch> phase/<slug>`.
4. Work inside `owns`, commit to `<branch>`, then merge `worker → phase → main`.

**Naming (pre-assigned, deterministic):**
| Kind | Name |
|------|------|
| Phase branch | `phase/<slug>` (see table below) |
| Task branch | `task/<phase>-t<task>-<short-slug>` |
| Worktree | `worktrees/<phase>-t<task>-<short-slug>` |

Example: T2.3 conflict detection → `task/p2-t2.3-conflict-detection`,
`worktrees/p2-t2.3-conflict-detection` on `phase/p2-graph-core`.

**Shared-file policy (single writer = the integrate/chassis step):**
- Only `Cargo.toml`, `Cargo.lock`, and `src/lib.rs` module declarations are exempt
  from `owns`; announce them in the Handoff Log (unchanged rule).
- At `phase → main` merge, resolve `Cargo.lock` with `-X theirs` (divergent lockfile
  noise); run `cargo build` once after to re-generate.
- The dev-diary **status board** and phase-doc `status:` lines are updated at
  **phase-merge time**, not per-task, so docs don't become a merge battlefield.

**Phase branch map** (all branch from `main` when the phase's prerequisites are on
`main`):

| Phase | Branch |
|-------|--------|
| P0 | `phase/p0-ground` (historical, merged) |
| P1 | `phase/p1-contracts` (historical, merged) |
| P2 | `phase/p2-graph-core` (merged to main 2026-08-11) |
| P3 | `phase/p3-stores` (merged to main 2026-08-11) |
| P4 | `phase/p4-daemon` (merged to main 2026-08-12) |
| P5 | `phase/p5-recall` (merged to main 2026-08-12) |
| P6 | `phase/p6-canonization` (merged to main 2026-08-13) |
| P7 | `phase/p7-embeddings` (merged to main 2026-08-13; T7.1 Titan still open on its own branch when AWS access lands) |
| P8 | `phase/p8-surface` ← **next** |
| P9 | `phase/p9-ship` |
