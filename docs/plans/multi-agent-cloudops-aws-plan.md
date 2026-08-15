# Plan: Multi-Agent CloudOps Demo & Real AWS Infrastructure Protection

```yaml
status: PROPOSED / READY FOR EXECUTION POST-BINARY-VALIDATION
owner: nryn
target_hackathon: CockroachDB AI Hackathon on Devpost
target_services: AWS (EC2, Lambda, RDS, Secrets Manager, VPC) + CockroachDB Cloud
execution_mode: External scripts driving Lambo as a CLI/MCP binary (Zero core Rust changes)
```

---

## 1. Overview & Problem Statement

Multi-agent autonomous systems managing cloud infrastructure (CDK, Terraform, CloudFormation, AWS CLI) lack unified, dependency-aware memory. 

- **The Danger**: `Agent 1` provisions foundation network & perimeter security (VPCs, Subnets, Shared Security Groups). `Agent 2` independently attaches workloads, databases, and serverless functions into that network. When `Agent 1` or an automated drift-remediation routine attempts to deprecate or delete "idle" or "unused" base security groups, flat vector RAG cannot calculate the cross-agent blast radius, causing catastrophic production outages.
- **The Solution (Lambo)**: Lambo maintains a bipartite graph memory in CockroachDB. It automatically promotes foundational cloud infrastructure to **`Canonical`** pillars through earned structural evidence. When any agent queries or attempts a destructive action against a shared resource, Lambo issues **blast-radius warnings** and **recency conflict alerts**.

---

## 2. Real AWS Infrastructure Architecture (`us-east-1`)

This architecture earns the remaining **$60 in AWS Promotional Credits** while maintaining a real, functional cloud stack for the judge exhibit and demo video.

```
                                  AWS CLOUD (us-east-1)
┌────────────────────────────────────────────────────────────────────────────────────────┐
│  VPC: VPC-Enterprise-Prod (10.0.0.0/16)                                                │
│                                                                                        │
│   ┌──────────────────────────────────┐      ┌──────────────────────────────────────┐   │
│   │ Public Subnet (10.0.1.0/24)      │      │ Private Subnet (10.0.2.0/24)         │   │
│   │                                  │      │                                      │   │
│   │  ┌────────────────────────────┐  │      │  ┌────────────────────────────────┐  │   │
│   │  │ EC2-LamboWebExhibit        │  │      │  │ RDS-Lambo-Demo-DB              │  │   │
│   │  │ (t4g.micro - ARM64)        │  │      │  │ (db.t4g.micro Postgres)        │  │   │
│   │  │ • Caddy Reverse Proxy (443)│  │      │  │ • Data store tier              │  │   │
│   │  │ • lambo serve-web (7710)   │  │      │  │ • [$20 Credit Activity]        │  │   │
│   │  │ • [$20 Credit Activity]    │  │      │  └────────────────────────────────┘  │   │
│   │  └──────────────┬─────────────┘  │      │                 ▲                    │   │
│   └─────────────────┼────────────────┘      └─────────────────┼────────────────────┘   │
│                     │ SG-PublicWeb (80/443/22)                │ SG-Base-VPC (Internal) │
│                     │                                         │                        │
│   ┌─────────────────┴─────────────────────────────────────────┴────────────────────┐   │
│   │ Serverless App Tier: Lambda-LamboStats-API (Python 3.12 + Function URL)        │   │
│   │ • Read-only CockroachDB & AWS Cloud health dashboard                           │   │
│   │ • [$20 Credit Activity]                                                        │   │
│   └─────────────────────────────────┬──────────────────────────────────────────────┘   │
│                                     │                                                  │
│   ┌─────────────────────────────────┴──────────────────────────────────────────────┐   │
│   │ AWS Secrets Manager: lambo/cockroach-dsn                                       │   │
│   │ • Runtime secure credential resolution via asm-exec                            │   │
│   └────────────────────────────────────────────────────────────────────────────────┘   │
└────────────────────────────────────────────────────────────────────────────────────────┘
```

---

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

## 5. Implementation Checklist

### Phase 1: Local Binary Validation (Prerequisite)
- [ ] Run full unit and integration test suite: `cargo test --all-features`.
- [ ] Verify `lambo serve`, `lambo serve-web`, `lambo derive`, `lambo record-action`, and `lambo recall` against local SQLite & live CockroachDB.
- [ ] Ensure binary build outputs are clean and ready for packaging.

### Phase 2: CockroachDB Agent Skill (`skills/lambo-cloudops/`)
- [ ] Create `skills/lambo-cloudops/SKILL.md` defining the Lambo Memory & CockroachDB safety rules for agents.

### Phase 3: AWS Resource Provisioning Scripts (`scripts/aws-infra/`)
- [ ] `provision_network.py` (VPC, Subnets, Gateways, Route Tables, SGs, Secrets Manager).
- [ ] `provision_app_data.py` (RDS instance, Lambda function with Function URL).
- [ ] `launch_exhibit_ec2.py` (EC2 `t4g.micro` with Caddy + `lambo serve-web` systemd service).

### Phase 4: CloudOps Multi-Agent Orchestration (`scripts/cloudops/`)
- [ ] `01_network_agent.py`: Executes Network Agent actions and feeds derivations into Lambo.
- [ ] `02_app_data_agent.py`: Executes App Agent actions, queries Lambo, and links dependencies.
- [ ] `03_crossover_protect.py`: Executes the destructive query, verifies Lambo's blast-radius warning, and renders outcome.

### Phase 5: Verification & Demo Recording
- [ ] Verify judge URL (`https://<EC2-IP-or-Domain>`) renders live `lambo serve-web` session window.
- [ ] Verify Lambda Function URL returns live stats.
- [ ] Record 3-minute video covering the multi-agent workflow and blast-radius protection.
