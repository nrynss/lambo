# #22 PR 1: image input and query role on the Embedder trait (decisions)

Base: main `db709d3`. Design of record:
`lambo-handoff-2026-10-08/design/22-DESIGN.md` (approved 2026-10-09, all 22
recommendations), sections 2, 3, 10 and 12. This is the first of five PRs. It
adds the trait surface and the image validation rule, and switches the one
recall call site. It adds no storage, no MCP or CLI surface and no EmbeddingGemma 2
adapter.

## What changed

| piece | where |
|---|---|
| `Modalities`, `ImageMime`, `ImageInput`, `EmbedError::Unsupported`, the default methods `embed_query` / `modalities` / `embed_image` | `src/embed/mod.rs` |
| tests of the defaults | `src/embed/trait_tests.rs` |
| `surface::image::validate`, `MAX_IMAGE_BYTES`, `MAX_IMAGE_SIDE_PX` | `src/surface/image.rs`, tests in `src/surface/image/tests.rs` |
| recall's query embed now calls `embed_query` | `src/recall/candidates.rs` (`embed_query`) |
| query-cache docs: entries are query-role vectors only | `src/recall/query_cache.rs` |
| `TextRole` helper; 14 delegating test embedders forward every method | `src/test_util.rs` and the test files listed in the commit |

## Decisions

**Additive default methods, not an `EmbedInput` enum** (design §2.1, owner
decision 1). Every adapter in the tree (BGE-M3, candle, Gemini, fixture, and
#21's bearer change) compiles without an edit. The one non-additive line is the
recall call site.

**The query role is a trait method** (owner decision 2). Recall is the only
caller of `embed_query`. Derive, `record_action`, re-embed, the write-queue
probe and calibration, and replay liveness keep `embed`, the document role.
For every shipped adapter the two are the same call, so recall answers exactly
as before. The tests prove the call site with an asymmetric embedder, not with
the fixture, which cannot tell the roles apart.

**The #14 cache key needs no change.** The cache is filled only from
`candidates::embed_query`, so `(query text, contract)` names exactly what was
embedded: that text, in the query role, under that contract. An adapter with a
query prompt puts its prompt profile in the contract's `model` (design §3.2), so
a profile change misses the cache. The module docs now forbid inserting a
document-role vector. `recall_caches_the_query_role_vector` pins this on both
vector sources.

**Image validation takes raw bytes.** Base64 decoding and the encoded-length
cap belong to PR 4, with its `base64` dependency. This PR changes no
`Cargo.toml` or `Cargo.lock`: `bitflags` and `sha2` were already direct
dependencies.

**The header parser is hand-written and reads headers only:**

- PNG: the first chunk must be a complete 13-byte `IHDR`.
- JPEG: the parser walks the marker segments to the first `SOF0`, `SOF1` or
  `SOF2`. It skips fill bytes, `TEM` and `RST`. It refuses any other frame type,
  and a scan or end-of-image before the frame header.
- WebP: the first chunk after RIFF must be `VP8 `, `VP8L` or `VP8X`. A WebP
  whose first chunk is anything else is refused.
- WebP `VP8X` (review M1): the `VP8X` header declares only a canvas, while the
  pixels sit in a later `VP8 `/`VP8L` chunk (up to 16383 px a side) or in
  animation frames. So the animation flag is refused, an `ANMF` chunk before
  the image chunk is refused, and the parser walks the RIFF chunks (bounded by
  the bytes supplied) to the first `VP8 `/`VP8L` chunk, whose dimensions must
  equal the canvas. A `VP8X` with no image chunk is unreadable. This keeps the
  promise that the checked dimensions bound the decode true for every format.
  Animation is refused because an embedding needs one still image and the
  design never asked for it.

Validation is header-only (review L2): a file with a valid header and no or
corrupt image data passes, so an adapter that decodes maps a decode failure to
`EmbedError::Backend`. The `ImageInput` docs say so.

The JPEG walk accepts any run of `0xFF` fill bytes before a marker, as the
JPEG spec allows, and refuses any other byte where a marker is expected
(review L4). libjpeg instead warns about "extraneous bytes" and resyncs, so a
file from a buggy encoder that libjpeg decodes can be refused here as
"truncated or unreadable"; that is deliberate, and recognisable if a PR 4
user reports it.

Anything the parser cannot read is refused as "truncated or unreadable".
`every_truncation_of_a_header_is_refused` checks every prefix of each header,
so none panics or reads out of bounds.

Lossless and arithmetic-coded JPEG (`SOF3`, `SOF5` to `SOF15`) is refused on
purpose, following the design's SOF0/1/2 rule. Few cameras or browsers emit
them, and an embedding backend may not decode them.

**JPEG height 0 is refused.** It means the height comes later, in a DNL
segment, so the header does not bound the decode. The 1..=4096 rule refuses
it as a zero side.

**No refusal quotes the payload or the declared MIME string.** The declared
type is client text of unbounded length, so a message names the rule and at
most the *sniffed* format. `ImageInput`'s `Debug` prints only the length and
the format.

**`ImageInput` has a crate-private constructor**
(`ImageInput::from_validated`), and `surface::image` is a public module. A
library consumer gets an `ImageInput` only through `validate`, so an adapter
can rely on its guarantees. `ImageMime::from_mime` is exact and
case-sensitive, so `image/jpg` is refused.

**Wrapper trap (design risk R6).** Each delegating test embedder now keeps its
behaviour in one `embed_as(text, role)` method and reaches its inner embedder
through `test_util::TextRole`. Its `embed` and `embed_query` therefore count,
gate and refuse the same way, while the inner adapter sees the role the caller
asked for. `modalities` and `embed_image` forward straight to the inner
embedder, so image calls are not counted or gated: no test sends an image yet,
and PR 3 decides whether its image tests need that. Fakes that wrap nothing
keep the defaults, which are correct for them.

## Not in this PR

- Storage of `embedding_source`: PR 2.
- The supplied-vector derive, `derive_image_as`, the fixture image embed and
  the acceptance recall: PR 3.
- `lambo_derive_image`, the CLI verb, base64 and the stats fields: PR 4.
- The EmbeddingGemma 2 HTTP adapter and sidecar: PR 5.
