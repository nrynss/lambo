# GCP-hosted Postgres + the shared service account (2026-08-24)

Documents two things done for Lambo's Mooshik deployment: a live smoke test of the
`store-postgres` adapter against a real Cloud SQL pgvector instance, and the shared-SA
path where ONE GCP service account authenticates to both the Gemini embedder (Vertex) and
the Postgres store (Cloud SQL IAM database auth).

## Infrastructure provisioned (project `mooshik`, owner SA `cachy-nryn`)

All created with the owner SA key in `GCP_LAMBO_CREDENTIALS`:

| Resource | Detail |
| --- | --- |
| Cloud SQL instance | `lambo-pg` - PostgreSQL 16, `db-f1-micro`, Enterprise, `us-central1-c`, public IP `136.64.220.174` |
| Database | `lambo` |
| Built-in user | `lambo` (password) - the simple password-DSN path |
| Extension | pgvector 0.8.5, enabled and verified (vector(3) insert/select) |
| IAM DB auth | `cloudsql.iam_authentication=on`; SA `cachy-nryn@mooshik.iam` added as a `CLOUD_IAM_SERVICE_ACCOUNT` DB user; SA granted `roles/cloudsql.instanceUser` (and is owner) |
| Network | allowlisted only `49.207.62.199/32` (Lambo host egress) |
| Env (in `~/.zshrc`) | `GCP_LAMBO_CREDENTIALS`, `LAMBO_POSTGRES_DSN` (password path) |
| Secrets | `~/.config/lambo-pg-root-password.txt`, `lambo-pg-app-password.txt` (mode 600) |

The IAM DB username for a service account strips `.gserviceaccount.com`, so the Postgres
login is `cachy-nryn@mooshik.iam`, not the full email.

## 1. Smoke test: `store-postgres` against live Cloud SQL

Ran the B dead-path live suite against the instance as the postgres root user (the live
tests create scratch databases and `CREATE EXTENSION vector`, which need a superuser; the
app-level `lambo` user has every non-superuser grant on the `lambo` db):

```
LAMBO_POSTGRES_DSN='postgresql://postgres:<root-pw>@136.64.220.174:5432/lambo?sslmode=require' \
  cargo test --features store-postgres,store-sqlite,store-memory,fixtures \
    --lib store::pg::postgres:: -- --ignored
```

Result: **4 passed / 0 failed** in ~46s.

- `init_schema_at_two_widths_creates_hnsw` - migration + hnsw at two widths
- `explain_recall_uses_hnsw` - recall plans through the hnsw index
- `fencing_refuses_stale_write_and_upserts_replay` - fencing tokens survive
- `live_schema_width_refuses_a_config_that_disagrees` - width authority honoured

That proves the adapter (migrations, pgvector DDL, hnsw, recall, fencing) end to end
against the managed instance.

## 2. Shared-SA path: one account for Vertex and Postgres

### Why
The hosted embedder (Gemini on Vertex) and the Cloud SQL store are both mooshik
infrastructure. Keeping one identity means no second credential, and Verted/DB access
are audited to the same service account.

### Infra (GCP side)
- Enable `cloudsql.iam_authentication=on` (instance patch; restarts).
- `gcloud sql users create cachy-nryn@mooshik.iam --instance=lambo-pg --type=CLOUD_IAM_SERVICE_ACCOUNT`
  (create with the suffix stripped).
- Grant the SA `roles/cloudsql.instanceUser` on the project.
- A service-account IAM database login authenticates with an OAuth token scoped to
  `sqlservice.login` (plus cloud-platform) as the Postgres password. No stored DB
  password for this path.

### Lambo integration (code)
`store-postgres` mints the shared SA token and injects it as the connection password when
opted in:

- `src/gcp_auth.rs` (gated `embed-gemini` OR `store-postgres`): the **one** Google auth
  path in the crate. It reads the shared credential file (`GCP_LAMBO_CREDENTIALS`, else
  `GOOGLE_APPLICATION_CREDENTIALS`), handles both credential kinds (service-account key by
  the `jwt-bearer` grant, authorized-user ADC by the `refresh_token` grant), and mints an
  access token for the **caller's** scope, caching until `expires_in - 60s`.
- `src/store/pg/mod.rs` `PgStore::pool()`: when `LAMBO_POSTGRES_IAM` is set it returns the
  IAM pool, whose connection password is that token, so the DSN carries the IAM user with
  NO password. The pool is **rotated at the token's expiry** (see the follow-ups closed
  below), not built once.
- Cargo: `store-postgres` also enables `dep:reqwest` and `dep:jsonwebtoken` (the token
  mint needs them); neither is in the default feature set.
- Tests: `iam_auth_connects_as_service_account` (live, ignored) plus two offline pins in
  `store/pg/postgres.rs`, `the_iam_pool_is_rebuilt_when_its_token_expires` and
  `iam_without_credentials_fails_closed_naming_the_variables`, and the auth module's own
  suite in `gcp_auth.rs`.

