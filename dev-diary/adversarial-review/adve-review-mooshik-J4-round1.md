# Adversarial review — mooshik J4, round 1

**Reviewer**: independent adversarial reviewer, agent_id `J4Reviewer`. Wrote nothing under
review except this file.
**Scope**: the four commits `da5495f..b58ef91` on `wt/j4` (worktree
`.claude/worktrees/j4`, base `lambo-for-mooshik` tip `9eab99f`): `da5495f` (store
`lease_refusals`), `427bcc8` (serve/proxy/writeq + completion schema), `7da87d2` (contract
tests), `b58ef91` (docs + Done-when tick). REVIEW-ONLY: no commit amended, no fix applied.
**Authority**: `J-multi-client.md` §J4 (lines ~2535-2606) incl. the Done-when box
"a refused lease acquisition appears in the ledger from both sides (J4)"; the J2/J3 J4
handoffs (`J-multi-client.md:768-774` proxying & proxying-stopped; `:2036-2039`,
`J3-durability-redesign.md:142-144,277-281` proof obligation 5, completion-line schema on
the same append path); `I-observability.md` (the I1 ledger J4 extends); the implementer's
yield (`agent://J4Implement`), every claim checked at source.
**Verdict**: **APPROVE** — all five deliverables present and load-bearing at source, both
hard constraints hold, the both-sides contract test passes end-to-end across two real
processes, the tests demonstrably FAIL on the pre-J4 base, and every gate reconciles. Two
graded **P3** findings (one doc/design mismatch, one data-quality nit); neither blocks.

## Summary

J4 is faithfully what the brief names it: a requirement placed on **I1's existing ledger**,
not a second one. Every new line (`startup`, `lease`, `completion`) rides the same
`Ledger::append` → single writer-thread → single file path the `call`/`stats` lines use; the
only production `Ledger::open` is the one `serve()` now opens pre-lease
(`serve.rs:1330`). The single-writer lease machinery is untouched — the only change to
`src/store/lease.rs` is an additive `LeaseRefusal` value type, with no weakening,
pre-emption, or fencing-token change. Both hard constraints hold.

The centerpiece — the J4 Done-when checkbox — is demonstrably true. The both-sides test
(`tests/serve_j4_lease_conflicts.rs:123-273`) drives two genuine `lambo serve` subprocesses
(the http loser and the stdio incumbent) sharing one SQLite file and one `--ledger` path,
asserts the loser's `kind:lease event:refused side:loser` line *and* the incumbent's
`kind:lease event:refused_takeover side:holder` line (written by a separate process that
learned of the refusal through the store, not by reading the ledger), asserts both startup
lines, and passes. The proxy and completion tests pass too.

## Deliverables (verified at source)

| # | Deliverable | Verdict | file:line |
| --- | --- | --- | --- |
| 1 | **Pre-lease startup line** — `serve()` opens `Ledger::open` BEFORE `resolve_role` and appends `kind:startup` before the acquire; refused-exit path drains the ledger | **HELD** | `serve.rs:1330-1335` (open+before acquire), `1331-1332` (startup append), `1177-1189`/`ledger.rs:579-587` (builder), `1343-1348` (Err drain), `1385-1387` (proxy branch drain). `Ledger::open` never fails/blocks (`ledger.rs:884-899`), so moving it pre-lease is free |
| 2 | **Loser records the refusal** — `resolve_role`'s refusal exits append `kind:lease event:refused side:loser` AND persist via `store.record_lease_refusal` (store clock, best-effort) | **HELD** | `serve.rs:1061,1077,1114,1140` (four terminal exits → `record_refused_loser`), `1195-1214` (append + persist, `let _ =` best-effort), `store/mod.rs:561-568` (trait), `lease.rs:270-280` (`LeaseRefusal`), SQLite `sqlite.rs:812-830`, Cockroach `cockroach.rs:2229-2247`, Memory `store/memory.rs:482-497` |
| 3 | **Proxy/degraded artifacts** — `HubProxy` gained a ledger field; `run()` appends `proxying` on first dial and `proxying_stopped` (with in-flight count) when the holder stops answering | **HELD** | `proxy.rs:887-893` (field), `1173-1182` (proxying), `1428-1437` (proxying_stopped, `lost` in detail). Branch exits drain the proxy ledger (`serve.rs:1385-1387`) |
| 4 | **Holder records refused takeovers** — `record_lease_refusal`+`pending_lease_refusals` (trait defaults + all three stores), `lease_refusals` table in BOTH migrations, 500 ms serve-level poller filtered on own holder token with dedup, appending `kind:lease event:refused_takeover side:holder` | **HELD** | `serve.rs:1216-1267` (poller, `REFUSAL_POLL_INTERVAL`=500 ms, `current_holder != my_token` filter, `(refused_by, at)` dedup), spawn `1473-1486` (holder path only, only with `--ledger`), abort `1566-1568` before ledger drain; `store/mod.rs:574-580` (pending), SQLite `sqlite.rs:833-858`, Cockroach `cockroach.rs:2250-2277`, Memory `store/memory.rs:499-511`; migrations `sqlite/001_init.sql:163-174`, `cockroach/001_init.sql:216-225` |
| 5 | **Completion-line schema** — ledger threaded Memory→write pipeline, emitted on `applied`/`failed`/`deferred`/`applied_after_restart` with `created_count`/`matched_count` (metric-2 facts), same append path | **HELD** | builder `ledger.rs:625-645`; `WriteCtx.ledger` `writeq.rs:1594-1599`, threaded `memory.rs:911-927`; `applied` `writeq.rs:2638-2648`, `failed` `2653-2660`, `deferred` `2912-2919`, `applied_after_restart` `3137-3147`, replay-`failed` `3199-3206` |

