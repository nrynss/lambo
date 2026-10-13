//! The offline split's destination (#33 D2): a new SQLite file that holds
//! only split targets, each imported in one `IMMEDIATE` transaction together
//! with its `split_imports` receipt.
//!
//! **Transaction boundaries.** [`SqliteStore::provision_split_destination`]
//! runs the schema DDL on a brand-new private file before it is published
//! under the output name. [`SqliteStore::verify_split_destination`] only
//! reads. [`SqliteStore::import_split_target`] is one transaction per target:
//! it takes the write lock, checks the destination, then either verifies an
//! existing receipted target (no statement that writes) or inserts the whole
//! target, its receipt last, re-reads it on the same transaction, compares it
//! with the expected snapshot and commits once. Any failure before that
//! commit, a refusal or an injected fault included, rolls the target back
//! entirely; targets committed earlier stay, and a rerun verifies them with
//! zero writes and imports the rest.
//!
//! **Why the receipt table is not in `001_init.sql`.** The preflight of every
//! ordinary store derives its required tables from that file, so a new table
//! there would refuse every store provisioned before it. `split_imports`
//! exists only in split destinations. It has no foreign key: a receipt is a
//! standalone record of what was imported, so ordinary verbs on the output
//! (`erase-session` among them) never trip over it. A rerun after such an
//! erase finds a receipt without its target and refuses.

use std::collections::BTreeSet;

use sqlx::Row;

use super::codec::{db_err, enum_to_text, ts_to_text};
use super::persistence::BULK_LIMITS;
use super::session_load::load_snapshot_in;
use super::write_rows::{upsert_concepts, upsert_edges, upsert_interactions};
use super::SqliteStore;
use crate::session::canonical_target;
use crate::store::batch::{seed_concept_rows, seed_edge_rows};
use crate::store::map_write_err;
use crate::types::{GraphSnapshot, StoreError};

/// The receipt table, created only in a new split destination.
pub(crate) const SPLIT_IMPORTS_DDL: &str = "CREATE TABLE split_imports (\
    target_session      TEXT PRIMARY KEY, \
    split_sha256        TEXT NOT NULL, \
    target_sha256       TEXT NOT NULL, \
    algorithm_version   INTEGER NOT NULL, \
    source_sha256       TEXT NOT NULL, \
    plan_sha256         TEXT NOT NULL, \
    interactions        INTEGER NOT NULL, \
    concepts            INTEGER NOT NULL, \
    edges               INTEGER NOT NULL, \
    canonization_events INTEGER NOT NULL, \
    synonyms            INTEGER NOT NULL)";

/// What a committed target import recorded about itself.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct SplitReceipt {
    pub target_session: String,
    pub split_sha256: String,
    pub target_sha256: String,
    pub algorithm_version: u32,
    pub source_sha256: String,
    pub plan_sha256: String,
    pub interactions: usize,
    pub concepts: usize,
    pub edges: usize,
    pub canonization_events: usize,
    pub synonyms: usize,
}

impl SplitReceipt {
    /// The receipt a complete import of `snapshot` records.
    pub(crate) fn for_target(
        snapshot: &GraphSnapshot,
        split_sha256: &str,
        target_sha256: &str,
        algorithm_version: u32,
        source_sha256: &str,
        plan_sha256: &str,
    ) -> Self {
        Self {
            target_session: snapshot.session_id.0.clone(),
            split_sha256: split_sha256.into(),
            target_sha256: target_sha256.into(),
            algorithm_version,
            source_sha256: source_sha256.into(),
            plan_sha256: plan_sha256.into(),
            interactions: snapshot.interactions.len(),
            concepts: snapshot.concepts.len(),
            edges: snapshot.edges.len(),
            canonization_events: snapshot.canonization_events.len(),
            synonyms: snapshot.synonyms.len(),
        }
    }
}

/// How one target import ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SplitImportOutcome {
    /// The target was absent and is now committed; `changes` is SQLite's
    /// count of rows the transaction inserted.
    Imported { changes: u64 },
    /// A receipted, exactly matching target was already there. The
    /// transaction ran no statement that writes, and SQLite's change counter
    /// confirmed it.
    AlreadyPresent,
}

/// A failure point inside a target import, for the rollback tests. Test
/// only: under `fixtures` alone nothing in the library would use it, and the
/// clippy rows deny dead code.
#[cfg(test)]
pub(crate) type SplitImportHook<'a> = &'a (dyn Fn(&str) -> Result<(), StoreError> + Sync);

