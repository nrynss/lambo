# L1 remediation, round 1 (2026-08-25)

Round 1 of adversarial review on `9580873` returned **REQUEST_CHANGES**, 1 P1, 3 P2, 6 P3
([`adve-review-mooshik-L1-round1.md`](../../adversarial-review/adve-review-mooshik-L1-round1.md)).
All ten are closed here. None was ruled not-a-finding; one had its **stated consequence**
corrected, and the correction is recorded rather than repeated.

Every pin below was mutated: the mutation was applied to the working tree, the pin was run,
the source was restored from a byte-for-byte backup, and `git status --short` was checked.

---

## P1-1. The refresh grant dropped the caller's scope. CLOSED.

**What was true.** `GoogleOAuthTokenSource::access_token_with_expiry` posted the
`refresh_token` grant with `grant_type`, `client_id`, `client_secret`, `refresh_token` and
nothing else. `self.scope` was consumed only by `mint_jwt`, which is service-account only. So
the module doc's *"the token source takes the scope string rather than assuming one, so
neither consumer silently borrows the other's authority"* was true of one grant and inert on
the other, and on an authorized-user ADC the embedder and the store received the same token
carrying whatever the ADC had been granted.

**What the review got wrong, and it matters.** The review asserted two things that were
measured false on this host:

1. *"An ADC file written by a plain `gcloud auth application-default login` carries
   `cloud-platform`, `openid` and `userinfo.email`, not `sqlservice.login`"*, therefore the
   Cloud SQL login "is refused" here. Not on this machine. Introspecting this host's ADC
   token at `https://oauth2.googleapis.com/tokeninfo` returns:

   ```
   scope = email
           https://www.googleapis.com/auth/cloud-platform
           https://www.googleapis.com/auth/sqlservice.login
           https://www.googleapis.com/auth/userinfo.email
           openid
   ```

   The shipped code therefore worked here, by luck of this ADC's granted scope set rather
   than by design.

2. *"The correct fix is not 'also send `scope`' (Google's refresh-token endpoint returns the
   originally granted scopes regardless)."* Measured against the real endpoint with this
   host's ADC, one request per row, printing only the response's `scope`:

   | request | response |
   | --- | --- |
   | no `scope` field (the shipped behaviour) | `200`, `scope=cloud-platform sqlservice.login userinfo.email openid` |
   | `scope=cloud-platform` | `200`, `scope=cloud-platform` |
   | `scope=cloud-platform sqlservice.login` | `200`, `scope=cloud-platform sqlservice.login` |
   | `scope=.../auth/drive` (never granted) | `400 {"error": "invalid_scope"}` |

   Narrowing works. Sending `scope` is exactly the fix, and asking for something never
   granted is refused **at the token endpoint, by name**, which is the loud failure worth
   having.

So the defect is real and the severity is real, but the consequence is not "the login is
refused on this host". It is: the documented contract was false on the ADC grant, and an ADC
granted **without** `sqlservice.login` would have failed confusingly at the database instead
of at the token endpoint.

**The fix.** `src/gcp_auth.rs` sends `("scope", self.scope.clone())` on the `refresh_token`
grant, with the RFC 6749 section 6 reasoning inline. Prose corrected in `src/gcp_auth.rs`
(module doc now states what each grant does and what an ADC can and cannot ask for),
`CHANGELOG.md`, `dev-diary/lambo-for-mooshik/L-gcp-hosted-postgres.md` and
`L1-gapfix-implementation.md` §2. No document now claims more than the code does.

**Pin.** `gcp_auth::tests::the_callers_scope_is_sent_on_the_refresh_grant`. Two token
sources, two mock endpoints: the Cloud SQL caller must post the whole body
`grant_type=refresh_token&client_id=...&client_secret=...&refresh_token=...&scope=<cloud-platform>+<sqlservice.login>`
and the Vertex caller must post the same body ending `scope=<cloud-platform>` alone. The
**whole** body is matched, not a fragment, and both callers are exercised, so a `scope`
hardcoded to either consumer's constant passes one half and fails the other.

**Mutation.** Delete the `("scope", self.scope.clone())` line:

```
test gcp_auth::tests::the_callers_scope_is_sent_on_the_refresh_grant ... FAILED
Backend("OAuth token endpoint returned 404 Not Found: {\"message\":\"Request did not match any route or mock\"}")
```

