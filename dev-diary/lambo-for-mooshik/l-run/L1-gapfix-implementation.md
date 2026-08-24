# L1 implementation: closing the four gaps the hosted tier landed with (2026-08-24)

Workstreams A (Gemini on Vertex) and B (Postgres/pgvector) landed and were live-verified,
and the shared-service-account commit `0602f3b` wired one GCP identity to both. This change
closes what that left open. Four gaps, plus one red CI row found while closing them.

Baseline: `0602f3b` on `lambo-for-mooshik`.

---

## 0. The red row, found first

`0602f3b` does not compile under `cargo check --no-default-features --features
store-cockroach`, which is CI's `cockroach` row.

`PgStore::pool()` referenced `crate::gcp_auth::CloudSqlTokenSource` unconditionally while
`lib.rs` gated `pub mod gcp_auth` on `#[cfg(feature = "store-postgres")]`, so a build with
the Cockroach dialect and no Postgres feature lost the module and kept the call. Verified by
building a detached worktree at `0602f3b`:

```
error[E0433]: failed to resolve: could not find `gcp_auth` in the crate root
note: found an item that was configured out ... the item is gated behind the
      `store-postgres` feature
```

**Correction, 2026-08-25 (round-1 P3-5). It was not one row, it was three plus the release
job.** Every build carrying `store-cockroach` WITHOUT `store-postgres` hit the same E0433,
and `ship` at `0602f3b` was exactly that shape. Re-measured in a detached worktree at
`0602f3b`, each command run to completion:

| command | CI row | exit at `0602f3b` |
| --- | --- | --- |
| `cargo check --no-default-features --features store-cockroach` | `cockroach` | 101 |
| `cargo test --no-default-features --features store-cockroach` | `cockroach` | 101 |
| `cargo check --features demo` | `demo` | 101 |
| `cargo check --all-targets --features ship,fixtures` | `ship-fixtures` | 101 |
| `cargo test --features ship,fixtures` | `ship-fixtures` | 101 |
| `cargo build --release --features ship` | release job (`FEATURES: ship`) | 101 |

The distinction matters for how the breakage reads: "one lint row" is a gap, "the release
job" is a shipping stop. The fix here closes all of them.

Closed here as a side effect of the consolidation: `gcp_auth` is now gated
`any(embed-gemini, store-postgres)` and every reference to it inside `pg` is gated
`store-postgres`. The `cockroach` row is green on this tree.

---

## 1. The IAM token expires; the pool did not

**The defect.** Cloud SQL IAM database authentication uses an OAuth token as the password,
valid about an hour. Postgres checks the password at connect time only. The old code minted
one token when the pool was created and never again, so the pool kept working on its open
connections and failed on the next one it had to open. A long-lived `serve` would have met
that as an authentication error a couple of hours in, looking nothing like an expiry.

**The fix.** `PgStore::iam_pool()` holds the token source, the live pool, and the instant the
token stops being handed out (`GoogleOAuthTokenSource::access_token_with_expiry`). Past that
instant it builds a replacement lazy pool with a freshly minted token. The superseded pool is
**dropped, not closed**: an in-flight query holds its connection (and through it the inner
pool) until it finishes, while nothing new is ever handed out from it.

**Why `pool()` now returns an owned `PgPool`.** A `&PgPool` into a slot that can be swapped
is not a reference the borrow checker can hand out. `PgPool` is an `Arc` internally, so the
clone is a refcount, and all 23 call sites bind it as `let pool = &self.pool().await?;`,
keeping every downstream use as `&PgPool`.

**Why rotation is at the pool, not the connection.** sqlx 0.8 has no per-connection connect
hook (`PoolConnector` arrives in 0.9). Building a new lazy pool costs one token mint and no
connection, so the coarser unit is not a real cost.

Pin: `store::pg::postgres::tests::the_iam_pool_is_rebuilt_when_its_token_expires`. It mints
against a mock OAuth endpoint returning `expires_in: 61` (a 1s TTL after the 60s refresh
margin), asserts one hit across two `pool()` calls, sleeps past the margin, and asserts the
second mint. Offline: `connect_lazy_with` opens nothing, so a DSN pointing at `127.0.0.1:1`
exercises the whole mint-and-rotate path without a database.

**Mutation:** forcing the freshness check true (`if true || Instant::now() < *expires_at`)
turns that test red.

---

## 2. Two Google auth implementations, one of them wrong for this machine

