# #175: evaluated, not adopted

**Decision, 2026-11-12: retire the tested graph-relevance propagation proposal for Lambo's current graph and recall pipeline.** Merge [PR #220](https://github.com/nrynss/lambo/pull/220) to preserve the research, tooling and regressions, and close [#175](https://github.com/nrynss/lambo/issues/175) as not planned. Production recall behavior stays unchanged.

## Why we are stopping

- Neither development-selected arm recovered any of the **three held-out missing facets** across two challenge questions. Direct-control known-facet coverage fell from **56.7%** to **26.7%** with damped propagation and **53.3%** with sender-degree normalization.
- The [main rebase and replay](rebase-2026-11-11.md) reproduced all 88 production candidate pools and every non-timing result across 49 development settings and three held-out settings. The negative result is reproducible on the tested pipeline.
- At the two selected damping values, every held-out missing support has at least 12 competitors no worse in both daemon intercept and propagation slope. Increasing gain alone cannot bring those supports into ordinary top-10; they fail before token fitting.
- An additional model label review found no complete direct-equivalent corrections, but two questions already had substantial partial direct answers. Compound-facet labels and the small enriched sample limit the effectiveness claim further.

This is an engineering decision to stop investing in this proposal. It does not establish that all graph-based ranking, PPR variants or other traversal policies are ineffective. Full candidate precision/recall, final-answer accuracy and release end-to-end latency were not established. There is no successful quality result requiring a rollout or further latency gate.

## What is retained

The census, both pilots, aggregates and replay diagnosis moved together from `evidence/issue-175/` into this research archive. Dated reports and aggregate JSON retain the decisions and fingerprints recorded at each milestone; their earlier instructions to keep the issue open are historical and superseded by this disposition. Raw aggregate JSON is unchanged.

The Python tools remain in `scripts/eval/`. Propagation and the frozen-snapshot operator remain test-only in `src/recall/experiment175*`; the crate-private assembly seam and warm/cold provenance regressions remain available for controlled future studies. Normal production callers supply no graph relevance. No propagation selector, config option or default change is introduced.

Pre-merge review hardened three tool edge cases: duplicate positive-node labels are rejected before they can lose facets, a no-answer-only split returns an empty coverage interval with null bounds, and census schema v2 fingerprints model text instead of publishing a possible local model path. The original frozen labels pass the stricter validation. A fresh census of the frozen snapshot reproduces every archived field after only the declared schema/model-fingerprint conversion; historical JSON remains unchanged. These fixes do not change the measured conclusions.

Private memory, source questions, labels, IDs and vectors remain outside Git. The retained [reports and aggregates](README.md) document their fingerprints and reproduction procedure.

## Ticket disposition and revisit condition

#175 is closed as **not planned** because the evaluated approach is not being adopted; merging the research PR does not claim delivery of the original PPR/runtime acceptance criteria. The #172 umbrella, #190 dataset work and #182 association-dynamics study remain open for their independent scope. Their tracking is updated to remove a pending or successful #175 rollout assumption.

Revisit graph relevance only with a materially different mechanism and concrete unanswered retrieval cases. Any new effectiveness study needs a prospective protocol, fresh independently reviewed question-relative atomic facets and direct alternatives, appropriate graph-only/two-hop/relevant-hub cases, and separate development/held-out topics. These inspected questions are regression cases, not fresh confirmation.