/// The hook as the import sees it; production passes none.
type Hook<'a> = Option<&'a (dyn Fn(&str) -> Result<(), StoreError> + Sync)>;

fn step(hook: Hook<'_>, at: &str) -> Result<(), StoreError> {
    hook.map_or(Ok(()), |h| h(at))
}

fn refused(msg: impl Into<String>) -> StoreError {
    StoreError::Invariant(format!("split destination refused: {}", msg.into()))
}

/// Tables whose rows carry a `session_id`.
const SESSION_TABLES: [&str; 11] = [
    "sessions",
    "interactions",
    "concepts",
    "edges",
    "synonyms",
    "canonization_events",
    "reservations",
    "session_leases",
    "session_stats",
    "write_intents",
    "lease_refusals",
];

async fn read_receipt(
    tx: &mut sqlx::SqliteConnection,
    target: &str,
) -> Result<Option<SplitReceipt>, StoreError> {
    let row = sqlx::query(
        "SELECT target_session, split_sha256, target_sha256, algorithm_version, source_sha256, \
                plan_sha256, interactions, concepts, edges, canonization_events, synonyms \
         FROM split_imports WHERE target_session = ?",
    )
    .bind(target)
    .fetch_optional(&mut *tx)
    .await
    .map_err(|e| db_err("split: read receipt", e))?;
    let Some(row) = row else { return Ok(None) };
    let int = |n: usize| -> Result<usize, StoreError> {
        let v: i64 = row
            .try_get(n)
            .map_err(|e| db_err("split: read receipt", e))?;
        usize::try_from(v).map_err(|_| refused("a receipt count is negative"))
    };
    let text = |n: usize| -> Result<String, StoreError> {
        row.try_get(n).map_err(|e| db_err("split: read receipt", e))
    };
    Ok(Some(SplitReceipt {
        target_session: text(0)?,
        split_sha256: text(1)?,
        target_sha256: text(2)?,
        algorithm_version: u32::try_from(int(3)?)
            .map_err(|_| refused("a receipt version is out of range"))?,
        source_sha256: text(4)?,
        plan_sha256: text(5)?,
        interactions: int(6)?,
        concepts: int(7)?,
        edges: int(8)?,
        canonization_events: int(9)?,
        synonyms: int(10)?,
    }))
}

/// The destination must hold this split's targets and nothing else: every
/// session row receipted by this split, every receipt this split's, and no
/// session-keyed row outside the receipted sessions.
async fn check_ownership(
    tx: &mut sqlx::SqliteConnection,
    split_sha256: &str,
    targets: &BTreeSet<String>,
) -> Result<BTreeSet<String>, StoreError> {
    let receipts: Vec<(String, String)> =
        sqlx::query_as("SELECT target_session, split_sha256 FROM split_imports")
            .fetch_all(&mut *tx)
            .await
            .map_err(|e| db_err("split: list receipts", e))?;
    let mut receipted = BTreeSet::new();
    for (session, sha) in receipts {
        if sha != split_sha256 || !targets.contains(&session) {
            return Err(refused(
                "it holds a target of a different split; use a new --output-database",
            ));
        }
        receipted.insert(session);
    }
    for table in SESSION_TABLES {
        let sessions: Vec<String> =
            sqlx::query_scalar(&format!("SELECT DISTINCT session_id FROM {table}"))
                .fetch_all(&mut *tx)
                .await
                .map_err(|e| db_err("split: list destination sessions", e))?;
        if sessions.iter().any(|s| !receipted.contains(s)) {
            return Err(refused(format!(
                "its {table} table holds rows of a session without a receipt of this split"
            )));
        }
    }
    let leases: i64 = sqlx::query_scalar("SELECT count(*) FROM session_leases")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| db_err("split: read leases", e))?;
    if leases != 0 {
        return Err(refused(
            "a writer has attached to it; it is no longer a split output",
        ));
    }
    Ok(receipted)
}

async fn total_changes(tx: &mut sqlx::SqliteConnection) -> Result<i64, StoreError> {
    sqlx::query_scalar("SELECT total_changes()")
        .fetch_one(&mut *tx)
        .await
        .map_err(|e| db_err("split: read change counter", e))
}

