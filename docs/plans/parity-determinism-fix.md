# Fix: `lambo demo` OUTCOME was not byte-identical across runs

```yaml
status: IMPLEMENTED
owner: nryn
surface_changed: none (library API, CLI flags and MCP tool schemas are untouched)
test: tests/binary_parity.rs::demo_outcome_meets_spec_13_and_is_identical_across_two_runs
```

---

## 1. Symptom

`tests/binary_parity.rs::demo_outcome_meets_spec_13_and_is_identical_across_two_runs`
runs the shipped binary twice and asserts the two rendered OUTCOME blocks are
byte-identical. It failed intermittently. It first surfaced as a CI failure on
`macos-arm64`, but it is not platform-specific.

Measured on Linux, before the fix:

| Sample | Failures |
|---|---|
| 25 runs | 2 |
| 40 runs | 4 |
| 25 runs (re-measured for this work) | 2 |

So roughly **8–10%**, which is why a 40-run green streak was never sufficient
evidence of a fix.

The test is gated
`#![cfg(all(feature = "store-sqlite", feature = "embed-fixture", unix))]`.
Without `--features ship` the harness runs **0 tests** and reports green, so
every measurement below uses:

```
cargo test --features ship --test binary_parity demo_outcome
```

The visible failure is always an ordering swap between two near-tied concepts:
`redis backend` and `handlers/login.rs` trade places in `recall_context`, and
the `Agent B wrote to it …` line moves with them inside `recall_warnings`.
Everything else — counts, statuses, the canonization trail, the ⚑ line — was
already stable.

## 2. Root cause

The demo's own rendered order is a function of the daemon composite score, and
that score was a function of the wall clock.

The chain, in file order:

1. `src/memory.rs:1761` (`Memory::begin_interaction`) stamped every
   interaction's `created_at` from `Utc::now()`. Concepts, edges and actions all
   inherit that stamp (`src/graph/derive.rs:202`, `src/graph/action.rs:115`), so
   it is the only clock reading that reaches the score.
2. `src/daemon/score.rs:4` defines the composite as
   `recency·0.25 + frequency·0.20 + session_activity·0.20 + density·0.35`.
3. `src/daemon/score.rs:245-250` computes `recency` as the concept's position
   inside the session's temporal extent:
   `(last_touch − start) / (end − start)`, where `start`/`end` are the min/max
   interaction `created_at` (`SessionContext::compute`, `score.rs:215`).
   `span_ms` is a **millisecond** span of a session that lasts about a tenth of
   a second.
4. `src/recall/assemble.rs:13` and `:154` fold that daemon score into
   `final_score = daemon_score × w_daemon + query_relevance × w_query`, and
   `assemble.rs:165` sorts on it.

`recency` is 25% of a score whose denominator is ~110ms of wall clock. The demo
paced its writes with `tokio::time::sleep(STEP_PACING)` (10ms), which dominates
the jitter for *ordering purposes* most of the time — but a real sleep returns
in 10ms *plus* whatever the scheduler adds, and each of the twelve writes
absorbs a different amount. That moved each concept's position inside the extent
by fractions of a percent, which moved the composite in its low decimals, which
flipped the rendered order of any pair sitting inside that margin. `redis
backend` and `handlers/login.rs` are such a pair.

The module docs at `src/cli/demo.rs` already asserted "every scoring dimension
is session-relative … never against the wall clock". That was half true: the
dimensions are session-relative, but the *session's own extent* was a wall-clock
measurement, so the jitter came back in through the denominator.

## 3. Evidence

**The scores themselves differed run to run** — this is not a tie-break
problem. Running the built binary twice and diffing raw stdout, before the fix:

```
-  GC headroom: closest to the eviction bar is 'user id column' at 2.06× — nothing in this session is collectable
+  GC headroom: closest to the eviction bar is 'user id column' at 2.07× — nothing in this session is collectable
```

That is a scalar, derived from the same composite the ordering uses, changing
between two runs of the same script on the same store.

**A stable tie-break key does not help, and was already tried and reverted.**
Adding `Concept::canonical_key` ahead of the node-id tie-break in the
`members.sort_by` at `src/recall/assemble.rs:165` was implemented and measured:
**still 4 failures in 40 runs**. It cannot help, because the scores are not
equal — a tie-break only fires on exact equality, and these composites differ in
the low decimals. Do not retry this.

(The hypothesis was reasonable: node ids are `Uuid::new_v4()`
(`src/types/mod.rs:22`) and several sorts tie-break on them, which *is* a latent
cross-run instability. It is simply not the one that was failing. See
§7 follow-ups.)

## 4. The fix: an internal clock seam

The demo now stamps its interactions from a **monotone script clock** instead of
the wall clock: interaction *k* is stamped `base + k × STEP_PACING`, exactly.
The session's temporal extent becomes a property of the script rather than of
the scheduler, so `recency` — and every score, P90 cut, GC margin and rendered
ordering downstream of it — is a pure function of the graph.

### How it preserves the no-caller-timestamps invariant

