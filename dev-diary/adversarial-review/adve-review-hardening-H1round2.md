# Adversarial Review - Hardening H1, round 2

- **Reviewer:** `h1_review_r2` (independent, source read-only)
- **Scope:** remediation commit `c72acf5fb1abd0f909d8bc2ef15f6d579df0d2fd`
  and disposition commit `298af97a4049e6ff4e642ec7be54dcec0373bc39`,
  against base `f32af2d38380a9e01cf7bd31439467ed383543ec`
- **Worktree:** `/Users/narayan/Documents/work/lambo/worktrees/hardening-h1`
- **Verdict:** **REQUEST_CHANGES** - 1 P2, 1 P3

All four round-1 findings are genuinely closed. In particular, a rejected
writer attach now holder-scoped-releases its lease, every in-repo vector
candidate path carries the exact query contract, the portal reloads durable
compatibility on every session/pulse request and reconciles the banner in both
directions, and Clap exposes the override on all six writer commands and none
of the six non-writer commands. No unsafe vector ranking was found after a
contract change.

The remediation is not clean yet. It made the race-free read a source-breaking
change to Lambo's frozen public adapter trait rather than an additive checked
surface, and the new Cockroach multi-statement SERIALIZABLE read is the only
such hot transaction in this adapter that does not use the existing retry
protocol.

## Round-1 finding closure

### H1-R1-1 (P1) - closed

`MemoryBuilder::build` validates the merged config before lease acquisition and
then contains every fallible post-acquisition startup operation (`load_session`,
legacy quarantine/stamp, mismatch refusal, and override validation) in the
`startup` result at `src/memory.rs:645-691`. Every error attempts
`release_lease(session, holder)` before returning the original startup error.
All three real stores scope deletion to the holder token, so a stale cleanup
cannot evict a replacement writer.

The subprocess regression is non-vacuous: `tests/cli_write_lease.rs:231-300`
uses the shipped binary for five separate invocations. It first writes model
v1, refuses v2, immediately succeeds with v1, refuses v2 again, and immediately
succeeds with the v2 override. Each process has a distinct PID-bearing
`LeaseHolder`; neither successful retry waits for the 45-second TTL.

### H1-R1-2 (P1) - closed for safety

The query-producing contract is now mandatory at the in-repo candidate
boundary and is propagated through reader recall, daemon recall, phase-1
gather, and hybrid writer matching. The compiler-enforced signature reached
all production calls and all in-repo adapters/test doubles. Cockroach reads the
durable contract, global ANN candidates, and exact-session fallback inside one
transaction (`src/store/cockroach.rs:2098-2193`), so both candidate branches
share the contract read's serializable snapshot. A newly incompatible contract
returns before either ranking can escape.

`recall::candidates::tests::h1_contract_change_between_initial_load_and_candidate_read_returns_no_rankings`
is deterministic rather than scheduler-probabilistic: it captures A, mutates
the store to B, invokes the candidate read with A, and verifies the planted
0.99 hit is not returned. The test double checks the contract inside the same
method that would return hits, so it directly pins the repaired call boundary.

Memory and SQLite correctly retain `Capability` refusal because neither
advertises vector search. Legacy snapshots are materialized after their
unattested vectors are stripped in RAM; the first durable `SetEmbedding`
quarantines those vectors transactionally in each store. Exact equality covers
kind, nullable model, and dimension. The known `None == None` server-default
blind spot is honestly recorded in the H1 handoff and cannot be solved by the
existing contract format.

### H1-R1-3 (P2) - closed

`AppState` no longer caches compatibility. `/api/session` reloads and
reclassifies the durable snapshot per request. `/api/pulse` obtains
`EmbeddingStatus` from the same live stats reload and derives `vector_search`
from that status. The structural stats/graph/inspect paths load without an
embedder contract and remain available, while `/api/recall` reuses the
contract-enforcing CLI reader.

The browser's `applyEmbeddingStatus` is idempotent by status/message key,
removes an obsolete banner, inserts exactly one mismatch banner, and updates
the search-capability copy on initial session load and every pulse. The
same-server test exercises compatible -> mismatch -> compatible through both
APIs, verifies structural routes remain 200, and verifies mismatched recall
returns 502 with both model ids. It does not execute a real DOM, but the state
transition itself is pinned server-side and the small DOM routine was inspected
directly; no failure-handling regression was found.

### H1-R1-4 (P3) - closed

The option is declared only on `serve`, `demo`, `derive`, `record-action`,
`reserve`, and `release`. The built binary's help contains it on all six and
omits it on `recall`, `serve-web`, `saints`, `inspect`, `stats`, and
`provision`; reader parsing rejects it at Clap rather than later in dispatch.
Extraction happens before the one `resolve_for_command` construction and only
sets the already-resolved bundle, preserving Level B's single-construction
site.

## New findings

### H1-R2-1 (P2) - The remediation breaks the frozen public `GraphStore` adapter API instead of adding a compatible checked read

- **Evidence:** `src/store/mod.rs:165-171` changes the required public trait
  method from `vector_candidates(session, embedding, limit)` to
  `vector_candidates(session, embedding, expected_contract, limit)`. Any
  external Level B store adapter implementing the v0.2.0 trait, and any library
  caller invoking the old method, now fails to compile. `GraphStore` is the
  documented plugin contract, and `dev-diary/README.md:97-100` explicitly
  freezes it after P1. The rule permits a necessary change only with
  MemoryStore/fixture updates and a handoff naming every dependent task. The
  in-repo implementations were updated, but no compatibility surface or
  dependent-task migration record exists.