**Constraints.**
- *Not a second ledger / one append path* — **HELD**. One production `Ledger::open`
  (`serve.rs:1330`); every J4 line including completion goes through `Ledger::append`
  (`writeq.rs:2638,2653,2912,3137,3199`), the same writer-thread channel as `call`/`stats`.
  `verify.sh` still parses exactly what it always parsed; new kinds are additive and the kit
  ignores unknown kinds.
- *Lease / single-writer untouched* — **HELD**. `git diff 9eab99f..b58ef91 --
  src/store/lease.rs` adds only the `LeaseRefusal` value type and docs; `acquire` /
  `release` / fencing / TTL are unchanged. No weakening, no pre-emption, no `current_token`
  change.

## The tests argue the right thing, and demonstrably FAIL on pre-J4

`7da87d2` adds three contract tests:

- `refused_acquire_appears_in_the_ledger_from_both_sides`
  (`tests/serve_j4_lease_conflicts.rs:123-273`) — the Done-when. Drives two real
  subprocesses sharing one SQLite file + one `--ledger` path; asserts loser line +
  holder line with correct agent roles (`agent-b` loser, `agent-a` holder), asserts the
  pre-lease `startup` line for BOTH, and asserts the http loser fails closed. **Passes
  (measured).**
- `a_proxying_serve_writes_a_proxying_line` (`:279-349`) — J2 handoff. **Passes.**
- `j4_a_completion_line_records_the_applied_derive_lifecycle`
  (`src/writeq.rs:5455-5505`) — proof obligation 5, asserts `state==applied`,
  `created_count==1`, `matched_count==0`. **Passes.**

Plus `ledger::tests::j4_line_builders_carry_the_field_head_and_no_aliases`
(`ledger.rs:995-1058`). Both-sides + proxy are `store-sqlite`-gated (compile to 0 in the
default gate) — same convention as their J3 sibling.

**FAIL-on-pre evidence.** I established this by **reasoning plus symbol absence at the
base, and a clean measurement of the two relevant gates at `9eab99f`** (a throwaway worktree
with its own target dir, since removed):
- `startup_line`, `completion_line`, `record_lease_refusal`, `record_refused_loser` and the
  three J4 test names are **absent** at `9eab99f` (`grep` confirmed zero hits). The tests
  reference APIs that do not compile on the base, so they cannot pass there.
- Behaviorally, base `serve()` opens the ledger only on the holder path; a losing serve
  never reaches it, so no `startup`/`refused` line and no persisted refusal can be produced.
  Base's `serve_single_writer_lease.rs` still pins fail-closed enforcement, but J4's
  artifact assertions have no producer.
- The new producers are, in turn, load-bearing: the 500 ms poller / `lease_refusals` row are
  the *only* path by which the holder's `refused_takeover` line can appear, and the test
  requires that line from a separate live process.

The §J4 Done-when checkbox is ticked at `J-multi-client.md:2691-2698` with the mechanism
stated.

## Gate reconciliation (Claimed vs Measured — all re-run independently)

