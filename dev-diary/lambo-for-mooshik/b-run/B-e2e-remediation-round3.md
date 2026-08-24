# Workstream B, end-to-end remediation round 3 (2026-08-24)

**Against**: `adve-review-mooshik-B-e2e-round3.md`, REQUEST_CHANGES, 1 finding (P3).
Round 3 verified all six of round 2's closures as genuine and re-opened none of
them, so this round touches exactly one thing.
**Result**: **B-E2E-R3-1 closed**, by rebuilding the redactor rather than extending
it. All gates green. **No round 4 review has run.**

## Provenance and environment, stated plainly

Worktree `/Users/narayan/Documents/work/lambo/.claude/worktrees/agent-acb0fd312c657c913`,
branch `worktree-agent-acb0fd312c657c913`, reset to `origin/lambo-for-mooshik` @ `fa58c9f`
before anything ran. The main checkout was never written. Commits are incremental.

`printenv` shows no `LAMBO_*` and no `DATABASE_URL` in the ambient environment, and the
worktree has no `.env`. The one place `LAMBO_POSTGRES_DSN` appears is inline on the five
CLI probe invocations below, scoped to those commands.

**No container was started and no `docker` command was issued this round.** The finding is
pure string handling — `canonical_store_dsn` is reached before any driver is constructed,
and the refusal it feeds is a config-layer refusal that never dials. The live CLI probes
below prove that by running against a DSN pointing at port 55434, where nothing is
listening: they still return rc 1 with the disagreement refusal, because the refusal comes
first. colima's `default` profile and the `docs-telemetry` container on it were not
touched. The live Postgres leg (`-- --ignored`, 7 tests) was therefore **not re-run**;
this round changed no SQL, no adapter, and no query path.

## Per-finding closure

| Finding | Closed | What changed | Reverting it breaks |
| --- | --- | --- | --- |
| **B-E2E-R3-1** (P3) a DSN with userinfo but no `://` prints its password under "(passwords stripped)" | yes | `redact_unparseable_dsn` stopped being a splice and became an **allowlist**. It builds an echo out of the pieces a recognised shape positively accounts for — a URL with an RFC 3986 scheme, or a libpq string whose *every* whitespace token is `key=value` — and anything it cannot account for becomes the constant `<unparseable dsn>`, which has no input in it and so cannot leak. One gate covers both branches: an echo ships only if it is non-empty, mentions no password, and carries no control characters. `strip_libpq_password_token` is deleted. `store_identity`'s and `overlay_env`'s doc claims are re-checked and now true | `an_unparseable_dsn_still_has_its_password_stripped` (14 shapes), `an_unrecognised_shape_is_replaced_wholesale`, `a_recognised_shape_still_shows_the_operator_the_typo`; and the five live CLI transcripts below |

### Two documents corrected alongside the code

Both were re-checked because the review asked for one of them by name, and the other
turned out to describe the failed mechanism as the fix.

* **`src/mcp/endpoint.rs`, `store_identity`.** The review's secondary consequence. Its
  "the password never appears in the string that is hashed" is true again, and the doc now
  says *why* it is true (the unparseable path replaces rather than echoes) and records that
  the sentence was false between R2-5 and this commit, so a future auditor is not misled by
  a claim that has quietly changed footing twice.
* **`dev-diary/lambo-for-mooshik/B-postgres-store.md`.** Still said "the fallback now drops
  the userinfo password too" — a description of the thing that broke. In-place correction
  in R2-4's style, naming the five shapes and the allowlist that replaced it.
* **`b-run/CYCLE.md`.** Standing gate table updated to this round's numbers, with the live
  Postgres row explicitly marked as *not re-run* rather than silently carried as if it had
  been.

## The design decision, and why the other option was rejected

The review named two options. They are not equally good, and the argument for the one
chosen is a measurement rather than a preference.

### First, the size of the hole was measured, not assumed

A throwaway probe (temporary test, run and reverted before any fix was written) put the
two filed shapes and five derived ones through the shipped R2-5 redactor. **Five leaked**:

