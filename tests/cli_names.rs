//! #146: a populated SQLite store upgrades through the shipped operator CLI.
#![cfg(all(
    feature = "store-sqlite",
    feature = "fixtures",
    feature = "embed-fixture"
))]

use std::path::Path;
use std::process::Output;

use lambo::graph::Graph;
use lambo::store::lease::{LeaseHolder, LEASE_TTL};
use lambo::store::{GraphStore, SqliteStore};
use lambo::types::{AgentId, EdgeType, GraphSnapshot, NodeId, SessionId, Synonym};

mod common;
use common::{RuntimeDir, ScratchDir};

const SESSION: &str = "session-fair-test";

fn session() -> SessionId {
    SessionId::from(SESSION)
}

fn fixture() -> GraphSnapshot {
    serde_json::from_str(include_str!("../fixtures/session-fair-test.json")).unwrap()
}

fn id_of(content_prefix: &str) -> NodeId {
    fixture()
        .concepts
        .into_iter()
        .find(|c| c.content.starts_with(content_prefix))
        .unwrap()
        .id
}

fn invoke(config: &Path, runtime: &RuntimeDir, args: &[&str]) -> Output {
    let mut command = common::lambo_command();
    runtime.isolate(&mut command);
    command
        .args(["--config", config.to_str().unwrap()])
        .args(args);
    command.output().expect("run names command")
}

fn report(output: Output) -> serde_json::Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("one JSON report")
}

fn config(dir: &ScratchDir) -> (std::path::PathBuf, std::path::PathBuf) {
    let database = dir.join("memory.sqlite");
    let config = dir.join("lambo.toml");
    std::fs::write(
        &config,
        format!(
        "[store]\nkind = \"sqlite\"\npath = \"{}\"\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
        database.display()
    ),
    )
    .unwrap();
    (database, config)
}

async fn load(path: &Path) -> GraphSnapshot {
    SqliteStore::connect(path.to_str().unwrap())
        .unwrap()
        .load_session(&session())
        .await
        .unwrap()
}

#[test]
fn populated_memory_migration_previews_applies_restarts_and_repeats_without_changes() {
    let dir = ScratchDir::new("lambo-cli-names");
    let runtime = RuntimeDir::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (database, config) = config(&dir);
    let alias = id_of("writer lease");
    let target = id_of("a writer keeps its lease");
    let mut snapshot = fixture();
    let target_key = snapshot
        .concepts
        .iter()
        .find(|c| c.id == target)
        .unwrap()
        .canonical_key
        .clone();
    snapshot.synonyms.push(Synonym {
        session_id: session(),
        source_key: "LeaseShortcut".into(),
        canonical_key: target_key,
    });
    rt.block_on(async {
        let store = SqliteStore::connect(database.to_str().unwrap()).unwrap();
        store.init_schema().await.unwrap();
        store.seed(&snapshot).await.unwrap();
    });
    let original = rt.block_on(load(&database));
    let map = dir.join("reviewed.map");
    std::fs::write(&map, format!("# reviewed ids\n{alias} {target}\n")).unwrap();
    let base = [
        "names",
        "migrate",
        "--session",
        SESSION,
        "--map",
        map.to_str().unwrap(),
    ];
    let preview = report(invoke(&config, &runtime, &base));
    assert_eq!(preview["applied"], false);
    assert_eq!(preview["changes"]["aliases_retired"], 1);
    assert_eq!(preview["bindings"].as_array().unwrap().len(), 2);
    assert_eq!(preview["retirements"][0]["alias"], alias.to_string());
    assert_eq!(preview["retirements"][0]["target"], target.to_string());
    assert_eq!(rt.block_on(load(&database)), original);

    let epoch = preview["source_epoch"].as_u64().unwrap().to_string();
    let mut apply = base.to_vec();
    apply.extend(["--apply", "--expect-epoch", &epoch]);
    let applied = report(invoke(&config, &runtime, &apply));
    assert_eq!(applied["changes"], preview["changes"]);
    assert_eq!(applied["bindings"], preview["bindings"]);
    assert_eq!(applied["retirements"], preview["retirements"]);
    let after = rt.block_on(load(&database));
    let restarted = Graph::from_snapshot(after.clone()).unwrap();
    assert_eq!(restarted.name_target("LEASE SHORTCUT"), Some(target));
    assert_eq!(restarted.name_target("writer lease"), Some(target));
    assert!(restarted.node(alias).is_none());
    let archive = restarted.retired_alias(alias).unwrap();
    assert_eq!(
        archive.concept,
        *original.concepts.iter().find(|c| c.id == alias).unwrap()
    );
    let inspected = invoke(
        &config,
        &runtime,
        &[
            "inspect",
            "--session",
            SESSION,
            "--focus",
            &alias.to_string(),
        ],
    );
    assert!(
        inspected.status.success(),
        "{}",
        String::from_utf8_lossy(&inspected.stderr)
    );
    let historical = String::from_utf8_lossy(&inspected.stdout);
    assert!(
        historical.contains(&format!("retired alias {alias}: writer lease")),
        "{historical}"
    );
    assert!(
        historical.contains(&format!("replacement: {target}")),
        "{historical}"
    );
    assert!(
        historical.contains("archived incident edges:"),
        "{historical}"
    );
    assert!(
        !historical.contains("next hop"),
        "retired history is not active canonization: {historical}"
    );
    for old in original
        .edges
        .iter()
        .filter(|e| e.edge_type == EdgeType::Dependency && e.target == alias)
    {
        let moved = after
            .edges
            .iter()
            .find(|e| {
                e.source == old.source && e.target == target && e.edge_type == EdgeType::Dependency
            })
            .unwrap();
        assert_eq!(
            (moved.created_at, moved.event_time, moved.last_reinforced),
            (old.created_at, old.event_time, old.last_reinforced)
        );
    }

    let stale = invoke(&config, &runtime, &apply);
    assert_eq!(stale.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&stale.stderr).contains("changed since preview"));
    assert_eq!(rt.block_on(load(&database)), after);

    let next_epoch = after.mutation_epoch.to_string();
    let mut repeat = base.to_vec();
    repeat.extend(["--apply", "--expect-epoch", &next_epoch]);
    let repeated = report(invoke(&config, &runtime, &repeat));
    assert_eq!(repeated["mutations"], 0);
    assert_eq!(repeated["bindings"], serde_json::json!([]));
    assert_eq!(rt.block_on(load(&database)), after);
}

