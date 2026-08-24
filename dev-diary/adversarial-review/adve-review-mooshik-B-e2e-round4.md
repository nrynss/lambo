# Adversarial review: mooshik B, whole workstream E2E, round 4 (verification of the round-3 remediation)

> Append-as-you-go log, committed after each settled finding. This is the final state.

**Reviewer**: independent E2E round-4 reviewer (Fable), worktree branch
`b0-pg-extraction` at
`/Users/narayan/Documents/work/lambo/.claude/worktrees/agent-a5a65dfa8f847f4f5`, reset
to `origin/lambo-for-mooshik` @ `48bfd11` before starting. The main checkout was never
written. Every probe was a temporary test inside this worktree, run and reverted; the
tree is clean at review end (`git status --porcelain` empty) and the only commits here
are this file's own.

**Scope**: the round-3 closure of **B-E2E-R3-1** (the redactor rebuilt as an allowlist,
commit `4f279f1`) and everything that closure newly introduced. Rounds 1 and 2's
closures were confirmed genuine under mutation by rounds 2 and 3 and are **not**
re-verified here, per `b-run/CYCLE.md`. Where a probe reached pre-round-3 code, it is
labelled as such and its provenance is established against the parent commit.

**Machine and environment.** MacBook. `printenv` shows no `LAMBO_*` and no
`DATABASE_URL`; no `.env` exists in the worktree or the main checkout. No container was
started this round, and the reasoning for that is a finding of its own (the live-Postgres
section below): the change is pure string handling and the live leg cannot observe it.
colima's `default` profile and `docs-telemetry` were never touched.

**Verdict: REQUEST_CHANGES**: **1 P2, 2 P3** (one of the P3s pre-existing and outside
the strict R3 scope, reported for the operator). The round-3 fix is a real improvement
over R2-5's splice and closes every one of the fourteen shapes it pins, byte-for-byte,
under mutation. But its two load-bearing claims — the libpq branch's password gate, and
the collapse concession's "no driver will dial" — do not hold under adversarial probing,
and a third credential leak sits one function away in the parse path the fix strengthened
the promise over.

## ZERO RESIDUE: **NOT REACHED.**

This is the operator's decision line and the reason this round exists. Under
`b-run/CYCLE.md`'s loop rule ("until a round returns APPROVE with zero residue") the
cycle **does not end here**. Two in-scope findings gate: **B-E2E-R4-1** (a secret under a
mistyped or abbreviated password key still prints under "(passwords stripped)") and
**B-E2E-R4-2** (the collapse concession is justified on a factually wrong premise, and a
constructible pair reopens E2E-F2). The workstream does not close on this round's word.

---

## What the remediation did, verified against the tree

R3 replaced R2-5's splice with an allowlist (`src/store/dsn.rs`): `redact_unparseable_dsn`
now tries `redact_url_shaped` then `redact_kv_shaped`, and echoes the result only if
`is_safe_to_echo` passes (non-empty, no control chars, no "password" substring);
otherwise the constant `UNPARSEABLE_DSN` = `<unparseable dsn>` ships. `strip_libpq_password_token`
is deleted. The diff (`git diff fa58c9f 4f279f1 -- src/store/dsn.rs`) touches only the
redactor, the two new constants, the removed helper, and the tests. **`parse_postgres_url`,
`split_authority`, `split_host_port`, `apply_query_overlays` and `to_identity` are not in
any hunk** — the parse path is byte-identical to the parent. The `endpoint.rs` and
`mod.rs` changes are 100% doc-comment text (verified line by line); no executable code
changed outside `dsn.rs`.

The four always-compiled redaction tests pass on the clean tree
(`cargo test --features store-cockroach --lib store::dsn` → 4/0/0).

### The pins are load-bearing (mandated mutation)

I applied the **doc-comment's own copy-pasteable recipe** verbatim — the splice body
written into the doc of `an_unparseable_dsn_still_has_its_password_stripped`, which
replaces the allowlist with R2-5's `find("://")` splice:

* `an_unparseable_dsn_still_has_its_password_stripped` **RED**, and the failure message
  prints the leak verbatim: `app:S3cretHunter@127.0.0.1:26257/lambo -> app:S3cretHunter@127.0.0.1:26257/lambo`.
