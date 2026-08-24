# Adversarial review - mooshik A (shared service account for Cloud SQL IAM), round 1

**Reviewer**: independent adversarial reviewer, agent_id `SharedSAReview`. Wrote nothing
under review except this file.
**Scope**: the uncommitted working-tree diff over committed HEAD `e37feff` implementing the
shared-SA path in `store-postgres` - `src/gcp_auth.rs` (new), `src/store/pg/mod.rs`
(`PgStore::pool()` IAM branch), `src/store/pg/postgres.rs` (live IAM test),
`src/lib.rs` (mod decl), `Cargo.toml` (feature additivity), and the ADR
`dev-diary/lambo-for-mooshik/L-gcp-hosted-postgres.md`.
**Worktree**: `/home/nryn/work/lambo` @ `e37feff` + the diff above (verified via
`git status --short` / `git diff` before starting).
**Verdict**: **APPROVE** - no P1, no P2, two P3 nits. The change is correct, honest in
scope, feature-additive, and passes every gate.

## Method

1. Read the full diff (`Cargo.toml`, `src/lib.rs`, `src/store/pg/mod.rs`,
   `src/store/pg/postgres.rs`), then the complete new `src/gcp_auth.rs`
   (154 lines, all read), the `pool()` / `connect_options()` context in
   `src/store/pg/mod.rs`, the new live test, and the ADR.
2. Verified each requirement from the brief at the source (details below).
3. Re-ran all six gates myself on the pristine tree (results in Part C).
4. Hunted for regressions and overclaiming in the doc and the new code.

## Part A - per-requirement verification

