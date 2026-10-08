# Changelog

## Unreleased

### Breaking

- Canonical keys change for text containing some invisible codepoints (#25,
  re-landing review finding V1, which was fixed on 2026-08-15 and then lost
  the same day). Stored concepts keep the keys they were written with; nothing
  is re-keyed on load, so every existing store still loads and its uniqueness
  check cannot newly collide.
  - The Mongolian free variation selectors `U+180B`-`U+180D` and `U+180F` are
    still accepted in content but are now stripped from canonical keys. A
    concept already stored with one keeps its old key, so a new derive of the
    same text no longer matches it. That derive creates a new concept, or merges
    into an existing plain-text concept with the stripped key, and the old row
    is left out of future matching.
  - Text containing the unassigned `Default_Ignorable_Code_Point` codepoints
    `U+2065`, `U+FFF0`-`U+FFF8`, `U+E0080`-`U+E00FF` and `U+E01F0`-`U+E0FFF` is
    now refused wherever a surface size-checks text: concept and action
    content, inspect focus, recall query, and agent and session ids.
    Stored content holding them still loads and renders, but the same text can
    no longer be submitted again.
  - **Operator action:** none expected. The lambo dogfood store (3,966 concepts)
    held none of these codepoints in content or keys when checked on
    2026-10-08. A store that does hold them can be found by comparing each
    stored `canonical_key` with one recomputed from its content.

## 0.3.0 (2026-10-07)

### Breaking

- No Windows binary in 0.3.0. The shared session endpoint added for multiple
  clients uses Unix sockets and Unix file metadata with no platform gate, so the
  crate does not compile for `x86_64-pc-windows-msvc`, and regular CI never built
  for Windows to catch it. The release matrix drops the Windows entry and
  `install.sh` points Windows users at v0.2.2, the last release with a Windows
  build. Restoring Windows is #39.

