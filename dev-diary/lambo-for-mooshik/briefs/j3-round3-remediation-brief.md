# J3 round-3 remediation — brief for the implementer

You are implementing adversarial-review round 3's remediation for the J3 durability
redesign on branch `wt/j3`. This is a single writer's remediation: you are responsible for
the whole round, from the prescribed P1 fix to the register sweep to the gate. There is no
second implementer; do not assume anyone else closes a finding.

## Authoritative sources (read these first, in this order)

1. `dev-diary/lambo-for-mooshik/J3-durability-redesign.md` — the design of record. Its
   `## Prescribed design for J3-R2R-1 (round 2's P1)` section is the prescribed fix you must
   implement; deviate only with a written argument.
2. `dev-diary/adversarial-review/adve-review-mooshik-J3-redesign-round2.md` — round 2's full
   review: one P1 (J3-R2R-1), three P2 (J3-R2R-2/3/4), five P3 (J3-R2R-5..9), each with
   file:line anchors and a prescribed remediation. Implement those as stated.
3. `dev-diary/adversarial-review/adve-review-mooshik-J3-redesign-round1.md` — reads as
   background on the N-series; not required unless a reference confuses you.

## Working tree

- Repo: `/home/nryn/work/lambo`, branch `wt/j3`, HEAD `7c036f0`. Work here directly;
  commit to `wt/j3`. Do NOT touch `main`/`lambo-for-mooshik`. Do NOT create a parallel
  implementation; this is the one.
- Conventional commits, matching the branch's style: `fix(writeq): …`, `test(hybrid): …`,
  `docs(j3): …`. Commit as distinct logical units (rule table, symmetry, taxonomy tests,
  register sweep, evidence driver, dispositions) rather than one blob.

## The findings to close (from round 2)

### J3-R2R-1 (P1) — the prescribed rule table. Implement EXACTLY the design doc's
`## Prescribed design for J3-R2R-1` section:
- `src/embed/bge_m3.rs:151-166`: replace the single non-success arm with an exhaustive,
  priority-ordered status table with NO wildcard default producing `Backend`. Classes:
  transient (conn failures unchanged; 408/425/429; 500; 502/503/504; un-named 5xx),
  content (400/413/415/422), permanent-config (401/403/404 → fail the write AND warn
  loudly), unclassified (everything else → treated as transient AND logged with the status).
- The class must be decided at the site that knows the status (the adapter), carried on
  `EmbedError` as a class, not re-derived from a message string. `is_transient` should need
  no change (verify).
- Terminating measure on the replay loop, Euclid's discipline: bound the LOOP (consecutive
  `LamboError::Embed` refusals within one attach, break after the bound and leave the rest
  durable), never the write's survival. The doc notes why the `attempts` column is wrong.
- Correct the self-bounding sentence at `src/embed/mod.rs:69-72` and wherever the Done-when
  box repeats it ("at most the one intent…" is FALSE; restate at the magnitude the table
  actually delivers — a 5xx now interrupts the loop and preserves the backlog).

### J3-R2R-2 (P2) — in-session symmetry. The in-session worker arm
`src/writeq.rs:2487-2496` still consumes `EmbedUnavailable` as `failed`. Symmetric handling
so an acked write reached during an embedder outage is not destroyed as `failed`. Then make
the docs true: correct the user-facing clause at `docs/reference/mcp.mdx:169`,
`site/src/content/docs/mcp.mdx:171`, and Done-when (6): *a write not yet attempted when the
session closes waits for an embedder; a write the worker reaches during the outage fails,
and its receipt says so.* Declare the asymmetry as a deviation in the design doc (replay arm
keeps `EmbedUnavailable`; in-session arm — state the final decision with its reason), and
name the symmetric J4 seam (re-queue with backoff vs leave unconsumed).

### J3-R2R-3 (P2) — F5's column gap. The preflight diffs tables, not columns; a store
missing one column attaches/acks/applies and loses everything, loud only at close. Preferred:
extend `preflight_schema` to columns from the same DDL source — parse the column list out of
each `CREATE TABLE … ( … )` block plus `ALTER TABLE … ADD COLUMN` lines (skip `--`,
`CONSTRAINT`, `PRIMARY KEY`, `UNIQUE`, `FOREIGN`, `CHECK`), diff against `PRAGMA
table_info` (SQLite) / `information_schema.columns` (Cockroach); reuse
`unprovisioned_store_err`; same shape as `tables_in_ddl` (`src/store/mod.rs:100-112`,
`preflight_schema` `src/store/mod.rs:170-172`). If you judge the full dialect-aware column
parser too much for one round, at minimum STATE THE MAGNITUDE at `src/store/mod.rs:167-169`
and in the Done-when box. Prefer the real column preflight; the Cockroach-dialect half
(`VECTOR`, `CREATE VECTOR INDEX`, `::STRING`) is verify-on-a-live-cluster — implement it
source-correct and note that the live Cockroach verification is a named follow-up (the
cluster MCP may be unreachable; do not block).

### J3-R2R-4 (P2) — taxonomy tests. `src/writeq.rs:849-852` claims "checked by a test now"
for "Eleven" variants but no such test exists; `every_answer_has_a_distinct_tag…`
(`:3463`) has only ten (`:3466-3486`), missing `PendingReplay`; `only_pending_is_unsettled`
(`:3511`) misses `PendingReplay` too, and reverting that arm to `!matches!(self, Pending)`
turns no test red. Fix: add `PendingReplay` to the array, add
`assert!(!ReceiptAnswer::PendingReplay.is_settled())`, rename
`only_the_two_pendings_are_unsettled`; make the docstring's "exhaustive by construction"
claim true with an `fn ordinal(&ReceiptAnswer) -> usize` with no `_` arm asserted to yield
eleven distinct values (a twelfth variant becomes a compile error).