The invariant is stated in `site/src/content/docs/mcp.mdx`: *"Do not send
timestamps. Lambo stamps all times itself, and no tool accepts a timestamp
argument. Sending one is refused by name."* It is enforced by
`deny_unknown_fields` on the MCP param structs and pinned by
`tests/binary_parity.rs::mcp_stdio_publishes_exactly_seven_tools_and_refuses_a_client_timestamp`.

Nothing about that changed. Specifically:

* **The seam is `pub(crate)`.** `MemoryBuilder::clock` (`src/memory.rs:495`) is
  not exported from the crate. `src/lib.rs` re-exports `MemoryBuilder`, but a
  `pub(crate)` method on it is invisible to every downstream user, so there is
  no way from outside the crate to install a clock at all. (The one new public
  item is `cli::demo::script_clock`, a demo helper kept `pub` to match every
  other helper in that module — `build_config`, `fresh_session_id`,
  `normalize_score`. It returns a `daemon::Clock`, a type that was already
  public because `Daemon::with_clock` and `CanonizationTask::with_clock` are.
  It grants no capability: without `MemoryBuilder::clock` there is nothing
  external it can be handed to.)
* **It is a construction-time process decision, not a per-call argument.** The
  distinction the invariant protects is *who* decides what "now" means. A
  process may decide, once, before its first write, what clock it reads — that
  is what `Utc::now` already was. A *caller* may not decide it, and may not
  decide it differently per write, because that is what lets an untrusted client
  backdate one interaction by 61s and neuter the `canonization_edge_min_age`
  inflation guard (P6 review F18). The seam is a builder setter for exactly this
  reason: it cannot be reached across the MCP boundary, cannot vary per
  `derive`, and cannot reorder interactions relative to each other.
* **The MCP and CLI surfaces are byte-identical.** No tool schema, no CLI flag,
  no config key was added. `lambo_derive` still refuses a `timestamp` argument
  by name; the parity test that asserts it still passes.
* **The demo's clock is still honest about absolute time.** `base` is a real
  `Utc::now()` taken at the start of the run, not a fixed epoch. Only the
  *interior spacing* is synthetic. Every consumer of absolute age — the Stage 2
  / Stage 3 `canonization_edge_min_age` floors, the 30s conflict-recency window,
  the single-writer lease TTL — still sees a session that is genuinely as old as
  it looks. Pinning `base` to a constant (e.g. `2024-01-01`) would have made the
  whole graph years old and silently turned those age floors into no-ops, which
  is precisely the guard the demo's compressed-knob table claims is still live.
* **The script clock never runs ahead of the wall clock.** `play` sleeps
  `STEP_PACING` *before* each write, so real elapsed time at stamp *k* is always
  at least `k × STEP_PACING`. Logical time therefore trails real time; the demo
  never claims a write happened in the future.

## 5. What changed

| File | Change |
|---|---|
| `src/memory.rs` | `Memory` gains a `clock: Clock` field (`:894`); `MemoryBuilder` gains a `pub(crate) fn clock` setter (`:495`) defaulting to `Arc::new(Utc::now)` (`:562`); `begin_interaction` reads `(self.clock)()` instead of `Utc::now()` (`:1761`). Module docs extended to state why the seam is not a hole in the F18 rule. |
| `src/cli/demo.rs` | New `script_clock()` (`:938`) returning a `crate::daemon::Clock` that hands out `base + k × STEP_PACING`. `run_scenario` mints one and shares it across all five `Memory` handles; `open` takes it. `STEP_PACING` docs now describe both clocks. Determinism list in the module docs gains item 3 and renumbers. `normalize_score`'s rationale updated. |

The existing `Clock` type alias (`src/daemon/mod.rs:99`) is reused rather than a
new one introduced — the daemon and the canonization task already carry the same
seam for the same reason (`Daemon::with_clock`, `CanonizationTask::with_clock`).

