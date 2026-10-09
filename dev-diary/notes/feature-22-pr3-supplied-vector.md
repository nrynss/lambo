# #22 PR 3: derive an image concept from a supplied vector (decisions)

Base: main `40f674a` (includes PR 1 #62 and PR 2 #67). Design of record:
`lambo-handoff-2026-10-08/design/22-DESIGN.md` (approved 2026-10-09, all 22
recommendations), sections 3.3, 4, 5, 7, 9, 11 and the PR 3 row of section
12. This is the third of five PRs. It adds the core image derive: the
supplied vector through hybrid derive, the write queue, the durable intent
and replay, the `Memory` entry points, the fixture's image embed and the
`re-embed` rules. No MCP tool, CLI verb, `base64` or stats field (PR 4); no
EmbeddingGemma 2 adapter (PR 5).

## What changed

| piece | where |
|---|---|
| `SuppliedVector { content, vector, contract, source }`, `check()`, `WriteIntentPayload::DeriveImage` | `src/types/mod.rs` |
| `hybrid::derive_with(.., supplied, on_commit)`; image item skips merge; text tier drops image candidates; repair; commit-lock check | `src/graph/hybrid.rs`, tests `src/graph/hybrid/tests/supplied.rs` |
| `ImageDerive`, `ImagePayload`, id/caption/type rules, content suffix, default ids, `normalize` | `src/graph/image.rs`, tests `src/graph/image/tests.rs` |
| `Memory::derive_image_as`, `Memory::derive_image_async_as` | `src/memory/image.rs`, tests `src/memory/tests/image.rs` |
| `JobPayload::DeriveImage`, `WriteKind::DeriveImage` (`lambo_derive_image`), byte charge, `submit_derive_image`, the image receipt sentence | `src/writeq/{admission,execution,receipts}.rs` |
| `FixtureEmbedder` `TEXT | IMAGE`, `embed_image`, `image_label`, `png_with_label` | `src/embed/fixture.rs`, re-exported from `embed` and `fixtures` |
| `re-embed --drop-image-vectors`, image-aware `--missing-only`; `Graph::reembed_all_dropping_image_vectors`; `embed_missing` refuses image concepts | `src/cli/re_embed.rs`, `src/main.rs`, `src/graph/graph/embeddings.rs` |
| AC3 on SQLite, erase with an image, the image intent round trip, the tier | `src/store/sqlite/tests/{image_e2e,erase,persistence}.rs`, `src/store/tiered/tests.rs`, `src/store/erase.rs` (testkit) |

## Decisions

**The supplied vector is one more argument to `derive_with`** (design 5.1).
The public `hybrid::derive` keeps its signature and passes `None`. With a
supplied vector the call carries exactly one concept, the image item, whose
content equals `supplied.content` (checked; anything else is an
`Invariant`). The image item resolves in the gather phase with no embed and
no candidate lookup: `Fresh` with the vector and source, or `CanonicalMatch`.

**Order of the apply-time checks, and their classes.** `derive_with` runs
`SuppliedVector::check(live)` first and maps a refusal to
`LamboError::Embed`: a vector declared under another contract, of the wrong
width, non-finite or zero-norm is a fact about *this input*, so the J3
replay consumes the intent `failed` and moves on (design 3.3 point 3). Only
then the `VECTOR_SEARCH` precondition (`Config`) and the one-concept shape
(`Invariant`). The stamped-session check under the commit lock
(`existing.ensure_compatible(&supplied.contract)`) stays `Config`: a session
stamped in another space is session-wide, and a replay should stop there.
When can the live contract differ from the acked one at replay? Not by an
ordinary attach (it refuses a mismatched session), but by the
`re-embed` migration's attach (`reembed_mode`, which runs the replay) and by
`--allow-embedding-mismatch` relabels. `a_replayed_image_intent_under_a_changed_live_contract_settles_failed`
models the intent left by a process acked under `model = acked-under-v1`.

**A text item never merges into an image** (design 4.4). The plan phase,
under the read lock it already holds, collects the ids of concepts with an
`embedding_source` (only when a text item will rank), and the gather drops
those hits **before** `top_tier`. Filtering after `top_tier` would drop a
valid text member of a tied tier along with the image. With the
8-candidate limit an image can still take a slot a text candidate would
have had; that only under-merges, the safe direction. Mutation-checked: with
the filter removed, `a_text_item_never_merges_into_an_image_concept` and the
tier test both fail.

**A canonical match repairs a missing image vector.** If the matched concept
is an image concept whose vector is missing (a quarantine or
`--drop-image-vectors` nulled it and kept its source), the match writes the
supplied vector and source and counts it embedded. A match on a concept that
has a vector keeps it (first image wins, design 4.2 dedup). This makes
`--drop-image-vectors` recoverable: derive the same image again under the
new embedder.

**A canonical match on a text concept refuses the image derive** (review
M1, replacing this PR's first rule that left the text concept alone and
reported `0 embedded`). Any text write can produce an image's canonical key
first: `lambo_derive "render 17 [image:r17]"`, an action's resources, a
`parent_of` end, and case and token order fold together. Keeping the text
concept would leave the image with no vector while the call reported
success, and the image would never be found. The refusal is decided under
the commit lock (`supplied_match_repair`) as `LamboError::Embed`: it is a
fact about this input, so a replayed intent settles `failed` instead of
blocking the replay. The message tells the caller to choose another image
id. Refusing a `[image:` token in text-derive content at the surface is a
wire change, left to PR 4.

**Image ids are `[a-z0-9]{1,64}`, not `[a-z0-9_-]`** (design R8, resolved,
deviating from section 4.2). `canonical::normalize_tokens` splits on `-` and
`_`, then drops stopwords and Porter-stems each piece, so `r17-a_b` and
`r17_a-b`, or `red-shoes` and `red-shoe`, would share a canonical key and two
images would become one concept. Without them `[image:<id>]` is one token
that ends in `]`, which no stem rule or stopword touches, so the id reaches
the key whole. `the_suffix_survives_canonicalization_whole` pins it with ids
a stemmer would rewrite (`cats`, `running`, `abed`) and stopwords (`the`,
`a`). Default ids are 16 lowercase hex characters, which pass. **PR 4's MCP
schema must publish the same pattern** (`graph::image::validate_image_id`).

**Other call-path rules** (`Config`, nothing written, not even an
interaction): the `Hybrid` strategy and `VECTOR_SEARCH` (Q16); for bytes, an
embedder whose `modalities()` include `IMAGE` (`EmbedError::Unsupported` from
`embed_image` maps to `Config` too); a non-blank, `check_size`-clean caption
that does not itself contain `[image:` (Lambo builds exactly one suffix;
checked on the raw text and on its canonical tokens, so `[IMAGE:` or a
suffix split by a zero-width character is refused too, review M2: two
captions smuggling each other's id would otherwise give two images one
key); no
`Observation` type (observations never canonical-match, so the same image
would duplicate); `check_size` on the final content. The image embed runs
under `HYBRID_IO_TIMEOUT` with the text derive's classes (timeout or
transient: `EmbedUnavailable`; refusal: `Embed`), and the adapter's output is
held to width, finiteness and norm (`Embed` if not).

