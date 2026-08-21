# Adversarial review — Mooshik J3 durability redesign, round 2

**Reviewer:** independent adversarial reviewer (Claude Opus 5), agent id `j3-redesign-reviewer-r2`.
Wrote none of the code or docs under review.

**Scope:** the five remediation commits `ed03266..bc28ac8` on `wt/j3`, claiming all fourteen
round-1 findings closed (1 P1 / 3 P2 / 1 unconfirmed P2 / 6 P3), plus the two argued
push-backs the remediation raised against the round-1 prescriptions.

**Verdict:** **REQUEST_CHANGES** — 1 P1, 3 P2, 5 P3. Twelve of the fourteen round-1 findings
are closed at the artifact and both argued push-backs are upheld; the founding invariant
reproduces exactly at an independently built binary. The P1 is new: N1's classification reads
the error *variant*, and the shipped adapter maps every non-success HTTP status to the
permanent one, so a live embedder answering `503` still destroys the whole durable backlog —
measured, 63 of 63.

---

## Method

Read-only at source unless noted. Everything below was done in the worktree
`/Users/narayan/Documents/work/lambo/.claude/worktrees/j3` at `bc28ac8`; the main
checkout was never touched.

1. **Memory first.** `lambo-dogfood` recalls on "J3 round 1 findings" and "J3 durable
   intents redesign" as `j3-redesign-reviewer-r2`, before reading any code, so the
   design rationale, the operator's rulings and the remediation's own derived decisions
   were in hand and could be diffed against the artifact.
2. **Source, not commit messages.** Every claim was checked at the file the commit
   message names, at HEAD, with `git show <rev> -- <path>` for what changed and
   `sed`/`rg` for what is there now. Where a commit message asserts a property of the
   *product* (loudness, placement, exhaustiveness) the property was checked at the
   product's own path, not at the test that pins it.
3. **An independently built binary.** `cargo build --release --features
   store-sqlite,embed-bge`, `LAMBO_GIT_SHA=bc28ac8`, into a target dir of this
   review's own (`<scratchpad>/j3r2-target`) so nothing was inherited from the
   remediation's build. llama.cpp BGE-M3 health verified `{"status":"ok"}` at
   `127.0.0.1:8080` before every live run.
4. **Their runs re-run, then pushed past.** The transport-failure red/green was
   re-run from the committed driver; then three harder shapes the remediation did not
   run (a slow-but-alive embedder, a status-faulting-but-alive embedder, `kill -9`
   during replay) and one F5 shape it declared out of scope (a missing *column*).
5. **The two push-backs adjudicated on their merits**, at source, as findings in their
   own right — a remediation that argues a review out of a prescription is the case
   most likely to be wrong with nobody re-testing it.
6. **Gates re-derived repo-wide**, summed across all 15 `test result:` lines, never the
   lib line; the name-set delta reconciled by NAME rather than by arithmetic.
7. Two read-only sub-investigators ran in parallel on the eleven remaining closures
   and on the numbers/register sweep; every row they returned that carries a verdict in
   this file was spot-checked by me at the cited line.

Verification-only edits: declared in **Gate results** below. Tree left clean; no
commit amended.


---

## The fourteen round-1 findings

Verdicts: **CLOSED** = the finding is answered and the answer is true at source;
**CLOSED (as scoped)** = answered for the case the finding named, with a new finding for a
case it did not; **PARTIAL** = the substance is closed and something the fix itself asserts
is not true.

