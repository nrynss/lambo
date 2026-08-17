# `scripts/aws-infra` — AWS provisioning for the CloudOps exhibit

Phase 3 of `docs/plans/multi-agent-cloudops-aws-plan.md` (revision 2). These four
scripts build, and tear down, the `us-east-1` stack that the plan's §2 diagram
describes: a VPC the two agents provision, a private-tier RDS workload, a stats
Lambda outside the VPC, and a public EC2 exhibit serving `lambo serve-web` behind
Caddy.

**Read the plan first.** In particular §2 (why the Lambda is outside the VPC),
§6 (why RDS is not a Lambo store), §7 (cost and teardown) and §8 (TLS). These
scripts implement revision 2 specifically; the general idea is not enough to
review them against.

---

## Prerequisites

| | |
|---|---|
| Python | 3.12+ |
| Packages | `boto3`. Nothing else. (`pg8000` is vendored into the Lambda zip at build time and is never imported by these scripts.) |
| Credentials | Any ordinary boto3 chain: `AWS_PROFILE`, environment keys, or an instance role. **No profile, account id, access key or DSN is hardcoded anywhere in this directory.** |
| Region | `us-east-1` by default; `--region` overrides on every script. |
| Tools for the Lambda build | `pip` and network access to PyPI, or a prebuilt zip passed with `--lambda-zip`. |

Sanity check before you start:

```sh
python3 -m pip install boto3
aws sts get-caller-identity          # or just let the scripts tell you
```

### IAM permissions the running identity needs

EC2 (VPC, subnets, route tables, internet gateways, security groups, addresses,
instances, images), RDS (instances and subnet groups), Lambda (functions,
function URLs, permissions), IAM (roles, role policies, instance profiles),
Secrets Manager (create, describe, delete, restore — **not** `GetSecretValue`;
nothing here reads the DSN), SSM `GetParameter` on the public AMI path, and
CloudWatch Logs `DescribeLogGroups`/`DeleteLogGroup` for teardown.

---

## Order to run things

```sh
SSH_CIDR="$(curl -s https://checkip.amazonaws.com)/32"
SESSION="<the lambo session id the exhibit shows>"

# 1. Network foundation, security groups, and the (empty) secret.
python3 scripts/aws-infra/provision_network.py --ssh-cidr "$SSH_CIDR"

# 2. Put the CockroachDB DSN in the secret. Once, by hand. See below.

# 3. RDS workload + stats Lambda.
python3 scripts/aws-infra/provision_app_data.py --session "$SESSION"

# 4. The public exhibit.
python3 scripts/aws-infra/launch_exhibit_ec2.py \
    --session "$SESSION" --hostname lambo.example.com --key-name my-keypair

# ... and, when the event is over:
python3 scripts/aws-infra/teardown.py               # report only
python3 scripts/aws-infra/teardown.py --confirm     # actually delete
```

Every script takes `--dry-run`, which prints the plan and **makes no AWS API call
at all** — not even a read. Use it freely; it works with no credentials
configured.

Every script is **idempotent**. Resources are looked up by tag before creation
and reported as `[exists  ]` rather than duplicated, so a run that dies halfway
is fixed by running it again.

### Setting the DSN

`provision_network.py` creates `lambo/cockroach-dsn` with **no value**, on
purpose: a DSN passed to a script ends up in shell history, in `ps`, and in any
log that captured the command. Set it once yourself:

```sh
read -rs LAMBO_DSN            # paste the DSN; it is not echoed
aws secretsmanager put-secret-value \
    --region us-east-1 --secret-id lambo/cockroach-dsn \
    --secret-string "$LAMBO_DSN"
unset LAMBO_DSN
```

The DSN must carry `sslmode=verify-full&sslrootcert=system` (AGENTS.md). Nothing
in this repo reads the value back. The exhibit instance and the stats Lambda each
resolve it themselves at runtime, through an IAM policy scoped to that one secret
ARN.

---

## What each script creates

### `provision_network.py`

