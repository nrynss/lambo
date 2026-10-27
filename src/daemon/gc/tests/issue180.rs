//! Issue #180 measurement on a dogfood snapshot. Not part of CI: it needs a
//! standalone copy of the dogfood database.
//!
//! ```text
//! sqlite3 ~/lambo-dogfood/lambo-dev.db ".backup '/tmp/lambo-180.db'"
//! # The dogfood file still stores the pre-#55 text literal. Scoring does
//! # not read the vector, and the current loader only accepts packed f32,
//! # so the copy drops the blobs before load.
//! sqlite3 /tmp/lambo-180.db "UPDATE concepts SET embedding = NULL"
//! LAMBO_ISSUE180_DB=/tmp/lambo-180.db \
//! LAMBO_ISSUE180_OUT=evidence/issue-180/ranking.json \
//!   cargo test --features store-sqlite --lib \
//!   issue180_dogfood -- --ignored --nocapture
//! ```

use std::collections::{BTreeMap, HashMap, HashSet};

use chrono::Utc;

use super::super::{
    eviction_score, eviction_threshold, has_gc_connection, reachable_from_temporal_chain,
    resources_with_dependents, GcParams, MIN_CONCEPT_SCORE,
};
use crate::config::ScoringWeights;
use crate::daemon::score::{
    rescore, score, score_concept, score_live_plus_frequency, SessionContext,
};
use crate::graph::Graph;
use crate::store::GraphStore;
use crate::types::{tie_break_by_key, NodeId};

#[tokio::test]
#[ignore = "dogfood snapshot; set LAMBO_ISSUE180_DB to a standalone sqlite copy"]
async fn issue180_dogfood_ranking_and_gc_cut() {
    let path = std::env::var("LAMBO_ISSUE180_DB").expect(
        "LAMBO_ISSUE180_DB must be a standalone copy of the dogfood database \
         (see the module docs); the live file is not opened",
    );
    let store = crate::store::SqliteStore::connect_read_only(std::path::Path::new(&path))
        .expect("open the snapshot copy");
    let session = crate::types::SessionId::from("lambo-dev");
    let snap = store.load_session(&session).await.expect("load lambo-dev");
    let graph = Graph::from_snapshot(snap).expect("snapshot is a graph");
    let weights = ScoringWeights::default();
    let ctx = SessionContext::compute(&graph);

    let mut histogram: BTreeMap<usize, usize> = BTreeMap::new();
    for c in graph.concepts() {
        *histogram.entry(ctx.derivations(c.id)).or_default() += 1;
    }
    let mut derivation_histogram = serde_json::Map::new();
    for (k, n) in &histogram {
        derivation_histogram.insert(k.to_string(), serde_json::json!(n));
    }

    let recall = ranked(&graph, &ctx, &weights, Activity::Recall);
    let ratio = ranked(&graph, &ctx, &weights, Activity::Ratio);
    let logged = ranked(&graph, &ctx, &weights, Activity::Log);
    let table = rescore(&graph, &weights);
    assert_eq!(recall.len(), table.len());
    for (row, scored) in recall.iter().zip(&table) {
        assert_eq!(row.id, scored.item);
        assert_eq!(
            row.score.to_bits(),
            scored.score.to_bits(),
            "the recall ranking must be the daemon table"
        );
    }

    let (tau_linear, conc_linear, disc_linear) = kendall(&ratio, &recall);
    let (tau_log, conc_log, disc_log) = kendall(&ratio, &logged);
    let moved = rank_moves(&ratio, &recall);

    let now = Utc::now();
    let params = GcParams {
        now,
        ..GcParams::default()
    };
    assert_eq!(params.min_concept_score, MIN_CONCEPT_SCORE);
    // A second clock past the recency window. The live snapshot is young, so
    // a sweep today can show an empty cut for both formulas; the aged clock
    // is the case #180 is about, where recency no longer carries the score.
    let aged = GcParams {
        now: now + chrono::Duration::days(366),
        ..params
    };
    let gc = gc_report(&graph, params);
    let gc_aged = gc_report(&graph, aged);
    let report = serde_json::json!({
        "session": "lambo-dev",
        "measured_at": now.to_rfc3339(),
        "aged_at": aged.now.to_rfc3339(),
        "concepts": graph.concepts().count(),
        "interactions": graph.interactions().count(),
        "max_derivations": ctx.max_derivations(),
        "derivation_histogram": derivation_histogram,
        "kendall": {
            "definition": "(concordant - discordant) / (concordant + discordant) on the daemon table order, tie-broken the way rescore breaks ties",
            "linear_vs_ratio": {
                "tau": tau_linear,
                "concordant": conc_linear,
                "discordant": disc_linear,
                "ranks_moved": moved.moved,
                "largest_climb": moved.largest_climb,
            },
            "log_vs_ratio": {
                "tau": tau_log,
                "concordant": conc_log,
                "discordant": disc_log,
            },
        },
        "gc": gc,
        "gc_aged_366d": gc_aged,
        "multi_derived_rank_moves": moved.notable,
    });

    let text = serde_json::to_string_pretty(&report).unwrap();
    println!("{text}");
    if let Ok(out) = std::env::var("LAMBO_ISSUE180_OUT") {
        std::fs::write(&out, text).unwrap_or_else(|e| panic!("write {out}: {e}"));
    }
}