**Live re-verification (required: this change touches the live-verified Vertex path).** The
embedder's ADC refresh grant now carries `scope=cloud-platform`.

```
GOOGLE_APPLICATION_CREDENTIALS=~/.config/gcloud/application_default_credentials.json \
  cargo test --features embed-gemini --lib \
  embed::gemini::tests::gemini_live_embeds_against_vertex -- --ignored --nocapture

test embed::gemini::tests::gemini_live_embeds_against_vertex ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 944 filtered out; finished in 1.95s
```

Real OAuth mint plus a real `embedContent` round trip against `us-central1`, asserting the
1536 width and unit norm. **No regression.**

---

## P2-1. The embedder ignored `GCP_LAMBO_CREDENTIALS`. CLOSED.

The store resolved `crate::gcp_auth::credentials_path_from_env()`
(`GCP_LAMBO_CREDENTIALS`, else `GOOGLE_APPLICATION_CREDENTIALS`); `build_gemini_embedder`
resolved `GOOGLE_APPLICATION_CREDENTIALS` alone. The export block in
`L-gcp-hosted-postgres.md` §"Running the shared-SA store path" therefore started the store
and refused the embedder, which is the shared-service-account design failing at precisely
the thing it exists to do.

**The fix.** `build_gemini_embedder` now falls back to
`crate::gcp_auth::credentials_path_from_env` after `cfg.gemini_credentials`. The refusal
names all three ways to answer it: *"Gemini embedder needs service-account credentials: set
`gemini_credentials` or GCP_LAMBO_CREDENTIALS / GOOGLE_APPLICATION_CREDENTIALS"*.

**Pin.** `embed::tests::gemini_resolves_the_shared_credential_variable`, three arms under the
env lock: `GCP_LAMBO_CREDENTIALS` alone builds the adapter (the arm that used to refuse),
`GOOGLE_APPLICATION_CREDENTIALS` alone still builds it, and neither set produces a refusal
naming all three. A fix that swapped one hardcoded variable for another passes only one arm.

`embed::tests::gemini_fail_closed_without_credentials` now clears both variables under the
same lock rather than assuming they are unset: the embedder reads one more variable than it
did, so a developer with either exported would otherwise have watched that test build an
embedder instead of refusing, and a sibling test setting one would race it.

**Mutation.** Restore the `GOOGLE_APPLICATION_CREDENTIALS`-only lookup:

```
test embed::tests::gemini_resolves_the_shared_credential_variable ... FAILED
GCP_LAMBO_CREDENTIALS alone must build the embedder, as it builds the store:
embedder unavailable: Gemini embedder needs service-account credentials: ...
```

---

## P2-2. The allowlist script's empty-list guard was dead code. CLOSED.

Under `set -euo pipefail`, `grep -vx` filtering away the last entry exits 1, `pipefail` gives
the pipeline status 1, the assignment inherits it, and `set -e` killed the script before
`if [ -z "$desired" ]` could run. The guard was unreachable in exactly the case it was
written for.

**The fix.** `desired="$(... | paste -sd, - || true)"`. `|| true` applies to the whole
pipeline, so an empty result reaches the guard as an empty string.

**Verified** with a stub `gcloud` on `PATH` (no live GCP call), removing the only entry:

```
instance: mooshik/lambo-pg
current:  1.2.3.4/32
target:   1.2.3.4/32 (remove)
refusing to empty the allowlist: that locks every machine out of the store
exit=1
```

Before the fix this printed the first three lines and then exited 1 in silence. Removing one
of two entries still behaves (`desired: 5.6.7.8/32`, no patch).

---

## P2-3. Nothing offline proved the token becomes the connection password. CLOSED.

The reviewer's mutation 2 (delete `.password(&token)`) left the whole
`--no-default-features --features store-postgres` suite green. That is the one line the
hosted tier rests on.

**The pin.** `store::pg::postgres::tests::the_minted_token_is_the_connection_password_across_a_rotation`
stands up a `TcpListener` on `127.0.0.1:0` that speaks enough PostgreSQL v3 to answer one
question. The sketch in the brief was checked against sqlx 0.8.6 rather than trusted, and
three things came back:

* **The SSLRequest arm is unnecessary but kept.** `sqlx-postgres` 0.8.6
  `connection/tls.rs:25` returns the plain socket for `PgSslMode::Disable`, and the DSN says
  `sslmode=disable`, so no SSLRequest is sent. The helper still recognises the length-8 /
  `Int32(80877103)` form and answers `N`, because two lines are cheaper than this pin turning
  into a hang if a default ever changes.
* **`AuthenticationCleartextPassword` gets the password verbatim.** `establish.rs:74-83`
  answers it with `Password::Cleartext(options.password.as_deref().unwrap_or_default())`.
  md5 or SCRAM would hash the very thing being asserted; cleartext is the point.
* **The refusal must not be retryable, or `acquire()` would spin to its 30s deadline.**
  `pool/inner.rs:333-381` loops on connect errors, but only for `ConnectionRefused` and for
  database errors where `is_transient_in_connect_phase()` holds, which for postgres
  (`sqlx-postgres/src/error.rs:189-201`) is exactly `53300` and `57P03`. The helper answers
  `ErrorResponse` with SQLSTATE **28P01**, so `acquire()` returns at once.

The captured password is pushed to an `mpsc` channel **before** the ErrorResponse is written,
so the assertion never races the client's return. The DSN carries **no** password, so a
captured `tok-*` cannot have come from anywhere else.

Both halves are asserted in one run because they are one property: the OAuth mock hands out
`tok-1` with `expires_in: 62` (a 2s TTL after the 60s refresh margin), the first connection
must present `tok-1`; the mock is then replaced with one handing out `tok-2`, the test sleeps
2300ms, `pool()` rotates, and the replacement pool's connection must present `tok-2`. A
rotation that rebuilt the pool but kept the old password passes the existing cadence pin and
fails this one.