| Gate | Claimed | Measured (J4 HEAD) |
| --- | --- | --- |
| `cargo test --all --features fixtures` | 902 / 0 / 1 | **902 / 0 / 1** — lib 902 passed, 1 ignored (`embed::bge_m3::tests::live_smoke_against_llama_server`, environmental); sqlite-gated integration rows compile to 0; all rows `ok` |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 973 / 0 / 1 | **973 / 0 / 1** — lib 973 passed, 1 ignored (same live llama test); `serve_j4_lease_conflicts` 2/2; all rows `ok` |
| `cargo test --no-default-features --features store-cockroach` | 588 / 0 / 1 | **559 / 0 / 0** — lib 559; the implementer's 588/1 corresponds to the `+embed-fixture` variant (I measured that: 588 / 0 / 1), not the gate's exact command. See note below |
| `bash scripts/observability/verify.sh` | 46 ok, byte-identical | **46 ok, ALL CHECKS PASSED**; `sample/calls.jsonl` md5 `d95898f2…` unchanged before and after (regenerated, then re-matched) |
| `cargo fmt --all -- --check` | clean | **clean** (exit 0) |
| `cargo clippy --all-targets -- -D warnings` ×4 | clean | **clean ×4** — fixtures / store-sqlite,embed-fixture,fixtures / no-default store-cockroach / all-features, all exit 0 |

**Fixtures-count reconciliation (by name).** Measured at the actual pre-J4 base `9eab99f`
(own target dir): fixtures lib = **900** / 0 / 1, sqlite lib = **971** / 0 / 1. J4 = 902 and
973 respectively — a precise **+2** each, exactly the two new default-feature J4 lib tests:
`ledger::tests::j4_line_builders_carry_the_field_head_and_no_aliases` and
`writeq::pipeline_tests::j4_a_completion_line_records_the_applied_derive_lifecycle`. The two
other J4 tests are `store-sqlite`-gated integration tests (counted in the sqlite gate's
`serve_j4_lease_conflicts` row). No test removed, renamed, or de-ignored; the 1 ignored
(live llama) is identical and pre-existing. The brief's "J3-era ~908/912" is not
reproducible from the actual base (900) — that gap is J3-era measurement drift, not J4.