/// Refuse when any id the target will insert already names a row.
async fn check_ids_free(
    tx: &mut sqlx::SqliteConnection,
    s: &GraphSnapshot,
) -> Result<(), StoreError> {
    let groups: [(&str, Vec<String>); 4] = [
        (
            "interactions",
            s.interactions.iter().map(|i| i.id.0.to_string()).collect(),
        ),
        (
            "concepts",
            s.concepts.iter().map(|c| c.id.0.to_string()).collect(),
        ),
        (
            "edges",
            s.edges.iter().map(|e| e.id.0.to_string()).collect(),
        ),
        (
            "canonization_events",
            s.canonization_events
                .iter()
                .map(|e| e.id.0.to_string())
                .collect(),
        ),
    ];
    for (table, ids) in groups {
        for chunk in ids.chunks(400) {
            let marks = vec!["?"; chunk.len()].join(", ");
            let sql = format!("SELECT count(*) FROM {table} WHERE id IN ({marks})");
            let mut q = sqlx::query_scalar::<_, i64>(&sql);
            for id in chunk {
                q = q.bind(id);
            }
            let n = q
                .fetch_one(&mut *tx)
                .await
                .map_err(|e| db_err("split: check id collisions", e))?;
            if n != 0 {
                return Err(refused(format!(
                    "a {table} id of the target already exists"
                )));
            }
        }
    }
    Ok(())
}

