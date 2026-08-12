# Adversarial Review: P4 — Daemon (branch `phase/p4-daemon`) — opus

```text
╔════════════════════════════════════════════════════════════════╗
║  STATUS: OPEN — merge to main BLOCKED pending P1 remediation   ║
║  Scope:  P4 daemon tier (T4.1–T4.6): scoring, hot list,        ║
║          conflict, drift, GC, event channel                    ║
║  Source: phase/p4-daemon @ cd9340e (diff vs main: 11 files,    ║
║          5,802 insertions)                                     ║
║  Date:   2026-08-12                                            ║
║  Reviewer: opus (claude-opus-5) — three parallel reviewers     ║
║          (concurrency/runtime, spec-§9 algorithms, cross-phase ║
║          contracts + test honesty); orchestrated by fable,     ║
║          every P1 independently re-verified against source     ║
║  Verdict: 25 findings (8 P1 / 11 P2 / 6 P3; 30 raw, overlaps  ║
║          merged). The tier's plumbing is sound — mutation      ║
║          parity, cancellation safety, epoch semantics, spec    ║
║          constants all verified clean — but the P1s sit on     ║
║          the demo path (GC eats the demo fixture, events can   ║
║          drop the Conflict, revalidation lies about time, P6's ║
║          seam is unreachable) and on §6.4 (lock-hold budget).  ║
║          Fix P1s before phase→main.                            ║
╚════════════════════════════════════════════════════════════════╝
```

## Grounding

Reviewed in a dedicated worktree at `cd9340e`. Read in full: all seven
`src/daemon/*` modules (prod + tests), the `graph.rs` diff (+307),
`types::Mutation` + both store appliers, `config.rs`, PHASE-4/5/6 docs, spec
§5/§6.1/§6.4/§9/§10, flush.rs as precedent. Three reviewers, one per
dimension; the orchestrator re-verified every P1 mechanism directly (lock
sites, closure captures, sender visibility, GC threshold + dead frequency
term, CI trigger behavior via `gh run list`).

## Gates (orchestrator runs, worktree @ cd9340e)

