# Dogfood findings — what Lambo did when Lambo built Lambo

Running log, opened 2026-08-20. [DOGFOOD.md](DOGFOOD.md) is the design and says *"write
these down as they happen, not retrospectively"* — this file is where they get written.
Append dated sections; never rewrite an earlier one (corrections are new entries that name
what they correct). Per DOGFOOD.md's own rule, **nothing here is evidence until exported**:
claims that want to reach `evidence/` or the README go through the normal adversarial
treatment first.

**Rig over the period so far:** pinned binary `3039b82` (2026-08-19, F vector leg + G2
floor) re-pinned to `0f672f1` (2026-08-20, I's ledger); SQLite store + BGE-M3 q8_0 at
`127.0.0.1:8080`, contract `bge_m3/1024`; session `lambo-dev`, owner agent
`claude-orchestrator`; ledger at `~/lambo-dogfood/calls.jsonl`, heartbeat 300s. Workload:
the J workstream itself — J0 (two review rounds, two remediation rounds, integrated) and
J1 (implement + review round 1 + remediation, in flight), roughly ten agent runs across
two days.

---

## 2026-08-20 — first full read of the rig, mid-J1

### The store, by the numbers

`lambo_stats` at time of writing: 124 nodes, 100 concepts, 241 edges, **0 canonical**,
94 canonization cycles, 0 failures, 5,658 daemon cycles, flush_lag ~120s, nothing
dead-lettered, not degraded. Ledger: 33 lines written, 0 dropped, 0 queued.

### Metric 1 — recall-first compliance: the orchestrator is the least compliant agent

`recall_first.py` over the live ledger (16 call lines, 37 heartbeats, 5 serve restarts
splitting sessions):

| agent | write seqs | recall-first | derive-no-recall |
| --- | --- | --- | --- |
| claude-orchestrator | 2 | 0 — **0.0%** | 4 |
| j1-implementor | 1 | 1 — 100% | 0 |
| j1-reviewer-r1 | 1 | 1 — 100% | 0 |

The subagents were *prompted* with a recall-first protocol and followed it exactly. The
orchestrator — the agent that wrote that protocol — derives without recalling, because its
writes are "I already know this, I'm recording it", which is precisely the state of mind
the protocol exists to interrupt. Finding: **compliance came from the prompt, not the
agent's judgement or the tool's affordances.** Nobody used lambo unprompted at all: across
the five J0 agent runs (before the protocol block existed in prompts), zero lambo calls
were made, despite AGENTS.md obliging it and every agent having read AGENTS.md. The
obligation has to travel in the prompt.

### Metric 2 — re-derivation: real saves, and a measured miss pattern

`dedup_rate.py`: 7 write calls in the ledger window, 24 created, 2 matched, **dedup
0.077** (j1-reviewer-r1 the only agent to match: 0.333).

Qualitative saves that the number does not show:
* The J1 implementor's brief was written from recalls (identity decision, J0 catches,
  file:line anchors) in minutes instead of a fresh exploration pass.
* The J0 arithmetic-deviation reasoning (`accepted − written − write_failed`) was in the
  graph the whole time but was only *findable* once the reader knew the key name — a
  topic-level query ranked it too low. This produced the **recall-twice protocol** (topic
  before reading code, targeted recall on real names after), now in every agent prompt.
* The J2 catches (unguarded proxy branch, `authorize_bind` collision) were derived into
  the graph by J0's reviewer *before J2 exists*; J2's brief will inherit them.

The miss pattern is the next entry.

### Metric 3 — duplicates: three pairs above the merge threshold, unmerged, and why

`duplicates.py`: 100 concepts, **only 8 with a durable embedding**, 28 comparable pairs,
3 pairs at or above the 0.85 merge threshold (0.9258, 0.9127, 0.9088) — all three are
same-author near-restatements (the orchestrator deriving the same lesson twice in
different words). Hybrid derive should have matched them; the report's own ordering
caveat applies (a pair cannot merge if the older half had no embedding when the newer
was written), which points at the finding below rather than at the threshold. G1's
prediction that real duplicates land in [0.65, 0.85) has, so far, **zero** pairs in that
band — the observed duplicates are all *above* the threshold and were preventable only if
embeddings had been present.

