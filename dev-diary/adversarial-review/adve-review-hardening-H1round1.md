# Adversarial Review - Hardening H1, round 1

- **Reviewer:** `h1_review_r1` (independent, source read-only)
- **Scope:** implementation commit `1a3accf9d1349dde7ce01cc41538fa755275436c`
  against base `f32af2d38380a9e01cf7bd31439467ed383543ec`, plus the full H1
  specification in `dev-diary/notes/hardening-tasks.md`
- **Worktree:** `/Users/narayan/Documents/work/lambo/worktrees/hardening-h1`
- **Verdict:** **REQUEST_CHANGES** - 2 P1, 1 P2, 1 P3

The implementation correctly centralizes the normal stored/live comparison,
keeps legacy nullable contracts loadable, uses ordered `SetEmbedding` mutations,
and preserves structural access for a mismatched `serve-web`. It is not clean,
however: the ordinary refusal wedges the writer lease, and a reader can still
return vector results from a contract that changed after its one compatibility
check. The portal warning is also a startup snapshot rather than live state.

## Findings

### H1-R1-1 (P1) - The expected mismatch refusal strands the durable writer lease, so the advertised override retry fails

- **Evidence:** `src/memory.rs:612-637` acquires the 45-second store lease before
  loading the session. The new refusal returns directly at `src/memory.rs:652-671`
  without calling `release_lease`. The source comment at `src/memory.rs:612-614`
  explicitly confirms that a model-mix refusal leaves the lease until TTL.
- **Impact:** The normal CLI flow is: try a writer, receive the actionable
  mismatch error telling the operator to retry with
  `--allow-embedding-mismatch`, then run that explicit retry in a new process.
  The new PID produces a different `LeaseHolder`, so the retry is rejected as a
  second writer for up to `LEASE_TTL` (45 seconds). This fails H1's explicit
  verification requirement that the override works and can also block an
  unrelated correctly configured writer after a typo/misconfiguration.
- **Reproduced through the real binary and SQLite adapter:** provisioned a
  temporary SQLite store; wrote session `h1-lease-review` with fixture model
  `fixture-model-v1`; reopened with `fixture-model-v2` and got the intended
  mismatch; immediately retried in a new process with the override and got
  `conflict: session h1-lease-review is already held by another writer` rather
  than a successful rename.
- **Why tests miss it:** `src/memory.rs:3410-3435` and
  `src/cli/mod.rs:955-992` run refusal and override inside one test process with
  the same agent. The lease token is agent + PID + host, so the second build is
  treated as the same holder refreshing its lease; it does not model two CLI
  invocations.
- **Required remediation:** Treat a contract refusal as a clean startup failure,
  not a crash. Release the acquired lease on every post-acquire error (including
  refused overrides), or restructure startup so the comparison can be made
  safely without leaking a lease. Add a subprocess/different-holder regression
  proving the immediate override retry and a correctly configured retry both
  work without waiting for TTL.

### H1-R1-2 (P1) - Reader recall has a check/use race and can still rank against a newly incompatible vector space

- **Evidence:** `src/cli/mod.rs:67-73` loads a snapshot and compares its contract
  once. `src/cli/recall.rs:70-115` then constructs the reader, awaits query
  embedding, and later enters recall. The vector leg ultimately calls
  `GraphStore::vector_candidates` (`src/graph/hybrid.rs:513-516`). Cockroach's
  implementation at `src/store/cockroach.rs:2112-2125` only checks that the
  session currently has *some* non-null contract; the expected live contract is
  not part of the store call and is never compared there.
- **Impact:** `serve-web` is intentionally concurrent with a writer. A reader
  can load contract A and pass the check, then a writer can atomically migrate
  the durable session to contract B and B vectors while the reader awaits the
  embedder. The subsequent candidate query accepts the non-null B contract and
  returns B-space rankings for an A-space query. That is the exact silent,
  plausible-but-meaningless result H1 is meant to eliminate. Direct
  `lambo recall` has the same window.
- **Required remediation:** Bind the expected `EmbeddingContract` to the vector
  candidate read itself so the adapter atomically refuses unless the durable
  contract still matches (with Memory/SQLite/Cockroach parity), or provide an
  equivalently race-free protocol. A check only before embedding is
  insufficient. Add a controlled store regression that changes A to B between
  the initial load and candidate lookup and proves no vector ranking is
  returned.

### H1-R1-3 (P2) - `serve-web` freezes compatibility at startup, so the prominent warning and `/api/session` become false during live writes

