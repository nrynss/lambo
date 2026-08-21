//! J3 durable-intent proof obligations, at the shipped binary
//! (`dev-diary/lambo-for-mooshik/J3-durability-redesign.md`, §Proof
//! obligations).
//!
//! Two of the design's obligations can only be observed through a store that
//! outlives a process, driven through the real `lambo serve` binary over
//! stdio — the same discipline as `serve_sigterm_durability.rs`:
//!
//! 1. **Replay**: durable intents left by a previous process are applied by
//!    the next serve, in per-lane admission order, and the ORIGINAL receipt
//!    ids answer `applied_after_restart` (agent-scoped) in the new process.
//! 2. **Idempotency under `kill -9` mid-replay**: consumption rides the same
//!    flush transaction as the apply, so however a crash interleaves with the
//!    replay and the flush cadence, the final durable state applies every
//!    intent **exactly once** — re-replayed when nothing was durable, skipped
//!    when apply+consume committed, and never half of each.
//!
//! Every durability assertion here reads the store's **embedding column** (or
//! edge state), never `applied` counts — the J3-R3-1 rule.
//!
//! The fixture embedder is instant, so close-time *deferral* cannot be forced
//! at this binary (a fixture queue always drains); the deferral half of the
//! invariant is pinned at `Memory` level
//! (`an_acked_write_survives_a_clean_close_as_a_durable_intent_and_replays`)
//! and at the pipeline
//! (`a_close_that_cannot_drain_defers_acked_writes_as_durable_intents`), and
//! demonstrated against the live BGE-M3 in §J3's status note.
#![cfg(all(
    feature = "store-sqlite",
    feature = "embed-fixture",
    feature = "fixtures",
    unix
))]

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use chrono::Utc;
use lambo::store::{GraphStore, SqliteStore};
use lambo::types::{
    AgentId, ConceptType, EmbeddingContract, GraphSnapshot, Interaction, NodeId, SessionId,
    WriteIntent, WriteIntentPayload,
};

/// A fabricated previous-process epoch, distinct from anything a live pipeline
/// can mint for itself in this test's lifetime.
const OLD_EPOCH: &str = "00000000deadbeef";

fn receipt_id(seq: u64) -> String {
    // The wire form `ReceiptId` prints and parses: lwr1.{epoch:016x}.{ms:x}.{seq:x}
    format!("lwr1.{OLD_EPOCH}.18f00000000.{seq:x}")
}

fn intent(
    session: &SessionId,
    interaction: NodeId,
    agent: &str,
    seq: u64,
    payload: WriteIntentPayload,
) -> WriteIntent {
    WriteIntent {
        session_id: session.clone(),
        receipt: receipt_id(seq),
        agent: AgentId::new(agent),
        interaction,
        lane_seq: seq,
        issued_ms: 1_755_000_000_000,
        payload,
        created_at: Utc::now(),
        outcome: None,
    }
}

fn derive_payload(content: &str) -> WriteIntentPayload {
    WriteIntentPayload::Derive {
        concepts: vec![(content.to_string(), ConceptType::Entity)],
        pairs: Vec::new(),
    }
}

struct Serve {
    child: Child,
    stdin: ChildStdin,
    rx: mpsc::Receiver<String>,
    next_id: u64,
    _reader: std::thread::JoinHandle<()>,
}

impl Serve {
    fn launch(cfg: &std::path::Path, session: &str) -> Self {
        let mut child = Command::new(env!("CARGO_BIN_EXE_lambo"))
            .args([
                "--config",
                cfg.to_str().unwrap(),
                "serve",
                "--session",
                session,
                "--agent",
                "agent-a",
                "--transport",
                "stdio",
            ])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn lambo serve");
        let stdout = child.stdout.take().expect("child stdout");
        let (tx, rx) = mpsc::channel::<String>();
        let reader = std::thread::spawn(move || {
            let mut lines = BufReader::new(stdout).lines();
            while let Some(Ok(line)) = lines.next() {
                if tx.send(line).is_err() {
                    break;
                }
            }
        });
        let stdin = child.stdin.take().expect("child stdin");
        let mut serve = Serve {
            child,
            stdin,
            rx,
            next_id: 1,
            _reader: reader,
        };
        serve.handshake();
        serve
    }

