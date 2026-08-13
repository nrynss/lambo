# Adversarial Review: P6 — Canonization (fable ×5)

```text
╔══════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN                                                    ║
║  Findings: 19 (2 P1 / 5 P2 / 12 P3) — awaiting disposition       ║
║  Opened: 2026-08-13                                              ║
╚══════════════════════════════════════════════════════════════════╝
```

**Phase:** P6 — Canonization (T6.1–T6.4, spec §10)
**Branch reviewed:** `phase/p6-canonization` @ `a4e9cec` (post R1+R2 remediation: 20f88a6, b48ec05, 75b073b)
**Scope:** `src/canon/{stage1,stage2,stage3,eval,mod}.rs`, `src/store/memory.rs` delta, plus every platform seam the diff touches (daemon T4.5/T4.6, stores ×3, recall P5, config)
**Method:** 5 independent fable reviewers, each owning one slice (stage1 / stage2 / stage3 / eval loop / cross-cutting deep), run in parallel against a dedicated worktree. Three reviewers ran **mutation experiments** (mutate → full gate → revert → clean-tree re-verify). Every finding carries file:line evidence and a concrete failure scenario.

**Gates on the clean tree (final):** `cargo test canon` 64/0; `--features store-sqlite` 66/0 (incl. `sqlite_three_hop_progression_matches_memory`); `RUSTFLAGS="-D warnings" cargo check --all-targets` clean on both feature sets.

> **Gate anomaly note:** one mid-review `cargo check -D warnings` failure and one 66-passed/2-failed test run observed by the cross-cutting reviewer were unreproducible on the verified-clean tree. Root cause identified at compile time of this record: the stage1 and stage3 reviewers were running live mutation experiments in the same shared worktree during that window. Final clean-tree runs ×2 are green; not a finding.

---

## Consolidated findings

Duplicates across reviewers merged; original per-reviewer IDs retained in parentheses.

| ID | Sev | Title | Where |
|----|-----|-------|-------|
| F1 | **P1** | Round-robin cursor violates "anti-starvation preserved" — churn skips nodes; sustained churn starves indefinitely (demonstrated) | eval.rs:53-56, 251-265 |
| F2 | **P1** | The evaluation *loop* does not exist: `eval_cycle` has no caller, `canonization_eval_interval` is dead config, and the API shape is unsatisfiable under production locking | eval.rs:118, config.rs:120, daemon/mod.rs:704-736 |
| F3 | P2 | Store error mid-cycle: transition committed to RAM but `Canonized` event lost; entire `EvalOutcome` (incl. durably committed hops) silently dropped | eval.rs:284-294 |
| F4 | P2 | Spec §10 "Canonical nodes always promoted first" in recall is implemented by nobody — a Canonical below the recall cut is silently absent | recall/assemble.rs:125-129, candidates.rs, expand.rs |
| F5 | P2 | SQL `interaction_span` adapters count cross-session origin interactions (no `i.session_id = $1` in span CTE); Cockroach coverage unclamped, can exceed 1.0 | cockroach.rs:372-400, sqlite.rs:173-191 |
| F6 | P2 | Stage 1 peer-set definition ("non-Canonical", not "== None") completely unpinned: spec-deviating mutant survives all 461 tests (demonstrated) | stage1.rs:56 + tests |
| F7 | P2 | Stage 3 `min_edge_age` forwarding untested: mutant dropping the inflation guard ships green through both CI gates (demonstrated) | stage3.rs:57 + tests |
| F8 | P3 | Clock injection stops at the store boundary: `interaction_span`/`blast_radius` use internal `Utc::now()`; one eval cycle reads two clocks; the min-age guard is un-simulatable under a mocked clock | memory.rs:419/365, sqlite.rs:791/744, cockroach.rs:1421/1399 |
| F9 | P3 | `blast_radius` queried twice per Stage-3 promotion (predicate + audit stamp) — stamped value can contradict the gate; cooldown short-circuit sits after the store call | stage3.rs:57-61, eval.rs:211-229 |
| F10 | P3 | `EvalOutcome::stage3_batch` misreports: includes nodes never evaluated after the budget-cap `break` | eval.rs:77-78, 197, 207-209 |
| F11 | P3 | Audit trail unbounded in RAM + store; MemoryStore dedupe is an O(n) scan per event (O(n²) aggregate) | memory.rs:173-179, graph.rs:621 |
| F12 | P3 | Dual-write (immediate `record_canonization` + flush replay) can regress the durable concept status and duplicate an audit hop after a crash; **re-grade P2 if F2 is fixed without addressing this** | eval.rs:284-294, sqlite.rs:1110-1136 |
| F13 | P3 | Per-cycle work unbounded for Stages 1–2: `batch_size` caps only Stage 3; one `interaction_span` query per Candidate per cycle, forever, no cap/backoff | eval.rs:152-180 |
| F14 | P3 | Stage 1 skips NaN sanitization (`sane_weight` convention, ALGO-10); doc claims parity with assemble that doesn't hold | stage1.rs:62-67 vs recall/assemble.rs:120 |
| F15 | P3 | `ScoreTable.epoch` freshness never checked in stage1/eval — P90 cut computed over a population that may not match the score snapshot | stage1.rs:53, eval.rs:118-133 |
| F16 | P3 | Exact `coverage == 0.3` boundary untested: `>=`→`>` regression survives the suite | stage2.rs:30, 40 + tests |
| F17 | P3 | Module-level burst test fires both age gates together; single-gate attacks (fresh edge/old interaction, aged edge/fresh interaction) covered only in store suites | stage2.rs:270-334 |
| F18 | P3 | **P8 watch item:** timestamps are caller-supplied end-to-end (`insert_interaction`); if P8's MCP `record` accepts client timestamps, backdating 61s makes the whole min-age guard a no-op — MCP layer must stamp `created_at` server-side | graph.rs:313, action.rs:115-120, mcp/mod.rs |
| F19 | P3 | Eval test gaps: no churn-ring test (hides F1), no `record_canonization` failure injection (F3 unasserted), no budget-contention (remaining=1, two eligible) or demotion blast-radius tie test | eval.rs:407-1206 |

