# Workstream B, end-to-end remediation round 4 (2026-08-24)

**Against**: `adve-review-mooshik-B-e2e-round4.md`, REQUEST_CHANGES, 3 findings
(1 P2 raised to P1 by operator ruling, 2 P3). Round 4 confirmed round 3's closure
of B-E2E-R3-1 as genuine under mutation and did not re-open it; what it found is
that the two *claims* that closure rested on are both false, and that a third leak
of the same class sits one function away in the parse path.
**Result**: **all three closed**, by taking apart the decision round 3 made rather
than patching the pair it missed. All gates green. **No round 5 review has run.**

## Provenance and environment, stated plainly

Worktree `/Users/narayan/Documents/work/lambo/.claude/worktrees/agent-aae0b0d775916bcc8`,
**branch `b-e2e-round4-remediation`**, created fresh at `origin/lambo-for-mooshik` @
`c927ebc` before anything ran. The main checkout was never written, and no existing
branch pointer was moved — round 3's remediation agent checked out `b0-pg-extraction`
in its worktree and moved a real branch; this round did not. Commits are incremental.

`printenv` shows no `LAMBO_*` and no `DATABASE_URL` in the ambient environment, and the
worktree has no `.env`. The one place `LAMBO_POSTGRES_DSN` appears is inline on the CLI
probe invocations below, scoped to those commands.

**No container was started and no `docker` command was issued this round.** Round 4
ruled skipping the live Postgres leg sound for a config-layer-only change and the
operator agreed, *conditional on the change staying out of the adapter and out of any
SQL*. It did: `git diff c927ebc --stat` touches `src/store/dsn.rs`, `src/store/mod.rs`
(the `overlay_env` comparison and its doc) and `src/mcp/endpoint.rs` (one call site and
its doc), and nothing under `src/store/pg/`. No SQL string, no migration, no query path,
no pool construction changed. `store_dsn_identity` and `store_dsn_echo` are both reached
before any pool exists. The live CLI probes below demonstrate it directly: they run
against a DSN pointing at port 55434, where nothing is listening, and still return the
refusal, because the refusal comes first. colima's `default` profile and the
`docs-telemetry` container on it were not touched. The live Postgres leg (`-- --ignored`,
7 tests) was therefore **not re-run**; that row still carries the round-3 review's
measurement.

## Per-finding closure

| Finding | Closed | What changed | Reverting it breaks |
| --- | --- | --- | --- |
| **B-E2E-R4-2** (P2, operator-raised to P1) the collapse concession is ratified on a factually wrong premise, and a constructible pair reopens E2E-F2 | yes | The trade was **withdrawn**, not re-argued. `canonical_store_dsn` was doing two jobs; it is now two functions. `store_dsn_echo` is what a human is shown and may withhold; `store_dsn_identity` is what is compared and hashed, never collapses (SHA-256 digest of the input for a spelling neither parser accounts for), and is never printed. `overlay_env` compares identities and prints echoes, and says so when a quote was withheld | `two_unparseable_spellings_are_two_identities`, `two_unquotable_dsns_still_reach_the_disagreement_refusal` (end-to-end through the real `overlay_env`), `sqlx_dials_what_this_module_cannot_parse`, `the_echo_and_the_identity_do_not_borrow_each_others_answers`, `an_unparseable_dsn_still_has_its_password_stripped` |
| **B-E2E-R4-1** (P3) the kv branch is a keyword blacklist, so a secret under a mistyped or abbreviated password key prints under "(passwords stripped)" | yes | `redact_kv_shaped` echoes only tokens whose key is on a positive list (`is_echoable_libpq_key`: the place/mode/name keywords). Everything else is dropped, **including keys this module has never heard of** — which is the only version of "allowlist" that means anything. `password`, `sslpassword`, `passfile` and `options` are off the list on purpose, each for a stated reason | `a_libpq_key_is_echoed_only_if_it_is_on_the_list`, and five new rows in `UNPARSEABLE_SHAPES` |
| **B-E2E-R4-3** (P3, pre-existing) `parse_postgres_url` echoes a credential into the identity when userinfo carries an unencoded `/` or `?` | yes | Two guards in the parse path: a path component may not carry an unencoded `:`, and a `:` in a hostport that is not a port separator is not part of a host. Both send the string to the now-hardened redactor instead of silently re-reading a password as a host or a database name | `an_unencoded_slash_in_a_password_does_not_reach_the_identity`, three new rows in `UNPARSEABLE_SHAPES` |