### Finding: 92 of 100 concepts have no durable embedding — semantic memory is mostly blind

The store's embedded/unembedded split is not a clean outage window: embedded and
unembedded concepts **interleave across both days** (unembedded from 11:10:40Z on the
19th to 04:48Z on the 20th; embedded from 11:21:01Z on the 19th to 04:08Z on the 20th).
By type: all 44 Resources (the `record_action` shape) are unembedded; 13 of 15
Constraints, 14 of 19 Logics, all 14 Entities.

Consequences observed, not hypothesized: recall's vector leg can only ever see 8
concepts; the dedup rate above; the three unmergeable duplicates. The damage is
**cumulative and permanent on current behaviour** — there is no backfill path, so a
concept written while the embedder was unreachable (or via a path that does not embed)
stays invisible to semantic recall forever.

Not yet settled: *why*. Candidate explanations to check against the source and the serve
logs: (a) `record_action` concepts are never embedded by design — plausible for all 44
Resources; (b) intermittent llama-server unavailability degrading hybrid derive to
canonical (keyword) matching, which the degrade warning reports per-call but nothing
aggregates; (c) a flush-path gap for matched-vs-created concepts. (a)+(b) together fit
the data. **Candidate product work either way: an embedding-coverage number in
`lambo_stats` (embedded/total), and a `lambo re-embed` backfill verb.** A silent degrade
that permanently reduces recall quality is exactly the class of failure J exists to make
loud.

### Metric 4 — score bands: G2's floor is holding

`score_bands.py`: 4 recalls, 3 with a vector leg. Observed cosines n=3, median 0.6298,
all within or a hair above G1's true_recall band [0.4599, 0.6577]; mean delta +0.0189.
**Floor masking: 0 occurrences** — no hit had a real cosine discarded by the flat recency
score. Early, tiny n, but G2's lowered floor is doing on real traffic what it was
recalibrated to do.

### Metric 5 — blast radius: structurally silent, and the reason is a C finding

0 warnings fired across all recalls. The report itself says why this is a finding rather
than a pass: **nothing is canonical** — 0 promotions in 94 canonization cycles, because
the swarm promotion policy needs independent multi-agent convergence that a
one-orchestrator-plus-ephemeral-subagents workload never produces. Metric 5 cannot fire
until something promotes. This is the C workstream's motivating claim (spec §3.2)
observed in our own rig: **the dogfood workload is Mooshik-shaped, and the swarm policy
promotes nothing on it.** C's SoloPolicy is not hypothetical product work; our own
dogfood needs it.

### Metric 6 — friction, honestly

* **The J outage remains the biggest catch** (2026-08-19, already in
  [J-multi-client.md](J-multi-client.md)): two clients, documented stdio wiring, one
  lease, the second serve exits 1 with no error reaching the agent. Found within hours of
  first real use — the whole J workstream is dogfood output.
* **Every subagent call carries the attribution warning** (the serve records their work
  as `claude-orchestrator`). Correct pre-J1 behaviour, per-call noise all the same; the
  J1 implementor quoted the warning whose text its own commit deletes. J1 closes this.
* **The ledger only sees what runs under a `--ledger` serve**: 5 restarts and earlier
  un-ledgered sessions mean the store holds 100 concepts while the ledger accounts for 43
  created — `dedup_rate.py`'s store cross-check flags the shortfall honestly
  (`shortfall=-57`). Metrics 1–5 are windows, not totals, until the rig runs ledgered
  continuously.
* **Protocol cost**: the recall/derive protocol added no measurable wall-clock to agent
  cycles (J1 implement: 20 min including 7 derives). The real cost was orchestrator-side:
  designing the protocol and writing briefs that carry it. One-time.
