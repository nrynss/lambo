# Adversarial Review: Remediation T3 — Round 2 (worktree `remed-T3`)

```text
╔════════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — Round 2 of the review/remediate loop               ║
║  Scope:  Re-review the FULL current diff of the remediated T3       ║
║          worktree, verify each of the 8 Round-1 remediations        ║
║          (5 P3 + 3 nits) genuinely delivered, re-verify the four    ║
║          original T3 parts and the no-writer-lease test, and scan   ║
║          for regressions from the R1-2 graph blast_radius           ║
║          type/consistency change and the R1-1 gate_progress         ║
║          signature change.                                           ║
║  Branch: remed-T3 (worktree /home/nryn/work/worktrees/remed-T3,     ║
║          detached HEAD @ f158720, 5-file working-tree diff:         ║
║          serve_web.rs +779/-13, canon mod/stage1/stage2/stage3      ║
║          (+pub(super) threshold exposure), new src/canon/gate.rs)   ║
║  Date:   2026-08-17                                                 ║
║  Reviewer: T3ReviewR2 (read-only)                                   ║
║  Verdict: APPROVE — 0 P1 / 0 P2 / 0 P3 / 2 nits.                   ║
║          All 8 Round-1 remediations delivered faithfully and no     ║
║          regression was introduced by the type/signature changes.   ║
║          The remaining nits are documentation/coverage niceties,    ║
║          not defects.                                                ║
╚════════════════════════════════════════════════════════════════════╝
```

## Grounding

Read-only. Read `adve-review-remed-T3round1.md`, the `## T3` + `## T11`
sections of `dev-diary/notes/remediation-tasks.md`, the full working-tree diff
(`git diff HEAD`: `src/canon/{mod,stage1,stage2,stage3}.rs` + new
`src/canon/gate.rs`, `src/cli/serve_web.rs`), and re-read every changed region
in context on disk: `tokens_match`, `InspectResponse`/`InspectDependent`,
`structural_dependents`, `api_inspect`, `api_graph`, `GateProgress`/`gate.rs`,
the new tests, and `blast_radii` / the `GraphStore` trait / `stage3.rs`
(`in_repromotion_cooldown`, `MIN_BLAST_RADIUS`) / `Config` (the two threaded
knobs). Ran targeted builds/tests (Main owns full-suite/formatter/clippy):

- `cargo check --all-features` — **clean, no warnings.**
- `cli::serve_web::tests` — **30 passed / 0 failed**, including the four Round-1
  tests and **five new** (`graph_and_inspect_agree_on_a_live_blast_radius_for_non_canonical`,
  `inspect_truncates_and_reports_at_the_dependents_bound`,
  `graph_truncates_and_reports_at_the_nodes_bound`,
  `inspect_ignores_the_depth_parameter`,
  `inspect_surfaces_a_cooling_concepts_repromotion_cooldown`), and — critically —
  `the_module_registers_only_get_routes` (the **no-writer-lease** source-grep
  test), `read_only_router_has_no_mutating_route`, and
  `routes_constant_covers_every_registered_route` all still pass.
- `canon::*` — **59 passed / 0 failed / 1 ignored** (stage1/2/3 + eval + task;
  the `pub(super)` threshold exposure changed nothing; `gate.rs` compiles and its
  mirror logic is exercised via the serve_web cooldown test).

---

## Verify each of the 8 Round-1 remediations

### 1. T3-R1-1 (P3) — blast-radius gate now surfaces the eval's *aged* number — **DELIVERED**
`gate_progress` (`src/canon/gate.rs:131-133`) now calls
`store.blast_radius(session, concept.id, min_edge_age, now)` directly — the same
aged query Stage 3 runs (adds `e.created_at <= now - min_edge_age`), not an
age-unfiltered `format::blast_radii` mirror. `serve_web.rs:914` threads
`config.canonization_edge_min_age` through (same value the eval passes to Stage 3,
`eval.rs:205-206`). The **operator is still strictly `>`** (`GateMetric::strictly_above`,
`gate.rs:68`, `met: current > bar`), matching Stage 3. The caller-supplied
in-memory blast arg **is genuinely gone** — the new signature
`(store, session, concept, min_edge_age, cooldown, now)` has no blast parameter,
and the only remaining `blast_radii` in `api_inspect` (`serve_web.rs:889`) feeds
the *top-level* `blast_radius`, not the gate. **No dead code** — `cargo check` is
warning-clean. The aged store value matches Stage 3 semantics; the gate can no
longer read `met:true` on fresh (<60s) edges the engine would not yet promote.

