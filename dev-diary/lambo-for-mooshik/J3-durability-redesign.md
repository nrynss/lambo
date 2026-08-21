# J3 durability redesign — durable intents, and estimates that carry theorems

**Status: ADOPTED AND BUILT** — Part 1 in full, Part 2 deferred with its seam; see
[§As built](#as-built-2026-08-21--part-1-shipped-part-2-deferred-with-its-seam) at the
end for the implementation record, the open-question decisions, and the deviations.
Originally written 2026-08-21 as a proposal while the workstream was paused at J3 round 3
(`wt/j3` @ `ed22476`, REQUEST_CHANGES, 2 P1), commissioned by the operator ("we should
think of something more durable — maybe apply some new math"). Where this doc and the
implementation disagree, the implementation's review decides.

---

## The evidence: five falsified axes in three rounds

J3's async-ack implementation has produced a P1 in every review round, and every P1 is
the same defect through a different door — an *estimate* of drain capacity, falsified by
an axis of the workload the estimate did not sample:

| round | axis | mechanism | measured damage |
| --- | --- | --- | --- |
| 1 | **width** | 4-wide probe throughput vs 1-wide lane drain | 61/80 acked writes abandoned at clean close |
| 2 | **warmth** | one-shot probe reads 21–150 items/s on one host | 7× swing, all `measured: true` |
| 2 | **length** | 35-byte probe text vs 700–1500 B real concepts | 4.0× optimism vs 2× slack; 35/77 abandoned |
| 3 | **failure shape** | embedder refusal returns `Ok` + `embedding=NULL`, sampled as a 3 ms "write" | rate inflated to 364–852 items/s; 326/361 abandoned |
| 3 | **concurrency scaling** | aggregate ceiling derived per-lane, applied across lanes | ≥8 agents → 13/16 abandoned |

Each round's fix was competent and each was falsified on the next axis. The series does
not converge, because an estimator is wrong in as many ways as the workload has
covariates — nonstationarity and remote-embedder tail latency are visibly next in line.

**The structural error:** the durability invariant — *no acked write is ever silently
abandoned* — is **coupled to the estimator's correctness**. While that coupling exists,
every estimation error is a P1. Round 3's own evidence points at the cut: receipts
stayed honest through every failure (`restart_lost` after restart), so the only broken
thing is that a *clean close* has a deadline and the deadline's arithmetic rests on an
estimate.

---

## Part 1 — the load-bearing fix: durable post-validation intents

On ack, persist the job itself — validated concepts, interaction id, receipt id, lane
sequence — as a small **intent record**, written through the *existing write-behind
path*, which the C-series concurrency capture already proved drains durably at clean
close ("session closed, tail durable").

Consequences, in order of importance:

1. **Clean close stops being an embedding deadline.** The drain applies what fits in its
   budget; whatever remains is durable *as an intent*. Acked ⇒ (applied ∨ intent
   durable) at clean close **by construction** — independent of any estimate being
   right. All five axes above, and any future axis, demote from durability P1s to
   fairness/latency tuning.
2. **Replay.** The next serve (same session) replays unconsumed intents before serving:
   per-lane order preserved via the recorded sequence; insertion idempotent per receipt
   id; the intent is marked consumed **in the same transaction** as the flush of the
   resulting mutation, so a crash mid-replay re-replays rather than double-applies.
   Receipts gain one honest state: `applied-after-restart`.
3. **The crash window is unchanged.** A `kill -9` loses intents exactly as it loses
   today's write-behind tail; receipts say `restart_lost`. §J3 already accepts this as
   "widened but not new".
4. **Why the J-doc's rejection of intent queues does not apply.** §J2 rejected a durable
   write-intent queue because "validation needs the graph" — that argument was about
   *losers* queueing unvalidated intents in a process holding no graph. These intents
   are written **in the holder, after validation passed**. The doc explicitly kept the
   idea "in reserve for the holder-restart window, where nothing else helps" — and the
   close-drain abandonment *is* the holder-stop window. This cashes that reservation; it
   does not overturn the decision.
5. **What it simplifies.** The probe/ceiling apparatus (7 constants, 3 build guards, the
   representative leg, the ceilings) stops being load-bearing and becomes telemetry.
   Admission control survives only for memory pressure and fairness, where being wrong
   costs a refusal or a replay, never a loss. The close-drain becomes an online knapsack
   where even a greedy policy is safe, because "rejected from the knapsack" means
   "persisted as an intent".

**Independent of the redesign, two round-3 defects need fixing under any design:**

* **J3-R3-1's honesty half.** An embedder refusal must be an `Err`, never an
  applied-with-`NULL`-embedding success — *applied ≠ embedded* becomes a first-class
  receipt distinction, and E2E/durability tests must assert on the store's `embedding`
  column, never on `applied` counts.
* **The dogfood connection.** Silent applied-without-embedding is the located mechanism
  behind the 92/100 unembedded concepts in the dogfood store (DOGFOOD-FINDINGS,
  2026-08-20). The `lambo re-embed` backfill verb graduates from candidate to required —
  it is both the repair for existing damage and workstream A's embedder-migration path.

---

## Part 2 — the math: estimates that carry theorems

What remains estimated after Part 1 is fairness-grade, but the same discipline the
review culture applies to prose — *keep score against reality, act on divergence* — has
exact mathematical forms. Two are worth shipping; three are worth knowing.

### Ship

* **Adaptive conformal inference (ACI) for per-job cost bounds.** Model each job's cost
  (embed time is near-linear in input length; round 2's 1.74×/2.81×/5.07× table is the
  evidence), then wrap the prediction in a conformal upper bound: distribution-free,
  finite-sample coverage — P(true cost ≤ bound) ≥ 1−α with **no** distributional
  assumptions — and under ACI the miscoverage feedback retunes α_t online, so the bound
  *provably re-tracks under distribution shift*. Every falsified axis was a violation of
  "the sample represents the workload"; conformal is the statistics of exactly that.
  Admission: admit while Σ(conformal bounds over queued jobs) ≤ budget share. Replaces
  the share constant, the ceilings, and any residual-quantile margin with one parameter
  that carries a theorem.
* **An e-process (test martingale) on calibration divergence.** Round 3's most damning
  observation: `probe_optimism` read an *impossible* 0.02–0.05 and nothing acted on it.
  An e-process accumulates evidence against "the calibration is honest" as a nonnegative
  martingale, valid at **any** stopping time — a serve loop peeks continuously, and
  anytime-validity is the property that makes continuous peeking sound. Crossing the
  threshold licenses a circuit-breaker: drop to conservative admission, recalibrate,
  log the event. Self-diagnosis becomes an actuated control, not a stats key.

### Know (adopt if the implementation wants them)

* **Freedman's inequality** to *derive* the drain-budget share from tracked residual
  variance, with failure probability as the named parameter — no more chosen constants
  whose stated reasons become findings.
* **Kaplan–Meier over right-censored receipts** for retention: a still-running job at
  expiry-check time is a censored observation, not an absent one (round 1's
  expire-while-running bug was precisely a censoring error). Retention becomes the age
  at which the survival curve says a job is dead. Reusable for reservation TTLs.
* **t-digest** for the quantile/variance plumbing (mergeable, bounded memory,
  relative-error guarantees) instead of sorted vectors.

---

## Proof obligations for the implementation review

The eventual round-1 reviewer should attack these, not re-attack estimation:

1. Acked ⇒ (applied ∨ durable intent) at clean close, demonstrated at the release
   binary with **realistic concept sizes (700–1500 B and larger) and multi-agent
   bursts** — sub-probe-size single-agent content is the regime where this class hides.
2. Replay: idempotent under crash-during-replay (kill −9 mid-replay, restart, count);
   per-lane order across restart; embedding contract enforced at replay time; the
   receipt truth table including `applied-after-restart`.
3. Applied ≠ embedded: an embedder refusal is an `Err` receipt, and every durability
   assertion reads the `embedding` column.
4. The e-process breaker fires on a constructed calibration lie (a scripted embedder
   that speeds up 10× mid-session) and the system demonstrably tightens admission.
5. The intent records ride the ledger (J4's completion-line schema is the natural
   vehicle) so metric 2 regains its facts.

## Open questions for the design review

* Intent record placement: a new mutation kind in the write-behind log, or a sibling
  table? (The former reuses drain guarantees wholesale; the latter isolates schema.)
* Replay throttling: a restart with a deep intent backlog must not starve the fresh
  session's first calls — same admission math, applied to replay.
* Receipt retention across restart: `applied-after-restart` needs the receipt store to
  survive the restart too, or the answer degrades to `restart_lost` even though the
  write applied. Decide which store owns receipts.
* Does the proxy need to know? (Believed no: intents are holder-internal; the proxy's
  -32002 wording already says "outcome UNKNOWN — recall before re-deriving".)

---

## As built (2026-08-21) — Part 1 shipped, Part 2 deferred with its seam

Implemented on `wt/j3` in five staged commits (`8605f46` the honesty fix, `e7ff6f2`
durable intents, `9e48dca` the estimator demotion, `2bba0e9` the proof obligations at the
binary, plus the docs commit carrying this section). The narrative of record — findings
closed, live-binary numbers, register sweep, gates — is §J3's round-3 section in
`J-multi-client.md`; this section records only what belongs to the design: the
open-question decisions, the deviations, and what of Part 2 shipped.

### The open questions, decided

* **Intent placement → a new mutation kind in the write-behind log**
  (`Mutation::PutWriteIntent` / `Mutation::ConsumeWriteIntent`), not a sibling path. The
  former reuses the drain, the close-time final flush, the fencing token, and the
  batch-is-one-transaction property wholesale — and that last one turned out to be the
  load-bearing half: consumption is appended **inside the same graph-lock critical section
  as the commit** (a `CommitHook` at hybrid's epoch-checked commit; inline under the guard
  for canonical/action), and since the flush drain takes that same lock, apply + consume
  always travel in one store transaction. That is the whole idempotency argument: a crash
  can never leave a write durable beside its unconsumed intent. Schema isolation was had
  anyway — a `write_intents` table in both SQL adapters, snapshot rows in `MemoryStore`.
* **Replay throttling → sequential background replay, at most one write in flight, not
  through admission.** The doc's "same admission math, applied to replay" was considered
  and rejected at the site: admission-routed replay pre-fills lanes and answers
  `lane_full` to the very calls the restart interrupted — precisely the starvation the
  question worries about. A replayed intent already paid for admission in the session that
  acked it. Cost accepted and documented: a fresh write can land before a replayed intent
  from the same agent; cross-restart interleaving is unordered (the concurrent-submission
  scope §Ordering already declares), while order among replayed intents is exact
  (`issued_ms`, `lane_seq`).
* **Receipt-store ownership across restart → the intent record is the receipt store's
  durable half.** Consumed rows are retained for `types::WRITE_INTENT_RETENTION`
  (const-asserted equal to `RECEIPT_RETENTION` — one window, or a receipt's answer would
  depend on whether a restart intervened) and answer `applied_after_restart` / `failed`
  on the original receipt id, agent-scoped; unconsumed rows answer `pending`. Nothing
  else survives: `restart_lost` remains the honest answer for receipts with no record.
* **Does the proxy need to know? → No, verified**: intents are holder-internal,
  `src/mcp/proxy.rs` has no round-3 change, and -32002's wording stays correct for the
  record-less case.

### Deviations from this doc, argued

* **The timeout arm fails the write too.** The doc's honesty clause named the embedder
  *refusal*; the implementation also makes an embed **timeout** an `Err`. Same argument,
  same mechanism: a per-input surprise that used to become applied-with-`NULL` silently.
  The capability-absent arm stays a degrade — a declared, session-uniform configuration.
* **Part 2 is deferred entirely, with its seam** — not for budget but because stage 3
  dissolved its premise: ACI conformal admission and the e-process breaker were specified
  for an admission that still estimates, and after the demotion no estimate gates
  anything. Shipping a theorem to guard a telemetry key would be math for its own sake.
  The seam left ready: the probe/observed rate pair survives, `probe_optimism` is exactly
  the divergence statistic an e-process would consume, and the flip line publishes it. If
  a future workstream re-couples admission to a measurement, Part 2 is the pre-approved
  math and this seam is where it plugs in. ("Know" items likewise: Kaplan–Meier's
  censoring lesson is already structural — unsettled receipts never expire — and Freedman
  / t-digest have no consumer.)
* **Proof obligation 4 (the e-process firing on a constructed lie)** falls with Part 2,
  by the same argument.
* **Proof obligation 5 (intents ride the ledger)** is deferred to J4, whose
  completion-line schema is the vehicle this doc itself names — the same disposition as
  §J3's declared metric-2 regression, and for the same one-append-path reason. The
  `write_intents` table is queryable in the meantime.
* **One prediction of this doc corrected**: "receipts gain one honest state" — they
  gained two. `applied_after_restart` as designed, and `intent_durable`, because the
  closing process itself must answer honestly about a write it is deferring; settling it
  `failed` (the old behaviour) became the lie once the write stopped being lost.

### The invariant at the binary, in one line

Live BGE-M3, release binary, 16 agents × 4 × 1024 B, immediate close: **64 acked == 1
applied-with-embedding + 63 durable intents; replay applies all 63; final store 64
embedded / 0 NULL / 0 unconsumed** (`evidence/mooshik-j3-durable-intents/`). The red
baseline it replaces: 326/361 and 13/16 acked writes abandoned (round 3, `ed22476`).
