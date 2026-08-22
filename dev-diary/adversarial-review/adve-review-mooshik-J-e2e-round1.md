# Adversarial review — workstream J end-to-end (multi-client survivability), round 1

**Reviewer**: independent E2E adversarial reviewer (Fable). REVIEW-ONLY — no file created, edited, or deleted in the repo; two temporary local mutations (one gate probe, one test-integrity probe) were made and fully reverted, verified via `git status`.
**Scope**: the whole of workstream J (J0–J5) at `lambo-for-mooshik` HEAD `313c447`, reviewed against `dev-diary/lambo-for-mooshik/J-multi-client.md` (all 2,919 lines read) and all eleven prior per-task rounds (`adve-review-mooshik-J*.md`). This round attacks the composition, not the tasks.
**Verdict**: **REQUEST_CHANGES** — no P1. Five P2 (two behavioral defects on the J4 surface, one socket-lifecycle race J2×fencing composition opens, one defective closure claim, one mandated-sweep omission with three stale load-bearing claims), seven P3. The core J story — identity across the pipe, the wedge invariant, durable intents, both-sides artifacts, the drift gate — **holds at source and at the binary**. What does not hold is, once again, the register: the workstream's own named recurring defect class (prose not keeping up with a moved claim-family) recurred in the J4/J5 landing itself, including at a docstring now stale for the third time.

## Gates (all re-run at `313c447`)

| Gate | Result |
| --- | --- |
| `cargo test --all --features fixtures` | exit 0, all green |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | **1011 / 0 / 3** repo-wide (3 ignored = live-llama/BGE, environmental) |
| `cargo clippy --all-targets --features store-sqlite,embed-fixture,fixtures -- -D warnings` | clean |
| `scripts/observability/verify.sh` | **46 ok, ALL CHECKS PASSED** |
| `scripts/docs/check-mirror-drift.sh` | green; **red under a shared-prose mutation** (verified, reverted); green after revert |
| `tests/serve_j4_lease_conflicts.rs` | 2/2; **red under mutation** (see below); 2/2 after revert |

**CI coverage verified, not assumed**: `embed-fixture` is a *default* feature (`Cargo.toml`), so the `sqlite` matrix row (`cargo test --features store-sqlite,fixtures`, `.github/workflows/ci.yml:144-145`) compiles and runs every `store-sqlite,embed-fixture,unix`-gated integration test — `serve_j4_lease_conflicts`, `serve_proxy_multi_client`, `serve_intent_durability`, `serve_pre_handshake_durability`. No J behavior is CI-blind. The `docs-mirror` job and its path filters are wired on both push and PR as claimed.

## Closures sampled at source (all real)

- **J1-R2-1**: `breaks_one_line` (`server.rs:630`) is genuinely shared by `check_agent_id` (`:1225`) and `conflict_err`'s fold (`:696`); U+2028/29 + Cc + 256-cap pinned by `every_tool_refuses_an_unusable_agent_id`.
- **J1-R2-2**: `LamboError::SoftLock` produced only by `graph/reserve.rs`; `conflict_err` selects on it (`server.rs:1838,1854`); `lease_lost_error` (which still interpolates `OPERATOR_OVERRIDE`, `memory.rs:2528-2538`) stays `Conflict` and flattens through `tool_err`. The async pipeline's fenced arm hand-writes its own clean message (`writeq.rs:2548-2554`), so no producer of the operator SQL reaches a receipt.
- **J2-R1-1**: the inflight list, generation tagging, `answer_lost`, and any-generation response retirement are all in `HubProxy::run` as described. The pipe forwards `&str` frames verbatim (`proxy.rs:1576-1580`); id extraction parses a copy — J1's untrimmed-identity contract survives J2 by construction.
- **J4-R1-1/R1-2**: `record_refused_loser` is called in `probe_holder`'s `Ok(())` arm before `Role::Proxy` (`serve.rs:1098-1106`); both lease lines carry `&self.agent` (`proxy.rs:1181-1186, 1436-1443`).
- **J5-R1-1**: the site-only strip is marker-keyed; mutation-verified meaningful (shared-prose edit → exit 1).

**Test-gate integrity, mutated for real**: removing the proxy-branch `record_refused_loser` call turned `a_proxying_serve_writes_a_proxying_line` red in 6.3s with the exact missing lines named — the two-process both-sides gate is not vacuous. Reverted; green.

