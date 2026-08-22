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

Implemented on `wt/j3` in **six** staged commits (`8605f46` the honesty fix, `e7ff6f2`
durable intents, `9e48dca` the estimator demotion, `2bba0e9` the proof obligations at the
binary, `5ef7038` the docs commit carrying this section, and `66f5aaa` the register sweep —
which round 1 caught this sentence omitting while claiming five, N9: the commit whose
subject is register accuracy was the one missing from the register). The round-1 review
remediation adds further commits on top; its dispositions are in §J3. The narrative of record — findings
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
  **for one agent's sequential submissions** — the same scope every neighbouring claim
  carries, and the scope this sentence was missing (round-1 N6). Replay sorts on
  (`issued_ms`, `lane_seq`), which `next_receipt` mints; the drain position is decided by
  the `lanes.lock()` **push_back** in a later critical section. For two writes one agent has
  in flight *simultaneously* those are two different keys, so the replay sort can invert the
  order the in-session drain would have used — flipping which of two identical concepts is
  `created` and which is `matched`. Already excluded twice for the same reason
  (`writeq.rs` §Ordering and the Done-when box), so this is a missing scope, not an
  uncovered defect.

  **The review's suggested three-line closure was checked and does not close it**, which is
  why the sentence was scoped instead: moving the clock read and the `fetch_add` inside the
  `receipts.lock()` that `next_receipt` already takes makes `(issued_ms, seq)` mutually
  monotone, but the window is not inside `next_receipt` — it is between minting a receipt
  and reaching the lanes lock, so a thread with the lower `seq` can still be preempted and
  enqueued second. Genuinely closing it means minting the receipt *inside* the
  `graph.write() → lanes.lock()` critical section, i.e. adding `receipts` to that nesting on
  the admission hot path, and buying a documented ordering scope with a new lock-order risk
  is the wrong trade.
