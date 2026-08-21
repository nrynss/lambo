# Adversarial review — mooshik J3, round 3

**Reviewer**: independent adversarial reviewer (Opus 5), agent_id `j3-reviewer-r3`. Wrote
nothing under review.
**Scope**: the three remediation commits `869b898..ea1e577` on `wt/j3` — `41b46d9`
(J3-R2-1/2/4, the probe sizes nothing load-bearing), `b107981` (J3-R2-6/9, the ordering
scope and the piggyback loop), `ea1e577` (J3-R2-3/5/8, the Done-when box and the numbers).
Against `adve-review-mooshik-J3-round2.md`'s ten findings, §J3's rewritten status note and
`[~]` Done-when box, and the graph's two linked constraints — *a projection is not a bound*
and *a measurement is only a bound for the workload it sampled*.
**Worktree**: `/Users/narayan/Documents/work/lambo/.claude/worktrees/j3`, branch `wt/j3`.
No commit amended. Every verification edit reverted; `src/writeq.rs` md5
`6a3a7b179e1359c7583b3416f3ac9a43` before and after, `git diff HEAD -- src/writeq.rs`
empty, tree clean but for this file.
**Verdict**: **REQUEST_CHANGES** — **two new P1**, one **P2**, three **P3**.

**All ten round-2 findings are closed at the artifact**, and this remediation is the most
carefully evidenced of the three. I reproduced both sides of the P1 independently, ran their
mutation pair and got their exact numbers back, built a decisive mutant for the one finding
round 2 said was under-pinned, reconciled all four gate figures repo-wide by name, and
re-derived every new constant and every re-measured figure from the artifact. The
representative leg, the ceilings, the retained probe figure, the flip log line, the four new
probe tests and the piggyback assertion are all real, all load-bearing, and all better than
the review asked for.

The blockers are **not** regressions and **not** round-2 findings reopened. They are the
*same hazard* — an acked write abandoned at a clean `close()` — reached through the two doors
the new admission still leaves open, and both are measured at the release binary against the
live embedder:

* **J3-R3-1** — the observed rate samples writes that **never embedded**. `spawn_worker`'s
  `if outcome.is_ok()` filter is the J3-R2-2 fix, and an embedder failure on the hybrid
  derive path is not an `Err`: the concept is applied with `embedding = NULL` and the write
  returns `Ok`. The rate inflates 20–45×, the lane bound with it, and in the `Observed` era
  there is no ceiling. **326 and 361 acked writes abandoned**, two runs.
* **J3-R3-2** — `PROBE_AGGREGATE_CEILING` = 16 admits sixteen concurrent writes across
  sixteen lanes, each lane inside its own bound of four. Sixteen-way concurrency on this rig
  runs ~5× slower than the probe's 4-wide figure, so they do not drain in 2 s.
  **13 of 16 abandoned**, 3/3 runs, with abandonment starting at **eight** concurrent agents.

One of those two is partly my predecessor's doing and I say so plainly below: round 2
affirmatively cleared the lane-count direction as "safe by construction" in its *attacks
that did not land*, and the remediation reasonably declined a fix on that clearance. The
clearance was wrong. The defect is real regardless.

## What I did

1. Read the round-2 review in full (463 lines) and recalled the graph
   (`lambo_recall "J3 probe representative ceilings observed"`, `j3-reviewer-r3`), which
   returned the remediation's own design record including the 1536 B refusal measurement.
2. Read every new and changed constant, `from_probe` / `from_rates` / `with_observed_serial`
   / `probe_optimism` / `project` / `rate_of`, `probe_embedder` / `probe_text_at`,
   `spawn_worker`'s sampling site, `ObservedRate`, `DropReason::describe`, `mark_delivered` /
   `take_piggyback`, `stats_json`'s queue block, and the four new probe tests.
3. **Part A, at `cargo test`**: four mutations applied to a clean tree and reverted, plus one
   instrumentation pass, to establish which invariants are pinned rather than asserted.
4. **Part B, live at the release binary**: `--features store-sqlite,embed-bge` driven over
   raw stdio JSON-RPC against the rig's llama.cpp BGE-M3 q8_0 on CPU (`127.0.0.1:8080`,
   `{"status":"ok"}`, verified before and during). A throwaway detached worktree at `869b898`
   with its own target dir for the parent binary, verified parent-only by string absence.
   Twenty-six sessions: four HEAD 512 B bursts, four parent 512 B bursts, a 32 B control, a
   refusal ladder, a lane-count sweep from 2 to 32 agents, receipt-state-after-restart runs,
   `probe_optimism` runs at 512 / 1024 / 1500 B, and two constructed inflation runs. Store
   counts read back with `sqlite3` rather than taken from the payload.
5. All three gates re-run from scratch at HEAD, **tallied repo-wide across all fourteen
   binaries** rather than off the lib line, plus `cargo fmt --all -- --check` and
   `cargo clippy --all-targets -- -D warnings` on two feature sets. Independent test
   name-set diff at parent and HEAD in separate target dirs. `verify.sh` at HEAD.