#[derive(Clone, Copy)]
enum Activity {
    /// Max-normalized, what [`rescore`] publishes.
    Recall,
    /// `derived_by / |interactions|`, what GC and the old table used.
    Ratio,
    /// `ln(1+k) / ln(1+k_max)`, the variant #180 leaves unused.
    Log,
}

struct Row {
    id: NodeId,
    score: f64,
    key: String,
    content: String,
    derivations: usize,
}

fn ranked(
    graph: &Graph,
    ctx: &SessionContext,
    weights: &ScoringWeights,
    activity: Activity,
) -> Vec<Row> {
    let max = ctx.max_derivations();
    let mut rows: Vec<Row> = graph
        .concepts()
        .map(|c| {
            let mut dims = score_concept(graph, c, ctx);
            let k = ctx.derivations(c.id);
            match activity {
                Activity::Recall => {}
                Activity::Ratio => {
                    dims.session_activity = ctx.per_interaction_activity(c.id);
                }
                Activity::Log => {
                    dims.session_activity = if max == 0 {
                        0.0
                    } else {
                        ((1 + k) as f64).ln() / ((1 + max) as f64).ln()
                    };
                }
            }
            Row {
                id: c.id,
                score: score(dims, weights),
                key: c.canonical_key.clone(),
                content: c.content.clone(),
                derivations: k,
            }
        })
        .collect();
    rows.sort_by(|a, b| {
        b.score.total_cmp(&a.score).then_with(|| {
            tie_break_by_key(Some(a.key.as_str()), &a.id, Some(b.key.as_str()), &b.id)
        })
    });
    rows
}

fn kendall(before: &[Row], after: &[Row]) -> (f64, u64, u64) {
    let pos: HashMap<NodeId, usize> = after
        .iter()
        .enumerate()
        .map(|(i, row)| (row.id, i))
        .collect();
    let mut concordant = 0u64;
    let mut discordant = 0u64;
    for i in 0..before.len() {
        for j in (i + 1)..before.len() {
            let (pi, pj) = (pos[&before[i].id], pos[&before[j].id]);
            if pi < pj {
                concordant += 1;
            } else if pi > pj {
                discordant += 1;
            }
        }
    }
    let denom = concordant + discordant;
    let tau = if denom == 0 {
        1.0
    } else {
        (concordant as f64 - discordant as f64) / denom as f64
    };
    (tau, concordant, discordant)
}

struct Moves {
    moved: usize,
    largest_climb: i64,
    notable: Vec<serde_json::Value>,
}

