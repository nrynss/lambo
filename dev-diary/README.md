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
   output in `evidence/`.
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
| Sat Aug 15 | P8 complete: serve, demo scenario reproducible end-to-end | ✅ T8.1–T8.8 DONE/CLEAN and live-verified; T9.1 docs merged (`7b0a9f8`) |
| Sun Aug 16 | Full-system adversarial review; buffer | ✅ T8.9 release process merged; `v0.1.0` published; the CloudOps agents ran and provisioned real AWS infrastructure (`evidence/cloudops-run/`) |
| Mon Aug 17 | P9: video, README, diagram, Devpost draft | ✅ Remediation T1–T12 + E2E closed, hardening H1–H6 closed, `v0.2.0` and `v0.2.1` shipped, D3 docs/submission text done. ❌ Video and Devpost not started; D1 redeploy not started |
| Tue Aug 18 | Submit **before** 5:00 pm ET | **Delivery date. T-1d as of this update (2026-08-17)** |

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
| P8 | 9 / 9 | **COMPLETE (T8.9 merged 2026-08-16, branch `task/release`).** T8.1–T8.8 DONE/CLEAN and live-verified (T8.2 MCP, T8.3 CLI, T8.4 demo ×2, T8.5 serve-web, T8.6 lease incl. the live leg, T8.7 hardening, T8.8 docs, see [PHASE-8-surface.md §Handoff Log](PHASE-8-surface.md)); T8.9 shipped the release workflow that `v0.1.0`/`v0.2.0`/`v0.2.1` then used. Every [exit criterion](PHASE-8-surface.md) is met **except** the concurrency leg (T8.2 N1/N2), which the C-series capture closes 2026-08-18 (C1–C3 DONE on the Linux box — see the stream row below and [notes/concurrency-capture.md](notes/concurrency-capture.md)); the MBP requirement was dropped 2026-08-18 as incidental to what the criterion tests. Ran SERIAL on one branch (decision 2026-08-13) with a task → adve-review → remediation → review loop |
| P9 | 2 / 2 required (+1 / 1 optional) | **T9.1 docs DONE** (merged `7b0a9f8`, on `main` and pushed; the "do not push" release blocker is discharged: three releases published, install path live) · **T9.2 diagram DONE** (mermaid in [README.md](../README.md) §Architecture + the site topology) · **Video NOT STARTED** (tracked as D2, needs D1 first). T9.6 was rewritten the same day so the requirement is the swarm itself rather than a named model or rig. Not covered, recorded in the task: `reserve` coordination was never exercised, and the headline throughput figures come from the fallback loop, so the model-chosen number is the agentic run's 1120 tasks/hour at dedup 0.857. See [PHASE-9-ship.md](PHASE-9-ship.md) |

### Work streams outside the P0–P9 numbering

Three streams opened after P8 and are numbered separately from each other and
from the phases. They are tracked in `notes/`, not in a phase doc, and this is
the only place the board points at them.

| Stream | Doc | State (2026-08-18) |
|---|---|---|
| **T1–T12** remediation | [notes/remediation-tasks.md](notes/remediation-tasks.md) | **ALL DONE**, plus the whole-tree **E2E** review: R1 APPROVE with 3 P3 remediated (`5db0b90`), R2 APPROVE, then a further E2E round merged as `1dd5b48`. `v0.2.0` (`35c86fb`) and `v0.2.1` shipped from it |
| **H1–H7** hardening | [notes/hardening-tasks.md](notes/hardening-tasks.md) | **ALL IMPLEMENTATION TASKS CLOSED.** H1, H2, H3 each ran implement → adversarial review → remediation to CLEAN; H4, H5, H6 were closed by the portal rebuild `5ccd48f` and the `0.2.1` release. **H7 is PARKED / NEEDS DESIGN** and is not claimable until its selection/discovery/URL/auth decisions are recorded |
| **C1–C5** concurrency capture | [notes/concurrency-capture.md](notes/concurrency-capture.md) | **ALL DONE (2026-08-18).** C1–C3: K=12 load driver (`scripts/loadtest/`), SIGTERM capture and durability readback against a scratch SQLite store on the Linux box (cachyos-x8664, 12 threads, so K=12 meets K ≥ the worker count): exact `lambo serve: session closed, tail durable`, 0 `tail lost on exit`, signal→exit 1419 ms, exit 0; interactions yardstick AHEAD by 21 (in-flight writes already swept by the 1 s flush loop, so the close drain was a no-op and `CLOSE_GRACE` stays untested at its limit); concept shortfall 107 fully explained by one daemon GC sweep, proven by the `gc_interval=1` control run; wire-hygiene scan clean. Evidence: `evidence/concurrency/`. **C5 (real models): DONE-with-findings, re-run 2026-08-18.** The load-bearing result is the agentic run (`scripts/loadtest/mcp_agentic.py`: the lambo-cloudops skill text verbatim as system prompt, the four lambo MCP tools only, model-chosen calls), Qwen3-0.6B × 3 agents × 151 s: 55 tasks (1120/hour), **43/55 tasks recall-first and 0 of 45 derives without a prior recall**, dedup 0.857, durability exact after clean SIGTERM. Tool-call capability findings: Qwen3-0.6B emits a valid `lambo_derive` at the protocol level and selects it correctly under a narrowed OMP toolset; LFM2-350M and functiongemma-270m cannot emit tool calls at all (each probed two ways). The `mcp_swarm.py` figures (LFM2 3961 derive-calls/hour, dedup 0.183; Qwen3 2956/hour, dedup 0.893) measure **loop throughput and concept-text behavior, not model tool selection** (that harness hardcodes the call sequence), and Qwen's higher dedup reflects repetition, including one placeholder-echo derive. 22% unparseable turns, and the OMP leg's writes to the inherited live lambo, are disclosed in `evidence/swarm/`. P8's last exit box is ticked. Detail: [§C5 model re-run](#c5-model-re-run-2026-08-18) |
| **D1–D3** deployment & submission | [notes/deployment-and-submission.md](notes/deployment-and-submission.md) | **D3 done** (docs + submission text, landed early). **D1 done 2026-08-18**: the exhibit host is a clean product of `launch_exhibit_ec2.py`, old instance terminated, new one launched from zero, Elastic IP re-associated so `lambo.nryn.dev` needed no DNS change, serving 0.2.2 (evidence `evidence/d1-redeploy/`) — "rebuildable from the scripts alone" is now supportable. **D2 recording NOT STARTED**, no longer blocked |

