# Issue 79: recall cold start

Recall's final score is `w_daemon × d + w_query × q`, where `d` is the
daemon's score-table entry and `q` is the max-merged phase-1 score (vector
cosine, keyword BM25, or the recent leg's flat `RECENT_SCORE` 0.35). Before
#79 a concept with no table entry scored `d = 0` inside that blend. A fresh
relevant image therefore ranked below older noise that the daemon had
already scored. Measured on live EG2 (table below), the fresh red image
scored 0.3616 and ranked #4, under old blue (0.6576) and old green (0.6554).

Calibration (`RECENT_SCORE`, merge threshold, cosine scaling) is #87 and is
untouched here.

## The rule

In `recall::assemble::assemble_with_legs`, a recall is **cold** when
`w_query > 0` and at least one expanded phase-1 candidate meets all four
conditions:

- it is backed by the keyword or vector leg (`LegScores`), not only by the
  recent leg;
- it is a concept present in the graph;
- it has **no** entry in the score table (an explicit `0.0` counts as
  scored);
- query-only order would **emit** it: within `top_k`, or force-included as
  a hot member (review M1, added in remediation). Its `d` is 0, so the
  blend never ranks it higher than query-only order does; a fresh concept
  query-only order cannot show cannot be shown at all, and it no longer
  withholds the daemon share from the hits that are shown.

The result reports the mode as `DetailedRecall::cold_start`, and the
`serve --ledger` recall line carries it as `cold_start` (review M2).

In a cold recall every expanded member scores `w_query × q`, older scored
members included. Traversal and sibling members have `q = 0`, so they rank
after every phase-1 hit. A member with an active reservation that the
blended order would have emitted is force-included while cold, so its
Reservation warning survives the window (review L4). Canonical-first
order, hot-list force-inclusion and the key/id tie-break are unchanged. Once
the daemon publishes a table that holds the concept, the normal blend
returns, and ranks may change at that transition. The user accepted that
limit when choosing this rule.

The daemon (`Daemon::recall_routed`) is the only production caller, and it
always passes the leg map. `assemble` without legs is now `#[cfg(test)]`.

### Why every member, not just the new one

Giving only the new concept a stand-in score cannot be made safe. With EG2,
fresh noise has `q` up to 0.6776 and an established relevant image has
`q = 0.75`. A query stand-in for the fresh concept alone puts that noise
above the established image whenever the image's `d < 0.6052`. Any cap on
the stand-in hits the impossibility recorded in the graph: scored A
(q .80, d 0) = .40, scored B (q .66, d .40) = .53, fresh relevant F
(q .75). No placement of F between A and B preserves their order and puts
F above B. Ordering the whole recall by `q` for the short unscored window
satisfies every case.

## Risk (a): how often is a recall cold?

The wip cadence review measured how close recalls came to derive *call*
times. That says nothing about how long a concept actually stays
unscored. Measured here instead:

- **Window length.** `Memory::derive`, `derive_image_as` and the write
  queue's apply path (`writeq/execution.rs`) all wake the daemon. The next
  cycle rescores because the epoch changed, and `rescore` scores **every**
  concept, so nothing stays unscored past one cycle.
  `rescore` at dogfood scale (6,466 nodes, 11,601 edges, release) takes
  **4.51 ms median, 4.80 ms p90, 5.64 ms max** over 30 runs. That was a
  temporary local timing test, not committed.
- **Replay.** I used the read-only ledger copy (13,516 rows, 2026-08-19 to
  2026-10-10). New concepts become visible at the `completion` rows'
  `applied` timestamps (`created_count > 0`) and at synchronous
  derive/record_action calls with `created`: 1,668 applies. A recall can be
  cold only if one of those applies falls within `L` before its score
  snapshot. The snapshot lies somewhere within the recall's logged span.
  Counting every recall whose span overlaps `[apply, apply + L]`, as an
  upper bound:

  | L | recalls that could be cold |
  |---:|---:|
  | 5 ms | 13 / 1,098 (1.18%) |
  | 50 ms | 13 / 1,098 (1.18%) |
  | 300 ms | 15 / 1,098 (1.37%) |
  | 1 s | 17 / 1,098 (1.55%) |
  | 2 s | 18 / 1,098 (1.64%) |

  The bound barely moves with `L`. It comes from recalls that are in flight
  for 1 to 5 s (query embedding) while an agent's derive lands, not from
  the window. The true rate is lower again: it also needs the fresh concept
  to be a keyword or vector hit of that recall.