### 2. T3-R1-2 (P3) — `/api/graph` blast_radius: i32 row-column → u64 live, agreeing with `/api/inspect` — **DELIVERED, no regression**
- **Type change sound.** `GraphNode.blast_radius` is now `u64` (`serve_web.rs:556`),
  fed by `radii = blast_radii(&g)` (`:961`) and `radii.get(&c.id).copied().unwrap_or(0)`
  (`:968`) — the same helper `/api/inspect` uses at `:889`. `/api/inspect`'s
  top-level `blast_radius` was already `u64` from `blast_radii`, so the two now
  share one provenance. No persisted `i32` narrowing anywhere on this path
  (CON-6 honored; `u64` was never narrowed to `i32`).
- **Both endpoints use the identical helper — no drift.** Grep confirms exactly
  two production `blast_radii` call sites (`:889`, `:961`), both live-dependent-count.
  The new test `graph_and_inspect_agree_on_a_live_blast_radius_for_non_canonical`
  drives a status-`None` node (`create user`) that stands behind a dependent and
  asserts **both endpoints report the same nonzero radius** (`:2119-2131`).
- **JSON shape still matches the contract.** `blast_radius` is a JSON number
  (`u64` serializes numeric); the graph test checks `as_i64`/the agree test checks
  `as_u64`. Numeric shape preserved.
