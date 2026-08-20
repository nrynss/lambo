# Adversarial review — mooshik J3, round 1

**Reviewer**: independent adversarial reviewer (Opus 5), agent_id `j3-reviewer-r1`. Wrote
nothing under review.
**Scope**: the five staged commits `166a3c8..528ade6` on `wt/j3` — `427fabf` (the write
pipeline), `dcf29de` (the MCP ack / piggyback / receipt surface), `f9abfbb` (the tests),
`3e1bea4` (the two defects found by measuring the binary), `528ade6` (docs and register
sweep). Against §J3 of `J-multi-client.md` — the rule, the three-part shape, the five
constraints, and the amended Done-when boxes.
**Worktree**: `/Users/narayan/Documents/work/lambo/.claude/worktrees/j3`, branch `wt/j3`.
No commit amended; tree left clean but for this file.
**Verdict**: **REQUEST_CHANGES** — one **P1**, four **P2**, eight **P3**. The headline claims
are real and I verified them independently at the release binary against the live BGE-M3:
the ack is 0.039–0.057 ms median with the embedder out of the call, all 22 writes applied,
and a receipt wait restores read-your-writes. The blocking finding is not the ack; it is
the **queue bound's arithmetic**. The admission bound is a `PROBE_CONCURRENCY`-wide
throughput projection, but one agent's lane has one consumer — so a single-agent session
admits a population it cannot drain, and I measured **61 of 80 acked writes abandoned at a
clean `close()`**. That is the founding hazard of this workstream (an acked write that never
applies) reached on the *clean* path, by construction, and it falsifies the stated reason
the drain budget is one constant rather than two.

The test-set reconciliation is **clean**: the numbers in the handoff were reported off the
wrong line of `cargo test` output. Nothing was deleted or de-ignored.

## Method

1. `lambo_recall` as `j3-reviewer-r1` on "J3 async ack receipts implementation" — 25 hits
   carrying the J3 rule, the J2-R2-7/J2-R3-3 coupled residual, the queue-placement
   decision, the receipt taxonomy, the measured-ceiling re-derivation and the declared
   ledger regression. Graph read as context; adjudication is against §J3 → the commits →
   the source, in that order. **No graph↔code drift found**: every derived decision in the
   graph is reproduced in the code and the prose, including the two defect narratives and
   the correction to the residual's phrasing.
2. Read §J3 in full (`J-multi-client.md:1299-1570`) and the amended Done-when box, then
   every line of `src/writeq.rs` (2442 lines), the `src/memory.rs` and `src/graph/*`
   diffs, the `lambo_stats` receipt handler and the `answered` piggyback wrapper.
3. **Independent test-set reconciliation**: `cargo test -- --list` and
   `--list --ignored` for all three profiles at HEAD *and* at the parent `166a3c8` in a
   separate detached worktree with its own target dir, then name-set diffs. Every
   add/remove accounted for by name.
4. All three gates re-run from scratch at `528ade6`, plus `cargo fmt --all -- --check` and
   `cargo clippy --all-targets -- -D warnings` on four feature sets.
5. **Two executable attacks**, written into `pipeline_tests`' own idiom (a `GraphStore`
   shim advertising `VECTOR_SEARCH` so `hybrid::derive` actually embeds — the same
   `VectorCapable` trick the implementor's own `the_ack_lands_before_the_embedder_is_called`
   uses, and for the same reason), run, recorded, then **reverted**. `src/writeq.rs` is
   byte-identical to `528ade6` (md5 `4eac858e9cbc66acdfa0b48980c36ec5` before and after).
6. **Part B, live at the binary**: release build (`--features store-sqlite,embed-bge`)
   driven over raw stdio JSON-RPC against the rig's live llama.cpp BGE-M3 q8_0 on CPU
   (`127.0.0.1:8080`, `{"status":"ok"}`) — five sessions, the latency claim, the no-op
   control, and four repeats of the calibration probe.
7. Register sweep: the "seven tools" assertions, the ten new `lambo_stats` keys, both
   `mcp.mdx` mirrors, `verify.sh` at both commits, and `dedup_rate.py` driven against a
   hand-built J3-shaped ledger line.

**Order of authority.** §J3 is the claim; the source is what ships; where a docstring and
the code disagree the code wins and the disagreement is a finding. Three of this round's
findings are exactly that.

## Part A — the three-part shape, the constraints, the deviation, the regression

### Shape

