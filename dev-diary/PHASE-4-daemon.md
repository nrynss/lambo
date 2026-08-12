# P4 — Daemon

```yaml
id:       P4
branch:   phase/p4-daemon
requires: [P1]
blocks:   P8; T4.5 blocks P6 (gc_survived is canonization Stage 1's input)
parallel: high   # T4.1 first; then T4.2 ‖ T4.3 ‖ T4.4 ‖ T4.5; T4.6 threads through all
runs-parallel-with: P2, P3, P5, P6, P7
```

**Goal:** the background scorer and sentinel — spec §9. A tokio task that takes the lock,
works, releases, and never holds it across I/O. Everything here is fixture-ok: the daemon
reads graph state, and fixture graphs provide every trigger condition.

**Priority note:** T4.3 (conflict) and T4.5 (GC) carry the demo; T4.4 (drift) is 5th in the
cut order. Sequence accordingly if time compresses.

---

## Integration contracts from P2 review closes (2026-08-11)

Binding notes for P4 tasks; source: grok branch review (CLOSED). Do not re-derive.

- **No Hierarchical acyclicity guarantee from write-time checks (G6 → T4.5 GC).**
  `derive`/`record_action` only reject *self-loop* Hierarchical edges; multi-hop
  Hierarchical cycles ARE writable across calls (`A parent_of B` → `B parent_of C`
  → `C parent_of A`). The daemon must NOT assume Hierarchical acyclicity: every
  traversal (GC disconnected-component cleanup via BFS from the temporal chain,
  drift path finding) needs visited-set cycle detection. `Graph::assert_invariants`
  remains the safety net (its dfs covers Causal/Dependency/Hierarchical).
- **Scaling note (G4 → T4.x scoring/GC benchmarks).** `insert_concept`'s
  canonical-key UNIQUE check is an O(N) RAM scan (no key index — deliberate cut).
  GC must not benchmark long-session writes against this as a hot path without
  first adding a key index.

---

