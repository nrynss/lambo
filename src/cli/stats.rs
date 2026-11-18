//! `lambo stats` — lease-free reader snapshot of durable graph counts.
//!
//! A reader cannot see the writer's flush task. `flush_lag` / `log_depth` /
//! `daemon_cycles` / `canonization_cycles` are reported as `n/a`, never as
//! zeros that would look like live writer stats.

use super::caps::{check_size_cli, require_nonempty, CliError};
use super::load_reader_graph;
use crate::store::GraphStore;
use crate::types::{CanonizationStatus, EmbeddingContract};

/// Session health from the durable snapshot.
pub async fn run(store: &dyn GraphStore, session: &str) -> Result<String, CliError> {
    require_nonempty("session", session)?;
    check_size_cli("session", session)?;

    let loaded = load_reader_graph(store, session).await?;
    let g = loaded.graph.read();
    let concept_count = g.concepts().count();
    let canonical_count = g
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .count();
    // K2: embedded/total answers "how many concepts actually carry a vector"
    // from the durable snapshot — the counter that makes NULL-embedding rows
    // visible without opening a writer.
    let embedded = g.concepts().filter(|c| c.embedding.is_some()).count();
    // #221: name the stored contract's calibration status. An uncalibrated
    // contract (no built-in row, not the BGE-M3 reference) recalls on
    // BGE-M3-shaped constants over a foreign scale; the operator should see
    // that in the same snapshot that shows the vectors are there.
    let calibration = calibration_line(g.embedding(), concept_count);
    let text = format!(
        "session '{}' (reader snapshot)\n\
         nodes={} edges={} concepts={} canonical={}\n\
         embedded={}/{}\n\
         epoch={}\n\
         {}\n\
         flush_lag=n/a log_depth=n/a daemon_cycles=n/a canonization_cycles=n/a\n\
         note: flush_lag / log_depth / daemon_cycles / canonization_cycles are writer-only; \
         this is a reader process",
        session,
        g.node_count(),
        g.edge_count(),
        concept_count,
        canonical_count,
        embedded,
        concept_count,
        g.epoch(),
        calibration,
    );
    Ok(text)
}