fn rank_moves(before: &[Row], after: &[Row]) -> Moves {
    let pos: HashMap<NodeId, usize> = after
        .iter()
        .enumerate()
        .map(|(i, row)| (row.id, i))
        .collect();
    let mut moved = 0usize;
    let mut largest_climb = 0i64;
    let mut notable = Vec::new();
    for (old, row) in before.iter().enumerate() {
        let new = pos[&row.id];
        let climb = old as i64 - new as i64;
        if old != new {
            moved += 1;
        }
        if climb > largest_climb {
            largest_climb = climb;
        }
        if row.derivations > 1 {
            notable.push(serde_json::json!({
                "derivations": row.derivations,
                "old_rank": old,
                "new_rank": new,
                "climb": climb,
                "content": truncate(&row.content),
            }));
        }
    }
    notable.sort_by(|a, b| {
        b["climb"]
            .as_i64()
            .unwrap_or(0)
            .cmp(&a["climb"].as_i64().unwrap_or(0))
    });
    Moves {
        moved,
        largest_climb,
        notable,
    }
}

fn truncate(s: &str) -> String {
    let mut out: String = s.chars().take(100).collect();
    if s.chars().count() > 100 {
        out.push('…');
    }
    out
}

struct GcSets {
    /// Score under the bar, among concepts the sweep would score at all
    /// (not protected, not exempt). Not a collection count.
    ratio_under: HashSet<NodeId>,
    maxnorm_under: HashSet<NodeId>,
    /// `gc::run`'s step-2 `below` list: under the bar, connected, reachable,
    /// and not spared as a Resource with dependents. Orphans and
    /// disconnected components are not in this set.
    step2_ratio: HashSet<NodeId>,
    step2_maxnorm: HashSet<NodeId>,
    /// How many of those step-2 candidates one sweep takes after orphans and
    /// disconnected components have consumed the collection cap.
    sweep_taken_ratio: usize,
    sweep_taken_maxnorm: usize,
    cap: usize,
    structural_before_score_cut: usize,
    bit_mismatches: usize,
}

fn gc_report(graph: &Graph, params: GcParams) -> serde_json::Value {
    let gc = gc_sets(graph, params);
    assert_eq!(
        gc.bit_mismatches, 0,
        "eviction_score must be the per-interaction ratio on every concept"
    );
    assert!(
        gc.maxnorm_under.is_subset(&gc.ratio_under),
        "max-normalized activity is at least the ratio, so it cannot put a \
         concept under the bar that the ratio left above it"
    );
    assert!(gc.step2_maxnorm.is_subset(&gc.step2_ratio));
    // The ratio column is what `gc::run` takes from the score cut. Orphans,
    // disconnected components and the post-collection cascade may add other
    // ids; every score-cut id the sweep collects has to be one of ours, and
    // the count has to be the cap's take.
    let mut swept = graph.clone();
    let outcome = super::super::run(&mut swept, params);
    let from_cut = outcome
        .concepts_collected
        .iter()
        .filter(|id| gc.step2_ratio.contains(id))
        .count();
    assert_eq!(
        from_cut,
        gc.sweep_taken_ratio,
        "the step-2 take must match gc::run (collected {}, cap {})",
        outcome.concepts_collected.len(),
        gc.cap
    );
    serde_json::json!({
        "below_bar_scored_ratio": gc.ratio_under.len(),
        "below_bar_scored_maxnorm": gc.maxnorm_under.len(),
        "step2_candidates_ratio": gc.step2_ratio.len(),
        "step2_candidates_maxnorm": gc.step2_maxnorm.len(),
        "step2_candidates_only_in_ratio": gc.step2_ratio.difference(&gc.step2_maxnorm).count(),
        "collection_cap": gc.cap,
        "structural_before_score_cut": gc.structural_before_score_cut,
        "one_sweep_score_cut_taken_ratio": gc.sweep_taken_ratio,
        "one_sweep_score_cut_taken_maxnorm": gc.sweep_taken_maxnorm,
        "eviction_score_matches_ratio": true,
    })
}

