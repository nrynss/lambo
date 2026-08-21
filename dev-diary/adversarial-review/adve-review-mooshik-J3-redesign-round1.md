# Adversarial review — mooshik J3 durability redesign, round 1

**Reviewer**: independent adversarial reviewer (Opus 5), agent_id `j3-redesign-reviewer-r1`.
Wrote nothing under review.
**Scope**: the six staged commits `8605f46..66f5aaa` on `wt/j3` (base `867b650`, the round-3
review commit, itself the rebase onto `lambo-for-mooshik`) — `8605f46` (the honesty fix:
an embedder failure fails the write, applied ≠ embedded), `e7ff6f2` (durable
post-validation intents), `9e48dca` (the estimator demotion), `2bba0e9` (proof obligations
at the binary + the live BGE-M3 demonstration), `5ef7038` (§J3, the design doc's as-built
section, both `mcp.mdx` mirrors), `66f5aaa` (the register sweep).
3454 insertions / 874 deletions over 22 files.
**Authorities**: `dev-diary/lambo-for-mooshik/J3-durability-redesign.md` (the design of
record and its five proof obligations), `adve-review-mooshik-J3-round3.md` (the 2 P1 /
1 P2 / 3 P3 this work had to close), `evidence/mooshik-j3-durable-intents/`, and §J3 of
`J-multi-client.md`.
**Worktree**: `/Users/narayan/Documents/work/lambo/.claude/worktrees/j3`, branch `wt/j3`.
No commit amended. Nothing pushed, nothing merged. No verification edit was applied to any
source file: this review's method was read-only at source plus out-of-tree drivers.
**Verdict**: **REQUEST_CHANGES** — 1 P1, 3 P2, 1 P2-candidate (unconfirmed), 6 P3.
The founding invariant holds at the binary. The durable half's *promise of eventual
application* does not.

## Method

*(every row below is something I ran or read, not something the commits claim.)*

1. Recalled the graph twice as instructed (`lambo_recall "J3 durable intents redesign"`,
   `lambo_recall "J3 round 3 findings"`, agent_id `j3-redesign-reviewer-r1`) before reading
   any code: the graph carries the design rationale, the five falsified estimator axes, the
   pause-point ruling and the round-3 P1 mechanisms.
2. Read the design of record in full (233 lines) including its proof obligations, open
   questions and as-built/deviations section; read the round-3 review in full (523 lines);
   read §J3's round-3 section and the `Done when` box in full.
3. Read the source of every write path for the idempotency claim, then the replay path,
   the receipt taxonomy, the admission path and every surviving constant in `src/writeq.rs`.
4. Rebuilt the release binary in its own target dir
   (`--features store-sqlite,embed-bge`, `LAMBO_GIT_SHA=66f5aaa`) and re-ran the
   implementor's driver plus harder variants against the rig's llama.cpp BGE-M3
   (`127.0.0.1:8080`, `{"status":"ok"}` verified).
5. Re-ran all four gates repo-wide across all test binaries, never the lib line.

**Order of authority.** The design doc is the claim; the source is what ships; the binary
against the live embedder outranks both.

**This review ran in two passes, and the second one was environment-crippled — stated
plainly because it bounds what the verdict rests on.** Partway through pass 1's live phase,
macOS revoked this process tree's access to `/Users/narayan/Documents`. It persists with the
tool sandbox disabled, so it is TCC, not the sandbox. In pass 2 the revocation was still in
force and had tightened: `bash` cannot reach the repository at all (`git`, `cargo`, `rg`,
`ls` all return `Operation not permitted`), and file reads are restricted to a per-file
allowlist. Consequences, itemised:

* **Steps 4 and 5 above were completed in pass 1 and are not re-runnable in pass 2.** The
  gate table and the live-binary results below are pass-1 measurements by this same agent
  id — my own work, not the implementor's claims taken on trust — and they are labelled as
  such wherever they appear.
* **The live matrix could not be pushed further than pass 1 took it** (larger concept
  sizes and higher concurrency beyond 16 × 4 × 1024 B remain unrun; the `kill -9` variant
  *was* run, in pass 1).
* **Five files under review could not be read in pass 2**: `src/types/mod.rs`,
  `src/store/{load,batch,flush,sqlite,cockroach}.rs`, `src/embed/mod.rs`, `src/config.rs`,
  `tests/serve_intent_durability.rs`, `docs/reference/mcp.mdx` and its `site/` mirror.
  Findings resting on them are carried from pass 1 with their provenance marked; the
  `mcp.mdx` byte-identity claim and the test-name reconciliation are **not verified by me**
  and are listed as residuals, not as cleared.
* **This file could be written but not committed.** `git` is unreachable. The review is on
  disk, uncommitted, on `wt/j3`.

---

## Working notes (verified at source; consolidated into the tables below)

### The idempotency proof — traced on every write path

* **Hybrid.** `src/graph/hybrid.rs:986-993`: after `*guard = g;` the `CommitHook` runs
  **under that same write-lock hold**, so `ConsumeWriteIntent` is appended to the very
  mutation-log the commit's own mutations went into, with no lock release between them.
  `src/writeq.rs:1496-1512` builds the hook.
* **Canonical.** `src/writeq.rs:1531-1546`: `let mut g = self.graph.write();` … `graph_derive`
  … `g.consume_write_intent(…)` — one guard, inline. ✓
* **`record_action`.** `src/writeq.rs:1583-1608`: same shape, one guard. ✓
* **No fourth path.** `rg 'consume_write_intent'` returns exactly these three sites plus the
  replay-failure arm (`src/writeq.rs:2749-2759`, which has no commit to ride and says so) —
  no write path was missed.
* **The drain cannot interleave.** `src/store/flush.rs:475-480` takes `graph.write()` for
  the drain only; the guard above therefore excludes it.
* **One transaction, both SQL adapters — read, not taken on trust.**
  `src/store/sqlite.rs:807-889`: one `begin()`, `for step in plan_flush(…) { apply_step }`,
  one `commit()`; any `?` drops `tx` and rolls the whole batch back.
  `src/store/cockroach.rs:2078-2141`: identical shape inside `tx_retry`.
* **The batch is never split by size at the transaction boundary.**
  `src/store/flush.rs:521-523` uses `max_batch` only as a *trigger* (`if !tick &&
  self.pending.len() < self.params.max_batch { return; }`) and then flushes the **whole**
  `pending` buffer in one `flush()` call. `plan_flush`'s `BULK_LIMITS` chunking
  (`src/store/batch.rs:244-254`) splits *statements*, never transactions.
* **Order is preserved through the planner.** `src/store/batch.rs:189-205`: both intent
  mutations fall into the `barrier` arm, which drains the open buckets and emits
  `FlushStep::Single` **in log order**. A `ConsumeWriteIntent` can therefore never be
  planned ahead of its `PutWriteIntent`. This is the half of the argument the design doc
  does not spell out and it is the half that could have been wrong.
