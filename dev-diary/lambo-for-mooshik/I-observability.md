# I — Observability: prove Lambo is working as intended

**Goal:** make every claim about Lambo's behaviour in live use *measurable from artifacts*,
not remembered from conversations.

**Why now, before further workstreams (decided 2026-08-19):** the dogfood rig is live and
the DOGFOOD.md measurement list is already half-unmeasurable. Lambo is strong on **state**
observability — `canonization_events` audits every promotion, `lambo_stats` reports
health, the store itself answers any post-hoc SQL question, `serve-web` renders it — and
weak on **flow**: nothing records the calls agents actually make. The JSONL ledgers in
`evidence/` were written by the loadtest *harnesses*; over MCP there is no serve-side
record of "agent X recalled, then derived, and a blast-radius warning rendered." Fired
warnings currently exist only in the conversation that received them — unverifiable
later, which by this repo's standards means unclaimable later. Every implementation cycle
that runs before I lands is dogfood data lost.

---

## What must become measurable (from DOGFOOD.md, mechanism per metric)

| DOGFOOD metric | Mechanism after I |
| --- | --- |
| 1. Recall-first compliance per agent | ledger: call sequence per agent_id (I1) |
| 2. Re-derivation savings | ledger `matched` counts + store SQL (I1, I3) |
| 3. Duplicate-creation rate under 0.85 | post-hoc cosine scan, scripted (I3) |
| 4. Real query scores vs G1 bands | ledger: recall scores persisted (I1, I3) |
| 5. Blast-radius warnings fired | ledger: warning-rendered flag per recall (I1) |
| 6. Friction | human notes (unchanged) |

---

## I1 — The serve call ledger

`lambo serve --ledger <path>`: append-only JSONL, one line per MCP tool call. Default
**off** — no behaviour change for anyone not asking.

Per line: server timestamp, `agent_id`, tool name, outcome (ok / error kind), duration,
and per-tool payload facts — for `derive`: concepts created / matched / demoted counts;
for `recall`: query, top-k hit ids with their **final scores and per-leg provenance**
(vector cosine, BM25, recent floor — the numbers G needs), and whether a canonical marker,
conflict line, or **blast-radius warning rendered**; for `record_action`: edge counts;
for `reserve`: grant/refusal.

Design constraints, decided here rather than discovered:

* **Observability must never take down memory.** The ledger write is a buffered append
  that logs its own failure once and drops lines rather than failing or delaying the tool
  call. A dropped-line counter goes into `lambo_stats` so silence is visible (no silent
  caps — the same rule the evidence culture already enforces).
* **The ledger is not the store.** No replay semantics, no reads from it in the serve
  path, no schema promises beyond "one JSON object per line, `v` field for versioning."
* Content parity with the store: concept text already lives in the database, so the
  ledger may carry it; the file inherits the store's hygiene rules — lives outside the
  repo (`~/lambo-dogfood/`), curated export only, never Endor-internal content.
* Rotation is the operator's problem (`logrotate`/manual); the serve only appends.

**Depends on:** nothing.

## I2 — Heartbeat lines

The serve appends a `stats` line to the same ledger on a fixed interval (default off;
`--ledger-heartbeat <secs>`): the `lambo_stats` payload plus process uptime and the
binary's version/sha. Gives growth-over-time, GC visibility, flush-lag history, and
"which pinned binary produced this stretch of ledger" for free — the upgrade events
DOGFOOD requires become self-documenting.

**Depends on:** I1.

## I3 — The analysis kit

Committed scripts (`scripts/observability/`), duckdb/jq over the ledger + sqlite, one per
question, each emitting a small report:

* `recall_first.py` — per agent, per work-session: fraction of write sequences preceded
  by a recall (metric 1), from ledger ordering alone.
* `dedup_rate.py` — derive created-vs-matched over time (metric 2), ledger + store.
* `score_bands.py` — real recall score distribution against G1's measured bands and the
  0.35 floor (metric 4); flags any true-hit-below-floor recurrence.
* `warnings.py` — every blast-radius warning fired: when, to whom, over which concept,
  and (join with git log by time window) whether the touching commit happened anyway
  (metric 5 — the honest version of it).
* `duplicates.py` — cosine scan for near-duplicate concepts above/below the merge
  threshold (metric 3), reusing the F evidence probe shape.

