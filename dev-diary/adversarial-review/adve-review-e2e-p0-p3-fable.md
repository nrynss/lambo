# Adversarial Review: E2E P0–P3 — fable

```text
╔════════════════════════════════════════════════════════════════╗
║  STATUS: CLOSED — all 45 findings dispositioned (2026-08-12)   ║
║  Scope:  E2E across P0 (ground), P1 (contracts), P2 (graph     ║
║          core), P3 (stores) — all claimed DONE + merged        ║
║  Source: main @ 61e37d9                                        ║
║  Date:   2026-08-11                                            ║
║  Reviewer: fable (claude-fable-5) — orchestrator + five        ║
║          parallel fable reviewers (contracts, graph core,      ║
║          stores/SQL, tests/CI, cross-phase coherence); every   ║
║          P1/P2 finding independently re-verified against the   ║
║          code by the orchestrator before inclusion             ║
║  Discount: Bedrock account authorization (T0.4/T7.1) — support ║
║          ticket raised; unavailability not held against P0-P3  ║
║  Evidence: dev-diary/evidence/e2e-p0-p3-fable-gates.txt        ║
║  Verdict: PLATFORM HOLDS — no false closures, gates green,     ║
║          docs honest. 45 findings: 7 P1, 15 P2, 23 P3. No P0.  ║
║          Disposition the P1s before the tracks that consume    ║
║          them start (see Recommended disposition order).       ║
║  Verified: 2026-08-12 — 7/7 P1, 32/38 P2/P3 (see addendum)     ║
╚════════════════════════════════════════════════════════════════╝
```

## Grounding

Read: the frozen spec in full (incl. a diff of the "frozen" spec against its
original commit 480f620 to enumerate every erratum), all of PHASE-0..3 (tasks,
statuses, exit criteria, handoff logs), cross-phase contract sections of
PHASE-4..7, all 20 prior review records + both evidence files, notes/, README,
AGENTS.md, CI workflow, migrations, scripts, fixtures, and `src/**` in full
across the five dimensions. Executed: fmt, clippy (default and
sqlite+cockroach, `-D warnings`), test suites on four feature combos, fixture
regeneration, `gh` CI/API checks (see evidence file).

Method: five parallel adversarial reviewers, one per dimension, each briefed
to hunt real defects and re-verify prior closures rather than trust records.
The orchestrator then re-verified every P1 and most P2s directly against
source before accepting them; overlapping findings were merged (48 raw → 45).

## Gates (this review's runs, main @ 61e37d9)

| Gate | Result |
|---|---|
| `cargo fmt --check` | clean |
| `cargo clippy --all-targets -- -D warnings` (default) | clean |
| `cargo clippy --all-targets --features store-sqlite,store-cockroach -- -D warnings` | clean |
| `cargo test` (default) | 220 pass, 0 fail, 3 ignored (visible) |
| `cargo test --features store-sqlite` | 237 pass, 0 fail (incl. 180-assertion T3.6 matrix) |
| `cargo test --features store-cockroach` (no DSN) | 237 "pass" — **but** cockroach live tests skip-as-green (TEST-1) |
| `cargo test --no-default-features` | 144 pass |
| `cargo test --no-default-features --features store-sqlite` | **build error** (CON-4, folded into TEST-2) |
| `scripts/gen-fixtures.py` regeneration | all 5 fixtures byte-identical |
| GitHub CI on main HEAD | green (run 31510286505) |

## Prior-closure verification — no false closures found

Three reviewers independently re-verified every prior CLOSED record against
current main. **All remediations are present; nothing was closed on paper
only.** Specifically verified holding:

- **P3 partials (gemini36flash/opus46, F1–F6, commit 1e78a92/0e4f6b3):** F1
  single-point-span coverage 1.0 in all three stores + tests; F2 load timeout
  (30s, hung-store test); F3 retained-batch backoff (gate after degrade
  check, flat-attempts test); F4 `load_session_async` core + parity tests;
  F5 flush idempotency contract pinned on the trait docstring
  (mod.rs:68–76); F6 reservations reader-filter comments in both migrations.
- **T3.6 (dde550f):** both span age gates behaviorally discriminated
  (distinct-origin e-gate probe, fresh-origin i-gate probe, in both
  adapters); sqlite matrix text-locked; cockroach matrix locked at exactly
  180. The three-way gate genuinely discriminates: dropping the e-gate,
  i-gate, Derives exclusion, or self-exclusion each provably fails a test.
- **P2 (muse-spark M1–M4/S1/S3, grok G1–G3):** partial-UNIQUE enforcement,
  index owner contract, validate-then-mutate derive, demote logical clock,
  query-term dedup, avgdl guard, chunk_group_id rejection — all in code with
  their named regression tests. G6/G7 cross-phase notes present in
  PHASE-4/PHASE-5 docs as claimed.
- **§6.4 lock discipline: PASS.** The only `Arc<RwLock<Graph>>` holder on
  main is flush.rs; the write-guard scope (flush.rs:289–294) dies before any
  I/O; `flush_with_retry` awaits with no guard held. Graph module itself is
  lock-free/sync.
- **Level B invariants: PASS.** Uncompiled kind = hard error (tested);
  unknown TOML key rejected; single construction site holds (`main.rs` uses
  `resolve_from_config_path`/`resolve_store_only` only; cli/mcp/daemon are
  stubs); capabilities truthful per adapter; nothing above the trait
  branches on adapter identity.
- **SQL-injection sweep: clean.** Every dynamic statement interpolates only
  placeholder ordinals or module constants; all values bound; no LIKE
  anywhere; `pragma_table_info(?)` binds. DSN redacted in `StoreConfig`
  Debug (+test); no DSN reaches logs/errors.
- **Process honesty:** all task `status:` lines are `done` (no stale
  claims); every cited test name exists and passes; fixture stage-gate
  numbers match claims (21 non-Canonical peers, gc_survived 4,
  blast_radius 8); fixtures regenerate byte-identical; all cited commits
  exist with claimed content; spec edits are marked as errata (one
  bookkeeping gap: COH-11).

## Findings summary

Severity: P1 = will bite a specific downstream task or demo claim if not
dispositioned; P2 = real defect/gap, reachable but off the current demo
path; P3 = latent/robustness. No P0 (nothing red today).