* **A `Single`-step failure cannot drop the consume while the apply lands** — on both
  adapters `apply_step(…)?` propagates and the transaction rolls back whole.
* **`Constraint` dead-lettering drops the batch whole** (`src/store/flush.rs:539-559`), so
  apply+consume die together and the intent survives unconsumed → re-replayed. Safe
  direction.
* **Replay idempotency under `tx_retry`.** `put_write_intent` is an
  `ON CONFLICT (session_id, receipt) DO UPDATE` upsert; `consume_write_intent` is an
  `UPDATE … WHERE session_id AND receipt`, a no-op on an absent row. Both are idempotent
  under a whole-closure retry. ✓

### The admission path after the estimator deletion — clean

* `WritePipeline::admit` (`src/writeq.rs:2078-2088`) consults **only**
  `WRITE_QUEUE_LANE_MAX`, `WRITE_QUEUE_MAX` and `WRITE_QUEUE_MAX_BYTES`. No probe, no EWMA,
  no `Calibration` read on the admission path at all.
* `Calibration::from_rates` (`1080-1099`) hardcodes `lane_bound: WRITE_QUEUE_LANE_MAX` and
  `bound: WRITE_QUEUE_MAX` for **every** source, including `Unmeasured`. There is no path
  by which a rate reaches a bound.
* `await_calibration` is gone; `admit` is not `await`-blocked on the probe (its own
  docstring at `2053-2059` records the deletion and the reason).
* `ObservedRate` (`1796-1828`) and `probe_optimism` (`1120-1127`) feed telemetry only.
* No dead constant survives: `PROBE_CLAMP_RPS` is still consumed (`rate_of` at `1136`,
  `ObservedRate::items_per_sec` at `1824`), `MEASURED_LOCAL_EMBEDDER_RPS` by its build
  guard, and every `PROBE_*` constant by `probe_embedder`. What survives is not dead code —
  it is **stale prose and two stale log lines** (N3) and a stale build-time derivation (N4).

### The receipt truth table — all ten states walked

`ReceiptAnswer` has **ten** variants (`src/writeq.rs:804-841`). Decision order in `lookup`
(`2392-2419`) is: live entry → agent check → restart map → agent check → foreign epoch →
`seq > highest_seq` → else expired. That order is sound: agent scoping precedes every
answer, and the epoch test precedes the sequence test so a foreign id can never be read
against this process's counter.

| answer | tag | reachable at | distinguishable by |
| --- | --- | --- | --- |
| Pending | `pending` | admitted (`2199`); timed-out wait (`2456-2459`); **replay-owed prior-process intent** (`2700`) | the only unsettled answer |
| Applied | `applied` | worker success (`2369`) | carries `AppliedSummary` |
| IntentRecorded | `intent_durable` | `abort_workers` on a still-pending receipt (`2619`) | own tag |
| AppliedAfterRestart | `applied_after_restart` | replay success (`2747`); loaded consumed row, non-`failed` tag (`2702`) | own tag |
| Failed | `failed` | worker `Err` (`2380`); replay `Err` (`2769`); loaded consumed row tag `failed` (`2701`) | own tag |
| Dropped | `dropped` | admission refusal (`2166`) | own tag + the bound that refused |
| Expired | `expired` | this epoch, `seq ≤ highest_seq`, not held (`2419`) | own tag |
| RestartLost | `restart_lost` | foreign epoch, no durable record (`2413`) | own tag |
| NeverIssued | `never_issued` | this epoch, `seq > highest_seq` (`2417`) | own tag |
| Forbidden | `forbidden` | agent mismatch, live (`2397`) or restart map (`2408`) | own tag |

**All ten reachable, all ten distinguishable, no pair conflatable by tag.** Three
attempted conflations that do **not** land: `expired`/`never_issued` (separated by
`highest_seq`, which is only ever written under the receipts lock at `2040-2041` and is
monotone within a process); `expired`/`restart_lost` (separated by epoch, before the
sequence test); `restart_lost`/`applied_after_restart` (separated by whether the durable
record loaded — and honest in both directions). Two that do land, both P3: **N8** (the
`pending` conflation) and **F2**'s expiry asymmetry.

**The `intent_durable`-is-a-promise edge, adjudicated.** Does a caller who saw
`intent_durable` ever learn of a later replay failure? Four cases, and only the first is
clean:

1. Replay succeeds → the next process answers `applied_after_restart` on the original id,
   agent-scoped, for one retention window. **The caller learns.**
2. Replay is refused → the next process answers `failed` with the refusal text
   (`2751-2752`, `2769`). **The caller learns** — but only by asking again, in a *later*
   process, within the window, under the same agent id. Nothing pushes it.
3. The intent never flushed → `restart_lost`. Honest, but it contradicts the
   `intent_durable` the closing process already asserted (**N7**).
4. Nobody reopens the session → the caller never learns. Declared: Done-when limit (2).

So the answer is *yes, conditionally* — and the condition is the caller's own initiative in
a later process. That is acceptable for a pull-based receipt surface. What is **not**
acceptable is the caller-facing sentence: `IntentRecorded::describe()` (`875-880`) says
"the next serve of this session **will apply it**". The docstring twelve lines above
(`823`) already concedes "or `failed`, if the replay is refused", and under N1 the refusal
case is not exotic. The one string a model actually reads is the one that overpromises.

---

## The five proof obligations

