//! Level B process resolution: one place to build store + embedder and check compatibility.
//!
//! Callers (CLI, future `Memory::builder` / `serve`) must use [`resolve_backends`] rather
//! than calling `build_store` + `build_embedder` separately and re-checking.

use crate::embed::{build_embedder, Embedder, EmbedderConfig, EmbedderKind};
use crate::store::{
    build_store, build_store_with_vector_dim, Capabilities, GraphStore, StoreConfig,
};
use crate::types::{EmbeddingContract, LamboError};
use crate::LamboFile;

/// Fully resolved Level B backends ready for `Memory` / CLI.
///
/// `#[non_exhaustive]` is deliberate: adding a field is a breaking change for
/// library consumers of the resolved-backend bundle (see T1-P2-2), so the
/// attribute future-proofs the struct against the next field addition — a
/// one-time break now (callers can no longer literal-construct or
/// exhaustively destructure it) that buys permanence for every field after.
#[non_exhaustive]
pub struct ResolvedBackends {
    pub store: Box<dyn GraphStore>,
    pub embedder: Box<dyn Embedder>,
    pub store_cfg: StoreConfig,
    pub embedder_cfg: EmbedderConfig,
    /// Contract to stamp on the session / refuse mid-session model swaps.
    pub embedding: EmbeddingContract,
    /// Deliberate operator escape hatch for a same-width embedding-space
    /// rename/migration. `false` is the safe default from every resolver.
    /// CLI parsing may set this only for writer commands.
    pub allow_embedding_mismatch: bool,
    /// Product config with any `[daemon]` cadence overrides from the file
    /// already applied. Writers pass this to `Memory::builder().config(..)`.
    pub config: crate::Config,
}

/// Store vector width vs embedder output dim.
///
/// * `None` store width (MemoryStore) → any positive embedder dim OK.
/// * `Some(n)` → embedder must emit exactly `n`. Cockroach's `n` is its `VECTOR(n)`
///   DDL — a real schema authority, so this is a real check for Cockroach.
///
/// **For a width-agnostic store this can be a self-comparison** (F-R1-2). SQLite's
/// `BLOB` has no width of its own, so unless the operator sets
/// [`crate::store::StoreConfig::vector_dim`] it reports the very embedder width this
/// function is handed, and `store_dim != embedder_dim` is unreachable. The check that
/// bites on that path is the explicit pin comparison in [`resolve_backends`]; the
/// authority that attests the stored vectors' space is the session's durable contract,
/// enforced per candidate read inside the adapter.
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
    let daemon_cfg = file.daemon;
    // Fail closed at the file boundary: every file-driven command rejects a
    // degenerate cadence here, uniformly and BEFORE any store/embedder build
    // (an embedder build may load a model, so we reject the file first).
    let mut config = crate::Config::default();
    daemon_cfg.apply_to(&mut config);
    config.validate()?;
    // A store whose vector column carries no width of its own (SQLite's BLOB) reports
    // the operator's `store.vector_dim` pin when one is set, and otherwise **echoes**
    // the configured embedder width — an echo, not a store-side authority, which is
    // why `check_vector_compatibility` alone cannot catch a SQLite disagreement and
    // the explicit pin check below exists (F-R1-2).
    // A zero dim is passed through as `None` so `build_embedder` produces the canonical
    // "embedder dim must be > 0" error rather than a store-shaped one.
    let store =
        build_store_with_vector_dim(store_cfg.clone(), Some(embedder_cfg.dim).filter(|d| *d > 0))
            .map_err(|e| LamboError::Config(e.to_string()))?;
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
    // F-R1-2: the pin is the one width authority a width-agnostic store can carry that
    // the embedder did not supply, so a disagreement between them is a real, reachable
    // resolution failure rather than the self-comparison `check_vector_compatibility`
    // performs when no pin is set. Refuse here — at the serving verbs' resolution
    // boundary — and NOT in `build_store*`: a migration verb (a future `lambo reembed`)
    // must still be able to open a store whose sessions carry a different contract.
    if let Some(pinned) = store_cfg.vector_dim {
        if pinned != embed_dim {
            return Err(LamboError::Config(format!(
                "store.vector_dim is pinned to {pinned} but the configured embedder emits \
                 {embed_dim} — refusing to resolve: the pin asserts what this database \
                 already holds, so serving with a different width would write vectors no \
                 reader can interpret (drop the pin, change the embedder, or re-embed the \
                 database)"
            )));
        }
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
        allow_embedding_mismatch: false,
        config,
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
/// Deliberately does NOT call `config.validate()`: store-only commands never
/// run a daemon `[daemon]` interval, so a degenerate cadence is rejected only
/// at the full `resolve_backends` boundary, not here.
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
    match session_embedding_compatibility(session, live) {
        SessionEmbeddingCompatibility::Unrecorded | SessionEmbeddingCompatibility::Compatible => {
            Ok(())
        }
        SessionEmbeddingCompatibility::Mismatch { stored, live } => {
            Err(embedding_mismatch_error(&stored, &live))
        }
    }
}