- **Scope evidence:** the H1 claim owns 12 code/UI paths at
  `hardening-tasks.md:35-37`, but the required-trait ripple edited another 10
  production/test paths, including `src/store/mod.rs`, `src/store/memory.rs`,
  `src/store/{flush,load}.rs`, `src/recall/candidates.rs`, `src/daemon/mod.rs`,
  `src/graph/hybrid.rs`, and both canon modules. Those changes are mechanically
  coherent, but the unrecorded ownership expansion is exactly the integration
  hazard the frozen-contract convention is designed to prevent.
- **Impact:** H1 would make a released v0.2.0 library consumer rewrite its
  adapter just to compile, even when that adapter has no vector capability.
  This is avoidable: correctness requires a new race-safe production boundary,
  not removal of the existing public signature.
- **Required remediation:** preserve the existing required
  `vector_candidates(session, embedding, limit)` method and add an additive
  contract-checked method with a safe default (a vector-capable adapter that has
  not implemented atomic checking must fail closed, while non-vector adapters
  may retain their capability refusal). Route H1 production callers through the
  checked method and override it atomically in Cockroach. Alternatively,
  document and version a deliberate public breaking change, but that conflicts
  with the repository's frozen-contract rule and is not justified for this
  additive hardening task. Update the H1 ownership/handoff record for every
  touched dependent path.

### H1-R2-2 (P3) - The new Cockroach SERIALIZABLE candidate transaction omits the adapter's retry protocol

- **Evidence:** `src/store/cockroach.rs:2098-2193` calls `pool.begin`, performs
  the contract read plus one or more global/fallback candidate statements, and
  commits directly. It is not wrapped in `tx_retry`. The same module documents
  that Cockroach returns SQLSTATE 40001 for conflicting serializable
  transactions and that sqlx does not replay them; `seed`, `flush`,
  `load_session`, and `record_canonization` all use the bounded helper. This
  read became multi-statement specifically to race safely with a concurrent
  writer, so that conflict is part of its normal operating envelope.
- **Impact:** a retryable conflict reaches `candidates::gather` as a backend
  error. `Daemon::recall` intentionally degrades any gather error to an empty
  vector leg, and the CLI path has no tracing subscriber, so a compatible
  Cockroach session can silently produce lower-quality keyword/recent-only
  recall during concurrent writes. It remains safety-correct - no mismatched
  ranking escapes - but loses vector availability without using the retry
  machinery already built for precisely this database behavior.
- **Required remediation:** replay the entire contract-plus-candidate read with
  the existing bounded `tx_retry` helper, including both the global growth loop
  and exact fallback, and add a test seam proving a retryable first attempt is
  replayed while a real contract mismatch is returned immediately rather than
  retried as a serialization conflict. A live concurrent Cockroach test is
  desirable when a DSN is available.

## Other attack results

- The override cannot open a mixed-vector window: different dimensions always
  fail; cross-kind replacement fails while any vectors remain; same-kind,
  same-width replacement is the explicitly attested model-id alias case; and
  the ordered `SetEmbedding` barrier plus concept changes commit in one store
  transaction.
- Global ANN growth, boundary-tie fallback, and exact session fallback all run
  after the contract check in the same Cockroach transaction. Session filtering
  and deterministic tie handling are unchanged.
- No graph lock spans an await in the changed recall/portal paths. MemoryStore's
  mutation apply remains one locked working-copy swap; SQLite's load and flush
  retain transaction-level consistency.
- Store/embedder construction remains centralized in `resolve_for_command` /
  `resolve_backends`; H1 does not construct an adapter inside a command.
- No secret, schema migration, model weight, or unrelated product behavior was
  added. The broad diff is principally the required trait-signature ripple,
  which is coherent but should be made additive and recorded as above.

## Commands and results

| Command/check | Result |
|---|---|
| `git rev-parse HEAD` | exact requested head `298af97a4049e6ff4e642ec7be54dcec0373bc39` |
| Full diff and call-site trace from `f32af2d..HEAD` | 24 files, +1460/-215; every production H1 path and remediation site inspected |
| `env -u RUST_LOG cargo test --all-features h1_ -- --nocapture` | pass: 6 library H1 tests, 1 binary parser test, and the real subprocess lease test |
| `env -u RUST_LOG cargo test --no-default-features --features store-sqlite,embed-fixture h1_` | pass: 4 library H1 tests, parser test, and subprocess lease test |
| `env -u RUST_LOG cargo test --all-features` | pass: library 825 passed / 8 live ignored; all binary, integration, and doc harnesses passed; 2 live calibration tests ignored |
| `cargo test --all-features store::cockroach::tests::` | pass: 23 non-live Cockroach unit tests |
| `cargo check --all-targets --no-default-features --features store-cockroach,embed-fixture` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass |
| `cargo fmt --all -- --check`; `git diff --check f32af2d..HEAD` | pass |
| Built-binary help sweep over all 12 subcommands | override present on all 6 writers and absent on all 6 non-writers |

No live CockroachDB run was possible because `LAMBO_COCKROACH_DSN` was not
available. The live conformance legs remained explicitly ignored; they are not
reported as passed.

## Verdict

**REQUEST_CHANGES.** Every round-1 finding is closed and H1's vector-safety
property now holds across the inspected in-repo paths. Before integration, make
the checked vector read additive rather than source-breaking, record the
expanded frozen-contract ownership/handoff, and apply Cockroach's established
bounded SERIALIZABLE retry protocol to the new multi-statement read. Re-review
should compile an old-signature external adapter unchanged and exercise a
retryable candidate-read attempt in addition to re-running the H1 matrices.
