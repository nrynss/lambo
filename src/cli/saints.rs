//! `lambo saints` — lease-free list of Canonical memories.
//!
//! Scans the loaded graph exactly like [`Memory::canonical_memories`]: Canonical
//! only; order blast-radius desc, then created_at, then id. Not a store query.

use super::caps::{check_size_cli, require_nonempty, CliError};
use super::load_reader_graph;
use crate::memory::CanonicalMemory;
use crate::recall::format;
use crate::resolve::ResolvedBackends;
use crate::types::CanonizationStatus;

/// List the session's canonical memories.
pub async fn run(backends: &ResolvedBackends, session: &str) -> Result<String, CliError> {
    require_nonempty("session", session)?;
    check_size_cli("session", session)?;

    let loaded = load_reader_graph(backends.store.as_ref(), session).await?;
    let saints = canonical_memories_from_graph(&loaded.graph.read());

    let mut text = format!(
        "{} canonical memor{} in session '{}'\n",
        saints.len(),
        if saints.len() == 1 { "y" } else { "ies" },
        session
    );
    for s in &saints {
        text.push_str(&format!(
            "  {} [{:?}, canonical]  blast_radius={}  accesses={}  since {}\n",
            s.content,
            s.concept_type,
            s.blast_radius,
            s.access_count,
            s.created_at.to_rfc3339()
        ));
    }
    Ok(text)
}

/// Graph scan mirroring [`crate::memory::Memory::canonical_memories`].
pub(crate) fn canonical_memories_from_graph(g: &crate::graph::Graph) -> Vec<CanonicalMemory> {
    let radii = format::blast_radii(g);
    let mut out: Vec<CanonicalMemory> = g
        .concepts()
        .filter(|c| c.canonization_status == CanonizationStatus::Canonical)
        .map(|c| CanonicalMemory {
            node_id: c.id,
            content: c.content.clone(),
            concept_type: c.concept_type,
            blast_radius: radii.get(&c.id).copied().unwrap_or(0),
            created_at: c.created_at,
            access_count: c.access_count,
        })
        .collect();
    out.sort_by(|a, b| {
        b.blast_radius
            .cmp(&a.blast_radius)
            .then(a.created_at.cmp(&b.created_at))
            .then(a.node_id.0.cmp(&b.node_id.0))
    });
    out
}

#[cfg(all(test, feature = "store-cockroach", feature = "embed-fixture"))]
mod live {
    use super::*;
    use crate::cli::caps::ConceptKind;
    use crate::resolve::resolve_from_config_path;

    /// Live-cluster saints + stats. `#[ignore]`d without `LAMBO_COCKROACH_DSN`
    /// so default `cargo test` never needs a cluster. Run with
    /// `cargo test --features store-cockroach -- --ignored`.
    #[tokio::test]
    #[ignore = "live: requires LAMBO_COCKROACH_DSN"]
    async fn saints_and_stats_against_live_cockroach() {
        let dsn = {
            let _g = crate::test_util::env_lock();
            std::env::var("LAMBO_COCKROACH_DSN")
                .ok()
                .filter(|s| !s.is_empty())
        };
        let Some(_) = dsn else {
            panic!(
                "saints_and_stats_against_live_cockroach: LAMBO_COCKROACH_DSN is unset; \
                 this test is #[ignore]d — run with --ignored against a live cluster"
            );
        };

        let dir = std::env::temp_dir().join(format!(
            "lambo-cli-crdb-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("scratch");
        let cfg = dir.join("lambo.toml");
        std::fs::write(
            &cfg,
            "[store]\nkind = \"cockroach\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
        )
        .expect("write toml");

        let session = format!("t83-saints-{}", uuid::Uuid::new_v4());
        let backends = {
            let _g = crate::test_util::env_lock();
            resolve_from_config_path(Some(&cfg)).expect("resolve cockroach")
        };
        crate::cli::derive::run(
            backends,
            crate::cli::derive::Args {
                session: session.clone(),
                agent: "t83-live".into(),
                content: "live cluster probe".into(),
                kind: ConceptKind::Entity,
                parent_of: vec![],
                concept: vec![],
            },
        )
        .await
        .expect("derive against live cluster");

        let backends = {
            let _g = crate::test_util::env_lock();
            resolve_from_config_path(Some(&cfg)).expect("resolve for read")
        };
        let saints = run(&backends, &session)
            .await
            .expect("saints against live cluster");
        assert!(
            saints.contains(&session),
            "saints must name the session: {saints}"
        );

        let stats = crate::cli::stats::run(&backends, &session)
            .await
            .expect("stats against live cluster");
        assert!(stats.contains("nodes="), "{stats}");
        assert!(
            stats.contains("n/a") || stats.contains("writer-only"),
            "{stats}"
        );

        let _ = std::fs::remove_dir_all(&dir);
    }
}
