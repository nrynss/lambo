//! Level B process resolution: one place to build store + embedder and check compatibility.
//!
//! Callers (CLI, future `Memory::builder` / `serve`) must use [`resolve_backends`] rather
//! than calling `build_store` + `build_embedder` separately and re-checking.

use crate::embed::{build_embedder, Embedder, EmbedderConfig, EmbedderKind};
use crate::store::{build_store, Capabilities, GraphStore, StoreConfig};
use crate::types::{EmbeddingContract, LamboError};
use crate::LamboFile;

/// Fully resolved Level B backends ready for `Memory` / CLI.
pub struct ResolvedBackends {
    pub store: Box<dyn GraphStore>,
    pub embedder: Box<dyn Embedder>,
    pub store_cfg: StoreConfig,
    pub embedder_cfg: EmbedderConfig,
    /// Contract to stamp on the session / refuse mid-session model swaps.
    pub embedding: EmbeddingContract,
}

/// Store vector width vs embedder output dim (store is the authority when it persists vectors).
///
/// * `None` store width (MemoryStore, SQLite without vectors) → any positive embedder dim OK.
/// * `Some(n)` (e.g. Cockroach `VECTOR(n)`) → embedder must emit exactly `n`.
pub fn check_vector_compatibility(
    store_vector_dim: Option<usize>,
    embedder_dim: usize,
) -> Result<(), LamboError> {
    if embedder_dim == 0 {
        return Err(LamboError::Config("embedder dimension must be > 0".into()));
    }
    if let Some(store_dim) = store_vector_dim {
        if store_dim != embedder_dim {
            return Err(LamboError::Config(format!(
                "embedder dim {embedder_dim} is incompatible with store vector width {store_dim} \
                 (store schema is the authority; change the embedder or the store, not a global constant)"
            )));
        }
    }
    Ok(())
}

/// Fail-closed Level B consistency check (CON-5): a resolved store must not
/// claim [`Capabilities::VECTOR_SEARCH`] without reporting a concrete
/// `vector_dimensions()`, and must not report dimensions without the
/// capability. The capability and the schema width are two halves of one
/// contract — an adapter that splits them would let vectors land beside
/// inapplicable stores with no error at resolution time.
pub fn check_vector_search_contract(
    store: &dyn GraphStore,
    kind: crate::store::StoreKind,
) -> Result<(), LamboError> {
    match (
        store.capabilities().contains(Capabilities::VECTOR_SEARCH),
        store.vector_dimensions(),
    ) {
        (true, None) => Err(LamboError::Config(format!(
            "store `{kind:?}` claims the VECTOR_SEARCH capability but reports no \
             vector_dimensions — an adapter with a vector column MUST report its width \
             (the store schema is the authority); refusing to resolve"
        ))),
        (false, Some(dim)) => Err(LamboError::Config(format!(
            "store `{kind:?}` reports vector_dimensions = {dim} but does not claim \
             VECTOR_SEARCH — a store without the capability cannot persist or query \
             vectors; refusing to resolve"
        ))),
        _ => Ok(()),
    }
}

/// Build store + embedder from a resolved process file and enforce store×embedder dim match.
pub fn resolve_backends(file: LamboFile) -> Result<ResolvedBackends, LamboError> {
    let store_cfg = file.store;
    let embedder_cfg = file.embedder;
    let store = build_store(store_cfg.clone()).map_err(|e| LamboError::Config(e.to_string()))?;
    let embedder =
        build_embedder(embedder_cfg.clone()).map_err(|e| LamboError::Config(e.to_string()))?;
    check_vector_search_contract(store.as_ref(), store_cfg.kind)?;

    let embed_dim = embedder.dimensions();
    if embed_dim != embedder_cfg.dim {
        return Err(LamboError::Config(format!(
            "embedder reported dim {embed_dim} but config requested {}",
            embedder_cfg.dim
        )));
    }
    check_vector_compatibility(store.vector_dimensions(), embed_dim)?;

    let embedding = EmbeddingContract {
        kind: embedder_cfg.kind.to_string(),
        model: embedder_cfg.llama_model.clone().filter(|s| !s.is_empty()),
        dim: embed_dim,
    };

    Ok(ResolvedBackends {
        store,
        embedder,
        store_cfg,
        embedder_cfg,
        embedding,
    })
}

/// Convenience: load file/env then resolve (same as CLI).
pub fn resolve_from_config_path(
    explicit: Option<&std::path::Path>,
) -> Result<ResolvedBackends, LamboError> {
    let file = LamboFile::load_resolved(explicit)?;
    resolve_backends(file)
}

/// Store-only resolution (provision / reader tools that do not embed).
pub fn resolve_store_only(
    explicit: Option<&std::path::Path>,
) -> Result<Box<dyn GraphStore>, LamboError> {
    let file = LamboFile::load_resolved(explicit)?;
    build_store(file.store).map_err(|e| LamboError::Config(e.to_string()))
}

/// Refuse to use an embedder that disagrees with the session's stamped contract.
///
/// Call on `load_session` / serve attach when `GraphSnapshot.embedding` is `Some`.
pub fn assert_session_embedding_compatible(
    session: Option<&EmbeddingContract>,
    live: &EmbeddingContract,
) -> Result<(), LamboError> {
    match session {
        None => Ok(()),
        Some(existing) => existing.ensure_compatible(live),
    }
}

