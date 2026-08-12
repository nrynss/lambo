# Lambo — Post-P3 Enhancement Ideas & AWS Integration Strategy

```yaml
created:    2026-08-11
status:     PROPOSED / OPTIONAL ENHANCEMENTS
context:    P3 completing early (2 days ahead of schedule); 7 days remaining to Aug 18 submission deadline.
```

---

## 1. AWS Strategic Pivot — Lambo for AWS DevOps & Cloud Management

If Amazon Bedrock Titan V2 authorization remains pending, Lambo can pivot its primary AWS story to **Agentic Memory for AWS Infrastructure & CloudOps Agents**.

### Problem Statement
Multi-agent systems executing AWS CDK, CloudFormation, or AWS CLI commands lack dependency awareness. If Agent A provisions a VPC and CockroachDB cluster, and Agent B later attempts to clean up "unused" security groups or subnets, flat vector RAG cannot warn Agent B of cross-agent infrastructure dependencies.

### The Solution: Lambo CloudOps Protection
1. **AWS Concept Derivation**: Agent actions derive cloud resource concepts (`VPC-Prod`, `IAM-ExecutionRole`, `ECS-TaskDef`, `CockroachDB-Cluster`).
2. **Infrastructure Canonization**: Shared network and security components earn `Canonical` status as load-bearing infrastructure pillars.
3. **Blast Radius Protection**: Destructive commands (e.g., `aws ec2 delete-vpc` or security group deletion) trigger Lambo's blast-radius calculation across dependent AWS concepts:
   > `⚑ Load-bearing AWS pillar — 14 dependent CloudFormation stacks / ECS tasks rely on this resource. Modification blocked.`

### AWS Service Implementations
* **AWS Lambda Canonization Worker** (Spec §12.2): A Rust (`lambda_runtime`) or Python Lambda function triggered via **AWS EventBridge** cron to perform background canonization sweeps & GC on idle sessions in CockroachDB.
* **AWS Secrets Manager**: Secure runtime resolution of CockroachDB DSN (`DATABASE_URL`) and MCP credentials using `asm-exec`.
* **AWS App Runner / ECS Fargate**: Hosting for `lambo serve` container for cloud-native MCP client access over HTTPS.

---

## 2. Super-Demo & Visualizer Force-Multipliers

### A. Live Interactive Graph Visualizer (`lambo UI`)
* **Concept**: Expose an embedded web dashboard (`/visualizer`) via Axum in `lambo serve` using D3.js or Cytoscape.js.
* **Feature**: Real-time visualization of the bipartite graph. Nodes promote visually to **Canonical (glowing Gold)**, and `Dependency` edges turn red when blast-radius warnings trigger during agent execution.
* **Value**: Converts terminal log outputs into a visually striking demo video.

### B. Automated Multi-Agent Scenario Harness (`lambo run-demo`)
* **Concept**: Scripted multi-agent scenario runner (`cargo run --features demo -- run-demo`).
* **Workflow**:
  1. Agent A provisions `user schema`, `auth middleware`, and `session store`.
  2. Lambo canonizes `user schema` through structural evidence.
  3. Agent B attempts a breaking schema change.
  4. Lambo emits the `Load-bearing pillar` blast radius warning.
* **Value**: Guarantees a 100% deterministic, reproducible run for the 3-minute submission video.

### C. Benchmark & Evaluation Harness (Lambo vs. Flat Vector RAG)
* **Concept**: A comparative evaluation script testing multi-hop reasoning and breaking-change detection.
* **Metric**: Compare Lambo's graph recall + blast-radius protection against standard top-$K$ flat vector search.
* **Value**: Provides quantitative proof that associative structure + canonization outperforms flat RAG.

### D. Dual MCP Integration with CockroachDB Cloud MCP
* **Concept**: Wire Claude Code to two simultaneous MCP servers:
  1. `lambo serve` (stdio/HTTP MCP interface for active memory).
  2. CockroachDB Cloud MCP (read-only direct SQL inspection).
* **Workflow**: Claude Code inspects `canonization_events` and `edges` live on CockroachDB to explain *why* a cloud concept was canonized.

---

## 3. Recommended Execution Priority

1. **P4 Daemon & P5 Recall** (Aug 12–13)
2. **P6 Canonization & P8 Surface (MCP Serve)** (Aug 13–14)
3. **AWS Lambda Worker & AWS DevOps Demo Scenario** (Aug 15)
4. **Interactive Graph Visualizer & Demo Recording** (Aug 16–17)
5. **Submission Polish** (Aug 18)

---

## 4. Update (2026-08-12) — post-E2E scoping. NOTHING FINAL.

```yaml
status:   SCOPING ONLY — decisions land with P9 planning
context:  P0-P3 E2E review closed (45 findings dispositioned + verified);
          P4 under remediation on this branch; P5/P7 tonight, P6 ∥ P8 Aug 13
supersedes: §3's priority table (it spent the full-system review buffer;
          the current plan keeps Aug 14 as the MVP E2E day)
```

### Governing rule — extras sit ON TOP of the MVP, never inside it