6. Register sweep run independently over the touched files, the §J3 constants table, and
   both `mcp.mdx` mirrors.

**Order of authority.** §J3 is the claim; the source is what ships; the *binary against the
live embedder* outranks both. Both of this round's blocking findings come from the binary.

## Part A — the ten round-2 findings, adjudicated

| # | Finding | Verdict | Evidence |
| --- | --- | --- | --- |
| **J3-R2-1** (P1) | The probe's text is not the workload | **CLOSED for one agent; NOT closed for many** (see J3-R3-2) | Red reproduced at the parent across four runs: **12, 12, 35 and 44** acked writes abandoned of 50, 50, 73 and 82 acked, every close burning its whole ~2.01 s — the claimed 37 of 68 sits inside that band. Green at HEAD on the same parameters, 4/4 runs: acked 4, dropped 196, **abandoned 0**, close 279–292 ms, `lane_bound` 4, `bound` 16, `source` probe. The 32 B control green both sides. Both halves are load-bearing and each at its own size band — see the mutation table |
| **J3-R2-2** (P2) | A fast-failing embedder biases the rate upward | **CLOSED at source, NOT closed in effect** (J3-R3-1) | `spawn_worker` really does gate on `if outcome.is_ok()`, with the argument stated one step further than the fenced case, and `a_failed_write_is_never_sampled_into_the_observed_rate` is a genuinely good test: eight stopword-only derives fail, `source` stays `Probe`, then four real concepts flip it to `Observed` at a rate that reflects the 100 ms writes. Failure-speed is distinguished from work-speed exactly as claimed. **But an embedder failure does not arrive as `Err` on the shipping path** |
| **J3-R2-3** (P2) | The `[~]` box presented three limits as complete | **CLOSED for what round 2 named; still not complete** | The box is rewritten to four limits with the completeness claim dated ("complete as of round 2"). Limit (3) is restated at its real magnitude with the arithmetic right: 4 writes per lane each slower than a quarter of a 2 s budget is 4 × 500 ms = the whole budget. Limit (4) is new, found by re-reading the fix rather than the review, and its mechanism is correct. The throughput cost is in the box and I reproduced its headline number exactly (4 acked of 200). J3-R3-2's magnitude is not there, and limit (4)'s is understated — J3-R3-6 |
| **J3-R2-4** (P2) | The two rates were both published and never compared | **CLOSED, and fully** | `probe_serial_items_per_sec` survives `with_observed_serial` (it is threaded through `from_rates` explicitly, and `from_probe` seeds it with the published slower-of-two). `probe_optimism()` gates on `Observed` and on `now > 0.0`. `write_queue_probe_serial_items_per_sec` is the **15th** key — I counted 13 `write_queue_*` plus `dropped_closed` plus `receipts_retained` in `stats_json`, matching both `mcp.mdx` enumerations and the test's key list, and matching the note's "fifteen unconditional keys (ten at first landing; round 1 added four; round 2 added one)". The INFO line fires **once** and carries both numbers, the ratio, both bounds and the sample count — I read it in the field at `probe_optimism=31.475` and again at `1.431`. OFF-payload discipline intact: the `ledger_*` block is still gated on `if let Some(ledger)`, untouched this round (the only `ledger` hunk in `server.rs` is inside a test), and no `ledger_*` key appears in a no-`--ledger` payload at the binary |
| **J3-R2-5** (P3) | 221 ms did not reproduce; the `observed`-by-the-end claim | **CLOSED** | Re-measured band 273.5–301.1 ms with the warmth caveat. My two independent fresh-session figures: **299.5 ms** and **315.2 ms**. The first is inside the band; the second is 4.7% above its top, which is warmth, not a wrong claim — and the claim now carries the caveat that makes that reading legitimate. The `observed` sentence is corrected to four **completed** writes, which is what the code does and what I watched happen (`samples=4` on the flip line) |
| **J3-R2-6** (P3) | Three unscoped restatements of the ordering promise | **CLOSED** | Re-swept independently. `server.rs`'s "is exactly that agent's submission order" is gone. The Done-when line (now `J-multi-client.md:2049`) repeats the scope in full rather than pointing at it, as the remediation said it would. `J-multi-client.md:1415` carries "**Scoped, at round 1, to one agent's *sequential* submissions**". And `writeq`'s §Ordering now **leads** with the scope, naming J3-R2-6 and the nine-line retraction it replaces — the structural nit fixed as suggested. No unscoped restatement survives the sweep |
| **J3-R2-7** (P3) | `probe_embedder` had no test | **CLOSED, and well** | Four tests on a `ScriptedEmbedder` with `start_paused = true`, covering the slower-of-two rule, a refused representative leg, a hanging one, and the budget asserted at each required leg in turn. Their `Leg::PerByte` reproduces the length-proportional shape the old docstring denied. Mutation-verified (M4) |
| **J3-R2-8** (P3) | Four stated-reason blemishes | **CLOSED at all four cited sites**; two siblings of item 1 survive (J3-R3-5) | The "22–25 ms" misquote is corrected at `DRAIN_PROJECTION_SHARE` with the finding named and the band re-derived against round 2's own live figures. Both `lambo_receipt` references are gone — the one surviving mention is the correction ("there is no `lambo_receipt` tool"). The six-space collapse in the `PROBE_CLAMP_RPS` guard message is gone. The keys bullet now explains all five keys added since first landing |
| **J3-R2-9** (P3) | The piggyback test did not prove removal rather than suppression | **CLOSED, and mutation-verified precisely** | The exact assertion round 2 asked for is at `tests/serve_proxy_multi_client.rs:388-396`. I built the mutant round 2 described — a `Receipts.suppress_once` set that `mark_delivered` populates and `take_piggyback` honours once, re-queueing the id — and it lands on **that assertion, at line 394, with its own message**, while the older J3-R1-9 assertion at 373 still passes. The new assertion is load-bearing against the implementation round 2 said would pass unchanged |