- **Candidates/Venerables now show nonzero radius.** The new test asserts a
  non-canonical node reports `>= 1` (the frozen `Option<i32>` column, `None` until
  promotion, previously rendered `0`). The tree-intent from the task ("mark
  load-bearing nodes without a second call") is now actually met.
- **Per-request cost is bounded and acceptable.** `blast_radii` is one O(E) pass
  (`inbound_sources` → increment per unique structural inbound source, `format.rs:119-133`).
  `/api/graph` already materializes every node and edge (O(N+E)) to build the
  payload, so the extra O(E) map is the same order and runs once per request (a
  tree view loads it once on paint, not per frame). No unbounded/pathological
  behavior. Not a finding.

### 3. T3-R1-3 (P3) — `structural_dependents` truncation flag false-positive — **DELIVERED**
The cap check moved to **after** the `is_structural` filter (`:511`), `seen` dedup
(`:519`), and `Node::Concept` gate (`:525`): `if deps.len() >= MAX_INSPECT_NODES { truncated = true; break; }` now runs at `:528`, so `truncated` fires **only** when a structural, unique Concept neighbour would push the list past the bound. A `CoOccurrence`/duplicate/interaction incident edge can no longer trip it. Truncation still genuinely triggers at >64 structural deps (verified by the new inspect cap test, below).

### 4. T3-R1-4 (P3) — re-promotion cooldown surfaced; mirror correct — **DELIVERED**
`GateProgress` gains `in_cooldown: bool` + `cooldown_until: Option<DateTime<Utc>>`
(`gate.rs:91-94`; `cooldown_until` is `skip_serializing_if`). The mirror is
**exact**: `in_cooldown` (`gate.rs:170-185`) matches `stage3::in_repromotion_cooldown`
(`stage3.rs:87-102`) — `None` is not a cooldown, `now < last_demotion + cooldown`,
and an unrepresentable cooldown conservatively stays cooling. Both derive from
`concept.last_demotion_time` + `config.canonization_repromotion_cooldown`
(`serve_web.rs:915`). `cooldown_until` = `last_demotion + cooldown`
(`gate.rs:157-164`); the payload records it only while `in_cooldown`
(`:148`). `met_count` still counts exactly the **four** gates — cooldown is
deliberately excluded (`gate.rs:101-111`). Fields are **additive** (a hit gains
two keys; a miss omits the whole `gate_progress`). Test
`inspect_surfaces_a_cooling_concepts_repromotion_cooldown` seeds a concept
demoted 5s ago and asserts `in_cooldown:true` + a present `cooldown_until`.
**No transient-state leak:** `gate_progress` (with cooldown) is only serialized on
a hit; on a miss it is `None`. The truthfulness is per-request current state, not
a misleading cached value.

### 5. T3-R1-5 (P3) — over-cap truncation tests per endpoint — **DELIVERED, genuinely over-seeded, not vacuous**
- `inspect_truncates_and_reports_at_the_dependents_bound` seeds
  `MAX_INSPECT_NODES + 1 = 65` dependent concepts, **each** wired to the focus via a
  `Dependency` structural edge (`seed_chain_around` `:1609-1618`), so the focus has
  65 structural unique Concept dependents. It asserts `truncated == true` **and**
  `dependents.len() == MAX_INSPECT_NODES`. A regression that removed the bound (or
  that re-broke R1-3's pre-gate check) would fail. Passes.
- `graph_truncates_and_reports_at_the_nodes_bound` seeds
  `MAX_GRAPH_NODES + 1 = 4097` concepts (`seed_many_concepts`, no canonization), so
  `nodes.len() == 4097 > 4096`; asserts `truncated == true` and
  `nodes.len() == MAX_GRAPH_NODES`. Genuinely over-cap. Passes.

### 6. T3-R1-N1 (nit) — `truncated` is now a plain `bool` — **DELIVERED**
`InspectResponse.truncated: bool` (`serve_web.rs:469`, no `Option`, no
`skip_serializing_if`); `InspectResponse::missing` sets `truncated: false`
(`:485`). Miss shape still matches the contract: `{focus, found:false,
blast_radius:0, dependents:[]}` plus the additive `truncated:false` (the Round-1
note explicitly wanted a plain bool present on both hit and miss). The miss test
(`inspect_endpoint_miss_is_a_200_with_found_false`) passes.

### 7. T3-R1-N2 (nit) — `tokens_match` test comment — **DELIVERED, no behavior change**
The comment now correctly credits the **truncated-prefix refusals**
(`tokens_match(b"s3cr", token)` must be `false`, `:2424`, annotated `:2411-2415`)
as the genuine non-short-circuit guard, and explicitly walks back the old claim
that the last-byte case alone proves a full scan (`:2404-2406`). No assertion
changed; the test still passes.

### 8. T3-R1-N3 (nit) — `depth=1`/`depth=3` same-shape test — **DELIVERED**
`inspect_ignores_the_depth_parameter` (`:2177-2190`) issues
`/api/inspect?focus=user%20schema&depth=1` and `depth=3` and asserts identical
`found` and identical `dependents` arrays — pinning ignore-not-reject. Passes.

---

## Regression scan on the two risky changes

### R1-2 graph/i32→u64 + R1-1 gate_progress signature change
- **No existing caller broke.** `gate_progress`'s only caller is `api_inspect`
  (`:910`), updated to the new signature; the store trait methods it uses are
  unchanged and match the trait (`store/mod.rs:182,193`). `GraphNode.blast_radius`
  is consumed only by serialization; no other reader of that field exists. Cargo
  check is clean.
- **`/api/graph` contract shape intact** — keys `{content, concept_type, status,
  blast_radius}` with numeric `blast_radius`. Verified by the graph skeleton test.
- **`/api/inspect`'s top-level `blast_radius` semantics unchanged** — still the live
  dependent count (it was already `u64` from `blast_radii` in Round 1).
- **Is `gate_progress.blast_radius` (aged) vs the top-level `blast_radius` (live)
  a new drift? No — it is the correct, deliberate resolution of Round-1's two
  findings.** Round 1 objected to two *redundant* inconsistencies: (a) inspect-top
  vs graph both claimed "blast_radius" with different provenance, and (b) the gate
  metric didn't surface the engine's aged number. The remediation makes each
  *pair* consistent: graph and inspect-top now share one live provenance
  (`blast_radii`), and the gate metric now surfaces the engine's own aged
  `store.blast_radius`. The two keys answer different questions — top-level = "how
  load-bearing is this node right now" (the /api/inspect contract's dependent
  count), `gate_progress.blast_radius` = "does it clear the Stage-3 bar" (the
  eval's evidence). They can transiently differ (edges younger than
  `canonization_edge_min_age` counted live but not aged) — an intended,
  documented distinction in the code comments (`gate.rs:11-14`,
  `serve_web.rs:902-909`). This is a judgement call the maintainers asked for: it
  is **acceptable**, not a defect; see nit T3-R2-N1 to make it explicit in the
  written contract.
- **Cooldown surfacing (R1-4) is correct** and does not leak transient state
  incorrectly — it reflects per-request current truth, is additive, and is omitted
  on a miss. The mirror to `stage3::in_repromotion_cooldown` is exact.

### The four original T3 parts still hold
- **Part A (`/api/inspect`)** — structural-only dependents, 200 on every miss kind,
  read-only/no-writer-lease, depth ignored as 1, bounded with honest `truncated`,
  status strings — all re-verified unchanged and tested.
- **Part B (`/api/graph`)** — structural-only skeleton, row status, bounded,
  deterministic ordering, read-only, renderer contract shape — unchanged except the
  intended `blast_radius` live change.
- **Part C (T11 gate-progress)** — all four metrics now genuinely *surfaced* through
  the eval's own queries and the `pub(super)` single-sourced stage thresholds
  (`MIN_GC_SURVIVED`, `MIN_DISTINCT`, `MIN_COVERAGE`, `MIN_BLAST_RADIUS` — values
  unchanged at 3/3/0.3/5), operators `>=`/`>` matching each stage, degrading to
  `null` on store failure with 200. Now includes cooldown.
- **Part D (constant-time comparator)** — loop count fixed by secret length, no
  panic/OOB, length folded via XOR, `black_box` retained; truncated-prefix refusal
  pinned.
- **No-writer-lease test** — `the_module_registers_only_get_routes` still passes,
  and both new routes remain GET-only and reader-path (`load_reader_graph_with_contract`).

## Verified-OK (probed, not defects)

- **Per-request cost of `/api/graph`'s `blast_radii`** — one O(E) in-memory pass,
  same order as the payload build itself; bounded, no per-frame churn. Fine.
- **Threshold single-source** — `gate.rs` imports the `pub(super)` stage consts
  (`:34-36`); no restated literals in the web layer. `cargo check` clean.
- **Gate old-age mirror correctness** — `in_cooldown` matches `in_repromotion_cooldown`
  for all reachable inputs (the unrepresentable-cooldown divergence only triggers
  for `cooldown` > ~5.8×10¹¹ years, an impossible config).
- **`gate_progress` ages only via the store's own queries** — the read guard is
  scoped out before the await (`serve_web.rs:884-899`), so the `!Send` guard never
  spans the store query (unchanged from Round 1).
- **Edge-bound (16 384) truncation for `/api/graph`** is code-symmetric with the
  (tested) node bound but not itself exercised — see nit T3-R2-N2.

## Findings

No P1, no P2, no P3. All eight Round-1 findings are genuinely remediated and
independently re-verified against the code, the store trait, and `stage3.rs`.

### Nits

#### T3-R2-N1 (nit) — the two `blast_radius` keys in one `/api/inspect` hit have different provenance (live vs aged); worth one sentence in the written contract
- **Where:** `src/cli/serve_web.rs:889` (top-level `blast_radius` = live
  `blast_radii(&g)`) vs `src/canon/gate.rs:131-133` (`gate_progress.blast_radius` =
  aged `store.blast_radius`).
- **What:** Grossly, both are "blast radius", and on a concept with edges younger
  than `canonization_edge_min_age` they differ (live counts them, aged does not).
  This is the intended resolution of R1-1/R1-2 and is documented in code comments,
  but `remediation-tasks.md` (T3/T11) — the wire contract a front-end is built
  against — does not say the two answer different questions.
- **Why it matters:** a consumer comparing `gate_progress.blast_radius.current`
  against the top-level `blast_radius` could read a transient mismatch as a bug.
- **Fix:** one doc sentence in the T3/T11 contract: top-level `blast_radius` is the
  live dependent count (tree marking); `gate_progress.blast_radius` is the engine's
  aged gate evidence with the `min_edge_age` cutoff. Cosmetic.

#### T3-R2-N2 (nit) — `/api/graph`'s edge bound (16 384) truncation is untested
- **Where:** `src/cli/serve_web.rs:997` (`edges_trunc = raw.len() > MAX_GRAPH_EDGES`)
  and `:1000` (`.take(MAX_GRAPH_EDGES)`).
- **What:** the Round-1 R1-5 truncation requirement is now covered for the
  inspect-nodes bound and the graph-**node** bound, but the graph **edge** bound —
  the branch that would catch a regression silently dropping `MAX_GRAPH_EDGES` —
  has no over-seed test. The logic is trivially symmetric with the tested node
  bound, so this is low risk.
- **Fix:** optional — seed >16 384 structural edges and assert `truncated` with
  `edges.len() == MAX_GRAPH_EDGES` (heavier fixture; likely why it was omitted).
  Or accept the symmetry argument. Cosmetic.

## Summary

Round 2 of the T3 remediation is **clean**. All five P3s and all three nits from
Round 1 are genuinely delivered and independently re-verified: the blast-radius
gate now surfaces the engine's own aged `store.blast_radius` (strictly `>`, no
dead in-memory blast arg); `/api/graph` and `/api/inspect` now share one live
`blast_radii` provenance with a sound `u64` field, sound JSON shape, and a
regression test proving the two endpoints agree on a non-canonical node's nonzero
radius; the `truncated` flag only fires on a genuine structural unique Concept
past the bound; the re-promotion cooldown is surfaced with an exact mirror of
`stage3::in_repromotion_cooldown` and correctly excluded from `met_count`; and
both endpoints now have genuinely over-seeded (non-vacuous) over-cap truncation
tests. The `truncated`→`bool` change, the reworded `tokens_match` comment, and
the depth-ignored test are all in place. The four original T3 parts still hold and
the no-writer-lease test, the read-only/mutating-route sweep, and the route
coverage test all still pass (30 serve_web + 59 canon tests green; `cargo check`
clean). The one *judgement* the task asked for — inspect-top live vs gate-aged
blast radius — is a deliberate, correct resolution of Round-1's redundancy, not a
new drift. The two remaining nits are documentation and a low-value extra
truncation test, not defects. **APPROVE.**

