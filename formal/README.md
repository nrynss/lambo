# Lean 4 proofs about Lambo

This directory holds a Lean 4 development that models parts of the Lambo engine
and proves properties the source states in its doc comments and pins in its
tests. It has no dependencies beyond Lean itself (no Mathlib).

## Status: not yet machine-checked

**These files have not been compiled.** They were written in a cloud session
where running a Lean toolchain was not permitted, and the session's GitHub
credential could not add a workflow file, so neither a local `lake build` nor
CI has ever run on them. Each proof was written by hand and then read line by
line by an independent reviewer against the Lean `v4.36.0` core source text.
That review found one real error class (nine uses of `Option.noConfusion`,
whose shape changed in 4.36) and one deprecated name, both fixed. A read-through
is not the same thing as Lean accepting the files.

Until `lake build` passes, treat every theorem here as a *claim with a proposed
proof*. Expect some proofs to need small repairs (a renamed library lemma, a
tactic that closes a goal one step earlier than assumed). The statements and
the models are the part that took the care; they were written against the Rust
source at commit `c3be141`.

To check them:

```bash
curl https://elan.lean-lang.org/elan-init.sh -sSf | sh
```

```bash
cd formal && lake build
```

`lean-toolchain` pins Lean `v4.36.0`. A clean build prints the axiom audit from
`LamboProofs/Audit.lean`; no line of it may mention `sorryAx`.

To have CI check them on every change, move `ci/lean.yml` to
`.github/workflows/lean.yml`. It downloads the pinned Lean release, verifies its
SHA-256, builds, and fails on any `sorry`. It uses no third-party action beyond
the checkout pin `ci.yml` already uses.

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