### Running the shared-SA store path
```
export GCP_LAMBO_CREDENTIALS=/home/nryn/.config/mooshik-8f7bb8506bc4.json
export LAMBO_POSTGRES_IAM=1
export LAMBO_POSTGRES_DSN='postgresql://cachy-nryn%40mooshik.iam@136.64.220.174:5432/lambo?sslmode=require'
```
(the `%40` is the literal `@` in the IAM DB username, URL-encoded). Lambo mints the SA
token and authenticates as `cachy-nryn@mooshik.iam` to Postgres - the same SA that calls
Vertex via `LAMBO_GEMINI_*` / the config.

Live proof (via the store's own pool):
```
IAM authenticated to Postgres as: cachy-nryn@mooshik.iam
```

`cachy-nryn@mooshik.iam` is the name of the specific SA provisioned here; what the code
asserts is that the login IS an IAM user (`current_user` contains a domain `@`), not this
exact string, so a differently-named SA still works.

### Follow-ups, closed 2026-08-24
The three that the first cut named and deferred are now done. What remains open is named
below them, and named honestly.

- **Token lifetime: closed.** The IAM token lives about an hour, and Postgres checks the
  password only at connect time, so a pool built with an expired token keeps working on its
  open connections and fails on the next one it opens. A `serve` would have met that as an
  authentication error two hours in, looking nothing like an expiry. `PgStore::iam_pool()`
  now holds the token source, the live pool, and the instant the token stops being handed
  out, and builds a replacement lazy pool at that instant. The superseded pool is dropped
  rather than closed: an in-flight query holds its connection until it finishes, and
  nothing new is handed out from it. `pool()` returns an owned `PgPool` (an `Arc` clone) for
  this reason; a `&PgPool` into a slot that can be swapped is not a reference that can be
  handed out. Pinned by `the_iam_pool_is_rebuilt_when_its_token_expires`, which mints
  against a mock OAuth endpoint with `expires_in: 61` and asserts the second mint.
- **The opt-in is PostgreSQL only.** `LAMBO_POSTGRES_IAM` names Postgres, and `ship`
  carries both adapters, so a variable exported for the hosted store could have reached a
  Cockroach cluster in the same binary. The dialect decides
  (`Dialect::SUPPORTS_CLOUD_SQL_IAM_AUTH`, false by default, true for Postgres only), and
  `cockroach_ignores_the_cloud_sql_iam_opt_in` fails when that gate is removed.
- **Two auth implementations: closed.** They are one module now (`src/gcp_auth.rs`), shared
  by the embedder and the store, with the scope supplied per caller. This was not
  cosmetic. The store's copy parsed service-account keys ONLY, so on the machine whose
  credential is an authorized-user ADC file (which is what the live Vertex verification
  actually ran with) the embedder worked and the store could not authenticate at all.
  `an_authorized_user_adc_file_loads_for_the_cloud_sql_path` is that regression's pin.
  The error classification the embedder depends on survives the move one for one
  (`GoogleAuthError` maps onto `EmbedError`), pinned by
  `auth_error_classification_is_preserved`.
- **One machine on the allowlist: closed.** The instance admitted exactly one /32, the
  cachyos box's egress address at provisioning time. Mooshik spans two machines, and a
  home address rotates; either way every hosted call fails to connect and reads as an
  outage rather than an allowlist miss. `scripts/cloudsql-allowlist.sh` adds the running
  host's egress IP idempotently (`--list`, `--dry-run`, `--ip`, `--remove`), preserves the
  entries it did not add, and refuses to empty the list. Each machine runs it once, and
  again whenever its address changes.
- **Released binaries could not run this tier: closed.** `ship` (the feature set behind
  every prebuilt binary) carried neither `store-postgres` nor `embed-gemini`, so a
  `lambo.toml` naming `postgres` or `gemini` failed on a released binary with a
  rebuild-the-binary error, against a README that promises the full adapter set. Both are
  in `ship` now. Neither adds a native toolchain dependency.

### Constraints (honest limits)
- **Rotation is at the pool, not the connection.** sqlx 0.8 has no per-connection connect
  hook (`PoolConnector` is 0.9), so the unit of rotation is the pool. That is sufficient
  here (a new pool is lazy and costs one token mint) and it is why the fix reads the way it
  does rather than as a password callback.
- **IAM DB user privileges.** The SA is a `CLOUD_IAM_SERVICE_ACCOUNT` login, not a
  superuser. Run lambo's schema DDL (which needs `CREATE EXTENSION`/`CREATE DATABASE`)
  as the postgres root once, then operate with the SA. The `lambo` built-in user remains
  the simple password path.
- **Public IP plus allowlist is dev-grade.** Production should use a private IP and the
  Cloud SQL Auth Proxy. The allowlist script makes the dev-grade path survivable across two
  machines and a rotating address; it does not make it production.

## Cost
`db-f1-micro` ~ $7-9/mo plus small storage/backup. Stop it when idle if you want to drop it.

## Reproduce
1. Smoke (password path): the command in section 1 with the root password.
2. Shared-SA: export the three vars in section "Running the shared-SA store path" and run
   the `iam_auth_connects_as_service_account` ignored test.
