# Dependency-question anchors match whole words only (0.6.0)

No issue number: found while reviewing #33 PR 2 (layered recall, #134). That
PR lets a layer's traversal answer bypass the layer's relevance floor on the
strength of the resolved anchor, so a wrong anchor now admits a wrong answer
past a floor as well as dispatching it.

## The defect

`recall::dispatch::resolve_anchor` chose the anchor of a dependency question
("what depends on X", "blast radius of X", ...) as any concept whose whole
content was a case-insensitive **substring** of the query, ranked
Entity/Resource first, then **shortest** content. Two failures followed:

- A short concept anchored from inside a longer word: `db` anchored "what
  depends on dbadmin", `a` anchored nearly every query ("what", "safe"), and
  all of that concept's dependents were returned as the answer.
- The shortest match won, so `db` beat `user db schema` when the query spelled
  out `user db schema`. The shortest rule was written to prefer "the bare
  identifier, not a longer clause that happens to contain it", but a longer
  clause can only match when the query contains all of it, in which case it is
  the thing the query named.

## The rule now

1. **Whole words.** `word_bounded_spans(lower_query, lower_content)` returns
   every occurrence (overlapping ones included, so an embedded first hit
   cannot hide a later whole-word one) whose edges sit on word boundaries.
   Word characters are `char::is_alphanumeric` (Unicode-aware: `café`, `数据`,
   `Ωmega` are words; CJK runs are one word, so `数据` does not match inside
   `数据库`). A content edge that is itself punctuation delimits itself, so
   `.env` matches in `config/.env` and `prod.env`, and `src/main.rs` matches
   in "what depends on `src/main.rs`?". `_`, `-`, `/` and `.` separate words,
   following the task's definition; `db` therefore matches in `user_db`, and
   the containment rule below is what keeps a longer `user_db` concept ahead.
2. **The question's own wording never anchors.**
   - A match wholly inside a structural-marker occurrence (`marker_spans`) is
     ignored: a concept literally called `blast radius` would otherwise anchor
     every "blast radius of ..." question, and with longest-wins it would beat
     the real subject. A longer concept that only starts with marker words
     (`blast radius report`) is not inside the span and still anchors.
   - `ANCHOR_STOP_WORDS`: a concept made only of closed-class function words
     (`it`, `the`, `what`, `of`, `on`, `is`, ...) never anchors. Justified
     after the boundary fix: "is **it** safe to delete the payments queue"
     still contains `it` as a whole word, so a concept `it` (or `IT`) would
     hijack the dispatch whenever the real subject is not a concept. The cost
     is that "what depends on IT" falls through to the blended recall, which
     still finds `IT` by word match, a safe refusal.
   - No minimum length: `db`, `id`, `vpc` are real identifiers, and once
     boundaries exist they no longer match inside words.
3. **Containment.** A candidate is dropped when every one of its spans lies
   inside a strictly longer span of another candidate (`db` inside
   `user db schema`, `main.rs` inside `src/main.rs`, `café` inside `Café-DB`).
   This decides the overlap case regardless of concept type, so an Entity `db`
   no longer beats a Logic `user db schema` the query spelled out.
4. **Rank** what is left: Entity/Resource first (unchanged), then the
   **longest** content in chars (was shortest), then content, then the
   issue-2 chain (canonical key, node id). Only disjoint matches reach the
   length key now ("what depends on cache and orders queue"), and there the
   longer name is the more specific one.
5. The "security group" prose fallback is unchanged.

## Goldens

No fixture or golden output changed: `fixtures/recall-*`, the H3 goldens
(`h3_structural_payload_matches_golden`, `h3_blended_payload_matches_golden`),
`recall_entry_reproduces_context_golden`, `t84_demo` and `binary_parity` all
pass untouched. The only structural queries in fixtures and tests name a
concept as a whole word (`SG-Base-VPC`, `the anchor`) or use the
security-group fallback, and none had a competing shorter or longer match.

## Tests

In `src/recall/dispatch.rs`: `word_bounded_spans_need_word_edges_and_see_overlaps`,
`short_concept_inside_a_longer_word_does_not_anchor`,
`short_whole_word_concept_anchors_when_it_is_the_best_match`,
`the_longer_named_anchor_wins_over_a_word_inside_it`,
`multibyte_anchors_match_on_unicode_word_boundaries`,
`punctuation_and_path_anchors_match_inside_a_query`,
`marker_and_stop_word_concepts_do_not_anchor_on_question_scaffolding`.

## Other resolvers checked (not changed here)

- `surface::focus::resolve_focus` fuzzy leg (`fuzzy_candidates`): the focus
  is a substring of concept content, the reverse direction. A short focus such
  as `db` resolves to a lone `dbadmin` concept as `Focus::Fuzzy`, which the
  caller is told about, and several matches refuse as `Ambiguous`; its sort is
  shortest-first, which is right for that direction (least padding around
  what was typed). Not silent, so lower risk, but it is the same substring
  class and could prefer whole-word matches first.
- `near_matches_over` counts focus tokens by substring for suggestions only.
- Store `keyword_candidates` (memory `contains`, SQLite `instr`, Postgres
  equivalent) score by token substring by design, with cross-store parity
  pinned; that is ranking, not anchor resolution.
