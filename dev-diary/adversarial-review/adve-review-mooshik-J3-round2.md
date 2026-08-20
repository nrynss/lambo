# Adversarial review — mooshik J3, round 2

**Reviewer**: independent adversarial reviewer (Opus 5), agent_id `j3-reviewer-r2`. Wrote
nothing under review.
**Scope**: the four remediation commits `355e85e..8797ab2` on `wt/j3` — `ee6f4a2` (the P1
cluster), `cab9881` (the two cheap P2s), `08c138d` (the eight P3s, register sweep, count
convention), `8797ab2` (the P1 measured at the binary). Against
`adve-review-mooshik-J3-round1.md`'s thirteen findings, §J3's rewritten status note and
constants table, and the graph's *a projection is not a bound* constraint.
**Worktree**: `/Users/narayan/Documents/work/lambo/.claude/worktrees/j3`, branch `wt/j3`.
No commit amended. Every verification edit reverted; `src/writeq.rs` is byte-identical to
`8797ab2` (md5 `1b9e3b51f4bdb5387a4bf0607836d927` before and after), tree clean but for this
file.
**Verdict**: **REQUEST_CHANGES** — one **new P1**, three **P2**, six **P3**.

**All thirteen round-1 findings are genuinely closed**, and the remediation is better work
than round 1's. I reproduced both sides of the P1 independently, mutation-tested six of the
new invariants and each was caught by the right test, reconciled all four gate numbers
repo-wide by name, and re-derived every new constant from source. The retention fix is
structural rather than arithmetic, which is the stronger answer to J3-R1-3. The count
convention is corrected. `verify.sh` really is 46 ok with a byte-identical sample.

The blocker is **not** a regression and **not** a round-1 finding reopened. It is the *same
hazard* — an acked write abandoned at a clean `close()` — reached through the one door the
new admission still leaves open: **the probe's fixed 35-byte `PROBE_TEXT` is not the
workload**. Measured at the release binary against the live BGE-M3, with 512-byte concept
content, `close()` burned its whole 2020 ms and **35 of 77 acked writes were abandoned**.
The bound is right about the drain's *width* now; it is still wrong about the drain's *rate*
whenever a concept is bigger than a short sentence — and lambo's own dogfood concepts are.

## Method

1. `lambo_recall` as `j3-reviewer-r2` on "J3 projection bound remediation lanes EWMA" — 15
   hits carrying the remediation design, the P1 root cause, the pinned invariant and its one
   declared residual, the J3-R1-3 and J3-R1-6/7 details, and the four commit records. **No
   graph↔code drift found**: every element of the recalled design is present in the code
   as described, including the three-legged probe, the per-lane condition, the half share,
   and the EWMA replacement with a reported source.
2. Read the constants block (`writeq.rs:150-520`) line by line, then `Calibration`,
   `project`, `rate_of`, `admit`, `Lanes`, `ObservedRate`, `spawn_worker`, `Receipts::expire`
   / `evict` / `forget`, `probe_embedder`, `stats_json`'s queue block, `derive_async_as`, and
   the piggyback path.
3. **The P1 red at the parent**: a detached worktree at `355e85e` with its own target dir,
   the HEAD test's shims (`VectorCapable`, `SlowEmbedder`, `Rig::hybrid`) ported in and the
   invariant replaced with instrumentation so the parent's real numbers print instead of
   tripping an earlier assertion. **The P1 green at HEAD**: their own test, plus a temporary
   instrumentation pass to read the numbers behind it.
4. **Six mutations**, each applied and reverted, to establish that the new invariants are
   pinned rather than asserted.
5. All three gates re-run from scratch at HEAD, **tallied repo-wide across all fourteen
   binaries** rather than off the lib line, plus `cargo fmt --all -- --check` and
   `cargo clippy --all-targets -- -D warnings` on two feature sets. Independent test
   name-set diff at parent and HEAD in separate target dirs.
6. **Part B, live at the binary**: the release build (`--features store-sqlite,embed-bge`)
   driven over raw stdio JSON-RPC against the rig's llama.cpp BGE-M3 q8_0 on CPU
   (`127.0.0.1:8080`, `{"status":"ok"}`) — their 200-derive control reproduced, four
   fresh-session probe-cost runs, the same control at three content sizes, and a direct
   length-sensitivity sweep of the embedder itself.
7. **Two constructed attacks** in `pipeline_tests`' own idiom, run and reverted: a
   length-proportional embedder, and a hanging embedder against `PROBE_BUDGET`.
