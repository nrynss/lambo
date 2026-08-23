# J4 — adversarial review (REVIEW-ONLY)

You are the independent adversarial reviewer for the J4 workstream ("Lease conflicts leave
an artifact") on the J4 worktree. REVIEW-ONLY: verify and report; do NOT implement, fix, or
commit. If something is wrong, call it in your verdict and leave the fix to a remediation
round. The only file you write is your review document.

## The work reviewed

Branch `wt/j4` in the worktree `/home/nryn/work/lambo/.claude/worktrees/j4`, four commits
pushed to `origin/wt/j4` (= `b58ef91`), on top of `lambo-for-mooshik` tip `9eab99f`:
`da5495f` (store lease_refusals), `427bcc8` (serve/writeq/proxy + completion schema),
`7da87d2` (tests), `b58ef91` (docs + Done-when tick). Worktree is clean at the pushed HEAD.

## Authority (read in this order)

1. `dev-diary/lambo-for-mooshik/J-multi-client.md` §J4 (lines ~2535-2544) — the definition:
   *A serve that loses the lease exits before it can open a ledger, so the most common
   multi-agent failure is structurally invisible to I1 as specified.* Two halves: a
   **pre-lease startup line** written before the acquire attempt, and the **holder recording
   refused takeovers**. Plus the Done-when box: "A refused lease acquisition appears in the
   ledger from both sides (J4)".
2. The J2/J3 handoffs: `J-multi-client.md:768-774` (record not only *refused* but
   *proxying*, and `proxying to a holder that stopped answering`); `J-multi-client.md:2036-2039`
   and `J3-durability-redesign.md:142-144,277-281` (proof obligation 5 — the ledger
   **completion-line schema** carries durable intents on the same append path, so metric 2
   regains its facts).
3. `dev-diary/lambo-for-mooshik/I-observability.md` — the I1 ledger design J4 extends.
4. The implementer's yield (agent://J4Implement) — treat every claim as unverified until
   checked at source.

## Verify, independently, all five deliverables at source

1. **Pre-lease startup line.** `serve.rs` opens `Ledger::open` BEFORE `resolve_role` and
   appends a `startup_line` (kind=startup, state=acquiring) before the acquire attempt. So a
   serve about to lose the lease has already left an artifact.
2. **Loser records the refusal.** `resolve_role`'s refusal exits record it — a
   `lease_line(event=refused, side=loser)` appended AND persisted to the store
   (`record_lease_refusal`, best-effort).
3. **Proxy/degraded artifacts.** The `HubProxy` gained a ledger field; `run()` appends
   `proxying` on first dial and `proxying_stopped` (with the lost/undrained count) when a
   holder stops answering.
4. **Holder records refused takeovers.** `GraphStore` gained `record_lease_refusal` +
   `pending_lease_refusals` (trait defaults + Sqlite/Cockroach/Memory); the `lease_refusals`
   table is in BOTH migrations; the holder path spawns a poller (claimed 500 ms) filtering
   on its own holder token with dedup, appending `lease_line(refused_takeover, side=holder)`.
5. **Completion-line schema.** A `completion_line` builder + a ledger threaded from Memory
   through the write pipeline, emitted on `applied` / `failed` / `deferred` /
   `applied_after_restart` with `created_count` / `matched_count` (the metric-2 facts).

For each: cite file:line. Confirm the two hard constraints in the brief held:
- **Not a second ledger** — it extends I1's existing ledger (same lines, additive kinds); no
  parallel append stream. **One append path** for the completion lines.
- **Lease/single-writer untouched** — no weakening, no preemption, no fencing-token change.

## Verify the tests argue the right thing

`7da87d2` — the both-sides test (loser line + holder line on one refusal), the pre-lease
test, the proxying/degraded tests, the completion-schema test. Confirm they FAIL against the
pre-J4 base `9eab99f` (no producer existed there) — check by reasoning or by running against
the base if cheap, and say which. The J4 Done-when checkbox must be demonstrably ticked.

## Gate reconciliation (do not take the implementer's numbers on faith)

Report Claimed/Measured for each of the six: `cargo test --all --features fixtures` (they
claim 902/0 — reconcile against the J3-era ~908/912: which tests moved and why, by name);
`cargo test --features store-sqlite,embed-fixture,fixtures` (973/0); `cargo test
--no-default-features --features store-cockroach` (588/0, 1 live ignored, no DSN — confirm
it is the live test and nothing else silently skips); `bash scripts/observability/verify.sh`
(must be 46 ok AND `sample/calls.jsonl` byte-identical — the additive-no-drift claim);
`cargo fmt --all -- --check`; clippy x4. If a gate is blocked by this machine (e.g. Python
version, no DSN), say so and confirm it is environmental, not from J4.

## Output

Write your review to `dev-diary/adversarial-review/adve-review-mooshik-J4-round1.md`
(worktree path), matching the J-series review format (Method / findings table / gate results /
verdict APPROVE | REQUEST_CHANGES with graded new findings). The repo tree must be left clean
apart from your review doc.

## Report back (concise)

Per-deliverable verdict + file:line; the two constraints held or not; the tests' FAIL-on-pre
evidence; the gate table (Claimed/Measured) incl. the fixtures-count reconciliation and the
verify.sh byte-identical check; any new findings graded; overall APPROVE/REQUEST_CHANGES.