| Resource | Detail |
|---|---|
| `VPC-Enterprise-Prod` | `10.0.0.0/16`, DNS support and hostnames on |
| `Subnet-Public-1a` | `10.0.1.0/24`, `us-east-1a`, auto-assign public IPv4 |
| `Subnet-Private-1a` | `10.0.2.0/24`, `us-east-1a` |
| `Subnet-Private-1b` | `10.0.3.0/24`, `us-east-1b` — **not in the plan**; RDS refuses a DB subnet group spanning fewer than two AZs, even for a single-AZ instance. The database itself still lands in `1a`. |
| `InternetGateway` | attached to the VPC |
| `RouteTable-Public` | `0.0.0.0/0` → IGW, associated with the public subnet |
| `SG-Base-VPC` | self-referential all-protocol mesh, plus `5432` from `SG-PublicWeb` |
| `SG-PublicWeb` | `80`, `443` from `0.0.0.0/0`; `22` from `--ssh-cidr` |
| `lambo/cockroach-dsn` | Secrets Manager, created empty |

**No NAT gateway, anywhere.** Nothing in this design needs one (plan §2), and the
private subnet deliberately has no route table of its own — it uses the VPC main
table, which carries only the local route.

Arguments: `--ssh-cidr` (**required**, no default, `0.0.0.0/0` is rejected),
`--region`, `--profile`, `--dry-run`.

### `provision_app_data.py`

| Resource | Detail |
|---|---|
| `lambo-cloudops-db-subnets` | DB subnet group over the two private subnets |
| `rds-lambo-demo-db` | `db.t4g.micro` PostgreSQL, 20 GB gp3, encrypted, single-AZ, `PubliclyAccessible=False`, in `SG-Base-VPC`, `BackupRetentionPeriod=0` |
| `lambo-cloudops-stats-role` | Lambda execution role: basic logging, plus `GetSecretValue`/`DescribeSecret` on **the one secret ARN** |
| `Lambda-LamboStats-API` | `python3.12` on **arm64**, **no `VpcConfig`**, Function URL with `AuthType=NONE` |

The master password is generated and held by RDS itself
(`ManageMasterUserPassword=True`), so no password is handled, printed or stored
by these scripts, and RDS deletes that secret with the instance.

**RDS is the tracked workload, not a Lambo store** (plan §6).
`migrations/cockroach/001_init.sql` uses `VECTOR(1024)` and `CREATE VECTOR INDEX`
and will not apply to stock PostgreSQL. The trap: `StoreKind::from_str` accepts
`"postgres"`/`"pg"` as aliases for Cockroach (`src/store/mod.rs:403`), so pointing
`lambo.toml` at this instance connects cleanly and *then* fails on migration. Do
not wire Lambo at it. Its job is to be the private-tier workload whose dependency
on `SG-Base-VPC` and `Subnet-Private-1a` is what gives those nodes blast radius.

The Lambda's deployment zip is built locally from
`scripts/aws-infra/lambda_src/lambo_stats.py` plus a `pip install --target` of
`pg8000` (pure Python, so one package works on arm64). The zip is built with a
fixed timestamp and a sorted walk, so it is byte-reproducible and a re-run is a
genuine no-op.

Arguments: `--session` (**required**), `--skip-rds`, `--skip-lambda`,
`--lambda-zip`, `--no-wait`, plus the common four.

### `launch_exhibit_ec2.py`

| Resource | Detail |
|---|---|
| `lambo-cloudops-exhibit-role` | `GetSecretValue`/`DescribeSecret` on **exactly** `lambo/cockroach-dsn`. Not `secretsmanager:*`, not a resource wildcard. |
| `lambo-cloudops-exhibit-profile` | instance profile holding that role |
| `EC2-LamboWebExhibit` | `t4g.large` (Graviton/arm64; x86_64 families like `m7i-flex.large` also accepted) Ubuntu 26.04 LTS in `Subnet-Public-1a` / `SG-PublicWeb`, 8 GB gp3, IMDSv2 required |
| `llama-server.service` | llama.cpp built from a pinned tag, serving BGE-M3 (GGUF, Q8_0) on `127.0.0.1:8080`, loopback only |
| `EIP-LamboWebExhibit` | Elastic IP, so the A record survives a stop/start (`--no-eip` to skip) |

User data installs, as systemd services:

* `lambo-web.service` — fetches `lambo-<version>-linux-arm64` from the GitHub
  release and **verifies it against the `.sha256` published beside it** before
  installing. `t4g` is ARM, so this is the `linux-arm64` asset; an x86_64 build
  would boot and then fail. The version is `--lambo-version` (default `0.2.0`).
* `caddy.service` — same treatment: the `caddy_<v>_linux_arm64.tar.gz` release
  asset, verified against the release's `checksums.txt`.

