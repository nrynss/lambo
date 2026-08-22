# Adversarial review — mooshik D (event-time clock), round 1

**Reviewer**: independent adversarial reviewer, agent_id `DReview`. Wrote nothing under
review except this file (mutation probes ran in a scratch clone `/tmp/mut-d`, deleted
afterwards; the reviewed tree was never modified).
**Scope**: the seven commits `7dbd08d..f1431ce` on branch `d-event-clock` (`faeaea7`
types, `02657e9` D1 write path, `0409137` D2 gates + persistence, `7ea62e1` SoloScorer
doc, `b903534` doctest, `7cee2d3` span SQL + fixture defaults, `f1431ce` column
constants). Against §D1/§D2/Done-when of `dev-diary/lambo-for-mooshik/D-event-clock.md`.
**Worktree**: `/tmp/lambo-d`, branch `d-event-clock`, HEAD `f1431ce`.
**Verdict**: **REQUEST_CHANGES** — one **P1**, two **P2**, two **P3**.

The load-bearing design claims survive the attack. The fallback rule (`about_time()` =
`COALESCE(event_time, created_at)`) is applied consistently across all three adapters and
the graph tier; every production `Edge` constructor inherits the writing interaction's
event time (I traced all six sites); the coverage-monotonicity argument for mixed
sessions is **correct** (proof sketch under "verified holds"); the pinned-clock claim is
complete for the evaluated paths; and the Done-when Box 2 test is **honest** — I
mutation-tested it twice and both mutants kill it. What fails: the implementer shipped a
const-assert violation that does not compile under `--features store-sqlite,fixtures`
(proving that gate was never run), the persistence round-trip of a *non-NULL* event time
has zero test coverage on both durable adapters, and the async derive seam cannot carry
event time at all while its doc claims otherwise.

## Method

1. Read the spec, then the full `git diff 7dbd08d..f1431ce` (46 files) plus the complete
   `src/canon/event_time.rs` (477 lines).
2. Traced persistence end to end: both migrations, `batch.rs` column constants vs the
   actual INSERT column lists, `upsert_interactions`/`upsert_edges` binds and ON CONFLICT
   clauses, load queries, seed path, and the Cockroach SQL-shape asserts.
3. Enumerated every `Edge {`/`Interaction {` initializer in production code (grep across
   `src/graph`, `src/daemon`, `src/canon`, `src/recall`, `src/writeq`, `src/mcp`,
   `src/cli`) and checked each for event-time provenance.
4. Grepped every `Utc::now()` in `src/` (≈130 sites) and adjudicated each non-test hit
   against the "pinned eval is deterministic" claim.
5. Re-ran the gates personally:
   - `cargo fmt --all -- --check` — **pass**.
   - `cargo clippy --all-targets -- -D warnings` — **pass**.
   - `cargo clippy --all-targets --features store-cockroach -- -D warnings` — **pass**.
   - `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` —
     **FAILS TO COMPILE** (D-R1-1, below).
   - `cargo test` (default) — **pass** (no failures; doc-tests ok).
   - `cargo test --features store-sqlite,fixtures` — **FAILS TO COMPILE** (same root).