| # | Finding (round 1) | Verdict | Evidence at `bc28ac8` |
|---|---|---|---|
| N1 | P1 — a transient embedder outage at attach burns the whole durable-intent backlog | **CLOSED (as scoped)** | Liveness gate `src/writeq.rs:2925-2941`; structural classification `src/embed/mod.rs:74-77` + `src/types/mod.rs:920-944`; consume-only-`Embed` arm `src/writeq.rs:2983-2999`; `PendingReplay` `src/writeq.rs:874`; `write_queue_replay_owed` `src/mcp/server.rs:1131`. Re-measured by me at my own binary: outage session leaves `unconsumed=63 failed_rows=0 replay_owed=63 replayed=0`, next healthy serve applies all 63 with embeddings. The transport class is genuinely fixed. The **status** class is not — J3-R2R-1 |
| N2 | P2 — the timeout deviation shipped with no test | **CLOSED** | `an_embed_timeout_fails_the_write_and_writes_nothing`, `src/graph/hybrid.rs:2226-2277`, `start_paused` against the production arm at `src/graph/hybrid.rs:637-644`. The pre-change behaviour returned `Ok`, so the test cannot pass against it |
| N3 | P2 — twelve estimator-era stated reasons in `writeq.rs`, two of them production log lines claiming measured bounds | **PARTIAL** | Ten docstrings and both log lines rewritten (`src/writeq.rs:2057-2069`, `:2070-2084`); the `info!` line reads true at my own live run — "bounds are static (lane 64, queue 1024) and no rate moves them; the rates below are telemetry". An operator reads that correctly. **Two live survivors** the sweep missed, plus a measured table that no longer reproduces — J3-R2R-5, J3-R2R-6 |
| N4 | P3 — the deleted estimator still sized both bounds at build time | **CLOSED** | `PROBE_CLAMP_RPS: u64 = 1_024` standing alone at `src/writeq.rs:305`, guard restated `:330-345`. All eleven const-asserts read; none couples a live bound to a measured rate. One in-kind residual (`RECEIPT_RETENTION` vs `MEASURED_WORST_FLUSH_LAG_SECS`, `src/writeq.rs:553-567`) is legitimate — a retention window *must* exceed observed lag — and unmentioned |
| N5 | P3 — the accounting expression drifted | **CLOSED** | `src/writeq.rs:117` and `:1339` now carry the same four-term expression, and `:126-127` names the code as the authority and the doc as a copy of it. Two copies, one of them declared subordinate — acceptable |
| N6 | P3 — replay order vs drain order | **CLOSED**, and the declination is **correct** | Mint at `src/writeq.rs:2179-2180`, `receipts.lock()` at `:2181`, the `graph.write()`+`lanes.lock()` hold at `:2220-2221`, `push_back` at `:2253`. See the push-back row below |
| N7 | P3 — `intent_durable` settled before the flush that makes it true | **CLOSED (doc)** | Tense declared at `src/writeq.rs:895-919`; `describe()` now says the next serve will **re-attempt** it (`:975-981`). Mechanism deliberately unchanged, with the reason given |
| N8 | P3 — `spawn_replay` seeded prior-process intents as `Pending` | **PARTIAL** | `PendingReplay` exists, is seeded (`src/writeq.rs:2883`), is unsettled (`:952-954`), has its own tag and sentence (`:938`, `:960-966`), and answers end-to-end at my live binary. But it is in **neither** of the two tests that keep the taxonomy honest — J3-R2R-4 |
| N9 | P3 — five commits claimed where six exist | **CLOSED** | `J3-durability-redesign.md:162-166`; `J-multi-client.md:1987` item 6 = `66f5aaa` |
| F1 | P3 — `ReceiptAnswer` docstring said "Seven variants" at ten | **PARTIAL** | The count is right — "Eleven" at `src/writeq.rs:849` against exactly eleven variants at `:862-889`. The same sentence adds a claim that is false — J3-R2R-4 |
| F2 | P3 — "expired rows are skipped at load" with no such filter | **CLOSED** | Fixed one layer up, at the seed rather than the load: `stale_before` and the `continue` at `src/writeq.rs:2863-2872`; the doc now *admits* the loads are unfiltered (`src/types/mod.rs:537-563`) and the loads are indeed unfiltered (`src/store/sqlite.rs:1781`, `src/store/cockroach.rs:1774`). The asymmetry is changed rather than erased — non-restarted answers `expired`, restarted answers `restart_lost` — and the doc says so, which is what the finding asked for |
| F3 | P2 — the reversed pin's undeclared blast radius; behaviour argued and kept | **UPHELD on the merits; PARTIAL on placement** | Boundary restated at the source that makes the claim (`src/graph/hybrid.rs` module doc, `:554`); declared at `docs/reference/mcp.mdx:169`, `site/src/content/docs/mcp.mdx:171`, Done-when limit (6), `src/config.rs:131-134`, `src/types/mod.rs:207-231`. See the adjudication below: the argument holds, one clause of the declaration is false at the binary (J3-R2R-2) and two mirrors were missed (J3-R2R-7) |
| F4 | P3 — "two extra Single mutations per write" understated Cockroach | **CLOSED** | Three statements confirmed at `src/store/cockroach.rs:1740-1761` (UPDATE + retention DELETE, plus the insert); both intent mutations fall into `plan_flush`'s barrier arm (`src/store/batch.rs:190-201`) so the batching fragmentation is real and now stated (`J3-durability-redesign.md:287-306`) |
| F5 | P2-candidate — an un-migrated store voids the founding invariant | **CLOSED (as scoped)** | `tables_in_ddl` `src/store/mod.rs:100-112`; `preflight_schema` `src/store/mod.rs:170-172` (default `Ok(())`), `src/store/sqlite.rs:706-720`, `src/store/cockroach.rs:1975-1996`; called at `src/memory.rs:691`, **before** the lease acquire at `:702`, inside `build_attach` so every entry path runs it. Ten tables in each migration, all in the one matched idiom — I checked both dialects by hand. The declared column gap has F5's own magnitude — J3-R2R-3 |

### The DDL parser, attacked directly (F5)