8. `verify.sh` executed at both commits. Register sweep run independently over the touched
   files.

**Order of authority.** §J3 is the claim; the source is what ships; the *binary against the
live embedder* outranks both. This round's blocking finding comes from the binary.

## Part A — the thirteen round-1 findings, adjudicated

| # | Finding | Verdict | Evidence |
| --- | --- | --- | --- |
| **J3-R1-1** (P1) | 4-wide projection, 1-wide drain | **CLOSED for the width** (see J3-R2-1 for the rate) | Red reproduced at `355e85e`: bound 79, 79 accepted, quiesce 2.0019 s, **60 of 79 acked writes abandoned**, applied 19 — inside round 1's 58-61 band. Green at HEAD on the same parameters: serial 9.86/s, concurrent 38.53/s, `lane_bound` 10, `bound` 39, accepted 10, dropped 70, quiesce **1.036 s of 2 s**, **abandoned 0**, applied 10. Arithmetic re-derived: `ceil(9.86 × 2/2) = 10`, `ceil(38.53 × 1) = 39`. `admit` checks the per-lane leg **first**, and `lane_outstanding` counts queued **plus** `running_per_lane`, so the in-flight job is in the population |
| **J3-R1-2** (P2) | Probe measures warmth (7× swing) | **CLOSED** | Warm-up discard works, measured: four fresh sessions at the binary read 74.2, 54.7, 75.5, 65.2 items/s — a **1.4× spread against round 1's 7×**. The EWMA replacement is real and one-way: `ObservedRate::samples` only ever `saturating_add`s and `items_per_sec()` gates on `< OBSERVED_MIN_SAMPLES`, so `write_queue_bound_source` goes `probe`/`unmeasured` → `observed` and **cannot flap back**. I watched it flip at the binary (`bound_source=observed`, `lane_bound` 77 → 20). The **fenced-refusal exclusion is verified at source**: `sample()` sits inside `spawn_worker`'s non-fenced `else` branch only, with the reason stated |
| **J3-R1-3** (P2) | `expired` reachable for a running job | **CLOSED, and structurally** | `Receipts::expire` **and** `Receipts::evict` both skip unsettled entries and re-push them in order. The fix is **skip**, not a bigger quarter — and `evict`'s docstring names round 1's drop-storm scenario by name and says why the arithmetic argument was insufficient ("refusals get receipts as well"). Re-fetch after settle **then** expiry falls through `lookup` to `Expired`, which is the honest answer. Mutating `expire` to `forget` an unsettled entry reddens 15+ `mcp::server` tests. The false build guard is replaced by `RECEIPT_RETENTION > MEASURED_WORST_FLUSH_LAG_SECS`, which is true (300 > 227), non-vacuous, and matches §Measurements' "145 to 227 s" exactly |
| **J3-R1-4** (P2) | Pre-pass docstring wrong for the default strategy | **CLOSED** | Read the code, not the doc: `memory.rs:1428` runs `hybrid::validate_limits` unconditionally, then `:1432` `hybrid::validate_graph_inputs(&g, parent_of)` for `Hybrid` and `:1434` `graph::derive::validate(&g, concepts, parent_of)` for `Canonical`. `config.rs:167` sets `Hybrid` as the default and `:381` pins it. The new bullet splits per strategy, names the two omitted rules, enumerates the five error classes that move, and states no rule left the write path. **Doc matches code, arm for arm.** See J3-R2-8 for the one residual |
| **J3-R1-5** (P2) | Burst test sealed instead of filling; two drop paths untested | **CLOSED, and mutation-verified** | Renamed to `a_sealed_queue_refuses_and_counts_it`; three new tests. Each mutation-checked: defang the byte gauge → `a_burst_past_the_byte_cap_drops_and_counts_it` alone goes red; short-circuit the aggregate leg → `enough_lanes_together_reach_the_aggregate_bound_and_it_counts_them` alone; halve the projection share → `a_burst_past_the_lane_bound_drops_and_counts_it` and the invariant test. The byte test *scales* its payload from the constant by design, so a 4096× constant mutation is caught by `the_constants_say_what_their_docs_say` instead — legitimate two-part pinning, not a gap |
| **J3-R1-6** (P3) | ~10 MiB counted one of two id lists | **CLOSED** | Recomputed: 128 ids × (36 B text + 24 B `String` header) = 7680 B; × 4096 = **30.0 MiB** of ids, ~32 MiB with the summary — "≈31 MiB" is inside rounding. Realistic 64 ids → 3840 B × 4096 = 15.0 MiB vs "≈16 MiB", also inside rounding. The honesty is in naming that the correction does **not** move the constant, and the real driver is re-derivable: `PROBE_CLAMP_RPS > 3 × 141` needs `WRITE_QUEUE_MAX ≥ 424` hence `MAX_RETAINED_RECEIPTS ≥ 1696` — I re-derived both |
| **J3-R1-7** (P3) | Vacuous `WRITE_QUEUE_MAX` guard | **CLOSED** | The replacement can fail: shrink `MAX_RETAINED_RECEIPTS` below 16 and `WRITE_QUEUE_MAX >= WRITE_QUEUE_MIN` inverts. `PROBE_CLAMP_RPS > 3 * MEASURED_LOCAL_EMBEDDER_RPS` (1024 > 423) fails below 1696. The sub-second divide-by-zero nit got its own guard (`DRAIN_BUDGET.as_secs() > 0`). `PROBE_CLAMP_RPS = 1024 × 2 / 2 = **1024**` re-derived from source, matching the claim |
| **J3-R1-8** (P3) | `write_queue_dropped` conflated backpressure with closing | **CLOSED** | `dropped_closed` is its own counter, summed into `dropped()` with the other two; the three classes are disjoint and **none enters `accepted`**, so `outstanding = accepted − applied − failed` is untouched — the ledger lesson holds, no never-accepted class is subtracted. Mutating `Closed` back onto `dropped_queue_full` reddens `a_sealed_queue_refuses_and_counts_it` alone |
| **J3-R1-9** (P3) | Waiting `lambo_stats` states its outcome twice | **CLOSED** | `mark_delivered` (`writeq.rs:2011`) `retain`s the id out of `undelivered`; its single call site (`server.rs:2031`) is inside the awaited body, and `answered` (`:1232`) awaits that body **before** `take_piggyback` (`:1239`). Ordering is enforced by sequence, correctly. The proxy test passed in-gate. See J3-R2-9 for what it does not prove |
| **J3-R1-10** (P3) | "Chain order by construction" has a window | **CLOSED on every agent-facing surface** | The instructions string, both `mcp.mdx` mirrors and the mirrored instructions block are all scoped to "writes you send one after another", with the concurrent case named and declined. The regression guard at `server.rs:4879` is **real, not vacuous**: `git log -S` confirms `"your writes are applied in the order you sent them"` shipped at `355e85e` (`server.rs:2112`) and was removed here. See J3-R2-6 for three surviving developer-facing restatements |
| **J3-R1-11** (P3) | `dedup_rate.py` named two wrong causes | **CLOSED** | The async ack now leads ("Most likely the writes were acknowledged asynchronously (J3)"), `lambo_stats(receipt=...)` follows as the remedy, and the two pre-J3 hypotheses are demoted to a trailing "Otherwise…" |
| **J3-R1-12** (P3) | No J3-shaped line in the committed sample | **DEVIATION ACCEPTED** | Adjudicated below |
| **J3-R1-13** (P3) | Gate triples quoted off the lib line | **CLOSED** | Reconciled by name; see the table below |

