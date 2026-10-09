# #18: an Elasticsearch recall tier beside the durable store (decisions)

Base: main `b0e8207`. Decisions and why; the commits carry the mechanics.

## What changed

A top-level `[recall]` section in `lambo.toml` wraps the configured store in a
`TieredStore`. The store (`primary`) stays the source of truth and keeps every
obligation that needs a transaction: leases, fencing, canonization bookkeeping,
blast radius, interaction span, flush stats. An Elasticsearch index serves one
thing, the vector leg of phase-1 recall, through the #26 seam
(`VectorCandidateSource`).

| piece | where |
|---|---|
| `[recall]` config, fail-closed registry arm `wrap_with_recall_tier`, `RecallBackfillReport` | `src/store/recall_tier.rs` (always compiled) |
| `TieredStore`: delegation, mirroring, sync state, repair, erase, backfill | `src/store/tiered/mod.rs` |
| projection, version arithmetic, contract-keyed index naming | `src/store/tiered/project.rs` |
| the index seam `RecallIndex` | `src/store/tiered/index.rs` |
| the Elasticsearch REST client | `src/store/tiered/elastic.rs` |
| in-process fake index (tests) | `src/store/tiered/fake.rs` |
| wiring at the single construction site | `resolve_backends`, `resolve_store_only` in `src/resolve.rs` |
| `GraphStore::backfill_recall_index` (default `Ok(None)`) | `src/store/mod.rs` |
| `lambo recall-index backfill` | `src/cli/recall_index.rs`, `src/main.rs` |

Feature `recall-elastic = ["dep:reqwest"]`: the client is plain HTTP over the
`reqwest` 0.12 the embedders already carry, so no crate is added and
`Cargo.lock` does not change. The six endpoints used (`_bulk`, `_search` with
`knn`, `_delete_by_query`, index create, and document get/put/delete on the
marker index) do not justify an Elasticsearch crate.

## Decisions

**Not a store kind.** Elastic cannot make "check the lease, then bulk-write N
documents" atomic, so it cannot meet `flush`'s fencing contract, and graph
traversal is not what it is for. The tier wraps whatever `[store]` built.

**`[recall]`, not `[store.recall]`.** The issue sketched the table under
`[store]`. `StoreConfig` is a public struct with public fields built literally
at about 45 call sites, one of them in a file the governance hooks flag on
read; a field there breaks all of them and every library consumer that builds
one. A top-level `LamboFile` section breaks four test literals and reads the
same to an operator. `deny_unknown_fields` applies to it as to every other
table. A binary without the feature refuses the section by feature name.

**Secrets by reference only.** `api_key = { env = "NAME" }` is the only form;
an inline string does not parse, and a URL with `user:pass@` is refused at
construction. The key is read once when the tier is built, sent only in the
`Authorization` header (marked sensitive), and never appears in an error. The
variable name is chosen by the file, so it cannot join `RESOLVE_ENV_VARS`; a
hermetic harness simply does not configure `[recall]`.

**The vector leg goes through `VectorCandidateSource`; everything else is
delegation.** `vector_candidates_checked` is the tier's source. The unchecked
v0.2.0 `vector_candidates` stays the primary's (it cannot bind a contract, so
there is no index to choose). `capabilities()` is the primary's plus
`VECTOR_SEARCH`, and `vector_dimensions()` is the primary's or, for a primary
with no vector column, the configured embedder width, which keeps resolve's
`VECTOR_SEARCH`-implies-width check honest.

**`exact_vector_scan()` stays `false`, and a holder does not switch to its
graph for small sessions (#8).** The question the issue asks: the graph is
exact, fresh and about 3 ms at 3,600 concepts, so should a holder use it below
some size? No, for three reasons. A size threshold flips the ranking between an
exact scan and HNSW as a session grows, so the same query over the same data
would rank differently on either side of the boundary, which is the kind of
nondeterminism the recall goldens and H1 exist to keep out. The tier is opt-in:
an operator whose sessions fit an exact scan should not configure it, and gets
#8's graph source on SQLite by not doing so. And readers (`lambo recall`,
`serve-web`) have no graph and need the index regardless. The cost is
freshness on the vector leg (unflushed concepts, plus up to one refresh
interval after a flush); the keyword and recent legs are in RAM and still find
those concepts. If dogfooding shows the gap matters, the follow-up is an
explicit `[recall]` knob that names the threshold, not an implicit one.