| # | Obligation (design doc `129-144`) | Disposition |
| --- | --- | --- |
| 1 | Acked ⇒ (applied ∨ durable intent) at clean close, at the release binary, **realistic sizes and multi-agent bursts** | **DISCHARGED.** Reproduced exactly at a binary I built myself (pass 1), judged at the `embedding` column, not at `applied` counts: 64 acked == 1 applied-with-embedding + 63 durable intents, 0 `embedding IS NULL`. Sizes and concurrency as specified (16 × 4 × 1024 B, in-band per the 700–1500 B dogfood measurement). |
| 2 | Replay: kill −9 idempotent; per-lane order across restart; **embedding contract enforced at replay**; the truth table incl. `applied-after-restart` | **PARTIALLY DISCHARGED.** *Contract*: genuinely discharged **by construction** and re-verified this pass — `hybrid.rs:561-563` runs `existing.ensure_compatible(embedding)?` before any embed, on the same `hybrid::derive` replay uses, and intents carry **text**, not vectors, so a replay re-embeds under the current contract. That is the only sense in which the obligation can be met, and §J3's claim (`2015`) is accurate. *Truth table*: walked above; sound, two P3 conflations. *Order*: N6 — the design doc's "exact" overreaches. *kill −9 mid-replay*: the argument at source is sound (consume rides the commit lock; a `Single`-step failure rolls back whole), and pass 1's `kill -9` run confirmed the nothing-durable case; **the test itself was unreadable this pass** and is a residual. |
| 3 | Applied ≠ embedded: a refusal is an `Err` receipt, every durability assertion reads the `embedding` column | **DISCHARGED.** `AppliedSummary::embedded` is `Some` only for hybrid derive (`786-794` — absent, never zero, and the reason is on the field); `derive_sentence` (`1431-1451`) carries the count into the receipt; the refusal arm is an `Err` (`hybrid.rs:593-612`) and is pinned (`hybrid.rs:2114-2164`). Pass 1's live figures all read the `embedding` column. **But the timeout arm — this branch's deviation — has no test (N2).** |
| 4 | The e-process breaker fires on a constructed calibration lie | **DEFERRED — deferral UPHELD.** The premise genuinely dissolved: ACI admission and the e-process were specified for an admission that estimates, and `admit` no longer reads a rate at all. Shipping a theorem to guard a telemetry key would be math for its own sake, and the doc says so in those words. The seam is real, not rhetorical: the probe/observed pair survives, `probe_optimism` is exactly the divergence statistic, and the flip line publishes it. |
| 5 | Intent records ride the ledger so metric 2 regains its facts | **DEFERRED to J4 — UPHELD**, same one-append-path reason as the already-declared metric-2 regression, and the `write_intents` table is queryable meanwhile. One consequence worth stating that the deferral does not: metric-2 facts (`semantic_merged`, `reinforced`, `edges`) live on the **in-RAM** receipt only. The durable half carries a `summary` sentence, not the fields — so a write that survives as an intent and is replayed loses its metric-2 facts, and a `restart_lost` receipt loses them entirely. |

---

## The round-3 findings this work had to close

| # | Finding | Claimed closed by | My verdict |
| --- | --- | --- | --- |
| **J3-R3-1** (P1) | Refusal returned `Ok` + `embedding=NULL`; the observed rate sampled 3 ms non-writes; 326/361 abandoned | Commit 1 (refusal is `Err` at source) + commit 2 (abandonment gone); estimator half moot under commit 3 | **CLOSED.** All three halves verified independently: the `Err` at `hybrid.rs:593-612` and its test; the `is_ok()` sampling filter at `writeq.rs:2327-2328` whose premise is now true; and abandonment structurally gone (`abort_workers` settles `intent_durable`, not `failed`). Pass 1's refusal probe settled `failed` with **no row written**. |
| **J3-R3-2** (P1) | Aggregate ceiling derived per-lane, applied across lanes; 13/16 abandoned from 8 agents up | Closed **by deletion**, argument recorded at the site | **CLOSED, and closed correctly.** `PROBE_AGGREGATE_CEILING` and its four siblings are gone with the argument recorded in place (`writeq.rs:194-207`); the replacement is a division of two structural constants, per-lane by construction (`222`), enforced per-lane at `2083`. Closure by deletion is the right move and the argument is the strongest prose in the branch. |
| **J3-R3-3** (P2) | The `probe_optimism` "1.14×" claim did not reproduce (1.43×) | Corrected at the claim with the reviewer's figures; the ratio is telemetry now | **CLOSED.** The ratio gates nothing (`1116-1117`), which retires the finding's stakes rather than only its arithmetic. |
| **J3-R3-4** (P3) | The refusal message attributed the bound to a measured rate | `DropReason::describe` rewritten | **CLOSED.** `919-943`: the lane refusal names the per-agent fair share, the queue refusal the memory cap, the byte refusal the payload cap. No refusal claims a measurement. Verified at the string. |
| **J3-R3-5** (P3) | The 22–25 ms misquote, surviving at three siblings | All three | **CLOSED** at the two `writeq.rs` sites I can read (module header `7-9`, `PROBE_CLAMP_RPS` `279-280`) and at §J3's opening line. |
| **J3-R3-6** (P3) | Limit (4) of the `[~]` box inherited limit (3)'s magnitude | The box rewritten wholesale with the redesign's own limits | **NOT CLOSED — the same defect recurs in the rewritten box.** Limits (1), (2) and (3) are stated at their own magnitudes and are honest. Limit (4) is not: it states a replay failure at the magnitude of one poison record ("an embedder **still refusing that content**") when the code's magnitude is the **entire backlog on any embedder outage spanning one attach**. This is N1, and it is the J3-R3-6 lesson failing inside the box that claims to have learned it. |

---

## Deviations from the design of record, adjudicated

| Deviation (as-built `200-225`) | My adjudication |
| --- | --- |
| **The timeout arm fails the write too** — the doc's honesty clause named the *refusal*; the implementation also makes an embed **timeout** an `Err`. "The capability-absent arm stays a degrade — a declared, session-uniform configuration." | **UPHELD on the argument. REJECTED as stated, and REJECTED as tested.** The argument is right and I accept it: a timeout is the same per-input surprise as a refusal, and applied-with-`NULL` is not a lesser success. But (a) the stated boundary is not the boundary in code — `vector_ok` at `hybrid.rs:554` is the **store's** `VECTOR_SEARCH` capability, and there is no arm in which a missing or dead *embedder* degrades (**F3**); (b) the availability consequence is not stated anywhere a user reads (**F3**); (c) the arm the deviation *adds* is the one arm with no test (**N2**). Three of the four things a deviation owes — argument, boundary, blast radius, pin — and only the first is paid. |
| **Part 2 deferred entirely, with its seam** | **UPHELD, without reservation.** This is the right call for the right reason, and the reason is the strongest kind: not budget, but that stage 3 dissolved Part 2's premise. Declining to ship a theorem that would guard a telemetry key is better engineering than shipping it. |
| **Proof obligation 4 falls with Part 2** | **UPHELD**, same argument. |
| **Proof obligation 5 deferred to J4** | **UPHELD**, with the metric-2 consequence stated in the obligations table above. |
| **`intent_durable` — an undeclared second receipt state** ("receipts gain one honest state" → they gained two) | **UPHELD as necessary, and the self-declaration is exactly right.** Settling a deferred write `failed` was the lie the moment the write stopped being lost; a fourth settle class was forced. Declaring the correction against the doc's own prediction is the behaviour the review culture is for. Two reservations, both filed as P3: it is the only answer in the taxonomy that is a **prediction** rather than a record (**N7**), and its caller-facing sentence promises what the code can refuse (**N8**, under N1). |

---

## New findings

### N1 (P1) — a transient embedder outage at attach destroys the entire durable-intent backlog

`spawn_replay` runs unconditionally at session build (`src/memory.rs:904`) with **no
embedder liveness check**. Its failure arm (`src/writeq.rs:2749-2770`) consumes every
refused intent as `failed`, with no retry, no backoff, and — the load-bearing gap — **no
discrimination between a content refusal and an unreachable or timed-out embedder**:

