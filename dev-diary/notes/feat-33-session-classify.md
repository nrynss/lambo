# #33 D1: read-only session classification

The classifier plans a split without loading an embedder, acquiring a writer
lease, initializing schema or modifying source data. The CLI dispatches through
one explicit source resolver. SQLite uses read-only immutable connections and
refuses missing files, links and nonempty WALs; callers must supply a copy of a
completed backup. Snapshots work even without a store feature. A new private
output file avoids overwriting source/config/label aliases.

The pure classifier uses configurable folders, actor globs and bounded literal
keyword hints. Fixed high-confidence neighbours provide a noncascading second
pass; actor, content and neighbours must agree. Generic unknown records retain
the fallback session. Automatic general requires independent ruling, prompt
and operator actor evidence and excludes references. Shared Resource and Entity identity hubs
carry placement hints; D1 writes no target sessions.

The plan conserves every source interaction and concept exactly once. Full
durable source fingerprints include vectors, contracts, metadata, canonization
history and durable write intents; volatile reservations are excluded. Durable
row collections and object keys are canonicalized, while ordered arrays remain
ordered. Rules and the versioned plan have separate hashes. The seal detects
corruption, not forgery or operator authorization. D2 must validate source and
plan before writing.

Reviewed legacy v2 JSONL labels are accepted only as explicit operator assertions
with complete exact source coverage. Their canonical hash documents the import;
it does not establish their original source provenance. Separate JSONL overrides
carry reasons and optional expected prior assignments. Original private labels,
ids and store content remain outside the repo.

## Review remediation

An adversarial review identified that imported origin labels could erase excluded
project provenance. Protection now captures source actors and repository-path evidence before
imports or overrides, including a concept's recorded origin actor. Both concept
and interaction general placements refuse protected provenance. Reviewed general
interactions require a nonempty set of all-general origin concepts. Regression
coverage exercises mismatched origin actors, relabeled protected prompts, audited
protected text and interaction-only general imports.

The approved design section 8 covers Resource and Entity identities broadly;
path-only automatic inference was an implementation restriction and has been
removed. Synthetic nonpath identities now receive incident-session placements.
Inference uses one fixed assignment set so hub-to-hub inference cannot cascade.
Explicit complete reviewed legacy placements remain authoritative for reproducing
the historical report, rather than being replaced with inferred placements.

Final plan validation now rechecks the general interaction invariant after
corrections, so moving a child out of general cannot leave its prompt in the
shared layer. It also rejects a resealed direct plan with that inconsistency.

SQLite previously omitted persisted session creation/close times on load.
The loader now reads both; normal writable schema initialization converges old
partial session tables against the already-authoritative timestamp columns.
Classification remains read-only and never calls that initialization. Regression
oracles compare SQLite timestamps with their exact persisted SQL values, while
still deep-comparing every graph field with the Memory/Graph oracle. A seeded
metadata roundtrip demonstrates that a changed close time invalidates the plan.

## Validation and follow-ups

Synthetic fixture coverage checks deterministic plans, strict general guards,
noncascading event-time smoothing, complete reviewed imports, audited overrides,
hub hints, source/plan tampering, ordered-array fingerprints and immutable input
handling. Full lane gate results are recorded in the final readiness section.

On the prepared backup copy, schema preflight succeeds and the source hash stays
unchanged. Complete reviewed labels need explicit audited treatment for historical
general assignments whose phrases/references differ from the generic heuristic.
The operator authorized an external, provenance-recorded conversion of ten reviewed
assertions plus the two original corrections. Import then identified five historical replication entries whose source types
are not identities and whose contents are narrative records, not bare paths.
The root resolved the conflict: the newer, explicitly authoritative historical
report takes precedence over section 8's no-ruling-copy restriction for those
already-reviewed placements. A generic `shared_hub_assertion` on a distinct
audited correction, with exact repeated placements and reason, records that
operator decision. Complete reviewed import provenance is required, all general
placements are refused, and no source type/content/status changes. The validator
requires this evidence even after resealing. Automatic inference and unaudited
imports remain Resource/Entity-only. Synthetic narrative Constraint coverage
checks accepted audited, refused unaudited/general and missing-audit direct plans.
Five separate assertions preserve the reviewed replication, making seventeen
audits in total (ten general assertions, two original corrections, five hubs). The private report retains counts and original input hashes; source ids
and contents remain outside this note.

D2 performs copying and exact edge accounting; D3 compares layered recall. Neither
a live rig run nor owner cut-over belongs to D1. A later run on the prepared backup
copy can import the owner-reviewed labels; automatic classification alone is not
a promise to reproduce those reviewed placements.

The audited legacy import exposed a product-name mention in a global privacy
rule. The root distinguished mention from provenance: excluded source session
attribution, matching source actors and configured repository folder evidence
are immutable barriers. A literal product keyword alone remains soft evidence
that blocks automatic general classification but may be addressed by an explicit
audited operator assertion. This applies to all configured projects, with no
owner-specific exception. Synthetic neutral-actor privacy, excluded actor/folder
and declared excluded source-session regressions exercise that distinction.

## Final readiness review

The final delta review found that narrowing protected provenance to hard
evidence had also removed soft keyword evidence from plan validation. A resealed
automatic plan, or an unaudited reviewed label, could place a ruling whose origin
prompt or sibling concept mentioned a protected project into general. Validation
now requires an explicit audited general assertion when the origin prompt or a
sibling concept mentions a project excluded from general. A keyword in the ruling
itself already fails the conservative guards. A synthetic regression covers
resealed, unaudited reviewed and audited cases.

A focused follow-up review found that the first version of this check counted
keywords of every project. The classifier guards only excluded projects, so a
prompt naming a non-excluded project produced an automatic general placement that
validation then refused, failing the whole run. The validator now uses the
classifier's scope. Regressions on the automatic, resealed and reviewed paths
show that a non-excluded mention passes and an excluded mention still needs an
audit.

The same review found that a failed write could leave a partial plan behind. The
plan is now written to a private create-new sibling, synced and hard-linked
into place. Linking refuses any existing entry, so the output stays create-new
and never replaces an input, and the temporary name is always removed. The
summary line reports that the source was opened read-only. It no longer claims
an unverified "source unchanged". The full lane gate passed after these fixes,
with no failing rows.