| ID | Sev | Area | One line |
|---|---|---|---|
| GRAPH-1 | P1 | write path | Step-5 matcher ignores `concept_type`: derive/record_action silently attach to demoted Observations, nondeterministically under duplicate keys |
| STORE-1 | P1 | contract | `EmbeddingContract` has no production write path; `CockroachStore::seed` drops it (MemoryStore preserves it) — model-mixing refusal inert across restarts |
| TEST-1 | P1 | false green | Cockroach conformance/matrix tests skip-as-`ok` when `LAMBO_COCKROACH_DSN` unset — "green ×3" spoofable, skip invisible |
| TEST-2 | P1 | CI (= COH-4 + CON-4) | CI builds default features only: sqlite/cockroach tier CI-dark; `--no-default-features --features store-sqlite` doesn't even build; cockroach-only combo trips `-D warnings` |
| COH-1 | P1 | demo claim | Shipped `vector_candidates` uses the exact session-filtered shape T0.3's own evidence shows **bypasses** the vector index — required-tool claim currently false |
| CON-1 | P1 | sqlite | `create_if_missing` never set: file-backed SQLite cannot bootstrap a new DB (reproduced live); every documented file-path config fails at first use |
| CON-2 | P1 | embeddings | BGE client silently retries 400 with model cleared → embeds in server-default space while the contract records the configured model; no log |
| GRAPH-2 | P2 | invariants | No bipartite/endpoint-type validation on `upsert_edge`; `assert_invariants` silent; an in-repo test already writes a type-invalid edge |
| GRAPH-3 | P2 | load path | `dfs_cycle` unboundedly recursive — deep Causal/Dependency chains stack-overflow `load_session` (SIGABRT), session permanently unloadable |
| GRAPH-4 | P2 | audit | `apply_canonization_transition` never validates `from_status` vs current status; audit trail can record fabricated transitions (demo centerpiece) |
| GRAPH-5 | P2 | write path | Swapped-order re-derive creates reverse-duplicate CoOccurrence edge instead of reinforcing — violates T2.3 exit criterion; skews P4 density (heaviest weight) |
| STORE-2 | P2 | flush | No per-attempt timeout on `store.flush(...)` and no statement timeout — a hung store wedges the flush loop forever (no retry/retain/degrade) |
| STORE-3 | P2 | flush | Post-degrade, `cycle` keeps draining the log into `pending` and never frees — unbounded RAM growth for the session's remaining life |
| STORE-4 | P2 | flush | No error classification/dead-letter: a deterministic constraint violation poisons the head batch permanently → inevitable degrade |
| TEST-3 | P2 | process | CI triggers only on main push/PR; P0's "[x] CI green" predates the first CI run; no phase-branch commit was ever CI-validated |
| TEST-4 | P2 | conformance (= COH-8) | Promised generic `tests/store_conformance.rs` never landed; "Conformance suite green ×3" = three non-identical per-adapter oracles |
| TEST-5 | P2 | evidence | No committed artifact for T0.2 provisioning or the "cockroach-live green" run — combined with TEST-1/TEST-2, live-green is unfalsifiable from the repo |
| COH-2 | P2 | docs→P8 | `rmcp` silently removed from Cargo.toml by 8f9e527 while P0 handoff still says "pinned at 0.1.x — upgrade carefully in P8" |
| COH-3 | P2 | contract→P6 | No shipped write path sets `Concept.last_demotion_time`; `CanonizationEvent` carries no field for it — T6.3 cooldown/T6.4 demotion depend on it |
| COH-5 | P2 | docs→P4 | PHASE-4 tells T4.1 to consume a "mutation notify from T2.3" and a T3.5 rescore signal that were explicitly deferred and do not exist |
| COH-6 | P2 | docs→P8 | opus46-S1 closed "shutdown drain = v0.7.0" but spec §6.1 `close()` requires a final flush in v0.1; FlushTask exposes no stop/drain API |
| CON-3 | P2 | Level B | `kind = "sqlite"` with no path silently becomes ephemeral `sqlite::memory:` — the exact silent-fallback class Level B forbids (Cockroach hard-errors) |
| GRAPH-6 | P3 | load | Empty graph doesn't round-trip: `from_snapshot` rejects zero-interaction snapshots ("expected exactly one chain head, found 0") |
| GRAPH-7 | P3 | load | Duplicate-natural-key edges in a snapshot silently merge via reinforcement instead of rejecting — loaded graph ≠ stored snapshot |
| GRAPH-8 | P3 | write path | Empty/stopword-only content accepted; all junk collapses onto one key-`""` concept with a frozen arbitrary type |
| GRAPH-9 | P3 | canonical | No NFC/NFKC normalization; camelCase split is ASCII-only — visually identical non-ASCII contents fragment into distinct concepts |
| GRAPH-10 | P3 | contract | mod.rs "edges reference nodes upserted in the same batch" is false for `record_action` after a drain (edge-only batches) |
| STORE-5 | P3 | parity | Session `created_at` after identical flush history: Some(now) on Cockroach, None on SQLite/Memory — adapter-observable divergence |
| STORE-6 | P3 | parity | `MemoryStore::flush` non-atomic (prefix applied on mid-batch error) while SQL adapters roll back — reference oracle diverges on failure path |
| STORE-7 | P3 | parity | Corrupt contract rows: SQLite errors on kind-XOR-dim both ways; Cockroach silently returns None for (NULL kind, dim set) |
| STORE-8 | P3 | robustness | SQLite load uses panicking `row.get` (40+ sites) vs Cockroach's `try_get` — corrupt file panics the loading task instead of typed error |
| STORE-9 | P3 | config | File-backed SQLite gets no WAL/busy_timeout tuning — spec §2.2 external-reader story causes SQLITE_BUSY flush failures |
| TEST-6 | P3 | scripts | provision.sh migration split is line-based `grep -v 'CREATE VECTOR INDEX'` — reformatting the statement corrupts both halves |
| TEST-7 | P3 | scripts | fetch-bge-m3.sh has no checksum verification (pinned revision mitigates; partial-file trust on "already present" branch) |
| TEST-8 | P3 | toolchain | `stable` floats in rust-toolchain.toml and CI while enforcing `-D warnings` — a new stable can redden CI with zero code changes |
| COH-7 | P3 | board | Board denominators wrong: P0 "4/4" (T0.4 is blocked-account), P1 "4/4" (five tasks), P7 "1/3" (four tasks) |
| COH-9 | P3 | index | adversarial-review/README.md indexes 4 of 21 records |
| COH-10 | P3 | docs | T7.0 handoff self-contradicts on live-smoke status and cites a nonexistent review filename |
| COH-11 | P3 | process | T1.5 erratum reworded spec §12.2/§14 (incl. adding a never-cut entry) without naming those sections in the erratum record |
| COH-12 | P3 | git | P3 wave-1 commits exist twice (twin hashes on parallel lineages) — revert/bisect ambiguity; rebase P4–P7 branches before use |
| CON-5 | P3 | contract | `resolve_backends` never cross-checks `VECTOR_SEARCH` capability vs `vector_dimensions().is_some()` — latent footgun for the next adapter |
| CON-6 | P3 | types | Blast radius u64 on the trait vs Option\<i32\> on Concept/CanonizationEvent — frozen types force every P6 implementer into ad-hoc narrowing |
| CON-7 | P3 | embed | Empty-input behavior unspecified: FixtureEmbedder embeds "" fine, BGE errors `Unavailable` — fixture-green, live-brittle |
| CON-8 | P3 | parity | SQLite flush→load silently drops `Concept.embedding` (column never written/read) — flush→load parity for embeddings differs per adapter |
| CON-9 | P3 | docs | `LAMBO_SQLITE_PATH` honored by the env overlay but absent from .env.example — funnels operators into CON-3 |

## P1 findings — detail

### GRAPH-1 — step-5 matcher is type-blind (and nondeterministic under duplicate keys)