* `a_recognised_shape_still_shows_the_operator_the_typo` **RED** at the `?password=` case
  (`left: postgres://app@127.0.0.1:70000/lambo?password=S3cretHunter`).
* `an_unrecognised_shape_is_replaced_wholesale` **RED**.
* `a_parseable_dsn_has_its_password_stripped` green (parse path unaffected by the redactor
  mutation).

Three of four red, the secret printed in the failure output, matching the remediation's
M-R3R-1. **The recipe in the doc comment works exactly as written.** Reverted; 4/0 green.

### Regression checks (mandated), all hold

* R2-5's four filed shapes print byte-identically — pinned with `assert_eq!` in
  `a_recognised_shape_still_shows_the_operator_the_typo`, green.
* The last-`@` over-redaction rule is preserved: `postgres://app:S3cretHunter@127.0.0.1:70000/lam@bo`
  → `postgres://app@bo`, pinned and green.
* `strip_libpq_password_token`'s deletion drops no coverage: its naive password-token
  filter is subsumed by `redact_kv_shaped`, which the new tests exercise. (The subsuming
  code has the R4-1 gap below, but that is a new gap, not lost coverage.)

---

## Findings

### B-E2E-R4-1 (P3): the libpq/kv redaction branch is a keyword blacklist, and leaks a secret parked under any key that does not contain the literal substring "password"

