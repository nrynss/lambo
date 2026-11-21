# Lean 4 proofs about Lambo

This directory holds a Lean 4 development that models parts of the Lambo engine
and proves properties the source states in its doc comments and pins in its
tests. It has no dependencies beyond Lean itself (no Mathlib).

## Status: machine-checked

`lake build` passes on Lean `v4.36.0` with no errors, no warnings and no
`sorry`: 319 theorems across 14 files. The axiom audit in
`LamboProofs/Audit.lean` reports only Lean's standard axioms (`propext`,
`Quot.sound`, and `Classical.choice` for ten theorems); 9 of the 50 audited
theorems use no axioms at all.

To check them yourself:

```bash
curl https://elan.lean-lang.org/elan-init.sh -sSf | sh
```

```bash
cd formal && lake build --wfail
```

`lean-toolchain` pins the Lean version, and elan fetches it on first use.

CI does not check these yet. To enable it, move `ci/lean.yml` to
`.github/workflows/lean.yml`. It downloads the pinned Lean release, verifies its
SHA-256, builds with warnings as errors, and fails on any `sorry`. It uses no
third-party action beyond the checkout pin `ci.yml` already uses. (The session
that wrote this could not add a workflow file.)

## What is modelled, and what each file proves

Every file opens with a comment that says which Rust items it models, what the
model leaves out, and which doc-comment sentence or test each theorem
corresponds to.

