# #22 PR 6: recall by image or by query vector (decisions)

Base: main `63df918` (PR 1 #62, PR 2 #67, PR 3 #71, PR 4, #74, #32 PR 8).
Design of record: `lambo-handoff-2026-10-08/design/22-DESIGN.md` section 1
(the non-goal note that deferred recall by image), section 7 (recall
behaviour; 7.2, image queries are not cached), the PR 6 row of section 12
and recommendation 22; the Dresscode write-up
`22-YOUCAM-EG2-BACKEND.md` (Option A, "the client embeds everything and
sends vectors", needs recall by vector). PR 5 (the EmbeddingGemma 2
embedder) runs in parallel and is untouched here: nothing in `src/embed/`
or the embedder config changed.

## What changed

| piece | where |
|---|---|
| `QueryBy` (`Image` / `Vector`), `resolve` | `src/recall/query_vector.rs` |
| `Daemon::recall_by_vector_with` (structural dispatch off) | `src/daemon/mod.rs` |
| `Memory::recall_by`, `recall_by_detailed` | `src/memory/reads.rs` |
| `lambo_recall`'s `image`, `query_vector`, optional `query` | `src/mcp/server/{params.rs,tools/recall.rs,server.rs}` |
| `check_submitted_vector_as` (names the field) | `src/surface/image.rs` |
| `lambo recall --image [--mime] \| --query-vector-json` | `src/cli/recall.rs`, `src/main.rs`; shared read and parse in `src/cli/derive_image.rs` |
| the `lambo_recall` golden | `src/mcp/server/tests/tool_schemas.golden.json` |
| Dresscode fixtures, `GraphRanked` | `src/test_util/{dresscode.rs,vector_searchable.rs}` |

## Decisions

**The text is optional only beside an image or a vector.** Without
either, `query` is required exactly as before (the same `require_nonempty`
refusal; a missing field is now that refusal rather than serde's "missing
field"). Beside either, blank text is no text. On the wire this is
`#[serde(default)]`, so the schema drops `query` from `required` and adds
`"default": ""`; the description says when it is required. Recommended by
the orchestrator and consistent with design 7: the image is the query, the
words are an optional extra.

**What the legs do with no text.** The keyword leg searches the empty
token set and finds nothing. **The recent leg is skipped** (orchestrator
decision after review M2; `candidates::RecentLeg::Skip`, chosen by the
daemon's `Route::ByVector` when the text is blank). The vector leg
searches by the image's or vector's embedding. With text beside the
image, all three legs run exactly as in a text recall: the keyword leg
reads the words, the recent leg joins at `RECENT_SCORE`, max-merge
applies.

Why skip it. The recent leg gives the last three interactions' members a
flat `RECENT_SCORE = 0.35`, calibrated against BGE-M3 *text* cosines
(lowest true hit 0.399). With no text it carries no relevance to the
image at all, and design 7.3's evidence is that image and cross-modal
cosines can sit below 0.35 (the EG2 cosines; PR 5's live evidence shows
fresh relevant images scoring low). So in Dresscode, right after a user
derives a few looks, any true match below 0.35 would rank under whatever
was derived last. Pinned by `graded_similarity_ranks_by_cosine_not_recency`
(below): looks at cosines 0.8, 0.5 and 0.3 to a client vector, then two
unrelated looks derived last. Before the change the 0.3 look did not
even make the top 5 (the three recent members at 0.35 pushed it out);
after it, the three rank 0.8 > 0.5 > 0.3, each at its cosine within 1e-3,
on the holder graph, the store's checked read, SQLite (holder and
reload) and the tier.

**No structural dispatch.** `Daemon::recall_by_vector_with` runs the
blended pipeline even when the text is a structural phrasing ("what depends
on X"): T9's traversal skips the gather, and with it the vector leg, so it
would silently answer without the image. `recall_with` and its three
callers are unchanged (both go through a private `recall_routed`).

**Never cached.** The #14 query-embedding cache is not consulted or
filled: it holds query-role text vectors keyed by the text, and design 7.2
rules out keeping a digest of a user's image across requests. The
pipeline recall cache already never serves or stores a vector-dependent
result (P1-2); `Memory::recall_by_detailed` goes further and hands the
daemon a fresh, discarded cache rather than the session's, so the session's
cache is never even locked. Pinned by
`memory::tests::recall_by::a_similar_image_and_a_client_vector_find_the_dismissed_look`
(both caches empty afterwards, on both holder sources).

**An image embed failure fails the recall, and so does a failed vector
read.** A text recall whose embed fails degrades to keyword + recent with
a warning. A recall by image does not: the caller asked what is near this
image, and a keyword-only answer would answer a different question. The
same holds for the store's vector read (review M1): `recall_routed` takes
a `Route`, and `Route::ByVector` makes the vector leg required, so a
backend error, a timeout, a tier whose durable fallback also failed, or
an embedding-contract race is returned as `LamboError::Store` (MCP: the
bare class `store error`) instead of an answer from the other legs, which
with no text was just the recent leg. Pinned by
`a_failed_vector_read_fails_a_recall_by_image_or_vector`. The text
route's own silent drop of the leg now says so (`vector_degraded`
annotation and the same line in `warnings`,
`a_failed_vector_read_on_a_text_recall_says_the_leg_was_skipped`). The same classes as an image derive:
`EmbedUnavailable` (timeout, unreachable), `Embed` (refused, or an unusable
vector), `Config` (no image modality). A store without `VECTOR_SEARCH` is a
`Config` error in the core and a named `configuration error` on the wire,
checked before decoding anything; the CLI refuses it before any I/O.

**Images embed with no prompt.** `Embedder::embed_image`, as an image
derive does (design 3.2's `lambo-eg2-v1` profile prefixes text only), so a
query image's vector and a stored image's are the same function of the
pixels. A client vector is checked with the core's
`check_supplied_values` (exact contract, width, finite, non-zero) and
normalized, as an image derive's is.

**The wire checks are PR 4's, in the same order.** One-of first (`send at
most one of image or query_vector`), the text, the knobs, then: the store's
vector search; for `image` the embedder's `IMAGE` modality, the base64 cap
before decoding (`MAX_IMAGE_B64_LEN`, stdio has no transport cap), then
`surface::image::validate`; for `query_vector` the
`accept_client_vectors` opt-in (refusal names the key and the env var),
then `check_submitted_vector_as("query_vector", ..)`, which names the
differing contract fields and the live contract and never the declared
strings or a component. Precondition refusals are `config_refusal`s naming
Lambo's own settings. `WireImage` is reused (its schema is pinned with the
image tool's); the vector is a new `WireQueryVector` with recall wording,
reusing `WireEmbeddingContract`. Both redact in `Debug`.

**Ledger.** The recall line is the text recall's (`recall_facts`) plus
`by: "image" | "vector"` on a recall by either; `query` is the (possibly
empty) text. No base64, bytes or component
(`mcp::server::tests::recall_image::the_ledger_carries_only_the_payload_kind`).

**The schema changes for `lambo_recall` only, deliberately.** Two optional
properties (`image`, `query_vector`), `$defs` for `WireImage`,
`WireQueryVector` and `WireEmbeddingContract`, and `query` loses
`required`. The golden's `lambo_recall` entry was replaced with the
published schema and nothing else in the file changed (the file was
re-serialized with the same formatting and diffed: only that entry moved).
The other seven schemas are byte-identical. The fields are published on
every deployment, text-only ones included, so every session of one serve
lists the same `lambo_recall` (#32); a deployment that cannot serve one
refuses it by name, the PR 4 "fallback" shape, because a conditional
schema for an existing tool would make tool lists differ between
deployments for a field most callers never send.

**CLI.** `lambo recall` stays a lease-free reader. `--image` and
`--query-vector-json` are read through `take(cap + 1)` under
derive-image's caps (2 MiB, 1 MiB), sharing `read_capped` and
`VectorFile::parse` (a malformed file is reported by line and column
only). `--query` becomes `required_unless_present_any`. `run` and
`run_detailed`, and so `/api/recall`, are unchanged. The reader's vector
leg is the store's checked read, as for text.

**Every vector source, no store change.** The holder's graph (#8,
`VectorCandidates::Graph`), the store's checked read (SQLite's scan,
Postgres and Cockroach's `vector_candidates_checked`) and #18's tier all
take the query vector as they take a text query's. Tested on the first
two (memory store, both sources), SQLite end to end (holder, reload, CLI
reader), and a holder over `TieredStore` with the fake index (the kNN is
asked). Postgres is the same checked read with no new SQL; its live row
(`postgres-live`) is CI-only and was not run here.

## Fixtures

The fixture embeds a labelled PNG as exactly its label's text vector, and
labels are case-folded, so `similar_query_png()` (the label "Red Silk
Saree": other bytes, another digest) is the fixture's "photo of a similar
outfit" for the look dismissed for Onam (`red silk saree`).
`client_query_vector()` is that vector scaled by 2.5, so the server's
renormalization runs. `GraphRanked` is `VectorSearchable` declaring an
exact scan, so an MCP test's holder ranks in its graph without a flush
(`VectorSearchable`'s own checked read answers empty).

## Tests added

| test | runs in |
|---|---|
| `recall::query_vector::tests::*` (4) | rows with `embed-fixture` |
| `memory::tests::recall_by::*` (3) | `store-memory` + `embed-fixture` |
| `mcp::server::tests::recall_image::*` (8) | same |
| `mcp::server::tests::schemas::the_recall_schema_publishes_the_image_and_vector_caps` | same |
| `cli::recall::by_tests::the_flags_files_and_opt_in_are_checked_before_any_recall` | same |
| `store::sqlite::tests::image_e2e::recall_by_image_and_by_vector_find_the_dismissed_look` | `store-sqlite` + `embed-fixture` |
| `store::tiered::tests::a_recall_by_image_or_vector_over_the_tier_reads_the_index` | `recall-elastic` + `store-memory` + `embed-fixture` |
| `tests::recall_query_is_optional_only_beside_an_image_or_a_vector` (bin) | every row |

Changed tests: the golden, the F18 property set and the maxima table cover
the new fields; `unknown_fields_are_refused_by_every_params_struct` refuses
a `path`/`url` key in both; `every_session_of_one_serve_lists_the_same_tools`
checks `lambo_recall` publishes both in every deployment. No CI-grepped name
changed.

## Not in this PR

- Caching image queries (design 7.2: if ever, namespaced and per session).
- Per-modality score calibration (design 7.3, PR 5's evidence).
- Per-credential client vectors (#32 PR 5).
- `[embedder] accept_client_vectors`'s rustdoc in `src/embed/mod.rs` and
  its comment in `lambo.example.toml` still name only the derive paths;
  they sit in the embedder config PR 5 is editing, so they are left for
  that merge (the reference docs already name the recall paths).