**Wedge invariant**: zero `acquire_lease`/`refresh_lease`/`release_lease` call sites in `proxy.rs`/`endpoint.rs`; the only acquire is `resolve_role`'s election, pre-handshake; pinned by `a_dead_holder_leaves_the_proxy_honest_and_the_lease_unclaimed`. Holds — but see JE2E-2 and JE2E-4 for two hub-identity edges the invariant's *neighborhood* leaves open.

---

## Findings

### New defects

**JE2E-1 (P2) — `lease_refusals` grows without bound, and the holder's poller re-reads the entire history every 500 ms.**
*Claim*: J4's holder-side recorder is a cheap poll ("polls `pending_lease_refusals` for its own session… dedups", §J4 as-built).
*Evidence*: `record_refused_takeovers` computes `since` **once** at task start (`serve.rs:1253-1255`) and never advances it; its `seen` HashSet (`serve.rs:1252`) grows monotonically; no adapter ever deletes a `lease_refusals` row (`sqlite.rs:812-858`, `cockroach.rs:2229-2277`, `memory.rs:482-511` — INSERT and SELECT only, no purge, no index; contrast `write_intents`, which got a lazy retention purge).
*Failure scenario*: the workstream's own founding scenario — a client that auto-respawns a losing serve (opencode/cursor both do) against a held session inserts one row per respawn forever. On a long-lived `--ledger` holder, every 500 ms poll fetches and allocates the whole accumulated set (`refused_at >= since`, `since` frozen at task start minus TTL, so it covers the full holder uptime), quadratic work over time, plus an unbounded dedup set; across restarts the table itself grows forever on the dogfood store.
*Close*: advance `since` to the max `at` seen each poll (the dedup already tolerates overlap), add a retention DELETE mirroring `write_intents`' lazy purge, and an index on `(session_id, refused_at)`.

**JE2E-2 (P2) — a resumed ex-holder's exit unlinks the *current* holder's live socket, silently disabling multi-client attach for the new holder's whole lifetime.**
*Claim*: "the lease is what licenses the stale-socket unlink" (§J2) — true at `bind` (`endpoint.rs:277-283`), but exit-side `unlink` is licensed by nothing.
*Evidence*: `serve.rs:1591-1593` runs `endpoint.unlink()` unconditionally on the holder exit path; `SessionEndpoint::unlink` is a plain `remove_file` by path (`endpoint.rs:315-330`), and the address is a pure function of session+store, so every holder generation binds the same path.
*Failure scenario*: holder A wedges >45 s (suspended, store stall); B starts, wins the lapsed lease, `bind` lawfully clears A's stale socket and binds its own; A resumes fenced (writes refused — correct), then exits (SIGTERM, client EOF) and `unlink` removes **B's live socket**. B keeps serving its own client, but every later loser probes `ENDPOINT_NOT_ACCEPTING`, is told the holder "has most likely died" (false — `correct_the_refresh_claim`, `serve.rs:980-986`), and gets **no memory** for as long as B lives — the original J outage recreated by a race, with a misleading refusal on top.
*Close*: skip the exit unlink when `mem.lease_lost()` (or when `close` did not release the lease), or stat-compare the socket before removing. Pin with a test: fence a holder, bind a second, run the first's exit path, assert the socket survives.

**JE2E-3 (P2) — the "proxying to a holder that stopped answering" artifact exists only when calls were lost in flight; the common degraded state books nothing.**
*Claim*: §J4 as-built — "`proxying_stopped` (with the in-flight count) **when the holder it forwarded to stops answering**"; §J2's handoff names "*proxying to a holder that stopped answering*" as one of the two states J4 must record.
*Evidence*: the only `proxying_stopped` append is inside `if lost > 0` (`proxy.rs:1431-1444`). An idle connection close books nothing; every subsequent failed dial (`Dialled::Failed` → `tracing::warn!` only, `proxy.rs:1332-1336`) and every `-32001 unreachable_reply` books nothing; a successful reconnect to a new holder books nothing.
*Failure scenario*: holder dies with the proxy idle (the commonest shape — most of a session is between calls). The client's next N calls all fail `-32001`; the agent has no memory for up to 45 s+; the ledger's whole story is one `proxying` line — exactly the "why did this agent have no memory" question J4 exists to answer, unanswerable from artifacts on this path. No test covers `proxying_stopped` at all.
*Close*: book `proxying_stopped` on any `Closed` of the current generation (with `lost` as detail, possibly 0), or book a line on the first failed re-dial; add the missing test.

