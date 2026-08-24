# Adversarial review: mooshik B, whole workstream E2E, round 5 (verification of the round-4 remediation)

> Append-as-you-go log, committed after each settled finding. This is the final state.

**Reviewer**: independent E2E round-5 reviewer (Fable), worktree branch
`b-e2e-review-round5` at
`/Users/narayan/Documents/work/lambo/.claude/worktrees/agent-a1a46bc27a2a1135f`, created
fresh at `origin/lambo-for-mooshik` @ `5527d12` before anything ran. No existing branch
pointer was moved — a previous reviewer checked out `b0-pg-extraction` and moved it; this
round did not. The main checkout was never written. Every probe was a temporary test or a
throwaway config inside this worktree, run and reverted; the tree is clean at review end
(`git status --porcelain` empty) and the only commits here are this file's own. All gate
logs live under `target/review-gates/` (git-ignored) and are not committed.

**Scope**: round 4's three closures (**R4-1**, **R4-2**, **R4-3**) and anything round 4
newly introduced (the echo/identity split, `unaccounted_identity`, `is_echoable_libpq_key`,
`withheld_note`, the two `parse_postgres_url` guards). Rounds 1–3 were confirmed under
mutation by later rounds and are **not** re-verified, per `b-run/CYCLE.md`.

**Machine and environment.** MacBook. `printenv` shows no `LAMBO_*` and no `DATABASE_URL`;
no `.env` exists in the worktree or the main checkout. `LAMBO_POSTGRES_DSN` and
`LAMBO_CONFIG` appear only inline on the CLI probe invocations below, scoped to those
commands. No container was started and no `docker` command was issued this round — the
live-Postgres skip is verified sound below, so none was needed. colima's `default` profile
and the `docs-telemetry` container on it were never touched.

**Verdict: REQUEST_CHANGES**: **1 P3, in scope.** The round-4 split is correct and holds
its invariant under mutation; R4-2 and R4-3 are genuinely and completely closed. But R4-1
is closed in only one of the two redaction branches. Round 4 rebuilt the libpq
`key=value` branch into a positive key allowlist and declared the keyword-substring
blacklist retired — but the sibling **URL-query** branch still decides redaction by the
same `mentions_password` substring test, so a credential parked under a misspelled or
abbreviated *query* password parameter (`?pwd=`, `?pass=`, `?passwrod=`, `?secret=`,
`?passfile=`) prints verbatim under "(passwords stripped)". This is R4-1's exact defect,
one branch over, on the identical trigger class round 4 spent a whole round closing.

## ZERO RESIDUE: **NOT REACHED.**