    fn write_frame(&mut self, frame: &str) {
        self.stdin.write_all(frame.as_bytes()).expect("write frame");
        self.stdin.write_all(b"\n").expect("write newline");
        self.stdin.flush().expect("flush frame");
    }

    fn read_response(&mut self, id: u64) -> serde_json::Value {
        let needle = format!("\"id\": {id}");
        let needle_compact = format!("\"id\":{id}");
        loop {
            let line = self
                .rx
                .recv_timeout(Duration::from_secs(30))
                .unwrap_or_else(|e| panic!("no JSON-RPC frame with id {id} within 30s: {e}"));
            if line.contains(&needle) || line.contains(&needle_compact) {
                return serde_json::from_str(&line)
                    .unwrap_or_else(|e| panic!("unparseable frame {line:?}: {e}"));
            }
        }
    }

    fn handshake(&mut self) {
        let id = self.next_id;
        self.next_id += 1;
        self.write_frame(&format!(
            r#"{{"jsonrpc":"2.0","id":{id},"method":"initialize","params":{{"protocolVersion":"2025-06-18","capabilities":{{}},"clientInfo":{{"name":"intent-test","version":"1"}}}}}}"#
        ));
        let init = self.read_response(id);
        assert!(
            init["result"]["serverInfo"].is_object(),
            "no initialize result: {init}"
        );
        self.write_frame(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);
    }

    fn call(&mut self, tool: &str, args: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        let frame = serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": tool, "arguments": args },
        });
        self.write_frame(&frame.to_string());
        self.read_response(id)["result"].clone()
    }

    fn stats(&mut self, agent: &str) -> serde_json::Value {
        self.call("lambo_stats", serde_json::json!({ "agent_id": agent }))["structuredContent"]
            .clone()
    }

    fn receipt_state(&mut self, agent: &str, receipt: &str) -> serde_json::Value {
        self.call(
            "lambo_stats",
            serde_json::json!({ "agent_id": agent, "receipt": receipt }),
        )["structuredContent"]["receipt"]
            .clone()
    }

    /// SIGTERM and wait for a clean exit — the close path, releasing the lease.
    fn shutdown_clean(self) {
        let pid = self.child.id();
        let status = Command::new("kill")
            .arg("-TERM")
            .arg(pid.to_string())
            .status()
            .expect("spawn kill");
        assert!(status.success(), "kill -TERM {pid} failed");
        let (wait_tx, wait_rx) = mpsc::channel();
        let mut child = self.child;
        let waiter = std::thread::spawn(move || {
            let _ = wait_tx.send(child.wait());
        });
        match wait_rx.recv_timeout(Duration::from_secs(20)) {
            Ok(status) => {
                let status = status.expect("wait on child");
                assert!(status.success(), "serve must exit cleanly: {status:?}");
            }
            Err(_) => {
                let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
                let _ = waiter.join();
                panic!("serve did not exit within 20s of SIGTERM");
            }
        }
        let _ = waiter.join();
    }

    /// `kill -9`: the crash the design keeps honest. No close, no lease
    /// release — the caller clears the lease row with the documented operator
    /// override before relaunching.
    fn kill_nine(mut self) {
        let pid = self.child.id();
        let _ = Command::new("kill").arg("-9").arg(pid.to_string()).status();
        let _ = self.child.wait();
    }
}

struct Rig {
    dir: std::path::PathBuf,
    cfg: std::path::PathBuf,
    db: String,
    session: SessionId,
    rt: tokio::runtime::Runtime,
}

