//! #148 on Postgres and Cockroach: read, plan
//! ([`crate::store::rekey::plan_rekey`]) and write the re-key in one
//! transaction. `lambo provision` runs it on both, after the schema.

use std::collections::HashMap;

use super::codec::backend;
use super::dialect::Dialect;
use super::PgStore;
use crate::store::rekey::{live_lease_err, placeholder_key, plan_rekey, KeyRow, RekeyPlan};
use crate::types::StoreError;

impl<D: Dialect> PgStore<D> {
    pub(super) async fn rekey_keys(&self) -> Result<RekeyPlan, StoreError> {
        let pool = self.pool().await?;
        let mut tx = pool.begin().await.map_err(backend)?;
        let rows: Vec<(uuid::Uuid, String, String, String, String)> = sqlx::query_as(
            "SELECT id, session_id, content, canonical_key, concept_type FROM concepts \
             ORDER BY id FOR UPDATE",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(backend)?;
        let rows: Vec<KeyRow> = rows
            .into_iter()
            .map(|(id, session_id, content, canonical_key, ty)| KeyRow {
                id: id.to_string(),
                session_id,
                content,
                canonical_key,
                observation: ty == "Observation",
            })
            .collect();
        let synonyms: Vec<(String, String, String)> =
            sqlx::query_as("SELECT session_id, source_key, canonical_key FROM synonyms")
                .fetch_all(&mut *tx)
                .await
                .map_err(backend)?;
        let synonyms: HashMap<(String, String), String> =
            synonyms.into_iter().map(|(s, k, v)| ((s, k), v)).collect();
        let plan = plan_rekey(&rows, &synonyms);
        if plan.changes.is_empty() {
            tx.rollback().await.map_err(backend)?;
            return Ok(plan);
        }
        let live: Vec<(String, String)> = sqlx::query_as(
            "SELECT session_id, holder FROM session_leases WHERE expires_at > now()",
        )
        .fetch_all(&mut *tx)
        .await
        .map_err(backend)?;
        if let Some((session, holder)) = crate::store::rekey::first_live_holder(&live) {
            return Err(live_lease_err(session, holder, plan.changes.len()));
        }
        for last in [false, true] {
            for change in &plan.changes {
                let (from, to) = if last {
                    (placeholder_key(&change.id), change.new_key.clone())
                } else {
                    (change.old_key.clone(), placeholder_key(&change.id))
                };
                let id = uuid::Uuid::parse_str(&change.id).map_err(backend)?;
                let done = sqlx::query(
                    "UPDATE concepts SET canonical_key = $1 WHERE id = $2 AND canonical_key = $3",
                )
                .bind(&to)
                .bind(id)
                .bind(&from)
                .execute(&mut *tx)
                .await
                .map_err(backend)?;
                if done.rows_affected() != 1 {
                    return Err(StoreError::Backend(format!(
                        "rekey: concept {} changed under the re-key",
                        change.id
                    )));
                }
            }
        }
        tx.commit().await.map_err(backend)?;
        Ok(plan)
    }
}

/// #148 on live Postgres: a session keyed by the old rule is re-keyed by the
/// provision step, a held session refuses it, and a rerun is a no-op. Run it
/// by name, alone (`--exact`): the re-key is store-wide, so a live test
/// holding another session in parallel makes it refuse.
#[cfg(feature = "store-postgres")]
#[tokio::test]
#[ignore = "live: requires LAMBO_POSTGRES_DSN against pinned pgvector/pgvector:pg17"]
async fn postgres_rekeys_identifier_concepts() {
    use std::time::Duration;

    use super::postgres::{postgres_dsn_or_skip, PostgresStore};
    use crate::store::lease::{LeaseHolder, LeaseOutcome};
    use crate::store::rekey::testkit::{check_rekeyed, plant_legacy};
    use crate::store::{GraphStore, StoreConfig, StoreKind};
    use crate::types::{AgentId, SessionId};

    let Some(dsn) = postgres_dsn_or_skip("postgres_rekeys_identifier_concepts") else {
        return;
    };
    let store = PostgresStore::new(StoreConfig {
        kind: StoreKind::Postgres,
        dsn: Some(dsn),
        path: None,
        vector_dim: None,
    })
    .expect("construct");
    store.init_schema().await.expect("init_schema");
    let sid = SessionId::from(format!("rekey-{}", uuid::Uuid::new_v4()));
    let holder = LeaseHolder {
        endpoint: None,
        agent: AgentId::new("rekey-test"),
        pid: 1,
        host: "test".into(),
    };
    let LeaseOutcome::Acquired(lease) = store
        .acquire_lease(&sid, &holder, Duration::from_secs(60))
        .await
        .expect("acquire")
    else {
        panic!("a fresh session's lease is free");
    };
    plant_legacy(&store, &sid, Some(lease.token)).await;
    let err = store
        .rekey_canonical_keys()
        .await
        .expect_err("a held session refuses the re-key");
    assert!(err.to_string().contains("lambo provision"), "{err}");
    store.release_lease(&sid, &holder).await.expect("release");
    check_rekeyed(&store, &sid).await;
    store
        .erase_session(&sid, &holder)
        .await
        .expect("erase the test session");
}