No dashboards — `serve-web` already renders state; these are report generators whose
outputs can graduate to `evidence/` through the normal curated path.

**Depends on:** I1 (ledger format), I2 (heartbeats, for the time axis).

---

## Interim, and what I does not replace

Until I lands: Agent Governance in audit-only mode logs calls client-side (fragmented per
client, but real), and daily `lambo_stats` snapshots are manual. After I lands these stay
complementary — Governance sees what the *client* did across all tools; the ledger sees
what *lambo* did with per-leg score detail no client can observe.

**Rig consequence:** I changes `serve`, so landing it means a new pinned binary and a
deliberate dogfood upgrade event (DOGFOOD.md rules) — the first real exercise of the
upgrade path, with the heartbeat's sha field proving it happened.

---

## Done when

- [ ] `--ledger` off by default; on, every tool call appends one line **whose shape
      `duckdb`'s `read_json` can consume** (one flat JSON object per line, no nesting it
      cannot flatten) — **the first two thirds are done and tested; the third is
      reworded.** The original wording was "a full dogfood day parses with `duckdb` end
      to end", and that is not what any artifact here shows: `duckdb` is never invoked in
      the kit, and it *refuses* the committed sample outright, because that sample ends
      in a deliberate torn line. What the suite asserts is the property `read_json`
      needs, and the kit's README now states the recipes' provenance (hand-verified, not
      executed against the sample) rather than implying a run. Closing this box needs
      `duckdb` run over a real day's ledger with the torn tail stripped — which is the
      re-pinned rig, i.e. the box below.
- [x] Recall lines carry final + per-leg scores and the warning-rendered flag; derive
      lines carry created/matched counts
- [x] A ledger-write failure drops lines, logs once, counts in `lambo_stats`, and never
      fails or delays a tool call — tested by making the path unwritable mid-run, and
      (round-1 remediation) by holding the writer inside its write to fill the channel,
      which is the other arm of the same guarantee
- [x] Heartbeat lines carry stats + binary sha; an upgrade shows as a sha change in the
      same file
- [ ] The five analysis scripts run against a real dogfood ledger and each emits its
      report; their outputs are reproducible from committed inputs when exported —
      **all five run and are exercised end to end by `verify.sh`, but against the
      committed fabricated sample, not a real dogfood ledger.** The only ledger in this
      repo is the disclosed-fabricated one; I1's hygiene rule keeps real ledgers outside
      the repo and admits them to `evidence/` only through the curated export path. This
      box was checked on evidence that does not exist and is unchecked rather than
      backfilled: it waits on the rig re-pin below, and closes with an exported artifact
      under `evidence/`, not with a claim.
- [ ] The dogfood rig is re-pinned to a ledger-carrying binary and DOGFOOD.md's
      measurement list points here — **half done, and the remaining half is now
      writable-down rather than only operable:** DOGFOOD.md's measurement list names a
      script per metric, and (round-1 remediation) `DOGFOOD-SETUP.md` §2 sets
      `LAMBO_GIT_SHA` in the build step and §4 carries `--ledger` /
      `--ledger-heartbeat` on the server command and the client blocks — which was in
      scope and missing, and which `src/ledger.rs` claimed that file already did.
      Performing the re-pin is still an operator action on the dogfood machine. The two
      boxes above are blocked on it.

---

## Handoff Log

### I1–I3 implemented (2026-08-19)

**What exists now.** `lambo serve --ledger <path> [--ledger-heartbeat <secs>]`, both
off by default, plus five report generators in
[`scripts/observability/`](../../scripts/observability/) with a README, a fabricated
sample ledger, and a `verify.sh` that asserts each report still finds its planted facts.

* [`src/ledger.rs`](../../src/ledger.rs) — the whole never-block mechanism. `append`
  serializes on the calling thread and `try_send`s to a **dedicated OS thread**, not a
  Tokio task: a hung filesystem must not occupy the worker that would otherwise run
  `Memory::close` on SIGTERM. Every failure is a counted drop (channel full, unopenable
  path, failed write, post-shutdown call), one WARN per failure *run*, and `shutdown` is
  bounded at 500 ms.