| shape | why it walked past the splice | leaked |
| --- | --- | --- |
| `app:S3cretHunter@127.0.0.1:26257/lambo` | scheme dropped, so `find("://")` fails | yes, verbatim |
| `postgres:/app:S3cretHunter@127.0.0.1:26257/lambo` | one keystroke, same reason | yes, verbatim |
| `postgres://app@127.0.0.1:70000/lambo?password=S3cretHunter` | **has** `://` **and** `@`; the secret is behind both, in the query. This is PostgreSQL's own URI form | yes, verbatim |
| `postgres://127.0.0.1:70000/lambo?password=S3cretHunter` | same, and no userinfo at all, so the `rfind('@')` arm returns early | yes, verbatim |
| `host=127.0.0.1 port=70000 dbname=lambo user=app password = S3cretHunter` | libpq permits spaces around `=`, so no whitespace token is ever spelled `password=…` | yes, as `… user=app = S3cretHunter` |
| `postgres://app:pa://S3cretHunter@127.0.0.1:70000/lambo` | predicted to move the first-`://` anchor | **no** — prediction wrong, recorded as wrong |
| `postgres://app:S3cretHunter@127.0.0.1:70000/lam@bo` | the last-`@` rule | no, over-redacts to `postgres://app@bo` |

The first two are the finding. The middle three were derived here, and they are the whole
argument: **they carry the `://` that the proposed repair would have anchored on.**

### Then the rejected option was built and measured too

The review's option A is "extend the splice to a scheme-less authority — when there is no
`://` but the string carries a `:` inside a `userinfo@` prefix before any `/`, drop `:…`
up to the `@`". That was implemented literally (mutation M-R3R-2 below) and run against
the full shape table. It closes **two of the seven** leaking or near-leaking shapes.

It does not close the review's *own second filed shape*. In `postgres:/app:S3cret@h/db`
the `@` falls **after** the first `/`, so the "before any `/`" clause never fires and the
string is returned verbatim. The option as written closes the first example in the finding
and not the second one in the same paragraph. It closes none of the three derived shapes.

That is the answer to "what would the third missed anchor be": the third, fourth and fifth
already exist, two of them inside the `://` the repair was aimed at, and one of them in
libpq's documented spacing. A splice is a blacklist, and a blacklist over a string we have
already admitted we cannot parse has to enumerate every hiding place a secret has. This one
has now been shown to miss on two independent occasions. There is no reason to believe a
sixth does not exist, and no way to become confident there isn't one short of writing the
parser whose absence put us on this path in the first place.

### So: the placeholder, but not for everything

Option B (refuse to echo, print a fixed placeholder) cannot leak by construction, and that
is the right property. Applied to *everything*, though, it breaks something real, and the
review's framing of its cost as "diagnostic value" understates it:

**`canonical_store_dsn` is not only a printer. It is the identity function.**
`store_identity` hashes it into the session socket path and `overlay_env` compares it to
decide whether the config and the environment name the same database. Collapse every
unparseable spelling onto one constant and two *different* malformed DSNs compare equal —
so E2E-F2's disagreement refusal, the thing this message exists to print, stops firing and
the environment outranks the file in silence. Trading a P3 leak for that would be a bad
trade.

The hybrid the review allowed is what shipped, and it is drawn at the line where the
argument actually is:

* **A shape we can account for is echoed.** All four of R2-5's filed shapes still print
  exactly what they printed before this round — `postgres://app@127.0.0.1:70000/lambo` and
  the rest, byte for byte, asserted with `assert_eq!`. The operator still sees the port
  they mistyped, which is the only reason the message quotes the DSN at all. The last-`@`
  over-redaction rule the review verified is kept verbatim, including its `postgres://app@bo`
  behaviour on an `@` in the path.
* **A query string that mentions a password loses the query, not the authority.**
  `postgres://app@127.0.0.1:70000/lambo?password=…` prints as
  `postgres://app@127.0.0.1:70000/lambo?<query redacted>`. The leak is gone and the mistyped
  port survives, so this shape pays none of the diagnostic cost.
* **A shape no branch accounts for is replaced wholesale.** No substring of it is echoed,
  so there is nothing to be wrong about.
* **And one gate over both branches**, so the property is checkable in one place rather
  than argued per shape: `is_safe_to_echo` — non-empty, no `password` substring
  (case-insensitive, which also catches `sslpassword`), no control characters. The last
  clause is a free hardening: this string is written to a terminal, and an ANSI escape in a
  DSN had nothing to do there either.

The keyword check over-redacts a username or an `application_name` containing "password",
and that is deliberate — over-redacting stays the safe direction to be wrong in, which is
the same rule R2-5 wrote down and the same one this round kept.