- **Empty table.** When nothing is scored yet (process start), every
  `d = 0`, so cold mode gives exactly the old scores.

**Decision: keep the global rule.** At about 1% of recalls, a narrower
variant would only add complexity, and it would reintroduce the A/B/F
impossibility for exactly the recalls it targets. (Remediation narrowed
only the trigger, per M1: a fresh concept that cannot be shown no longer
starts cold mode. Once it starts, it is still global.)

### The real window (review M2, measured in remediation)

The 4.5 ms above timed `rescore()` alone. The window runs from the apply
to the publish of the next completed cycle: wake latency, the rest of any
cycle already running (access apply, four detectors, GC drain or sweep),
then the next rescore. Measured with a temporary, uncommitted release test
on a synthetic graph at dogfood scale (2,000 interactions, 4,466 concepts,
6,466 nodes, 11,600 edges, six agents, 51 days, 1-in-90 canonical), never
the live rig. The test file is kept at
`scratchpad/79r/window_bench.rs`, its output at `79r/window-bench.log`.

| step (30 runs) | p50 | p90 | max |
|---|---:|---:|---:|
| `rescore` | 4.69 ms | 4.91 ms | 5.60 ms |
| four detectors | 5.58 ms | 5.70 ms | 6.65 ms |
| `gc::run`, full sweep | 11.10 ms | 11.27 ms | 12.10 ms |
| `run_cycle`, no GC | 9.17 ms | 9.65 ms | 11.44 ms |

Apply to publish through the real loop (`Daemon::from_config`, 4 worker
threads, derive under one write guard then wake, as `Memory` does):

| load | derive → published | some concept unscored | longest unscored stretch |
|---|---|---:|---:|
| 60 isolated derives | p50 6.4 ms, p99 9.0 ms, max 23.0 ms | — | — |
| a derive every 50 ms, 3 s | p50 15.3 ms, max 17.0 ms | 4.5% of samples | 16.9 ms |
| every 10 ms | p50 9.6 ms, max 11.8 ms | 44.4% | 43.7 ms |
| every 2 ms | p50 7.6 ms, max 13.9 ms | 99.9% | the whole 3 s |

So each concept is scored within one or two cycles, but under a burst
faster than a cycle (about 10 ms here) some concept is always unscored.
"Some concept unscored" is an upper bound on cold recalls: with M1 a
recall goes cold only when that concept would be shown. While GC sweeps it
holds `graph.write()`, so a recall blocks for it rather than going cold.
`cold_start` on the ledger line makes the real rate measurable.

A rescore that panicked used to mark its epoch done before running, so the
table stayed stale until the next write, which in an idle session kept
such recalls cold indefinitely. `last_epoch` is now set after the publish,
and the next cycle retries (review L1).

Rejected narrower variants:

- *Query-only among the unscored hits and their direct competitors.*
  "Competitor" has no clean definition: any scored hit between them in
  either ordering competes, which is the A/B/F case.
- *Cold only for one daemon cycle after a derive.* The window is already
  one cycle, because derive wakes the daemon. A wall-clock bound would add
  clock reads to recall and change nothing measurable.

## Risk (b): leg scale

In cold mode, `q` is the same max-merged value phase 1 already ranks on, so
cold ordering equals phase-1 ordering across all three legs. The hazard
runs the other way: under the *normal* blend, a recent-only hit is recent,
so its daemon recency is high.

- `memory::tests::recall_by::sqlite_cold_text_recall_orders_vector_keyword_and_recent_legs_on_one_scale`
  is a public `Memory` text recall on SQLite with a frozen table. It covers
  two keyword hits (BM25 2.32 and 0.90), a recent-only concept with
  `d = 0.7`, and a fresh unscored vector hit at cosine 0.80. Under the old
  blend the recent hit would score `0.35 + 0.175 = 0.525`, above the fresh
  hit at 0.40. In cold mode it scores 0.175 and ranks below the fresh hit
  and both keyword hits. The keyword hits keep BM25 order, every hit is
  `0.5 × q`, and the list is in non-increasing `q` order.