The counter is shared across handles on purpose: the demo opens five `Memory`
handles (agent A, agent B, agent A, the canonization attach, agent B's read),
and only the first three write. Sharing one counter makes the twelve
interactions `base + 0ms … base + 110ms` regardless of how the acts split them.
`declare_synonym` and `recall` open no interaction, so the stamp count is
exactly `EXPECT_INTERACTIONS` (12).

Nothing about the test was weakened: no `#[ignore]`, no order-insensitive
comparison, no rounding, no loosened assertion. `normalize_score` /
`normalize_conflict_age` / `normalize_node_ids` are unchanged from before the
bug was reported.

## 6. Verification

All numbers below are real, from this worktree on Linux.

### 6.1 Baseline (before the fix)

```
$ loop.sh 25   # cargo test --features ship --test binary_parity demo_outcome
FAIL run 7
FAIL run 12
failures: 2 / 25
```

Matches the previously reported 2/25 and 4/40.

### 6.2 After the fix

```
$ loop.sh 25    → failures: 0 / 25     (first check after the change)
$ loop.sh 100   → failures: 0 / 100    (doc comments were edited mid-loop)
$ loop.sh 100   → failures: 0 / 100    (clean: no edits during the run)
$ loop.sh 25    → failures: 0 / 25     (final binary, after the rustdoc-link fixups)
```

**100 consecutive passing runs on a frozen tree**, and 250 passing runs in
total across the four samples. At the measured ~8% base rate, the probability of
100 clean runs by luck is about 0.92^100 ≈ 2 × 10⁻⁴.

The 100-run sample is reported twice on purpose: doc comments were edited while
the first one was in flight, so `cargo test` rebuilt partway through it. The
change was comment-only and the result was 0/100 either way, but the honest
number is the second run, where nothing in the tree moved.

### 6.3 Full suite

```
$ cargo test --features ship
```

Green. `0 failed` on every target: 792 lib tests, the integration targets
(including all four `binary_parity` tests and both `t84_demo`
`scenario_is_identical_twice_*` tests), and 2 doc-tests. The 10 `ignored` tests
are pre-existing and unrelated (live-cluster gated).

### 6.4 Raw binary, run twice

```
$ ./target/debug/lambo --config <scratch>/lambo.toml demo > a.txt
$ ./target/debug/lambo --config <scratch>/lambo.toml demo > b.txt
$ diff -u a.txt b.txt
```

The whole diff is five lines, in three places:

1. `session demo-rest-api-<uuid>` — intentionally random, fresh per run (P6
   R3-1).
2. `High-risk modification: high-value node <uuid>` — intentionally random node
   id.
3. `cycle 0 / cycle 1` vs `cycle 1 / cycle 2` on two **narration** lines: which
   canonization cycle index the `Candidate` and `Venerable` hops landed on.

Everything else is byte-identical, including every rendered score
(`score 2.27`, `score 1.50`, …) and the GC headroom line, which is now a stable
`2.10×` where it previously alternated between `2.06×` and `2.07×`.

Item 3 is a residual and is called out as a follow-up in §7. It is a count of
completed background cycles, not a property of the fixed point; it is outside
the OUTCOME block and outside the ×2 assertion, and the demo's own docs already
argue the fixed point is independent of which cycle each hop happened on.

### 6.5 Command log

| Command | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo build --features ship --tests --bins` | clean, no warnings |
| `cargo test --features ship --test binary_parity demo_outcome` ×25 (pre-fix) | 2 failures |
| `cargo test --features ship --test binary_parity demo_outcome` ×25 (post-fix) | 0 failures |
| `cargo test --features ship --test binary_parity demo_outcome` ×100 (post-fix, clean tree) | 0 failures |
| `cargo test --features ship` | 0 failed (792 lib + 25 across the integration targets + 2 doc-tests; 10 pre-existing `ignored`) |
| `cargo clippy --features ship --all-targets` | no warnings |
| `cargo doc --features ship --no-deps` | no warnings |

## 7. Residual nondeterminism (explicit follow-ups, not fixed here)

1. **`Uuid::new_v4()` node ids as a sort tie-break.** Node ids are random
   (`src/types/mod.rs:22`) and several sorts tie-break on them
   (`src/daemon/score.rs:300`, `src/recall/assemble.rs:165`,
   `DemoOutcome::transitions`' grouping comment at `src/cli/demo.rs`). Today the
   demo works around it by *grouping* the audit trail by concept rather than
   comparing raw commit order. This is a real latent cross-run instability for
   any graph that does produce exact score ties — it just was not the cause of
   this bug, and the tie-break experiment above proves it. Worth fixing as a
   separate change (a content-derived stable key would be the natural choice),
   deliberately not bundled here.
2. **Narrated canonization cycle indices.** See §6.4 item 3. Making these stable
   would mean pinning the interleaving of the canonization timer against the
   settle loop — a bigger change to the demo's pacing than this fix warrants,
   and with no effect on the asserted OUTCOME.
3. **The conflict age integer.** Still genuinely wall-clock derived and still
   normalized to `<n>` by `normalize_conflict_age`. Unchanged, and correctly so:
   it is the true age of agent A's write at read time.
4. **Cross-platform float reproducibility is not claimed.** `normalize_score`
   is retained. Scores now agree across runs on one machine, but the ×2 bar
   deliberately asserts the outcome (which concepts, in which order, with which
   warnings), not the f64 summation order of the scoring loop.

## 8. Note for the owner: the evidence capture

`evidence/demo-live-diff.txt` records the two live-cluster demo runs as
`IDENTICAL - T8.4 x2 met`, and `docs/reference/evidence.mdx` (and its
`site/src/content/docs/evidence.mdx` twin) quote that result under
"Deterministic convergence on live clusters".

That capture was taken while this bug was live, so it passed **by luck** at
roughly a 90% per-attempt probability rather than by construction. The claim it
makes is now true by construction, so the conclusion stands — but the capture
predates the fix and may warrant an annotation saying so.

**Nothing under `evidence/` was modified by this change** (verbatim captures;
editing them destroys their value). Flagging only; the decision is the owner's.