**Round 2's own P3 count is corrected, and correctly.** Round 2's verdict prose said "the six
P3s are advisory" where only five were numbered (J3-R2-5 through -9). The note records
"Round-2 remediation (1 P1, 3 P2, 5 P3 — all closed)". Five is right.

### The new shape, attacked

**The ceilings, and the cold-burst transition.** `from_rates` gives `Probe` and `Unmeasured`
the pair `(PROBE_LANE_CEILING, PROBE_AGGREGATE_CEILING)` = (4, 16) and `Observed` the pair
`(WRITE_QUEUE_MAX, WRITE_QUEUE_MAX)`. On this rig `project(19.10) = 20`, so in the probe era
the **ceiling** is what binds, not the rate — the fix is structural, as claimed.

*Is the 5th write of a cold burst refused, or queued behind?* **Refused, and the receipt is
honest.** Seven derives into a cold lane: four `pending`, then three `dropped` with
"1 concept(s) were NOT written: DROPPED before it was attempted, nothing was written". The
ack is not a lie. (What the message says *about the reason* is J3-R3-4.)

*Does a cold multi-agent burst reach `observed` before the aggregate ceiling starves lanes?*
It reaches `observed` — 16 lanes × 5 derives, then a 3 s settle, and the flip line fires. And
no lane is starved below its floor: the aggregate ceiling spreads exactly 16 admissions over
16 lanes, **1 each**, with 64 refused. That is a throughput cost in the declared direction.
But the sixteen it admits do not drain — J3-R3-2.

**The representative leg, and the budget split.** The arithmetic re-derives exactly. One
`deadline` at the top; `representative_deadline = representative_started +
deadline.saturating_duration_since(representative_started) / 2`, i.e. half of what is *left*,
with the concurrent leg keeping the original `deadline` and therefore the other half. Their
test claims a hang cannot starve the required leg; I ran it and then instrumented it to read
the boundary: warm-up 35 ms + serial 35 ms = 70 ms spent, remaining 4930 ms, half 2465 ms,
concurrent leg 35 ms — **elapsed 2.57 s**, exactly `70 + 2465 + 35`. The fallback is correct
too: `serial` falls back to the short figure (28.57/s) and the concurrent leg to `PROBE_TEXT`,
landing `lane_bound` 4, `bound` 16. At the degenerate boundary the behaviour stays honest: if
the required legs have already consumed the budget, `saturating_duration_since` yields zero,
the optional leg times out immediately and the concurrent leg's own `timeout_at(deadline, …)`
fires at once, giving `Unmeasured` rather than a number. Mutation-verified (M4).

**The HTTP-500-at-1536 B fact, at the rig.** Verified, one probe per size:

| input | http | time |
| --- | --- | --- |
| 1024 B | **200** | 78.8 ms |
| 1280 B | **200** | 89.4 ms |
| 1536 B | **500** | 2.2 ms |
| 2048 B | **500** | 1.8 ms |

So `PROBE_TEXT_BYTES = 1024` really does sit under the smallest measured refusal, and the
2.2 ms refusal independently confirms the "~2 ms, 30× faster than a write it accepts"
premise under J3-R2-2 (36× on my numbers). The constants' own length table also reproduces:
they record 1024 B at **60.0 ms** and I measure a median of **60.2 ms**; they record the
512 B embed at **36.3 ms** in `DRAIN_PROJECTION_SHARE`'s re-derivation and I measure
**36.4 ms**.

**`MEASURED_LOCAL_EMBEDDER_RPS` deliberately staying 141 — the argument holds.** The guard is
`PROBE_CLAMP_RPS > 3 * MEASURED_LOCAL_EMBEDDER_RPS`, i.e. 1024 > 423. Its job is to keep the
clamp clear of a real embedder's throughput, so the constant must be the **largest** real
rate; substituting ~20 would make the assertion `1024 > 60`, which is *easier* to satisfy.
The direction is exactly as claimed at source: lowering it loosens the guard while looking
like an update. Keeping 141 with the reason stated beside it is the right call, and the
docstring says so in terms.