6. Mutation-tested Done-when Box 2 in a scratch clone (`/tmp/mut-d`, CARGO_TARGET_DIR
   shared, reviewed tree untouched):
   - Mutant A — session extent measured on `created_at` instead of `about_time()`
     (`store/memory.rs` interaction_span): test **FAILS** with
     `coverage=1 expected=0.7996715927750411`. The asserted 0.8 is genuinely derived
     from the gates' arithmetic, not restated construction parameters.
   - Mutant B — `about_time()` returns `created_at` unconditionally (`types/mod.rs`,
     both impls): test **FAILS** (event half collapses onto the ingest half's numbers).
7. Verified the P1's fix direction in the same clone: `edges: 100 → 99` makes
   `cargo check --all-targets --features store-sqlite,fixtures` pass, and the **entire**
   `cargo test --features store-sqlite,fixtures` suite then passes (1006 lib + 40
   integration tests, 0 failed) — nothing else hides behind the compile error.
8. Attempted to falsify the mixed-session coverage direction (attack surface 3); could
   not. Argument: in all three adapters the span's origin interactions are
   session-filtered (`i.session_id = ?` / MemoryStore is per-session), so the support
   instants are a subset of the session extent's domain; widening `[lo, hi]` can only
   lower or preserve `span/extent` (clamped ≤ 1.0 in all three implementations), and the
   `sess_span <= 0 → 1.0` F1 branch is parity-checked across MemoryStore
   (`memory.rs:831-837`), SQLite (`sqlite.rs:1383-1391`), and Cockroach SQL
   (`cockroach.rs:570-581`). No counterexample corpus exists; coverage gets harder, never
   easier, under mixing. **HELD.**

Order of authority: the spec is the claim; the source is what ships; where a docstring
and the code disagree, the code wins and the disagreement is a finding (D-R1-3 is
exactly that).

## Findings

### D-R1-1 (P1) — `--features store-sqlite,fixtures` does not compile: edges chunk exceeds SQLITE_MAX_VARIABLE_NUMBER

**Evidence**: `cargo clippy --all-targets --features store-sqlite,fixtures` and
`cargo test --features store-sqlite,fixtures` both abort with

```
error[E0080]: evaluation panicked: edges chunk exceeds SQLITE_MAX_VARIABLE_NUMBER
  --> src/store/sqlite.rs:292:15
```

`f1431ce` bumped `EDGE_COLUMNS` 9 → 10 (`store/batch.rs:78`) but left
`BULK_LIMITS.edges = 100` (`store/sqlite.rs:270-274`): 100 × 10 = 1000 > 999, tripping
the R1-4 const assert the branch itself documents as "prose does not fail a build". The
prose above the constant (`sqlite.rs:262-269`) still argues "9 × 100 = 900 both fit
either way" — the exact arithmetic the assert just falsified. Interactions
(100 × 7 = 700) and concepts (60 × 16 = 960) still fit.

**Scenario**: any CI or developer running the documented sqlite gate hits a hard build
failure. Worse, this is proof the implementer never ran the sqlite+fixtures gate after
`f1431ce` — the commit message claims the constants were bumped *for* the bind limit,
and the very gate that exists to catch this was left red.

**Fix** (verified in scratch clone): set `BULK_LIMITS.edges = 99` (99 × 10 = 990 ≤ 999)
and rewrite the stale prose to `10 × 99 = 990`. With that one-line change the full
sqlite+fixtures suite passes (my run: 1006 lib + all integration suites, 0 failed), so
no further breakage is hiding behind the compile error.

### D-R1-2 (P2) — no durable-adapter test writes a non-NULL event_time; the round-trip claim is untested

**Evidence**: `grep -rn 'event_time: Some' src/ tests/` returns **zero** hits across the
entire repo. The only tests exercising `Some` are the MemoryStore canon tests in
`canon/event_time.rs` (via `.then(|| …)`), which never touch SQLite or Cockroach. The
sqlite load reads event_time **positionally** (`row.try_get(9)` for edges,
`sqlite.rs:2668`; interactions likewise at `sqlite.rs:2553`), and the cockroach load by
name (`cockroach.rs:1052,1104`) — neither is exercised with a populated value. The
commit-message claim "full upsert/load round-trip sqlite.rs + cockroach.rs" describes
code that exists but behaviour no test observes.

**Scenario**: a regression that binds `i.created_at` where `event_time` belongs (or an
index drift in the positional edge read) leaves every existing test green — the column
is NULL on both sides of every current adapter test, so NULL ≡ NULL passes — while every
historical fact silently re-ages onto flush time and Stage 2 flips back to the exact
bootstrap failure D exists to fix. The failure mode is total and invisible.

**Fix**: one round-trip test per durable adapter: upsert an interaction and an edge with
`event_time: Some(distinct_instant)` (distinct from `created_at`, so a mis-bind cannot
pass), flush, load, assert equality; plus a companion row with `None` asserting it stays
`None` after reload.

### D-R1-3 (P2) — `derive_async_as` cannot carry event time, and the module doc's claimed workaround is impossible

**Evidence**: `Memory::derive_async_as` (`memory.rs:1603-1649`) opens its interaction via
`begin_interaction_as(agent, Some(prompt))` — no event-time parameter exists on the
method, on `pipeline.submit_derive` (`writeq.rs:2505`), or anywhere in the async derive
path. `record_action_async_as` *did* get the seam (`memory.rs:1674-1677`). The design doc
(`canon/event_time.rs` §1) asserts: "an ingester that wants queued writes stamped can
open them through the sync seam first" — but `begin_interaction_full` is private, the
async derive opens its own interaction internally with `None`, and the sync seam
(`derive_for_ingest`) applies synchronously, so no queued derive can ever be stamped.
MCP's derive tool already routes through this path (`mcp/server.rs:1620`).

**Scenario**: C2 builds the historical-corpus ingester on the async lanes (the natural
choice for a ten-year replay, which is precisely D's motivating workload — ordering with
`record_action` is the documented reason the lane exists, `memory.rs:1668-1674`). Every
queued derive lands `event_time: NULL`, the corpus canonizes on flush time, and D's
entire benefit evaporates **silently** — no error, no test, just the old bootstrap
behaviour.

**Fix**: either add `event_time: Option<DateTime<Utc>>` to `derive_async_as` (threaded
through `submit_derive` into the pre-opened interaction — the plumbing already exists on
the action side), or correct the `event_time.rs` §1 doc to state the restriction
explicitly ("stamped writes must use the sync seams") so C2 inherits a true statement
instead of a trap.

### D-R1-4 (P3) — reinforcement's event-time preservation is verified-by-trace only; no test pins it

**Evidence**: the graph-tier reinforcement arm of `record_edge`
(`graph.rs:1544-1552`) mutates only `weight`, `reinforcements`, and `last_reinforced`,
so the original edge's `event_time` is preserved exactly as "original id and
created_at preserved" requires — I traced it, and the claim is **true**. The store tier
deliberately does whole-record replace on natural-key conflict (`cockroach.rs:334-341`,
`sqlite.rs:2240-2248`, I2 convention documented inline), which is consistent because the
graph tier emits the preserved record. But no test anywhere pins the preservation: a
future refactor that copies incoming fields into the existing edge (the arm is three
assignments away from doing so) would silently re-age every reinforced edge in
event-timed sessions.

**Fix**: one graph-tier test — create an edge under an event-timed interaction,
reinforce it from a differently-timed one, assert `event_time` unchanged. Three lines
against the existing `graph.rs` test helpers.

### D-R1-5 (P3) — `separated_session_count` has no production caller; D2's separation deliverable is an uncalled helper

**Evidence**: `grep -rn separated_session_count src/ tests/` finds only the definition
(`canon/event_time.rs:73`), the re-export (`canon/mod.rs:24`), a doc pointer
(`canon/policy.rs:199`), and its own tests. Spec D2 says "move … session separation onto
the injected clock", and Done-when box 1 says the separation gate honours event time —
currently the only consumer is C2, which does not exist. Scope is defensible:
`SoloScorer`'s `unimplemented!` refusal pre-exists at base `7dbd08d`
(`policy.rs:208`) and `Config::validate` refuses `PromotionPolicy::Solo`, so this is not
a regression (attack surface 7: **cleared**). The residual risk is that C2 wires the
recurrence term to flush stamps instead of `about_time()`; the only guard is a prose
warning in `policy.rs:194-200`.

**Fix**: none required for D beyond what exists; when C2 lands, its tests must assert
the recurrence term resolves session starts through `about_time()` (a mutant test in the
C2 review should kill a flush-stamp substitution).

## Verified holds (attack surfaces with no finding)

- **Schema + persistence shape** (surface 1, minus D-R1-2): both migrations add the
  nullable column in the right tables (`migrations/sqlite/001_init.sql`,
  `migrations/cockroach/001_init.sql`); INSERT column lists in both adapters match the
  bumped `INTERACTION_COLUMNS`/`EDGE_COLUMNS` bind counts (interactions 7, edges 10 —
  verified against `upsert_interactions`/`upsert_edges` bind chains); serde
  `default` + `skip_serializing_if` keeps the seven fixture JSONs and golden files
  loading unchanged (default suite green); the seed path reuses the same upsert
  functions (`sqlite.rs:555-566`, `cockroach.rs:1280-1290`), so snapshots carry event
  time. Cockroach "tests" are SQL-shape asserts only (`cockroach.rs:3378-3399`) — honest
  about needing no cluster, but see Operator-leg items.
- **The mechanical sweep** (surface 2): all six production `Edge` constructors inherit
  the writing interaction's event time (`graph.rs:392` temporal, `graph.rs:484` Derives,
  `derive.rs:389,463`, `hybrid.rs:919,959,1022`); `writeq.rs:1858`'s
  `Action { event_time: None }` is inert because `record_action` reads event time from
  the *interaction node* (`action.rs:123-127`), and the queued job's interaction was
  pre-opened at submit; GC and canonization re-snapshot concepts only — no production
  code re-emits an `Interaction` node (all `Node::Interaction(` re-emits are test
  helpers). No lost-stamp site found.
