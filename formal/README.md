# Lean 4 proofs about Lambo

This directory holds a Lean 4 development that models parts of the Lambo engine
and proves properties the source states in its doc comments and pins in its
tests. It has no dependencies beyond Lean itself (no Mathlib).

See [Formal verification with Lean](../docs/reference/formal-verification.mdx)
to understand the modeled guarantees and run the local checks.

## Status: machine-checked

`lake build` passes on Lean `v4.36.0` with no errors, no warnings and no
`sorry`, and the axiom audit in `LamboProofs/Audit.lean` reports only Lean's
standard axioms (`propext`, `Quot.sound`, and `Classical.choice` for 343
theorems); 33 of the 814 audited theorems use no axioms at all.

What the models prove is in the table below, file by file, led by each model's
end results: the theorems that state a property of the Rust, or, for a
finding, show where the Rust fell short. Each is named, so it can be read in
its file. Most of a file's theorems are the lemmas those rest on, so how many
theorems a file has says little about what it proves.[^count]
`formal/audit/unreferenced-theorems.md` lists the theorems nothing references:
no other proof uses or names it, and no `example`, docs anchor, claims ledger
row, Rust comment or `#print axioms` line in `Audit.lean` cites it. Each is
sorted into keep, deletion candidate or unclear, by the criteria at the top of
the report.

To check them yourself:

```bash
curl https://elan.lean-lang.org/elan-init.sh -sSf | sh
```

```bash
cd formal && lake build --wfail
```

`lean-toolchain` pins the Lean version, and elan fetches it on first use.

CI checks them in `.github/workflows/lean.yml` on every push or pull request
that touches `formal/`. It downloads the pinned Lean release, verifies its
SHA-256, builds with warnings as errors, fails on any `sorry`, and re-runs the
axiom audit, failing on any axiom other than the three standard ones. The
build covers every file under `LamboProofs/` (`globs` in `lakefile.toml`), so a
model nothing imports is still checked. It also regenerates the differential
oracle's vectors and fails if `formal/oracle/` is stale (see "Differential
oracle" below). It keeps
`formal/.lake` in the Actions cache (saved from `main` only), so an unchanged
module is not rebuilt. Besides the checkout pin `ci.yml` already uses, the only
action is `actions/cache`, pinned by commit.

[^count]: For the record: 2,368 theorems across 59 model files, and 18 shared
    lemmas about lists in `LamboProofs/Lib/List.lean`. The count is of `theorem`
    declarations in the model files (every `.lean` file under `LamboProofs/`,
    subdirectories included, other than `Audit.lean` and the shared lemmas under
    `Lib/`; a `Spec` file is one and holds no theorem), and it comes from `python3 -I scripts/formal/count-theorems.py`,
    which prints it per file and in all. A declaration is a line outside every
    comment (line comments and nested block comments, doc comments included) and
    string literal that starts at column 0 with `theorem`, `private theorem` or
    `protected theorem`, optionally after attributes such as `@[simp]`, followed
    by the theorem's name; its binders and colon may be on that line or the next
    ones, so a line holding only `theorem name` counts. Prose in doc comments that
    happens to start with "theorem" is not counted, and neither are `example`s.
    `scripts/formal/test_count_theorems.py` tests the rule. Before the script the
    count was taken by hand: this README said 2,393 for the files the script
    counts as 2,394 (at `8ee90cd3`).

## How the models are written

The models are Lambo's algorithm and its specification: the definitions are
the algorithm, as the Rust runs it, and the theorems are the guarantees. Every
theorem earns its place in one of two ways. Either a proof uses it (a stepping
stone is fine), or it states a property of the modelled Rust and its doc
comment says, in one sentence, what it guarantees about which Rust behaviour:
a function, a receipt answer, an order, an invariant. A doc comment that only
restates the Lean in words is not enough; a theorem that cannot be given that
sentence truthfully is deleted, not documented into place
(`formal/audit/deleted-theorems.tsv` lists the deletions and why).

Every model file opens with a header that says what it models, which Rust it
covers, its end results and its boundary: the Rust behaviour it does not cover
(concurrency beyond the modelled lock order, store and SQL behaviour, embedder
internals, panics where they are not modelled, ...), so that silence is not
read as a guarantee. Prose that only narrates the Lean is left out.

CI enforces what a machine can check: `python3 -I
scripts/formal/audit-theorems.py --check` fails when a theorem is used by no
proof (and named by no other declaration, as an `rfl` lemma handed to `simp`
is) and carries no doc comment, and when a theorem nothing references (no
proof, example, docs anchor, claims ledger row, Rust comment or `Audit.lean`
line) has no verdict in `formal/audit/reviewed-theorems.tsv`. A person gives
that verdict after reading the statement, the doc comment and the Rust it
names: `keep`, `narrowed` (the doc comment was narrowed to what the statement
proves) or `deleted`, with a one-line reason. A new unreferenced theorem fails
CI until it is reviewed and added there. A `@[simp]` lemma no proof names
counts as used when a build probe showed a proof needs it
(`formal/audit/simp-used.tsv`). That probe was run once; a listed lemma that
later becomes unneeded stays exempt until someone re-probes it: remove the
lemma from a copy of its model, elaborate the copy with `lake env lean` (not
`lake build`, whose cache can hide the change), and drop the row when the copy
still elaborates with no error or warning.

## Reading a Spec file

A model in Spec and Proofs form is two layers. Its `Spec` file holds the definitions, which are the algorithm, and each result as a named proposition with no proof. Its `Proofs` file proves each proposition, as the theorem of its name, beside the lemmas the proofs need. So what a model says can be read without its proofs. The drain model (`Drain/`) is the first in this form (#350): its specification is four files of about 1,400 lines, comments included, against about 3,200 lines of proofs. Every other model still states each theorem in full beside its proof; they are to follow one at a time.

A result that read

```lean
/-- The third transient in a row with nothing answered between stops the
replay as `embedder` (`EMBEDDER_SICK_THRESHOLD`). -/
theorem unavailable_stops (img : Bool) (s : Streaks) (h : sickThreshold ≤ s.transient + 1) :
    (replayStep img (.run (.err .unavailable)) s).1 = .stop .embedder := by
  ...
```

is now, in `Drain/Spec/Base.lean` and `Drain/Proofs/Base.lean`:

```lean
/-- The third transient in a row with nothing answered between stops the
replay as `embedder` (`EMBEDDER_SICK_THRESHOLD`). -/
abbrev Spec.unavailable_stops : Prop :=
  ∀ (img : Bool) (s : Streaks),
    sickThreshold ≤ s.transient + 1 →
    (replayStep img (.run (.err .unavailable)) s).1 = .stop .embedder
```

```lean
theorem unavailable_stops : Spec.unavailable_stops := by
  intro img s h
  ...
```

The layout, for a model `M` in the namespace `Lambo.M`: the specification is `LamboProofs/M/Spec.lean`, or one file per subsystem under `LamboProofs/M/Spec/`, and the proofs are `LamboProofs/M/Proofs.lean` or the files of the same names under `LamboProofs/M/Proofs/`. Definitions and theorems keep their names in `Lambo.M`; the proposition of `Lambo.M.name` is `Lambo.M.Spec.name`. A history layer or an examples file (`Drain/History.lean`, `Drain/Examples.lean`) is in neither layer and is not specification.

To read a `Spec` file:

* The header says what is modelled, which Rust it covers, its results and its boundary. What the boundary leaves out, the model says nothing about: silence is not a guarantee.
* A `def`, `structure` or `inductive` models the Rust item its doc comment names. A definition often builds on another (`closeD` on `closeW`, `stepQ` on `stepFixed`); a result over the inner one says so in the header's boundary.
* An `abbrev Spec.name : Prop` is a result. Its doc comment says what it guarantees about which Rust behaviour, and its body is the exact statement: the variables it quantifies over, each hypothesis followed by `→`, then the conclusion. It is an `abbrev` so that `simp`, `rw` and `decide` see through it.
* A proposition holds because `theorem name : Spec.name` in the `Proofs` file passes `lake build`. `audit-theorems.py --check` fails on a proposition with no such theorem, on a theorem stated as another name's proposition, on a theorem in a `Spec` file, on a proposition declared in a `Proofs` file, and on a `Proofs` lemma that no proof uses (it must be a `Spec` proposition, if it is a guarantee about the Rust, or go).

A `Spec` proposition exists only for a guarantee about the current Rust. Everything else is a `Proofs` lemma, a `History` or `Examples` entry, or deleted: a theorem a proof uses is a `Proofs` lemma, stated in full, even when a docs page once cited it, but only if a kept result's statement entails every sentence that cited it (the citation then moves to that result); otherwise it keeps its `Spec` proposition (`replayP_preserves`, which `sessionW_then_serve_order` uses); a finding about the Rust before a fix, or about a rule not taken, is an `Examples.lean` entry, and a result over a layer the Rust has left behind a `History.lean` one, each stated in full beside the evidence it explains; and a theorem that is none of these (an `rfl` unfolding of a definition's arm, the model compared with itself) is deleted and listed in `formal/audit/deleted-theorems.tsv`. A definition only the proofs use is in the `Proofs` file (`workerStep`, `InvW`), one only examples or history use in theirs.

The `Spec` files are short on purpose. A header is about 40 lines: what is modelled, the Rust items of that file, the results one line each, and the boundary. A section header is a title, with at most two lines when a modelling choice needs them. A definition's doc comment names its Rust item in at most three lines; a proposition's says its guarantee in one sentence, plus the Rust test where there is one, and no more than its statement. No history (before or since an issue, commit hashes, what review found): that goes beside the theorem it explains in `History.lean` or `Examples.lean`, or stays in the CHANGELOG and git. A `Proofs` file's header is one line, `Proofs of Drain/Spec/X.lean.`, and its section headings carry no history.