/// Classification used by readers that need to report an incompatibility
/// without pretending vector recall is safe.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SessionEmbeddingCompatibility {
    /// Nullable snapshot metadata from an older/un-stamped session.
    Unrecorded,
    /// Stored and live contracts identify the same embedding space.
    Compatible,
    /// The stored vectors and live embedder identify different spaces.
    Mismatch {
        stored: EmbeddingContract,
        live: EmbeddingContract,
    },
}

/// Compare the nullable stored contract with the resolved live embedder.
pub fn session_embedding_compatibility(
    session: Option<&EmbeddingContract>,
    live: &EmbeddingContract,
) -> SessionEmbeddingCompatibility {
    match session {
        None => SessionEmbeddingCompatibility::Unrecorded,
        Some(stored) if stored == live => SessionEmbeddingCompatibility::Compatible,
        Some(stored) => SessionEmbeddingCompatibility::Mismatch {
            stored: stored.clone(),
            live: live.clone(),
        },
    }
}

/// Actionable fail-closed message for a session/live mismatch.
///
/// The override is intentionally named in the error, but is only lawful for
/// equal dimensions; callers enforce that restriction before replacing a
/// contract. A different width is never a model-rename migration.
pub fn embedding_mismatch_error(
    stored: &EmbeddingContract,
    live: &EmbeddingContract,
) -> LamboError {
    let stored_model = stored.model.as_deref().unwrap_or("(default)");
    let live_model = live.model.as_deref().unwrap_or("(default)");
    let reason = if stored.dim == live.dim {
        "equal dimensions do not make model vector spaces compatible"
    } else {
        "the stored and configured vector widths differ"
    };
    LamboError::Config(format!(
        "embedding contract is incompatible (mismatch): this session's vectors were written by \
         kind={} model={} dim={}, but the configured embedder is kind={} model={} dim={}. \
         Refusing because {reason}. \
         A writer may use --allow-embedding-mismatch only for a verified same-kind, same-width \
         model-identifier rename, or after a controlled migration has atomically removed the old \
         vectors; readers remain fail-closed",
        stored.kind, stored_model, stored.dim, live.kind, live_model, live.dim
    ))
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
                vector_dim: None,
            },
            embedder: EmbedderConfig {
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
            },
            daemon: Default::default(),
        };
        let r = resolve_backends(file).unwrap();
        assert_eq!(r.embedder.dimensions(), 1024);
        assert_eq!(r.store.vector_dimensions(), None);
        assert_eq!(r.embedding.dim, 1024);
        assert_eq!(r.embedding.kind, "fixture");
    }

    /// F-R1-2: a width disagreement that is **reachable through
    /// `resolve_backends`**, which is precisely what the pre-remediation tree could
    /// not produce for SQLite. The old test built a `(Some(1536), 768)` pair by hand
    /// and asserted `check_vector_compatibility` rejected it; but with the store
    /// echoing the embedder, no config could make `resolve_backends` reach that pair,
    /// so the check was `x == x` on every real path. `store.vector_dim` is the
    /// operator's pin, so it *can* disagree — and must be refused, naming both widths.
    // Needs a real embedder as well as the adapter: `resolve_backends` builds the
    // embedder before it reaches the pin check, so without `embed-fixture` the
    // refusal under test is masked by a missing-feature error.
    #[cfg(all(feature = "store-sqlite", feature = "embed-fixture"))]
    #[test]
    fn a_pinned_store_width_disagreeing_with_the_embedder_is_refused_at_resolve() {
        use crate::embed::EmbedderKind;
        use crate::store::StoreKind;
        let file = |vector_dim: Option<usize>| LamboFile {
            store: StoreConfig {
                kind: StoreKind::Sqlite,
                dsn: None,
                path: Some("sqlite::memory:".into()),
                vector_dim,
            },
            embedder: EmbedderConfig {
                // FixtureEmbedder emits 1024.
                kind: EmbedderKind::Fixture,
                dim: 1024,
                llama_url: None,
                llama_model: None,
            },
            daemon: Default::default(),
        };

        // The pin asserts this database holds 768-wide vectors; the embedder emits
        // 1024. Refused at process resolution, with both numbers in the message.
        // `ResolvedBackends` is not `Debug`, so match rather than `unwrap_err`.
        let msg = match resolve_backends(file(Some(768))) {
            Err(err) => err.to_string(),
            Ok(_) => panic!("a pin disagreeing with the embedder must not resolve"),
        };
        for needle in ["768", "1024", "store.vector_dim"] {
            assert!(
                msg.contains(needle),
                "the refusal must name the pin and both widths: {msg}"
            );
        }

        // An agreeing pin resolves, and the store reports the pinned width.
        let ok = resolve_backends(file(Some(1024))).unwrap();
        assert_eq!(ok.store.vector_dimensions(), Some(1024));

        // No pin: the store echoes the embedder, so this cannot fail — the vacuity
        // the finding names. Pinned here so a future change that gives SQLite a real
        // store-side authority has to revisit this assertion deliberately.
        let echo = resolve_backends(file(None)).unwrap();
        assert_eq!(
            echo.store.vector_dimensions(),
            Some(1024),
            "with no pin, vector_dimensions() is an echo of the embedder width"
        );
    }

    /// The pin's refusal belongs to the serving verbs' resolution path, NOT to store
    /// construction: a future `lambo reembed` migration verb must be able to open a
    /// store whose sessions carry a different contract in order to rewrite them.
    #[cfg(feature = "store-sqlite")]
    #[test]
    fn the_pin_does_not_block_store_construction() {
        use crate::store::StoreKind;
        let store = crate::store::build_store(StoreConfig {
            kind: StoreKind::Sqlite,
            dsn: None,
            path: Some("sqlite::memory:".into()),
            vector_dim: Some(768),
        })
        .expect("a pinned width must not stop a store from opening");
        assert_eq!(store.vector_dimensions(), Some(768));
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

    #[test]
    fn h1_session_contract_classifies_legacy_match_and_same_width_model_mismatch() {
        let stored = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("bge-m3-old.gguf".into()),
            dim: 1024,
        };
        let live = EmbeddingContract {
            kind: "bge_m3".into(),
            model: Some("bge-m3-new.gguf".into()),
            dim: 1024,
        };

        assert_eq!(
            session_embedding_compatibility(None, &live),
            SessionEmbeddingCompatibility::Unrecorded
        );
        assert_session_embedding_compatible(None, &live)
            .expect("a legacy session with no contract must still open");
        assert_eq!(
            session_embedding_compatibility(Some(&stored), &stored),
            SessionEmbeddingCompatibility::Compatible
        );

        let err = assert_session_embedding_compatible(Some(&stored), &live).unwrap_err();
        let text = err.to_string();
        for expected in [
            "bge-m3-old.gguf",
            "bge-m3-new.gguf",
            "dim=1024",
            "--allow-embedding-mismatch",
        ] {
            assert!(text.contains(expected), "missing {expected:?} in {text}");
        }
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
            _token: Option<u64>,
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
            Ok(Vec::new())
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
            _token: Option<u64>,
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

    #[tokio::test]
    async fn legacy_vector_adapter_compiles_unchanged_and_checked_default_fails_closed() {
        // StubStore intentionally implements only v0.2.0's required three-argument
        // vector_candidates method. This pins source compatibility for external
        // adapters while proving that Lambo's additive checked surface cannot
        // silently trust a vector-capable adapter that has not implemented it.
        let store = StubStore {
            capabilities: Capabilities::VECTOR_SEARCH,
            vector_dim: Some(3),
        };
        assert!(store
            .vector_candidates(&crate::types::SessionId::from("legacy"), &[0.0; 3], 1)
            .await
            .unwrap()
            .is_empty());
        let expected = crate::types::EmbeddingContract {
            kind: "fixture".into(),
            model: Some("fixture-v1".into()),
            dim: 3,
        };
        let err = store
            .vector_candidates_checked(
                &crate::types::SessionId::from("legacy"),
                &[0.0; 3],
                &expected,
                1,
            )
            .await
            .unwrap_err();
        assert!(matches!(err, crate::types::StoreError::Capability(_)));
        assert!(
            err.to_string().contains("atomic embedding-contract"),
            "{err}"
        );
    }
}