---

## P1 details

### F1 (E-1, X-4) — Positional cursor skips and starves under churn

`stage3_cursor` is a bare `usize` into a NodeId-sorted Venerable vector **rebuilt every cycle**. Any Stage-3 promotion removes ring members before the cursor; the ring shifts left underneath it and the next window skips the longest-waiting nodes.

- **Demonstrated (common case):** 6 Venerables, `batch_size=2`. Cycle 1 evaluates `[1,2]`, promotes both, cursor=2. Cycle 2's ring is `[3,4,5,6]`, `start = 2 % 4 = 2` → batch is `[5,6]`. A temp test asserting `[3,4]` failed with `left: [nid(5), nid(6)]`. Every successful promotion produces this skid.
- **Demonstrated (unbounded starvation):** victims Venerable from cycle 1; each cycle two fresh Venerables arrive (Stage-2 inflow) straddling the victims in sort order and promote out. 4-cycle temp test: batches `[1,2], [20,21], [3,4], [22,23]` — victims never evaluated; cursor alternates 0→2→0→2 forever.
- Related: when budget is full (`remaining == 0`), `take_stage3_batch` still consumes the window — the ring rotates emptily while `stage3_batch` reports those nodes as evaluated (F10).
- **Fix shape:** anchor the cursor to identity — store the last-evaluated NodeId, start each cycle at the first ring element strictly greater (wrapping). Churn-immune by construction, same cost.

### F2 (X-1, E-2) — No loop, no caller, no owner; API unsatisfiable as shipped

Spec §10: "every `canonization_eval_interval=60s` …". What exists is a cycle *function*; the loop exists nowhere:

