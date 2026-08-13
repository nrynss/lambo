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

Spec §14 build plan, mapped to what actually remains:

| Date | Plan |
|---|---|
| Sun Aug 9 | Spike day per spec — **not evidenced in repo; P0 is now first order of business** |
| Mon–Tue Aug 10–11 | Spec says paused for Native Builder / DataHub. Any Lambo time goes to **P0 + P1 only** — they are the neck |
| Wed Aug 12 | P2/P3/P4/P5/P6/P7 launch wide |
| Thu Aug 13 | Tracks continue; P3 live adapters land; P6 swaps to live structural queries |
| Fri Aug 14 | Tracks converge; P8 starts as soon as T2.x + T5.3 allow |
| Sat Aug 15 | P8 complete: serve, demo scenario reproducible end-to-end |
| Sun Aug 16 | Full-system adversarial review; buffer |
| Mon Aug 17 | P9: video, README, diagram, Devpost draft |
| Tue Aug 18 | Submit **before** 5:00 pm ET |

**Decision gate (spec §14, still open):** if `sqlx` × CockroachDB `VECTOR` (T0.3) is not
working by end of the first P0 session, fall back to Python — the graph logic is identical.
Do not let a driver fight eat a mid-week day.

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
| P6 | 0 / 4 | OPEN for claiming |
| P7 | 3 / 4 | Implementation complete except T7.1 Titan (blocked on account authorization). T7.0, T7.2, and T7.3 are done; P8 owns live hybrid wiring/demo evidence, while ship still needs an index-favorable camera-proof EXPLAIN. |
| P8 | 0 / 5 | blocked on P2 P4 P5 |
| P9 | 0 / 5 | blocked on P8 |

---

## Git workflow (parallel build) — star model

Each **phase** is a `phase/<slug>` branch; each **task** is a worktree on a
`task/<…>` branch. `main` is the single integrator.

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
| P5 | `phase/p5-recall` |
| P6 | `phase/p6-canonization` |
| P7 | `phase/p7-embeddings` |
| P8 | `phase/p8-surface` |
| P9 | `phase/p9-ship` |