### J3-R1-12 — the deviation, adjudicated

**Their argument holds and I verified both halves of it.** `sample/calls.jsonl` and
`make_sample.py` are byte-identical across `355e85e..8797ab2` — same git blob shas
(`795935f0…`, `b5bd6848…`), empty `git diff --stat`. And the new `verify.sh` step is a real
check, not an echo: the heredoc fixture plants `concepts_requested` / `admitted` / `receipt`
on two derives, a `record_action` and a refused derive with no facts; the report prints
`dedup = n/a` in the TOTAL row; and the `--json` leg asserts
`d["totals"]["dedup_rate"] is None` together with `created == 0 and matched == 0` and
`derive_calls_without_facts == 3`. **That is the "n/a not 0.000" property, asserted where it
can be asserted.**

The reason for the deviation is sound: fact-less lines in the committed sample would move
the exact numbers five existing checks read (the 66.7% compliance figure, the "rising"
convergence, the per-day rates), and a fixture that perturbs the plants it shares a file with
defends one schema by weakening five checks. I ran `verify.sh` at both commits: **40 ok at
`355e85e`, 46 ok at `8797ab2`, ALL CHECKS PASSED both times.** All six new ok lines sit
behind a real predicate — five `check` calls (substring assertions against captured output)
and one inline `printf` behind the three-way Python `assert`. None is unconditional.

### The numbers, reconciled repo-wide and by name

