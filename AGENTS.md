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

- Authority, highest first: the source, then the reference docs (`docs/reference/`, mirrored in `site/src/content/docs/`; keep each pair in sync with `scripts/docs/check-mirror-drift.sh`), then memory. Operational runbooks live in `docs/operations/`.
- The v0.1 hackathon spec (`lambo-hackathon-spec-v0.1.md`) and the `dev-diary/` tree were removed from the repo because Lambo has outgrown them; they are history, never authority. Code comments, migrations, the CHANGELOG and `evidence/` still cite `spec §N` and `dev-diary/` paths, which name those retired files. To read one, run `git show v0.15.0:lambo-hackathon-spec-v0.1.md` (or `git show v0.15.0:<dev-diary path>`). Where a retired file disagrees with the code, the code is right; do not restore either file or cite it as a rule.
- Never commit `.env`. Cockroach DSN uses `sslmode=verify-full&sslrootcert=system`.
- Single-writer deployment model: each session has exactly one writer, enforced by a store lease (see `docs/reference/api.mdx#the-single-writer-lease`).
- **Level B pluggability** (rules: `docs/reference/config.mdx`, "Features"):
  - Cargo **features** compile adapters in (`store-*`, `embed-*`, `recall-elastic`).
  - **`lambo.toml` / env** select among compiled kinds. Each process builds its store
    and embedder once, through a resolver in `src/resolve.rs` (`resolve_backends` /
    `resolve_from_config_path` -> `ResolvedBackends` for commands that embed).
    Commands never call `build_store` / `build_embedder` or resolve twice.
  - Selecting an uncompiled kind or an unknown TOML key is a **hard error**.
  - New backend: follow "Adding a backend" in that section (adapter module, feature,
    registry arm, the formal `EmbedderKind.implemented` table for embedders, docs).
    Not a core fork.
  - Example config: `lambo.example.toml`.
- **Vector width belongs to the store** (`GraphStore::vector_dimensions()`), not the
  embedder factory. `embedder.dim` is the expected width (default 1024 for every kind).
  Resolve refuses a mismatch; for SQLite the reported width echoes `[store] vector_dim`
  or the embedder, so the pin check and the session contract are what protect it.
- **Embeddings are portable** (see `docs/reference/config.mdx`, "Embedder" and "Features"):
  - Default: `bge_m3` (`embed-bge`), reference setup a BGE-M3 GGUF from Hugging Face
    in llama.cpp's `llama-server` (`scripts/fetch-bge-m3.sh`, `scripts/run-llama-embed.sh`).
  - Other kinds: `candle`, `gemini`, `embeddinggemma2`, `elastic`. `embed-bedrock` is
    reserved and not implemented.
  - Tests: `FixtureEmbedder` (`embed-fixture`). Never mix model vectors in one session
    without `lambo re-embed`; the `EmbeddingContract` on `GraphSnapshot` is enforced at
    writer attach and on vector reads.
  - Never commit `models/` weights.

## Consulting memory (Lambo MCP)

If a Lambo MCP server (`lambo-dogfood` or an infrastructure session) is attached,
use it as shared multi-agent memory alongside the repo. For repository
development work (no server attached = no obligation), never block on an outage:
fall back to the source and the reference docs and note it. For destructive infrastructure actions,
stay fail-closed and stop until recall succeeds (see
`skills/lambo-cloudops/SKILL.md`).

### Before acting
- **Recall early, then refine:** Call `lambo_recall` on the task topic (or target
  AWS/infra resource) *before* reading code or editing; run a second targeted recall
  once you learn exact symbol or resource names.
- **Authority order:** Memory is prior context, not unquestioned authority. For code,
  `source → reference docs → memory`; for infra, memory answers *"would this break
  something another agent built?"* while live provider state answers what exists now.
  On conflict, name it explicitly and update memory with `against`.
- **Triage warnings (don't false-halt, never ignore real conflicts):**
  - *Act immediately:* `⚠ Contradicted`, `⚠ Rests on a contradicted concept`,
    `⚠ Possibly superseded`, `Reserved by`, or recent-write conflict lines. Read the
    contradicting concept and list stale dependents with `lambo_stale` when cleaning up.
  - *Scope-check `⚑ Load-bearing pillar`:* Canonical-first ordering can surface
    incidental Canonical hits from unrelated subsystems. Treat `⚑` as **blocking only
    when your change modifies, removes, or breaks that concept's surface or resource** —
    then call `lambo_inspect` on its ID or durable name (with matching `scope` for
    layer hits) and name the dependents before editing.

### Writing back
- **Derive decisions, not logs:** Call `lambo_derive` at decision time with one atomic
  fact + its *why* (`logic` or `constraint`). Activity (merges, review rounds, binary
  re-pins, infra changes) goes to `lambo_record_action`.
- **Cite what you relied on:** Pass the 8-hex ID or durable name from recall in
  `depends_on` (on `lambo_derive` concepts or `lambo_record_action`). Both 8-hex IDs
  and durable names in `depends_on` and `against` must belong to the same target
  session (when layered, `hits[].session` matches target).
- **Contradict stale facts explicitly:** When new evidence invalidates a recalled
  concept, pass its 8-hex ID or durable name in `against` (same-session rule applies;
  never both `depends_on` and `against` for the same concept in one call, and never
  `against` a `replacement_candidates` entry marked `newer`).
- **Name standing rulings durably (`concepts[].name`):** Give a 2–4 word lowercase
  kebab-case `<domain>-<subject>` name (no slashes; e.g. `single-writer-lease`,
  `vector-dim-authority`) **only** to settled `logic`/`constraint` rulings or shared
  infra pillars meant to be cited across workstreams. Never name `observation`s,
  one-off findings, or file paths (names stay permanently reserved even after GC).
  Wait via `lambo_stats(receipt, wait_ms)` before citing a brand-new name in the same
  turn. Reference resolution differs by tool: `lambo_derive` refuses unknown
  references, whereas in `lambo_record_action.depends_on` unregistered text creates a
  new `Entity` concept (or a `Resource` for path-like entries under `repo_scope`;
  registered names whose target was deleted, and retired alias IDs, are still refused) —
  only pass durable names already confirmed in recall or an `applied` (or
  `applied_after_restart`) receipt.
- **Scope & paths (dogfood):** Write Lambo repo decisions without `scope` (session
  `lambo`), using `repo_scope: "github.com/nrynss/lambo"` on new path-like `resource`
  concepts and actions (older unscoped concepts must still be cited by ID). Write
  cross-project preferences with `scope: "general"` (no bare relative paths). Shared
  Lambo design surfaces that warrant recall + citation: the dialect trait, width/pin
  semantics, quarantine/canonization rules, recall constants, and CI matrix rows.
- **`agent_id`:** Use the exact model ID for this session (e.g. `claude-opus-5-5`,
  `gpt-6.2-sol`, `gemini-4-neon`; harness name only as fallback). Keep it identical
  across every call — never invent per-task or per-role IDs (add a stable instance
  suffix if concurrent sessions must not share soft locks).
