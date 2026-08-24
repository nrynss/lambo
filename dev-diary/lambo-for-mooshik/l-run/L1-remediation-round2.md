# L1 remediation, round 2 (2026-08-25)

Round 2 of adversarial review (`adve-review-mooshik-L1-round2.md`) returned **APPROVE** with
zero P1 and zero P2, verifying all ten round-1 closures by mutation, and raised three P3s.
The ruling here was to close all three rather than carry them: each is a few lines, and two of
them are about documents currently claiming more than the code proves, which is the exact
failure round 1 already caught once.

## L1-R2-1: neither consumer's scope was pinned

**The finding.** Swapping the store to Vertex's cloud-platform-only scope left the whole
postgres row green (620 passed), and swapping the embedder to the database login scope left
the gemini row green. Scope was a string typed at two call sites with nothing asserting
either. Worse after round 1 than before it, because the refresh grant now sends the scope as
a narrowing request, so a wrong scope becomes a live failure instead of an inert argument.

**The fix, which removes the defect rather than testing around it.** Callers no longer spell a
scope. `GoogleOAuthTokenSource::for_vertex` and `::for_cloud_sql` name the **consumer**, and
the scope is theirs to know. `GoogleOAuthTokenSource::new(creds, client, scope)` remains for
tests and for a consumer neither constructor describes.

Pins, and what each one catches:

| Pin | Catches |
| --- | --- |
| `gcp_auth::tests::vertex_asks_for_cloud_platform_only` | a scope edited inside `for_vertex` |
| `gcp_auth::tests::a_cloud_sql_login_asks_for_the_database_login_scope` | a scope edited inside `for_cloud_sql` |
| `store::pg::postgres::tests::the_minted_token_is_the_connection_password_across_a_rotation` | the **store** calling the wrong constructor |

The third one needed work. On the service-account grant the scope travels inside the signed
assertion, not as a form field, so a substring match on the request body would have passed on
anything. The mock now matches with `assertion_asks_for_the_sql_login_scope`, which pulls the
`assertion` out of the form body, opens it with the test key pair, and reads the `scope`
claim. A store that asked for Vertex's scope matches no mock and dies on the 404.

**Mutations.** `for_cloud_sql` to `for_vertex` at the store's call site turns the wire pin red.
Swapping the two constructors' scope constants turns `vertex_asks_for_cloud_platform_only` red.

## L1-R2-2: precedence was documented but not asserted

**The finding.** The new three-arm pin never set both variables at once, so reversing the two
arms of `credentials_path_from_env` left every row green, as did demoting
`cfg.gemini_credentials` below the environment. Three documents sell that precedence as the
reason `GCP_LAMBO_CREDENTIALS` exists.

**The fix.** `gcp_auth::tests::the_shared_variable_wins_but_an_empty_one_does_not_shadow`
sets both and asserts the winner. A fourth arm in
`embed::tests::gemini_resolves_the_shared_credential_variable` points both variables at files
that do not exist and the config key at a real one, so an embedder that consulted the
environment first cannot build.

**Mutations.** Reversing the two arms turns the first pin red. Demoting `gemini_credentials`
below the environment turns the fourth arm red.

## L1-R2-3: an empty `GCP_LAMBO_CREDENTIALS` shadowed a working ADC

**The finding.** `credentials_path_from_env` used `var_os` alone, so
`GCP_LAMBO_CREDENTIALS=` left in a shell profile shadowed a perfectly good
`GOOGLE_APPLICATION_CREDENTIALS` and produced a refusal naming an empty path. That contradicts
the convention `Config::overlay_env` applies everywhere else, and the one round 1 had just
closed P3-4 to establish for `LAMBO_POSTGRES_IAM`.

**The fix.** `non_empty_env` treats empty as unset for both variables. Pinned by the same
`the_shared_variable_wins_but_an_empty_one_does_not_shadow` (its third and fourth
assertions); removing the `.filter(|v| !v.is_empty())` turns it red.

## Gates

Run on the remediated tree, on the cachyos Linux box.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | clean |
| `cargo clippy --all-targets --no-default-features --features store-postgres -- -D warnings` | clean |
| `cargo clippy --all-targets --no-default-features --features store-postgres,store-sqlite,fixtures -- -D warnings` | clean |
| `cargo test --features embed-bge,embed-fixture --lib` | 918 passed / 0 failed / 1 ignored |
| `cargo test --features ship,fixtures` | 1132 lib passed / 0 failed / 18 ignored |
| `cargo test --no-default-features --features store-postgres` | 623 passed / 0 failed / 5 ignored |
| `cargo test --no-default-features --features store-cockroach` | 619 passed / 0 failed |
| `cargo test --features embed-gemini` | 945 passed / 0 failed / 2 ignored |
| `cargo check --no-default-features` | clean |

Lib deltas against round 1: postgres 620 to 623 (+3), gemini 943 to 945 (+3 added, 1 deleted),
ship 1130 to 1132, default 918 unchanged. Three new pins (`vertex_asks_for_cloud_platform_only`,
`a_cloud_sql_login_asks_for_the_database_login_scope`,
`the_shared_variable_wins_but_an_empty_one_does_not_shadow`), one existing pin extended with a
fourth arm, and one deleted: `embed::gemini::tests::vertex_asks_for_the_cloud_platform_scope_only`
asserted a constant against its own definition, and the constant it guarded
(`gemini::OAUTH_SCOPE`) had no callers left once `for_vertex` replaced them. Both are gone; the
claim they were reaching for is now `gcp_auth::tests::vertex_asks_for_cloud_platform_only`,
which pins the constructor every caller actually goes through.

## Residue

None. Every round-1 and round-2 finding is closed and mutation-proven.

**The live Cloud SQL IAM leg was re-run on this tree** and passes, which closes the residue the
round-1 record carried:

```
GCP_LAMBO_CREDENTIALS=~/.config/mooshik-8f7bb8506bc4.json LAMBO_POSTGRES_IAM=1 \
LAMBO_POSTGRES_DSN='postgresql://cachy-nryn%40mooshik.iam@136.64.220.174:5432/lambo?sslmode=require' \
  cargo test --no-default-features --features store-postgres --lib -- \
  iam_auth_connects_as_service_account --ignored --nocapture

IAM authenticated to Postgres as: cachy-nryn@mooshik.iam
test result: ok. 1 passed; 0 failed
```

So the whole path is live-proven end to end after the consolidation and the scope change: a
token minted by the shared module, carrying `sqlservice.login`, accepted by Cloud SQL as the
connection password for the service account. Live Vertex was re-run in round 1 and passed.

Two things stay unverified for environmental reasons rather than by choice. **Rotation has an
offline proof and no live one**: proving it against the instance means waiting out a real
token hour, since Google will not mint a short-lived one on request. And `postgres-live` has
not run on a real GitHub runner. Round 2's own limit is recorded in its file: it could not
independently reproduce the live tokeninfo measurement, because reading the ADC file was
refused in its sandbox.
