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
- Single-writer deployment model: each session has at most one writer at a time, enforced by a store lease (see `docs/reference/api.mdx#the-single-writer-lease`).
- **Level B pluggability** (rules: `docs/reference/config.mdx`, "Features"):
  - Cargo **features** compile adapters in (`store-*`, `embed-*`, `recall-elastic`).
  - **`lambo.toml` / env** select among compiled kinds. Each process builds the
    configured store and embedder once, through a resolver in `src/resolve.rs`
    (`resolve_backends` / `resolve_from_config_path` -> `ResolvedBackends` for
    commands that need both; store-only, embedder-only and session split/verify/
    classify have their own). Commands never call `build_store` /
    `build_embedder` or resolve twice.
  - Selecting an uncompiled kind or an unknown TOML key is a **hard error**.
  - New backend: follow "Adding a backend" in that section (adapter module, feature,
    registry arm, the formal `EmbedderKind.implemented` table for embedders, docs).
    Not a core fork.
  - Example config: `lambo.example.toml`.
- **Vector width belongs to the store** (`GraphStore::vector_dimensions()`), not the
  embedder factory. `embedder.dim` is the expected width (default 1024 for every kind).
  The width is checked in layers, and each layer fails closed before a session is served:
  - At resolve, two checks can refuse: a `[store] vector_dim` pin that differs from the
    embedder's width (every store kind), and an embedder whose width differs from
    Cockroach's `VECTOR(n)`, parsed from the shipped DDL. For SQLite and Postgres,
    `vector_dimensions()` at resolve is the pin if one is set, otherwise the embedder's
    own width, so `check_vector_compatibility` there compares a number with itself and
    never refuses.
  - Postgres: the live `vector(n)` column is probed against that width at init
    (`provision`) and at attach (the schema preflight).
  - Every store: the session's durable embedding contract (kind, model, width) is checked
    against the embedder at writer attach and on vector reads, and SQLite's write gate
    refuses a vector whose width differs from it.
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
  session (when layered, `hits[].session` matches target). *Enforced* in `depends_on`
  and `against` on `lambo_derive` and in `against` on `lambo_record_action`: a reference
  that names no concept in the target session refuses the whole call, as cross-session
  when it names one in another session the call reads. `lambo_record_action.depends_on`
  creates unregistered text instead (below), so check the session yourself there.
- **Contradict stale facts explicitly:** When new evidence invalidates a recalled
  concept, pass its 8-hex ID or durable name in `against` (same-session rule applies).
  - *Enforced:* one concept (or action) must not name the same target in both its
    `depends_on` and its `against`; the server refuses the whole call. The check is per
    citing concept, by the `(content, target)` pair after resolution, so an ID and a name
    for one concept still collide. Another concept in the same call, or a later write,
    may cite what this concept contradicts.
  - *Advisory, not checked:* never `against` a `replacement_candidates` entry marked
    `newer`. The server accepts it and writes the `Contradicts` edge, but a `newer`
    candidate is the possible replacement of your concept, never the one it replaces
    (`selectF_newer_never_replaced_by_new` in
    `formal/LamboProofs/Supersession/Suspected.lean`).
- **Name standing rulings durably (`concepts[].name`):** Give a 2–4 word lowercase
  kebab-case `<domain>-<subject>` name (no slashes; e.g. `single-writer-lease`,
  `vector-dim-authority`) **only** to settled `logic`/`constraint` rulings or shared
  infra pillars meant to be cited across workstreams. Never name `observation`s,
  one-off findings, or file paths (names stay permanently reserved even after GC).
  *Advisory, not checked:* the shape and what to name; the server accepts any spelling
  and any concept type. *Enforced:* the name's key. Two names are the same name when
  their keys match, and the first spelling to take a key owns it for good; any spelling
  with that key resolves to that concept. A name is reduced to its key in these steps:
  1. Trim surrounding whitespace.
  2. Erase invisible characters, then apply Unicode NFC. The few a name may contain
     (zero-width joiner and non-joiner, variation selectors and the like, which some
     scripts and emoji need) are erased from the key, so the words on either side join
     into one; any other invisible character refuses the name.
  3. Split into words at `-`, `_` and any whitespace, and at camelCase boundaries. A
     camelCase boundary is only an ASCII lowercase letter directly followed by an ASCII
     uppercase letter.
  4. Lowercase.
  5. Drop the stopwords: the, a, an, for, of, at, in, to, on, and, or, is, are.
  6. Reduce each word to its English (Snowball) stem, except words containing a digit,
     `.` or `/`, which are kept as written apart from case. Empty stems are dropped.
  7. Sort the words.

  These collide: word order (`single-writer-lease`, `lease-writer-single`,
  `writer single lease`); separators (`-`, `_`, spaces and tabs are interchangeable, and
  a run counts once); case (`SINGLE_WRITER_LEASE`); camelCase (`SingleWriterLeases`,
  `singleWriterLease`; `XmlParser` is `xml-parser`, `leaseV2` is `v2-lease`; a camelCase
  boundary splits even inside a dotted or slashed piece, so `src/MyFile.rs` is
  `File.rs src/My`); word forms (`leases`, `leasing`, `lease`); stopwords (`the-lease`
  is `lease`, `the-lease-of-the-writer` is `writer-lease`). These do not: a run-together
  word (`singlewriterlease`), a run of capitals (`XMLParser` is one word), a digit
  (`v2Lease` is one word), dotted or slashed pieces without a camelCase boundary
  (`single.writer.lease`; `leases/v2` is not `v2/leases`, though case is still
  ignored), repeated words (`lease lease` is not `lease`), a stopword glued into a word
  (`thelease`). Keys are not stable under re-typing: the key
  `leas singl writer`, typed back as a name, keys as `lea singl writer`. Always reuse
  the original spelling, never a key. A name is refused when another name
  already holds its key (`NameTaken`); when another concept's canonical key is the same
  (normally its content's key after graph synonyms; observations count; the name's own
  target does not block it); when its key is empty (only stopwords or separators:
  `the`, `the-of`); when it is id-shaped (a UUID, or exactly 8 hex digits such as
  `DeadBeef`); or when it fails the size or character check (control and invisible characters). Proved in
  `formal/LamboProofs/Canonical.lean` (`nameKey_eq_iff_perm`, `keyF_eq_iff_perm`,
  `issue290_order`, `issue290_stopword`); the code is `name_key` in
  `src/graph/graph/names.rs` and `canonical_key` in `src/graph/canonical.rs`.
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
