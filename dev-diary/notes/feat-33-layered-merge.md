# #33 PR 2: layered recall in the library

**Superseded admission/defaults in #157:** ordinary layer hits now require
absolute vector cosine >= 0.50; keyword matches do not bypass the floor.
The rank weight stays 0.5 and compatible anchored dependency answers still
pass. See [the measured tuning](feat-157-layer-tuning.md). The initial
implementation history below records the older 0.40 / keyword-bypass design.

Design of record: the #33 layered-recall design (posted on issue 33),
sections 1.1, 2 and 5, PR 2 row of section 10. PR 1 (#133) supplied
`format::fit_blocks`, `ContextBlock.scope` and `DetailedRecall::mark_context`.
Library only: no serve config, no wire. Serve (`layers`, `[[serve.layer]]`,
`MAX_LAYERS = 1`) is PR 3.

## What changed

- `src/recall/layered.rs` (new): `merge(&[TierList], MergeParams, token_fn)`
  over N ranked lists, the public result types `LayeredRecall`,
  `LayeredHit`, `LayerCopy` (all `#[non_exhaustive]`), and
  `DEFAULT_LAYER_WEIGHT` (0.5) / `DEFAULT_LAYER_MIN_SIMILARITY` (0.40),
  documented as uncalibrated until #87.
- `src/memory/layered.rs` (new): `LayerRef` (`#[non_exhaustive]`,
  `LayerRef::new(&memory)`, `with_weight`, `with_min_similarity`) and
  `Memory::recall_layered(&[LayerRef], RecallQuery)`.
- `src/memory/reads.rs`: `recall_detailed` split into `embed_recall_query`
  and `recall_text_with` (the pipeline with a supplied vector, noting no
  accesses). `recall_detailed` composes them exactly as before.
- `src/daemon/mod.rs`: `Route::Layer` and `Daemon::recall_layer_with`.
- `src/recall/detail.rs`: `AnnotationKind::LayerUnavailable` (pub(crate),
  `layer_unavailable` on the wire once PR 3 projects it; the ledger's
  response-kind list already names it).
- `RecallResult`, `RecallHit`, `CacheKey` unchanged. `format.rs` untouched
  (lane B rebase).

## Decisions (revised after the adversarial review)

- **The floor is absolute.** A layer hit merges only with keyword evidence,
  vector cosine at or above the layer's floor, or as a dependent in the
  layer's own traversal answer (anchored evidence). Forcing (Conflict, Hot,
  Reservation; design 2.2) only lifts the `top_k` cut (review H1: a busy
  `general`'s hot or reserved concepts at cosine 0.1 crowded the caller).
  The set is wider than assembly's: assembly forces hot-list payloads
  (Conflict and the Hot kinds) and reservations only in cold mode
  (`assemble.rs` 255-284); the first draft's "matches assembly" was wrong.
- **Layers answer dependency questions** (orchestrator decision on M1).
  `Route::Layer` dispatches to traversal when the query's anchor resolves
  in the layer (`resolve_anchor` gates relevance); the merge admits those
  dependents as anchored evidence (the tier's `traversal` response
  annotation). Why: after the #33 split rulings live in `general` and
  cross-session edges are dropped, so otherwise "what depends on X" for an
  X in `general` has no answer anywhere. Traversal stays inside one graph.
  After the merged cut, each session's headline is recomputed from its own
  dependents the merged context renders (T9-R1-N4), via
  `dispatch::traversal_headline`. No recent leg, never cached.
- **Rank is the position in the session's own list, before the floor.**
- **Merge walk.** Candidates are walked in merged order (key desc, tier
  asc, position asc). A candidate that is the same memory as an emitted
  group joins it: equal canonical key (hash map), or holder-vector cosine
  at or above the threshold against **every** member (transitive, L1),
  and a group takes at most one copy per tier. The group shows its
  nearest tier's copy at its best copy's position. A copy cut by `top_k`
  before its group exists is remembered by key and joins later (so a
  forced farther copy still shows as the nearer one); by key only.
  Vector comparisons are capped at `MAX_VECTOR_DEDUPE_PAIRS` (65,536)
  per merge (L7); past it only keys dedupe.
- **Meta text**: `from <session>`, `also <short id> in <session>`;
  `ContextBlock::for_hit`'s label extended by `layered::block_with_meta`.
- **Warnings**: caller's lines unchanged; layer lines and response
  annotations prefixed `[<session>] `; a hit's lines leave `warnings` only
  when no copy of it reached the answer. Unavailable: one
  `layer_unavailable` annotation and one line per layer.
- **Unavailable** means closed, contract mismatch, or a panic in that
  layer's recall (caught per layer in `join_all`, L3). A failed vector read
  is not unavailability: the layer degrades to its other legs with a
  prefixed warning.
- **Embed once**: by the caller. When the caller's store cannot search but
  a compatible layer's can, the caller still embeds once for the layers,
  its own side runs with no vector (so its recall cache serves, as its
  plain recall would), and an embed failure goes to the layers that lost
  their vector leg, prefixed (L4).
- **Weights**: finite and positive; 0 is refused at the library boundary
  (L5). Floors in `[0, 1]`.
- **Contract check is unconditional.** L6 (note only): it is
  `EmbeddingContract` equality, so `model: None` and `Some("")` differ and
  such a layer is unavailable; PR 3's start-time contract check should
  normalise or report it.
- **Order of checks**: the stack is checked first, so a bad stack is a
  Config error even with a `top_k` the pipeline refuses (M3); then no
  layers or a refused `top_k` return the plain recall, byte-identical.
- **Legs**: `detail.legs` keeps the caller's session only (keyed by node
  id, unique within a session); `LayeredRecall.hit_legs` holds legs per
  merged hit by position, because the split may replicate identity nodes
  with one UUID across sessions (L2).