| # | Claim | Verdict | Evidence |
| --- | --- | --- | --- |
| **1** | Synchronous validation pre-pass, then ack | **HELD, but its docstring is wrong** — see J3-R1-4 | `Memory::derive_async_as` (`src/memory.rs:1387`) takes `begin_write()`, runs `hybrid::validate_limits` then a strategy-chosen graph pre-pass, opens the interaction, enqueues. `graph::derive::validate` and `hybrid::validate_limits`/`validate_graph_inputs` are genuine **extractions**, not reimplementations: `validate_graph_inputs` is still called inside `derive_planned`'s phase 1 (`hybrid.rs:470`), so **no rule was removed from the write path**. Defect 3's story checks out at source |
| **2** | Embed, canonicalize, insert in the background through the ordinary path | **HELD** | `WriteCtx::run` (`writeq.rs:849-971`) calls `hybrid::derive` / `graph::derive::derive` / `graph::action::record_action` unchanged, then `mirror_concepts` (one shared free function, lock order graph-read→index-write) and `daemon_wake.notify_one()`. Dedup unaffected: embedding still precedes insertion |
| **3** | The ack carries a receipt; delivered by piggyback and by id; doubles as opt-in synchrony | **HELD** | Eight answers, none "unknown" (`ReceiptAnswer`, `writeq.rs:500-560`); ids self-describing (`lwr1.<epoch>.<issued_ms>.<seq>`); `answered` (`server.rs:1197`) is one wrapper every tool goes through so the piggyback cannot be forgotten; `lambo_stats(receipt, wait_ms)` is the fetch and the wait. **Verified live**: wait-on-last-receipt returned `applied` and made all 22 concepts visible |

### Constraints

| Constraint | Verdict | Evidence |
| --- | --- | --- |
| **Backpressure** — bound derived from a ceiling measured on the deployment's own embedder, never a constant | **PARTIALLY HELD — J3-R1-1 (P1) and J3-R1-2 (P2)** | The probe is real and per-deployment (`probe_embedder`, 4 concurrent embeds, wall-clocked; admission *awaits* it rather than falling back to a constant). But the bound it produces is a **4-wide** projection while a lane drains **1-wide** (J3-R1-1), and the one-shot timing makes the same rig measure 21–150 items/s depending on embedder warmth (J3-R1-2). Drop policy is as specified: bound, drop, log once (`drop_logged` latch), count |
| **Per-agent FIFO**; interleaving across agents fine | **HELD, and stronger than specified** | The interaction is opened on the call path, so chain order is submission order independent of drain order. One `VecDeque` and one worker per `AgentId`, worker presence *is* liveness under the same lock an enqueue takes (`writeq.rs:1043-1052`, `1385-1412`) — the "lane emptied, worker exited, job arrived" race is genuinely closed. Minor precision issue at J3-R1-10 |
| **Receipts**: expired ≠ unknown, restart-lost ≠ unknown, per-agent scoped | **HELD** | `lookup` (`writeq.rs:1462-1479`): held→answer, foreign epoch→`restart_lost`, `seq > highest_seq`→`never_issued`, else→`expired`; other agent→`forbidden`. `RestartLost.describe()` carries "UNKNOWN" and "Recall before re-deriving" **word-for-word** with the proxy's -32002, asserted by test. Eviction is oldest-first and pinned at `MAX_RETAINED_RECEIPTS + 8`, so collapsing eviction into `expired` is honest |
| **The crash window widens but is not new** | **HELD** | `Memory::derive_as` also opens the interaction *before* deriving (`memory.rs:1283`), so an interaction with no concepts is pre-existing, not a J3 invention. J3 widens the window from ~25 ms to queue residency and says so. Note in J3's favour: the async path runs its pre-pass **before** `begin_interaction_as`, so a validation refusal leaves no orphan, where the synchronous path does |
| **`ledger_queued_lines` arithmetic re-derived, not assumed** | **HELD — and this is the best-executed part of the change** | The queue keeps its own `WriteQueueCounters` and never touches `LedgerCounters`, so the ledger's exclusivity argument is untouched. `outstanding = accepted − applied − failed` is one saturating expression serving gauge and shutdown; `abandoned` is a label on a subset of `failed`. **`outstanding_excludes_refusals_because_they_never_reached_accepted` really does assert both wrong formulas wrong** — I re-derived it: `10−4−1−10` underflows (panics unsaturated), `10−4−1−1 = 4 ≠ 5`. I re-derived the right formula from the counter sites myself and it is the one in the code. See J3-R1-8 for the one blemish |

### The deviation: no eighth tool

**Adjudicated: the deviation is correct and the register argument is sound.** I verified it
rather than taking it: `git grep -i 'seven tools'` at both commits is **byte-identical
line for line** except two new lines inside the §J3 note itself explaining the deviation
(65→67 occurrences, 28 files both). No `evidence/` record was rewritten.
`the_router_publishes_exactly_the_seven_spec_tools` and
`every_tool_schema_is_an_object_requiring_agent_id` both still pass, as does
`f18_tool_schemas_match_the_golden_property_set`. §J3 asks for outcomes "fetchable by id",
not for a tool, and `lambo_stats` was already the introspection surface.

Then I attacked the chosen surface, and **it holds**:

* **Can a heartbeat wait?** No. `stats_json()` is a synchronous `fn` (`server.rs:966`) and
  `heartbeat_line()` (`:1076`) calls it directly. The `receipt`/`wait_ms` path lives only
  in the tool handler (`:1985`). The blocking surface is unreachable from the timer.