```rust
let answer = match this.ctx.run(&job, Some(stamp)).await {
    Ok(summary)  => { … ReceiptAnswer::AppliedAfterRestart(summary.summary) }
    Err(e) => {
        let why = format!("replay after restart was refused ({e}); nothing was written");
        this.ctx.graph.write().consume_write_intent(intent.receipt.clone(), …"failed"…);
```

That distinction is one this branch **created**: before `8605f46`, an unreachable embedder
degraded to keyword-only and a replay would have *applied* the write; after it, every
embed error and every `HYBRID_IO_TIMEOUT` is an `Err` (`hybrid.rs:593-612`). So the branch
introduced the failure mode and then handled it as if it were the other one.

The consequence: a llama-server that is down, restarting, or merely slow at the moment a
`lambo serve` attaches burns the whole deferred backlog. Pass 1's own green run left **63**
durable intents; a 30-second embedder outage overlapping the next attach settles all 63
`failed` — at roughly 2 ms each, since a refusing llama returns HTTP 500 fast (the figure
the branch itself records at `writeq.rs:2314`). "Acked ⇒ applied ∨ durable intent" survives
literally; the durable half's *reason for existing* does not.

Two amplifiers make this likelier than it sounds, and both come from inside this codebase:

* **A slow embedder is worse than a dead one.** A hanging embedder fails each intent at one
  `HYBRID_IO_TIMEOUT` (30 s, `hybrid.rs:175`), so a 63-intent backlog burns for 31 minutes,
  and `close()` aborting replay part-way leaves a partially-burned backlog.
* **J2's lease arithmetic aims the attach at the outage.** After an abrupt holder death the
  next serve is *refused* for the remaining lease (30–45 s per §J3's own Done-when text at
  `J-multi-client.md:2197`). The attach that finally succeeds is therefore systematically
  displaced into the window in which the environment is still unhealthy — which is exactly
  when a co-located embedder is also still coming back.

And the limit is documented at the wrong magnitude. Done-when limit (4)
(`J-multi-client.md:2263-2266`) reads "an embedder **still refusing that content** at
replay time settles it, mirroring the in-session worker" — the magnitude of one poison
record. The real magnitude is N records on one unrelated dependency outage. This is
J3-R3-6's lesson ("state each limit at its own magnitude") failing inside the box rewritten
to honour it.

I want to be fair about what is *not* wrong: nothing lies. The receipt says `failed` and
"nothing was written", which is true. The design's stated reason for consuming on failure
— not retrying forever — is a real concern and a correct one to have. The defect is that
the fix for "retry forever" was "never retry", with no test for which kind of failure
occurred.

**Remediation, in dependency order:**

1. `src/writeq.rs`, `spawn_replay`, before the loop: one liveness embed of `PROBE_TEXT`
   (35 bytes, chosen precisely because "every embedder accepts it", `writeq.rs:405-406`).
   On failure, `tracing::warn!` and `return` **without consuming anything** — the intents
   stay durable and the next serve tries again. ~12 lines, one existing constant, no schema
   change. This alone closes the mass case.
2. In the failure arm, consume-as-`failed` only for a content-level refusal; for a transport
   error or a timeout, leave the intent unconsumed and `break` the loop. Needs a
   discriminator on the embed error; if none exists, (1) is the whole fix and this is a J4
   follow-on.
3. Bound the retry-forever the design rightly fears: an `attempts` column on
   `write_intents`, consumed-as-`failed` after *k*. This answers the design's own objection
   rather than trading it for a worse one.
4. Restate limit (4) at its true magnitude in §J3's Done-when box, the design doc's as-built
   section, and both `mcp.mdx` mirrors; and soften
   `IntentRecorded::describe()` (`writeq.rs:875-880`) from "will apply it" to "will
   re-attempt it".

### N2 (P2) — the deviation has no test

There is **no timeout-arm test in `src/graph/hybrid.rs`**. `pause()`, `start_paused`,
`advance(` appear nowhere in the file, and `timed out` appears only at the two production
sites (`610`, `643`). The refusal arm *is* pinned, thoroughly, with the reversed-pin
argument recorded in-comment:

```rust
async fn embed_failure_fails_the_write_and_writes_nothing() {
    // J3-R3-1 (reverses this test's previous pin, which asserted the L82-4
    // era behaviour "a dead embedder degrades the write, it does not fail it").
```
(`hybrid.rs:2114-2164` — and it asserts the right four things: `LamboError::Embed`, the
"nothing was written" wording, `node_count() == 1`, and that no contract stamp is bound.)

So the arm that the design doc authorised is pinned and the arm the implementation *added
beyond the doc* is not — and it is the commoner field condition of the two. §J3's register
sweep row for `hybrid.rs` says "the L82-4 test pin reversed with the argument", singular,
which is accurate about what was done and silent about what was not.

**Remediation:** a `HangingEmbedder` (awaits a never-resolving future) plus
`#[tokio::test(start_paused = true)]` — tokio auto-advances a paused clock when all tasks
are idle, so `timeout_at` fires immediately and the test costs no wall time. Assert
`LamboError::Embed`, `"timed out"` in the message, `node_count() == 1`, and
`g.embedding().is_none()`. ~20 lines beside the existing test, reusing its fixtures.

### N3 (P2) — twelve surviving estimator-era stated reasons in `src/writeq.rs`, two of them production log lines

The round-3 register-sweep table (`J-multi-client.md:2092`) names `src/writeq.rs` **first**
and claims it was swept for "every stated reason naming a projection, a ceiling, a share,
or the close's abandonment". The module doc's §Backpressure genuinely was — it is rewritten
well (`writeq.rs:76-107`), and so are `DropReason::describe`, the `Calibration` table and
the deleted-constants note. The item-level docstrings were not, and two of the survivors
are not prose at all:

* **`writeq.rs:1932-1933`** — `tracing::info!`, emitted on **every session start** when the
  probe succeeds: *"write queue: bounds measured on this deployment's embedder — the lane
  bound from the serial leg, the aggregate from the concurrent one"*, with `bound` and
  `lane_bound` fields carrying `WRITE_QUEUE_MAX` and `WRITE_QUEUE_LANE_MAX`. An operator
  reading their own logs is told the bounds came from the probe. They did not.
* **`writeq.rs:1938-1939`** — `tracing::warn!` when the probe fails: *"the bound is the
  unmeasured floor"*. `WRITE_QUEUE_MIN`, the unmeasured floor, was **deleted in this
  branch** — the deletion is recorded eleven hundred lines above at `194-207`. The bound is
  the same static 1024 either way.