| Gate | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo test` (default) | **328 pass, 0 fail, 3 ignored** (323 lib; +81 vs main) |
| `cargo test --features store-sqlite` | **359 pass, 0 fail** (354 lib) |
| `gh run list --branch phase/p4-daemon` | **empty — CI has never run on this branch** (XP-1) |

## Verified clean (adversarially checked, no finding)

- **§6.4 lock/await discipline:** zero `.await` inside any guard in
  production daemon code; the only suspension points are the `select!` and
  the spawned `run_loop`. Full lock-site inventory in the concurrency
  reviewer's report — every guard scope accounted for.
- **No deadlock surface:** daemon and FlushTask share only the graph lock;
  ordering is uniformly graph → hot; `holds` closures capture only `Copy`
  scalars (re-entrant locking structurally impossible).
- **Cancellation safety:** all mutations are synchronous between awaits;
  `handle.abort()` cannot half-apply GC.
- **Epoch coherence:** GC's mutations and epoch bumps happen under one write
  guard; `last_gc_epoch` rebases to `epoch_after` so GC's own mutations
  cannot re-trigger it (no runaway).
- **Mutation-emission parity (the write-behind contract):** every graph
  mutation GC makes emits — `DeleteEdge`, cascaded `DeleteNode`,
  `UpsertNode` per gc_survived bump, `CanonizationTransition` for
  auto-Venerable — and both MemoryStore and SqliteStore apply them. Store
  divergence from the daemon: **nil**. The one non-emitting write is
  `root_goal` itself (pre-existing P2 gap, see XP-8).
- **Scoring math:** spec formula verbatim; per-dimension clamp to [0,1]
  before weighting; NaN/Inf dimensions → 0.0; bonuses capped; 8⁴-input
  bounds sweep. Division-by-zero guarded in all three normalizers.
- **Spec constants:** 30s / 5 hops / 1000 / 10,000 / 1000 / weights
  0.25/0.20/0.20/0.35 all match; no quiet drift. Unspecified v0.1 constants
  documented at their definitions.
- **Wake seam matches COH-5's corrected doc:** `select!` on tick +
  `Notify`; no phantom mutation-notify channel.
- **Test honesty (pass-direction):** no assert-nothing loops, no
  skip-as-green, no Debug-string assertions, `wait_until` panics on timeout.
  (Fail-direction weaknesses are XP-6.)

## Findings summary

| ID | Sev | Area | One line |
|---|---|---|---|
| ALGO-1 | P1 | GC | `MIN_CONCEPT_SCORE=0.3` mass-collects a healthy session (15/22 demo-fixture concepts on first sweep; frequency term is identically 0 until P5) — starves canonization Stage 1 below its 20-peer minimum |
| CONC-1 | P1 | locks | Guards held 186–272ms per cycle @ 4k concepts (release) — `incident_edges` is a full O(E log E) scan though the adjacency index exists; recall stalls behind writer-preferring queue |
| CONC-2 (+ALGO-8) | P1 | events | Emit-on-transition + 256-slot ring + per-concept Stale storms (4,000 in one burst on warm-up) = a lagged-away Conflict is lost **permanently** (never re-emitted while it holds) |
| XP-1 | P1 | CI | `phase-*` filter never matches `phase/p4-daemon` — all 5,802 lines merged with zero CI validation; also voids the TEST-3 remediation for every phase branch |
| XP-2 | P1 | process | Zero T4.x review records committed despite 8 claimed review events (per-task ACCEPTs, remediation rounds, "three loop redesigns") — unauditable; index claim now false |
| XP-3 | P1 | P5 seam | `revalidate` predicates freeze `now` at detection — recency conditions re-validate true **forever** against an unchanged graph; the demo sentence ("wrote 11s ago") becomes a lie served by the exact API T5.3 is told to call |
| XP-4 (+ALGO-7) | P1 | P6 seam | `emit_canonized` needs the broadcast Sender; `Daemon` exposes only a Receiver — P6's documented seam is unreachable without violating P4 ownership |
| XP-11 | P1 | CI matrix | Ungated `use crate::fixtures;` in daemon test modules (conflict.rs:212, hotlist.rs:606) breaks both `--no-default-features` matrix rows (E0432 ×2) — a P4 regression of the TEST-2/CON-4 surface, invisible only because of XP-1 |
| CONC-3 | P2 | events | `subscribe()` after `spawn()` misses the warm-up cycle's entire condition set (incl. the planted demo Conflict); every loop test subscribes before spawn, so the suite can't see it |
| CONC-4 | P2 | runtime | No panic containment (flush.rs precedent not adopted): one panic silently kills scoring/GC/events for the process lifetime; config-reachable `expect()` on `ChronoDuration::from_std` |
| CONC-5 | P2 | P5 seam | Each `revalidate` predicate re-runs whole-graph detection (~0.9ms–79ms release) — recall force-including 10 hot nodes adds ~0.8s of graph-lock hold |
| XP-5 | P2 | GC | `GcOutcome` is dropped except `epoch_after`: canonical-budget signal promised to T6.4 is unreachable, `max_concept_nodes` warning unobservable (daemon has zero logging), `sync_index` uncallable by construction |
| XP-6 (+CONC-7) | P2 | tests | Zero daemon tests use `start_paused`; negative assertions are wall-clock sleeps that pass vacuously when a cycle overruns; 27–30s wall budgets on conflict fixtures; never run on CI (XP-1) |
| XP-7 | P2 | config | The daemon tick has no default, no const, no `Config` field — the one parameter P8 cannot derive governs stale latency, GC latency, and XP-3's ghost window; `CycleParams::default()` duplicates spec constants as literals |
| ALGO-2 | P2 | conflict | Payload records when the newest write happened but not **who** — the §13 attribution sentence is underivable and provably wrong on the shipped fixture (newest write is agent-b's; naive rendering blames agent-a) |
| ALGO-3 | P2 | conflict | `writer_of` attributes edges to the source concept's `origin_agent`; canonical concept reuse (`record_action` resolve) mis-credits cross-agent writes — silent false negatives on the demo trigger |
| ALGO-4 | P2 | GC | GC rescoring hardcodes `ScoringWeights::default()` — eviction and recall ranking can use two different score functions; `GcParams` can't express the session's weights |
| ALGO-5 (+XP-9) | P2 | drift | Spec says warn on ">5 hops **or no path**"; no-path concepts are structurally unreported (filter over reachable set only) — the maximally drifted case never warns; phase doc also still says "weighted" for an unweighted BFS |
| ALGO-6 | P2 | drift/GC | Root-goal matching accepts only a bare JSON string; spec §6.1's own example is a list — array goals silently disable drift, auto-Venerable, and GC's root-goal exclusion (T1.4 carried the shape decision to P4; P4 froze it without recording) |
| CONC-6 (+XP-10) | P3 | GC | One `UpsertNode{Concept}` clone per survivor per run inside the write guard (up to 10k mutations / 20 flush batches per sweep; ~40MB clone traffic post-P7 embeddings) |
| XP-8 | P3 | durability | `root_goal` has no Mutation kind — after crash+reload, drift is silently disabled and GC's goal exclusion empties (auto-Venerable survives as the only fallback); goal changes don't bump the epoch (stale T5.4 cache) |
| ALGO-9 | P3 | GC | Step-1 cleanup ignores the §5 decay table (all seven edge types eligible); a collected `Derives` edge on a protected concept violates §5.7 with zero weight-margin today (structural writes sit exactly at the 0.5 threshold, strict `<`) |
| ALGO-10 | P3 | scoring | Non-finite `ScoringWeights` (public f64s, TOML-admissible) produce NaN scores — garbage ranking and GC's `NaN < 0.3 == false` silently disables collection; §5.7 requires finite composites |
| ALGO-11 | P3 | GC | `ConceptType::eviction_resistance` has zero call sites — Constraints (1.5) and Observations (0.7) face the identical flat cut; spec §5 retains the resistances |
| ALGO-12 | P3 | canon path | `set_root_goal` promotes the *first* HashMap match (nondeterministic under multi-match) and stamps `occurred_at` with `Utc::now()` in an otherwise logical-time write path — non-monotonic audit trail on the on-camera table |

## P1 findings — detail

### ALGO-1 — GC's flat score threshold eats the demo session

- **Location:** gc.rs:78 (`MIN_CONCEPT_SCORE = 0.3`), gc.rs:163–185 (step 2), mod.rs:386–393
- **Mechanism (all verified):** frequency ≡ 0 for every concept today —
  every write path creates `access_count: 0` and nothing increments it until
  P5 recall lands; `session_activity → 1/I`; density is max-normalized
  against the session hub. An ordinary 2-edge concept scores
  ≈ 0.133 + 0.25·recency, so it clears 0.3 only while its recency rank is in
  the top third of the session timeline — and recency is relative to the
  *current* extent, so growth pushes every earlier concept back under the
  bar. Rolling deletion of everything but hubs and protected nodes.
- **Measured on the demo fixture** (score replication cross-checked against
  the in-repo ranking test): 15 of 22 `session-rest-api` concepts below 0.3
  and unprotected — including `auth middleware` (0.20), named in spec §13
  step 1. After one sweep the session has 6 non-Canonical peers; Stage 1
  requires ≥ 20, so **GC permanently starves the canonization pipeline it
  exists to feed.**
- **Why the suite is blind:** gc.rs tests load only `session-drift`
  (9 concepts, all scoring 0.57–0.71). No test runs `gc::run` against
  `session-rest-api`.
- **Disposition needed:** recalibrate (threshold ∝ observed distribution, or
  gate the frequency term out of the cut while `access_count` is dead, or
  percentile-based cut), honor `eviction_resistance` (ALGO-11), take the
  session's weights (ALGO-4), and add the missing fixture test:
  `gc::run(session-rest-api)` asserting the demo's named concepts survive.

### CONC-1 — critical sections are not short (§6.4, second clause)

- **Location:** mod.rs:326–331 / 335–350 / 384–394; root cause graph.rs:849
- **Verified:** `incident_edges` is a full `edges.values().filter(...)` +
  sort per call while the adjacency index exists (and `remove_node` already
  uses it, citing the T2.1 review). Every detector and `rescore` call it per
  concept; GC runs a second full rescore inside the write guard.
- **Measured (release, 4k concepts / 8.5k edges):** rescore read-guard
  186ms; detection guard 158ms (holding `hot.write()` throughout); GC write
  guard 272ms. parking_lot is writer-preferring: one queued writer parks
  every subsequent reader, so a single `record_interaction` during the
  daemon's guard stalls all `recall()` for the rest of the hold.
- **Disposition:** route `incident_edges` through the adjacency index
  (O(degree)); hoist the second rescore out of the GC write guard; consider
  scoring against a snapshot rather than under the graph lock.

### CONC-2 (+ ALGO-8) — the event channel can permanently lose the demo's Conflict

- **Location:** mod.rs:355–380 (burst + `prev_conditions = fresh`),
  events.rs:69 (`EVENT_CAPACITY = 256`), events.rs:196–214 (per-concept
  Stale)
- **Verified:** emit-on-transition updates `prev_conditions`
  unconditionally, so an event evicted from the 256-slot ring while its
  condition still holds is **never re-emitted**. Stale is detected per
  concept with no cap: warm-up on a resumed session (spec §2.5 — every
  restart) emits one Stale per concept in a single synchronous burst
  (measured: 4,000 into 256 slots; the fixture test shows 22/22). Conflicts
  are emitted *before* Stale in the same burst — wrapped out of the ring
  before any consumer can drain.
- **Disposition:** one Stale per **session** (spec §9 wording is "stale
  session"), or cap/coalesce per cycle; re-arm emission for conditions still
  in the hot list after a `Lagged` (or emit entering-conditions *after*
  computing, highest-severity first); consider per-kind channels or a
  capacity bump as defense-in-depth.

### XP-1 — CI never runs on phase branches (false TEST-3 closure)

- **Location:** .github/workflows/ci.yml:5 (`'phase-*'`)
- **Verified twice** (orchestrator + reviewer): GitHub branch globs don't
  cross `/` and the literal hyphen never matches `phase/p4-daemon`.
  `gh run list --branch phase/p4-daemon` → empty; no PRs exist, so the PR
  trigger never fired either. All 5,802 lines reached the phase branch with
  zero CI. This also reopens the Wave-1 TEST-3 disposition for every phase
  branch.
- **Disposition:** `'phase/**'` (one-line fix); re-run the matrix on this
  branch before merge.

### XP-2 — eight claimed review events, zero committed records

- **Location:** PHASE-4-daemon.md status lines (:44, :67, :83, :100, :117,
  :140, plus cd9340e's "final review remediation" and ":222 three loop
  redesigns") vs dev-diary/adversarial-review/ (no `adve-review-t4.*`)
- **Verified:** directory contains zero T4.x records; every P2/P3 task has
  one; the review index claims completeness. The remediations themselves are
  present in code (emit-on-transition, retain_conditions, epoch-gating), so
  this is process/auditability, not false remediation — but house rule 8 is
  unambiguous, and a future regression has no reopen criteria.
- **Disposition:** backfill the six T4.x records (findings, verdicts,
  remediation commits) before merge; update the index.

### XP-3 — hot-list revalidation cannot age anything out (P5 will serve lies)

- **Location:** hotlist.rs:265–280; predicates conflict.rs:203,
  events.rs:296, events.rs:328; contract PHASE-4:71/:74 → PHASE-5:81–82
- **Verified:** the `holds` closures capture `now` **by move at detection
  time**. `conflict_at(g, node, window, now)` derives its window from that
  frozen instant, so a Conflict/HighRisk entry re-validates true forever
  against an unchanged graph. The documented contract ("stale entries drop
  out then, **not on a timer**") is exactly inverted — entries age out only
  via the daemon's per-cycle `retain_conditions`, never on revalidation.
  T5.3 calling `revalidate` five minutes later gets `true` and renders the
  frozen `seconds_ago` — "wrote to it eleven seconds ago" as a factual lie
  on camera. The T4.6 review fixed this exact ghost class *for the loop* and
  knowingly kept the broken predicate public for recall.
- **Why tests miss it:** the discriminating test (advance clock →
  `revalidate` → assert false) doesn't exist; existing tests mutate the
  graph, not the clock.
- **Disposition:** capture the window, not the instant — predicates take
  `now` as a parameter (`revalidate(graph, node, now)`) or re-read a Clock;
  recompute `seconds_ago` at read time; add the clock-advance regression.

### XP-4 (+ ALGO-7) — P6's event seam is unreachable

- **Location:** events.rs:125 (`pub(crate) emit_canonized(sender, …)`),
  mod.rs:87 (private field), mod.rs:224 (Receiver-only accessor); contract
  PHASE-4:207/:218 → PHASE-6:83–85
- **Verified:** no public path to the `Sender` exists anywhere in `src/`.
  P6 can neither emit on the daemon's channel nor own the modules (T4.x
  `owns`). The exit-criterion test builds a *local* channel — it proves the
  variant serializes, not that the seam is callable. `#[allow(dead_code)]`
  on the helper is the compiler agreeing.
