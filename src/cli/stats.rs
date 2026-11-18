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
    let calibration = calibration_line(g.embedding(), embedded);
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
/// unknown.
///
/// A calibrated line states two facts beside each other (#254): where the
/// contract's layer floor comes from
/// ([`layer_floor_provenance`](crate::embed::calibration::layer_floor_provenance):
/// the layer size its bar was chosen on) and how many of this session's
/// concepts carry a vector (`embedded`), which is the number a layer floor
/// is tested against. It does not warn or predict. A reader cannot know
/// whether the session is served as a layer, whether a serve sets a floor
/// for it, or which calibration switches that serve runs with; the serve
/// that does know logs the warning
/// ([`layer_floor_size_warning`](crate::embed::calibration::layer_floor_size_warning)).
fn calibration_line(contract: Option<&EmbeddingContract>, embedded: usize) -> String {
    use crate::embed::calibration::{is_calibrated, layer_floor_provenance};
    match contract {
        Some(contract) if is_calibrated(contract) => match layer_floor_provenance(contract) {
            Some(provenance) => format!(
                "calibration=calibrated ({provenance}; this session holds {embedded} concepts \
                 with vectors)"
            ),
            None => "calibration=calibrated".to_string(),
        },
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

    const BGE_FLOOR: &str = "default layer floor 0.5 chosen on a 12-concept layer";

    fn row_floor(floor: &str) -> String {
        format!(
            "calibrated layer floor {floor} matched, on a 100-concept corpus, to a bar chosen \
             on a 12-concept layer"
        )
    }

    #[test]
    fn calibration_line_names_all_three_arms() {
        // Calibrated: the BGE-M3 reference contract (server default at 1024),
        // whose 0.50 layer floor was chosen on a 12-concept layer (#157, #254).
        assert_eq!(
            calibration_line(Some(&contract("bge_m3", None, 1024)), 12),
            format!(
                "calibration=calibrated ({BGE_FLOOR}; this session holds 12 concepts with vectors)"
            )
        );
        // Uncalibrated: names the contract so the operator sees which space
        // recalls on BGE-M3-shaped constants. (Qwen3 Embedding 4B has measured
        // rows now, so a foreign model stands in.)
        let qwen = calibration_line(
            Some(&contract("bge_m3", Some("some-unmeasured-model"), 2560)),
            5000,
        );
        // The uncalibrated arm's text is unchanged: no size, no count.
        assert_eq!(
            qwen,
            "calibration=uncalibrated (kind=bge_m3 model=some-unmeasured-model dim=2560: no \
             built-in row; recall uses BGE-M3 constants)"
        );
        // A measured row's floor is matched to BGE-M3's on the #190 set's
        // 100 concepts, and its bar was chosen on the same 12-concept layer.
        let line = |floor: &str, embedded: usize| {
            format!(
                "calibration=calibrated ({}; this session holds {embedded} concepts with vectors)",
                row_floor(floor)
            )
        };
        let measured = calibration_line(
            Some(&contract("bge_m3", Some("qwen3-embedding-4b"), 2560)),
            100,
        );
        assert_eq!(measured, line("0.59", 100));
        // The 1024 truncation has its own row: calibrated too.
        let truncated = calibration_line(
            Some(&contract("bge_m3", Some("qwen3-embedding-4b"), 1024)),
            0,
        );
        assert_eq!(truncated, line("0.6", 0));
        // #237: the exact Elastic Jina contract has a measured row too.
        let elastic = calibration_line(
            Some(&contract("elastic", Some(".jina-embeddings-v3"), 1024)),
            99,
        );
        assert_eq!(elastic, line("0.39", 99));
        // The EG2 row (#87) was measured on the same set.
        let eg2 = calibration_line(
            Some(&contract(
                "embeddinggemma2",
                Some("ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2"),
                768,
            )),
            100,
        );
        assert_eq!(eg2, line("0.72", 100));
        // A default-model uncalibrated contract prints `(default)`, not empty.
        let defaulted = calibration_line(Some(&contract("embeddinggemma2", None, 768)), 5000);
        assert!(defaulted.contains("model=(default)"), "{defaulted}");
        // Unknown: no stamped contract (nothing embedded yet).
        assert_eq!(
            calibration_line(None, 5000),
            "calibration=unknown (no stamped contract)"
        );
    }

    /// #254: the calibrated line states the size the floor was chosen on and
    /// the session's count of concepts with vectors, and nothing else: the
    /// same sentence below, at and far above that size, with no warning and
    /// no prediction. A reader does not know whether the session is a layer
    /// or which floor a serve gives it.
    #[test]
    fn calibration_line_states_sizes_and_never_warns() {
        let bge = contract("bge_m3", None, 1024);
        let qwen = contract("bge_m3", Some("qwen3-embedding-4b"), 2560);
        for embedded in [0, 12, 13, 100, 101, 5000] {
            assert_eq!(
                calibration_line(Some(&bge), embedded),
                format!(
                    "calibration=calibrated ({BGE_FLOOR}; this session holds {embedded} \
                     concepts with vectors)"
                )
            );
            let row = calibration_line(Some(&qwen), embedded);
            assert_eq!(
                row,
                format!(
                    "calibration=calibrated ({}; this session holds {embedded} concepts with \
                     vectors)",
                    row_floor("0.59")
                )
            );
            for line in [calibration_line(Some(&bge), embedded), row] {
                assert_eq!(line.lines().count(), 1, "{line}");
                for claim in ["admit", "unrelated", "more", "#254", "warn"] {
                    assert!(!line.contains(claim), "{claim:?} in {line}");
                }
            }
        }
    }
}