Both migrations were enumerated by hand against `tables_in_ddl`'s single idiom:
`migrations/sqlite/001_init.sql` has ten `CREATE TABLE IF NOT EXISTS <name> (` at column 0
(lines 53, 63, 72, 96, 109, 116, 131, 154, 168, 184) and `migrations/cockroach/001_init.sql`
has ten (lines 4, 24, 34, 141, 156, 163, 183, 207, 225, 244) — matching the `== 10` pins.
Cockroach's dialect differences do **not** defeat the parser: `CREATE VECTOR INDEX IF NOT
EXISTS` (`:137`), `CREATE UNIQUE INDEX` (`:65`), `ALTER TABLE … ADD COLUMN IF NOT EXISTS`
(`:20-21`) and `::STRING` casts are all *not* `CREATE TABLE`, so they are correctly ignored
rather than mis-parsed. No `CREATE TABLE` exists anywhere in `src/` outside test fixtures, so
the DDL really is the whole set. Under-reporting needs a future statement written in another
form — lower-case, without `IF NOT EXISTS`, or with the name on the next line — and the
docstring states that limit as a deliberate choice ("a statement written any other way is
deliberately not matched"). Over-reporting is possible through a block comment (`/* CREATE
TABLE IF NOT EXISTS … */` would match); neither migration contains one, and the failure
direction is a loud false refusal rather than a silent hole. The `--` comment decoy in the
pin is genuinely rejected. **The parse is sound; the pins are the right pins.**

The default `Ok(())` is the right call, not a hole: there are twenty-odd `impl GraphStore`
test doubles and `MemoryStore`, none of which has an external schema. Making it required
would churn twenty test doubles to protect two adapters. The residual risk — a *future* SQL
adapter that forgets to override — is real but cheap to catch at review, and the trait
docstring states the contract.

Placement checked at source and by consequence: `preflight_schema` is called inside
`build_attach` (`src/memory.rs:691`), which is the single door for `build`, for `resolve_role`'s
election loop and therefore for `--transport http` and for a J2 **proxy** start alike — a
proxy is only reached after `Attach::Held`, which is *downstream* of the preflight, so a
newer-build proxy against an older store refuses rather than forwarding into a void. A
refusal strands no lease: the acquire is the next statement, and the adapter pin asserts no
lease row is left.

---

## The two argued push-backs, adjudicated

| Push-back | The remediation's argument | Verdict | Reasoning |
|---|---|---|---|
| **The prescribed `attempts` column, declined** | Bounding retry-forever by destroying the write after *k* is a smaller version of the trade N1 condemned; the poison case is already consumed on the first attempt; a transient failure now ends the loop rather than churning, so the per-attach cost is O(1); and a new column is precisely the schema change F5's preflight cannot see | **UPHELD**, and stronger than argued | All three legs hold at source. The O(1) claim is measured: my slow-embedder run cost the attach **one** `HYBRID_IO_TIMEOUT` and consumed nothing (`applied=0 failed=0 owed=63`), where round 1 measured 63 timeouts. The third leg is not rhetoric — I measured it. A store missing one *column* passes the preflight, attaches, acks, reports `write_queue_applied=4`, and leaves `concepts=0` (J3-R2R-3). Adding `attempts` would have created exactly that hazard for every operator who upgrades without re-provisioning. **Rider:** the substitute — visibility instead of a bound — is one key short of doing its job (J3-R2R-8) |
| **The review's own N6 three-line fix, declined** | Moving the clock and seq reads inside the receipts lock does not close the window, because the gap is between *minting* and the `lanes.lock()` `push_back` | **UPHELD** | Correct at source. `next_receipt` (`src/writeq.rs:2178-2184`) samples `seq` and the clock and only *then* takes `receipts.lock()`; `admit` calls it at `:2203` and the pair is not published until the `graph.write()`+`lanes.lock()` hold at `:2220-2221` pushes at `:2253`. Co-locating the two reads under the receipts lock leaves the mint-to-push gap untouched, so the prescription was wrong and declining it was right. The fix that *would* close it is to mint inside the graph+lanes hold — also small, and worth naming in the doc as the option not taken, since it is not obvious that one exists. Materially the window is harmless: lanes are per-agent, and one agent's *simultaneous* submissions are already excluded by contract (`src/writeq.rs:63-74`), so the only casualty was the doc's "order among replayed intents is exact", which is now scoped (`J3-durability-redesign.md:191-192`, analysis at `:203-211`) |

### F3's kept behaviour, adjudicated

**UPHELD.** The review asked whether a connection-level embedder failure deserves the
"declared, session-uniform" treatment an absent store capability gets. It does not, and the
remediation's reason is the right one: `vector_ok` is a static configuration fact known at
build (`src/graph/hybrid.rs:554`), while embedder reachability is dynamic and its *permanence*
is undecidable at the protocol. Auto-degrading on that guess writes exactly the silent
`embedding: NULL` concepts the J3-R3-1 honesty fix ended — and writes them session-wide
instead of per-call, which is worse. Spec §3.2's degraded mode staying reachable only by
declaration is the honest arrangement, and the opt-out is named where a user reads it.

What does **not** hold is one clause of the declaration. Both mirrors and Done-when limit (6)
say "nothing acked is lost, since an acked write is a durable intent that waits for an
embedder rather than dying with one". Measured, that is false on the common path — see
J3-R2R-2. The argument survives the correction; the sentence does not.

---

## New findings

One P1, three P2, five P3.

### J3-R2R-1 (P1) — a live embedder's non-success **status** still burns the whole backlog, and the bound that says it cannot is written into the fix

`EmbedError::is_transient` (`src/embed/mod.rs:35-77`) is the single hinge the
consume-or-keep decision turns on, and it reads the *variant*, not the HTTP status:

* `src/embed/bge_m3.rs:150` — a connect-level failure is `Unavailable` (transient). Right.
* `src/embed/bge_m3.rs:151-166` — **every non-success status** is `Backend`, i.e. permanent
  "for this input". That bucket contains `503 no slot available`, `503 Loading model`,
  a proxy's `502`/`504`, and — for the documented Bedrock Titan V2 swap-in — `429`
  ThrottlingException. None of those is a statement about the input.

`LamboError::Embed` has exactly one construction site (`src/graph/hybrid.rs:655`, the
`Ok(Err(e))` arm where `!e.is_transient()`), and it is the one class `spawn_replay`
consumes on (`src/writeq.rs:2983`). The liveness gate does not help: it embeds the
35-byte `PROBE_TEXT` (`src/writeq.rs:2925`), so any fault that spares a 35-byte request
passes it. And the failure arm **continues the loop** — only the non-`Embed` arm breaks
(`src/writeq.rs:2983-2999`) — so a uniform fault is applied to every intent in turn.

Measured at my own release binary (`LAMBO_GIT_SHA=bc28ac8`, `store-sqlite,embed-bge`),
session 1 against the real BGE-M3, session 2 against a front that answers `200` for
`PROBE_TEXT` by proxying to the real llama and `503 {"type":"unavailable_error",
"message":"no slot available"}` for real content:

```
session 1 (real BGE-M3): acked=64 embedded=1 durable_intents=63
session 2 (503-for-content stub, 1.0s in): replayed=0 replay_owed=0
                                           sampled_last_batch_receipt=failed
  store: unconsumed=0 failed_rows=63 embedded=1 applied=1 after_restart=0