Two things not filed as findings changed alongside, both in the same invariant:

* **The control-character clause now holds on the parsed echo too.** Round 3 applied
  it only to the rebuilt echo, so `postgres://app@h:5432/lam%1b%5b2Jbo` parsed cleanly
  and handed a terminal an ANSI escape. `is_terminal_safe` is now the clause every
  echo passes, parsed or not; `is_safe_to_echo` is it plus the keyword check, which
  applies only where we cannot prove where the secret is.
* **The leak assertion tests lowercased fragments, not the whole secret.** Measured,
  not assumed — see M-R4R-6.

## The design decision, and why the alternatives were rejected — with measurements

The operator's ruling was that the question is not "how do I close this pair" but "is
collapsing identities acceptable at all". Answering that needed the premise measured
first, because round 3's error was asserting driver behaviour instead of testing it.

### First: sqlx, measured, not reasoned about

`PgConnectOptions::from_str` is the real dial path, not a stand-in — `store::pg`'s
`connect_options` reaches the network through `dsn.parse::<PgConnectOptions>()`
(`src/store/pg/mod.rs:1358`). Reading its source settles the mechanism: `from_str` hands
the string to the `url` crate and reads components off whatever comes back
(`sqlx-postgres-0.8.6/src/options/parse.rs`). **It validates no scheme at all.** A
throwaway probe (temporary test, run and reverted before any fix was written) put
nineteen shapes through both:

| shape | this module, round 3 | sqlx |
| --- | --- | --- |
| `app:one@host-a:26257/db-a` | `<unparseable dsn>` | **OK** host `localhost`, db `one@host-a:26257/db-a` |
| `app:two@host-b:26257/db-b` | `<unparseable dsn>` | **OK** host `localhost`, db `two@host-b:26257/db-b` |
| `postgres://app@host-a:/db_password_a` | `<unparseable dsn>` | **OK** host **`host-a`**, db **`db_password_a`** |
| `postgres://app@host-b:/db_password_b` | `<unparseable dsn>` | **OK** host **`host-b`**, db **`db_password_b`** |
| `postgre://app@h:26257/db` | `postgre://app@h:26257/db` | **OK** host `h`, port `26257` |
| `postgres://h:/db` | `postgres://h:/db` | **OK** host `h`, port 5432 |
| `postgres://app:S3cret/Hunter@h/db` | `postgres://[app:s3cret]:5432/Hunter@h/db` | **ERR** invalid port number |
| `postgres://app:S3cret?Hunter@h/db` | `postgres://[app:s3cret]:5432/` | **ERR** invalid port number |
| `postgres://ap/p:S3cretHunter@h:70000/db` | `postgres://ap:5432/p:S3cretHunter@h:70000/db` | **OK** host `ap`, db `p:S3cretHunter@h:70000/db` |
| `postgres://app:S3cretHunter@h:70000/db` | `postgres://app@h:70000/db` | **ERR** invalid port number |
| `host=… port=70000 user=app passwrod=S3cretHunter` | echoed **verbatim** | **ERR** relative URL without a base |

Three things fall out of that table, and all three matter.

1. **Round 3's premise is false, including on its own pinned example.** Rows 1–2 are the
   exact pair `an_unrecognised_shape_is_replaced_wholesale` asserted no driver would
   dial. sqlx dials them.