**The DSN never lands on disk.** There is no `EnvironmentFile`. A wrapper at
`/usr/local/bin/lambo-serve-web` resolves the secret at *service start* and
`exec`s `lambo`, so the value exists only in the environment of the running
process. Nothing to leak in an AMI, a snapshot, or a log — and the bootstrap
script deliberately never enables `set -x`. Rotating the secret needs only
`systemctl restart lambo-web`.

`lambo serve-web` binds `127.0.0.1:7710`. Caddy is the only thing that talks to
it. That is deliberate: a non-loopback bind refuses to start without a bearer
token, and the public portal is meant to be readable without one.

#### TLS — you must choose (plan §8)

`https://<EC2-IP>` **cannot** get a trusted certificate; public CAs do not issue
for bare IP addresses. The script refuses to run without a decision:

* `--hostname lambo.example.com` — Caddy issues and renews automatically. After
  the script prints the Elastic IP, create an `A` record pointing at it; Caddy
  retries the ACME order until it resolves. **Recommended.**
* `--self-signed` — Caddy's internal CA. Works instantly, and **every visitor
  including every judge sees a browser security warning**. The script says so in
  the plan output and again at the end. There is no silent fallback.
* Cloudflare Tunnel, §8's third option, is not automated here. It would need no
  inbound ports at all.

If Caddy has to fall back to the ACME HTTP-01 challenge, port 80 is already open
by default (`Provision 80 from 0.0.0.0/0`).

#### Embedder

`serve-web` resolves a store *and* an embedder, and `/api/recall` embeds the
judge's query with it. **The embedder must be the same model that wrote the
vectors in the store**, not merely one of the same width: `resolve_backends`
checks only that the embedder's dimension matches the store's `VECTOR(1024)`, so
a mismatched model resolves cleanly and then ranks the judge's query against a
vector space the stored embeddings do not share. Nothing errors. The answers just
quietly stop meaning anything.

The live sessions were written with `bge_m3`, so:

* `--embedder bge_m3` (default) — installs llama.cpp and BGE-M3 on the instance
  and serves them on loopback. This is why the default instance type is
  `t4g.large`: BGE-M3 does not fit in a `t4g.micro`'s 1 GiB, and sizing it too
  small does not fail at launch — llama-server loads the model and is then killed
  by the OOM killer, usually mid-demo. The script refuses the too-small types
  rather than letting that happen.
* `--embedder bge_m3 --llama-url <url>` — point at a llama.cpp already running
  somewhere reachable and install nothing.
* `--embedder fixture` — no external service at all. **The vector arm of recall
  returns noise**, for the reason above. Lexical and structural recall still
  work, so the blast-radius and canonical markers are unaffected — the demo's
  actual punchline survives, but "recalls by meaning" does not. The script warns
  on every run.

Boot takes a few minutes longer with a local llama.cpp: it is a pinned
prebuilt binary fetched by SHA-256 (no source build). `lambo-web.service` waits
for the embedder's health endpoint before starting, so it does not crash-loop
under `Restart=always` while the model loads.

`--caddy-version`, `--embedder`, `--llama-url`, `--instance-type` (Graviton
`t4g.large` by default; `m7i-flex.large` for the x86_64 free tier),
`--volume-size`, `--key-name`, `--no-eip`, plus the common four.

### `teardown.py`

Deletes everything tagged `Project=lambo-cloudops`, in dependency order:

1. `Lambda-LamboStats-API` (its Function URL goes with it)
2. `/aws/lambda/Lambda-LamboStats-API` — the log group the Lambda service creates
   implicitly. It carries no tags, so it is deleted by name; otherwise it is the
   one thing tag-filtered discovery would never find.
3. `lambo-cloudops-stats-role`
4. Elastic IP — disassociated *and released*. An unassociated EIP is billed by
   the hour and is the most common thing left behind by a manual cleanup.
5. `EC2-LamboWebExhibit` — terminate, then **wait for `terminated`**, because the
   instance's ENI holds `SG-PublicWeb` and the security-group delete fails with
   `DependencyViolation` until it is released.
6. instance profile, then `lambo-cloudops-exhibit-role`
7. `rds-lambo-demo-db` — `SkipFinalSnapshot=True` (plan §7), then wait for gone
8. `lambo-cloudops-db-subnets`
9. `SG-Base-VPC`, `SG-PublicWeb` — **all rules revoked on both first**. The two
   groups reference each other, and AWS refuses to delete a group any rule still
   points at, so no ordering of deletes alone can break the cycle.
