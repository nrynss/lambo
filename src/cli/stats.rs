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
    run_with_missing_vectors(store, session, false).await
}

/// Optionally list every missing vector in the durable snapshot.
pub async fn run_with_missing_vectors(
    store: &dyn GraphStore,
    session: &str,
    missing_vectors: bool,
) -> Result<String, CliError> {
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
    let calibration = calibration_line(g.embedding());
    let mut text = format!(
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
    if missing_vectors {
        let mut missing: Vec<_> = g.concepts().filter(|c| c.embedding.is_none()).collect();
        missing.sort_by_key(|c| (c.created_at, c.id.0));
        text.push_str(&format!(
            "\nmissing vectors (durable snapshot): {}",
            missing.len()
        ));
        for c in missing {
            let kind = if c.embedding_source.is_some() {
                "image"
            } else {
                "text"
            };
            text.push_str(&format!(
                "\n  {} kind={} created_at={}",
                c.id,
                kind,
                c.created_at.to_rfc3339()
            ));
        }
    }
    Ok(text)
}

/// The `calibration=` line of the stats snapshot, extracted pure (P2-2) so
/// its three arms are unit-testable without a store: the BGE-M3 reference
/// (or a measured row) reports calibrated, any other stamped contract names
/// itself as uncalibrated, and a session with no stamped contract reports
/// unknown. A calibrated line also names how many concepts the contract's
/// layer floor was measured against (#254), so an operator can compare it
/// with the size of a layer.
fn calibration_line(contract: Option<&EmbeddingContract>) -> String {
    use crate::embed::calibration::{is_calibrated, layer_floor_measured_concepts};
    match contract {
        Some(contract) if is_calibrated(contract) => {
            match layer_floor_measured_concepts(contract) {
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

    #[cfg(feature = "fixtures")]
    #[tokio::test]
    async fn missing_inventory_reads_all_snapshot_kinds_in_order() {
        use crate::types::{
            AgentId, Concept, ConceptType, Edge, EdgeType, EmbeddingSource, GraphSnapshot,
            Interaction, NodeId, SessionId, SourceModality, VectorOrigin,
        };
        use crate::MemoryStore;
        use chrono::{TimeZone, Utc};
        let session = SessionId::from("inventory");
        let origin = NodeId::new();
        let make = |id: NodeId, second, image, embedded| Concept {
            id,
            session_id: session.clone(),
            content: "private payload".into(),
            canonical_key: id.to_string(),
            concept_type: ConceptType::Entity,
            origin_interaction: origin,
            origin_agent: AgentId::from("a"),
            created_at: Utc.timestamp_opt(second, 0).unwrap(),
            access_count: 0,
            last_accessed: None,
            gc_survived: 0,
            human_confirmed: 0,
            canonization_status: CanonizationStatus::None,
            blast_radius: None,
            last_demotion_time: None,
            embedding: if embedded { Some(vec![1.0]) } else { None },
            embedding_source: if image {
                Some(EmbeddingSource {
                    modality: SourceModality::Image,
                    origin: VectorOrigin::Client,
                    sha256: None,
                    mime: None,
                })
            } else {
                None
            },
            chunk_group_id: None,
        };
        let text_id = NodeId::new();
        let image_id = NodeId::new();
        let embedded_id = NodeId::new();
        let store = MemoryStore::new();
        store
            .seed(GraphSnapshot {
                session_id: session.clone(),
                embedding: Some(contract("fixture", None, 1)),
                interactions: vec![Interaction {
                    id: origin,
                    session_id: session.clone(),
                    agent_id: AgentId::from("a"),
                    prompt_text: None,
                    previous_id: None,
                    created_at: Utc.timestamp_opt(0, 0).unwrap(),
                    event_time: None,
                }],
                edges: [text_id, image_id, embedded_id]
                    .into_iter()
                    .map(|target| Edge {
                        id: NodeId::new(),
                        session_id: session.clone(),
                        source: origin,
                        target,
                        edge_type: EdgeType::Derives,
                        weight: 1.0,
                        reinforcements: 0,
                        created_at: Utc.timestamp_opt(0, 0).unwrap(),
                        last_reinforced: Utc.timestamp_opt(0, 0).unwrap(),
                        event_time: None,
                    })
                    .collect(),
                concepts: vec![
                    make(text_id, 20, false, false),
                    make(image_id, 10, true, false),
                    make(embedded_id, 5, false, true),
                ],
                ..Default::default()
            })
            .unwrap();
        let normal = run(&store, "inventory").await.unwrap();
        assert!(!normal.contains("missing vectors"));
        let listed = run_with_missing_vectors(&store, "inventory", true)
            .await
            .unwrap();
        assert!(listed.starts_with(&normal));
        assert!(
            listed.contains("missing vectors (durable snapshot): 2"),
            "{listed}"
        );
        assert!(listed.contains(&format!("{text_id} kind=text created_at=")));
        assert!(listed.contains(&format!("{image_id} kind=image created_at=")));
        assert!(listed.find(&image_id.to_string()) < listed.find(&text_id.to_string()));
        assert!(!listed.contains(&embedded_id.to_string()));
        assert!(!listed.contains("private payload"));
        let empty = MemoryStore::new();
        empty
            .seed(GraphSnapshot {
                session_id: SessionId::from("empty"),
                ..Default::default()
            })
            .unwrap();
        assert!(run_with_missing_vectors(&empty, "empty", true)
            .await
            .unwrap()
            .contains("missing vectors (durable snapshot): 0"));
    }

    #[test]
    fn calibration_line_names_all_three_arms() {
        // Calibrated: the BGE-M3 reference contract (server default at 1024),
        // whose 0.50 layer floor was chosen on a 12-concept layer (#157, #254).
        assert_eq!(
            calibration_line(Some(&contract("bge_m3", None, 1024))),
            "calibration=calibrated (layer floor measured on 12 concepts)"
        );
        // Uncalibrated: names the contract so the operator sees which space
        // recalls on BGE-M3-shaped constants. (Qwen3 Embedding 4B has measured
        // rows now, so a foreign model stands in.)
        let qwen = calibration_line(Some(&contract(
            "bge_m3",
            Some("some-unmeasured-model"),
            2560,
        )));
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
        let measured =
            calibration_line(Some(&contract("bge_m3", Some("qwen3-embedding-4b"), 2560)));
        assert_eq!(measured, on_eval_set, "{measured}");
        // The 1024 truncation has its own row: calibrated too.
        let truncated =
            calibration_line(Some(&contract("bge_m3", Some("qwen3-embedding-4b"), 1024)));
        assert_eq!(truncated, on_eval_set, "{truncated}");
        // #237: the exact Elastic Jina contract has a measured row too.
        let elastic = calibration_line(Some(&contract(
            "elastic",
            Some(".jina-embeddings-v3"),
            1024,
        )));
        assert_eq!(elastic, on_eval_set, "{elastic}");
        // The EG2 row (#87) was measured on the same set.
        let eg2 = calibration_line(Some(&contract(
            "embeddinggemma2",
            Some("ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0;prompts=lambo-eg2-v2"),
            768,
        )));
        assert_eq!(eg2, on_eval_set, "{eg2}");
        // A default-model uncalibrated contract prints `(default)`, not empty.
        let defaulted = calibration_line(Some(&contract("embeddinggemma2", None, 768)));
        assert!(defaulted.contains("model=(default)"), "{defaulted}");
        // Unknown: no stamped contract (nothing embedded yet).
        assert_eq!(
            calibration_line(None),
            "calibration=unknown (no stamped contract)"
        );
    }
}