/// Human-readable label for logs (never include secrets).
pub fn describe_embedder(kind: EmbedderKind, dim: usize, model: Option<&str>) -> String {
    match model {
        Some(m) if !m.is_empty() => format!("{kind} dim={dim} model={m}"),
        _ => format!("{kind} dim={dim}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    #[test]
    fn vector_compat_none_store_accepts_any_positive_dim() {
        check_vector_compatibility(None, 512).unwrap();
        check_vector_compatibility(None, 1024).unwrap();
        check_vector_compatibility(None, 0).unwrap_err();
    }

    #[test]
    fn vector_compat_store_requires_exact_match() {
        check_vector_compatibility(Some(1024), 1024).unwrap();
        let err = check_vector_compatibility(Some(1024), 512).unwrap_err();
        assert!(err.to_string().contains("incompatible"), "{err}");
    }

    #[test]
    #[cfg(all(feature = "store-memory", feature = "embed-fixture"))]
    fn resolve_memory_plus_fixture_any_configured_dim() {
        use crate::embed::EmbedderKind;
        use crate::store::StoreKind;
        // MemoryStore has no vector column → 768 is allowed if the embedder emits 768.
        // FixtureEmbedder is currently fixed at 1024; use matching config.
        let file = LamboFile {
            store: StoreConfig {
                kind: StoreKind::Memory,
                dsn: None,
                path: None,
            },
            embedder: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
            },
        };
        let r = resolve_backends(file).unwrap();
        assert_eq!(r.embedder.dimensions(), 1024);
        assert_eq!(r.store.vector_dimensions(), None);
        assert_eq!(r.embedding.dim, 1024);
        assert_eq!(r.embedding.kind, "fixture");
    }

    #[test]
    fn session_contract_rejects_model_space_mix() {
        let a = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("bge-m3-FP16".into()),
            dim: 1024,
        };
        let b = EmbeddingContract {
            kind: "bedrock".into(),
            model: Some("titan-v2".into()),
            dim: 1024,
        };
        assert!(a.ensure_compatible(&b).is_err());
        assert!(a.ensure_compatible(&a).is_ok());
        // Same kind+dim, model change still a mix (different embedding space risk).
        let c = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("other-gguf".into()),
            dim: 1024,
        };
        assert!(a.ensure_compatible(&c).is_err());
    }

    /// Minimal `GraphStore` stub for Level B consistency checks (CON-5): only
    /// `capabilities` / `vector_dimensions` matter; every other surface is a
    /// stub that must never be called by the resolve path.
    struct StubStore {
        capabilities: Capabilities,
        vector_dim: Option<usize>,
    }

    #[async_trait]
    impl GraphStore for StubStore {
        async fn init_schema(&self) -> Result<(), crate::types::StoreError> {
            unreachable!("resolve never initializes schema")
        }
        fn capabilities(&self) -> Capabilities {
            self.capabilities
        }
        fn vector_dimensions(&self) -> Option<usize> {
            self.vector_dim
        }
        async fn flush(
            &self,
            _batch: &crate::types::MutationBatch,
        ) -> Result<(), crate::types::StoreError> {
            unreachable!("resolve never flushes")
        }
        async fn load_session(
            &self,
            _session: &crate::types::SessionId,
        ) -> Result<crate::types::GraphSnapshot, crate::types::StoreError> {
            unreachable!("resolve never loads")
        }
        async fn keyword_candidates(
            &self,
            _session: &crate::types::SessionId,
            _tokens: &[String],
            _limit: usize,
        ) -> Result<Vec<crate::types::Scored<crate::types::NodeId>>, crate::types::StoreError>
        {
            unreachable!("resolve never queries")
        }
        async fn vector_candidates(
            &self,
            _session: &crate::types::SessionId,
            _embedding: &[f32],
            _limit: usize,
        ) -> Result<Vec<crate::types::Scored<crate::types::NodeId>>, crate::types::StoreError>
        {
            unreachable!("resolve never queries")
        }
        async fn blast_radius(
            &self,
            _session: &crate::types::SessionId,
            _node: crate::types::NodeId,
            _min_edge_age: std::time::Duration,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> Result<u64, crate::types::StoreError> {
            unreachable!("resolve never queries")
        }
        async fn interaction_span(
            &self,
            _session: &crate::types::SessionId,
            _node: crate::types::NodeId,
            _min_age: std::time::Duration,
            _now: chrono::DateTime<chrono::Utc>,
        ) -> Result<crate::types::InteractionSpan, crate::types::StoreError> {
            unreachable!("resolve never queries")
        }
        async fn record_canonization(
            &self,
            _event: &crate::types::CanonizationEvent,
        ) -> Result<(), crate::types::StoreError> {
            unreachable!("resolve never writes")
        }
    }

    #[test]
    fn vector_search_capability_requires_dimensions_fail_closed() {
        use crate::store::StoreKind;
        // Claiming VECTOR_SEARCH without reporting a width is refuse-able (CON-5).
        let no_dim = StubStore {
            capabilities: Capabilities::VECTOR_SEARCH,
            vector_dim: None,
        };
        let err = check_vector_search_contract(&no_dim, StoreKind::Cockroach).unwrap_err();
        assert!(err.to_string().contains("VECTOR_SEARCH"), "{err}");
        assert!(err.to_string().contains("vector_dimensions"), "{err}");
        // Reporting a width without the capability is equally refuse-able.
        let no_cap = StubStore {
            capabilities: Capabilities::empty(),
            vector_dim: Some(1024),
        };
        let err = check_vector_search_contract(&no_cap, StoreKind::Sqlite).unwrap_err();
        assert!(err.to_string().contains("1024"), "{err}");
        // Consistent pairs resolve.
        check_vector_search_contract(
            &StubStore {
                capabilities: Capabilities::VECTOR_SEARCH,
                vector_dim: Some(1024),
            },
            StoreKind::Cockroach,
        )
        .unwrap();
        check_vector_search_contract(
            &StubStore {
                capabilities: Capabilities::empty(),
                vector_dim: None,
            },
            StoreKind::Memory,
        )
        .unwrap();
    }
}