* **Serve babysitting**: none. 5,658 daemon cycles, zero dead-letters, zero drops,
  flush_lag steady ~2 min. The lease + fencing machinery has been invisible, which is
  what it is for.

### What changed in how we work because of the graph (the part git cannot show)

1. Recall-twice protocol (topic, then targeted-by-name after reading code) — from the
   arithmetic-deviation retrieval miss.
2. Derive-at-decision-time — from six watchdog kills teaching that un-persisted analysis
   dies with the agent; the graph is the checkpoint that survives.
3. Graph-as-context-not-instruction, authority order spec → phase doc → source → graph —
   operator decision after pushback on "recorded decisions are settled".
4. Forward-findings as first-class graph objects: J2 has never run, but already owns
   derived constraints its implementor will recall.

---

## 2026-08-20 (later) — correction: the canonization entry overclaimed

The metric-5 entry above says 0 promotions in 94 cycles is "C's motivating claim observed
on our own rig." Operator pushback, sustained: **at this age it is expected under any
policy.** The swarm path wants recurrence across three or more distinct sessions separated
by 24+ hours, and the rig is barely a day old — zero promotions is what a correct policy
produces on a day-old store, and blast-radius silence follows trivially (nothing has been
in the graph long enough to be load-bearing). "Consistent with C's claim" is the most the
data supports.

What survives the correction is the structural half: this rig runs a pre-J1 binary, so
every write from every subagent is attributed to `claude-orchestrator` — from the graph's
side there has only ever been **one agent**, which makes *independent multi-agent
convergence* impossible regardless of elapsed time, not merely slow. That is an
attribution artifact, resolved by the J1 re-pin, and it means the clock on a fair test of
the swarm policy has not started yet.

The fair test, written down before the fact: after the rig re-pins to a J1-carrying
binary, run a week of real sessions with distinct per-call ids. If concepts that genuinely
recur across cycles (the review conventions, the register-sweep rule) have promoted
nothing by **2026-08-27**, that becomes evidence about the policy on this workload; until
then metric 5 is "not yet measurable," not a finding.

---

## 2026-08-20 (later still) — refinement: the corpus is chaff-heavy, so the fair test needs ground truth

Operator observation, accepted: **nothing derived so far deserves canonization or
veneration** — the graph holds regular history and decisions, and a policy that promoted
them would be broken in the opposite direction from the one metric 5 worries about. So the
2026-08-27 fair test cannot be "does anything promote"; it is precision *and* recall
against ground truth named before the fact.

Ground truth, nominated now (the four genuinely canon-shaped invariants in the store —
durable constraints other work provably rests on): (1) the lease stays, a proxy moves the
call not the write; (2) the wedge invariant — never acquire a lease you cannot serve;
(3) the cooperative-identity ruling; (4) the async-ack rule — a write may be acked
asynchronously only when its result does not gate the caller's next action. The test on
2026-08-27: **do these promote while the other ~96 concepts do not.** Either direction
failing is a finding.

Second refinement, on us rather than the policy: **blast radius cannot fire on concepts
nobody linked.** The orchestrator's derives are mostly free-floating prose — almost no
`parent_of` pairs, dependency edges only via `record_action` — so "nothing is
load-bearing" is partly a statement about how we write, not what we know. The wedge
invariant genuinely bears J2's future promotion extension, and it was derived as an
island. Candidate product observation: nothing on the write surface nudges a caller to
declare what a constraint supports; the ergonomics favour islands.

---

## 2026-08-20 — the first agent-run two-product live probe (J2 pre-remediation)

An agent drove **cursor-agent 2026.08.11 + opencode 1.18.18** — two real client products,
project-scoped configs only — against a `bbac803` (J2 implementation) build on a scratch
store. pi 0.84.2 was unusable (no ready generative provider; its `local` provider is
hardwired to the embedding server). Full timeline and per-run artifacts in the session
scratchpad; curated export pending review.

What only a two-product probe could find:

* **Endpoint derivation trusts client-inherited env, and clients disagree** (J2-L1, P2).
  cursor-agent scrubs `TMPDIR` from its MCP child; opencode passes macOS's per-user
  `TMPDIR` through. Same binary, store, session — two derived endpoint directories; the
  losing serve refused to forward ("could reach a socket serving a different graph") and
  opencode declared the server failed at 31.96s. **Cross-client memory silently absent on
  default wiring** — the exact outage J exists to fix, reachable through an env variable
  no same-binary test varies. Routed to the in-flight round-1 remediation.
* **Server-side wait budgets must fit under client spawn gates** (J2-L2, P2): serve's 50s
  lease-lapse wait exceeds opencode's ~32s MCP startup tolerance, converting a
  recoverable wait into "server unavailable, no tools".
* **J2-R1-1 (the round-1 P1) reproduced against real products**: holder killed mid-call →
  proxy detected the closed connection in 18ms, sent the client nothing, client hung
  **121.7s = exactly its configured timeout** (`MCP error -32001`). The idle-death path
  was honest (~2s), and a fresh start self-healed in 43.6s. The round-2 review re-runs
  this same harness on the remediated tree; the hang must become an immediate honest
  error carrying the in-flight id.
* With `TMPDIR` aligned, **everything J2 promises worked across products**: proxy line
  verbatim, cross-product read-your-writes both directions, one holder + live socket in
  the lease row, opencode's whole run 10s.
* Operator frictions worth the runbook's attention: cursor-agent needs `--trust` and
  `--force` (else every MCP tool call is silently rejected); macOS has no `timeout(1)`.

Process note: the "two different client products" Done-when box — previously assumed to
need a human at the keyboard — was run end-to-end by an agent, including the attended
holder-kill. The box itself stays unticked until the remediated tree passes the same
probe.

---

## 2026-08-21 — what the G3 spike found about the graph itself

The G3 Laplacian spike (`spikes/g3-laplacian/`, outcome in G-recall-calibration.md §G3)
shelved its diffusion hypothesis with numbers and upheld its resistance hypothesis pending
operator judgement — but its side-findings are the dogfood entries:

* **Derived decisions arrive edge-less, and the provenance spine does not rescue them.**
  386 concepts, 41 components; the giant component holds 92% of Resources and 85% of
  Entities but only **37% of Logic and 30% of Observation**. Mechanism, read at the API:
  `record_action` writes `Causal`/`Dependency` edges to shared file/commit hubs, so
  artifacts connect; `lambo_derive` writes only intra-call `CoOccurrence` cliques plus the
  rarely-used `parent_of` — so *reasoning strands*. Adding the `Derives` co-parent
  projection collapses 41 islands to only 40. No expansion algorithm can reach what has no
  edges — which is also *why* diffusion could not beat fixed-depth here. The islands
  problem is a **`lambo_derive` API-ergonomics question, not a graph-algorithm question**.
* **Canonization has never completed on this store**: 0 of 386 concepts carry a stored
  blast_radius after thousands of daemon cycles. The pillar warning has never actually
  fired for a structural reason upstream of the promotion policy — this sharpens the
  2026-08-27 fair test again (check Stage-3 completion, not just promotion counts).
* **The exclusivity filter hides pillars** (H2's upheld half, awaiting operator judgement
  on the table in `spikes/g3-laplacian/README.md`): a dependent with path multiplicity has
  another inbound structural source and is excluded *by construction*, so 156/159 counted
  pairs sit at identical resistance while the discriminating structure lives outside the
  counted set. Concrete case: the J3-implementation concept has 13 dependents and the
  graph's three deepest-supported pairs, yet blast_radius 4 — below the warning bar —
  while a pendant-6 concept warns today.
* **The cheap actionable H1 surfaced**: recall phase-2 expansion is out-edge only;
  248/386 single seeds expand to nothing, and symmetrizing the same BFS multiplies
  expansion 2–4× for one function call. "Should phase 2 follow in-edges" is now a
  measured, one-line question for whichever workstream next touches recall.

---

*(next entry appends here)*