This is the class the redesign exists to end. J3-R3-4 was raised and closed for exactly
this on the *receipt* surface; the *log* surface still says it.

Ten docstring/comment survivors, all in `writeq.rs` at `66f5aaa`:

| line | surviving claim |
| --- | --- |
| `232` | `WRITE_QUEUE_MAX`'s headline: "Upper clamp on **the measured bound**" — it is the bound |
| `315-316` | the `PROBE_CLAMP_RPS` build-assert **message**: "or the queue bound stops being a per-deployment measurement and **becomes a constant**" — which is now the intended state |
| `336-337` | `PROBE_CONCURRENCY`: "It **sizes the aggregate bound only** — the per-lane bound comes from the serial leg" |
| `371` | `OBSERVED_MIN_SAMPLES`: "a probe that failed outright and **floored the bound**" |
| `378-379` | `OBSERVED_EWMA_WEIGHT`: "A weight of 1 would make **the bound** track a single slow write and oscillate" |
| `1915-1917` | `WritePipeline::spawn`: "It is nonetheless **the only source of the bound** — admission awaits its result rather than falling back to a constant" — both halves false |
| `1837-1838` | `lane_outstanding`: "This is the population `Calibration::lane_bound` **bounds**" |
| `2303-2304` | the worker's timing comment: "*is* the serial service time **the admission bound needs**" |
| `2876-2877` | `probe_embedder` leg 2: "what `Calibration::lane_bound` is **projected from**" — `project()` was deleted |
| `2887-2888` | `probe_embedder` leg 4: "for the **aggregate bound**" |

**Remediation:** mechanical, one pass, one file. The two log lines must state the real
provenance — "bounds are static (lane 64 / queue 1024); the rates below are telemetry" —
and the ten docstrings need the same demotion the module doc already received.

### N4 (P3) — the deleted estimator still sizes both surviving bounds, at build time

`no_rate_can_move_the_bounds` is true of *runtime* rates. It is false of the build:

```
MAX_RETAINED_RECEIPTS = 4096
WRITE_QUEUE_MAX       = MAX_RETAINED_RECEIPTS / 4                    = 1024
WRITE_QUEUE_LANE_MAX  = WRITE_QUEUE_MAX / MAX_CONCURRENT_RECEIPT_WAITS =   64
```

and `MAX_RETAINED_RECEIPTS`'s own stated reason (`writeq.rs:560-573`) is: *"4096 is driven
by `PROBE_CLAMP_RPS > 3 × MEASURED_LOCAL_EMBEDDER_RPS`, which needs `WRITE_QUEUE_MAX ≥ 424`
and therefore `MAX_RETAINED_RECEIPTS ≥ 1696`. The memory budget is the sanity check on
that, not its source."* That inequality is a **live `const_assert`** (`313-317`) against a
constant that is a *measured embedder rate* (`MEASURED_LOCAL_EMBEDDER_RPS = 141`,
`289-302`).

So both magnitudes the brief quotes — 64 and 1024 — trace through a live build assertion to
a measurement of this rig's llama.cpp. The bounds are structural in *kind* and measured in
*magnitude*. A future edit that shrinks the receipt cap for memory reasons fails the build
citing a rationale the branch declares retired. §J3's constants table concedes the guard
"stays" without noticing that it is now the thing doing the sizing.

**Remediation:** invert the stated dependency — derive 4096 from the memory budget the same
docstring already computes honestly (≈31 MiB worst case, corrected in this very branch),
and either delete the clamp guard or restate it as pure telemetry hygiene with no bound in
its message.

### N5 (P3) — the accounting expression drifted from the code it exists to pin

`writeq.rs:117` (module doc, §Accounting): `outstanding = accepted − applied − failed`.
`WriteQueueCounters::outstanding` (`1234-1239`) subtracts a **fourth** term, and its own
docstring insists on it: *"`deferred` IS a term: a close-deferred job settled
`intent_durable` is out of this process's custody without being applied or failed."*

The drift is inside the section titled "the `ledger_queued_lines` lesson, re-derived",
whose entire thesis is that this must be **one expression** which "cannot drift between
them". **Remediation:** one line.

### N6 (P3) — replay order and drain order are different keys, so "exact" overreaches

`next_receipt` (`2037-2043`) samples the clock and `fetch_add`s `seq` **outside** the
`lanes.lock()` that decides drain position (taken at `2080`, enqueue at `2112`). For two
writes one agent has in flight simultaneously, the `(issued_ms, lane_seq)` replay sort can
therefore invert the order the in-session drain would have used — flipping which of two
identical concepts is `created` and which is `matched`, the distinction the receipt reports.

This is a **doc overreach, not an uncovered defect**, and the branch deserves the credit:
for one agent's *sequential* submissions both keys are monotone and the order is exact, and
the simultaneous case is already excluded twice — at `writeq.rs:63-74` and in the Done-when
box at `J-multi-client.md:2278-2280`. What overreaches is the design doc's as-built line
`188-189`: "order among replayed intents is **exact** (`issued_ms`, `lane_seq`)", with no
scope attached, where every neighbouring claim carries one.

**Remediation:** either scope the sentence, or close the window in three lines — move the
clock read and the `fetch_add` inside the `self.receipts.lock()` that `next_receipt`
already takes, which makes the pair monotone by construction.

### N7 (P3) — `intent_durable` is settled before the flush that makes it true