/// The step-2 candidate set `gc::run` would build, and the same set if the
/// cut used recall's max-normalized activity.
///
/// Order matches `run`: protection (including a concept that contradicts a
/// promoted one, and a live correction) is fixed before step 1; decayed
/// edges are removed; then the context, dependents and reachability are
/// read. The cap is applied the way `run` applies it — orphans, then
/// disconnected components, then the score cut — so the taken counts are
/// not the candidate-set sizes.
fn gc_sets(graph: &Graph, params: GcParams) -> GcSets {
    use super::super::{
        contradicts_a_promoted_concept, dead_edge_ids, is_protected, live_corrections,
    };

    let mut graph = graph.clone();
    let goals = graph.root_goal_texts();
    let mut protected: HashSet<NodeId> = graph
        .concepts()
        .filter(|c| {
            is_protected(c, &goals) || contradicts_a_promoted_concept(&graph, c.id, params.now)
        })
        .map(|c| c.id)
        .collect();
    protected.extend(live_corrections(&graph, &protected, params));
    for eid in dead_edge_ids(&graph, params) {
        let _ = graph.remove_edge(eid);
    }
    let ctx = SessionContext::compute(&graph);
    let depended = resources_with_dependents(&graph);
    let reachable = reachable_from_temporal_chain(&graph);
    let mut orphans: Vec<NodeId> = Vec::new();
    let mut out = GcSets {
        ratio_under: HashSet::new(),
        maxnorm_under: HashSet::new(),
        step2_ratio: HashSet::new(),
        step2_maxnorm: HashSet::new(),
        sweep_taken_ratio: 0,
        sweep_taken_maxnorm: 0,
        cap: 0,
        structural_before_score_cut: 0,
        bit_mismatches: 0,
    };
    for c in graph.concepts() {
        if protected.contains(&c.id) {
            continue;
        }
        if !has_gc_connection(&graph, c.id) {
            orphans.push(c.id);
            continue;
        }
        if c.concept_type.exempt_from_gc_score_cut() {
            continue;
        }
        let mut dims = score_concept(&graph, c, &ctx);
        dims.recency = crate::daemon::gc::eviction_recency(c, params.now, params.recency_window);
        let recall_activity = dims.session_activity;
        dims.session_activity = ctx.per_interaction_activity(c.id);
        let ratio = score_live_plus_frequency(dims, &params.weights);
        let actual = eviction_score(&graph, c, &ctx, params);
        if actual.to_bits() != ratio.to_bits() {
            out.bit_mismatches += 1;
        }
        dims.session_activity = recall_activity;
        let maxnorm = score_live_plus_frequency(dims, &params.weights);
        let bar = eviction_threshold(params.min_concept_score, c.concept_type);
        if ratio < bar {
            out.ratio_under.insert(c.id);
            if !depended.contains(&c.id) && reachable.contains(&c.id) {
                out.step2_ratio.insert(c.id);
            }
        }
        if maxnorm < bar {
            out.maxnorm_under.insert(c.id);
            if !depended.contains(&c.id) && reachable.contains(&c.id) {
                out.step2_maxnorm.insert(c.id);
            }
        }
    }
    let orphan_set: HashSet<NodeId> = orphans.iter().copied().collect();
    let disconnected = graph
        .concepts()
        .map(|c| c.id)
        .filter(|id| !reachable.contains(id) && !protected.contains(id) && !orphan_set.contains(id))
        .count();
    let structural = orphans.len() + disconnected;
    let unprotected = graph
        .concepts()
        .filter(|c| !protected.contains(&c.id))
        .count();
    let cap = super::super::collection_cap(unprotected, params);
    out.cap = cap;
    out.structural_before_score_cut = structural;
    out.sweep_taken_ratio = out.step2_ratio.len().min(cap.saturating_sub(structural));
    out.sweep_taken_maxnorm = out.step2_maxnorm.len().min(cap.saturating_sub(structural));
    out
}