- **Evidence:** `src/cli/serve_web.rs:1187-1212` computes
  `EmbeddingStatus::inspect` exactly once and stores it in `AppState`.
  `/api/session` returns that clone at `src/cli/serve_web.rs:810-824`; it never
  reloads the session contract. The browser fetches `/api/session` only once at
  `web/app.js:435-440`; its recurring poll (`web/app.js:271-287`) fetches only
  `/api/pulse`, whose payload contains no embedding status.
- **Impact:** Starting the portal on an unrecorded/compatible session and then
  attaching a writer with another contract leaves the page claiming
  `unrecorded`/`compatible`, leaves `vector_search` true, and never renders the
  required warning. Starting mismatched and later repairing/re-embedding leaves
  the opposite stale warning. The recall endpoint often refuses because it
  reloads independently, but H1 explicitly selected structural-only mode with
  a *prominent, actionable, live* mismatch surface; the UI and API state do not
  satisfy that policy for the long-running portal.
- **Why tests miss it:** `src/cli/serve_web.rs:2655-2725` manually constructs a
  fixed mismatched `AppState` and only tests that immutable state. It never
  changes the store contract after the router starts; its UI assertion merely
  checks that two strings exist in the embedded JavaScript.
- **Required remediation:** Refresh compatibility from durable state as part of
  the live polling contract (or otherwise invalidate/update it), update the
  banner in both directions without duplication, and test compatible ->
  mismatch -> compatible while the same server and browser polling model stay
  alive.

### H1-R1-4 (P3) - The writer-only escape hatch is globally advertised and accepted by reader subcommands

- **Evidence:** `src/main.rs:19-25` declares the option as `global = true`.
  Clap therefore advertises it in `lambo recall --help`, `lambo serve-web
  --help`, and store-only reader help even though `src/main.rs:393-399` later
  rejects it. The test at `src/main.rs:685-703` positively asserts that a reader
  parses the dangerous option rather than asserting parser-level scoping.
- **Impact:** Read-only commands present a dangerous writer operation in their
  documented interface and accept its syntax, only to fail later with a usage
  error. This contradicts the option's own writer-only contract and makes the
  most sensitive new flag needlessly confusing.
- **Required remediation:** Scope the flag to writer subcommands (or a flattened
  writer-only argument group) so reader help does not advertise it and reader
  parsing rejects it directly. Add help/parser assertions for both writer and
  reader commands.

## Positive observations

- `session_embedding_compatibility` compares kind, model, and dimension exactly;
  equal width alone is not accepted.
- The operator relabel path refuses width changes and cross-kind relabeling while
  vectors remain (`src/graph/graph.rs`), while the pre-existing store mutation
  paths quarantine legacy uncontracted vectors atomically on first stamp.
- Normal writer construction still flows through `ResolvedBackends` and
  `MemoryBuilder`; no duplicate backend factory was introduced, preserving the
  Level B single-construction rule.
- Structural `serve-web` routes do not acquire a writer lease, and the actual
  recall route reuses the CLI recall path rather than implementing a second
  ranking pipeline.

## Commands and results

| Command/check | Result |
|---|---|
| `git diff --stat f32af2d..1a3accf` and full per-file diff/context trace | 12 files, +772/-78; all changed production and test paths reviewed |
| `cargo test h1_ --all-features` | pass: 5 library H1 tests + 1 binary parser test |
| `env -u RUST_LOG cargo test --all-features` | pass: library 825 passed/8 ignored; every binary/integration/doc harness passed; 2 additional live calibration tests ignored |
| `env -u RUST_LOG cargo test --no-default-features --features store-sqlite,embed-fixture h1_` | pass: 3 library H1 tests + 1 binary parser test |
| `cargo fmt --all -- --check` | pass |
| `git diff --check f32af2d..1a3accf` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass |
| Real binary SQLite refusal -> immediate override retry in a new process | reproduced H1-R1-1: first command returned the intended contract mismatch; override retry returned live-lease conflict |
| `target/debug/lambo recall --help`; `target/debug/lambo serve-web --help`; `target/debug/lambo derive --help` | reproduced H1-R1-4: override advertised on readers and writer alike |

No live CockroachDB test was run because this review had no
`LAMBO_COCKROACH_DSN`; the all-feature suite reported its live legs as ignored,
not passed. The Cockroach production SQL/read paths were inspected directly.

## Verdict

**REQUEST_CHANGES.** H1 is not safe to integrate with H1-R1-1 and H1-R1-2
open. After remediation, re-review must exercise a cross-process lease retry, a
contract-changing reader race, and a live `serve-web` status transition rather
than only fixed in-process snapshots.