### What this costs, stated rather than buried

Two *unrecognised* spellings now collapse onto one identity, where before the splice kept
them apart. `overlay_env` cannot distinguish `app:one@host-a/db-a` from
`app:two@host-b/db-b`; it will take the environment's DSN instead of refusing. Both are
strings no driver will dial — anything sqlx can actually connect to parses through
`parse_postgres_url` and never reaches this function — so the observable consequence is a
connection error instead of a disagreement refusal, on a config that was already broken.
It is a real narrowing and it is pinned by an assertion in
`an_unrecognised_shape_is_replaced_wholesale` with a comment saying so, so that paying it
stays a decision rather than an accident.

One behaviour change is worth calling out because it contradicts a pin round 2 wrote:
`canonical_store_dsn("weird-thing password=S3cretHunter")` used to return `"weird-thing"`
and now returns `<unparseable dsn>`. That old answer was only safe by luck of spacing —
`password = S3cretHunter` in the same string is the leaking shape above. A token that is
not `key=value` disqualifies the whole libpq branch, and `weird-thing` is such a token.

## Mutations run

| # | Mutation | Expected red | Observed | Reverted |
|---|---|---|---|---|
| M-R3R-1 | `redact_unparseable_dsn` reverted to the shipped R2-5 splice, `strip_libpq_password_token` restored with it | all three redaction tests | **all three RED.** `an_unparseable_dsn_still_has_its_password_stripped` panicked at `dsn.rs:517` printing `app:S3cretHunter@127.0.0.1:26257/lambo -> app:S3cretHunter@127.0.0.1:26257/lambo` — the leak verbatim, in the failure message. `a_recognised_shape_still_shows_the_operator_the_typo` RED at `dsn.rs:557` with `left: "postgres://app@127.0.0.1:70000/lambo?password=S3cretHunter"`. `an_unrecognised_shape_is_replaced_wholesale` RED at `dsn.rs:617` | yes, 4/4 green after |
| M-R3R-2 | the **rejected option**: the R2-5 splice plus the review's own scheme-less extension, implemented literally | measurement, not a pin | 5 of the 14 shapes still leak under it, including the review's second filed shape `postgres:/app:…@…` (its `@` is after the first `/`), both `?password=` shapes, the libpq-spacing shape, and the username-position shape. It also echoes the ANSI escape | yes, restored |

Line numbers in that table are positions in the **mutated** tree, which carries the
restored splice and so sits about twenty lines below the committed one.

The loop in `an_unparseable_dsn_still_has_its_password_stripped` aborts at the first
leaking row rather than reporting all five, which is why the five-row table above comes
from the enumerating probe instead. Both were run; both were reverted.

Two mutations, four distinct red outcomes plus one measured comparison. Round 2's
`M-R2-5` / round 3's `M-R3-5` recipe — "restore the `strip_libpq_password_token(trimmed)`
fallback" — **no longer applies**, because that function is deleted. The replacement recipe
is written into the doc comment on
`an_unparseable_dsn_still_has_its_password_stripped` as a copy-pasteable body, so the next
reviewer does not have to reconstruct it.

## Live CLI probes (debug binary, this round's own runs)

`lambo.toml` carrying `store.kind = "postgres"` and the shape under test as `store.dsn`;
`LAMBO_POSTGRES_DSN=postgres://lambo:lambo@127.0.0.1:55434/livetest` inline; `lambo stats
--session probe`. Nothing listens on 55434 — the refusal is reached before any dial, which
is the point. All five returned rc 1 and none of the five output strings contains
`S3cretHunter`.