* Per-leg recall provenance is the one thing that needed real threading, because it did
  not exist. `candidates()` max-merged the three legs into a single `f64` and threw the
  components away; the leg *names* were tracked, but only under a TRACE subscriber and
  without the numbers. `candidates_with_legs()` now collects `LegScores { keyword,
  recent, vector }` unconditionally, `candidates()` is its projection (so the ranking
  and the provenance cannot describe different arithmetic), and the map rides
  `RecallPipeline` → `DetailedRecall.legs` (`#[serde(skip)]` — the H3 wire contract is
  pinned and I1 has no business changing the `serve-web` payload).
* `Memory::recall` is now a projection of a new `pub(crate) recall_detailed`, so
  `lambo_recall` can read the typed H3 annotation kinds and per-leg scores from the same
  single execution its response is built from.
* Per-tool facts reach the ledger through a `tokio::task_local` slot rather than changed
  `*_impl` signatures. That is what makes "off" cost nothing: with no ledger the scope is
  never established, so the fact-building closures never run.

**What surprised us, and is worth not re-deriving.**

1. **`derive` has no `demoted` count and cannot have one.** The task brief and this doc
   both asked for created/matched/**demoted** on derive lines. In this codebase demotion
   is `Memory::demote`'s context-overflow split (not an MCP tool) and the canonization
   task's `Canonical → None` regression (a daemon action). `DeriveOutcome` has no such
   field and `derive` performs no demotion. The lines carry `created`, `matched`,
   `semantic_merged` and `reinforced` instead, and `semantic_merged` is deliberately NOT
   folded into `matched`: a similarity merge adds a decaying `Semantic` edge and does not
   re-upsert the target or add a `Derives` edge, so counting it as re-derivation savings
   would overstate them. Demotions remain audited in `canonization_events`.
2. **The HTTP transport was rebuilding the whole `ToolRouter` per request.**
   `serve_http`'s service factory called `LamboServer::new` on every request, which
   rebuilds every tool's JSON schema — exactly the cost
   `#[tool_handler(router = self.tool_router)]` exists to avoid. It now clones one handle
   built in `serve()`. That was forced by I1 (all requests must share one ledger, and the
   heartbeat's uptime must be the session's, not a request's) but it is a straight
   improvement independent of the ledger.
3. **The writer reopens the file per batch, on purpose.** It makes `logrotate` (or a bare
   `mv`) work with no signal handling, and it is what makes "the path became unwritable
   mid-run" a condition the code can observe at all. The mid-run test removes the
   ledger's *directory* rather than `chmod`-ing it, because that fails the same way for
   root — some CI containers are.
4. **`git_sha` is `option_env!`, not a `build.rs`.** Least machinery that satisfies "an
   upgrade shows as a sha change", per the brief. The consequence must be operated
   around: without `LAMBO_GIT_SHA` set at build time, two builds of the same crate
   version both report `"unknown"` and the upgrade is invisible. The rig's build step has
   to set it; `DOGFOOD.md` now says so.
5. **The committed analysis scripts are stdlib Python, not duckdb.** The acceptance
   criterion is that they *run*, and a generator that needs a `pip install` first does not
   run on the box where the ledger lives. duckdb/jq recipes for the ad-hoc questions are
   in the kit's README, **hand-verified and not executed against the committed sample** —
   `duckdb` refuses that file, because it ends in a deliberate torn line. The README now
   says so where the recipes are, and the Done-when box is reworded to the property the
   suite actually asserts.
6. **The leg-provenance map is retained on `RecallPipeline` and cloned on every
   recall-cache interaction.** The parent dropped the leg map as soon as the merged
   scores were computed; it now lives as long as the pipeline result does, at roughly
   48 bytes per phase-1 candidate against the parent's dropped 8-byte-per-entry map.
   Bounded by the phase-1 candidate count (keyword over-fetch × `top_k`, plus the recent
   and vector legs), so it is almost certainly immaterial — but it is **unmeasured**, and
   was unmentioned until round-1 review pointed at it. If a recall-latency regression ever
   shows up on a large session, this is a thing to measure before it is a thing to
   assume is fine.

**What the next agent should know.** CI does not execute `scripts/**`, so
`scripts/observability/verify.sh` is a manual gate — run it after touching anything in
that folder. The Rust side needed no new CI rows: `serve`, `server`, `ledger` and
`candidates` are all default-feature code already compiled and tested by the existing
`check` job and linted by the `ship-fixtures` and `sqlite-vectors` rows.

### Round-1 review remediation (2026-08-19)

Adversarial review `adve-review-mooshik-I-round1.md` (REQUEST_CHANGES; 5 P2, 7 P3) found
the mechanism sound and the *claims about it* written from intent rather than from
artifacts. All twelve findings are remediated. The four that changed behaviour rather
than prose, and are worth not re-deriving:

1. **`Ledger::open` no longer touches the filesystem; the writer thread probes.** The
   startup probe was a synchronous unbounded `open_for_append` on the runtime's main
   task, reached from `serve()` *after* the single-writer lease is taken and *before* the
   SIGTERM handler is armed. `--ledger <a FIFO with no reader>` therefore wedged startup:
   lease held, no transport, and SIGTERM hit the default disposition, killing the process
   with the tail unflushed — observability taking down memory through the flag that turns
   observability on. The probe is now the writer thread's first act, where a blocking
   `open` parks the one thread the OS-thread design exists to let park. The module's
   failure table distinguishes "cannot open" (a typo: loud, every batch drops) from
   "open blocks" (a filesystem condition: the writer parks, serve serves), and
   `open`'s doc says "never fails **the server**" rather than "never fails".
2. **The drop counter is split, because the channel-full arm was untestable.** Both drop
   causes incremented one `AtomicU64`, so no black-box test could tell backpressure from
   a broken path — and the test named for the channel-full row measured the
   unopenable-path arm instead (`writer_loop` calls `rx.recv()` first, so its receiver is
   never parked by a path it cannot open; an instrumented run showed 0 channel-full drops
   in 3072). Now `dropped_channel_full` / `dropped_write_failed`, both in `lambo_stats`
   beside the unchanged `ledger_dropped_lines` total, and the writer's durable-write step
   is injectable (`BatchSink`) so a test can hold the writer *inside* its write, push past
   `CHANNEL_CAPACITY`, and assert exactly the overflow dropped with `append` returning
   promptly. The split is better operational information anyway: "the filesystem is slow"
   and "the path is wrong" were one number.
3. **`canonical_marker` was reporting a marker the agent never received.** The five
   set-level flags were all computed over every returned hit. That is defensible for four
   of them and wrong for the fifth: the `⚑`/conflict/hot/reservation lines go into the
   flat `warnings` vector for every hit regardless of the token budget and arrive as a
   second text block, but `[canonical]` exists *only* inside a hit's budget-gated block.
   A `max_tokens: 1` recall of a Canonical concept returned an empty context and a ledger
   line saying `canonical_marker: true`. It is now gated on `included_in_context`, the
   asymmetry is documented where the flags are computed and in the kit README's choices
   list, `warnings.py` reports the cut-canonical case (and no longer says "the model never
   saw the block" of a warning that *was* delivered), and the reviewer's probe is a test.
4. **Duplicate vector-leg entries max-merge again.** `candidates_with_legs` assigned
   rather than max-merged, so a duplicate `NodeId` from `vector_candidates` let the last
   entry win: `[(dup, 0.90), (dup, 0.10)]` ranked at 0.1 where the parent ranked it at
   0.9. No shipped adapter emits duplicates and the trait contract does not forbid them —
   B2's adapter is being written against it now, so the tolerance is stated in the trait
   doc and held down by a test rather than left as a property of the current adapters.

Also: the recall `query` is truncated (2000 chars — wider than concept text, because the
reports print the query verbatim and a query is the input under study); the
heartbeat-zero refusal moved into `authorize_ledger` so a library caller gets it and both
ledger configuration errors exit the same way; `reserve` sets its `op`/`granted` facts
*before* `attribution`, so its two early exits stop reporting `op=None granted=None`; the
per-call `agent_id` clone happens only when a ledger is attached; `_ledger.py` acts on `v`
instead of merely recording it (warn, do not refuse — a mixed file's v1 lines are still
good); `parse_ts` truncates chrono's nine-digit fractional seconds, which removes the
pre-3.11 Python floor the kit never declared; `score_bands.py --floor` is gone in favour
of the floor the ledger states; and two Done-when boxes are unchecked with reasons rather
than backfilled with fabricated evidence.
