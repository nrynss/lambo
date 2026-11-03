//! `lambo stats` — lease-free reader snapshot of durable graph counts.
//!
//! A reader cannot see the writer's flush task. `flush_lag` / `log_depth` /
//! `daemon_cycles` / `canonization_cycles` are reported as `n/a`, never as
//! zeros that would look like live writer stats.

use super::caps::{check_size_cli, require_nonempty, CliError};
use super::load_reader_graph;
use crate::store::GraphStore;
use crate::types::CanonizationStatus;

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
    let calibration = match g.embedding() {
        Some(contract) if crate::embed::calibration::is_calibrated(contract) => {
            "calibration=calibrated".to_string()
        }
        Some(contract) => format!(
            "calibration=uncalibrated (kind={} model={} dim={}: no built-in row; recall uses \
             BGE-M3 constants)",
            contract.kind,
            contract.model.as_deref().unwrap_or("(default)"),
            contract.dim
        ),
        None => "calibration=unknown (no stamped contract)".to_string(),
    };
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
