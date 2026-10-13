# #19(c): contradiction warnings, revalidation and the GC rule

Recorded 2026-10-13 for 0.6.0. Branch `feat/19c-contradiction-warning`
from `main` `389df857`. Design of record: issue #19 (body, the Phase 0
refresh and its approval), the three owner decisions for (c) dated
2026-10-12, and the coordinator's dating decision (option A, below). Builds
on [feature-19a-contradicts-edges.md](feature-19a-contradicts-edges.md)
(#140, the edge) and [feature-19b-against.md](feature-19b-against.md)
(#151/#152, the write field).

## What it adds

- `daemon::contradiction`: `detect`, `contradiction_at`,
  `insert_contradictions`, `ContradictionHit`. The daemon cycle runs it
  beside the conflict detector.
- `HotListPayload::Contradiction { by, writer, at }` and
  `Condition::Contradiction` (conflict-tier severity). Hot-list only: no
  `DaemonEvent` (the enum is frozen), so its pairs keep their hot-list
  entries through `retain_conditions` but never enter the event-arming set.
- Recall: the `contradiction` line on the hit, the contradicting concept
  pulled in with a `contradicts` line, both force-included; annotation kinds
  `contradiction` and `contradicts`; layered forcing; the ledger's
  `contradiction_warning`.
- GC: `EdgeType::counts_toward_gc_connectivity` (false only for
  `Contradicts`) and the promoted-target protection.
- `CycleParams::contradiction_evidence_min_age`, from
  `Config::canonization_edge_min_age`.

## Decisions

### When it fires

The hit is Canonical or Venerable; another concept of its session has a
`Contradicts` edge to it; that edge's about-time is not after `now` and is
strictly later than the hit's latest eligible dependent evidence, or, with
none, than the hit's origin interaction's about-time (the Phase 0 baseline).
Self-edges never fire.

### Dating: about-time on both sides (coordinator, option A)

The brief said "the edge's write time". The approved Phase 0 design says
"the declaring contradiction edge's about-time", and #36's evidence is
already measured on `about_time()`. Comparing a write-time contradiction
against about-time evidence would let a backdated contradiction (a
historical ingest: contradiction about 2018, citations about 2020, all
written today) beat the evidence that outweighed it. Reported and stopped;
the coordinator chose A: `edge.about_time()` against the evidence's
`about_time()`. "The write that declares it" names which write counts (the
edge, not the contradicting concept's creation), not which clock.

Re-sending a contradiction reinforces the edge (`last_reinforced`, weight)
and keeps its first `event_time` and `created_at`, so it is not re-dated,
and its writer is attributed at `created_at` (the first declaration), not
at the latest reinforcement. To restate a contradiction as new, write a new
assertion. Pinned by
`re_sending_a_contradiction_does_not_re_date_or_re_attribute_it`.

### Eligible evidence

One piece per dependence edge into the hit (#36: `Dependency` sources and
`Hierarchical` children, concept to concept), dated by that edge's
about-time. Guards: the edge's about-time is at or before
`now - canonization_edge_min_age` (age and future guard, Stage 2's floor);
the dependent's origin interaction is not the hit's own. Per-edge rather
than per-dependent-origin-interaction because a citation written today by an
old concept is today's evidence, and the contradiction itself is dated per
edge; the two sides then measure the same thing.

Consequence of the age guard: support younger than the floor (60 s by
default) does not silence a contradiction until it ages in; recall's
revalidation then drops the warning on its own
(`revalidation_drops_the_entry_when_newer_evidence_arrives_or_ages_in`).

### Self-suppression

Evidence never counts against a contradiction when it was written by the
agent that declared the contradiction (write-time attribution at the
evidence edge's `created_at`; when one instant has several candidate agents,
any shared candidate excludes it, erring toward the warning) or when the
dependent is the contradicting concept itself. #19(b) refuses citing and
contradicting one concept in one call, but separate writes can do both; a
writer cannot outweigh its own contradiction. Pinned by
`a_writer_cannot_outweigh_its_own_contradiction`.

### One payload per hit

The hot list keeps one entry per `(node, condition)`. The payload is the
qualifying contradiction with the newest about-time, then the smallest
canonical key of `by`, then the smallest id. The payload shape is exactly
the settled `{ by, writer, at }`; other contradictions are visible through
`lambo_inspect`. When the newest lapses (its concept collected), the next
revalidation falls back to the next qualifying one
(`revalidation_moves_to_the_next_contradiction_when_one_lapses`).

### Writer

The agent acting when the edge was first written: `daemon::conflict`'s
write-time attribution, now exposed as `attribute_at` with the instant
passed in (the conflict detector still attributes at the latest write),
falling back to `by`'s origin agent.

### Companion and rendering

Assembly collects every `by` of a `Contradiction` payload in the
re-validated hot map, when it is a concept of the target's session, and adds
it to the members (scored like any member, so with no phase-1 relevance it
scores its daemon share only) and to the forced set, so it lifts `top_k`
like the hit. Its block still counts toward `max_tokens`. The hit's line
always quotes up to 80 characters of `by` and names its short id, so a
companion cut by the budget or a layer floor loses nothing. A companion
contradicting several shown hits carries one `contradicts` line per hit, in
id order, and appears once. A payload whose `by` left the graph pulls
nothing in and renders the line without a quote.

Line shapes:

```text
⚠ Contradicted at 2026-08-10T10:28:00Z by agent-c: "accounts replaced the users table" (id 8c41d0e2)
⚠ Contradicts "user schema" (id f0000000)
```

The writer is rendered as its raw agent id, like the reservation line, not
through the demo's `agent_display`.

### Layered recall (#33)

Each tier's recall runs over its own session's graph, so `by` is always the
same session's concept (cross-session `against` is refused at write time,
and the detector and assembly both skip a foreign `by` defensively). The
merge treats both new kinds as forced: past `top_k`, never past a floor. A
layer's companion without keyword or vector evidence stays out; the layer's
prefixed line still names it. Pinned at merge level
(`contradiction_annotations_force_past_top_k_but_never_the_floor`) and end
to end (`a_layered_recall_keeps_contradictions_inside_their_session`).

### GC (owner decision 1, option C), made exact

- `Contradicts` does not count for step 2's orphan test or step 3's
  reachability walk.
- Protection: a concept with an outgoing `Contradicts` edge to a concept
  that is Venerable or Canonical when the sweep starts is added to the
  sweep's protected set, exactly what a Venerable concept gets: not an
  orphan, not a disconnected component, not a score-cut victim, and outside
  the collection cap's base. Being contradicted by a promoted concept
  protects nothing; contradicting a Candidate or None concept protects
  nothing.
- Protection is not connectivity: a concept reached only through a
  protected contradicting concept is still collected.
- The boundary: once the target is demoted (Canonical to None; Venerable
  never demotes directly), the next sweep judges the contradicting concept
  like any other, and with only its contradiction it is an orphan.

### Decisions 2 and 3

2B (conflict writers) was applied in #19(b) and is unchanged. 3A
(high-risk freshness) is unchanged and now pinned:
`a_fresh_contradiction_fires_the_high_risk_alert_on_a_high_value_target`
(Canonical and big-radius targets alert, an ordinary one does not, the
contradiction is not a dependent, and the alert leaves with the window).

## Tests

| Issue / decision | Test |
|---|---|
| Canonical/Venerable newer contradiction annotated, forced, source in result | `daemon::contradiction::tests::a_newer_contradiction_of_a_canonical_or_venerable_concept_fires`, `daemon::tests::recall::recall_renders_a_contradiction_and_pulls_in_its_source` (golden), `recall::assemble::tests::a_contradicted_hit_is_forced_and_pulls_in_its_contradicting_concept`, `memory::tests::contradiction::an_against_write_on_a_canonical_belief_warns_until_it_is_demoted` |
| Older contradiction quiet; equal quiet; future ignored | `later_evidence_outweighs_an_older_contradiction`, `evidence_at_the_same_instant_outweighs_it`, `a_future_dated_contradiction_does_not_fire_and_future_evidence_does_not_count` |
| Backdated ingest (option A) | `a_backdated_contradiction_is_outweighed_by_later_dated_evidence` |
| Age eligibility, own origin, Hierarchical orientation, non-evidence edges, baseline | `evidence_counts_only_once_it_is_age_eligible`, `a_dependent_from_the_targets_own_origin_is_not_evidence`, `hierarchical_evidence_is_the_targets_children_not_its_parents`, `causal_and_contradicts_edges_are_not_evidence`, `without_evidence_the_targets_origin_about_time_is_the_baseline` |
| None/Candidate never | `a_none_or_candidate_concept_is_never_contradicted` |
| Multiple, deterministic | `several_contradictions_resolve_to_the_newest_then_key_then_id` |
| Revalidation | `revalidation_drops_the_entry_when_the_target_is_demoted`, `..._when_newer_evidence_arrives_or_ages_in`, `..._when_the_contradicting_concept_is_collected`, `revalidation_moves_to_the_next_contradiction_when_one_lapses`, `daemon::tests::conditions::a_contradiction_is_held_on_the_hot_list_without_an_event` |
| Companion, top_k, budget | `a_companion_already_in_the_result_is_not_duplicated`, `a_companion_contradicting_two_hits_names_both_in_id_order`, `a_companion_cut_by_the_budget_is_still_named_on_its_target`, `a_missing_contradicting_concept_is_named_by_id_only` |
| Layered | `recall::layered::tests::contradiction_annotations_force_past_top_k_but_never_the_floor`, `memory::tests::contradiction::a_layered_recall_keeps_contradictions_inside_their_session` |
| Decision 1C | `daemon::gc::tests::contradictions::*` (7 tests), `types::tests::edge_type_gc_connectivity_table` |
| Decision 3A | `daemon::events::tests::a_fresh_contradiction_fires_the_high_risk_alert_on_a_high_value_target` |
| Span/blast unchanged | #140/#151's `canon::contradicts_tests` (unchanged, passing) |
| Existing goldens unchanged | `recall_entry_reproduces_context_golden`, `recall-goldens.json`, `recall-h3-goldens.json` unchanged; new `fixtures/recall-context-contradiction-golden.txt` |

No fair-test fixture case was added: `fixtures/session-fair-test.json` drives
canonization verdicts, which a contradiction must not change (already
pinned by `canon::contradicts_tests`), and the warning needs a promoted
concept plus a hot-list pass, which the REST API fixture golden covers.

## Spec errata

**§8 recall / §9 hot list.** A fifth hot-list condition, `Contradiction`,
hot-list only (no `DaemonEvent`), conflict-tier, rendered as above, with
its companion force-included. **§9 GC.** `Contradicts` is not connectivity;
a concept contradicting a promoted one is a protected class while the
target holds its status.