**The defect.** `embed/gemini.rs` had a full implementation (both credential kinds, both
grants) and `gcp_auth.rs` had a service-account-only copy for the store. Not merely
duplication: the copy deserialized service-account key JSON, so on a host whose credential
is an authorized-user ADC file, which is what `gcloud auth application-default login` writes
and what the live Vertex verification actually ran with, the embedder worked and the store
could not authenticate at all.

**The fix.** One module, `src/gcp_auth.rs`, gated `any(embed-gemini, store-postgres)`:
`GoogleCredentials`, `load_credentials`, `credentials_path_from_env`, `build_client`, and
`GoogleOAuthTokenSource` with the scope supplied **per caller** (Vertex asks for
cloud-platform; a Cloud SQL login asks for cloud-platform plus `sqlservice.login`). The
embedder re-exports the names it always used, so its call sites are unchanged.

**Correction, 2026-08-25 (round-1 P1-1 / P2-1).** As first written this section claimed more
than the code did, twice. The scope was sent on the `jwt-bearer` grant only, so on an
authorized-user ADC the scope argument was inert and both consumers received whatever the
ADC had been granted; and `build_gemini_embedder` still resolved
`GOOGLE_APPLICATION_CREDENTIALS` alone, so "one credential file" was true of the store only.
Both are fixed in `L1-remediation-round1.md`: the refresh grant now carries `scope`, and the
embedder resolves `credentials_path_from_env`. Read this section with that correction.

**The classification contract survives.** A3's degradation rule depends on transport failures
being `Unavailable` and refused grants being `Backend`. The shared module has its own
`GoogleAuthError` with that split, mapped one-for-one onto `EmbedError`.

Pins: `gcp_auth::tests::an_authorized_user_adc_file_loads_for_the_cloud_sql_path` (the
regression this fixes), `the_callers_scope_is_the_one_signed`,
`embed::gemini::tests::auth_error_classification_is_preserved`,
`gcp_auth::tests::vertex_asks_for_cloud_platform_only` (round 2 replaced this section's
original same-named gemini pin, which asserted a constant against its own definition), plus the
five auth tests that moved with the
code and still pass.

---

## 3. `LAMBO_POSTGRES_IAM` could have reached a Cockroach cluster

Found while doing 1 and 2. The variable names Postgres, the check was dialect-blind, and
`ship` carries both adapters, so exporting it for the hosted store in a process that also
opens a Cockroach cluster would have handed a Cloud SQL token to Cockroach.

`Dialect::SUPPORTS_CLOUD_SQL_IAM_AUTH` (default `false`, `true` for Postgres only) now gates
the opt-in. Pin: `store::pg::cockroach::tests::cockroach_ignores_the_cloud_sql_iam_opt_in`.

**Mutation:** dropping `D::SUPPORTS_CLOUD_SQL_IAM_AUTH` from the constructor's condition
turns that test red with `CockroachStore IAM auth setup: LAMBO_POSTGRES_IAM is set but
GCP_LAMBO_CREDENTIALS / GOOGLE_APPLICATION_CREDENTIALS is unset`.

**Where the opt-in is read.** In `PgStore::new`, which is synchronous, for the same reason
`connect_options` is: a test that pins an env-driven option must not have to hold a lock
across an `.await` (spec §6.4, `clippy::await_holding_lock`). The async path reads no
environment at all now.

---

## 4. One machine on the allowlist

The instance admitted exactly one `/32`, the cachyos box's egress address at provisioning
time. Mooshik's autobiography spans two machines, so the second could not reach the store at
all, and a home address rotates, after which every hosted call fails to connect and reads as
an outage rather than an allowlist miss.

`scripts/cloudsql-allowlist.sh` adds the running host's egress IP idempotently. It reads the
address from two independent resolvers and refuses when they disagree, preserves entries it
did not add, refuses to empty the list, and has `--list`, `--dry-run`, `--ip` and `--remove`.
Exercised read-only against the live instance: `--list` reports the single existing entry and
`--dry-run` on this host reports *already allowlisted; nothing to do*. **No live change was
made to the instance.**

---

## 5. Released binaries could not run the tier that was just built

`ship`, the feature set behind every prebuilt binary, carried neither `store-postgres` nor
`embed-gemini`, against a README that promises *"released binaries carry the full adapter
set, so picking a backend is a config decision"*. A `lambo.toml` naming `postgres` or
`gemini` would have failed on a released binary with a rebuild-the-binary error.