impl Rig {
    fn new(name: &str, session: &str) -> Self {
        let dir = std::env::temp_dir().join(format!(
            "lambo-intent-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&dir).expect("create scratch dir");
        let db = dir.join("intents.sqlite").to_str().unwrap().to_string();
        let cfg = dir.join("lambo.toml");
        std::fs::write(
            &cfg,
            format!(
                "[store]\nkind = \"sqlite\"\npath = \"{db}\"\n\n[embedder]\nkind = \"fixture\"\ndim = 1024\n"
            ),
        )
        .expect("write config");
        Rig {
            dir,
            cfg,
            db,
            session: SessionId::new(session),
            rt: tokio::runtime::Runtime::new().expect("runtime"),
        }
    }

    /// Provision the schema and seed one interaction plus `intents`, exactly
    /// as a previous process's close-time final flush would have left them.
    fn seed(&self, intents: Vec<WriteIntent>) -> NodeId {
        let interaction_id = NodeId::new();
        let session = self.session.clone();
        let db = self.db.clone();
        self.rt.block_on(async move {
            let store = SqliteStore::connect(&db).expect("connect for provision");
            store.init_schema().await.expect("init_schema");
            let snapshot = GraphSnapshot {
                session_id: session.clone(),
                embedding: Some(EmbeddingContract {
                    kind: "fixture".into(),
                    model: None,
                    dim: 1024,
                }),
                interactions: vec![Interaction {
                    id: interaction_id,
                    session_id: session,
                    agent_id: AgentId::new("agent-a"),
                    prompt_text: Some("seeded by the intent-durability test".into()),
                    previous_id: None,
                    created_at: Utc::now(),
                }],
                write_intents: intents,
                ..GraphSnapshot::default()
            };
            store.seed(&snapshot).await.expect("seed intents");
        });
        interaction_id
    }

    fn load(&self) -> GraphSnapshot {
        let db = self.db.clone();
        let session = self.session.clone();
        self.rt.block_on(async move {
            let store = SqliteStore::connect(&db).expect("reconnect");
            store.load_session(&session).await.expect("load_session")
        })
    }

    /// The documented operator override for a holder that died without
    /// releasing (`migrations/*/001_init.sql` beside `session_leases`): a
    /// `kill -9` leaves the lease row live for its whole TTL, and this test
    /// cannot wait 45 s.
    fn clear_lease(&self) {
        let db = self.db.clone();
        let session = self.session.0.clone();
        self.rt.block_on(async move {
            let pool = sqlx::sqlite::SqlitePoolOptions::new()
                .max_connections(1)
                .connect(&format!("sqlite://{db}"))
                .await
                .expect("open for lease override");
            sqlx::query("DELETE FROM session_leases WHERE session_id = ?")
                .bind(&session)
                .execute(&pool)
                .await
                .expect("operator lease override");
        });
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// Poll `lambo_stats` until the replay counter reaches `expected` (the replay
/// runs concurrently with serving, so the first stats call can race it).
fn await_replayed(serve: &mut Serve, expected: u64) -> serde_json::Value {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let stats = serve.stats("agent-a");
        let replayed = stats["write_queue_replayed"].as_u64().unwrap_or(0);
        if replayed >= expected {
            return stats;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "replay did not reach {expected} within 20s: {stats}"
        );
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// **Proof obligation 2, replay half**: unconsumed intents left by a previous
/// process are applied by the next serve — in per-lane admission order — and
/// the original receipt ids answer `applied_after_restart`, agent-scoped, in
/// the new process. Durability is judged at the embedding column.
#[test]
fn seeded_intents_replay_in_lane_order_and_answer_applied_after_restart() {
    let rig = Rig::new("replay", "j3-intent-replay");
    let interaction = rig.seed(vec![
        // Two derives of the SAME content on one lane: replayed in order, the
        // first creates (and embeds) and the second matches. An order
        // inversion would flip the two receipts' summaries.
        intent(
            &rig.session,
            interaction_placeholder(),
            "agent-a",
            1,
            derive_payload("replay order probe"),
        ),
        intent(
            &rig.session,
            interaction_placeholder(),
            "agent-a",
            2,
            derive_payload("replay order probe"),
        ),
        // A second lane, cross-kind.
        intent(
            &rig.session,
            interaction_placeholder(),
            "agent-b",
            3,
            WriteIntentPayload::Action {
                action: "seeded action replayed after restart".into(),
                produces: vec!["seeded artifact".into()],
                modifies: Vec::new(),
                depends_on: Vec::new(),
            },
        ),
    ]);
    // The placeholder ids must be the seeded interaction's real id.
    fix_interactions(&rig, interaction);

    let mut serve = Serve::launch(&rig.cfg, "j3-intent-replay");
    let stats = await_replayed(&mut serve, 3);
    assert_eq!(stats["write_queue_replayed"], 3, "{stats}");
    assert_eq!(
        stats["write_queue_accepted"], 0,
        "replay must not ride admission: {stats}"
    );

    // The ORIGINAL receipt ids answer in the NEW process — the cross-restart
    // receipt truth table.
    let first = serve.receipt_state("agent-a", &receipt_id(1));
    assert_eq!(first["state"], "applied_after_restart", "{first}");
    let detail = first["detail"].as_str().expect("detail");
    assert!(
        detail.contains("1 created (1 embedded)"),
        "the FIRST replay of this content creates and embeds: {detail}"
    );
    let second = serve.receipt_state("agent-a", &receipt_id(2));
    assert_eq!(second["state"], "applied_after_restart", "{second}");
    let detail = second["detail"].as_str().expect("detail");
    assert!(
        detail.contains("0 created (0 embedded), 1 matched"),
        "the SECOND replay of the same content matches — per-lane order across restart: {detail}"
    );
    // Agent-scoped across restart, like everything else about receipts.
    let foreign = serve.receipt_state("agent-b", &receipt_id(1));
    assert_eq!(foreign["state"], "forbidden", "{foreign}");
    let action = serve.receipt_state("agent-b", &receipt_id(3));
    assert_eq!(action["state"], "applied_after_restart", "{action}");

    serve.shutdown_clean();

    // The store's word, judged at the EMBEDDING column (J3-R3-1).
    let snap = rig.load();
    let concept = snap
        .concepts
        .iter()
        .find(|c| c.content == "replay order probe")
        .expect("the replayed concept landed");
    assert!(
        concept.embedding.is_some(),
        "durability is judged at the embedding column, never applied counts"
    );
    assert!(
        snap.concepts
            .iter()
            .any(|c| c.content == "seeded action replayed after restart"),
        "the replayed action landed"
    );
    for intent in &snap.write_intents {
        let outcome = intent
            .outcome
            .as_ref()
            .unwrap_or_else(|| panic!("intent {} still unconsumed", intent.receipt));
        assert_eq!(outcome.tag, "applied_after_restart", "{intent:?}");
    }
}

/// **Proof obligation 2, crash half**: `kill -9` mid-replay, restart, count.
/// Apply and consume ride one flush transaction, so whatever the crash
/// interleaved with the flush cadence, the final state applies every intent
/// exactly once: every Derives edge from the seeded interaction carries
/// `reinforcements == 1` (a double apply would reinforce it to 2), every
/// concept exists once WITH its embedding, and every intent is consumed once.
#[test]
fn a_kill_nine_mid_replay_re_replays_idempotently() {
    const INTENTS: u64 = 60;
    let rig = Rig::new("kill9", "j3-intent-kill9");
    let mut seeds = Vec::new();
    for seq in 1..=INTENTS {
        let agent = format!("agent-{}", seq % 3);
        seeds.push(intent(
            &rig.session,
            interaction_placeholder(),
            &agent,
            seq,
            derive_payload(&format!("kill-nine probe concept {seq:03}")),
        ));
    }
    let interaction = rig.seed(seeds);
    fix_interactions(&rig, interaction);

    // First serve: killed -9 while the replay's applies and consumes are, at
    // most, partially flushed (the flush interval is 1 s; the kill lands well
    // inside it, so on this rig the common interleaving is "nothing durable
    // yet" — the full re-replay case; a slower rig can land the partial one,
    // and the assertions below hold for every interleaving).
    let serve = Serve::launch(&rig.cfg, "j3-intent-kill9");
    std::thread::sleep(Duration::from_millis(150));
    serve.kill_nine();
    rig.clear_lease();

    // Second serve: replays whatever the crash left unconsumed (how much that
    // is depends on where the kill landed against the flush cadence — the
    // assertions below hold for every interleaving), then closes cleanly.
    let mut serve = Serve::launch(&rig.cfg, "j3-intent-kill9");
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    let mut last = u64::MAX;
    let mut stable = 0;
    while stable < 5 {
        assert!(
            std::time::Instant::now() < deadline,
            "the replay counter never stabilized"
        );
        let stats = serve.stats("agent-a");
        let replayed = stats["write_queue_replayed"].as_u64().unwrap_or(0);
        if replayed == last {
            stable += 1;
        } else {
            stable = 0;
            last = replayed;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    serve.shutdown_clean();

    let snap = rig.load();
    // Exactly once, judged at the store: every intent consumed…
    assert_eq!(snap.write_intents.len() as u64, INTENTS);
    for intent in &snap.write_intents {
        assert!(
            intent.outcome.is_some(),
            "intent {} was never applied by either process",
            intent.receipt
        );
    }
    // …every concept present once, WITH its embedding…
    for seq in 1..=INTENTS {
        let content = format!("kill-nine probe concept {seq:03}");
        let hits: Vec<_> = snap
            .concepts
            .iter()
            .filter(|c| c.content == content)
            .collect();
        assert_eq!(hits.len(), 1, "{content} must exist exactly once");
        assert!(
            hits[0].embedding.is_some(),
            "durability is judged at the embedding column: {content}"
        );
    }
    // …and no Derives edge reinforced twice: a consumed-durable intent whose
    // write was replayed again would reinforce its own edge.
    let concept_ids: std::collections::HashSet<_> = snap
        .concepts
        .iter()
        .filter(|c| c.content.starts_with("kill-nine probe concept"))
        .map(|c| c.id)
        .collect();
    for edge in snap
        .edges
        .iter()
        .filter(|e| e.source == interaction && concept_ids.contains(&e.target))
    {
        assert_eq!(
            edge.reinforcements, 1,
            "a reinforced Derives edge is the double-apply this design excludes: {edge:?}"
        );
    }
}

/// **F5 (J3 round 1)**: a store whose schema predates this change refuses the
/// attach instead of acking into a void.
///
/// `init_schema` runs only from `lambo provision`, never on the attach path, so
/// "provisioned by the previous build, binary upgraded, never re-provisioned" is
/// reachable through the product's own path. Before the preflight this test
/// pins, that store **attached, acked four derives (two even settling
/// `applied`), reported `degraded=false dead_lettered=0` all session, and left
/// `concepts=0 embedded=0`** — total durability loss, because a write's
/// mutations and its `PutWriteIntent` share one flush transaction, so the one
/// missing table rolled every batch back whole and the only signal was the
/// failed final flush at close.
///
/// Dropping `write_intents` after `init_schema` is byte-equivalent to a pre-J3
/// store: the table is this branch's ONLY schema change
/// (`git diff 867b650..HEAD -- migrations/` is one `CREATE TABLE` per adapter).
#[test]
fn an_unmigrated_store_refuses_the_attach_naming_lambo_provision() {
    let rig = Rig::new("unmigrated", "j3-unmigrated");
    rig.seed(Vec::new());
    let db = rig.db.clone();
    rig.rt.block_on(async move {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{db}"))
            .await
            .expect("open to un-migrate");
        sqlx::query("DROP TABLE write_intents")
            .execute(&pool)
            .await
            .expect("drop write_intents");
    });

    let out = Command::new(env!("CARGO_BIN_EXE_lambo"))
        .args([
            "--config",
            rig.cfg.to_str().unwrap(),
            "serve",
            "--session",
            "j3-unmigrated",
            "--agent",
            "agent-a",
            "--transport",
            "stdio",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .expect("spawn lambo serve");

    assert!(
        !out.status.success(),
        "serve must refuse an un-migrated store, not attach to it: {:?}",
        out.status
    );
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(
        err.contains("write_intents"),
        "the refusal must name the missing table: {err}"
    );
    assert!(
        err.contains("lambo provision"),
        "the refusal must name the fix: {err}"
    );
    // And it must refuse BEFORE taking the lease — otherwise a refused attach
    // locks the session out for the whole TTL.
    let db = rig.db.clone();
    let leases: i64 = rig.rt.block_on(async move {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{db}"))
            .await
            .expect("open to count leases");
        sqlx::query_scalar("SELECT count(*) FROM session_leases WHERE session_id = ?")
            .bind("j3-unmigrated")
            .fetch_one(&pool)
            .await
            .expect("count leases")
    });
    assert_eq!(leases, 0, "a refused attach must leave no lease row");
}

/// Placeholder swapped for the real seeded interaction id by
/// [`fix_interactions`] — the id is minted inside [`Rig::seed`], after the
/// intent list is built.
fn interaction_placeholder() -> NodeId {
    NodeId::new()
}

/// Point every seeded intent at the real interaction row.
fn fix_interactions(rig: &Rig, interaction: NodeId) {
    let db = rig.db.clone();
    rig.rt.block_on(async move {
        let pool = sqlx::sqlite::SqlitePoolOptions::new()
            .max_connections(1)
            .connect(&format!("sqlite://{db}"))
            .await
            .expect("open for fixup");
        sqlx::query("UPDATE write_intents SET interaction_id = ?")
            .bind(interaction.0.to_string())
            .execute(&pool)
            .await
            .expect("point intents at the seeded interaction");
    });
}