```json
{ "verdict": "APPROVE", "findings": { "P1": [], "P2": [], "P3": [], "nits": ["T3-R2-N1", "T3-R2-N2"] }, "summary": "All 8 Round-1 remediations (T3-R1-1..5, N1..N3) genuinely delivered and independently re-verified against the code, the GraphStore trait and stage3.rs. The R1-2 graph blast_radius i32->u64 live change and the R1-1 gate_progress signature change break no caller, keep the JSON shape numeric, and make /api/graph and /api/inspect agree via the shared blast_radii helper (new agree-on-non-canonical test); blast_radii is one bounded O(E) pass so the per-request /api/graph recompute is fine. The inspect-top live vs gate_progress aged blast_radius split is the correct, deliberate resolution of Round 1 (top-level = load-bearing dependent count, gate = engine's aged evidence) and is documented in code; nit to add one sentence to the written contract. structural_dependents truncation now fires only on a genuine structural unique Concept past the bound; in_cooldown/cooldown_until mirror stage3::in_repromotion_cooldown exactly, are additive, and met_count still counts the 4 gates; truncated is a plain bool; tokens_match comment reworded; depth-ignored test added. Both over-cap truncation tests genuinely over-seed and are non-vacuous. Four original T3 parts hold; no-writer-lease, read-only sweep and route-coverage tests pass. 30 serve_web + 59 canon tests green, cargo check clean. No P1/P2/P3; 2 nits (contract doc for the two blast_radius keys, optional graph-edge-bound truncation test). APPROVE." }
```