2. **A constructible pair reopens E2E-F2 against two different *real* hosts.** Rows 3–4
   both collapse here (the empty port defeats `split_host_port`, and the literal
   "password" in the database name fails `is_safe_to_echo`, so the echo becomes the
   constant) and resolve there to `host-a` and `host-b`. Under round 3 `overlay_env`
   found `from_file == from_env`, skipped the refusal, and set `self.dsn = env_dsn`.
   That is the workstream's original P1, reopened by construction.
3. **The R4-3 guards are the driver's rule, not a new opinion.** sqlx answers "invalid
   port number" to both slash/question shapes for exactly the reason we now refuse them.

### Then: is collapsing identities acceptable at all? No.

Round 3's reason for keeping any echo at all was that `canonical_store_dsn` was *also*
the identity, so a pure placeholder would defeat `overlay_env`'s refusal. That reason
was real. It was also the whole problem: **it is an argument that only exists because
one function was doing two jobs.**

The two jobs want opposite things from a spelling neither parser understands. Printing
wants an answer with no input in it — saying nothing is the only way to keep
"(passwords stripped)" true about a string we could not parse. Identity wants an answer
that is *different* for every different input — collapsing is the only way to make two
databases look like one. There is no single return value that does both, which is why
round 3 had to trade, and why round 2 before it had to splice.

So the fix is not a better trade. It is that the two callers get two functions:

* **`store_dsn_echo`** — for `overlay_env`'s message. Unchanged behaviour on every
  recognised shape (R2-5's four filed transcripts print byte-for-byte what they printed
  in round 2), and `<unparseable dsn>` otherwise. Free to withhold, because withholding
  costs a printer only diagnostics.
