# P6 — Canonization

```yaml
id:       P6
requires: [P1, T4.5]     # Stage 1 consumes gc_survived; stages test fixture-first via MemoryStore
blocks:   P8 demo story
parallel: medium   # T6.1 ‖ T6.2 ‖ T6.3 (independent predicates); T6.4 integrates
runs-parallel-with: P2, P3, P5, P7
```

**Goal:** spec §10, retained whole. **This is the differentiator and it does not get cut.**
Nobody else ships memory that decides for itself what is load-bearing.

All three stage predicates are pure functions of (graph state, store queries, config,
clock) — build and test each against `MemoryStore` + `session-rest-api` (which plants a
lawful full-progression node, T1.4), swap to live structural queries when T3.6 lands.
Inject the clock; every threshold here is time-sensitive and untestable otherwise.

---

### T6.1 — Stage 1: Candidate
```yaml
requires:   T1.1, T4.1
fixture-ok: yes
owns:       src/canon/stage1.rs
status:     not-started
```
`gc_survived >= 3` AND composite score above the 90th percentile of non-Canonical peers —
evaluated only when ≥ `canonization_min_peer_count=20` non-Canonical concepts exist.
Percentile over daemon scores (T4.1's output).

**Done when:** peer-count gate, percentile edge (exactly-at-P90), and the fixture's
planted candidate all behave.

---

### T6.2 — Stage 2: Venerable
```yaml
requires:   T1.1, T1.2
fixture-ok: yes
owns:       src/canon/stage2.rs
status:     not-started
```
Inbound `Dependency`/`Causal`/`Hierarchical` sources tracing to ≥3 distinct interactions
spanning ≥0.3 of session temporal extent — via `store.interaction_span()`. Only edges and
interactions older than `canonization_edge_min_age=60s` count (**the adversarial inflation
guard — test it adversarially**: a burst of fresh edges must not promote). Venerable ⇒
eviction-immune (T4.5 already excludes; assert the wiring).

**Done when:** the fixture's stage-2-pass/stage-3-fail node goes Venerable and a
freshly-inflated twin does not.

---

### T6.3 — Stage 3: Canonical
```yaml
requires:   T1.1, T1.2
fixture-ok: yes
owns:       src/canon/stage3.rs
status:     not-started
```
`store.blast_radius() > 5` hypothetical-removal orphans. Re-promotion blocked for
`canonization_repromotion_cooldown=300s` after any demotion (`last_demotion_time`).

**Done when:** the planted pillar promotes, the sub-threshold node doesn't, and a
just-demoted node is refused until cooldown lapses (mocked clock).

---

### T6.4 — Evaluation loop, budget, demotion, audit ★
```yaml
requires:   T6.1, T6.2, T6.3, T4.6
fixture-ok: yes
owns:       src/canon/mod.rs
status:     not-started
```
Every `canonization_eval_interval=60s`: ≤ `canonization_eval_batch_size=50` Venerable nodes
per cycle, round-robin cursor, score-descending within batch (anti-starvation preserved).
Budget: `max_canonical_nodes=1000`, lowest-blast-radius demoted first. Demotion sets
`last_demotion_time`, nulls `blast_radius`, writes `canonization_events`. **Every**
transition (up and down) goes through `store.record_canonization()` and emits
`DaemonEvent::Canonized` — the audit trail is what the CockroachDB MCP server shows on
screen in the demo (spec §13 step 5); an unrecorded transition is a demo bug.

**Done when:** fixture session progresses None → Candidate → Venerable → Canonical across
simulated eval cycles with a `canonization_events` row per hop, and budget overflow demotes
the lowest blast radius with a recorded event.

---

## Exit criteria

- [ ] Full three-stage progression reproducible in one test (mocked clock, MemoryStore)
- [ ] Same test green against SQLite once T3.6 lands (the swap proves the abstraction)
- [ ] Inflation guard and cooldown adversarially tested
- [ ] Audit trail complete: transitions in test == rows recorded

---

## Handoff Log

> _Fill on completion._