session 3 (real BGE-M3 back): replayed=0 replay_owed=0
  final store: embedded=1 of 64 acked; unconsumed=0 failed_rows=63
```

**63 of 63 acked, reported-durable writes destroyed in about one second, by an embedder
that was alive and answering.** These are round 1's red numbers exactly
(`failed_rows=63`, one concept surviving) reached through a different door, with the P1's
fix in place. The debt was not merely lost — `write_queue_replay_owed` was *discharged to
zero by destruction*, which is the opposite of what the visibility answer was built for.

It also falsifies, at the binary, the sentence the fix uses to bound its own imprecision
(`src/embed/mod.rs:69-72`): "a misclassification can cost at most the one intent that met
the fault, not the backlog". It cost the backlog. That is the round-1 lesson the
remediation was closing — state each limit at its own magnitude — restated wrongly inside
the commit that closes it.

**Remediation (three parts, all small):**
1. `src/embed/bge_m3.rs:151-166` — classify by status class at the site that knows it:
   `408 | 425 | 429 | 502 | 503 | 504` → `EmbedError::Unavailable`; leave `4xx` and `500`
   in `Backend` (llama-server uses `500` for "input is too long", which *is* a statement
   about the input). Structural, one place, and `is_transient` needs no change.
2. `src/writeq.rs:2999-3020` — bound the failure arm: break after `k` **consecutive**
   `LamboError::Embed` refusals (`k=3` is enough). A per-content poison record is
   isolated by construction, so consecutive refusals are evidence of a session-wide
   condition, which is precisely the case the non-`Embed` arm already breaks on. This
   makes the stated bound true instead of aspirational.
3. Correct the bound sentence at `src/embed/mod.rs:69-72` and wherever the Done-when box
   repeats it, to the magnitude measured above.

### J3-R2R-2 (P2) — an acked write does **not** wait for an embedder; the in-session arm kills it, and the paragraph this remediation added says the opposite

N1's classification was wired into the replay arm only. The in-session worker's failure arm
consumes on **any** error class (`src/writeq.rs:2487-2496`):

```rust
if let Err(why) = &outcome {
    ctx.graph.write().consume_write_intent(
        job.receipt.to_string(),
        WriteIntentOutcome { tag: "failed".into(), summary: why.clone(), … },
    );
}
```

`LamboError::EmbedUnavailable` — the class created precisely to mean "nothing was learned
about this write" — is settled `failed` here exactly like a content refusal. Measured at my
binary, one session whose embedder was unreachable for its whole life:

```
acked: 16
stats: applied=0 failed=16 deferred=0 owed=0
receipt states: {'failed': 16}
store: concepts=0 embedded=0 intents=16 unconsumed=0 failed=16
```

Sixteen of sixteen acked writes destroyed by the same transient condition N1 was raised
about, on the path that handles almost every write (an intent only survives to replay if the
close beat its worker to it). The founding invariant is not violated — `failed` is an honest
terminal answer and the caller learns it on its next tool response — so this is a P2, not a
P1. What makes it a finding rather than a design choice is that the remediation shipped a
**user-facing durability claim that contradicts it**, in `docs/reference/mcp.mdx:169`,
`site/src/content/docs/mcp.mdx:171` and Done-when limit (6):

> nothing is lost that was already acked — an acked write is a durable intent, so it waits
> for an embedder rather than dying with one, which is what N1's fix restored

**Remediation.** Correct the clause on all three surfaces to what is true: *a write not yet
attempted when the session closes waits for an embedder; a write the worker reaches during
the outage fails, and its receipt says so*. Then declare the asymmetry as a deviation in
`J3-durability-redesign.md` — the replay arm keeps `EmbedUnavailable` intents, the in-session
arm does not — with the reason (the in-session caller has a receipt to read; the replay has
no caller), and name the symmetric treatment as a J4 seam: on `EmbedUnavailable`, re-queue
with backoff instead of settling `failed`, or leave the intent unconsumed and let the next
serve replay it.

### J3-R2R-3 (P2) — F5's uncovered half has F5's own magnitude, measured, and the limit is stated without it

`preflight_schema` diffs **tables**. Columns converge through `init_schema`'s `ensure_column`
ladder (`src/store/sqlite.rs:645-698`, seven columns) and Cockroach's `ALTER TABLE … ADD
COLUMN IF NOT EXISTS` lines (`migrations/cockroach/001_init.sql:20-21`) — both on the
**provision-only path**, which is the exact reachability that made the missing table bite.
The trait states the gap in one clause (`src/store/mod.rs:167-169`: "a missing column is not
covered here") and gives it no magnitude.

Measured. A store provisioned by a build predating `concepts.chunk_group_id` — all ten tables
present, one column absent, the honest analogue of F5's `init_schema; DROP TABLE
write_intents`:

```
store: all 10 tables present (10), concepts.chunk_group_id absent
PREFLIGHT: the session ATTACHED (a missing column does not refuse)
in-session signal: degraded=False dead_lettered=None applied=4 failed=0 deferred=0
serve exit code: 1
store after close: concepts=0 embedded=0 intents=0 unconsumed=0
… ERROR close: final flush failed; 24 tail mutations returned to the graph log
  error=… table concepts has no column named chunk_group_id