*This is the residual the remediation self-reported ("a secret parked under a non-libpq
key (`secret_pw=…`) would be echoed"), which the task directed me to rule on. I rule the
defence unsound.*

*Claim tested*: the round-3 doc calls the fix an allowlist that "builds an echo out of
pieces a recognised shape positively accounts for" and says the single gate `is_safe_to_echo`
is "the property … checkable in one place". The self-reported defence for the residue is
that a string with an unknown key "is not a DSN — libpq rejects unknown connection options
outright, so it would never dial".

*Evidence* (temporary probe, reverted). Every shape below defeats both parsers and reaches
`redact_kv_shaped`, which keeps every whitespace token whose key does not contain the
substring "password", then passes `is_safe_to_echo` (which checks the same substring):

```
host=127.0.0.1 port=70000 user=app passwrod=S3cretHunter  -> ...passwrod=S3cretHunter   LEAK
host=127.0.0.1 port=70000 user=app pwd=S3cretHunter       -> ...pwd=S3cretHunter        LEAK
host=127.0.0.1 port=70000 user=app pass=S3cretHunter      -> ...pass=S3cretHunter       LEAK
host=127.0.0.1 port=70000 user=app secret_pw=S3cretHunter -> ...secret_pw=S3cretHunter  LEAK
host=127.0.0.1 port=70000 user=app password=S3cretHunter  -> host=... user=app          (caught, control)
```

The trigger is the R2-5 typo class itself: `port=70000` makes `parse_libpq_kv` fail
(`v.parse().ok()?` on the bad port), so a libpq string that would otherwise parse and drop
its password is forced onto the unparseable path, where `redact_kv_shaped` echoes the
whole thing including the secret. `passwrod` is a letter-transposition of `password`;
`pwd` and `pass` are abbreviations operators type constantly. None contains the substring
"password", so neither `redact_kv_shaped` nor `is_safe_to_echo` drops them.

*Why the defence does not hold*: the leak occurs in `overlay_env`'s **refusal message**,
which fires precisely because the config is broken (file and env disagree). Whether the
DSN would *dial* is irrelevant on the error path — and R2-5's own filed shapes
(port 70000, 99999, `notaport`) never dial either, yet were graded genuine leaks and
remediated. "Never dials", applied consistently, would have dismissed R2-5 entirely. It
proves too much.

*And it contradicts the fix's own thesis*: R3-1 was filed to retire a blacklist ("a
blacklist has to enumerate every place a secret can hide"). The URL branch honours that.
The kv branch does not: keeping every token whose key lacks "password" **is** a blacklist
over key names, and the single gate the remediation nominated as the guarantee is a
keyword substring check — which the remediation itself concedes is "a keyword check, not
understanding". The allowlist framing holds for one of the two branches only.

*Failure scenario*: identical to R2-5 and R3-1 — a live credential printed to stderr on
the line that says "(passwords stripped)".

*Grade*: **P3**, the same class and observability as R2-5/R3-1 (the operator's own
credential to the operator's own terminal; not published, logged, or hashed-and-leaked).
The compound trigger (bad-port typo **and** a mistyped/abbreviated password key) makes it
somewhat narrower than R2-5's single typo, which is why it is not graded higher.

*What closes it*: make the kv branch a true allowlist — echo only tokens whose key is one
of the recognised libpq identity keywords (`host`/`hostaddr`/`port`/`dbname`/`database`/`user`
and the ssl*/timeout/`application_name` set), dropping every other token including unknown
ones, rather than dropping only keys that spell "password". Extend
`an_unrecognised_shape_is_replaced_wholesale` with `passwrod=`/`pwd=`/`pass=` under a bad
port.

### B-E2E-R4-2 (P2): the collapse concession is ratified on a factually wrong premise ("no driver will dial"), and a constructible pair reopens E2E-F2

*This is the "verify the concession is actually bounded" task item, and the highest-value
attempt of the round.*

*Claim tested*: the round-3 doc and `an_unrecognised_shape_is_replaced_wholesale` accept,
as pinned residue, that two *unrecognised* spellings collapse onto `<unparseable dsn>`.
The justification (doc comment, commit message, and the report's "What this costs"
section) is: "both are strings no driver will dial — anything sqlx can actually connect to
parses through `parse_postgres_url` and never reaches this function — so the observable
consequence is a connection error instead of a disagreement refusal".

*Evidence* (temporary probe under `store-postgres`, comparing `canonical_store_dsn` to
`sqlx::postgres::PgConnectOptions::from_str`, reverted). The premise is false. The
remediation's **own pinned collapse example** is dialable by sqlx:

```
app:one@host-a:26257/db-a  -> canon <unparseable dsn> | sqlx OK host="localhost" db="one@host-a:26257/db-a"
app:two@host-b:26257/db-b  -> canon <unparseable dsn> | sqlx OK host="localhost" db="two@host-b:26257/db-b"
```

sqlx does not reject these; it treats `app` as the scheme, defaults the host to
`localhost`, and takes the rest as the database name. More sqlx-accepts-we-reject
divergences: `postgre://app@h:26257/db` (misspelled scheme, sqlx OK host=h port=26257),
`postgres://h:/db` and `postgres://app@[::1]:/db` (empty port, sqlx OK port=5432). So
"anything sqlx can connect to parses through `parse_postgres_url`" is simply not true.

For the realistic collapse triggers (scheme-less pairs) the *conclusion* survives the
broken premise: sqlx dials `localhost` with a garbage database name that will not exist,
so the outcome is a connection error, not a silent wrong database. But the concession is
less bounded than documented, and a **constructible pair actually reopens E2E-F2**:

```
file store.dsn = postgres://app@host-a:/db_password_a  -> canon <unparseable dsn> | sqlx OK host="host-a" db="db_password_a"
env  ...        = postgres://app@host-b:/db_password_b  -> canon <unparseable dsn> | sqlx OK host="host-b" db="db_password_b"
```

Both collapse to the identical `<unparseable dsn>`, so `overlay_env` finds
`from_file == from_env`, skips the disagreement refusal, and sets `self.dsn = env_dsn` —
silently taking the environment's DSN, which dials a **different real host and database**
than the file named. That is exactly the E2E-F2 P1 the refusal exists to prevent. The
mechanism: the empty-port typo (`host:/`) makes our parser reject (sqlx accepts, defaults
5432), and the substring "password" appearing in the database name forces `is_safe_to_echo`
to fail so the echo collapses to the constant.

*Grade*: **P2**, not P1. The silent-wrong-**real**-database path needs a database whose
name contains the literal substring "password" plus an empty-port typo, which is not a
realistic operator scenario; the realistic collapse triggers degrade safely to a
localhost connection error. It is above P3 because (a) an accepted, ratified residue is
justified on a demonstrably false premise — a reviewer must not let a wrong proof stand as
the basis for accepted risk — and (b) the original P1's consequence is reachable by
construction, not merely argued away. **If the operator weights any constructible E2E-F2
reopening as meeting the P1 bar regardless of trigger realism, this is a P1.** Either way
it gates, and the justification must be corrected rather than carried forward.

*What closes it*: either (i) make the collapse distinguishing where it can be without
re-introducing a leak — e.g. hash-suffix the placeholder, `<unparseable dsn:AB12>`, so two
different unparseable spellings do not compare equal in `overlay_env` while still carrying
no input substring — or (ii) at minimum, correct the doc/report/`CYCLE.md` claim to state
the true bound (sqlx *does* dial several shapes our parser rejects; the safety rests on
those shapes dialling a nonexistent target, and on no identity component containing
"password"), and pin the constructed reopening as a known hole rather than asserting it
cannot happen.

### B-E2E-R4-3 (P3, pre-existing — reported, does not itself set the verdict): `parse_postgres_url` echoes a credential into the canonical identity when the userinfo carries an unencoded `/` or `?`

*Provenance stated first, because it decides how this is weighed*: `parse_postgres_url`,
`split_authority`, and `split_host_port` are **byte-identical to the parent commit** —
R3 did not touch them. This leak therefore predates the round-3 closure (it is in B1 /
R2-5's DSN-identity code) and is **outside the strict scope** of "the round-3 closure and
what it newly introduced". It is reported because the task directed me to derive shapes
with "userinfo containing `/` or `?` or `@`", because it is a live credential leak of the
same class as R2-5, and because it falsifies the *universal* promise the R3-1 fix
strengthened ("safe to print … on **every** input" — module doc, `dsn.rs:12-15`).

*Evidence* (temporary probe, reverted):

```
postgres://ap/p:S3cretHunter@h:70000/db   -> canon postgres://ap:5432/p:S3cretHunter@h:70000/db   FULL secret in the db component
postgres://app:S3cret/Hunter@h/db         -> canon postgres://[app:s3cret]:5432/Hunter@h/db        partial secret (s3cret, Hunter)
postgres://app:S3cret?Hunter@h/db         -> canon postgres://[app:s3cret]:5432/                    partial secret (s3cret)
```

An unencoded `/` in the userinfo makes `split_authority` cut the authority early, so the
password lands in the path (database) or the host component of the identity and is echoed
verbatim under "(passwords stripped)". A `/` in a password is common; the correct spelling
is `%2F`, and a properly percent-encoded password does not leak (`S3cret%2FHunter` →
`postgres://app@h:5432/db`, confirmed). sqlx rejects the slash-in-password form ("invalid
port number"), so it would not dial — but the leak is on the refusal path, where that does
not matter, exactly as in R2-5.

*Grade*: **P3**, same class and observability as R2-5. It does not by itself decide this
round's verdict (already REQUEST_CHANGES on R4-1 and R4-2), but the operator should
schedule the parse path's robustness alongside the redactor, because the "every input"
promise is made about `canonical_store_dsn` as a whole and the parse path is half of it.

*What closes it*: reject (return `None` from `parse_postgres_url`, sending the string to
the now-hardened redactor) a userinfo/authority that contains an unencoded `/` or `?`
before the `@`, rather than silently re-interpreting it as a path boundary. A pin over the
three shapes above.

---

## Gate table: claimed vs measured

Claimed = the round-3 remediation report / post-remediation `CYCLE.md`. Measured = this
worktree at `48bfd11`, clean tree (every probe reverted). Suite numbers are the sum across
each invocation's test binaries, the same arithmetic every prior round used.

| Gate | Claimed | Measured |
|---|---|---|
| `cargo fmt --all -- --check` | pass | **pass** |
| `cargo clippy --all-targets -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-cockroach,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | pass | **pass** |
| `cargo test --features store-cockroach` | 952 / 0 / 4 | **952 / 0 / 4** |
| `cargo test --features store-cockroach,fixtures` | 1012 / 0 / 12 | **1012 / 0 / 12** |
| `cargo test --features store-postgres` | 939 / 0 / 7 | **939 / 0 / 7** |
| `cargo test --features store-postgres,fixtures` | 996 / 0 / 7 | **996 / 0 / 7** |
| `cargo test --features store-sqlite,fixtures` | 1078 / 0 / 3 | **1078 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | 615 / 0 / 0 | **615 / 0 / 0** |
| `cargo doc … --features store-cockroach,fixtures` | 53 | **53** (summary line; naive `^warning` grep = 54, counts the summary itself) |
| `cargo doc … --features store-postgres,store-cockroach,store-sqlite,fixtures` | 53 | **53** |
| live Postgres `-- --ignored` (pinned container) | not re-run | **not re-run — call is sound, see below** |

Zero drift on every offline row: every claimed number reproduced exactly.

### The live-Postgres "not re-run" call is sound; no container was needed

The remediation did not re-run the live leg, on the grounds that "no SQL, no adapter, no
query path" changed. Verified structurally: the only executable change in the whole
commit is `src/store/dsn.rs`'s redactor (`endpoint.rs`/`mod.rs` are doc-comment-only, and
the parse path is byte-identical to the parent). `canonical_store_dsn` is a config-layer
function reached before any pool is constructed; the live tests dial the container through
a clean, parseable `LAMBO_POSTGRES_DSN` that never enters the redaction path. The redactor
cannot affect fencing, schema-width, hnsw, or recall-parity. **I agree with the call and
took no container.** `CYCLE.md` marking the row "not re-run" rather than carrying it
silently is the correct bookkeeping.

---

## Mutations run

| # | Mutation | Expected | Observed | Reverted |
|---|---|---|---|---|
| M-R4-1 | `redact_unparseable_dsn` replaced by the splice recipe written in the doc comment of `an_unparseable_dsn_still_has_its_password_stripped`, verbatim | the three redaction pins red, secret printed | `an_unparseable_dsn_still_has_its_password_stripped`, `a_recognised_shape_still_shows_the_operator_the_typo`, `an_unrecognised_shape_is_replaced_wholesale` all RED; leak `app:S3cretHunter@…` printed in the failure output; `a_parseable_dsn…` green | yes, 4/0 after |

On top of the mandated mutation: four temporary adversarial probes (an independent
23-shape leak sweep; a parse-path slash/question sweep; a sqlx-vs-our-parser divergence
sweep; and a P1-collapse constructor), each a `#[test]` added, run, and removed. They
surfaced R4-1, R4-2, and R4-3 and are the evidence quoted above.

---

## Summary

| Grade | Count | Findings |
|---|---:|---|
| P1 | 0 | (R4-2 is a P2 that becomes P1 only if the operator weights a contrived-trigger E2E-F2 reopening as meeting the bar) |
| P2 | 1 | B-E2E-R4-2 (collapse concession ratified on a false "no driver will dial" premise; constructible E2E-F2 reopening) |
| P3 | 2 | B-E2E-R4-1 (kv branch is a keyword blacklist; mistyped/abbreviated password key leaks); B-E2E-R4-3 (pre-existing parse-path slash leak, out of strict scope) |

**Zero residue: NOT reached.** Verdict **REQUEST_CHANGES**. B-E2E-R4-1 and B-E2E-R4-2 are
in-scope and gate the next pass; B-E2E-R4-3 is a pre-existing leak of the same class,
reported so it is fixed in the same neighbourhood rather than found again later. The
round-3 fix is genuinely better than R2-5's splice and every shape it pins is safe under
mutation — but the two claims it rests on (the kv password gate, the collapse's
undialability) are both falsifiable, and were falsified here.

Stated for the operator plainly: closing R4-1 is a one-branch change (allowlist the kv
keys instead of blacklisting "password"). Closing R4-2 is either a distinguishing
placeholder or an honest correction of the documented bound plus a pin on the constructed
hole. R4-3 is a parse-path guard. None is large; all three are in `src/store/dsn.rs`. On
the standing rule as written, the cycle continues.

Still not verified, carried forward unchanged: the live Cockroach leg (no safe DSN from
this machine), `postgres-live` on a real GitHub runner, and park-and-fail-over (unbuilt by
design, owned by its FUTURE entry).

## Cleanliness

Five temporary probe tests and one code mutation applied, every one reverted;
`git status --porcelain` empty at review end. No container started, no `.env` created, no
`docker` command issued; `docs-telemetry` and colima's `default` profile untouched. The
main checkout at `/Users/narayan/Documents/work/lambo` was never written. The only commits
in this worktree are this review file's own.
