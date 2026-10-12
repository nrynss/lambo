# #130 and #33 PR 1: one budget cut, and a long top hit is truncated

Issue #130: a recall rendered an empty `context` whenever the top-ranked block
alone exceeded `max_tokens`, although `hits` held the answer. At the default
500 tokens (`ceil(bytes / 3.5)`) that is any block over about 1,750 bytes; the
lambo-paper evaluation saw 6 of 12 renders come back empty. Design of record:
#33 design section 2.5 and PR 1 in section 10.

## What changed

- `src/recall/format/fit.rs` holds the one budget cut,
  `format::fit_blocks(&[ContextBlock], max_tokens, token_fn) -> FittedContext`.
  It replaces the two copies of the loop (`assemble.rs`, `dispatch.rs`). The
  #33 layered merge (PR 2) calls it a third time over the merged list.
- `ContextBlock { text, inspect_id }`. `inspect_id` is a free string so a
  layer hit can pass `<session>:<short id>` (design 2.5).
- `FittedContext { context, whole, first_truncated }`, with `rendered()` =
  `whole + first_truncated`. Callers set `included_in_context` for
  `i < whole` and `truncated_in_context` for hit 0 when `first_truncated`.

## The rule

1. Longest ranked prefix of whole blocks, measured on the actual joined text
   (separators included). A count of `usize::MAX` never fits (the existing
   `checked_add` guard). Stop at the first block that does not fit; never
   skip ahead (GPT5.6sol P1-4).
2. Only when no whole block fits: the first block's head, cut at a UTF-8
   char boundary with trailing whitespace trimmed, then on its own line
   `… [truncated: <n> more tokens; lambo_inspect <id> for the full concept]`.
   `n` is `token_fn` of the omitted tail. Nothing follows.
3. If the marker alone (empty head) does not fit, the context is `""`, as
   before. `max_tokens` 0 and 1 stay empty; with an 8-char id the marker is
   about 22 tokens.

Option 1 of the issue, chosen over option 2 (always include the top block)
because `max_tokens` stays a hard ceiling and the acceptance test
`max_tokens_zero_yields_empty_context_but_full_hits` keeps its meaning.

The cut point is a binary search over char boundaries. Every candidate is
measured as the exact string returned, so the result fits for any estimator.
For a monotone estimator (the default is) it is also the longest head: one
more char adds at least a byte while the marker loses at most one digit.

## Surfaces

- `DetailedHit.truncated_in_context`: serialized only when true, so every
  H3 golden and every recall whose first block fits is byte-identical. The
  truncated hit keeps `included_in_context: false` (its complete block is not
  in context).
- `render_cli_text` (CLI and portal text) cannot re-render a truncated block
  from the hit, so it uses the pipeline's `context` for it, and sends the
  hit's annotations to the header unless the head already shows them.
- Structural dispatch: the traversal headline counts a truncated dependent
  as rendered ("(1 dependents)").
- Ledger `canonical_marker`: a truncated Canonical hit counts only if the
  head starts with its full `concept_label` (so `[…, canonical]` was shown).
  No ledger key was added.

## Tests

`recall::format::fit::tests` (empty list, exact fit, one token over, huge
2/3/4-byte and ZWJ text across budgets, below-marker budgets incl. 0 and 1,
second block dropped with a fitting third after it, saturated estimator,
whitespace trim, and byte identity against a copy of the pre-#130 loop over
every budget where the first block fits, for two estimators).
`recall::dispatch::tests::structural_oversized_first_dependent_is_truncated_not_dropped`
and `assemble_and_dispatch_cut_the_budget_identically` (both sites over a
budget sweep that crosses empty, truncated and whole).
`cli::recall::tests::a_truncated_first_block_renders_as_the_pipeline_context`.
The existing goldens (`fixtures/recall-h3-goldens.json`, the user-schema demo
block) are unchanged.
