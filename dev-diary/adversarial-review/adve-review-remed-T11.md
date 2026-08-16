# Adversarial Review: Remediation T11 — satisfied-by-T3 verification

**Task:** Verdict on whether T11 ("Surface *why* a concept is not canonical yet") is already
fully delivered by the T3 work merged at `5a6f633` into `main`, with no separate code change.
**Mode:** READ-ONLY against the merged `main` tree (`98c4e8c`). No edits.
**Verification method:** re-read `src/canon/gate.rs`, `src/cli/serve_web.rs` (`/api/inspect`
handler + wiring + tests), the `MIN_*` constants in `src/canon/stage{1,2,3}.rs`, and the T3
review docs (`adve-review-remed-T3round{1..3}.md`); ran the `/api/inspect` test suite.

```text
╔══════════════════════════════════════════════════════════════════════╗
║  VERDICT: T11-SATISFIED-BY-T3                                        ║
║  No T11-specific code change is needed — T3 delivers the T11         ║
║  gate-progress payload complete in /api/inspect.                     ║
╚══════════════════════════════════════════════════════════════════════╝
```

## T11 restated (the bar it must meet)

"Surface why a concept is not canonical yet": report per concept which canonization gates are
met/not with current value vs the bar, folded into T3's `/api/inspect` response (which already
carries `status` and `blast_radius` from the same query path).

## Requirement-by-requirement evidence

### 1. `/api/inspect` carries a `gate_progress` payload per hit — MET

- `GateMetric { current, bar, met, strictly_above }` — `src/canon/gate.rs:42-52`.
- `GateProgress { gc_survived, blast_radius, distinct_interactions, coverage, in_cooldown,
  cooldown_until }` — `src/canon/gate.rs:79-95`; `cooldown_until` is `skip_serializing_if = none`
  (present only while cooling). `met_count()` at `gate.rs:98-111` excludes the cooldown (a
  non-threshold reason).