10. `RouteTable-Public` — disassociate, then delete
11. `InternetGateway` — detach, then delete
12. the three subnets
13. `VPC-Enterprise-Prod`
14. `lambo/cockroach-dsn`, last, because everything above may still read it

**Nothing is deleted without `--confirm`.**

```sh
python3 scripts/aws-infra/teardown.py               # discover and report; deletes nothing
python3 scripts/aws-infra/teardown.py --dry-run     # offline plan; makes no AWS call at all
python3 scripts/aws-infra/teardown.py --confirm     # delete
python3 scripts/aws-infra/teardown.py --verify-only # the sweep, on its own
```

The secret is deleted with a **7-day recovery window** by default;
`provision_network.py` calls `restore_secret` if you rebuild inside that window,
so a teardown-and-rebuild cycle just works. `--force-delete-secret` purges it
immediately and irreversibly.

#### Teardown verification sweep

`--confirm` ends by running the sweep automatically, and `--verify-only` runs it
alone. It is the tag-filtered `describe-*` pass plan §7 asks for: every one of
these must come back empty.

```sh
REGION=us-east-1
FILTER='Name=tag:Project,Values=lambo-cloudops'

aws ec2 describe-vpcs               --region $REGION --filters "$FILTER" --query 'Vpcs[].VpcId'
aws ec2 describe-subnets            --region $REGION --filters "$FILTER" --query 'Subnets[].SubnetId'
aws ec2 describe-route-tables       --region $REGION --filters "$FILTER" --query 'RouteTables[].RouteTableId'
aws ec2 describe-internet-gateways  --region $REGION --filters "$FILTER" --query 'InternetGateways[].InternetGatewayId'
aws ec2 describe-security-groups    --region $REGION --filters "$FILTER" --query 'SecurityGroups[].GroupId'
aws ec2 describe-addresses          --region $REGION --filters "$FILTER" --query 'Addresses[].AllocationId'
aws ec2 describe-instances          --region $REGION --filters "$FILTER" \
    'Name=instance-state-name,Values=pending,running,stopping,stopped' \
    --query 'Reservations[].Instances[].InstanceId'

aws rds    describe-db-instances    --region $REGION --db-instance-identifier rds-lambo-demo-db
aws lambda get-function             --region $REGION --function-name Lambda-LamboStats-API
aws iam    get-role --role-name lambo-cloudops-exhibit-role
aws iam    get-role --role-name lambo-cloudops-stats-role
```

The last four are expected to fail with `DBInstanceNotFound`, `ResourceNotFound`
and `NoSuchEntity` respectively — that failure *is* the pass. Everything above
them must print `[]`.

A secret sitting in its 7-day recovery window is expected after a non-forced
teardown and is not counted as a leftover.

---

## Design notes worth not re-litigating

* **Tags are the only inventory.** Every resource is stamped
  `Project=lambo-cloudops` plus a `Name` *at creation* (EC2 `TagSpecifications`,
  not a follow-up `create-tags`), so a crash mid-run cannot produce something
  teardown is blind to. There is no state file and no id list.
* **Names come from the plan verbatim.** Phase 4's agent scripts derive Lambo
  graph nodes under these exact names, so renaming one here silently breaks the
  blast-radius demo: the graph would describe resources that no longer match the
  account. The one exception is the RDS identifier, which must be lowercase
  (`rds-lambo-demo-db`); the plan's `RDS-Lambo-Demo-DB` is carried on its `Name`
  tag.
* **`--dry-run` constructs no client.** That is what makes the plan output usable
  as a review artifact and testable without credentials.
* **IAM propagation is retried, not documented away.** Both `RunInstances` with a
  fresh instance profile and `CreateFunction` with a fresh role fail for a few
  seconds after the role is created; both call sites retry.

## Files

```
scripts/aws-infra/
  _common.py                    tags, lookups, clients, error handling, waiting
  provision_network.py          VPC, subnets, IGW, route table, SGs, secret
  provision_app_data.py         RDS, stats Lambda, Function URL
  launch_exhibit_ec2.py         instance profile, EC2, EIP, Caddy + systemd user data
  teardown.py                   ordered deletion + verification sweep
  lambda_src/lambo_stats.py     the Lambda handler (packaged, not deployed as-is)
```
