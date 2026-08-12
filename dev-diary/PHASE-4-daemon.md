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
modification, drift, stale session. **Conditions re-validated on each `recall()`** — expose
`revalidate(node) -> bool` for T5.3 to call; stale entries drop out then, not on a timer.

**Done when:** bound enforced under overflow, and a condition that stops holding is evicted
on revalidation.

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
Weighted shortest path over `Causal`/`Dependency`/`Hierarchical` to any root goal node;
warn beyond `drift_threshold=5` hops or no path. Root goal nodes are auto-`Venerable` at
`set_root_goal()` (write the transition through T2.1's mutation path). **Cut-order note:**
5th — keep isolated.

**Done when:** `fixtures/session-drift.json` triggers exactly one Drift event, for the
planted node.

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
status:     not-started
```
`tokio::sync::broadcast` of `DaemonEvent` (spec §6.1): Conflict, Drift, Stale, HighRisk,
Canonized. `mem.events() -> Receiver<DaemonEvent>`. A dropped/lagging receiver is not an
error — daemon never blocks on consumers. No callbacks, no pool (deleted wholesale per
spec).

**Done when:** slow-consumer test shows the daemon unblocked and the consumer seeing
`Lagged`, not a hang.

---

## Exit criteria

- [ ] All five event kinds emitted from fixture-driven tests
- [ ] GC full cycle green; canonization inputs (`gc_survived`, epoch) proven
- [ ] No lock held across `.await` anywhere in `src/daemon/`

---

## Handoff Log

> _Fill on completion._
