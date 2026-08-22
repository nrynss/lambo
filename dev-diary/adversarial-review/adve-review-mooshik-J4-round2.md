# Adversarial review — mooshik J4, round 2 (confirmation of remediation `eb1507f`)

**Reviewer**: independent adversarial reviewer, agent_id `J4Review2`. REVIEW-ONLY — wrote
nothing under review except this file; no commit amended; no code changed.
**Scope**: the remediation commit `eb1507f` (`b58ef91..eb1507f`, pushed), closing the two
round-1 findings J4-R1-1 (P3) and J4-R1-2 (upgraded P2). **Verdict**: **APPROVE** — the
branch is clean and ready to integrate. Both closures are real, honest, and load-bearing at
source, covered end-to-end by a genuine two-process test, and every gate re-runs clean.
**HEAD `eb1507f` == `origin/wt/j4`; tree clean** (no untracked/modified files apart from
this doc).

## Per-finding disposition (confirmed at source, not from the remediation's yield)

### J4-R1-1 — CLOSED (wired, not doc-corrected)
`record_refused_loser(ledger, &held.store, &session, &opts.agent, &my_token,
&held.current.holder)` is now called in the `Ok(())` arm of `probe_holder`
(`src/mcp/serve.rs:1098-1106`) **immediately before** `return Ok(Role::Proxy(...))`
(at `serve.rs:1107-1114`), reusing exactly the same helper and the same argument shape as
the four terminal-refusal exits (`serve.rs:1061,1077,1129,1155`). The holder is passed as
`held.current.holder`, the same field the terminal exits use, so the incumbent poller
(500 ms, filtered on own token) learns it was contended from the persisted
`lease_refusals` row. This makes "from both sides" hold on the stdio loser→proxy path and
matches the as-built `J-multi-client.md:2603-2604` claim. The refusal decision itself is
unchanged — `record_refused_loser` is best-effort (`let _ =` in the helper, pre-existing)
and cannot alter the outcome.

**Two-process both-sides evidence.** The extended `a_proxying_serve_writes_a_proxying_line`
(`tests/serve_j4_lease_conflicts.rs:279-405`) drives **two real `lambo serve`
subprocesses** via `spawn_serve` (`Command::new(env!("CARGO_BIN_EXE_lambo"))`, `:65`), each
on `--transport stdio`, sharing one SQLite file and one `--ledger` path: holder **A**
(`agent-a`, initialized and confirmed holder at `:307-326`) and proxy **B** (`agent-b`,
`:330`). The `wait_ledger` predicate (`:342-353`) requires **all three** conditions to
become true in the same ledger file: (a) a `kind:lease event:proxying agent_id=="agent-b"`
line, (b) a `event:refused side:loser` line, and (c) an `event:refused_takeover
side:holder` line. `wait_ledger` (`:98-115`) polls up to 5 s and **panics** on timeout, so
if either side's line were absent the predicate would never hold and the test would FAIL —
there is no mock, no fixture-ledger, no reading of the peer's own file; the holder's
`refused_takeover` line is written by process A ("A's refused_takeover … learned it was
contended, through the store, from a separate process", `:367-369`) from the row B
persisted. The explicit post-predicate asserts then pin the per-line fields
(`:370-397`): loser side:loser, agent-b, holder `agent-a@…`; holder side:holder, agent-a,
holder `agent-b@…`. **Measured passing** (below).

### J4-R1-2 — CLOSED
No literal `agent="proxy"` remains on the lease lines — `grep '"proxy"' src/mcp/proxy.rs`
returns **zero** hits. `HubProxy` gained an `agent: String` field threaded through
`HubProxy::new` (`src/mcp/proxy.rs:887-920`); `serve` passes `opts.agent.clone()`
(`serve.rs:1112`); both `lease_line` calls now pass `&self.agent` instead of `"proxy"` —
`proxying` at `proxy.rs:1185` (`agent` arg) and `proxying_stopped` at `proxy.rs:1440`.
Flow is therefore `opts.agent` → `HubProxy::new` → both `lease_line` calls. The only other
`HubProxy::new` caller, the in-file unit test helper `proxy_onto_a_hung_store`
(`proxy.rs:1688`), was updated to the new signature. The test asserts the `proxying` line's
`agent_id == "agent-b"` and never `"proxy"` — both in the wait predicate (`:344`) and the
explicit `assert_eq!(px["agent_id"], "agent-b", …)` (`:362-365`).

## Constraints re-confirmed (quick)
- **Lease / single-writer untouched** — remediation diff touches only
  `src/mcp/proxy.rs`, `src/mcp/serve.rs`, `tests/serve_j4_lease_conflicts.rs`, and the
  review doc. `src/store/lease.rs`, `writeq.rs`, `ledger.rs`, and both migrations are **not
  in the diff**; no second ledger, no new `Ledger::open`, no append-path change. The single
  writer-thread `Ledger::append` path is therefore unchanged.
- **NOT-a-second-ledger** — the proxy still books its own `proxying`/`proxying_stopped`
  lines onto the same shared ledger the serve opened pre-lease (passed as `ledger.clone()`
  into `HubProxy::new`), and the new `record_refused_loser` call appends through that same
  ledger. No new ledger file is created anywhere.

## Gate table (Claimed vs Measured)

| Gate | Claimed | Measured (this session) |
| --- | --- | --- |
| `cargo test --features store-sqlite,embed-fixture,fixtures` | 973 / 0 / 1 | **973 / 0 / 1** (lib); all integration rows ok incl. `serve_j4_lease_conflicts` 2/2 |
| `serve_j4_lease_conflicts` (targeted, `--nocapture`) | 2/2 | **2/2 ok** — including the extended proxy both-sides test |
| `bash scripts/observability/verify.sh` | 46 ok, ALL CHECKS PASSED | **46 ok, ALL CHECKS PASSED** |
| `cargo fmt --all -- --check` | clean | **clean** (exit 0) |
| `cargo clippy --all-targets --features store-sqlite,embed-fixture,fixtures -- -D warnings` | clean | **clean** (exit 0) |
| `cargo check --no-default-features --features store-cockroach` | compiles | **compiles** (exit 0) |

**Cockroach gate — env-blocked, not code-broken.** The feature set compiles cleanly
(measured above). The live conformance tests are `#[ignore = "live: requires
LAMBO_COCKROACH_DSN"]` and go through `dsn_or_skip` (`cockroach.rs:3674`), which reports a
missing DSN as **ignored**, never skip-as-green, and panics under `LAMBO_REQUIRE_LIVE`.
This machine has **no `LAMBO_COCKROACH_DSN`** in the environment (verified), so the gate is
environment-blocked, exactly as round-1 measured. The remediation touches no Cockroach-only
code.

## New findings
None. Both round-1 findings are genuinely closed; the closures introduce no new issue.

## Environment / cleanliness
Worktree left clean — `git status --short` empty apart from this review doc (untracked,
uncommitted, as required). HEAD `eb1507f` == `origin/wt/j4`. No commits made. `verify.sh`
needs `python3` (present). `cargo test`/`check`/`clippy`/`fmt` all exercised in-place.
