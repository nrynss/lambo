# Changelog

## Unreleased (0.3.0)

### Breaking

- `lambo::canon::gate_progress` takes a `PromotionPolicy` argument (fourth
  position, before `min_edge_age`). It decides whether the four store-evidence
  gates are measured at all, so the caller cannot be trusted to check it: under
  `Solo` none of `gc_survived`, blast radius, distinct interactions or coverage
  participates in promotion, and shipping them anyway put "0 of 4 gates met"
  beside a concept due to go Canonical on the next cycle.
- `GateProgress`'s four gate fields moved into a new `SwarmGates` struct behind
  `GateProgress::gates: Option<SwarmGates>`, absent under `Solo`. Rust field
  access moves one level down, but the **move itself** changes no JSON: the
  holder is `#[serde(flatten)]`ed, so under `Swarm` the four keys serialize in
  the same place, with the same names, in the same order as before. The object
  around them is not unchanged — it gained `policy`, and under `Solo` the four
  keys are absent — so a client that only reads the four gates under `Swarm`
  needs no change, while one that enumerates the object's keys does. The
  cooldown fields stayed put deliberately: the re-promotion cooldown is
  policy-independent, and this struct is its only carrier.
- `GateProgress::met_count` returns `Option<usize>` rather than `usize`: `None`
  when the live policy does not read the four gates, so a caller cannot render
  "0 of 4" for a concept whose policy has no such count.
- `LamboFile` gained a public `promotion_policy: Option<PromotionPolicy>`
  field. The struct has all-public fields and no `#[non_exhaustive]`, so every
  external struct-literal construction and exhaustive destructuring of it
  breaks. `..Default::default()` does not.

- `lambo serve` exits **0**, not 1, when a client disconnects before completing
  the MCP `initialize` handshake. That is `rmcp`'s stream-ended case — on stdio,
  EOF on stdin — and reporting it as a config error called an ordinary
  lifecycle event a misconfiguration: the session had already closed and
  flushed cleanly, and the failing status was appended to a successful
  shutdown. It also split the contract across the handshake boundary, since an
  EOF one frame *later* already exited 0. Anything scripting `lambo serve` that
  treated a pre-handshake hangup as a failure will now see success. Every other
  handshake error stays fatal: a client opening with the wrong frame, a
  protocol violation, or a transport fault is still a non-zero exit.

- `store.kind = "postgres"` and `"pg"` now select `StoreKind::Postgres`, not
  CockroachDB. `"cockroach"` and `"crdb"` remain Cockroach. A leftover
  `kind = "postgres"` pointed at a Cockroach cluster fails at provision
  (`CREATE EXTENSION vector`) or first vector query (`<=>`) rather than
  silently ranking with Cockroach SQL.

- `ActionOutcome` gained a public `embedded: usize` field — how many of
  `created` were written with a vector. The struct has all-public fields and no
  `#[non_exhaustive]`, so external struct-literal construction and exhaustive
  destructuring of it break; field access and `..` patterns do not. It is
  reported rather than inferred so that "embedded nothing because it created
  nothing" is distinguishable from "created concepts and left them unembedded",
  which is the exact distinction whose absence hid the defect below.

### Added

- `SessionEndpoint::resolve_in` is now public: `resolve` with the endpoint
  directory supplied, for deriving the address a holder would bind in a
  directory other than this process's own.
- `promotion_policy` as a top-level `lambo.toml` key with a
  `LAMBO_PROMOTION_POLICY` environment override (non-empty env wins; an empty
  value is unset, as everywhere else Lambo overlays the environment). It
  threads onto the `Config` that `Memory::builder().config(..)` reads, so a
  `lambo serve` process selects a canonization policy without a code change.
  The default stays `Swarm`, and an unset key does not move existing
  behaviour. Both surfaces parse through `PromotionPolicy::from_str` —
  trimmed and case-insensitive, like `store.kind` and `embedder.kind` — and an
  unrecognised value is a hard startup error naming both what was written and
  the valid set, never a silent fallback to the default. There is no
  `serve --promotion-policy` flag: no `serve` flag in Lambo duplicates a
  `lambo.toml` key.
- `PromotionPolicy::ALL`, `FromStr` and `Display`. Every refusal message
  enumerates `ALL` and `from_str` searches it, so a variant missing from it
  would be unselectable from both `lambo.toml` and `LAMBO_PROMOTION_POLICY`
  while every refusal kept naming only the others. `ALL` and the enum are
  therefore generated from one variant list, length included, which makes that
  state unrepresentable rather than merely tested — a `PromotionPolicy` variant
  cannot be declared anywhere else.