**1. `src/gcp_auth.rs` (new).** `ServiceAccountKey` (`{client_email, private_key,
token_uri?}`) parses the standard service-account JSON (`ClientEmail`/`PrivateKey`
mirror the embedder's shape). JWT is RS256: `jsonwebtoken::Header::new(RS256)` with
`iss = client_email`, `aud = token_uri`, `scope = cloud-platform + sqlservice.login`,
`iat`, `exp = now + 3600` (`gcp_auth.rs:94-101`), signed via
`EncodingKey::from_rsa_pem(private_key)` (`:102`). JWT-bearer exchange POSTs
`grant_type=urn:ietf:params:oauth:grant-type:jwt-bearer` + `assertion` to
`token_uri` form-encoded (`:107-117`), non-2xx surfaces the status+body as a named
error (`:123-125`), and `access_token` is parsed from the JSON response (`:126-130`).
Caching: cached token is returned only while
`Instant::now() < expires_at`, and the cache margin is `expires_in - 60s`
(`TOKEN_CACHE_MARGIN`, saturated, min 1 s) (`:84-88, 131-138`).
**Mutation-check**: `access_token` takes `&mut self` and writes `self.cached`
(`:138`) - the mutation that makes caching observable is captured; nothing is cached
when the OAuth exchange fails. `from_env` reads `GCP_LAMBO_CREDENTIALS` then
`GOOGLE_APPLICATION_CREDENTIALS` and returns `None` (not an error) when unset, so a
non-IAM deployment is untouched (`:51-55`). All four brief sub-points hold.

**2. `pg/mod.rs` `pool()` IAM branch (`:1342-1371`).** Gated on
`std::env::var_os("LAMBO_POSTGRES_IAM").is_some()` (`:1350`). When set, it builds
`CloudSqlTokenSource::from_env()`, maps a missing credential file to a clear error
("LAMBO_POSTGRES_IAM is set but GCP_LAMBO_CREDENTIALS / GOOGLE_APPLICATION_CREDENTIALS
is unset"), wraps any setup/token error in `backend(...)` with the dialect name
(prefixed `ServiceAccountKey`/`IAM token`, `:1357-1363`), mints the token via
`src.access_token().await`, and injects it with `options.password(&token)` (`:1364`).
It then returns `PgPoolOptions::new().max_connections(..).connect_lazy_with(options)`
(`:1366-1368`) - lazy, so no network I/O at pool build; the token is the per-connection
password. Errors are wrapped in `backend`.
**Mint-once constraint**: `pool()` runs inside `self.pool.get_or_try_init(|| async {..})`
(`:1343-1344`), so the closure (and thus the token mint) executes at most once per
store; the token source additionally caches for ~59 min, and all connections from the
pool share the baked-in password. The "minted once at pool creation" constraint is
explicitly stated in the code comment (`:1348-1349`) and the ADR (`:96-100`). Honest
and correct.
**No-static-password DSN shape**: `connect_options` parses the DSN then applies
session options; `options.password(&token)` overrides any DSN password, so the IAM
DSN carries the user with an empty password and cloud SQL IAM reads the token as the
password. Sound.

**3. Feature additivity.** `Cargo.toml`:
`store-postgres = ["dep:sqlx", "sqlx/postgres", "dep:reqwest", "dep:jsonwebtoken"]`.
Both `reqwest` and `jsonwebtoken` are `optional = true` and gated behind this feature;
`default = ["store-memory", "embed-bge", "embed-fixture"]` does not include
`store-postgres`, and `lib.rs` gates `pub mod gcp_auth` on `#[cfg(feature =
"store-postgres")]`, so none of it compiles into default builds. `jsonwebtoken`
leaks into no feature except `store-postgres` / `embed-gemini`; `reqwest` was already
reachable via `embed-bge`. No branch of `pool()` references `gcp_auth` outside the
feature-gated store module. Confirmed empirically: default check and embed-gemini
clippy both pass (Part C).

**4. Live test `iam_auth_connects_as_service_account`.** `#[ignore = "live: ..."]`
so it is skipped in normal runs. Inside, it short-circuits via
`postgres_dsn_or_skip` (returns early when `LAMBO_POSTGRES_DSN` is unset) and an
explicit early return when `LAMBO_POSTGRES_IAM` is unset, so it skips cleanly with
only a `#![ignore]` + env check, no hard failure when unset. It constructs a real
`PostgresStore`, calls `store.pool().await`, and runs a read-only
`SELECT current_user`, asserting the result contains `'@'` (the IAM SA DB username
form, e.g. `cachy-nryn@mooshik.iam`). Mutation-sound: the assertion fails if the
store falls back to a non-IAM login without an `@`; it is read-only and creates no
database/rows.

**5. Doc honesty (`L-gcp-hosted-postgres.md`).** The 1h token-mint-once constraint
(`:96-100`) matches the code exactly (pool-creation mint, ~1h valid, cache-refresh
follow-up, Auth Proxy for rollover). The non-superuser IAM limitation (`:101-104`)
is accurate and correctly scoped (SA is a `CLOUD_IAM_SERVICE_ACCOUNT` login; DDL
runs as root). The `gcp_auth` / `embed/gemini.rs` auth-dedup follow-up (`:107-109`)
is stated honestly as a follow-up, not a blocker, and the shared identity really is
the single credential file. The "Auth Proxy" wording (`gcp_auth.rs:9`,
ADR `:98, 105-106`) is accurate - the Cloud SQL Auth Proxy accepts the IAM token as
the connection password and handles rollover; it is not presented as shipped code.
Cost, reproduce, and infra tables are concrete and not overclaiming. The ADR's token
claim "caching until expires_in - 60s" matches `TOKEN_CACHE_MARGIN`. No overclaim
found.

## Part B - findings

**P1**: none.

**P2**: none.

**P3-1 (dead abstraction, gcp_auth.rs:145-154).** `pub trait DbTokenSource` +
its impl are never consumed anywhere in `src/` (grep: only the definition and impl
in `gcp_auth.rs`); `pool()` calls `CloudSqlTokenSource::access_token` directly. The
doc-comment says "so tests can inject a stub", but no stub or trait-typed caller
exists. It is `pub` so it escapes the dead-code lint. Minor scope noise - either
wire `pool()` to hold a `Box<dyn DbTokenSource>` and add the stub test, or delete the
trait for now.

**P3-2 (doc phrase).** ADR `:92` shows the live proof line
"IAM authenticated to Postgres as: cachy-nryn@mooshik.iam". The live test only
asserts `current_user` contains `'@'` and prints whatever it is; the doc's concrete
username is the observed value from the author's run (plausible and consistent with
the infra, `cachy-nryn@mooshik.iam` being the documented SA). Not an overclaim, but a
future reader could read it as an invariant. Optional: soften to "<the IAM SA user>".
Not blocking.

## Part C - gate re-runs (pristine tree)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | PASS (exit 0, no diffs) |
| `cargo clippy --all-targets --features store-postgres,fixtures -- -D warnings` | PASS (0 warnings) |
| `cargo check --features store-postgres` | PASS |
| `cargo check` (default) | PASS |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | PASS (0 warnings) |
| `cargo test --features store-postgres --lib store::pg::` | 15 passed, 0 failed, 5 ignored |

The live IAM test was not run (env not set in this shell, as permitted); its
skippability was verified by inspection and by its `/* ignored */` report in the
lib-suite run above.

## Conclusion

APPROVE. Correctness verified at the source (JWT mint, jwt-bearer exchange, cache
with 60 s margin, correct scopes, `&mut self` mutation captured), pool integration is
lazy and error-wrapped with the mint-once constraint stated, feature additivity holds
no leak into default/embed-gemini builds (both compile), the live test is genuinely
gated and mutation-sound, and every gate passes. Two P3 nits only.

- SharedSAReview, 2026-08-24
