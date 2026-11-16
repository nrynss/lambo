# #146: durable concept names and existing-memory repair

Implementation contract for the owner's 2026-11-13 request to implement
[#146](https://github.com/nrynss/lambo/issues/146) fully, including migration,
repair and tests across the store adapters. The earlier assessment and
upgrade discussion are part of the accepted scope. This amends the S5
restriction for agent-facing names; the frozen hackathon spec is not edited.

## Identity and persistence

A durable name is a normalized, session-local key bound permanently to a
`NodeId`. `ConceptName` retains its display spelling as well as its normalized
key. Registration and lookup use the existing canonical-key normalizer, so
case, Unicode composition, token order and stemming do not create different
names. UUID-shaped and eight-hex names are refused to avoid a second meaning
for the reference forms agents already use. A normalized name or concept key
cannot be taken from another identity. Identical redeclaration is a no-op.

The normalized name registry is separate from the raw spelling-based legacy
synonym map. Raw low-level `Graph::declare_synonym` input is an import format,
not a durable write-reference namespace. This preserves the S2 legacy format
while giving normal names the stronger contract. `Memory::declare_synonym`
registers a durable name against an existing concept instead of adding
ephemeral writer metadata.

Names enter the graph transaction's undo log and the ordinary mutation log.
`SetConceptName` reaches MemoryStore, SQLite, Postgres and Cockroach through
the same fenced flush transaction as the concept write. Snapshots, loading,
seeding and session erasure include names. Deleting a target leaves its name
reserved; name resolution fails explicitly instead of creating or choosing
another concept under its text.

Per-concept `name` is optional on `lambo_derive`; CLI `derive --name` applies
to its first concept. Commit binds it to the identity the actual canonical or
hybrid write selected. Citations and contradictions can use durable names.
Their identity is pinned at acknowledgement and checked again at application
and restart replay. Named writes use distinct durable-intent variants, so an
older decoder cannot silently replay them without their naming semantics.

Exact-name recall contributes direct identity evidence even without content
keyword overlap. Dependency questions can name the declared subject. Rendered
and detailed recall hits expose up to three durable names in stable order.
Old pending Action/ActionCited intents retain their id-only pin semantics;
their ordinary text slots are re-resolved at apply, while ActionNamed pins
ordinary text, including a previously missing target.

## Existing memory

Existing concepts, IDs, vectors, correct citations and provenance remain in
place. Surviving legacy synonym rows can be normalized and bound to the
existing targets. A chain, missing target, ambiguous target or conflicting
normalized name refuses the entire upgrade. Names already lost from RAM with
no surviving evidence cannot be inferred safely; attach them to known IDs
explicitly with `names assign`.

A reviewed `names assign` also resolves a missing, chained or conflicting
legacy spelling before re-running migration. Pre-existing durable bindings
take precedence over those old rows. Bindings created during the current
import do not receive that exemption, so conflicting legacy rows still refuse
atomically rather than silently choosing the first one.

Historical aliases need a reviewed map of full `alias-id target-id` pairs.
Detection and similarity may suggest a pair but do not authorize a repair.
The supported `names migrate --map` operation reuses the alias citation plan:
historical Dependency citations move to the intended target with their
original timestamps, duplicate support is removed, and cycle-producing moves
refuse the operation. Self mappings, chains and cycles among pairs refuse too.

Retirement is more than moving citation edges. The original Entity and its
incident history are stored as `RetiredAlias`, outside active concepts,
before its active node is removed and its durable name is declared. The
archived old ID remains inspectable. Its previous promotion status is history,
not active recall or canonization evidence, and its old canonical key no longer
blocks the intended name. Full-ID writes do not silently follow a retired ID.
Only reviewed name-artifact Entities are eligible. Substantive relationships
that cannot be retired safely require an explicit disposition first; the
upgrade does not move arbitrary edges or reset global canonization state.

## Operator path

The configured store is resolved once, with no embedder construction. All
compiled store adapters use the same CLI path:

1. Back up the store and let accepted writes settle. Stop its writer before
   provisioning or applying changes.
2. Run the new binary's `lambo provision` to install the additional tables.
3. Preview `names migrate --session S [--map reviewed.map]`, or
   `names assign --session S --name N --target UUID`.
4. Review the report's exact bindings, retired IDs and targets, archived edge
   counts and previous promotion states. Note its `source_epoch`.
5. Apply the same operation with `--apply --expect-epoch E`. It reloads under
   the single-writer lease, refuses pending intents and a changed epoch, and
   writes the complete graph transaction in one fenced flush.
6. Restart and verify ID and name lookup, retired history and citation
   behavior. Repeat the migration to verify it is a no-op.

Preview takes no lease and does not mutate the store. A live writer blocks
apply. A changed source or an invalid reviewed line never produces a partial
repair. Interruption before the flush leaves no graph mutation durable;
interruption after commit leaves the complete transaction durable. Repeating
the map is idempotent even after its aliases have left the active graph.

Older binaries ignore the added tables, so downgrade after names or repair is
unsupported. Drain named intents before changing versions. Restore the
pre-upgrade backup to return to the old behavior; no command silently discards
names or reuses old IDs to simulate compatibility.

Session splitting refuses snapshots that contain durable names or retired
alias history; it never drops that metadata or guesses how to remap fixed
identities into new sessions. Upgrade already split sessions individually.

## Verification surface

The implementation's acceptance checks include graph rollback and collision
rules; async identity pinning, durable intents and restart replay; populated
snapshots and mutation persistence across all adapters; whole-batch refusal
and fencing; old-ID retirement; CLI preview/apply, stale epochs, writer
refusal, idempotence and restart; and the reference/site documentation mirror
gate. Private memory is not committed into fixtures or used as a CI input.

The new binary and repair tools are implementation deliverables. Re-pinning a
running writer and applying a reviewed live-memory repair are separate
operator actions; this work does not rewrite the running dogfood graph.

## Validation (2026-11-15)

`cargo test --features store-sqlite,store-postgres,store-cockroach,fixtures
--all-targets` passed: 3,171 tests across 36 targets, zero failures, 51
tests ignored by default (live name checks were run separately below). The
library accounts for 3,083 passes. Clippy
with the same features and `--all-targets -- -D warnings`, formatting, diff
whitespace and reference/site mirror checks passed.
Each adapter also passed a separate warning-free binary build with default
features disabled (`cargo clippy --no-default-features --features store-KIND
--bins -- -D warnings`, for memory, sqlite, postgres and cockroach).

The new `store::names::tests` conformance ran on MemoryStore, SQLite and
isolated live PostgreSQL and CockroachDB servers. The live cases are
normally ignored; set only isolated test DSNs in `LAMBO_POSTGRES_DSN` and
`LAMBO_COCKROACH_DSN` and run that filter with `--include-ignored`.
CockroachDB v25.2.4 requires `feature.vector_index.enabled = true` before
provisioning. The tests exercise the real populated-graph upgrade, fenced
flush, reload, immutable bindings, preserved citation and canonization
history, whole-batch rollback and erasure of both new tables.

The full run exposed an existing #221 test race: its first daemon cycle
could precede completion of the eight-concept fixture. It now waits for
score coverage of all eight concepts before checking blended ranking;
the ranking assertions and production scoring are unchanged.