```

That is F5, line for line: attaches, acks, `write_queue_applied=4`, `degraded=false`, total
loss, loud only at close. The half the fix covers and the half it does not have the same
magnitude and the same trigger; only the covered half refuses.

**Remediation.** Either (a) extend the preflight to columns from the same source — parse the
column list out of each `CREATE TABLE … ( … )` block plus the `ALTER TABLE … ADD COLUMN`
lines (skip lines beginning `--`, `CONSTRAINT`, `PRIMARY KEY`, `UNIQUE`, `FOREIGN`, `CHECK`)
and diff against `PRAGMA table_info` / `information_schema.columns`, which is the same shape
of code as `tables_in_ddl` and reuses `unprovisioned_store_err`; or (b) if that is judged too
much parser for the payoff, **state the magnitude** at `src/store/mod.rs:167-169` and in the
Done-when box — "a missing column is not covered, and its consequence is the same total loss,
measured" — which is the round-1 lesson this remediation was closing. (a) is preferable
because the gap will be re-opened by the next migration, and the declined `attempts` column
is the concrete example already on the table.

### J3-R2R-4 (P2) — the new receipt state is in neither taxonomy test, and F1's fix asserts a test that does not exist

`src/writeq.rs:849-852` now reads: "**Eleven** variants … it is checked by a test now rather
than restated by hand". No such test exists. The only candidate,
`every_answer_has_a_distinct_tag_and_none_of_them_is_unknown` (`src/writeq.rs:3463`), is a
hand-written array of **ten** (`:3466-3486`) whose missing element is
`ReceiptAnswer::PendingReplay` — the variant this remediation added. So the new state's tag is
never checked for distinctness and its `describe()` never checked non-empty, while the
sentence that fixes a false count states a false claim of its own.

The sibling test is worse: `only_pending_is_unsettled` (`src/writeq.rs:3511`) asserts
`!Pending.is_settled()` and iterates the eight terminal answers. `PendingReplay` is in neither
list, and `is_settled` (`:952-954`) is now `!matches!(self, Pending | PendingReplay)`. Reverting
that arm to `!matches!(self, Pending)` — which would make `wait()` (`:2575`) return instantly
on a replay-owed receipt and the settle machinery treat it as terminal — turns **no test
red**. `pending_replay` appears in exactly one assertion in the whole tree
(`src/memory.rs:4831`), and that is an end-to-end tag check, not a taxonomy pin. (Argued from
source rather than by mutation: the identifier is absent from both tests, so no mutation of
its arm can be observed by them.)

**Remediation.** Add `ReceiptAnswer::PendingReplay` to the array at `src/writeq.rs:3466` and
`assert!(!ReceiptAnswer::PendingReplay.is_settled())` beside `:3513`, renaming that test
`only_the_two_pendings_are_unsettled`. Then make the docstring's claim true by making the
list exhaustive by construction — an `fn ordinal(&ReceiptAnswer) -> usize` with no `_` arm,
asserted to yield eleven distinct values, so adding a twelfth variant is a compile error
rather than a stale sentence.

### J3-R2R-5 (P3) — two estimator-era stated reasons the N3 sweep missed, one inside a build assert

* `src/writeq.rs:628-630` (`RECEIPT_WAIT_MAX`): "a job admitted at the instant the queue was
  full is **projected** to *start* at ≈ one budget".
* `src/writeq.rs:638-641`, the message of the const-assert at `:636`: "the queue admits work
  **it projects to drain within one budget**".

Admission projects nothing since the redesign — it is a static fairness/memory cap, as
`WRITE_QUEUE_DRAIN_BUDGET`'s own docstring says four hundred lines earlier
(`src/writeq.rs:161-168`, "the redesign retired that role"). The second is the same sub-class
as an N3 item that *was* fixed (a stated reason inside a build assert), and it leaves a live
build coupling — `RECEIPT_WAIT_MAX >= 2 × WRITE_QUEUE_DRAIN_BUDGET` — resting on a retired
rationale, which is N4's failure mode. **Fix:** restate both on the surviving reason (a close's
quiesce drains for one whole budget, so a wait of one budget would expire on jobs the quiesce
is still retiring) and drop "projected".

Three more sentences the `PendingReplay` split falsified, all in `writeq.rs`:
`:2573` ("a wait that runs out returns `Pending`" — it returns `PendingReplay` for a
replay-owed id), `:2549-2550` ("`pending` while its replay is owed"), and `:862` ("Also what a
timed-out wait answers", now half the story). Three one-line edits.

And two register siblings outside `writeq.rs`: `src/types/mod.rs:574-576` states `consumed_at`
purging as an unconditional sweep, which lines `:546-550` of the same docstring explicitly
correct (it is lazy, at the next consume, scoped to one session — `src/store/sqlite.rs:1761-1770`);
and `src/memory.rs:547-548`, the builder setter a library caller actually reaches for, still
describes `match_strategy` as recall-and-merge only, which is the half-truth
`src/types/mod.rs:207` was just fixed for.

### J3-R2R-6 (P3) — `PROBE_TEXT`'s measured table does not reproduce on the rig it names, and a constant's stated derivation rests on its last row

`src/writeq.rs:450-465` publishes a measured table for "this rig's llama.cpp BGE-M3 q8_0",
ending "**1536 B and up — HTTP 500, 8 of 8**", and calls that row the reason the short probe
text survives. `PROBE_TEXT_BYTES = 1024` is then justified (`:477-481`) as "the largest power
of two under the smallest refusal measured here". Re-measured against the live rig, median of
three `/v1/embeddings` calls each, realistic filler text:

| input | this review | the docstring |
|---|---|---|
| 35 B | 200, 15.8 ms | 13.6 ms |
| 512 B | 200, 26.2 ms | 36.3 ms |
| 1024 B | 200, 37.7 ms | 60.0 ms |
| 1280 B | 200, 49.6 ms | 75.8 ms |
| **1536 B** | **200, 54.6 ms** | **HTTP 500, 8 of 8** |
| 2048 B | 200, 71.7 ms | — |
| 3072 B | **HTTP 500** | — |

The refusal is real but sits between 2048 B and 3072 B, not at 1536 B, so the derivation
of `PROBE_TEXT_BYTES` cites a number that no longer holds (the honest reading of today's rig
would put the largest power of two under the smallest refusal at 2048). The latencies differ
by up to 1.6× in the other direction. Most likely the server was re-started with different
batch flags since; that is exactly why a measured table needs its conditions. **Fix:** re-measure
and stamp the row with the server's `-c`/`-b`/`-ub` flags and a date, or drop the numbers and
keep the shape of the argument (input length is first-order; an embedder has an input ceiling
of its own). This is in the N3 class — a load-bearing stated reason in `writeq.rs` — and the
sweep left it.

### J3-R2R-7 (P3) — the declaration and its new authority are each one surface short

* `docs/reference/api.mdx:74` and `site/src/content/docs/api.mdx:76` still read
  `| match_strategy | Hybrid | Canonical or Hybrid concept matching. |` — recall-only, the
  exact framing the remediation disowned in `src/types/mod.rs:207`. That table is where a
  library caller picks the setting, and it is the one reader surface F3's declaration missed.
* The new single authority asserts its own completeness — `src/config.rs:132-134` says
  "see `MatchStrategy`, which documents **both**" — and `match_strategy` has a **third**
  consequence: it selects the call-time validation rule set (`src/memory.rs:1455-1460`;
  `Canonical` additionally runs `reject_repeated_observation` and the single-`Hierarchical`-parent
  rule, `src/graph/derive.rs:275-277`). So the opt-out F3 sends a user to — flip to
  `Canonical` to escape the embedder dependency — also starts rejecting inputs `Hybrid`
  accepted. Stated in the design doc (`J-multi-client.md:1577-1582`), absent from the
  authority. **Fix:** a third bullet at `src/types/mod.rs:207-222` and "all three" in
  `config.rs`.

### J3-R2R-8 (P3) — the visibility that replaced the `attempts` column cannot tell "draining" from "wedged"

Declining `attempts` was right (above), and the substitute is `write_queue_replay_owed` plus
one warn line per attach plus `pending_replay` per receipt. But `replay_owed` is a *level*, and
the two situations an operator most needs to separate produce the same reading:

* a healthy backlog mid-drain — `owed=57`, falling;
* a permanently blocked replay — `owed=57`, static, because the loop broke on a store error
  or a lease move (`src/writeq.rs:2983-2999`) and will break again at every attach.

The docstring at `src/writeq.rs:2919-2924` says as much — "non-zero while `replayed` does not
move" is the readable form — but that is a *derivative*, so it needs two polls and a memory of
the first, and nothing on the stats surface records why the loop stopped. **Fix:** one more
unconditional key, `write_queue_replay_blocked` (the class of the error that ended the last
replay, or `null`), set in the non-`Embed` arm and in the liveness-gate return. One line each,
and it turns a two-poll inference into an answer.

### J3-R2R-9 (P3) — the committed N1 evidence driver's PASS is order-dependent

`evidence/mooshik-j3-durable-intents/j3_n1_outage_demo.py:152` samples
`next(iter(receipts))` — the *first* write of session 1, i.e. the one likeliest to have drained
in-session — and then asserts that receipt reads `pending_replay`. Run unmodified against my
own binary at `bc28ac8`, the driver reports:

```
session 2 (embedder at http://127.0.0.1:9): replayed=0 replay_owed=63
                                            sampled_receipt=applied_after_restart
  store: unconsumed=63 failed_rows=0 embedded=1
  N1: the outage consumed NOTHING and the debt is visible -> False
OVERALL: FAIL
```

The mechanism is green — `unconsumed=63`, `failed_rows=0`, `replayed=0`, `replay_owed=63`, and
session 3 applied all 63 with embeddings. The `FAIL` is the sampled receipt being the one write
session 1 *did* drain, which a later process answers `applied_after_restart` (declared at
`src/writeq.rs:869-874`). So the committed evidence's `OVERALL: PASS` is a timing accident, and
this driver will fail for its next reader too. **Fix:** sample a receipt from the last admitted
batch (which cannot have drained) or assert over every receipt, partitioned by the store's
`outcome_tag`.

---

## Attacks that did not land

| Attack | Outcome |
|---|---|
| **A slow-but-alive embedder passes the liveness gate and then burns the backlog one timeout at a time** (round 1's 31-minute amplifier) | **Does not land, and this is the fix's best result.** Stub proxying to the real BGE-M3 with a 40 s delay on real content, probe fast: the gate passed, the first intent timed out at `HYBRID_IO_TIMEOUT`, the arm broke — `applied=0 failed=0 owed=63`, `unconsumed=63`, `failed_rows=0`, receipt `pending_replay` — and the next healthy serve applied all 63 (`embedded=64 of 64`, `0 NULL`, `0 unconsumed`). One timeout for the attach, not 63 |
| **`kill -9` in the middle of a replay breaks exactly-once** | Does not land. Replay slowed to 2 s/intent, killed after 6 had applied: `concepts=7 embedded=7 NULL=0 unconsumed=57 failed=0 duplicate_contents=0`; the next serve replayed all 57 (`applied=57 failed=0 owed=0`), final store 64 embedded, 0 NULL, 0 duplicate contents. The consume-rides-the-commit-lock argument holds at the binary |
| **The DDL parse under-reports on Cockroach's dialect** | Does not land — `VECTOR`, `CREATE VECTOR INDEX`, `::STRING` and `ALTER … ADD COLUMN` are all non-`CREATE TABLE` and correctly ignored; ten of ten tables found in each dialect |
| **A comment defeats the parser** | Does not land in the under-reporting direction (a `--` line fails `strip_prefix` on the trimmed line). A block comment would over-report, i.e. refuse a healthy store loudly; neither migration has one |
| **`preflight_schema`'s default `Ok(())` is a hole** | Does not land. Twenty-plus test doubles and `MemoryStore` have no external schema; both production SQL adapters override |
| **A refusal strands a lease for a TTL** | Does not land. `preflight_schema` is the statement before the acquire (`src/memory.rs:691` then `:702`), and the pin asserts no lease row survives a refusal |
| **A proxy start (J2) skips the preflight, or refuses where the holder would not** | Does not land. Both roles enter through `build_attach`, and `Attach::Held` is downstream of the preflight, so a proxy runs the same check as a holder |
| **Something downstream matches `LamboError` exhaustively and now misses `EmbedUnavailable`** | Does not land. The one exhaustive match is `err_class` (`src/mcp/server.rs:554`), which pairs the two variants — so the compiler enforced it. `#[error("embed: {0}")]` is byte-identical on both variants (`src/types/mod.rs:919`, `:944`), so `Display`, receipt text and the ledger's `error_kind` really are unchanged |
| **The replay arm consumes something other than `LamboError::Embed`** | Does not land. `LamboError::Embed` has exactly **one** construction site in the whole tree (`src/graph/hybrid.rs:655`); the arm is `Err(e) if !matches!(e, LamboError::Embed(_)) => break` |
| **A `503`-while-loading reaches the classifier** | Does not land *for a total outage*: the liveness gate returns on **any** `Err`, including `Backend`, so a server that 503s everything never starts the loop. It lands only when the fault spares the 35-byte probe — which is J3-R2R-1 |
| **The N2 test passes against the reverted behaviour** | Does not land: the pre-change arm returned `Ok(Resolution::Fresh { embedding: None })`, so `.unwrap_err()` panics |
| **The gate numbers or the test-count delta are massaged** | Does not land. All four gates reproduce exactly, and the "9 added, 0 removed" claim reconciles **by name** (1016 names present at both revisions, 9 added, 0 removed) |

## Positive observations

* **The founding invariant holds at my own binary, unchanged.** The branch's unmodified
  `j3_live_demo.py`: `64 == 1 + 63 -> True`, then `replayed=63`, final store `embedded=64`,
  `NULL_rows=0`, `unconsumed=0`, `OVERALL: PASS`. The liveness gate costs the healthy path
  nothing measurable.
* **The transport class of N1 is genuinely and fully fixed**, and the fix is the right shape:
  one structural predicate at the site that knows the cause, one liveness embed, and an arm
  that consumes only the class that says something about the write. `LamboError::Embed` having
  exactly one construction site is what makes the arm auditable in one grep.
* **F5 was settled better than the review asked.** The finding asked for a probe for the
  table; the fix derives the required set *from the shipped DDL*, so it cannot drift behind
  the next migration — which is the difference between closing a finding and closing its
  class. Refusing rather than degrading is also the right call, for the reason given.
* **Both push-backs were right**, and one of them was right for a reason the remediation could
  only assert and I could measure.
* **The N3 log lines read true to an operator.** From my own live run: "write queue: bounds are
  static (lane 64, queue 1024) and no rate moves them; the rates below are telemetry measured
  on this deployment's embedder". That is the sentence the round-1 finding asked for.
* **The gates are honest to the digit** — 908/0/3, 1000/0/3, 565/0/0, verify.sh 46, fmt clean,
  clippy clean on all four claimed rows — and the name-level reconciliation shows nothing
  renamed away.

## Gate results

Re-derived independently at `bc28ac8`, summed across all **15** `test result:` lines (never
the lib line):

| Gate | Command | Claimed | Measured |
|---|---|---|---|
| fixtures | `cargo test --all --features fixtures` | 908 / 0 / 3 | **908 / 0 / 3** |
| sqlite | `cargo test --features store-sqlite,embed-fixture,fixtures` | 1000 / 0 / 3 | **1000 / 0 / 3** |
| cockroach | `cargo test --no-default-features --features store-cockroach` | 565 / 0 / 0 | **565 / 0 / 0** |
| verify | `bash scripts/observability/verify.sh` | 46 ok | **46 ok, ALL CHECKS PASSED** |
| fmt | `cargo fmt --all -- --check` | clean | **clean** |
| clippy | default; `store-sqlite,fixtures`; `ship,fixtures`; `--no-default-features store-cockroach,embed-fixture` | clean ×4 | **clean ×4, 0 warnings** |

Test-name delta `ed03266..bc28ac8`, by name: **9 added, 0 removed, 0 renamed** (1016 names at
both revisions). The nine are the five F5/N1/N2 pins named in the commit messages plus
`a_consumed_intent_past_its_retention_window_is_not_answered_from`,
`a_content_refusal_at_replay_still_settles_the_intent_failed`,
`the_unprovisioned_refusal_is_actionable` and
`preflight_schema_refuses_an_unprovisioned_or_unmigrated_target`. (A looser extraction reports
ten; the tenth, `dimensions`, is a trait method on a test double, not a test.)

**Verification-only edits: none.** Nothing in the repo was modified for this review — every
attack ran from the committed drivers or from drivers written outside the tree (in the
review's scratchpad). `git status` clean apart from this file; no commit amended. Live rig:
llama.cpp BGE-M3 q8_0 at `127.0.0.1:8080`, `{"status":"ok"}` verified before each run;
binary built by me at `LAMBO_GIT_SHA=bc28ac8`, features `store-sqlite,embed-bge`, in this
review's own target dir.

## Verdict

**REQUEST_CHANGES.**

The redesign is right and this remediation is good work: twelve of the fourteen findings are
closed at the artifact, the two argued push-backs are both correct on their merits, the
founding invariant reproduces exactly at an independently built binary, and the two hardest
new shapes I could think of — a slow-but-alive embedder and `kill -9` mid-replay — are both
handled cleanly, the first of them being the amplifier round 1 measured at 31 minutes and now
costing one timeout.

What blocks it is that N1's classification is one axis short of its own promise. The consume
decision reads the error *variant*, and the shipped adapter collapses every non-success HTTP
status into the permanent one — so an embedder that is alive and answering `503 no slot
available` (or `429`, or a proxy's `502`) destroys the entire durable backlog, one intent at a
time, in about a second. I measured 63 of 63 acked writes destroyed with the fix in place:
round 1's red numbers, through a door the fix left open. The bound the fix writes down to
excuse this — "at most the one intent that met the fault" — is false at the binary, which is
the same limit-magnitude error the remediation was closing.

Under the zero-residue rule: one P1, three P2 and five P3 to close before merge.
