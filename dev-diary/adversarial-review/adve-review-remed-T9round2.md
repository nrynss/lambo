# Adversarial review — remed-T9 (round 2)

**Scope:** re-review of the round-1 REQUEST_CHANGES remediations (2 P2 + 5 P3 + 4 nits) on the T9 worktree (detached HEAD `f7ef210`). Read-only; verified the tracked diff (`src/daemon/mod.rs`, `src/recall/{assemble,candidates,format,mod}.rs`) plus the untracked `src/recall/dispatch.rs` (656 lines), the daemon routing region, `format.rs` blast-radius contract, and all dispatch tests.
**Method:** full diff + full re-read of the changed regions; `cargo check --lib` PASS; ran the dispatch (7), recall (80), daemon (117) and MCP (63) test sets — all green. No source edited; exactly this file written.

**Verdict: APPROVE.**
**Disposition: APPROVE** — all 6 substantive remediations (R1-1, R1-2, R1-3, R1-5, R1-6, R1-7) are genuinely delivered, the instrumentation gating (R1-4) and all four nits (N1–N4) are genuine and complete, and there is no regression, deadlock, or residual ordinary-prose misrouting. Both exemplar failures remain fixed after hardening, and the false-positive test is real and non-vacuous. Two P3s noted below are pre-existing/environmental, neither blocks.

---

## Verification of the remediations

### T9-R1-1 (P2) — classifier hardened — GENUINE
`dispatch.rs:56-64` now has exactly 7 explicit phrasings: `["depends on","depend on","depends-on","dependents of","blast radius","safe to delete","what uses"]`. Bare `"depend"`, `"references"`, `"is it safe"` are dropped. I checked each surviving marker adversarially for ordinary-prose collisions: none of the 7 is a substring of a non-dependency word — `independent` no longer matches (no bare `"depend"`), `references` no longer matches. The pair `"depends on"`/`"depend on"` is *not* redundant: `"depends on"` does not contain the contiguous substring `"depend on"` (test `does anything depend on this` fires only via the `"depend on"` marker); `"depends-on"` and `"dependents of"` each fire distinct forms. No redundant marker remains (N1 resolved).

The new test `marker_bearing_non_dependency_questions_stay_general` (dispatch.rs:491-510) asserts `"is the system independent of a single region"` and `"the report references the changelog"` both classify `General` and that `try_structural` of the former returns `None`. **Non-vacuous**: re-introducing bare `"depend"` would make `classify("…independent…")` return `Structural` and fail the assertion, so the test genuinely guards the hardening.

The anchor-gate (R1-3) is the second line of defense: even a residual marker collision cannot degrade or short-circuit the blend unless `fits_structural` resolves an anchor with structural dependents. So the "no ordinary-prose word can now trip it" acceptance is met.