## Remediation disposition

- **Remediation agent:** `h1_remediation_r1`
- **Remediation commit:** `c72acf5fb1abd0f909d8bc2ef15f6d579df0d2fd`
- **Disposition:** all four round-1 findings remediated; awaiting independent
  re-review. The original `REQUEST_CHANGES` verdict above is unchanged.

### H1-R1-1 (P1) - remediated

`MemoryBuilder::build` now groups every fallible startup step after lease
acquisition in one startup result and holder-scoped releases the lease before
returning any clean startup error (`src/memory.rs:645-691`). This covers load,
legacy stamp, default mismatch refusal, and refused/failed operator override;
the process-crash path still correctly relies on TTL.

`tests/cli_write_lease.rs:231-300` is a real shipped-binary SQLite regression.
Distinct processes write model v1, refuse model v2, immediately retry the
correct v1, refuse v2 again, and immediately retry v2 with the override. Both
post-refusal acquisitions succeed without waiting for the 45-second lease TTL.

### H1-R1-2 (P1) - remediated

`GraphStore::vector_candidates` now requires the exact contract that produced
the query embedding (`src/store/mod.rs:155-172`). That contract travels with
the embedding through every production vector path: reader recall
(`src/cli/recall.rs`, `src/daemon/mod.rs`, `src/recall/candidates.rs`) and hybrid
writer matching (`src/graph/hybrid.rs`). All adapter and test-double signatures
were updated, so a new caller cannot accidentally omit it.

Cockroach validates the durable contract and runs both the global-index query
and exact session fallback in one serializable read transaction
(`src/store/cockroach.rs:2080-2192`). A concurrent ordered `SetEmbedding` plus
vector rewrite is therefore observed wholly before or wholly after candidate
retrieval; a newly incompatible contract returns an error before rankings.
Memory and SQLite retain their no-vector capability refusal while implementing
the same contract-bearing trait surface.

`src/recall/candidates.rs:523-542` deterministically loads contract A, changes
the store to contract B at the point where a reader would be awaiting its query
embedding, and proves candidate gathering returns a mismatch error instead of
the planted high-confidence hit. This regression could not be expressed
safely against the old contract-free candidate interface.

### H1-R1-3 (P2) - remediated

`AppState` no longer freezes embedding compatibility at startup. `/api/session`
loads the live durable snapshot for every request (`src/cli/serve_web.rs:823`),
and the existing `/api/pulse` polling response now carries compatibility and
trusted-vector availability from the same live stats snapshot
(`src/cli/serve_web.rs:455-464`, `:877-909`). Structural routes remain
available and recall still uses the fail-closed reader path.

The browser reconciles the embedding banner on initial session load and every
pulse, removes it after repair, and avoids duplicate/unchanged DOM insertions
(`web/app.js:82-106`, `:290`). The same-server regression at
`src/cli/serve_web.rs:2682-2794` proves compatible -> mismatch -> compatible
through both `/api/session` and `/api/pulse`, structural availability, and
mismatched recall refusal.

### H1-R1-4 (P3) - remediated

The override is no longer a global Clap option. It exists only on writer
subcommands and is extracted from those variants before the single backend
construction (`src/main.rs:55-59`, `:322-353`, `:421-440`). Reader parsing now
rejects the option directly, reader help omits it, and writer help advertises
it (`src/main.rs:703-747`).

### Remediation verification

| Command/check | Result |
|---|---|
| `env -u RUST_LOG cargo test --all-features h1_ -- --nocapture` | pass: 6 library H1 tests, binary parser test, and real subprocess SQLite lease regression |
| `env -u RUST_LOG cargo test --all-features` | pass: library 825 passed/8 ignored; every binary, integration, and doc harness passed; 2 live calibration tests ignored |
| `env -u RUST_LOG cargo test --no-default-features --features store-sqlite,embed-fixture` | pass: library 510 passed; every enabled binary, integration, and doc harness passed |
| `cargo test --all-features store::cockroach::tests::` | pass: 23 non-live Cockroach unit tests |
| `cargo check --all-targets --all-features` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass |
| `cargo clippy --all-targets --no-default-features --features store-sqlite,embed-fixture -- -D warnings` | pass |
| `cargo fmt --all -- --check`; `git diff --check` | pass |

No live CockroachDB conformance was run because `LAMBO_COCKROACH_DSN` was not
available. The next independent reviewer should inspect the serializable read
transaction and, when a DSN is available, exercise a concurrent contract/vector
rewrite against both the global-index and exact-fallback candidate paths.