* **Does the ledger line double-count?** No. `answered` → `observed(tool, …, fut)` → one
  call line per call; the wait is inside the timed future, so a 4 s wait is reported as a
  4 s `lambo_stats` call, which is accurate rather than duplicated.
* **Cross-agent fetch refused, both ways?** Yes.
  `another_agents_receipt_is_refused_through_lambo_stats` asserts `forbidden` **and** that
  `created` is absent (no outcome leaks), then asserts the owner still gets `applied`.
  `the_four_non_answers_are_distinguishable` covers the same at pipeline level.
* **Is the wait bounded on both ends?** Yes, and the J2 link is a build guard:
  `RECEIPT_WAIT_MAX` (4 s) clamps duration, `MAX_CONCURRENT_RECEIPT_WAITS` (16) clamps
  population via an owned semaphore permit, and
  `const _: () = assert!(MAX_CONCURRENT_RECEIPT_WAITS * 2 <= INFLIGHT_DEPTH_WARN)` pins the
  relation. A refused *wait* still returns the current answer rather than an error — right
  call. **The residual's correction is also correct**: a non-waiting ack never enters the
  pump's `inflight` list, so the queue bound is not the burst length.
* **Defect 1's fix is real at source**: the wait resolves *before* `self.mem.stats()` and
  `self.stats_json()` (`server.rs:1985-2027`), so a self-contradicting payload is
  structurally impossible now, not just tested.

### The declared ledger regression

**Handling is acceptable, and better than the note claims.** The README states it twice —
at the fact table (`README.md:289`) and in a dedicated paragraph at the metric-2 section —
and `_ledger.py`'s schema comment carries it too. Crucially I checked the failure mode the
I-lesson warns about (a zero that reads as "no duplicates" rather than "no data"), and
**`dedup_rate.py` already refuses to fold the two together** — that discipline was built in
by workstream I. Driven against a hand-built J3-shaped derive line it prints:

```
TOTAL                2        0        0           0     n/a
   2 SUCCESSFUL derive call(s) carried NO created/matched facts at all — not the same as
   creating and matching nothing; the rates above are computed over the remaining calls only.
```

`dedup=n/a`, not `0.000`. **No misleading number is emitted.** `verify.sh` is **40 ok** at
both `166a3c8` and `528ade6`, and no sample file changed.

Deferring the completion ledger line was **the right call**: it is a schema change across
`_ledger.py`, `dedup_rate.py`, `duplicates.py`, the README and `verify.sh`, and doing it
inside J3 would mean two append paths for one tool — exactly I-round3 flip D's drift
hazard. Two blemishes, both P3: J3-R1-11 (the runtime message names two wrong causes and
not the real one) and J3-R1-12 (no J3-shaped line in the committed sample, so the gate
never sees the new shape).

## New findings

### J3-R1-1 (P1) — the bound is a 4-wide projection; one agent's lane drains 1-wide, so a clean `close()` abandons acked writes at scale

`Calibration::from_probe` (`src/writeq.rs:640-658`) computes
`rate = PROBE_CONCURRENCY / wall` — a **4-wide** throughput — and
`bound = ceil(rate × WRITE_QUEUE_DRAIN_BUDGET)`. `admit` (`:1236-1246`) compares the
**global** `lanes.outstanding()` against that bound. But `spawn_worker` (`:1379`) runs
**one consumer per `AgentId`** and `ctx.run(&job).await`s each job before popping the next,
so a single agent's jobs are strictly **serial**. `rate_1wide ≤ rate_4wide` always, and the
probe's entire justification is that the gap is large (§Measurements' 5.94× parallelism
figure).

So `outstanding < bound` does **not** imply "drains within one budget", and the
`WRITE_QUEUE_DRAIN_BUDGET` docstring's stated reason is false:

> "One constant serves both admission projection and quiesce, so a queue cannot admit more
> than shutdown will wait for." — `src/writeq.rs:113-121`

**Measured.** A 100 ms/embed embedder with unbounded concurrency (the case the 4-wide probe
rewards), `MatchStrategy::Hybrid`, one agent:

```
REVIEW: 4-wide probe measured Some(39.555055074233714) items/s -> bound 80 (1-wide would be 10.00/s)
REVIEW: admitted 80 jobs from ONE agent; outstanding=80
REVIEW: quiesce 2.002816s (budget 2s); abandoned 61 of 80 ACKED writes; applied=19 failed=61 embed_calls=24
```