* **`store_dsn_identity`** — for `overlay_env`'s comparison and `store_identity`'s hash.
  A SHA-256 digest of the trimmed input for anything neither parser accounts for. It
  cannot collapse two different strings, it contains no substring of its input (so
  `store_identity`'s "the password never appears in the string that is hashed" survives),
  and nothing in the crate renders it to a terminal, a log or a filesystem path except
  as the input to a further hash.

### The three alternatives, and why each was rejected

**(a) Hash-suffix the placeholder** — the review's own suggestion, `<unparseable dsn:AB12>`,
one function still doing both jobs. Rejected on a real tension rather than a preference:
the suffix has to be short enough to be safe to print (it is derived from a credential,
and printing a digest of a secret is a new exposure of a class the review explicitly
noted this leak was *not*) and long enough not to collide (it is an identity). Four hex
characters is 16 bits — a 1-in-65 536 chance that two malformed DSNs share an identity,
which is a *silent wrong database*, and the review's own grading says one constructible
E2E-F2 reopening gates. Splitting the functions removes the tension instead of picking a
point on it: the digest is never printed, so it can be 128 bits.

**(b) Refuse in `overlay_env` when either side is unparseable.** Attractive and nearly
right — and it is what happens now in every case that matters, but as a *consequence* of
the identities differing rather than as a rule of its own. As a rule it is wrong in one
case: file and environment carrying the **same** malformed spelling. That is a config
that works today (sqlx dials `postgre://…`), the environment's value is byte-identical
to the file's, and there is nothing to disagree about. A blanket refusal breaks it; the
digest does not, because one string has one digest. Measured live below ("same
unquotable spelling on both sides"): it does not refuse, and proceeds to dial.

**(c) Use the echo as the identity when the echo is not the placeholder** — the cheap
version, which would have kept round 3's "two credentials, one identity" pin intact. It
**reopens R4-2 in a new spelling** and was rejected on that. The echo replaces a
password-mentioning query wholesale, so `postgre://app@h/db?password=a&host=host-a` and
`postgre://app@h/db?password=b&host=host-b` produce one echo — and sqlx applies the
`host=` query overlay, dialling `host-a` and `host-b`. Identical identity, two real
hosts. That is the same defect with a different trigger, which is exactly the outcome
this round exists to stop happening a third time.

### What it costs, stated rather than buried

**Two malformed spellings that differ only by a password are now two identities.** A
digest of the raw input cannot know that `postgre://app@h/db` and
`postgre://app:S3cret@h/db` are one database, so `overlay_env` refuses where round 3
would have overlaid. On the **parsed** path — where the documented "the file names the
database, the environment supplies the password" pattern actually lives, and where the
existing test `env_dsn_that_only_adds_credentials_overlays_the_file_dsn` sits — it costs
nothing, and that test is green.

The price is paid only by a DSN this module cannot parse *and* the operator was relying
on for the credential-overlay pattern. The consequence is a refusal that names both
sides and the variable, fixed by one edit. The alternative was the environment silently
outranking the file against a different real database. **Over-splitting is the safe
direction to be wrong in, the same way over-redacting is** — which is the rule R2-5
wrote down, applied to the other half of the function. It is pinned with a comment
saying so in `two_unparseable_spellings_are_two_identities`, so that paying it stays a
decision.

Round 3's pin that this replaces — "two credentials for one unparseable spelling are
still one identity" — carried its own instruction: *"If a future change makes the
placeholder distinguishing, this assertion is the one to rewrite."* It is rewritten,
and the *echo* half of it is kept: the operator still sees the same string either way.

### And the message had to learn to explain itself

With the identities distinguishing, a disagreement between two spellings neither parser
understands now refuses — and quotes `<unparseable dsn>` on both sides, which reads like
a bug ("those are the same, why is it refusing?"). `withheld_note` appends one sentence
when either quote is withheld, saying that the placeholder is not a quotation and that
the comparison ran on the strings themselves. It is empty when both sides were quotable,
so the ordinary message is unchanged.

## Documents corrected alongside the code

Annotated in place, in the style R2-4 and R3 used, rather than rewritten silently:

* **`b-run/B-e2e-remediation-round3.md`**, twice. The "What this costs" section now
  carries a **SUPERSEDED** block above the paragraph whose central claim round 4
  falsified, with the sqlx measurement that falsifies it and a note that the residue is
  *withdrawn* rather than re-argued. The "Still not verified" bullet about the libpq
  branch's boundary carries a second one: it was right that this was residue, wrong
  about how narrow, and the "would never dial" defence does not apply on a refusal path.
  Both original paragraphs are kept verbatim underneath — they are the reasoning a later
  round falsified, and deleting them would hide that this happened.
* **`B-postgres-store.md`**, "Comparison is on identity, not spelling". Its
  post-R3-1 correction said the fallback "is now an allowlist" — true of one branch of
  two — and did not say that one function was serving both callers. A second in-place
  correction states all three: the split, the kv key set, and the parse-path guards.
* **`src/mcp/endpoint.rs`, `store_identity`.** Its sentence about the pre-hash string is
  still true and now says why it is true *of a digest*, and records the second thing the
  digest buys: two serves whose DSNs this module cannot parse no longer share a socket
  path, which is J2-R1-2's collision in a different costume.
* **`src/store/mod.rs`, `overlay_env`.** A new section, "What is compared is not what is
  printed", because the previous doc described one function doing both.
* **`b-run/CYCLE.md`.** Standing gate table updated to this round's numbers, live
  Postgres row still explicitly marked *not re-run*.

## Mutations run

Every closure was mutation-checked. Red/green recorded as observed, not as expected.

| # | Mutation | Expected | Observed | Reverted |
|---|---|---|---|---|
| **M-R4R-1** | `unaccounted_identity` returns `UNPARSEABLE_DSN` — i.e. exactly what round 3 shipped | the R4-2 pins red | **5 RED.** `two_unparseable_spellings_are_two_identities` (`left: "<unparseable dsn>" right: "<unparseable dsn>"` on the `host-a`/`host-b` pair), `the_echo_and_the_identity_do_not_borrow_each_others_answers`, `an_unparseable_dsn_still_has_its_password_stripped`, `sqlx_dials_what_this_module_cannot_parse`, and — the one that matters — `store::tests::two_unquotable_dsns_still_reach_the_disagreement_refusal` panicked on its `expect_err`, meaning **the real `overlay_env` returned `Ok` and took the environment's DSN.** R4-2's reopening, reproduced end to end | yes, green after |
| **M-R4R-2** | `is_echoable_libpq_key` becomes `!mentions_password(key)` — round 3's blacklist | the R4-1 pins red | **2 RED**, and both failure messages print the leak verbatim: `the echo carries "s3crethunter" of the password: host=127.0.0.1 port=70000 user=app passwrod=S3cretHunter -> host=127.0.0.1 port=70000 user=app passwrod=S3cretHunter` | yes, green after |
| **M-R4R-3** | drop the `path.contains(':')` guard in `parse_postgres_url` | the R4-3 pins red | **2 RED.** `an_unencoded_slash_in_a_password_does_not_reach_the_identity` at `postgres://ap/p:S3cretHunter@h:70000/db`, plus the universal sweep | yes, green after |
| **M-R4R-4** | restore `split_host_port`'s `_ => Some((percent_decode(hostport), None))` fallback | the R4-3 pins red | **5 RED**, including both R4-2 pins and the end-to-end `overlay_env` test — the fallback changes which shapes reach the unparseable path at all, so it moves more than R4-3 | yes, green after |
| **M-R4R-5** | drop the `is_terminal_safe` clause from `store_dsn_echo`'s parsed arm | one pin red | **1 RED.** `a_parsed_echo_still_may_not_carry_a_control_character`: `left: "postgres://app@h:5432/lam\u{1b}[2Jbo"` — an ANSI escape on its way to a terminal | yes, green after |
| **M-R4R-6** | *measurement, not a pin*: restore the R4-3 leak **and** round 3's assertion (`!canon.contains("S3cretHunter")`, case-sensitive, whole secret only) | measurement | Round 3's assertion **catches** `postgres://ap/p:S3cretHunter@h:70000/db` (full secret intact) but is **GREEN** on `postgres://app:S3cret/Hunter@h/db` → `postgres://[app:s3cret]:5432/Hunter@h/db` — the entire password, split across the host and database components, one of them lowercased. That is why the check is now lowercased fragments. The same run surfaced a **sixth** R4-3-class shape nobody had enumerated: round 3's `postgres://app:pa://S3cretHunter@…` leaks through the *parse* path as `postgres://[app:pa:]:5432/S3cretHunter@…` once the guards are gone | yes, restored |
| **M-R4R-7** | `withheld_note` always returns `""` | the explanation pins red | **2 RED**, and the failure output is the message the note exists to prevent: `… the config file says <unparseable dsn> and LAMBO_POSTGRES_DSN says <unparseable dsn> (passwords stripped). Refusing to guess which one you meant…` | yes, green after |

Seven mutations, seventeen distinct red outcomes and two measured comparisons. The
copy-pasteable recipe in the doc comment of
`an_unparseable_dsn_still_has_its_password_stripped` still works and is unchanged;
`a_libpq_key_is_echoed_only_if_it_is_on_the_list` and
`two_unparseable_spellings_are_two_identities` carry their own one-line recipes for the
same reason.

Also run before any fix was written: the sqlx-vs-ours divergence probe quoted above
(nineteen shapes), added as a `#[test]`, run, and removed. Its content survives as the
permanent test `sqlx_dials_what_this_module_cannot_parse`, so the measurement round 3
asserted instead of running is now executed on every build that has the driver.

## Live CLI probes (debug binary, this round's own runs)

`lambo.toml` carrying `store.kind = "postgres"` and the shape under test as `store.dsn`;
`LAMBO_POSTGRES_DSN=postgres://lambo:lambo@127.0.0.1:55434/livetest` inline; `lambo stats
--session probe`. Nothing listens on 55434 — the refusal is reached before any dial,
which is the point. All eight returned rc 1 and **none of the eight output strings
contains `S3cretHunter`.**

| `store.dsn` in the config file | what the refusal printed for the file side |
| --- | --- |
| `postgres:/app:S3cretHunter@127.0.0.1:26257/lambo` | `<unparseable dsn>` + the withheld note |
| `app:S3cretHunter@127.0.0.1:26257/lambo` | `<unparseable dsn>` + the withheld note |
| `postgres://app@127.0.0.1:70000/lambo?password=S3cretHunter` | `postgres://app@127.0.0.1:70000/lambo?<query redacted>` — **unchanged from round 3** |
| `host=127.0.0.1 port=70000 dbname=lambo user=app password = S3cretHunter` | `<unparseable dsn>` + the withheld note |
| `postgres://app:S3cretHunter@127.0.0.1:70000/lambo` | `postgres://app@127.0.0.1:70000/lambo` — **unchanged from round 2** |
| `host=127.0.0.1 port=70000 user=app passwrod=S3cretHunter` (R4-1) | `host=127.0.0.1 port=70000 user=app` — the secret is gone, the mistyped port is not |
| `postgres://ap/p:S3cretHunter@h:70000/db` (R4-3) | `postgres://ap/p@h:70000/db` |
| `postgres://app:S3cret/Hunter@h/db` (R4-3) | `postgres://app@h/db` |

Rows 3 and 5 are the regression checks: rounds 2 and 3's transcripts still read exactly
as they recorded them. Rows 1, 2 and 4 read as round 3 recorded them plus the withheld
note, which is the only change to those three.

Three more probes, on the behaviours the design turns on:

* **R4-2's constructible pair** (file `postgres://app@host-a:/db_password_a`, env
  `postgres://app@host-b:/db_password_b`): **refuses**, quoting `<unparseable dsn>` on
  both sides with the note explaining why they read the same. Under round 3 this
  returned success and silently used `host-b`.
* **The same unquotable spelling on both sides**: does **not** refuse. It proceeds to
  dial and fails with `failed to lookup address information` for `host-a` — which is the
  environment's value being taken, exactly as before. This is the case a blanket refusal
  would have broken.
* **The env-only CI path** (no `store.dsn` in the file at all): no refusal, dials
  127.0.0.1:55434 and times out. Untouched.

## Gates, all run on the finished tree

| Gate | Round-4 review baseline | This round |
| --- | --- | --- |
| `cargo fmt --all -- --check` | pass | **pass** |
| `cargo clippy --all-targets -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-cockroach,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | pass | **pass** |
| `cargo test --features store-cockroach` | 952 / 0 / 4 | **959 / 0 / 4** |
| `cargo test --features store-cockroach,fixtures` | 1012 / 0 / 12 | **1019 / 0 / 12** |
| `cargo test --features store-postgres` | 939 / 0 / 7 | **946 / 0 / 7** |
| `cargo test --features store-postgres,fixtures` | 996 / 0 / 7 | **1003 / 0 / 7** |
| `cargo test --features store-sqlite,fixtures` | 1078 / 0 / 3 | **1084 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | 615 / 0 / 0 | **622 / 0 / 0** |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 53 warnings | **53** |
| `cargo doc … --features store-postgres,store-cockroach,store-sqlite,fixtures` | 53 warnings | **53** |
| live Postgres `-- --ignored` (pinned container) | 7 / 0 (round-3 review's measurement) | **not run**, deliberately — see the environment note |

Suite numbers are the sum across each invocation's test binaries, the same arithmetic
the four prior tables used.

**The deltas are not uniform this round, and that is deliberate.** Round 3's tables could
say "+2 in every suite" because none of its tests was feature-gated. One of this round's
is: `sqlx_dials_what_this_module_cannot_parse` needs the driver, so it is
`#[cfg(any(feature = "store-postgres", feature = "store-cockroach"))]`. That is the point
of the test — a measurement of sqlx cannot be made without sqlx — and it is why
`store-sqlite,fixtures` moves by one less than the others.

| suite | delta | why |
| --- | ---: | --- |
| `store-cockroach` | +7 | six new `store::dsn` tests (the sqlx one included, the driver is present) + `two_unquotable_dsns_still_reach_the_disagreement_refusal` |
| `store-cockroach,fixtures` | +7 | same |
| `store-postgres` | +7 | same |
| `store-postgres,fixtures` | +7 | same |
| `store-sqlite,fixtures` | +6 | **no postgres driver**, so `sqlx_dials_what_this_module_cannot_parse` does not compile |
| `no-default store-cockroach` | +7 | driver present |

Both doc rows are reproduced in full, both feature sets, as `CYCLE.md`'s standing rule
requires. The naive `^warning` grep reports one more than the summary line on each by
counting `cargo doc`'s own summary; the round-3 review established that and it is still
not drift.

## Still not verified

* **No round 5 review.** This closure is self-reported by the party that made it. Three
  rounds in a row have now found a hole in this module's previous closure, and each of
  those closures passed its own tests. What is different this round is that the central
  claim is a *measurement* rather than an argument — `sqlx_dials_what_this_module_cannot_parse`
  executes the thing round 3 asserted — but the design *around* that measurement is still
  a judgement made by the same party.
* **The identity's non-collapse rests on SHA-256, and the over-split is real.** "Two
  different strings get two different identities" is true modulo a 128-bit collision,
  which is not something a typo finds. The cost in the other direction is not
  hypothetical: two malformed spellings differing only by a password now refuse where
  they used to overlay. That is a deliberate trade, argued above and pinned, not an
  oversight — but it is a behaviour change an operator could hit.
* **The username position is still echoed, on every path, by design.** A password typed
  into `?user=` or into the username field is part of the identity this module is
  documented to keep ("Username is kept"), so it is echoed unless it happens to spell
  "password". `postgres://h/db?user=app:S3cret@x` prints its argument. Narrowing it
  would break the legitimate `user@server` spelling that some managed Postgres offerings
  require, so this is stated as a bounded residue rather than closed. It is the same
  class as R2-5 and it is **not** closed.
* **`?host=` and `?dbname=` query overlays are echoed verbatim.** Same reasoning: they
  are identity components the module is documented to honour. A secret parked in one of
  them prints. Not measured against a realistic operator scenario, and not closed.
* **The libpq key list is a judgement about which keywords are not secrets.** It is a
  much smaller judgement than round 3's — the failure mode of a wrong entry is now
  "dropped a useful diagnostic" for anything omitted, and only a *wrongly included* key
  can leak. `sslkey` (a path to a key file, not the key) is the closest call on it.
* **`store_identity`'s doc sentence is true, and was false in between.** Unchanged from
  round 3's statement: it held before R2-5, was false from R2-5 until R3-1, and is true
  again — now of a digest. Any socket path derived during that window was derived from a
  string that contained a credential. The hashes were never published, so nothing needs
  rotating.
* **The live Cockroach leg**, unchanged and still unrun. No safe DSN from this machine;
  the 15 `#[ignore]`d tests remain on the orchestrator. Nothing in this round touches
  Cockroach.
* **The live Postgres leg was not re-run.** Justified above and structurally checked (no
  file under `src/store/pg/` is in the diff), but stated as a gap rather than a pass: the
  7 `-- --ignored` tests carry the round-3 review's numbers, not this round's.
* **`postgres-live` on a real GitHub runner** and **park-and-fail-over** (unbuilt by
  design, owned by its FUTURE entry) are carried forward unchanged.
