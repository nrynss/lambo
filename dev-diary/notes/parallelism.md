# Parallelism analysis — what can run wide, what cannot

Derived from the spec's dependency structure, not from the spec §14 day plan (which is a
single-developer serialization of this graph; with agents, the graph is the schedule).

## The serial neck (nothing else matters until this is done)

1. **T0.3 — the sqlx × VECTOR spike.** The only task in the project that can invalidate the
   language choice. Run first, alone if necessary. Everything downstream is hostage to its
   go/no-go.
2. **P1 — contracts.** T1.1 (types) → T1.2 (trait + MemoryStore) → T1.4 (fixtures), with
   T1.3 alongside. Roughly one focused session. Rushing it is false economy: every hour
   saved here is repaid as cross-track rework.

## After P1: six independent tracks

| Track | Phase | True upstream | Why it's independent |
|---|---|---|---|
| Graph core | P2 | types only | pure in-RAM data structure work |
| Stores | P3 | trait + DDL + spike | consumes `MutationBatch`, produces `GraphSnapshot` — both frozen in P1 |
| Daemon | P4 | types + fixture graphs | reads graph state; fixtures plant every trigger |
| Recall | P5 | index (T2.6) + fixture graphs | goldens defined against fixtures, not live code |
| Canonization | P6 | MemoryStore's structural queries | predicates are pure functions; live SQL swaps in later |
| Embeddings | P7 | Embedder trait + spikes | capability-gated; nothing blocks on it |

**Maximum useful width: ~6 agents** during Aug 12–13. Beyond that, agents start queueing on
`owns` collisions and cross-track review load.

## Real cross-track edges (task-level, binding)

- T2.6 (index) → T5.1 (recall candidates) — the only P2→P5 edge; land T2.6 early.
- T4.5 (GC) → T6.1 (Stage 1 needs `gc_survived`) — fixtures pre-plant survived counts, so
  T6.1 can *test* before T4.5 lands, but integration waits on it.
- T4.2 (hot list) → T5.3 (force-include + revalidate) — stub behind a trait if T4.2 lags.
- T4.3 (conflict payload) → T5.3 (renders the warning line) — agree the payload fields in
  types.rs at P1 time to decouple fully.
- T3.6 (live structural queries) → P6 exit criterion (same test green on SQLite) — not a
  start-blocker, an exit-blocker.
- T3.2 (CockroachStore) → T7.3 (vector path lives inside it) — same file, same owner;
  sequence, don't parallelize.
- T2.2 (canonicalization) → T7.2 (hybrid sits behind its `Unmatched` seam).

## The convergence point

**T8.1 (Memory assembly) is where every track lands.** Expect the real integration bugs
there — lock discipline across module seams, shutdown ordering, flush-on-close. Schedule it
for the strongest agent/session, not as an afterthought, and start it the moment T2.3/T2.4,
T3.4/T3.5, T4.1/T4.6, and T5.3 exist even if the rest of their phases are open.

## Sequencing within the calendar

- **Aug 10–11 (nominally paused):** P0 + P1 only. They are small; even an hour each day
  keeps Aug 12's fan-out on schedule.
- **Aug 12–13:** all six tracks wide. Priority order if agent-hours are short:
  P2 > P5 > P4 > P3 > P6 > P7 (demo-path first; P3/P7 have external-service risk so don't
  leave them past Aug 13 either — tension noted, resolve toward whatever T0.3 revealed).
- **Aug 14–15:** converge on P8. T8.4 (demo determinism) is the long pole — two full days
  of margin between "demo works once" and "record video" is the plan, not padding.
- **Aug 16:** full-system adversarial review (inner-life practice: findings doc per phase,
  close before proceeding).
- **Aug 17:** P9. **Aug 18:** submit with hours to spare.

## Known external-dependency risks (start early, they don't parallelize with willpower)

- CockroachDB **managed MCP server** setup (console-side) — needed only for demo step 5,
  but it's a third-party flow; verify during P3, not P8.
- Bedrock **model access toggle** — T0.4 exists to catch this on day one.
- ccloud cluster quota/billing — T0.2.
