# B1 round-1 remediation brief

Orchestrator: grok-agent. Close every finding in
`dev-diary/adversarial-review/adve-review-mooshik-B-B1-round1.md`.
Work on `b0-pg-extraction` at `/home/nryn/work/lambo`. **Do not commit.**

Do not merge to `lambo-for-mooshik`. Do not reopen B0. Do not split
`init_schema` / `connect_options`. No em dashes. No `.env`. No `models/`.
Do not start Postgres. Do not run live Cockroach.

The dirty tree is the B1 implementation plus an orchestrator CYCLE.md
merge-policy line (B stays on this branch until B0-B4 are all closed).
Keep that line.

## Findings

### B1-R1-1 (P2) pin omitted-database identity
`two_spellings_of_one_database_derive_one_endpoint` never uses a DSN
without `/db`. Deleting the default-db-to-username branch stays green.
Add the omitted-database pair (`postgres://u@host` vs `postgres://u@host/u`)
and, cheaply, one libpq `key=value` spelling vs the URL form. Mutation
M8 must go red. Headline port/password pins must stay red.

### B1-R1-2 (P3) pin exhaustiveness, not only the value
`store_is_shareable(Postgres) = true` is pinned; `_ => true` with Sqlite
special-cased stays green. Fix so a future variant cannot inherit
shareable without a ruling (compile-fail or an exhaustive helper that
names every variant in the test), or drop the "pinned by" sentence and
leave the compiler as the ruling. Prefer a test that goes red on `_ =>`.

### B1-R1-3 (P3) test `provision::run` Postgres arm
`provision.rs` Postgres arm that refuses `scripts/provision.sh` is
untested. Pin `run(..., StoreKind::Postgres)` expecting B2; no real store.

### B1-R1-4 (P3) two em dashes
`src/store/mod.rs` ~1218 panic string; `src/store/pg/mod.rs` ~65 comment.
Replace with colon, comma, or full stop.

### B1-R1-5 (P3) config.mdx shareable list
`docs/reference/config.mdx` and `site/src/content/docs/config.mdx`: a
second process seeing the same session can use sqlite, cockroach, **or
postgres**. Keep the pair in lockstep (`check-mirror-drift.sh` if that
gates this page).

## Gates
CYCLE plus `store-postgres` compile/unit. Mutation-prove R1-1 M8 and
whatever pins R1-2/R1-3, then revert. sqlite.rs untouched.

## Report
`b-run/B1-remediation.md` plus closures appendix on the round-1 review
(do not rewrite Part E or the verdict). agent_id `b1-remediator`.