- **Location:** src/graph/canonical.rs:120–127; consumed at derive.rs:394–453, action.rs:258–259
- **Claim:** `canonicalize` matches `graph.concepts().find(|c| c.canonical_key == key)` with **no `concept_type` filter**, so `derive`/`record_action` silently attach agent-declared concepts to demoted context-overflow **Observations** — and, because `concepts()` iterates a `HashMap` and duplicate Observation keys are legal (partial-UNIQUE errata), *which* node wins is iteration-order nondeterministic.
- **Failure scenario:** `demote(i1, agent, "user schema", "g")` creates an Observation keyed `"schema user"`. A later `derive(i2, [("UserSchema", ConceptType::Entity)])` **matches the Observation**: the requested Entity type is dropped; Derives/CoOccurrence/Hierarchical (and record_action's Causal/Dependency — Stage-2/3 canonization evidence) hang off a context-overflow record with Observation eviction resistance (0.7) and score multiplier (0.9). With two same-key Observations, rebuild determinism breaks; the same content in `concepts` and `parent_of` can even resolve to two different nodes within one call.
- **Root cause of the miss:** T2.2's "at most one match in a well-formed graph" justification predates the M1/M2 errata that legalized duplicate Observation keys; the matcher was never revisited. G7's closure covered the *recall* side only, not the write path.
- **Disposition needed before:** P5 (recall disambiguation assumes types are right) and P6 (canonization evidence quality). Suggested fix: filter non-Observation in step 5 (Observations skip matching per spec §7 demote semantics), or match type-compatibly with deterministic tie-break; regression-test derive-after-demote.

### STORE-1 — `EmbeddingContract` never persists; seed paths diverge

- **Location:** src/store/cockroach.rs:133–140 (`UPSERT_SESSION_SQL` writes only `session_id, root_goal, created_at, closed_at`), cockroach.rs:826–835 (`seed`), src/store/mod.rs:51–111 (trait has no contract-write surface)
- **Claim:** No production path writes `sessions.embedding_{kind,model,dim}` — the only writer in the tree is a raw UPDATE inside a conformance test (cockroach.rs:2214). `CockroachStore::seed` (the S5 full-snapshot path) silently **drops** `GraphSnapshot.embedding` while `MemoryStore::seed` preserves it — an untested adapter divergence.
- **Failure scenario:** session embeds with BGE (1024-d); restart configured for Titan V2 (also 1024-d — the exact swap the DDL comment anticipates): `load_session` returns `embedding: None`, both compatibility checks pass vacuously, Titan vectors land beside BGE vectors, `vector_candidates` ranks across incompatible spaces → wrong semantic merges (§7.1 step 6) with no error anywhere.
- **Honest nuance:** the write-path absence *is* a documented S5-class deferral (spec §4 DDL comment; PHASE-3 handoffs; enforcement wired at T7.2/T8.1). What is **not** documented or acceptable: the seed divergence, and the fact that neither the P7 nor P8 doc names "create the contract write path" as a prerequisite task — as shipped, the never-cut "refuse mid-session model mixing" promise cannot fire across restarts.
- **Disposition needed before:** T7.1/T7.2/T8.1. Fix: extend `UPSERT_SESSION_SQL` + seed to carry the contract (columns exist in both DDLs; SQLite already reads them), and name the owner.

### TEST-1 — cockroach live tests skip-as-green

- **Location:** src/store/cockroach.rs:1706–1714 (`dsn_or_skip`), used at 1860, 2705
- **Claim:** With `LAMBO_COCKROACH_DSN` unset, `conformance_suite` and `build_store_returns_working_adapter` print an eprintln (captured, invisible) and `return` — reporting `ok`. Verified live: `cargo test --features store-cockroach` with no DSN → "237 passed, 0 failed", zero cockroach behavior exercised.
- **Failure scenario:** the P3 exit criterion "Conformance suite green ×3" is count-indistinguishable from a skip run; any cockroach regression ships invisibly; the demo profile breaks on stage.
- **Disposition:** make skips loud and countable — `#[ignore]`-by-default live tests run via an explicit alias (matches live_calibration.rs's honest pattern), or a `LAMBO_REQUIRE_LIVE=1` env that turns skip into fail, plus a committed evidence capture per live run (cf. TEST-5).

### TEST-2 (merged: + COH-4, CON-4) — CI is default-features-only, and non-default combos are actually broken

- **Location:** .github/workflows/ci.yml:26–33; src/store/sqlite.rs:1144 (test module imports `MemoryStore` without a `store-memory` gate)
- **Claim:** CI = fmt + clippy + `cargo test --all`, all default features. Therefore: the entire P3 adapter tier (17 sqlite tests incl. the T3.6 matrix; cockroach compile) has **no CI backing**; the opus46 close-note "the P3 close gate runs all feature combos" was a local ritual. And two combos are already broken today: `--no-default-features --features store-sqlite` fails to build tests (E0432, reproduced), and `--no-default-features --features store-cockroach` emits dead-code warnings that CI's own `RUSTFLAGS=-D warnings` would hard-fail.
- **Failure scenario:** during the six-wide P4–P8 week, any refactor of `store/mod.rs`/types/graph API can break sqlite/cockroach silently; first discovery at demo assembly.
- **Disposition:** one-line matrix job — `--features store-sqlite` test, `--features demo` check, `--no-default-features` check — and cfg-gate the sqlite test module's MemoryStore import. Cheap; do it before P4 launches.

### COH-1 — shipped `vector_candidates` bypasses the vector index the project must demo

- **Location:** src/store/cockroach.rs:1267–1274 vs dev-diary/evidence/t0.3-vector-spike.txt; consumed by PHASE-7 T7.3 and spec §12.1
- **Claim:** The shipped SQL (`WHERE session_id = $1 AND embedding IS NOT NULL ORDER BY dist LIMIT $3`) is the exact session-filtered shape the project's own spike evidence shows does a table scan, not `vector search` ("EXPLAIN filtered … does NOT use vector index; filtered=false"). T0.3's GO verdict was on the *pure* query; T3.2 silently took "accept-no-index" without recording the choice; module doc never mentions it.
- **Failure scenario:** T7.3's done-when is EXPLAIN-verified index usage "true on camera"; spec §12.1 lists Distributed Vector Indexing as a required tool. As merged, that claim is false; whoever picks up T7.3 (late, one of the last tasks) discovers it on EXPLAIN day and rewrites the query under deadline pressure.
- **Disposition:** decide now (T0.3's own options): global vector search + Rust-side session filter, or the spike's recommended composite/`STORING` index. Record the choice in the T7.3 task text either way.

### CON-1 — file-backed SQLite cannot bootstrap (reproduced)

- **Location:** src/store/sqlite.rs:203–210 (`connect`), store/mod.rs:339–340
- **Claim:** `SqliteConnectOptions::from_str(path)?.foreign_keys(true)` — `create_if_missing` never enabled (sqlx default false); no `?mode=rwc` anywhere in code or docs. Reproduced: fresh path → `init_schema` → `(code: 14) unable to open database file`. Every committed test uses `sqlite::memory:`, so the suite can't see it.
- **Failure scenario:** the exact commented example in lambo.example.toml (`path = "./lambo.db"`) fails on first use; the "local-first embedded tier" (spec §3.3, "ships") is unusable on a fresh machine.
- **Disposition:** `.create_if_missing(true)` + one file-backed round-trip test (tempdir). Trivial fix, P1 only because a spec-"ships" adapter is dead on arrival for its primary configuration.

### CON-2 — BGE 400-retry silently swaps embedding spaces out from under the contract

- **Location:** src/embed/bge_m3.rs:154–165; interacts with resolve.rs:60–64
- **Claim:** On HTTP 400 with a configured model, the client clears `model` and retries against the server default — silently (zero `tracing` in the file). `resolve_backends` stamps `EmbeddingContract.model` from *config*, so the session's vectors can be from a different space than the contract labels, undetectably (same dim passes the only runtime check).
- **Failure scenario:** llama.cpp runs a different GGUF than configured → every embed 400s → silent fallback embeds in server-default space; ops later fixes the server → same session now contains mixed-space vectors that the T7.2/T8.1 guard **can never catch** (contract matches config on both sides). Defeats design-of-record invariant "never mix model spaces" even after enforcement lands. (The T7.0 review assessed the retry as client robustness only; the contract interaction was never reviewed — this is not a re-report.)
- **Disposition:** fail hard on model-mismatch 400 (or verify the server's actual model id against config at construction) and `tracing::warn!` any fallback. Must land before T7.2 hybrid matching trusts vectors.

## P2 findings — detail (condensed)

- **GRAPH-2 — bipartite invariant unenforced.** `record_edge`
  (graph.rs:963–1011) validates session/existence/id-consistency/weight but
  never endpoint types; `assert_invariants` (794–949) cannot flag
  type-invalid edges (its Temporal/Derives checks look the other way). An
  in-repo test (graph.rs:1636) already writes `Semantic` from an
  *interaction* without complaint. Spec §5 pins what each edge type
  connects; edges deliberately carry no FK (spec §4: "the writer enforces
  it" — it doesn't). Any P4–P7 source/target mixup pollutes recall BFS
  permanently. Fix: type-check in `record_edge` + invariant scan arm.
- **GRAPH-3 — recursive cycle check can SIGABRT session load.**
  `dfs_cycle` (graph.rs:1059–1084) recurses per chain node; reachable in
  production via `from_snapshot` ← load.rs:92 on a worker thread (~2 MiB
  stack). A ~10k-deep Dependency chain (plausible for a long
  record_action-heavy session) overflows → abort → session **permanently
  unloadable** — precisely the "typed error, never a panic" promise load.rs
  makes. Fix: iterative DFS (every other traversal in P2 is already
  iterative); drop the dead `path` vec.
- **GRAPH-4 — canonization transitions unvalidated.**
  `apply_canonization_transition` (graph.rs:545–568) never compares
  `event.from_status` to the concept's current status and has no
  legal-transition matrix; `blast_radius` overwritten unconditionally.
  MemoryStore mirrors the blind write, so fabricated history persists into
  `canonization_events` — the table the demo queries on camera (spec §13
  step 5). P6 is the only planned guard and doesn't exist yet. Fix: reject
  `from_status != current`, validate transitions against the §10 state
  machine at this write gate. **Sibling: COH-3** — the same surface also
  cannot carry `last_demotion_time` (no field on `CanonizationEvent`, no
  setter anywhere: graph.rs sets status+blast only; all three adapters'
  UPDATE writes status+blast only), yet T6.3's cooldown and T6.4's
  "demotion sets last_demotion_time" depend on it. P6 will have to extend
  P2-owned graph.rs or the frozen event type mid-week — flag the owner now.
- **GRAPH-5 — CoOccurrence reinforcement is direction-blind.**
  derive.rs:267–294 probes `edge_between(source, target)` in call order
  only; `derive([x,y])` then `derive([y,x])` yields two 0.5-weight edges
  instead of one reinforced 1.5-weight edge, violating the T2.3 exit
  criterion ("deriving the same concepts twice … reinforces") for
  swapped-order calls — the common multi-agent case — and double-counting
  the pair in P4's density dimension (0.35, the heaviest weight). Fix:
  probe both directions (module comment already says the relation is
  symmetric).
- **STORE-2 — hung store wedges the flush loop.** flush_with_retry
  (flush.rs:366–414) awaits `store.flush(...)` with no
  `tokio::time::timeout`; pool has no statement timeout/keepalive
  (PgPoolOptions sets only `max_connections(4)`). A mid-statement network
  partition (demo-wifi class) freezes the singleton loop: no retry, no
  retain, no degrade, log grows, loss bound becomes unbounded. F2 fixed
  exactly this class for load; flush was left open. Fix: per-attempt
  timeout mapped into the existing retry path.
- **STORE-3 — post-degrade retention grows forever.** cycle
  (flush.rs:287–316) keeps draining the graph log into `self.pending` on
  every tick after degradation and never clears; a degraded busy session
  retains every mutation (embeddings ≈ 4KB+ each) for its remaining life —
  and the existing test locks the growth in (asserts depth keeps counting
  post-degrade). Spec §2.3 "none = pure RAM" implies dropping. Fix: clear
  pending on degrade; keep counting depth for stats if desired.
- **STORE-4 — no terminal-error classification.** A deterministic flush
  failure (e.g. unique-constraint violation from a dirty cluster or an
  upstream key bug) is retried identically to a network error forever
  (order preserved → head-of-line blocking) until degrade ends durability
  for the session. Related string-typing: cockroach `is_retryable` is
  substring matching (`"40001"`) over flattened error text. Fix: classify
  constraint-class errors as non-retryable → dead-letter the batch (log +
  drop or park) instead of poisoning the queue.
- **TEST-3 — "CI green" was retrospective.** CI triggers only on main
  push/PR; exactly 3 runs exist, all 2026-08-11, while P0's criterion was
  checked 2026-08-10 (repo wasn't on GitHub yet — handoff says so). Current
  HEAD *is* green; this is a process-integrity finding: phase branches
  accumulate breakage invisibly (see TEST-2 disposition).
- **TEST-4 (= COH-8) — the generic conformance suite doesn't exist.**
  Promised twice (PHASE-1:80–82 "written generically so T3.2/T3.3 reuse it
  verbatim"; P3 exit "[x] Conformance suite green ×3"), `tests/` contains
  no store_conformance.rs; the ×3 is three different in-module oracles of
  different strictness (sqlite has full snapshot-vs-memory parity; cockroach
  equivalents run live-only, compounding TEST-1). Divergence outside the
  T3.6 matrix (keyword_candidates ranking, record_canonization semantics)
  has no shared oracle. Fix: either extract the harness or re-word the exit
  criterion honestly and name which tests constitute the suite.
- **TEST-5 — live-green claims have no committed evidence.** T0.3 set the
  pattern (evidence file with captured EXPLAIN); T0.2 provisioning and the
  P3 "232 live / 0 SKIPs" run have none. With TEST-1 + TEST-2, nothing in
  the repo can distinguish "cockroach conformance passed live" from "never
  ran". Fix: capture per-live-run evidence files (the review convention
  already mandates the directory).
- **COH-2 — rmcp silently dropped while P8 is told it's pinned.** 8f9e527
  removed `rmcp = "0.1"` from Cargo.toml (and edited PHASE-0 the same
  commit without touching its two rmcp claims); spec §6.3 erratum kept the
  row. T8.2 (MCP serve — never-cut demo chain) will plan against a
  dependency that isn't there and re-litigate 0.1-vs-v3 under pressure.
  Fix: one Handoff Log correction naming the re-add owner + version choice.
- **COH-5 — P4 doc cites wake sources that don't exist.** PHASE-4 T4.1
  says "wake on mutation notify from T2.3, warm-up rescore on load (T3.5's
  signal)"; derive.rs:80–86 explicitly says "do NOT build the channel here …
  no stubs, no channel types" and T3.5's handoff says neither transport nor
  skeleton exists. A P4 agent scheduling off task-level requires burns time
  hunting, then faces an ownership question mid-week. Fix: correct PHASE-4
  to "poll `Graph::epoch()` / explicit wake in tests; notify seam lands
  with T8.1" or assign the seam.
- **COH-6 — S1 closure mislabeled a v0.1 obligation.** FlushTask exposes
  `new/spawn/stats/degraded` only (no stop/drain), but spec §6.1 `close()`
  requires a final flush and PHASE-8 T8.1 asserts "`close()` flushes the
  tail". The opus46-S1 rationale ("v0.7.0 item") is unsound as written;
  the deferral lands on T8.1 with no warning in the P8 doc. Fix: note in
  PHASE-8 (hand-roll drain: abort handle → take lock → drain_log → direct
  store.flush) or add a drain API to FlushTask now.
- **CON-3 — sqlite-without-path is a silent durability downgrade.**
  store/mod.rs:339 defaults missing path to `sqlite::memory:`; a
  user selecting the durable sqlite tier who forgets `path` gets a working
  process whose data evaporates at exit. Cockroach-without-DSN hard-errors;
  Level B's whole point is fail-closed selection. Fix: make missing path a
  hard error (or at minimum a loud warn + doc), and document
  `LAMBO_SQLITE_PATH` (CON-9).

## P3 findings — notes

Each is real but low-urgency; locations in the summary table.

- **GRAPH-6/7:** `from_snapshot` asymmetries — zero-interaction snapshots
  rejected ("found 0" chain-head error) so empty graphs don't round-trip;
  duplicate-natural-key edges silently merge via reinforcement (loaded ≠
  stored). Exposure: fixtures/seed paths; adapters can't produce either.
- **GRAPH-8:** no content validation → all empty/stopword-only junk merges
  into one key-`""` concept with a frozen arbitrary type; MCP will forward
  agent strings verbatim. Cheap guard at derive/record_action entry.
- **GRAPH-9:** no NFC/NFKC + ASCII-only camel split → non-ASCII homoglyph
  fragmentation. Robustness, not spec violation (fixtures are ASCII).
- **GRAPH-10:** mod.rs's "edges reference nodes upserted in the same batch"
  is false for record_action after a drain (edge-only batches); safe for
  current adapters, a trap for the next one. Fix the sentence or re-upsert.
- **STORE-5/6/7/8:** adapter-parity edges — session `created_at`
  Some-vs-None; MemoryStore non-atomic flush on illegal batches; corrupt
  contract-row handling differs per adapter; SQLite load panics
  (`row.get`) where Cockroach returns typed errors. None reachable on the
  demo path; all worth one-line notes in store/mod.rs or cheap fixes.
- **STORE-9:** no WAL/busy_timeout on file-backed SQLite → §2.2
  reader-contention flush failures; pairs with STORE-4's poison behavior.
- **TEST-6/7/8:** provision.sh line-based migration split (latent);
  fetch-bge-m3.sh no checksum (revision pinned mitigates); floating
  `stable` toolchain under `-D warnings` (hackathon-week time-sink risk).
- **COH-7/9/10/11/12:** board denominators wrong (P0 4/4 with T0.4
  blocked-account, P1 4/4 of five tasks, P7 1/3 of four); review index
  lists 4 of 21 records; T7.0 handoff self-contradicts on live-smoke status
  + cites a nonexistent filename; T1.5 erratum touched §12.2/§14 without
  listing them; P3 wave-1 twin commits break single-lineage assumptions
  (rebase P4–P7 branches — they sit one docs-commit behind main — and don't
  re-apply task commits on two lines).
- **CON-5/6/7/8/9:** VECTOR_SEARCH-capability vs vector_dimensions
  cross-check missing (one-line startup assert); blast-radius u64/i32 type
  split in frozen types (P6 must narrow ad hoc — pick the rule now);
  empty-input embed divergence (fixture Ok vs BGE Err(Unavailable) — which,
  per the documented degradation contract, would permanently disable hybrid
  on the first blank string); SQLite drops `Concept.embedding` on
  flush→load (documented at DDL level, unstated at trait level); missing
  `LAMBO_SQLITE_PATH` in .env.example.

## Cross-cutting themes

1. **The false-green triangle (TEST-1 + TEST-2 + TEST-5).** Skip-as-green
   live tests, CI that never compiles the adapters, and no committed
   evidence of live runs reinforce each other: a cockroach regression is
   currently undetectable by any artifact in the repo. Each fix is cheap;
   together they close the platform's biggest trust gap for the six-wide
   week.
2. **Embedding-space integrity is currently unenforceable end-to-end
   (STORE-1 + CON-2, with CON-7/CON-8 edges).** The contract type exists
   and is checked nowhere it matters: never persisted by production writes,
   dropped by cockroach seed, and mislabelable at the source by the BGE
   fallback. The never-cut §3.3 refusal promise needs a named owner before
   T7.2.
3. **Write-gate validation is one notch too trusting at the graph tier
   (GRAPH-1/2/4/8).** P2's internal invariants are strong, but the public
   write surface accepts type-confused matches, type-invalid edges,
   fabricated canonization transitions, and junk content. P4–P6 build
   *on top of* these gates; hardening them now is cheaper than debugging
   through them later.
4. **Doc promises to future phases drifted in four places
   (COH-2/3/5/6).** Each costs a downstream agent an hour of confusion at
   the week's highest-friction moment; each is a five-minute doc fix now.

## Recommended disposition order

Before P4/P5/P6 launch (cheap, unblocking):
1. TEST-2 CI matrix + sqlite test-module cfg gate (minutes; protects
   everything else all week)
2. TEST-1 loud skips (+ TEST-5 evidence convention for the next live run)
3. Doc corrections: COH-2 (rmcp), COH-5 (P4 wake sources), COH-6 (P8 drain
   note), COH-3 owner call, COH-7 board arithmetic
4. CON-1 `create_if_missing` + CON-3 fail-closed path (small, spec-visible)

Before the tracks that consume them:
5. GRAPH-1 type-aware matcher — before P5/P6 trust write-path output
6. GRAPH-4 transition validation (+ COH-3 field decision) — before T6.x
7. GRAPH-5 direction-blind CoOccurrence — before T4.1 scoring calibrates
   density
8. COH-1 vector-index query decision — decide now, implement by T7.3
9. STORE-1 contract write path + CON-2 fail-hard/log — before T7.1/T7.2
10. STORE-2/3/4 flush-loop hardening — before the demo relies on
    write-behind under real network conditions

P3s: batch opportunistically; none block a track.

## Verdict

**The P0–P3 platform holds.** Two closed review cycles' remediations are
all genuinely present on main; the gates are green on every combination
that compiles; the dev-diary is honest to an unusual degree (statuses,
cited tests, fixture numbers, commit references all check out); lock
discipline, Level B fail-closed selection, SQL parameterization, and secret
hygiene all pass adversarial inspection.

What this E2E pass adds beyond the closed per-task reviews is the seams:
the write gates trust their callers one notch too much, the embedding
contract is scaffolding without a write path, the cockroach live-test
story cannot currently prove itself, and four doc promises to P4–P8 have
drifted from the shipped code. None of it is red today; all of it is
exactly the kind of thing that turns into a lost day when six tracks start
building on these surfaces Wednesday. Disposition the seven P1s first —
items 1–4 above are an afternoon combined.

— fable (claude-fable-5), 2026-08-11

---

## Disposition record (2026-08-12) — all 45 findings CLOSED

Remediated in eight waves (one commit per wave, main, each gated by an
adversarial review + remediation loop; the per-wave review agents caught 12
additional defects in the remediation itself, all fixed and re-reviewed before
their wave committed):

| Wave | Findings | Commit |
|---|---|---|
| 1 — CI & trust | TEST-1/2/3/5, CON-4 | 0cf585f |
| 2 — SQLite bootstrap/config | CON-1/3/9, STORE-9 | 671d99c |
| 3 — Graph write gates | GRAPH-1..8, GRAPH-10, COH-3, CON-6 | 1849d3e |
| 4 — Flush loop | STORE-2/3/4/6 | 3083586 |
| 5 — Embedding integrity | STORE-1, CON-2/5/7/8 | 95234b6 |
| 6/7 — Docs, decisions, git | COH-1/2/5/6/7/9/10/11/12, TEST-4, STORE-1 owner, STORE-5/7/8 | 6266f53 |
| 8 — Scripts/toolchain | TEST-6/7/8 | 28500f3 |
| Wrap-up — live-schema convergence | canonization_events.last_demotion_time on pre-existing clusters (caught by the Wave 1 loud-skip machinery on final main) | 99052e7 |

Final gates (main @ 99052e7, 2026-08-12): fmt clean; clippy `-D warnings`
clean; default 242 passed / 0 failed; `--features store-sqlite` 268+5 passed /
0 failed; `--no-default-features --features store-cockroach` 182 passed / 0
failed under `-D warnings`; `--features store-cockroach` 263 passed / 5 ignored
(live tests honest); fixtures regenerate byte-identical; live Cockroach
conformance 2/2 green with committed evidence
(dev-diary/evidence/20260812-025148-cockroach-live.txt); phase/p4-p7
fast-forwarded onto final main (COH-12).

The three cross-cutting themes from this review are materially closed:
the false-green triangle is gone (CI matrix covers all adapter combos, live
skips are loud and countable, live evidence is committed); embedding-space
integrity has a persisted contract write path with fail-closed resolve checks
and no silent fallbacks; the graph write gates validate types, transitions,
and content before mutation. Known follow-ups are tracked in the phase docs:
T7.3 implements DECISION D1 (global vector search + Rust-side filter, EXPLAIN
verified), and the T7.2/T8.1 owners are named in PHASE-7/PHASE-8.

## Verification addendum (2026-08-12) — fable

Independent verification of the disposition record's 45 closures, performed
against the worktree `worktrees/p4-review` on **`phase/p4-daemon` @ cd9340e**.
Method: for each finding, the original claim/failure scenario was re-read, the
claimed remediation located in this branch's code/docs (wave table → `git
show`), and checked for actually neutralizing the scenario — not merely
touching the file; regression tests were executed (`RUSTFLAGS="-D warnings"
cargo test --features store-sqlite` → **359 passed / 0 failed / 3 ignored**;
default `cargo test` → **328 / 0 / 3**; plus every ci.yml feature-matrix row,
see below). The 7 P1s were verified by the orchestrator (table included
verbatim); this addendum independently verifies the 38 P2/P3 dispositions and
spot-checks that P4's new daemon code does not regress them.

### Headline: branch-lineage gap — the last three main commits are NOT on this branch

Waves 1–7 (0cf585f, 671d99c, 1849d3e, 3083586, 95234b6, 6266f53) are ancestors
of HEAD. **Wave 8 (28500f3, TEST-6/7/8), the wrap-up (99052e7,
canonization_events.last_demotion_time convergence + the newest live evidence),
and the closure commit (dc5da31, the disposition record itself) are NOT
ancestors** — `phase/p4-daemon`'s first P4 commit (721dfe0, 08:29 IST) was cut
from 6266f53 even though final main (dc5da31, 02:53 IST) existed ~5.5 h
earlier. Consequences on this branch: (a) TEST-6/7/8 remediations are absent
(worktree shows the original defects); (b) pre-existing SQLite/Cockroach
databases would lack `canonization_events.last_demotion_time` (the wrap-up's
idempotent ALTER/ensure_column is missing from `migrations/cockroach/001_init.sql`
and `sqlite.rs::init_schema`); (c) this record file is the stale OPEN version
(no disposition record). P4 touched none of the affected files
(`git diff 6266f53..HEAD --stat`: only `src/daemon/*`, `src/graph/graph.rs`,
`src/lib.rs`, two docs), so a merge into main keeps main's fixed versions —
but the branch's own gates never ran against them, and the disposition claim
"phase/p4-p7 fast-forwarded onto final main (COH-12)" is factually false for
this branch. **Required before merge: merge/rebase final main into
`phase/p4-daemon` and re-run the matrix.**

### P4 regression found (TEST-2 surface, CON-4 class)

`cargo test --no-default-features --features store-cockroach` and
`--no-default-features --features store-sqlite` — two of the five ci.yml
feature-matrix rows — **fail to build on this branch**: E0432 unresolved
`crate::fixtures` at **src/daemon/conflict.rs:212** (module-level `use` in
`#[cfg(test)] mod tests` without a `fixtures` gate) and
**src/daemon/hotlist.rs:606** (`fixture_graph_predicate_revalidates_against_real_state`
lacks `#[cfg(feature = "fixtures")]`; the sibling daemon tests are correctly
gated). This is exactly the CON-4 class Wave 1 fixed in sqlite.rs. It shipped
invisibly because CI cannot see this branch (see TEST-3 below). Matrix status
on this branch: sqlite GREEN (359/0), minimal GREEN, demo GREEN,
**sqlite-minimal RED, cockroach RED**. TEST-2 itself: VERIFIED on the P0–P3
surface (main), **REGRESSED by P4** at the two lines above.

### P1 verification (orchestrator, verbatim)

| ID | Orchestrator verdict |
|---|---|
| GRAPH-1 | VERIFIED — canonical.rs ~:131 filters `concept_type != Observation` + `min_by_key(c.id.0)` deterministic tie-break; regression tests incl. Observation-must-not-match and Entity-wins-over-shadowing-Observation |
| STORE-1 | VERIFIED (P3 scope) — UPSERT_SESSION_SQL carries embedding_kind/model/dim + ON CONFLICT updates; seed binds all three from snapshot.embedding; runtime attach enforcement T8.1-owned (PHASE-8-surface.md ~:83 dated owner note) |
| TEST-1 | VERIFIED — live tests #[ignore]d; LAMBO_REQUIRE_LIVE=1 panics on missing DSN; non-ignored honesty-gate test; committed live evidence 20260812-025148-cockroach-live.txt (2/2 green, 65s, rustc 1.97.1) |
| TEST-2 | VERIFIED — ci.yml feature-matrix job (sqlite / sqlite-minimal / minimal / cockroach / demo) under RUSTFLAGS=-D warnings; both previously-broken combos re-run green locally by orchestrator (183+3 and 179+3) |
| COH-1 | VERIFIED as recorded decision — DECISION D1 in PHASE-7-embeddings.md ~:117 (global vector search + Rust-side session filter), T7.3 done-when requires committed EXPLAIN evidence; SQL intentionally unchanged until T7.3 |
| CON-1 | VERIFIED — sqlite.rs connect: .create_if_missing(true) + WAL/busy_timeout for file-backed; bootstrap→flush→load→reopen round-trip test (~sqlite.rs:2794) |
| CON-2 | VERIFIED — silent 400 retry-without-model removed; tracing::error! + hard EmbedError::Backend; rationale doc-pinned in bge_m3.rs |

Note (branch scope): TEST-1's cited evidence file 20260812-025148 arrived with
the wrap-up (99052e7) and is therefore main-only; this branch carries the
Wave-1 evidence (20260811-233251) plus the #[ignore]/REQUIRE_LIVE machinery.

### P2/P3 verification — 38 dispositions

| ID | Claimed disposition | Verdict | Evidence (this branch) |
|---|---|---|---|
| GRAPH-2 | Wave 3 / 1849d3e | VERIFIED | `record_edge` §5 endpoint gate graph.rs:1116–1125 via `edge_endpoint_error` :1268; `assert_invariants` scan arm :948–955; tests `record_edge_rejects_type_invalid_endpoints` :1655, `assert_invariants_flags_type_invalid_edges` :1696. P4 spot-check: daemon prod paths write only via `remove_node`/`bump_gc_survived`/`upsert_edge` — no bypass |
| GRAPH-3 | Wave 3 / 1849d3e | VERIFIED | iterative `dfs_cycle` (explicit stack, three-color, dead `path` vec gone) graph.rs:1211–1236; test `deep_chain_cycle_check_does_not_overflow_stack` :2511 |
| GRAPH-4 | Wave 3 / 1849d3e | VERIFIED | `from_status != current` rejected graph.rs:599–605; §10 matrix `legal_canonization_transition` :606–612/:1301; matrix pinned as conservative inference in notes/adve-wave3-graph-decisions.md; tests `transition_from_status_mismatch_is_rejected` :1859, `illegal_transition_pairs_are_rejected` :1893. P4 spot-check: `set_root_goal` auto-Venerable routes THROUGH this gate (graph.rs:679–706) — no regression |
| GRAPH-5 | Wave 3 / 1849d3e | VERIFIED | both-direction CoOccurrence probe, existing direction adopted, derive.rs:469–478; test `derive_swapped_order_reinforces_single_cooccurrence_edge` :1132 |
| STORE-2 | Wave 4 / 3083586 | VERIFIED | `FLUSH_ATTEMPT_TIMEOUT` 30s flush.rs:73, timeout+panic containment :451–457; cockroach per-statement `statement_timeout` cockroach.rs:440–444/:838; test `hung_store_flush_times_out_never_wedges_the_loop` :1648 |
| STORE-3 | Wave 4 / 3083586 | VERIFIED | degraded branch drops each drained batch + clears pending flush.rs:325–333 (spec §2.3 "none = pure RAM"); test `degrades_past_log_max_and_stops_flushing` :1252 |
| STORE-4 | Wave 4 / 3083586 | VERIFIED | `StoreError::Constraint` types/mod.rs:496 + `is_retryable` :508; structured classifier store/error.rs:47–96 (SQLSTATE 23xxx / SQLITE code 19 → Constraint; 40xxx/08xxx/57P01-3/BUSY → retryable; substring "40001" gone); dead-letter drop-after-log + `FlushStats::dead_lettered` flush.rs:383/:111; test `constraint_violation_dead_letters_the_batch` :1731 |
| TEST-3 | Wave 1 / 0cf585f | **NOT VERIFIED** | ci.yml:5 `branches: [main, master, 'phase-*']` — GitHub Actions `*` does not match `/`, and real phase branches are `phase/<slug>` (dev-diary/README.md:164/:214; this branch is `phase/p4-daemon`), so the push trigger can never fire for them (none is pushed to origin either). Failure scenario demonstrably NOT neutralized: 2/5 matrix rows are red on this branch, invisibly. Fix: `'phase/**'` + push phase branches |
| TEST-4 | Waves 6/7 / 6266f53 | DOC-DISPOSITIONED | exit criteria re-worded honestly (the review's own alternative): PHASE-1-contracts.md:82/:172 and PHASE-3-stores.md:186–194 name the three per-adapter suites and state "there is NO generic tests/store_conformance.rs" |
| TEST-5 | Wave 1 / 0cf585f | VERIFIED | scripts/capture-cockroach-evidence.sh + committed dev-diary/evidence/20260811-233251-cockroach-live.txt (2/2 green). Note: the newer 20260812-025148 evidence is main-only (wrap-up commit) |
| COH-2 | Waves 6/7 / 6266f53 | DOC-DISPOSITIONED | rmcp-removed + T8.2 re-add owner named in all three claimed places: PHASE-0-ground.md:44/:132, spec §6.3 erratum (lambo-hackathon-spec-v0.1.md:549, incl. 0.1.x-vs-v3 choice), PHASE-8-surface.md:109 |
| COH-3 | Wave 3 / 1849d3e (+ wrap-up 99052e7) | VERIFIED (Wave-3 scope) | `CanonizationEvent.last_demotion_time` types/mod.rs:456–463; `demote()` stamps demote.rs:151 (test :293–295); non-clobber propagation graph.rs:615–620; column in both DDLs (cockroach :48/:113, sqlite :81/:118); cockroach COALESCE :236–242 + SQL-shape test :1552–1554; sqlite insert :416; memory :159–162; P6 audit-row carry flagged in the decisions note. **Caveat:** the wrap-up convergence for pre-existing DBs (99052e7) is NOT on this branch — see headline |
| COH-5 | Waves 6/7 / 6266f53 | DOC-DISPOSITIONED | PHASE-4-daemon.md:50–54: poll `Graph::epoch()`, "There is no mutation-notify channel and no T3.5 rescore signal — both were explicitly deferred", seam lands T8.1. Survived P4's edits; P4's daemon actually implements the polling design (:191–192) |
| COH-6 | Waves 6/7 / 6266f53 | DOC-DISPOSITIONED | PHASE-8-surface.md:36–64: opus46-S1 "v0.7.0" closure declared unsound; P8-owned hand-rolled drain in `Memory::close` (Notify stop + `biased;` select + requeue_pending, drain via drain_log + direct store.flush). Consistent with code: FlushTask still exposes no stop/drain (T8.1-owned by design) |
| CON-3 | Wave 2 / 671d99c | VERIFIED | sqlite-without-path hard error store/mod.rs:368–377 ("SqliteStore requires a path (store.path or LAMBO_SQLITE_PATH)"); test `sqlite_without_path_is_hard_error` :517–530 |
| GRAPH-6 | Wave 3 / 1849d3e | VERIFIED | zero-interaction snapshot = valid empty graph graph.rs:161–166; test `empty_snapshot_roundtrips` :2478 |
| GRAPH-7 | Wave 3 / 1849d3e | VERIFIED | duplicate natural-key edges rejected up front graph.rs:197–208; test `from_snapshot_rejects_duplicate_natural_key_edges` :2490 |
| GRAPH-8 | Wave 3 / 1849d3e | VERIFIED | `reject_empty_key` read-only pre-pass derive.rs:204–227/:489 and action.rs:260–266/:346; test `record_action_rejects_empty_and_stopword_only_content` action.rs:752 |
| GRAPH-9 | **none — absent from the wave table** | **NOT VERIFIED** | No NFC/NFKC normalization anywhere (no unicode-normalization dep in Cargo.toml; canonical.rs unchanged on this axis), and no disposition/acceptance recorded — `git grep GRAPH-9 dc5da31` hits only the finding's own two lines. The disposition table ("GRAPH-1..8, GRAPH-10") silently skips it: **"all 45 findings CLOSED" is arithmetically false — 44 dispositioned** |
| GRAPH-10 | Wave 3 / 1849d3e | DOC-DISPOSITIONED | batch contract corrected as the review recommended: graph/mod.rs:20–30 — record_action after a drain produces edge-only batches; "Adapters MUST tolerate edge rows" whose endpoints committed in a prior flush |
| STORE-5 | Waves 6/7 / 6266f53 | DOC-DISPOSITIONED | accepted divergence recorded on the trait: store/mod.rs:91–95 "created_at parity (STORE-5, accepted divergence)... do NOT rely on created_at presence" — matches the review's one-line-note recommendation |
| STORE-6 | Wave 4 / 3083586 | VERIFIED | MemoryStore working-copy + swap-on-full-success memory.rs:198–261; test `failed_flush_leaves_session_state_unchanged` :999 |
| STORE-7 | Waves 6/7 / 6266f53 | VERIFIED | `session_embedding_from_parts` cockroach.rs:402–417: kind-XOR-dim → typed Backend corruption error (negative dim typed), mirroring sqlite; test `session_embedding_xor_corruption_errors_not_silent_none` :1737 |
| STORE-8 | Waves 6/7 / 6266f53 | VERIFIED | sqlite.rs load paths: 57 `.try_get(` sites, zero remaining panicking `row.get(` (swept grep) |
| STORE-9 | Wave 2 / 671d99c | VERIFIED | file-backed WAL + busy_timeout(8s) sqlite.rs:217–228; `is_in_memory_uri` sqlx-grammar guard :244; non-vacuous 8s>5s-default assertion test :2809–2811 |
| TEST-6 | Wave 8 / 28500f3 | **NOT VERIFIED (on this branch)** | worktree scripts/provision.sh:77–79 is still the line-based `grep -v -i 'CREATE VECTOR INDEX'` split — the statement-aware scanner exists only on main (28500f3 not an ancestor). P4 didn't touch the file; a merge restores the fix, but this branch's provisioning is the original defect |
| TEST-7 | Wave 8 / 28500f3 | **NOT VERIFIED (on this branch)** | scripts/fetch-bge-m3.sh has no sha256/checksum verification in the worktree (grep: zero hits); pinned-sha fix is main-only |
| TEST-8 | Wave 8 / 28500f3 | **NOT VERIFIED (on this branch)** | rust-toolchain.toml `channel = "stable"` and ci.yml `dtolnay/rust-toolchain@stable` (:18) still float under `-D warnings`; the 1.97.1 pin is main-only |
| COH-7 | Waves 6/7 / 6266f53 | DOC-DISPOSITIONED | board denominators corrected and survived P4's board edit: dev-diary/README.md — P0 3/4, P1 5/5, P7 1/4 (P4 row 6/6 added alongside, not clobbering) |
| COH-9 | Waves 6/7 / 6266f53 | DOC-DISPOSITIONED | adversarial-review/README.md indexes all 22 records (verified against the directory: 23 files − README). Branch-lineage note: the e2e row still reads OPEN here because dc5da31 is main-only |
| COH-10 | Waves 6/7 / 6266f53 | DOC-DISPOSITIONED | PHASE-7-embeddings.md:243 "Live smoke status (COH-10, 2026-08-12 — corrected): ... PASSED"; :217 now cites adve-review-t70-embeddings.md, which exists |
| COH-11 | Waves 6/7 / 6266f53 | DOC-DISPOSITIONED | PHASE-1-contracts.md:258–261: erratum record now names §12.2 and §14 explicitly, with the COH-11 correction note |
| COH-12 | Waves 6/7 / 6266f53 (claim: "phase/p4-p7 fast-forwarded onto final main") | **NOT VERIFIED — violated by this branch** | phase/p4-daemon forked from 6266f53; P4 work began 08:29 IST, ~5.5 h after Wave 8/wrap-up/closure landed on main — the branch sits three commits behind final main, the exact lineage-hygiene failure COH-12 flagged (no twin-commit duplication observed on the branch itself, but the recorded fast-forward claim is false for the one phase branch now in flight) |
| CON-5 | Wave 5 / 95234b6 | VERIFIED | `resolve_backends` fail-closed both-direction check (VECTOR_SEARCH ⇄ vector_dimensions) resolve.rs:43–66; stub-store refusal tests :209–285 |
| CON-6 | Wave 3 / 1849d3e | DOC-DISPOSITIONED (rule pinned, D6) | trait doc store/mod.rs:117–124: implementers MUST narrow at the write gate via `u32::try_from` → typed invariant error, never a silent `as`; D6 recorded in notes/adve-wave3-graph-decisions.md. (Read-side i64→i32 `as` casts remain at cockroach.rs:722/:779 — load-path, outside D6's pinned write-gate scope; worth a P6-era sweep) |
| CON-7 | Wave 5 / 95234b6 | VERIFIED | Embedder trait input contract embed/mod.rs:37–41 (empty/whitespace MUST reject with Unavailable); FixtureEmbedder enforces identically fixture.rs:119–123; test `rejects_empty_and_whitespace_input` :144 |
| CON-8 | Wave 5 / 95234b6 | VERIFIED (upgraded beyond the doc-note recommendation) | shared codec store/vector.rs (non-finite rejected); sqlite upsert+select carry `Concept.embedding` (sqlite.rs:6–7 module doc; corrupt blobs → typed error); parity covered in the green sqlite suite |
| CON-9 | Wave 2 / 671d99c | VERIFIED | .env.example documents `LAMBO_SQLITE_PATH=./lambo.db` with the hard-error/no-silent-memory note (mirrors CON-3) |

### Not verified / regressed — goes to the P4 remediation

1. **Branch lineage (headline):** merge/rebase final main (dc5da31) into
   `phase/p4-daemon` before the P4 merge — restores TEST-6/7/8, the COH-3
   wrap-up convergence, the closed record + index, and the pinned toolchain.
   No file overlap with P4's diff, so it is a clean merge.
2. **TEST-2 surface REGRESSED by P4:** src/daemon/conflict.rs:212 (module-level
   `use crate::fixtures;`) and src/daemon/hotlist.rs:606 (ungated fixture test)
   break `--no-default-features --features store-cockroach` and
   `--no-default-features --features store-sqlite` (E0432; 2/5 matrix rows
   red). Gate both with `#[cfg(feature = "fixtures")]` (CON-4 pattern).
3. **TEST-3 NOT VERIFIED:** ci.yml `'phase-*'` glob cannot match `phase/<slug>`
   branches (GitHub `*` doesn't cross `/`). Change to `'phase/**'` and push the
   phase branches — this exact gap is why item 2 shipped invisibly.
4. **GRAPH-9 NOT VERIFIED / undispositioned:** no NFC/NFKC remediation and no
   recorded acceptance anywhere (wave table skips it). Either fix (normalize in
   `canonicalize`) or record an explicit accepted-residual-risk disposition;
   until then the record's "all 45 CLOSED" claim is false by one.
   GRAPH-9 remediated in this branch (`phase/p4-daemon`, Wave 7).
5. **COH-12 NOT VERIFIED:** the recorded "fast-forwarded onto final main" claim
   is false for `phase/p4-daemon`; re-anchor the branch (item 1) and correct or
   re-date the claim.
6. **TEST-6/7/8 NOT VERIFIED on this branch** (remediations are main-only,
   28500f3): resolved automatically by item 1.

**Verification: 7/7 P1, 32/38 P2/P3 verified (21 code + 11 doc-dispositioned);
closure stands except GRAPH-9, TEST-3, COH-12 — and, on this branch only,
TEST-6/7/8 (Wave 8 absent) plus a P4 regression of the TEST-2 matrix. Merge
final main into phase/p4-daemon, gate the two fixtures imports, fix the CI
glob, and disposition GRAPH-9 before phase/p4-daemon merges.**

— fable (claude-fable-5), verification pass, 2026-08-12 (phase/p4-daemon @ cd9340e)