This is the operator's decision line. Under `b-run/CYCLE.md`'s loop rule ("until a round
returns APPROVE with zero residue") the cycle **does not end here.** One in-scope finding
gates: **B-E2E-R5-1** (P3) — the URL-query redaction is still a keyword blacklist, so R4-1
is not closed as a class. B does **not** close on this round's word.

**But it is one branch from closeable, and I say so plainly.** Every other claim round 4
made is true under adversarial probing and mutation. The remaining non-R5-1 residues
(username / `?user=` / `?host=` / `?dbname=` echoed verbatim; the SHA-256 128-bit
identity floor; the deliberate over-split price) are the **acceptable irreducible floor**
and I rule on them explicitly below — they are *not* findings. R5-1 is a **local fix** (see
its classification line): the same one-branch, one-edit change R4-1 was. Close it the way
round 4 closed the kv branch (allowlist the query keys instead of blacklisting the word
"password") and a round-6 review should be able to return APPROVE with zero residue.

---

## What the remediation did, verified against the tree

Round 4 split `canonical_store_dsn` into two functions (commit `c567054`):

* `store_dsn_identity` (`src/store/dsn.rs:89`) — compared by `overlay_env`, hashed by
  `store_identity`, **never printed**. A shape neither parser accounts for gets a
  SHA-256 digest of the trimmed input (`unaccounted_identity`, `dsn.rs:176`), truncated to
  128 bits, prefixed `<unparseable dsn#…>`.
* `store_dsn_echo` (`dsn.rs:109`) — the human-facing quote. A parsed identity if it is
  terminal-safe, else the redactor's echo, else the constant `<unparseable dsn>`.

`overlay_env` (`src/store/mod.rs:966`) now compares identities (`id_file != id_env`) and
prints echoes; `withheld_note` (`dsn.rs:203`) appends one sentence when either quote is the
placeholder. The kv redactor became a positive allowlist (`is_echoable_libpq_key`,
`dsn.rs:394`); two guards were added to `parse_postgres_url` (`path.contains(':')` at
`dsn.rs:487`, and `split_host_port`'s `Some(_) => None` at `dsn.rs:548`).

The diff (`git diff c927ebc c567054 --name-only`) touches exactly `src/store/dsn.rs`,
`src/store/mod.rs`, `src/mcp/endpoint.rs` — **nothing under `src/store/pg/`, no `.sql`, no
migration** (verified: the `grep` returns NONE). The `endpoint.rs` change is one call-site
rename (`canonical_store_dsn` → `store_dsn_identity`) plus doc; the `mod.rs` change is the
`overlay_env` identity/echo split plus doc.

`store::pg` reaches the network through `dsn.parse::<PgConnectOptions>()`
(`connect_options`, `src/store/pg/mod.rs:1358`), so `sqlx::postgres::PgConnectOptions::from_str`
is genuinely the dial path round 4's measurement rests on — confirmed by reading
`sqlx-postgres-0.8.6/src/options/parse.rs`: `from_str` hands the string to the `url` crate
and validates no scheme at all.

---

## The split holds its invariant (pressed hardest, per the task)

I attacked the two halves independently with my own shapes (not round 4's nineteen):
percent-encoded credentials, IPv6 expansion, `?host=`/`?dbname=`/`?user=`/`?database=`
overlays, nested connection URIs, trailing slashes, and shapes where this module's parsing
diverges from sqlx's `url`-crate parsing. Temporary `#[test]` probes, run and reverted.

**Identity never collapses two databases that dial differently** — confirmed on the
constructed pairs round 4 pins, and on five *new* divergence pairs I built where this
module's canonicalisation makes two spellings equal that sqlx dials to different targets:

| shape A / shape B | this module's identity | sqlx dials |
|---|---|---|
| `…/dbA?database=X` / `…/X` | both `…/X` | db `dbA` vs db `X` |
| `…/dbA?database=X` / `…/dbB?database=X` | both `…/X` | db `dbA` vs db `dbB` |
| `…/db/` / `…/db` | both `…/db` | db `db/` vs db `db` |
| `…@hA/db` / `…@h%41/db` | both `…@ha…` | host `hA` vs host `h%41` |
| `a%2Fb@h/db` / `a%2Fb%40h/db` | both user `a/b`, host `h` | user `a/b` vs user (default), host `a%2Fb%40h` |

These are **safe** and not a finding: they are the module's documented normalisation
(`?database=`/`dbname=` overlay the authority per the sqlx rule, trailing `/` trimmed, host
percent-decoded then lowercased). The *consequence* of collapsing here is that
`overlay_env` treats two spellings as one database and lets the environment's win — but the
collapse only fires when both spellings share the module's *own* canonical form, i.e. the
operator wrote two spellings the module considers equal. sqlx's divergent reading is a
connection-target question, not an E2E-F2 silent-swap: `overlay_env` is comparing two
spellings the operator supplied, and if the operator wrote `?database=X` on one side and
`/X` on the other, they *are* naming database X either way as far as this module's contract
goes. None of the five produces a "file says one real DB, env silently takes a different
real DB" where the two are the *documented* credential-overlay shape. I record them so the
next round need not re-derive them; they do not gate.

**The echo never contains a secret** — this is where R5-1 lives (below). On the *parsed*
path the echo is four fields none of which is the password, plus the control-character
clause (`is_terminal_safe`), verified. On the *unparseable* path the kv branch is a sound
allowlist. The **URL branch is not**, and that is the finding.

---

## Findings

### B-E2E-R5-1 (P3, in scope): R4-1 is closed in the kv branch only — the URL-query redactor is still a `mentions_password` blacklist, and leaks a secret under any query password parameter that does not literally spell "password"

**Closing it is a LOCAL FIX.** It is a bounded edit inside `redact_url_shaped`'s existing
query-handling arm plus the shared gate — allowlist the query keys the way
`redact_kv_shaped`'s keys are already allowlisted, or drop the query wholesale on the
unparseable URL path. No interface changes, no contract changes, no invariant changes, and
no earlier decision reopened: it is the *same* class of edit as R4-1 and R4-3, both of
which round 4 (and the operator) treated as local fixes. The two-function split R4-2 forced
is untouched by it. There is no ambiguity here — the module already contains the exact
mechanism the fix needs (`is_echoable_libpq_key`); R5-1 is applying it to the branch that
was skipped.

*Claim tested*: round 4's per-finding closure of R4-1 states the redactor "becomes a
positive key set now (`is_echoable_libpq_key`); everything else is dropped, unknown keys
included", and the module doc (`dsn.rs:16-17`) calls the whole `redact_unparseable_dsn`
family "rebuilt as an allowlist … a shape this module cannot account for is not echoed at
all". Round 4's R4-1 root cause was: *the redactor decides what to drop by testing the
substring "password", so a credential under `pwd=`/`pass=`/`passwrod=`/`secret_pw=` sails
through.* Round 4 fixed that in `redact_kv_shaped`. It did **not** fix it in
`redact_url_shaped`, which is the branch a URL-shaped unparseable DSN takes.

*Evidence* (`src/store/dsn.rs`, temporary probe + live binary, both reverted):

`redact_url_shaped` (`dsn.rs:326`) echoes `scheme://user + tail`, and redacts the query
**only** when it mentions the substring "password":

```
dsn.rs:339   let tail = match tail.split_once('?') {
dsn.rs:340       Some((before, query)) if mentions_password(query) => { …REDACTED_QUERY }
dsn.rs:343       _ => tail.to_string(),      // <-- query echoed verbatim otherwise
```

The final gate is no backstop, because it is the *same* blacklist:

```
dsn.rs:304   fn is_safe_to_echo(echo) -> bool { is_terminal_safe(echo) && !mentions_password(echo) }
```

So a query key that carries a credential but does not contain the literal substring
"password" escapes both the query redaction and the gate. Every shape below defeats
`parse_postgres_url` (port past u16), is not kv-shaped (`contains("://")`), reaches
`redact_url_shaped`, and prints the secret:

```
postgres://app@127.0.0.1:70000/lambo?passwrod=S3cretHunter -> ...?passwrod=S3cretHunter   LEAK
postgres://app@127.0.0.1:70000/lambo?pwd=S3cretHunter      -> ...?pwd=S3cretHunter        LEAK
postgres://app@127.0.0.1:70000/lambo?pass=S3cretHunter     -> ...?pass=S3cretHunter       LEAK
postgres://app@127.0.0.1:70000/lambo?secret=S3cretHunter   -> ...?secret=S3cretHunter     LEAK
postgres://app@127.0.0.1:70000/lambo?passfile=%2Fhome%2FS3cretHunter -> ...?passfile=...  LEAK
postgres://app@127.0.0.1:70000/lambo?password=S3cretHunter -> ...?<query redacted>        (caught, control)
```

Reproduced **end to end through the release binary** (debug build,
`LAMBO_CONFIG` → a probe `lambo.toml` carrying the shape as `store.dsn`,
`LAMBO_POSTGRES_DSN` inline pointing elsewhere, `lambo stats --session probe`, rc 1):

```
store.dsn and LAMBO_POSTGRES_DSN name different databases: the config file says
postgres://app@127.0.0.1:70000/lambo?pwd=S3cretHunter and LAMBO_POSTGRES_DSN says
postgres://lambo@127.0.0.1:55434/livetest (passwords stripped). Refusing to guess…
```

The live credential `S3cretHunter` is printed on the refusal line that ends "(passwords
stripped)".

*Why this is R4-1, not a new class*: R4-1's own filing was "a keyword blacklist wearing an
allowlist's name — `passwrod`, `pwd`, `pass`, `secret_pw` all print". `pwd`/`pass`/`passwrod`
are the identical keys, on the identical trigger (a bad-port typo forcing the unparseable
path). Round 4's fix was branch-local: it allowlisted keys in `redact_kv_shaped` but left
`redact_url_shaped`'s query decision and the shared final gate as substring blacklists.
The real libpq/URI password parameter is spelled `password` (caught) — so, exactly as in
R4-1, the leak is the *misspelled or abbreviated* form, which is precisely the case round 4
accepted as a genuine leak worth a round.

*A second shape of the same root* (value not inspected): the kv allowlist gates on the
*key* but echoes the whole *value*, and libpq expands a `dbname` value that is itself a
connection URI. `host=127.0.0.1 port=70000 user=app dbname=postgres://app:S3cretHunter@h/db`
→ echoed verbatim, secret intact. Narrower than the query leak (needs libpq's recursive
`dbname` expansion, obscure), reported as corroboration rather than graded separately. Its
close is also a **local fix** (redact an allowlisted value that itself looks like a
connection string), same character as the primary.

*Failure scenario*: identical to R4-1/R2-5 — a live credential printed to the operator's
own stderr on the line that promises it was stripped. Not published, not logged, not
hashed-and-leaked.

*Grade*: **P3**, the same class and observability round 4 assigned R4-1, and it gates for
the same reason R4-1 gated: an in-scope credential leak on the refusal path. It is not
higher because the exposure is the operator's own secret to the operator's own terminal,
and the trigger is compound (a parse-defeating typo **and** a misspelled query password
key). Grade and classification are independent: this is a P3 *and* a local fix.

*What closes it*: give `redact_url_shaped`'s query the same treatment `redact_kv_shaped`
got — echo only query keys on a positive identity list (`host`/`port`/`dbname`/`user`/`ssl*`
/`connect_timeout`/`application_name`), dropping every other key including unknown ones,
rather than replacing the query only when it says "password". Equivalently, drop the query
wholesale on the unparseable URL path (it is never load-bearing for a typo diagnostic the
way host/port are). Extend `a_recognised_shape_still_shows_the_operator_the_typo` and
`an_unrecognised_shape_is_replaced_wholesale` with `?pwd=`/`?pass=`/`?passwrod=`/`?secret=`
under a bad port. The final `is_safe_to_echo` gate should stay, but it cannot be the only
defence — a backstop that shares the blacklist it backs up is not a backstop.

---

## Rulings the task demanded

### The stated residue (username / `?user=` / `?host=` / `?dbname=` echoed): **acceptable bounded residue, NOT a finding.**

Round 4 declares NOT closed, by design, that a secret an operator places into the
*username* position, or into a `?user=`/`?host=`/`?dbname=` query overlay, is echoed —
because those are the four identity components (user, host, port, database) the module is
*documented and required* to preserve, and narrowing the username would break the
`user@server` spelling managed Postgres (Azure and others) requires.

I rule this the **acceptable irreducible floor**, and the reasoning is a principled line,
not a shrug. There are two distinct situations:

1. A secret placed by the operator into a **structural identity field** the module must
   echo to do its job (username, host, dbname, port). The module cannot tell a
   username-that-is-secret from a real username; it is contractually required to keep it;
   and both this module *and sqlx* read it as that field. Echoing it is not a redaction
   failure — it is the operator mislabelling their own credential as structure. I probed
   the seventh-shape space here (`postgres://app/S3cretHunter@h/db`,
   `postgres://app/S3cretHunter@h:70000/db`, the `:`-typed-as-`/` class): every leak I
   could construct lands the secret in the **username or database** component that both
   the module and sqlx agree is that field — i.e. it falls squarely inside this declared
   residue, and unlike R4-3 the *driver does not reject it either* (sqlx dials it as
   host+db), so there is no divergence to close. These are the floor, not new findings.

2. A secret placed under a param the operator **named as a password** (`?pwd=`, `?pass=`),
   which the redactor should drop but does not. That is *not* a structural field, the
   operator did *not* mislabel it as structure, and round 4 already accepted (in the kv
   branch) that dropping it is required. That is **R5-1**, and it is a finding.

The line is: *identity components are echoed by contract (floor); password-intent params
must be dropped (finding if they are not).* The username residue is the former; R5-1 is the
latter. Zero residue is reachable precisely because the floor is (1) and the only thing
standing between here and it is (2), which is one edit.

**If instead one wanted to narrow the username residue** (i.e. treat it as a finding and
close it): that would be a **DESIGN IMPLICATION**, not a local fix. The module's `# The
rule` doc records "Username is kept" as a deliberate identity choice (two roles on one
cluster can be two deployments), and the `user@server` spelling that managed Postgres
requires means any narrowing must distinguish "username that legitimately contains `@`/`:`"
from "password mistyped into the username" — a judgement the module cannot make from the
string alone, so closing it would change the identity contract and reopen a settled
trade-off. That structural cost is exactly why I rule it acceptable residue rather than a
finding: the fix is not bounded, and the current behaviour is the documented, required one.
It leans clearly to design-implication, and that is the reason it stays residue.

### R4-2's reopening, reconstructed and confirmed refused.

`M-R5-1` (below) restores round 3's collapse (`unaccounted_identity` returns the constant).
The end-to-end test `store::tests::two_unquotable_dsns_still_reach_the_disagreement_refusal`
**fails for the right reason**: its `expect_err` panics because the *real* `overlay_env`
returned `Ok` and took the environment's DSN — E2E-F2 reproduced through the production code
path, not a stubbed comparison. With the digest in place the same config refuses, quoting
`<unparseable dsn>` on both sides with the withheld note. R4-2 is genuinely closed.

### The price (two malformed spellings differing only by a password now refuse): bounded to malformed input, costs nothing on the parsed path.

Verified: `store_dsn_identity("postgres://app@h:26257/db") == store_dsn_identity("postgres://app:S3cretHunter@h:26257/db")` holds (parsed path — the documented credential-overlay pattern), while the `postgre://…` (misspelled-scheme, unparseable) pair differs. The env-only CI path (no `store.dsn` in the file) and
`env_dsn_that_only_adds_credentials_overlays_the_file_dsn` are both green in the gate run.
The price is real but paid only by a DSN this module cannot parse *and* that the operator
relied on for credential overlay — a loud refusal, one edit to fix. Bounded as claimed.

### The libpq key allowlist (`is_echoable_libpq_key`): exclusions complete for the leak that matters.

The positive set is host/hostaddr/port/dbname/database/user/sslmode/sslrootcert/sslcert/
sslkey/connect_timeout/application_name/fallback_application_name/target_session_attrs/
client_encoding. `password`, `sslpassword`, `passfile`, `options` are excluded on purpose.
Against libpq's parameter list, the security question is only "is any *secret-bearing* key
wrongly *included*" — and none is: the three secret-ish keys (`password`, `sslpassword`,
`passfile`) and the free-form `options` are all out. `sslkey` (a *path* to a key file, not
the key) is the closest call and is defensibly in, as round 4 noted. A key omitted from the
set merely loses a diagnostic, which is the safe direction. The kv allowlist is sound —
which is exactly why R5-1 (the *URL* branch, which has no such allowlist) is the gap.

### The sixth R4-3-class shape (M-R4R-6) is closed; the seventh search found only floor.

`postgres://app:pa://S3cretHunter@…` is now row `dsn.rs:709` of `UNPARSEABLE_SHAPES` and is
caught (`an_unparseable_dsn_still_has_its_password_stripped` green). My hunt for a seventh
`:`-typed-as-`/` shape surfaced `postgres://app/S3cretHunter@h/db` (parses; secret lands in
the *database* component, which both this module and sqlx read as a db name) and its
bad-port twin (unparseable; secret lands in the *username* position) — both fall inside the
declared username/dbname structural residue ruled acceptable above, and unlike R4-3 the
driver does not reject them, so there is nothing to close. No seventh *leak-class* shape.

---

## Gate table: claimed vs measured

Claimed = the round-4 remediation report / `CYCLE.md`. Measured = this worktree at
`5527d12`, clean tree, every probe reverted. Suite numbers are the sum across each
invocation's test binaries, the same arithmetic every prior round used.

| Gate | Claimed | Measured |
|---|---|---|
| `cargo fmt --all -- --check` | pass | **pass** |
| `cargo clippy --all-targets -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-cockroach,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --no-default-features --features store-cockroach -- -D warnings` | pass | **pass** |
| `cargo test --features store-cockroach` | 959 / 0 / 4 | **959 / 0 / 4** |
| `cargo test --features store-cockroach,fixtures` | 1019 / 0 / 12 | **1019 / 0 / 12** |
| `cargo test --features store-postgres` | 946 / 0 / 7 | **946 / 0 / 7** |
| `cargo test --features store-postgres,fixtures` | 1003 / 0 / 7 | **1003 / 0 / 7** |
| `cargo test --features store-sqlite,fixtures` | 1084 / 0 / 3 | **1084 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | 622 / 0 / 0 | **622 / 0 / 0** |
| `cargo doc … --features store-cockroach,fixtures` | 53 | **53** (summary line; naive `^warning` grep = 54, counts the summary itself) |
| `cargo doc … --features store-postgres,store-cockroach,store-sqlite,fixtures` | 53 | **53** |
| live Postgres `-- --ignored` (pinned container) | not re-run | **not re-run — call verified sound, see below** |

Zero drift on every offline row: every claimed number reproduced exactly, including the
naive-grep 54 the report warns about.

### The non-uniform delta (+7 everywhere, +6 for sqlite+fixtures) is verified, not accepted on faith

Round 4's explanation is that `sqlx_dials_what_this_module_cannot_parse` is
`#[cfg(any(store-postgres, store-cockroach))]` and so does not compile under
`store-sqlite,fixtures`, making that suite move by one less. Verified directly:

* `grep -c sqlx_dials_what_this_module_cannot_parse` = **1** in the postgres, cockroach and
  no-default logs; **0** in the sqlite+fixtures log. The measurement test is present exactly
  where the driver is and absent where it is not.
* The other six always-compiled new tests
  (`two_unquotable_dsns_still_reach_the_disagreement_refusal`,
  `two_unparseable_spellings_are_two_identities`,
  `a_libpq_key_is_echoed_only_if_it_is_on_the_list`,
  `an_unencoded_slash_in_a_password_does_not_reach_the_identity`,
  `the_echo_and_the_identity_do_not_borrow_each_others_answers`,
  `a_parsed_echo_still_may_not_carry_a_control_character`) are all present in the
  sqlite+fixtures log (`grep -c` = **6**). So sqlite+fixtures gains 6 and the driver-bearing
  suites gain 7. The explanation holds.

### The live-Postgres "not re-run" call is sound; no container was needed

`git diff c927ebc c567054 --name-only` = `src/mcp/endpoint.rs`, `src/store/dsn.rs`,
`src/store/mod.rs`; the pg-adapter/SQL grep returns **NONE**. The only executable changes
are the config-layer `dsn.rs` redactor/identity split, the `overlay_env` comparison in
`mod.rs`, and one call-site rename in `endpoint.rs`. `store_dsn_identity` and
`store_dsn_echo` are both reached before any pool is constructed, and the live tests dial
through a clean, parseable `LAMBO_POSTGRES_DSN` that never enters redaction. The change
cannot affect fencing, schema width, hnsw, or recall parity. **I agree with the call and
took no container.** The 7 `-- --ignored` tests carry the round-3 review's numbers, as
`CYCLE.md` records.

---

## Mutations run

| # | Mutation | Expected | Observed | Reverted |
|---|---|---|---|---|
| **M-R5-1** | `unaccounted_identity` returns `UNPARSEABLE_DSN` (round 3's collapse) | R4-2 pins red, E2E refusal red | `an_unparseable_dsn_still_has_its_password_stripped`, `two_unparseable_spellings_are_two_identities`, `the_echo_and_the_identity_do_not_borrow_each_others_answers`, `sqlx_dials_what_this_module_cannot_parse` RED (4 in `store::dsn`); and `store::tests::two_unquotable_dsns_still_reach_the_disagreement_refusal` RED — panicked on `expect_err` because the **real `overlay_env` returned `Ok`** and took the env DSN. R4-2 reproduced end to end. | yes, green after |
| **M-R5-2** | `is_echoable_libpq_key` → `!mentions_password(key)` (round 3's kv blacklist) | R4-1 kv pins red, leak printed | `a_libpq_key_is_echoed_only_if_it_is_on_the_list` and `an_unparseable_dsn_still_has_its_password_stripped` RED; failure prints `… passwrod=S3cretHunter -> host=… passwrod=S3cretHunter`. R4-1's kv closure is load-bearing. | yes, green after |
| **M-R5-3** | drop `path.contains(':')` guard in `parse_postgres_url` | R4-3 pins red | `an_unencoded_slash_in_a_password_does_not_reach_the_identity` and `an_unparseable_dsn_still_has_its_password_stripped` RED (`…must not parse: postgres://ap/p:S3cretHunter@h:70000/db`). R4-3's guard is load-bearing. | yes, green after |

Three mutations, nine distinct red outcomes; the E2E `overlay_env` red under M-R5-1 is the
one that matters, because it proves the refusal fires through production code and not only
in a unit assertion.

On top of the mandated mutations: six adversarial probe tests (a URL-query keyword sweep, a
kv-value nested-DSN sweep, a fragment/tail sweep, an identity-collapse-vs-sqlx sweep, an
over-split sanity sweep, and a `:`-as-`/` separator-typo sweep), each a temporary `#[test]`
added, run, and removed. They surfaced R5-1 and confirmed the declared residues are the
floor. Two live CLI probes against the debug binary (R5-1's leak, and a `?database=`
identity-collapse that slips the refusal and proceeds straight to a pool timeout) reproduce
R5-1 and the collapse behaviour through the real command surface.

---

## Summary

| Grade | Count | Findings | Close is |
|---|---:|---|---|
| P1 | 0 | — | — |
| P2 | 0 | — | — |
| P3 | 1 | **B-E2E-R5-1** (in scope): R4-1 closed in the kv branch only; the URL-query redactor is still a `mentions_password` blacklist and leaks `?pwd=`/`?pass=`/`?passwrod=`/`?secret=`/`?passfile=` | **Local fix** |

**Per prior findings:**

* **R4-1** — closed in `redact_kv_shaped` (verified genuine under M-R5-2), **but not as a
  class**: the identical substring-blacklist defect stands in `redact_url_shaped`'s query
  handling and the shared `is_safe_to_echo` gate. Reopened as **R5-1** (local fix).
* **R4-2** — **closed.** The echo/identity split is correct; the digest does not collapse;
  the E2E refusal fires through the real `overlay_env` (M-R5-1 reproduces the round-3
  reopening for the right reason). The over-split price is bounded to malformed input and
  costs nothing on the parsed path. (This was the design-implication finding; it stays
  closed.)
* **R4-3** — **closed.** Both `parse_postgres_url` guards are load-bearing (M-R5-3); the
  sixth shape (M-R4R-6) is pinned and caught; the seventh-shape search found only the
  declared structural residue, not a new leak.

**Zero residue: NOT reached.** Verdict **REQUEST_CHANGES** on **B-E2E-R5-1** (P3, in scope,
**local fix**). The round-4 split is a genuine, correct piece of work and R4-2/R4-3 are
completely closed — but R4-1 was fixed in one of two redaction branches, and the identical
leak survives in the other on the identical trigger class. Stated plainly for the operator:
**B is one branch and one edit from closeable.** Allowlist the URL-query keys the way round
4 allowlisted the kv keys, pin the `?pwd=`/`?pass=` shapes, and the remaining residues are
the acceptable identity-component floor I ruled on above (whose only narrowing route is a
design change, which is why it stays residue). On the standing rule as written, the cycle
continues one more round.

Still not verified, carried forward unchanged: the live Cockroach leg (no safe DSN from
this machine), the live Postgres leg (`-- --ignored`, 7 tests, carrying the round-3
review's numbers), `postgres-live` on a real GitHub runner, and park-and-fail-over (unbuilt
by design, owned by its FUTURE entry).

## Cleanliness

Six temporary probe tests, three code mutations, and two throwaway probe configs applied,
every one reverted; `git status --porcelain` empty at review end. Gate logs live under
`target/review-gates/` (git-ignored, not committed). No container started, no `.env`
created, no `docker` command issued; `docs-telemetry` and colima's `default` profile
untouched. The main checkout at `/Users/narayan/Documents/work/lambo` was never written.
The only commits in this worktree are this review file's own.