#[test]
fn migration_refuses_a_writer_and_invalid_map_before_changing_a_store() {
    let dir = ScratchDir::new("lambo-cli-names-lease");
    let runtime = RuntimeDir::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (database, config) = config(&dir);
    let snapshot = fixture();
    let holder = LeaseHolder::for_this_process(&AgentId::new("names-test-existing-writer"));
    rt.block_on(async {
        let store = SqliteStore::connect(database.to_str().unwrap()).unwrap();
        store.init_schema().await.unwrap();
        store.seed(&snapshot).await.unwrap();
        store
            .acquire_lease(&session(), &holder, LEASE_TTL)
            .await
            .unwrap();
    });
    let original = rt.block_on(load(&database));
    let epoch = original.mutation_epoch.to_string();
    let blocked = invoke(
        &config,
        &runtime,
        &[
            "names",
            "migrate",
            "--session",
            SESSION,
            "--apply",
            "--expect-epoch",
            &epoch,
        ],
    );
    assert_eq!(blocked.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&blocked.stderr).contains("another writer"));
    assert_eq!(rt.block_on(load(&database)), original);

    let malformed = dir.join("bad.map");
    std::fs::write(&malformed, "short-name full-ruling-content\n").unwrap();
    let refused = invoke(
        &config,
        &runtime,
        &[
            "names",
            "migrate",
            "--session",
            SESSION,
            "--map",
            malformed.to_str().unwrap(),
            "--apply",
            "--expect-epoch",
            &epoch,
        ],
    );
    assert_eq!(refused.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("two full hyphenated UUIDs"));
    assert_eq!(rt.block_on(load(&database)), original);
}

#[test]
fn an_existing_schema_gets_an_actionable_provision_upgrade_without_rebuilding_memory() {
    let dir = ScratchDir::new("lambo-cli-names-schema");
    let runtime = RuntimeDir::new();
    let rt = tokio::runtime::Runtime::new().unwrap();
    let (database, config) = config(&dir);
    rt.block_on(async {
        let store = SqliteStore::connect(database.to_str().unwrap()).unwrap();
        store.init_schema().await.unwrap();
        store.seed(&fixture()).await.unwrap();
    });
    let original = rt.block_on(load(&database));
    // Reproduce the additive part of the pre-name schema on this private file.
    rt.block_on(async {
        let options = database
            .to_str()
            .unwrap()
            .parse::<sqlx::sqlite::SqliteConnectOptions>()
            .unwrap();
        let pool = sqlx::SqlitePool::connect_with(options).await.unwrap();
        sqlx::query("DROP TABLE concept_names")
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("DROP TABLE retired_aliases")
            .execute(&pool)
            .await
            .unwrap();
        pool.close().await;
    });
    let refused = invoke(
        &config,
        &runtime,
        &["names", "migrate", "--session", SESSION],
    );
    assert_eq!(refused.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&refused.stderr).contains("lambo provision"));
    let provisioned = invoke(&config, &runtime, &["provision"]);
    assert!(
        provisioned.status.success(),
        "{}",
        String::from_utf8_lossy(&provisioned.stderr)
    );
    let preview = report(invoke(
        &config,
        &runtime,
        &["names", "migrate", "--session", SESSION],
    ));
    assert_eq!(preview["mutations"], 0);
    assert_eq!(rt.block_on(load(&database)), original);
}
