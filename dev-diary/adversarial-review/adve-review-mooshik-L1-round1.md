# Adversarial review - mooshik L1 (hosted-tier gap fixes), round 1

**Reviewer**: independent adversarial reviewer, agent_id `L1Review1`. Wrote nothing under
review except this file.
**Scope**: commit `9580873` ("fix(l1): close the hosted tier's four gaps, and the CI row it
broke") against baseline `0602f3b` on `lambo-for-mooshik`. 15 files, +1413/-593.
**Worktree**: `/home/nryn/work/lambo` @ `9580873`, verified clean with `git status --short`
before starting and again after every mutation cycle.
**Verdict**: **REQUEST_CHANGES** - 1 P1, 3 P2, 6 P3. The rotation fix, the dialect gate and
the `ship` change are all real and all correctly pinned. The auth consolidation is where the
change claims more than it delivers.

## Method

1. Read `git show 9580873` and the full `git diff 0602f3b..9580873`, then the complete
   post-change `src/gcp_auth.rs` (652 lines), `src/embed/gemini.rs` (527 lines), the
   `pool()` / `iam_pool()` / `connect_options()` region of `src/store/pg/mod.rs`, the new
   tests in `src/store/pg/postgres.rs` and `src/store/pg/cockroach.rs`,
   `scripts/cloudsql-allowlist.sh`, `Cargo.toml`, `.github/workflows/{ci,release}.yml`,
   and the CHANGELOG entries.
2. Read the implementation record `dev-diary/lambo-for-mooshik/l-run/L1-gapfix-implementation.md`,
   `L-gcp-hosted-postgres.md`, `dev-diary/lambo-for-mooshik/README.md`, and the round-1
   review of the amended commit (`adve-review-mooshik-A-sharedsa-round1.md`). Every claim in
   the record was treated as a hypothesis, not evidence.
3. Read the **pre-change** `src/embed/gemini.rs` and `src/gcp_auth.rs` at `0602f3b` and
   diffed the error classification path by path, and the test list name by name.
4. Read sqlx-core 0.8.6 `pool/{inner,connection}.rs` at the source to check the "dropped,
   not closed" claim rather than take it on trust.
5. **Four source mutations**, each reverted, each verified by `git status --short` returning
   empty afterwards. Results in Part B.
6. Re-ran all twelve required gates myself, plus the baseline test counts in a detached
   worktree at `0602f3b`, plus a 30-run flake hunt and a CPU-loaded stress of the new
   rotation pin.
7. Exercised `scripts/cloudsql-allowlist.sh` against a stub `gcloud` on PATH (five cases,
   no live GCP call).

## Part A - the five claims

### Claim 1: IAM pool rotation. HOLDS, with one coverage hole (P2-3).

`iam_pool()` (`src/store/pg/mod.rs:1432-1474`) takes a `tokio::sync::Mutex`, lazily builds
the token source, returns the cached pool while `Instant::now() < expires_at`, and otherwise
mints a fresh token and builds a replacement lazy pool. The mechanism is sound:

- **No double-mint, no double-build.** The mutex is held across the whole read-check-mint-swap
  sequence, so two concurrent callers serialise; the second sees `state.live` already fresh.
  The cache-hit path takes no `.await` inside the critical section.
- **No deadlock, no lost wakeup.** `iam_pool` never re-enters `pool()`, so there is no
  re-entrancy; `tokio::sync::Mutex` is fair-queued, and cancellation mid-mint drops the guard
  and leaves both `state.live` and `source.cached` untouched, so the next caller re-mints.
- **Expiry consistency.** `state.live`'s deadline is the token source's own deadline
  (`access_token_with_expiry` returns the same `Instant` it caches). At the moment `iam_pool`
  decides to rotate, the source's own `Instant::now() < *expires_at` is false too, so it
  cannot hand back the stale token for the new pool.
- **"Dropped, not closed" is accurate.** Checked in sqlx-core 0.8.6: `PoolConnection` holds an
  `Arc<PoolInner>` (`pool/connection.rs:24`), and `PoolInner::drop` is the only place
  `mark_closed()` runs on this path (`pool/inner.rs:436-444`). So an in-flight query keeps the
  superseded `PoolInner` alive and its connection usable; when the last handle and the last
  checked-out connection go, `PoolInner` drops and its idle queue drops with it. No unbounded
  leak: each rotation abandons at most `MAX_POOL_CONNECTIONS = 4` connections and they are
  reclaimed as soon as the last transient clone is dropped.
- **The `&self.pool().await?` pattern is correct at every call site.** All 23 production sites
  in `src/store/pg/mod.rs` and both dialect files bind `let pool = &self.pool().await?;`; the
  temporary is lifetime-extended by the `let` initialiser, which is why it compiles at all.
  No call site drops the pool earlier than before: on the non-IAM path the `OnceCell` still
  owns it, on the IAM path `state.live` does, and the clone is a refcount. The `async move`
  closures inside `flush` / `load_session` / `record_canonization` capture `&PgPool` by copy,
  exactly as they did when `pool()` returned a borrow.
- **A caller holding a clone of a superseded pool** keeps working on it: it is a live pool
  with the old token baked into its connect options, so already-open connections serve it and
  a new connection it opens will use the old token. That window is bounded by the 60s refresh
  margin, so the old token is still valid for the whole life of any such transient handle.

The one behavioural consequence worth naming and not a defect: on the IAM path every store
operation now takes a process-global async mutex. The critical section is a compare and a
refcount bump except at rotation, where all callers block on one token mint (bounded by the
60s request timeout). The pre-change code serialised the same way through
`OnceCell::get_or_try_init`, so this is not a regression.

Where it falls short is Part B, mutation 2: the pin proves the mint **cadence** and nothing
about the minted token reaching the connection. See P2-3.

### Claim 2: auth consolidation. Classification preserved, no tests lost, but the scope contract is false on one grant (P1-1) and the "one credential file" claim is false for the embedder (P2-1).

**Classification is preserved one for one.** I diffed the pre-change `embed/gemini.rs` against
the post-change `gcp_auth.rs` path by path. Every arm maps identically, message text included:

| path | pre-change `EmbedError` | post-change `GoogleAuthError` -> `EmbedError` |
| --- | --- | --- |
| credential file unreadable | Unavailable | Unavailable |
| credential JSON malformed | Unavailable | Unavailable |
| service-account JSON malformed / fields empty | Unavailable | Unavailable |
| authorized-user JSON malformed / fields empty | Unavailable | Unavailable |
| unsupported `type` | Unavailable | Unavailable |
| `build_client` failure | Unavailable | Unavailable |
| `mint_jwt` on non-service-account | Unavailable | Unavailable |
| clock before epoch | Unavailable | Unavailable |
| private key PEM will not parse | Backend | Backend |
| JWT signing failure | Backend | Backend |
| token endpoint transport failure | Unavailable | Unavailable |
| token endpoint unparseable JSON | Backend | Backend |
| token endpoint non-2xx | Backend | Backend |
| response missing `access_token` | Backend | Backend |

`impl From<GoogleAuthError> for EmbedError` (`src/embed/gemini.rs:65-70`) is the only bridge,
and `build_gemini_embedder` reaches it through `?` on `load_credentials`, `build_client` and
`GoogleOAuthTokenSource::new` alike. `DEFAULT_TOKEN_TTL_SECS = 3600` and
`TOKEN_CACHE_MARGIN = 60s` reproduce the pre-change literals.

**No test coverage was lost.** Name by name, pre-change `embed::gemini::tests` held 15 tests.
Ten stayed in `embed::gemini::tests`; five moved verbatim into `gcp_auth::tests`
(`mints_and_verifies_service_account_jwt`, `exchanges_jwt_for_access_token_and_caches`,
`token_endpoint_transport_failure_is_unavailable`, `token_endpoint_http_error_is_backend`,
`authorized_user_uses_refresh_token_grant_and_caches`). Two are new in gemini
(`auth_error_classification_is_preserved`, `vertex_asks_for_the_cloud_platform_scope_only`)
and four are new in `gcp_auth` (`the_callers_scope_is_the_one_signed`,
`expiry_is_reported_and_carries_the_refresh_margin`,
`an_authorized_user_adc_file_loads_for_the_cloud_sql_path`,
`an_unknown_credential_type_is_named`). 15 -> 21, nothing dropped. Measured delta on the
gemini row confirms it: 935 lib tests at `0602f3b`, 941 here, exactly +6.

**`SCOPES_CLOUD_SQL_LOGIN` is well formed.** The `\`-continued literal collapses to exactly
two space-separated scopes; I compiled the constant standalone to confirm
(`split_whitespace().count() == 2`), because a stray run of spaces there would be a silently
wrong `scope` claim in every JWT.

**Where it fails**: the scope is threaded correctly for the service-account grant only, and
the "one credential file" claim does not hold for the embedder. P1-1 and P2-1.

### Claim 3: dialect gate. HOLDS.

`Dialect::SUPPORTS_CLOUD_SQL_IAM_AUTH` defaults to `false` (`src/store/pg/dialect.rs:114`) and
is `true` only for `PostgresDialect` (`src/store/pg/postgres.rs:131`). The default is the
fail-closed direction, which is the right shape for a defaulted associated const: a future
dialect that inherits it gets "no Cloud SQL IAM", which is a missing capability, never a
misdirected credential. `cockroach_ignores_the_cloud_sql_iam_opt_in` additionally pins the
value with a `const { assert!(!CockroachDialect::SUPPORTS_CLOUD_SQL_IAM_AUTH) }`, so a future
dialect flipping Cockroach's answer is caught at compile time. Mutation-verified (Part B,
mutation 3) with exactly the message the record claims.

The one residual: nothing forces a *new* dialect's author to make the decision, since the
default answers for them. Given the default is the safe answer, I did not raise this.

### Claim 4: opt-in read at construction. HOLDS, with one undocumented narrowing (P3-4).

The IAM opt-in and credential path move from `pool()` (async, first use) into `PgStore::new`
(sync, `src/store/pg/mod.rs:1293-1296`). I checked the three ways that could bite:

- **Nothing sets the env after building the store.** `LAMBO_POSTGRES_IAM` has exactly one
  reader in the crate (`iam_auth_requested`, `src/store/pg/mod.rs:109`), and every production
  construction path is `build_store` / `build_store_with_vector_dim` / `store_from_env` at
  process start, all of which construct after config resolution and never mutate the
  environment.
- **The live test still works.** `iam_auth_connects_as_service_account`
  (`src/store/pg/postgres.rs:882`) reads `LAMBO_POSTGRES_IAM` for its own skip check *before*
  constructing, then constructs, then calls `pool()`. Construction-time capture is exactly
  what it needs.
- **Parallelism is safe.** `crate::test_util::env_lock()` is a plain global mutex, so it only
  serialises tests that also take it. The exposure window is the microseconds of synchronous
  construction inside `with_iam_env` / the cockroach helper. I enumerated the offline tests
  that construct a `PgStore`: `recall_sql_pairs_cosine_operator_with_text_and_vector_casts`,
  `postgres_store_new_constructs_with_a_dsn`, `with_forced_exact_scan_is_off_by_default`,
  `postgres_build_behavior`, `postgres_copies_embedder_width_when_pin_is_absent` and the H3
  harness. None of them calls `pool()` offline, so a store accidentally constructed inside the
  window is harmless. Both env helpers restore all three variables correctly, including the
  "was unset, stays unset" arm. Empirically: 30 consecutive full runs of the `ship,fixtures`
  lib suite, 0 failures.
- **The 1s TTL in the rotation pin is not a flake.** `expires_in: 61` leaves a 1s live window
  between the first `pool()` and `mint.assert_hits(1)`. I ran that single test 25 times with
  36 busy-loop CPU hogs on a 12-core box: 0 failures. The window is two adjacent statements
  and a mock-server round trip.

### Claim 5: `ship` gained `store-postgres` and `embed-gemini`. HOLDS.

- **Default behaviour is unchanged.** `StoreConfig::default()` is `StoreKind::Memory`
  (`src/store/mod.rs:856-864`) and `EmbedderConfig::default()` is `EmbedderKind::BgeM3`
  (`src/embed/mod.rs:297-316`), neither feature-conditional. There is no auto-detect that
  could now select postgres or gemini. What changes is only that `is_compiled()` is now true
  for both, so the rebuild-hint pre-check in `build_store_with_vector_dim` /
  `build_embedder` stops firing and the real constructor runs. That is the intent.
- **No native toolchain dependency added.** Measured `cargo tree` delta for
  `--no-default-features --features ship`, old set against new: 11 crates, all pure Rust
  (`jsonwebtoken`, `pem`, `simple_asn1`, `num-bigint`, `num-integer`, `num-conv`, `time`,
  `time-core`, `time-macros`, `deranged`, `powerfmt`). `ring` was already in the tree via
  rustls, so `jsonwebtoken` adds no new C/asm build. `store-postgres` adds **zero** crates:
  the sqlx postgres driver and reqwest were both already there. The record's claim is exact.
- **Binary size.** `cargo build --release --bin lambo`, same source tree, old ship feature set
  against new: **26,025,152 -> 26,505,384 bytes, +480,232 (+1.85%)**.
- **The release workflow and its glibc floor step are unaffected.** All four targets
  (`x86_64-unknown-linux-gnu`, `aarch64-unknown-linux-gnu`, `aarch64-apple-darwin`,
  `x86_64-pc-windows-msvc`) support the added crates. The glibc floor step
  (`.github/workflows/release.yml:150-160`) reads `readelf -V` and compares against 2.39; none
  of the 11 added crates links a new libc symbol (all pure Rust, no `cc` build script), and
  `ring` was already linked before.
- **The corrected CI comment is true.** `.github/workflows/ci.yml:199-204` now says `ship`
  gained `store-postgres` "so the ship rows lint it too". Verified: `ship-fixtures`
  (`ci.yml:250-251`) runs `cargo clippy --all-targets --features ship,fixtures -- -D warnings`,
  which now covers store-postgres. The claim that the `postgres` row "keeps the
  NO-default-features lint, the one combination `ship` can never give" is also true: `ship`
  never turns default features off.
- **A side benefit the record does not claim**: the `cockroach_ignores_the_cloud_sql_iam_opt_in`
  pin is `#[cfg(feature = "store-postgres")]`, so it can only run in a build carrying both
  adapters. Before this change no CI row compiled both; now `ship-fixtures` and
  `cargo test --features ship,fixtures` do. Confirmed by `--list`: all 12 new tests are in the
  ship lib suite.

The record's §0 understates the baseline breakage. See P3-5.

## Part B - mutations performed

Every mutation was applied to the working tree, run, then reverted from a byte-for-byte
backup, with `git status --short` confirmed empty after each.

| # | Mutation | Target | Outcome |
| --- | --- | --- | --- |
| 1 | `iam_pool`'s freshness check forced true (`if true \|\| Instant::now() < *expires_at`), i.e. the pre-change build-once behaviour | `store::pg::postgres::tests::the_iam_pool_is_rebuilt_when_its_token_expires` | **FAILED** at `postgres.rs:967` (`mint.assert_hits(2)`). The pin is not vacuous. |
| 2 | `.password(&token)` dropped from the IAM connect options (`let options = Self::connect_options(&self.dsn)?;`) | whole offline suite, `--no-default-features --features store-postgres` | **617 passed / 0 failed.** Nothing caught it. This is P2-3. |
| 3 | `D::SUPPORTS_CLOUD_SQL_IAM_AUTH &&` dropped from the `iam_setup` condition in `PgStore::new` | `store::pg::cockroach::tests::cockroach_ignores_the_cloud_sql_iam_opt_in` | **FAILED** at `cockroach.rs:280` with exactly the message the record predicts: `CockroachStore IAM auth setup: LAMBO_POSTGRES_IAM is set but GCP_LAMBO_CREDENTIALS / GOOGLE_APPLICATION_CREDENTIALS is unset`. |
| 4 | Environment-only, no source change: ran `embed::tests::gemini_fail_closed_without_credentials` twice, once with `GCP_LAMBO_CREDENTIALS` pointing at a valid service-account key and `GOOGLE_APPLICATION_CREDENTIALS` unset, once the other way round | `build_gemini_embedder` | With `GCP_LAMBO_CREDENTIALS`: **passed** (the embedder still refused, i.e. it never read the variable). With `GOOGLE_APPLICATION_CREDENTIALS` on the same file: **FAILED** (the embedder built). This is P2-1. |

Mutation score: 2 of 3 source mutations were caught by the claimed pin. The third
(mutation 2) is an unpinned surface, not a false claim by the record, which does not pin it.

Shell exercise of `scripts/cloudsql-allowlist.sh` against a stub `gcloud` (no live call):

| case | outcome |
| --- | --- |
| `--remove --ip X --dry-run` where X is the only entry | exit 1, **no message**; the "refusing to empty the allowlist" guard never runs. P2-2. |
| `--remove --ip X --dry-run`, two entries | correct: `desired: 5.6.7.8/32`, no patch |
| `--ip` with no value | exit 1, no message. P3-6. |
| add to an empty allowlist, `--dry-run` | correct: `desired: 9.9.9.9/32` |
| `--list` on an empty allowlist | prints a header and a bare indented blank line. P3-6. |

## Part C - gates rerun (my own runs, tree @ `9580873`, pristine)

| Gate | Record's claim | My result |
| --- | --- | --- |
| `cargo fmt --all -- --check` | clean | **pass** (exit 0) |
| `cargo clippy --all-targets -- -D warnings` | clean | **pass** |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | clean | **pass** |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | clean | **pass** |
| `cargo clippy --all-targets --no-default-features --features store-postgres -- -D warnings` | clean | **pass** |
| `cargo clippy --all-targets --no-default-features --features store-postgres,store-sqlite,fixtures -- -D warnings` | clean | **pass** |
| `cargo test --features embed-bge,embed-fixture --lib` | 918 / 0 / 1 ignored | **918 passed / 0 failed / 1 ignored** |
| `cargo test --features ship,fixtures` | 1126 lib + 42, 0 failed, 18 ignored | **1126 lib + 40 integration = 1166 passed / 0 failed / 20 ignored** |
| `cargo test --no-default-features --features store-postgres` | 617 + 6, 0 failed, 5 ignored | **617 lib + 10 integration = 627 passed / 0 failed / 5 ignored** |
| `cargo test --no-default-features --features store-cockroach` | 619 + 6, 0 failed | **619 lib + 10 integration = 629 passed / 0 failed / 0 ignored** |
| `cargo test --features embed-gemini` | 941 + 7, 0 failed, 2 ignored | **941 lib + 13 integration = 954 passed / 0 failed / 4 ignored** |
| `cargo check --no-default-features` | clean | **pass** |

Every headline lib count matches exactly. The integration-binary sums and the ignored totals
do not; see P3-1.

Baseline counts measured in a detached worktree at `0602f3b` (mine, not the record's):

| row | `0602f3b` | `9580873` | delta |
| --- | --- | --- | --- |
| `--features embed-bge,embed-fixture --lib` | 918 | 918 | 0 |
| `--features embed-gemini` (lib) | **935** | 941 | +6 |
| `--no-default-features --features store-postgres` (lib) | **606** | 617 | +11 |

## Part D - findings

### P1-1. The caller's scope is silently ignored on the authorized-user grant, so the one property that makes a shared credential safe is false for the grant type gap 2 exists to support.

`src/gcp_auth.rs:335-345`. The `AuthorizedUser` arm of `access_token_with_expiry` posts
`grant_type=refresh_token`, `client_id`, `client_secret`, `refresh_token` and **nothing else**.
`self.scope` is never sent, never checked, and never reflected in the returned token. It is
consumed only by `mint_jwt`, which is service-account only (`src/gcp_auth.rs:274-284`).

The module doc states the opposite as an unconditional property
(`src/gcp_auth.rs:22-24`): *"The token source takes the scope string rather than assuming one,
so neither consumer silently borrows the other's authority."* The CHANGELOG repeats it
(`CHANGELOG.md:35-37`), and `L-gcp-hosted-postgres.md:68-72` says the module "mints an access
token for the **caller's** scope". On the ADC path both consumers get exactly the same token
with exactly the scopes the refresh token was granted, which is the definition of borrowing
each other's authority.

Neither pin can see it. `the_callers_scope_is_the_one_signed` (`src/gcp_auth.rs:481-501`)
decodes a JWT, so it only exercises the service-account grant.
`authorized_user_uses_refresh_token_grant_and_caches` (`src/gcp_auth.rs:587-606`) asserts the
body contains the four fields and never asserts a scope, because there is none to assert.
`an_authorized_user_adc_file_loads_for_the_cloud_sql_path` (`src/gcp_auth.rs:611-640`) proves
the file parses and that `mint_jwt` refuses politely. None of the three touches what the token
is authorised for.

**Concrete failure scenario.** Cloud SQL IAM database authentication requires the access
token presented as the password to carry `https://www.googleapis.com/auth/sqlservice.login`.
An ADC file written by a plain `gcloud auth application-default login` carries
`cloud-platform`, `openid` and `userinfo.email`, not `sqlservice.login`. On the machine the
record names, whose credential *is* an authorized-user ADC file
(`L1-gapfix-implementation.md:60-64`: "which is what `gcloud auth application-default login`
writes, and what the live Vertex verification actually ran with"), an operator sets
`LAMBO_POSTGRES_IAM=1` and `GCP_LAMBO_CREDENTIALS` to that file. `iam_pool` loads it, mints
via `refresh_token`, drops `SCOPES_CLOUD_SQL_LOGIN` on the floor, and hands Cloud SQL a token
without `sqlservice.login`. The login is refused, surfacing as
`PostgresStore IAM token`-adjacent authentication noise, and gap 2 is not closed on the only
host it was written for. The record concedes the live leg has not been re-run on this tree
("Still open, deliberately"), so nothing in the change contradicts this and nothing verifies
it either.

Note the direction of the regression: the pre-change `embed/gemini.rs` used a hardcoded
`OAUTH_SCOPE` constant and never claimed per-caller scoping, so the *code* is no worse. What
is new is a documented contract the code does not honour, on the exact path the change
advertises as its headline fix.

The correct fix is not "also send `scope`" (Google's refresh-token endpoint returns the
originally granted scopes regardless). It is one of: refuse an `AuthorizedUser` credential
when the requested scope set includes `SCOPE_SQL_LOGIN` and the file cannot be shown to carry
it; verify the minted token via `tokeninfo` once and fail closed with a named error; or state
plainly, in the module doc, the CHANGELOG and `L-gcp-hosted-postgres.md`, that a Cloud SQL IAM
login needs a service-account key or an ADC minted with
`gcloud auth application-default login --scopes=...,sqlservice.login`, and that
`GoogleOAuthTokenSource::new`'s `scope` argument is inert on the ADC grant.

### P2-1. `GCP_LAMBO_CREDENTIALS` is read by the store and not by the embedder, while the new module doc says both read it.

`src/gcp_auth.rs:5-6` states: *"Both read the same credential file (`GCP_LAMBO_CREDENTIALS`,
falling back to `GOOGLE_APPLICATION_CREDENTIALS`)"*. Only the store does.
`credentials_path_from_env()` (`src/gcp_auth.rs:145-149`) implements that fallback chain and
has exactly one caller, `PgStore::new` (`src/store/pg/mod.rs:1295`).
`build_gemini_embedder` (`src/embed/mod.rs:453-465`) still resolves its own path from
`cfg.gemini_credentials` or `std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS")` and has never
heard of `GCP_LAMBO_CREDENTIALS`.

Proven by mutation 4 (environment only, no source change): with `GCP_LAMBO_CREDENTIALS`
pointing at a valid service-account key and `GOOGLE_APPLICATION_CREDENTIALS` unset,
`gemini_fail_closed_without_credentials` passes, meaning the embedder still refused. Point
`GOOGLE_APPLICATION_CREDENTIALS` at the same file and the same test fails, because the
embedder builds.

**Concrete failure scenario.** An operator follows `L-gcp-hosted-postgres.md:84-89`
("Running the shared-SA store path"), which exports `GCP_LAMBO_CREDENTIALS`,
`LAMBO_POSTGRES_IAM` and `LAMBO_POSTGRES_DSN` and then says "the same SA that calls Vertex".
The store authenticates. The embedder refuses to build with *"Gemini embedder needs
service-account credentials: set `gemini_credentials` or GOOGLE_APPLICATION_CREDENTIALS"*, and
the process will not start. The one-identity claim is the entire premise of the consolidation
and the variable the operator doc names does not deliver it.

Fix: have `build_gemini_embedder` fall back to `gcp_auth::credentials_path_from_env()` instead
of its own `GOOGLE_APPLICATION_CREDENTIALS`-only lookup, and update the error text to name
both variables. Or, if the asymmetry is deliberate, correct `src/gcp_auth.rs:5` and the
operator doc.

### P2-2. The allowlist script's "refuses to empty the list" guard is unreachable under `set -euo pipefail`.

`scripts/cloudsql-allowlist.sh:100-104`. Line 100 is
`desired="$(echo "$current" | grep -vx "$CIDR" | sed ... | sort -u | paste -sd, -)"`. When the
address being removed is the only entry, `grep -vx` matches nothing and exits 1. With
`pipefail` the pipeline's status is 1; a simple assignment inherits its command
substitution's status; `set -e` then terminates the script. The `if [ -z "$desired" ]` guard on
line 101 and its message on line 102 are dead code in precisely the case they were written for.

Exercised against a stub `gcloud`: `--remove --ip 1.2.3.4 --dry-run` with `1.2.3.4/32` as the
sole entry prints the `instance:` / `current:` / `target:` lines and then exits 1 with no
further output.

**Concrete failure scenario.** An operator whose home address rotated runs
`scripts/cloudsql-allowlist.sh --remove --ip <old address>` while the old address is the only
entry. The script exits 1 with no diagnostic. The safety outcome survives by accident (no
patch is issued), but the operator is told nothing, and the record and
`L-gcp-hosted-postgres.md:136-137` both advertise "refuses to empty the list" as a property
the script demonstrably never demonstrates.

Fix: `desired="$(... || true)"`, or capture with `if ! desired="$(...)"; then desired=""; fi`,
so the emptiness check actually runs.

### P2-3. Nothing offline proves the minted token becomes the connection password.

`src/store/pg/mod.rs:1469`. Mutation 2: deleting `.password(&token)` from the IAM connect
options leaves the whole `--no-default-features --features store-postgres` suite green
(617 passed / 0 failed). `the_iam_pool_is_rebuilt_when_its_token_expires` counts mints at the
mock OAuth endpoint and never observes what the pool does with the result;
`iam_without_credentials_fails_closed_naming_the_variables` never gets that far.

The only pin for "the token is the password" is `iam_auth_connects_as_service_account`, which
is `#[ignore]`d and, by the record's own "Still open, deliberately" section, has not been run
against Cloud SQL on this tree.

**Concrete failure scenario.** A future refactor of `connect_options` (for example threading
options through a builder, or hoisting the shared `Self::connect_options(&self.dsn)?` call out
of the two branches of `pool()`) drops the `.password(&token)` chain. Every CI row stays green.
The hosted store then connects with whatever password the DSN carries, which for the IAM DSN
in `L-gcp-hosted-postgres.md:88` is none, and every hosted call fails at runtime with a
password-authentication error that the offline suite is structurally unable to predict.

Fix: an offline pin is cheap. `connect_lazy_with` opens nothing, but `pool.acquire()` against a
`TcpListener` bound on `127.0.0.1:0` does: accept one connection, reply
`AuthenticationCleartextPassword`, and assert the client's `PasswordMessage` carries the token
the mock endpoint minted. Alternatively, split the two lines into a
`fn iam_connect_options(dsn, token)` and pin that function's output.

### P3-1. The record's baseline comparison numbers are wrong, which defeats the paragraph's own purpose.

`dev-diary/lambo-for-mooshik/l-run/L1-gapfix-implementation.md:169-172` says "the gemini row
was 940 and is 941" and "the postgres row was 616 and is 617". Measured in a detached worktree
at `0602f3b`: the gemini lib row was **935** (+6, not +1) and the postgres lib row was **606**
(+11, not +1). The stated purpose of that paragraph is to show no test was lost; a baseline
that is 5 and 10 too high would have concealed exactly such a loss.

The same section's integration-binary sums report only the first integration binary rather
than the sum: `ship` is 40 not 42 (and 20 ignored, not 18), `store-postgres` is 10 not 6,
`store-cockroach` is 10 not 6, `embed-gemini` is 13 not 7 (and 4 ignored, not 2). The lib
counts in the gates table are all exact.

### P3-2. Duplicated doc-comment paragraph.

`src/store/pg/postgres.rs:908-919`. The paragraph beginning "The IAM token is a password with
an expiry" and its three following lines appear twice, back to back, on
`the_iam_pool_is_rebuilt_when_its_token_expires`. Harmless, but it is the doc a future reader
uses to decide whether the pin covers their change, and reading the same claim twice invites
skimming past the third paragraph, which is the one that explains the env-lock window.

### P3-3. README's `lambo.toml` sample no longer enumerates what a released binary can select.

`README.md:137` and `README.md:140` still read `kind = "memory" # memory | sqlite | cockroach`
and `kind = "fixture" # fixture | bge_m3`. That was accurate before this commit and is stale
now: `ship` carries `store-postgres` and `embed-gemini`, and `README.md:160` promises
"Released binaries carry the full adapter set, so picking a backend is a config decision".
The change quotes exactly that promise as its justification (`L1-gapfix-implementation.md:133`)
and then leaves the one place in the README that lists the choices behind.
`lambo.example.toml:10` already lists `memory | cockroach | postgres | sqlite`, so the two
files now disagree.

### P3-4. `LAMBO_POSTGRES_IAM=` (set but empty) silently changed meaning.

`src/store/pg/mod.rs:109-111`. The pre-change gate was
`std::env::var_os("LAMBO_POSTGRES_IAM").is_some()`; the new `iam_auth_requested` is
`.is_some_and(|v| !v.is_empty())`. An operator with `export LAMBO_POSTGRES_IAM=` in a profile
or a `.env` placeholder previously got the IAM path (and, without credentials, a loud
fail-closed refusal). They now get the ordinary password path with no message at all. The
narrowing is defensible and matches how `overlay_env` treats empty values elsewhere, but a
security-relevant opt-in silently turning itself off is the one direction that deserves a
sentence, and neither the record, the CHANGELOG, nor the doc comment mentions it.

### P3-5. The record understates which CI rows `0602f3b` broke.

`L1-gapfix-implementation.md:11-14` says `0602f3b` "does not compile under `cargo check
--no-default-features --features store-cockroach`, which is CI's `cockroach` row". Built at
`0602f3b` in a detached worktree, all of the following also fail with the same E0433:

| command | exit at `0602f3b` |
| --- | --- |
| `cargo check --no-default-features --features store-cockroach` (CI `cockroach`) | 101 |
| `cargo check --features demo` (CI `demo`) | 101 |
| `cargo check --all-targets --features ship,fixtures` (CI `ship-fixtures`) | 101 |
| `cargo build --release --no-default-features --features ship` (release workflow, `FEATURES: ship`) | 101 |

Every row carrying `store-cockroach` without `store-postgres` was red, which is three CI rows
and the release job, not one row. The fix here is complete and closes all of them; only the
record's account of the blast radius is narrower than the truth. Worth correcting because
"one row" reads as a lint gap and "the release job" reads as a shipping stop.

### P3-6. Allowlist script nits.

`scripts/cloudsql-allowlist.sh`.

- Lines 90, 96, 100 use `grep -qx` / `grep -vx` with the CIDR as a **regex**, so `.` matches any
  character. For IPv4 CIDRs a false match is not realistic, but `grep -qxF` / `grep -vxF` is
  the correct spelling and costs nothing. The `-v` case is the one that matters: an
  over-matching pattern removes an entry the operator did not name.
- Line 44: `--ip` with no following value sets `IP=""`, shifts once inside the case and once at
  the loop foot, and the second `shift` on an empty argument list returns non-zero, so `set -e`
  exits 1 with no message. Verified. `--instance` and `--project` (lines 45, 46) have the same
  shape.
- Line 47: `-h|--help` prints `sed -n '2,32p' "$0"`, which includes line 31
  (`set -euo pipefail`) and line 32 (blank) in the help text. `2,30p` is the intended range.
- Line 60: `--list` on an instance with no authorized networks prints the header and a single
  indented blank line rather than saying the list is empty.
- Not raised as a finding but worth knowing: the add and remove paths are a read-modify-write
  of the whole list, so two machines running the script at the same time can lose one of the
  two additions. The header documents that names are not preserved; it does not document this.

## Part E - regressions from the amended commit, checked and clear

`adve-review-mooshik-A-sharedsa-round1.md` approved `0602f3b` with two P3s. Both are closed
and nothing it approved was silently regressed:

- **P3-1 (dead `DbTokenSource` abstraction) stayed closed.** Grep over `src/` and `dev-diary/`
  for `DbTokenSource` and `CloudSqlTokenSource`: zero live hits. The only remaining mentions
  are one historical reference in a doc comment (`src/gcp_auth.rs:610`) and the record. The
  whole file was rewritten and the trait did not come back.
- **P3-2 (the concrete SA username read as an invariant) is closed.**
  `L-gcp-hosted-postgres.md:99-101` now says in so many words that the code asserts the login
  is *an* IAM user, not that exact string.
- The named refusal the earlier review verified ("`LAMBO_POSTGRES_IAM` is set but
  `GCP_LAMBO_CREDENTIALS` / `GOOGLE_APPLICATION_CREDENTIALS` is unset") survives verbatim,
  now with the dialect name prefixed, and is pinned by
  `iam_without_credentials_fails_closed_naming_the_variables`.
- `options.password(&token)` overriding any DSN password survives (`src/store/pg/mod.rs:1469`),
  though it is now unpinned offline (P2-3).
- Feature additivity survives: `cargo check --no-default-features` is clean, `jsonwebtoken` and
  `reqwest` remain optional and reachable only through `store-postgres` / `embed-gemini` /
  `embed-bge`, and `gcp_auth` is now gated `any(embed-gemini, store-postgres)` which is
  strictly the correct gate for a module both call.
- `StoreError` classification is unchanged: `iam_pool` wraps every `GoogleAuthError` in
  `StoreError::Backend`, as the pre-change code did. `Backend` is retryable
  (`src/types/mod.rs:989-992`), so a transient token-mint failure still retries rather than
  dead-lettering. No regression.

## Part F - CHANGELOG accuracy

`CHANGELOG.md:31-46`. Four of the five entries are accurate as written. The `gcp_auth` entry
(lines 32-37) carries the same scope overclaim as the module doc and should be corrected with
P1-1. No duplicate entries; `embed-gemini` and `store-postgres` each appear once in the
Unreleased section.

## Verdict

**REQUEST_CHANGES** - 1 P1, 3 P2, 6 P3.

The engineering is good. Rotation is correctly mechanised, correctly serialised, correctly
pinned, and the "dropped, not closed" claim survives reading sqlx's own source. The dialect
gate is the right shape with the right default and a mutation-verified pin. `ship` gains two
adapters for 11 pure-Rust crates and 480 KB, with no behavioural change to a default binary
and a CI comment that is now true. The consolidation genuinely preserves the classification
contract path for path and loses no test.

What blocks it is that the consolidation's headline claim, one credential for two services
with the scope kept separate, is not true of the authorized-user grant. On that grant the
scope argument is inert and unpinned, and the embedder does not read the variable the operator
doc tells you to export. Both are cheap to fix or cheap to document honestly; neither should
ship stated as a property the code has.

- L1Review1, 2026-08-25