- `promotion_policy` on the `lambo_stats` payload, its text summary, and the
  `serve --ledger` heartbeat. The payload and the heartbeat come from one
  shared builder (`stats_json`) and so cannot disagree; the text summary is a
  separate `format!` over the same live `Config`, so all three report the same
  value but only two of them are structurally prevented from drifting.
  Read from the live `Config`, so it reports the value that won —
  file, environment, or default. The cycle counters cannot answer the question:
  `canonization_cycles` climbs identically under both policies, and
  `canonical_count` staying at zero is both the normal reading for a young
  `Swarm` session and the whole symptom of a `Solo` selection that did not
  take.
- `/api/inspect` names its own resolution in `promotion_policy` (always
  present, including on a miss) and labels every absent gate block in
  `gate_progress_omitted` (`already_canonical` or `unavailable`). `serve-web`
  is a lease-free reader that resolves its own config, so a gate count with no
  policy beside it was unattributable; the two omission causes used to be one
  indistinguishable `null`. The page acts on the difference rather than merely
  receiving it: `already_canonical` draws nothing, and `unavailable` says the
  checks could not be read — a failed store query no longer renders identically
  to a promoted fact.
- `lambo::RESOLVE_ENV_VARS`: **every** environment variable a Level B resolve
  reads, in one place, for tests and harnesses that need the config to be
  exactly the `--config` file they wrote. Replaces five hand-maintained copies
  of the same list, which had already fallen out of step — and completes it:
  the five copies all named nine variables, omitting `LAMBO_POSTGRES_DSN` and
  the five (`LAMBO_EMBED_DEVICE`, `LAMBO_GEMINI_*`) that `EmbedderConfig`
  overlays regardless of `embedder.kind`. A harness that cleared the old list,
  wrote `store.kind = "postgres"` with no `dsn`, and ran `lambo provision` took
  its DSN from the ambient shell.
  "Every variable a resolve reads" also covers what `build_store` and
  `build_embedder` read after the file overlay, which the first version of this
  list did not: `GCP_LAMBO_CREDENTIALS`, `GOOGLE_APPLICATION_CREDENTIALS` (both
  through `gcp_auth::credentials_path_from_env`, called eagerly by the Gemini
  embedder build and by `PgStore::new` under the IAM opt-in) and
  `LAMBO_POSTGRES_IAM`. Those three pick an *identity* rather than a database:
  a harness that cleared the old list and built a Gemini embedder with no
  `gemini_credentials` authenticated as whatever service account the ambient
  shell named, and billed Vertex calls to it. `LAMBO_VECTOR_BEAM_SIZE` is
  excluded on purpose — it is read when the pool is first used, not during the
  resolve.
- `lambo::store::POSTGRES_IAM_ENV`, the `LAMBO_POSTGRES_IAM` name as a public
  const, declared unconditionally so `RESOLVE_ENV_VARS` can name it in every
  feature row. Pinned equal to the `store-postgres` const `PgStore::new`
  actually reads.

- `StoreKind::Postgres` and Cargo feature `store-postgres` (same sqlx postgres
  driver as `store-cockroach`; no second driver). B2 lands templated-width
  pgvector DDL with an hnsw index from init. Dimensions above 2000 are
  refused at init, naming the pgvector hnsw ceiling and the unimplemented
  `halfvec` hatch. `lambo provision` for `kind = "postgres"` runs
  `init_schema` (not `scripts/provision.sh`).
- DSN identity normalisation beside `store_identity`: two spellings of one
  database (`postgres://u@host/db` and `postgres://u@host:5432/db`) derive one
  session endpoint. Password is stripped from the identity so it never reaches
  the filesystem or the lease row.
- `store_is_shareable(Postgres) = true`: a networked store another process can
  open, ruled in the exhaustive match, not defaulted.
- Postgres ranking: pgvector `<=>` (cosine distance) converts with `1 - d`.
  Cockroach stays `<->` L2 and `1 - d^2/2`. Copying either formula onto the
  other dialect is pinned to fail.

- Cargo feature `embed-gemini`: Vertex `gemini-embedding-001` embeddings, with a
  dim guard for the 768 / 1536 / 3072 the model truncates to.
