# Adversarial Review: Remediation T3 — `/api/graph`, `/api/inspect`, T11 gate-progress payload, token comparator (worktree `remed-T3`, round 1)

```text
╔════════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — Round 1 of the review/remediate loop               ║
║  Scope:  T3 — (A) /api/inspect, (B) /api/graph, (C) T11 gate-      ║
║          progress in /api/inspect, (D) constant-time token         ║
║          comparator (T1-P3-1).                                      ║
║  Branch: remed-T3 (worktree /home/nryn/work/worktrees/remed-T3,     ║
║          detached HEAD @ f158720, 6-file uncommitted diff:          ║
║          serve_web.rs +487, canon mod/stage1/stage2/stage3, new     ║
║          src/canon/gate.rs)                                         ║
║  Date:   2026-08-17                                                 ║
║  Reviewer: T3ReviewR1 (read-only)                                   ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 5 P3 / 3 nits.                   ║
║          All four parts implement their contracts faithfully. The   ║
║          P3s are fidelity/test-coverage gaps (the blast-radius       ║
║          gate is the only metric that re-computes instead of        ║
║          surfacing the eval's own number, plus per-endpoint         ║
║          blast-radius semantic drift, a truncation flag false-      ║
║          positive, the un-exposed re-promotion cooldown, and an     ║
║          untested truncation path). None defeats the task's goal.   ║
╚════════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed read-only in the `remed-T3` worktree. Read the `## T3` + `## T11`
sections of `dev-diary/notes/remediation-tasks.md`, the full 6-file diff
(`git diff HEAD`: `src/canon/{mod,stage1,stage2,stage3}.rs` + new
`src/canon/gate.rs`, `src/cli/serve_web.rs` +487/−13), re-read every changed
region in context on disk, and traced each contract point against the code
(`resolve_focus`, `format::blast_radii`, the stage modules' thresholds and
comparison operators, `sqlite.rs` `STRUCTURAL_EDGE_IN`/`blast_radius`/
`interaction_span`, and the no-writer-lease test). Ran targeted builds/tests
(Main owns full-suite/formatter/clippy):

- `cargo check --all-features` — **clean.**
- `cli::serve_web::tests` — **25 passed / 0 failed**, including the four new
  tests (`inspect_endpoint_reports_a_focus_...`, `inspect_endpoint_miss_is_a_200_...`,
  `graph_endpoint_returns_the_structural_skeleton`, `tokens_match_scans_...`)
  and — critically — `the_module_registers_only_get_routes` (the **no-writer-lease
  source-grep test**), `routes_constant_covers_every_registered_route`, and
  `read_only_router_has_no_mutating_route` all still pass.
- `canon::*` — **59 passed / 0 failed / 1 ignored** (stage1/2/3 + eval + task
  suites, confirming the `pub(super)` threshold exposure changed nothing).

---

## Part A — `/api/inspect?focus=<string>&depth=1`

**Contract verdicts:**

1. **Structural edges only, `CoOccurrence` absent — PASS.**
   `structural_dependents` (`serve_web.rs:503`) keeps only
   `Dependency | Causal | Hierarchical` via `is_structural` (`:494` matches
   `EdgeType`, mirroring `sqlite.rs:207` `STRUCTURAL_EDGE_IN`), dedups neighbours
   by `NodeId`, and guards `Node::Concept`. The new test asserts no
   non-structural edge leaks onto the page.
2. **A miss is 200 `found:false`, never non-2xx — PASS for ALL miss kinds.**
   `resolve_focus` returns `Exact`/`Fuzzy`/`Ambiguous`/`Missing`/`Oversized`.
   `api_inspect` maps `Ambiguous | Missing | Oversized` to `None` (`.896-898`)
   → `InspectResponse::missing` (200). A **blank** focus is short-circuited to a
   200 miss (`.868-872`). An **interaction-node** focus (`focus=<interaction-uuid>`
   resolves through the `g.node(id).is_some()` UUID leg, `.64-70`) hits
   `Some(Node::Interaction(_))` → `_ => None` → 200 miss. Every reachable path is
   a 200 miss; the route never returns a non-2xx for a miss.