**`normalize` is bit-exact on a unit vector.** It renormalizes in `f64` only
when the norm is more than `1e-6` from 1, so a correct embedder's or client's
vector is stored exactly as it answered (the design called the step
idempotent; a blind divide was not, at the last bit).

**The digest is built from the validated input only** (PR 2 decision):
`EmbeddingSource.sha256 = hex(ImageInput::sha256())`, lowercase, so the
write-side check never sees anything else.

**`accept_client_vectors` is not in the core.** The core accepts a
`Vector` payload; the operator opt-in and its refusal naming the key are PR
4's surface acceptance.

**The write queue.** `JobPayload::DeriveImage` and
`WriteIntentPayload::DeriveImage` are their own variants (Q18). An image job
runs the derive arm with the vector supplied; it is refused with `Config`
under the `Canonical` strategy (a replay in a differently configured process
meets that). The byte charge is the derive's strings plus the content, four
bytes per component and the contract's strings. `WriteKind::DeriveImage`
reports `lambo_derive_image`, and the receipt reads
`derived 1 image concept(s): C created (E embedded), M matched existing`.
A `parent_of` pair whose child is the image concept resolves to it and counts
it matched, as for text.

**A settled image intent keeps no vector** (review L1). A consumed row is
retained for the receipt window and purged only lazily, by a later consume
in the same session, so an idle session kept an applied or failed image
intent, vector and all, indefinitely: an older build could not load the
session though nothing was owed (contradicting design R5, which says only an
*unconsumed* intent stops a downgrade), an old-space vector outlived
`re-embed --drop-image-vectors`, and a `failed` intent kept the vector it was
refused for. Nothing reads a settled row's payload (the replay answers its
receipt from the outcome and replays only unconsumed rows), so every adapter
now overwrites a settled `DeriveImage` payload with
`WriteIntentPayload::settled()`, the empty `Derive`
(`SETTLED_IMAGE_INTENT_PAYLOAD`): SQLite and the Postgres family in the
consume `UPDATE` itself (`payload LIKE DERIVE_IMAGE_PAYLOAD_LIKE`, so no
read and no new mutation field), and on any put of a row that already
carries an outcome (snapshot saves); the in-memory adapter on both too. The
outcome tag and summary, agent, interaction and timestamps stay for
diagnostics. Text payloads are unchanged. Purging settled rows instead would
have cost the image receipt its `applied_after_restart` answer. No stored
data needs migrating: no build that wrote image intents has shipped. The
shared check `embedding_source_testkit::check_a_settled_image_intent_keeps_no_vector`
runs on SQLite and Memory in CI, on Postgres inside the existing
`postgres_round_trips_the_embedding_source` live test (no CI change), and in
the Cockroach conformance suite; the Postgres-family SQL is not run locally.