**Cockroach gate — environment-blocked, not J4-caused.** The live conformance suite
(`cockroach.rs:3636-3711`) is `#[cfg(all(test, feature="store-cockroach", feature="fixtures"))]`
and needs `LAMBO_COCKROACH_DSN`; this machine has none. Without `fixtures` the module is not
even compiled (hence the exact gate's 0 ignored — **nothing silently skips**); with
`fixtures` the live tests report **honestly `#[ignore]`d** via `dsn_or_skip`, never
skip-as-green, and a `LAMBO_REQUIRE_LIVE=1` run fails loudly on a missing DSN. J4 added no
Cockroach unit tests (base cockroach lib was also 559), so the count is unchanged by J4; the
588↔559 discrepancy is a feature-set/command mismatch in the implementer's gate table, not a
code defect.

## New findings

### J4-R1-1 (P3) — the §J4 deviation note overclaims the proxy path: a degrade-to-proxy loser records no store refusal
`J-multi-client.md:2603-2604` states *"A refused loser still records its store refusal even
when it degrades to a proxy (the acquire was refused; the holder should know it was
contended)."* The shipped code does not do this. `record_refused_loser` is called at exactly
the **four terminal-refusal exits** (`serve.rs:1061, 1077, 1114, 1140`); the Proxy branch
(`serve.rs:1091-1100`) returns `Ok(Role::Proxy(...))` without calling it. So on the stdio
loser→proxy path there is **no `lease_refusals` row and no `kind:lease event:refused
side:loser` line** — the incumbent's poller never learns it was contended, and the runner-up
side of "from both sides" does not exist for that path (the loser instead writes `proxying`
lines, which is visible but is not a *refused* record). Whether this is the intended design
(e.g. a proxied runner-up is not a refused one) is defensible, but the note claims behavior
the code does not have. **Actionable**: correct the note to state that the proxy path records
`proxying`/`proxying_stopped` lines only and deliberately writes no store refusal (or wire
`record_refused_loser` into the proxy branch if the contended-acquisition record is wanted).
file:`dev-diary/lambo-for-mooshik/J-multi-client.md:2603-2604`, `src/mcp/serve.rs:1091-1100`.

### J4-R1-2 (P3) — `proxying`/`proxying_stopped` lines hardcode `agent_id:"proxy"` instead of the proxying agent
`proxy.rs:1178` and `:1433` pass the literal `"proxy"` as the `agent` argument to
`lease_line`; `HubProxy` is not given its own agent (`proxy.rs:880-910` takes
session/endpoint/store/host/ledger, no agent). Every other J4 line carries the real agent.
In practice the identity is recoverable because the same serve's `startup` line carries the
real agent in the shared ledger (and the runner-up `proxying` path is always preceded by the
startup line), so impact is low — but on the line itself the proxying actor is anonymous.
**Actionable**: pass `opts.agent` into `HubProxy::new` and use it as the `agent` on both
lines.
file:`src/mcp/proxy.rs:1178`, `src/mcp/proxy.rs:1433`.

## Residual (not graded, pre-existing)
A refused loser that degrades to a proxy leaves no `refused` record (finding J4-R1-1) — the
J3-era `serve_single_writer_lease.rs` fail-closed enforcement is unaffected, and the
http-terminal path (the test's subject) is fully covered from both sides.

## Environment / cleanliness
Cockroach gate needs `LAMBO_COCKROACH_DSN` (absent) — environmental, not J4-caused; the
feature compiles and its offline rows pass. `verify.sh` needs `python3` (present).
Worktree left clean apart from this file; no commits made; the throwaway base worktree and its
target dir were created for measurement and removed.

## Disposition — round-1 remediation (J4-R1-1, J4-R1-2)

Both findings are **closed** by the round-1 remediation on `wt/j4` (the commit that contains
this disposition; see `git log` on `wt/j4`). Neither required a change to the lease /
single-writer machinery; both stay in the J4 ledger-path additive convention, and both are
covered by the extended probe-path integration test.

**J4-R1-1 — CLOSED, wired (not doc-corrected).** The remediation took the brief's preferred
path: `record_refused_loser` is now called in the Proxy branch before a degrade-to-proxy loser
returns `Role::Proxy` (`src/mcp/serve.rs` — the `Ok(())` arm of `probe_holder`), reusing the
same helper the four terminal-refusal exits use. A loser that can still proxy to the holder
was *refused the acquisition*, so the incumbent must learn it was contended — this is the
point of the J4 metric-6 / why-no-memory story, and it makes "from both sides" genuinely hold
on the proxy path, matching the as-built `J-multi-client.md:2603-2604` claim rather than
correcting the note away. The `proxying`/`proxying_stopped` lines are unchanged and still
written by the proxy; the refusal is now additionally persisted (store `lease_refusals` row +
`kind:lease event:refused side:loser` ledger line) exactly like the terminal exits.

**J4-R1-2 — CLOSED, graded P2 (upgraded from P3 on operator review).** The proxying and
proxying_stopped lease lines now carry the real agent id instead of the literal `"proxy"`:
`HubProxy` gained an `agent` field threaded through `HubProxy::new`
(`src/mcp/proxy.rs`), the serve feeds it `opts.agent` (`src/mcp/serve.rs`), and both
`lease_line` calls use `&self.agent` (`proxy.rs` `proxying` and `proxying_stopped`).
**Grade upgrade rationale:** the `proxying` line is J4's signature diagnostic artifact for the
loser side, and an anonymized `agent_id:"proxy"` undermines the "why no memory / metric 6"
purpose — a reader of the line could not tell which agent degraded to proxying. The cost of
the fix is nil (the agent is already known to `serve`), so it is a data-quality defect that
should be corrected, hence **P2** rather than the original P3.

**Test evidence.** The existing `a_proxying_serve_writes_a_proxying_line` integration test
(`tests/serve_j4_lease_conflicts.rs`, `store-sqlite`/`embed-fixture`-gated) was extended to
assert, on the real two-process stdio loser→proxy path: (a) the `proxying` line's `agent_id`
equals the real proxying agent (`agent-b`), never `"proxy"` (J4-R1-2); and (b) **both sides**
of the refusal appear — the proxy loser's own `refused side:loser` line *and* the holder's
`refused_takeover side:holder` line, the latter produced by the separate holder process from
the persisted store row (J4-R1-1). The test passes (measured, `serve_j4_lease_conflicts`
2/2 under `--features store-sqlite,embed-fixture,fixtures`).
