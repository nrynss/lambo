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
`store-postgres` now mints the shared SA token and injects it as the connection
password when opted in:

- `src/gcp_auth.rs` (new, gated `store-postgres`): `CloudSqlTokenSource` reads the same
  credential file the embedder uses (`GCP_LAMBO_CREDENTIALS`, else
  `GOOGLE_APPLICATION_CREDENTIALS`), mints an RS256 JWT and exchanges it for an OAuth
  access token scoped `cloud-platform sqlservice.login`, caching until `expires_in - 60s`.
- `src/store/pg/mod.rs` `PgStore::pool()`: when `LAMBO_POSTGRES_IAM` is set, it mints the
  token and sets it as the `PgConnectOptions` password (`options.password(&token)`), so
  the DSN carries the IAM user with NO password.
- Cargo: `store-postgres` now also enables `dep:reqwest` and `dep:jsonwebtoken` (the
  token mint needs them); neither leaks into default builds (store-postgres is not
  default).
- Live test `iam_auth_connects_as_service_account` (ignored) in `store/pg/postgres.rs`.

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

### Constraints (honest limits)
- **Token lifetime.** The IAM token is minted once when the SQLx pool is first created
  and cached by `CloudSqlTokenSource`; it is valid ~1h. A long-running `serve` should
  re-establish the pool before expiry, or use the Cloud SQL Auth Proxy which handles
  rollover. For a smoke run and intermittent use this is fine; flagged as the production
  follow-up.
- **IAM DB user privileges.** The SA is a `CLOUD_IAM_SERVICE_ACCOUNT` login, not a
  superuser. Run lambo's schema DDL (which needs `CREATE EXTENSION`/`CREATE DATABASE`)
  as the postgres root once, then operate with the SA. The `lambo` built-in user remains
  the simple password path.
- **Public IP + allowlist** is dev-grade. Production should use a private IP + the Cloud
  SQL Auth Proxy (which also resolves the 1h-token rollover).
- The embedder's own token logic lives in `embed/gemini.rs` and this store module in
  `gcp_auth.rs`; consolidating both behind one shared Google-auth module is a noted
  follow-up (the shared identity, via `GCP_LAMBO_CREDENTIALS`, is already one file).

## Cost
`db-f1-micro` ~ $7-9/mo plus small storage/backup. Stop it when idle if you want to drop it.

## Reproduce
1. Smoke (password path): the command in section 1 with the root password.
2. Shared-SA: export the three vars in section "Running the shared-SA store path" and run
   the `iam_auth_connects_as_service_account` ignored test.
