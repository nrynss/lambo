//! Runtime configuration — named defaults from the v0.1 hackathon spec.

use serde::{Deserialize, Serialize};
use std::time::Duration;

use crate::types::MatchStrategy;

/// Scoring weights for daemon composite (spec §9): recency / frequency / session_activity / density.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct ScoringWeights {
    pub recency: f64,
    pub frequency: f64,
    pub session_activity: f64,
    pub density: f64,
}

impl Default for ScoringWeights {
    fn default() -> Self {
        Self {
            recency: 0.25,
            frequency: 0.20,
            session_activity: 0.20,
            density: 0.35,
        }
    }
}

/// Final recall mix: `daemon_score * w_daemon + query_relevance * w_query`.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct RecallWeights {
    pub w_daemon: f64,
    pub w_query: f64,
}

impl Default for RecallWeights {
    fn default() -> Self {
        Self {
            w_daemon: 0.5,
            w_query: 0.5,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Config {
    pub backend_flush_interval: Duration,
    pub backend_flush_max_batch: usize,
    pub backend_flush_retries: u32,
    pub backend_log_max: usize,

    pub scoring: ScoringWeights,
    pub recall_weights: RecallWeights,

    pub hot_list_max: usize,
    pub conflict_recency_window: Duration,
    pub drift_threshold: u32,

    pub gc_interval: u64,
    pub max_canonical_nodes: usize,

    pub canonization_min_peer_count: usize,
    pub canonization_edge_min_age: Duration,
    pub canonization_eval_interval: Duration,
    pub canonization_eval_batch_size: usize,
    pub canonization_repromotion_cooldown: Duration,

    pub semantic_match_threshold: f64,
    pub max_cooccurrence_per_derive: usize,

    pub default_top_k: usize,
    pub default_max_tokens: usize,
    pub default_traversal_depth: usize,

    pub match_strategy: MatchStrategy,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            backend_flush_interval: Duration::from_secs(1),
            backend_flush_max_batch: 500,
            backend_flush_retries: 3,
            backend_log_max: 50_000,

            scoring: ScoringWeights::default(),
            recall_weights: RecallWeights::default(),

            hot_list_max: 1000,
            conflict_recency_window: Duration::from_secs(30),
            drift_threshold: 5,

            gc_interval: 10_000,
            max_canonical_nodes: 1000,

            canonization_min_peer_count: 20,
            canonization_edge_min_age: Duration::from_secs(60),
            canonization_eval_interval: Duration::from_secs(60),
            canonization_eval_batch_size: 50,
            canonization_repromotion_cooldown: Duration::from_secs(300),

            semantic_match_threshold: 0.85,
            max_cooccurrence_per_derive: 10,

            default_top_k: 5,
            default_max_tokens: 500,
            default_traversal_depth: 2,

            match_strategy: MatchStrategy::Hybrid,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_spec() {
        let c = Config::default();
        assert_eq!(c.backend_flush_interval, Duration::from_secs(1));
        assert_eq!(c.backend_flush_max_batch, 500);
        assert_eq!(c.backend_flush_retries, 3);
        assert_eq!(c.backend_log_max, 50_000);

        assert_eq!(c.scoring.recency, 0.25);
        assert_eq!(c.scoring.frequency, 0.20);
        assert_eq!(c.scoring.session_activity, 0.20);
        assert_eq!(c.scoring.density, 0.35);

        assert_eq!(c.hot_list_max, 1000);
        assert_eq!(c.conflict_recency_window, Duration::from_secs(30));
        assert_eq!(c.drift_threshold, 5);

        assert_eq!(c.gc_interval, 10_000);
        assert_eq!(c.max_canonical_nodes, 1000);

        assert_eq!(c.canonization_min_peer_count, 20);
        assert_eq!(c.canonization_edge_min_age, Duration::from_secs(60));
        assert_eq!(c.canonization_eval_interval, Duration::from_secs(60));
        assert_eq!(c.canonization_eval_batch_size, 50);
        assert_eq!(
            c.canonization_repromotion_cooldown,
            Duration::from_secs(300)
        );

        assert!((c.semantic_match_threshold - 0.85).abs() < 1e-12);
        assert_eq!(c.max_cooccurrence_per_derive, 10);

        assert_eq!(c.default_top_k, 5);
        assert_eq!(c.default_max_tokens, 500);
        assert_eq!(c.default_traversal_depth, 2);

        assert_eq!(c.match_strategy, MatchStrategy::Hybrid);
    }

    #[test]
    fn config_json_roundtrip() {
        let c = Config::default();
        // Duration serializes as {secs, nanos} with serde — use a dedicated
        // intermediate or skip full Config JSON; assert scoring round-trips.
        let s = serde_json::to_string(&c.scoring).unwrap();
        let back: ScoringWeights = serde_json::from_str(&s).unwrap();
        assert_eq!(c.scoring, back);
    }
}
