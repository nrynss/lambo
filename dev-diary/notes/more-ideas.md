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