- **Disposition:** add `Daemon::event_sender()` (or pass the sender into
  P6's evaluator at construction); wire one integration test proving a
  `Canonized` emitted by a non-daemon caller reaches `Daemon::events()`
  subscribers.

### XP-11 — P4 regressed the CI feature matrix (build failure on two rows)

- **Location:** src/daemon/conflict.rs:212, src/daemon/hotlist.rs:606
- **Verified:** bare `use crate::fixtures;` / fixture-loading test without a
  `#[cfg(feature = "fixtures")]` gate. `fixtures` is an optional feature, so
  `cargo test --no-default-features --features store-sqlite` and
  `--features store-cockroach` both fail with E0432 — exactly the CON-4
  class the E2E's TEST-2 remediation added matrix rows to prevent. Invisible
  on this branch solely because CI never runs here (XP-1); the moment the
  glob is fixed, these two rows go red.
- **Disposition:** gate the imports/tests behind
  `#[cfg(feature = "fixtures")]` (the sqlite module shows the pattern);
  re-run all five matrix rows on the branch.

## Branch-state findings (from the E2E verification addendum)

The E2E verification pass (see the addendum in
`adve-review-e2e-p0-p3-fable.md`, same commit) surfaced three branch-level
facts that gate this merge alongside the P1s above:

1. **Stale base:** `phase/p4-daemon` branched from `6266f53` — Wave 8
   (`28500f3`), the wrap-up (`99052e7`), and the E2E closure (`dc5da31`)
   are NOT ancestors of this branch. No file overlap, so merging main
   restores them — but this branch's gates never ran against them, and the
   disposition record's "phase/p4-p7 fast-forwarded onto final main" claim
   is false for this branch (COH-12 recurrence). **Merge final main into
   the branch before remediation testing.**
2. **GRAPH-9 was never dispositioned:** the wave table reads
   "GRAPH-1..8, GRAPH-10" — the E2E closure's "all 45" is arithmetically
   44. Fix (NFC/NFKC normalization) or record an explicit acceptance;
   either way the closure needs a correction.
3. **E2E verification verdict:** 7/7 P1 and 32/38 P2/P3 dispositions
   verified (21 code, 11 doc); closure stands except GRAPH-9, TEST-3
   (CI glob = XP-1 here), COH-12 — plus, branch-only, TEST-6/7/8 absent
   with Wave 8 and the COH-3 schema-convergence wrap-up.

## P2 / P3 findings — condensed

(Full detail in the reviewer reports; locations in the summary table.)

- **CONC-3:** warm-up emits before any late subscriber exists;
  P8 must subscribe before `spawn` or the planted demo Conflict is never
  delivered (emit-on-transition ⇒ no recovery). Document the ordering on the
  P8 seam + add a subscribe-after-spawn test (all eight loop tests subscribe
  first today).
- **CONC-4:** adopt flush.rs's panic containment (`CatchUnwindPoll` +
  respawn-or-flag); replace config-reachable `expect()`s on
  `ChronoDuration::from_std` with checked conversion.
