# #130 and #33 PR 1: one budget cut, and a long top hit is truncated

Issue #130: a recall rendered an empty `context` whenever the top-ranked block
alone exceeded `max_tokens`, although `hits` held the answer. At the default
500 tokens (`ceil(bytes / 3.5)`) that is any block over about 1,750 bytes; the
lambo-paper evaluation saw 6 of 12 renders come back empty. Design of record:
#33 design section 2.5 and PR 1 in section 10. Revised after the lane D
adversarial review (findings M1-M5, L6-L13).

## What changed

- `src/recall/format/fit.rs` holds the one budget cut,
  `format::fit_blocks(&[ContextBlock], max_tokens, token_fn) -> FittedContext`.
  It replaces the two copies of the loop (`assemble.rs`, `dispatch.rs`). The
  #33 layered merge (PR 2) calls it a third time over the merged list.
- `ContextBlock { text, content_len, label_end, inspect_id, scope }`, built
  by `ContextBlock::for_hit(hit, warning_lines)` so the offsets match
  `render_block`. `scope` is `None` for the caller's own session; a layer hit
  (PR 2) sets it and the marker then reads `lambo_inspect <id> (scope
  <session>)`, matching design 2.3's `scope` parameter. The earlier
  `<session>:<id>` idea was dropped: `resolve_focus` does not parse it
  (review M3).
- `FittedContext { context, whole, truncation: Option<Truncation> }`.
- `DetailedRecall.truncation` is the single source of truth (review M2).
  `DetailedRecall::mark_context` is the only writer of the per-hit
  `included_in_context` / `truncated_in_context` flags, which are its wire
  projection; `truncated_hit()` is the only reader the renderers use.

## The rule

1. Longest ranked prefix of whole blocks, measured on the actual joined text
   (separators included). A count of `usize::MAX` never fits (the existing
   `checked_add` guard). Stop at the first block that does not fit; never
   skip ahead (GPT5.6sol P1-4).
2. Only when no whole block fits, the first block is truncated **inside its
   content** (review M1): `<content head>…<rest>` then the line
   `[truncated: <n> tokens left out; lambo_inspect <id> for the full concept]`.
   The first form that fits wins:
   - `WithWarnings`: rest = label suffix + warning lines;
   - `LabelOnly`: rest = label suffix (type, canonical marker, score, id,
     blast radius); warning lines go to `warnings` / the CLI header only;
   - `ContentOnly`: no rest; its shortest form is the marker alone.
   `<n>` is `token_fn` of exactly the block text left out (review L7).
   Nothing follows a truncated block.
3. If the marker alone does not fit, the context is `""`, as before.
   `max_tokens` 0 and 1 stay empty; with an 8-char id the marker is about 22
   tokens.

Option 1 of the issue, chosen over option 2 (always include the top block)
because `max_tokens` stays a hard ceiling and the acceptance test
`max_tokens_zero_yields_empty_context_but_full_hits` keeps its meaning.

The cut point is a binary search over content char boundaries that do not
follow whitespace, so the head never ends in whitespace and is never trimmed
(review L6: trimming made the search non-monotone). Every candidate is
measured as the exact string returned, so the result fits for any estimator;
for an estimator monotone in bytes (the default) it is the longest such head,
checked against a linear scan in `the_head_is_the_longest_that_fits`.

Cuts are on char boundaries, not grapheme clusters: a head may end inside a
ZWJ emoji sequence or before a combining mark (review L8). Cosmetic, valid
UTF-8, and not worth a segmentation dependency.

## Surfaces

- `DetailedHit.truncated_in_context`: serialized only when true, so every
  H3 golden and every recall whose first block fits is byte-identical. The
  truncated hit keeps `included_in_context: false`.
- `render_cli_text` (CLI and portal text) uses the pipeline's `context` for
  the truncated block, and prints the hit's annotations in the header unless
  the cut kept the warning lines, decided by the `Truncation` form, never by
  searching the text (review L10).
- The H3 re-render parity (portal payload re-rendered through
  `render_cli_text` equals `context`) cannot hold for a truncated payload:
  the cut depends on the budget and estimator, which the payload does not
  carry. `context` is the authority there; `recall_endpoint_flags_a_truncated_top_hit`
  checks the flag and the marker instead.
- Portal UI (`web/app.js`): a truncated card is labelled "Partly in context
  (truncated)", expanded, and its warnings are not listed in the
  excluded-warnings area (review M4).
- MCP `structuredContent.hits[0]` gets `"truncated_in_context": true` when
  truncated, absent otherwise (review L12). The library `RecallResult` is a
  public struct with public fields and no `#[non_exhaustive]`, so a new
  field would break struct literals downstream; it is unchanged.
- Ledger (review M5): the recall line's hit gets `"truncated_in_context":
  true` only when set (`v` stays 1). `canonical_marker` counts a truncated
  Canonical hit when its form kept the label (`WithWarnings`, `LabelOnly`).
- Structural dispatch: the traversal headline counts a truncated dependent
  as rendered ("(1 dependents)").
- `lambo demo` normaliser masks the marker's `lambo_inspect <hex8>` as well
  as `id <hex8>` (review L11).

## For #33 PR 2 (layered merge)

- Each session's recall in the merge must run with no effective budget, or
  have its `context` ignored: the merge re-renders the merged blocks (each
  via `render_detailed_block` / `ContextBlock::for_hit`, with `scope` set for
  layer hits) and runs `fit_blocks` once over the merged list.
- After that cut, call `DetailedRecall::mark_context` on the merged hits so
  per-session `included_in_context` / `truncated_in_context` flags are reset
  from the merged cut, never carried over, and set `truncation` from it.

## Tests

`recall::format::fit::tests`: offsets of `for_hit`; empty list; exact fit;
one token over (label kept, marker count); canonical marker and warning kept;
label-only then content-only fallback; huge 2/3/4-byte and ZWJ text across
budgets; below-marker budgets incl. 0 and 1; second block dropped with a
fitting third after it; byte identity (context and `whole`) against a copy of
the pre-#130 loop for two estimators; saturated estimator; head never ends in
whitespace; longest head against a linear scan; marker count is exactly the
left-out text; scoped marker.
`recall::dispatch::tests::structural_oversized_first_dependent_is_truncated_not_dropped`
and `assemble_and_dispatch_cut_the_budget_identically`.
`recall::assemble::tests::ranked_prefix_stops_at_first_nonfit_block` (now also
above the marker size).
`cli::recall::tests`: truncated block with warnings kept; without them, with
the warning text also inside the content head.
`mcp::server::tests::ledger`: `i1_the_recall_line_reports_a_truncated_top_hit`
(canonical_marker per form, ledger key) and
`i1_a_truncated_top_hit_is_flagged_in_the_response_and_the_ledger` (real tool).
`cli::serve_web::tests::recall::recall_endpoint_flags_a_truncated_top_hit`.
`cli::demo::tests::normalize_node_ids_masks_uuids_and_preserves_multibyte_text`
(marker id). The existing goldens are unchanged.
