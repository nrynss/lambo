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
project provenance. Protection now captures source actors and text evidence before
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

## Validation and follow-ups

Synthetic fixture coverage checks deterministic plans, strict general guards,
noncascading event-time smoothing, complete reviewed imports, audited overrides,
hub hints, source/plan tampering, ordered-array fingerprints and immutable input
handling. Full lane gate results are recorded after execution.

D2 performs copying and exact edge accounting; D3 compares layered recall. Neither
a live rig run nor owner cut-over belongs to D1. A later run on the prepared backup
copy can import the owner-reviewed labels; automatic classification alone is not
a promise to reproduce those reviewed placements.
