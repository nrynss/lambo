//! #173: measure phase-1 leg fusion (`[fusion] kind = "max"` against
//! reciprocal rank fusion) on the `fixtures/eval/` set, through the whole
//! recall pipeline.
//!
//! Unlike `eval_measure` (bare cosines over the corpus), this runs what a
//! session runs: the corpus is derived into a SQLite session one concept per
//! derive (so each is self-framed, as a one-concept `lambo_derive` stores
//! it), and every query goes through `Memory::recall`: keyword, recent and
//! vector legs, the fusion under test, traversal, and the daemon blend.
//!
//! The session is derived once. For each fusion variant a copy of the
//! database is reopened with that variant in its `Config`, the daemon's
//! warm-up cycle is awaited (so no recall runs in cold mode), and every
//! query is recalled. Embeddings come from an `eval_measure` cache when one
//! is given (`--cache`, same key), so variants cost no model time.
//!
//! Only the `dev` split is reported unless `--held-out` is passed: variants
//! are compared and chosen on `dev`, and `held_out` only confirms.
//!
//! ```text
//! LAMBO_EMBEDDER=candle LAMBO_EMBED_DEVICE=cpu \
//!   cargo run --release --features embed-candle,store-sqlite --example eval_fusion -- \
//!   --cache /path/to/bge-m3.jsonl --variant max --variant rrf --variant rrf:k=20 \
//!   --out evidence/issue-173-fusion/runs/bge-m3-dev.json
//! ```
//!
//! A variant is `max` or `rrf`, optionally followed by `:` and
//! comma-separated `k=`, `keyword=`, `vector=`, `recent=` (the RRF weights)
//! and `wd=` (the daemon's share of the final score; `w_query = 1 − wd`).
//! A trailing `@label` names a repeat of the same variant (a noise check).
//!
//! With `--out X.json`, per-query results (top three concept ids and the top
//! score, per variant) go to `X.queries.jsonl` beside it. Neither names a
//! path, URL or concept text: query ids, families and numbers only.

#[path = "../eval_measure/schema.rs"]
#[allow(dead_code)]
mod schema;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use lambo::embed::{EmbedError, EmbedderConfig};
use lambo::graph::derive::ParentOf;
use lambo::memory::Memory;
use lambo::recall::fusion::{FusionKind, RecallFusion};
use lambo::{ConceptType, Embedder, LamboFile, RecallQuery, RecallWeights};
use serde_json::{json, Value};

const SESSION: &str = "eval-fusion";
const TOP_K: usize = 10;
const ALPHA: f64 = 0.10;

struct Args {
    cache: Option<PathBuf>,
    out: Option<PathBuf>,
    variants: Vec<String>,
    held_out: bool,
    allow_fixture: bool,
}

fn parse_args() -> Result<Args, String> {
    let mut a = Args {
        cache: None,
        out: None,
        variants: Vec::new(),
        held_out: false,
        allow_fixture: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--cache" => a.cache = Some(it.next().ok_or("--cache needs a path")?.into()),
            "--out" => a.out = Some(it.next().ok_or("--out needs a path")?.into()),
            "--variant" => a.variants.push(it.next().ok_or("--variant needs a value")?),
            "--held-out" => a.held_out = true,
            "--allow-fixture" => a.allow_fixture = true,
            other => return Err(format!("unknown argument {other}")),
        }
    }
    if a.variants.is_empty() {
        a.variants = vec!["max".into(), "rrf".into()];
    }
    Ok(a)
}

/// A variant: the fusion and the recall weights it runs with.
fn parse_variant(spec: &str) -> Result<(RecallFusion, RecallWeights), String> {
    // A trailing `@label` only names a repeat of the same variant.
    let spec = spec.split_once('@').map_or(spec, |(s, _)| s);
    let (kind, rest) = spec.split_once(':').unwrap_or((spec, ""));
    let mut fusion = RecallFusion {
        kind: match kind {
            "max" => FusionKind::Max,
            "rrf" => FusionKind::Rrf,
            _ => return Err(format!("variant {spec}: kind is max or rrf")),
        },
        ..RecallFusion::default()
    };
    let mut weights = RecallWeights::default();
    for kv in rest.split(',').filter(|s| !s.is_empty()) {
        let (k, v) = kv
            .split_once('=')
            .ok_or_else(|| format!("variant {spec}: {kv} is not key=value"))?;
        let v: f64 = v
            .parse()
            .map_err(|_| format!("variant {spec}: {v} is not a number"))?;
        match k {
            "k" => fusion.k = v,
            "keyword" => fusion.keyword_weight = v,
            "vector" => fusion.vector_weight = v,
            "recent" => fusion.recent_weight = v,
            "wd" => {
                weights.w_daemon = v;
                weights.w_query = 1.0 - v;
            }
            _ => return Err(format!("variant {spec}: unknown key {k}")),
        }
    }
    fusion.validate().map_err(|e| e.to_string())?;
    Ok((fusion, weights))
}