- `sqlite_public_recall_by_ranks_fresh_supplied_image_and_guards_noise`
  is a recall by vector with no text. A recent, scored text concept gets no
  recent leg (#22 PR 6 skips it) and no daemon share. Every hit is
  `0.5 × cosine`, and the order is fresh relevant 0.85, old relevant 0.75,
  fresh noise 0.68, old noise 0.61.
- `recall::assemble::tests::stale_vector_id_absent_from_the_graph_does_not_start_cold_mode`:
  phase 1 keeps vector ids the graph no longer holds (`candidates::rank`
  does not filter them). Without this test, removing the graph-membership
  check survived mutation.

Neither leg test needed a rule change.

## Risk (c): goldens

- No fixture file changed (`fixtures/recall-goldens.json`,
  `recall-h3-goldens.json`, `recall-context-golden.txt`, tool-schema
  golden).
- I temporarily made cold mode panic and ran all twelve golden tests
  (recall goldens, context golden, H3 blended/structural payloads, phase-2
  membership, keyword-leg goldens, schema goldens). All passed, so none of
  them enters cold mode. Their output is unchanged by construction, because
  `cold == false` is the old formula.
- **One legacy expected output changes.** In `final_score_mixes_daemon_and_relevance_with_planted_weights`,
  c2 was deliberately missing from the table. The wip rewrote that test,
  which lost its pin on the blended formula. It now keeps its original
  golden byte for byte by giving c2 an explicit `0.0`. The missing-c2 case
  moved to `a_missing_phase1_daemon_score_puts_the_planted_fixture_in_cold_mode`:
  order `c1,c2,c5,c6,c4,c3` (`.95,.375,.30,.15,.10,.05`) becomes
  `c1,c2,c5,c3,c4,c6` (`.75,.375,.15,0,0,0`). The structural-only members
  lose their daemon share and tie at zero, so canonical key order decides
  them.

## The flaky graded-cosine test

`store::sqlite::tests::image_e2e::graded_similarity_ranks_by_cosine_not_recency_on_sqlite`
sometimes ranked the 0.3 look above the 0.5 look. #79's rule is **not** the
fix: `settle_daemon` scores every look before the read.

Root cause: `score_concept` computes recency as
`(last_touch - start).num_milliseconds() / span.num_milliseconds()`, both
truncated, and the fixture's whole session lasts about 2 ms. Instrumented
runs showed all three looks at `d = 0.3833` in most runs. Whenever a
millisecond boundary fell between derives, the scores split (for example
0.3833/0.5500/0.6333). The fixture derived the looks best first. A
scheduler stall before the 0.3 look's derive, as happens under a loaded
test run, gave the 0.3 look recency near 1 while the 0.5 look kept 0. That
is +0.25 on `d`, worth +0.104 of the final score at 0.5/0.5, against a 0.1
query gap, so the 0.3 look won.

Fix (`test_util::dresscode::derive_graded_looks`): derive the graded looks
**worst first**. Timestamps are monotone and the looks differ in no other
dimension, so recency can only widen the cosine order. This also covers the
holder and two tier tests that share the fixture. The test keeps the
default blend, so the wip's query-only weights are dropped.
`graded_similarity_survives_a_stall_between_derives_on_sqlite` pins the fix
with a 40 ms stall before the last graded derive. With the old best-first
order it fails 3/3 with the CI symptom (0.3 look 0.4667 over 0.5 look
0.4417). On HEAD the five graded tests passed 20/20.

The millisecond truncation itself is not a production defect: real
sessions span minutes, so the quantum is negligible. Switching to
microseconds would not have fixed the test, because a stall still moves
recency by the same fraction.

## Live EG2 through the real cold path (remediation, review M3)

The original live test computed its cold column as `w_query × q` in the
test, so it could not fail on a regression of the rule. It is replaced by
`memory::tests::live_eg2_cold::live_eg2_cold_start_ranks_the_fresh_matching_image_first`,
in the crate so that `stop_daemon_for_cold_start` can hold the daemon
back. It derives the older images, settles and stops the daemon, derives
the fresh ones, and recalls by the query vector. It asserts `cold_start`,
that every hit scores `w_query × cosine`, that the fresh red image is #1,
and that every relevant image ranks above every noise image. With cold
mode disabled it fails (`cold_start` is false and the order is the old
blend's). Run against my own `llama-server` on port 18311 (stopped after):

| concept | relevant | q | d frozen | old blend (calc) | #79 recall |
|---|---|---:|---:|---:|---:|
| old dark red | yes | 0.7011 | 0.4167 | 0.5589 (#2) | 0.3506 (#2) |
| old blue | no | 0.6486 | 0.4167 | 0.5326 (#3) | 0.3243 (#3) |
| old green | no | 0.6440 | 0.6667 | 0.6554 (#1) | 0.3220 (#4) |
| fresh red | yes | 0.7233 | missing | 0.3616 (#4) | 0.3616 (#1) |
| fresh gray | no | 0.6186 | missing | 0.3093 (#5) | 0.3093 (#5) |

The `d` values differ from the first run's because these are read from the
frozen table directly, and recency follows derive order. The other three
live tests also pass. The live command is now
`cargo test --features embed-eg2,store-sqlite --lib --test live_eg2 --
--ignored --nocapture live_eg2`. Without `store-sqlite`,
`tests/live_eg2.rs::live_eg2_cold_start_needs_store_sqlite` fails and names
that command when a server is configured (review L5), instead of the
cold-path test compiling out unnoticed. The `evidence/issue-22-eg2/` files
are records of the #22 run and keep the command that run used.

## Live EG2 before/after, first run (Q8_0 model and mmproj, llama.cpp b11517)

I ran `tests/live_eg2.rs` against my own `llama-server` on port 18300, with
the flags from `lambo.example.toml`, query "a red square", and default
weights. The frozen `d` comes from the settled recall before the fresh
derives.

| concept | relevant | q | d frozen | pre-#79 cold | #79 cold | settled |
|---|---|---:|---:|---:|---:|---:|
| old dark red | yes | 0.7011 | 0.6667 | 0.6839 (#1) | 0.3506 (#2) | 0.6806 (#2) |
| old blue | no | 0.6486 | 0.6667 | 0.6576 (#2) | 0.3243 (#3) | 0.6543 (#3) |
| old green | no | 0.6440 | 0.6667 | 0.6554 (#3) | 0.3220 (#4) | 0.6520 (#4) |
| fresh red | yes | 0.7233 | missing | 0.3616 (#4) | 0.3616 (#1) | 0.6816 (#1) |
| fresh gray | no | 0.6186 | missing | 0.3093 (#5) | 0.3093 (#5) | 0.6293 (#5) |

The public `Memory` route cannot freeze its daemon, so the two cold
columns are computed from the measured `q` and `d`. The frozen-table
behaviour of the code is asserted in the fixture tests.

The reference-set separation from `live_eg2_text_and_image` also feeds the
rule. At `w_query = 0.5`, text→image relevant min 0.3750 beats irrelevant
max 0.3388, and text→text relevant min 0.4114 beats irrelevant max 0.3328.

Running the new test beside `live_eg2_size_invariance` made that test's
bit-identity assertion fail 3/3. llama-server batches concurrent requests
across its four slots, and a batched embedding is not bit-identical to a
lone one. The live tests share one async lock. It serializes only within
the `live_eg2` binary; the in-crate cold-path test runs in the lib binary,
which cargo never runs beside it, but another client of the same server
still batches.

## What the wip changed after its report

`proposed-79.patch` versus the wip commit: formatting for edition 2024,
and compile fixes:

- `check_server` returns an enum;
- image ids must be `[a-z0-9]` (`validate_image_id`);
- `ContextTolerantEmbedder` is used in the SQLite memory tests;
- `assemble` is `#[cfg(test)]` and the daemon calls `assemble_with_legs`.

There was no behavioural difference in the rule.

## Gates (base 9c63b060, re-baselined; the wip baselines were on fada6e13)

| row | base passed/failed/ignored | branch |
|---|---|---|
| `store-sqlite,fixtures` | 1921/0/4 | 1934/0/4 |
| `--all --features fixtures` | 1743/0/4 | 1750/0/4 |
| `--no-default-features --features store-sqlite` | 1043/0/0 | 1049/0/0 |
| `--no-default-features --features store-postgres` | 1001/0/14 | 1007/0/14 |
| `embed-eg2` | 1736/0/7 | 1743/0/7 |
| `recall-elastic,store-sqlite,fixtures` | 2005/0/4 | 2018/0/4 |

No test was lost, every added test is accounted for by name, and ignores
are unchanged. Clippy `-D warnings` is clean on default,
`store-sqlite,fixtures`, `ship,fixtures`, `embed-eg2`,
`embed-eg2,store-sqlite`, no-default `store-postgres`, no-default
`store-postgres,store-sqlite,fixtures` and
`recall-elastic,store-sqlite,fixtures`. fmt, the docs mirror check, the CI
vector row, `check --no-default-features` and `check --features demo` also
pass.

## Mutation checks (each one killed by at least one test)

| mutation | killed by |
|---|---|
| disable cold mode | 6 tests (unit, SQLite store, public Memory text and image) |
| trigger ignores leg provenance | recent-only unit test, public recent-only keyword test |
| explicit 0.0 treated as missing | explicit-zero unit test |
| trigger ignores graph membership | stale-vector-id unit test (added; survived before) |
| cold score `q` without `w_query` | 5 tests |
| `w_query = 0` enters cold mode | daemon-only unit test |
| graded looks best first, with stall | stall regression test (3/3) |

## Opus review remediation (2026-10-10)

Review: `scratchpad/79/review-opus.md` (no High; M1-M3, L1-L5, I1-I5).
Merged origin/main first (#91, #93, #94; no conflicts) and re-baselined.

| finding | fix | test (mutation-checked: fails without the fix) |
|---|---|---|
| M1 trigger on unshowable candidates | trigger only when query-only order would emit the fresh concept; `rank_members` and `emitted` shared with assembly | `unscored_hit_below_the_top_k_cut_keeps_the_blend` (both directions), `unscored_hot_hit_below_the_top_k_cut_still_starts_cold_mode` |
| M2(a) unobservable | `DetailedRecall::cold_start` (serde-skipped like `legs`), ledger key `cold_start`, always present; additive per the ledger rule that consumers ignore unknown keys, `v` stays 1 | `i1_the_recall_line_reports_cold_start`, live ledger line asserts a boolean, assemble and public SQLite tests assert the flag |
| M2(b) / L1 rescore panic | `last_epoch` set after the publish; test-only `Daemon::fail_next_rescores` | `a_failed_rescore_is_retried_on_the_next_cycle` |
| M2(c) / I5 window | measured (above); api.mdx (both copies) and CHANGELOG corrected, 1.18-1.55% range | docs |
| M3 live test cannot fail | in-crate live test through the real cold path | fails with cold mode disabled (run live) |
| L2 text test passed without the fix | recent noise derived before the freeze, hazard asserted | fails with cold mode disabled (ordering, not only the flag) |
| L3 stale comment | reworded to the abort-and-join | — |
| L4 reservation and traversal | blend-shown reservation holders force-included while cold; traversal/sibling behaviour documented | `cold_mode_keeps_a_reservation_holder_the_blend_would_show` (two mutations) |
| L5 / open question | live command `embed-eg2,store-sqlite` everywhere it is documented; loud stand-in without `store-sqlite`; #22 note row updated | stand-in fails with a server set, skips without |

I2 (recency normalised over the session span) is left for its own issue.
#87 is untouched.

### Gates (base: origin/main merged into the branch, before remediation)

| row | before | after | delta |
|---|---|---|---|
| `store-sqlite,fixtures` | 1975/0/4 | 1980/0/4 | +5 |
| `--all --features fixtures` | 1789/0/4 | 1794/0/4 | +5 |
| `--no-default-features --features store-sqlite` | 1049/0/0 | 1053/0/0 | +4 |
| `--no-default-features --features store-postgres` | 1007/0/14 | 1011/0/14 | +4 |
| `embed-eg2` | 1782/0/7 | 1787/0/8 | +5, +1 ignored |
| `recall-elastic,store-sqlite,fixtures` | 2059/0/4 | 2064/0/4 | +5 |

The +5 are the three assemble tests, the rescore-retry test and the ledger
test; the no-default rows do not build the MCP ledger test. The +1 ignored
is `live_eg2_cold_start_needs_store_sqlite`. fmt, clippy `-D warnings` on
default, `store-sqlite,fixtures`, `ship,fixtures`, `embed-eg2`,
`embed-eg2,store-sqlite`, no-default `store-postgres,store-sqlite,fixtures`
and `recall-elastic,store-sqlite,fixtures`, the docs mirror check, the CI
vector row, and the graded `image_e2e` tests 20/20 all pass. The live EG2
run passes (4 tests). Postgres and Cockroach live legs were not run.
