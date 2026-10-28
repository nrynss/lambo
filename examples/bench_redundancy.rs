//! #181: recall latency with `[selection] skip_redundant` off and on.
//!
//! Two in-process sessions (memory store, fixture embedder, 1024 dims) are
//! built from the same writes, one per setting, and every query is recalled
//! against both in turn, `ROUNDS` times, at the default `top_k = 5` (a
//! window of 20). Two stores: `distinct` (no two concepts redundant, so the
//! rule checks and skips nothing: its pure overhead) and `duplicated` (one
//! concept in four also stored as an Observation with the same text, so
//! most windows skip). Prints p50 / p90 / mean per store and setting.
//!
//! ```text
//! cargo run --release --example bench_redundancy
//! ```

use std::time::{Duration, Instant};

use lambo::graph::derive::ParentOf;
use lambo::memory::Memory;
use lambo::recall::redundancy::RecallSelection;
use lambo::{ConceptType, LamboFile, RecallQuery};

const CONCEPTS: usize = 2_000;
const QUERIES: usize = 40;
const ROUNDS: usize = 25;

const WORDS: [&str; 16] = [
    "lease", "flush", "recall", "embedder", "daemon", "canon", "ledger", "window", "budget",
    "vector", "keyword", "session", "layer", "cursor", "schema", "index",
];

const OWNERS: [&str; 16] = [
    "alpha", "bravo", "charlie", "delta", "echo", "foxtrot", "golf", "hotel", "india", "juliet",
    "kilo", "lima", "mike", "november", "oscar", "papa",
];
const TIMES: [&str; 8] = [
    "mornings",
    "evenings",
    "releases",
    "reviews",
    "audits",
    "migrations",
    "outages",
    "weekends",
];

/// A text with no digit (so the identifier guard never decides), one word
/// from each of three disjoint lists, so every `i` below 2048 has its own
/// canonical key.
fn text(i: usize) -> String {
    format!(
        "the {} team keeps the {} steady across {}",
        OWNERS[i % 16],
        WORDS[(i / 16) % 16],
        TIMES[(i / 256) % 8],
    )
}

async fn session(name: &str, skip_redundant: bool, duplicated: bool) -> Result<Memory, String> {
    let file = LamboFile::from_toml_str(
        "[store]\nkind = \"memory\"\n[embedder]\nkind = \"fixture\"\ndim = 1024\n",
    )
    .map_err(|e| e.to_string())?;
    let resolved = lambo::resolve::resolve_backends(file).map_err(|e| e.to_string())?;
    let mut config = resolved.config.clone();
    // Every signal on: the costlier configuration, so the timing bounds
    // the shipped default (identical content only) from above.
    config.recall_selection = RecallSelection {
        skip_redundant,
        similar_signals: skip_redundant,
    };
    config.daemon_tick_interval = Duration::from_secs(3600);
    let mem = Memory::builder()
        .session(name)
        .agent("bench")
        .backends(resolved)
        .config(config)
        .build()
        .await
        .map_err(|e| e.to_string())?;
    for i in 0..CONCEPTS {
        let t = text(i);
        // The Observation first: the canonical-key matcher skips it, so
        // the Logic concept after it is a second concept.
        if duplicated && i % 4 == 0 {
            mem.derive(&[(t.as_str(), ConceptType::Observation)], &ParentOf::none())
                .await
                .map_err(|e| e.to_string())?;
        }
        mem.derive(&[(t.as_str(), ConceptType::Logic)], &ParentOf::none())
            .await
            .map_err(|e| e.to_string())?;
    }
    let waited = Instant::now();
    while mem.stats().daemon_cycles == 0 {
        if waited.elapsed() > Duration::from_secs(60) {
            return Err("the daemon's warm-up cycle never ran".into());
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    Ok(mem)
}

fn summary(mut v: Vec<f64>) -> String {
    v.sort_by(f64::total_cmp);
    let at = |q: f64| v[((v.len() - 1) as f64 * q).round() as usize];
    let mean = v.iter().sum::<f64>() / v.len() as f64;
    format!(
        "p50 {:.3} ms, p90 {:.3} ms, mean {:.3} ms (n {})",
        at(0.5),
        at(0.9),
        mean,
        v.len()
    )
}

#[tokio::main]
async fn main() -> Result<(), String> {
    for duplicated in [false, true] {
        let store = if duplicated { "duplicated" } else { "distinct" };
        let off = session(&format!("bench-{store}-off"), false, duplicated).await?;
        let on = session(&format!("bench-{store}-on"), true, duplicated).await?;
        // Each query is one of the texts that the duplicated store holds
        // twice, so its two copies rank first there.
        let queries: Vec<String> = (0..QUERIES).map(|i| text(i * 48)).collect();
        let query = |q: &str| RecallQuery {
            query: q.to_string(),
            top_k: 5,
            max_tokens: 500,
            traversal_depth: 2,
        };
        if std::env::var_os("BENCH_DEBUG").is_some() {
            let r = on
                .recall(query(&queries[0]))
                .await
                .map_err(|e| e.to_string())?;
            eprintln!("{store}: stats {:?}", on.stats());
            for h in &r.hits {
                eprintln!("  {:?} {:.3} {}", h.concept_type, h.score, h.content);
            }
        }
        // Warm both (caches, allocator) before timing.
        for q in &queries {
            off.recall(query(q)).await.map_err(|e| e.to_string())?;
            on.recall(query(q)).await.map_err(|e| e.to_string())?;
        }
        let (mut t_off, mut t_on) = (Vec::new(), Vec::new());
        let mut skipped = 0usize;
        for round in 0..ROUNDS {
            for q in &queries {
                // Alternate which setting goes first, so neither always
                // runs on a cache the other just warmed.
                let order: [bool; 2] = if round % 2 == 0 {
                    [false, true]
                } else {
                    [true, false]
                };
                for skip in order {
                    let mem = if skip { &on } else { &off };
                    let started = Instant::now();
                    let r = mem.recall(query(q)).await.map_err(|e| e.to_string())?;
                    let ms = started.elapsed().as_secs_f64() * 1e3;
                    if skip {
                        t_on.push(ms);
                        if round == 0 && r.context.contains(", also ") {
                            skipped += 1;
                        }
                    } else {
                        t_off.push(ms);
                    }
                }
            }
        }
        println!("{store}: off {}", summary(t_off));
        println!("{store}: on  {}", summary(t_on));
        println!("{store}: queries whose context names a skip: {skipped}/{QUERIES}");
        off.close().await.map_err(|e| e.to_string())?;
        on.close().await.map_err(|e| e.to_string())?;
    }
    Ok(())
}
