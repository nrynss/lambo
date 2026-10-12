# #36 PR 4: citations, names and the shared resolve-or-refuse resolver

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
  rests on, at most 8 per concept and 32 per call) and `name` (a short name
  later references can use, at most 256 bytes). A citation is written as
  `citing -Dependency-> cited`, the edge `record_action`'s `depends_on`
  writes, so the cited concept gains a dependent under PR 1's relation.
- `lambo_record_action` `depends_on` entries that are an existing concept's
  full node id or recall's short id resolve to it; an unknown full id, an
  interaction id or a shared short id is refused. Every other entry keeps
  the 0.5.0 rule (canonical match, names included, else create).
- `lambo derive --depends-on <REF> --name <NAME>` for the first concept.
- Library: `Memory::derive_cited_as`, `derive_cited_for_ingest_as`,
  `derive_cited_async_as`; `ConceptCitations`, `PinnedCitations`,
  `PinnedReference`, `LamboError::Reference(Box<ReferenceRefusal>)` with
  `ReferenceRefusalKind`; `graph::cite` (limits, pre-pass, commit step).

## Decisions

### The resolver lives in `crate::surface::resolve`, beside `focus`

`resolve_reference(&Graph, &str) -> Resolved { Concept { node, leg },
Missing, Ambiguous, NotAConcept }`. Legs: a full node id (exclusive: content
never parses as a UUID), a short id (8 to 31 hex characters, a unique
prefix), a name (a raw synonym, exactly as derive's canonicalization looks
it up) and the canonical key (`graph::canonical::canonicalize`). The
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
`canonicalize`, so NFC, invisible-character stripping, case folding, word
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
commit: a name declared between the ack and the commit can make the citing
content canonical-match the cited concept, and that is refused as
`SelfReference` (`the_commit_refuses_self_and_cycle_it_could_not_see_at_the_ack`).
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

**`record_action` pins only id entries.** An entry resolved by id is
pinned, and one that read as an id but named nothing at the ack (to be
created) is pinned as `None`, so it is refused if it comes to name a
concept before the apply. A text entry keeps the 0.5.0 behaviour, races
included (a concept removed in between is created again, as before).

### A call cannot cite what it creates

References resolve against the graph as it is; a concept the same call
derives does not exist yet, so citing it is `Missing`. This keeps the
pre-pass and the commit on the same answer and keeps "never create the
target" literal. It costs nothing for canonization: an own-interaction
dependent never counted (PR 1). Derive the ruling, then cite it.

### Names are synonyms, so they are not durable