**JE2E-4 (P3) — a fenced ex-holder never re-evaluates its role: attached proxies and its own client get stale reads indefinitely, when exiting would now self-heal.**
*Evidence*: the fence is an AtomicBool that gates writes only (`memory.rs:336-360, 2139-2161`); nothing tears down the serve, its endpoint listener, or established proxy connections on lease loss; a proxy re-dials only when its connection closes (`proxy.rs:1308-1315`).
*Failure scenario*: after a takeover, clients still attached to the fenced ex-holder get honest write refusals but silently stale reads (no staleness label — the thing §J2's rejected read-only-attach fallback was rejected *for*), for as long as the process lives.
*Why P3 and not P2*: the precondition is rare (wedged >45 s **and** a contender started in the window), writes stay safe, and the pre-J2 behavior was identical for the holder's own client. But J2 changed the trade: a fenced serve that **exited** would be respawned by its client and come back as a proxy to the real holder — working memory restored automatically. Nobody re-made the fenced-holder decision after J2 handed it that option. Worth an explicit decision recorded either way.

### Defective or incomplete closures

**JE2E-5 (P2) — "This restores the declared metric-2 regression" is an overclaim: the producer shipped, none of the five named consumers moved, and the README now asserts the opposite of the tree.**
*Claim*: §J4 as-built (`J-multi-client.md:2589-2590`): the completion line "restores the declared metric-2 regression and closes proof obligation 5."
*Evidence*: J3's handoff defined the repair as a schema change that "moves `_ledger.py`, `dedup_rate.py`, `duplicates.py`, the observability README and `verify.sh`" (`J-multi-client.md:1532-1536`). Zero of the five moved: no kit script reads `kind:"completion"` (verified — the only grep hit is an unrelated comment in `_ledger.py:183`); `dedup_rate.py`/`duplicates.py` still see nothing from MCP sessions; `scripts/observability/README.md:368-379` still says "**What is missing is a ledger line for the *completion***… belongs to a later workstream, not to J3" — false at HEAD, J4 shipped it; `_ledger.py:10` still documents `kind ("call" | "stats")` against a five-kind ledger; `docs/reference/cli.mdx:257`'s `--ledger` row still says "one JSON line **per MCP tool call**" though the flag now also produces `startup`/`lease`/`completion` lines unconditionally.
*What is true*: proof obligation 5 as literally worded (a completion line on the same append path, with the facts) **is** met — `writeq.rs:2638-2648, 2912-2919, 3137-3147, 3199-3206` all verified, correctly attributed to the caller's per-call agent id.
*Close*: either finish the repair (teach `dedup_rate.py`/`duplicates.py` the completion join, update README/`_ledger.py`/cli.mdx, extend `verify.sh`) or narrow the diary claim to "the schema half of the repair" and update the README's "missing" paragraph to point at the now-existing line. Whichever way, the README/cli.mdx register must stop contradicting the tree.

**JE2E-6 (P2) — J4 moved the serve-startup ordering (the workstream's most-swept claim family) without the sweep the J0 carryover mandates; at least three sites are false at HEAD, one for the third time.**
*Claim family*: J0's carried guidance — "serve-startup ordering claims live in more prose sites than any one of them signals — the cheap defence when touching that ordering is an `rg` sweep for the claim." J2 ran that sweep twice. J4 moved `Ledger::open` above `resolve_role` (`serve.rs:1345`) — a real ordering move, safety argued at the move site — and its as-built contains **no sweep at all** (the only J task section without one).
*Stale sites*:
1. `ledger.rs:251-255` (`Ledger::open` docstring): "serve calls this **after** the single-writer lease is taken and … **after** the SIGTERM handler is armed (`shutdown_signal()` is the first statement once `resolve_role` returns a `Role::Holder`; this call is the next one)" — all three clauses false post-J4 (open is pre-lease, pre-arming, and nowhere near "next"). This is the exact docstring I-R2-1 corrected and I-R3-1 corrected again — **third generation of staleness at one site**, and the availability-hazard argument that follows it now rests on false premises (a blocking open would today wedge a serve that holds *no* lease).
2. `ledger.rs:884-899` (test doc for `opening_a_ledger_does_not_block…`): "serve calls `Ledger::open` after the single-writer lease is taken, so an `open` on that path would hold the lease" — false; the property it pins still holds (and matters *more* pre-lease), but the stated reason is dead.
3. `serve.rs:1407-1416` (the arming comment): enumerates "`Ledger::open`" among "the startup work **below** it" that the arming guards — false; `Ledger::open` and the startup append now run *above* `resolve_role`, unguarded (harmless because open never blocks, but the guarded-work enumeration is the load-bearing R2-a claim and it is wrong).
*Close*: run the ordering sweep J4 owed (the sites above plus an `rg` for the family), and record it in §J4's as-built the way every other J task did.

**JE2E-7 (P3) — "50 seconds by design" survives in the arming rationale in two places, contradicted by `ELECTION_BUDGET`'s own docstring in the same file; J2-R2's sweep claimed this family "re-checked and still true".**
*Evidence*: `serve.rs:1436-1441` — "`resolve_role` … a loop that can *legitimately* run for `LEASE_TTL + ELECTION_SLACK` = **50 seconds**: that is its designed behaviour" — and `serve.rs:2066-2067` ("that loop is allowed to run for 50 seconds by design"), both present tense, both citing J2-R1-7. J2-L2 cut the election to `ELECTION_BUDGET = 20s` (`serve.rs:895`), whose docstring says the 50 s formula "**used to be**" the rule. The arming decision survives at the true number (20 s of unkillable wait is still the wrong trade), so this is doc-precision — but the J2-R2 register-sweep row asserting the 50 s figures were re-checked and still true was itself defective, which is why an E2E pass caught what two J2 rounds did not.
*Close*: restate both sites at `ELECTION_BUDGET`, re-making the argument at 20 s.

### Register / doc drift

**JE2E-8 (P3) — the Done-when register contradicts itself at HEAD.** The J4 box appears twice: ticked with evidence at `J-multi-client.md:2719-2730` and **unticked** at `:2844`; J5's box at `:2845` is unticked though §J5's as-built landed and all four mirrors carry the prose (verified in the gate's canonical output). A reader auditing the box from the bottom concludes J4/J5 are open. Delete the stale pair or tick with pointers.

**JE2E-9 (P3) — the recorded runbook-edit list instructs the operator to look for a log line the tree retired.** `J-multi-client.md:2914-2919` ("J3, §2's startup line") says a J3-carrying serve logs "`write queue: bound measured on this deployment's embedder`" and that "a `WARN` in its place means the queue is on its **unmeasured floor**". Both were deleted by the round-3 redesign + N3: the shipped line is "write queue: **bounds are static** (lane {}, queue {}) and no rate moves them" (`writeq.rs:2176`), and the floor no longer exists. This section is explicitly the checklist the future DOGFOOD-SETUP edit "has to work through" — executed as written it installs false operating instructions in a runbook. N3 swept `writeq.rs`'s twelve sites but not this diary bullet.

**JE2E-10 (P3) — the mirror-drift gate has a demonstrated false-pass direction, narrow but real.** The `/lambo/` normalization is applied to *both* sides, so a site-style `/lambo/config/#http-transport` link pasted into the **reference** copy — a broken link on the docs site — passes the gate green (demonstrated live: mutate → exit 0 → reverted). This falsifies J5-round-1's "normalisation is symmetric … so it cannot mask a real shared-line difference" for exactly this class. Safe against shared-prose drift (the gate's actual job — separately verified red on a real mutation), so P3: consider refusing `/lambo/` in the reference copies before normalizing (`scripts/docs/check-mirror-drift.sh:56-63`).

**JE2E-11 (P3) — the new `lease` line's `holder` key means three different things.** On `refused side:loser` it is the incumbent's token; on `refused_takeover side:holder` it is the **loser's** token; on `proxying`/`proxying_stopped` it is a **socket path** (`proxy.rs:1186, 1441`; observed live in the mutation run's ledger dump). No consumer breaks (the kit ignores unknown kinds), but an operator grepping `holder:` gets counterparties and file paths interchangeably. Rename per-event (`counterparty`/`dialled`) or document at `ledger::lease_line`.

**JE2E-12 (P3, advisory) — the Failed-receipt text is the one model-facing surface that carries raw error strings, flattening less than `tool_err` does.** `ReceiptAnswer::Failed(why).describe()` (`writeq.rs:1035`) interpolates `e.to_string()` verbatim; every render path applies `redact_urls` (verified: `server.rs:764, 802, 1653, 1781, 2115`), the fenced arm hand-writes a clean message, and no `OPERATOR_OVERRIDE`/SQL producer reaches this path (checked) — so nothing credential-shaped flows today, and the position is stated at `attach_receipts` (`server.rs:787-788`). The residual: driver strings and store file paths (e.g. a sqlite "unable to open database file: /path") pass, where the same error through the synchronous path is flattened to "store error (the detail was logged)". Not re-reporting the declared cooperative-identity residual — and confirmed **J4/J5 did not widen** the model-facing agent-id render surface (all new J4 lines are operator-facing JSONL; serde escapes line-forgers).

### Failure-mode walkthroughs (verified clean, for the record)

- **Unclean holder death with proxies attached**: in-flight calls answered `-32002` ("outcome UNKNOWN — recall before re-deriving"); subsequent calls `-32001` with the honest 45 s framing; recovery via reconnect-on-call + bounded handshake replay once a new start wins the lapsed lease; the proxy never touches the lease. Matches the diary, pinned by tests. (The artifact gap on this path is JE2E-3.)
- **Ack then death before flush**: `call` line with receipt and no `completion` line is the crash signature; flushed intents replay with `applied_after_restart` completion lines in the *next* serve's ledger (`WriteCtx.ledger` threading verified); unflushed ones answer `restart_lost`. Deferred completion lines are written during `close()`, which runs **before** `ledger.shutdown()` (`serve.rs:1569-1603`) — nothing is dropped by ordering.
- **Fence mid-write**: per-job fence check (`writeq.rs:2536-2554`), intent deliberately unconsumed, hand-written clean message; store-level `StaleWrite` backstop. Clean.
- **J5×J2 composition**: the mcp.mdx HTTP-default prose correctly prescribes *one* serve + every client pointed at its URL (not per-client http serves, which would recreate the outage) — checked in both mirrors via the gate's canonical form.

## Environment / cleanliness

Working tree at review end: the reviewer's two probe mutations reverted (verified); one concurrent operator edit appeared mid-review (`dev-diary/lambo-for-mooshik/K-candle-embedder.md`, +6 lines, dated today, operator-signed, K-workstream — untouched by the reviewer); two untracked probe-debris files at the repo root (`r2-shape.stderr`, `r2-shape2.stderr`, J2-round-2 leftovers from 2026-08-20) — worth deleting or gitignoring.

## What the per-task rounds structurally could not have seen

1. **A moved ordering's blast radius lands in *other* tasks' files.** J4's `Ledger::open` move falsified I-era and J2-era claims in `ledger.rs` and `serve.rs` (JE2E-6) — files J4's round-1 reviewer read only for the J4 deliverables, and which no J4-scoped sweep existed to cover because J4 ran none. The third-generation staleness at `ledger.rs:251` is only visible to a pass that carries the I→J0→J2 history.
2. **Handoff accounting across tasks.** J3 declared a regression and specified its repair as five files; J4 claimed the restoration while touching one (JE2E-5). Each round graded its own task's literal deliverables correct — only the join shows the claim and the README asserting opposite facts.
3. **Composition of two correct mechanisms into one race.** The lease-licensed *bind*-side unlink (J2, reviewed clean) and the unconditional *exit*-side unlink (pre-J2 hygiene, reviewed clean) compose into JE2E-2 — the current holder's live socket deleted by a fenced predecessor. Neither round had the other's mechanism in scope.
4. **A fixed defect's dual on the untested path.** J4's rounds proved `proxying_stopped` exists and the both-sides test is real; only walking the *idle*-death and re-dial paths end-to-end shows the artifact never fires on the commonest degraded shape (JE2E-3), and that no test would notice.
5. **Sweep-claims audited as claims.** J2-R2's register table asserted the 50 s family clean; two sites in the same file were not (JE2E-7). Per-task rounds verify sweeps of *their* round's families; only an E2E pass re-runs an old round's sweep against today's tree.
6. **Gate false-pass directions the gate's own round argued away.** J5-R1 reasoned symmetry makes masking impossible; a live probe from outside that round's framing found the one asymmetric input (JE2E-10) in five minutes.

What survived every attack, and deserves saying: the byte-verbatim pipe (J1×J2), the wedge invariant, the durable-intent truth table (J3×J2 proxy, J3×J4 completion lines), the caller-id attribution on completion lines through a proxy, the F18 store-clock discipline on `lease_refusals`, N4 on every new error producer, and the two-process both-sides test — which went red under mutation exactly where it should.