- Every extra is a **reader**, an **adapter**, or **deployment packaging**.
  No core edits. An extra that needs a core change is either a bugfix in
  disguise (review it as one) or scope creep (cut it).
- After the Aug 14 MVP E2E closes, core paths are **frozen-except-bugfix**
  (`src/graph`, `src/store`, `src/daemon`, `src/recall`, `src/canon`,
  `src/mcp`). Extras land in leaf locations only: infra/scripts, a separate
  viz page, `src/embed/bedrock.rs` behind its feature, new demo fixtures.
- Every extra must be droppable on Aug 17–18 with **zero de-integration
  cost** — the §14 cut order stays cheap all the way to submission.
- The video records from a **private** run; the submission never depends on
  any extra being alive.

### Disposition of the §1–§2 ideas (per the E2E-era evaluation)

| Idea | Disposition | Why |
|---|---|---|
| Hosting (`lambo serve` on AWS) | **ADOPT — actually required** | §12.4 demands a functional demo-app URL; realized as the EC2 exhibit box below (supersedes the App Runner bullet) |
| Secrets Manager (asm-exec DSN) | **ADOPT** | cheap, real AWS-service usage, matches house secret rules |
| CloudOps pivot | **ADOPT narrative only** | Real-World Impact framing in video/writeup. Optional second scenario (`lambo demo --scenario cloud-ops`) ONLY if P8 lands early — no rework of the rest-api demo fixtures |
| Graph visualizer | **CONDITIONAL** | reader-process form only (SQL against Cockroach — the §2.2 reader story made visible); time-boxed 1 day; starts only after P8 is green; feature-gated if ever embedded in serve |
| `lambo run-demo` harness | already planned | = T8.3; not an extra |
| Dual MCP integration | already required | = spec §12.1 / §13 step 5; not an extra |
| Lambda canonization worker | **REJECT unless** Bedrock is dead AND P8 lands early AND the AWS requirement isn't already satisfied | first item on the §14 cut list; duplicates canonization logic in a second runtime; single-writer hazards |
| Benchmark vs flat RAG | **REJECT** | LoCoMo harness is spec-deferred to v0.7; a one-day benchmark is rigged-or-risky. Reduce to a scripted A/B beat inside the demo; never call it a benchmark in the writeup |
| Bedrock T7.1 roll-in | **SLOT WHEN UNBLOCKED** | decision expected Aug 12–13. If authorized: 2–3h adapter behind `embed-bedrock` + registry arm, lands in the extras window WITHOUT reopening the MVP E2E (default path unchanged). If not: one honest writeup line ("Titan V2 ready behind `embed-bedrock`, pending account authorization") |

### EC2 exhibit box (judge-facing; hackathon credits ≈ $150, spend ≈ $25–40/wk)

- **Shape:** one EC2 (t3.large ≈ $14/wk; c7i.xlarge ≈ $30/wk if llama.cpp
  runs on-box). CockroachDB stays on ccloud. Caddy for HTTPS.
- **Two tiers:** OPEN web, no auth (read-only visualizer + live recall /
  `canonization_events` view) + **SSH login with published demo
  credentials** (judges drive the lambo CLI / scripted scenario in tmux).
- **Containment** (the box WILL be probed once credentials are public —
  design goal is worthless-to-abuse + trivial-to-rebuild, not keep-them-out):
  - **No IAM instance profile** — account blast radius zero. `admin` user
    key-only; `demo` user no sudo, systemd user-slice caps (CPU/mem/tasks).
  - **Egress allowlist** in the security group (Cockroach host, Anthropic
    API if Claude Code ships on-box, DNS; SMTP blocked outright) — kills
    abuse value and the account-suspension risk.
  - **Sandbox blast radius:** dedicated sandbox database + scoped SQL user;
    hourly reseed cron heals vandalism; submission demo data never touches
    this box.
  - Any LLM key on the box is readable by design → **hard spend cap
    ($10–20), rotated after judging**. That cap is the only real control.
  - Billing alarms at $30/$60 + CPU alarm. Box is cattle: one
    CDK/user-data stack, 10-minute rebuild.
- `lambo serve` + visualizer run under a service user the demo account
  cannot kill — the single-writer/reader model, literally enforced by unix
  users on the exhibit.

### Schedule slots (current plan, replaces §3)

| Date | Slot |
|---|---|
| Aug 12 (tonight) | P4 remediation loop → review → merge; then P5, then P7 (sequenced: P7 shares `canonical.rs`/`cockroach.rs` with the remediation) |
| Aug 13 | P6 (strong agent + deep reviews) ∥ P8 (agree the T6.4-evaluator ↔ T8.1 spawn seam first; T8.3's live-arc assertion trails P6) |
| Aug 14 | **MVP E2E on the live cluster + fixes. MVP freeze after.** |
| Aug 15–16 | Extras above + their reviews; Bedrock roll-in if authorized; demo rehearsal |
| Aug 17 | P9: video, README, diagram, Devpost draft |
| Aug 18 | Submit before 5:00 pm ET |