`name` calls `Graph::declare_synonym(name, content-of-the-named-concept)`
after the transaction commits (synonyms are outside its undo log). A
synonym is what derive's canonicalization and `record_action`'s text rule
already consult, so a named ruling takes later citations of the name, and
a later concept with exactly that text matches the ruling instead of
minting an alias Entity (#10's growth). Synonyms are RAM-local by the
pinned S5 contract (`store::load`'s test asserts nobody "fixes" that by
inventing a mutation), so **a name is lost when the writer restarts**. It
fails closed: a citation by a forgotten name is `Missing`, a replayed intent
that cited one settles `failed` with `Changed { now: None }`, and nothing is
ever linked elsewhere. The wire, CLI and docs say so. Making names durable
needs a `Mutation` kind for synonyms in every adapter and an owner decision
against S5; it is listed under open questions.

A name that already resolves to another concept, an existing alias Entity
included, is `NameTaken`, never taken over: a synonym shadows a key, so
taking over "writer lease" would silently redirect every later derive of
that text. Redirecting existing aliases is the reviewed backfill's job
(#36 PR 5). Also refused: an id-shaped name, a name with no words after
stopwords, naming an Observation (canonical matching never matches one), a
name whose words are another concept of the same call, one name for two
concepts, two names for one concept.

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

8 references per concept and 32 per call (the design's numbers), a name at
most 256 bytes, every reference within the 16 KB string cap. The MCP
surface and the CLI refuse them as bad parameters before the core; the core
refuses them too (`ReferenceRefusalKind::Limit`) for library callers. The
schema publishes `maxItems: 8` and `maxLength: 256`.

## Spec errata

**§6.1 / §6.2 `lambo_derive`.** Each concept may carry `depends_on` and
`name` as above. `depends_on` writes `Dependency` edges from the concept to
each cited concept, subject to §5.7's write-time acyclicity over
`Causal`/`Dependency`, which now covers derive as well as `record_action`.

**§7 `record_action`.** A `depends_on` entry is first resolved as a
reference (full node id, unique short id); only an entry that is not one
goes through canonicalize-or-create.

**§7.1 step 4.** Synonyms gain a writer: a derive's `name`. They stay
RAM-local (S5).

## Fair-test fixture

Re-run: every expected status is unchanged, `lease_alias` included. The
fixture's three alias citations are edges already written to the alias
Entity; PR 4 changes what future writes do (a named ruling takes them, and
no alias is minted) and deliberately rewrites nothing, and it refuses to
give the ruling a name the alias holds. So the row stays Venerable and its
comment in `fair_test::expected` now says that only the reviewed backfill
(PR 5) flips it. The forward behaviour is pinned on a fresh graph instead
(`a_named_ruling_takes_later_citations_and_an_existing_alias_is_not_taken_over`).

## Tests

Design list:

| Requirement | Test |
|---|---|
| ID, short ID and synonym resolve the same node | `every_leg_reaches_the_same_concept` (full, simple, short, upper-case short, longer prefix, name, content in other case, word order and Unicode composition), `every_reference_form_writes_the_same_one_edge` |
| Missing, ambiguous, foreign, self, cyclic refuse with typed errors before the ack | `every_bad_reference_is_refused_with_its_kind_and_nothing_is_written`, `nothing_resolves_to_missing_and_an_interaction_id_is_not_a_concept`, `a_shared_prefix_is_ambiguous_until_it_is_long_enough`, `legs_that_disagree_are_ambiguous`, `a_refused_reference_is_an_error_before_the_ack_and_writes_nothing`, `a_bad_reference_is_refused_before_the_ack_with_its_reason` (MCP), `a_reference_held_elsewhere_is_refused_as_foreign` |
| Size and count limits | `every_bad_reference_...` (9 on one concept), `the_call_wide_reference_limit_is_enforced`, `the_surface_limits_are_bad_params` (MCP) |
| Natural-key idempotency | `a_resent_citation_reinforces_one_edge_and_one_name` |
| Pre-pass and background agree | `the_sync_and_async_paths_refuse_the_same_references`, `the_commit_and_the_pre_pass_refuse_the_same_kinds`, `an_accepted_citation_applies_in_the_background_under_both_strategies` |
| Graph before hot list; no lock across an await | `cited_writes_recalls_and_the_daemon_run_together_without_deadlock`, `the_foreign_probe_runs_with_this_sessions_lock_released`; the `Send` bound and clippy's `await_holding_lock` (above) |
| Legacy requests unchanged | `a_derive_without_citations_is_unchanged`, `legacy_payloads_are_byte_identical`, `record_action_resolves_ids_first_and_keeps_its_legacy_text_rule` (text and commit-hash entries), every pre-existing test (none changed except the schema goldens) |
| Tool mirrors and schema goldens | `tool_schemas.golden.json` (`lambo_derive`, `lambo_record_action`), `f18_tool_schemas_match_the_golden_property_set`, `published_schemas_carry_runtime_maxima` |

Also asked for:

| Requirement | Test |
|---|---|
| Cross-session refusal | `a_citation_of_a_layer_concept_is_refused_as_cross_session` (MCP, layered: short id, full id and content, derive and record_action, neither session written) |
| Replay re-resolution | `a_replayed_citation_re_resolves_and_settles_applied_or_failed` (kept target applies after restart; removed target and forgotten name settle `failed`) |
| `plan_still_holds` behaviour | `a_cited_concept_removed_between_the_ack_and_the_commit_fails_the_receipt` (hybrid, removal while the embed is in flight), `a_reference_that_changes_after_the_ack_refuses_at_the_commit` (removed, renamed, shadowed; graph, epoch and log unchanged), `the_commit_refuses_self_and_cycle_it_could_not_see_at_the_ack` |
| Fuzzy inspect never authorizes a write | `a_substring_inspect_resolves_never_authorizes_a_write`, `inspect_and_writes_share_the_short_id_leg` |

Plus `record_action_pins_id_entries_and_refuses_a_changed_one`,
`a_named_ruling_takes_later_citations_and_an_existing_alias_is_not_taken_over`,
`a_citation_makes_the_cited_concept_load_bearing`,
`a_cited_derive_and_a_named_action_land_on_the_ruling` (MCP, with inspect's
dependents), `a_cited_derive_is_its_own_variant_and_round_trips`,
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
No store, migration or SQL changed: intents are serialized JSON, and
`Dependency` edges are an existing type. No `Cargo.toml`, `Cargo.lock`,
workflow or `src/store/pg/postgres/tests.rs` change was needed.

One pre-existing doc error fixed in its own commit: `graph::graph::keys`
said a key could hold two concepts for a loaded snapshot carrying a
duplicate. `Graph::from_snapshot` runs `assert_invariants`, which refuses
that, so the list and the lowest-id pick are defence in depth.

## Open questions for the owner

1. **Durable names.** Names die with the writer (S5). Persisting them needs
   a synonym `Mutation` in every adapter, which S5 forbids without a
   decision. Until then the rollout should cite by id.
2. **Rollout.** Agents start citing only when told to: the tool description
   and docs describe `depends_on`, but `get_info` and the agent rules
   (`AGENTS.md`'s dogfood section) do not yet ask for it. The design keeps
   rollout an operator action; the dogfood rules are the obvious place.
3. **Image derives** take no citations (`lambo_derive_image` is unchanged).