- **CONC-5:** per-node revalidation should call per-node predicates
  (`conflict_at` pattern) — not whole-graph `detect_*` passes — before T5.3
  puts it on the recall path under the graph lock.
- **XP-5:** return/expose `GcOutcome` (Daemon field or event): T6.4's
  budget signal, the advisory warning (and *any* daemon logging — the tier
  currently has zero tracing), and a callable `sync_index` all depend on it.
- **XP-6 (+CONC-7):** port the daemon suite to
  `#[tokio::test(start_paused = true)]`; the negative assertions
  (sleep-then-assert-empty) are vacuous on a loaded runner, and none of this
  has ever run on CI (XP-1).
- **XP-7:** give the tick a named const + `Config` field + default;
  reconcile `u32` vs `usize` drift_threshold; stop duplicating spec
  constants as literals in `CycleParams::default()`.
- **ALGO-2/ALGO-3:** carry `writer: AgentId` (from the newest qualifying
  edge) in the conflict payload, and attribute writes by *acting* agent
  (thread through `record_action`) rather than the resolved source concept's
  `origin_agent` — both are demo-sentence integrity fixes.
- **ALGO-4:** `GcParams` takes the session's `ScoringWeights`.
- **ALGO-5 (+XP-9):** decide no-path semantics (spec says warn; code
  doesn't) and record it; fix PHASE-4:103's stale "weighted" wording either
  way.
- **ALGO-6:** accept array root goals (spec §6.1's own example) across
  `set_root_goal` / drift / GC — or freeze string-only *in the phase doc*
  with a P8 conversion shim; T1.4 explicitly carried this decision to P4.
- **CONC-6 (+XP-10):** bound or coalesce the per-survivor `UpsertNode`
  burst (counter-delta mutation, or chunked bumps across cycles); today one
  sweep can enqueue 20 full flush batches from inside the write guard.
- **XP-8:** `root_goal` durability — needs a Mutation kind (or a documented
  P8 re-set-on-load obligation); today drift silently dies on session
  reload.
- **ALGO-9/ALGO-10/ALGO-11/ALGO-12:** honor the §5 decay table in step 1;
  sanitize non-finite weights (one line); use `eviction_resistance` in the
  step-2 cut; make `set_root_goal` promotion deterministic (all matches or
  lowest-id) and stamp `occurred_at` from logical time.

## Recommended remediation order

0. **Merge final main (`dc5da31`) into the branch** — restores Wave 8,
   the schema wrap-up, and the closed E2E record; everything after is
   tested against the real base. Then **XP-11** (gate the two fixtures
   imports) so the matrix can actually pass.
1. **XP-1** (one line) + re-run CI matrix on the branch — protects
   everything else.
2. **ALGO-1** — recalibrate the GC cut + add the `session-rest-api` GC
   test. Nothing downstream is trustworthy while GC eats the demo session.
3. **XP-3** + **ALGO-2/3** — the recall-facing truthfulness cluster
   (revalidate with live time; payload carries the actual writer). These
   land before P5 T5.3 consumes the API.
4. **XP-4** — `Daemon::event_sender()`; unblocks P6's design.
5. **CONC-2** — Stale-per-session + re-arm policy; protects the demo event.
6. **CONC-1** — adjacency-index `incident_edges` (mechanical, big win).
7. **XP-2** — backfill T4.x records; then CONC-3/4/5, XP-5/6/7, ALGO-4/5/6
   and the P3 batch as capacity allows. None of the P3s block the merge.

## Verdict

The daemon's foundations are genuinely solid — the write-behind contract
holds perfectly (every GC mutation reaches the stores), the lock/await rule
is honored, cancellation is safe, the scoring math matches the spec to the
letter, and 86 new tests are honest in the pass direction. What this review
blocks on is the layer above the plumbing: a GC calibration that would
visibly destroy the demo session it exists to curate, an event channel that
can permanently drop the single most important event of the demo, a
revalidation API that will make the demo's headline sentence false, an
unreachable seam P6 is contractually told to call, and a CI fix that never
actually fired. All are cheap relative to what they protect. Merge final
main into the branch, remediate the eight P1s, re-run the matrix here,
backfill the T4.x records, and this tier is ready for `phase → main`.

— opus (claude-opus-5) ×3, orchestrated and verified by fable
  (claude-fable-5), 2026-08-12