| `store.dsn` in the config file | what the refusal printed |
| --- | --- |
| `postgres:/app:S3cretHunter@127.0.0.1:26257/lambo` | `the config file says <unparseable dsn> and LAMBO_POSTGRES_DSN says postgres://lambo@127.0.0.1:55434/livetest (passwords stripped)` |
| `app:S3cretHunter@127.0.0.1:26257/lambo` | `… says <unparseable dsn> …` |
| `postgres://app@127.0.0.1:70000/lambo?password=S3cretHunter` | `… says postgres://app@127.0.0.1:70000/lambo?<query redacted> …` |
| `host=127.0.0.1 port=70000 dbname=lambo user=app password = S3cretHunter` | `… says <unparseable dsn> …` |
| `postgres://app:S3cretHunter@127.0.0.1:70000/lambo` (R2-5's transcript) | `… says postgres://app@127.0.0.1:70000/lambo …` — **unchanged** from round 2 |

The first two are the review's live demonstration, re-run. The third is the derived shape
that keeps its diagnostic value anyway. The last is the regression check: round 2's
transcript still reads exactly as round 2 recorded it.

## Gates, all run on the finished tree

| Gate | Round-3 review baseline | This round |
| --- | --- | --- |
| `cargo fmt --all -- --check` | pass | **pass** |
| `cargo clippy --all-targets -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-cockroach,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-postgres,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings` | pass | **pass** |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | pass | **pass** |
| `cargo test --features store-cockroach` | 950 / 0 / 4 | **952 / 0 / 4** |
| `cargo test --features store-cockroach,fixtures` | 1010 / 0 / 12 | **1012 / 0 / 12** |
| `cargo test --features store-postgres` | 937 / 0 / 7 | **939 / 0 / 7** |
| `cargo test --features store-postgres,fixtures` | 994 / 0 / 7 | **996 / 0 / 7** |
| `cargo test --features store-sqlite,fixtures` | 1076 / 0 / 3 | **1078 / 0 / 3** |
| `cargo test --no-default-features --features store-cockroach` | 613 / 0 / 0 | **615 / 0 / 0** |
| `cargo doc --no-deps --document-private-items --features store-cockroach,fixtures` | 53 warnings | **53** |
| `cargo doc … --features store-postgres,store-cockroach,store-sqlite,fixtures` | 53 warnings | **53** |
| live Postgres `-- --ignored` (pinned container) | 7 / 0 | **not run**, deliberately — see the environment note |

Suite numbers are the sum across each invocation's test binaries (16 per suite), the same
arithmetic the three prior tables used.

**+2 in every one of the six combinations**, and exactly +2: this round added two
always-compiled tests (`a_recognised_shape_still_shows_the_operator_the_typo`,
`an_unrecognised_shape_is_replaced_wholesale`) and kept
`an_unparseable_dsn_still_has_its_password_stripped` under its own name while growing it
from four shapes to fourteen. Nothing is feature-gated, so no suite moves by a different
amount. Zero failures and unchanged ignore counts everywhere.

Both doc rows hold at **53**. The naive `^warning` grep reports 54 on each, by counting
`cargo doc`'s own summary line — the round-3 review established that and it is still not
drift. The doc row is standing per `CYCLE.md` and is reproduced here in full, both feature
sets, as that rule requires.

## Still not verified

* **No round 4 review.** This closure is self-reported by the party that made it. Two
  rounds in a row have now found a hole in this exact function's previous closure, and
  round 2's fix passed round 2's own tests. The general claim made here — "an allowlist
  cannot leak, so the class is closed" — is stronger than the claim R2-5 made, but it is
  still a claim from the same party, and the only thing that has actually been demonstrated
  is that fourteen specific shapes are safe.
* **The allowlist's own boundary is a judgement, not a proof.** The libpq branch accepts
  any identifier-shaped key, so a secret parked under a non-libpq key (`secret_pw=…`) would
  be echoed. The reasoning for drawing it there is that such a string is not a DSN — libpq
  rejects unknown connection options outright, so it would never dial — but that is an
  argument, and arguments are what the last two rounds have been about. The `password`
  substring check is the thing standing behind it, and it is a keyword check, not
  understanding.
* **`store_identity`'s doc sentence is now true, and was false in between.** The sentence
  "the password never appears in the string that is hashed" held before R2-5, was false
  from R2-5 until this commit for a DSN with userinfo and no `://`, and is true again. The
  hashes themselves were never published, so nothing needs rotating — but any socket path
  derived from one of those shapes during that window was derived from a string that
  contained a credential.
* **The live Cockroach leg**, unchanged and still unrun. No safe DSN from this machine; the
  15 `#[ignore]`d tests remain on the orchestrator. Nothing in this round touches Cockroach.
* **The live Postgres leg was not re-run.** Justified above (no SQL, no adapter, no query
  path touched) but stated as a gap rather than a pass: the 7 `-- --ignored` tests carry the
  round-3 review's numbers, not this round's.
* **Park and fail over is still unimplemented**, unchanged since R2-4 closed its
  bookkeeping. B's behaviour on a lost lease is still that the loser refuses.
