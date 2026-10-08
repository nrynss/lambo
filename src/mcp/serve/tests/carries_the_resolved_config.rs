use super::*;
use crate::canon::PromotionPolicy;
use crate::embed::FixtureEmbedder;
use crate::store::{MemoryStore, StoreConfig};
use crate::types::EmbeddingContract;

fn backends(config: crate::Config) -> ResolvedBackends {
    ResolvedBackends {
        store: Box::new(MemoryStore::new()),
        embedder: Box::new(FixtureEmbedder::new()),
        store_cfg: StoreConfig {
            kind: Default::default(),
            dsn: None,
            path: None,
            vector_dim: None,
        },
        embedder_cfg: Default::default(),
        embedding: EmbeddingContract {
            kind: "fixture".into(),
            model: None,
            dim: 1024,
        },
        allow_embedding_mismatch: false,
        config,
    }
}

#[tokio::test]
async fn serve_builder_forwards_the_resolved_promotion_policy() {
    for policy in PromotionPolicy::ALL {
        let session = format!("serve-policy-{}", policy.as_str().to_ascii_lowercase());
        let opts = ServeOptions::new(session.clone(), "agent-a");
        // A non-default cadence rides along as the control: if the
        // whole `.config(..)` call went missing rather than just this
        // field, both assertions fail and say so separately.
        let resolved = crate::Config {
            promotion_policy: policy,
            gc_interval: 17,
            ..crate::Config::default()
        };
        let mem = serve_builder(
            &opts,
            backends(resolved),
            None,
            None,
            EarlyShutdown::unarmed(),
        )
        .build()
        .await
        .expect("the serve builder attaches");
        assert_eq!(
            mem.config().promotion_policy,
            policy,
            "the policy `lambo serve` resolved must be the policy the \
                     Memory it built canonizes under"
        );
        assert_eq!(
            mem.config().gc_interval,
            17,
            "the resolved config as a whole must cross the seam"
        );
        mem.close().await.expect("close");
    }
}