### J3-R2R-5 (P3) — register sweep. Restate on the surviving reason and drop "projected":
`src/writeq.rs:628-630` (`RECEIPT_WAIT_MAX`), `src/writeq.rs:638-641` const-assert message
(`:636`). Fix the three sentences the `PendingReplay` split falsified: `:2573` returns
`PendingReplay` not `Pending`, `:2549-2550`, `:862`. Two register siblings outside
`writeq.rs`: `src/types/mod.rs:574-576` (consumed_at purging is lazy, not unconditional —
correct against `:546-550` + `src/store/sqlite.rs:1761-1770`), and `src/memory.rs:547-548`
(`match_strategy` is not recall-and-merge only; mirror `src/types/mod.rs:207`).

### J3-R2R-6 (P3) — PROBE_TEXT table. `src/writeq.rs:450-465` publishes a measured table
ending "1536 B and up — HTTP 500, 8 of 8"; the rig now refuses between 2048 B and 3072 B.
Re-measure against the live llama BGE-M3 (`127.0.0.1:8080`) and stamp the table with the
server's `-c`/`-b`/`-ub` flags and a date, or drop the numbers and keep the shape of the
argument. Fix `PROBE_TEXT_BYTES`'s justification (`:477-481`) to match.

### J3-R2R-7 (P3) — declaration one surface short. `docs/reference/api.mdx:74` and
`site/src/content/docs/api.mdx:76` still say `match_strategy | Hybrid | Canonical or Hybrid`
recall-only. Add a third bullet at `src/types/mod.rs:207-222` (the third consequence: it
selects the call-time validation rule set — `Canonical` adds `reject_repeated_observation`
and the single-`Hierarchical`-parent rule, `src/memory.rs:1455-1460`, `src/graph/derive.rs:275-277`),
and "all three" in `src/config.rs:132-134`.

### J3-R2R-8 (P3) — `write_queue_replay_blocked`. `replay_owed` is a level; an operator
cannot tell draining from wedged. Add one unconditional stat key
`write_queue_replay_blocked` (the class of the error that ended the last replay, or null),
set in the non-`Embed` arm and in the liveness-gate return (`src/writeq.rs:2983-2999`,
`2925-2941`). Follow the existing `write_queue_replay_owed` plumbing
(`src/mcp/server.rs:1131`).

### J3-R2R-9 (P3) — evidence driver order-dependence.
`evidence/mooshik-j3-durable-intents/j3_n1_outage_demo.py:152` samples
`next(iter(receipts))` (the first write, likeliest drained in-session) then asserts
`pending_replay`. Sample a receipt from the LAST admitted batch (which cannot have drained),
or assert over every receipt partitioned by the store's `outcome_tag`. Keep the committed
driver deterministic across runs.

## Acceptance (do ALL before committing the round as done)

1. Every finding above is closed at source with a comment/commit naming the finding id.
2. The rule table has no `_ =>` arm that produces `Backend`; `unclassified` is conservative
   and logged.
3. Code changes are behavioral and minimal; do not reformat or restyle unrelated code; do
   not fix anything not in scope.
4. Re-gate, exactly as round 2 recorded (`adve-review-mooshik-J3-redesign-round2.md` "Gate
   results"): `cargo test --all --features fixtures`; `cargo test --features
   store-sqlite,embed-fixture,fixtures`; `cargo test --no-default-features --features
   store-cockroach`; `bash scripts/observability/verify.sh`; `cargo fmt --all -- --check`;
   clippy clean on default, `store-sqlite,fixtures`, `ship,fixtures`,
   `--no-default-features store-cockroach,embed-fixture`. Report each as Claimed/Measured.
   Where a gate needs a live embedder, use the running llama BGE-M3 at `127.0.0.1:8080`
   (health `{"status":"ok"}`) and the release binary with `LAMBO_GIT_SHA` set.
5. If the findings included a new test contract, add tests that defend the new observable
   behaviour (e.g. the rule-table classification, the in-session symmetry, the ordinal
   exhaustive claim) and FAIL on the pre-fix code.
6. Record dispositions: update `J3-durability-redesign.md` with an "As built (round 3)" /
   findings table naming each J3-R2R-N and its verdict, and any deviations with argument.
7. Commit to `wt/j3` (conventional style), then `git push origin wt/j3` so the branch
   travels. Report the commit hashes.

## Explicit non-goals

- Do NOT merge into `lambo-for-mooshik` or delete `wt/j3` — integration is a separate,
   operator-gated step after round 3 clears.
- Do NOT run round-3 review yourself; the report below is what the reviewer reads.
- Do NOT modify the dogfood rig, `.env`, or memory server wiring.
- Live-Cockroach verification (R-3's Cockroach dialect, F4 measurement) is a named follow-up,
   not a blocker: implement source-correct, note it, move on.

## Report back (concise, evidence-first)

- Per finding: verdict (CLOSED / PARTIAL + why) and the commit(s) + file:line that close it.
- Gate table (each command, measured result).
- Anything you could not close and exactly what is missing (e.g. live Cockroach).
- Final `git status` clean, `git log --oneline` of the new commits, `git push` confirmation.