3. **Read-only, no writer lease — PASS.** Both new routes go through
   `load_reader_graph_with_contract` (no `Memory::builder`/`open_writer`/
   `acquire_lease`/`.spawn()`), and the read guard is released before the
   (`!Send`) gate-progress await (scoped block `.885-900`, comment `.883`). The
   no-writer-lease source-grep test **passes**; the new routes were added to the
   `ROUTES` list (`.1284-1296` + `.1290/.1291`) so the method sweep covers them,
   and `routes_constant_covers_every_registered_route` confirms the list is
   complete.
4. **depth ignored/treated as 1 — PASS (per contract).** `InspectParams.depth`
   is `#[allow(dead_code)]`, deliberately ignored; hop-1 only.
5. **Dependents bounded by `MAX_INSPECT_NODES` (=64) with `truncated`, not a
   silent cut — PARTIAL; see T3-R1-3** (false-positive `truncated` flag) and
   T3-R1-5 (no test exercises the bound). The cap is honoured; the *flag* can
   misfire.
6. **Status strings — PASS.** `status_str` (`:597`) yields
   `"None"|"Candidate"|"Venerable"|"Canonical"`; `Option` + `skip_serializing_if`
   omits it (and `gate_progress`) on a miss, matching the contract's miss shape.

## Part B — `/api/graph`

**Contract verdicts:**

1. **Structural edges only, no `CoOccurrence` — PASS.** `:975` filters
   `is_structural`, and both endpoints must be `Concept` (`:977`). The T7 false
   Lambda→RDS `CoOccurrence` edge cannot appear.
2. **`status`/`blast_radius` from the concepts row — PASS per the contract
   letter, but the semantics matter (see T3-R1-2).** `status` from
   `canonization_status`; `blast_radius` from the persisted frozen
   `Concept::blast_radius` (`c.blast_radius.unwrap_or(0)`, `.964`), i.e. only set
   on promotion to Canonical, cleared on demotion — exactly "from the concepts
   row" as the contract specifies, but it differs from `/api/inspect`'s *live*
   computed count (see finding).
