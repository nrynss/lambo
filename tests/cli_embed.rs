#![cfg(feature = "embed-fixture")]

use std::fs;
use std::path::Path;
use std::process::Output;

use serde_json::{json, Value};

mod common;
use common::{lambo_command, ScratchDir};

fn invoke(config: &Path, arguments: &[&str]) -> Output {
    lambo_command()
        .arg("--config")
        .arg(config)
        .args(arguments)
        .output()
        .unwrap()
}

fn success(output: Output) -> Output {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

fn vector(output: &Output) -> Value {
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn embed_exports_query_document_and_batch_vectors_without_a_store() {
    let scratch = ScratchDir::new("lambo-cli-embed");
    let config = scratch.join("lambo.toml");
    // A normal backend resolution cannot build this SQLite store without a
    // path (or in a build without SQLite). Embed needs only the fixture.
    fs::write(
        &config,
        "[store]\nkind = \"sqlite\"\n[embedder]\nkind = \"fixture\"\ndim = 8\n",
    )
    .unwrap();

    let first = success(invoke(&config, &["embed", "--query", "café policy"]));
    let second = success(invoke(&config, &["embed", "--query", "renewal"]));
    let first_vector = vector(&first);
    assert_eq!(
        first_vector["contract"],
        json!({"kind": "fixture", "dim": 8})
    );
    assert_eq!(first_vector["values"].as_array().unwrap().len(), 8);
    assert_ne!(first_vector["values"], vector(&second)["values"]);

    // The fixture is symmetric; compare the document vector to its literal
    // framed input and also to the bare document.
    let bare = success(invoke(&config, &["embed", "--document", "café policy"]));
    assert_eq!(vector(&bare), first_vector);
    let framed = success(invoke(
        &config,
        &[
            "embed",
            "--document",
            "café policy",
            "--origin",
            " renewal ",
        ],
    ));
    let literal = success(invoke(
        &config,
        &["embed", "--document", "café policy — renewal"],
    ));
    assert_eq!(vector(&framed), vector(&literal));
    assert_ne!(vector(&framed), vector(&bare));

    let path = scratch.join("queries.txt");
    fs::write(&path, "café policy\r\nrenewal").unwrap();
    let args = ["embed", "--queries-file", path.to_str().unwrap()];
    let batch = success(invoke(&config, &args));
    assert_eq!(batch.stdout, [first.stdout.clone(), second.stdout].concat());

    fs::write(&path, "café policy\n\n").unwrap();
    let blank = invoke(&config, &args);
    assert_eq!(blank.status.code(), Some(2));
    assert_eq!(blank.stdout, first.stdout);
    assert!(String::from_utf8_lossy(&blank.stderr).contains("--queries-file line 2"));
}

#[cfg(feature = "store-sqlite")]
#[test]
fn exported_query_vectors_replay_text_and_dependency_recall() {
    let scratch = ScratchDir::new("lambo-cli-embed-replay");
    let config = scratch.join("lambo.toml");
    let database = scratch.join("store.sqlite");
    fs::write(
        &config,
        format!(
            "[store]\nkind = \"sqlite\"\npath = {:?}\n\
             [embedder]\nkind = \"fixture\"\ndim = 8\naccept_client_vectors = true\n",
            database.to_str().unwrap()
        ),
    )
    .unwrap();
    success(invoke(&config, &["provision"]));
    success(invoke(
        &config,
        &[
            "derive",
            "--session",
            "demo",
            "--agent",
            "test",
            "--content",
            "reader policy",
            "--kind",
            "entity",
        ],
    ));
    success(invoke(
        &config,
        &[
            "derive",
            "--session",
            "demo",
            "--agent",
            "test",
            "--content",
            "lease renewal rule",
            "--kind",
            "logic",
            "--depends-on",
            "reader policy",
        ],
    ));

    let query_scores = |stderr: &[u8]| -> String {
        std::str::from_utf8(stderr)
            .unwrap()
            .lines()
            .find(|line| line.starts_with("recall scores: "))
            .unwrap()
            .to_owned()
    };
    for query in ["reader policy", "what depends on reader policy"] {
        let ordinary = success(invoke(
            &config,
            &["recall", "--session", "demo", "--query", query],
        ));
        let exported = success(invoke(&config, &["embed", "--query", query]));
        let path = scratch.join("query.json");
        let path_arg = path.to_str().unwrap();
        fs::write(&path, &exported.stdout).unwrap();
        let replay = success(invoke(
            &config,
            &[
                "recall",
                "--session",
                "demo",
                "--query",
                query,
                "--query-vector-json",
                path_arg,
                "--query-vector-as-text",
            ],
        ));
        assert_eq!(ordinary.stdout, replay.stdout, "{query}");
        assert_eq!(
            query_scores(&ordinary.stderr),
            query_scores(&replay.stderr),
            "{query}"
        );

        if query.starts_with("what depends") {
            assert!(String::from_utf8_lossy(&ordinary.stdout).contains("lease renewal rule"));
            let vector_search = success(invoke(
                &config,
                &[
                    "recall",
                    "--session",
                    "demo",
                    "--query",
                    query,
                    "--query-vector-json",
                    path_arg,
                ],
            ));
            assert_ne!(ordinary.stdout, vector_search.stdout);
        }
    }
}

#[cfg(feature = "store-sqlite")]
#[test]
fn text_replay_preserves_near_unit_bits_and_rejects_scaled_vectors() {
    let scratch = ScratchDir::new("lambo-cli-embed-replay-drift");
    let config = scratch.join("lambo.toml");
    let database = scratch.join("store.sqlite");
    fs::write(&config, format!(
        "[store]\nkind = \"sqlite\"\npath = {:?}\n[embedder]\nkind = \"fixture\"\ndim = 4096\naccept_client_vectors = true\n",
        database.to_str().unwrap()
    )).unwrap();
    success(invoke(&config, &["provision"]));
    for content in ["probe concept", "probe other concept"] {
        success(invoke(
            &config,
            &[
                "derive",
                "--session",
                "demo",
                "--agent",
                "test",
                "--content",
                content,
                "--kind",
                "entity",
            ],
        ));
    }
    let query = "policy 487";
    let exported = success(invoke(&config, &["embed", "--query", query]));
    let mut vector = vector(&exported);
    let norm = vector["values"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_f64().unwrap().powi(2))
        .sum::<f64>()
        .sqrt();
    assert!(
        (norm - 1.0).abs() > 1e-6 && (norm - 1.0).abs() < 1e-5,
        "{norm}"
    );
    let path = scratch.join("query.json");
    fs::write(&path, &exported.stdout).unwrap();
    let path_arg = path.to_str().unwrap();
    let ordinary = success(invoke(
        &config,
        &["recall", "--session", "demo", "--query", query],
    ));
    let replay = success(invoke(
        &config,
        &[
            "recall",
            "--session",
            "demo",
            "--query",
            query,
            "--query-vector-json",
            path_arg,
            "--query-vector-as-text",
        ],
    ));
    assert_eq!(ordinary.stdout, replay.stdout);
    let scores = |o: &Output| {
        String::from_utf8_lossy(&o.stderr)
            .lines()
            .find(|l| l.starts_with("recall scores: "))
            .unwrap()
            .to_owned()
    };
    assert_eq!(scores(&ordinary), scores(&replay));

    // Match the BGE adapter's f32 accumulation and normalization. The small
    // squares round away from the sum, but remain visible in an f64 norm.
    let mut wide = vec![2_f32.powi(-13); 4096];
    wide[0] = 1.0;
    let f32_norm = wide.iter().map(|x| x * x).sum::<f32>().sqrt();
    for value in &mut wide {
        *value /= f32_norm;
    }
    let wide_norm = wide
        .iter()
        .map(|x| f64::from(*x).powi(2))
        .sum::<f64>()
        .sqrt();
    assert!(wide_norm - 1.0 > 1e-5, "{wide_norm}");
    let mut wide_vector = vector.clone();
    wide_vector["values"] = json!(wide);
    fs::write(&path, serde_json::to_vec(&wide_vector).unwrap()).unwrap();
    success(invoke(
        &config,
        &[
            "recall",
            "--session",
            "demo",
            "--query",
            query,
            "--query-vector-json",
            path_arg,
            "--query-vector-as-text",
        ],
    ));

    for value in vector["values"].as_array_mut().unwrap() {
        *value = json!(value.as_f64().unwrap() * 2.0);
    }
    fs::write(&path, serde_json::to_vec(&vector).unwrap()).unwrap();
    let rejected = invoke(
        &config,
        &[
            "recall",
            "--session",
            "demo",
            "--query",
            query,
            "--query-vector-json",
            path_arg,
            "--query-vector-as-text",
        ],
    );
    assert_eq!(rejected.status.code(), Some(2));
    assert!(String::from_utf8_lossy(&rejected.stderr).contains("requires a near-unit query vector"));
    success(invoke(
        &config,
        &[
            "recall",
            "--session",
            "demo",
            "--query",
            query,
            "--query-vector-json",
            path_arg,
        ],
    ));
}