/// The `eval_measure` cache, read-only plus appends for misses, wrapped
/// around the real embedder.
struct CachedEmbedder {
    inner: Arc<dyn Embedder>,
    contract: String,
    stored: HashMap<String, Vec<f32>>,
    file: Option<Mutex<std::fs::File>>,
    hits: AtomicUsize,
    misses: AtomicUsize,
}

impl CachedEmbedder {
    fn key(&self, role: &str, text: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for part in [self.contract.as_str(), role, text] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn lookup(&self, role: &str, text: &str) -> Option<Vec<f32>> {
        let v = self.stored.get(&self.key(role, text)).cloned();
        if v.is_some() {
            self.hits.fetch_add(1, Ordering::Relaxed);
        } else {
            self.misses.fetch_add(1, Ordering::Relaxed);
        }
        v
    }

    fn keep(&self, role: &str, text: &str, v: &[f32]) {
        use std::io::Write;
        if let Some(f) = &self.file {
            let line = json!({"key": self.key(role, text), "v": v}).to_string();
            let _ = writeln!(f.lock().expect("cache file"), "{line}");
        }
    }
}

#[async_trait]
impl Embedder for CachedEmbedder {
    fn dimensions(&self) -> usize {
        self.inner.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if let Some(v) = self.lookup("doc", text) {
            return Ok(v);
        }
        let v = self.inner.embed(text).await?;
        self.keep("doc", text, &v);
        Ok(v)
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        if let Some(v) = self.lookup("query", text) {
            return Ok(v);
        }
        let v = self.inner.embed_query(text).await?;
        self.keep("query", text, &v);
        Ok(v)
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.inner.as_any()
    }
}

fn concept_type(kind: &str) -> Result<ConceptType, String> {
    Ok(match kind {
        "Entity" => ConceptType::Entity,
        "Logic" => ConceptType::Logic,
        "Constraint" => ConceptType::Constraint,
        "Resource" => ConceptType::Resource,
        "Observation" => ConceptType::Observation,
        other => return Err(format!("unknown concept type {other}")),
    })
}

fn r4(x: f64) -> f64 {
    (x * 10_000.0).round() / 10_000.0
}

fn fraction(num: usize, den: usize) -> Value {
    if den == 0 {
        Value::Null
    } else {
        json!(r4(num as f64 / den as f64))
    }
}

#[derive(Default)]
struct Acc {
    n: usize,
    hit1: usize,
    hit5: usize,
    hit10: usize,
    rr: f64,
    ndcg: f64,
}

async fn open(
    db: &std::path::Path,
    embedder: Arc<dyn Embedder>,
    variant: Option<(RecallFusion, RecallWeights)>,
) -> Result<Memory, String> {
    let toml = format!(
        "[store]\nkind = \"sqlite\"\npath = {}\n",
        toml_string(&db.display().to_string())
    );
    let mut file = LamboFile::from_toml_str(&toml).map_err(|e| e.to_string())?;
    file.embedder = file.embedder.overlay_env().map_err(|e| e.to_string())?;
    let mut resolved = lambo::resolve::resolve_backends(file).map_err(|e| e.to_string())?;
    // `lambo provision` for SQLite: idempotent, so a reopened copy is fine.
    resolved
        .store
        .init_schema()
        .await
        .map_err(|e| format!("init_schema: {e}"))?;
    resolved.embedder = Box::new(Shared(embedder));
    // The resolved process config, as a serve would run it, with only the
    // variant's fusion and weights changed. An hour's tick: the warm-up cycle
    // scores the reopened session, and no later cycle runs mid-measurement.
    let mut config = resolved.config.clone();
    if let Some((fusion, weights)) = variant {
        config.recall_fusion = fusion;
        config.recall_weights = weights;
        config.daemon_tick_interval = Duration::from_secs(3600);
    }
    Memory::builder()
        .session(SESSION)
        .agent("eval")
        .backends(resolved)
        .config(config)
        .build()
        .await
        .map_err(|e| e.to_string())
}

fn toml_string(s: &str) -> String {
    toml::Value::String(s.into()).to_string()
}

/// An `Arc<dyn Embedder>` as a `Box<dyn Embedder>`.
struct Shared(Arc<dyn Embedder>);

#[async_trait]
impl Embedder for Shared {
    fn dimensions(&self) -> usize {
        self.0.dimensions()
    }
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.0.embed(text).await
    }
    async fn embed_query(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        self.0.embed_query(text).await
    }
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        self.0.as_any()
    }
}