- **Coverage monotonicity under mixing** (surface 3): proven monotone, see Method step
  8. **HELD.**
- **Pinned-clock completeness** (surface 6): every `Utc::now()` outside tests is either
  the advisory lease default (`store/mod.rs:475`), the GC cycle's own flush-domain stamp
  (`daemon/gc.rs:166`), the daemon score's stored-`created_at` extent with `Utc::now()`
  only as an empty-session fallback (`daemon/score.rs:222` — deterministic given stored
  data, and deliberately flush-domain per issue #2's framing), or diagnostic/report
  paths (`memory.rs:1801` retract report, web UI). `CanonizationTask::with_clock`
  (`task.rs:156`) feeds the single `now` used by every store gate query. No wall clock
  behind the eval's back.
- **Done-when Box 2 honesty** (surface 5): two mutants, both killed (Method step 6).
  The asserted `distinct = 3` / `coverage ≈ 0.7997` / pass-vs-fail pair are computed
  from the gates' arithmetic at a pinned instant. **HONEST.**

## Operator-leg items

1. **Live-Cockroach parity**: this environment has no CockroachDB cluster, so the D
   changes to four Cockroach queries (`BLAST_RADIUS_SQL`, `INTERACTION_SPAN_SQL`,
   interaction/edge loads — `COALESCE(event_time, created_at)` in six predicates) and
   the two `TIMESTAMPTZ` DDL columns are verified by SQL-shape asserts only
   (`cockroach.rs:3378-3399`). A live leg (seed event-timed rows through
   `CockroachStore`, reload, run `interaction_span`) should be run by the operator per
   the established runbook before the branch merges; `COALESCE` over `TIMESTAMPTZ` is
   standard but the `extract(epoch …)` coverage arithmetic in `INTERACTION_SPAN_SQL`
   deserves one live observation.
2. **Process**: D-R1-1 is also a process finding — the sqlite+fixtures gate was
   demonstrably not run after the commit that changed the bind-limit constants. The
   remediation round should state which gates were executed for the fix commit.

## Round 1 closures

| Finding | Closure (commit + evidence) |
| --- | --- |
| D-R1-1 | `ec15d9e` — `BULK_LIMITS.edges` 100 → 99 (99 × 10 = 990 ≤ 999) and the
  stale prose above the constant rewritten to the same arithmetic
  (`store/sqlite.rs:262-274`). Verified post-fix: both sqlite+fixtures clippy gates
  compile clean and the full `cargo test --features store-sqlite,fixtures` suite passes
  (1047 passed, 0 failed) — confirming the reviewer's scratch-clone finding that nothing
  else hid behind the compile error. Gates run for the fix commit: all six listed below. |
| D-R1-2 | `4473c1a` — sqlite: `event_time_survives_the_flush_load_round_trip` flushes an
  interaction and an edge stamped with an about-time distinct from `created_at`
  (`1999-12-31T23:59:59Z` vs `2026-01-02T03:04:05Z`, so a mis-bind cannot pass), loads,
  asserts survival, plus `None` companions asserting they stay `None`. Cockroach: no live
  cluster per the Operator-leg items, so the SQL-shape contract is pinned instead —
  `event_time_rides_the_upsert_and_select_shape` asserts the generated upserts bind
  event_time as the last of 7/10 columns, `DO UPDATE SET` re-stamps it, and both SELECTs
  read it back by name. |
| D-R1-3 | `5c115cc` — `derive_async_as` now takes `Option<DateTime<Utc>>` and opens its
  interaction via `begin_interaction_full` at submit time (the same seam
  `record_action_async_as` already used), so every edge the queued derive creates inherits
  the stamp; no `submit_derive` payload change was needed because the interaction is
  pre-opened before queueing. Existing call sites pass `None` (MCP derive
  `mcp/server.rs`, three in-crate tests). The dead-agent worktree hunk that dropped
  `.collect::<Vec<_>>()` from the prompt builder was found and repaired.
  `begin_interaction_as` lost its last caller and was removed (clean cutover). §1 of
  `canon/event_time.rs` now describes this real mechanism instead of the impossible
  "open through the sync seam first" workaround. |
| D-R1-4 | `3a2c0d3` — `reinforcement_preserves_the_original_edge_event_time` (graph-tier
  test): an edge written under a historical about-time is reinforced from a differently-
  timed turn; the test asserts the original `event_time` survives while weight bump,
  reinforcement count, and `last_reinforced` move exactly as the arm specifies. |
| D-R1-5 | `1d30eee` — docs only, as the finding allows: `separated_session_count`'s doc
  now states plainly that it has **no production caller by design**, names
  `dev-diary/lambo-for-mooshik/C-solopolicy.md` as the consuming spec, and records the
  mutant C2's tests must kill (flush-stamp substitution for `about_time`). No fake
  callers were added. |

Gates at closure HEAD (all green):

- `cargo fmt --all && cargo fmt --all -- --check` — pass.
- `cargo clippy --all-targets -- -D warnings` — pass.
- `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` — pass.
- `cargo clippy --all-targets --features store-cockroach -- -D warnings` — pass.
- `cargo test` — pass (16 suites, 0 failures).
- `cargo test --features store-sqlite,fixtures` — pass (1047 passed, 0 failed).
