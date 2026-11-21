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
import LamboProofs.BatchQueue
import LamboProofs.Registry
import LamboProofs.Resolve
import LamboProofs.Tiered

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
#print axioms Lease.beat_latches_iff
#print axioms Lease.override_beat_latches
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
-- empty stems dropped (#301)
#print axioms Canonical.canonicalKeyF_tidy
#print axioms Canonical.keyF_tidy
#print axioms Canonical.keyF_eq_key
#print axioms Canonical.canonicalKeyF_eq_canonicalKey
#print axioms Canonical.nameKeyF_eq_nameKey
#print axioms Canonical.keyF_all_empty
#print axioms Canonical.nameKeyF_all_empty
#print axioms Canonical.keyF_eq_iff_perm
#print axioms Canonical.keyF0_tidy
#print axioms Canonical.issue301_fixed
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
-- word-boundary markers (#299)
#print axioms Dispatch.classifyWB_iff
#print axioms Dispatch.wb_inside_word
#print axioms Dispatch.wb_into_word
#print axioms Dispatch.wb_hyphen_before
#print axioms Dispatch.wb_hyphen_after
#print axioms Dispatch.issue_queries_general
#print axioms Dispatch.classifyWB_marker
#print axioms Dispatch.classifyWB_each_marker
#print axioms Dispatch.classifyWB_structural
#print axioms Dispatch.classifyWB_agrees
#print axioms Dispatch.traversal_exact_wb
#print axioms Dispatch.fitsStructuralWB_iff
#print axioms Dispatch.recall_blend_input_wb
#print axioms Dispatch.general_blends_wb
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
-- the fixed re-embed verb (#296)
#print axioms Contract.reembedCmdFixed_repairs_drift
#print axioms Contract.reembedCmdFixed_full_spec
#print axioms Contract.missingOnly_fixed_refuses_drift
#print axioms Contract.fixed_repin_unsafe_needs_optin
#print axioms Contract.gatePinAfter_only_creates
#print axioms Contract.pinAfterFixed_changes_to_live
#print axioms Contract.pinAfterFixed_no_silent_replace
#print axioms Contract.runAll2_keeps
#print axioms Contract.contract_change_explicit2
#print axioms Contract.runAll2_oneSpace
#print axioms Contract.attachWarnsFixed_relabelled
-- re-embed on a contract-less session (#322)
#print axioms Contract.reembed_contractless_durable_loses_vectors
#print axioms Contract.reembedOrdered_contractless_durable
#print axioms Contract.reembedOrdered_contractless_matches_graph
#print axioms Contract.reembedCmd322_contractless_durable
#print axioms Contract.reembedCmd322_stamped_agrees
#print axioms Contract.reembedOrdered_graph_eq
#print axioms Contract.reembedBatchOrdered_perm
#print axioms Contract.missingOnly_contractless_writes_nothing
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
-- highest score per id (#302)
#print axioms Lambo.Supersession.keepF_best
#print axioms Lambo.Supersession.selectF_order_independent
#print axioms Lambo.Supersession.selectF_swap
#print axioms Lambo.Supersession.selectF_mem
#print axioms Lambo.Supersession.selectF_length
#print axioms Lambo.Supersession.selectF_newer_matches_trigger
#print axioms Lambo.Supersession.selectF_newer_not_replaced
#print axioms Lambo.Supersession.selectF_newer_never_replaced_by_new
#print axioms Lambo.Supersession.selectF_tie_not_newer
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
-- dependence dated by its writer (#300)
#print axioms Policy.reused_action_dependency_is_dated_by_its_writer
#print axioms Policy.own_origin_is_the_writing_turn
#print axioms Policy.dependentOriginsW_not_own
#print axioms Policy.dependency_time_supports
#print axioms Policy.supportingTimesW_eq
#print axioms Policy.recurrenceSessionsW_eq
#print axioms Policy.dependentOriginsW_eq
#print axioms Policy.recurrenceW_mono_edge
#print axioms Policy.dependentOriginsW_mono_edge
#print axioms Policy.causal_not_recurrenceW
#print axioms Policy.policy_cycle_one_hop_fixed
#print axioms Policy.band_mono_fixed_edge
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
-- first and latest writer per edge (#297)
#print axioms Lambo.Conflict.resend_keeps_creator
#print axioms Lambo.Conflict.resend_still_fires
#print axioms Lambo.Conflict.conflictAtFixed_of_unreinforced
#print axioms Lambo.Conflict.conflictAtFixed_of_conflictAt
#print axioms Lambo.Conflict.conflictAtFixed_perm_secondsAgo
#print axioms Lambo.Conflict.issue_297_fixed
#print axioms Lambo.Conflict.second_resend_drops_first_resender
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
-- the cycle since #298
#print axioms Lambo.Hotlist.cycleFixed_inv
#print axioms Lambo.Hotlist.cycleFixed_sound
#print axioms Lambo.Hotlist.cycleFixed_evicts_only_for_held
#print axioms Lambo.Hotlist.cycleFixed_keeps_hits
#print axioms Lambo.Hotlist.cycleFixed_keys_eq_hits
#print axioms Lambo.Hotlist.cycleFixed_perm_cycle_of_no_evict
#print axioms Lambo.Hotlist.cycleFixed_keeps_fresh_hit
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
-- input refusals, per-agent order and the in-session hold (#294, #295)
#print axioms Lambo.Drain.replayStep_inputRefusal_stops
#print axioms Lambo.Drain.loopWith_replayStep
#print axioms Lambo.Drain.replayWith_replayStep
#print axioms Lambo.Drain.stepInput_agrees
#print axioms Lambo.Drain.stepInput_inputRefusal
#print axioms Lambo.Drain.stepInput_refusesInput
#print axioms Lambo.Drain.stepInput_sessionWide
#print axioms Lambo.Drain.fixed_input_goes_on
#print axioms Lambo.Drain.fixed_sessionWide_stops
#print axioms Lambo.Drain.fixed_agrees_with_worker
#print axioms Lambo.Drain.stepFixed_agrees
#print axioms Lambo.Drain.stepFixed_badId
#print axioms Lambo.Drain.stepFixed_refusesInput
#print axioms Lambo.Drain.stepFixed_skip
#print axioms Lambo.Drain.loopD_deferred
#print axioms Lambo.Drain.loopD_applied
#print axioms Lambo.Drain.loopD_failed
#print axioms Lambo.Drain.loopD_skip
#print axioms Lambo.Drain.loopD_stop
#print axioms Lambo.Drain.loopD_facts
#print axioms Lambo.Drain.loopD_no_overtake
#print axioms Lambo.Drain.replayD_cases
#print axioms Lambo.Drain.replayD_facts
#print axioms Lambo.Drain.replayD_applied_pending
#print axioms Lambo.Drain.mem_eq_of_key
#print axioms Lambo.Drain.fixed_agent_order
#print axioms Lambo.Drain.fixed_badId_settled
#print axioms Lambo.Drain.fixedD_input_goes_on
#print axioms Lambo.Drain.minE_spec
#print axioms Lambo.Drain.minE_self
#print axioms Lambo.Drain.swapIn_perm
#print axioms Lambo.Drain.swapIn_length
#print axioms Lambo.Drain.swapIn_epochs
#print axioms Lambo.Drain.regroupN_perm
#print axioms Lambo.Drain.regroupN_epochs
#print axioms Lambo.Drain.regroupN_sorted
#print axioms Lambo.Drain.regroupN_of_sorted
#print axioms Lambo.Drain.insertK_perm
#print axioms Lambo.Drain.loadOrder_perm
#print axioms Lambo.Drain.fixedLoad_perm
#print axioms Lambo.Drain.fixedLoad_sorted
#print axioms Lambo.Drain.fixedLoad_epochs
#print axioms Lambo.Drain.fixedLoad_agrees
#print axioms Lambo.Drain.fixed_epoch_seq_order
#print axioms Lambo.Drain.fixed_agent_order_load
#print axioms Lambo.Drain.orderInv_nil
#print axioms Lambo.Drain.orderInv_perm
#print axioms Lambo.Drain.applyMut_jobs
#print axioms Lambo.Drain.foldl_jobs
#print axioms Lambo.Drain.loopD_preserves
#print axioms Lambo.Drain.replayD_preserves
#print axioms Lambo.Drain.drainHeld_split
#print axioms Lambo.Drain.drainHeld_clears
#print axioms Lambo.Drain.filter_agent_of_all
#print axioms Lambo.Drain.arriveH_split
#print axioms Lambo.Drain.workH_split
#print axioms Lambo.Drain.hold_split
#print axioms Lambo.Drain.pairwise_of_agent_filters
#print axioms Lambo.Drain.session_orderInv
#print axioms Lambo.Drain.hold_then_serve_order
#print axioms Lambo.Drain.arriveH_clears
#print axioms Lambo.Drain.stepQ_agrees
#print axioms Lambo.Drain.stepQ_badId
#print axioms Lambo.Drain.stepQ_unknownFormat
#print axioms Lambo.Drain.stepQ_skip
#print axioms Lambo.Drain.serveQ_block
#print axioms Lambo.Drain.serveQ_log
#print axioms Lambo.Drain.applyLog_append
#print axioms Lambo.Drain.applyLog_keys
#print axioms Lambo.Drain.applyLog_sorted
#print axioms Lambo.Drain.replayP_preserves
#print axioms Lambo.Drain.loopD_settles
#print axioms Lambo.Drain.replayP_progress
#print axioms Lambo.Drain.hold_then_serveQ_order
#print axioms Lambo.Drain.ofAgent_append
#print axioms Lambo.Drain.ofAgent_cons
#print axioms Lambo.Drain.ofAgent_nil
#print axioms Lambo.Drain.mem_ofAgent
#print axioms Lambo.Drain.head_agent
#print axioms Lambo.Drain.ofAgent_single
#print axioms Lambo.Drain.dropFirst_spec
#print axioms Lambo.Drain.ordW_init
#print axioms Lambo.Drain.ordW_move
#print axioms Lambo.Drain.startW_ord
#print axioms Lambo.Drain.finishW_ord
#print axioms Lambo.Drain.stepW_ord
#print axioms Lambo.Drain.runW_ord
#print axioms Lambo.Drain.capsW_shrink
#print axioms Lambo.Drain.startW_caps
#print axioms Lambo.Drain.finishW_caps
#print axioms Lambo.Drain.stepW_caps
#print axioms Lambo.Drain.runW_caps
#print axioms Lambo.Drain.stepW_caps_free
#print axioms Lambo.Drain.refused_wakes
#print axioms Lambo.Drain.retry_settles_head
#print axioms Lambo.Drain.retry_enabled
#print axioms Lambo.Drain.stepW_fenced
#print axioms Lambo.Drain.fence_stops_applies
#print axioms Lambo.Drain.sessionW_orderInv
#print axioms Lambo.Drain.serveR_reported
#print axioms Lambo.Drain.serveR_log
#print axioms Lambo.Drain.sessionW_then_serve_order
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
-- the fence since #293
#print axioms Flush.stepFixed_agrees
#print axioms Flush.takeover_fences_fixed
#print axioms Flush.no_flush_after_fence_fixed
#print axioms Flush.fence_before_due_tick_fixed
#print axioms Flush.stale_write_never_resent
#print axioms Flush.stop_drops_fenced
#print axioms Flush.good_reachable_fixed
#print axioms Flush.no_silent_loss_fixed
#print axioms Flush.close_ok_no_drop
#print axioms Flush.close_ok_over_drop
-- guarded lease and stats calls (#301)
#print axioms Flush.lease_panic_loses
#print axioms Flush.stats_panic_loses
#print axioms Flush.lease_hang_blocks_stop
#print axioms Flush.close_ok_over_panic
#print axioms Flush.stepUnguarded_agrees
#print axioms Flush.closeOkUnguarded_agrees
#print axioms Flush.cycleFixed_erased_irrelevant
#print axioms Flush.good_reachable_guarded
#print axioms Flush.no_silent_loss_guarded
#print axioms Flush.stop_exits_guarded
#print axioms Flush.close_ok_no_drop_guarded
#print axioms Flush.close_ok_accounted_guarded
-- error classes (#301)
#print axioms Flush.permanent_retryable_old
#print axioms Flush.permanent_retried_retained
#print axioms Flush.permanent_one_call
#print axioms Flush.permanent_degrades_classed
#print axioms Flush.permanent_never_resent_classed
#print axioms Flush.transient_retryable_fixed
#print axioms Flush.via_fixed_agrees
#print axioms Flush.stepClassed_agrees
#print axioms Flush.runClassed_old
#print axioms Flush.good_reachable_classed
#print axioms Flush.no_silent_loss_classed
#print axioms Flush.stopped_accounted_classed
#print axioms Flush.close_not_ok_degraded
#print axioms Flush.close_ok_no_drop_classed
#print axioms Flush.close_ok_accounted_classed
-- value-scoped and store-scoped permanent errors (#301)
#print axioms Flush.value_rejected_not_retryable
#print axioms Flush.store_permanent_not_retryable
#print axioms Flush.transient_retryable_split
#print axioms Flush.via_cls
#print axioms Flush.via_split_agrees
#print axioms Flush.value_rejected_dead_letters
#print axioms Flush.store_permanent_degrades_split
#print axioms Flush.no_silent_loss_split
#print axioms Flush.close_ok_accounted_split
-- Candle batch queue
#print axioms Lambo.BatchQueue.answered_at_most_once
#print axioms Lambo.BatchQueue.eventually_all_answered
#print axioms Lambo.BatchQueue.served_when_left_alone
#print axioms Lambo.BatchQueue.shutdown_drains
#print axioms Lambo.BatchQueue.answers_are_own
#print axioms Lambo.BatchQueue.answers_wired
#print axioms Lambo.BatchQueue.forwards_capped
#print axioms Lambo.BatchQueue.forwardIsolated_rowwise
#print axioms Lambo.BatchQueue.take_ends
#print axioms Lambo.BatchQueue.pick_progress
#print axioms Lambo.BatchQueue.takePrioritized_fst
#print axioms Lambo.BatchQueue.lengthChunks_perm
#print axioms Lambo.BatchQueue.chunksOf_eq_lit
#print axioms Lambo.BatchQueue.removeDesc_eq
#print axioms Lambo.BatchQueue.worker_death_permanent
#print axioms Lambo.BatchQueue.lone_transient_fault_permanent
#print axioms Lambo.BatchQueue.queued_bulk_starves
#print axioms Lambo.BatchQueue.test_stream_of_singles
#print axioms Lambo.BatchQueue.long_recall_waits_out_the_take
#print axioms Lambo.BatchQueue.death_after_shutdown_abandons_queue
#print axioms Lambo.BatchQueue.wedged_stays
-- Serve session registry
#print axioms Lambo.Registry.reach_inv
#print axioms Lambo.Registry.mems_le_places
#print axioms Lambo.Registry.attaches_le_permits
#print axioms Lambo.Registry.unplaced_le_cap
#print axioms Lambo.Registry.one_build
#print axioms Lambo.Registry.erase_claim_quiescent
#print axioms Lambo.Registry.erasing_no_build
#print axioms Lambo.Registry.retry_admits_over_held
#print axioms Lambo.Registry.drop_clears
#print axioms Lambo.Registry.timeout_clears
#print axioms Lambo.Registry.retry_waits_previous
#print axioms Lambo.Registry.retry_backoff
#print axioms Lambo.Registry.sweep_skips_in_flight
#print axioms Lambo.Registry.detach_release
#print axioms Lambo.Registry.write_refused_fenced
#print axioms Lambo.Registry.beat_latches
#print axioms Lambo.Registry.stale_quiet
#print axioms Lambo.Registry.watcher_fires_after_latch
#print axioms Lambo.Registry.lost_detach_takes_healthy
#print axioms Lambo.Registry.dropped_flight_fences_next
#print axioms Lambo.Registry.chooseVictim_max
#print axioms Lambo.Registry.at_share_evicts_own
#print axioms Lambo.Registry.placeShare_fair
-- Command resolution and serve preflight
#print axioms Lambo.Resolve.load_refuses_unknown
#print axioms Lambo.Resolve.load_store_no_fallback
#print axioms Lambo.Resolve.resolve_built_allowed
#print axioms Lambo.Resolve.main_built_allowed
#print axioms Lambo.Resolve.resolve_builds_at_most_one
#print axioms Lambo.Resolve.embedder_only_when_needed
#print axioms Lambo.Resolve.verify_builds_embedder_though_needs_embedder_false
#print axioms Lambo.Resolve.store_read_only_iff
#print axioms Lambo.Resolve.full_ok
#print axioms Lambo.Resolve.storeOnly_ok
#print axioms Lambo.Resolve.storeOnly_kind_and_store_differ
#print axioms Lambo.Resolve.echo_never_width_mismatch
#print axioms Lambo.Resolve.cockroach_width_refused
#print axioms Lambo.Resolve.pin_mismatch_refused
#print axioms Lambo.Resolve.search_contract_holds
#print axioms Lambo.Resolve.resolve_ok_arm
#print axioms Lambo.Resolve.main_never_mismatch_or_panic
#print axioms Lambo.Resolve.main_canon
#print axioms Lambo.Resolve.resolve_canon_would_build_store
#print axioms Lambo.Resolve.unknown_key_exit1
#print axioms Lambo.Resolve.preflight_exit_codes
#print axioms Lambo.Resolve.preflight_exit1
#print axioms Lambo.Resolve.preflight_stdio_many
#print axioms Lambo.Resolve.preflight_stdio_ignores_tokens
#print axioms Lambo.Resolve.preflight_bad_flag
#print axioms Lambo.Resolve.preflight_bad_env
#print axioms Lambo.Resolve.preflight_env_wins
#print axioms Lambo.Resolve.preflight_ok_stdio
#print axioms Lambo.Resolve.preflight_ok_http
#print axioms Lambo.Resolve.bind_refused_after_build
#print axioms Lambo.Resolve.heartbeat_without_ledger_after_build
#print axioms Lambo.Resolve.serve_preflight_refusal_builds_nothing
#print axioms Lambo.Resolve.render_noninterference
#print axioms Lambo.Resolve.parseAuthArg_total
#print axioms Lambo.Resolve.statsValid_iff
#print axioms Lambo.Resolve.embedValid_iff
#print axioms Lambo.Resolve.credential_refusal_reveals_equality
#print axioms Lambo.Resolve.serve_web_flag_refusal_leaks
-- auth-token redaction (#315)
#print axioms Lambo.Resolve.parseWebAuthArg_ok_iff
#print axioms Lambo.Resolve.parseWebAuthArg_error_iff
#print axioms Lambo.Resolve.web_flag_refusal_eq_serve
#print axioms Lambo.Resolve.web_flag_refusal_keeps_reason
#print axioms Lambo.Resolve.serve_web_flag_refusal_fixed
#print axioms Lambo.Resolve.hyphen_token_leaked
#print axioms Lambo.Resolve.unexpected_arg_leaked
#print axioms Lambo.Resolve.unexpected_arg_constant
#print axioms Lambo.Resolve.argShown_from_flag
#print axioms Lambo.Resolve.unexpected_arg_fixed
#print axioms Lambo.Resolve.flagTakesHyphen_total
#print axioms Lambo.Resolve.b1_fail_open
#print axioms Lambo.Resolve.authFlagValue_spaced_none_iff
#print axioms Lambo.Resolve.authFlagValue_eq_form
#print axioms Lambo.Resolve.flagTakes_agrees_base
#print axioms Lambo.Resolve.argShownFixed_from_flag
#print axioms Lambo.Resolve.quotedRender_hidden
#print axioms Lambo.Resolve.quotedRender_shown
#print axioms Lambo.Resolve.quotedRender_noninterference
#print axioms Lambo.Resolve.quotedRender_unexpected_is_render
#print axioms Lambo.Resolve.missing_value_kept
#print axioms Lambo.Resolve.re_review_fixed
-- Tiered store
#print axioms Lambo.Tiered.indexSide_keeps_durable
#print axioms Lambo.Tiered.write_commits
#print axioms Lambo.Tiered.lease_mono
#print axioms Lambo.Tiered.mirror_ends
#print axioms Lambo.Tiered.repair_converges
#print axioms Lambo.Tiered.repair_silent_after_loss
#print axioms Lambo.Tiered.erase_silences_repair
#print axioms Lambo.Tiered.mirror_starts_held
#print axioms Lambo.Tiered.settle_consistent
#print axioms Lambo.Tiered.reader_stays_inSync
#print axioms Lambo.Tiered.breaker_open_falls_back
#print axioms Lambo.Tiered.breaker_one_probe
#print axioms Lambo.Tiered.read_breaker_open
#print axioms Lambo.Tiered.finish_overtaken
#print axioms Lambo.Tiered.rerank_exact
#print axioms Lambo.Tiered.rerank_ignores_engine
#print axioms Lambo.Tiered.knnFetch_bounds
#print axioms Lambo.Tiered.nextVersion_gt
#print axioms Lambo.Tiered.noteRefresh_never_adopts
#print axioms Lambo.Tiered.reembed_migration_misindexed
#print axioms Lambo.Tiered.inplace_reembed_mixes_builds
#print axioms Lambo.Tiered.reader_never_rechecks
#print axioms Lambo.Tiered.mirror_lands_after_erase
#print axioms Lambo.Tiered.repair_from_pre_recheck_snapshot
#print axioms Lambo.Tiered.edge_only_batch_leaves_marker_behind
#print axioms Lambo.Tiered.rerank_partial_not_exact
#print axioms Lambo.Tiered.stale_pass_rewinds_version
-- contract-moving batches (#319)
#print axioms Lambo.Tiered.switches_noSet
#print axioms Lambo.Tiered.switches_stampFirst
#print axioms Lambo.Tiered.switches_names
#print axioms Lambo.Tiered.mirror_agrees
#print axioms Lambo.Tiered.write_agrees
#print axioms Lambo.Tiered.write_agrees_noSet
#print axioms Lambo.Tiered.write_agrees_stampFirst
#print axioms Lambo.Tiered.mirror_switch_writes_nothing
#print axioms Lambo.Tiered.write_switch_writes_nothing
#print axioms Lambo.Tiered.reembed_migration_held_back
#print axioms Lambo.Tiered.reembed_migration_repaired
#print axioms Lambo.Tiered.relabel_leaves_new_index_empty
#print axioms Lambo.Tiered.projectFinal_misses_relabel
#print axioms Lambo.Tiered.projectFinal_ahead_of_durable
#print axioms Lambo.Tiered.reorder_changes_durable