| File | Rust source | Main results |
| --- | --- | --- |
| `Score.lean` | `src/daemon/score.rs`, `ScoringWeights::sanitized` | The composite is in `[0, 1 + MAX_BONUS]` for every input and weight set (`score_range`). With weights summing to at most 1 the upper clamp never fires (`rawScore_le`). The score is monotone in every dimension (`score_mono`). GC's eviction composite equals the live score at zero frequency and never falls below it (`livePlusFrequency_of_frequency_zero`, `live_le_livePlusFrequency`). The two recency formulas agree in exact arithmetic (`historical_eq_floor`). Max-normalized dimensions are at most 1 (`le_listMax`). |
| `Order.lean` | `tie_break_by_key`, `rescore`, `rank` (candidates), `rank_members` | `then_with` of lawful comparators is lawful (`lawful_lex`). The three ranking comparators are strict total orders (`rankMembers_lawful`, `rankMembers_separating`). A sorted list is determined by its set of members, so the output does not depend on input order (`sorted_unique`). Canonical members order first whatever the scores (`rankMembers_canonical_first`). |
| `Expand.lean` | `expand` in `src/recall/expand.rs` | The expansion is exactly the set of nodes within `depth` traversable edges of a candidate (`expand_sound`, `expand_complete`), each recorded once on any graph, cyclic or not (`expand_nodup`), candidates first in input order (`expand_prefix`). |
| `TopK.lean` | `emitted` in `src/recall/assemble.rs` | Forced members are never cut and take no slot (`forced_mem_emit`, `emit_free_le_topK`). The ordinary hits are exactly the first `top_k` ordinary concepts in rank order (`emit_free_eq_take`). Emission keeps rank order (`emit_sublist`). |
| `Fit.lean` | `fit_blocks` in `src/recall/format/fit.rs` | Every accepted prefix fits the budget for any estimator (`fit_prefix_ok`). The cut stops at the first block that does not fit (`fit_stops_at_first_miss`). With a monotone estimator it is the longest fitting prefix (`fit_longest`). |
| `Canon.lean` | `legal_canonization_transition`, `apply` in `src/canon/eval.rs`, `stage{1,2,3}.rs` | `Canonical → None` is the only demotion (`legal_demotion_unique`). Canonical is entered only from Venerable (`canonical_only_from_venerable`). A cycle moves a concept along at most one edge (`cycle_one_hop`). Stage 3 cannot overflow the budget (`stage3_no_overflow`). |
| `Gc.lean` | `gc::run`, `bump_gc_survived`, `sweep_due` | Protected concepts, in particular Venerable and Canonical ones, are never collected (`venerable_canonical_never_collected`). The score cut removes only Entities and Resources (`score_cut_only_entities_and_resources`). `gc_survived` never decreases (`bump_never_resets`). An idle session never sweeps on time alone (`idle_never_sweeps`). |
| `Cite.lean` | `closing_edge` in `src/graph/action.rs`, `src/graph/cite.rs` | A write that passes the cycle check leaves the `Causal`/`Dependency` graph acyclic (`batch_acyclic`). |
| `Names.lean` | `src/graph/graph/names.rs`, `resolve_reference_scoped` | A name binding is permanent (`names_permanent`) and never resolves to a different concept, even after its target is collected (`resolve_never_rebinds`). |
| `Lease.lean` | `src/store/lease.rs`, lease upsert in `src/store/memory.rs` | The fencing token never decreases (`token_mono`). A live lease refuses other identities (`acquire_refused_while_live`). Once another identity holds the lease, the previous holder's token is refused by the fence forever (`stale_token_refused`, `later_holder_excludes_earlier`). |
| `Receipts.lean` | `src/writeq/receipts.rs`, `admission.rs` | A settled outcome is never overwritten (`settleAll_fixed`). Unsettled receipts are never expired or evicted (`expire_keeps_unsettled`, `evict_keeps_unsettled`). A lane applies writes in submission order (`lane_fifo`). Admission keeps lanes and the queue within their caps. |
| `Reserve.lean` | `src/graph/reserve.rs` | A live reservation excludes other agents (`reserve_denied`). Takeover happens only at or after expiry (`takeover_needs_expiry`). |
| `Contract.lean` | `EmbeddingContract`, `stamp_embedding`, `check_vector_compatibility` | Compatibility is equality of all three fields (`compatible_iff_eq`). A stamped session's contract cannot be changed by ordinary stamps (`stampAll_keeps`). |
| `Fusion.lean` | phase 1 in `candidates_with_keyword_leg_as`, `merge_max`, `LegScores::merged` (`src/recall/candidates.rs`) | Max fusion is monotone in each leg (`merged_mono`). Assigning `1.0` to the named concept's keyword leg can drop it below a weaker lexical neighbour (`assign_lowers_named_concept`, the #288 example). Raising the capped slot instead (#288's first fix) gives a keyword leg of at least `1.0` and at least its BM25 within the over-fetch cap (`raise_keyword_ge_one`, `raise_keyword_ge_bm25`), leaves every other node's leg slots unchanged (`raise_local`), never lowers its fused score or its rank (`raise_no_demotion`, `raise_ahead_le`), and agrees with assigning whenever its BM25 is at most `1.0` or absent (`raise_eq_assign`); a hit beyond the cap still gets only `1.0` (`beyond_cap_floor_only`). The step Rust runs now reads the named concept's BM25 off the whole ranked list: its keyword leg is exactly `max(BM25, 1.0)`, or `1.0` with no hit, wherever the hit lies (`full_keyword_eq`, `full_keyword_ge_bm25`), other nodes' leg slots are unchanged (`full_local`), so an unnamed node beyond the cap stays out of the leg (with `lastScore_of_not_mem`; shown concretely by `full_keeps_beyond_cap_bm25`), its rank cannot get worse against either earlier table (`full_ahead_le`, `full_ahead_le_raise`), it agrees with the capped raise when the hit is within the cap or absent (`full_eq_raise`), and it keeps a beyond-cap BM25 the capped raise dropped (`full_keeps_beyond_cap_bm25`). Locality is about leg slots under max fusion: in Rust `rank` also recomputes every node's `keyword_rank` and `rrf`, and RRF is not modelled. |
| `Audit.lean` | | `#print axioms` for the headline theorems. |

## How far the proofs reach

A proof here is about the *model* in the same file, not about the Rust code.
The link between the two is the reading of the source recorded in each file's
header, and it is only as good as that reading. In particular:

* Arithmetic is exact. `Score.lean` uses fixed-point integers; `f64` rounding
  and overflow are outside the model (the header says where that matters).
* Concurrency is not modelled. The lease, receipt and reservation files prove
  properties of sequences of atomic operations; that each operation is atomic
  is the job of the store transaction or the lock, and is assumed.
* Several algorithms are modelled without an optional or rarely taken path
  (the redundancy skip in `emitted`, truncation of an oversize first block in
  `fit_blocks`, the supersession probe, the operator embedding override). Each
  header lists what it leaves out.
* Predicates whose inputs are measured by store queries (Stage 2 coverage,
  blast radius, eviction score) are modelled from those inputs onward.

If the Rust changes, the model has to be re-read against it; nothing ties the
two together mechanically.
