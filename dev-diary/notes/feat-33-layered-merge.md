# #33 PR 2: layered recall in the library

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

## Decisions

- **`Route::Layer` does not dispatch structural queries.** A dispatched
  traversal answer has no phase-1 leg scores, so the per-layer floor would
  drop all of it. The blend always runs in a layer, with traversal
  expansion inside the layer's own graph (edges never cross sessions).
  The caller's own session keeps `Route::Text` and still dispatches. Also:
  no recent leg, never cached.
- **Rank is the position in the session's own list, before the floor.** A
  layer whose top hits fail its floor does not get its later hits promoted.
- **Dedupe keeps the nearer copy at the better key**, and forces it if either
  copy was forced, so a memory is never lost because the nearer copy ranked
  below `top_k`. Copies in one list are never compared.
- **Forced = Conflict, Hot or Reservation** (design 2.2; matches assembly,
  whose hot list covers conflict and the other hot kinds).
- **Meta text**: `from <session>` and `also <short id> in <session>`, not
  `<session>:<id>`, following PR 1's marker ruling (`resolve_focus` does not
  parse a session prefix). Inserted into `ContextBlock::for_hit`'s label by
  `layered::block_with_meta`.
- **Warnings**: the caller's session's lines unchanged; each layer's lines
  and response annotations prefixed `[<session>] `; a hit's lines leave
  `warnings` only when no copy of it reached the answer (a dedupe-dropped
  copy keeps its lines). Unavailable: one `layer_unavailable` annotation and
  one line per layer, `recall: layer X is unavailable; results are from
  <available sessions>` (`<s> only` when one remains).
- **Contract check is unconditional**: a layer whose contract differs is
  unavailable even when no query vector exists, since dedupe compares holder
  vectors across sessions.
- **Embed once**: by the caller; if the caller's store cannot vector-search
  but a compatible layer's can, the caller still embeds once for the layers.
- **Unlayered and refused calls are the plain recall**: no layers, or a
  `top_k` the pipeline refuses, return `recall_detailed` wrapped, so output
  is byte-identical.
- **Accesses (#30)**: each session notes only its own hits the merge
  returned.
- Concurrency without a `futures` dependency: `tokio::join!` for the caller
  and a small `poll_fn` join over the layer futures.

## For PR 3

- `LayeredRecall.detail` (pub(crate)) is the merged `DetailedRecall`; the
  per-hit session is `LayeredRecall.hits[i].session`. The CLI/portal
  re-render (`render_detailed_block`) does not know `from <session>`; PR 3
  must render layered payloads from the merged context or teach it the meta.
- Validation (self-layer, duplicate, weight, floor range) is also at the
  library boundary; PR 3's config checks will run first.

## Tests

`recall::layered::tests`: 2-list and 3-list interleave; weights (above 1,
zero); floor (below, at, keyword, expansion-only, custom floor); forced
members past floor and `top_k` with warning-line handling; dedupe by key at
the better rank with `also` meta and kept warning; 3-tier dedupe by vector
precedence, key-only without vectors, never within one list; one budget and
flag reset; truncated layer first block with `(scope general)`; degradation
(2 and 3 tiers, context equal to the caller's own blocks); prefixes;
determinism and tie-breaks; `block_with_meta` identity.
`memory::tests::layered`: unlayered byte identity at three budgets and for a
refused `top_k`; a real two-session merge (session names, meta, no layer
recent leg, per-session accesses); closed and incompatible layers degrade to
the plain answer; refused stacks.