async fn insert_target(
    tx: &mut sqlx::SqliteConnection,
    s: &GraphSnapshot,
    receipt: &SplitReceipt,
    hook: Hook<'_>,
) -> Result<(), StoreError> {
    let (kind, model, dim) = match &s.embedding {
        Some(c) => (
            Some(c.kind.as_str()),
            c.model.as_deref(),
            Some(i64::try_from(c.dim).map_err(|_| refused("embedding width out of range"))?),
        ),
        None => (None, None, None),
    };
    let created_at = s
        .created_at
        .ok_or_else(|| refused("a target has no created_at"))?;
    if s.root_goal.is_some()
        || s.closed_at.is_some()
        || !s.reservations.is_empty()
        || !s.write_intents.is_empty()
    {
        return Err(refused(
            "a split target carries a root goal, close time, reservation or write intent",
        ));
    }
    let too_big = |_| refused("a counter exceeds the SQLite integer range");
    sqlx::query(
        "INSERT INTO sessions (session_id, root_goal, created_at, closed_at, embedding_kind, \
             embedding_model, embedding_dim, mutation_epoch, last_gc_epoch, last_gc_at) \
         VALUES (?, NULL, ?, NULL, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&s.session_id.0)
    .bind(ts_to_text(created_at))
    .bind(kind)
    .bind(model)
    .bind(dim)
    .bind(i64::try_from(s.mutation_epoch).map_err(too_big)?)
    .bind(i64::try_from(s.gc_mark.last_gc_epoch).map_err(too_big)?)
    .bind(s.gc_mark.last_gc_at.map(ts_to_text))
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("split: insert session: {m}")))?;
    step(hook, "sessions")?;
    // Chain order (head first), so every previous_id already exists.
    let interactions: Vec<_> = s.interactions.iter().collect();
    for chunk in interactions.chunks(BULK_LIMITS.interactions) {
        upsert_interactions(&mut *tx, chunk).await?;
    }
    step(hook, "interactions")?;
    for chunk in seed_concept_rows(&s.concepts).chunks(BULK_LIMITS.concepts) {
        upsert_concepts(&mut *tx, chunk).await?;
    }
    step(hook, "concepts")?;
    for chunk in seed_edge_rows(&s.edges).chunks(BULK_LIMITS.edges) {
        upsert_edges(&mut *tx, chunk).await?;
    }
    step(hook, "edges")?;
    for syn in &s.synonyms {
        sqlx::query(
            "INSERT INTO synonyms (session_id, source_key, canonical_key) VALUES (?, ?, ?)",
        )
        .bind(&syn.session_id.0)
        .bind(&syn.source_key)
        .bind(&syn.canonical_key)
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("split: insert synonym: {m}")))?;
    }
    step(hook, "synonyms")?;
    for ev in &s.canonization_events {
        sqlx::query(
            "INSERT INTO canonization_events (id, session_id, node_id, from_status, to_status, \
                 blast_radius, last_demotion_time, occurred_at) \
             VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(ev.id.0.to_string())
        .bind(&ev.session_id.0)
        .bind(ev.node_id.0.to_string())
        .bind(enum_to_text(&ev.from_status, "from_status")?)
        .bind(enum_to_text(&ev.to_status, "to_status")?)
        .bind(ev.blast_radius)
        .bind(ev.last_demotion_time.map(ts_to_text))
        .bind(ts_to_text(ev.occurred_at))
        .execute(&mut *tx)
        .await
        .map_err(|e| map_write_err(e, |m| format!("split: insert canonization event: {m}")))?;
    }
    step(hook, "canonization_events")?;
    let count = |n: usize| i64::try_from(n).map_err(|_| refused("a count is out of range"));
    sqlx::query(
        "INSERT INTO split_imports (target_session, split_sha256, target_sha256, \
             algorithm_version, source_sha256, plan_sha256, interactions, concepts, edges, \
             canonization_events, synonyms) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&receipt.target_session)
    .bind(&receipt.split_sha256)
    .bind(&receipt.target_sha256)
    .bind(i64::from(receipt.algorithm_version))
    .bind(&receipt.source_sha256)
    .bind(&receipt.plan_sha256)
    .bind(count(receipt.interactions)?)
    .bind(count(receipt.concepts)?)
    .bind(count(receipt.edges)?)
    .bind(count(receipt.canonization_events)?)
    .bind(count(receipt.synonyms)?)
    .execute(&mut *tx)
    .await
    .map_err(|e| map_write_err(e, |m| format!("split: insert receipt: {m}")))?;
    step(hook, "receipt")
}

/// Whether the target and its receipt as stored on `tx` are exactly
/// `expected` and `receipt`.
async fn matches_expected(
    tx: &mut sqlx::SqliteConnection,
    expected: &GraphSnapshot,
    receipt: &SplitReceipt,
) -> Result<bool, StoreError> {
    if read_receipt(&mut *tx, &receipt.target_session)
        .await?
        .as_ref()
        != Some(receipt)
    {
        return Ok(false);
    }
    let stored = load_snapshot_in(&mut *tx, &expected.session_id).await?;
    Ok(stored.map(canonical_target) == Some(canonical_target(expected.clone())))
}

impl SqliteStore {
    /// Create the schema in a brand-new destination file: the store DDL plus
    /// the receipt table. Never called on an existing output.
    pub(crate) async fn provision_split_destination(&self) -> Result<(), StoreError> {
        let tables: i64 = sqlx::query_scalar("SELECT count(*) FROM sqlite_master")
            .fetch_one(self.pool())
            .await
            .map_err(|e| db_err("split: inspect new destination", e))?;
        if tables != 0 {
            return Err(refused("a new destination file is not empty"));
        }
        self.apply_schema().await?;
        sqlx::query(SPLIT_IMPORTS_DDL)
            .execute(self.pool())
            .await
            .map_err(|e| db_err("split: create receipt table", e))?;
        Ok(())
    }

    /// Read-only checks of an existing destination before any import: the
    /// store schema and the receipt table are present, and it holds only
    /// targets of this split. Runs no DDL and no DML.
    pub(crate) async fn verify_split_destination(
        &self,
        split_sha256: &str,
        targets: &BTreeSet<String>,
    ) -> Result<BTreeSet<String>, StoreError> {
        self.verify_schema().await?;
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| db_err("split: begin destination check", e))?;
        let present: Option<String> = sqlx::query_scalar(
            "SELECT name FROM sqlite_master WHERE type = 'table' AND name = 'split_imports'",
        )
        .fetch_optional(&mut *tx)
        .await
        .map_err(|e| db_err("split: inspect destination", e))?;
        if present.is_none() {
            return Err(refused(
                "the file exists but is not a split output; choose a new --output-database",
            ));
        }
        let receipted = check_ownership(&mut tx, split_sha256, targets).await?;
        tx.rollback()
            .await
            .map_err(|e| db_err("split: end destination check", e))?;
        Ok(receipted)
    }

    /// Read one committed target of this split for `lambo session verify`
    /// (#33 D3), in one read transaction: the destination holds this split's
    /// targets and nothing else, every target in `targets` is receipted, the
    /// stored receipt equals `receipt`, and the stored rows equal `expected`
    /// (in canonical order). Returns the stored snapshot. Runs no DDL and no
    /// DML; on a read-only connection.
    pub(crate) async fn read_verified_split_target(
        &self,
        receipt: &SplitReceipt,
        expected: &GraphSnapshot,
        targets: &BTreeSet<String>,
    ) -> Result<GraphSnapshot, StoreError> {
        let mut tx = self
            .pool()
            .begin()
            .await
            .map_err(|e| db_err("split: begin verify read", e))?;
        let receipted = check_ownership(&mut tx, &receipt.split_sha256, targets).await?;
        if &receipted != targets {
            return Err(refused(
                "the split output does not hold every target of this split; finish the split \
                 (rerun it) before verifying",
            ));
        }
        let target = &receipt.target_session;
        if read_receipt(&mut tx, target).await?.as_ref() != Some(receipt) {
            return Err(refused(format!(
                "target {target}'s receipt does not match this source and plan"
            )));
        }
        let stored = load_snapshot_in(&mut tx, &expected.session_id)
            .await?
            .ok_or_else(|| refused(format!("target {target} is receipted but missing")))?;
        tx.rollback()
            .await
            .map_err(|e| db_err("split: end verify read", e))?;
        if canonical_target(stored.clone()) != canonical_target(expected.clone()) {
            return Err(refused(format!(
                "target {target} changed after its import; it no longer verifies"
            )));
        }
        Ok(stored)
    }

    /// Import one target atomically, or verify it when this split already
    /// committed it. See the module doc for the transaction boundaries.
    pub(crate) async fn import_split_target(
        &self,
        receipt: &SplitReceipt,
        expected: &GraphSnapshot,
        targets: &BTreeSet<String>,
    ) -> Result<SplitImportOutcome, StoreError> {
        self.import_with(receipt, expected, targets, None).await
    }

    /// [`Self::import_split_target`] with a fault hook between steps.
    #[cfg(test)]
    pub(crate) async fn import_split_target_hooked(
        &self,
        receipt: &SplitReceipt,
        expected: &GraphSnapshot,
        targets: &BTreeSet<String>,
        hook: SplitImportHook<'_>,
    ) -> Result<SplitImportOutcome, StoreError> {
        self.import_with(receipt, expected, targets, Some(hook))
            .await
    }

    async fn import_with(
        &self,
        receipt: &SplitReceipt,
        expected: &GraphSnapshot,
        targets: &BTreeSet<String>,
        hook: Hook<'_>,
    ) -> Result<SplitImportOutcome, StoreError> {
        if receipt.target_session != expected.session_id.0 {
            return Err(refused("receipt and target name different sessions"));
        }
        let mut tx = self
            .pool()
            .begin_with("BEGIN IMMEDIATE")
            .await
            .map_err(|e| map_write_err(e, |m| format!("split: begin import: {m}")))?;
        let before = total_changes(&mut tx).await?;
        let receipted = check_ownership(&mut tx, &receipt.split_sha256, targets).await?;
        let target = &receipt.target_session;
        match read_receipt(&mut tx, target).await? {
            Some(stored) => {
                if &stored != receipt {
                    return Err(refused(format!(
                        "target {target} has a receipt that does not match this split"
                    )));
                }
                if !matches_expected(&mut tx, expected, receipt).await? {
                    return Err(refused(format!(
                        "target {target} changed after its import; it no longer verifies"
                    )));
                }
                let changes = total_changes(&mut tx).await? - before;
                tx.rollback()
                    .await
                    .map_err(|e| db_err("split: end verification", e))?;
                if changes != 0 {
                    return Err(refused("a verification wrote to the destination"));
                }
                Ok(SplitImportOutcome::AlreadyPresent)
            }
            None => {
                if receipted.contains(target) {
                    return Err(refused(format!("target {target} lost its receipt")));
                }
                check_ids_free(&mut tx, expected).await?;
                insert_target(&mut tx, expected, receipt, hook).await?;
                if !matches_expected(&mut tx, expected, receipt).await? {
                    return Err(refused(format!(
                        "target {target} did not read back exactly as built; rolled back"
                    )));
                }
                let changes = total_changes(&mut tx).await? - before;
                tx.commit()
                    .await
                    .map_err(|e| map_write_err(e, |m| format!("split: commit import: {m}")))?;
                Ok(SplitImportOutcome::Imported {
                    changes: u64::try_from(changes).unwrap_or(0),
                })
            }
        }
    }

    /// End a split's use of a destination file: fold any WAL back into the
    /// file (a no-op in the rollback-journal mode new outputs use, and for
    /// an empty WAL), then close the pool so nothing is left open before
    /// the file is linked into place or the command exits. A checkpoint
    /// failure is returned, not swallowed.
    pub(crate) async fn finish_split_destination(&self) -> Result<(), StoreError> {
        let Some(pool) = self.pool.get() else {
            return Ok(());
        };
        let checkpoint = sqlx::query("PRAGMA wal_checkpoint(TRUNCATE)")
            .execute(pool)
            .await
            .map(|_| ())
            .map_err(|e| db_err("split: checkpoint destination", e));
        pool.close().await;
        checkpoint
    }
}