/// The `calibration=` line of the stats snapshot, extracted pure (P2-2) so
/// its three arms are unit-testable without a store: the BGE-M3 reference
/// (or a measured row) reports calibrated, any other stamped contract names
/// itself as uncalibrated, and a session with no stamped contract reports
/// unknown. A calibrated line also names how many concepts the contract's
/// layer floor was measured against (#254), and says so when the session
/// holds more (`concepts`): served as a layer on that floor, it admits a hit
/// for more unrelated queries than the floor was chosen to. A reader does
/// not know whether the session is served as a layer or which floor a serve
/// gives it, so the line states the comparison and its condition.
fn calibration_line(contract: Option<&EmbeddingContract>, concepts: usize) -> String {
    use crate::embed::calibration::{is_calibrated, layer_floor_measured_concepts};
    match contract {
        Some(contract) if is_calibrated(contract) => {
            match layer_floor_measured_concepts(contract) {
                Some(n) if concepts > n => format!(
                    "calibration=calibrated (layer floor measured on {n} concepts; this session \
                     holds {concepts}, so as a layer on the default floor it admits more \
                     unrelated hits than at that size: #254)"
                ),
                Some(n) => format!("calibration=calibrated (layer floor measured on {n} concepts)"),
                None => "calibration=calibrated".to_string(),
            }
        }
        Some(contract) => format!(
            "calibration=uncalibrated (kind={} model={} dim={}: no built-in row; recall uses \
             BGE-M3 constants)",
            contract.kind,
            contract.model.as_deref().unwrap_or("(default)"),
            contract.dim
        ),
        None => "calibration=unknown (no stamped contract)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn contract(kind: &str, model: Option<&str>, dim: usize) -> EmbeddingContract {
        EmbeddingContract {
            kind: kind.into(),
            model: model.map(Into::into),
            dim,
        }
    }

    #[test]
    fn calibration_line_names_all_three_arms() {
        // Calibrated: the BGE-M3 reference contract (server default at 1024),
        // whose 0.50 layer floor was chosen on a 12-concept layer (#157, #254).
        assert_eq!(
            calibration_line(Some(&contract("bge_m3", None, 1024)), 12),
            "calibration=calibrated (layer floor measured on 12 concepts)"
        );
        // Uncalibrated: names the contract so the operator sees which space
        // recalls on BGE-M3-shaped constants. (Qwen3 Embedding 4B has measured
        // rows now, so a foreign model stands in.)
        let qwen = calibration_line(
            Some(&contract("bge_m3", Some("some-unmeasured-model"), 2560)),
            5000,
        );
        assert!(qwen.starts_with("calibration=uncalibrated"), "{qwen}");
        assert!(qwen.contains("kind=bge_m3"), "{qwen}");
        assert!(qwen.contains("model=some-unmeasured-model"), "{qwen}");
        assert!(qwen.contains("dim=2560"), "{qwen}");
        // The uncalibrated arm's text is unchanged: no measured size.
        assert_eq!(
            qwen,
            "calibration=uncalibrated (kind=bge_m3 model=some-unmeasured-model dim=2560: no \
             built-in row; recall uses BGE-M3 constants)"
        );
        // A measured Qwen contract is calibrated, on the #190 set's 100
        // concepts.
        let on_eval_set = "calibration=calibrated (layer floor measured on 100 concepts)";
        let measured = calibration_line(
            Some(&contract("bge_m3", Some("qwen3-embedding-4b"), 2560)),
            100,
        );
        assert_eq!(measured, on_eval_set, "{measured}");
        // The 1024 truncation has its own row: calibrated too.
        let truncated = calibration_line(
            Some(&contract("bge_m3", Some("qwen3-embedding-4b"), 1024)),
            0,
        );
        assert_eq!(truncated, on_eval_set, "{truncated}");
        // #237: the exact Elastic Jina contract has a measured row too.
        let elastic = calibration_line(
            Some(&contract("elastic", Some(".jina-embeddings-v3"), 1024)),
            99,
        );
        assert_eq!(elastic, on_eval_set, "{elastic}");
        // The EG2 row (#87) was measured on the same set.
        let eg2 = calibration_line(
            Some(&contract(
                "embeddinggemma2",
                Some("ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2"),
                768,
            )),
            100,
        );
        assert_eq!(eg2, on_eval_set, "{eg2}");
        // A default-model uncalibrated contract prints `(default)`, not empty.
        let defaulted = calibration_line(Some(&contract("embeddinggemma2", None, 768)), 5000);
        assert!(defaulted.contains("model=(default)"), "{defaulted}");
        // Unknown: no stamped contract (nothing embedded yet).
        assert_eq!(
            calibration_line(None, 5000),
            "calibration=unknown (no stamped contract)"
        );
    }

    /// #254: a calibrated session holding more concepts than its layer floor
    /// was measured on says so; at the measured size or below the line is
    /// the plain one, and the uncalibrated and unknown arms never change.
    #[test]
    fn calibration_line_says_when_the_session_outgrew_the_measured_size() {
        let bge = contract("bge_m3", None, 1024);
        assert_eq!(
            calibration_line(Some(&bge), 12),
            "calibration=calibrated (layer floor measured on 12 concepts)"
        );
        assert_eq!(
            calibration_line(Some(&bge), 13),
            "calibration=calibrated (layer floor measured on 12 concepts; this session holds \
             13, so as a layer on the default floor it admits more unrelated hits than at that \
             size: #254)"
        );
        let qwen = contract("bge_m3", Some("qwen3-embedding-4b"), 2560);
        assert_eq!(
            calibration_line(Some(&qwen), 100),
            "calibration=calibrated (layer floor measured on 100 concepts)"
        );
        let grown = calibration_line(Some(&qwen), 2048);
        assert!(
            grown.contains("measured on 100 concepts; this session holds 2048"),
            "{grown}"
        );
        assert_eq!(grown.lines().count(), 1, "{grown}");
    }
}