A docs anchor cites the proposition, `{/* lean: Drain.Spec.runW_ord */}`, or a definition; `scripts/docs/check-doc-anchors.py` refuses a theorem or a lemma of the proofs layer. A model's entry in `formal/models.toml` is its `Spec` file's, and the `Proofs` file has none. Putting a model into this form must leave every statement as it was, which `check-decl-equivalence.py --spec-form` checks (see "Restructuring the models without changing what they prove").

## What is modelled, and what each file proves

Every file opens with a comment that says which Rust items it models, what the
model leaves out, and which doc-comment sentence or test each theorem
corresponds to. Each row leads with the file's end results, by theorem name;
the lemmas they rest on are in the files and not listed here.

| File | End results | Rust source |
| --- | --- | --- |
| `Score.lean` | The composite is in `[0, 1 + MAX_BONUS]` for every input and weight set (`score_range`). With weights summing to at most 1 the upper clamp never fires (`rawScore_le`). The score is monotone in every dimension (`score_mono`). GC's eviction composite equals the live score at zero frequency and never falls below it (`livePlusFrequency_of_frequency_zero`, `live_le_livePlusFrequency`). The two recency formulas agree in exact arithmetic (`historical_eq_floor`). Max-normalized dimensions are at most 1 (`le_listMax`). | `src/daemon/score.rs`, `ScoringWeights::sanitized` |
| `Order.lean` | `then_with` of lawful comparators is lawful (`lawful_lex`). The three ranking comparators are strict total orders (`rankMembers_lawful`, `rankMembers_separating`). A sorted list is determined by its set of members, so the output does not depend on input order (`sorted_unique`). Canonical members order first whatever the scores (`rankMembers_canonical_first`). | `tie_break_by_key`, `rescore`, `rank` (candidates), `rank_members` |
| `Expand.lean` | The expansion is exactly the set of nodes within `depth` traversable edges of a candidate (`expand_sound`, `expand_complete`), each recorded once on any graph, cyclic or not (`expand_nodup`), candidates first in input order (`expand_prefix`). | `expand` in `src/recall/expand.rs` |
| `TopK.lean` | Forced members are never cut and take no slot (`forced_mem_emit`, `emit_free_le_topK`). The ordinary hits are exactly the first `top_k` ordinary concepts in rank order (`emit_free_eq_take`). Emission keeps rank order (`emit_sublist`). With the redundancy skip: order and forced members are kept (`emitR_sublist`, `forced_mem_emitR`), skipped members take no slot (`emitR_free_count`), at most `window top_k` are skipped (`skipsR_length_le`), no redundant pair means the no-skip result (`emitR_eq_emit_of_no_redundant_pair`, `emitR_eq_emit_of_no_redundant_distinct_pair`), the copy ranked first is kept (`skipsR_pairwise`), and a skipped Canonical member is skipped for a Canonical copy, whose ⚑ banner is shown instead of its own (`skipped_canonical_kept_canonical`). | `emitted` in `src/recall/assemble.rs`, with the #181 rule of `src/recall/redundancy.rs` |
| `Fit.lean` | Every accepted prefix fits the budget for any estimator (`fit_prefix_ok`). The cut stops at the first block that does not fit (`fit_stops_at_first_miss`). With a monotone estimator it is the longest fitting prefix (`fit_longest`). With the oversize-first-block truncation (#130), whatever is rendered fits the budget for any estimator (`fitBlocks_fits`), a first block that fits gives exactly the whole-block result (`fitBlocks_of_first_fits`), the context is non-empty whenever there is a block and the marker alone fits (`fitBlocks_context_ne_nil`), and a truncated context is a content prefix, `…`, a prefix of the label and warnings, and a marker counting exactly the text left out (`fitBlocks_truncated_prefix`). For the default and byte estimators with the real marker the head is the longest that fits and the first fitting form wins (`rust_default_steady`, `rust_default_longest`); that does not hold for every estimator monotone in bytes (`rust_jump_skips_a_fitting_form`, on a real block), while rule 3 does, on blocks of the Rust's sizes (`rust_rule3_of_mono`). | `fit_blocks`, `truncate_first`, `content_cuts` in `src/recall/format/fit.rs` |
| `Canon.lean` | `Canonical → None` is the only demotion (`legal_demotion_unique`). Canonical is entered only from Venerable (`canonical_only_from_venerable`). A cycle moves a concept along at most one edge (`cycle_one_hop`). Stage 3 cannot overflow the budget (`stage3_no_overflow`). | `legal_canonization_transition`, `apply` in `src/canon/eval.rs`, `stage{1,2,3}.rs` |
| `Gc.lean` | Protected concepts, in particular Venerable and Canonical ones, are never collected (`venerable_canonical_never_collected`). The score cut removes only Entities and Resources (`score_cut_only_entities_and_resources`). `gc_survived` never decreases (`bump_never_resets`). An idle session never sweeps on time alone (`idle_never_sweeps`). | `gc::run`, `bump_gc_survived`, `sweep_due` |
| `Cite.lean` | A write that passes the cycle check leaves the `Causal`/`Dependency` graph acyclic (`batch_acyclic`). Each write gate, the alias redirect and edge removal keep both load conditions, `Causal`/`Dependency` acyclic and `Hierarchical` acyclic (`cite_gate_preserves`, `parent_gate_preserves`, `redirect_preserves`, `drop_preserves`), so every written session loads (`written_loadOk`). Before #325 the load check walked their union and refused a child citing its own parent, a session the gates write (`union_check_rejects_written`). The new check accepts everything the old one did (`loadOk_of_loadOkUnion`) and still refuses a pure cycle of either kind (`hierarchical_cycle_not_loadOk`, `dependency_cycle_not_loadOk`). | `closing_edge` in `src/graph/action.rs`, `src/graph/cite.rs`; `reject_closing_edge` in `src/graph/derive/cycles.rs`; the alias redirect in `src/canon/alias.rs`; `Graph::assert_invariants`'s cycle check |
| `Names.lean` | A name binding is permanent (`names_permanent`) and never resolves to a different concept, even after its target is collected (`resolve_never_rebinds`). | `src/graph/graph/names.rs`, `resolve_reference_scoped` |
| `Lease.lean` | The fencing token never decreases (`token_mono`). A live lease refuses other identities (`acquire_refused_while_live`). Once another identity holds the lease, the previous holder's token is refused by the fence forever (`stale_token_refused`, `later_holder_excludes_earlier`). The heartbeat since #293 latches the fence exactly when the row has left the holder's generation, so the first beat after any takeover latches, the operator-override re-take included (`beat_latches_iff`, `override_beat_latches`). | `src/store/lease.rs`, lease upsert in `src/store/memory.rs`, the heartbeat's fence latch in `src/memory/leases.rs` |
| `Receipts.lean` | A settled outcome is never overwritten (`settleAll_fixed`). Unsettled receipts are never expired or evicted (`expire_keeps_unsettled`, `evict_keeps_unsettled`). A lane applies writes in submission order (`lane_fifo`). Admission keeps lanes and the queue within their caps. | `src/writeq/receipts.rs`, `admission.rs` |
| `Reserve.lean` | A live reservation excludes other agents (`reserve_denied`). Takeover happens only at or after expiry (`takeover_needs_expiry`). | `src/graph/reserve.rs` |
| `Contract/Stamp.lean` | Compatibility is equality of all three fields (`compatible_iff_eq`). A stamped session's contract cannot be changed by ordinary stamps (`stampAll_keeps`), and every stamp that succeeds on a stamped session offered that very contract (`stamp_ok_eq`); only an unstamped session is a wildcard (`classify_unrecorded_iff`), and the width check is store-authoritative (`vector_check_store_authoritative`). | `EmbeddingContract`, `stamp_embedding`, `session_embedding_compatibility`, `check_vector_compatibility` |
| `Contract/Reembed.lean` | No ordinary write changes a stamped contract, override or not (`runAll_ordinary_keeps`); it changes only by a same-width override attach, a same-width re-embed, or a re-stamp of a session with no vectors (`contract_change_explicit`). Without the override the attach is exactly `stamp` (`attach_no_override_ok_iff`), and no read ever returns a vector from another contract (`no_mixing_without_override`). The override admits exactly a same-width mismatch with no vectors or an unchanged kind (`attach_override_ok_iff`) and, in the rename-over-vectors case, leaves two labels behind one contract (`attach_override_relabel_mixes`). A successful re-embed leaves every vector in the new space (`reembed_one_space`). Before #296 it could not rewrite a session already under the live contract, the canary-drift case (`reembedCmd_refuses_same_contract`), whose only repair was a same-width rename or kind hop (`example_rename_hop`); a full re-embed re-pinned the canary without the other-sessions warning (`reembed_repins_without_warning`), and `re-embed --missing-only` skipped the canary (`missingOnly_skips_canary`). The fixed verb (#296) repairs a drift in place with `--rewrite-same-contract` and leaves every vector in the live space (`reembedCmdFixed_repairs_drift`, `reembedCmdFixed_full_spec`). It runs the canary on `--missing-only` (`missingOnly_fixed_refuses_drift`), re-pins over an unconfirmed pin only with `--repin-canary` and then warns (`fixed_repin_unsafe_needs_optin`); the `--missing-only` gate only creates a missing pin, and an unconfirmed pin changes only through an opted-in full run (`pinAfterFixed_no_silent_replace`); and it adds no way to move the contract (`runAll2_keeps`, `contract_change_explicit2`, `runAll2_oneSpace`). | the writer attach's contract check and `--allow-embedding-mismatch` (`build_attach`), `reembed_all`, `embed_missing`, `lambo re-embed`, `quarantine_legacy_vectors`, the checked vector read, `canary_gate` |
| `Contract/Store.lean` | Before #322, on a contract-less session the re-embed reported full coverage while the store's first-stamp quarantine erased every new vector (`reembed_contractless_durable_loses_vectors`). With the contract stamped first, the store keeps every new vector and equals the graph (`reembedCmd322_contractless_durable`, `reembedOrdered_contractless_matches_graph`), a stamped session is unchanged (`reembedCmd322_stamped_agrees`), and `--missing-only` on a non-empty contract-less session is refused before anything is written (`missingOnly_contractless_writes_nothing`). | the store applying a flushed batch (`MemoryStore::flush`, `plan_flush`, `write_rows.rs`), with the first-stamp legacy quarantine; `Graph::reembed`'s order |
| `Fusion/Max.lean` | Max fusion is monotone in each leg (`merged_mono`). Assigning `1.0` to the named concept's keyword leg can drop it below a weaker lexical neighbour (`assign_lowers_named_concept`, the #288 example). Raising the capped slot instead (#288's first fix) gives a keyword leg of at least `1.0` and at least its BM25 within the over-fetch cap (`raise_keyword_ge_one`, `raise_keyword_ge_bm25`), leaves every other node's leg slots unchanged (`raise_local`), never lowers its fused score or its rank (`raise_no_demotion`, `raise_ahead_le`), and agrees with assigning whenever its BM25 is at most `1.0` or absent (`raise_eq_assign`); a hit beyond the cap still gets only `1.0` (`beyond_cap_floor_only`). The step Rust runs now reads the named concept's BM25 off the whole ranked list: its keyword leg is exactly `max(BM25, 1.0)`, or `1.0` with no hit, wherever the hit lies (`full_keyword_eq`, `full_keyword_ge_bm25`), other nodes' leg slots are unchanged (`full_local`), so an unnamed node beyond the cap stays out of the leg (with `lastScore_of_not_mem`; shown concretely by `full_keeps_beyond_cap_bm25`), its rank cannot get worse against either earlier table (`full_ahead_le`, `full_ahead_le_raise`), it agrees with the capped raise when the hit is within the cap or absent (`full_eq_raise`), and it keeps a beyond-cap BM25 the capped raise dropped (`full_keeps_beyond_cap_bm25`). | phase 1 in `candidates_with_keyword_leg_as`, `merge_max`, `LegScores::merged` (`src/recall/candidates.rs`) |
| `Fusion/Rrf.lean` | Under RRF (#173, exact rationals), `rrf` is in `[0, 1]` (`rrf_range`) and monotone in each leg's rank (`rrfSum_mono`); leg ranks follow the leg's score order and tie-break and ignore input order (`legRank_lt_iff`, `legRank_perm`, `rrf_perm`); with one weighted leg RRF orders as that leg does (`rrf_one_leg_agrees`). The name step never lowers the named concept's `rrf` or position (`rrf_full_named_up`, `rrf_full_ahead_le`); it moves another node's keyword rank by at most one place, exactly when the named concept passes it, and only from the `1.0` floor (`rrf_full_other`, `full_passes_only_below_floor`), so it can reorder two unnamed nodes (`rrf_name_step_reorders_unnamed`). The beyond-cap fix changes no rank or `rrf` against the capped raise (`full_rank_eq_raise`, `full_rrf_eq_raise`). | the RRF half of `rank` and `recent_concepts` (`src/recall/candidates.rs`), `RecallFusion` (`src/recall/fusion.rs`) |
| `Confidence.lean` | The value is in `[0, 1]` for every input (`provisional_inUnit`); the keyword term is `1/2` at `KEYWORD_HALF_SCORE` and tends to `1` (`keywordTerm_half`, `keywordTerm_tendsto_one`). The value is monotone in BM25 and in the cosine (`provisional_mono_bm25`, `provisional_mono_cosine`); the noisy-OR is never below either term (`keywordOnly_le_noisyOr`, `noisyOr_ge_vector`) and keyword-only is the two-leg value at `b = 0` (`keywordOnly_le_noisyOr`), these last in exact arithmetic. The keyword-only basis applies exactly when `vector_cosine` is `None` (`provisional_basis_iff`); with both legs and no median the value is absent (`provisional_twoLeg_without_median`); `exact_name_match` never changes it (`provisional_exactName`). `top_legs.bm25` is the raw best BM25, untouched by the cap loop and the exact-name step (#288; `topLegs_keywordPhase`, `keywordLeg_bm25_best`). All in exact arithmetic over any scale, non-finite `f64` included. | `src/recall/confidence.rs` (`top_legs`, `keyword_term`, `noisy_or`, `provisional`), `KeywordLeg` in `candidates.rs`, `null_median_cosine` |
| `Canonical.lean` | For any stopword list and stemmer, the key ignores token order (`key_perm`), treats `-`, `_` and whitespace alike (`key_sep_alike`), ignores case except across a camelCase boundary (`key_lower`, `camel_not_lower`) and ignores stopwords (`key_stop_segment`); a stopword-only input has the empty key and `name_key` refuses it (`key_all_stop`, `nameKey_all_stop`). Two inputs share a key exactly when their stems form the same multiset (`key_eq_iff_perm`), and two valid names collide exactly then (`nameKey_eq_iff_perm`; the #290 examples `issue290_order`, `issue290_stopword`). The key is idempotent for a stemmer with fixed, lowercase, non-stopword outputs (`key_idem`); the real Snowball stemmer is not (`leas_not_idempotent`, `ons_not_idempotent`), and its empty stems gave keys with stray spaces (`empty_stem_key`). The #301 fix drops empty stems (`keyF`): for any stemmer that keeps separators out of its stems, a key then never has a leading, trailing or doubled space (`keyF_tidy`, `canonicalKeyF_tidy`), agrees with the old key wherever no stem is empty (`keyF_eq_key`, `nameKeyF_eq_nameKey`), is empty and refused as a name when every stem is empty (`keyF_all_empty`, `nameKeyF_all_empty`), and two keys are equal exactly when the nonempty stems form the same multiset (`keyF_eq_iff_perm`); the issue's inputs are checked by `decide` (`issue301_fixed`). PostgreSQL indexes a fixed-width digest of the full key per session (`pgIndexKey_width`, `pgIndexKey_same_key`). | `canonical_key`, `normalize_tokens` in `src/graph/canonical.rs`, `name_key`; the #301 fix (`normalize_tokens` drops empty stems) |
| `Dispatch.lean` | Bare "depend" is no marker (`depend_not_marker`). Before #299 a query was structural exactly when its lowercase text contained a marker (`classify_iff`), a substring test, so a glued marker ("independents of", "depends only on") routed structural (`classify_inside_word`, `classify_into_word`); the fixed classifier counts a marker only on word boundaries (`classifyWB_iff`, `wb_inside_word`, `wb_into_word`, `wb_hyphen_before`, `wb_hyphen_after`, `issue_queries_general`), every marker as whole words still classifies structural (`classifyWB_marker`), the fix only narrows (`classifyWB_structural`), and the dispatch theorems carry over to it (`traversal_exact_wb`, `fitsStructuralWB_iff`, `recall_blend_input_wb`). Only `Dependency` and `Hierarchical` edges declare a dependence (`dependence_types`). The dependents are exactly the anchor's direct structural dependents at any `traversal_depth` (`mem_ranked`, `traversal_exact`), in Canonical-first order (`ranked_canonical_first`), at most `top_k` of them (`traversal_length_le`). The gather-skip pre-check holds exactly when the dispatch answers (`fitsStructuralWith_iff`), and across concurrent writes a recall that blends always blends on the gathered input (`recall_blend_input`). | `classify`, `classifyWB`, `dependents`, `try_structural_with`, `fits_structural_with` in `src/recall/dispatch.rs`; `dependents_of`; the structural routing in `Daemon::recall_routed` |
| `Supersession/Suspected.lean` | #198: a suspected pair runs from the strictly later-dated concept to the earlier one and a tie records nothing (`candidate_dir`, `candidate_tie`), so the relation is acyclic (`suspected_acyclic`); a candidate marked `newer` is never one the new concept replaces, on any graph (`select_newer_matches_trigger`, `select_newer_never_replaced_by_new`). #302's fix keeps a repeated id's highest score (`keepF`, `selectF`): the kept list is exactly the qualifying concepts, each at its best score (`keepF_best`), so the candidates do not depend on the order or repetition of the store's hits (`selectF_order_independent`), and the select theorems carry over (`selectF_mem`, `selectF_newer_never_replaced_by_new`); the old `select` kept the first score. | `src/graph/suspected.rs`, `suspected/slot.rs`, `suspected/replacement.rs` |
| `Supersession/Pass.lean` | #20: a verdict needs the source's counted evidence strictly after the target's watermark (`verdict_sound`), never runs both ways (`pair_rule`) and demotes the side with the older evidence (`verdict_outranks_reverse`); the pass moves only Canonical → None through the gate (`applySup_unique`), equals `Canon.cycle`'s supersession pass (`applySup_eq_cycle`), and with `canonization_supersede` off changes nothing (`off_agrees`). A suspected edge never demotes (`probesFor_ignore_suspected`). | `src/canon/supersession.rs`, `src/store/evidence.rs`, `apply` in `src/canon/eval.rs` |
| `Supersession/Recall.lean` | A suspected pair is never redundant in recall and neither end is skipped (`signal_suspected`, `suspected_ends_not_skipped`); the "possibly superseded" warning is on the older concept and names the newest shown newer one (`pss_sound`, `pss_newest`); the #20 history line is never on a Canonical concept (`supersededLine_canonical`). | `never_redundant`, `Redundancy::signal`, `possibly_superseded_by`, `superseded_by` and the line gate in `src/recall/` |
| `Policy/Sessions.lean` | The greedy session count is order-free and ignores exact repeats (`sessionCount_perm`, `sessionCount_dup`), and it is the maximum number of pairwise `gap`-separated starts, exact rather than a lower bound (`sessionCount_max`, `sessionCount_attained`); adding a start raises it by at most one and a wider gap never counts more (`sessionCount_cons_le`, `sessionCount_antitone_gap`); the Rust test `separation_counts_clusters_once_and_stragglers_each`. | `separated_session_count` (`src/canon/event_time.rs`), `SESSION_SEPARATION` |
| `Policy/Support.lean` | Only `Derives` and `Dependency` edges add recurrence, and no edge lowers it (`causal_not_recurrence`, `recurrence_mono_edge`); with event times flush times are irrelevant (`recurrence_reflush`); the Rust tests' corpora (`a_bulk_ingest_recurs_only_under_event_time`, `recurring_burst_once`). Since #300 a dependence is dated by the turn that wrote it (`reused_action_dependency_is_dated_by_its_writer`, `own_origin_is_the_writing_turn`), agreeing with the old dating whenever every dependence was written in its dependent's origin turn (`recurrenceSessionsW_eq`, `dependentOriginsW_eq`). Findings: a `parent_of` child counts as a dependent but never as recurrence (`hierarchical_child_is_dependent_not_recurrence`); before #300 a reused action's dependency was dated by its origin (`reused_action_dependency_is_dated_by_its_origin`). | `supporting_interaction_times`, `recurrence_sessions` (`src/canon/policy.rs`), the dependent counts of `src/canon/stage1.rs`, `about_time`, `WriterTimeline` |
| `Policy/Score.lean` | The score is monotone in each term, each unit moving it by exactly its weight times the resistance (`score10_mono`, `score10_steps`); the bands partition the score range (`classify_*_iff`); Stage 1 under Solo is exactly "resistant score ≥ 3.0" (`soloStage1_eq_admitsHop`); under either policy a cycle moves a concept at most one rung, a Solo promotion lands only on a rung its band admits, and no policy promotes past a failed verdict (`policy_cycle_one_hop`, `solo_cycle_capped`, `promotion_needs_verdict`); `PromotionPolicy::from_str` is fail-closed (`parse_sound`). In `f64` the order differs from the exact one on ties (`exact_tie_orders_by_key`). | `raw_solo_score`, `classify`, `SoloScorer`, `SwarmScorer`, `PromotionPolicy` (`src/canon/policy.rs`), `eviction_resistance`, `Evaluator::gather`, `apply` |
| `Conflict/Detect.lean` | A conflict fires iff two distinct agents are resolved writers of non-excluded edges incident to the node and one `Causal`/`Dependency` write is in the window, both ends inclusive (`conflictAt_isSome_iff`, `qualifies_iff`); the verdict, agents and age do not depend on edge order (`conflictAt_perm_isSome`, `conflictAt_perm_agents`, `conflictAt_perm_secondsAgo`); `Contradicts` and `SuspectedSupersedes` edges change nothing (`excluded_edge_irrelevant`); a conflict lapses once its newest write leaves the window, and its age grows with the caller's clock until then (`lapse_of_old`, `lapse_permanent`, `secondsAgo_advances`). Before #297 a re-send by another agent dropped the original writer (`resend_drops_original_writer`); the fixed rule keeps the creator, adds the re-sender and never loses a conflict the old rule found (`resend_keeps_creator`, `resend_adds_resender`, `conflictAtFixed_of_conflictAt`, `issue_297_fixed`). | `conflict_at_with`, `edge_writers`, `attribute_at`, the window and the payload in `src/daemon/conflict.rs`; `HotList::revalidate`'s re-run of the check; `record_edge`'s re-send |
| `Conflict/Contradiction.lean` | `declared_at` never dates a declaration after its write and a re-send never re-dates it (`declaredAt_le_createdAt`, `declaredAt_resend`); evidence counts by write time, so a backdated citation does not skip the age floor (`eligible_eventTime`, `backdated_not_eligible`); with `now - min_age` out of range nothing is eligible (`unrepresentable_minAge`); a hit is an admissible declaration written by `now`, dated strictly after the baseline (`contradictionAt_sound`), and it is the newest, then smallest key, then smallest id, whatever the edge order (`contradictionAt_max`, `contradictionAt_perm`). | `contradiction_at_with`, `declared_at`, `written_by` and the evidence guards in `src/daemon/contradiction.rs`, over `conflict.rs`'s attribution |
| `Hotlist/List.lean` | The bound and the contradiction budget always hold (`enforce_length_le`, `enforce_countC_le`), with one entry per `(node, condition)`. What is evicted ranks at or below everything kept, except a contradiction dropped by the budget (`enforce_evicts_lowest`); one insert evicts at most one entry (`insertFull_evicts_le_one`), a refresh evicts nothing (`insert_refresh`), and the batch insert keeps the invariant (`insertMany_inv`). While nothing is evicted insertion order does not change the live pairs (`insertAll_order_irrelevant`); once full it does (`order_matters_when_full`). Recall gets a payload for a member exactly when an entry of that node holds at `now` (`mem_hotPayloads`). | `src/daemon/hotlist.rs`, `HotListPayload` |
| `Hotlist/Recall.lean` | Conflict, high-risk, drift and stale payloads force, and a contradiction forces only on a Canonical or Venerable concept (`forces_iff`); every forced member is emitted (`hot_member_emitted`, via `TopK.forced_mem_emit`) and carries a hot-list line (`forced_has_line`). A member whose hot-list entries have all lapsed is not forced into recall by the hot list (`lapsed_not_forced`; it can still be forced as a contradiction companion or a cold-mode reservation holder). | `forces`, `contradiction_companions` and the forced predicates in `src/recall/assemble.rs`, the warning lines in `src/recall/format.rs`, `forces_inclusion` |
| `Hotlist/Cycle.lean` | Before #298 the cycle inserted before it retained, so a fresh lower-tier hit that fit could be evicted (`cycle_evicts_fresh_hit`). The cycle since #298 (deferred inserts, then the cut, then one `enforce`) keeps the invariant (`cycleFixed_inv`), only detected entries that rank lowest are evicted, apart from a contradiction dropped by the budget (`cycleFixed_evicts_only_for_held`), every fresh hit that fits is kept and the list equals the cycle's hits up to the bound (`cycleFixed_keeps_hits`, `cycleFixed_keys_eq_hits`), it agrees with the old cycle whenever that one evicted nothing (`cycleFixed_perm_cycle_of_no_evict`; agreement whenever no entry lapses is `decide`-checked only, as it rests on the unproved `insert_many` equivalence), and the issue's scenario now keeps both hits (`cycleFixed_keeps_fresh_hit`). | `detect_into_hot_list`, `condition_set` (`src/daemon/mod.rs`), `HotList::sync_cycle` |
| `Drain/Spec/Base.lean`, proved in `Drain/Proofs/Base.lean` | The definitions the other drain files share: jobs, the mutation log, the receipt store, `settle_deferred`'s set-aside, durable intents, the load order and the replay's step. At replay a refusal that is a fact about the intent's input settles `failed`, and a transient answer skips the intent until three in a row stop the replay (`replayStep_inputFact`, `unavailable_skips`, `unavailable_stops`). Intent rows and consumes are keyed by receipt id (epoch, issue time, sequence), so multi-process backlogs are modelled. The `Drop` path settles nothing (the documented exception, in the header's boundary). The drain model is in Spec and Proofs form, split by subsystem: four files under `Drain/Spec/`, their proofs under `Drain/Proofs/`, and `Drain/Examples.lean` and `Drain/History.lean`, all in the namespace `Lambo.Drain`. | `WriteCtx::run`'s consume and `ConsumeStamp` (`src/writeq/execution.rs`), `settle_one`, the `CloseFlush`, `abort_workers` and `settle_deferred` of `src/writeq/drain.rs`, `record_write_intent` and `consume_write_intent` (`src/graph/graph/mutation_log.rs`), the adapters' load order (`session_load.rs`), the error classes; `abort_all_sync` and `Memory`'s `Drop` (`src/memory/shutdown.rs`); `spawn_replay`'s step, constants and restart-map seed (`src/writeq/replay.rs`) |
| `Drain/Spec/Replay.lean`, proved in `Drain/Proofs/Replay.lean` | At replay an agent is deferred at a transient answer or at a receipt id of another format version, and deferred agents are retried once at the end of the pass, so no write applies ahead of an earlier one of its agent (`loopD_no_overtake`, `stepQ_skip`), and a retry that gets an answer for every write leaves nothing owed (`replayP_progress`). Each process's rows load in `lane_seq` order with no clock assumption, the adapters' order kept across processes (`fixedLoad_sorted`, `fixedLoad_epochs`, `fixedLoad_agrees`). A non-transient error that is not an input refusal stops the replay (`fixed_sessionWide_stops`, stated over an earlier loop whose arms the current one keeps for those answers). A malformed id is settled `failed`, and an id of another format version is left owed with its agent deferred (the step `stepQ`). A rejected mixed batch settles the bad intent and leaves its neighbour owed once (`replay_dead_letter_two_restarts`). | `spawn_replay` and `replay_order` (`src/writeq/replay.rs`), `src/store/replay_flush.rs`, `ReceiptId`, `ReplayBlockReason`, the adapters' load order |
| `Drain/Spec/Worker.lean`, proved in `Drain/Proofs/Worker.lean` | In session, a write met by an embedder outage, or a first-attempt slow vector lookup once three in a row have timed out with nothing applied between, holds its agent's later writes (`pending_replay`), and the agent's writes apply in admission order (`runW_ord`). A retry of held writes needs no admission and is always available while they are held (`retry_settles_head`, `retry_enabled`; that the retry comes due is the server's backoff timer, which is not modelled). The caps count held writes and their bytes at every step (`runW_caps`), and nothing taken after a lost lease is applied (`fence_stops_applies`). A block reason is reported whenever intents are left owed, halts included (`serveR_reported`). Together, each agent's writes of one process apply in admission order from the session through the next serve, whatever the embedder answers (`sessionW_then_serve_order`). | `spawn_worker` (`src/writeq/execution.rs`), the held writes and caps of `Lanes` and the cap check in `WritePipeline::admit` (`src/writeq/admission.rs`, #295), `settle_one`, `hold_one` |
| `Drain/Spec/Close.lean`, proved in `Drain/Proofs/Close.lean` | After a close every admitted write is exactly one of applied, failed with its intent consumed, failed after the fence with its intent left unconsumed, `intent_durable` and owed in the flushed log, or `close_unflushed`, and none is left pending (`shutdown_no_silent_lossD`, `shutdown_exactly_oneW`); a held write answers `pending_replay` while held and the close never answers it `failed` (`outage_holdsW`, `held_at_closeW`); a lookup timeout settles `failed` and consumes its intent, except a first attempt at least the third since the last commit, which holds (`lookupTimeout_settlesW`, `lookupTimeout_run_holdsW`, `heldErrW_holds`, `heldNew_iff`, `timeout_after_commit_settlesW`). Against a flush history in which each batch of the log was committed or dead-lettered, a close whose final flush completed answers a set-aside write `intent_durable` exactly when its put was committed, and a close whose flush did not complete answers every set-aside write `close_unflushed` (`closeD_durable_iff`, `closeD_never_vouches_dropped`, `closeD_vouches_only_committed`); a retried close upgrades only a committed one (`closeD_retryD`), and with no dead letter the close is `closeW` (`closeD_agrees`). The dead-lettered set, pruned at a write's settle, never lets the close vouch for a dropped put, worker panics included (`settlePrune_never_vouches_dropped`, `settlePrune_exact`, `settlePrune_unsettled_bounded`, `settlePrune_settled_forgotten`). The serve applies only stored, unconsumed rows, so never a write whose durable batch carries its consume (`serveR_applied_pending`, `crash_never_reappliesW`). The flush outcome is a parameter, so that a fenced close answers `close_unflushed` is the code's, not a theorem; a write refused at admission keeps a `pending` receipt and a put in the model, so the theorems speak only of admitted writes; and the drain trace may retry a held write after the lanes are sealed, an over-approximation. | `quiesce`, `abort_workers`, `settle_deferred` (`src/writeq/drain.rs`), the `CloseFlush` that `Memory::close` and `Memory::close_fenced` pass it (`src/memory/shutdown.rs`), the dead-lettered intents the flush notes (`src/graph/graph/unflushed.rs`, #345), `FlushLoop::drain_and_flush`, the commit's streak reset (`WorkerCommit`, #346) |
| `Drain/Examples.lean` | Every theorem here is a finding about the Rust before a fix or about a rule not taken, so docs anchors may not cite this file. Before #294 and #295: a transient embed answer reorders one agent's writes (`transient_skip_reorders`, `outage_then_replay_reorders`), the load order trusts the wall clock (`clock_step_reorders`), a graph-dependent non-cycle `Invariant` blocks the backlog every serve (`store_error_blocks_backlog`), an unparseable receipt id is never settled (`bad_id_never_settled`), and an outage answered `failed` though the write was later applied (`outage_failed_then_applied`). Before #345 the close vouched for a dead-lettered put on the Rust test's scenario (`closeW_vouches_dead_lettered`), and pruning the dead-lettered set at the consume vouches for a dropped put after a worker panic (`keptAtConsume_vouches_after_panic`). Before #346 the lookup-timeout streak, reset at the settle and then in the `Ok` arm, counted a timeout that followed a commit (`streak_old_counts_across_commit`, `streak_okArm_counts_across_commit`); resetting at both would forget a run, a rule not taken (`streak_both_forgets_run`). | (concrete cases over the other drain files; no Rust of its own, so no drift entry) |
| `Drain/History.lean` | **The worker before #295** (`workerStep`/`work`/`close`) answered an in-session embedder outage or lookup timeout `failed`; it is kept to document what #295 replaced, and its theorems do not describe the current Rust: a lane drained to completion took its submitted writes in order and settled each once (`drain_fifo`, `drain_settles_each`); a durable write carries its consume, so no replay after a crash re-applies it (`crash_never_reapplies`); after `close()` every admitted write is drained, `intent_durable`, or `close_unflushed` (`shutdown_no_silent_loss`, `close_answer`); an outage answered `failed` and left the write owed (`outage_failed_but_owed`). **The replay before #294**: it applied only unconsumed rows and a drained queue replayed to nothing (`replay_drained`, `replay_idempotent`); one serve applied each process's writes in sequence order when issue times followed it (`load_seq_order`, `replay_epoch_seq_order`). An input refusal settled `failed` and the replay went on; any other non-transient error stopped it (`inputFact_goes_on`, `sessionWide_stops`). **The replay over #295's first step** (`loopD stepFixed`): an input refusal settled `failed` and the loop went on with that step, which consumed an id in another receipt format `failed` where the current step quarantines it (`fixedD_input_goes_on`). **The close before #345** vouched for a set-aside write whose put a dead letter dropped, in every reachable session (`closeW_vouches_dead_letteredW`). | (none: the worker before #295, the replay before #294, the replay over #295's first step and the close before #345; not drift-checked, and docs do not cite it) |
| `Flush/Retry.lean` | Backoff sleeps never decrease and are capped at `BACKOFF_CAP` (`flushWithRetry_backoff`); a ladder makes at most `retries + 1` calls, all with one batch, and only a retryable error is retried (`ladder_exhausted`, `stale_write_one_call`). With the idempotent upsert the store contract requires (`upsert_idempotent`) and no concurrent writer, a retried batch lands once in effect (`ladder_ok_effect`). | `flush_with_retry`, `next_backoff` and the backoff constants in `src/store/flush.rs`, `StoreError::is_retryable`, the `GraphStore::flush` contract |
| `Flush/Loop.lean` | Over every run the ledger, `pending` and graph log are exactly the history (`good_reachable`); each batch sent is the unconsumed tail in log order (`step_calls_unconsumed`, `committed_sublist`) and nothing consumed is sent again (`never_resent`). Exhausted retries retain the batch (`exhausted_retained`); a constraint violation is dead-lettered once and counted (`constraint_dead_lettered`). After a stop every mutation is committed, dead-lettered, dropped with a visible flag, or back in the graph log (`no_silent_loss`); only a hard abort loses `pending` (`abort_loses`). After a takeover the old token never lands (`fencing_refuses_after_takeover`, #1). Shown about the Rust before #293: a `StaleWrite` was retained and re-sent, not treated like a constraint (`stale_write_retained`); after the operator override the holder's own refresh re-took the lease at a new token the flush loop never adopted, so the fence never latched (`override_refresh_retakes`); a fence latched within one poll before a due tick was seen one pass late (`flush_after_fence_latched`). Fixed by #293 (`runFixed`, `Lease.beatLatches`): after any takeover, the operator-override re-take included, the heartbeat latches the fence at its next beat (`Lease.beat_latches_iff`, `takeover_fences_fixed`) and then stops beating (not modelled); no flush is issued once the fence is latched (`no_flush_after_fence_fixed`, `fence_before_due_tick_fixed`); a stale write fences the loop and is never re-sent (`stale_write_never_resent`); a stop after the fence drops the tail (`stop_drops_fenced`); the loop's invariants still hold (`good_reachable_fixed`, `no_silent_loss_fixed`), and a close that re-checks the fence after joining the loop returns Ok only when every batch that left the loop was committed or dead-lettered (`close_ok_no_drop`; without the re-check it can report Ok over a fenced drop, `close_ok_over_drop`); nothing else changes (`stepFixed_agrees`). Before #301, the lease read behind `erased()` and `write_flush_stats` ran outside the flush attempt's timeout and catch-unwind. A panic in either killed the task holding `pending`, with `degraded()` false (`lease_panic_loses`, `stats_panic_loses`), and `close` returned `Ok` over the loss (`close_ok_over_panic`). A call that never returned blocked the stop (`lease_hang_blocks_stop`). With both guarded, a failed lease read counts as not erased, and the invariants and close theorems carry over (`no_silent_loss_guarded`, `close_ok_accounted_guarded`, `stop_exits_guarded`). | `FlushLoop` (`run`, `drain_and_flush`, `requeue_pending`, and the store calls of `publish_stats` and `refused_as_erased`) in `src/store/flush.rs` |
| `Flush/Errors.lean` | Before #301, a permanent SQL error that is not a constraint violation reached the loop as a retryable `Backend`, so it was retried and retained until the backlog degraded the session (`permanent_retried_retained`). Since #301 a permanent error about the store costs one call and degrades the session at once (`permanent_one_call`, `permanent_degrades_classed`); nothing calls the store again (`permanent_never_resent_classed`) and `close` errors (`close_not_ok_degraded`). Every other class is unchanged (`runClassed_old`, `stepClassed_agrees`), and the invariants and close theorems carry over (`no_silent_loss_classed`, `close_ok_no_drop_classed`, `close_ok_accounted_classed`). The `exGone` examples show why a store-scoped error degrades rather than dead-letters. A permanent error about a value the batch holds is dead-lettered like a constraint violation and the session goes on (`value_rejected_dead_letters`); only one about the store degrades (`store_permanent_degrades_split`), and the results carry over (`via_split_agrees`, `no_silent_loss_split`, `close_ok_accounted_split`). | the adapters' error classification in `src/store/error.rs` |
| `BatchQueue/Pure.lean` | A take is singles then bulk in arrival order (`takePrioritized_fst`) and holds at most `MAX_BATCH`; every text is in one chunk of at most `FORWARD_WIDTH` texts within the length slack (`lengthChunks_perm`, `chunksOf_slack`, `chunksOf_eq_lit`); the `remove` loop takes exactly the chunk (`removeDesc_eq`); with a row-wise forward every caller gets its own text's answer (`forwardIsolated_rowwise`); a take runs at most `MAX_SINGLE_STREAK` single chunks in a row (`pick_progress`) and ends within `\|single\| + 132·\|bulk\| + 1` iterations (`take_ends`). Since #310 a device fault that has passed by the canary changes no text's answer: every text of the chunk, a lone text included, is rerun alone and gets what it gets alone (`transient_fault_recovers`). | `take_prioritized`, `length_chunks`, `forward_isolated` and `serve_take`'s take loop in `src/embed/candle.rs` |
| `BatchQueue/Worker.lean` | No caller is answered twice in any run (`answered_at_most_once`), and every request is answered exactly once once the worker catches up or the embedder shuts down (`served_when_left_alone`, `eventually_all_answered`), across deaths and restarts. Answers are never cross-wired after reordering by length (`answers_wired`); with a row-wise forward every caller gets its own text's answer (`answers_are_own`). Every forward holds at most `FORWARD_WIDTH` texts, and every take at most `MAX_BATCH` (`forwards_capped`, `topUp_cap`); shutdown exits only after draining (`shutdown_drains`). The model is a step relation over pushes, timeouts, worker steps, deaths, supervisor restarts and shutdown. | `BatchQueue`, `coalesce_loop`, `supervise`, `serve_take`, `still_awaited`, `embed`/`embed_batch`/`recv_bounded`/`Handle::drop` in `src/embed/candle.rs` |
| `BatchQueue/Runs.lean` | Since #309 a worker death answers the requests the worker held by dropping their senders, which the model's class table, pinned by the Rust test, marks transient (`worker_death_transient`). Shown about the Rust: queued bulk work can starve behind `MAX_BATCH` singles (`queued_bulk_starves`); a long recall can wait out the whole take (`long_recall_waits_out_the_take`). | runs of `src/embed/candle.rs` from its coalescer tests |
| `Registry/Step.lean` | The invariant holds in every reachable state (`reach_inv`). On-demand Memories never exceed the places (`mems_le_places`), permits and unprobed attaches stay bounded (`attaches_le_permits`, `unplaced_le_cap`), and there is never more than one build per session (`one_build`). An erase claims only with no attach holding a permit, and nothing acquires an erasing session (`erase_claim_quiescent`, `erasing_no_build`); `admit_if` never refuses (`retry_admits_over_held`). A cancelled or timed-out attach frees its place and permit (`drop_clears`, `timeout_clears`); no re-attach while the previous handle lives (`retry_waits_previous`); retry backoff holds (`retry_backoff`); the idle sweep skips in-flight and pinned sessions (`sweep_skips_in_flight`); a detach releases exactly once and only when unfenced (`detach_release`). A fenced handle refuses writes, a beat off its own generation latches and wakes the watcher, and a stale flush wakes the watcher when it first latches the fence (`write_refused_fenced`, `beat_latches`, `stale_latches_and_wakes`, `watcher_fires_after_latch`). The model is one step relation over requests, on-demand flights, the pinned retry, detaches, the idle sweeper, `watch_lease`, erases and the shutdown. | `SessionRegistry` in `src/mcp/serve/registry.rs` and `registry/{erase,on_demand}.rs`; the heartbeat and fence of `src/memory/leases.rs` after #293 |
| `Registry/Pure.lean` | Victim choice and the fair share (`chooseVictim_max`, `at_share_evicts_own`, `placeShare_fair`); the lease-loss policy, the pinned retry's floor, the answers and `/admin/sessions` (`forScope_exit_iff`, `pinnedRetry_ge`, `answer_erasing`, `slotViews_complete`); what the negative cache keeps (`busy_never_cached`, `panic_after_admit`). | `choose_victim`, `place_share`, `unplaced_cap` and `NegativeCache` in `registry/on_demand.rs`, `answer_for`, `slot_views`, `LeaseLossPolicy::for_scope` |
| `Registry/Runs.lean` | A queued lease-loss detach leaves a replacement handle and its active call alone (`lost_detach_preserves_replacement`). A dropped flight's release can still release the next attach's lease once the cached `Failed` is gone. The next heartbeat then fences that handle (`dropped_flight_fences_next`). | runs of `src/mcp/serve/registry.rs` and `registry/on_demand.rs` |
| `Resolve/Construction.lean` | Everything a command builds comes from the `src/resolve.rs` entry point its `Resolved` variant names (`resolve_built_allowed`); at most one store and one embedder (`resolve_builds_at_most_one`); no embedder unless needed, except `session verify` (`embedder_only_when_needed`, `verify_builds_embedder_though_needs_embedder_false`); a successful resolve gets exactly the configured, compiled kinds, so an uncompiled kind, an unknown key or an unknown kind spelling is a hard error (`full_ok`, `storeOnly_ok`, `load_refuses_unknown`); the width is store-authoritative and, for SQLite and Postgres, the compatibility check is an echo (`echo_never_width_mismatch`, `cockroach_width_refused`, `pin_mismatch_refused`). Shown about the Rust: the store-only path reads the file twice (`storeOnly_kind_and_store_differ`). The feature set and each read of `lambo.toml` are parameters. | `Commands::needs_embedder`, `Resolved` and `resolve_for_command` in `src/main.rs`; `resolve_backends`, `resolve_store_only`, `resolve_embedder_only` and the session resolvers in `src/resolve.rs`; `build_store_with_options`, `wrap_with_recall_tier`, `build_embedder`, `LamboFile::load_resolved` |
| `Resolve/Serve.lean` | `main` builds nothing the resolve did not (`main_built_allowed`, by construction); an unknown key exits 1 (`unknown_key_exit1`); `main`'s mismatch arm and override `unreachable!` are unreachable (`main_never_mismatch_or_panic`); serve preflight exits 2 except for file errors (1), a stdio serve ignores tokens, the env token wins and is never a silent fallback (`preflight_*`). Since #316 `serve`'s own start checks never refuse a plan preflight passed (`preflight_ok_start_allowed`), so a non-loopback bind without a credential (in `serve` and `serve-web` alike) and `--ledger-heartbeat` without `--ledger` exit 2 before any backend is built (`exposed_bind_refused_before_build`, `heartbeat_without_ledger_refused_before_build`). | `ServePlan`, `serve_preflight`, `stdio_session` and `main`'s dispatch in `src/main.rs`; `pin_sessions`, `resolve_auth_token`, `SecretToken::new`, `check_bind`, `authorize_bind`, `check_ledger`, `authorize_ledger`, `serve_web::plan_bind` |
| `Resolve/Auth.lean` | Redaction: on every modelled print path (`PrintPath`) the output depends on the token only through its verdict, never its bytes (`render_noninterference`, by definition, so only as strong as that list of paths). Before #315, `lambo serve-web --auth-token`'s clap error quoted the refused value (`serve_web_flag_refusal_leaks`), and clap's unexpected-argument error quoted a token starting with `-` or the tail of a split token (`hyphen_token_leaked`, `unexpected_arg_leaked`). Since #315 the `serve-web` parser refuses the same tokens for the same rule in `serve`'s words (`parseWebAuthArg_ok_iff`, `web_flag_refusal_keeps_reason`, `web_flag_refusal_eq_serve`, `serve_web_flag_refusal_fixed`); a spaced `--auth-token` refuses a flag-like value and `--auth-token=-x` takes any value (`authFlagValue_spaced_none_iff`, `authFlagValue_eq_form`; the fail-open alternative is `b1_fail_open`); and every quoting error prints the same for every argument from the flag on (`quotedRender_noninterference`), except a missing value (`missing_value_kept`) and the flag's own spelling. Not covered: the legacy-token refusal reveals whether the token equals a configured credential's (`credential_refusal_reveals_equality`), and a token split by an unquoted space still reveals that it was split and may change the error, and a piece an accepted flag takes may be printed as that flag's value (`--transport <piece>` prints "unknown transport" with it, and a `--session <piece>` appears in `serve-web`'s banner). The split-token cases predate #315. | `AuthTokenArg`, `http_auth_token_flag` and `redact_auth_token_echo` in `src/main.rs`; `SecretToken` |
| `Resolve/Args.lean` | The clap rules of `stats`, `recall`, `embed` and `canon` (`statsValid_iff`, `embedValid_iff`), and the construction, width and preflight results on examples (`hard_error_examples`, `width_examples`). | the clap `Args` of `src/main.rs` |
| `Tiered/Pure.lean` | Versions order by holder token, then counter, and `next_version` never goes back (`version_holder_first`, `nextVersion_gt`, `nextVersion_refuses_older`); the over-fetch stays within the candidate limit (`knnFetch_bounds`); the breaker trips, falls back while open and probes once (`breaker_*`); a refresh never adopts a re-taken token (`noteRefresh_never_adopts`). | `mirror_version`, `project` and `Projection::switched` (`src/store/tiered/project.rs`); `next_version`, `knn_fetch`, `check_marker`, the breaker and `note_lease`/`note_refresh` in `src/store/tiered/mod.rs` |
| `Tiered/Machine.lean` | The index never touches the durable store, and an accepted flush is exactly its batch (`indexSide_keeps_durable`, `write_commits`, `mirror_ends`). A repair with no further writes leaves the index matching the durable session, in sync, given that every indexed version is below the pass's (`repair_converges`), and an overtaken pass is not trusted (`finish_overtaken`). Once the lease moves on, no repair write under the old token lands after the one in flight (`repair_silent_after_loss`, `erase_silences_repair`), and only a held, accepted flush mirrors (`mirror_starts_held`). While the breaker is open a read is the durable store's (`read_breaker_open`). Loads settle to a consistent marker state (`settle_consistent`). With every stored vector returned, the re-rank ignores the engine's scores and is the exact scan on the candidates (`rerank_ignores_engine`, `rerank_exact`). Since #319 a batch that moves the contract is repaired, not mirrored (`mirror_switch_writes_nothing`); every other flush is unchanged (`write_agrees`). The model is one session's events; its mirror uses one bulk and does not model Rust's per-chunk fences. | `src/store/tiered/` (`mod.rs` mirror, repair, reads and lease; the engine's rules in `index.rs` and `fake.rs`), `rank_by_cosine` |
| `Tiered/Runs.lean` | Since #319 a re-embed migration ends with the new contract's index matching the store (`reembed_migration_held_back`, `reembed_migration_repaired`). Shown about the Rust: an in-place re-embed briefly mixes builds (`inplace_reembed_mixes_builds`); an in-sync reader never re-checks (`reader_never_rechecks`); before #320, a stalled mirror could write both vectors and the marker after an erase (`mirror_lands_after_erase`); since #320, a lease check gates the modeled bulk and marker and a previously submitted bulk can still land (`mirror_does_not_submit_after_erase`, `mirror_bulk_in_flight_does_not_recreate_marker`); since #321, a repair reconciles from the snapshot used for its recheck (`repair_uses_rechecked_snapshot`); an edge-only batch leaves the marker behind (`edge_only_batch_leaves_marker_behind`); a stale repair pass leaves the newer holder's version counter intact (`stale_pass_keeps_newer_version`). | runs of `src/store/tiered/` from its tests |
| `Tiered/History.lean` | Not current Rust, and not citable from the docs. Before #319 a re-embed migration was mirrored into the old contract's index (`reembed_migration_misindexed`) and a relabel left the new contract's index empty in sync (`relabel_leaves_new_index_empty`), both on the previous mirror (`Pprev`); the fixes #319 did not take miss the relabel or index quarantined vectors (`projectFinal_misses_relabel`, `projectFinal_ahead_of_durable`), or change what the durable store keeps (`reorder_changes_durable`). | `src/store/tiered/` before #319 |
| `Discovery.lean` | Shown about #287's first rule (join any read that finished since arrival): a request arriving during a read that already saw a session lists it after it was erased (`old_lists_erased`, `old_not_fresh`, `old_violates`). Under either rule no listing names a session erased before its read began (`listing_safe`), and the portal issues at most one read at a time (`one_read_at_a_time`; an abandoned read's query may still run in the backend, which is not modelled). With the fix (join only a read numbered above the `started` count seen on arrival): every listing comes from a read that began after its request arrived (`fixed_fresh`), so no request lists a session erased before it arrived (`fixed_no_erased_listed`, `fixed_coderabbit`); requests arriving during a read share the next one (`fixed_shares`: three requests, two reads). The model is an event model of requests queueing on the gate (taken in any order), reads that observe the live set when they start and then finish or are abandoned (timed out or dropped), and erases. | `Discovery::sessions`, `Discovery::new` and `Last` in `src/cli/serve_web/discovery.rs` (the `/api/sessions` store-wide read with `[web] discover_sessions`, #276) |
| `Lib/List.lean` | Facts about lists that mention no Lambo type (filters, duplicate-free lists, permutations, prefixes, `getD`, folds), moved here from the models that first proved them so every model can import them. Each kept its statement; `formal/audit/moved-helpers.tsv` maps old names to new. | (none: generic lemmas, not a model) |
| `Audit.lean` | `#print axioms` for the headline theorems. | |

## How far the proofs reach

A proof here is about the *model* in the same file, not about the Rust code.
The link between the two is the reading of the source recorded in each file's
header, and it is only as good as that reading. In particular:

* Some numerical models use exact arithmetic. The bounds in `Score.lean`
  apply to its fixed-point integer representation.
* The lease, receipt and reservation models assume atomic operations under a
  store transaction or lock. The worker, registry and tiered index models
  cover interleavings represented by their event sequences.
* Several algorithms are modelled from their inputs onward or without a path
  that does not affect the stated results; each header lists what it leaves
  out. Where a model's statement holds only in exact arithmetic, the header
  says so (`Confidence.lean`, `Policy/Score.lean`, `Fit.lean`).
* Predicates whose inputs are measured by store queries (Stage 2 coverage,
  blast radius, eviction score) are modelled from those inputs onward.

If the Rust changes, the model has to be re-read against it. Nothing proves
the two agree; the drift check below only makes a change to modelled Rust
visible, and the differential oracle (last section) checks that the Rust
gives the model's outputs on a fixed set of generated inputs.

## Keeping the models in step with the Rust

`formal/models.toml` lists, for every model file (every `.lean` file under
`LamboProofs/`, subdirectories included, except `Audit.lean`, the shared
lemmas under `Lib/`, the history layers `Drain/History.lean`,
`Drain/Examples.lean` and `Tiered/History.lean`, which describe no current
Rust, and the proofs layer of a model in Spec and Proofs form, whose `Spec`
files have the entries), the Rust it
models and `read_digest`, a fingerprint of that Rust as it was when the model
was last read against it. A Rust ref is a whole file (`src/recall/expand.rs`)
or one item in it (`src/types/mod.rs#tie_break_by_key`,
`src/store/mod.rs#GraphStore::flush`); items are used where a model reads one
part of a large shared file. The file's header comment gives the full syntax.
An item ref covers that item's own text (with its attributes, and for a type
its `impl` blocks), not the `use` imports it relies on or the helpers,
constants and types it calls or names: list those as refs of their own when
the model depends on them.

The fingerprint is a SHA-256 over the entry's refs, sorted, and each ref's
code with comments and `#[cfg(test)]` code blanked and whitespace collapsed
outside string and char literals (literals are kept verbatim; for an item,
only that item's code). It is written `v<N>:sha256:<hex>`, N being the digest
scheme. `scripts/formal/check-model-drift.py` recomputes it and compares:

```bash
python3 -I scripts/formal/check-model-drift.py
```

It needs Python 3.11 or later (for `tomllib`) and git.

A model whose fingerprint no longer matches its `read_digest` has drifted. A
change to comments, to whitespace between tokens or to test-only code alone
does not count (a change inside a literal does), and an item ref counts only a
change inside that item. The fingerprint needs no git
history, so the check gives the same answer after a squash or rebase merge
and in a shallow clone, and a warning stays on every later run, on every
branch, until the model is bumped.

To clear a warning, re-read the model against the Rust as it is now, update
the model file (its statements, or the header's reading of the Rust) if the
change matters to it, and bump it:

```bash
python3 -I scripts/formal/check-model-drift.py --bump Canon
```

`--bump` takes one or more model names (`Canon`, `Canon.lean` or
`formal/LamboProofs/Canon.lean`), recomputes their `read_digest` from the
working tree and rewrites only those lines of `formal/models.toml`. It refuses,
writing nothing, while any of the model's refs does not resolve, and it names
any ref file whose code differs from `HEAD`: the digest then records
uncommitted edits, so commit them with the manifest. Editing the Lean file
alone does not clear drift; only a bump does, and a bump is a claim that the
model was re-read, so bump only models you have re-read. `--bump-all` bumps
every model: for a new manifest, or after a change to the script's
normalisation. Such a change bumps the script's digest scheme, and the check
then reports "digest scheme changed" (and fails) until `--bump-all` is run;
run the previous script first to make sure no model had drifted.
An older script run on a manifest with a newer digest scheme fails the same
way but refuses to bump, so it cannot downgrade the manifest.

The ref list is part of the fingerprint. Adding a ref, dropping one,
narrowing a whole file to some of its items or re-pointing a ref at code that
moved is drift until the model is bumped, so drift cannot be hidden by
editing the manifest. The Lean path is not part of it: renaming a model file
together with its entry is not drift.

`read_against` is optional information: the commit the model was last read
against. Only its format is checked. When a model drifts, the report uses it
only if it is a commit in the clone and the model's refs there fingerprint to
exactly its `read_digest`, that is, only if it is the state the digest was
taken at. Then the report compares each ref there with now, names only the
refs that changed, each at its first changed line, and adds a
`git diff <read_against> -- <path>` hint. Otherwise (no `read_against`, a
commit missing from the clone such as a squash-merged branch's or one outside
a shallow clone's depth, or a commit that is not the digest's state) it says why and lists all the model's refs.
`--bump` does not change `read_against`; update it by hand if you keep it.

CI runs the check on pushes and pull requests as a warning
(`.github/workflows/model-drift.yml`): drift is a reminder to re-read the
model, not a failure (`--strict` makes it exit 1). CI checks out the full
history (`fetch-depth: 0`) so that `read_against` resolves and each warning
points at the changed lines; without that history (a shallow clone) the
report lists every ref of a drifted model. Each warning is attached to a
drifted Rust ref: at its first changed line when `read_against` narrows the
report, otherwise at the item's keyword line, or on the whole file for a
whole-file ref. The manifest itself is always checked, and a broken one fails
(exit 3): a model file with no entry, a listed file or item that does not
exist, a `read_digest` or `read_against` that is not well formed, or a
`read_digest` of another digest scheme.

When modelled code moves to another file or is renamed, update its ref in the
manifest in the same change; the check fails until the ref names something
that exists, and the model counts as drifted until it is re-read and bumped.
A new model file needs its own entry, then `--bump`. To run the script's own
tests:

```bash
python3 -I scripts/formal/check-model-drift.py --self-test
```

## Restructuring the models without changing what they prove

A change that only moves, splits or tidies the models (a file split, a shared
lemma moved to `LamboProofs/Lib/`) must leave every theorem's statement as it
was. `scripts/formal/check-decl-equivalence.py` checks that against a base
commit: it exports the base's `formal/` with `git archive`, builds it and the
working tree, dumps every declaration of every module under `LamboProofs/`
with `scripts/formal/DumpDecls.lean` (a Lean program that walks the elaborated
environment, so it compares what Lean checked rather than the source text),
and compares them by fully qualified name.

```bash
PATH="$HOME/.elan/bin:$PATH" python3 -I scripts/formal/check-decl-equivalence.py --base origin/main \
  --renames formal/audit/moved-helpers.tsv --deletions formal/audit/deleted-theorems.tsv
```

It fails unless every theorem at the base exists with the same kind, universe
parameters and statement (the kernel term, binder names included; its proof
may differ), every definition, inductive and structure keeps its type and
body, nothing is added, and every theorem rests on `propext`, `Quot.sound` and
`Classical.choice` at most. Which module a declaration lives in is reported,
not compared. A declaration that goes private or public fails, since that
changes its kernel name. A declaration may change name only when `--renames FILE` lists
it as `OLD<TAB>NEW` (a helper moved to the shared library; several old names
may map to one new one), and then the new one must have the old one's
statement. A theorem may be missing only when `--deletions FILE` lists it
(`formal/audit/deleted-theorems.tsv`: name, reason, and the kept theorem
that states the same thing or more). Both files keep every move and deletion,
so against a later base a row made before it is counted, not compared. A
declaration that went private or public is listed in `--renames` under its
own name (`Drain.kP`, made public in #350). The constants Lean generates (`foo.match_1`, `foo._proof_1`,
`_auxLemma.N`) are compared by what they are, not by name, since their names
follow the file layout.

A change that puts a model into Spec and Proofs form changes the form of every result it restates: the type of `theorem foo` is no longer the statement but the constant `Spec.foo`. Run the check with `--spec-form` for such a change:

```bash
PATH="$HOME/.elan/bin:$PATH" python3 -I scripts/formal/check-decl-equivalence.py --base origin/main --spec-form \
  --renames formal/audit/moved-helpers.tsv --deletions formal/audit/deleted-theorems.tsv
```

Everything above still applies, and a theorem whose type differs passes only when it changed form and states the same thing: its type is exactly `Lambo.<Model>.Spec.<name>`, a `Prop`-valued definition, and that definition's body is the base's statement as the kernel term. `DumpDecls.lean` does the unfolding in Lean (one definition, its universe levels put in, and the proofs Lean abstracts out of a definition's body put back). Its kernel check compares `Spec.foo` with that body in the same environment, so it guards only the putting back of the proofs; that `Spec.foo` says what the old statement said rests on the two printed kernel terms being equal and on every definition the statement reaches, through definition bodies, being unchanged (which `--spec-form` checks for every definition anyway, and `--only-form-changes` for those). A weakened statement, an added hypothesis or a dropped conclusion fails with the first place the two terms differ. The printed term names a bound variable only where something refers to it, so `(h : P)` before and `P →` now compare equal. A `Spec` proposition may be added only as the statement of the theorem of its name.

`--only-form-changes` runs that one comparison and nothing else: for each theorem declared on both sides whose statement is in Spec form on one side and spelled out on the other, the two statements must be the same, and so must every definition the statement reaches. When `formal/lean-toolchain` differs between the base and the working tree it skips, saying so, since CI (which has no elan) would build the base with the wrong Lean. A theorem added, deleted or restated without changing form is not its business, so a model change passes it; it is the check for CI to run on every pull request. Either way the base is built from a copy of the working tree's `formal/.lake` when there is one, so Lake rebuilds only the modules that differ. The script's tests run without Lean:

```bash
python3 -I scripts/formal/test_check_decl_equivalence.py
```

To find theorems nothing references (no proof, `example`, docs anchor, claims
ledger row, Rust comment or `#print axioms` line), and statements proved twice, run
`python3 -I scripts/formal/audit-theorems.py --out
formal/audit/unreferenced-theorems.md`; it builds the models, dumps them with
`DumpDecls.lean` and sorts each unreferenced theorem into reviewed (with the
review's verdict and reason), deletion candidate or not settled. `--duplicates` lists the theorems that state the same
thing.

## Differential oracle

The drift check says the Rust changed; it does not say whether the Rust and
the model still agree. The models are executable (their definitions compute;
the `decide` examples run them), so the oracle uses the Lean definitions
themselves as the reference. `lake exe oracle` (sources in `OracleExport/`,
a library and executable outside the default target, so `lake build` builds
the proofs alone) evaluates covered definitions over a fixed set of
generated inputs and writes inputs and outputs to `oracle/<Model>.json`. The
Rust test `formal_oracle_tests::oracle` (`src/formal_oracle_tests.rs`, one
replay module per model in `src/formal_oracle_tests/`, part of the default
`cargo test`) checks that every file in `oracle/` has a replay and rebuilds each
input as the Rust types, calls the real function and asserts the output the
Lean computed, naming the model, function, case index and input on a
mismatch. Unlike the property tests (`src/**/proptests.rs`), nothing in the
comparison restates the model in Rust by hand.

The inputs come from a SplitMix64 generator with one fixed seed per model,
biased toward boundaries (ties, empty input, exact budgets, NaN and signed
zeros where the model admits them), at most a few hundred cases and about
150 KB per file. Each file's `notes` say how the model's abstract parameters
were instantiated. To regenerate after changing a model or the exporter, and
to check the committed vectors are current:

```bash
cd formal && lake exe oracle
```

```bash
scripts/formal/check-oracle.sh
```

The second regenerates into a temporary directory and fails if anything
differs from `oracle/`. Commit regenerated vectors with the change that
moved them; a change under `oracle/` runs the Rust CI.

Covered:

| Model | Lean | Rust | Instantiation |
| --- | --- | --- | --- |
| `Order` | `rank_cmp`, `rank_members`, `tie_break_by_key` | scores by `f64::total_cmp` on bit patterns; keys by `str::cmp` (no scoped key, so `tie_form` is the identity: checked); ids by number | `rankMembers`, `tieBreak` |
| `Fit` | `fit_blocks` | estimators `defaultTokenCount`, bytes, chars, saturated | `fitBlocks` over `rustEnv`, `rustBlock` |
| `Canonical` | `canonical_key`, `unfiltered_canonical_key`, `name_key` | `stop0`, `stem0`; inputs are drawn from a fixed vocabulary (the `stemTable` words, function words, identifiers), and the real stemmer is checked against `stem0` on every word the model stems; `check_size` passes and no input is a UUID (checked) | `canonicalKeyF`, `canonicalKey`, `nameKeyF` |
| `TopK` | `emitted` | the pair verdict is "same content class", checked against `Redundancy::signal` on every pair | `emit`, `emitR`/`skipsR` |
| `Expand` | `expand` | `nbrs` built per `TRAVERSAL_ORDER`, id-ascending, and checked against the graph's adjacency | `expand` |
| `BatchQueue` | `length_chunks` (feature `embed-candle`) | none | `lengthChunks Cfg.rust` |
| `Score` | `bonus_and_modifier`, `ScoreDims::composite`, `score` | fixed point at the model's scale; compared within `1e-9` (the model is exact, the Rust `f64`) | `bonusAndModifier`, `score` |
| `Fusion` | `rank` under `FusionKind::Rrf` | `tb` as in `Order`; the recent rank is the model's `recentRank`, written into the input; values within 16 ulps (relative), order exact | `RrfRecall.rrf`, `rrfAhead` |
| `Canon` | `apply_canonization_transition`, the stage predicates | `gate` exhaustively, with no demotion reason | `gate`, `stage1Passes`, `stage2Passes`, `inCooldown` |

A model's parameter that is itself Rust logic the model does not define
(the stemmer, `tie_form`, the redundancy signals, the graph's adjacency
order) is never reimplemented in Lean to make the two agree: the inputs are
restricted to where it is trivial or given as a table, and the Rust test
checks that restriction first, failing with "the oracle's instantiation
does not hold" rather than as a model mismatch.

Not covered, and why:

* `Canon.cycle`: its flags stand for "this stage would promote", and a
  `CyclePlan`, `Verdicts` and `EvalParams` that mean exactly that need
  fixtures far larger than the model.
* Exact `Fusion` ties above zero through different rank vectors: the model's
  header says `f64` rounding, not the tie-break, decides those, so no case
  has one.
* Stateful or I/O models (`Lease`, `Receipts`, `Drain`, `Flush`,
  `Registry`, `Tiered`, `Resolve`): their Rust runs against a store, a
  queue, a clock or the serve process, so a replay needs a harness per
  model; the property tests and constant pins cover what they can.
* Not attempted yet: `Cite`, `Confidence`, `Conflict`, `Contract`,
  `Dispatch`, `Gc`, `Hotlist`, `Names`, `Policy`, `Reserve`,
  `Supersession`. Their definitions compute, but the Rust they model reads
  a graph, a hot list or store queries built per case; they are the next
  candidates, one exporter module and one test module each.

Found by the oracle (#328, fixed in the Rust): under RRF, a candidate whose
exact value is zero because it is ranked only in legs of weight zero got
`+0.0` (a sum of zero terms), and one ranked in no leg (its only leg score
non-finite) got `-0.0` (an empty `f64` sum); `total_cmp` ordered `+0.0`
first, so between the two the tie-break was not applied, where the model
orders them by it. `rank` now sums from `+0.0`. Those cases stay a separate
function in `oracle/Fusion.json`, replayed with the rest.
