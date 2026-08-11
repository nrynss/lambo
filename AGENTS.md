# Lambo — agent rules

## AWS Guidance

- Prefer the AWS MCP Server for AWS interactions — it provides sandboxed
  execution, observability, and audit logging. If unavailable, use the
  AWS CLI directly.
- Before starting a task, check whether a relevant AWS skill is available.
  Load the skill and prefer its guidance over general knowledge.
- When uncertain about specific AWS details (API parameters, permissions,
  limits, error codes), verify against documentation rather than guessing.
  State uncertainty explicitly if you cannot confirm.
- When creating infrastructure, prefer infrastructure-as-code (AWS CDK or
  CloudFormation) over direct CLI commands.
- When working with infrastructure, follow AWS Well-Architected Framework
  principles.
- Do not use em dashes in AWS resource names or descriptions. Use
  hyphens instead.

## Secret Safety

- MUST load the `aws-secrets-manager` skill first for any secret,
  credential, API key, token, or password task. MUST NOT call
  `secretsmanager get-secret-value` or `batch-get-secret-value`, and MUST
  NOT hit the Secrets Manager Agent daemon directly. MUST use
  `{{resolve:secretsmanager:secret-id:SecretString:json-key}}` with
  `asm-exec` so the secret resolves at runtime without entering context.

## Lambo project notes

- Spec of record: `lambo-hackathon-spec-v0.1.md` (frozen). Phase handoffs in `dev-diary/`.
- Never commit `.env`. Cockroach DSN uses `sslmode=verify-full&sslrootcert=system`.
- Single-writer deployment model for `lambo serve` (see spec §2.2).
- **Level B pluggability** (design of record: `dev-diary/notes/level-b-pluggability.md`):
  - Cargo **features** compile adapters in (`store-*`, `embed-*`).
  - **`lambo.toml` / env** select among compiled kinds; process start uses
    `resolve_backends` / `resolve_from_config_path` (**single construction site** →
    `ResolvedBackends`). Do not rebuild store/embedder inside each command.
  - Selecting an uncompiled kind or unknown TOML key is a **hard error**.
  - New backend = adapter module + feature + registry arm + docs — not a core fork.
  - Example config: `lambo.example.toml`.
- **Vector dim is store-authoritative** (`GraphStore::vector_dimensions()`), not hardwired
  in the embedder factory. Config `embedder.dim` is expected embedder width (default 1024
  for BGE demos). Compatibility is checked at resolve time.
- **Embeddings are portable** (see `dev-diary/notes/embeddings-portable.md`):
  - Default: **BGE-M3** from **Hugging Face**, run with **llama.cpp** (`embed-bge`).
  - Swap-in: **Bedrock Titan V2** when authorized (`embed-bedrock`, T7.1).
  - Tests: `FixtureEmbedder` (`embed-fixture`). Never mix model vectors in one session
    without re-embed — use `EmbeddingContract` on `GraphSnapshot`.
  - Never commit `models/` weights.
