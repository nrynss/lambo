# Plan: Multi-Agent CloudOps Demo & Real AWS Infrastructure Protection

```yaml
status: PROPOSED / READY FOR EXECUTION POST-BINARY-VALIDATION
revision: 2 (hardened 2026-08-16 — see §0)
owner: nryn
target_hackathon: CockroachDB AI Hackathon on Devpost
target_services: AWS (EC2, Lambda, RDS, Secrets Manager, VPC) + CockroachDB Cloud
execution_mode: >
  Agents and `lambo serve` run on the operator's own machine and act on AWS
  through the AWS APIs. AWS hosts the resources being provisioned and the
  public read-only judge portal. Lambo itself is not hosted in AWS, apart from
  `lambo serve-web` on the exhibit EC2 instance.
```

---

## 0. Revision 2 — what changed and why

Rev 1 was reviewed against the code and priced. Five things changed:

1. **Topology corrected.** Rev 1 read as though Lambo ran in AWS. It does not: the
   agents and the single-writer `lambo serve` run locally, and AWS is the *subject*
   the agents act on plus the *host* of the judge portal. Everything below is
   rewritten to that shape.
2. **Lambda moved out of the VPC.** Rev 1 put the stats Lambda in the VPC alongside
   RDS. A VPC-attached Lambda has no route to the internet, so it could not have
   reached CockroachDB Cloud without a NAT gateway — about **$32/month**, over half
   the credit budget, for a demo that lasts days. The Lambda reads CockroachDB, not
   RDS, so it belongs outside the VPC and needs no NAT. See §7.
3. **RDS reframed honestly.** RDS cannot be a Lambo store — see §6 for the proof and
   the trap that makes it look like it could be. It stays, but as the *workload the
   app-data agent provisions and Lambo dependency-tracks*, which is a true and
   defensible claim rather than a decorative one.
4. **TLS made real.** `https://<EC2-IP>` cannot get a certificate. See §8.
5. **Per-agent timeline cut.** It carried a real cost (no agent identity exists
   anywhere in the portal today) and, worse, the obvious implementation would have
   attributed canonization to an agent — inventing a fact and contradicting the
   claim that status is earned structurally. Cut in §5; Phase 5 now needs no Rust
   change, so "zero core Rust changes" holds for the whole plan.
