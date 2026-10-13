# #19(b): `against`, contradictions through the shared resolver

Recorded 2026-10-13 for 0.6.0. Design of record: the refreshed #19 design
([#19](https://github.com/nrynss/lambo/issues/19) comment 6127049450), its
approval (6127133919), the three #19(c) owner decisions, and the **owner
decision for #19(b), option A** (latest comment): `against` targets behave
exactly like #36 PR 4's citations. Builds on
[feature-19a-contradicts-edges.md](feature-19a-contradicts-edges.md) (#140,
the edge type and its exclusions) and
[feature-36-pr4-citations.md](feature-36-pr4-citations.md) (#147, the
resolver and the pin, recheck and edge-write path). Branch
`feat/19b-against` from `origin/main` `e333bc7c`.

## What it adds

- `lambo_derive` concepts take `against`: existing concepts this one says
  are wrong. Each resolved reference is written as
  `newer -Contradicts-> older`, concept to concept.
- `lambo_record_action` takes `against` too (the approved design puts the
  field on both): the action's own `Resource` concept contradicts each
  target ("refunded: charger overheated" against "brand Y chargers are
  fine").
- CLI: `lambo derive --against <REF>` (first concept) and
  `lambo record-action --against <REF>`.
- Library: `ConceptCitations::against`, `PinnedCitations::against`,
  `DeriveOutcome::contradicted`, `Memory::record_action_against_async_as`,
  `Memory::record_action_against_embedded_as`,
  `WriteIntentPayload::ActionCited`, `ACTION_CITED_PAYLOAD_LIKE`,
  `ReferenceField::{Against, ActionAgainst}`, `graph::cite::{AGAINST,
  ACTION_AGAINST, MAX_AGAINST_PER_CONCEPT, MAX_AGAINST_PER_DERIVE}`.
- The tool descriptions, `get_info` and `AGENTS.md`'s dogfood rules ask
  agents to name a concept a write shows to be wrong in `against`.

## Decisions

### One path: the PR 4 spec struct, unchanged in shape

`graph::cite::{pin, recheck, write_edges}` already took a `ReferenceSpec`
(field, edge type, weight, acyclic, limits). `against` adds two specs,
`AGAINST` (derive) and `ACTION_AGAINST` (record_action), with
`EdgeType::Contradicts`, and one new spec flag, below. Validate pins
`depends_on` then `against` against the same graph read; the commit
re-resolves both lists (`cite::recheck_all`) inside the write's
transaction before the call's own writes, and writes both edge lists
(`cite::write_all_edges`) after the concepts. **Not** in hybrid's
`plan_still_holds` (owner decision): a changed target is a typed
`Changed` refusal the agent can retry, checked under the same lock hold.

`record_action` gains `graph::action::validate_against` (the pre-pass, the
action as the one citing concept) and `record_action_against`, which wraps
`record_action_pinned` in a `Graph::transaction` with the recheck before
and the edge write after, so a refusal at the commit rolls the action
itself back. With no `against` it is `record_action_pinned` exactly,
outside any transaction, as before. `Action` is unchanged (it is a public
struct literal in many places); `against` is a separate argument.

### No acyclicity for contradictions

`ReferenceSpec::acyclic` is false for both specs. Why: mutual
contradiction is legitimate (each asserts the other is wrong; #140's
fixture carries `Q <-> X`); `Contradicts` is outside §5.7's
`Causal`/`Dependency` family; and nothing traverses it (no dependence,
recall expansion or GC structural walk), so a cycle of contradictions
cannot loop anything. A contradiction against a concept that depends on
the contradicting one is no `Dependency` cycle either. Self-contradiction
is still refused (`SelfReference`), at the ack and again at the commit.

### A similarity merge keeps the contradiction

New spec flag `merge_is_reinforcement`: true for `depends_on` (PR 4 review
M2), false for `against`. A hybrid similarity merge writes the new
concept as its own node tied to its near twin by a `Semantic` edge. For a
citation of that twin the tie already says it; for a contradiction it does
not, and near-identical text often states the opposite claim ("runs
small" / "runs true to size"), so the `Contradicts` edge is written.

### Weight and budget

The edge weight is 1.0 (`CONTRADICTS_WEIGHT`): `Contradicts` never decays
and feeds no score (#140), so the weight only says the opposition was
declared outright. Limits are `against`'s own, counted apart from
`depends_on`: 8 per concept and 32 per derive call (the design's
citation-equivalent bounds), 8 per action. The MCP surface and the CLI
refuse them as bad parameters before the core; the core refuses them too
(`Limit`). `against` is not counted in `record_action`'s 64-target fan-out
cap: it creates nothing and has its own bound.

### Same target in `depends_on` and `against`

**One call: refused** (review M2, orchestrator decision: the conservative
choice, easy to undo; the owner sees it in the PR body). A concept whose
`depends_on` and `against` resolve to the same concept, or an action
whose `depends_on` entry (by id or by text, as the action would write it)
resolves to an `against` target, is refused with the new
`ReferenceRefusalKind::DependsOnAndAgainst`, field `against`, before the
ack and again at the commit (`cite::refuse_cited_and_contradicted`,
`action::refuse_depended_and_contradicted`). Different concepts of one
call may cite and contradict the same target. **Separate writes may still
produce both edges** between one pair: the graph admits it, and refusing
across writes would need history the write path does not consult.

### Refusals and wording

Every refusal kind of PR 4 applies except `Cycle`. `ReferenceRefusal`'s
message is field-aware: an `against` refusal says `against "<ref>"`,
"name a concept that already exists" for a missing one, "names the
contradicting concept itself" for self, and, cross-session, "cross-session
contradictions are not recorded. Contradict a concept in this session; to
challenge that one, write the assertion in scope "general" itself": the
approved layered rule (project to general refused; a deliberate assertion
within general is the way). The cross-session attribution is PR 4's
`attribute_foreign`, run after this session's guard is dropped, for both
fields of both tools.

### Durable intents

- A derive with `against` stays `DeriveCited`: `PinnedCitations` gains an
  `against` list, skipped when empty, so a derive that only cites
  serializes exactly as PR 4 wrote it (pinned by
  `a_derive_s_contradictions_ride_in_the_cited_intent`). `PinnedCitations`
  and `PinnedReference` keep `deny_unknown_fields`, so a PR 4-era build
  refuses an unconsumed intent carrying `against` rather than dropping it.
- An action with `against` is a new kind, `ActionCited` (the action fields,
  its `depends_on_pins`, and `against: Vec<PinnedReference>`), for design
  Q18's reason: a defaulted field on `Action` would let a 0.5.0 replay
  apply it with the contradictions silently dropped. An action without
  `against` stays `Action`, byte for byte.
- Both settle to the empty `Derive` (`WriteIntentPayload::settled`, and the
  SQL consume UPDATE gains `ACTION_CITED_PAYLOAD_LIKE` in SQLite and both
  Postgres-family statements), so a 0.5.0 build still loads a session
  whose contradicting writes all applied. Checked with PR 4's frozen
  v0.5.0 decoder (`a_settled_contradicting_intent_decodes_under_the_0_5_0_payload`)
  and by the shared `check_a_settled_image_intent_keeps_no_vector`
  (MemoryStore and SQLite here; Postgres and Cockroach live, not run).
- A replay re-resolves and either applies or settles `failed` with the
  typed refusal (`LamboError::Reference` was already in the replay's
  "fact about this input" arm).

### Receipts and outputs

A derive receipt ends `; N contradiction(s) recorded` when the call added
some, as citations do; a call without `against` reads exactly as before.
An action's new contradiction edges count in `ActionOutcome::edges`. The
CLI's derive line says the same as the receipt.

### #33 PR 4 (`scope` on writes)

#33 PR 4 merged to main as #149 (`6add6f12`) while this branch was open;
it was merged in (not rebased). `against` needed no new resolver code to
respect `scope`: the derive's `against` rides in `ConceptCitations`, which
`derive_cited_async_scoped` already resolves against the target session's
graph with the target's `LayerProbe`, and `record_action_async_scoped`
now takes `against` beside `deliver_to` and resolves it against the
target memory, refusing a target only another session holds (for a write
scoped to a layer, the caller's own session first) as cross-session. A
contradiction creates nothing, so #149's `text_holder_of` minting guard
and relative-path check do not apply to it. The `against` parameter docs
now say "the session the write goes to".
`scoped_writes::a_cross_session_contradiction_is_refused_in_both_directions`
pins it: project concept from a write scoped to `general`, `general`
concept from an unscoped write, by short and full id, on derive and
action, nothing written in either session; then a scoped contradiction
lands inside `general` only.

## Spec errata

**§6.1 / §6.2 `lambo_derive` and §7 `record_action`.** Each concept, and
each action, may carry `against` as above. It writes `Contradicts` edges
(the eighth edge type, #19(a) errata), which are outside §5.7's
write-time acyclicity.

## Tests

| Requirement | Test |
|---|---|
| Each refusal kind | `graph::cite::against_tests::every_bad_against_is_refused_with_its_kind_and_nothing_is_written` (missing, ambiguous, interaction, self, limit, unknown), `record_action_against_writes_from_the_action_node_and_refuses_bad_targets`, `against_has_its_own_budget` (call limit), `memory::tests::against::a_refused_against_is_an_error_before_the_ack_and_writes_nothing` (derive and action, foreign), `mcp::server::tests::against::a_bad_against_is_refused_before_the_ack_with_its_reason`, `the_against_limits_are_bad_params`, `types::reference` message test |
| Pin and recheck, target removed in flight | `a_changed_against_target_refuses_at_the_commit` (removed, shadowed; graph, epoch and log unchanged; the call's citation rolls back too), `the_commit_refuses_a_self_contradiction_it_could_not_see_at_the_ack`, `record_action_against_rolls_back_on_a_changed_target`, `memory::tests::against::a_contradicted_concept_removed_while_the_write_is_in_flight_fails_the_receipt` (hybrid, embed held; derive and action) |
| Replay | `a_replayed_contradiction_re_resolves_and_settles_applied_or_failed` (derive and action; kept target applies, removed settles `failed`) |
| Settled intent under the frozen 0.5.0 decoder | `writeq::tests::intents::a_settled_contradicting_intent_decodes_under_the_0_5_0_payload`, `an_action_with_against_is_its_own_variant_and_round_trips`, `an_unknown_pin_field_is_refused` (now uses an unknown `supersedes`, and checks `against` entries refuse a field), the shared settle check (5 intents on SQLite) |
| Mutual contradiction | `mutual_contradiction_is_allowed`, `mcp::server::tests::against::mutual_and_resent_contradictions_over_the_wire` |
| No score or verdict change | `canon::contradicts_tests::contradictions_written_through_against_change_no_score_or_verdict` (#140's checks, over edges written by `against`: daemon scores, Stage 1, dependent interactions, Solo score, blast radius, store-side span, blast, Stage 2 and 3) |
| Cross-session under a layered serve | `mcp::server::tests::layered::a_contradiction_of_a_layer_concept_is_refused_as_cross_session` (short id, full id, content; derive and action; neither session written), `scoped_writes::a_cross_session_contradiction_is_refused_in_both_directions` (with `scope`, both directions) |
| Byte identity without `against` | `legacy_payloads_are_byte_identical` (unchanged), `a_derive_s_contradictions_ride_in_the_cited_intent` (PR 4's cited form exact), `an_action_with_against_is_its_own_variant_and_round_trips` (plain action has no `against`), every pre-existing test unchanged except the schema goldens and the SQLite intent count |

Also: `against_writes_one_contradicts_edge_newer_to_older` (one edge, weight,
no dependence, natural-key replay), `a_similarity_merge_keeps_the_contradiction`
(graph) and `a_similarity_merge_into_the_contradicted_target_keeps_the_contradiction`
(hybrid), `an_accepted_against_applies_in_the_background_under_both_strategies`,
`the_synchronous_paths_record_and_refuse_the_same`,
`a_contradicting_derive_and_action_land_on_their_targets` (MCP),
`cli::tests::cli_against_records_contradictions_and_refuses_bad_references`,
and the tool schema goldens.

## Out of scope

The detector, recall annotations, companion recall, revalidation and the
three #19(c) owner decisions (GC connectivity, conflict writers,
high-risk freshness) are #19(c). Nothing in recall reads `Contradicts` yet.

## Fixed in passing

`docs(cite)`: four doc comments still described PR 4's removed `name`
field.