- `src/gcp_auth.rs`: one Google OAuth path for every adapter that authenticates
  as a Google principal, gated `embed-gemini` OR `store-postgres`. Handles both
  credential kinds (service-account key by `jwt-bearer`, authorized-user ADC by
  `refresh_token`) and asks for the caller's own scope on both grants (the signed
  `scope` claim on one, the `scope` form field on the other), so a Cloud SQL login
  and a Vertex call share an identity without sharing authority. On an ADC the ask
  can only narrow what `gcloud auth application-default login` granted; asking for
  more is refused as `invalid_scope` by the token endpoint rather than later by the
  database.
- The Gemini embedder resolves credentials from `GCP_LAMBO_CREDENTIALS` before
  `GOOGLE_APPLICATION_CREDENTIALS`, the chain the Postgres store already used, so
  one exported variable names one identity for both.
- Cloud SQL IAM database authentication for `store-postgres`, opted into with
  `LAMBO_POSTGRES_IAM` set to a **non-empty** value (empty is the ordinary password
  path, as everywhere else lambo overlays the environment): the connection password
  is an OAuth token minted from the shared credential file, and the pool is rebuilt
  when that token expires. The opt-in is PostgreSQL only
  (`Dialect::SUPPORTS_CLOUD_SQL_IAM_AUTH`), so a build carrying both adapters never
  hands a Cloud SQL token to a Cockroach cluster.
- `scripts/cloudsql-allowlist.sh`: add the running host's egress IP to a Cloud SQL
  instance's authorized networks, idempotently, preserving entries it did not add.
- Released binaries (`ship`) now carry `store-postgres` and `embed-gemini`, so a
  `lambo.toml` naming `postgres` or `gemini` runs on a prebuilt binary.