- **Identities** are copied 64 hits per graph read, so a large `top_k`
  does not hold one lock across a long vector copy (L7).
- **Send**: a compile-time check that `recall_layered`'s future is `Send`
  (L8).
- **`layer_unavailable` on the wire (L9)**: it reaches no wire in PR 2.
  The portal (`web/app.js`) renders annotation kinds generically
  (`a.kind.replace(/_/g, " ")`, CSS class `a-<kind>`), the ledger writes
  its own string, and the only Rust deserializer of `DetailedRecall` is the
  same binary's portal parity test, so no consumer rejects the new value.
- Concurrency without a `futures` dependency: `tokio::join!` for the caller
  and a `poll_fn` join over the layer futures.

## For PR 3

- `LayeredRecall.detail` (pub(crate)) is the merged `DetailedRecall`; the
  per-hit session is `LayeredRecall.hits[i].session`. The CLI/portal
  re-render (`render_detailed_block`) does not know `from <session>`; PR 3
  must render layered payloads from the merged context or teach it the meta.
- Validation (self-layer, duplicate, weight, floor range) is also at the
  library boundary; PR 3's config checks will run first.

## Tests

`recall::layered::tests`: 2- and 3-list interleave; weights; floor (below,
at, keyword, expansion-only, custom); forced members past `top_k` but never
the floor; a layer's traversal answer admitted as anchored evidence with
its headline recomputed after the cut, and the caller's own headline;
dedupe by key at the better rank, with a forced cut copy shown as the
nearer one; 3-tier precedence by vector; transitive dedupe and one copy
per tier; the vector budget falling back to keys; per-hit legs with a node
id shared across sessions; one budget and flag reset; truncated layer
first block with `(scope general)`; degradation; prefixes; determinism and
tie-breaks; `block_with_meta` identity; `layer_unavailable` serde.
`memory::layered::tests`: a panicking future is that future's error only.
`memory::tests::layered`: unlayered byte identity field by field
(`DetailedRecall` equality, both flags, per-hit legs, access counts) at
three budgets including a truncation; a refused `top_k` (2049) is the plain
answer and a bad stack is still refused; the query embedded once with two
layers (counting embedder); a caller without vector search embeds once for
its layer; a real two-session merge (no layer recent leg, per-session
accesses); a layer answering a dependency question; closed and
incompatible layers degrade; refused stacks (weight 0 included).