**`re-embed` (Q19)** replaces PR 2's blanket refusal (decision `c4bf7dcd`):

- a full migration refuses while any image concept still carries a vector,
  before any embed or write, releasing the lease and naming
  `--drop-image-vectors`;
- `--drop-image-vectors` nulls those vectors in the same ordered batch as the
  rewrite (all `UpsertNode`s, then `SetEmbedding`), **keeps each source**,
  and reports the count. Keeping the source follows the L1 quarantine
  decision (`12414772`): the concept stays "image vector missing", so a later
  `--missing-only` skips it and re-deriving the image repairs it. (PR 2's
  note anticipated that a drop might clear the source; it does not.)
- `--missing-only` and the full migration skip image concepts whose vector is
  missing and report how many; `--missing-only --drop-image-vectors` is a
  usage error.

The graph enforces the same: `reembed_all` covers text concepts only,
refuses an image vector left behind and any update aimed at an image
concept; `embed_missing` refuses an image concept.

**AC3 asserts the top hit after a daemon cycle** (review L2, replacing
"the leg, not the final rank"). Design section 9 says "the image concept is
the top hit". The final score is `0.5 * daemon + 0.5 * query`, and a concept
derived moments ago is not in the daemon's score table until its next cycle,
so the rank of any fresh concept, text or image, races the daemon (one
earlier async run lost to two older noise concepts at 0.533 vs 0.5). The
check now waits for the daemon to score the current epoch
(`Memory::settle_daemon`, whose test gate widens to SQLite) and then asserts
the image is `hits[0]`, along with the leg facts it already asserted: the
image's vector leg is about 1, it has no keyword leg, and no other
candidate's best leg reaches 0.5. It holds synchronously, through the write
queue, and after a reload; SQLite's own checked scan ranks it first too.