`abort_workers` (`2607-2626`) settles every unsettled receipt `IntentRecorded`, and
`close()` runs the final flush **after** the quiesce — forced, and correctly documented
(`memory.rs:2097-2100`; `quiesce`'s own docstring at `2541-2542` says "the close's final
flush — which runs AFTER this quiesce — persists them"). So the answer is a **prediction**:
alone in the ten-state taxonomy, `intent_durable` asserts a future rather than recording a
past, and if that flush fails the process has told the caller "recorded as a DURABLE INTENT
and the next serve of this session will apply it" about a record that never reached the
store.

Mitigating, and it is real: the contradiction is transient and self-correcting. After a
restart the id has no durable record and answers `restart_lost` — honest. The only
observation window is a `lambo_stats(receipt=…)` racing the close. There is also an elegant
accident worth crediting: if a worker's commit *did* land and the abort beat its settle,
the consumed row makes the next process answer `applied_after_restart`, so the momentary
`intent_durable` self-corrects to the truth from the other direction too.

**But** this is also the mechanism by which **F5** would look healthy: with no
`write_intents` table, every `PutWriteIntent` flush fails and every receipt still reads
`intent_durable`.

**Remediation:** say so where the answer is defined (`823`: "durable as of the mutation
log, pending the close's final flush"), or settle `intent_durable` only after the final
flush reports success.

### N8 (P3) — the truth table's one conflation: two materially different `pending`s

`spawn_replay` seeds unconsumed prior-process intents into the restart map as
`ReceiptAnswer::Pending` (`2700`), and `lookup` returns them (`2406-2411`) with the same
`describe()` as a live admitted write: *"pending — admitted, not yet applied; ask again"*.

They are not the same situation. A live `pending` settles in ~27 ms. A replay-owed
`pending` sits behind a sequential backlog, is interrupted by `close()` (`stop_replay`), is
re-seeded as `pending` by the *next* process, can therefore be a caller's answer across
arbitrarily many processes while its `describe()` says "ask again" — and, under N1, can
settle `failed`. In a taxonomy whose stated principle is that "every non-answer is a
*specific* non-answer" (`2391`), this is the one place two answers share a tag.

**Remediation:** a distinct tag (`pending_replay`), or one clause in `describe()` naming
the backlog.

### N9 (P3) — both narratives of record say five staged commits; the branch has six

Design doc as-built `162-164`: "five staged commits (`8605f46` …, `e7ff6f2` …, `9e48dca` …,
`2bba0e9` …, plus the docs commit carrying this section)". §J3 `1923-1976`: a numbered list
of five, item 5 being "This note." Both omit `66f5aaa` — the **register-sweep** commit.
The commit whose subject is register accuracy is the one missing from the register.

**Remediation:** one word and one list item in each.

---

## Findings carried from pass 1

Verified in pass 1 by this same agent id; where a pass-2 re-verification was possible it is
marked. Nothing here is the implementor's claim taken on trust.

### F1 (P3) — `ReceiptAnswer`'s docstring says "Seven variants"; there are ten
**Re-verified this pass.** `writeq.rs:799` reads "Seven variants, and none of them is
'unknown'"; the enum at `804-841` has ten (Pending, Applied, Failed, AppliedAfterRestart,
IntentRecorded, Dropped, Expired, RestartLost, NeverIssued, Forbidden). It said "Seven" at
eight in the parent `867b650`, so it was already off by one; this branch adds the two new
states and leaves it off by three — on the docstring of the enum the redesign extends, in
the class `66f5aaa` claims to sweep. The Done-when box repeats it: "seven states"
(`J-multi-client.md:2221`). **Remediation:** one word in each, and re-check the sentence
that follows, which names only three of the non-answers.

### F2 (P3) — `WRITE_INTENT_RETENTION` claims a load-time skip that does not exist
*(carried; `types/mod.rs` and the store adapters were unreadable this pass.)*
`src/types/mod.rs:521-527` ends "…and expired rows are **skipped at load**". No such filter
exists: `load_write_intents` is an unfiltered `SELECT … WHERE session_id = ? ORDER BY
issued_ms, lane_seq` in both adapters (`sqlite.rs:1750-1762`, `cockroach.rs:1769-1782`) and
nothing between there and `spawn_replay` filters by age (`load.rs:98-105`,
`memory.rs:751-904`, `writeq.rs:2687-2706`). Purging is **lazy** — inside a *later* consume
step — so a session that goes quiet after a burst keeps its consumed rows indefinitely, and
at the next attach a receipt id far older than `RECEIPT_RETENTION` answers
`applied_after_restart` where the same id in a non-restarted process would have been swept
to `expired`. That is the *opposite* asymmetry from the one the `const_assert` at
`writeq.rs:506-509` exists to prevent. Benign in direction; the stated reason is false.
Note that Done-when limit (2)'s magnitude ("outlives only the retention window of the NEXT
process") inherits the same imprecision. **Remediation:** add
`AND (consumed_at IS NULL OR consumed_at >= ?cutoff)` to both statements, or delete the
clause and say purging is lazy and clocked by the next consume.

### F3 (P2) — the reversed pin's blast radius is wider than "capability-absent stays a degrade" implies
**The boundary was re-verified this pass**; the four supporting references are carried.
The deviation's stated boundary is the *store* capability, not the embedder:
`hybrid.rs:554` is `let vector_ok = store.capabilities().contains(Capabilities::VECTOR_SEARCH);`
and the embed-failure/timeout arms that now `return Err` sit in the `None =>` branch below
it (`588-612`). There is **no arm in which a missing or dead embedder degrades**:

* `sqlite.rs:697-704` advertises `VECTOR_SEARCH` **unconditionally**. *(carried)*
* `embed/mod.rs:260-268` (`build_embedder`) always yields an embedder or a startup error —
  there is no "no embedder configured" state that reaches `hybrid::derive`. *(carried)*
* `EmbedderKind::BgeM3` is `#[default]` (`embed/mod.rs:82-85`) at `http://127.0.0.1:8080`
  (`272-275`). *(carried)*
* `MatchStrategy::Hybrid` is the config default (`config.rs:167`). *(carried)*

So on the **default deployment** a llama that is down or slow means every `lambo_derive`
fails, where before this branch it degraded to keyword-only and the session kept working —
and lambo's own spec §3.2 promises keyword-only as a lawful degraded mode. The probe is
telemetry now, so a failed probe does not refuse startup: the session comes up healthy and
then refuses every write.

*Is it honest?* Yes, per call. Pass 1's live probe returned
`state=failed detail='FAILED, nothing was written — embed: the embedder refused this
content (backend: llama.cpp returned …'` — specific and true.
*Is the blast radius stated anywhere a user reads?* **No.** The Done-when box's four limits
do not mention it; §J3's commit-1 bullet states the boundary as "the capability-absent arm
stays a degrade", which as above is not the embedder. (`mcp.mdx` was unreadable this pass
and is a residual.)
**Remediation:** state the availability consequence in §J3's Done-when limits and both
`mcp.mdx` mirrors — "with `match_strategy = hybrid` (the default), an unreachable or
timing-out embedder fails every write; configure `match_strategy = \"canonical\"` for
keyword-only availability" — and consider whether a *connection-level* failure (the
embedder unreachable for the whole session) deserves the same "declared, session-uniform"
treatment the absent store capability gets, since it is the same kind of fact and not a
per-input surprise. That second half is the same seam N1 needs.