- `rg "eval_cycle|Evaluator|EvalParams|canonization_eval_interval|canon::"` across src/daemon, src/main.rs, src/lib.rs, src/cli, src/mcp: **zero hits**. Only `#[cfg(test)]` callers.
- `canonization_eval_interval` (config.rs:120, default 60s) is consumed by nothing — `EvalParams::from_config` (eval.rs:82-91) maps the other five knobs, no timer reads it.
- No wiring plan owns it: P8's T8.1 enumerates "graph + daemon + flush task + startup load" — no evaluator. GC step 6 "delegates demotion to T6.4" (gc.rs:45) — delegating to a component nothing schedules.
- The API **cannot** be hosted by either existing loop without redesign: the daemon cycle is deliberately synchronous (no `.await` may span the graph lock, daemon/mod.rs:707-710), but `eval_cycle` is `async`, takes `&mut Graph`, and awaits store calls (stage2 per candidate at eval.rs:162; stage3 + blast stamp at :211-229; `record_canonization` per hop at :291; one `blast_radius` per Canonical during demotion at :316-318) while the graph is mutably borrowed. Producing `&mut Graph` from `Arc<RwLock<Graph>>` requires holding the write guard across those awaits — which eval.rs's own module doc forbids (eval.rs:35-37) and `parking_lot`'s `!Send` guards reject. There is no third option.
- **Consequence in the assembled product:** no node ever transitions; `canonization_events` stays empty; the P8 demo step "user schema progresses Candidate → Venerable → Canonical" ("do not fake transitions", PHASE-8-surface.md:156-157) is impossible; `max_canonical_nodes` never enforced. T6.4's fixture-level "done when" was genuinely met — the status lines aren't false — but the P6→P8 seam is unowned and the shipped shape blocks the obvious wiring. **Fix shape:** restructure to gather-before-lock (snapshot reads → async store verdicts → synchronous apply under the guard), then schedule it from the daemon loop off `canonization_eval_interval`.

---

## P2 details

### F3 (E-3) — Failed `record_canonization` loses the event and the cycle's outcome
`commit_transition` order is graph-apply → `record_canonization` → emit. On record failure: the transition is already applied to the graph (status changed, audit appended, mutation logged — graph.rs:613-622) but `emit_canonized` never runs, and the `?` aborts `eval_cycle`, discarding the `EvalOutcome` including hops already durably committed earlier in the cycle. The transition reaches the durable audit later via flush (dedupe on id), but its `DaemonEvent::Canonized` is lost forever — by the phase's own standard ("an unrecorded transition is a demo bug"). Realistic trigger: `record_canonization` returns `NotFound` for concepts the flush loop hasn't persisted yet (graph runs ahead of store by the flush interval; flush retains failed batches). Module doc covers only the failed-*apply* direction. Fix: emit after graph-apply (the actual commit point) and return partial outcome alongside the error.

### F4 (X-2) — "Always promoted first" unimplemented
The `[canonical]` marker, `is_canonical` flag, and ⚑ blast-radius warning exist (P5: format.rs:143, assemble.rs:176, :187-190). But recall ordering is purely score-based (assemble.rs:125-129); `CanonizationStatus` appears nowhere in candidates.rs or expand.rs; no force-include, no rank boost. `rg "promoted first"` hits only the spec. A spec §10 line with no owner in any phase plan.

### F5 (S2-1) — Cross-session origin interactions count; Cockroach coverage unclamped
Both SQL span CTEs join `interactions i ON i.id = src.origin_interaction` with no `i.session_id = $1`, while the extent CTE *is* session-filtered; the schema permits cross-session `origin_interaction` (global FK). On a shared cluster, concepts in session S pointing at three ancient interactions in S′ yield `distinct = 3` from interactions never part of S — and since those timestamps can lie outside S's extent, Cockroach's ratio can exceed 1.0 (no clamp, unlike memory.rs:482 and sqlite.rs:833). MemoryStore silently answers differently (skips lookup misses), so the "three-way agreement" oracle only holds on session-local data. Graph-tier `insert_concept` guards the normal path, but `GraphStore::flush` is public and accepts raw mutations. Fix: `AND i.session_id = $1` in both span CTEs + a clamp in the Cockroach CASE.

### F6 (S1-1) — Peer-set semantics unpinned (mutant survives 461 tests)
Mutating stage1.rs:56 from `!= CanonizationStatus::Canonical` to `== CanonizationStatus::None` left the full suite green (456 lib + all integration). Every stage1 unit test uses only `None`/`Canonical` statuses; the fixture holds exactly 20 `None` concepts, so excluding its one Venerable still leaves the gate open at exactly 20 — pure fixture luck. Regression consequence: 19 `None` + 1 Venerable session (spec: evaluate) silently stops promoting; Candidate/Venerable scores silently drop from the P90 distribution. Fix: discriminating tests — 19 None + 1 Venerable must evaluate; a Venerable with a distribution-shifting score must move P90.

### F7 (S3-1) — Stage 3 inflation guard has zero regression protection (mutant survives)
Mutating stage3.rs:57 to pass `Duration::ZERO` instead of `min_edge_age` ran green through both CI gates (456/0 lib; 489/0 with store-sqlite). Every stage3 test passes `Duration::ZERO` (stage3.rs:235, :345, :393, :402, :447) and eval tests override to zero (eval.rs:481). The production default 60s guard (wired at eval.rs:86) would silently vanish in a refactor. Store-level tests cover `blast_radius`'s age gate, not that Stage 3 forwards a nonzero age. Root cause is F8: the wall clock inside `blast_radius` makes a mocked-clock test at this layer impossible — fixing F8 unlocks fixing F7.