3. **Bounded (4096/16384) with `truncated` — PASS (but untested; T3-R1-5).**
   `nodes_trunc`/`edges_trunc` are computed from the *pre-truncation* lengths, so
   `truncated:true` is truthful here. Deterministic ordering: nodes sorted by
   content/status; edges sorted by `structural_rank` (`'Causal' < 'Dependency' <
   'Hierarchical'`, matching the reference SQL's TEXT ordering) then content —
   deterministic (see Verified-OK).
4. **Read-only, no writer lease — PASS.** Same reader path; no-writer-lease test
   intact.
5. **Renderer contract — PASS.** `nodes` = `{content,concept_type,status,blast_radius}`,
   `edges` = `{parent,child,edge}`, top-level `{session,nodes,edges,truncated}` —
   field names/shapes exactly match the doc. Building from the in-memory `Graph`
   produces the same structural skeleton as the reference SQL (both filtered to
   the three structural types and both endpoints concepts); no shape divergence.

## Part C — T11 gate-progress payload (`/api/inspect`)

**Is it genuinely *surfacing* (reusing the eval's own queries/thresholds) or a
parallel re-computation?**

- **`gc_survived` — surfaced (correct).** Read from the persisted
  `Concept::gc_survived` field (`gate.rs:117`), the same field Stage 1 filters
  (`stage1.rs:74` `c.gc_survived >= MIN_GC_SURVIVED`). Operator matches
  (`>=`, `at_least`). Same source.
- **`distinct_interactions` / `coverage` — surfaced (correct).** Re-run via
  `store.interaction_span(session, id, min_age, now)` (`gate.rs:113-115`), the
  exact query Stage 2 uses, with the same `min_age` (serve-web passes
  `canonization_edge_min_age`, which is what the eval passes to Stage 2).
  Comparison operators match Stage 2 exactly
  (`stage2.rs:48`, `>=` for both). Same query, same operator, same threshold.
- **`blast_radius` — PARALLEL RE-COMPUTATION, the one drift (T3-R1-1).**
  `gate_progress` uses a caller-supplied `blast` that serve-web computes from
  `format::blast_radii(&g)` over the *in-memory graph* (`serve_web.rs:890`),
  which applies **no `min_edge_age` cutoff**. Stage 3 measures
  `store.blast_radius(session, node, min_edge_age, now)` (`sqlite.rs:899`, with
  `e.created_at <= ?`). The operator matches (`>`, `strictly_above`) but the
  **source differs on edge age**. This is the sole place the payload does not
  surface the eval's own number — it re-calculates a near-mirror. See finding.
- **Bars single-sourced — PASS.** `MIN_GC_SURVIVED`/`MIN_DISTINCT`/
  `MIN_COVERAGE`/`MIN_BLAST_RADIUS` promoted to `pub(super)` and imported into
  `gate.rs` from the stage modules (`stage1.rs:51`, `stage2.rs:35,37`,
  `stage3.rs:57`) — one source, not restated in the web layer. Values verified
  unchanged (3, 3, 0.3, 5).
- **Payload shape per T11's design — PASS.** `GateProgress` =
  `{gc_survived, blast_radius, distinct_interactions, coverage}`, each a
  `GateMetric` `{current, bar, met, strictly_above}`; `strictly_above` is `true`
  only for blast radius (Stage 3's `>` semantics), matching how the eval decides
  promotion. Per-concept and additive.
- **Degradation to omitted keeps 200 — PASS.** `gate_progress` failure (`Err`)
  is caught and logged, `gate_progress: None` (`serve_web.rs:919-924`); the route
  still returns `200`. Additive field, `skip_serializing_if`.
- **Is re-running store queries honest/sound for a lease-free reader? — YES.**
  A reader cannot see the eval's transient in-process numbers, so re-issuing the
  eval's *own* queries against the persisted store is the correct, honest way to
  surface them; it is not a parallel metric that could disagree with the engine
  (except the age-cutoff note above), and the read guard is not held across the
  await.
- **`met_count` — correct** (`gate.rs:85-95`), counts the four `met` flags.

## Part D — constant-time token comparator (T1-P3-1)

**Contract verdicts — PASS (all).**

1. **Loop count independent of `presented` length — PASS.**
   `tokens_match` now iterates `expected.iter().enumerate()` (`serve_web.rs:228`),
   so the iteration count is fixed by the *secret's* length, never by `presented`.
2. **No panic / no OOB — PASS.** `presented.is_empty()` is guarded before the
   `i % presented.len()` modulo (`:231-235`); empty `presented` substitutes
   `0` (any mismatch against `exp_byte` sets `diff`, so empty is refused).
   `i < expected.len()` and `i % presented.len() < presented.len()` — in bounds.
3. **Length case still refused — PASS.** `diff` is seeded with
   `(presented.len() ^ expected.len())` (`:227`), so any length difference keeps
   `diff != 0` and returns `false`, even when the shorter input is an exact
   prefix (`"s3cr"` vs `"s3cret"`) or padded (`"s3cret-extra"`).
4. **Genuinely constant-time — PASS.** Accumulate-then-test: no early return, no
   data-dependent branch on secret bytes, `black_box(diff)` stops the optimiser
   from proving the accumulator out. The only branch (`presented.is_empty()`) and
   the divisor (`presented.len()`) depend on *input length*, a per-request
   constant, not on the secret.
5. **Comparison correct for equal tokens — PASS.** `"s3cret"` vs `"s3cret"`
   leaves `diff == 0` → `true` (test asserts it).
6. **Test pins non-short-circuit — PASS (not vacuous) but with an overstated
   comment (nit T3-R1-N2).** The *decisive* regression guard is
   `tokens_match(b"s3cr", token)` **must be false**: a naive `zip`-style
   short-circuit would compare only the overlapping 4 bytes, find them equal, and
   return `true`. The padded case and the single-byte cases also pin the length
   fold and last-position scan. The comment's claim that the *last-byte-differs*
   assertion "proves the scan reaches the end" is inaccurate (a short-circuit
   catches a first-diff at any depth too) — the actual proof of full scan is the
   prefix/truncation refusal.

---

## Findings

### P3

#### T3-R1-1 (P3) — the blast-radius gate is re-computed, not surfaced: it lacks the eval's `min_edge_age` cutoff, so `met` can be optimistic on fresh edges
- **Where:** `src/canon/gate.rs:118` (`GateMetric::strictly_above(blast_radius, …)`),
  fed by `src/cli/serve_web.rs:890` (`blast = blast_radii(&g)…`) and used at
  `serve_web.rs:929`.
- **What:** T11's charter is "surface, don't calculate — every number is already
  computed during evaluation and discarded". `gc_survived`, `distinct`,
  `coverage` are surfaced faithfully. **Blast radius is not:** the eval's Stage 3
  measures `store.blast_radius(session, node, min_edge_age, now)`
  (`sqlite.rs:899`, which adds `e.created_at <= now - min_edge_age`), while the
  payload uses `format::blast_radii(&g)` over the in-memory graph with **no age
  cutoff** (`recall/format.rs:119`). `gate.rs`'s own doc claims it "mirrors
  `GraphStore::blast_radius`"; the mirror omits the age filter.
- **Why it matters:** with structural edges younger than `min_edge_age` (default
  60s — exactly what fresh derives in a live demo produce), the surfaced
  `blast_radius.current`/`met` can report `met:true` (e.g. 6 > 5) when the engine
  would **not** yet promote (its aged count ≤ 5). The page's whole job is to
  explain "why not canonical yet", and this gate can give an optimistically-wrong
  answer until the edges age. It is the single place the implementation drifts
  from the "surface" charter. Borderline P2; held at P3 because it is
  self-consistent with the payload's own `blast_radius` field, bounded to the
  fresh-edge window, and resolves on its own in ≤ `min_edge_age`.
- **Fix:** pass the eval's own measurement — call `store.blast_radius(session,
  node, min_edge_age, now)` in `gate_progress` (the store is already available)
  and use its result for the `blast_radius` metric, exactly as Stage 3 does; or
  have the eval persist its measured (aged) count. Then the surfaced value *is*
  the number the engine compared.

#### T3-R1-2 (P3) — `/api/graph` and `/api/inspect` report `blast_radius` with different (live vs frozen) semantics; the tree can't mark load-bearing non-canonical nodes
- **Where:** `src/cli/serve_web.rs:964` (`/api/graph`, `c.blast_radius.unwrap_or(0)`)
  vs `serve_web.rs:890` (`/api/inspect`, `blast_radii(&g)`).
- **What:** `/api/inspect` uses the live computed dependent count;
  `/api/graph` uses the persisted `Concept::blast_radius`, an `Option<i32>` that
  is `Some` only after promotion to Canonical and reset to `None` on demotion
  (see `canon/eval.rs` `promotion_event`/demotion). So a `Candidate`/`Venerable`
  node with many dependents reports `blast_radius: 0` on the tree, even though it
  is the load-bearing pillar the tree is meant to foreground.
- **Why it matters:** T3's stated purpose for grafting `status`+`blast_radius`
  onto `/api/graph` is "the tree can mark load-bearing nodes without a second
  call". The frozen field under-marks exactly the nodes that have not yet been
  promoted — the interesting ones. It is letter-compliant with the contract
  ("from the concepts row") but undermines the feature's intent, and two adjacent
  endpoints now answer "blast_radius" with different provenance.
- **Fix:** either have `/api/graph` also use the computed count (mutual
  consistency, correctly marks Candidates), or keep the frozen field and add one
  doc sentence naming the difference so a caller does not conflate the two. Add a
  test that a non-canonical node with dependents reports a nonzero radius if the
  live semantic is chosen.

#### T3-R1-3 (P3) — `/api/inspect` `truncated` flag can be `true` while the structural dependents list is actually complete
- **Where:** `src/cli/serve_web.rs:509-513` (`structural_dependents`).
- **What:** the loop checks `if deps.len() >= MAX_INSPECT_NODES` at the **head of
  every iteration, before** the `is_structural` filter and the `seen` dedup. If a
  focus has exactly 64 unique structural dependents and *any* further incident
  edge (a `CoOccurrence`, a duplicate/parallel edge, or a path to an interaction),
  the next iteration breaks with `truncated = true` — but all 64 structural
  dependents were already returned, so the list is complete.
- **Why it matters:** a front-end reading `truncated` will show a spurious
  "more results exist" affordance and hold back a complete list. Low likelihood
  (needs ≥65 incident edges), but it misreports a contract field.
- **Fix:** only bump `truncated` when the *current candidate* is structural,
  unique, and would push the list past the bound — i.e. move the cap check to
  after the `is_structural`/`seen`/`Concept` gates, or use a separate structural
  counter.

#### T3-R1-4 (P3) — the Stage-3 re-promotion cooldown is not surfaced, so a cooling Venerable can show all four gates met yet never promote
- **Where:** `src/canon/gate.rs` (whole payload) vs the cooldown gate absent; the
  cooldown lives in the eval (`canon/stage3.rs` `in_repromotion_cooldown`,
  `last_demotion_time`).
- **What:** Stage 3's promotion predicate is `blast > 5 AND not in cooldown`.
  `GateProgress` reports only the four thresholds — including `blast_radius` —
  and has no cooldown state. A concept just demoted (inside its 300s cooldown)
  can present `gc_survived`+`blast_radius`+`distinct`+`coverage` **all met**, and
  still not become Canonical, with the page unable to explain why.
- **Why it matters:** "why is this not canonical yet" has a fifth answer the
  payload is silent on. Minor because cooldown is transient, but it is the one
  non-threshold reason a promotion stalls and it is fully knowable per concept
  (`last_demotion_time` is on the row).
- **Fix:** optionally add a `cooldown_until` / `in_cooldown` field to
  `GateProgress` (computed from `concept.last_demotion_time` + the cooldown
  config, which serve-web already has), or document that the payload covers only
  the four threshold gates and that cooldown is a separate transient state.

#### T3-R1-5 (P3) — the bounded/`truncated`-reported contract is asserted nowhere for either endpoint
- **Where:** `src/cli/serve_web.rs` tests (new inspect/graph tests only assert
  `truncated == false` on small fixtures).
- **What:** T3 makes "bound and say so rather than cutting silently" an explicit
  requirement, but no test drives either endpoint past its bound —
  `MAX_INSPECT_NODES` (64) for `/api/inspect`, `MAX_GRAPH_NODES`/`MAX_GRAPH_EDGES`
  (4096/16384) for `/api/graph`. A regression that silently truncates (or removes
  the bound) would pass the suite. This is also exactly how T3-R1-3 slipped
  through — a dedicated truncation test would have caught the false-positive
  flag.
- **Why it matters:** the truncation contract is load-bearing for a
  pathological session, and it is currently upheld only by inspection.
- **Fix:** add one test per endpoint that over-seeds the fixture past the bound
  and asserts `truncated == true` with the payload at the cap. (For
  `/api/inspect`, seeding ≥65 structural dependents on one focus; for
  `/api/graph`, an over-cap concept/edge seed.) This also pins the T3-R1-3 fix.

### Nits

#### T3-R1-N1 (nit) — `InspectResponse.truncated` is `Option<bool>` but always `Some` on a hit
- **Where:** `src/cli/serve_web.rs:469` and `:931`.
- **What:** `truncated: Option<bool>` with `skip_serializing_if = Option::is_none`
  is `Some(truncated)` on every hit and `None` only on a miss. The `Option` adds
  nothing (a miss already has `found:false`); a plain `bool` would read cleaner.
- **Fix:** make it `bool` (skip `skip_serializing_if`), or leave the `Option` if
  the miss shape is deliberately kept minimal — cosmetic only.

#### T3-R1-N2 (nit) — `tokens_match` test comment overclaims that the last-byte-differs assertion proves a full scan
- **Where:** `src/cli/serve_web.rs:557-559` (and the enclosing comment `:562-567`).
- **What:** the comment says the last-only case "proves the scan reaches the
  end", but a first-difference short-circuit returns `false` for that input too,
  so that single assertion does not distinguish short-circuit from full scan. The
  genuine proof is the **truncated-prefix refusal** (`"s3cr"` must be `false`, a
  `zip`-style compare would return `true`) — which is correctly asserted.
- **Fix:** reword the comment to credit the prefix refusals (not the last-byte
  case) as the non-short-circuit guard. No test behavior change.

#### T3-R1-N3 (nit) — `#[allow(dead_code)]` on `InspectParams.depth`; the ignored `depth` parameter has no test
- **Where:** `src/cli/serve_web.rs:437-440` (struct) and the new inspect tests.
- **What:** `depth` is deliberately ignored (treated as 1), which is correct per
  the contract, but no test passes a `depth=` value, so the "ignored, not
  rejected" behaviour is unpinned. If axum ever rejected an unknown-typed value,
  nothing would catch it.
- **Fix:** optional — one assertion that `/api/inspect?focus=X&depth=1` (and
  `depth=3`) returns the same hop-1 shape, pinning the ignore-without-reject
  semantics.

## Verified-OK (probed, not defects)

- **Deterministic ordering.** `resolve_focus` sorts by a total order
  (`inspect.rs`); `structural_dependents` iterates id-ascending `incident_edges`;
  `/api/graph` sorts nodes by `(content, status)` and edges by
  `(structural_rank, parent, child)`, so payloads are run-to-run stable.
- **No-writer-lease + route coverage intact.** `the_module_registers_only_get_routes`
  (greps the prod slice for `Memory::builder`/`open_writer`/`acquire_lease`/
  `.spawn()`) passes; the new routes are added to `ROUTES` (`.1290/.1291`) and
  `routes_constant_covers_every_registered_route` confirms completeness; the new
  test code sits after the `#[cfg(all(test…))` split so it never pollutes `prod`.
- **Threshold single-source + operator fidelity.** `pub(super)` consts carry the
  same values; `at_least`/`strictly_above` match the stage modules' `>=`/`>`
  exactly (stage1 `>=`, stage2 `>=` both, stage3 `>`).
- **No lock held across the gate-progress await.** The read guard is scoped out
  (`.885-900`) before `gate_progress` runs, so the `!Send` guard never spans the
  store await.
- **Interaction-node focus is a clean 200 miss**, not an error (UUID leg can
  return an interaction id; `.888` `_ => None` handles it).
- **Token comparator safety** (`presented` empty / shorter / padded / equal) all
  checked by the test; no panic path, no OOB, no early return.

## Summary

All four parts of T3 land faithful to their contracts. **(A)** `/api/inspect`
produces structural-only hop-1 dependents, is bounded, turns every miss kind
(blank, Missing, Ambiguous, interaction-node, Oversized) into a `200
found:false`, and stays lease-free with the no-writer-lease test still green.
**(B)** `/api/graph` ships the structural skeleton with row `status`/`blast_radius`,
exact doc-shaped `nodes`/`edges` fields, deterministic ordering, honest
`truncated`, and no `CoOccurrence` — so the false T7 edge cannot appear.
**(C)** The T11 `gate_progress` payload genuinely surfaces three of the four
measurements through the eval's own queries and single-sourced stage thresholds,
degrades to omitted on store failure while keeping 200, and matches the
per-concept met/not-met design — with one fidelity gap, the blast-radius gate,
which re-computes an age-unfiltered mirror instead of the eval's aged
`store.blast_radius` (T3-R1-1). **(D)** The constant-time comparator correctly
iterates only over the fixed secret length (input length independent), folds
length via XOR, cannot panic, keeps `black_box`, and is pinned by a genuine
non-short-circuit regression test. No P1/P2; five P3s (the blast-gate fidelity
gap, per-endpoint `blast_radius` semantic drift, a `truncated` false-positive,
the un-exposed re-promotion cooldown, and an untested truncation path) plus three
nits. None defeats the task's goal, so this round is **APPROVE**; the P3s are
recommended cleanups (T3-R1-1 deserves promotion to P2 if the maintainers want
strict "surface, don't calculate" fidelity for blast radius).

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": ["T3-R1-1", "T3-R1-2", "T3-R1-3", "T3-R1-4", "T3-R1-5"], "nits": ["T3-R1-N1", "T3-R1-N2", "T3-R1-N3"] }, "summary": "T3 is implemented faithfully across all four parts: /api/inspect gives structural-only hop-1 dependents, 200-on-every-miss, bounded with truncated reported, and lease-free (no-writer-lease test intact, routes registered + covered); /api/graph ships the doc-exact structural skeleton with row status/blast_radius, deterministic ordering, and truthful truncation with no CoOccurrence; the T11 gate_progress payload surfaces gc_survived/distinct/coverage through the eval's own interaction_span query and single-sourced stage thresholds (met/not-met + current-vs-bar, strictly_above on blast only), degrading to omitted on store error while keeping 200; the token comparator iterates only over the fixed secret length (input-length-independent), folds length via XOR without panic, keeps black_box, and is pinned by a real non-short-circuit regression test. No P1/P2; five P3s (blast-radius gate re-computed without the eval's min_edge_age cutoff so met can be optimistic on fresh edges, per-endpoint blast_radius live-vs-frozen semantic drift, a truncated-flag false positive at exactly MAX structural deps, the un-surfaced Stage-3 re-promotion cooldown, and untested truncation bounds on both endpoints) and three nits; all non-blocking, so APPROVE." }
```