**The `~78%` re-derivation** (§J3's constants table and `DRAIN_PROJECTION_SHARE`): embed
36.3 ms, whole of `run` ~64 ms, so the remainder is 27.7/36.3 = **76.3%**. Stated as "~78%",
which needs `run` ≈ 64.6 ms. Inside the tilde and inside my own measurement noise; the
load-bearing half of the claim — that the remainder is ~4× what the old "~1/5" implied and
that half the budget still covers it — is right.

### Mutation testing

Every mutant applied to a clean tree and reverted; `src/writeq.rs` md5 unchanged throughout.

| mutant | caught by | verdict |
| --- | --- | --- |
| **M1** `mark_delivered` a no-op | J3-R1-9's own assertion (`:373`) | too crude to discriminate — see M1c |
| **M1b** `mark_delivered` re-pushes to the back | `:373` again (`take_piggyback` drains the queue) | still not the shape round 2 described |
| **M1c** a `suppress_once` set: suppressed for *this* call, re-queued after | **`:394`, the new J3-R2-9 assertion, alone and by name** | the assertion is load-bearing against precisely the implementation round 2 said would pass |
| **M2** the probe-era ceilings → `WRITE_QUEUE_MAX` | the invariant test **plus four others** | red reads "at 8192-byte concepts a clean close abandoned **19 of 24** ACKED writes" — their claimed figure, to the write |
| **M3** M2 plus `from_probe` ignoring the representative leg | the invariant test at the **512 B** parameterisation | red at "**194 of 262**" (they reported 299 of 368; the acked depth is wall-clock-dependent in this test, the property is not). So each half is load-bearing at its own size band: ceilings removed fails only past ~1 KiB, both removed fails at 512 B |
| **M4** the optional leg gets the whole budget | `a_hanging_representative_leg_leaves_the_concurrent_leg_its_budget`, **alone** | the split is pinned; the catch comes through the `source == Probe` assertion rather than the loose elapsed one, which is the right way round |

### The numbers, reconciled repo-wide and by name

| Gate | Parent `869b898` | HEAD claimed | HEAD (I measured) |
| --- | --- | --- | --- |
| `cargo test --all --features fixtures` | 891 / 0 / 3 | 898 / 0 / 3 | **898 / 0 / 3** |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 979 / 0 / 3 | 986 / 0 / 3 | **986 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | 560 / 0 / 0 | 564 / 0 / 0 | **564 / 0 / 0** |
| `scripts/observability/verify.sh` | 46 ok | 46 ok | **46 ok, ALL CHECKS PASSED** |

Tallied across all fourteen binaries in each gate, zero non-`ok` results.
`cargo fmt --all -- --check` clean; `cargo clippy --all-targets -- -D warnings` clean on
`store-sqlite,fixtures` and on `--no-default-features store-cockroach,embed-fixture`.

**Name-set diff** (`--list` at both commits, separate target dirs): 894 → 901 unique names,
**+7 added, 0 removed, 0 renamed, 0 de-ignored** — exactly the claim, and the counts close on
both sides (894 − 3 ignored = 891, 901 − 3 = 898). The seven:
`a_burst_of_concepts_larger_than_the_probes_text_still_drains_at_a_clean_close`,
`a_failed_write_is_never_sampled_into_the_observed_rate`,
`the_probes_serial_figure_survives_the_observed_rate_that_replaces_it`,
`the_probes_serial_figure_is_the_slower_of_its_two_input_sizes`,
`a_refused_representative_leg_falls_back_to_the_short_one`,
`a_hanging_representative_leg_leaves_the_concurrent_leg_its_budget`,
`a_probe_that_cannot_finish_inside_its_budget_reports_no_measurement`.

**The two changed tests, and their stated reasons.** Both check out.

* The stats-payload test gains `write_queue_probe_serial_items_per_sec` in its key list and a
  new assertion that the probe figure and the rate in force are the **same number** while the
  source is `probe` — additive, and it pins the seam J3-R2-4 turns on.
* The I1 ledger test (`AGENTS = 8`, `PER_AGENT = 60`) gains a four-write warm-up so the day
  is admitted against an `observed` bound rather than the probe's. **It does not weaken what
  the test pins.** `total` is updated to `AGENTS * PER_AGENT + WARM`, so "one line per call,
  none torn" now covers 484 calls instead of 480; the warm-up asserts each of its own writes
  `applied`; and a *new* assertion requires `bound_source == "observed"` before the burst. Its
  stated reason is accurate down to the detail I went to check — the comment says "every call
  below is `agent-a`, so they share one lane", and the burst body really does send
  `"agent_id": "agent-a"` from all eight tasks.

**`sample/calls.jsonl` is byte-identical** to the parent (blob `795935f0…`, unchanged from
round 2), as is `make_sample.py` (`b5bd6848…`). Nothing under `scripts/` changed at all in
these three commits, so `verify.sh`'s 46 is inherited rather than re-earned — and true.

**Both `mcp.mdx` mirrors are passage-identical**: I diffed the two change bodies against each
other and they are byte-identical. The prose is also the right prose — it explains the
probe-era shallow queue to a model in the model's own terms ("That is why the first burst of
a fresh session may be refused where a later one of the same size is accepted") and explains
what a `probe_optimism` above one means without using the word.

## New findings

### J3-R3-1 (P1) — the observed rate samples writes that never embedded: 326 and 361 acked writes abandoned at a clean close, at the release binary

`spawn_worker` samples only successes, and says why:

> "**And neither is a failure** (J3-R2-2): … A write that fails — **embedder error**,
> `HYBRID_IO_TIMEOUT`, a store error — fails FAST, and sampling it says 'this deployment
> retires work quickly' on the evidence of work it did not retire." — `src/writeq.rs:2179-2192`

The filter is `if outcome.is_ok() { observed.lock().sample(…) }`. It is correct about what it
excludes and wrong about what reaches it: **on the shipping path an embedder error is not an
`Err`.** The hybrid derive applies the concept anyway, with a null vector, and returns `Ok`.

Verified at the store, not inferred. Four derives at four sizes through one release-binary
session, then `sqlite3` on the same file:

```
length(content) | embedding
          1024  | NULL
           512  | len=12743
          1024  | len=12747
          1500  | NULL
```

Both null-vector concepts were reported to the caller as `applied — derived 1 concept(s):
1 created, 0 matched existing`, with `warnings: []`. The receipt says nothing about the
missing vector, and neither does the ack.

**What that does to the estimator.** A write that skips a 60 ms embed retires in ~3 ms, and
that 3 ms is an `Ok`, so it is sampled. Two runs, release binary, live embedder, six 1500 B
derives (all six land with `embedding IS NULL`, confirmed in the store) followed by a 400-deep
burst of 512 B concepts that *do* embed:

| | after the six | burst | close | store | **abandoned** |
| --- | --- | --- | --- | --- | --- |
| run 1 | `serial` **364.3/s**, `lane_bound` **365**, `source` observed | 365 acked | 2034.6 ms | 39 of 365 | **326** |
| run 2 | `serial` **852.3/s**, `lane_bound` **853**, `source` observed | 400 acked | 2056.6 ms | — | **361** |

That is larger than round 1's P1 (61 of 80) and round 2's (37 of 68), in the same units, on
the clean path, with no adversary. And it lands in the **one era with no ceiling**: the
`Observed` row of `from_rates` is clamped to `WRITE_QUEUE_MAX` = 1024, because observation is
"the only source that sampled the workload". Here observation sampled work that was not done.

**The diagnosis is already in the payload and nothing acts on it.** `probe_optimism()` read
**0.022** and **0.052** on those two runs — the observed rate 20× to 45× *faster* than the
probe's. That value is prima facie impossible for a real deployment, because the observed
figure times the whole of `WriteCtx::run`, which strictly contains the embed the probe timed;
an observed rate far above the probe's serial rate is evidence of **non-work**, not of speed.
J3-R2-4 built exactly the signal that says so, and the INFO line even prints it. Nothing
refuses to believe it.

**Why this is P1.** It is the founding hazard of the workstream, at the largest magnitude any
round has measured, reached on the clean path at the release binary against the real
embedder, on content sizes inside the product's own declared 700–1500 B band. It is also the
finding that reopens J3-R2-2: the P2 was graded on this exact mechanism ("an embedder that
fast-fails a burst inflates the bound, then comes back and services the inflated queue at its
real rate"), and the fix closes the `Err` door while the shipping path uses the `Ok` one.

**Why it is narrower than it looks, stated plainly.** (a) The receipts are honest — I read
them back after restart and all sixteen tracked ids answered `restart_lost` with "the write
may or may not have been applied", so the invariant *as worded* survives; it is the
test-pinned form (`abandoned == 0`, `applied == accepted`) that does not. (b) It needs the
embedder to be refusing some inputs — but that is a *measured* condition on this rig, not a
hypothetical, and the refusal is silent to the caller. (c) The EWMA does come back, in ~4
samples — after the burst has already been admitted, which is the whole point.

**Remediation, cheapest first.** (a) Sample only writes that actually embedded — the summary
already distinguishes them, and a vector-less apply is not evidence about the pipeline for
the same reason a fenced refusal is not. (b) Or refuse to believe an observed rate that
exceeds the probe's serial rate by more than `DRAIN_PROJECTION_SHARE`, since `run` contains
the embed and cannot legitimately be that much faster — that is `probe_optimism()` used as a
guard rather than as a report, and it needs no new measurement. (c) Or keep a ceiling in the
`Observed` era too, sized from the probe's concurrent leg, so no single estimator error can
reach `WRITE_QUEUE_MAX`. And either way extend
`a_burst_of_concepts_larger_than_the_probes_text_still_drains_at_a_clean_close` with an
embedder that returns `Ok` fast — the existing `ScriptedEmbedder` needs one new `Leg`.

Separately and outside J3: a concept applied with a null vector is unfindable by semantic
recall, and neither the ack, the receipt, nor a warning says so. That is pre-existing hybrid
behaviour, not something these commits touched, but J3 is what made it load-bearing. Filed as
a residual below.

### J3-R3-2 (P1) — `PROBE_AGGREGATE_CEILING` admits sixteen writes that sixteen lanes cannot drain: 13 of 16 abandoned, from eight concurrent agents up

`PROBE_AGGREGATE_CEILING = PROBE_CONCURRENCY * PROBE_LANE_CEILING` = 16. The stated reason is
the widest population `PROBE_LANE_CEILING` authorises, and `PROBE_LANE_CEILING`'s own
justification is that four writes are what it takes to *replace* the probe's word — sound per
lane: four 512 B writes in one lane retire in ~280 ms of a 2 s budget, which is what the
green runs show. **It does not survive being spread across sixteen lanes**, because sixteen
concurrent writes are not sixteen serial ones.

Release binary, HEAD, 512 B concepts, one `serve`, N agents round-robin, close immediately.
Store counts read with `sqlite3` after restart:

| agents | acked | in store | **abandoned** | close |
| --- | --- | --- | --- | --- |
| 2 | 8 | 8 | 0 | 702 ms |
| 4 | 16 | 16 | 0 | 1486 ms |
| 6 | 16 | 16 | 0 | 1812 ms |
| **8** | 16 | 12 | **3** | 2065 ms |
| **12** | 16 | 4 | **12** | 2026 ms |
| **16** | 16 | 3 | **13** | 2044 / 2046 / 2051 ms (3/3 runs) |
| **24** | 16 | — | **13** | 2006 ms |
| **32** | 16 | — | **13** | 2045 ms |

Every abandoning row burns the whole drain budget. `source` is `probe` throughout — nothing
completed, so nothing retired the estimate.

**The mechanism, measured.** This rig's aggregate throughput at sixteen concurrent lanes is
~3.7 items/s (sixteen writes retired in ~4.3 s across a settle plus a close), against the
probe's 4-wide reading of 19.1–21.3 items/s. A **~5× fall**. Sixteen outstanding needs >4 s;
`close()` waits 2 s.

**Where the declaration falls short.** Limit (2) of the `[~]` box says nothing measures
throughput past four lanes, "so the aggregate bound assumes throughput does not *fall* as
lanes grow past four". That names the assumption, and the assumption is false on this rig by
5×. What the box does not say is the **consequence**: not a loose bound, but thirteen acked
writes abandoned at a clean close from eight agents up, in a box whose closing sentence is
"None of the four limits is a burst that degrades invisibly, which is what the box is for".
Limit (3) can be read to cover it arithmetically (four per lane, capped by the aggregate) but
its stated condition is a write "slower than a quarter of the drain budget", and these writes
are 70 ms in isolation — slow only because of concurrency **the queue itself authorised**.
The measured cost the box quotes is single-agent throughout.

**Where the test coverage falls short.**
`a_burst_of_concepts_larger_than_the_probes_text_still_drains_at_a_clean_close` parameterises
**content size** (512 B and 8192 B) and uses **one agent** — `AgentId::new("agent-a")` for
the whole burst. Round 2 asked for a second parameterisation whose per-job service time
exceeds the probe's; it got one, over the axis round 2 named. The lane-count axis, which is
the axis the aggregate leg exists for, is unpinned.

**Attribution, stated plainly.** The remediation declined to re-derive the aggregate bound,
recording the decline as "a redesign no finding asked for, with limit (2) already declaring
the assumption it rests on". That is a fair reading of round 2, which had affirmatively
cleared this direction in *attacks that did not land*: "Structurally the lane-*count*
direction is safe (a fixed aggregate spread over more lanes gives each a shallower queue);
only sublinear total throughput is exposed, which is exactly what is declared. **No
finding.**" That reasoning holds only while a lane bound is above its floor. At
`WRITE_QUEUE_LANE_MIN = 1` the aggregate is the *sole* guard, and it is a 4-wide figure that
is never re-measured — `with_observed_serial` keeps `self.items_per_sec` for the life of the
session, so there is no observed aggregate, ever. The clearance was wrong and I am
withdrawing it.

**Remediation, cheapest first.** (a) Divide the aggregate ceiling by the active lane count
rather than fixing it at `PROBE_CONCURRENCY × PROBE_LANE_CEILING` — the population that
matters is what the drain retires, and the drain's width is the number of live lanes, not
four. (b) Or observe the aggregate: the workers already time every `run`, and lanes already
know how many are running, so a concurrency-adjusted rate is available from data the
pipeline holds. (c) Or, cheapest and honest rather than correct, cap the aggregate at
`PROBE_LANE_CEILING` while the source is `probe` and accept the throughput. Then add a
lane-count parameterisation to the invariant test — my sweep is a starting point, and the
threshold to bracket is eight.

### J3-R3-3 (P2) — the `probe_optimism` "fell to 1.14×" claim does not reproduce, and 1024 B is not the top of the band the note itself declares

§J3 records the representative leg's benefit as "measured `probe_optimism` fell from 4.0× to
**1.14×**". At 512 B, single agent, release binary, two fresh sessions, I read the flip line
directly: **1.4308** and **1.4312**. Not noise — the same number twice, and the note's own
figure is 25% optimistic about the remaining margin.

The arithmetic says it should be. The probe's serial leg now times a 1024 B **embed**
(60.2 ms measured, 20.4/s at the binary); a 512 B **write** is the whole of `run`
(70 ms, 14.2/s). 20.4 / 14.2 = 1.43. The claimed 1.14 would need the observed rate at
~17.9/s, which is a 56 ms `run` — faster than I could measure on this rig at 512 B.

This matters past bookkeeping, because 1.43× is the figure a reader checks against
`DRAIN_PROJECTION_SHARE = 2`. It is still under 2, so the *argument* survives — but the
headroom is 1.4× rather than the 1.75× the note implies, and the trend is the wrong way at
the sizes the note itself nominates. Extrapolating the same two measurements to the top of
the declared band, a 1500 B `run` on this rig is ~90–130 ms (7.7–11/s) against a probe
reading 20.4/s, i.e. **1.9× to 2.7×** — at or through the half-share. The ceilings cap the
probe-era exposure at four writes per lane, so this is limit (3) and not a new hazard; but
`PROBE_TEXT_BYTES = 1024` is the *bottom* of "700 to 1500 bytes", not its middle, and the
note reads as though it were representative of the whole band.

**Remediation.** Re-measure and quote `probe_optimism` as a range with the content size it
was measured at, the way J3-R2-5's first-ack figure is now quoted; and either say that
`PROBE_TEXT_BYTES` covers the lower half of the declared band, or set it nearer 1280 B, which
this rig answers in 89.4 ms and which is still under the smallest measured refusal.

### J3-R3-4 (P3) — the refusal message attributes the bound to a rate, in the era where the ceiling decides

`DropReason::describe` is unconditional:

> `LaneFull` → "this agent's background write lane is full ({bound} outstanding, **a bound
> measured at the rate one lane drains at on this deployment's embedder**)" —
> `src/writeq.rs:936-946`

In the probe era — the regime this remediation *creates*, and the one every cold burst starts
in — that is false. `project(19.10) = 20`, and the bound is 4 because `PROBE_LANE_CEILING`
clamped it. An operator or agent reading `lane_bound: 4` beside
`serial_items_per_sec: 19.10` and this sentence has no way to reconcile them, and is pointed
at the embedder when the answer is "wait 250 ms". `QueueFull`'s "a bound measured on this
deployment's embedder" has the same problem at 16. `LaneFull`'s own docstring ("the per-lane
count bound, derived from the serial rate its single consumer drains at") carries it too.

Measured: **196 of 200** refusals in the HEAD 512 B green run carried that sentence, and
64 of 80 in the 16-lane run.

Two things keep this a P3 rather than a P2. The WARN log line carries `source="probe"` and
both bounds as fields, so a log reader can tell. And both `mcp.mdx` mirrors now explain the
real mechanism to the model in the right words. It is the *receipt* — the surface this
workstream nominates as authoritative about a write's fate — that states a reason the code
does not act on.

**Remediation.** One `match` on the source in `describe`: name the ceiling and the four
completed writes that lift it while the source is `probe`/`unmeasured`, and keep the present
sentence for `observed`. The text to reuse already exists in `mcp.mdx`.

### J3-R3-5 (P3) — the misquote is fixed at the line the review cited and survives at three siblings

J3-R2-8 item 1 is closed thoroughly at `DRAIN_PROJECTION_SHARE`, with the finding named and
the correct band quoted. The identical misquote survives three times, twice in the same file:

* `src/writeq.rs:7` — the **module header**: "a warm `derive` is 27 ms of which 22 to 25 ms is
  the embedding call".
* `src/writeq.rs:302` — `PROBE_CLAMP_RPS`: "≈40–45 items/s serial, from the same 22–25 ms
  embed".
* `dev-diary/lambo-for-mooshik/J-multi-client.md:1308` — the note's own prose.

§Measurements (`J-multi-client.md:1917`) says **22 to 27 ms**. This is the register-sweep
class in its purest form: the cited line was corrected and its siblings were not, in a sweep
whose stated scope was "every numeric claim and stated reason in the constants block".

A second, more consequential blemish at the same site. `PROBE_CLAMP_RPS`'s docstring is
explicitly the operator's reference for reading `write_queue_serial_items_per_sec` — "the
number an operator needs to read `write_queue_serial_items_per_sec` against" — and every
figure it offers is a 35-byte figure (110–141 items/s 4-wide, ≈40–45 serial). Since this
round, `serial_items_per_sec` is the slower-of-two and reads ~18–21 on this rig. The key's
meaning changed and its reference figures did not follow.
`MEASURED_LOCAL_EMBEDDER_RPS` does carry the 1024 B numbers; `PROBE_CLAMP_RPS`, which is
where an operator is sent, does not.

### J3-R3-6 (P3) — limit (4) inherits limit (3)'s magnitude, and its own is up to 256× larger

Limit (4) is a good find, honestly credited to re-reading the fix rather than the review. Its
magnitude sentence is "Bounded the same way: **at most a lane's worth** before the average
follows". True, and misleading in context: limit (3) two sentences earlier pins a lane's
worth at `PROBE_LANE_CEILING = 4`, but limit (4) is a property of the **`Observed`** era,
where a lane's worth is `project(observed)` clamped to `WRITE_QUEUE_MAX`. On this rig that is
15 (`lane_bound` 15 on both ordinary observed 512 B sessions I measured, against an aggregate
`bound` of 22); in principle it is 1024. So a reader carrying limit (3)'s number forward
under-counts limit (4) by ~4× here and by up to 256× at the clamp.

The box earned its credibility this round precisely by restating limit (3) "at its real
magnitude" after round 2 caught it understating. The same standard applies to limit (4).

**Remediation.** State it as the observed lane bound, with the rig's figure and the
`WRITE_QUEUE_MAX` ceiling, the way limit (1) states `PROBE_CLAMP_RPS`.

## Attacks that did not land

* **Does an `Err`-heavy workload starve observation?** Yes, and it is the safe direction, so
  no finding. Their own test demonstrates it: eight consecutive failures leave `source` at
  `probe` and the ceilings (4 / 16) permanently in force. The cost is throughput, and it is
  visible in the two places it should be — `write_queue_bound_source` stays `probe` and
  `write_queue_failed` climbs. Worth one sentence in the box, not a finding. (The dangerous
  cousin — an `Ok`-heavy workload that starves observation of *real* work — is J3-R3-1.)
* **Can the aggregate leg be the reason a lane is refused below its own bound?** No.
  `from_rates` ends with `.max(lane_bound)` on the aggregate, with the reason stated, and I
  confirmed the ordering: `admit` checks the lane leg first.
* **Can the source flap `observed` → `probe`?** No. `samples` only `saturating_add`s and the
  gate is a `<` on a monotone counter. Still one-way.
* **Can a hanging optional leg cost the probe its measurement?** No — half the remaining
  budget, verified at 2.57 s of 5 s, and mutation-caught.
* **Does the probe's concurrent leg keep the length bias when the representative leg is
  refused?** Yes — `concurrent_text` falls back to `PROBE_TEXT`, so the aggregate rate is a
  35-byte rate on such a deployment, and it is never re-measured. But it cannot abandon
  writes on its own while a lane bound is above its floor, and at the floor the exposure is
  J3-R3-2, which I have filed on its own measurement rather than this hypothetical.
* **Is the OFF-payload byte-identity promise broken by the 15th key?** No. The 15th key is a
  `write_queue_*` key, deliberately unconditional with the reason stated at the code and in
  both `mcp.mdx` mirrors; the `ledger_*` block's `if let Some(ledger)` gate is untouched, and
  a no-`--ledger` payload at the binary carries no `ledger_*` key.
* **Does the flip line fire more than once?** No. One line per session across every run,
  carrying both rates, the ratio, both bounds and `samples=4`.
* **Their single-agent 512 B control**: reproduced 4/4 at HEAD — 4 acked, 0 abandoned, close
  279–292 ms — and the 32 B control likewise. The claim "4 acked of 200 where the parent
  acked 68 and lost 37 of them" is honest in both halves.

## Verdict

**REQUEST_CHANGES**, blocking **J3-R3-1** and **J3-R3-2**.

Ten of ten round-2 findings closed at the artifact, several of them beyond what the review
asked for: the mutation pair returns their exact numbers, the piggyback assertion survives
the specific mutant round 2 said would defeat it, the four new probe tests close the last
untested load-bearing function, and every gate figure and re-measured constant reconciles.
The founding invariant now holds at the binary **for one agent, at every content size I
tried** — which is a real advance over both previous rounds, and the parent comparison shows
it: 61 abandoned at sixteen lanes becomes 13, and 20 becomes 3.

It does not hold for the two cases the fix's own shape opens. Sixteen agents at 512 B lose
thirteen of sixteen acked writes at a clean close, from eight agents up, because the
aggregate ceiling was derived per-lane and applied across lanes. And a session whose embedder
silently refuses some inputs inflates its own lane bound twenty- to forty-fivefold, because
the estimator's exclusion filter tests for `Err` on a path that returns `Ok` — and loses 326
of 365. Both are on the clean path, at the release binary, on content the product writes.

J3-R3-3 is worth closing in the same pass as J3-R3-1, since both are about what the
estimator is allowed to believe. J3-R3-4 through J3-R3-6 are advisory.

---
*Verification-only edits: five mutants and one instrumentation pass in `src/writeq.rs`, each
applied to a clean tree and reverted. `src/writeq.rs` md5
`6a3a7b179e1359c7583b3416f3ac9a43` matches `git show HEAD:src/writeq.rs`; `git diff HEAD --`
reports no change to any tracked file but this review. A throwaway detached worktree at
`869b898` with its own target dir, removed. No commit amended.*