---

## P3 details (condensed)

- **F8 (S2-2, S3-2, X-5):** phase doc mandates clock injection; cooldown honors it, but `interaction_span`/`blast_radius` compute `cutoff = Utc::now() - min_age` internally in all three adapters. One eval cycle reads two clocks; every eval-level test must zero the guards to escape the wall clock, so no test exercises the loop with the 60s guard active; deterministic replay with production parameters is impossible as built. Thread `now` through the two store methods.
- **F9 (S3-3, E-4):** `stage3_passes` discards the measured blast count, so eval re-queries to stamp the event — up to 100 queries/cycle, and against a live shared store the stamped value can be ≤ 5 on a promotion row (`canonization_events` contradicting the gate that admitted it — the on-screen demo artifact). Also, the expensive `blast_radius` call runs before the cheap cooldown short-circuit, and a store error on a cooling node aborts the cycle. Return the measured `u64` from `stage3_passes`.
- **F10 (E-5):** `stage3_batch` documented as "in the order it was evaluated" but recorded before the budget-cap loop; after a `break` (or when budget is already full) it lists nodes never run through the predicate.
- **F11 (E-6):** `canonization_events` grows forever in graph RAM and store; MemoryStore dedupe scans the whole vector per event. Growth is arguably spec-intended (full audit); the O(n²) scan is not — index seen ids.
- **F12 (X-3):** flush replay's concept-row UPDATE is unconditional, not monotonic. Lagging flush of hop1 committing after hop2's immediate write regresses the durable status; self-heals on hop2's flush *unless* the process crashes first — reload then yields a regressed status while the audit durably holds the later hop; the evaluator re-promotes under a fresh event id → the same hop appears twice in the on-screen audit. Unreachable today only because of F2. Guard: flush skips the status UPDATE for already-recorded event ids, or a status-version check.
- **F13 (X-6):** Stage 2 issues one `interaction_span` store query per still-Candidate concept per cycle, unbounded, forever — N sequential SQL round-trips per 60s against Cockroach. No cursor/cap/backoff. Scale note for whoever fixes F2.
- **F14 (S1-2):** stage1 uses raw `ScoreTable` values where assemble wraps the identical lookup in `sane_weight` (ALGO-10); ≥3 NaNs among 20 peers → P90 = NaN → stage silently closed. NaN unreachable from `rescore` today (all divisions guarded), hence P3 — but the stage1.rs:15 doc claim of parity with assemble is inaccurate either way.
- **F15 (S1-3):** `ScoreTable.epoch` exists for freshness and recall refuses stale tables (daemon/mod.rs:387-390); stage1/eval accept any table — post-rescore inserts enter the peer distribution at 0.0, inflating n and flooding the bottom of the P90 population. Assert/debug-log `scores.epoch == graph.epoch()` in `eval_cycle` or document caller-owned freshness.
- **F16 (S2-3):** code is correctly inclusive (`>= 0.3`) but tests bracket at 0.2/0.5/0.545 — never pin exactly 0.3 → pass. (`distinct == 3` exactly *is* covered.)
- **F17 (S2-4):** the burst test's burst is both-fresh (edge gate and interaction gate fire together); the two single-gate attacks live only in store suites — the stage2 module test stays green if a future adapter drops one gate.
- **F18 (S2-5):** `insert_interaction` accepts caller-set `created_at`; edges inherit it (action.rs:115-120); no server-side stamping exists (MCP is a P8 stub). If P8's `record` accepts client timestamps, backdating 61s neutralizes the entire inflation guard. **Must be on the P8 review checklist: MCP stamps `created_at` server-side.**
- **F19 (E-7):** the ring test uses a static 55-node ring (zero promotions — exactly the churn F1 breaks is untested); no mid-cycle `record_canonization` failure injection; no remaining=1-with-two-eligible budget-contention test; no demotion blast-radius-tie test.

---

## Verified holds (attacked, did not break)

