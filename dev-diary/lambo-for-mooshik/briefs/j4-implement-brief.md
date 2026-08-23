# J4 — Lease conflicts leave an artifact (implement brief)

Work entirely inside the worktree `/home/nryn/work/lambo/.claude/worktrees/j4` (branch
`wt/j4`, at the `lambo-for-mooshik` tip `9eab99f`). This is a single implementer's round:
you own the whole workstream. Do NOT integrate (merge into `lambo-for-mooshik`) — the
operator gates that after the review + remediation rounds clear.

## The workstream (read these first, in order)

1. `dev-diary/lambo-for-mooshik/J-multi-client.md` §J4 (lines ~2535-2544) — the definition:
   *A serve that loses the lease exits before it can open a ledger, so the most common
   multi-agent failure is structurally invisible to I1 as specified.* Two halves: (a) a
   **pre-lease startup line** written before the acquire attempt; (b) the **holder
   recording refused takeovers**. Without these, metric 6 friction and every "why did this
   agent have no memory" question stay unanswerable from artifacts.
2. The same file's Done-when box (`J-multi-client.md:2596+`): **"A refused lease acquisition
   appears in the ledger from both sides (J4)"**.
3. The J2/J3 handoffs mentioning J4:
   * `J-multi-client.md:768-774` — the proxy-degraded state's artifact is J4's: record not
     only *refused* but *proxying*, and *proxying to a holder that stopped answering*.
   * `J-multi-client.md:2036-2039` and `J3-durability-redesign.md:142-144,277-281` —
     **proof obligation 5 (intents ride the ledger)** is deferred to J4, whose
     **completion-line schema** is the vehicle; same disposition as the declared metric-2
     regression (the one-append-path reason). The `write_intents` table is queryable meanwhile.
4. `dev-diary/lambo-for-mooshik/I-observability.md` — the ledger (I1) design: the JSONL
   line shape, `Ledger::open`, `--ledger`/`--ledger-heartbeat`, and how
   `scripts/observability/*` consume it. **J4 is a requirement placed on I1, not a second
   ledger** — extend the existing ledger, do not build a parallel one.
5. Read at source: `src/ledger.rs`, `src/mcp/serve.rs` (startup ordering — the acquire
   happens where J2's proxy branches), `src/mcp/proxy.rs`, `src/store/lease.rs`
   (acquire/refusal machinery), and `scripts/observability/` (the consumers you must not
   break).

## Scope (build all of it)

1. **Pre-lease startup line.** Before the lease-acquire attempt, the serve writes a ledger
   line recording its intent to acquire (session, agent, role). So a serve that is about to
   lose the lease has already left an artifact — the acquire itself cannot be where its
   story starts.
2. **Holder records refused takeovers.** When a second writer attempts the lease and the
   holder (or the store) refuses it, that refusal is recorded in the holder's ledger — both
   the refused loser's line and the refusing holder's line ("from both sides").
3. **Proxy / degraded artifacts (J2 handoff).** A proxying serve is alive and can write its
   own lines: record `proxying` (which holder it forwards to) and `proxying to a holder
   that stopped answering`. The `write_queue_replay_blocked` seam from J3's R-8 may be
   relevant where the holder is unreachable.
4. **Proof obligation 5 — the completion-line schema.** The ledger gains a completion-line
   schema so a durable write intent's lifecycle (admitted → applied/failed/deferred) can be
   measured; this restores metric 2's facts (the declared regression). Wire the durable
   intents (J3) so their completion reaches the ledger on the same append path as every
   other ledger line — **one append path**, do not create a second one.
5. **Docs + dispositions.** Update `J-multi-client.md` §J4 with the as-built design (the
   "from both sides" closure, the completion-line schema shape), and any Deviation notes.

## Hard constraints

- **Single-writer stays the deployment model; the lease is untouched** (no weakening,
  no preemption, no fencing-token change). J is about what the losers record.
- **Not a second ledger.** Reuse `Ledger::open` and the existing JSONL line stream. If the
  schema grows, keep every existing line shape intact so `scripts/observability/*` and
  `verify.sh` keep working — `verify.sh` must stay at 46 ok with `sample/calls.jsonl`
  byte-identical unless you deliberately and defensibly extend it (say so).
- **One append path.** The completion lines must ride the same append the other ledger
  lines use (the design names this explicitly). No skip-ahead or side-channel appends.
- Gate, per house convention, and report Claimed/Measured for each:
  `cargo test --all --features fixtures`; `cargo test --features
  store-sqlite,embed-fixture,fixtures`; `cargo test --no-default-features --features
  store-cockroach` (live with the `.env` DSN if it clears); `bash
  scripts/observability/verify.sh` (must be 46 ok); `cargo fmt --all -- --check`; clippy on
  the four rows.

## Deliverables

1. Code + tests that defend the new observable contracts (pre-lease line present before a
   refused acquire; refusal recorded on BOTH sides; proxying/degraded lines; the
   completion-line schema restores metric-2 facts). Tests FAIL on the pre-J4 behavior.
2. A test proving "a refused lease acquisition appears in the ledger from both sides" — the
   J4 Done-when checkbox, ticked.
3. Design-doc as-built section + dispositions.
4. Commit conventionally on `wt/j4` as logical units, then `git push origin wt/j4` so the
   branch travels. Report the commit hashes.

## Report back
Per-deliverable status, the gate table (Claimed/Measured), the both-sides test evidence, what
you could not close and why, final clean `git status` in the worktree, push confirmation.