* **Receipt-store ownership across restart → the intent record is the receipt store's
  durable half.** Consumed rows are retained for `types::WRITE_INTENT_RETENTION`
  (const-asserted equal to `RECEIPT_RETENTION` — one window, or a receipt's answer would
  depend on whether a restart intervened) and answer `applied_after_restart` / `failed`
  on the original receipt id, agent-scoped; unconsumed rows answer `pending_replay`
  (`pending` until the round-1 review's N8 — the two waits are not the same wait). Nothing
  else survives: `restart_lost` remains the honest answer for receipts with no record.
* **Does the proxy need to know? → No, verified**: intents are holder-internal,
  `src/mcp/proxy.rs` has no round-3 change, and -32002's wording stays correct for the
  record-less case.

### Deviations from this doc, argued

* **The timeout arm fails the write too.** The doc's honesty clause named the embedder
  *refusal*; the implementation also makes an embed **timeout** an `Err`. Same argument,
  same mechanism: a per-input surprise that used to become applied-with-`NULL` silently.
  The capability-absent arm stays a degrade — a declared, session-uniform configuration.
  **Two things this deviation owed and did not pay, both raised at round 1 and both now
  paid.** (a) The *boundary* as stated was wrong: `vector_ok` is the **store's**
  `VECTOR_SEARCH` capability, not the embedder's reachability, and since SQLite advertises
  it unconditionally and `build_embedder` always yields an embedder, there is no arm in
  which a missing or dead *embedder* degrades — so on the default deployment (`hybrid` +
  BGE-M3 at `127.0.0.1:8080`) an unreachable embedder fails **every** write. That
  availability consequence is now stated where a user reads it (§J3's Done-when limits and
  both `mcp.mdx` mirrors) together with the configuration that keeps a session writable
  without an embedder. (b) The arm the deviation *added* — the timeout — had no test; it
  has one now (`an_embed_timeout_fails_the_write_and_writes_nothing`, on a paused clock so
  it costs no wall time).
* **The replay's failure arm distinguishes two failures, where this doc named one**
  (round-1 N1, a P1). The doc said a refused replay consumes its intent as `failed` rather
  than retrying forever, and that is right for a *content* refusal. It is wrong for a
  transient one: as first built, any embedder error consumed the intent, so a llama.cpp
  that was down, restarting or merely slow at the moment of an attach settled the **whole
  backlog** `failed` — a 63-intent backlog at one `HYBRID_IO_TIMEOUT` each — destroying
  acked writes because a dependency the write does not need in order to be *recorded* had
  blinked. The branch created that exposure itself by making a timeout an `Err`, and then
  handled it as if it were the other failure. As built now: a liveness embed of
  `PROBE_TEXT` gates the loop and consumes nothing on failure, and the failure arm consumes
  only `LamboError::Embed` — the class meaning "the embedder answered and refused this
  content" — leaving `EmbedUnavailable`, store and lease failures unconsumed and ending the
  loop. The distinction is a **type**, not a string match: `EmbedError::is_transient`
  classifies at the site that knows the cause and `LamboError::EmbedUnavailable` carries it
  out, on the J1-R2-2 precedent that a class a decision turns on must be a type.
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
* **Two further techniques were pre-approved and then became moot, recorded here so nobody
  goes looking for them in the code.** **Laplace's rule of succession** — `(s+1)/(n+2)` —
  was the right estimator for the cold-count era, where 0 failures in 4 probe samples reads
  as "never fails" and produced round 3's overconfidence; it is unnecessary now for the same
  reason Part 2 is, because there is no cold-count era left to be overconfident in. And the
  **Laplace–Stieltjes / busy-period reading** (Takács, Pollaczek–Khinchine) is the classical
  theory of exactly the question the close-drain asks — "how long until this queue empties,
  given heterogeneous service times" — and was kept as a *cross-check* on a conformal
  admission rule rather than as an implementation, since it needs a parametric service
  distribution, which is the assumption conformal exists to avoid. With admission demoted to
  two static bounds, there is nothing left to cross-check. Both stay in this section rather
  than in the code, which is the correct place for math that a design decision retired.
* **Proof obligation 4 (the e-process firing on a constructed lie)** falls with Part 2,
  by the same argument.
* **Proof obligation 5 (intents ride the ledger)** is deferred to J4, whose
  completion-line schema is the vehicle this doc itself names — the same disposition as
  §J3's declared metric-2 regression, and for the same one-append-path reason. The
  `write_intents` table is queryable in the meantime.
* **The design's step (3) for N1 — an `attempts` column, consumed as `failed` after k —
  was argued down rather than shipped, and this is the deviation with the most in it.** The
  review prescribed it to bound "retry forever"; it bounds it by *destroying* the write
  after k tries, which is a smaller version of the very trade N1 condemned ("the fix for
  retry-forever was never retry"). Three facts make the bound unnecessary here. A record
  no embedder will ever accept is a `LamboError::Embed` and is consumed on the **first**
  attempt, so the poison case never retries at all. A transient failure now **ends** the
  loop instead of churning through it, and the liveness gate means a dead embedder costs
  one embed per attach rather than one timeout per intent — so the per-attach cost is O(1),
  not O(backlog). What is left unbounded is only *how long an owed intent stays owed*
  against a permanently absent embedder, and for that the honest answer is to keep the
  write and make the debt visible, not to delete it: `write_queue_replay_owed` on the stats
  surface, `pending_replay` on every affected receipt, and one warn line per attach naming
  the backlog. A fourth reason is specific to this branch: a new column is exactly the
  schema change the F5 preflight **cannot** see (it checks tables), so adding one now would
  open a fresh un-migrated-store hazard in the one dimension left unguarded.
* **The Cockroach cost this section named is understated, and the real figure is stated
  here rather than corrected away** (round-1 F4). "Two extra `Single` mutations per write" is
  right about mutations and wrong about cost, twice over. First, `consume_write_intent`
  issues **two** statements, not one — the `UPDATE`, then the retention
  `DELETE … WHERE session_id = $1 AND consumed_at IS NOT NULL AND consumed_at < $2` — so the
  per-write cost is **three extra statements**, and on Cockroach every statement is a round
  trip inside the flush transaction. Second, and larger: both intent mutations fall into
  `plan_flush`'s `barrier` arm, which calls `buckets.drain_into` before emitting the step, so
  **each one flushes the open bulk buckets** — fragmenting exactly the multi-row batching
  L82-1 introduced to survive a serverless cluster's per-round-trip latency. Two bucket
  drains per write, in the transaction the close-time flush depends on.

  *Why the purge stays on the per-write path.* Moving it off would remove one of the three
  statements and none of the two drains — the drains are inherent, because a consume must
  ride its apply's transaction in log order, which is the whole idempotency argument. And the
  seams available are worse than the saving: `apply_step` is a free function with no store
  state, so throttling the purge means either changing that signature on the flush hot path
  in both adapters or adding a trait method for an attach-time sweep. Paying either for a
  statement whose cost is a PK-prefix scan bounded by one session's retained rows
  (`PRIMARY KEY (session_id, receipt)`) is the wrong trade; recording the real number is
  not. If the risk below materialises, the barriers are what to attack, not the `DELETE`.

  *The risk for B/Mooshik, unchanged by this remediation because it is a measurement nobody
  has taken.* Three extra statements plus two bucket drains per write, inside the
  close-time flush transaction, against a serverless cluster at ~40 ms RTT, is the shape of
  problem that shows up as a blown `CLOSE_FLUSH_GRACE` and nothing else. Two things bound
  it and one does not. Bounded: nothing is *lost* if that flush is slow or fails — the
  intents that do not make it stay on the log and the receipts stay honest, which is
  precisely what J3 bought; and `SetEmbedding` was already a barrier on the hybrid path, so
  bulk batching was already fragmented one-per-write before this change. Unbounded: the
  absolute close-time latency at scale, which is a number, and nobody has it. F4 does
  **not** move my assessment of the risk — it sharpens the multiplier from 2 to 3 statements
  plus names the drains — and the assessment is that this is the one item on the branch
  whose cost is unmeasured against real Cockroach, which is a *measurement to schedule*
  rather than a change to make blind.
* **One prediction of this doc corrected**: "receipts gain one honest state" — they
  gained two. `applied_after_restart` as designed, and `intent_durable`, because the
  closing process itself must answer honestly about a write it is deferring; settling it
  `failed` (the old behaviour) became the lie once the write stopped being lost.

### The invariant at the binary, in one line

Live BGE-M3, release binary, 16 agents × 4 × 1024 B, immediate close: **64 acked == 1
applied-with-embedding + 63 durable intents; replay applies all 63; final store 64
embedded / 0 NULL / 0 unconsumed** (`evidence/mooshik-j3-durable-intents/`). The red
baseline it replaces: 326/361 and 13/16 acked writes abandoned (round 3, `ed22476`).

---

## Prescribed design for J3-R2R-1 (round 2's P1) — a rule table, not a wildcard

Written 2026-08-21 at the operator's request, as the handoff to a Cockroach-capable machine.
This is the design the next remediation should implement; deviate with argument as usual.

**The defect, precisely.** `src/embed/bge_m3.rs:151-166` has exactly one non-success arm:

```rust
Err(EmbedError::Backend(format!(
    "llama.cpp returned {status} for model {model:?}: {text_body}"
)))
```

Every status that is not 2xx becomes `Backend`, and J3's own classifier reads `Backend` as
*permanent for this input*. So `503 no slot available` — a live embedder under load, the most
ordinary transient there is — is treated as a content refusal, and the replay arm consumes the
intent and **continues the loop**. Round 2 measured 63 of 63 acked, reported-durable writes
destroyed in about a second, with `write_queue_replay_owed` discharged to zero by destruction.
A single wildcard arm re-opened the whole P1 that N1's transport classification had closed.

**The principle to apply — Pāṇini's, not a new invention.** The Aṣṭādhyāyī's discipline is
that no case is handled by default: rules are ordered, a general rule may not apply where a
specific one does, and conflicts resolve by a stated metarule rather than by whichever rule
happened to be reached first. The direct translation here: **an exhaustive, priority-ordered
status table with no wildcard default, and "unrecognised" as its own named class** rather
than a synonym for permanent.

```
transient  (leave the intent durable, break the loop, count the debt)
    connection refused / reset / DNS / TLS failure   -> already Unavailable, unchanged
    408 Request Timeout, 425 Too Early
    429 Too Many Requests
    500 Internal Server Error        (a loaded llama.cpp answers this)
    502 Bad Gateway, 503 Service Unavailable, 504 Gateway Timeout
    509 / 529 / any 5xx not named below
content    (consume as failed — this input will never work)
    400 Bad Request, 413 Payload Too Large, 415, 422 Unprocessable
    (the 3072 B refusal this rig actually produces lands here)
permanent-config (fail the write AND warn loudly; retrying cannot help)
    401, 403      credentials or authorization — an operator must act
    404           wrong URL or model name
unclassified (treat as transient, and log the status that was not recognised)
    everything else, by construction
```

Three properties this must have, each of which the current shape lacks:

1. **No `_ =>` arm that produces `Backend`.** `unclassified` is the catch-all and it is
   conservative: durability is preserved and the operator learns which status the table does
   not know. An unknown status is a gap in *our* table, not a statement about the caller's
   content — and the branch has already paid twice for treating absence of knowledge as
   knowledge.
2. **The class is decided where the status is known** (in the adapter), not re-derived from a
   message string upstream. This is J1-R2-2's standing lesson: a variant is not a cause, and
   string-matching an error text is not classification. `EmbedError` should carry the class,
   not a formatted sentence a caller has to parse.
3. **A termination measure on the loop, Euclid's discipline.** Round 2 asked for a
   "consecutive-failure bound"; the honest form is a non-negative measure that strictly
   decreases on every iteration, so termination is proven rather than hoped. Note *why* the
   `attempts`-column proposal was rightly declined in round 1: it bounded the wrong quantity —
   it decreased the **write's survival**, not the loop's measure. Bound the loop
   (consecutive transient failures within one attach, after which the arm stops and leaves
   the remaining backlog durable), never the write.

**Also in scope for the same commit, because they are the same defect at other sites:**

* **J3-R2R-2** — the classification was wired into the replay arm only; the in-session arm
  (`src/writeq.rs:2487-2496`) still consumes `EmbedUnavailable` as `failed`, which contradicts
  the paragraph this branch added to both `mcp.mdx` mirrors and to Done-when (6) ("an acked
  write waits for an embedder rather than dying with one"). Symmetric handling, or the
  documented promise changes.
* **The self-bounding sentence** the fix wrote about itself ("at most the one intent that met
  the fault, not the backlog") was falsified at the binary. Restate it at the magnitude the
  new table actually delivers.

**Where the ancient reading stops being useful:** Archimedes' two-sided bracketing is the
right shape for the estimator work, and it is already discharged — the WAL made the bracket
unnecessary, which is a better outcome than a better estimate. Wald's sequential probability
ratio test is the principled version of "how many failures before blaming the embedder rather
than the content", minimising intents burned before the decision; it is worth knowing and
**not** worth building before the rule table exists. The rule table is the fix; the rest is
shape.

## Items that want the Cockroach-capable machine

* **F4 — now measured, not scheduled** (`evidence/mooshik-f4-cockroach/README.md`): close-time
  latency against the real cluster (live CockroachDB serverless, fixture embedder) is
  `close_ms ≈ 221 + 249·K`, where K is the durable-intent tail the close flush carries
  (K=10→2718 ms, K=25→6449 ms, K=30→7708 ms; linear fit slope 249.4 ms per durable intent,
  intercept 221 ms). **The budget crosses 8 s at ≈ 31 durable intents, and at ≥ 35 the close
  abandons and loses the tail** (K=35 → 0/35 durable, exit 1; K=400 → 22/400; every lost
  write was acked on the wire). One sentence of the old framing was inverted by the data:
  "Bounded in consequence (a slow or failed close-time flush now loses nothing)" — on the
  **serve** path there is no on-disk WAL, so an abandoned close is followed by process exit
  and the in-memory tail dies with it (`serve.rs`: "the un-flushed tail is LOST").
  The pessimistic barrier model (2 drains + 3 statements per durable-intent write) is
  confirmed, not the benign planned-statement model: the multi-row batching L82-1
  introduced is fragmented at every durable-intent barrier, so a realistic burst over the
  1 s write-behind rate is a *data-loss close*, not a slow one. This is no longer a
  measurement to schedule — the design doc's own "if the risk below materialises, the
  barriers are what to attack" is now the standing recommendation: attack `plan_flush`'s
  per-intent `bucket.drain_into` so durable-intent mutations stop fragmenting the bulk
  batching.

  **Fix attempted (2026-08-22) and found insufficient — the barriers were not the
  dominant cost.** `plan_flush` was changed to batch `PutWriteIntent`/`ConsumeWriteIntent`
  into multi-row `write_intents` statements instead of emitting a per-intent barrier
  (new `FlushStep::PutIntents`/`ConsumeIntents`; `batch.rs` + both adapters; unit-tested,
  no regression on either adapter's full suite). Re-measuring against the live cluster
  (fixed binary, same driver, K ∈ {30, 50, 100, 200}): the close flush is **unchanged** —
  K=30 is still ≥ 8 s tail-lost (pre-fix it was 7.7 s durable; both sit on the 8 s cliff),
  so the intent savings are swamped. The earlier "249 ms per durable intent" fit was a
  misattribution: K correlates one-for-one with the per-action flood, and the real cost is
  the **aggregate round-trip count** of the tail (≈ 54 mutations per `record_action`,
  ~110 ms effective per statement) — dominated by un-batched `interactions`
  (`BULK_LIMITS.interactions = 1`, one RTT each) plus the concept/edge vector upserts and
  the distributed-txn commit latency. A realistic burst tail (~1 000+ mutations) cannot
  flush inside an 8 s serverless close regardless of the intent barriers.

  **Option 1 — batch `interactions` — SHIPPED and measured (2026-08-22):** raised
  `BULK_LIMITS.interactions` from 1 → 256 (Cockroach) / 100 (SQLite, 999-bind budget),
  reusing the R1-1 first-position self-FK dedupe that already makes multi-row interactions
  safe; the self-FK was verified against the live cluster (the old "cannot verify without a
  cluster" blocker is gone). One real bug surfaced and was fixed along the way: the new
  batched `write_intents` statements had a double-`VALUES` (the author wrote `VALUES` into
  the prefix and `sqlx::QueryBuilder::push_values` emits it too) — a syntax error that only
  appeared once interactions batched and the flush reached the (last-emitted) intent steps
  before the 8 s timeout. Re-measured against the live cluster with both fixes:

  | K durable intents | before Option 1 | after Option 1 |
  |---|---|---|
  | 30 | ≥ 8 s tail-lost | **1.4 s, 30/30 durable** |
  | 50 | ≥ 8 s | **2.3 s, 50/50** |
  | 100 | ≥ 8 s (0/100) | **2.6 s, 100/100 durable** |
  | 200 | ≥ 8 s | 8.1 s, 154/200 (46 lost) |
  | 400 | ≥ 8 s | 8.2 s, 152/398 |

  So the budget cliff moved from ≈ 31 durable intents to ≈ 150–200; realistic deferred
  tails (≤ 100 intents) now flush in ~2.6 s with ~5 s of headroom and 1:1 durability.
  **Residual:** beyond ~150 intents / ~2 700 mutations the serverless close still cannot
  flush inside 8 s and abandons (tail partially lost). That residual is a rare extreme (a
  burst > ~150 deferred intents landed within seconds of close); the honest next options if
  it must close are (2) fold the per-concept embedding write into the concept upsert
  (bigger; first diagnose the mutation composition — premise still unverified), or (3)
  accept-and-document the serverless close-flush ceiling rather than move the grace budget
  (reviewers rejected budget-moving as masking).

  **Accepted operating range (operator, 2026-08-22).** The branch ships with the
  close-flush guarantee bounded as: **durable-intent tails ≤ ~150 (≈ 2 700 mutations, a
  `record_action`-shaped burst) fit inside `CLOSE_FLUSH_GRACE` with ~5 s headroom at
  K=100**; bursts beyond that on serverless Cockroach may abandon the close and lose part
  of the acked tail (`evidence/mooshik-f4-cockroach/opt1/` for the K=30..400 re-measurement
  after Option 1). This range is **documented, not budget-raised** — moving the 8 s budget
  was rejected as masking. Option 2 (fold per-concept embedding into the concept upsert)
  remains the open lever if a deployment needs a larger guaranteed envelope.

  **What this envelope is NOT for — the bootstrap uses another method (operator,
  2026-08-22).** Mooshik's decade-scale bootstrap ingest will not ride the interactive
  serve path at all, so the K ≤ ~150 envelope is not its constraint and Option 2 is not
  its prerequisite. The serve path is shaped for interactive traffic — per-write acks,
  receipts, the intent WAL, a close budget, a lease per session — and a bulk ingest wants
  none of that: there is no agent awaiting an ack, no second client to coordinate with,
  and no reason to pay per-write intent bookkeeping when the whole run is one resumable
  job. The bootstrap's shape is a **bulk ingest verb**: a single process with exclusive
  store ownership, batch embedding, direct bulk flushes, and checkpoint/resume at file
  granularity — the WAL's job done per checkpoint instead of per write. F4's own
  arithmetic endorses the split: at ~110 ms effective per statement on serverless
  Cockroach, a decade of history through the interactive path was never viable, so the
  bulk path is not an optimisation but the only shape the numbers permit.

  The pieces already exist on the map rather than needing a new subsystem: `seed()` is
  deliberately off-lease (`lease_permits_write` passes when no lease was ever minted —
  the bulk bypass is part of the fencing contract, not a hack around it); K2's `re-embed`
  verb is the same pipeline (bootstrap = re-embed + create-the-concepts); and **D is the
  real blocker**, not throughput — without event time, a bulk ingest produces the
  everything-happened-at-once graph that breaks every temporal gate, which is D's founding
  premise. Sequencing therefore stays D → (K2's plumbing) → a thin bootstrap verb
  composing the three, designed under E/Mooshik planning rather than as a J concern.
* **J3-R2R-3** — F5's column gap, measured at F5's own magnitude: a store missing one column
  attaches, acks, reports `applied=4 degraded=false`, and leaves `concepts=0`, loud only at
  close with exit 1. The judgement call is column preflight versus a stated magnitude, and
  the dialect-aware half (Cockroach's `VECTOR`, `CREATE VECTOR INDEX`, `::STRING`) is better
  settled against a live cluster than by reading SQL.
* **The `preflight_schema` DDL parser** generally — verify on the Cockroach dialect, not only
  SQLite's.

## This work must be integrated into `lambo-for-mooshik` — it is not done until it is

**Where it lives.** All of J3 — six redesign commits, two adversarial reviews, five
remediation commits — is on the branch **`wt/j3`**, pushed to origin so it travels between
machines. The *worktree* at `.claude/worktrees/j3` is a local checkout and does not travel;
the branch is the artifact. On another machine:

```
git fetch origin && git worktree add .claude/worktrees/j3 wt/j3
```

**The integration is owed.** `lambo-for-mooshik` carries J0, J1 and J2; J3 is the only
landed-but-unmerged workstream, and §J3 of `J-multi-client.md` on the *branch* still
describes the pre-redesign state. Until `wt/j3` merges, the branch's J3 story is wrong and every
downstream workstream (J4's ledger states, J5's docs, the E2E cycle, K's second embedder
adapter) builds against a J3 that is not there.

**The rule that gates it (operator, standing).** Nothing integrates carrying open findings of
any grade. Round 2's verdict is REQUEST_CHANGES with 1 P1 / 3 P2 / 5 P3, so the order is:
close every finding → re-gate → round 3 → integrate. When a remediation round runs at all, it
closes that round's P3s too; the orchestrator's fix-at-integration judgement applies only to a
round that needs no remediation.

**The procedure, matching how J0/J1/J2 landed:**

```
git -C .claude/worktrees/j3 rebase lambo-for-mooshik   # replay onto the tip
git checkout lambo-for-mooshik && git merge --ff-only wt/j3
git push origin lambo-for-mooshik
git worktree remove .claude/worktrees/j3 && git branch -d wt/j3
git push origin --delete wt/j3                          # the travelling copy is done
```

Note that `wt/j3` is **not** in `ci.yml`'s push trigger (`main`, `master`, `phase/**`,
`lambo-for-mooshik`), so no CI has run against any of this work. The first CI signal arrives
on the integration push — which is also the first time the nine gate invocations run on a
machine that is not this one. Treat a red run there as expected-and-informative rather than
surprising; I-R2-1 was exactly that shape, a race that every fast local machine won and CI
lost.

**Then, and only then:** J4 → J5 → the phase close (the rig re-pin and the DOGFOOD-SETUP
runbook rewrite in one commit, per §"Rig re-pin rides with J's landing") → the E2E adversarial
cycle → K1 → K2 if it clears → D.

---

## As built (round 3) — the R-2 remediation, all nine findings closed

Implemented on `wt/j3` after round 2's REQUEST_CHANGES (1 P1 / 3 P2 / 5 P3). Every finding
is closed at source; the dispositions below record what this doc prescribed and how the
implementation honoured or deviated from it.

| # | Grade | Verdict | What closed it |
|---|---|---|---|
| J3-R2R-1 | P1 | **CLOSED** | `src/embed/bge_m3.rs`: an exhaustive, priority-ordered status-class rule table (`EmbedStatusClass` + `classify_status`) with **no wildcard producing `Backend`**; `unclassified` is conservative (mapped to `Unavailable`, logged). The sequential decision rule (`src/writeq.rs` `EMBEDDER_SICK_THRESHOLD`) bounds the replay loop; the self-bounding sentence restated at its real magnitude. |
| J3-R2R-2 | P2 | **CLOSED** | In-session worker leaves `EmbedUnavailable` intents unconsumed (the write is not destroyed); all three user surfaces (both `mcp.mdx` mirrors + Done-when (6)) corrected to the honest asymmetry; the asymmetry is declared as a deviation below with the J4 seam named. |
| J3-R2R-3 | P2 | **CLOSED** | `columns_in_ddl` + `unprovisioned_column_err` extend `preflight_schema` to columns from the same DDL source, in **both** adapters; verified against the **live Cockroach cluster** (the operator confirmed it reachable on this machine) — see below. |
| J3-R2R-4 | P2 | **CLOSED** | `PendingReplay` added to both taxonomy tests; `only_the_two_pendings_are_unsettled`; `ReceiptAnswer::ordinal` with no `_` arm asserted to yield eleven distinct values (a twelfth variant is a compile error). |
| J3-R2R-5 | P3 | **CLOSED** | Register sweep: `RECEIPT_WAIT_MAX` + const-assert restated on the surviving reason; three `PendingReplay`-falsified sentences fixed; `consumed_at` purge restated as lazy; `memory.rs` `match_strategy` setter fixed. |
| J3-R2R-6 | P3 | **CLOSED** | `PROBE_TEXT`/`PROBE_TEXT_BYTES` measured numbers dropped (the live BGE-M3 embedder was not running on the implementer's machine to re-measure) and the argument kept in shape, grounded in round 2's re-measurement (refusal between 2048–3072 B, not at 1536 B). |
| J3-R2R-7 | P3 | **CLOSED** | Third consequence (call-time validation rule set) added to `MatchStrategy`'s authority; "all three" in `config.rs`; both `api.mdx` mirrors updated. |
| J3-R2R-8 | P3 | **CLOSED** | `write_queue_replay_blocked` stat (`ReplayBlockReason`, `null`/`"embedder"`/`"other"`), set in the liveness-gate return and both breaking arms. |
| J3-R2R-9 | P3 | **CLOSED** | `j3_n1_outage_demo.py` changed from `next(iter(receipts))` to asserting over **every** receipt partitioned by state (deterministic). |

### R-1 — the rule table and the sequential decision rule (threshold, stated)

The rule table (`EmbedStatusClass`) decides the class at the adapter (the site that knows
the status) and collapses to the two `EmbedError` variants, so `is_transient` needed no
change. Transient / unclassified → `Unavailable`; content / permanent-config → `Backend`
permanent-for-this-input (consumed). 401/403/404 are permanent-config: the write is failed
AND the adapter warns loudly; content (400/413/415/422) consumes silently.

The replay-loop termination is **not** a throwaway `break-after-k=3`. It is the
sequential decision rule this doc's §Prescribed design defers until the rule table exists
(Wald's SPRT, p. 410-416). Each replayed intent's status-classifier outcome is a Bernoulli
draw (transient vs content); a **content** rejection is absorbing — consumed as `failed`
immediately, permanent for that input, never counted toward embedder-sickness — and a
success resets the streak (observed health). The sickness evidence is a run of
`EMBEDDER_SICK_THRESHOLD = 3` *consecutive transients with nothing answered in between*.
Stated posture (the two controls):

* **False-alarm tolerance** — we stop only after three consecutive transients with no
  applied success or content refusal between them, so a healthy embedder that blips once or
  twice (or that a content refusal proves is alive) is never wrongly labeled sick.
* **Burn bound** — at most three intents per attach are spent as transient probe-embeds
  before concluding the embedder is sick; none is consumed (all stay durable), so the cost
  is time (≤ 3 × `HYBRID_IO_TIMEOUT`), never durability. When the threshold trips, the
  loop stops and leaves the rest of the backlog durable, and `write_queue_replay_blocked`
  is set to `"embedder"` so an operator sees **wedged**, not draining.

### R-2 — the asymmetry, declared

The replay arm keeps `EmbedUnavailable` intents durable (they have no caller to answer),
as N1 built it. The in-session arm — **decision: leave the intent unconsumed, settle the
receipt `failed`** — was made symmetric with it so an acked write reached during an outage
is not *destroyed*: the caller's receipt honestly says `failed` (nothing was written by
this process), while the durable intent stays unconsumed and the next serve re-attempts it.
The declared asymmetry that remains is *who learns immediately* — the in-session caller has
a receipt to read; the replay has no caller — which is why the receipt is settled here but
the intent is left for a later process. The **J4 seam** for true symmetry: on
`EmbedUnavailable`, re-queue with backoff in-session instead of settling `failed`, or
continue leaving the intent unconsumed for the next serve to replay.

### R-3 — column preflight, verified against live Cockroach

The preferred option (real column preflight, not a stated magnitude) was implemented:
`columns_in_ddl` parses each `CREATE TABLE … ( … )` block plus `ALTER TABLE … ADD COLUMN
` lines (skipping `--`, `CONSTRAINT`, `PRIMARY`, `UNIQUE`, `FOREIGN`, `CHECK`, `INDEX` and
the Cockroach `VECTOR`/`CREATE VECTOR INDEX`/`::STRING` non-table statements), and the
preflight diffs those against `PRAGMA table_info` (SQLite) / `information_schema.columns`
(Cockroach), reusing the table-preflight's refusal shape
(`unprovisioned_column_err`). **Verified against the real Cockroach cluster** (the operator
confirmed `LAMBO_COCKROACH_DSN` is reachable on this machine; the DSN was sourced from
`.env` for the run and never printed or committed): `init_schema`, a passing preflight, a
required column renamed away → preflight refused by table + column name, then renamed back.
The one named follow-up that remained, **F4**, is now measured — close-time latency against
the live cluster blows the 8 s close budget at ≈ 31 durable intents and abandons/loses the
tail at ≥ 35 (see the F4 bullet in "Items that want the Cockroach-capable machine"), so it
is no longer the unmeasured item on this branch.

### R-6 — PROBE_TEXT, numbers dropped rather than restated

The live BGE-M3 embedder at `127.0.0.1:8080` was not running on this machine during round 3
(only a chat llama-server on :8082), so the "re-measure and stamp" option was not possible
without fabricating numbers — which this branch has been burned by. The measured table was
dropped and the argument kept in shape, grounded in round 2's re-measurement (the refusal
sits between 2048 B and 3072 B, not at 1536 B, and the latencies swung by up to 1.6×). A
future machine with the embedder up may re-measure and stamp the table per R-6's first
option.

### Uncertainties / named follow-ups

* **F4** — now measured, resolved (see the F4 bullet above): `close_ms ≈ 221 + 249·K`,
  budget crosses 8 s at K ≈ 31, tail lost at K ≥ 35 — the standing recommendation is to
  attack the `plan_flush` barriers, not to keep scheduling a measurement.

