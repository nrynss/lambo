# Adversarial Review - Hardening H1, round 3

- **Reviewer:** `h1_review_r3` (fresh independent reviewer; source read-only)
- **Date:** 2026-08-17
- **Scope:** round-2 remediation commit
  `7cd81943398cd4c7c8249e8605560c62074bb6a4` and disposition commit
  `de3b4b7aa862597a929dcfccf4e1f19c13d06790`, against base
  `f32af2d38380a9e01cf7bd31439467ed383543ec`
- **Worktree:** `/Users/narayan/Documents/work/lambo/worktrees/hardening-h1`
- **Verdict:** **CLEAN / APPROVE** - 0 P0, 0 P1, 0 P2, 0 P3 findings

Round 2's additive-API and transaction-retry findings are closed, and all four
round-1 findings remain closed. The released three-argument adapter method is
source-compatible, the new checked default fails closed for a legacy
vector-capable adapter without recursing into the unchecked method, every
in-repo production vector lookup uses the checked method, and Cockroach binds
the exact nullable contract comparison to both candidate branches in one
retried serializable transaction. No path that can return a mismatched vector
ranking was found.

## Prior-finding closure

### H1-R2-1 (P2) - closed

`GraphStore::vector_candidates(session, embedding, limit)` has its exact
v0.2.0 required signature. `vector_candidates_checked` is an additive method
with a provided implementation, so an old adapter implementing only the
three-argument method still compiles and its old callers remain valid.

The checked default examines capabilities before any delegation. An adapter
advertising `VECTOR_SEARCH` receives `StoreError::Capability` and its unchecked
method is never called; this is fail-closed and has no default recursion or
bypass. A non-vector adapter delegates to its original method, preserving the
adapter's established capability refusal. The shipped Memory and SQLite
adapters follow that non-vector path. Cockroach overrides the checked method;
its legacy method loads the current stored contract and then enters the checked
transaction, so even direct compatibility calls cannot race into an unchecked
ranking.

The complete call-site trace has only two production entry points:
`recall::candidates::gather` and `graph::hybrid::derive`. Both call
`vector_candidates_checked`. Remaining checked calls are adapter/test
forwarders or conformance tests; remaining unchecked calls are the frozen
adapter implementations/forwarders and the explicit source-compatibility
regression. The recall and hybrid spies panic if production reaches the old
method.

The H1 `Owns` record now names every changed implementation, adapter, wrapper,
CLI/MCP/daemon path, test, and review record. The additive trait extension,
Memory/SQLite behavior, and handoff are consistent with the frozen-contract
rule in `dev-diary/README.md`.

### H1-R2-2 (P3) - closed

Cockroach's checked method validates bounds and probe width before pool work,
then wraps the whole operation in `tx_retry`: transaction begin, session
contract read, global ANN growth loop, boundary/crowd-out exact-session
fallback, and commit. Every retry starts with a fresh transaction and rereads
the durable contract. Statement, parse, and commit errors drop the transaction;
sqlx rollback-on-drop prevents a failed attempt from retaining transaction
state or a connection indefinitely.

The stored contract is decoded from `embedding_kind`, nullable
`embedding_model`, and `embedding_dim`; exact `EmbeddingContract` equality is
enforced through `ensure_compatible`, including `Some`/`None` model changes.
A mismatch is mapped to `StoreError::Invariant`, which `tx_retryable` rejects,
so it returns on the first attempt. Backend failures use the repository's
existing bounded five-attempt retry policy. Both the global candidate query and
the exact session fallback execute only after the contract check and before the
same transaction commits.

The focused retry regression proves the helper replays a retryable first
attempt and does not replay an invariant mismatch. It is a non-live seam rather
than an injected database transaction; source inspection confirms the checked
method actually encloses the complete transaction body in that helper, and the
full Cockroach unit suite pins the global/fallback decision logic. No live DSN
was available, so no claim is made about a live 40001 reproduction.

## Round-1 regression sweep

- **Lease cleanup remains closed.** Every ordinary error after writer lease
  acquisition, including load failure, mismatch refusal, and rejected
  override, passes through holder-scoped release. The shipped-binary SQLite
  test uses distinct processes and proves both a correct-model retry and an
  explicit override retry acquire immediately rather than waiting 45 seconds.
- **Checked-read race remains closed.** Reader recall carries the contract that
  produced the query embedding into the candidate read. Hybrid writer matching
  does the same and revalidates the graph contract before commit. The
  deterministic A-to-B race regression returns an error and no planted hit.
- **Live portal state remains closed.** `/api/session` and `/api/pulse` reload
  durable compatibility; browser polling reconciles the warning in both
  directions and removes stale banners without duplication. Structural routes
  stay available, while recall uses the fail-closed reader path.
- **CLI scoping remains closed.** The dangerous override is present on the six
  writer variants only, extracted before the single resolved-backend
  construction, and absent from reader parsing/help.

