# Dependency-question anchor remediation (0.6.0)

Updated 2026-10-12. Issue [#138](https://github.com/nrynss/lambo/issues/138). Existing implementation commits 9fbb0e62 and b632f661
remain in history. Main 422ed101 (#137) was fetched and merged without a
CHANGELOG conflict. No public API, schema, dependency or manifest changed.

## Decisions and reasons

- H1: choose the surviving occurrence nearest after a structural marker.
  Type, length, content, canonical key and node id break ties. Choosing the
  longest disjoint name confused the deleted SG with its parent VPC. Refusing
  every multi-name question would discard useful unambiguous object syntax.
- M1: sort spans by start ascending/end descending and sweep the furthest
  end. Equal spans remain eligible. Queries over 16 KiB or any candidate
  over 256 bounded spans refuse all anchoring, preserving complete evidence
  instead of truncating a candidate and selecting the wrong subject. The
  stop-word check follows the span search. The span helper stops after the
  first overflow span (257), so repetitive queries cannot collect all spans.
- M2: underscores and hyphens between word characters join identifiers.
  `db` cannot name `user_db`; `SG-Base` cannot name `SG-Base-VPC`, even if the
  full identifier is absent from the graph. Standalone hyphens delimit prose.
- M3: function-word concepts require an exact bounded uppercase occurrence
  when they are uppercase acronyms. The exception uses only those uppercase
  occurrences, not earlier lowercase question scaffolding. Added that, with,
  from, by, at, how, who, when, will, i and we. The earlier claim that ordinary
  recall always finds IT was wrong: canonical tokenization omits it too.
- M4: private, serde-skipped DetailedRecall anchor metadata is captured under
  the same graph guard as traversal/assembly and compared at merge admission.
  A different layer anchor cannot lift its traversal dependents past the
  floor; its rejected traversal headline is removed too. The normalized
  caller identity is resolved even when it has no dependents. When the
  caller has no anchor, layer-only traversal remains valid under #33. A
  separately reconstructed identity was rejected because concurrent derives
  could change the winning anchor between reads; metadata follows the actual
  traversal snapshot and removes extra graph reads.
- L1: NFC on query/content followed by per-character lowercase and NFC;
  combining marks count as word characters. This avoids contextual sigma
  lowercasing and matches composed/decomposed accents. Existing Unicode
  crates suffice; no dependency change.
- L2: require an alphanumeric base character; punctuation-only text cannot
  anchor, including underscore-only names even though underscores and combining
  marks act as word continuations at boundaries.
- L3: marker-overlapping clauses remain eligible but cannot contain/suppress
  other subjects; wholly marker-contained matches are ignored. This keeps a
  real `blast radius report` query possible while preserving `db` in a
  `safe to delete db` question with a longer matching clause.
- Fresh adversarial review found one marker-overlap/subject-position interaction:
  `blast radius report` was assigned artificial maximum distance, so a later
  disjoint `db` won despite the report being the first subject after `depends
  on`. Preserve each clause occurrence's actual position/distance; exclude
  clauses only from containment suppression. Extended the marker-overlap
  regression to assert the report's traversal answer, not just its identity.
- L4: both MCP docs mirrors and CHANGELOG now explain the per-edge boundary
  rule: `.env` matches in `config/.env`; `src/main.rs` has word edges.
- L5: CHANGELOG names the #134 floor-bypass consequence explicitly.

The initial anchor helper declared Dependency edges from anchor to consumer,
which stopped its traversal assertions passing after #135 changed the meaning
of dependents. Fixed separately to declare consumer -> anchor; this is test
fixture repair, not a relaxation of the production relation.

## Regressions and validation

Ten new tests cover separate SG/VPC subjects with four markers and a
pre-marker name; underscore/internal-hyphen identifiers; eight uppercase
acronyms and eleven added scaffolding words; NFC in both directions, combining
marks and sigma; punctuation-only names; marker-overlapping clauses; immutable traversal subject metadata after a later graph mutation; a 16 KiB
repetitive query with 30 names plus oversized refusal; merge-level wrong and
matching layer identities; and real Memory layered recall where the caller
holds `user db schema` without dependents and the layer holds `db`.

The repetitive-span regression has a one-second ceiling in a debug test and
asserts the helper's 257-span bound. Recall tests: 173 passed/0 failed/0 ignored;
Memory layered tests: 9 passed/0 failed/0 ignored. The final prescribed lane gate passed on reviewed source 4727d48f:

- `cargo fmt --check`.
- `cargo clippy --all-targets --features store-sqlite,fixtures -- -D warnings`.
- `cargo clippy --all-targets --features ship,fixtures -- -D warnings`.
- `cargo clippy --all-targets --no-default-features --features store-postgres -- -D warnings`.
- `cargo clippy --all-targets --features store-postgres,store-sqlite,fixtures -- -D warnings`.
- `cargo clippy --all-targets --features embed-eg2 -- -D warnings`.
- `cargo clippy --all-targets --features recall-elastic -- -D warnings`.
- `cargo clippy --all-targets --features embed-candle -- -D warnings`.
- `cargo test --features store-sqlite,fixtures`: 2,189 passed, 0 failed,
  7 ignored across unit, integration and doc tests. The unit block has
  2,109 passed and 5 ignored (2,114 total); the initial focused run's unit
  inventory was 2,104 including filtered tests, so ten tests were added.
  No test was removed or newly ignored.
- `cargo test --no-run --no-default-features --features store-sqlite`.
- `cargo check --no-default-features`.
- `bash scripts/docs/check-mirror-drift.sh`.

Every cargo invocation had its own temporary XDG_RUNTIME_DIR. Live database
rows were not run; the prescribed lane gate does not require them.
Private logs live in the handoff scratch/A directory, never the repository.
No fixture or golden was updated. No live database was used.

## Follow-ups

Fresh adversarial review closed with 0 open findings on b5a74757 after
one new Medium marker-clause position finding was fixed. Focused verification
reported READY on 4727d48f, with 0 High/Medium/Low open findings and a passing
no-default check. The only change after the adversarial review was restoration
of the existing named-anchor trace event. The orchestrator opened #138 and
handles PR creation, push and merge. No release or rig
change belongs to this branch.
