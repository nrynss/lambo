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

## Consulting memory before infrastructure work

If a Lambo MCP server is attached, treat it as the record of what other agents
have already built, and consult it **before** advising on or performing any
change to shared infrastructure.

- Before deleting, detaching, renaming or reconfiguring a resource, call
  `lambo_recall` with the resource name, and `lambo_inspect` on it when the
  answer is not obvious. Do this first, before reading code or searching the
  filesystem: the graph records what other agents actually provisioned, which
  source files do not.
- A load-bearing warning or a non-zero blast radius means other work depends on
  the resource. Say so plainly and name the dependents rather than proceeding.
- Recorded dependents are authoritative for the question "would this break
  something another agent built". They are not a substitute for verifying live
  provider state, so say which of the two you checked.
- After doing the work, write it back with `lambo_derive` and
  `lambo_record_action` so the next agent inherits it.

## Consulting memory during development work (dogfood)

If a `lambo-dogfood` MCP server is attached, this project's own development memory
is live (see `dev-diary/lambo-for-mooshik/DOGFOOD.md`). The rules mirror the
infrastructure section above, applied to design surface instead of resources:

- **Recall before starting a workstream** or touching shared design surface (the
  dialect trait, width/pin semantics, quarantine rules, recall constants, CI rows):
  call `lambo_recall` with the topic. Decisions recorded there are settled — do not
  re-litigate them; if one must change, say so explicitly and update the graph.
- A load-bearing warning or non-zero blast radius on a concept means another
  workstream rests on it. Treat it as blocking: name the dependents before editing.
- **Derive decisions, not activity.** After a design decision, review verdict, or
  constant change, call `lambo_derive` with the decision *and its why*. Git records
  what was done; the graph records what the next agent must not re-derive.
- **`record_action` on merges** — workstream landed, review round closed, dogfood
  binary re-pinned.
- **Cite the rulings you rely on by id** (once the dogfood writer runs 0.6.0; a 0.5.0
  writer stores an id as text). When a decision or action rests on a ruling recall
  showed you, put its id (the 8 hex characters recall shows) in `depends_on`: on
  `lambo_record_action`, or on the concept in `lambo_derive`. The ruling then gains a
  dependent, instead of a short alias Entity collecting the citation.
- **Name what a write shows to be wrong in `against`** (also 0.6.0 only; a 0.5.0 writer
  refuses the field). When a decision, finding or action contradicts a concept already in
  memory, put that concept's id in `against`, on the concept in `lambo_derive` or on
  `lambo_record_action`. It records the contradiction and changes no score; it never
  replaces the older concept.
- Use one stable `agent_id` for an agent's whole life (model or client id: `claude-opus-5-5`,
  `codex-agent`, …), never per call, task or topic. Agents sharing an id share soft locks, so
  concurrent agents that must not collide need distinct stable ids (model + stable suffix).
- No server attached = no obligation. Never block on the memory being up; fall back
  to the dev-diary and note the outage.
