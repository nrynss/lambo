# Workstream B, end-to-end remediation round 5 (2026-08-24) — and the cycle's close

**Against**: `adve-review-mooshik-B-e2e-round5.md`, REQUEST_CHANGES, one finding
(B-E2E-R5-1, P3, classified by the reviewer as a **local fix**).
**Result**: closed. All gates green, zero drift.
**The cycle ends here**, by operator ruling — see "Why this round closes the cycle".

## Per-finding closure

| Finding | Closed | What changed | Reverting it breaks |
| --- | --- | --- | --- |
| **B-E2E-R5-1** (P3) query branch was a blacklist while its sibling was an allowlist | yes | `redact_url_shaped`'s query test is now `query_is_all_echoable`, which shares `is_echoable_libpq_key` with `redact_kv_shaped` | `an_unparseable_dsn_still_has_its_password_stripped` (7 new shapes) |

### What the defect was

Round 4 rebuilt `redact_kv_shaped` as a positive key allowlist to close B-E2E-R4-1,
and left the sibling URL-query branch on the `mentions_password` **blacklist** it had
always used. The two branches redact the same secrets reached by two spellings, so
the hole survived in the one nobody was looking at: a credential under `?pwd=`,
`?pass=`, `?passwrod=`, `?secret=` or `?passfile=` printed verbatim under
"(passwords stripped)". The reviewer confirmed it end to end through the release
binary.

The trigger is R2-5's own typo class and worth restating, because it is what makes a
query-parameter secret reach this code at all: `:70000` is not a port, so
`parse_postgres_url` fails, so the string goes down the unparseable path where the
echo is built by redaction rather than by reconstruction from parsed fields.

### The fix, and one decision inside it

`query_is_all_echoable` splits on `&` and `;`, takes each parameter's key, and
requires **every** one to be on `is_echoable_libpq_key`'s list.

Two things chosen deliberately:

* **It shares the libpq list rather than growing a second one.** A Postgres URL's
  query parameters *are* libpq keywords — that is what the `?key=value` form means.
  Two lists could disagree, and a key admitted on one path but not the other is
  exactly the shape of the defect being closed. One list, two spellings; adding a
  key admits it on both at once.
* **Whole-query redaction, not per-parameter.** `?sslmode=require&<query redacted>`
  would be more informative and more dangerous: a query is one field to a reader,
  and showing part of it invites the belief that what is shown is all there was.
  One of the seven new shapes (`?sslmode=require&pwd=…`) exists to pin this — it is
  the case per-parameter redaction would have handled prettily and wrongly.

`is_safe_to_echo`'s `mentions_password` clause is left in place. It is now a
backstop rather than the protection: both branches build their echo from an
allowlist, so nothing should reach it carrying a secret. It costs nothing and it
fails closed.

## Tests

`UNPARSEABLE_SHAPES` gained **seven** rows: the five the reviewer filed
(`?pwd=`, `?pass=`, `?passwrod=`, `?secret=`, `?passfile=`), plus two derived here —
a disallowed key hidden behind an allowed one (`?sslmode=require&pwd=…`), and the
semicolon separator the URL form also accepts. No new test *functions*, so every
suite count is unchanged; the coverage is in the table the existing test iterates.

**Mutation** (applied, observed, reverted): restoring the `mentions_password(query)`
blacklist — the exact pre-fix line — turns
`an_unparseable_dsn_still_has_its_password_stripped` RED at `dsn.rs:694`, with the
leak printed in the failure output:

```
the echo carries "s3crethunter" of the password:
postgres://app@127.0.0.1:70000/lambo?pwd=S3cretHunter
  -> postgres://app@127.0.0.1:70000/lambo?pwd=S3cretHunter
```

Reverted; 10/0 green.

## Gates, measured on the finished tree

| Gate | Baseline (round 4) | Measured |
| --- | --- | --- |
| `cargo fmt --all -- --check` | pass | **pass** |
| clippy default / `store-cockroach,fixtures` / `store-postgres` / `store-postgres,fixtures` / `store-sqlite,fixtures` / `ship,fixtures`, all `-D warnings` | pass | **all six pass** |
| `store-cockroach` | 959 | **959 / 0 failed** |
| `store-cockroach,fixtures` | 1019 | **1019 / 0** |
| `store-postgres` | 946 | **946 / 0** |
| `store-postgres,fixtures` | 1003 | **1003 / 0** |
| `store-sqlite,fixtures` | 1084 | **1084 / 0** |
| `--no-default-features --features store-cockroach` | 622 | **622 / 0** |
| doc gate | 53 | **53** (a naive grep counts 54 — the summary line, not drift) |

Zero drift. Counts are unchanged by design: the fix adds table rows, not test
functions.

**Live Postgres: not run.** The diff touches `src/store/dsn.rs` only — no SQL, no
migration, nothing under `src/store/pg/`. This is the same condition rounds 3 and 4
were judged sound on, and round 5 verified that judgement against the diff.

## Why this round closes the cycle

`CYCLE.md`'s rule is that the loop ends when a round returns APPROVE with zero
residue. Round 5 returned REQUEST_CHANGES, so on the letter of the rule the cycle
would continue.

**Operator ruling, 2026-08-24:** if the next round produced only a non-serious P3
with no design implication, close it, commit, and end the cycle. Round 5 met that
test exactly, and said so in the terms the ruling asked for: one finding, P3,
explicitly classified a **local fix** — and its closure here changed one predicate
and added seven table rows, which is what a local fix looks like.

Round 5 also ruled the remaining items **acceptable bounded residue rather than
findings**, and that ruling stands as the workstream's floor:

* **The username position is echoed on every path**, by design. Narrowing it is a
  *design implication*: it would change the documented "username is kept" contract,
  and `user@server` — the spelling managed Postgres requires — cannot be told from a
  mistyped password by looking at the string.
* **`?host=` / `?dbname=` / `?user=` overlays** are identity components the module
  must echo for the identity to mean anything.
* **The 128-bit SHA-256 identity floor** and the **deliberate over-split price**
  (two malformed spellings differing only by a password are two identities, so
  `overlay_env` refuses where it once overlaid — loud and one edit to fix, rather
  than silent and unfixable).

Carried forward unchanged, and **not** closed by this cycle: the live Cockroach leg
(no safe DSN on the machines used), `postgres-live` never exercised on a real GitHub
runner, and park-and-fail-over, which has an owner in
[FUTURE.md](../FUTURE.md) and remains unimplemented.

## The cycle, end to end

| Round | Verdict | Findings |
| --- | --- | --- |
| 1 | REQUEST_CHANGES | 2 P1 / 2 P2 / 7 P3, plus E2E-1 and E2E-2 |
| 2 | REQUEST_CHANGES | 1 P2 / 5 P3 — all ten round-1 closures verified genuine |
| 3 | REQUEST_CHANGES | 1 P3 |
| 4 | REQUEST_CHANGES | 1 P2 (ruled P1 by the operator) / 2 P3 |
| 5 | REQUEST_CHANGES | 1 P3, local fix — **closed here** |

Findings per round: 11 → 6 → 1 → 3 → 1. The two that mattered both concerned one
thing — that a DSN is simultaneously a secret to be hidden and an identity to be
compared — and the cycle only stopped rediscovering it when round 4 stopped trying
to make one function do both.