| Gate | Parent `355e85e` | HEAD `8797ab2` (claimed) | HEAD (I measured) |
| --- | --- | --- | --- |
| `cargo test --all --features fixtures` | 885 / 0 / 3 | 891 / 0 / 3 | **891 / 0 / 3** |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 973 / 0 / 3 | 979 / 0 / 3 | **979 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | 559 / 0 / 0 | 560 / 0 / 0 | **560 / 0 / 0** |
| `scripts/observability/verify.sh` | 40 ok | 46 ok | **46 ok** |

Tallied across all fourteen binaries, not the lib line — the convention J3-R1-13 asked for,
and the note now uses it. `cargo fmt --all -- --check` clean; `cargo clippy --all-targets`
clean with `-D warnings` on `store-sqlite,fixtures` and on
`--no-default-features store-cockroach,embed-fixture`.

**Name-set diff** (`--list` at both commits, separate target dirs): 888 → 894 unique names.
Nine added, three removed, and all three removals are the declared renames —
`a_burst_past_the_bound_drops_and_counts_it` → `a_sealed_queue_refuses_and_counts_it`,
`eviction_is_oldest_first_so_an_evicted_id_is_older_than_everything_held` →
`eviction_is_oldest_settled_first_and_never_takes_a_running_writes_receipt`,
`the_bound_tracks_the_measurement_between_the_clamps` →
`the_bounds_track_their_own_legs_between_the_clamps`. So **+6 genuinely new, 3 renamed, 0
removed, 0 de-ignored** — exactly the claim. 888 − 3 ignored = 885 and 894 − 3 = 891, so the
counts close on both sides.

### Mutation testing

Every mutant applied to a clean tree and reverted; `src/writeq.rs` md5 unchanged throughout.

| mutant | caught by | verdict |
| --- | --- | --- |
| `DRAIN_PROJECTION_SHARE` 2 → 1 | the invariant test **itself** (+4 others) | red reads "abandoned 1 of 20 ACKED writes in 2.0013 s" — the half share is load-bearing and measured |
| `WRITE_QUEUE_MAX_BYTES` × 4096 | `the_constants_say_what_their_docs_say` | caught (the byte test correctly pins the relation, the constants test the literal) |
| byte gauge never returns bytes on pop | `a_burst_past_the_byte_cap_drops_and_counts_it` | right test, alone |
| `Closed` rides `dropped_queue_full` again | `a_sealed_queue_refuses_and_counts_it` | right test, alone |
| `expire` forgets unsettled instead of skipping | 15+ `mcp::server` tests | heavily pinned |
| aggregate leg short-circuited out of `admit` | `enough_lanes_together_reach_the_aggregate_bound_...` | right test, alone |

### The half share, and the aggregate leg's assumption

**`DRAIN_PROJECTION_SHARE = 2` is honestly justified and *generous*, for the reason given.**
Their reason is that the probe times the embedder only and a warm derive's remainder is ~1/5
of the embed. §Measurements: warm `derive` 27 ms, raw embedding call **22 to 27 ms** →
remainder 0-5 ms, i.e. 0 to 22.7% (1/4.4) of the embed. Half authorises a 100% remainder
against a measured worst case of ~23%: a **4.3× margin**, not a tight one. Confirmed at the
green run, which drained in 1.036 s of 2 s.

**The aggregate leg's assumption is declared, and at the artifact** — `writeq.rs:842`,
immediately above `struct Calibration` where both bounds are computed: "the aggregate bound
assumes throughput does not *fall* as lanes grow past four", repeated as limit (2) of §J3's
`[~]` box. The aggregate check itself works and is tested: the test builds
`bound/lane_bound + 2` lanes, fills each to its own lane bound, and asserts by message that
every refusal is the aggregate one. Structurally the lane-*count* direction is safe (a fixed
aggregate spread over more lanes gives each a shallower queue); only sublinear total
throughput is exposed, which is exactly what is declared. **No finding.**

### `PROBE_BUDGET` under a hanging embedder — verified, and the docstring is exactly right

`probe_embedder` computes **one** `deadline` at the top and wraps every one of the three legs
in `timeout_at(deadline, …)`, so the budget covers all `PROBE_EMBEDS` **together** —
precisely what the docstring claims, and not "5 s per leg". Constructed and run: an embedder
that never answers returns `Calibration::unmeasured()` in **5.0017 s**; one that hangs only
on the *warm-up* leg (the leg added after the budget was sized) returns in **5.0026 s**. Both
land on `lane_bound = 1`, `bound = 4`, `source: Unmeasured`, which is honest and — because
the observed rate then takes over after four completed writes — self-healing. There is **no
committed test for either**; see J3-R2-7.