- `lambo serve` embedder keep-warm (issue #13): a holder-side task embeds one
  short fixed probe every interval and discards the vector, so an idle writer's
  recently touched model weights are more likely to still be resident when the
  next call lands, which reduces how often a call pays the swap-in. It does not
  guarantee residency under heavy memory pressure. Set with `[embedder]
  keep_warm_secs` / `LAMBO_EMBED_KEEP_WARM_SECS`: omitted means auto, `0` is
  off, `N` is every N seconds for any embedder kind. **Behaviour change for
  candle on Metal:** auto is on there (every 10 s), because those weights sit in
  unified memory the macOS pager compresses. Everything else defaults off: CUDA
  weights live in VRAM, the fixture has no weights, and `bge_m3`/`gemini` hold
  their weights in another process. A touch writes nothing (no store I/O, graph
  mutation, ledger line or recall-cache entry), is not part of the embedding
  contract, and first fires one interval after startup, so the handshake gains
  no work. Proxies and one-shot CLI commands never run it.

### Fixed

- A `lambo serve` that loses the election and proxies to the session holder no
  longer keeps its resolved embedder alive. It used to hold the full model for
  as long as its client stayed attached (with candle on Metal, ~1.1 GB of
  weights plus the coalescer threads) without ever embedding; the backends are
  now released before the proxy starts. Pre-existing, found in the issue #13
  review.
- Canonization now fires in long-running low-write deployments: the mutation
  epoch the GC interval measures persists with the session instead of resetting
  on every writer start (issue #17). Thirteen days of dogfooding produced zero
  consolidations because the chain was closed by construction — Stage 1 needs
  `gc_survived >= 3`, survivor bumps come only from GC sweeps, GC ran only
  every 10,000 **in-process** mutations, and the epoch restarted at 0 each
  start, so a single supervised writer never crossed even one sweep. The fix
  is the accounting, not the predicates: `Graph::drain_log` stamps each flushed
  batch with the absolute epoch, every store upserts
  `sessions.mutation_epoch = MAX/GREATEST(existing, stamped)` inside the flush
  transaction (replays converge; a crash cannot separate the counter from the
  content it counts), `load_session` returns it, and `Graph::from_snapshot`
  resumes it. The daemon is unchanged; a writer attaching to a session whose
  resumed epoch already exceeds the interval sweeps once immediately (a
  catch-up, not new collection criteria), then the normal cadence holds.
  Operator-visible: the schema gains `sessions.mutation_epoch` on all three
  dialects, so an already-provisioned store refuses to attach until
  re-provisioned (`lambo provision`); existing rows backfill to 0 and
  accumulate forward — past mutations are not backdated. The recall cache's
  `mutation_epoch` key component now keeps one comparable scale across
  restarts instead of restarting at 0 with each process.
- `lambo_inspect` no longer refuses the focus approximations a model actually
  types, and recall's rendered text now carries the handle it talks about
  (issue #9). On the dogfood rig inspect failed 28.2% of the time (22 of 78
  calls), every one the bare `no concept matching` refusal, because a caller
  working from recall's rendered block had no way to reach the durable node id:
  the id lived only in `structuredContent`, the text a model actually read
  carried none, and at p90 a concept is 548 B a model will not retype. Four
  changes close the gap, and the 2,000-concept fuzzy-scan cap survives all of
  them with its value, its keying on concept count and the O(total-content)
  lowercase pass it refuses all intact:

  - Recall's rendered block is now
    `<content> [Type] (score N.NN, id <short8>, blast radius N)`, the short
    form being the first 8 hex chars of the node id's simple form. The renderer
    and the resolver share one constant (`format::SHORT_ID_CHARS`), and a
    pure-hex focus of 8..=32 chars now resolves through a short-form id leg in
    `resolve_focus` (unique prefix resolves; several refuse with named
    candidates; none falls through to the content legs), so the token a caller
    reads is itself a valid, durable focus by construction. The floor of 8 is
    the deliberate tradeoff: shorter hex prefixes collide with concept ids
    often enough that hex-looking words like "def" would hijack substring
    foci. The portal's structured hit payload gained an additive `node_id`
    field because its byte-for-byte parity check re-renders the context from
    the payload; the MCP `structuredContent` shapes did not move.
  - A focus that matches nothing now offers near-matches the way the Ambiguous
    arm does: the closest concepts by focus-token overlap, recency breaking
    ties, listed with their node ids under an explicit "suggestions (not
    matches)" header, never a silent match. The ranking is bounded by
    construction (it runs only under the scan cap and keeps at most
    `MAX_INSPECT_CANDIDATES` candidates), a focus sharing no token stays a
    bare refusal, and past the cap the suggestions come from the bounded
    subset only, named as such.
  - Past 2,000 concepts the fuzzy leg no longer refuses outright: it scans a
    bounded subset, the 256 most recently created concepts plus the 256
    highest live blast-radius concepts, deterministic and comparison-only to
    build, and every outcome announces the bound (the fuzzy note, the
    ambiguous message and the refusal all name the subset and the cap). A
    match inside the subset resolves; a match outside it fails honestly as
    Oversized rather than pretending nothing exists. Refusing, not trimming,
    remains the rule for the full pass: it is the fallback that is bounded and
    loud, not the search that is silent.
  - Every failed inspect now records its failure mode and its focus in the
    ledger facts before the error returns (`failure` carrying `missing`,
    `ambiguous` or `oversized`, alongside a focus truncated to 200 characters
    with the explicit marker), so the rig's telemetry can classify Missing,
    Ambiguous and Oversized without reproducing the call. The ledger's
    `error_kind` on these lines deliberately stays `unclassified`: the
    classification rides the new fact, and no new error_kind vocabulary was
    minted.

  Acceptance 4 of the issue, the post-change error rate on the rig, is
  post-deploy measurement and has not been taken; nothing here deploys.

- The H3 cross-store parity suite no longer requires zero rank displacement
  from the `postgres-exact` lane. Issue #2's tie-break change invalidated the
  old contract silently: the midpoint probe of "user schema" and "create
  user" ties exactly in the f64 lane (now settled by canonical key, "creat
  user" first) while pgvector's f32 round-trip sees a strict order the other
  way. Both adapters are faithful to their own scores, so a rank swap whose
  score gap sits inside the f32 round-trip bound (`H3_SCORE_SKEW_EPSILON`)
  in both outputs is now accepted, and any swap across a real gap above the
  bound still fails the suite.
- Every score-ordered list now breaks an exact tie on the concept's
  `canonical_key` before falling back to its node id (issue #2). Node ids
  are minted per run (`Uuid::new_v4`), so an id-settled tie was
  deterministic inside a run and arbitrary across them: the same session
  recalled in a different order, and which synonym duplicate surfaced
  first was seeded by process start, not by the data. The chain is
  **score first, then `canonical_key` ascending, then `NodeId`
  ascending**, stated once in `types::tie_break_by_key` and used by
  recall rank and assemble, the inverted index, daemon rescore and the
  stale-session anchor, canonization window and budget demotion, saints,
  inspect focus, the hybrid merge pick, and the store adapters (sqlite
  and the pg family order in SQL, where sqlite's `BINARY` collation
  makes byte order equal to Rust `str` order; `canonical_key` is
  `NOT NULL` in all three schemas, so the SQL `ASC` can never reach the
  comparator's keyless-sorts-last arm).

  Key *presence* is part of the order — a keyless node sorts after every
  keyed one — because the obvious comparator (compare keys when both are
  present, fall through to the id otherwise) is intransitive on mixed
  input: a keyed node could sort both behind and ahead of the same
  keyless one depending on the third element, so sort results depended
  on input order. The id fallback stays because the key is not a total
  separator — synonym duplicates share one, and interactions carry none
  — but it is final only inside each presence class.

  Three fixes rode along where the chain exposed adjacent defects: the
  inverted index's `remove` now drops the tie-break key entry along with
  the postings, so the map's lifecycle mirrors the postings' it was
  pinned to; the hybrid merge pick moved from the async gather phase
  (which holds no graph, and had been picking by raw id) to the commit
  phase, which validates every tier member as a Concept and picks by
  canonical key; and the web event feed orders same-instant events by
  the moved concept's key, which eval makes routine by stamping every
  event in a cycle with the same `now`.

  Deliberately left id-primary, each named at its site: the
  recent-interactions leg (interactions carry no canonical key),
  presentation orderings with no score to tie (drift hits, conflict and
  high-risk output lists), the hotlist (its `seq` is already a total
  order), and rows a `LIMIT` cut before any tie-break could see them —
  `has_boundary_tie` still forces the exact session query, which is
  authoritative, not unconditionally deterministic.

- `lambo_record_action` embeds the concepts it creates. It had no embedder hop
  at all, so every concept it wrote stored `embedding: NULL` while `derive`'s
  concepts were embedded — an inversion in which everything an agent
  **concluded** was findable by semantic recall and everything it **did** was
  reachable by keyword and graph traversal only. Found by dogfooding on
  2026-09-01: 555 of 946 concepts on the live session carried no vector, and
  the split was categorical rather than partial (Observation 68/68, Logic
  109/109, Constraint 130/130 embedded, all of them `derive`'s; Resource
  394/454 and Entity 161/185 NULL, the action string plus every
  `produces`/`modifies`/`depends_on` entry). It accrued daily rather than
  draining, so it was not a backlog any re-embed could clear.

  Embedding happens off-lock through `embed_action_contents`, with
  `hybrid::derive`'s deadline and its transient/permanent error split, because
  the durable-intent replay's consume-or-keep decision turns on that
  difference. An embedder failure **fails the write** rather than degrading it
  (J3-R3-1): degrading silently to `NULL` is what let this go unnoticed. Both
  the write-queue path (every MCP client) and the new async
  `Memory::record_action_embedded_as` (the CLI's `lambo record-action`) stamp
  the contract first and are gated on `MatchStrategy::Hybrid` — under
  `Canonical` there is no vector leg, and embedding there would stamp a
  contract on a session that asked for none. The synchronous
  `Memory::record_action` keeps its old keyword-only behaviour: a sync
  signature has nowhere to put a model call, so it is now the deliberate
  choice rather than the only one.

- `lambo re-embed --missing-only` backfills concepts that have no vector,
  leaving every existing vector and the session contract untouched. Without it
  a session that accumulated `NULL` vectors inside its own current space had no
  repair path: the full migration refuses to run when the live contract is
  already the stored one ("already carries exactly this contract"), which is
  correct for a migration and is exactly the state a repair runs in. The
  backing `Graph::embed_missing` refuses to overwrite an existing vector (that
  is a migration, and migrations move the contract with them) and appends no
  trailing `SetEmbedding`, so a repair never reads as a migration in the
  mutation log.

- A bounded close could abandon its own flush. `close_bounded` registered a
  *fresh* shutdown listener, so the SIGTERM that started the shutdown could be
  the one that listener recorded — and the close then read it as the operator's
  give-up second press and stopped flushing. `EarlyShutdown` now counts signal
  arrivals rather than latching a flag, so the wind-down and the bounded close
  read one shared record and "a second press" means exactly that. The escape
  hatch is deliberately kept: without it a stalled flush is an unkillable
  process.
- `lambo demo` no longer waits on an exact canonization status. It polled for
  equality every 2ms against a ladder advancing one rung per 25ms cycle, so a
  loaded machine could step Candidate → Venerable *between two polls*, after
  which the wait could never match and burned its full 60s deadline on a state
  that had already passed. It now waits for the concept to have reached **at
  least** the rung in question, which is what the demo means and cannot be
  missed.

### Notes

- B0's extraction already made `crate::store::pg::{PgStore, Dialect}` and
  `crate::store::pg::cockroach::CockroachDialect` public crate API. 0.3.0 is
  the first release that carries them.
