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
import LamboProofs.Confidence
import LamboProofs.Canonical
import LamboProofs.Dispatch
import LamboProofs.Supersession
import LamboProofs.Policy
import LamboProofs.Conflict
import LamboProofs.Hotlist
import LamboProofs.Drain
import LamboProofs.Flush

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
#print axioms Fusion.full_keyword_ge_bm25
#print axioms Fusion.full_keyword_eq
#print axioms Fusion.full_local
#print axioms Fusion.full_ahead_le
#print axioms Fusion.full_ahead_le_raise
#print axioms Fusion.full_eq_raise
#print axioms Fusion.full_keeps_beyond_cap_bm25

-- Top-K emission: redundancy skip (#181)
#print axioms TopK.emitR_sublist
#print axioms TopK.forced_mem_emitR
#print axioms TopK.emitR_free_count
#print axioms TopK.emitR_eq_emit_of_no_redundant_pair
#print axioms TopK.emitR_eq_emit_of_no_redundant_distinct_pair
#print axioms TopK.skipsR_nil_of_no_red_distinct
#print axioms TopK.emitR_window_zero
#print axioms TopK.skipsR_spec
#print axioms TopK.skipsR_pairwise
#print axioms TopK.skipsR_not_emitted
#print axioms TopK.skipsR_length_le
#print axioms TopK.skipped_canonical_kept_canonical
-- Gated Canonical-first (#280, design)
#print axioms Order.gatedRankMembers_off
#print axioms Order.gatedRankMembers_lawful
#print axioms Order.gatedRankMembers_separating
#print axioms Order.gatedRankMembers_promoted_first
#print axioms Order.gatedRankMembers_below_gate_score
#print axioms Order.gatedRankMembers_sorted_unique
-- Recall confidence
#print axioms Confidence.provisional_inUnit
#print axioms Confidence.keywordTerm_half
#print axioms Confidence.keywordTerm_tendsto_one
#print axioms Confidence.provisional_mono_bm25
#print axioms Confidence.provisional_mono_cosine
#print axioms Confidence.noisyOr_ge_keyword
#print axioms Confidence.noisyOr_eq_one_iff
#print axioms Confidence.keywordOnly_le_noisyOr
#print axioms Confidence.provisional_basis_iff
#print axioms Confidence.provisional_twoLeg_without_median
#print axioms Confidence.provisional_exactName
#print axioms Confidence.topLegs_keywordPhase
#print axioms Confidence.keywordLeg_bm25_best
#print axioms Confidence.topLegs_vector_max
#print axioms Confidence.provisional_isSome_of_built
-- Canonical keys and names
#print axioms Canonical.key_perm
#print axioms Canonical.key_segments_perm
#print axioms Canonical.key_sep_alike
#print axioms Canonical.key_lower
#print axioms Canonical.key_stop_segment
#print axioms Canonical.key_all_stop
#print axioms Canonical.key_eq_iff_perm
#print axioms Canonical.key_idem
#print axioms Canonical.nameKey_eq_iff_perm
#print axioms Canonical.nameKey_all_stop
#print axioms Canonical.issue290_order
#print axioms Canonical.issue290_stopword
#print axioms Canonical.ons_not_idempotent
#print axioms Canonical.leas_not_idempotent
#print axioms Canonical.empty_stem_key
-- Reciprocal rank fusion (#173) and the name step under RRF
#print axioms Fusion.rrf_range
#print axioms Fusion.rrfSum_mono
#print axioms Fusion.legRank_lt_iff
#print axioms Fusion.legRank_perm
#print axioms Fusion.rrf_perm
#print axioms Fusion.rrf_one_leg_agrees
#print axioms Fusion.rrf_full_named_up
#print axioms Fusion.rrf_full_other
#print axioms Fusion.rrf_full_ahead_le
#print axioms Fusion.full_passes_only_below_floor
#print axioms Fusion.full_rank_eq_raise
#print axioms Fusion.full_rrf_eq_raise
#print axioms Fusion.rrf_name_step_reorders_unnamed
#print axioms Fusion.rrf_beyond_cap_shifts_neighbour
-- Dependency-question dispatch
#print axioms Dispatch.classify_iff
#print axioms Dispatch.classify_marker
#print axioms Dispatch.classify_inside_word
#print axioms Dispatch.classify_into_word
#print axioms Dispatch.depends_only_structural
#print axioms Dispatch.depend_not_marker
#print axioms Dispatch.dependence_types
#print axioms Dispatch.mem_ranked
#print axioms Dispatch.weight_spec
#print axioms Dispatch.ranked_sorted
#print axioms Dispatch.ranked_canonical_first
#print axioms Dispatch.ranked_unique
#print axioms Dispatch.traversal_sound
#print axioms Dispatch.traversal_complete
#print axioms Dispatch.traversal_exact
#print axioms Dispatch.traversal_length_le
#print axioms Dispatch.traversal_canonical_first
#print axioms Dispatch.fitsStructuralWith_iff
#print axioms Dispatch.fitsStructural_iff
#print axioms Dispatch.recall_blend_input
#print axioms Dispatch.recall_one_snapshot
#print axioms Dispatch.recall_skip_fires
#print axioms Dispatch.marker_no_dependents_blends
-- Embedding contract: override and re-embed
#print axioms Contract.attach_no_override_ok_iff
#print axioms Contract.attach_override_ok_iff
#print axioms Contract.contract_change_explicit
#print axioms Contract.reembed_one_space
#print axioms Contract.no_mixing_without_override
#print axioms Contract.attach_override_relabel_mixes
#print axioms Contract.reembedCmd_refuses_same_contract
#print axioms Contract.reembed_repins_without_warning
#print axioms Contract.missingOnly_skips_canary
-- Context fitting with truncation (#130)
#print axioms Fit.fitBlocks_fits
#print axioms Fit.fitBlocks_of_first_fits
#print axioms Fit.fitBlocks_whole
#print axioms Fit.fitBlocks_rendered_pos
#print axioms Fit.fitBlocks_context_ne_nil
#print axioms Fit.fitBlocks_truncated_prefix
#print axioms Fit.mem_contentCuts
#print axioms Fit.bsearch_spec
#print axioms Fit.truncFirst_longest
#print axioms Fit.fitBlocks_rendered_zero_iff
#print axioms Fit.fitBlocks_rendered_zero_iff_of_le
#print axioms Fit.rust_default_steady
#print axioms Fit.rust_default_longest
#print axioms Fit.rust_rule3_of_mono
#print axioms Fit.rust_jump_skips_a_fitting_form
#print axioms Fit.jump_skips_a_fitting_form
#print axioms Fit.jump_renders_without_the_marker
-- Supersession (#20, #198)
#print axioms Lambo.Supersession.candidate_symm
#print axioms Lambo.Supersession.candidate_dir
#print axioms Lambo.Supersession.candidate_tie
#print axioms Lambo.Supersession.recordRefused_wellDated
#print axioms Lambo.Supersession.suspected_acyclic
#print axioms Lambo.Supersession.select_newer_matches_trigger
#print axioms Lambo.Supersession.select_newer_never_replaced_by_new
#print axioms Lambo.Supersession.verdict_sound
#print axioms Lambo.Supersession.verdict_ne
#print axioms Lambo.Supersession.pair_rule
#print axioms Lambo.Supersession.verdict_outranks_reverse
#print axioms Lambo.Supersession.applySup_unique
#print axioms Lambo.Supersession.applySup_eq_cycle
#print axioms Lambo.Supersession.off_agrees
#print axioms Lambo.Supersession.probesFor_ignore_suspected
#print axioms Lambo.Supersession.signal_suspected
#print axioms Lambo.Supersession.pss_newer
-- Solo policy
#print axioms Policy.sessionCount_perm
#print axioms Policy.sessionCount_max
#print axioms Policy.sessionCount_attained
#print axioms Policy.sessionCount_mono
#print axioms Policy.sessionCount_zero_gap
#print axioms Policy.sessionCount_window
#print axioms Policy.causal_not_recurrence
#print axioms Policy.supportingTimes_reflush
#print axioms Policy.a_bulk_ingest_recurs_only_under_event_time
#print axioms Policy.score10_mono
#print axioms Policy.classify_mono
#print axioms Policy.policy_cycle_one_hop
#print axioms Policy.solo_cycle_capped
#print axioms Policy.mem_soloCandidates
#print axioms Policy.parse_sound
#print axioms Policy.hierarchical_child_is_dependent_not_recurrence
#print axioms Policy.reused_action_dependency_is_dated_by_its_origin
#print axioms Policy.exact_tie_orders_by_key
#print axioms Policy.spread_days_on_the_boundary
-- Conflict detection and contradiction dating
#print axioms Lambo.Conflict.conflictAt_isSome_iff
#print axioms Lambo.Conflict.single_agent_never_conflicts
#print axioms Lambo.Conflict.qualifies_iff
#print axioms Lambo.Conflict.conflictAt_perm_secondsAgo
#print axioms Lambo.Conflict.excluded_edge_irrelevant
#print axioms Lambo.Conflict.lapse_permanent
#print axioms Lambo.Conflict.resend_drops_original_writer
#print axioms Lambo.Conflict.declaredAt_le_createdAt
#print axioms Lambo.Conflict.declaredAt_mono_write
#print axioms Lambo.Conflict.backdated_not_eligible
#print axioms Lambo.Conflict.contradictionAt_no_cutoff
#print axioms Lambo.Conflict.contradictionAt_sound
#print axioms Lambo.Conflict.contradictionAt_perm
-- Daemon hot list (#179, #196)
#print axioms Lambo.Hotlist.enforce_length_le
#print axioms Lambo.Hotlist.enforce_countC_le
#print axioms Lambo.Hotlist.enforce_evicts_lowest
#print axioms Lambo.Hotlist.enforce_keeps_reserved
#print axioms Lambo.Hotlist.insertFull_evicts_le_one
#print axioms Lambo.Hotlist.insert_refresh
#print axioms Lambo.Hotlist.insertFull_inv
#print axioms Lambo.Hotlist.insertMany_inv
#print axioms Lambo.Hotlist.prio_tie_inv
#print axioms Lambo.Hotlist.insertAll_order_irrelevant
#print axioms Lambo.Hotlist.order_matters_when_full
#print axioms Lambo.Hotlist.mem_hotPayloads
#print axioms Lambo.Hotlist.lapsed_not_forced
#print axioms Lambo.Hotlist.windowCheck_none_iff
#print axioms Lambo.Hotlist.forces_iff
#print axioms Lambo.Hotlist.hot_member_emitted
#print axioms Lambo.Hotlist.hot_member_emittedR
#print axioms Lambo.Hotlist.collected_not_emitted
#print axioms Lambo.Hotlist.cycle_drops_collected
#print axioms Lambo.Hotlist.cycle_evicts_fresh_hit
-- Write-queue drain and replay (#116, #117)
#print axioms Lambo.Drain.drain_fifo
#print axioms Lambo.Drain.drain_settles_each
#print axioms Lambo.Drain.crash_never_reapplies
#print axioms Lambo.Drain.close_settles_all
#print axioms Lambo.Drain.shutdown_no_silent_loss
#print axioms Lambo.Drain.replay_drained
#print axioms Lambo.Drain.replay_idempotent
#print axioms Lambo.Drain.replay_complete_idem
#print axioms Lambo.Drain.replay_seq_order
#print axioms Lambo.Drain.replay_epoch_seq_order
#print axioms Lambo.Drain.inputFact_goes_on
#print axioms Lambo.Drain.sessionWide_stops
#print axioms Lambo.Drain.transient_skip_reorders
#print axioms Lambo.Drain.outage_then_replay_reorders
#print axioms Lambo.Drain.clock_step_reorders
#print axioms Lambo.Drain.outage_failed_then_applied
#print axioms Lambo.Drain.store_error_blocks_backlog
-- Store flush path
#print axioms Flush.flushWithRetry_backoff
#print axioms Flush.ladder_exhausted
#print axioms Flush.ladder_ok_effect
#print axioms Flush.upsert_idempotent
#print axioms Flush.good_reachable
#print axioms Flush.step_calls_unconsumed
#print axioms Flush.never_resent
#print axioms Flush.committed_sublist
#print axioms Flush.transient_retried
#print axioms Flush.exhausted_retained
#print axioms Flush.constraint_dead_lettered
#print axioms Flush.stale_write_one_call
#print axioms Flush.stale_write_retained
#print axioms Flush.override_refresh_retakes
#print axioms Flush.flush_after_fence_latched
#print axioms Flush.stop_requeues
#print axioms Flush.stopped_accounted
#print axioms Flush.no_silent_loss
#print axioms Flush.abort_loses
#print axioms Flush.fencing_refuses_after_takeover
#print axioms Flush.degraded_silent