### `Arc::get_mut` at `Rig::hybrid` — their open item 3, closed

Test-only, and the `expect` is unreachable rather than merely untriggered.
`WritePipeline.ctx` is a **private** field and `Rig::hybrid` lives in
`#[cfg(all(test, …))] mod pipeline_tests`, a child of `writeq` — a library caller has no
path to it. And `WritePipeline::spawn` clones `ctx.embedder` and `ctx.session` for the probe
task but never `ctx` itself, doing `Arc::new(ctx)` last, so the Arc reaches `Rig::hybrid`
with strong count 1 deterministically. **I would close this rather than carry it.**

## New findings

### J3-R2-1 (P1) — the probe's text is not the workload: 35 of 77 acked writes abandoned at a clean close, at the release binary

`PROBE_TEXT` is 35 bytes, and its docstring states the assumption outright:

> "Short, fixed, and content-free: it is measuring the deployment's embedder, not its own
> input." — `src/writeq.rs:379-380`

For a transformer embedder, input length is a first-order determinant of latency, so the
serial leg systematically **over-estimates** what a lane retires for any workload whose
concepts are longer than a short sentence. `DRAIN_PROJECTION_SHARE` buys 2×. The gap is
larger than that at ordinary sizes.

**Measured at the release binary.** `--features store-sqlite,embed-bge`, live llama.cpp
BGE-M3 q8_0 on CPU, raw stdio JSON-RPC, one agent, 200 `lambo_derive` calls back to back
with **512-byte concept content**, then the pipe closed so `serve` runs its clean shutdown,
then a fresh `serve` on the same SQLite store counts what landed:

```
512B burst, CLOSE IMMEDIATELY: acked=77 dropped=123 close_ms=2020
  at-stats: lane_bound=77 bound=222 src=probe serial=76.46/s applied=0 outstanding=77
concepts_in_store_after_restart=42
```

and from `serve`'s own stderr:

```
ERROR lambo::writeq: write queue: 35 acked write(s) were NOT applied — the queue did not
drain within 2s of close(); their receipts say so  session=nc512 abandoned=35
```

**42 applied + 35 abandoned = 77 acked.** Zero embedder errors in that run. Clean close, no
adversary, no degradation, content at 1/32 of `MAX_CONTENT_BYTES`.

**The implementation reports the discrepancy itself.** On the *same* content, the probe's
serial leg read `write_queue_serial_items_per_sec = 76.46` (13.1 ms) while the lane workers'
own observed service time read **19.03** (52.5 ms) — a **4.0× over-estimate** against a 2×
slack. The two numbers are both in `lambo_stats`; nothing compares them.

Length sensitivity of this rig's embedder, measured directly (median of 8 raw
`/v1/embeddings` calls each):

| input | median | × `PROBE_TEXT` |
| --- | --- | --- |
| `PROBE_TEXT` (35 B) | 13.7 ms | 1.00 |
| 128 B | 18.7 ms | 1.36 |
| 256 B | 20.3 ms | 1.48 |
| 512 B | 23.8 ms | 1.74 |
| 1024 B | 38.5 ms | 2.81 |
| 2048 B | 69.5 ms | 5.07 |

The half share is exhausted somewhere between 512 B and 1 KiB **on the embed alone**, before
the non-embed remainder is counted. Lambo's own dogfood concepts — the `Logic` and
`Constraint` entries this review's `lambo_recall` returned — run 700 to 1500 bytes. This is
not an adversarial input class; it is the product's own recorded memory.

Reproduced in `cargo test`'s own idiom too (length-proportional embedder, 512-byte content,
reverted): `lane_bound 131`, accepted 131, quiesce 2.0032 s, **114 of 131 abandoned**,
`source` still `Probe` at the end of the burst.

**Why this is P1 and not P2.** It is the founding hazard of the workstream — an acked write
that never applies — reached on the **clean** path, with no adversary, on the product's own
content sizes, measured at the binary against the real embedder. Round 1 graded the same
shape at the same magnitude (61 of 80) a P1, and consistency argues the same here. §J3's
`[~]` box presents its honest limits as complete and names three; limit (3) names **one**
write slower than the budget, where the real limit is **a lane's worth**. And the
remediation's own test asserts `abandoned == 0` and `applied == accepted`, which this
falsifies for an ordinary workload.