MemoryStore still applies an entire mutation batch to working copies and swaps
only after success. SQLite keeps `SetEmbedding`, legacy-vector quarantine, and
concept writes in one ordered SQL transaction. Cockroach keeps the corresponding
barrier and writes in one retried transaction. Same-width same-kind relabeling
with extant vectors remains an explicit operator attestation; width changes and
cross-kind relabeling with vectors are rejected atomically. The documented
`model: None` server-default blind spot is inherent in the frozen persisted
contract and is not broadened by H1.

## Commands and results

| Command/check | Result |
|---|---|
| `git rev-parse HEAD` | exact requested head `de3b4b7aa862597a929dcfccf4e1f19c13d06790` before adding this review record |
| Full diff and call-site trace from `f32af2d..HEAD` | 25 files, +2127/-277; every changed production path, adapter, wrapper, test, task handoff, and prior disposition inspected |
| `cargo test --all-features legacy_vector_adapter_compiles_unchanged_and_checked_default_fails_closed -- --nocapture` | pass; old-only vector adapter and caller compile unchanged, checked default returns `Capability` |
| `cargo test --all-features checked_vector_transaction_retries_backend_but_not_contract_mismatch -- --nocapture` | pass; retryable attempt runs twice, mismatch once |
| `env -u RUST_LOG cargo test --all-features h1_ -- --nocapture` | pass: 6 library H1 tests, 1 parser test, 1 real subprocess lease test |
| `env -u RUST_LOG cargo test --no-default-features --features store-sqlite,embed-fixture h1_ -- --nocapture` | pass: 4 library H1 tests, 1 parser test, 1 real subprocess lease test |
| `env -u RUST_LOG cargo test --all-features store::cockroach::tests::` | pass: 24 non-live Cockroach unit tests |
| `env -u RUST_LOG cargo test --all-features` | pass: library 827 passed / 8 live ignored; every binary, integration, and doc harness passed; 2 calibration tests ignored |
| `cargo check --all-targets --no-default-features --features store-cockroach,embed-fixture` | pass |
| `cargo check --all-targets --no-default-features --features store-sqlite,embed-fixture` | pass |
| `cargo clippy --all-targets --all-features -- -D warnings` | pass |
| `cargo fmt --all -- --check`; `git diff --check f32af2d..HEAD` | pass |

`LAMBO_COCKROACH_DSN` was unset. The eight live Cockroach legs remained
explicitly ignored and are not reported as passed. This is residual test
coverage risk, not an open source finding: the transaction boundary, retry
classification, contract parsing/comparison, global/fallback paths, and cleanup
were inspected directly and their non-live suites pass.

## Verdict

**CLEAN / APPROVE.** H1 is ready for integration. No remediation round is
required after this review.

## Closeout verification - 2026-08-17

- **Closeout reviewed:**
  `9712333334071120b122c9e18c8957036ba9ae99`
- **Verification verdict:** **CLEAN / APPROVE** - no material false claim and
  no review-index correction required

The H1 `DONE / CLEAN` completion record preserves the original problem,
historical handoffs, claim history, ownership expansion, and accepted
limitations. Its commit chain exactly matches the linear worktree history:

1. implementation `1a3accf9d1349dde7ce01cc41538fa755275436c`;
2. round-1 `REQUEST_CHANGES` review
   `ce0c441de4172f158a4cb3719631ae96a2703dcf` with 2 P1 / 1 P2 / 1 P3;
3. remediation `c72acf5fb1abd0f909d8bc2ef15f6d579df0d2fd` and disposition
   `298af97a4049e6ff4e642ec7be54dcec0373bc39`;
4. round-2 `REQUEST_CHANGES` review
   `14c2d52f7ac3cb734778ec13576b5ad658b0e002` with 1 P2 / 1 P3;
5. remediation `7cd81943398cd4c7c8249e8605560c62074bb6a4` and disposition
   `de3b4b7aa862597a929dcfccf4e1f19c13d06790`;
6. round-3 `CLEAN / APPROVE` review
   `c57395eda2ce1864e4cc1542e729691ffcf27abe` with zero findings;
7. docs-only closeout `9712333334071120b122c9e18c8957036ba9ae99`.

The completion record's test names, counts, feature matrices, and results match
the contemporaneous Round-3 record above. It also retains all accepted
residuals without upgrading them into false guarantees: the dangerous
same-kind/same-width relabel remains an operator attestation; the frozen
unchecked API remains externally callable while in-repo production uses the
checked path; `model: None` cannot detect a changed unnamed server default;
and no live Cockroach or SQLSTATE 40001 reproduction is claimed because
`LAMBO_COCKROACH_DSN` was unset. The separately ignored two calibration tests
remain distinguished from the eight ignored live Cockroach library tests.

`dev-diary/adversarial-review/README.md` satisfies its stated count discipline:
the header says 56 records and the table has exactly 56 record rows. All 56
Markdown targets resolve to files. The three H1 rows accurately report the two
closed `REQUEST_CHANGES` rounds, their remediation/disposition SHAs and
severity counts, followed by the dated zero-finding `CLEAN / APPROVE` round.

**Closeout remains CLEAN / APPROVE.** The task document and review index are
accurate for integration; no production or index file was changed by this
verification.