**Open for PR 5 (with R2 / Q15):** the cold-start rank. Before the daemon's
next cycle a perfect vector match scores 0.5 and loses to unrelated, older,
daemon-scored concepts (about 0.533). That is spec section 8 behaviour for
any fresh concept, not an image defect, but it is a demo-quality question
for the ranking-parity measurement of design 7.3.

## Through #18's tier, #14's cache and #23's erase

- **Tier (M6).** A holder's hybrid derive ranks in its graph, so the image
  filter applies there unchanged. The image derive asks the index nothing,
  and the image's vector is mirrored like any concept's
  (`an_image_on_a_holder_over_the_tier_is_indexed_and_never_absorbs_text`). A
  dropped image vector is an upsert without a vector, which the projection
  already turns into an index delete.
- **Query cache (#14).** Filled only from recall's `embed_query`; an image
  derive, sync or queued, leaves it empty
  (`an_image_derive_never_touches_the_query_embedding_cache`).
- **Erase (#23).** No code change (design 4.5). The shared testkit's planted
  intent is now an unconsumed `DeriveImage`, so every adapter's census covers
  a vector-carrying intent row (counts unchanged), and
  `erase_leaves_nothing_of_a_derived_image` erases a SQLite session whose
  image concepts came from real image derives.

## Fixes made on the way

- `store/tiered/tests.rs`'s `LabelEmbedder` (#18) forwarded only `embed`,
  breaking design rule R6; it now forwards every method.
- `recall/query_cache.rs`'s public module doc linked a private function
  (a `cargo doc` warning from PR 1).

## Tests added

| test | runs in |
|---|---|
| `embed::fixture::tests::{png_with_label_is_a_valid_png_whose_label_the_fixture_reads, the_fixture_embeds_images_and_advertises_it, an_unlabelled_image_gets_a_digest_seeded_vector_far_from_text, a_lying_label_length_reads_nothing_and_never_panics}` | every row with `embed-fixture` |
| `embed::trait_tests::the_fixture_embeds_images_and_keeps_the_query_default` (replaces PR 1's `the_fixture_keeps_every_default`) | same |
| `graph::hybrid::tests::supplied::*` (11: no merge, text excludes images, dedupe, repair, a text concept holding the key refuses the image (two), AC4 at apply, stamped session, the first-writer race, preconditions and shape, `parent_of`) | same |
| `graph::image::tests::*` (8, two for the caption check on canonical tokens) | every row |
| `memory::tests::image::*` (9; the two replay tests need `fixtures`) | rows with `store-memory` + `embed-fixture` |
| `cli::re_embed::tests::{re_embed_refuses_image_vectors_unless_told_to_drop_them, re_embed_drop_image_vectors_nulls_them_and_reports_the_count, re_embed_never_gives_an_image_concept_a_caption_vector, re_embed_refuses_missing_only_with_drop_image_vectors}` (replace PR 2's `re_embed_refuses_a_session_with_an_embedding_source`) | same |
| `graph::graph::tests::embeddings::re_embed_and_backfill_never_give_an_image_concept_a_text_vector` | every row |
| `store::sqlite::tests::image_e2e::{a_text_query_recalls_an_image_concept_through_the_vector_leg, an_acknowledged_image_derive_is_recalled_through_the_vector_leg}` (AC3) | SQLite rows |
| `store::sqlite::tests::erase::erase_leaves_nothing_of_a_derived_image`, `store::sqlite::tests::persistence::an_image_derive_intent_survives_the_flush_load_round_trip` | SQLite rows |
| `store::tiered::tests::an_image_on_a_holder_over_the_tier_is_indexed_and_never_absorbs_text` | the `recall-elastic` row |

No CI-grepped test name changed. The image tests live in `image_e2e`, not
`vector_e2e`, so the grepped `store::sqlite::tests::vector` row is untouched.

## Not in this PR

- `lambo_derive_image`, `lambo derive-image`, base64, `accept_client_vectors`,
  `lambo_stats` contract fields: PR 4.
- The EmbeddingGemma 2 adapter, sidecar and ranking-parity evidence: PR 5.