**Why it is narrower than round 1's, stated plainly.** (a) The receipts are honest — 35
`failed` receipts saying "nothing was written" — so the invariant as *worded* in §J3
survives; it is the test-pinned form that does not. (b) It is **self-correcting**: `source`
stays `probe` only until `OBSERVED_MIN_SAMPLES` (4) real writes complete, after which the
observed rate takes over and the lane bound falls to the right value. I watched it do
exactly that (`bound_source=observed`, `lane_bound` 77 → 20 on an identical run given 6 s
before close). (c) It needs a burst. But acks cost 0.04 ms and writes cost 52 ms, so an
agent submits the whole lane bound in ~5 ms — **the first burst of every session is admitted
at the probe's figure**, and a session that bursts once and closes never gets the
correction.

**Remediation, cheapest first.** (a) Make the probe's input representative — embed a text
sized at the door's own typical payload rather than 35 bytes, or probe at two sizes and take
the slower rate; the probe already has a 5 s budget and uses ~80 ms of it. (b) Or make the
*lane bound* respect the observed rate before it is confident: floor the admitted depth at
`WRITE_QUEUE_MIN` until `OBSERVED_MIN_SAMPLES` land, so the exposed burst is 4 jobs rather
than 77 — the probe's figure then sizes nothing load-bearing and `PROBE_TEXT`'s assumption
stops mattering. (c) Or scale the projection by the ratio of the job's own payload bytes to
`PROBE_TEXT.len()`, which is information `admit` already has (`payload.bytes()`). Then
extend `one_agents_burst_never_outruns_its_own_lanes_drain_at_a_clean_close` with a second
parameterisation whose per-job service time exceeds the probe's by more than
`DRAIN_PROJECTION_SHARE`; my harness is a starting point. And either way, `PROBE_TEXT`'s
docstring needs the sentence removed or scoped — it is the false stated reason underneath
all of this.

### J3-R2-2 (P2) — a fast-failing embedder biases the observed rate in the dangerous direction

