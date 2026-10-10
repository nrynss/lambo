# #36 design: canonization follows declared dependence

Analysis/design, 2026-10-10; source baseline **212d3c4ac68327669c60baaf7a98f3af58ec2454**. No production implementation accompanies this note.

**Recommendation:** retain the operator-approved read-time dependents semantics, explicit resolvable citations/names, 14-day coverage denominator, evidence-led Swarm admission and evidence-gated Solo. Do not lower thresholds to force the four nominated invariants through. Fresh measurements confirm the diagnosis but expose a substantial citation-model sensitivity: future citation intent is not recoverable from historical data.

This supplements [#36](https://github.com/nrynss/lambo/issues/36), incorporates [#19](https://github.com/nrynss/lambo/issues/19), [#20](https://github.com/nrynss/lambo/issues/20), [#30](https://github.com/nrynss/lambo/issues/30) and [#87](https://github.com/nrynss/lambo/issues/87). All bodies/comments were read. The older separated-days recommendation in memory is superseded by the revised record and current operator-approved issue body. The frozen spec needs a dev-diary errata entry when implementation lands, not an edit.

## Measurement contract

The requested branch already existed, so this pass uses **design/36-canonization-analysis** in **lambo-wt-36**, from fresh origin/main. No other worktree was opened. Top-level handoff RULES.md/AGENTS.md were missing; available refactor/RULES.md and worktree AGENTS.md were read. Posting this comment is explicitly authorized by the task.

Only SQLite's online backup and disposable copies were evaluated. The real snapshot contains **4,857 concepts, 1,718 interactions, 11,831 edges**, with interaction extent **2026-08-19T11:10:40.394Z–2026-10-10T11:20:26.611Z**, or **52.0068 days**.

| Population | Count |
|---|---:|
| Resource / Entity | 2,114 / 757 |
| Logic / Constraint / Observation | 952 / 448 / 586 |
| Dependency / Causal / Hierarchical | 1,056 / 2,439 / 46 |
| Derives / CoOccurrence / Temporal / Semantic | 4,880 / 1,644 / 1,717 / 49 |
| Concepts with accesses / total accesses / maximum | 868 / 1,935 / 17 |

The stored embedding contract is BGE-M3, width 1024. EG2 calibration was not measured.

The existing **examples/canon36_proto.rs at 13af6f8** extends #29's dry-run procedure and calls the **real Evaluator::eval_cycle**. It was adapted temporarily for current main, fixture loading and score diagnostics. All experimental source was restored before finalizing the deliverable.

Each variant loads the same snapshot, runs three GC sweeps a day apart, drains deferred survivor bumps, then rescores before every evaluator cycle. Defaults remain: batch size 50, Canonical budget 1,000, injected time advances 61 seconds/cycle. Stop after more than ring_size/batch_size+3 quiet cycles; all reported runs stopped before the 2,000-cycle safety limit. No concepts were collected. Initial survival counts were **285 at 0, 387 at 1, 4,185 at 2**; after sweeps they were 3/4/5 respectively. Thus all clear survival in the dry run, whereas none did in the persisted starting snapshot.

**Parity:** shipped SQLite and the prototype under today's semantics produced identical counts, named-invariant statuses and Canonical IDs under both policies. Baseline Swarm/Solo took 24/89 cycles. The copied session_leases rows initially caused an expected unfenced-write refusal; they were deleted **only in the disposable parity copies**. No live lease changed. Prototype writes are no-ops, with no proposed statuses flushed to the original backup or writer.

Dataset identity: SHA-256 of the sorted-key compact JSON census of concept/edge/interaction rows is **f5b731acb0e2ee0b4ff5daed8e9c5d73ec92c7c0653a03b4922f0795b8ff073e**. Adapted scratch harness hash: **5e3e9c0095f4da652efa1371b8bb894f112646645c64b2589e676a8ce2b0d583**. Raw contents/dumps/replay/database files are not published or committed and are deleted at completion.

## 1. Diagnosis traced to source

File:line references are against the baseline commit. [Stage 1 source](https://github.com/nrynss/lambo/blob/212d3c4ac68327669c60baaf7a98f3af58ec2454/src/canon/stage1.rs#L55) anchors the first trace.

### Stage 1: survival opens eligibility; P90 chooses popularity

**src/canon/stage1.rs:55–77,106–113** requires 20 non-Canonical peers, gc_survived>=3 and score strictly above nearest-rank P90. Candidates/Venerables remain peers; Canonicals leave. With 4,857 peers, rank 4,372 leaves **485** initially above the cut, absent ties. The cut is **0.391001**. GC retention is necessary but does not establish that a proposition governs later work; survivor bumps are at **src/daemon/gc.rs:597–614**.

**src/daemon/score.rs:315–347,361–377** uses last touch for recency, accesses/10 for frequency, Derives counts for activity, and incident edges for density; weights are 0.25/0.20/0.20/0.35. Artifact hubs gain density while uncited old prose is disadvantaged. Baseline scores for lease-stays/wedge/identity/async-ack are **0.442724/0.233754/0.086722/0.343649**: only lease-stays initially clears P90.

**#30 changes membership, not necessarily monotonically.** Replay k=0/1/2 recalls per origin interaction, counting earlier returned hits as accesses: P90 rises **0.391001 → 0.469855 → 0.500288**, still admitting 485 initially. Final Swarm Candidate/Venerable/Canonical becomes **476/9/0 → 478/7/0 → 476/9/0**. Async-ack enters Candidate at k=2 but fails Stage 2. The historical 7→4→1 result belongs to its older corpus. Eligibility remains stable under the evidence-led redesign below; access can still change ordering and budget selection.

### Stage 2: inbound origins and session-wide coverage select hubs

**src/canon/stage2.rs:34–48** requires distinct>=3 and coverage>=0.3. **src/store/sqlite/structural.rs:38–58,210–269** joins inbound structural source concepts to their origin interactions, ages edge and interaction using event time with created-time fallback, and divides their time spread by the **whole session extent**. PG/Cockroach share **src/store/pg/sql.rs:425**; MemoryStore uses **src/store/memory.rs:873–941**.

The current floor requires **15.602 days** of source-time spread. **50** nodes pass, none Logic/Constraint. Async-ack has three inbound origins but coverage **0.00756**. Repeated Derives or accesses do not become structural evidence; reinforcing an edge does not create another source origin.

### Stage 3: outgoing exclusive targets select actions

**src/canon/stage3.rs:62–90** requires aged blast>5 and cooldown clearance. **src/store/sqlite/structural.rs:134–204** counts outgoing structural targets with no other structural source. PG matches this at **src/store/pg/sql.rs:384–403**; recall implements it independently at **src/recall/format.rs:82–141**.

**66** nodes pass, none Logic/Constraint. Stage 2/3 overlap is **zero**. Among 258 concepts with at least six outgoing structural edges, at most one distinct inbound structural source exists. No concept has three inbound and six outbound structural edges. #35 is fixed already; it does not explain this mismatch.

**src/graph/action.rs:364–397** writes action→artifact Causal edges and action→dependency Dependency edges. Stored Dependency direction correctly means the action depends on its target. The inversion is reading the action as the pillar for blast radius. Preserve that write contract; reinterpret dependence at read time. Exclusivity also hides a pillar when its dependent cites other things.

### Swarm versus Solo

Swarm consumes store evidence (**src/canon/policy.rs:360–380**): **476 Candidate / 9 Venerable / 0 Canonical**.

Solo counts inbound Causal/Dependency action sources (**policy.rs:473–510**), applies type resistance and score bands 3/6/10 (**:394–409,548–555**) and ignores evidence (**:604–615**). Its formula rewards recurrence, human confirmation and valid actions. Repeated modifications make files win without governing anything. Result: **1,783 Candidate / 218 Venerable / 121 Canonical**.

The 121 Canonicals are **108 file paths + 13 short Resource/Entity names**, all blast **0**, no Logic/Constraint. Thus **100% artifacts under #36's definition**. The inherited harness's narrower heuristic reports 92% because it omits ten short Entity names; that is not semantic precision. Top Solo Canonicals by action-source count: CHANGELOG.md, src/mcp/serve.rs, J-multi-client.md, docs/reference/config.mdx, src/memory.rs. Swarm has no top Canonicals.

**src/canon/eval.rs:560–578** still pays Stage 2/3 query costs under Solo while the policy discards their verdicts.

### Why fixtures and demo succeed

The REST fixture has 22 concepts/12 interactions over **55 minutes**. User schema receives **six Dependency citations** and sends **eight Dependency field edges**. Incoming edges satisfy Stage 2; outgoing ones satisfy today's blast=8. Both interpretations are planted; action workloads mostly write only action→artifact/dependency.

Reset statuses to None, clear audit history, then run the same sweeps/evaluator:

| Fixture | Today's Swarm C/V/Canonical | Today's Solo | Repaired Swarm | Repaired Solo |
|---|---|---|---|---|
| REST API | 1/1/1 | 9/2/1 | 0/1/1 | 9/1/1 |
| Drift: 9 concepts/2 interactions/30 minutes | 0/0/0 | 7/0/0 | 0/0/0 | 0/0/0 |

These are earned promotions after reset, not seeded Canonicals. Repaired user schema has six dependents and still passes; status/blast goldens change. The unchanged production **lambo demo passed**, user schema Canonical with **blast=9**: its live script graph differs from the committed fixture. It compresses cadence/age and settles survival (**src/cli/demo.rs:61–77,94–127**), so it demonstrates its scripted shape, not general workload precision.

## 2. Ruling versus artifact, and the four invariants

A ruling is a proposition later work knowingly rests on. Desired shape: **later decision/action --Dependency--> ruling**, optionally with Hierarchical children. A file/schema can legitimately be load-bearing; length/type should not prohibit it. Meaningful evidence is declared dependence across other interactions, not mere touches or association.

| Type | Median characters | No incident structural edges | Any inbound Dependency |
|---|---:|---:|---:|
| Logic | 538 | 927/952 | 5/952 |
| Constraint | 445 | 422/448 | 5/448 |
| Observation | 408.5 | 586/586 | 0/586 |
| Resource | 44 | 101/2,114 | 148/2,114 |
| Entity | 29 | 30/757 | 708/757 |

All Logic/Constraint/Observation texts exceed 60 characters; 1,312 Resources and 635 Entities are <=60. But **395 Resources exceed 300 characters**, so action prose overlaps ruling prose. Dependency edges are Resource→Entity **827**, Resource→Resource **218**, Resource→Logic/Constraint only **11**.

Derives exists for minted artifacts too, so provenance alone cannot distinguish authored assertions. CoOccurrence/Semantic are association/merge, not endorsement. Access medians are zero in every type; popularity is not authority.

**31 Entity names** match a ruling's leading phrase. **28 receive 41 Dependency citations**; enumerating all prefix matches suggests **48 redirect pairs** because some names are ambiguous. This is a backfill review queue, not authorization to redirect every pair. Refuse ambiguous references. The earlier 21-name/35-redirect measurements used an older snapshot.

The four full rulings were located by the prior prototype's stable IDs and checked against **dev-diary/lambo-for-mooshik/DOGFOOD-FINDINGS.md:184–197**. All originate under claude-orchestrator on Aug 19/20. Caller-asserted identities are not trusted independent witnesses.

| Full ruling / short ID | Type; characters; accesses | Derives / CoOccurrence | Real inbound / outbound structural edges | Origins; coverage; blast | Baseline Swarm / Solo |
|---|---|---|---|---|---|
| Lease-stays 0235df4b | Logic;539;2 | 2/4 | 1 Causal / 1 Hierarchical | 1;0;1 | Candidate / Candidate |
| Wedge 3e01bb4b | Constraint;1,164;0 | 2/2 | 1 Hierarchical / 1 Hierarchical | 1;0;1 | None / None |
| Cooperative identity a0205e60 | Logic;621;0 | 1/0 | 1 Dependency / none | 1;0;0 | None / Candidate |
| Async-ack 8baad4e0 | Constraint;529;0 | 5/4 | 2 Hierarchical+1 Dependency / 2 Hierarchical | 3;0.00756;2 | None / Venerable |

The wedge name **5da57fe5** is an **18-character Entity** with three inbound Dependency citations and a Hierarchical child. It becomes Solo Venerable while the full ruling stays None. That is a reference-resolution defect, not evidence for preferring short names.

## 3. Three candidate redesigns

### Shared proposed predicates

**dependents(X) = Dependency sources into X ∪ Hierarchical children of X.** Deduplicate IDs; exclude self; scope endpoints/origins to the session; preserve event-time and edge-age guards. Causal confers no load. Stage 2 counts dependent origin interactions other than X's own. Stage 3 counts **all** dependent concepts, not exclusive ones. Stores, recall warnings, inspect and budget demotion use one definition. This changes blast from exclusive hypothetical orphaning to declared dependent count and requires explicit errata.

Swarm Candidate: **gc_survived>=3 and >=2 other dependent interactions**, with no peer minimum/P90 quota. Stage 2: **>=3 origins, coverage>=0.3 against min(session extent,14 days)**. This is a denominator cap, **not a rolling expiry window**. Stage 3: **>5 dependents**, current cooldown. Solo keeps score-band admission, replaces valid actions with distinct dependent interactions, and consumes Stage 2/3 verdicts for later hops. Retain one-hop transitions, bounded probes and fencing.

### Simulation assumptions

Historical data cannot reveal future citation intent. Real recall queried each concept's first 16 words, kept five earlier hits, and separated tasks by agent plus a >30-minute gap. Result: **4,391 queries with earlier hits,4,240 with cross-task hits,391 inferred tasks**. No query vector was supplied; this measures lexical/graph/recent recall, not vector relevance. The final snapshot's later graph structure can leak context despite filtering hit creation times. Task boundaries are approximations.

- **Broad explicit replay:** every concept cites its top earlier cross-task Logic/Constraint hit, plus alias redirects: **3,126 added edges**. It includes minted artifacts incapable of independently making write-time citations.
- **Conservative authored-source proxy:** only Logic/Constraint/Observation or existing Causal/Dependency sources can make those citations: **2,594 eligible source nodes,2,182 added edges**. This may exclude legitimate Entity/Resource derives; it is not ground truth either.
- **Auto5:** links five earlier cross-task hits plus aliases: **19,754 edges**.
- **Noise control:** similar attempted volume to random earlier targets plus aliases: **19,776 edges** after deduplication.

| Candidate | Swarm Candidate/Venerable/Canonical | Solo Candidate/Venerable/Canonical | Full invariants Canonical under both |
|---|---|---|---|
| A: repaired predicates, existing history | **73/3/0** | **240/3/0** | none |
| B: A + explicit citation/name, broad proxy | **497/113/90** | **957/115/88** | lease-stays,async-ack |
| B sensitivity: authored-source proxy | **431/95/46** | **928/96/45** | lease-stays only |
| C: A + automatic top-five links | **1,544/475/840** | **2,217/480/818** | all four |
| C random control | **924/1,284/1,000** | **1,583/1,278/1,000** | all four |

Counts are final disjoint statuses. Broad B Stage 2/3/overlap **203/156/90**; authored B **141/76/46**; C **1,315/1,337/840**. Every B Canonical is Logic/Constraint with >5 dependents. This is **composition, not semantic precision**: the simulation intentionally selects ruling-typed targets. Real rules still allow Entity truths.

| Invariant | B broad: origins/coverage/dependents;status | B authored proxy |
|---|---|---|
| Lease-stays | 10/0.88/14;Canonical | 9/0.87/12;Canonical |
| Wedge | 5/1.00/**5**;Venerable | same |
| Identity | 3/**0.01**/**3**;Candidate | same |
| Async-ack | 9/1.00/9;Canonical | 4/**0.02**/**4**;Candidate |

Do not force promotion: wedge lacks the sixth dependent; identity lacks spread/load; async-ack passes only in the broad artifact-citation model. B broad counts and these statuses are identical at **k=0/1/2** access replay, under both policies. This proves eligibility stability on the model, not a production forecast of 90 Canonicals.

Top Canonicals by proposed dependent count, summarized rather than quoted:

| Run | Top five |
|---|---|
| A | none |
| B broad, both policies | old J1 server-identity gate(35);docs worktree directive(25);docs prose style rule(23);review-finding disposition(21);dated drift-remediation instruction(19) |
| B authored proxy | old J1 server-identity gate(22);docs worktree directive(16);review-finding disposition(14);telemetry platform decision(13);dated drift-remediation instruction(13) |
| C, both policies | old J1 server-identity gate(52);docs worktree directive(47);T3.2 review action(41);T3.3 review action(40);docs PR-opening action(39) |
| Random control, Swarm | J2 review-scope ruling(38);identity ruling(30);wedge ruling(28);old proxy-review blocker(25);dogfood database path(25) |

The old J1 gate conflicts with the later identity ruling. Dependency degree cannot determine semantic truth. One-off directives/history also appear. This is concrete motivation for #19/#20, and prevents calling 0% artifacts “100% standing-ruling precision.”

**A — repair without citation API.** Cheapest writer change, coherent support but no canon on current history. Names still miss rulings. Gaming by invented distinct actions remains possible. Stage 1 needs graph-wide/batched dependent-origin aggregation, not per-node SQL. #19/#20 gain coherent semantics but little actual authority to annotate/demote. Young Dresscode sessions lose the 20-peer blocker but still need survival and evidence. Necessary foundation, insufficient complete fix.

**B — explicit resolvable citations (recommended).** Optional per-concept depends_on/name; resolve by full ID, unique recall short ID, canonical key or synonym. Refuse unresolved/ambiguous/self/cyclic derive references before ack, never create their targets. Limits: eight references/concept,32/call. record_action resolves existing references before preserving legacy create-on-miss artifact strings. Explicit intent reduces accidental links but is not Sybil resistance: fabricated work can still game it. Resolver costs graph-index lookups, no embeddings; share dependence aggregation. Alias backfill is reviewed/copy-first. Share resolver with #19. Per-user sizing beliefs can express dependence without large peer populations, but remain ordinary memory until evidence accumulates. Do not pool users/contracts to manufacture support.

**C — auto-link recall.** All four pass, but seeing becomes endorsement. Swarm Canonicals: **317 Logic/Constraint,293 Resource,98 Entity,132 Observation**. At least **356/840 (42.4%)** are Resources or path/short-name Entities; Solo **353/818 (43.2%)**. Random control fills budget with at least **604/1,000** such artifacts. Edge growth is **167%** of the original table, versus broad B26%/authored B18%. Auto-links can make #19/#20 spurious conflict/supersession engines and let irrelevant EG2 hits contaminate personal beliefs. Reject default auto-linking. Any opt-in classifier/proposed-citation UI needs separate labelled precision evidence.

## 4. Integration, implementation PRs and acceptance

### Related contracts and young sessions

**#19:** Contradicts never counts as dependence/support/blast. Shared typed session-scoped resolver belongs in crate::surface beside surface::focus; inspect's fuzzy substring match must not authorize a write. Contradiction annotates Venerable/Canonical but does not promote. Latest evidence must use the new dependent orientation and event-time fallback, not old inbound-structural SQL. Old J1 gate versus cooperative identity is an acceptance case.

**#20:** reuse new Stage 2 with evidence **strictly after** the old concept's latest dependent evidence time. Filter before counting/spread. The capped session denominator remains; do not replace it with a tiny post-cutoff span. Assertion-only contradictions never demote. Default trait accessors interaction_span_since/latest_structural_evidence_at fail with Capability, preserving third-party adapter compatibility and fail-closed behavior. Supersession precedes budget, records reason, retains fencing/cooldown, does not auto-promote its source. Extend one shared fair-test fixture. Update “latest inbound structural evidence” terminology to “latest eligible dependent evidence.”

**#87:** do not equate global cosine with endorsement. EG2's 0.55–0.68 irrelevant-pair scores and suggested 0.80 merge threshold are issue-reported, not measured here. Default merge threshold is **0.85 at src/graph/hybrid.rs:202**. Calibration needs the labelled fashion dataset. False merges collapse dependence sets; unmerged paraphrases split support. Deterministic IDs/names reduce reference errors, not bad-merge consequences. Equivalent graphs under BGE/EG2 contract metadata should yield identical canonization verdicts; actual retrieval/citation relevance needs calibrated vectors.

**Dresscode:** per-user graphs should remain isolated. Removing Swarm's peer minimum helps small graphs, but gc_survived>=3 still requires sweeps; the default 10,000-mutation cadence is a separate cold-session limitation, visible in the starting snapshot. Sessions <14 days keep today's denominator. Short legitimate spans can qualify after ageing, and bursts can also occupy much of a tiny session: the four-day spread is not universal. A mandatory 24-hour separated-days rule would break legitimate short sessions/demo. Keep preferences as ordinary memory promptly; use distinct purchase/refund events with explicit dependencies for later evidence. A personal preference may have fewer dependents than a governance ruling; explain its gate failures instead of weakening global blast to force it through.

### Five reviewable PRs

Effort estimates exclude review and are not delivery dates. Refactor #24–#28 prerequisite is already complete.

1. **Dependents semantics/errata — medium,2–3 days.** MemoryStore,SQLite structural.rs,pg sql.rs/structural.rs,recall format.rs,inspect,budget. Read-time reinterpretation, no migration. Preserve #29's independently defined GC-protection predicate. Regenerate REST outgoing field relationships as Hierarchical: six citations+eight children=14 dependents, listing every golden change. Tests: action→X makes X load-bearing; Causal/Derives/Temporal/CoOccurrence/Semantic/Contradicts do not; shared dependents count; self/foreign/young edges excluded; all store and recall implementations agree.
2. **14-day denominator — small,1–2 days.** Validated/documented configuration and reusable Stage 2 predicate. Tests: short fixture coverage unchanged; zero/single-point extents; long sessions need4.2 days' spread; equality at three origins/0.3 passes; injected clock/event-time/age parity across adapters. Clearly label cap versus expiry.
3. **Swarm admission/Solo gates/fair-test fixture — medium,2–3 days.** src/canon plus indexed/batched dependent-origin inputs and inspect explanations. Composite orders, not percentile-selects. Tests: <20-peer graphs qualify on evidence; <3 survival/<2 other origins refuse Swarm Candidate; replayed reads preserve eligibility; Solo cannot bypass Stage2/3/cooldown; five dependents fail,six pass; bounded probes remain fair. Synthetic fixture gives each invariant six real-shaped dependents across >=3 aged interactions with sufficient spread, and includes chaff plus under-supported wedge/identity/async variants. Both policies promote supported invariants without seeded statuses or reciprocal Dependency tricks.
4. **Citation/name API and shared #19 resolver — large,3–5 days.** src/mcp/server/params.rs:133,tools/write.rs,src/memory/writes.rs:431,510,src/writeq/execution.rs:308,CLI,crate::surface. Tests: ID/short ID/synonym resolve same node; missing/ambiguous/foreign/self/cyclic references refuse with actionable typed errors before ack; size/count limits; natural-key idempotency; pre-pass/background validation agree; graph-before-hot-list lock order; no lock across await; legacy requests unchanged. Update tool mirrors/schema goldens. #19's own wire-field PR uses the same resolver.
5. **Read-only canon dry-run/alias preview — medium,2–3 days.** Snapshot/in-memory overlay, copied-lease handling, per-stage failures/counts/top IDs/invariant results and baseline comparison. Never persist proposals. Backfill preview handles ambiguity/cycles/idempotency. Tests: input database unchanged; independent oracle parity; deterministic stopping for budget/cooldown; isolated ports/runtime paths; fixture and aggregate regression. No private store enters CI.

**Rollout is an operator action after the PRs, not a sixth code PR.** Keep rig Swarm. Preview aliases on a fresh copy, review ambiguity, collect actual explicit citations, and confirm only rulings the operator would sign. Do not apply the broad simulation's 48 redirect pairs or fabricated citations to the writer. Re-pin dry-run records actual invariant gate failures. #20 remains opt-in; deliver supersession before treating growing canon as current truth.

### Acceptance before declaring #36 fixed

- Fair-test fixture promotes all four adequately supported invariants and rejects chaff/under-supported variants on Memory/SQLite and live PG/Cockroach where available.
- Store/graph/recall counts agree; warnings identify actual dependents; Contradicts changes neither promotion support nor blast.
- Shared #19/#20 fixture covers newer/older contradictions, cutoff-before-count, assertion-only refusal,no auto-promotion,fencing,anti-flapping.
- Snapshot after citation rollout measures real cited-source counts and standing/stale/task-history precision labels. Four invariants pass or have recorded evidence-based reasons not to; never manufacture citations for a target.
- Read replay/session partitioning preserves evidence eligibility. Budget-saturated ordering remains a separately tested sensitivity.
- #87 calibration follows its labelled dataset; no EG2 precision claim from lexical replay.

### Verification and limitations

Scratch harness built offline with store-sqlite,fixtures. Production-policy SQLite parity, all candidate/control runs, both committed fixtures and unchanged **cargo run --offline --features demo --bin lambo -- demo** completed. Every test/measurement invocation used a fresh XDG_RUNTIME_DIR under /tmp/lbX.*. Production sources and Cargo.toml/Cargo.lock were restored byte-for-byte; git diff --check passed. No push or PR.

No full CI matrix for this analysis-only document. Unmeasured: live PG/Cockroach parity,EG2 calibration,future citation intent,multi-reader semantic precision,production query latency,post-rollout fair-test outcomes. **46/45 conservative versus90/88 broad Canonicals is the central forecast limitation.** Graph-index cost is estimated, not benchmarked. Private backup/replay/dumps are deleted on completion.