The same choice reaches hybrid derive, because `VectorCandidates::for_holder`
is one rule for both callers (#8). On a holder over `TieredStore(sqlite)`,
derive's semantic match asks the index instead of the graph, so a concept
derived in the last flush interval plus one refresh is not a semantic-merge
candidate, exactly as on the pg family today. Exact canonical-key matches are
unaffected (they are resolved in the graph). Splitting the rule so derive
keeps the graph while recall uses the index is possible but reopens #8's
"one constructor, two callers" decision, so it is left as an open question
rather than done here.

**Index per contract.** `{prefix}-v-{hash}`, where the hash is the first 8
bytes of SHA-256 over `kind`, `model` and `dim` with separators that keep
`model = None` and `model = ""` apart. A checked read compares the expected
contract with the session's durable contract and then queries only the
expected contract's index. The SQL adapters do both in one snapshot; here they
are two reads. If the durable contract changes between them, the answer is
still in the expected embedding space: stale, never meaningless, the property
H1 protects. Accepted divergence, like `created_at` parity.

The durable contract is cached per session from the last load or flush through
the tier (`SetEmbedding` updates it), so a read costs no durable round trip.
A read that arrives before any load through this store loads the session once.

Indices are created lazily, on the first mirror for a contract, with an
explicit mapping (`dense_vector`, `dims = contract.dim`, `similarity = cosine`,
`index = true`). The issue said "at provision, never at attach". Provision
cannot know the contract without building the embedder (provision is a
store-only verb), and a session's contract only exists once it stamps one. The
attach-path rule exists so an unprovisioned durable store refuses before it
acks writes; the index is not durable, an explicit mapping on first write is
equivalent to one made earlier, and creation is idempotent. `init_schema`
(that is, `lambo provision` on SQLite and Postgres) creates the marker index.

**Index documents.** `session_id`, `node_id`, `canonical_key` (the issue-2
tie-break, returned with every hit so equal scores order like the other
sources), `content` (for a later hybrid leg), `concept_type`, `created_at`,
`embedding`, and `v`, the external version the document was written at.
Canonization state is **not** mirrored, and `CanonizationTransition` is not
projected: an `UpsertNode` carries a concept snapshot that can be stale on the
canonization columns (R2-1), and Elasticsearch's partial-update API cannot take
an external version, so a mirrored status could not be kept ordered. Nothing on
the vector leg reads it. Correction to the issue's table: `SetEmbedding` is the
session's contract, not a node's vector; it switches the index later writes in
the batch go to.

**Versioning.** `version = (fencing_token << 32) | flush_counter`, the
counter kept **per session** and restarted at 1 for each new token (#32: tokens
are per session; the store holds no "current session"). Index and delete
operations both go through `_bulk` with `version_type=external`, so a replayed
or late write older than what the index holds is refused as a conflict, which
counts as success. Within a batch only the last operation per node id is
written, because the batch shares one version and an equal version is refused.
A counter that would wrap, or a token too large to shift into a positive i64,
refuses the mirror and marks the session stale. Unleased writes (`token =
None`, seed and fixture paths) use the engine's own versioning: a counter that
restarted with the process would otherwise sit below versions it wrote earlier,
and those writes would be silently refused.

Delete tombstones keep their version for `index.gc_deletes` (60 s by default).
A late write from a fenced-out holder older than that could resurrect a deleted
document; a fenced-out holder cannot reach the mirror (its primary flush
fails first), so the window needs a crash mid-mirror plus a minute of delay,
and the next repair removes the document. Accepted.

**When the index is trusted.** Per session, `Unknown`, `InSync` or `Stale`.
Only `InSync` serves from the index. Otherwise the read falls back to the
primary's own checked read (exact on SQLite, the database's ranking on the pg
family), or to an empty vector leg when the primary has no vector search
(`MemoryStore`), so recall degrades to its keyword and recent legs. A failed
query on an in-sync session falls back for that read only.