#[tokio::main]
async fn main() {
    if let Err(e) = run().await {
        eprintln!("eval_fusion: {e}");
        std::process::exit(1);
    }
}

async fn run() -> Result<(), String> {
    let args = parse_args()?;
    let variants: Vec<(String, RecallFusion, RecallWeights)> = args
        .variants
        .iter()
        .map(|s| parse_variant(s).map(|(f, w)| (s.clone(), f, w)))
        .collect::<Result<_, _>>()?;
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/eval");
    let set = schema::load(&dir)?;
    let problems = schema::validate(&set, lambo::surface::limits::MAX_CONTENT_BYTES);
    if !problems.is_empty() {
        return Err(format!("the eval set fails its schema: {problems:?}"));
    }

    let cfg = EmbedderConfig::from_env().map_err(|e| e.to_string())?;
    let kind = format!("{:?}", cfg.kind);
    if kind.to_ascii_lowercase().contains("fixture") && !args.allow_fixture {
        return Err(
            "LAMBO_EMBEDDER is the fixture embedder; pass --allow-fixture for a smoke run".into(),
        );
    }
    let inner: Arc<dyn Embedder> =
        Arc::from(lambo::embed::build_embedder(cfg.clone()).map_err(|e| e.to_string())?);
    let identity = lambo::embed::candle_identity(inner.as_ref());
    // `eval_measure`'s cache key, field for field, so its cache is reused.
    let model = json!({
        "dim": cfg.dim,
        "llama_model": cfg.llama_model,
        "device": cfg.device,
        "repo": cfg.repo,
        "revision": cfg.revision,
        "weights_file": cfg.weights_file,
        "gemini_model": cfg.gemini_model,
    });
    let mut cached = CachedEmbedder {
        inner,
        contract: format!(
            "{kind}|{model}|{:?}|{:?}|{:?}|{:?}|{identity:?}",
            cfg.llama_url, cfg.weights_dir, cfg.gemini_project, cfg.gemini_location
        ),
        stored: HashMap::new(),
        file: None,
        hits: AtomicUsize::new(0),
        misses: AtomicUsize::new(0),
    };
    if let Some(path) = &args.cache {
        use std::io::BufRead;
        if let Ok(f) = std::fs::File::open(path) {
            for line in std::io::BufReader::new(f).lines().map_while(Result::ok) {
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && let (Some(k), Some(vs)) = (v["key"].as_str(), v["v"].as_array())
                {
                    let vec: Vec<f32> = vs
                        .iter()
                        .filter_map(|x| x.as_f64())
                        .map(|x| x as f32)
                        .collect();
                    cached.stored.insert(k.to_string(), vec);
                }
            }
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("opening the cache: {e}"))?;
        cached.file = Some(Mutex::new(f));
    }
    let embedder: Arc<CachedEmbedder> = Arc::new(cached);
    let as_dyn: Arc<dyn Embedder> = embedder.clone();

    // 1. Derive the corpus once, one concept per derive, in file order.
    let work = tempdir()?;
    let base_db = work.join("base.db");
    let started = Instant::now();
    let mut node_of: HashMap<lambo::NodeId, String> = HashMap::new();
    {
        let mem = open(&base_db, as_dyn.clone(), None).await?;
        for c in &set.corpus {
            let out = mem
                .derive(
                    &[(c.text.as_str(), concept_type(&c.kind)?)],
                    &ParentOf::none(),
                )
                .await
                .map_err(|e| format!("deriving {}: {e}", c.id))?;
            let id = out
                .created
                .first()
                .or(out.matched.first())
                .ok_or_else(|| format!("deriving {} produced no concept", c.id))?;
            if node_of.insert(*id, c.id.clone()).is_some() {
                return Err(format!("{} resolved onto another corpus concept", c.id));
            }
            // Distinct `created_at`s, so the recent leg's three newest
            // interactions (the corpus's last three) are the same every run
            // rather than a tie broken by random interaction ids.
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        mem.close().await.map_err(|e| e.to_string())?;
    }
    eprintln!(
        "derived {} concepts in {:.1}s",
        set.corpus.len(),
        started.elapsed().as_secs_f64()
    );

    let queries: Vec<&schema::Query> = set
        .queries
        .iter()
        .filter(|q| q.review.is_none())
        .filter(|q| q.split == "dev" || args.held_out)
        .collect();

    let mut report = BTreeMap::new();
    // Per-query results, one JSON line each, beside the report.
    let mut query_lines: Vec<String> = Vec::new();
    for (i, (name, fusion, weights)) in variants.iter().enumerate() {
        let db = work.join(format!("v{i}.db"));
        for suffix in ["", "-wal", "-shm"] {
            let from = PathBuf::from(format!("{}{suffix}", base_db.display()));
            if from.exists() {
                std::fs::copy(&from, format!("{}{suffix}", db.display()))
                    .map_err(|e| format!("copying the session: {e}"))?;
            }
        }
        let mem = open(&db, as_dyn.clone(), Some((*fusion, *weights))).await?;
        let waited = Instant::now();
        while mem.stats().daemon_cycles == 0 {
            if waited.elapsed() > Duration::from_secs(60) {
                return Err("the daemon's warm-up cycle never ran".into());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let depth = mem.config().default_traversal_depth;
        let mut acc: BTreeMap<String, Acc> = BTreeMap::new();
        let mut top1: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut per_query = Vec::new();
        for q in &queries {
            let result = mem
                .recall(RecallQuery {
                    query: q.query.clone(),
                    top_k: TOP_K,
                    max_tokens: 100_000,
                    traversal_depth: depth,
                })
                .await
                .map_err(|e| format!("recalling {}: {e}", q.id))?;
            let ranked: Vec<&str> = result
                .hits
                .iter()
                .filter_map(|h| node_of.get(&h.node_id).map(String::as_str))
                .collect();
            let first_score = result.hits.first().map_or(0.0, |h| h.score);
            top1.entry(format!("{}/{}", q.split, q.kind))
                .or_default()
                .push(first_score);
            per_query.push(json!({
                "id": q.id, "split": q.split, "kind": q.kind, "family": q.family,
                "top": ranked.iter().take(3).collect::<Vec<_>>(),
                "top1_score": r4(first_score),
            }));
            if q.kind == "no_answer" {
                continue;
            }
            let grade: HashMap<&str, u8> = q
                .relevant
                .iter()
                .map(|r| (r.concept.as_str(), r.grade))
                .collect();
            let first = ranked.iter().position(|id| grade.get(id) == Some(&2));
            let dcg: f64 = ranked
                .iter()
                .take(10)
                .enumerate()
                .map(|(i, id)| {
                    let g = f64::from(*grade.get(id).unwrap_or(&0));
                    (2f64.powf(g) - 1.0) / ((i + 2) as f64).log2()
                })
                .sum();
            let mut ideal: Vec<u8> = q.relevant.iter().map(|r| r.grade).collect();
            ideal.sort_unstable_by(|a, b| b.cmp(a));
            let idcg: f64 = ideal
                .iter()
                .take(10)
                .enumerate()
                .map(|(i, g)| (2f64.powf(f64::from(*g)) - 1.0) / ((i + 2) as f64).log2())
                .sum();
            for key in [
                format!("split/{}", q.split),
                format!("{}/family/{}", q.split, q.family),
            ] {
                let a = acc.entry(key).or_default();
                a.n += 1;
                if let Some(r) = first {
                    a.hit1 += usize::from(r < 1);
                    a.hit5 += usize::from(r < 5);
                    a.hit10 += usize::from(r < 10);
                    a.rr += 1.0 / (r + 1) as f64;
                }
                a.ndcg += if idcg > 0.0 { dcg / idcg } else { 0.0 };
            }
        }
        mem.close().await.map_err(|e| e.to_string())?;
        let metrics: BTreeMap<String, Value> = acc
            .into_iter()
            .map(|(k, a)| {
                (
                    k,
                    json!({
                        "n": a.n,
                        "hit@1": fraction(a.hit1, a.n),
                        "hit@5": fraction(a.hit5, a.n),
                        "hit@10": fraction(a.hit10, a.n),
                        "mrr@10": r4(a.rr / a.n as f64),
                        "ndcg@10": r4(a.ndcg / a.n as f64),
                    }),
                )
            })
            .collect();
        let no_answer: BTreeMap<String, Value> = ["dev", "held_out"]
            .iter()
            .filter_map(|split| {
                let ans = top1.get(&format!("{split}/answerable"))?;
                let none = top1.get(&format!("{split}/no_answer"))?;
                Some((split.to_string(), separation(ans, none, &top1)))
            })
            .collect();
        eprintln!("{name}: {}", json!(metrics.get("split/dev")));
        for q in per_query {
            let mut q = q;
            q["variant"] = json!(name);
            query_lines.push(q.to_string());
        }
        report.insert(
            name.clone(),
            json!({
                "fusion": {
                    "kind": fusion.kind.to_string(), "k": fusion.k,
                    "keyword_weight": fusion.keyword_weight,
                    "vector_weight": fusion.vector_weight,
                    "recent_weight": fusion.recent_weight,
                },
                "recall_weights": {"w_daemon": weights.w_daemon, "w_query": weights.w_query},
                "metrics": metrics,
                "no_answer": no_answer,
            }),
        );
    }
    let _ = std::fs::remove_dir_all(&work);
    let out = json!({
        "issue": 173,
        "embedder_kind": kind,
        "model": model,
        "top_k": TOP_K,
        "splits": if args.held_out { json!(["dev", "held_out"]) } else { json!(["dev"]) },
        "cache": {"hits": embedder.hits.load(Ordering::Relaxed), "misses": embedder.misses.load(Ordering::Relaxed)},
        "variants": report,
    });
    let text = serde_json::to_string_pretty(&out).expect("json");
    match &args.out {
        Some(path) => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
            }
            std::fs::write(path, text + "\n").map_err(|e| e.to_string())?;
            let lines = path.with_extension("queries.jsonl");
            std::fs::write(&lines, query_lines.join("\n") + "\n").map_err(|e| e.to_string())?;
        }
        None => println!("{text}"),
    }
    Ok(())
}

/// Top-1 final-score separation between answerable and no-answer queries:
/// the probability an answerable query's top-1 outscores a no-answer one's
/// (ties half), medians, and a split-conformal gate at α tuned on `dev`'s
/// answerable top-1 scores, with the share of each kind it passes.
fn separation(ans: &[f64], none: &[f64], all: &BTreeMap<String, Vec<f64>>) -> Value {
    let mut wins = 0.0;
    for a in ans {
        for n in none {
            wins += if a > n {
                1.0
            } else if a == n {
                0.5
            } else {
                0.0
            };
        }
    }
    let auc = wins / (ans.len() * none.len()).max(1) as f64;
    let median = |v: &[f64]| {
        let mut v = v.to_vec();
        v.sort_by(f64::total_cmp);
        v.get(v.len() / 2).copied().unwrap_or(0.0)
    };
    let mut dev = all.get("dev/answerable").cloned().unwrap_or_default();
    dev.sort_by(f64::total_cmp);
    let tau = if dev.is_empty() {
        None
    } else {
        let k = ((ALPHA * (dev.len() + 1) as f64).floor() as usize).clamp(1, dev.len());
        Some(dev[k - 1])
    };
    let pass = |v: &[f64]| tau.map(|t| fraction(v.iter().filter(|x| **x >= t).count(), v.len()));
    json!({
        "auc_top1": r4(auc),
        "median_top1_answerable": r4(median(ans)),
        "median_top1_no_answer": r4(median(none)),
        "distinct_top1_values": {
            "answerable": distinct(ans), "no_answer": distinct(none),
        },
        "gate_alpha": ALPHA,
        "gate_tau_from_dev": tau.map(r4),
        "gate_pass_answerable": pass(ans),
        "gate_pass_no_answer": pass(none),
    })
}

fn distinct(v: &[f64]) -> usize {
    let mut v: Vec<u64> = v.iter().map(|x| (r4(*x) * 10_000.0) as u64).collect();
    v.sort_unstable();
    v.dedup();
    v.len()
}

fn tempdir() -> Result<PathBuf, String> {
    let dir = std::env::temp_dir().join(format!("lambo-eval-fusion-{}", std::process::id()));
    std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
    Ok(dir)
}
