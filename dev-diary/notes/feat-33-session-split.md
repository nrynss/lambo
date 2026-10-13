# #33 D2: offline session split

`lambo session split` consumes a D1 `ClassificationPlan` and writes its target
sessions into a new offline SQLite file. It never touches a live writer and
performs no cut-over. D3 (layered recall comparison) is separate.

## Shape

- **One construction site.** `resolve::resolve_split_io` resolves the source
  exactly as classification does (immutable SQLite copy or snapshot) and the
  destination as an explicit file path, constructing the destination store
  there once (its pool is lazy, so a dry run never touches the file). URIs,
  `:memory:`, DSNs and other store kinds are refused with a generic "import
  unsupported" error; no `GraphStore` trait method was added, so no wrapper or
  tier can delegate an import. The destination sets no journal mode, so
  opening an existing file never rewrites its header.
- **Re-validation by reproduction.** `validate_plan` checks seals and
  coverage; the CLI then reruns D1 with `--rules`, the reviewed labels
  (required exactly when the plan imported some) and the overrides file
  (required exactly when the plan carries overrides), and requires the
  identical plan. The plan's own overrides never authorize themselves.
  Reproduction shows the plan follows from those files, not that the files are
  authorized.
- **Pure build.** `session::build_split` returns every target snapshot, the
  edge ledger, placements (id maps), accounting, digests and the low-confidence
  inventory. It validates the source and every target with
  `Graph::from_snapshot` and checks global id uniqueness before any
  destination is opened. Output does not depend on source row order.
  `build_validated` is the same build after `validate_plan`, separate so tests
  reach guards a valid plan cannot trigger.

## Transform rules

- Concept placements: the assigned session, or the hub's `copy_into`.
  Interaction placements: the assigned session plus each session holding a
  concept that originated there (one replica per `(interaction, target)`),
  hub copies included. Replicas cross projects, not only into `general`; the
  accounting carries a replica matrix (origin session -> target session) and
  splits replicas into those assigned concepts need and those only hub copies
  need.
- **Hard provenance never leaves its session.** An interaction with an
  excluded project's actor or repository folder (D1's never-waivable
  evidence) refuses the split if any placement would copy it elsewhere.
- **Soft replicas** go through one policy function, `replica_interaction`.
  It copies the interaction in full today. The owner is choosing between a
  full copy, a stub without the prompt, and refusal unless an interaction-level
  audit authorizes it; each is a change to that function alone.
- Ids: a v8 UUID from a domain-separated SHA-256 over the algorithm version,
  source and plan digests, row kind, target and original id. New chain links
  also bind the older interaction.
- Ledger: one category per source edge from **primary** assignments, in order
  intra, cross-session Temporal, hub reattached, Derives reattached, dropped.
  A dropped edge names its reason: `cross_session_structural` (concept to
  concept, no hub) or `derives_without_replica` (the interaction is not copied
  into the concept's session). The endpoint matrix allows only Temporal and
  Derives edges to touch an interaction, so no other edge can attach to a
  replica; non-Derives edges between a replica and a local concept cannot
  exist in a valid source.
- Output: non-Temporal edges that are not dropped go into every target holding
  both ends (so hub-hub edges multiply per shared placement). Temporal chains
  are the original chain's subsequence; adjacent pairs keep the original edge's
  metadata under a new id, new adjacencies get weight 1, zero reinforcements and
  the newer interaction's times. An intra Temporal edge links consecutive
  interactions, so it always survives in its own target.
- Metadata: `root_goal` and `closed_at` cleared; `created_at`, mutation epoch
  and GC watermark inherited; reset flag false; reservations dropped and
  counted.
- Write intents: pending ones refuse (a replay is owed). Consumed ones are
  receipts, not memory: they are counted and not migrated, so no payload text
  crosses sessions and a consumed pin to another session no longer blocks.

## Refusals (before any write)

Pending intents (receipts listed), synonyms that resolve to no concept,
ambiguously or cyclically, canonization events naming no source concept, a
missing session `created_at`, sub-millisecond timestamps or counters outside
SQLite's range (they could not verify after a round trip), non-finite vector
components or edge weights, a hard-provenance replica, and any invalid source
or target graph.

## Destination and transactions

- New output: an empty `create_new` private sibling gets the store DDL plus
  `split_imports`, is closed and then hard-linked into place. The receipt
  table stays out of `001_init.sql`, whose table list drives every store's
  preflight, and has no foreign key: a receipt is a standalone record, so
  `erase-session` and other ordinary verbs work on a split output. A rerun
  after such use refuses (lease row, or receipt without its target).
- Existing output: read-only checks (schema, receipt table, every receipt of
  this split, no session-keyed row without a receipt, no lease row). No DDL.
- Per target, one `BEGIN IMMEDIATE` transaction: ownership checks; with a
  receipt, compare the stored receipt and target with the expected ones
  (canonical order) and roll back, asserting SQLite's `total_changes()` did
  not move; without one, check id collisions, insert session, interactions
  (chain order), concepts, edges, synonyms, events and the receipt, re-read
  rows and receipt on the same transaction, compare, and commit once. Failure
  anywhere rolls back that target only; earlier commits remain and a rerun
  resumes. Concurrent runs serialize on the write lock and fail closed.
- The destination is finished with a checkpoint whose failure is reported,
  then closed.
- The report is published create-new and private after the imports; if that
  fails, the error says the imports are committed and a rerun verifies them.
- The split store methods and the receipt DDL are crate-private; the fault
  hook exists only in tests.

## Tests

`src/session/split_tests.rs` (synthetic alpha/beta): exact ledger and output
accounting with drop reasons and the replica matrix, chain subsequence with an
out-of-order event time and a created-at tie, hub replication with vector bit
equality and hub-hub multiplicity, one shared origin replica for two concepts,
metadata and dropped receipts, determinism under source reordering, refusals
(pending intents, synonyms, timestamps, key collisions, orphan events, stale or
tampered plans, non-finite vectors), the hard-provenance replica refusal (a
valid plan for a project target, a constructed plan for `general`), surfaced
general replicas, and SQLite: round trip, byte-identical zero-write rerun,
rollback at three fault points with resume, a read-back mismatch rolling back,
refusal of drifted, foreign, leased and non-split outputs, concurrent imports,
and an output used as an ordinary store (schema, preflight, lease, flush,
erase-session). `src/cli/session.rs` covers dry run, import and rerun, aliasing
(same path, hardlink, symlink, symlinked directory, sidecar, existing report),
plans that do not reproduce (rules, labels, missing or different overrides),
two concurrent CLI runs on a new output, a report that fails after the imports
commit, and an unchanged SQLite source with no sidecars created.

## Real data

A one-off run against a private copy of the prepared backup, before the
coordinator's no-real-data instruction, reproduced the authoritative concept
and edge figures and showed the 23/20 replica split and the general replicas
from an excluded project. It predates the hard-provenance replica refusal and
the dropped receipts, so it does not show whether the historical plan passes
the new guard: a hub copy whose origin has an excluded project's actor or
folder now refuses. No real-data run is planned; the owner decides when one is.
