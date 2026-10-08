//! Issue #30: recall and inspect accesses.

use super::*;

// -----------------------------------------------------------------------
// Issue #30 — recall and inspect accesses
// -----------------------------------------------------------------------

fn access_of(mem: &Memory, content: &str) -> (i32, Option<DateTime<Utc>>) {
    let g = mem.graph.read();
    let c = g
        .concepts()
        .find(|c| c.content == content)
        .unwrap_or_else(|| panic!("no concept {content:?}"));
    (c.access_count, c.last_accessed)
}

/// Poll (bounded) until the daemon has applied the ledger.
async fn settle_accesses(mem: &Memory) {
    tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            mem.daemon.wake();
            if mem.accesses.pending() == 0 {
                // One more full cycle so the take-then-apply has finished.
                let before = mem.daemon.cycles();
                mem.daemon.wake();
                while mem.daemon.cycles() == before {
                    tokio::time::sleep(Duration::from_millis(5)).await;
                }
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the daemon applies noted accesses within one cycle");
}

/// Every concept a recall returns counts once per recall — including a
/// recall the cache served (#14: a cached hit is still an access) — and
/// a concept the recall did not return does not count. None of it moves
/// the mutation epoch.
#[tokio::test]
async fn every_returned_hit_counts_once_per_recall_cache_served_or_not() {
    let mem = memory_on(Arc::new(MemoryStore::new()), "issue-30-recall").await;
    mem.derive(
        &[
            ("user schema", ConceptType::Entity),
            ("billing ledger", ConceptType::Entity),
        ],
        &ParentOf::none(),
    )
    .await
    .unwrap();
    // Let the daemon rescore the new epoch so the pipeline is cacheable
    // (P5-3: a compute whose scores lag the epoch is not cached).
    tokio::time::timeout(Duration::from_secs(10), async {
        while mem.daemon.scores().epoch != mem.graph.read().epoch() {
            mem.daemon.wake();
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    let epoch = mem.stats().epoch;

    for _ in 0..3 {
        let r = mem
            .recall(RecallQuery {
                top_k: 1,
                ..query("user schema")
            })
            .await
            .unwrap();
        let returned: Vec<&str> = r.hits.iter().map(|h| h.content.as_str()).collect();
        assert_eq!(returned, ["user schema"], "top_k 1 returns only the match");
    }
    assert_eq!(
        mem.recall_cache.lock().await.len(),
        1,
        "MemoryStore has no vector leg, so recalls 2 and 3 were cache-served"
    );
    settle_accesses(&mem).await;

    let (count, last) = access_of(&mem, "user schema");
    assert_eq!(count, 3, "three recalls, three accesses");
    assert!(last.is_some());
    assert_eq!(access_of(&mem, "billing ledger"), (0, None));
    assert_eq!(mem.stats().epoch, epoch, "reads never advance the epoch");
    mem.close().await.unwrap();
}

/// `close` applies whatever the daemon had not, in the final drain, and
/// the counts come back on the next attach with the epoch where the last
/// real write left it.
///
/// Deterministic about *who* applies: a one-hour daemon tick and a settled
/// score table mean no cycle runs between the recalls and `close` (reads
/// never wake the daemon), and the premise is asserted right before close.
/// Without close's own apply the reattached count is 0.
#[tokio::test]
async fn accesses_survive_close_and_reattach_without_moving_the_epoch() {
    let store: Arc<dyn GraphStore> = Arc::new(MemoryStore::new());
    let slow_daemon = || Config {
        daemon_tick_interval: Duration::from_secs(3_600),
        ..Config::default()
    };
    let mem = Memory::builder()
        .session("issue-30-restart")
        .agent("agent-a")
        .config(slow_daemon())
        .flush_interval(Duration::from_secs(3_600))
        .store(store.clone())
        .embedder(Arc::new(FixtureEmbedder::new()) as Arc<dyn Embedder>)
        .embedding_contract(contract("fixture", 1024))
        .build()
        .await
        .expect("build");
    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    mem.settle_daemon().await;
    let epoch = mem.stats().epoch;
    let cycles = mem.daemon.cycles();
    mem.recall(query("user schema")).await.unwrap();
    mem.recall(query("user schema")).await.unwrap();
    assert_eq!(
        mem.unapplied_accesses(),
        1,
        "premise: the daemon has not applied the recalls; close must"
    );
    assert_eq!(mem.daemon.cycles(), cycles, "premise: no cycle ran");
    assert_eq!(access_of(&mem, "user schema"), (0, None));
    mem.close().await.unwrap();

    let again = memory_on(store, "issue-30-restart").await;
    let (count, last) = access_of(&again, "user schema");
    assert_eq!(count, 2);
    assert!(last.is_some());
    assert_eq!(again.stats().epoch, epoch);
    again.close().await.unwrap();
}

/// Issue #30 (close race): a recall that was in flight when `close` took
/// the ledger notes its hits after it. Those are dropped by the closed
/// ledger, explicitly, instead of sitting in a ledger nothing applies.
#[tokio::test]
async fn a_read_finishing_after_close_is_dropped_not_left_pending() {
    let mem = memory_on(Arc::new(MemoryStore::new()), "issue-30-late-read").await;
    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let id = mem.graph.read().concepts().next().unwrap().id;
    mem.close().await.unwrap();

    // What a recall's `note_accesses` does once its pipeline returns.
    mem.note_accesses([id]);
    assert_eq!(mem.unapplied_accesses(), 0);
    assert_eq!(mem.accesses.dropped_after_close(), 1);
}

/// The same round trip through the real SQLite adapter (file-backed, a
/// second `connect`), which is what a writer restart actually reads.
#[cfg(feature = "store-sqlite")]
#[tokio::test]
async fn accesses_are_durable_across_a_writer_restart_on_sqlite() {
    use crate::store::SqliteStore;
    let dir = crate::test_util::ScratchDir::new("lambo-issue30");
    let path = dir.join("memory.db");
    let open = |path: std::path::PathBuf| async move {
        let store = SqliteStore::connect(path.to_str().unwrap()).unwrap();
        store.init_schema().await.unwrap();
        Arc::new(store) as Arc<dyn GraphStore>
    };

    let mem = memory_on(open(path.clone()).await, "issue-30-sqlite").await;
    mem.derive(&[("user schema", ConceptType::Entity)], &ParentOf::none())
        .await
        .unwrap();
    let epoch = mem.stats().epoch;
    for _ in 0..4 {
        mem.recall(query("user schema")).await.unwrap();
    }
    settle_accesses(&mem).await;
    let applied = access_of(&mem, "user schema");
    assert_eq!(applied.0, 4);
    mem.recall(query("user schema")).await.unwrap();
    mem.close().await.unwrap();
    drop(mem);

    let again = memory_on(open(path).await, "issue-30-sqlite").await;
    let (count, last) = access_of(&again, "user schema");
    assert_eq!(count, 5, "four applied by the daemon, one by close");
    assert!(last >= applied.1);
    assert_eq!(
        again.stats().epoch,
        epoch,
        "the durable watermark did not move"
    );
    again.close().await.unwrap();
}