- The four metrics are serialized as individual `GateMetric`s (`gate.rs:142-149`), each carrying
  `current`/`bar`/`met`; `strictly_above` is `true` only for blast radius (`gate.rs:64-71`,
  Stage 3's `>` semantics).
- Wired into `InspectResponse.gate_progress: Option<GateProgress>` — `src/cli/serve_web.rs:472`,
  populated in `api_inspect` (`serve_web.rs:910-934`).
- **Evidenced by test** `inspect_endpoint_reports_a_focus_and_its_structural_dependents`
  (`serve_web.rs:2041`) asserting the bars (`gc_survived==3`, `blast_radius==5`,
  `strictly_above==true`, `distinct_interactions==3`, `coverage` f64) at `serve_web.rs:2063-2069`,
  and `inspect_surfaces_a_cooling_concepts_repromotion_cooldown` (`serve_web.rs:2270`) asserting
  `in_cooldown:true` + a present `cooldown_until` at `serve_web.rs:2302-2307`.

### 2. Surfacing, not calculating — MET

Each number comes from the evaluation's own source, not a web-layer re-computation:

- **`gc_survived`** — read straight from the persisted `concept.gc_survived` field the GC bumps:
  `gate.rs:143` (`concept.gc_survived as f64`).
- **`blast_radius`** — `store.blast_radius(session, concept.id, min_edge_age, now)` — the exact
  aged query Stage 3 runs, with the eval's `min_edge_age` threaded from
  `config.canonization_edge_min_age` at `serve_web.rs:914`:
  `gate.rs:131-133`. (T3-R1-1 remediated this from the earlier age-unfiltered
  `format::blast_radii` mirror; `adve-review-remed-T3round2.md:57-60`.)
- **`distinct_interactions` / `coverage`** — `store.interaction_span(session, concept.id,
  min_edge_age, now)` — the exact query Stage 2 runs: `gate.rs:134-136`,
  `gate.rs:145-146` (`span.distinct`, `span.coverage`).
- **`in_cooldown` / `cooldown_until`** — mirror Stage 3's `in_repromotion_cooldown` from
  `concept.last_demotion_time` + the eval's `config.canonization_repromotion_cooldown`
  (`serve_web.rs:915`): `gate.rs:137-141`, `gate.rs:157-185`.
- **Bars single-sourced** from the stage modules' own `MIN_*` constants — imported at
  `gate.rs:34-36`, never restated:
  - `MIN_GC_SURVIVED = 3` — `stage1.rs:51`
  - `MIN_DISTINCT = 3` — `stage2.rs:35`
  - `MIN_COVERAGE = 0.3` — `stage2.rs:37`
  - `MIN_BLAST_RADIUS = 5` (u64, `>` comparison) — `stage3.rs:57`
- Comparison operators match each stage: `>=` via `at_least` for GC/distinct/coverage, `>`
  via `strictly_above` for blast radius (`gate.rs:55-71`, decided by `stage3_passes` `>`
  at `stage3.rs:19-22`).

### 3. Folded into the same query path as `status`/`blast_radius` — MET

- `gate_progress` is computed in the same `api_inspect` handler that already produces `status`
  and `blast_radius`, and is returned additively in the same `InspectResponse` struct alongside
  them: `serve_web.rs:926-934` (`status`, `blast_radius`, `dependents`, `truncated`,
  `gate_progress`).
- It is `Option` + `skip_serializing_if = is_none` and additive (`serve_web.rs:472`): a hit
  gains the field; a miss omits it (`serve_web.rs:470-487`, tested at `serve_web.rs:2091-2093`).
- A store read failure degrades the payload to `null` rather than failing the endpoint
  (`serve_web.rs:921-924`).
- Bars match the stage modules exactly: 3 / 3 / 0.3 / 5.

### 4. An inspect test exercises it — MET

- End-to-end `/api/inspect` tests exercise `gate_progress` (bars on a Canonical focus;
  `in_cooldown`/`cooldown_until` on a cooling Venerable), and they **pass**:
  `cargo test --lib inspect_` → **9 passed, 0 failed** (incl. both T11-relevant tests).
- `gate_progress` is reachable for any `Concept` regardless of status: the handler matches
  `Node::Concept(c)` and computes gate progress for it (`serve_web.rs:887-891`,
  `serve_web.rs:910-925`) — status-agnostic by construction.

### 5. Residual T11 gap — NONE (code)

No component of T11's charter is left uncovered by T3:

- **T3-R2-N1** (from `adve-review-remed-T3round2.md:226-239`) — the *wire-contract doc sentence*
  clarifying that top-level `blast_radius` is the live dependent count while
  `gate_progress.blast_radius` is the aged gate evidence — was explicitly recorded as a
  **docs-only cosmetic nit, deferred to Main's merge edit in `remediation-tasks.md`**, and is
  already reflected in code comments (`gate.rs:11-14`, `serve_web.rs:889`). It is not a source
  gap and requires no T11 code follow-up.

**Minor observation (not a gap):** the inspect tests assert the `bar`/`strictly_above` and
`in_cooldown` shape but do not directly assert `current`/`met` *values* on a specific
Candidate-status concept, and `gate.rs` itself has no dedicated unit-test module. Reachability
and functioning are verified (status-agnostic handler + passing end-to-end tests), so this is a
test-thoroughness nicety only — the T11 payload surface itself is complete and correct.

## Conclusion

**T11-SATISFIED-BY-T3.** The four canonization gates are surfaced per concept in `/api/inspect`
with `current`/`bar`/`met` (+ `strictly_above` on blast radius) plus the cooldown, all numbers
from the evaluation's own queries/persisted fields, all bars single-sourced from the stage
module `MIN_*` constants (3 / 3 / 0.3 / 5), folded additively into the same response as
`status`/`blast_radius`. No separate T11 code change is required.