**Critical path to submission:** D1 → D2 (video) → Devpost, tracked in [notes/deployment-and-submission.md](notes/deployment-and-submission.md). A
real-live capture of `03_crossover_protect.py` is recommended before D2: T8 ran
both cases live, but the committed evidence
(`evidence/remed-t8-crossover-run.md`) is a synthetic recapture with stubbed
I/O.

### C5 model re-run (2026-08-18)

Branch `codex/c5-models`, base `e3715e8`. The re-run replaced C5's single
LFM2-350M result with three models and, more to the point, with a harness that
lets the model decide.

**What it settles.** Given the lambo-cloudops skill text as its system prompt
and nothing but the four lambo MCP tools, a 0.6B model follows the recall-first
protocol: 43 of 55 tasks opened with a recall, and **not one of the 45 derive
calls happened without a prior recall in the same task**. The 12 tasks that did
not open with a recall made no tool calls at all. That is a statement about
lambo's surface being legible to a very small model, which is the thing C5
existed to test. Throughput (1120 completed tasks/hour) is secondary.

**Review loop.** Round 1
([adve-review-c5-models-round1.md](adversarial-review/adve-review-c5-models-round1.md))
verified every probe, ledger, durability and portal claim artifact-exact and
returned REQUEST_CHANGES on 2 P2 / 2 P3, all of them evidence-completeness gaps
rather than wrong numbers. The remediation (`decdc74`) ran the narrowed-toolset
OMP counterfactual and the genuine agentic re-run, corrected the probe
timestamps to UTC, and restated the portal-string counts. **Round 2 was waived
by decision on 2026-08-18:** the result that mattered was already captured and
independently verified at round 1, and the remediation added evidence rather
than revising any checked claim. Recorded here so the waived gate is visible
rather than implied.

**Caveats that travel with the numbers.**

- The `mcp_swarm.py` throughput figures are loop throughput, not agency. That
  harness hardcodes prompt → derive → recall, and its system prompt carries no
  lambo semantics at all.
- Qwen3-0.6B's 0.893 dedup is inflated by repetition: derives that echo recall
  context verbatim, plus one that shipped literal `<concept text>` placeholders.
- The OMP legs executed against the harness-inherited live lambo (agent
  `cursor-agent`) rather than the workspace scratch store, which read back 0
  rows. Disclosed in `evidence/swarm/probes/omp-harness-qwen3-narrowed.txt` and
  `probes/omp-swarm-qwen3-narrowed/README.md`. Those writes are still in that
  store and are an open operator item, not a captured result.

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
| P8 | `phase/p8-surface` (merged to main; T8.9 came in on `task/release` 2026-08-16) |
| P9 | `phase/p9-ship`: **never used.** T9.1 went straight to `main` from `task/t9.1-docs`, and every post-P8 stream (T1–T12, E2E, H1–H3) ran on `codex/<slug>` branches or detached worktrees merged directly to `main`. Treat `main` as the integrator from here |