6. **Two follow-ups filed as issues** rather than carried here: the
   `Uuid::new_v4` ordering tie-break (#2) and the Bedrock embedder adapter (#3).

---

## 1. Overview & Problem Statement

Multi-agent autonomous systems managing cloud infrastructure (CDK, Terraform, CloudFormation, AWS CLI) lack unified, dependency-aware memory. 

- **The Danger**: `Agent 1` provisions foundation network & perimeter security (VPCs, Subnets, Shared Security Groups). `Agent 2` independently attaches workloads, databases, and serverless functions into that network. When `Agent 1` or an automated drift-remediation routine attempts to deprecate or delete "idle" or "unused" base security groups, flat vector RAG cannot calculate the cross-agent blast radius, causing catastrophic production outages.
- **The Solution (Lambo)**: Lambo maintains a bipartite graph memory in CockroachDB. It automatically promotes foundational cloud infrastructure to **`Canonical`** pillars through earned structural evidence. When any agent queries or attempts a destructive action against a shared resource, Lambo issues **blast-radius warnings** and **recency conflict alerts**.

---

## 2. Real AWS Infrastructure Architecture (`us-east-1`)

The operator's machine runs the agents and the single writer. AWS holds the
resources the agents provision, plus the public read-only portal judges visit.

```
   OPERATOR MACHINE (local)                        AWS CLOUD (us-east-1)
┌──────────────────────────────┐        ┌──────────────────────────────────────────┐
│ network-infra-agent          │        │ VPC-Enterprise-Prod (10.0.0.0/16)        │
│ app-data-agent               │──AWS──▶│                                          │
│   (boto3 / AWS CLI)          │  APIs  │  Public Subnet 10.0.1.0/24               │
│                              │        │   ┌────────────────────────────────────┐ │
│ lambo serve (SINGLE WRITER)  │        │   │ EC2-LamboWebExhibit (t4g.large)    │ │
│   stdio MCP + CLI verbs      │        │   │  Caddy 443 ─▶ lambo serve-web 7710 │ │
└───────────────┬──────────────┘        │   │  llama.cpp BGE-M3 FP16 on :8080    │ │
                │ writes                │   │  READ-ONLY. Instance profile reads │ │
                │                       │   │  lambo/cockroach-dsn at boot.      │ │
                ▼                       │   └────────────────────────────────────┘ │
     ┌─────────────────────┐            │                                          │
     │  CockroachDB Cloud  │◀───reads───│  Private Subnet 10.0.2.0/24              │
     │  (durable store)    │            │   ┌────────────────────────────────────┐ │
     └─────────────────────┘            │   │ RDS-Lambo-Demo-DB (db.t4g.micro)   │ │
                ▲                       │   │  The tracked workload. NOT a Lambo │ │
                │ reads                 │   │  store — see §6. No public access. │ │
                │                       │   └────────────────────────────────────┘ │
     ┌──────────┴──────────┐            │                                          │
     │ Lambda-LamboStats   │            │  Secrets Manager: lambo/cockroach-dsn    │
     │ Function URL,       │            │  IGW + public route table. NO NAT.       │
     │ OUTSIDE the VPC     │            └──────────────────────────────────────────┘
     └─────────────────────┘
```

**Why the Lambda sits outside the VPC.** It reads CockroachDB Cloud, which is on
the public internet. A VPC-attached Lambda has no internet route without a NAT
gateway. It has no reason to reach RDS, so attaching it to the VPC would buy
nothing and add a failure mode. Outside the VPC it gets internet egress with no
extra component in the path.

**There is deliberately no NAT gateway anywhere in this design.** Credits are
not the reason — if a future step seems to need one, that is a signal something
has been placed in the wrong tier. Re-check the placement before provisioning it.

## 3. Two-Agent Execution Workflow

### Track 1: `network-infra-agent` (Cloud Foundation)
1. **Network Infrastructure**:
   - Provisions `VPC-Enterprise-Prod`, `Subnet-Public-1a`, `Subnet-Private-1a`, `InternetGateway`, and `RouteTable-Public`.
   - Derives network entities and structural hierarchy (`parent_of`) into Lambo.
2. **Security & Secrets**:
   - Provisions `SG-Base-VPC` (internal mesh) and `SG-PublicWeb` (perimeter ingress).
   - Provisions `lambo/cockroach-dsn` in AWS Secrets Manager.
3. **Public Exhibit Compute**:
   - Launches `EC2-LamboWebExhibit` (`t4g.micro`) in `Subnet-Public-1a`.
   - Records actions and causal dependencies in Lambo (`depends_on: ["VPC-Enterprise-Prod", "Subnet-Public-1a", "SG-PublicWeb"]`).

### Track 2: `app-data-agent` (App & Data Platform)
1. **Database & Data Tier**:
   - Provisions `RDS-Lambo-Demo-DB` (`db.t4g.micro` PostgreSQL) in `Subnet-Private-1a`.
   - Derives data entities and connection logic into Lambo.
2. **Serverless App Tier**:
   - Deploys `Lambda-LamboStats-API` with Function URL.
   - Creates app execution roles and security groups.
3. **Cross-Over Binding**:
   - Queries Lambo memory to discover existing network topology.
   - Binds RDS and Lambda into `Subnet-Private-1a` and `SG-Base-VPC`.
   - Records explicit dependency edges into Lambo (`depends_on: ["VPC-Enterprise-Prod", "Subnet-Private-1a", "SG-Base-VPC"]`).

### Climax: Conflict & Blast-Radius Protection
1. `VPC-Enterprise-Prod` and `SG-Base-VPC` accumulate massive incoming degree and earn `Canonical` status.
2. `network-infra-agent` simulates a drift cleanup / refactoring operation:
   ```bash
   lambo recall --session $SESSION --query "tear down Subnet-Private-1a and delete SG-Base-VPC"
   ```
3. **Lambo Intercepts**:
   ```text
   VPC-Enterprise-Prod [Entity, canonical] (score <s>, blast radius 8)
   ⚑ Load-bearing pillar — 8 nodes depend on this. Modify with caution.
   Agent app-data-agent wrote to it <n> seconds ago
   Dependents: RDS-Lambo-Demo-DB, Lambda-LamboStats-API, SG-Base-VPC, Subnet-Private-1a...
   ```
4. `network-infra-agent` aborts destructive action, averting outage.

---

## 4. CockroachDB Agent Skill: `lambo-cloudops`

To satisfy the hackathon requirement for **CockroachDB Agent Skills** and make this behavior machine-executable for any LLM agent (Claude Code, Cursor, Antigravity, OMP), we package a dedicated Agent Skill in `skills/lambo-cloudops/SKILL.md`.

### What the Skill Encodes:
1. **Pre-flight Recall Protocol**: Before executing destructive AWS commands (`delete-*`, `terminate-*`, `disassociate-*`), the agent MUST run `lambo recall` against the active session. If a `⚑ Load-bearing pillar` warning is returned with blast radius > 0, the modification MUST be halted or require explicit human override.
2. **Provenance & Derivation Protocol**: Whenever a new resource is provisioned (e.g. EC2, RDS, Lambda, Subnets), the agent MUST run `lambo derive` and `lambo record-action` registering parent-child hierarchies and cross-resource dependencies.
3. **CockroachDB Direct Inspection**: Teaches agents how to query CockroachDB's `canonization_events` and `concepts` tables (via CockroachDB Cloud MCP or SQL) to understand why a specific cloud component became canonical.

---

## 5. Judge Web Portal: Canonization Audit Trail

The judge portal on EC2 is `lambo serve-web`, read-only, showing why the graph
reached the state it did. It ships on the endpoints that already exist — no Rust
change (see §9).

1. **Canonization Audit Trail** *(primary)*:
   - Direct visibility into the CockroachDB `canonization_events` log explaining
     *why* `VPC-Enterprise-Prod` earned `Canonical` status: incident degree,
     distinct agents, survival across GC sweeps, blast radius > 5.
   - Served by `/api/events` and `/api/stats` as they stand.
2. **Live recall against the session**:
   - The interactive recall engine, so a judge can run the destructive query
     themselves and watch the blast-radius warning come back.
   - Served by `/api/recall`.

### Cut from this plan: the per-agent provenance timeline

Rev 1 asked for a live stream "broken down by agent". **Cut**, for two reasons.

It is not free: nothing in the portal carries agent identity today — neither
`WebEvent` nor `WebStats` has the field, and `agent` appears four times in the
whole of `src/cli/serve_web.rs`. Building it means a new payload, read path,
endpoint and front-end, which is the only thing in this plan that would have
broken "zero core Rust changes".

More importantly, the obvious implementation would have been **wrong**.
`canonization_events` has no agent column (`id, session_id, node_id,
from_status, to_status, blast_radius, last_demotion_time, occurred_at`) and that
is correct, not an oversight: canonization is earned structurally by the daemon,
and no agent performs it. Stamping an agent onto a promotion would invent a fact
and would directly undercut the product's central claim — that status is earned
from structural evidence rather than declared by an agent.

If a timeline is ever wanted, the honest shape is agent-attributed
**interactions and actions** (`Interaction` already carries `agent_id`) with
canonization shown as *system* events on the same axis. That reads better for a
judge in any case: two agents acting, and the system independently promoting.
Tracked as a future enhancement, not as part of this submission.

**Deterministic scenario replay** is also out for now. It depends on the
byte-identical demo property currently under repair; see §9.

---

## 6. RDS: what it is, and what it is not

RDS is provisioned by `app-data-agent` as the **workload whose dependencies Lambo
tracks**. That is its honest role, and it is a real one: it is the node that gives
`SG-Base-VPC` and `Subnet-Private-1a` their blast radius, and therefore the reason
the climax in §3 has any stakes at all. Delete the shared SG and the database
loses its network. That is the outage the demo prevents.

**RDS is not, and cannot be, a Lambo store.** `migrations/cockroach/001_init.sql`
is CockroachDB-specific by construction — `embedding VECTOR(1024)` and
`CREATE VECTOR INDEX` — so the schema will not apply to stock PostgreSQL.

There is a trap worth naming, because it will look like it works:
`StoreKind::from_str` accepts `"postgres"` and `"pg"` as **aliases for Cockroach**
(`src/store/mod.rs:403`). Pointing `lambo.toml` at an RDS DSN therefore connects
cleanly and only then fails applying the schema. Nobody should spend an afternoon
on that. If a Postgres-compatible store is ever wanted, it is a new adapter with a
non-vector migration, not a config change.

**How to describe it in the submission.** "Amazon RDS for PostgreSQL — provisioned
by the app-data agent as the private-tier workload; its dependency on the shared
security group and private subnet is what Lambo's blast-radius warning protects."
That is accurate. Do not write "data store tier" or imply Lambo persists to it.

---

## 7. Cost model and teardown

Credit position: **~140 promotional credits already held**, rising to ~160 once an
EC2 instance is running and ~180 with the other credit activities. Rough
`us-east-1` on-demand estimates for this stack:

| Resource | Approx. monthly |
|---|---|
| EC2 `t4g.large` (exhibit, always on) | ~$49 |
| Root gp3 volume, 32 GB | ~$2.60 |
| RDS `db.t4g.micro` PostgreSQL, single-AZ + 20 GB gp3 | ~$14 |
| Secrets Manager, 1 secret | ~$0.40 |
| Lambda + Function URL (demo traffic) | ~$0 (free tier) |
| VPC, subnets, IGW, route tables, security groups | $0 |
| Route 53 hosted zone (if used for TLS, §8) | ~$0.50 |
| **Total** | **~$66/month** |

At ~$66/month the stack runs for the intended month well inside the remaining
credit (~$140 held, plus ~$40 earned by running EC2 and RDS). **Cost is not a binding constraint on this design** — so do not let it
drive architecture decisions, and do not trim the exhibit to save single-digit
dollars.

**Why `t4g.large` rather than `t4g.micro`.** `serve-web` builds an embedder and
`/api/recall` embeds the judge's query with it. The live sessions were written
with `bge_m3`, and `resolve_backends` enforces only the vector *width* — so a
`fixture` embedder resolves cleanly and then ranks against a vector space the
stored embeddings do not share, with no error anywhere. The exhibit therefore
runs BGE-M3 itself via llama.cpp, which does not fit in the 1 GiB of a
`t4g.micro`. The model served is the **FP16** build pinned by sha256, byte-identical
to the one the operator's own llama.cpp serves and therefore to the one that wrote
every vector in the store — a quantized build would be close enough to look right
and be subtly wrong. The extra ~$43/month buys the portal's semantic recall being
real rather than plausible.

Two things still matter, for reasons other than the bill:

- **Keep the Lambda outside the VPC anyway.** With this budget a NAT gateway is
  affordable, but it would still be the wrong call: the Lambda reads CockroachDB
  Cloud over the internet and has no reason to reach RDS, so a NAT would add a
  failure mode and a moving part in exchange for nothing. The argument is
  architectural, not financial.
- **Still write the teardown script.** Not to protect credits, but so the exhibit
  can be rebuilt from scratch on demand and so nothing is left half-deleted by
  hand after the event. Write `scripts/aws-infra/teardown.py` alongside the
  provisioning scripts, deleting in dependency order — Lambda, EC2, RDS (skip
  final snapshot), security groups, subnets, route tables, IGW, VPC, then the
  secret. Tag every resource `Project=lambo-cloudops` at creation so teardown can
  find them, and verify with a tag-filtered `describe-*` sweep that comes back
  empty.

## 8. TLS for the judge URL

`https://<EC2-Public-IP>` cannot be served with a trusted certificate: public CAs
do not issue for bare IP addresses, so Caddy's automatic HTTPS has nothing to
work with and judges would meet a browser warning on the exhibit's front door.

Pick one before provisioning:

- **A hostname you control** (existing domain, or a Route 53 hosted zone at
  ~$0.50/month) with an A record to the instance's Elastic IP. Caddy then issues
  and renews automatically. Recommended, and the cost is already in §7.
- **Cloudflare Tunnel** — gives a public HTTPS hostname with no inbound ports and
  no certificate handling, so the security group needs no 80/443 ingress at all.
  Free, at the cost of one more moving part.

Whichever is chosen, restrict SSH ingress to the operator's own address rather
than `0.0.0.0/0`. Port 443 is the only thing the public needs.

Exposure is otherwise sound: `lambo serve-web` is read-only, and that is
test-enforced — `serve_web.rs` greps its own production source for
`Memory::builder`, `open_writer`, `acquire_lease` and `.spawn()`. The instance
profile should be scoped to reading exactly the one secret, nothing wider.

---

## 9. Phase 5 scope — decided: cut to what exists

**Decision (rev 2): the per-agent timeline is cut.** Rationale in §5. Phase 5
therefore needs **no Rust change**, and `execution_mode`'s "zero core Rust
changes" holds for the whole plan.

What `lambo serve-web` exposes today, and what the portal ships on:

```
/  /app.css  /app.js  /healthz
/api/session  /api/recall  /api/events  /api/stats  /api/pulse
```
(`src/cli/serve_web.rs:736`)

The canonization audit trail comes from `/api/events` + `/api/stats`, and live
recall from `/api/recall`. That is the substance of the exhibit.

For the record, the event payload — note the absence of an agent field, and see
§5 for why that absence is correct rather than a gap to fill:

```rust
struct WebEvent {
    seq: usize,
    occurred_at: String,
    node_id: String,
    content: Option<String>,
    from_status: &'static str,
    to_status: &'static str,
    blast_radius: Option<i32>,
}
```

### Still blocked: deterministic replay

The "exact diff proof demonstrating identical graph convergence" is on hold
regardless of scope. `binary_parity`'s byte-identical demo assertion currently
fails about one run in ten, because time-derived `recency` in the daemon score
varies between runs. `evidence/demo-live-diff.txt` recorded `IDENTICAL` for two
runs that passed by luck, and warrants annotation once the fix lands.

Do not put a determinism claim in front of judges until that is green. The fix
is in flight; the v0.1.0 release depends on it too.

---

## 10. Implementation Checklist

### Phase 0: Decisions to make before provisioning anything
- [ ] **TLS**: pick a hostname strategy (§8). Provisioning an Elastic IP before
      this is decided risks redoing the Caddy and security-group config.
- [ ] ~~Phase 5 scope~~ — **decided**: cut to existing endpoints (§5, §9). No
      Rust change; nothing further to choose.

### Phase 1: Local Binary Validation (Prerequisite)
- [ ] **Parity determinism must be green first.** `binary_parity`'s byte-identical
      demo assertion currently fails ~1 run in 10 (time-derived `recency` in the
      daemon score varies between runs). Fix is in flight; §5.2's replay claim and
      the v0.1.0 release both depend on it. Do not start Phase 6 recording until
      100 consecutive runs pass.
- [ ] Run full unit and integration test suite: `cargo test --all-features`.
- [ ] Verify `lambo serve`, `lambo serve-web`, `lambo derive`, `lambo record-action`, and `lambo recall` against local SQLite & live CockroachDB.
- [ ] Ensure binary build outputs are clean and ready for packaging.

### Phase 2: CockroachDB Agent Skill (`skills/lambo-cloudops/`)
- [ ] Create `skills/lambo-cloudops/SKILL.md` defining the Lambo Memory & CockroachDB safety rules for agents.

### Phase 3: AWS Resource Provisioning Scripts (`scripts/aws-infra/`)
- [ ] `provision_network.py` (VPC, Subnets, Gateways, Route Tables, SGs, Secrets Manager).
- [ ] `provision_app_data.py` (RDS instance, Lambda function with Function URL).
- [ ] `launch_exhibit_ec2.py` (EC2 `t4g.micro` with Caddy + `lambo serve-web` systemd service).
- [ ] Tag every resource `Project=lambo-cloudops` at creation, so teardown can find it.
- [ ] EC2 instance profile scoped to reading exactly `lambo/cockroach-dsn` — no wider.
- [ ] SSH ingress restricted to the operator address, not `0.0.0.0/0`.
- [ ] `teardown.py` (§7), written in this pass, not after the event.

### Phase 4: CloudOps Multi-Agent Orchestration (`scripts/cloudops/`)
- [ ] `01_network_agent.py`: Executes Network Agent actions and feeds derivations into Lambo.
- [ ] `02_app_data_agent.py`: Executes App Agent actions, queries Lambo, and links dependencies.
- [ ] `03_crossover_protect.py`: Executes the destructive query, verifies Lambo's blast-radius warning, and renders outcome.

### Phase 5: Judge Web Portal (no Rust change)
- [ ] Confirm the canonization audit trail renders from `/api/events` + `/api/stats`.
- [ ] Confirm live recall works against the session through `/api/recall`.
- [ ] Do **not** build the per-agent timeline — cut, see §5.

### Phase 6: Verification & Demo Recording
- [ ] Verify judge URL (§8 hostname) renders the live `lambo serve-web` session window and the canonization audit trail.
- [ ] Verify Lambda Function URL returns live stats.
- [ ] Record 3-minute video covering the multi-agent workflow and blast-radius protection.
- [ ] Replace the README's "AWS services used: None yet" with §11's text.
- [ ] Run `teardown.py` in a scratch region or account once, to prove it works
      before it is ever needed for real.

---

## 11. AWS services used — submission text

**Lead with the argument, not the inventory.** The point of this scenario is not
that it touches six AWS services. It is that autonomous agents are already
provisioning real AWS infrastructure, and that the failure mode — one agent
tearing down a shared security group another agent's workload depends on — is a
production outage that flat vector memory cannot see coming. Lambo makes the
dependency structure legible and stops the destructive action before it lands.

That is a direct answer to a problem AWS customers have today, demonstrated on
live AWS resources. A service checklist is the weaker claim, and a submission
that leads with it invites the question "so what?". Lead instead with the
outage that did not happen, and let the table below be supporting detail.

Every line is a service actually exercised by this scenario; none is aspirational.

| Service | How this project uses it |
|---|---|
| **Amazon EC2** | Hosts the public judge portal: `lambo serve-web` behind Caddy, serving the read-only view of a live session at [lambo.nryn.dev](https://lambo.nryn.dev). Read-only is test-enforced, not merely intended. **As built:** an `m7i-flex.large` on x86_64 running Ubuntu 26.04, not the `t4g.micro` this plan first assumed, because BGE-M3 does not fit in 1 GiB. The arm64 branch of the launcher is not exercised. |
| **Amazon VPC** (subnets, route tables, internet gateway, security groups) | The network the two agents provision and that Lambo tracks as graph nodes. The shared security group and private subnet are the load-bearing pillars whose blast radius the demo protects. |
| **AWS Secrets Manager** | Stores the CockroachDB DSN. The exhibit instance resolves it at boot through an instance profile, so no connection string is baked into user data, an AMI, or the repo. |
| **AWS Lambda** (Function URL) | A public read-only stats endpoint over the live CockroachDB session, [live and answering 200](https://uwvhgfb2rothsct6pnl44edk3q0kazsl.lambda-url.us-east-1.on.aws/). Runs outside the VPC, since it reads an internet-facing database. It 403d until T10 found that a public Function URL has required both `lambda:InvokeFunctionUrl` and `lambda:InvokeFunction` in its resource policy since October 2025. |
| **Amazon RDS for PostgreSQL** | Provisioned by the app-data agent as the private-tier workload. Its dependency on the shared security group and private subnet is precisely what the blast-radius warning protects. It is not a Lambo store — see §6. |
| **AWS IAM** | Instance profile and Lambda execution role, each scoped to the one thing it needs. |

**If Bedrock access clears**, add Amazon Titan Text Embeddings V2 as the dense
embedder behind the reserved `embed-bedrock` feature — that would be the only
entry in this table where AWS runs inside Lambo rather than around it, and it is
worth the upgrade. Until the account's model-access form is approved it stays out
of the table; `evidence/bedrock-blocked.txt` records the refusal.