Both are in `ship` now. Neither adds a native toolchain dependency: `store-postgres` is the
same sqlx postgres driver `store-cockroach` already pulls, and `embed-gemini` adds
jsonwebtoken (pure Rust plus ring, already in the tree via rustls) on top of the reqwest
`embed-bge` already carries. The stale CI comment claiming `ship` excludes `store-postgres`
is corrected in `ci.yml`.

---

## Gates

Every row run on this tree, on the cachyos Linux box.

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean |
| `cargo clippy --all-targets --features ship,fixtures -- -D warnings` | clean |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | clean |
| `cargo clippy --all-targets --no-default-features --features store-postgres -- -D warnings` | clean |
| `cargo clippy --all-targets --no-default-features --features store-postgres,store-sqlite,fixtures -- -D warnings` | clean |
| `cargo test --features embed-bge,embed-fixture --lib` | 918 passed / 0 failed / 1 ignored |
| `cargo test --features ship,fixtures` | 1126 lib + 42 across the integration binaries, 0 failed, 18 ignored |
| `cargo test --no-default-features --features store-postgres` | 617 + 6 passed / 0 failed / 5 ignored |
| `cargo test --no-default-features --features store-cockroach` | 619 + 6 passed / 0 failed |
| `cargo test --features embed-gemini` | 941 + 7 passed / 0 failed / 2 ignored |
| `cargo check --no-default-features` | clean |
| `cargo check --features ship,embed-gemini` | clean |
| `cargo test --features ship,fixtures --test binary_parity` | 4 passed / 0 failed |

**Correction, 2026-08-25 (round-1 P3-1). The baselines in this paragraph as first written
were wrong, and the integration sums in the table above understate.** Two separate mistakes:

1. The pre-change numbers were never measured against `0602f3b`. Measured there now, in a
   detached worktree, each row run to completion:

   | row at `0602f3b` | lib | other binaries | doc-tests | total |
   | --- | --- | --- | --- | --- |
   | `--features embed-bge,embed-fixture --lib` | 918 | - | - | 918 |
   | `--features ship,fixtures` | - | - | - | **does not compile** (§0) |
   | `--no-default-features --features store-postgres` | **606** | 8 | 2 | 616 |
   | `--no-default-features --features store-cockroach` | - | - | - | **does not compile** (§0) |
   | `--features embed-gemini` | **935** | 11 | 2 | 948 |

   So the gemini lib row went 935 -> 941 (+6, not +1) and the postgres lib row went 606 ->
   617 (+11, not +1). "was 616 and is 617" was comparing a pre-change **total** against a
   post-change **lib** count, which is how a baseline five and ten tests too high slipped
   through unnoticed. Since the whole point of the paragraph is to show that no test was
   lost, a baseline inflated in exactly that direction would have concealed the loss it
   exists to rule out.

2. Where the gates table above reads "N lib + M across the integration binaries", M is the
   count from the **first** integration binary only, not the sum. `cargo test` prints one
   `test result:` line per target, and summing requires all of them plus the `Doc-tests`
   line. Correct sums for this commit, as independently measured in round-1 review Part C:
   `ship,fixtures` 1126 lib + 40 (not 42) with 20 ignored (not 18); `store-postgres` 617 lib
   + 10 (not 6); `store-cockroach` 619 lib + 10 (not 6); `embed-gemini` 941 lib + 13 (not 7)
   with 4 ignored (not 2). Every **lib** count in the table is exact.

The current tree's numbers are not these: remediation added four tests. See
[`L1-remediation-round1.md`](L1-remediation-round1.md) for the freshly measured table.

## Out of tree, not a gate

An external crate took lambo as a path dep with `default-features = false, features =
["store-sqlite", "embed-fixture"]` and built a live `Memory` (E1's real question). Findings
are recorded in [README.md](../README.md) §E1: the embedding contract is mandatory at build
time, provisioning is `GraphStore::init_schema()` rather than a shell-out to `lambo
provision`, and `SessionEndpoint::resolve` takes `(session, &StoreConfig)`.

## Still open, deliberately

- The **live** shared-SA leg was verified before this change (`0602f3b`) and has not been
  re-run against Cloud SQL since; the rotation path in particular has an offline proof and no
  live one. Running `iam_auth_connects_as_service_account` on this tree would close it.
- Public IP plus allowlist stays dev-grade. Private IP plus the Cloud SQL Auth Proxy is the
  production answer, and the proxy also handles token rollover on its own.
- The second machine still has to run the allowlist script once; nothing here can do that for
  it.