`InSync` is proven, not assumed. The index keeps a **sync marker** per session
in `{prefix}-meta`: the durable `mutation_epoch` it reflects (persisted by the
primary in the flush transaction since #17). A clean mirror writes it at the
batch's version; a load compares it with the snapshot's epoch. A crash between
the primary's commit and the mirror, or a failed mirror followed by a restart,
leaves the marker behind, and the next load sees it. No durable schema change,
and nothing to keep in sync by hand.

A delete-only batch (a GC sweep) names no session. It is attributed through the
lease this store holds under the batch's token; when that is ambiguous (two
sessions held under equal token values, or an unleased write) the deletes go by
id across every data index and the marker is left where it is, so the owning
session repairs at its next load. Never wrong, occasionally redundant.

**Who repairs.** Only a process holding the session's lease writes to the
index: at load (`Memory` acquires the lease before it loads), on a flush while
the session is stale (at most once a minute while the index stays down, since a
repair reads the whole durable session), and `lambo recall-index backfill`,
which takes the lease itself and is refused while a live writer holds it.
Readers never write: a reader that finds the marker behind serves from the
primary. A repair indexes every stored vector at a fresh version, then deletes
every session document written at a lower version (deleted nodes, an older
contract's index), then writes the marker. Indexing first means the index is
never emptier than the durable state mid-repair, and a takeover during a long
backfill is safe: the new holder's versions outrank the backfill's.

The issue asked for the mirror failure count and last error "in flush stats".
`SessionFlushStats` is a durable two-field row shared by every store, and #11 is
reworking the writer side; adding fields there is a schema change for a
diagnostic. The tier keeps the count and last error per session in process and
logs every failure on `lambo::recall_tier` at warn.

**Erase (#23).** `erase_session` runs the durable erase first. Only when it
committed (`Erased`) does the tier delete the session's documents from every
data index and its marker, and if that fails it returns an error naming the
rerun: the deletion fan-out must not mark the account done while its vectors
are searchable. A rerun is idempotent: the durable erase reports
`already_absent` and the index cleanup is retried. `Held` touches nothing. An
erased session cannot be written again (the tombstone fences every token), so
nothing re-mirrors it.

**Shared indices across sessions (#32).** One index per contract with
`session_id` as a keyword filter, not one per session: per-session indices
would multiply shards with the session count, and every query and delete is
already session-filtered.

## Tests

No Elasticsearch runs locally and none was started. `src/store/tiered/tests.rs`
drives `TieredStore` over `MemoryStore` (and SQLite under `store-sqlite`) and an
in-process fake that applies the engine's external-version rules and exact
kNN, with switchable faults. `src/store/tiered/elastic.rs` tests the wire
format against `httpmock` (an existing dev-dependency): NDJSON bulk bodies at
external versions, conflicts and absent deletes as success, a rejected item as
failure, the kNN body and the `(1 + cos) / 2` score mapping, a missing index as
an empty answer, versioned markers, path-encoded session ids, delete-by-query
failure reporting, and the API key in the header only.

Covered from the issue's list: parity with the exact scan (SQLite primary,
within 1e-6), fencing unchanged with nothing mirrored, replay and late writes
across a takeover, contract mismatch and a mid-session switch, mirror failure
then repair, a crash between commit and mirror caught at the next load, a
deleted node's hit never reaching `lambo recall` (top_k 1 still filled by the
live concept), erase reaching the index and being retried. The issue's
Docker-gated integration suite against a real cluster is not written: no
Elasticsearch is available here and starting one was out of bounds. Recall
latency against the graph source, which the issue wants in the PR, needs a real
cluster and is not measured.