`spawn_worker` deliberately excludes a **fenced** refusal from sampling, with the right
reason stated ("calling it a fast write would bias the rate upward, which is the dangerous
direction"). But a write that **fails** — embedder error, `HYBRID_IO_TIMEOUT`, a store error
— *is* sampled, and those fail fast. I saw a 2048-byte run where llama returned 500s in ~2 ms
per call; on such a run the EWMA converges toward the failure latency and the lane bound
climbs toward `WRITE_QUEUE_MAX`.

It is self-consistent while the failures continue (a lane of fast failures drains fast). The
hazard is the **transition**: an embedder that fast-fails a burst, is admitted against the
resulting inflated bound, then recovers and services the queue slowly. With
`OBSERVED_EWMA_WEIGHT = 4` the average needs ~4 slow samples to come back, during which the
bound over-admits by the same ratio.

**Remediation.** Sample only `Ok` outcomes, or sample failures into a separate figure the
bound does not read. The one-line exclusion already exists for the fenced case; this is the
same argument for the same reason.

### J3-R2-3 (P2) — §J3's `[~]` box presents three honest limits as complete, and J3-R2-1 is not among them

The box's own framing is "**Tilde, and here are the honest limits.**" followed by three, and
closes "None of the three remaining limits is a burst that degrades invisibly, which is what
the box is for". Limit (3) names a **single** write slower than the drain budget and points
at `WRITE_QUEUE_LANE_MIN = 1` as the constant that names the case. J3-R2-1 is the same
character at 35× the magnitude, and it is absent.

This matters beyond bookkeeping: the box is the artifact a reader consults to decide what
they are accepting, and it is the surface J4/J5 will inherit. An enumeration that presents
itself as complete and is not is the J1-R2 register lesson ("a remediation's own
justification comments become false claims") in its cheapest form.

**Remediation.** Whatever J3-R2-1's fix turns out to be, the box needs a fourth limit
stating the residual honestly, with the ratio the half share actually covers.

### J3-R2-4 (P2) — the two rates that disagree are both published and never compared

`lambo_stats` carries `write_queue_serial_items_per_sec` (whichever source is in force) and
`write_queue_items_per_sec` (the probe's concurrent leg), plus `write_queue_bound_source`.
What it does **not** carry is the probe's serial figure once observation has replaced it — so
the single most diagnostic fact available, *"this deployment's real service time is 4× what
the probe measured"*, is destroyed at the moment it becomes knowable. `with_observed_serial`
overwrites `serial_items_per_sec` outright.

An operator reading a session that abandoned 35 writes has `bound_source: observed`,
`serial_items_per_sec: 19.03`, and no way to learn that the burst was admitted at 76.46.
Nothing logs the transition either.

**Remediation.** Keep the probe's serial figure alongside the observed one (a
`write_queue_probe_serial_items_per_sec` key, or retain it on `Calibration`), and
`tracing::info!` once when the source flips, with both numbers. That single line would have
made J3-R2-1 self-diagnosing in the field.

### J3-R2-5 (P3) — §J3's 221 ms first-ack cost does not reproduce, and its `observed`-by-the-end claim does not hold

§J3 states the cost as "**221 ms** at the binary for the first call". Four fresh sessions,
fresh SQLite each, same live embedder: **69.6, 80.2, 63.0, 67.5 ms**. The claim errs
conservative, which is the right direction — but it is a single number quoted without a
warmth caveat for a quantity this very workstream proved is warmth-dependent, in the same
note that proved it.

Second half, and the more consequential one: §J3 says "`write_queue_bound_source` read
`observed` by the end of that run" of 20 derives. In four 20-derive runs read immediately
after the last ack it reads **`probe`** — 20 acks complete in ~1.2 ms and four writes take
~55 ms. It does flip given time, but not "by the end of" a tight 20-call run. That sentence
is what a reader would use to conclude J3-R2-1's exposure window is short.

**Remediation.** Quote the first-ack cost as a range with the warmth caveat, and correct the
`observed` sentence to say what makes it flip (four *completed* writes, not four calls).

### J3-R2-6 (P3) — "everywhere claimed" is true of the agent-facing surfaces and not literally

The scoping is complete and well done where a model can read it: the instructions string
(`server.rs:2149-2150`), both `mcp.mdx` mirrors (passage-identical, verified), the mirrored
instructions block, and `writeq`'s §Ordering and `derive_async_as`'s bullet, both of which
cite J3-R1-10. Three developer-facing restatements survive unscoped:

* `src/writeq.rs:2926-2927` — "The `Temporal` chain is pinned by construction … so what this
  test has to prove is the other half".
* `src/mcp/server.rs:3293-3296` — "one agent's slice of it **is exactly** that agent's
  submission order".
* `dev-diary/lambo-for-mooshik/J-multi-client.md:1883-1885` — "chain order is submission
  order **by construction** and cannot be corrupted by an out-of-order drain **at all**".

The third is the one that matters: it is in the Done-when checklist the phase is closed
against, ~470 lines from the bullet that scopes it, with no pointer. Both tests submit
sequentially, so no test is wrong — only its prose.

A related note in the remediation's favour, since I set out to disprove it: `writeq`'s
§Ordering opens with the strong sentence and retracts its scope nine lines later, which reads
as an overclaim to anyone who stops at the paragraph break. That is a structural nit, not a
false claim — the retraction is explicit and cites the finding.

### J3-R2-7 (P3) — `probe_embedder`'s budget, the whole probe, has no test

Everything else in this remediation is pinned. `probe_embedder` is not: nothing exercises the
three legs, the discarded warm-up, the deadline, or `Calibration::unmeasured()`'s reachability
through it. `an_observed_rate_replaces_the_probes_serial_figure_after_enough_samples` tests
`ObservedRate` in isolation and `the_bounds_track_their_own_legs_between_the_clamps` tests
`from_probe`'s arithmetic, but the function that produces the load-bearing number is
untested. My two constructed tests took ten lines each and both passed — they are worth
committing, especially the warm-up-hangs case, since `PROBE_WARMUP_EMBEDS` was added *after*
`PROBE_BUDGET` was sized and the docstring now claims the budget covers all six embeds
together.

### J3-R2-8 (P3) — four small stated-reason blemishes in the constants block

The register sweep claims `src/writeq.rs` was swept for "every numeric claim and stated
reason in the constants block". Four survive:

1. `DRAIN_PROJECTION_SHARE`'s docstring quotes "**22-25 ms** is the embed"; §Measurements
   says "22 to **27** ms". The misquote is in the conservative direction but it is a misquote
   inside a load-bearing derivation.
2. `MAX_CONCURRENT_RECEIPT_WAITS`' docstring says "a waiting **`lambo_receipt`** call" twice.
   There is no `lambo_receipt` tool — that is the eighth tool the deviation declined; the
   surface is `lambo_stats(receipt=…)`. Pre-existing (2 occurrences at both commits) and not
   flagged in round 1, so this is a carryover, but it is exactly the class the sweep was for.
3. The `PROBE_CLAMP_RPS` guard's message contains six consecutive spaces mid-sentence
   (`"… or      the queue bound …"`) — a wrapped line collapsed into a literal. Cosmetic, and
   it is the text an operator sees if the build ever breaks there.
4. §J3's "`lambo_stats` gains fourteen unconditional keys" bullet is **correct** — I counted
   13 `write_queue_*` plus `receipts_retained` = 14 in `stats_json`, matching both `mcp.mdx`
   enumerations and the test's key list — but the bullet's rationale beneath it still explains
   only `write_queue_bound` / `_measured` / `_items_per_sec` and says nothing about the two
   bounds or `bound_source`, which are the keys that need explaining most.

The OFF-payload discipline **holds**: the `ledger_*` keys are still gated on
`if let Some(ledger) = &self.ledger` with the byte-identity promise intact, and the
write-queue keys are deliberately unconditional with the reason stated at the code and
documented in both `mcp.mdx` mirrors ("The write queue keys above have no such switch,
because there is no mode in which writes bypass the queue"). That was the shape at first J3
landing, not a change this round made.

### J3-R2-9 (P3) — the piggyback test does not close the loop on the new path

The rewrite is an improvement and its comments are honest about what it declines to assert
(which response carries the piggyback, deliberately not pinned — the right call, since round
1's version raced the holder's drain schedule). It proves the J3-R1-9 property at the right
response: `receipt_two`, the id the `waited` call answered explicitly, is absent from that
call's piggyback.

What it does not prove is that `mark_delivered` **removed** rather than **suppressed**.
Take-once is asserted for `receipt` (write one, delivered by ordinary `take_piggyback`), not
for `receipt_two` (delivered by `mark_delivered`), and no assertion is made about the
piggyback of the later `first` fetch — which is exactly where a resurfaced `receipt_two`
would appear. An implementation that skipped piggybacking any receipt settled during the
current call would pass unchanged.

**Remediation.** One added assertion: `!piggyback_of(&first).contains(&receipt_two)`.

## Attacks that did not land

* **Can the source flap `observed` → `probe`?** No. `samples` only `saturating_add`s and the
  gate is a `<` on a monotone counter. One-way by construction.
* **Can a running write's receipt be evicted by a drop storm?** No, and the fix is the
  structural one rather than the arithmetic one round 1 offered. `evict` skips unsettled and
  re-pushes in order; its docstring names the scenario and says why
  `WRITE_QUEUE_MAX ≤ MAX_RETAINED_RECEIPTS/4` was not enough. I confirmed a 200-derive
  session retains 200 receipts (refusals included) with the outstanding set intact.
* **Does the aggregate leg fail to catch N lanes each within their own bound?** No. Tested,
  mutation-verified, and safe in the lane-count direction by construction.
* **Does `dropped_closed` break the gauge?** No. Three disjoint refusal classes, summed, none
  entering `accepted`.
* **Is `PROBE_BUDGET` per-leg rather than total?** No — one `deadline`, `timeout_at` on all
  three legs. The docstring's stronger claim is the true one.
* **Can a library caller reach `Rig::hybrid`'s `expect`?** No — private field, test-only
  module, and the Arc is fresh with count 1 anyway.
* **Their 200-derive control**: reproduced exactly at HEAD — acked 76, **76 in the store after
  a clean close and restart, 0 lost**. It is a true measurement of a workload whose concepts
  are shorter than `PROBE_TEXT`.

## Verdict

**REQUEST_CHANGES**, blocking **J3-R2-1**.

Thirteen of thirteen round-1 findings closed, most of them better than the remediation asked
for. The invariant holds at the binary **for the workload the probe's text represents** and
fails for concept sizes the product itself writes. The fix is small and the diagnosis is
already in the payload — two numbers that disagree by 4× and nothing comparing them.

J3-R2-2 and J3-R2-4 are the two P2s worth closing in the same pass as the P1, since both are
in the same estimator and one of them is what would have made this finding self-reporting.
J3-R2-3 follows from whatever the P1's fix is. The six P3s are advisory.

---
*Verification-only edits: six mutations to `src/writeq.rs`, two constructed test blocks, one
instrumentation pass, and a ported test in a throwaway worktree at `355e85e`. All reverted;
`src/writeq.rs` md5 `1b9e3b51f4bdb5387a4bf0607836d927` matches `git show HEAD:src/writeq.rs`.
No commit amended.*