### T4.1 — Scoring
```yaml
requires:   T1.1
fixture-ok: yes
owns:       src/daemon/score.rs
status:     done (2026-08-12, reviewed ACCEPT after 1 remediation round; merged 40fdaee)
```
Spec §9 verbatim:
`score = recency·0.25 + frequency·0.20 + session_activity·0.20 + density·0.35 + edge_type_bonus + concept_type_modifier`
— every dimension clamped to [0,1] **before** weighting; NaN/Inf → 0.0. No centrality (cut).
Also the daemon task skeleton (`src/daemon/mod.rs`
shared claim with T4.6 — coordinate): poll `Graph::epoch()` on a tick interval for
rescoring, with explicit wake in tests. **There is no mutation-notify channel and no
T3.5 rescore signal — both were explicitly deferred** (derive.rs: "do NOT build the
channel here … no stubs, no channel types"; T3.5's handoff ships no transport). The
notify seam lands with T8.1 (COH-5, 2026-08-12).

**Done when:** property tests hold (bounded inputs ⇒ bounded finite score; monotonic in
each dimension) and rescoring `session-rest-api` produces a stable ordering with
`user schema` on top.

---

### T4.2 — Hot list
```yaml
requires:   T4.1
fixture-ok: yes
owns:       src/daemon/hotlist.rs
status:     done (2026-08-12, reviewed ACCEPT; merged c8f64f6)
```
Bounded priority queue, `hot_list_max=1000`. Entry conditions: conflict, high-risk
modification, drift, stale session. **Conditions re-validated on each `recall()`** — stale
entries drop out then, not on a timer.

**Re-validation contract (revised, XP-3 — 2026-08-12).** The seam T5.3 calls is:

```rust
HotList::revalidate(&mut self, graph: &Graph, node: NodeId, now: DateTime<Utc>) -> bool
```

`now` is the **caller's** clock, not an instant captured at detection, and the per-entry
predicate is `Fn(&Graph, DateTime<Utc>) -> Option<HotListPayload>` — it returns the
**recomputed payload**, so a surviving entry's `seconds_ago` / `seconds_inactive` / `hops` is
always read-time state and the refresh cannot be skipped. The original
`revalidate(node) -> bool` over a captured `now` made a recency-bound condition re-validate
`true` forever against an unchanged graph and serve a frozen age — the demo sentence would
still say "eleven seconds ago" five minutes later. Predicates are also **per-node** (CONC-5):
`conflict_at` / `drift_at` / `session_stale_at` / `high_risk_at`, one neighborhood walk each,
not a whole-graph detection pass under the graph lock.

**Done when:** bound enforced under overflow, a condition that stops holding is evicted on
revalidation, and advancing the clock alone (no graph mutation) ages a recency-bound entry
out.

---

### T4.3 — Conflict detection ★ (demo trigger)
```yaml
requires:   T4.1
fixture-ok: yes
owns:       src/daemon/conflict.rs
status:     done (2026-08-12, reviewed ACCEPT; merged c8f64f6)
```
Spec §9: ≥2 active agents with edges to the same node, at least one `Causal`/`Dependency`,
write activity inside `conflict_recency_window=30s`. Emits `DaemonEvent::Conflict` and a
hot-list entry carrying agent id + seconds-ago — T5.3 renders "Agent A wrote to it eleven
seconds ago" from this payload, so include what that sentence needs.

**Done when:** the planted conflict in `session-rest-api` fires; single-agent and
stale-window cases don't (mocked time).

---

### T4.4 — Drift detection
```yaml
requires:   T4.1
fixture-ok: yes
owns:       src/daemon/drift.rs
status:     done (2026-08-12, reviewed ACCEPT; merged c8f64f6)
```
Shortest path over `Causal`/`Dependency`/`Hierarchical` to any root goal node; warn beyond
`drift_threshold=5` hops **or on no path**. Root goal nodes are auto-`Venerable` at
`set_root_goal()` (write the transition through T2.1's mutation path). **Cut-order note:**
5th — keep isolated.

**Metric (XP-9 — 2026-08-12).** Spec §9 says "weighted shortest path", but the threshold is
denominated in **hops** and the fixture's chain is unit-weight, so the operative metric is an
unweighted multi-source BFS hop count. Edge weights are GC's concern (`min_edge_weight`
decay), not drift's. This doc previously said "weighted", which the code never was.

**No-path semantics (ALGO-5 — decided 2026-08-12).** A concept with **no** traversable route
to any root goal is the maximally drifted case and **does** warn, reported with no finite
distance (`DriftHit::hops == None`; the frozen `DaemonEvent::Drift`/`HotListPayload::Drift`
shapes carry the `DRIFT_HOPS_NO_PATH` sentinel against the nil root, and `detail` says it in
words). The earlier reading treated no-path as out of scope, which meant the one case the
spec is least ambiguous about never fired.

**Root-goal shape (ALGO-6 — decided 2026-08-12).** `root_goal` is accepted as a bare string,
**an array of strings** (spec §6.1's own example is a list), or a `{content, key}` object.
`graph::root_goal_texts` is the single reading, shared by `set_root_goal`, drift and GC's
exclusion list. T1.4 carried this decision to P4; the string-only reading stored an array
fine and then silently disabled drift, auto-`Venerable` and GC's root-goal exclusion.
Multiple goals are multiple BFS sources, and **every** named concept is auto-promoted,
id-ascending (ALGO-12 — the previous code took the first `HashMap` match).

**Done when:** `fixtures/session-drift.json` triggers a Drift event for the planted node at 6
hops, plus a no-path Drift for each member of its isolated pair. The isolated pair is *also*
GC's step-3 food; warning once before GC's interval collects it is correct — the daemon
detects every cycle, GC runs every `gc_interval` mutations.

---

### T4.5 — GC ★ (canonization's food)
```yaml
requires:   T4.1
fixture-ok: yes
owns:       src/daemon/gc.rs
status:     done (2026-08-12, reviewed ACCEPT; merged c8f64f6)
```
Spec §9, periodic only, every `gc_interval=10_000` mutations:
1. edge cleanup below `min_edge_weight` past `gc_edge_ttl`
2. concept cleanup: orphans + sub-threshold, **excluding Venerable/Canonical/root-goal**
3. disconnected-component cleanup, BFS from the temporal chain
4. index maintenance (T2.6 hooks)
5. **`gc_survived += 1` on all survivors** — Stage 1's input; the reason GC cannot be cut
6. canonical budget check against `max_canonical_nodes` (delegates to T6.4's demotion)
7. `MutationEpoch += 1` (invalidates T5.4's cache)

`max_concept_nodes` advisory-only: warn, never evict.

**Done when:** the planted disconnected component in `session-drift.json` is collected,
protected classes survive, survivors' counters increment, epoch bumps.

---

### T4.6 — Event channel
```yaml
requires:   T1.1
fixture-ok: yes
owns:       src/daemon/events.rs (+ shared src/daemon/mod.rs with T4.1 — coordinate)
status:     done (2026-08-12, reviewed ACCEPT after 1 remediation round; merged 8bcb816)
```
`tokio::sync::broadcast` of `DaemonEvent` (spec §6.1): Conflict, Drift, Stale, HighRisk,
Canonized. `mem.events() -> Receiver<DaemonEvent>`. A dropped/lagging receiver is not an
error — daemon never blocks on consumers. No callbacks, no pool (deleted wholesale per
spec).

**P8 seam — subscribe BEFORE spawn (CONC-3 — 2026-08-12).** The loop's first cycle is the
warm-up (spec §2.5) and runs immediately, publishing the whole condition set a resumed
session restored — including the planted demo `Conflict`. `broadcast` delivers only what is
sent after a receiver subscribes, and emission is on-transition, so a subscriber created
after `spawn` normally loses that set for good. **P8 must call `Daemon::events()` before
`Daemon::spawn()`.** Pinned by
`daemon::tests::late_subscriber_misses_the_warm_up_condition_set`; the re-arm path (below) is
a liveness backstop for ring eviction, not a delivery guarantee for a late subscriber.

**Emission policy (CONC-2 — 2026-08-12).** Entering conditions publish highest-severity-first
(Conflict, HighRisk, Drift, then the single session Stale). A held pair whose event has been
pushed out of the retained ring window is **re-armed** — re-published — at most **once per
cycle**, oldest-emission-first: re-arming every eligible pair at once would rebuild the burst
that causes the eviction. The guarantee is liveness, not exactly-once. `Stale` is per
**session**, not per concept (spec §9's "stale session"), which is what removed the warm-up
burst that could wrap the demo's `Conflict` out of the ring.

**P6 seam (XP-4 — 2026-08-12).** `Daemon::event_sender()` hands out the broadcast `Sender`;
P6's evaluator calls `events::emit_canonized` with it. Before that accessor existed there was
no public path to the sender anywhere in the crate, so P6's documented seam was unreachable.

**Done when:** slow-consumer test shows the daemon unblocked and the consumer seeing
`Lagged`, not a hang; a non-daemon caller's `Canonized` reaches a `Daemon::events()`
subscriber; and a still-held condition whose event left the retained window is re-published.

---

## Exit criteria

- [x] All five event kinds emitted — per kind, the exact test that proves it
      (all green in the default suite: 328 passed, 0 failed):
      - **Conflict** — `loop_emits_planted_conflict_from_rest_api_fixture`
        (mod.rs; `session-rest-api` fixture, 5s-rebased)
      - **Drift** — `loop_emits_planted_drift_from_session_drift_fixture`
        (mod.rs; `session-drift` fixture, planted "far budget concept")
      - **Stale** — `loop_emits_one_session_stale_from_rest_api_fixture_after_writes_age_out`
        (mod.rs; `session-rest-api` fixture, 2h-rebased — fixture-driven;
        **one** event for the session, not one per concept, since CONC-2),
        plus the synthetic `stale_fires_for_idle_session_after_window_elapses`
      - **HighRisk** — `loop_emits_high_risk_for_fresh_write_to_canonical_node`
        (mod.rs; synthetic clock — spec §6.1/§9 define no quantitative
        high-risk trigger, so no fixture plants one; the entered-gated emit
        path + `events::high_risk_event` mapper were previously untested)
      - **Canonized** — `non_daemon_caller_can_emit_canonized_on_the_daemon_channel`
        (mod.rs; since XP-4): a caller holding only
        `Daemon::event_sender()` emits and a `Daemon::events()` subscriber
        receives — the actual P6 seam, end to end. Plus
        `all_five_kinds_round_trip_through_emit` (events.rs) for the
        serialization round-trip. The emit site is P6 (canonization
        transitions); the loop does not fabricate Canonized events.
- [x] GC full cycle green; canonization inputs (`gc_survived`, epoch) proven —
      `session_drift_disconnected_component_collected` (gc.rs: planted
      disconnected component collected, protected classes survive,
      `gc_survived` 5→6 / 2→3, epoch bumps, invariants clean);
      `bump_gc_survived_increments_and_emits_upserts` (graph.rs: counter
      increments + UpsertNode mutation emission);
      `loop_runs_gc_every_gc_interval_mutations` (mod.rs: interval trigger).
- [x] No lock held across `.await` anywhere in `src/daemon/` — every `.await`
      in non-test daemon code is the `select` (tick/notified) or the bare
      `run_loop(...).await`; parking_lot guards are `!Send`, so
      `tokio::spawn`'s Send bound enforces the discipline at compile time.
      Since CONC-4 the whole cycle body is a **synchronous** `run_cycle`, so
      the rule holds structurally rather than by inspection.
- [x] Per-task review records committed (XP-2) — `adve-review-t4.1-scoring.md`
      … `adve-review-t4.6-events.md`, each labelled *reconstructed post-hoc*
      and each stating what is evidence and what is unrecoverable.

---

## Handoff Log

**What exists now** (all fixture-ok; everything the daemon emits lands on the
§6.1 `DaemonEvent` broadcast channel):

- `src/daemon/mod.rs` — the loop (T4.1 + T4.6): poll `Graph::epoch()` on a tick
  (first cycle is the warm-up), **rescore epoch-gated, detect every cycle**;
  the hot list is set equal to the cycle's fresh hits; events fire on condition
  **entry** (emit-on-transition, never per-cycle duplicates); GC runs every
  `gc_interval` mutations. Public surface: `Daemon::new/with_params/with_clock/
  spawn/wake/scores/events/hot_list`, `CycleParams`, `Clock`, `ScoreTable`.
  Lock order graph → hot, never across an `.await`.
- `score.rs` — composite scoring (spec §9 verbatim, dimensions clamped to [0,1]
  before weighting); `ScoreTable` is daemon-owned, replaced wholesale per
  rescore.
- `hotlist.rs` — bounded priority queue (`hot_list_max=1000`), entries carry
  `Condition` + renderable `HotListPayload`, `revalidate(node)` per recall
  (T5.3).
- `conflict.rs` / `drift.rs` — detectors + hot-list insertion; recency is a
  passed-in `now`, so fixtures and tests mock the clock.
- `events.rs` — broadcast transport (`emit` is fire-and-forget; dropped/lagged
  receivers never block the loop), the `Stale`/`HighRisk` v0.1 triggers, and
  `emit_canonized` — the P6 seam.
- `gc.rs` — periodic GC, spec §9 steps 1–7: edge TTL cleanup, concept cleanup
  (orphans + sub-threshold, excluding Venerable/Canonical/root-goal),
  disconnected-component cleanup (BFS, cycle-safe), `gc_survived += 1` on
  survivors (canonization Stage 1's food), canonical-budget record,
  `MutationEpoch += 1`.
- Seams for downstream phases: P8 wires `mem.events()` to `Daemon::events`
  (**subscribing before `spawn`** — CONC-3) and `Config` into `CycleParams` via
  `Daemon::from_config`; T5.3 renders hot-list payloads (conflict carries
  `agents` + **`writer`** + `seconds_ago` for the "Agent A wrote to it eleven
  seconds ago" sentence — ALGO-2 added `writer` because the subject is not
  recoverable from `agents`), calling `revalidate(graph, node, now)` with its own
  clock; the T8.1 notify seam replaces test `wake()`s; P6 canonization calls
  `emit_canonized` with `Daemon::event_sender()`.

**What surprised:**

- The T4.6 review forced **three loop redesigns**: (1) epoch-gating *detection*
  would have killed idle staleness — an untouched session must age into
  `Stale` purely because time passed, so only the rescore is epoch-gated, the
  four detectors run every cycle; (2) captured-`now` re-validation predicates
  left **ghost hot-list entries** — a frozen-`now` closure re-checks the same
  instant forever, so the loop instead keeps the hot list equal to the cycle's
  fresh `(condition, node)` set (`retain_conditions`); (3) level-triggered
  emission would **flood the 256-capacity channel** with one duplicate per
  cycle per persisting condition — events fire on entry only.
- The spec §9 constant tables (v0.6.0) were interpreted against the v0.1 text
  for the loops: `hot_list_max`, `conflict_recency_window`, `drift_threshold`,
  `gc_interval`, `max_canonical_nodes`, the score weights. Where the spec names
  **no bound** (Stale and HighRisk triggers), the v0.1 interpretation lives in
  `events.rs` module docs as a documented seam T5.x may refine.
- The T2.6 `sync_index` hook had **no production caller** — the inverted index is
  owner-side (P3 contract seam), so `gc::run(&mut Graph, …)` structurally cannot
  reach it. Closed by XP-5: `Daemon::with_index(index)` gives the daemon the
  owner's index and GC mirrors its collections into it after each sweep. An owner
  that prefers to mirror GC itself reads `Daemon::last_gc()` and does not call
  `with_index`.
- Worktree-path incident: one remediation round briefly edited the **main
  checkout** instead of the worktree; contents were restored from the worktree
  and no diff survived — but re-verify paths (main checkout vs worktree) before
  touching files on this branch.

**What not to re-derive:**

- The §10 matrix + score-dimension interpretation notes (which spec clause each
  constant and weight comes from) — recorded in this doc's T4.1 quote and the
  `score.rs` / `events.rs` module docs.
- Planted-fixture semantics: session-drift's "disconnected component" is only
  disconnected from the *temporal* chain — GC tests drop its incident edges
  test-only. Conflict agent attribution is **the acting agent** (ALGO-3, revised
  2026-08-12): an interaction-sourced edge belongs to that interaction's agent; a
  concept→concept edge belongs to the agent acting at the edge's write timestamp
  (`record_action` stamps edges with the interaction's `created_at` verbatim),
  falling back to the source concept's `origin_agent` only when no interaction
  was written at that instant. The superseded rule — "the edge-source-node's
  agent" — mis-credits cross-agent writes whenever `record_action` reuses an
  already-canonical concept as the source, which is the demo's own shape. The
  fixture's caching layer still conflicts (agent-a's `Derives` + agent-b's
  `Dependency` both touch it) and passed under both rules, which is why the
  fixture test was not evidence the old rule was right.

---

## Handoff Log — adve-review P4 remediation (2026-08-12)

`adve-review-p4-daemon-opus.md` (25 findings: 8 P1 / 11 P2 / 6 P3) was
remediated on this branch in eight waves. The review record stays OPEN until the
review loop closes it; this entry is the implementation side.

| Wave | Findings | Commit |
|---|---|---|
| 0 | XP-11, XP-1 — merge final main (`dc5da31`) into the branch; gate the two `use crate::fixtures;` imports behind `#[cfg(feature = "fixtures")]`; `ci.yml` branch glob `'phase-*'` → `'phase/**'` | `fda19d2` |
| 1 | ALGO-1, ALGO-4, ALGO-10, ALGO-11 — GC calibration: `MIN_CONCEPT_SCORE` 0.3 → 0.12 against the observed distribution, frequency renormalized out of the cut while `access_count` is dead, the session's own `ScoringWeights`, per-type bar via `eviction_resistance`, non-finite weights sanitized | `34ba3ca` |
| 2 | XP-3, ALGO-2, ALGO-3, CONC-5 | `49f735e` |
| 3 | XP-4, CONC-2/ALGO-8, CONC-3 | `695938a` |
| 4 | CONC-1, CONC-6/XP-10 | `c3144f8` |
| 5 | CONC-4, XP-5, XP-6, XP-7 | `aa237ad` |
| 6 | ALGO-5, ALGO-6, ALGO-9, ALGO-12, XP-8 | `09e331e` |
| 7 | XP-2, GRAPH-9, docs closure | *this commit* |

### Contract changes (P8 / P5 / P6 must read these)

- **`Mutation::SetRootGoal { session_id, goal }` (XP-8)** — new `Mutation` kind,
  threaded exactly as `CanonizationTransition` is: `types::Mutation`,
  `Graph::set_root_goal` (emits only on an actual change, so the epoch bumps),
  MemoryStore / SqliteStore / CockroachStore appliers, and **SqliteStore's load
  path**, which previously returned `root_goal: None` unconditionally. Both SQL
  schemas already carried `sessions.root_goal`, so no migration.
  **Dependent tasks:** T5.4 gets recall-cache invalidation on a goal change for
  free; **P8 must NOT re-set the goal on load as a workaround** (the log carries
  it now); T6.4 reads the budget signal from `Daemon::last_gc()`.
- **`HotList::revalidate` takes `now` and refreshes the payload (XP-3)** — see
  T4.2 above and the PHASE-5 T5.3 note. This is the API T5.3 is told to call.
- **`ConflictHit` / `HotListPayload::Conflict` carry `writer` (ALGO-2)** — the
  agent of the newest qualifying write, which is the subject of §13's sentence
  and is not recoverable from the sorted `agents` list.
- **`DriftHit::hops` / `::goal` are `Option` (ALGO-5)** — `None` is the no-path
  case; the frozen event/payload shapes carry `DRIFT_HOPS_NO_PATH` and the nil
  root.
- **`Config::drift_threshold` is `usize`** (was `u32`), and `Config` gains
  `daemon_tick_interval` (XP-7). `CycleParams::from(&Config)` is how P8 threads
  it; `CycleParams::default()` is now defined as `from(&Config::default())`, so
  the spec constants have one definition instead of two.
- **New `Daemon` surface:** `from_config`, `event_sender` (XP-4), `with_index`
  (XP-5), `last_gc` (XP-5), `cycles` (XP-6).
- **Additive dependency: `unicode-normalization` (GRAPH-9)** — Unicode **NFC**
  at the head of `normalize_tokens`, so composed and decomposed spellings of the
  same word share one canonical key instead of becoming two concepts
  canonicalization can never merge. NFC, not NFKC: compatibility folding changes
  what the content says. Pure ASCII is a fixed point, so every committed fixture
  and pinned canonicalization case is byte-identical (`gen-fixtures.py` re-run:
  no diff). This closes the one E2E finding that was never dispositioned; the
  E2E record's Not-verified subsection is annotated with the commit.

### Decisions recorded (do not re-derive)

- **GC step-2 scoring stays inside the write guard (CONC-1).** Step 2 must score
  post-step-1 state, so hoisting it to a read snapshot means two write guards
  with a TOCTOU window between them — a concurrent `record_action` can add edges
  to a node already marked for collection. The measured 272ms guard was
  root-caused to `incident_edges` being a full edge scan, which the same finding
  fixes (adjacency index, `O(degree)`). Single-guard atomicity kept.
- **Survivor-bump chunking converges exactly (CONC-6/XP-10).** Chunking changes
  *when* each `UpsertNode` is emitted, never *which*: the emitted multiset is
  identical to the unchunked sweep. GC does not re-run while a drain is
  outstanding, so no concept carries two outstanding bumps. Full argument on
  `gc::drain_survivor_bumps`.
- **Re-arm is one pair per cycle (CONC-2).** Re-arming every eligible pair at
  once rebuilds the burst that causes ring eviction. Oldest-emission-first gives
  round-robin coverage; the guarantee is liveness, not exactly-once.
- **Stale is per session (CONC-2/ALGO-8).** Spec §9 names the condition "stale
  session". With a 30s high-risk window inside a 1h stale window, Stale and
  HighRisk are mutually exclusive by construction — worth knowing before writing
  a test that expects both.
- **Panic containment continues the loop (CONC-4).** parking_lot guards release
  on unwind and do not poison; a partially-mutated graph is still consistent
  with the store because every applied mutation is already in the append-only
  log; the next cycle re-derives everything from graph state.
- **Root-goal shape and no-path drift** — see T4.4 above (ALGO-6, ALGO-5).
- **XP-2 records are reconstructions.** The six `adve-review-t4.*.md` files were
  rebuilt from this doc, the commit history and the code. They carry no reviewer
  prose and no gate numbers, and each says so. T4.1's remediation round and the
  T4.2–T4.5 ACCEPT rounds left no in-repo trace beyond their status lines — the
  task branches carry one commit each, so pre-remediation states were amended
  away. T4.6 is the best-attested: its three loop redesigns are named by number
  in `mod.rs` and described in the Handoff Log above.

### Test posture after remediation

The daemon suite runs on `#[tokio::test(start_paused = true)]` throughout (XP-6),
and every negative assertion waits on `Daemon::cycles()` via `wake_and_settle`
rather than sleeping — "nothing was published" can no longer pass vacuously
because the cycle had not started. One `sleep` remains, inside `wait_until`'s
poll.
