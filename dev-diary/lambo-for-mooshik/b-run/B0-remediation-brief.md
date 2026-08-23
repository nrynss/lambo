# B0 round-1 remediation brief

Orchestrator: grok-agent. Close every open finding in
`dev-diary/adversarial-review/adve-review-mooshik-B-B0-round1.md`. Work on
branch `b0-pg-extraction` at `/home/nryn/work/lambo`. **Do not commit.**
Leave the tree dirty for the next review round.

Authority order: spec of record `lambo-hackathon-spec-v0.1.md` →
`B-postgres-store.md` → source → the review file. Do not re-litigate
decisions in B-postgres-store.md. Do not touch the F write-gate /
NULL-only quarantine / width-from-DDL authority. Do not touch `.env` or
`models/`. No em dashes in anything you write.

Review SHAs in the round-1 header were taken before the implementer's
commits. HEAD is now `1368f83` (code `56e6bfc`, docs `1368f83`). Remediate
that tree.

## Findings

### B0-R1-1 (P2) — land the byte-identity proof permanently

The composed-SQL proof was written, run green, then deleted. Mutations
M5 (`STRING_CAST` `::STRING` → `::TEXT`) and M7 (`DISTANCE_OP` `<->` →
`<=>`) survive the entire 994-test offline suite. `DISTANCE_OP` is B3's
"dangerous" row: a wrong pairing mis-ranks silently.

**Fix:** put the proof in `src/store/pg/cockroach.rs` `mod tests` next to
the other SQL-shape tests. It must fail on a single whitespace change to
any composed statement, and on a change to any of `STRING_CAST`,
`VECTOR_CAST`, `DISTANCE_OP`. Compare against the pre-carve literals from
`7937de7` (parser-generated PRE_* constants, not transcribed by hand).
Update pinned counts in `CYCLE.md` to 939 / 598 / 1007 and name the new
test. Mutation-prove M5 and M7 go red, then revert.

### B0-R1-2 (P3) — two rustdoc links the move broke

1. `src/store/pg/cockroach.rs` (~114): `[PgStore::init_schema]` does not
   resolve. Spell `[GraphStore::init_schema]` or drop the link.
2. `src/store/pg/mod.rs` (~940): `[CockroachStore::normalize_tokens]` is
   out of scope. Spell `[cockroach::CockroachStore::normalize_tokens]` or
   `[PgStore::normalize_tokens]` if that is where the method lives.

Add `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures`
to the phase gate list in CYCLE.md (the only gate that sees private-item
doc rot). Confirm the two new warnings are gone.

### B0-R1-3 (P3) — describe over-merged functions, do not split them

Do **not** split `init_schema` or `connect_options` in B0. Change the
write-up: B0-implementation.md §3.4 / §4 must say plainly that those two
functions are known over-merged (executed DDL / session setting that is
not byte-identical for Postgres) and are owed a split at B2/B3, rather
than listing them as five error strings. B2 inherits the debt.

### B0-R1-4 (P3) — B0-N2 has two sites

Add `connect_options` (`pg/mod.rs` ~1234) next to `PgStore::new` (~1201)
on the B0-N2 row. Same string, different path.

### B0-R1-5 (P3) — fixtures row in CYCLE.md

CYCLE.md already added the fixtures baseline row and claims this finding
closed. Verify: the row is present with 1006 listed / 994 passed / 12
ignored, plus the note that fixtures compiles conformance and H2. If the
two listing files were promised and are missing from the scratch path,
note that; do not invent listings. If anything is still missing, finish
it. If already complete, record "closed before this remediation" with the
CYCLE.md citation.

### B0-R1-6 (P3) — 52 is 26

`B0-implementation.md` §2.2: "52 of the 938 entries" → **26** of 938
(26 of 597, 36 of 1006). Do not change the conclusion.

## Gates (re-run yourself; Claimed / Measured)

From CYCLE.md, including the fixtures row:

- `cargo fmt --all -- --check`
- `cargo clippy --all-targets -- -D warnings`
- `cargo clippy --all-targets --features store-cockroach -- -D warnings`
- `cargo test --features store-cockroach` (expect 939 listed after R1-1)
- `cargo test --no-default-features --features store-cockroach` (expect 598)
- `cargo test --features store-cockroach,fixtures` (expect 1007 listed)
- H1 lock: `h1_cross_store_parity` still green; `git diff src/store/sqlite.rs` empty
- `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures`
  (R1-2: the two new warnings gone)

Do not run live `#[ignore]`d Cockroach tests. Do not start a Postgres
container.

## Report

Write `dev-diary/lambo-for-mooshik/b-run/B0-remediation.md`: per-finding
fix with file:line, mutation evidence for R1-1, gate table
Claimed/Measured, and a closures table. Append a Closures section to
`adve-review-mooshik-B-B0-round1.md` marking each finding closed (or
already-closed) with what you did. Do not change the reviewer's verdict
or Part E text.

agent_id if you dogfood: `b0-remediator`.