- GC sweep accounting is durable and GC's step-2 cut changed (issue #29). Public
  structs gained fields, so struct-literal construction breaks where
  `..Default::default()` is not used: `MutationBatch::gc_mark` and
  `GraphSnapshot::gc_mark` (new `lambo::types::GcMark`; serde-defaulted and
  omitted when unset, so pre-#29 JSON round-trips unchanged),
  `Config::{gc_max_interval, gc_idle_floor}`,
  `DaemonConfig::{gc_max_interval_secs, gc_idle_floor}`,
  `CycleParams::{gc_max_interval, gc_idle_floor}`,
  `GcParams::{recency_window, max_collect_fraction, min_collect_cap}` and
  `GcOutcome::{collection_cap, collections_deferred, deferred, resources_spared_by_dependents, trigger}`.
  **Operator action:** the schema gains `sessions.last_gc_epoch` and
  `sessions.last_gc_at` on all three dialects, so an already-provisioned
  store (SQLite included: the attach preflight reads the DDL) refuses to
  attach until `lambo provision` re-runs; provisioning adds the columns in
  place (guarded `ALTER`, no data touched). If the rigs have not yet been re-provisioned for
  #17's `mutation_epoch`, one re-provision covers both. Existing rows backfill
  watermark 0 and no sweep time: the first writer to attach starts the
  `gc_max_interval` clock rather than sweeping, so the first timed sweep comes
  a day (and at least 100 mutations) after the upgrade.

- `lambo::Mutation` gained a `RecordAccess` variant (issue #30: the narrow,
  monotonic access-column update), so an exhaustive `match` on it no longer
  compiles; `lambo::store::batch::FlushStep` gained `Accesses` and
  `BulkLimits` gained an `accesses` field for the same reason. Not a wire or
  persisted format: mutations are applied in-process, and the only serialized
  form (the fixtures' JSON loader) gains an additive `record_access` tag.

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

- Public types gained fields since v0.2.2. None of these structs is
  `#[non_exhaustive]`, so external struct-literal construction and exhaustive
  destructuring break; field access and `..Default::default()` / `..` patterns
  do not. Serialized forms stay compatible: every new serde field is
  `#[serde(default)]`, so JSON written by v0.2.2 still loads.
  - Graph types: `Concept::human_confirmed: i32`;
    `event_time: Option<DateTime<Utc>>` on `Interaction`, `Edge` and
    `lambo::graph::Action` (omitted from JSON when unset);
    `GraphSnapshot::{mutation_epoch, write_intents}`;
    `MutationBatch::mutation_epoch`.
  - Outcomes and stats: `DeriveOutcome::embedded`,
    `MemoryStats::embedded_concepts`, `CanonicalMemory::canonical_key`.
  - Configuration: `Config::promotion_policy`, `EvalParams::promotion_policy`,
    `StoreConfig::vector_dim`, `ServeOptions::{ledger, ledger_heartbeat}`, and
    eleven `EmbedderConfig` fields for the candle and Gemini embedders
    (`device`, `repo`, `revision`, `weights_dir`, `weights_file`, `offline`,
    `keep_warm_secs`, `gemini_project`, `gemini_location`, `gemini_model`,
    `gemini_credentials`).
  - Leases and loading: `LeaseHolder::endpoint`, `LeaseInfo::endpoint`,
    `LoadedSession::write_intents`.
  - MCP parameter structs: `DeriveParams::event_time`,
    `RecordActionParams::event_time`, `StatsParams::{receipt, wait_ms}`; the
    CLI's `re_embed::Args` gained `allow_embedding_mismatch` and
    `missing_only`.

  **Operator action:** the new `event_time` (interactions, edges),
  `human_confirmed` (concepts) and `session_leases.endpoint` columns are added
  in place by `lambo provision` (guarded `ALTER`, no data touched); the same
  re-provision #17 and #29 call for covers them.
- Public enums gained variants, and none is `#[non_exhaustive]`, so an
  exhaustive `match` on them no longer compiles: `StoreKind::Postgres`,
  `EmbedderKind::{Candle, Gemini}`, `LamboError::{EmbedUnavailable, SoftLock}`,
  `Mutation::{PutWriteIntent, ConsumeWriteIntent}` and
  `lambo::store::batch::FlushStep::PutIntents`.
- `lambo::cli::provision::run` takes a third argument, `dsn: Option<&str>`
  (the resolved `store.dsn`; only the Cockroach arm reads it).
  `lambo::store::cockroach::CockroachStore` is now a type alias for
  `PgStore<CockroachDialect>`; its `new` and `seed` keep their signatures.
- Public constants changed value. `lambo::recall::candidates::RECENT_SCORE`
  dropped from `0.5` to `0.35`, a **behaviour change**: concepts that enter
  recall only through the recent-interaction leg now score 0.35, so they rank
  below any genuine BM25 or vector hit (BGE-M3 calibration put the lowest
  durable true semantic hit at 0.3991) instead of above many of them, and
  recall scores a client sees for those concepts drop accordingly (see
  `evidence/mooshik-g-recall-calibration/`). The bind-parameter counts in
  `lambo::store::batch` grew with the new columns: `CONCEPT_COLUMNS` 16 to 17,
  `INTERACTION_COLUMNS` 6 to 7, `EDGE_COLUMNS` 9 to 10.

### Added

- `lambo_stats` (and the `serve --ledger` heartbeat) carries a `gc` object:
  the session's durable sweep mark (`last_gc_at`, `last_gc_epoch`) and the
  last sweep this server ran (`trigger`, `collected`, `deferred`,
  `collection_cap`, `cap_bound`, `resources_spared_by_dependents`,
  `survivors_deferred`; `null` until one runs), plus a `gc:` line on the text
  summary. Additive: no existing key changed. Library: `Memory::gc_stats`,
  `GcStats`, `GcSweepSummary` (issue #29).
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
- A Metal release asset, `lambo-<version>-macos-arm64-metal` (with its
  `.sha256`): the `ship` adapter set plus `embed-candle-metal`, so an Apple
  silicon machine can run the in-process candle embedder (`[embedder]
  kind = "candle"`, `device = "metal"`) from a published binary instead of a
  hand build. It links only macOS system frameworks and `/usr/lib` libraries,
  which the release workflow asserts. The stock `ship` assets are unchanged and
  still carry no candle.
- `install.sh` takes `LAMBO_FLAVOR=metal` to install that asset, with the same
  SHA-256 verification. It is refused with an error on anything but macOS arm64;
  unset, the script installs the stock build as before. `LAMBO_DRY_RUN` set to
  any non-empty value other than `0` (`1`, `yes`, `true`, ...) prints the asset
  and URLs it would fetch, then exits without downloading; unset, empty or `0`
  installs.
- The release workflow can be run by hand (`workflow_dispatch`, `dry_run`
  default true) to build, parity-test and checksum every asset as workflow
  artifacts without tagging. A dispatch never publishes a GitHub release or a
  crate: those jobs run only on a `v*` tag push, and a dispatch with
  `dry_run=false` is refused. Release binaries are now built with
  `LAMBO_GIT_SHA`, so the `serve --ledger` heartbeat names the release commit
  instead of `unknown`.

- Historical event time on writes (D1/D2). `lambo_derive` and
  `lambo_record_action` accept an optional `event_time`, an RFC3339
  about-time such as a commit or document date, stored on the interaction and
  its edges beside the server-stamped `created_at`. Omit it for a live fact,
  which is about now. Every other timestamp argument is still refused by
  name, so `created_at` stays server-owned.

### Fixed

- SQLite's blast radius now counts only structural dependents (#35). A concept
  the focus reached through any edge, including `CoOccurrence`, `Semantic` and
  `Temporal`, counted as a dependent, while Postgres, Cockroach, the in-memory
  store and the graph count `Dependency`, `Causal` and `Hierarchical` edges
  only. On a 3,370-concept dogfood store 673 concepts got a different value and
  14 cleared Stage 3's `blast_radius > 5` bar through non-structural edges
  alone. Stage 3 and budget demotion recompute blast radius when they evaluate a
  concept, so new decisions use the corrected value; values already stored on
  concepts and in canonization events keep the old count until that concept is
  evaluated again.

- A `lambo serve` that loses the election and proxies to the session holder no
  longer keeps its resolved embedder alive. It used to hold the full model for
  as long as its client stayed attached (with candle on Metal, ~1.1 GB of
  weights plus the coalescer threads) without ever embedding; the backends are
  now released before the proxy starts. The model is still loaded while the
  backends resolve and through the election wait, until the role is known
  (tracked in #31). Pre-existing, found in the issue #13 review.
- GC sweeps at human pace without deleting reasoning by session age
  (issue #29). Behaviour changes, each deliberate:
  - **A restart no longer sweeps by itself.** GC's watermark was per-process state starting
    at 0, so once a session passed `gc_interval` lifetime mutations every writer
    restart swept once and bumped every `gc_survived` — three restarts reached
    Stage 1's floor with no new information. The watermark and the time of the
    last sweep now persist with the session beside `mutation_epoch` (stamped on
    every flushed batch, merged monotonically in the flush transaction,
    resumed on load), so a restarted writer sweeps only if a sweep was already
    due by the stored mark: a previously swept session that took at least
    `gc_idle_floor` mutations and was down past `gc_max_interval` sweeps once
    on its first cycle back, which is intended. A never-swept session anchors
    its clock on first attach instead of time-sweeping, and keeps #17's single
    catch-up sweep on the mutation count. The anchor only delays the timed
    trigger: a never-swept session counts its whole lifetime toward
    `gc_idle_floor`, so one with at least that many mutations sweeps once,
    `gc_max_interval_secs` after the anchor, even if idle since (a deliberate
    catch-up; afterwards the mark is set and idle sessions never sweep).
  - **Timed sweeps.** A session also sweeps once `[daemon] gc_max_interval_secs`
    (default 86 400) has passed since its last sweep, if it took at least
    `[daemon] gc_idle_floor` (default 100) mutations since. Idle sessions never
    sweep on time; a writer down for N days sweeps once. The 10 000-mutation
    trigger and every promotion threshold are unchanged.
  - **Step 2 protects reasoning.** Logic, Constraint and Observation are
    exempt from the score cut (orphans and disconnected components are still
    collected; the exemption is `ConceptType::exempt_from_gc_score_cut`, an
    exhaustive table), so the cut removes only Entities and isolated
    Resources. GC's eviction recency is now time since last touch over a fixed
    365-day window (Lambo is long-term memory; what the cut can still reach is
    mostly long-tail pointers, about 4% of the rig's store) instead of
    position in the session span; recall ranking and canonization
    still use the span-relative score. Collections per sweep are capped at
    `max(32, 5%)` of unprotected concepts; a sweep that hits the cap logs a
    warning and reports `GcOutcome::collections_deferred`.
  - **Resources with dependents are kept (operator decision).** The score cut
    no longer collects a Resource that another concept has a `Dependency`,
    `Causal` or `Hierarchical` edge into (what `record_action` writes into the
    things an action depends on, produces or modifies), nor one whose blast
    radius is non-zero (the action node that is the only structural source of
    something). Isolated, untouched Resources still age out over the 365-day
    window. Reported as `GcOutcome::resources_spared_by_dependents`.
  - **An access can only help in GC's cut.** GC no longer switches the whole
    session to the full composite once any concept has been read (ALGO-1's
    switch, which made every unread concept ~20% easier to collect: one access
    on one concept took the rig's first sweep from 159 to 412). Each concept
    scores the live-dimension score plus its own frequency term
    (`score::score_live_plus_frequency`). Recall ranking, the daemon's score
    table and canonization are unchanged.
  - **A future sweep time is re-anchored.** A forward wall-clock jump used to
    persist a future `last_gc_at` that the monotonic merge kept after the
    clock was corrected, switching the time trigger off until real time caught
    up. A stored time more than 5 minutes ahead of `now` is now re-anchored at
    `now`; the regression persists through a flagged path in the session-row
    upsert (`GcMark::last_gc_at_reset`, never stored). `last_gc_epoch` stays
    strictly monotonic.
  - **Repeated clock jumps persist.** A second forward jump and correction in
    one process now re-anchors in the store too; the writer-side reset flag is
    never carried in snapshots.
  - **Sweeps are cheap.** Scoring no longer scans every interaction per
    concept, so a sweep is linear in the store instead of concepts times
    interactions: on the rig copy the first sweep went from 67.5 ms to 9.0 ms
    under the write lock, with identical results.
  - `lambo_stats` reads GC stats once per answer, and
    `resources_spared_by_dependents` no longer counts a Resource that step 3
    collected as disconnected.
  - On a copy of the Metal rig store the first sweep now collects nothing
    (was 537, including 42 Logic and 324 Observations), and collection no
    longer grows with session span alone. Untouched sparse concepts still age
    out once older than the window: an untouched store converges on 130
    collections (Resource 121, Entity 9; 3.9% of the store), with 884 under-bar
    Resources kept by the dependents rule and no change in the concepts above
    Stage 3's blast-radius bar (41 before and after). One access never adds a
    collection. Details in `dev-diary/notes/gc-time-bound-29.md`.
- `access_count` and `last_accessed` are now written (issue #30). No path ever
  wrote them, so the spec §9 `frequency` dimension was dead: GC's step 2 ran on
  the renormalized three-dimension score (ALGO-1) and could not see what agents
  actually recall (all 3,370 concepts on the Metal rig read 0). Every concept
  hit a writer's recall returns now counts once per recall — cache-served or
  not, inside the token budget or not — and a resolved `lambo_inspect` counts
  its focus concept (not the neighbourhood; a refusal counts nothing). Reads
  note hits in a leaf-locked ledger; the daemon applies them in RAM once per
  tick and marks the concepts access-dirty; the write-behind flush persists
  them as one narrow, monotonic update per concept (`access_count` and
  `last_accessed` only, `max` of stored and new — no full-row rewrite, no
  embedding, no vector-index touch; idempotent on replay, fenced like every
  write) through the existing columns (no schema change, no re-provision), and
  `close` takes the remainder, so the counts survive a writer restart. A
  recall that finishes after `close` took the ledger is not counted. Reads
  never grow the mutation log: during a store outage the flush holds at most
  one access update per concept (and never more than half of
  `backend_log_max`), so read traffic alone cannot degrade a session to
  `durability="none"`, and the latest counts land once the store recovers.
  That held drain can occupy up to half of the degrade budget, so while reads
  are active during an outage, writes alone degrade the session at about half
  the configured bound.
  Accesses do **not** advance the mutation epoch, so GC's `gc_interval`
  trigger, the recall cache and hybrid replanning are unaffected and a
  read-heavy session does not sweep on reads. Reader processes (`lambo
  recall`, `serve-web`) count nothing; a proxied call counts once, in the
  holder. Operator-visible effects: recency counts a read as a touch (the later
  of `created_at` and `last_accessed`); **a read-only session no longer goes
  Stale** — reads are activity, by decision (the staleness detector already
  read `last_accessed`; it now has something to read); the daemon score
  table, which recall ranks with, picks up new frequency only at the next real
  write (it rescores on epoch change; an accepted lag: ranking between two
  writes stays stable and cacheable); `lambo_saints` reports non-zero `access_count`. GC's
  score cut adds each concept's own frequency term on top of the live-dimension
  score (#29's additive rule), so an access can only raise that concept's score
  and never makes another concept easier to collect; accesses never advance the
  epoch, so they cannot reach `gc_interval` or `gc_idle_floor`, and a read
  postpones GC's 365-day recency cut for the recalled concept (see
  `dev-diary/notes/issue-30-access-count.md`). Recall ranking, the score table
  and canonization Stage 1 have no such switch: an access moves only the
  accessed concept's score.
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

- `install.sh` on Apple silicon from an x86_64 (Rosetta) shell installs the
  native arm64 build instead of refusing the machine as Intel macOS: it checks
  `sysctl hw.optional.arm64` when `uname -m` says `x86_64`.

### Notes

- B0's extraction already made `crate::store::pg::{PgStore, Dialect}` and
  `crate::store::pg::cockroach::CockroachDialect` public crate API. 0.3.0 is
  the first release that carries them.
