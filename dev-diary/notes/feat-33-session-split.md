# #33 D2: offline session split

`lambo session split` consumes a D1 `ClassificationPlan` and writes its target
sessions into a new offline SQLite file. It never touches a live writer and
performs no cut-over. D3 (layered recall comparison) is separate.

## Shape

- **One construction site.** `resolve::resolve_split_io` resolves the source
  exactly as classification does (immutable SQLite copy or snapshot) and the
  destination as an explicit file path. URIs, `:memory:`, DSNs and other store
  kinds are refused with a generic "import unsupported" error; no
  `GraphStore` trait method was added, so no wrapper or tier can delegate an
  import. `SplitDestination::open` sets no journal mode, so opening an existing
  file never rewrites its header.
- **Re-validation by reproduction.** `validate_plan` checks seals and
  coverage; the CLI then reruns D1 with `--rules`, the reviewed labels (required
  exactly when the plan imported some) and the plan's audited overrides, and
  requires the identical plan. A resealed edit fails here.
- **Pure build.** `session::build_split` returns every target snapshot, the
  edge ledger, placements (id maps), accounting, digests and the low-confidence
  inventory. It validates the source and every target with
  `Graph::from_snapshot` and checks global id uniqueness before any
  destination is opened. Output does not depend on source row order.

## Transform rules

- Concept placements: the assigned session, or the hub's `copy_into`.
  Interaction placements: the assigned session plus each session holding a
  concept that originated there (one replica per `(interaction, target)`).
- Ids: a v8 UUID from a domain-separated SHA-256 over the algorithm version,
  source and plan digests, row kind, target and original id. New chain links
  also bind the older interaction.
- Ledger: one category per source edge from **primary** assignments, in order
  intra, cross-session Temporal, hub reattached, origin reattached, dropped.
- Output: non-Temporal edges that are not dropped go into every target holding
  both ends (so hub-hub edges multiply per shared placement). Temporal chains
  are the original chain's subsequence; adjacent pairs keep the original edge's
  metadata under a new id, new adjacencies get weight 1, zero reinforcements and
  the newer interaction's times. Since an intra Temporal edge links consecutive
  interactions, it always survives in its own target.
- Metadata: `root_goal` and `closed_at` cleared; `created_at`, mutation epoch
  and GC watermark inherited; reset flag false; reservations dropped and
  counted.
- Consumed receipts go to their interaction's primary target in stored
  (settled) form; Action pins are remapped or the split is refused.

## Refusals (before any write)

Pending intents (with receipts listed), nonlocal consumed pins, synonyms that
resolve to no concept, ambiguously or cyclically, canonization events or
intents naming no source row, a missing session `created_at`, sub-millisecond
timestamps or counters outside SQLite's range (they could not verify after a
round trip), and any invalid source or target graph. A general replica with
hard excluded-project provenance is refused; D1 already prevents it, and D2
re-derives the barrier from the source.

## Destination and transactions

- New output: an empty `create_new` private sibling gets the store DDL plus
  `split_imports` (kept out of `001_init.sql`, whose table list drives every
  store's preflight), is closed and then hard-linked into place.
- Existing output: read-only checks (schema, receipt table, every receipt of
  this split, no session-keyed row without a receipt, no lease row). No DDL.
- Per target, one `BEGIN IMMEDIATE` transaction: ownership checks; with a
  receipt, compare the stored target with the expected one (canonical order)
  and roll back, asserting SQLite's `total_changes()` did not move; without
  one, check id collisions, insert session, interactions (chain order),
  concepts, edges, synonyms, events, intents and the receipt, re-read on the
  same transaction, compare, and commit once. Failure anywhere rolls back that
  target only; earlier commits remain and a rerun resumes.
- The report is published create-new and private after the imports; if that
  fails, the error says the imports are committed and a rerun verifies them.

## Tests

`src/session/split_tests.rs` (synthetic alpha/beta): exact ledger and output
accounting, chain subsequence with an out-of-order event time and a
created-at tie, hub replication with vector bit equality and hub-hub
multiplicity, one shared origin replica for two concepts, metadata and receipt
handling, determinism under source reordering, refusals (pending intents,
nonlocal pins, synonyms, timestamps, key collisions, orphan events, stale or
tampered plans), surfaced general replicas, and SQLite import round trip,
byte-identical zero-write rerun, rollback at three fault points with resume,
and refusal of drifted, foreign and non-split outputs. `src/cli/session.rs`
covers dry run, import and rerun, aliasing (same path, hardlink, symlink,
symlinked directory, sidecar, existing report), non-reproducing plans and an
unchanged SQLite source with no sidecars created.

## Private acceptance run

On a private copy of the prepared backup, with the reviewed labels and the
audited corrections, the dry run and import reproduce the authoritative
figures: concepts 5,005 to 5,036 (27 hubs, 58 placements) and source edges
11,755 / 334 / 46 / 30 / 30. A rerun wrote 0 rows and left the output
byte-identical; the source copy's hash was unchanged.

Interactions go from 1,786 to 1,829, not 1,809. The report's 23 extra rows count
origins of assigned concepts only; hub copies need 20 more, because most hub
copies have no Derives edge from a local interaction and would otherwise be
invalid or need a rewritten `origin_interaction`. The summary prints both
parts. The general target receives seven origin replicas; four are assigned to
a project excluded from general, as origins of owner-audited general rulings.
They follow design section 8 (provenance is copied) and are listed in the
report and a summary banner for owner review before cut-over, together with the
66 low-confidence items.
