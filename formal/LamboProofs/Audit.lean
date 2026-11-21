import LamboProofs.Score
import LamboProofs.Order
import LamboProofs.Expand
import LamboProofs.TopK
import LamboProofs.Fit
import LamboProofs.Canon
import LamboProofs.Gc
import LamboProofs.Cite
import LamboProofs.Names
import LamboProofs.Lease
import LamboProofs.Receipts
import LamboProofs.Reserve
import LamboProofs.Contract
import LamboProofs.Fusion

/-
# Axiom audit

`#print axioms` lists every axiom a theorem depends on. For this development
the expected output for each line is either "does not depend on any axioms" or
a subset of Lean's three standard axioms: `propext`, `Classical.choice` and
`Quot.sound`. `sorryAx` must never appear; CI fails the build if it does
(see `formal/ci/lean.yml`).
-/

open Lambo

-- Scoring
#print axioms Score.score_range
#print axioms Score.rawScore_le
#print axioms Score.score_mono
#print axioms Score.live_le_livePlusFrequency
#print axioms Score.le_listMax

-- Ranking comparators
#print axioms Order.rankMembers_lawful
#print axioms Order.rankMembers_separating
#print axioms Order.rankMembers_canonical_first
#print axioms Order.sorted_unique

-- Recall: expansion, top-K, token budget
#print axioms Expand.expand_sound
#print axioms Expand.expand_complete
#print axioms Expand.expand_nodup
#print axioms TopK.forced_mem_emit
#print axioms TopK.emit_free_le_topK
#print axioms TopK.emit_free_eq_take
#print axioms Fit.fit_prefix_ok
#print axioms Fit.fit_stops_at_first_miss
#print axioms Fit.fit_longest

-- Canonization and garbage collection
#print axioms Canon.legal_demotion_unique
#print axioms Canon.cycle_one_hop
#print axioms Canon.cycle_never_skips
#print axioms Canon.stage3_no_overflow
#print axioms Gc.venerable_canonical_never_collected
#print axioms Gc.bump_never_resets

-- Graph writes
#print axioms Cite.batch_acyclic
#print axioms Names.names_permanent
#print axioms Names.resolve_never_rebinds

-- Writer lease, receipts, reservations, embedding contract
#print axioms Lease.token_mono
#print axioms Lease.stale_token_refused
#print axioms Lease.later_holder_excludes_earlier
#print axioms Receipts.settleAll_fixed
#print axioms Receipts.expire_keeps_unsettled
#print axioms Receipts.lane_fifo
#print axioms Reserve.takeover_needs_expiry
#print axioms Contract.stampAll_keeps

-- Phase-1 leg fusion and the exact-name step (#288)
#print axioms Fusion.merged_mono
#print axioms Fusion.assign_lowers_named_concept
#print axioms Fusion.raise_keyword_ge_one
#print axioms Fusion.raise_keyword_ge_bm25
#print axioms Fusion.raise_local
#print axioms Fusion.raise_no_demotion
#print axioms Fusion.raise_ahead_le
#print axioms Fusion.raise_eq_assign
