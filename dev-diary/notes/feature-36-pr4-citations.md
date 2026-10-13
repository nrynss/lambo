# #36 PR 4: citations and the shared resolve-or-refuse resolver

Recorded 2026-10-13 for 0.6.0. Design of record:
[design-36-canonization.md](design-36-canonization.md), section 3
(candidate B) and section 4, PR 4 and the #19 paragraph; operator
decisions 5 and 6 on [#36](https://github.com/nrynss/lambo/issues/36)
(2026-10-08). Interacts with #19(b) (refreshed design and approval on
[#19](https://github.com/nrynss/lambo/issues/19), comments 6127049450 and
6127133919) and #33 PR 4 (`design-33-layered-recall.md` section 7). Branch
`feat/36-pr4-resolver` from `origin/main` 9bef6ec8. The PR 1 to PR 3 notes
([dependents](feature-36-pr1-dependents.md),
[denominator](feature-36-pr2-denominator.md),
[admission](feature-36-pr3-admission.md)) are the contracts this builds on.

## What it adds

- `lambo_derive` concepts take `depends_on` (existing concepts the concept
  rests on, at most 8 per concept and 32 per call). A citation is written as
  `citing -Dependency-> cited`, the edge `record_action`'s `depends_on`
  writes, so the cited concept gains a dependent under PR 1's relation.
  (The first round also had `name`; the review removed it, see below.)
- `lambo_record_action` `depends_on` entries that are an existing concept's
  hyphenated node id, or a unique short id whose text names no concept,
  resolve to it; an interaction's id or a shared short id is refused, and a
  hyphenated id held only by a layer is refused as cross-session. Every
  other entry keeps the 0.5.0 text rule (canonical match, else create).
- `lambo derive --depends-on <REF>` for the first concept.
- The tool descriptions, `get_info` and `AGENTS.md`'s dogfood rules ask
  agents to cite the rulings they rely on by id.
- Library: `Memory::derive_cited_as`, `derive_cited_for_ingest_as`,
  `derive_cited_async_as`; `ConceptCitations`, `PinnedCitations`,
  `PinnedReference`, `LamboError::Reference(Box<ReferenceRefusal>)` with
  `ReferenceRefusalKind`; `graph::cite` (limits, pre-pass, commit step).

## Decisions

### The resolver lives in `crate::surface::resolve`, beside `focus`

`resolve_reference(&Graph, &str) -> Resolved { Concept { node, leg },
Missing, Ambiguous, NotAConcept }`. Legs: a full node id (exclusive: content
never has the hyphenated shape, the only full-id form a write accepts), a
short id (exactly 8 hex characters, a unique prefix; orchestrator decision
after the Sonnet review: 9 to 32 hex characters and the unhyphenated form
are text, so the surface that changed from 0.5.0 stays small and matches
the only ids agents see) and the canonical key, computed **without** the synonym lookup
(`graph::canonical::canonical_key(.., |_| None)`). No synonym is ever a
reference: synonyms are in-memory state (S5), so a reference resolved
through one could mean something else after a restart (review H1). The
distinct concepts the legs find decide: none is missing, one resolves, more
is ambiguous, so a hex string that is one concept's short id and another's
content refuses instead of picking.

**Why.** One function for every write that names an existing concept:
#36's citations, #19(b)'s `contradicts`, and #33 PR 4's scoped writes. It
takes the **target** session's graph as an argument, so a scoped write
passes the layer's graph and nothing in the resolver changes.
`resolve_existing` adds the typed refusal; `refusal` builds one for a kind
the caller decides (self, cycle, changed).

**Inspect's fuzzy leg cannot authorize a write.** `surface::focus` keeps its
substring leg for reads and announces it; the resolver has no substring
leg. `a_substring_inspect_resolves_never_authorizes_a_write` pins it: the
three foci `inspect` resolves by substring are `Missing` here and refused as
citations. The only code the two share is the short-id prefix matcher
(`short_id_matches`, `id_has_hex_prefix`, moved out of `focus.rs`), so a
rendered id resolves the same way for a read and a write.

**No second normalizer** (the #139 note asked for this). The key leg is
`graph::canonical`'s key derivation, so NFC, invisible-character stripping, case folding, word
order and stemming come from `graph::canonical`. The recall anchor resolver
(#139) is a different question (which subject a query is about) and is not
touched.

### Cross-session references are refused by name

A reference that names nothing in the target but resolves in another scope
the server reads is refused as `ForeignScope { scope }`: "`<ref>` is in
scope "general"; cross-session citations are not recorded" (design-33
section 7, Q1). The seam is the crate-private `ForeignScopes` trait: the
Memory pre-pass calls `attribute_foreign` on a `Missing` refusal **after its
own graph guard is dropped**, so one session's lock is held at a time. The
MCP server's `LayerProbe` looks each authorized, live layer up with a new
`LayerRead::Probe` (draws no rate token, for `Merged`'s reason) and reads
its graph under that graph's read lock alone. #33 PR 4 builds the same
probe over the addressed session plus the other layers.

### Pinned at the ack, re-resolved at the commit

The pre-pass (`cite::validate`) returns what each reference named
(`PinnedCitations`); the job and its durable intent carry it. The commit
re-resolves every reference under the graph write lock, **inside the
write's transaction and before the call's own writes** (`cite::recheck`),
and refuses with `Changed { accepted, now }` if the answer differs. Then the
concepts are written, and `cite::write_edges` checks self and cycle against
the sources **as written** and writes the edges; any refusal rolls the whole
transaction back.

**On `plan_still_holds`.** A citation target is not part of what hybrid's
plan depends on: the plan decides how each concept is matched or merged,
and a citation never changes that. A changed citation is a refusal, not a
reason to plan again, so it is checked beside the plan, under the same lock
hold, not inside `plan_still_holds`. What the plan *does* decide, the
written source, is exactly what the self and cycle checks re-read at
commit: a synonym declared through the library between the ack and the
commit can make the citing content canonical-match the cited concept, and
that is refused as `SelfReference`
(`the_commit_refuses_self_and_cycle_it_could_not_see_at_the_ack`). A hybrid
**similarity** merge into the cited concept is not refused (review M2): the
merge's `Semantic` edge already ties the two, so that citation is dropped
and counted as a reinforcement and the rest of the call applies
(`a_similarity_merge_into_the_cited_target_drops_that_citation`).

**#19 asks for more here, and that is an owner call.** #19's refreshed
design says to carry contradiction target resolutions in hybrid's read set
(`plan_still_holds`). This PR deliberately does not put citations there
(the review agreed): a changed reference is a refusal, checked under the
same lock hold. Whether #19(b)'s `against` should instead make a plan stale
is for the owner to decide before #19(b) starts.
Citation edges are edges, so they enter no plan's node-write window and
cannot make an unrelated hybrid plan stale (the #19 design's requirement for
contradiction edges holds here too).

**Durable intents.** A cited derive's intent is a new variant,
`WriteIntentPayload::DeriveCited`, for design Q18's reason: a build that
predates it refuses to decode an unconsumed one rather than apply it with
the citations dropped. A derive without citations stays `Derive`, byte for
byte. An action's intent gains `depends_on_pins` (skipped when empty, so a
request with no id-shaped entry is byte-identical); a 0.5.0 build ignores
it and resolves afresh, which is the 0.5.0 rule. A replay re-resolves and
either reaches the same answer or settles `failed` with the typed refusal
(`LamboError::Reference` joined the replay's "fact about this input" arm).
A **settled** `DeriveCited` is rewritten to the empty `Derive` (review H2),
by `WriteIntentPayload::settled` and by each SQL adapter's consume UPDATE
(`DERIVE_CITED_PAYLOAD_LIKE` beside the image pattern), exactly as a
settled image intent is; without that a 0.5.0 build, which decodes every
intent row at load, could not load a session after any cited write.
`a_settled_cited_intent_decodes_under_the_0_5_0_payload` decodes it with
a frozen copy of the v0.5.0 enum. `PinnedCitations` and `PinnedReference`
are `deny_unknown_fields`, so a later field is refused, not dropped.

**`record_action`: ids only in the forms people print, text otherwise**
(review M1). The first round refused unknown full ids, and took every
`Uuid::parse_str` spelling (32-hex, braced, `urn:uuid:`) as one, so it
refused depends_on strings 0.5.0 accepted. Now:

- a **hyphenated** id (the form recall, inspect and receipts print) that
  names a concept cites it; one naming an interaction is refused; one
  naming nothing is text (so a re-sent 0.5.0 request lands on the Entity
  its id text minted then), unless a layer holds it, which is refused as
  cross-session (`Memory::record_action_async_scoped`, via
  `graph::action::unresolved_ids`);
- exactly 8 **hex** characters whose text names no concept cite the one
  concept whose id they prefix, and a shared prefix is refused; a text
  match wins, as in 0.5.0. So a commit hash that happens to prefix a
  concept's id cites it (review L4): about (concepts) / 2^32 for 8
  characters, documented in mcp.mdx and the CHANGELOG;
- everything else, 9 to 32 hex characters (the unhyphenated form
  included), `urn:uuid:` and braced ids, is the 0.5.0 text rule
  (`only_the_hyphenated_id_and_exactly_eight_hex_characters_are_ids`).
  Inspect's read-only focus keeps its own 8-to-32 prefix check.

An entry resolved by id is pinned, and an id-shaped one that named nothing
at the ack is pinned as `None`, so it is refused if it comes to name a
concept before the apply. A text entry keeps the 0.5.0 behaviour, races
included.

### A call cannot cite what it creates

References resolve against the graph as it is; a concept the same call
derives does not exist yet, so citing it is `Missing`. This keeps the
pre-pass and the commit on the same answer and keeps "never create the
target" literal. It costs nothing for canonization: an own-interaction
dependent never counted (PR 1). Derive the ruling, then cite it.

### No names in 0.6.0 (review H1, orchestrator decision)

The first round let a derive give a concept a `name`, registered as a
synonym. The review showed by probe that in-memory synonyms under S5 are
not safe: after a restart a forgotten name, or one spelled in another case,
silently linked to an alias Entity that `record_action`'s text rule had
minted, and renaming was then refused as taken for good. So `name` is gone
from the wire, the CLI and the `Memory` API, and the resolver consults no
synonym at all: citations work by full id, unique short id or canonical
key. The first round's "fails closed" claims went with it.

**Follow-up: durable names.** A name that survives a restart needs a
synonym `Mutation` in every adapter and an owner decision against S5. The
orchestrator is opening the issue.

### Lock discipline

- The pre-pass reads under the graph read guard and drops it before the
  foreign probe and before any await; the commit takes the graph write
  lock, then (after it) the index write lock in `mirror_concepts`, the
  existing order. The write path never takes the hot list, so the daemon's
  graph-then-hot-list order has nothing to contend with
  (`cited_writes_recalls_and_the_daemon_run_together_without_deadlock`).
- No guard is held across an await: the rmcp tool futures must be `Send`
  and `parking_lot` guards are not, so a guard held across an await in
  these paths fails to compile (the first draft did, and was fixed by
  splitting the statement), and clippy's `await_holding_lock` covers the
  tests. `the_foreign_probe_runs_with_this_sessions_lock_released` checks
  that the probe can take this session's write lock while it runs.

### Limits

8 references per concept and 32 per call (the design's numbers), every
reference within the 16 KB string cap. The MCP surface and the CLI refuse
them as bad parameters before the core; the core refuses them too
(`ReferenceRefusalKind::Limit`, counted before anything resolves) for
library callers. The schema publishes `maxItems: 8`.

### One path for every reference field (review L1)

`graph::cite::{pin, recheck, write_edges}` take a `ReferenceSpec` (field,
edge type, weight, whether the edge family is acyclic, limits) and a
`Written` record of the commit (content to node, similarity merges).
`DEPENDS_ON` is the only spec; #19(b)'s `against` adds a `Contradicts`
spec (not acyclic, its own limits) and a second pinned list beside
`references`, and reuses the rest.

## Spec errata

**§6.1 / §6.2 `lambo_derive`.** Each concept may carry `depends_on` as
above. `depends_on` writes `Dependency` edges from the concept to
each cited concept, subject to §5.7's write-time acyclicity over
`Causal`/`Dependency`, which now covers derive as well as `record_action`.

**§7 `record_action`.** A `depends_on` entry that is a hyphenated id naming
a concept, or a short id whose text names nothing and that prefixes one
concept, cites that concept; the rest go through canonicalize-or-create as
before.

## Fair-test fixture

Re-run: every expected status is unchanged, `lease_alias` included. The
fixture's three alias citations are edges already written to the alias
Entity; PR 4 lets new work cite the ruling by id and deliberately rewrites
nothing, and a citation by the alias's text still lands on the alias. So
the row stays Venerable and its comment in `fair_test::expected` says that
only the reviewed backfill (PR 5) flips it.

## Tests

Design list:

| Requirement | Test |
|---|---|
| ID and short ID resolve the same node (synonyms deliberately do not) | `every_leg_reaches_the_same_concept` (full, simple, short, upper-case short, longer prefix, content in other case, word order and Unicode composition; a synonym is `Missing`), `every_reference_form_writes_the_same_one_edge` |
| Missing, ambiguous, foreign, self, cyclic refuse with typed errors before the ack | `every_bad_reference_is_refused_with_its_kind_and_nothing_is_written`, `nothing_resolves_to_missing_and_an_interaction_id_is_not_a_concept`, `a_shared_prefix_is_ambiguous_until_it_is_long_enough`, `legs_that_disagree_are_ambiguous`, `a_refused_reference_is_an_error_before_the_ack_and_writes_nothing`, `a_bad_reference_is_refused_before_the_ack_with_its_reason` (MCP), `a_reference_held_elsewhere_is_refused_as_foreign` |
| Size and count limits | `every_bad_reference_...` (9 on one concept), `the_call_wide_reference_limit_is_enforced`, `the_surface_limits_are_bad_params` (MCP) |
| Natural-key idempotency | `a_resent_citation_reinforces_one_edge` |
| Pre-pass and background agree | `the_sync_and_async_paths_refuse_the_same_references`, `the_commit_and_the_pre_pass_refuse_the_same_kinds`, `an_accepted_citation_applies_in_the_background_under_both_strategies` |
| Graph before hot list; no lock across an await | `cited_writes_recalls_and_the_daemon_run_together_without_deadlock`, `the_foreign_probe_runs_with_this_sessions_lock_released`; the `Send` bound and clippy's `await_holding_lock` (above) |
| Legacy requests unchanged | `a_derive_without_citations_is_unchanged`, `legacy_payloads_are_byte_identical`, `realistic_depends_on_entries_keep_the_0_5_0_text_rule` (7- and 8-char hashes, MD5, `urn:uuid:`, braced, unknown hyphenated id, path, issue refs; re-sent), `a_resent_0_5_0_request_lands_on_the_entity_its_id_text_minted`, every pre-existing test (none changed except the schema goldens) |
| Tool mirrors and schema goldens | `tool_schemas.golden.json` (`lambo_derive`, `lambo_record_action`), `f18_tool_schemas_match_the_golden_property_set`, `published_schemas_carry_runtime_maxima` |

Also asked for:

| Requirement | Test |
|---|---|
| Cross-session refusal | `a_citation_of_a_layer_concept_is_refused_as_cross_session` (MCP, layered: short id, full id and content, derive and record_action, neither session written) |
| Replay re-resolution | `a_replayed_citation_re_resolves_and_settles_applied_or_failed` (kept target applies after restart by short and full id; removed target settles `failed`) |
| Downgrade after cited writes (H2) | `a_settled_cited_intent_decodes_under_the_0_5_0_payload`, `an_unknown_pin_field_is_refused`, the shared `check_a_settled_image_intent_keeps_no_vector` (now with a cited intent, on Memory and SQLite; Postgres and Cockroach run it live) |
| Hybrid self-merge (M2) | `a_similarity_merge_into_the_cited_target_drops_that_citation` (fixture near pair at the 0.85 threshold) |
| `plan_still_holds` behaviour | `a_cited_concept_removed_between_the_ack_and_the_commit_fails_the_receipt` (hybrid, removal while the embed is in flight), `a_reference_that_changes_after_the_ack_refuses_at_the_commit` (removed, shadowed; graph, epoch and log unchanged), `the_commit_refuses_self_and_cycle_it_could_not_see_at_the_ack` |
| Fuzzy inspect never authorizes a write | `a_substring_inspect_resolves_never_authorizes_a_write`, `inspect_and_writes_share_the_short_id_leg` |

Plus `record_action_pins_id_entries_and_refuses_a_changed_one`,
`record_action_cites_ids_and_refuses_only_interactions_and_shared_prefixes`,
`a_commit_hash_that_prefixes_a_concept_id_cites_it_unless_its_text_exists`,
`a_citation_makes_the_cited_concept_load_bearing`,
`a_cited_derive_and_an_id_citing_action_land_on_their_targets` (MCP, with
inspect's dependents), `a_cited_derive_is_its_own_variant_and_round_trips`,
`a_cited_job_is_charged_its_citations`, `an_observation_is_reachable_by_id_only`,
`id_shapes`, and the refusal-message tests in `types::reference`.

## Re-verified against current main (9bef6ec8)

The design's anchors moved: `DeriveParams`/`WireConcept` are
`src/mcp/server/params.rs:145,165` (`RecordActionParams` :197); the write handlers
`src/mcp/server/tools/write.rs`; `Memory::derive_async_as` and
`record_action_async_as` `src/memory/writes.rs:431,510`; `WriteCtx::run`
`src/writeq/execution.rs:230` (the design's :308 is its canonical arm's
write lock, :307); hybrid's commit `src/graph/hybrid.rs` (`derive_planned`,
its transaction at :1383); the CLI `src/cli/derive.rs` and `src/main.rs`.
No schema or migration changed: intents are serialized JSON, and
`Dependency` edges are an existing type. The review round (H2) extended
the consume UPDATE's payload pattern in SQLite and the Postgres family. No `Cargo.toml`, `Cargo.lock`,
workflow or `src/store/pg/postgres/tests.rs` change was needed.

One pre-existing doc error fixed in its own commit: `graph::graph::keys`
said a key could hold two concepts for a loaded snapshot carrying a
duplicate. `Graph::from_snapshot` runs `assert_invariants`, which refuses
that, so the list and the lowest-id pick are defence in depth.

## Review round (2026-10-13)

Adversarial review: 2 High, 3 Medium, 4 Low, each fixed in its own commit.

- **H1** names removed (above); durable names are a follow-up issue.
- **H2** a settled `DeriveCited` settles to the empty `Derive` in
  `settled()` and in every adapter's consume UPDATE; v0.5.0 decode test.
- **M1** `record_action` takes only the hyphenated form as a full id, falls
  back to the 0.5.0 text rule for an id that names nothing, refuses only an
  interaction id, a shared short id, or a layer-held id; dogfood-shaped
  tests.
- **M2** a similarity merge into the cited target drops that citation.
- **M3, L2** (CLI `--name`, name length) went with H1.
- **L1** `deny_unknown_fields` on the pins; `ReferenceSpec`; the
  `plan_still_holds` question recorded above as an owner call.
- **L3** descriptions, `get_info` and `AGENTS.md` ask agents to cite by id;
  goldens and both mcp.mdx copies updated.
- **L4** the commit-hash prefix case documented and tested.

## Open questions for the owner

1. **Durable names** (follow-up issue): a synonym `Mutation` in every
   adapter and a decision on S5.
2. **#19(b) and `plan_still_holds`**: see "Pinned at the ack" above.
3. **Image derives** take no citations (`lambo_derive_image` is unchanged).