### T9-R1-2 (P2) — dependents() membership reconciled with §4.1 — GENUINE
`dependents()` (dispatch.rs:190-225) now reuses `format::inbound_sources` and applies the **exact** exclusive-single-source predicate: `dst != anchor && srcs.len() == 1 && srcs[0] == anchor` (dispatch.rs:194). This is bit-for-bit the §4.1 blast-radius predicate used by `format::blast_radii` (format.rs:119-133, same `srcs.len() == 1 && srcs[0] == only`). Multi-source dependents (which survive the anchor's deletion) are **not** returned — the round-1 over-reporting gap and the membership/field self-contradiction are both gone: every returned dependent is, by construction, a node whose sole structural source is the anchor, so no multi-source node is ever stamped as a dependent.

- `max_structural_strength` scans only `STRUCTURAL_EDGE_TYPES` and takes the max over either direction (dispatch.rs:229-241). ✓
- `format::inbound_sources` is now `pub` (format.rs:89). It is used by `blast_radius` (109) and `blast_radii` (120) already and by `dispatch::dependents` (191); the crate is a single lib, so `pub` introduces no external surface or caller to break — `cargo check --lib` clean confirms no unused/lint fallout. Visibility change is safe.
- RDS exemplar: `dependents(SG-Base-VPC)` returns `{12,13,14,11,30}`; RDS (30) scored at strongest edge weight 9.5, ranked first. ✓

### T9-R1-3 (P3) — dispatch gate + full-blend fall-through + lock — GENUINE
`Daemon::recall` (daemon/mod.rs:367-395) computes `dispatch_ready` under a **brief scoped graph read** (`let g = self.graph.read(); dispatch::fits_structural(&g, …)`), and skips the async store-gather **only** when the dispatch is going to fire (`candidates::Phase1Input::default()` at mod.rs:386). A structural phrasing that does *not* dispatch — no anchor or no dependents — runs the full gather and falls through to the complete word/vector/structural blend (mod.rs:387-395). **No keyword-only degraded fall-through remains** in the non-racy case: the only path that skips the gather is `dispatch_ready == true`, and `try_structural` (dispatch.rs:248-330) re-validates under the final lock and returns `Some` there, so the early return always accompanies a skipped gather. The refusal (dispatch returns `None`) always has the full gather.

Lock interaction: the brief `g` guard is dropped at the end of the `if structural` block, before the final `let graph = self.graph.read()` (mod.rs:400) — no overlapping guards, and parking_lot read locks are reentrant anyway, so no double-lock/deadlock. `fits_structural` (dispatch.rs:163-171) is the cheap in-memory predicate: classify + resolve_anchor + dependents-non-empty, matching `try_structural`'s gating exactly.

### T9-R1-4 (P3) — instrumentation gated on `tracing::enabled!` — GENUINE
- `candidates.rs:140-144`: the `legs: Option<HashMap<…>>` is built by `tracing::enabled!(target:"lambo::recall", TRACE).then(HashMap::new)` — when no subscriber, it is `None`, no allocation; the per-candidate `push` and per-output sort/dedup/arm-format (178-196) run only `if let Some(mut legs) = legs`. ✓
- `assemble.rs:165-187`: the `graph.node` lookup, `arm`/`content` string building, and the multi-field `trace!` are wrapped in `if tracing::enabled!(…)`. No eager allocation when disabled. ✓
- Behavior when enabled is unchanged — the `capture_logs(TRACE)` test (`instrumentation_reports_per_hit_arm_contributions`) drives the real general pipeline and asserts the classification, per-hit `"recall arm"` lines and the RDS dependent all appear; the test passes.

### T9-R1-5 (P3) — structural surface renders the ⚑ warning — GENUINE
`try_structural` (dispatch.rs:283-292): for canonical hits it pushes `format::blast_radius_warning(hit.blast_radius.unwrap_or_default())` into `lines` and calls `render_block(&hit, &lines)` — the identical pattern `assemble` uses (assemble.rs:262-266, 279). The structural and blend surfaces render the load-bearing-pillar ⚑ line identically for canonical hits. ✓

### T9-R1-6 (P3) — one-hop divergence documented + canonical-first promotion — GENUINE
Module doc (dispatch.rs:21-27) documents that `traversal_depth` is intentionally *not* honored (one-hop is exactly §4.1 blast-radius; multi-hop is the blend's `expand` domain). `dependents()` orders by **canonical-first** (`is_canonical(b).cmp(&is_canonical(a))`), then strength desc, then id asc (dispatch.rs:210-215) — the same partition `assemble` applies (assemble.rs:198-203). Determinism preserved (id-ascending tiebreak). ✓

### T9-R1-7 (P3) — no-cache/no-hotlist decision documented at the dispatch site — GENUINE, SOUND
daemon/mod.rs:407-414 records: the recall cache stores the blended pipeline to amortize expensive async gather I/O; a dispatched structural query skips the gather (cheap in-memory scan), so caching buys nothing and risks a stale traversal; the daemon HotList tracks conflict/condition entries, not recall recency — and neither the blend nor this path bumps recall recency, so there is no recency to refresh. All three claims are accurate against the code; the decision is sound and now recorded at the site. ✓

### Nits N1–N4 — GENUINE
- **N1** redundant markers: resolved (only the 7 non-redundant phrasings; each fires a distinct form).
- **N2** daemon comment corrected: mod.rs:363-366 & 404-413 now say the gather/blend are skipped *only for a dispatched structural query* — matching actual behavior.
- **N3** test asserts on `result.hits` (dispatch.rs:506, 515-527, 597-600, 650-654); no `let _ = try_structural` remains anywhere in `src/`.
- **N4** warning count uses the kept whole-block count (`let count = kept.len()`, dispatch.rs:322), so the headline never names dependents the truncated context omits.

---

## Explicit judgments

**Both exemplars still green after hardening.** `dependency_question_returns_structural_dependent_by_traversal` asserts RDS surfaces AND is ranked first (score 9.5 > show's 4.5); `delete_safety_question_returns_real_ranking_not_flat_floor` asserts ≥2 hits, non-constant scores, and descending order; `recall_entry_dispatches_structural_query` asserts the end-to-end `Daemon::recall` short-circuits and surfaces RDS. `"is it safe to delete the shared security group"` still classifies `Structural` via the `"safe to delete"` marker and resolves SG-Base-VPC (most-shared, 5 dependents vs PublicWeb's 2). All green.

**Classifier soundness — now SOUND.** Explicit phrasings only; no ordinary-prose word matches; residual generic-dependency phrasings ("…depends on…") are backstopped by the anchor-gate so they cannot degrade the blend. The false-positive test is genuine.

**Traversal/§4.1 soundness — now CONSISTENT.** Membership and the stamped `blast_radius` field both derive from the same single graph pass (`inbound_sources`); the over-reporting and self-contradiction are gone. The `blast_radius` value on a returned dependent is that node's own dependent count (distinct, well-defined), not a re-statement of membership — no contradiction.

**No regressions.** Tracked diff is purely additive and gated (routing + instrumentation + `pub` visibility + module export); existing recall/daemon/MCP tests all pass; `cargo check --lib` clean.

**No deadlock/double-lock.** Brief read is scoped and released before the final lock; only read locks on the same RwLock, reentrant-safe.

---

## Findings

### P1
None.

### P2
None.

### P3
**T9-R2-P3-1 — Stale skip-decision TOCTOU.** `dispatch_ready` is decided under the brief read (mod.rs:376), the gather is skipped, and `try_structural` re-validates under the *final* lock. If the graph mutates between the two reads, `try_structural` could return `None` after the gather was already skipped, falling through to a degraded (empty-gather) blend. **This cannot produce a wrong structural answer** (re-validation under the final lock prevents that), only a rare, cross-thread-mutation-dependent degraded blend. Extremely narrow and inherent to the "decide skip early, validate late" design; acceptable. *Clear.*

**T9-R2-P3-2 — `resolve_anchor`'s SG branch is O(SG × E).** `dispatch.rs:142` calls `dependents(graph, *id)` (each a full `inbound_sources` pass) per SG-shaped concept, and this runs twice (brief read + final lock). Bounded and fine for recall-sized graphs and few SG concepts; worth a precompute-if-it-ever-matters note, not a blocker. *Clear.*

### Nits
**T9-R2-N1 — SG tie-break depends on `graph.concepts()` order.** In `resolve_anchor` (dispatch.rs:142), equal-dependent SG candidates tie-break by iteration order. Deterministic for the exhibit and for any ordered concept store; a note that it is not an explicit key would remove the assumption. *Clear.*

---

## Summary

Round-1's REQUEST_CHANGES has been fully remediated: the classifier is pruned to explicit dependency phrasings with a genuine non-vacuous false-positive test and an anchor-gate backstop (R1-1); `dependents()` shares `format::inbound_sources` and the exact §4.1 exclusive-single-source predicate so membership and the stamped blast radius agree (R1-2); the gather is skipped only when dispatch actually fires, so the refusal is always the full blend and the brief/final locks do not overlap (R1-3); instrumentation is gated on `tracing::enabled!` (R1-4); structural hits render the ⚑ warning identically to the blend (R1-5); one-hop divergence is documented and canonical-first promotion applied (R1-6); and the no-cache/no-hotlist decision is recorded (R1-7). All four nits are resolved. Both exemplar failures remain fixed; all dispatch/recall/daemon/MCP tests and `cargo check --lib` pass. Two clear P3s and one nit are pre-existing/environmental. **Approve.**

---

{ verdict: "APPROVE", findings: { P1: [], P2: [], P3: ["T9-R2-P3-1 stale skip-decision TOCTOU between the brief dispatch-eligible read and the final lock can, only on graph mutation in between, skip the gather and then fail to dispatch into a degraded blend — cannot yield a wrong structural answer, re-validation under the final lock prevents it", "T9-R2-P3-2 resolve_anchor's SG branch recomputes full dependents()/inbound_sources per SG concept, twice per recall — O(SG x E), acceptable at recall graph sizes"], nits: ["T9-R2-N1 SG tie-break in resolve_anchor falls back to graph.concepts() iteration order rather than an explicit key"] }, summary: "All 8 round-1 remediations genuinely delivered; classifier hardened with explicit-only markers and a real non-vacuous false-positive test; dependents() now shares format::inbound_sources with the exact §4.1 sole-source predicate so membership and blast_radius agree; gather skipped only when dispatch fires so the refusal is always the full blend with no overlapping locks; instrumentation gated on tracing::enabled!; structural hits render the ⚑ warning identically; one-hop divergence documented with canonical-first promotion; no-cache/no-hotlist rationale recorded; all four nits fixed. Both exemplars (RDS first for 'what depends on SG-Base-VPC'; real non-flat ranking for 'is it safe to delete the shared security group') still pass, as do the full recall/daemon/MCP suites and cargo check. Two clear P3s and one nit, none blocking. APPROVE." }
