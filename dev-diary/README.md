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

---

## Phase graph

| Phase | Doc | Requires | Runs parallel with | Blocks |
|---|---|---|---|---|
| **P0** Ground & spike | [PHASE-0-ground.md](PHASE-0-ground.md) | — | (T0.1‖T0.2‖T0.4 internally) | everything |
| **P1** Contracts & fixtures | [PHASE-1-contracts.md](PHASE-1-contracts.md) | T0.1 | — | everything after |
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
| P0 | 4 / 4 | **GO on Rust** — T0.1–T0.3 done; T0.4 blocked on Bedrock use-case form (crate ready) |
| P1 | 4 / 4 | **COMPLETE — T1.4 fixtures landed: P2–P7 unblocked** |
| P2 | 0 / 7 | OPEN for claiming |
| P3 | 0 / 6 | OPEN (T3.1 DDL can start after T0.3) |
| P4 | 0 / 6 | OPEN for claiming |
| P5 | 0 / 4 | OPEN for claiming |
| P6 | 0 / 4 | OPEN for claiming |
| P7 | 1 / 3 | T7.0 done (BGE-M3 embedder); T7.1 blocked on account; T7.2/T7.3 OPEN |
| P8 | 0 / 5 | blocked on P2 P4 P5 |
| P9 | 0 / 5 | blocked on P8 |