**Stage 1:** nearest-rank `ceil(0.9n)` arithmetic exact at integral boundaries (f64 rel-error below half-ulp; n=10/20/21 locked incl. R7 interpolation rejection); gate at exactly 20 both directions; exactly-at-P90 rejected / just-above passes with duplicate scores at the cut; `gc_survived >= 3` floor locked both sides; empty/degenerate peer sets safe (P90=+∞, rank clamped); missing score → 0.0 locked against implicit-pass mutant; not-leave-one-out is a documented spec-compatible choice; output deterministic (NodeId asc, `total_cmp`); no zombie peers (remove_node deletes fully; `concepts()` yields live Concepts only); Stage 1 correctly time-free (cooldown is Stage 3's job per spec placement).

**Stage 2:** thresholds `>= 3` / `>= 0.3` match spec exactly; **both** edge and origin-interaction ages gated in all three adapters (2026-08-11 errata semantics); distinct by interaction id (one interaction with N edges counts once); edge-kind/direction exact (inbound Dependency/Causal/Hierarchical, sources pinned to concepts, Derives/Temporal/CoOccurrence/Semantic excluded); F1-rule div-by-zero consistency across adapters with per-adapter tests; min_age wired from config (60s default locked); **eviction immunity real**: GC `is_protected` includes Venerable, excluded from scoring and disconnected-component cuts, isolated-Venerable-island test asserts survival + `gc_survived` bump; sqlite bind order correct; `cutoff()` overflow errors instead of panicking; threshold tests pin the other variable above threshold (gates can't pass on each other's strength).

**Stage 3:** strict `> 5` matches spec, both sides pinned; `blast_radius` is real hypothetical-removal orphan counting in all three backends (not degree), matching spec §4.1 + errata, with cross-backend agreement tests; cooldown boundary blocked at 300s−1ns / allowed at exactly 300s with a genuinely discriminating mocked clock; `None` demotion time = no cooldown; overflow fails closed; store errors fail closed (`?`, tested); only budget demotion sets `last_demotion_time`, and all three stores + graph COALESCE-refuse to clobber it with `None` (COH-3); cooldown survives restart on durable backends (round-trip tested); no stage skipping (caller filters Venerable, re-checks pre-commit, `legal_canonization_transition` rejects fabricated hops).

**Eval loop:** b48ec05 cap genuinely holds — `remaining` computed after Stages 1/2, decremented only on committed promotions, cycle can never exceed the bound, same-tick promote-then-demote structurally impossible (the replaced test was upgraded, not lost); 20f88a6 audit dedupe holds in all three stores (`ON CONFLICT (id) DO NOTHING` shared by record + flush replay; dup test passes); one hop per cycle enforced by `hopped` set + per-hop status re-checks + graph state machine; demotion deterministic (blast asc, NodeId asc), sets `last_demotion_time`, nulls blast, writes audit — matches spec; cursor *arithmetic* per se safe (no wrap off-by-one, no dup ids, empty ring safe — the defect is identity-anchoring, F1); CON-6 narrowing via `i32::try_from`, tested.

**Cross-cutting:** all six config knobs exist with spec defaults, five of six consumed (the sixth is F2); Stage 1 reads the real T4.5 `gc_survived` field GC increments, protected from idle inflation by NEW-2; single `CanonizationStatus` enum, all writes through `apply_canonization_transition`, legal-transition gate makes stage skips impossible by construction; persistence parity real across all three stores (status + blast + COALESCE'd demotion time + deduped audit, on both mutation and record paths; SQLite three-hop progression parity test green); fixture-first claim true (MemoryStore + `session-rest-api`, planted full-progression `user schema` node rewound and re-progressed three hops with per-hop audit rows, plus stage-2-pass/stage-3-fail twin `api layer`); `emit_canonized` goes through the counted `EventSender` so P4 ring-eviction re-arm accounting holds.

---

## Disposition

Pending. Suggested remediation order: F2 (restructure to gather-before-lock + daemon scheduling — everything else lands in the code this creates), F1 (identity-anchored cursor, trivial once F2's shape is settled), F3/F12 together (commit-point + dual-write semantics), F5 (two-line SQL fix), F6/F7/F16/F19 (test hardening wave), F8 (thread `now` through the two store methods — unlocks F7's proper test), F4 (recall-side, needs a decision on force-include vs rank-boost), F9/F10/F11/F13/F14/F15/F17 (polish wave), F18 (P8 checklist item, not a P6 change).

Reopen criteria: any regression in the "Verified holds" list above.