**61 of 80 acked writes were abandoned at a clean close.** With the deployment's own
published numbers it is the same story: 4-wide 131.9 items/s → bound 264; 1-wide ≈ 40
items/s (§J3's own 22–25 ms embed) → 6.6 s to drain against a 2 s budget and a 2 s quiesce
≈ 200 abandoned. Reachable with no degradation and no adversary: one busy agent, default
rate limit 50/s, a clean shutdown. The receipts are honest (`failed`, "nothing was
written") and a `tracing::error!` fires — but the session is exiting, so nobody reads them.

Note this is also why the existing coverage misses it:
`close_makes_a_write_acked_just_before_it_durable` (`server.rs:3479`) uses `MemoryStore`
(no `VECTOR_SEARCH`, so no embed) and **one** job, and
`quiesce_settles_everything_it_could_not_apply` accepts either outcome. Queue *depth* at
close is untested.

**Remediation.** Make the projection match the drain. Any of:
(a) size admission per lane rather than globally — admit while
    `lane.len() < ceil(rate_1wide × DRAIN_BUDGET)`, deriving `rate_1wide` from a 1-wide
    probe leg (the probe already runs 4 embeds; time one of them separately);
(b) make the drain as wide as the measurement — a bounded `Semaphore(PROBE_CONCURRENCY)`
    of workers per lane would break FIFO, so instead keep one worker per lane but bound the
    global admission by `ceil(rate_4wide × DRAIN_BUDGET) / expected_lanes`, with
    `expected_lanes = 1` as the safe floor; or
(c) cheapest and honest: divide the projection by `PROBE_CONCURRENCY` —
    `bound = ceil(rate / PROBE_CONCURRENCY × DRAIN_BUDGET)` — and say at the constant that
    the bound is sized for the worst case of a single active lane.
Then add a test that submits `bound` jobs on **one** agent behind a slow embedder and
asserts `quiesce()` returns 0. My harness (in this review's history, reverted) is a
starting point.

### J3-R1-2 (P2) — the calibration probe measures embedder *warmth*, not the deployment's ceiling: a 7× swing on one machine

The probe fires once, at session build, before any real traffic — the coldest moment in the
process's life — and is never retried. Four consecutive runs of the release binary against
the *same* live llama-server on this machine:

| run | `write_queue_items_per_sec` | `write_queue_bound` | `write_queue_measured` |
| --- | --- | --- | --- |
| first (server idle) | **21.20** | **43** | `true` |
| second | 101.03 | 203 | `true` |
| third | **150.23** | **301** | `true` |
| fourth | 134.66 | 270 | `true` |

**A 7× swing in the load-bearing number and a 7× swing in the bound**, same binary, same
embedder, same host. The hot runs reproduce §J3's table well (128.8 → 258 in a fifth run,
against the note's 131.9 → 264), so the *documented* figures are honest — but "110–141
across repeats" omits the cold end entirely, and the cold end is what a real session build
will often hit. A session that starts while the embedder is cold runs its **whole life** on
bound 43 instead of ~270, dropping acked writes at one sixth of the intended burst
capacity, with `write_queue_measured: true` asserting the number as a measurement.

The Done-when box's tilde names two honest limits — probe *failure*, and an embedder above
`PROBE_CLAMP_RPS`. It does not name this one, which is the likeliest of the three: a probe
that **succeeds** against a cold embedder. Nothing in the payload distinguishes "21 items/s
is the ceiling" from "21 items/s is the model loading".

**Remediation.** Either (a) re-probe: keep the one-shot result as an initial bound and
re-measure from real write latencies as they accrue (an EWMA over `WriteCtx::run`
durations is free — the worker already has the timings), replacing the probe's figure once
*n* real writes have been seen; or (b) at minimum, discard the probe's first embed as a
warm-up (`PROBE_CONCURRENCY + 1` embeds, time the last `PROBE_CONCURRENCY`) and add a
`write_queue_bound_source: "probe" | "observed"` key. Either way the tilde's honest-limits
list needs this third case added.

### J3-R1-3 (P2) — `expired` is reachable for a job that is still running, and the outcome is then silently dropped

`RECEIPT_RETENTION`'s build guard (`writeq.rs:263-271`) asserts
`RETENTION > HYBRID_IO_TIMEOUT + WRITE_QUEUE_DRAIN_BUDGET` and states the conclusion:

> "…so a receipt could expire while its own write is still running" — i.e. the guard exists
> to make that unreachable.

It does not, because `Receipts::expire` keys on **issue** time (`:891-908`) and
`WRITE_QUEUE_DRAIN_BUDGET` is a *projection* of queue residency, not a bound on it — which
is J3-R1-1's root cause showing up a second time. Nothing caps how long a job sits in a
lane. **Measured** (clock injected through the rig's own `Clock`, job parked in the
embedder, `outstanding=1`):

```
REVIEW: outstanding=1 embed_calls=5; answer for a RUNNING job = "expired"
```

Worse, `settle_one` (`:1703-1713`) calls `r.expire(now)` **before** `entries.get_mut(id)`,
so when the write eventually applies its outcome is **silently discarded** — the counters
move but no receipt records it. Reachable in wall-clock terms whenever the probe measured
hot and the embedder later degrades (bound 264 × ~1.5 s/embed = 396 s > 300 s), which is
precisely the one-shot-probe scenario J3-R1-2 describes.

The saving grace, and why this is P2 not P1: `Expired.describe()` says "recall to see
whether the write is there", which is honest. What is false is the guard's stated claim.

**Remediation.** Never expire an unsettled receipt: in `Receipts::expire`, skip entries
whose `answer.is_settled()` is false (the `MAX_RETAINED_RECEIPTS` eviction bound already
protects the count side, and `WRITE_QUEUE_MAX ≤ MAX_RETAINED_RECEIPTS/4` guarantees the
outstanding set fits). Then correct the guard's docstring to say what it actually proves —
that a *promptly started* job cannot outlive retention — and pin the new invariant with a
test.

### J3-R1-4 (P2) — `derive_async_as`'s docstring contradicts itself and the code, on exactly the point defect 3 was about

`src/memory.rs:1362-1366`:

> "* The **validation pre-pass** … It is `crate::graph::derive::validate`, the read-only
> half of the synchronous path — **the same checks, run against the same graph, in the same
> order**."

For the **default** strategy this is false. `config.rs:167` sets
`match_strategy: MatchStrategy::Hybrid`, and the code four lines below the bullet runs
`hybrid::validate_limits` + `hybrid::validate_graph_inputs` — a strictly **smaller** rule
set that deliberately omits the repeated-`Observation` and single-`Hierarchical`-parent
rejections. The inline comment says so explicitly. So the docstring asserts the very thing
defect 3 was recorded for *not* being true, three lines above the comment correcting it.

This is the J2 false-stated-reason family, and it matters because a reader sizing "what
still fails at call time" will get the answer wrong for the default configuration.

**Error classes that moved from at-call to after-ack** (Hybrid; enumerated as the brief
asked): embedder failure and embedder dim/contract mismatch; `HYBRID_IO_TIMEOUT` expiry;
`MAX_HYBRID_REPLANS` exhaustion; store errors from `vector_candidates_checked`; the
`EmbeddingContract::ensure_compatible` mismatch against the session's stamp. All five need
the embedder or the store, so moving them is *correct* — the pre-pass keeps every check a
caller can act on. For `Canonical` nothing moves. **No rule was lost from the write path.**

**Remediation.** Replace the bullet with: "the pre-pass the session's `match_strategy`
actually uses — `hybrid::validate_limits` + `hybrid::validate_graph_inputs` for `Hybrid`,
`graph::derive::validate` for `Canonical`; hybrid's is deliberately the smaller set (see
defect 3)."

### J3-R1-5 (P2) — `a_burst_past_the_bound_drops_and_counts_it` does not exercise the bound, and two drop paths have no test at all

The test (`writeq.rs:2331-2372`) calls `rig.pipeline.seal()` and asserts the resulting
refusal. That is `DropReason::Closed`, not `DropReason::QueueFull`. Its own comment concedes
the substitution. Because `Closed` deliberately rides `dropped_queue_full`'s counter, the
`counters().dropped() == 1` assertion passes without the bound ever binding.

Consequences: **`DropReason::QueueFull` has no test**, and **`DropReason::QueueBytes` /
`WRITE_QUEUE_MAX_BYTES` (16 MiB, a fully derived constant with a 9 MiB worst-case
argument) have no test whatsoever** — `lanes.bytes` accounting is exercised by nothing. The
test's *name* asserts the property it skips, on a Done-when box that is already at tilde
partly for backpressure reasons.

**Remediation.** The `VecStore(MemoryStore)` shim plus a delayed embedder makes both
reachable deterministically (my harness admitted exactly `bound` and then hit
`QueueFull`); add one test per drop class, and either rename the existing test to
`a_sealed_queue_refuses_and_counts_it` or fold it in.

### J3-R1-6 (P3) — `MAX_RETAINED_RECEIPTS`' memory derivation counts one id list of two

The constant's stated basis (`writeq.rs:288-297`, mirrored in §J3's table) is "a summary
plus at most `MAX_RECEIPT_IDS` × 36-byte node ids ≈ 2.4 KiB, so 4096 of them is ≈ 9.4 MiB".
But `AppliedSummary` (`:466-476`) carries **two** lists — `created` *and* `matched` — each
truncated at `MAX_RECEIPT_IDS = 64`. Worst case is 128 ids, so ≈ 4.5 KiB of id bytes alone
(≈ 7.8 KiB counting `String` headers), and the real budget is **≈ 18–32 MiB, not ~10 MiB**.
The 10 MiB figure is the *stated reason* 1024 was raised to 4096, so it should be right.

**Remediation.** Restate as `2 × MAX_RECEIPT_IDS` and quote the corrected total, or cap the
two lists jointly at `MAX_RECEIPT_IDS`.

### J3-R1-7 (P3) — the `WRITE_QUEUE_MAX` build guard is vacuous

```rust
pub const WRITE_QUEUE_MAX: usize = MAX_RETAINED_RECEIPTS / 4;
const _: () = assert!(WRITE_QUEUE_MAX * 4 <= MAX_RETAINED_RECEIPTS, …);
```

`(N / 4) * 4 <= N` holds for every `usize` under integer division, so this guard **cannot
fail for any value of `MAX_RETAINED_RECEIPTS`**. It guards only against a future edit that
decouples the two definitions — which is a real if narrow purpose, but not the property the
message claims to prove. The other four guards are genuine (I checked each: 2×4≤8;
512>3×141; 4≥2×2; 16×2≤64). Related nit: `PROBE_CLAMP_RPS` divides by
`WRITE_QUEUE_DRAIN_BUDGET.as_secs()`, so any sub-second drain budget is a compile-time
divide-by-zero rather than a guard failure.

**Remediation.** Assert the thing that can actually be got wrong — e.g.
`assert!(WRITE_QUEUE_MAX <= MAX_RETAINED_RECEIPTS / 4)` written against a literal
`WRITE_QUEUE_MAX`, or add `assert!(WRITE_QUEUE_DRAIN_BUDGET.as_secs() > 0)`.

### J3-R1-8 (P3) — `write_queue_dropped` conflates backpressure with a closing session

`DropReason::Closed` increments `dropped_queue_full` (`writeq.rs:1290-1296`), documented as
deliberate ("the count must not vanish"). The cost is that `write_queue_dropped` — the key
the tilde'd Done-when box points at for "a burst degrades visibly" — cannot distinguish
"the embedder is the bottleneck" from "the session is shutting down and refused a tail".
Those want opposite operator responses.

**Remediation.** A third counter `dropped_closed`, summed into `dropped()` so no count
vanishes and the gauge's exclusivity argument is untouched, plus a `write_queue_dropped_closed`
key.

### J3-R1-9 (P3) — a waiting `lambo_stats` states the same outcome twice in one response

`answered` (`server.rs:1197-1212`) runs the tool body and *then* takes the piggyback. A
`lambo_stats(receipt=R, wait_ms=…)` call settles R inside the body, so the same response
carries both the explicit `receipt: {state: "applied", …}` block and a piggyback note
naming R. The proxy test asserts both are present, so this is intentional — but a model
reads its write outcome twice in one message. Take-once itself is sound: each receipt enters
`undelivered` exactly once (on settle, or on drop in `admit`), and I confirmed the
non-repeat assertion in both the in-process and proxied tests.

**Remediation.** In the receipt branch, drop the just-answered id from `undelivered` before
`answered` takes the piggyback.

### J3-R1-10 (P3) — "chain order is submission order by construction" has a window

The chain position is pinned by `begin_interaction_as`; the lane position is pinned by the
`lanes.lock()` inside `admit`. Those are two different moments with no ordering between
them across threads, so for **two concurrent `lambo_derive` calls from one agent** the chain
order and the drain order can disagree: task A inserts I₁, task B inserts I₂ and enqueues
J₂, then task A enqueues J₁ — chain `I₁,I₂`, lane `J₂,J₁`. Consequence is confined to
created/matched attribution (an earlier interaction's `Derives` edge pointing at a concept a
later interaction created), and for genuinely concurrent calls the client has no defined
order anyway — but the claim as written ("submission order *is* `Temporal`-chain order by
construction", and the tool instructions' "your writes are applied in the order you sent
them") is stronger than what holds. `interleaved_agents_each_keep_their_own_order_on_the_temporal_chain`
submits sequentially, so it does not probe this.

**Remediation.** State the scope: order is pinned for *sequential* submissions from one
agent, which is what an agent can actually assert. Or open the interaction under the same
`lanes.lock()` acquisition that enqueues.

### J3-R1-11 (P3) — `dedup_rate.py`'s runtime message names two wrong causes and not the real one

The message a reader actually hits reads: "Either the lines predate the facts, or a field
was renamed". Since J3 there is a third and now most likely cause, and it is not offered.
§J3 says "The README says all of this at the fact table, so nobody reads a zero as a zero" —
true of the zero, but the terminal output misdirects the diagnosis.

**Remediation.** Add "…or the writes were acknowledged asynchronously and the facts are on
the receipt (`lambo_stats(receipt=…)`) — see the README's metric-2 note" to that message.

### J3-R1-12 (P3) — no J3-shaped derive line in `sample/calls.jsonl`, so `verify.sh` never sees the new schema

`sample/calls.jsonl` still carries only pre-J3 derive lines (`created`/`matched`/…), so all
40 checks pass without ever exercising the `concepts_requested`/`admitted`/`receipt` shape
the README now documents. I had to hand-build a line to test it. A schema change with no
fixture is a schema change the gate cannot defend.

**Remediation.** Add two J3-shaped derive lines and one `record_action` line to the sample,
and a `verify.sh` check that fact-less successful derive lines report `n/a` rather than 0.

### J3-R1-13 (P3) — the reported gate triples do not follow the convention every prior review used

The handoff and §J3 report `872/0/1` fixtures, `940/0/1` sqlite, `550/0/0` cockroach. Those
are the `unittests src/lib.rs` result line alone. Every prior review in this series
(J2-R3's table, and the parent's own recorded `858/0/3` / `946/0/3` / `550/0/0`) quotes the
repo-wide total across all fourteen binaries. Under the convention the true figures are
**885/0/3, 973/0/3, 559/0/0**. Nothing is wrong with the tests; the mismatch cost a full
reconciliation pass to rule out deleted and de-ignored tests. `427fabf`'s commit message
(`854/0/1`, `922/0/1`) has the same shape, so the drift starts at stage 1.

**Remediation.** Requote all four commit messages' and §J3's figures as repo-wide totals,
or state explicitly at the table that the triple is the lib binary only.

## Attacks that did not land

* **Can a heartbeat block?** No — `stats_json()` is synchronous and the wait lives only in
  the tool handler. Clean separation.
* **Does the ledger double-count a waiting call?** No — one line per call, wait inside the
  timed future.
* **Is "carved out of `CLOSE_FLUSH_GRACE`" true, or do the budgets overlap?** **True.**
  `quiesce()` runs *inside* `Memory::close` before `self.writers.write()`
  (`memory.rs:2052-2057`), and `serve.rs:1481` wraps the whole `close` in
  `timeout(CLOSE_FLUSH_GRACE = 8 s)`. The 2 s is subtracted, not added. `2 × 4 ≤ 8` guard
  is genuine.
* **Cross-agent piggyback leak through one hub?** No. `answered` keys on the call's own
  caller-asserted `agent_id`, and
  `a_settled_receipt_is_piggybacked_on_that_agents_next_response` asserts the **negative**
  (agent-b's response must not contain agent-a's receipt) as well as the positive and
  take-once. Bounded at `MAX_PIGGYBACK_RECEIPTS = 8` with a remaining count, so no
  unbounded list.
* **Can a piggyback contradict a fresh fetch of the same receipt?** No. `take_piggyback`
  reads the answer live under the same lock, and only settled receipts ever enter
  `undelivered`, so the only possible progression is `pending` (fetch) → settled
  (piggyback). Redundancy only — J3-R1-9.
* **Did the validation refactor drop rules from the write path?** No.
  `validate_graph_inputs` is still called inside `derive_planned`'s phase 1. Genuine
  extraction.
* **Is `write_queue_measured: false` invisible?** No — the probe logs `tracing::warn!` at
  spawn when unmeasured, and the drop warning carries `measured`. `write_queue_items_per_sec`
  is JSON `null` when unmeasured, not `0` — the I-lesson honoured.
* **Is the queue-full eviction hazard real?** No. `WRITE_QUEUE_MAX ≤ MAX_RETAINED_RECEIPTS/4`
  means the outstanding set is always inside the newest retained window, and
  `eviction_is_oldest_first…` pins it at `MAX_RETAINED_RECEIPTS + 8`. The *count* side of
  the retention derivation is sound; only the *time* side fails (J3-R1-3).
* **Does a process crash leave a new garbage class?** No — the synchronous path also opens
  the interaction first. Widened, not new, and §J3 says so.
* **Does the C-series "session closed, tail durable" invariant survive?** Yes.
  `serve_sigterm_durability` (1 passed), `serve_pre_handshake_durability` (2 passed) and
  `serve_single_writer_lease` (1 passed) all green under the sqlite profile, and
  `close_makes_a_write_acked_just_before_it_durable` pins the new case at depth 1.

## Positive observations

1. **The latency claim is real, and I reproduced it independently at the binary.** Release
   build over raw stdio against the live BGE-M3: median ack **0.039–0.057 ms** across four
   sessions (§J3 claims 0.048 ms), min 0.039 ms. The **no-op control holds**: 22 derives,
   `write_queue_applied: 22`, `concept_count: 22`, `write_queue_failed: 0`, last receipt
   `applied`. Waiting on the last receipt restored read-your-writes end to end. This is the
   strong form of the claim and it stands.
2. **The `ledger_queued_lines` re-derivation is exemplary.** The queue keeps its own
   counters rather than riding the ledger's, `abandoned` is a label rather than a fourth
   term, one shared saturating expression serves gauge and shutdown, and the pinning test
   really does falsify both alternatives — including the one that panics on underflow. This
   is the I-round3 flip-D lesson applied properly rather than cited.
3. **The receipt taxonomy is the right design.** A self-describing id means `expired`,
   `restart_lost` and `never_issued` are separable with zero retained history, eviction
   collapses into expiry instead of becoming a fourth class, and `restart_lost`'s wording is
   verbatim-consistent with the proxy's -32002 — asserted by test, not by comment.
4. **`answered` as a single wrapper** is the right structural choice: receipt delivery
   cannot be forgotten for one tool, and keeping it outside `observed` means delivery does
   not depend on `--ledger`.
5. **The `close()` ordering is genuinely forced, and reasoned as such.** Latch → quiesce →
   gate, with workers deliberately never touching the gate, and abandoned jobs aborted
   **and joined** (the R3-1 lesson) rather than abort-and-hope.
6. **Two of the three defects were found by measuring the shipped binary while the suite was
   green**, and the implementor's own `the_ack_lands_before_the_embedder_is_called` carries
   the comment "that is exactly how this test first passed-by-accident, so the wrapper is
   load-bearing". I independently reinvented that same `VECTOR_SEARCH` shim before reading
   it. Recording the near-miss is worth more than the test.
7. **The deviation was argued from the register and the register was left untouched.** No
   evidence file was rewritten to accommodate an eighth tool. Exactly right.
8. **The regression was declared, located, costed, and handed forward** rather than
   discovered in review — and it degrades to `n/a`, not to a false zero.

## Gate results

Run from scratch at `528ade6`, `CARGO_TARGET_DIR=/Users/narayan/Documents/work/lambo/target`.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | **clean** |
| `cargo clippy --all-targets --all-features -- -D warnings` | **clean** |
| `cargo clippy --all-targets --features fixtures -- -D warnings` | **clean** |
| `cargo clippy --all-targets --features store-sqlite,embed-fixture,fixtures -- -D warnings` | **clean** |
| `cargo clippy --all-targets --no-default-features --features store-cockroach -- -D warnings` | **clean** |
| `cargo test --all --features fixtures` | **885 / 0 / 3** |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | **973 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | **559 / 0 / 0** |
| `scripts/observability/verify.sh` | **40 ok** (and 40 ok at `166a3c8`) |

### The test-set reconciliation

**The numbers did not reconcile because they were reported off the wrong line of the
output, not because tests disappeared.** The house convention (J2-R3) is the **repo-wide
total across all fourteen test binaries including doctests**. The J3 handoff reported the
`unittests src/lib.rs` line alone.

Proof at the parent, where the convention is known good:

| Profile | parent `--list` total | = pass + ignored | J2-R3 reported |
| --- | --- | --- | --- |
| fixtures | 861 | 858 + 3 | **858/0/3** ✓ |
| sqlite | 949 | 946 + 3 | **946/0/3** ✓ |
| cockroach | 550 | 550 + 0 | **550/0/0** ✓ |

At HEAD the same convention gives **885/0/3, 973/0/3, 559/0/0** — every profile strictly
up, and the ignored count **unchanged**. The reported `872/0/1` fixtures figure is the lib
binary's own line (`872 passed; 0 failed; 1 ignored`); the other 2 ignored live in
`tests/live_calibration.rs`, which J3 never touched. There was never a `−6` in sqlite and
never a `3→1` in ignored.

**Every name accounted for.** Name-set diff, parent → HEAD:

| Profile | added | removed | net |
| --- | --- | --- | --- |
| fixtures | 29 | 2 | +27 |
| sqlite | 29 | 2 | +27 |
| cockroach | 10 | 1 | +9 |

The two removals are:

1. `src/memory.rs - memory::Memory (line 921)` → `(line 946)`. A **doctest line-number
   move**, appearing once as a removal and once as an addition in every profile. Not a
   deleted test.
2. `mcp::server::tests::i1_derive_lines_carry_created_and_matched_counts` → **rewritten** as
   `mcp::server::tests::i1_derive_lines_carry_the_admission_and_the_receipt_carries_the_counts`.
   This is the declared ledger regression, and rewriting the test that pinned the old fact
   location is the correct response to relocating the facts.

Discounting the doctest move: **28 new tests in fixtures/sqlite, of which 27 are new and 1
is the rewrite — exactly the "27 new + 1 rewritten" the handoff claims.** Cockroach sees
only the 9 store-independent `writeq::tests`; the 19 fixtures-gated ones (7
`writeq::pipeline_tests` + 12 `mcp::server::tests`) are correctly absent.

**Ignored sets are byte-identical at parent and HEAD in all three profiles**
(`context_embedding_separation`, `embed::bge_m3::tests::live_smoke_against_llama_server`,
`report_bge3_cosine_distribution`). **Nothing was de-ignored and nothing was silently
deleted.** The only finding here is a reporting one, recorded as J3-R1-13 (P3): the handoff
and the §J3 note quote a triple that does not match the convention every prior review in
this series used, which is what made a clean change look like a regression.

### Verification-only edits, declared

Two test functions and a `GraphStore` shim were appended to `src/writeq.rs`, run, and
**reverted**. `md5 src/writeq.rs` is `4eac858e9cbc66acdfa0b48980c36ec5` before and after.
`git status --porcelain` shows only this review file. No commit was amended. A detached
worktree at `166a3c8` was created under the scratchpad for the parent measurements and is
not part of the branch.

---

**Verdict: REQUEST_CHANGES.** J3-R1-1 is blocking: the workstream exists to stop an acked
write from vanishing, and as shipped a single-agent session loses acked writes in bulk at
every clean close under load. J3-R1-2 and J3-R1-3 are the same root cause seen from two
other angles — a bound and a retention guard both resting on a projection treated as a
bound — and should be fixed in the same pass. J3-R1-4 and J3-R1-5 are a false stated reason
and a test that skips the property it names, both cheap. Everything else is advisory.

Nothing here questions the design. The ack is right, the receipts are right, the deviation
is right, the regression was handled honestly, and the latency claim is true in its strong
form and reproduced independently. What needs another round is the arithmetic between the
probe and the drain.