**Mutation.** `let options = Self::connect_options(&self.dsn)?;` (the `.password(&token)`
chain dropped, the reviewer's mutation 2):

```
test store::pg::postgres::tests::the_minted_token_is_the_connection_password_across_a_rotation ... FAILED
assertion `left == right` failed: the minted token must be the connection password
  left: ""
 right: "tok-1"
```

**Flake hunt.** 25 consecutive runs of this test, 0 failures.

---

## P3-1. The record's baseline test counts were wrong. CLOSED.

Measured in a detached worktree at `0602f3b` (`git worktree add --detach`, removed
afterwards), every row run to completion:

| row at `0602f3b` | lib | other binaries | doc-tests | total |
| --- | --- | --- | --- | --- |
| `--features embed-bge,embed-fixture --lib` | 918 | - | - | 918 |
| `--features ship,fixtures` | - | - | - | **does not compile** |
| `--no-default-features --features store-postgres` | **606** | 8 | 2 | 616 |
| `--no-default-features --features store-cockroach` | - | - | - | **does not compile** |
| `--features embed-gemini` | **935** | 11 | 2 | 948 |

The two non-compiling rows are the expected, already-recorded E0433 (§0 of the record).

The record claimed "the gemini row was 940" (it was 935 lib, 948 total) and "the postgres row
was 616 and is 617". That second one is the instructive mistake: 616 is the pre-change
**total** and 617 is the post-change **lib** count, so the paragraph was comparing two
different quantities and reporting the difference as +1 when the lib delta is +11. A baseline
inflated in that direction would have hidden exactly the test loss the paragraph exists to
rule out.

Separately, where the record's gates table reads "N lib + M across the integration binaries",
M is the count from the **first** integration binary only. `cargo test` prints one
`test result:` line per target and summing needs all of them plus `Doc-tests`. Corrected
sums are in `L1-gapfix-implementation.md`.

`L1-gapfix-implementation.md` now carries both corrections inline.

---

## P3-2. Duplicated doc-comment paragraph. CLOSED.

The paragraph beginning *"The IAM token is a password with an expiry"* appeared twice, back
to back, on `the_iam_pool_is_rebuilt_when_its_token_expires` (`src/store/pg/postgres.rs`).
The duplicate is deleted; the surviving copy is byte-identical to the original.

---

## P3-3. README's `lambo.toml` sample was stale. CLOSED.

`ship` now carries `store-postgres` and `embed-gemini`, and `README.md` promises "released
binaries carry the full adapter set". The sample listed neither.

```
kind = "memory"     # memory | sqlite | postgres | cockroach
kind = "fixture"    # fixture | bge_m3 | gemini
```

Both lines now enumerate exactly what `ship` compiles. `lambo.example.toml`'s embedder line
was stale in the other direction (`bge_m3 | bedrock | fixture`, missing two kinds that exist)
and now reads `bge_m3 | gemini | candle | bedrock | fixture`.

---

## P3-4. `LAMBO_POSTGRES_IAM=` (empty) means the password path. CLOSED, kept, and pinned.

The narrowing from `is_some()` to `is_some_and(|v| !v.is_empty())` is **kept**: it matches
this repo's env-overlay convention, where an empty value is absent rather than a value, and
it means a `.env` placeholder cannot turn a password deployment into a fail-closed IAM
refusal. What was wrong is that a security-relevant opt-in able to turn itself off was left
to a reader of `is_some_and`. It is now deliberate: stated in `L-gcp-hosted-postgres.md` and
in `CHANGELOG.md`, and pinned.

**Pin.** `store::pg::postgres::tests::an_empty_iam_opt_in_means_the_password_path`. Both
halves: `LAMBO_POSTGRES_IAM=""` with no credential yields a working lazy pool (the password
path, no credential demanded); `LAMBO_POSTGRES_IAM="1"` with no credential yields the named
refusal. A helper `with_iam_env_value` was extracted from `with_iam_env` so the test controls
the opt-in's **value**, not merely whether it is set.

**Mutation.** Restore `is_some()`:

```
test store::pg::postgres::tests::an_empty_iam_opt_in_means_the_password_path ... FAILED
an empty opt-in must take the password path, not demand a credential:
Backend("PostgresStore IAM auth setup: LAMBO_POSTGRES_IAM is set but
GCP_LAMBO_CREDENTIALS / GOOGLE_APPLICATION_CREDENTIALS is unset")
```

---

## P3-5. The record understated which CI rows `0602f3b` broke. CLOSED.

Verified in the same detached worktree at `0602f3b`. Every build carrying `store-cockroach`
**without** `store-postgres` hit the same E0433, and `ship` at that commit was exactly that
shape:

| command | CI row | exit at `0602f3b` |
| --- | --- | --- |
| `cargo check --no-default-features --features store-cockroach` | `cockroach` | 101 |
| `cargo test --no-default-features --features store-cockroach` | `cockroach` | 101 |
| `cargo check --features demo` | `demo` | 101 |
| `cargo check --all-targets --features ship,fixtures` | `ship-fixtures` | 101 |
| `cargo test --features ship,fixtures` | `ship-fixtures` | 101 |
| `cargo build --release --features ship` | release job (`FEATURES: ship`) | 101 |

The release-job row is run as the workflow runs it, with default features on
(`release.yml:101` is `cargo build --release --features "$FEATURES" --target <triple>`, and
`FEATURES: ship`); defaults do not include `store-postgres`, so the shape holds. Three CI
rows and the release job, not one row. `L1-gapfix-implementation.md` §0 now says so, with the
reason it matters: "one lint row" reads as a gap, "the release job" reads as a shipping stop.

---

## P3-6. Allowlist script nits. CLOSED.

All four, each exercised against a stub `gcloud` on `PATH`:

| nit | fix | evidence |
| --- | --- | --- |
| `grep -qx` / `grep -vx` read the CIDR as a **regex**, so `.` matches any character | `-qxF` / `-vxF` | `--remove --ip 1.2.3.4` against a list holding `1X2.3.4/32` now prints *not on the list; nothing to do* instead of removing an entry the operator never named |
| `--ip` (and `--instance`, `--project`) with no value set the variable empty, shifted twice, and died from the second `shift` with no message | a `need_value` helper checked before the assignment | `--remove --ip` prints `--ip needs a value`, exit 2 |
| `--help` printed `sed -n '2,32p'`, which included `set -euo pipefail` and a blank line | `awk 'NR > 1 && /^#/ { print; next } NR > 1 { exit }'`, which prints the header block however long it grows | 29 lines, 0 occurrences of `set -euo` |
| `--list` on an empty allowlist printed a bare indented blank line | an explicit branch | `  (none: the allowlist is empty, so no machine can reach the instance)` |

The `--help` fix deliberately drops the hardcoded line range rather than correcting it: a
stale range is the defect, and this one had already gone stale.

Not fixed, and named rather than hidden: the add and remove paths are still a
read-modify-write of the whole list, so two machines running the script simultaneously can
lose one of the two additions. The reviewer raised it as worth knowing rather than as a
finding, and the honest answer is that fixing it needs an ETag/optimistic-concurrency path
`gcloud sql instances patch` does not offer.

---

## Gates

Every row re-run on the remediated tree, cachyos Linux box. `lib` / `other` / `doc` are the
per-target sums (`cargo test` prints one `test result:` line per target; `other` is every
non-lib binary, `doc` is `Doc-tests`).

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean (exit 0) |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | clean |
| `cargo clippy --all-targets --no-default-features --features store-postgres -- -D warnings` | clean |
| `cargo clippy --all-targets --no-default-features --features store-postgres,store-sqlite,fixtures -- -D warnings` | clean |
| `cargo test --features embed-bge,embed-fixture --lib` | 918 lib, 0 failed, 1 ignored |
| `cargo test --features ship,fixtures` | 1130 lib + 38 other + 2 doc = **1170 passed**, 0 failed, 20 ignored |
| `cargo test --no-default-features --features store-postgres` | 620 lib + 8 other + 2 doc = **630 passed**, 0 failed, 5 ignored |
| `cargo test --no-default-features --features store-cockroach` | 619 lib + 8 other + 2 doc = **629 passed**, 0 failed, 0 ignored |
| `cargo test --features embed-gemini` | 943 lib + 11 other + 2 doc = **956 passed**, 0 failed, 4 ignored |
| `cargo check --no-default-features` | clean |
| `cargo check --features ship,embed-gemini` | clean |
| `cargo test --features ship,fixtures --test binary_parity` | 4 passed, 0 failed |
| live: `embed::gemini::tests::gemini_live_embeds_against_vertex` | **1 passed** (real Vertex round trip) |

**Drift against `9580873`**, using the round-1 review's independently measured lib counts as
the comparison point, since this tree has moved past that commit:

| row (lib count) | `9580873` | here | delta | which tests |
| --- | --- | --- | --- | --- |
| `embed-bge,embed-fixture` | 918 | 918 | 0 | the new pins are all behind postgres/gemini |
| `ship,fixtures` | 1126 | 1130 | **+4** | all four new pins compile here |
| `store-postgres` | 617 | 620 | **+3** | scope-on-refresh, wire password, empty opt-in |
| `store-cockroach` | 619 | 619 | 0 | `gcp_auth` is not in this build |
| `embed-gemini` | 941 | 943 | **+2** | scope-on-refresh, shared credential variable |

No row lost a test and no row's failure or ignore count moved except by the four additions.

---

## Files touched

`src/gcp_auth.rs`, `src/embed/mod.rs`, `src/store/pg/postgres.rs`,
`scripts/cloudsql-allowlist.sh`, `README.md`, `lambo.example.toml`, `CHANGELOG.md`,
`dev-diary/lambo-for-mooshik/L-gcp-hosted-postgres.md`,
`dev-diary/lambo-for-mooshik/l-run/L1-gapfix-implementation.md`, and this file.
`src/store/pg/mod.rs` is **unchanged**: every mutation against it was reverted.

## Still open, deliberately

- The **live** Cloud SQL leg (`iam_auth_connects_as_service_account`) has still not been
  re-run on this tree. It is now the only thing the store's IAM path has no proof of that a
  reasonable person would still want: the token-is-the-password property and the rotation
  both have offline proofs as of P2-3, and the scope ask has a measured proof against the
  real token endpoint, but nothing here has authenticated to the actual instance since
  `0602f3b`.
- Public IP plus allowlist stays dev-grade. Private IP plus the Cloud SQL Auth Proxy is the
  production answer, and the proxy handles token rollover itself.
- The allowlist script's read-modify-write race (P3-6, last row) is documented, not fixed.
- The second machine still has to run the allowlist script once; nothing here can do that
  for it.

- L1 remediation, 2026-08-25