### F4 (P3) — the Cockroach cost is understated: three statements per write, not two, and both intent mutations are batching barriers
*(carried; `cockroach.rs` and `batch.rs` were unreadable this pass.)*
The as-built section names "two extra `Single` mutations per write". Two *mutations*, but
`consume_write_intent` (`cockroach.rs:1735-1762`) issues **two** statements — the `UPDATE`,
then a retention `DELETE … WHERE session_id = $1 AND consumed_at IS NOT NULL AND
consumed_at < $2` — so three extra statements per write inside the flush transaction. And
because both intent mutations land in `plan_flush`'s `barrier` arm (`batch.rs:198-201`),
each **drains the open bulk buckets**, fragmenting the bulk batching L82-1 introduced
precisely to survive a serverless cluster's per-round-trip latency. On sqlite this is free;
on Cockroach every statement is a round trip, and `close()`'s grace window is what L82-1
was fixing. Mitigating: the `DELETE` is a PK-prefix scan (`PRIMARY KEY (session_id,
receipt)`), bounded by one session's retained rows; and `SetEmbedding` was already a barrier
on the hybrid path.
**Dialect read** (no DSN; compile+unit only): the new SQL is dialect-correct —
`STRING`/`UUID`/`INT`/`TIMESTAMPTZ` match the file's conventions,
`ON CONFLICT (session_id, receipt) DO UPDATE SET … excluded.*` is valid CRDB upsert syntax,
`$n` placeholders throughout, and there are **no `::STRING` casts** in the new statements
(nor are any needed — every bind is a typed parameter). Nothing here would fail only live,
with one caveat I could not test: `interaction_id UUID NOT NULL` is bound from
`intent.interaction.0` (a `uuid::Uuid`), which the sqlx Postgres driver maps natively —
correct, but unexercised.
**Risk for B/Mooshik:** the per-write statement cost is unmeasured against a real
serverless cluster. Three extra round trips per write plus two bucket drains, inside the
transaction the close-time flush depends on, is the kind of thing that only shows up as a
blown `CLOSE_FLUSH_GRACE` on a cluster with 40 ms RTT. It is a *risk*, not a finding — but
it is the one item on this branch whose cost nobody has numbers for.
**Remediation:** state the real per-write statement count and the barrier effect in the
as-built section, or move the retention purge off the per-write path (once per flush batch,
or at attach) so consume costs one statement.

### F5 (P2 candidate — observed, mechanism unconfirmed) — an un-migrated store silently voids the founding invariant
*(carried; the confirmation could not be run in either pass.)*
Observed reproducibly while wiring the pass-1 harness: a `serve` pointed at a sqlite file
whose schema predates this change (no `write_intents` table) **starts normally, accepts and
acks 64 `lambo_derive` calls**, and returns receipts — while every `PutWriteIntent` flush
must be failing against a missing table. Nothing refuses the write, nothing refuses
startup, and `FlushTask`'s failure ladder ends in `durability="none"` degradation the ack
surface never mentions. "Acked ⇒ applied ∨ durable intent **by construction**" is silently
void in that configuration, and N7 explains why it looks healthy: every deferred receipt
still reads `intent_durable`.
**Stated honestly: I do not know if this is reachable through the product's own path.** The
likeliest benign explanation is that `init_schema` creates the table with
`IF NOT EXISTS` at every attach, which would mean the harness bypassed it and this
collapses to nothing. `src/store/mod.rs`, the adapters and the migrations were unreadable
in pass 2, and `memory.rs` contains no `init_schema` call site of its own, so I could not
settle it. **This is the first thing a remediation agent should check**, before writing
anything.
**Remediation if it stands:** probe for the table when the write queue is armed and refuse
the async ack path without it (fall back to synchronous writes) rather than acking into a
void.

---

## Attacks that did not land

* **"The batch is split by size, so put/apply/consume can land in different
  transactions."** No — `max_batch` is a *trigger*, not a chunk size; the whole pending
  buffer goes in one `flush()`, and `BULK_LIMITS` splits statements inside the one
  transaction (`flush.rs:521-523`, `sqlite.rs:882-886`).
* **"`plan_flush` could reorder a consume ahead of its put."** No — both are `barrier`
  mutations, emitted as `FlushStep::Single` in log order after draining the buckets
  (`batch.rs:189-205`).
* **"A `Single`-step failure drops the consume while the apply commits."** No — `?`
  propagates and the transaction rolls back whole on both adapters.
* **"A fourth write path was missed."** No — three consume sites, one per strategy/verb,
  plus the replay-failure arm.
* **"The kill −9 case lies."** No. At an immediate `kill -9`, pass 1 measured
  `embedded=0 unconsumed_intents=0 intents=0` — 64 acked writes lost with the tail, exactly
  as Done-when limit (1) declares, because the write-behind tail had not flushed. Nothing
  is durable and nothing claims to be.
* **"Admission still consults the probe or the EWMA."** No — `admit` reads three static
  constants and nothing else (`2078-2088`), and `from_rates` hardcodes the statics for every
  source including `Unmeasured` (`1080-1099`).
* **"A dead constant or a stale guard survived the estimator deletion."** No dead *code* —
  every surviving `PROBE_*` constant has a live consumer. What survived is stale *prose*
  (N3) and a stale build-time derivation (N4).
* **"Replay starves the fresh session's first calls."** No — replay is sequential, one
  write in flight, `yield_now()` between jobs, and deliberately outside admission
  (`2722-2780`), so a backlog cannot occupy a lane and answer `lane_full` to the calls the
  restart interrupted. Replayed jobs also carry `bytes: 0` and never touch the byte cap.
  The design's rejection of "same admission math, applied to replay" is correct and the
  reason it gives is the right one.
* **"Two replayers race if a second serve starts mid-replay."** No, and the mechanism is
  worth naming because it is *not* the loop's own check. `spawn_replay` tests `lease_lost`
  only at the top of each iteration (`2726`), so the in-flight job can complete after the
  fence flips — but a fenced holder's flushes are refused at the store token, so its
  apply+consume never become durable and the intent stays unconsumed for the new holder to
  replay. Exactly-once holds **at the store**, not in RAM. §J3's "lease interplay" bullet
  (`2055-2058`) states this correctly.
* **"A changed embedding contract between runs corrupts a replay."** No — `ensure_compatible`
  gates before any embed on the path replay uses (`hybrid.rs:561-563`), and intents carry
  text rather than vectors, so a replay re-embeds under the current contract. The obligation
  is met in the only sense it can be.
* **"An abort between a worker's commit and its settle loses the outcome."** No — there is
  no `.await` from the completed graph write to `settle_one` (`2355-2358`), so "aborted"
  always means "not written"; and if the commit *did* land, the consumed row makes the next
  process answer `applied_after_restart`.
* **"`expired` can be answered about a running job."** No — `Receipts::expire` and
  `Receipts::evict` both skip unsettled entries outright (`1686-1761`), which makes the
  property structural rather than arithmetical. The vacuous guard J3-R1-7 caught is
  correctly replaced (`254-268`).

---

## Positive observations

* **The idempotency argument is not just correct, it is correct in the half the design doc
  never wrote down.** The doc rests everything on "consume rides the commit lock". The
  unstated half is that `plan_flush` must not reorder the pair, and it does not, because
  both are barrier mutations emitted in log order. That is the half that could have been
  wrong, and it is right at source on both adapters.
* **Closure by deletion, with the argument recorded where the deleted thing lived**
  (`writeq.rs:194-207`) is the strongest engineering move on the branch. Five constants and
  two functions are gone, and the note explains *why no parameter could have worked* rather
  than why these particular ones did not. J3-R3-2 is closed in a way that cannot recur.
* **Declining to ship Part 2 because stage 3 dissolved its premise.** The commissioned math
  was the visible, impressive deliverable; the branch shipped the argument for not shipping
  it instead, and left a seam that is real (the probe/observed pair, `probe_optimism`, the
  flip line) rather than a promise.
* **`intent_durable` was declared against the design doc's own prediction.** "Receipts gain
  one honest state" → "they gained two", stated in the as-built section as a correction to
  the doc rather than absorbed silently. That is exactly the behaviour that makes a design
  doc worth keeping.
* **`AppliedSummary::embedded` is `Some` only where embedding was attempted** (`786-794`),
  with the reason on the field: "an absent key must never read as 'zero of something that
  was attempted'". Applied ≠ embedded is modelled, not just asserted.
* **`deferred` is a fourth settle class, not a subset of `failed`** (`1171-1177`,
  `1224-1239`), with the exclusivity argument re-derived against these counter sites rather
  than inherited. The one place it drifted is a doc line, not the arithmetic (N5).
* **The Done-when box's limits (1), (2) and (3) are stated at their own magnitudes** and
  are honest, including the uncomfortable ones ("a session nobody reopens holds its intents
  indefinitely"; "durable is not applied"). Only limit (4) fails, and that is N1.
* **Unsettled receipts never expire and never evict** (`1686-1761`), turning J3-R1-3's
  arithmetic promise into a structural one — and the docstrings say plainly that the
  previous guard was vacuous rather than quietly replacing it.

---

## Gate results

Pass-1 measurements by this reviewer, tallied across **all 15** test binaries in each gate
(round 3 tallied 14; this change adds `tests/serve_intent_durability.rs`, so 15 is the right
number now and the claim's "repo-wide" figures are consistent with it). **Not re-runnable
in pass 2** — see Method.

| Gate | Claimed | I measured (pass 1) |
| --- | --- | --- |
| `cargo test --all --features fixtures` | 901 / 0 / 3 | **901 / 0 / 3** |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 991 / 0 / 3 | **991 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | 563 / 0 / 0 | **563 / 0 / 0** |
| `scripts/observability/verify.sh` | 46 ok | **46 ok, ALL CHECKS PASSED** |

`cargo fmt --all -- --check` clean. `cargo clippy --all-targets -- -D warnings` clean on
`store-sqlite,fixtures` and on `--no-default-features store-cockroach,embed-fixture`.

**The −1 reconciliation is NOT verified.** Baselines at `ed22476` are 898 / 986 / 564 / 46,
and the claim is that the −1 in the cockroach and lib-side counts is two calibration tests
merged into `no_rate_can_move_the_bounds`. The totals are consistent with that arithmetic,
but a by-**name** set diff — which is what would actually prove no test was silently
dropped — needs `cargo test -- --list` and was not runnable. Residual.

### The live evidence, at a binary I built myself (pass 1)

Own target dir, `LAMBO_GIT_SHA=66f5aaa`, `--features store-sqlite,embed-bge`, llama.cpp
BGE-M3 at `127.0.0.1:8080` (`{"status":"ok"}` verified before the run):

```
refusal probe: state=failed detail='FAILED, nothing was written — embed: the embedder
  refused this content (backend: llama.cpp returned '
session 1: acked=64 in 0.00s; close=2.06s
  store: embedded=1 embedding_NULL_rows=0 unconsumed_intents=63 consumed_applied=1
  INVARIANT acked == applied-with-embedding + durable-intent: 64 == 1 + 63 -> True
session 2: replayed=63; sampled cross-restart receipt state=applied_after_restart; close=0.01s
  store after replay: embedded=64 embedding_NULL_rows=0 unconsumed=0
    applied_after_restart=63 failed=1
OVERALL: PASS
```

Their headline claim is honest in every half, measured at the **embedding column**, at a
binary I built myself.

**Pushed harder — `kill -9` instead of a clean close.** Same shape (16 agents × 4 × 1024 B),
then `SIGKILL` immediately after the burst:

```
acked=64; kill -9 rc=-9
store right after kill -9: embedded=0 unconsumed_intents=0 intents=0
  => acked-(embedded+intents) = 64 LOST WITH THE TAIL
```

Confirms Done-when limit (1) and confirms the implementor's own concession that their
kill-9 test mostly exercises the nothing-durable case: at an immediate `kill -9` *nothing*
is durable, because the write-behind tail has not flushed — the intents die exactly as the
mutations do. No dishonesty; the invariant is declared to hold at a **clean** close only.

**One crash consequence nothing in §J3 states.** After the `kill -9`, the next `serve` on
that session **could not attach** across the whole attempt window I ran. That is J2's lease
fencing behaving as designed, but it has a J3 consequence: **durable-intent replay is
blocked for the remainder of the lease TTL after a crash**, and every receipt in that window
answers `restart_lost`. Honest, but it means "the next serve replays them" is not "the next
serve *attempt* replays them" — and it is the second amplifier under N1. Worth a fifth
limit in the box.

**Not run, and listed as residuals:** concept sizes above 1024 B in the live matrix,
concurrency above 16 agents, and a dead-embedder session at the binary (which is what would
settle F3's and N1's field behaviour end to end).

---

## Verdict

**REQUEST_CHANGES.**

The redesign's central move is right and the central proof holds. Coupling durability to a
transactional write-behind record instead of to an estimator is the correct cut, the
idempotency argument survives adversarial tracing on every write path and both SQL
adapters, the estimator is genuinely gone from admission, and the founding invariant
reproduces exactly at an independently built binary judged at the embedding column. Both
round-3 P1s are closed, one of them by a deletion that cannot recur. Under a `kill -9` the
receipts do not lie.

What fails is the durable half's *promise*. An acked write becomes a durable intent
(verified), and the durable intent is then destroyed — permanently, in bulk, with one
attempt — by a transient outage of a dependency the write does not need in order to be
recorded (N1). The branch created that exposure itself, by making a timeout an `Err`
(deviation 1), and then documented the resulting limit at the magnitude of a single poison
record. That is one P1 by the house standard: the invariant the redesign exists to
establish does not hold in a realistic configuration.

Beside it: the deviation that opened the exposure has no test (N2); the register sweep's
own first-named file still tells operators, in production log lines, that the bounds were
measured on their embedder (N3); and the reversed pin's availability consequence is stated
nowhere a user reads (F3).

Under the zero-residue rule this cannot integrate as it stands. It is close: one P1 with a
twelve-line first remediation, three P2s that are a test, a prose pass and a docs
paragraph, and one unconfirmed P2 candidate whose most likely resolution is that it is
nothing.
