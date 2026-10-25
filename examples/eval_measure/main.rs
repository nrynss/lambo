//! #190: measure the evaluation set in `fixtures/eval/` under one embedding
//! contract.
//!
//! Prints, and optionally writes as JSON, for the configured embedder:
//!
//! * cosine quantiles (nearest rank, as the canary computes them) of the
//!   related-but-distinct, paraphrase and unrelated pairs, overall and per
//!   family, both **raw** (the texts as given, which is what
//!   `[embedder] related_pairs` embeds) and **self-framed** (`"{t} — {t}"`,
//!   what a one-concept `lambo_derive` stores);
//! * what `[embedder] related_pairs` would derive from the export (its p99),
//!   whether it clears the canary's random-pair floor, and how long the
//!   export took to embed against `CANARY_EMBED_TIMEOUT`;
//! * query-to-concept true-recall bands (bare query against each grade-2
//!   concept), retrieval over the corpus (hit@k, MRR, nDCG@10) per framing,
//!   split and query family;
//! * the top-1 cosine of answerable against no-answer queries, and a
//!   split-conformal top-1 gate tuned on `dev` at α = 0.10 and scored on
//!   `held_out` (#174's baseline).
//!
//! Items awaiting the owner's review (`review` set) are left out of every
//! number and counted separately.
//!
//! The embedder comes from the environment, exactly as `lambo` reads it:
//!
//! ```text
//! LAMBO_EMBEDDER=candle LAMBO_EMBED_DEVICE=cpu \
//!   cargo run --release --features embed-candle --example eval_measure -- \
//!   --out evidence/eval-set/runs/bge-m3-candle-cpu.json
//! LAMBO_EMBEDDER=bge_m3 LAMBO_LLAMA_EMBED_URL=http://127.0.0.1:8080 \
//!   cargo run --release --example eval_measure
//! ```
//!
//! The fixture embedder (`LAMBO_EMBEDDER=fixture`) gives meaningless
//! numbers, so it is refused unless `--allow-fixture` is passed (a smoke run
//! of the harness itself).

mod schema;

use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use lambo::embed::canary::{
    cosine64, pair_quantiles, CANARY_EMBED_TIMEOUT, CANARY_PHRASES, MIN_RELATED_PAIRS,
};
use lambo::embed::{build_embedder, EmbedderConfig};
use lambo::surface::limits::MAX_CONTENT_BYTES;
use lambo::Embedder;
use serde_json::{json, Value};

/// α for the split-conformal gate: answerable queries keep at least 1 − α
/// coverage on exchangeable data.
const ALPHA: f64 = 0.10;
const FRAMINGS: [&str; 2] = ["raw", "self"];

/// What a one-concept derive embeds: `context_text(content, Some(origin))`
/// in src/graph/hybrid.rs with the derive prompt as origin, which for a
/// single concept is the content itself.
fn frame(text: &str, framing: &str) -> String {
    match framing {
        "raw" => text.to_string(),
        _ => format!("{text} — {text}"),
    }
}

/// The candle identity names a local weights directory when one overrides
/// the hub artifact (`dir:<path> <file> sha256:<12 hex>`); a run file keeps
/// the file name and digest, which pin the weights, and drops the path.
fn public_identity(identity: &str) -> String {
    match identity.strip_prefix("dir:") {
        Some(rest) => {
            let tail: Vec<&str> = rest.rsplitn(3, ' ').collect();
            match tail.as_slice() {
                [sha, file, _dir] => format!("dir:<local> {file} {sha}"),
                _ => "dir:<local>".to_string(),
            }
        }
        None => identity.to_string(),
    }
}

fn quantile(sorted: &[f64], q: f64) -> f64 {
    let rank = ((q * sorted.len() as f64).ceil() as usize).clamp(1, sorted.len());
    sorted[rank - 1]
}

fn summary(mut v: Vec<f64>) -> Value {
    if v.is_empty() {
        return json!({ "n": 0 });
    }
    v.sort_by(f64::total_cmp);
    let r = |x: f64| (x * 10_000.0).round() / 10_000.0;
    json!({
        "n": v.len(),
        "min": r(v[0]),
        "p05": r(quantile(&v, 0.05)),
        "p50": r(quantile(&v, 0.50)),
        "p95": r(quantile(&v, 0.95)),
        "p99": r(quantile(&v, 0.99)),
        "max": r(*v.last().unwrap()),
    })
}

/// How a merge threshold at the related-but-distinct p99 (#87) and at the
/// default 0.85 would treat the paraphrases and the related-distinct pairs.
fn separation(rbd: &[f64], para: &[f64]) -> Value {
    if rbd.is_empty() {
        return Value::Null;
    }
    let mut sorted = rbd.to_vec();
    sorted.sort_by(f64::total_cmp);
    let p99 = quantile(&sorted, 0.99);
    json!({
        "related_distinct": rbd.len(),
        "rbd_p99": (p99 * 10_000.0).round() / 10_000.0,
        "paraphrase_above_rbd_p99": fraction(para.iter().filter(|c| **c > p99).count(), para.len()),
        "rbd_at_or_above_0.85": fraction(rbd.iter().filter(|c| **c >= 0.85).count(), rbd.len()),
        "paraphrase_at_or_above_0.85": fraction(para.iter().filter(|c| **c >= 0.85).count(), para.len()),
    })
}

fn fraction(num: usize, den: usize) -> Value {
    if den == 0 {
        Value::Null
    } else {
        json!(((num as f64 / den as f64) * 10_000.0).round() / 10_000.0)
    }
}

struct Args {
    dir: PathBuf,
    out: Option<PathBuf>,
    cache: Option<PathBuf>,
    export_only: bool,
    detail: Option<PathBuf>,
    allow_fixture: bool,
}

fn args() -> Result<Args, String> {
    let usage = "usage: eval_measure [--dir fixtures/eval] [--out run.json] \
                 [--cache vectors.jsonl] [--export-only] [--detail items.json] [--allow-fixture]";
    let mut a = Args {
        dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/eval"),
        out: None,
        cache: None,
        export_only: false,
        detail: None,
        allow_fixture: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--dir" => a.dir = it.next().ok_or(usage)?.into(),
            "--out" => a.out = Some(it.next().ok_or(usage)?.into()),
            "--cache" => a.cache = Some(it.next().ok_or(usage)?.into()),
            "--export-only" => a.export_only = true,
            "--detail" => a.detail = Some(it.next().ok_or(usage)?.into()),
            "--allow-fixture" => a.allow_fixture = true,
            "-h" | "--help" => {
                println!("{usage}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{usage}")),
        }
    }
    if a.export_only && a.detail.is_some() {
        return Err(format!(
            "--detail lists pairs and queries, which --export-only skips\n{usage}"
        ));
    }
    Ok(a)
}

/// Embeds texts once each, in the document or query role, with progress on
/// stderr. With `--cache`, vectors are also appended to a JSON-lines file
/// keyed by SHA-256 of (contract, role, text), so a rerun after an edit only
/// embeds what changed, and an interrupted run keeps its progress. The file
/// holds vectors and hashes, never text; keep it out of the repository.
struct Cache<'e> {
    embedder: &'e dyn Embedder,
    docs: HashMap<String, Vec<f32>>,
    queries: HashMap<String, Vec<f32>>,
    done: usize,
    contract: String,
    stored: HashMap<String, Vec<f32>>,
    file: Option<std::fs::File>,
}

#[derive(serde::Serialize, serde::Deserialize)]
struct CacheLine {
    key: String,
    v: Vec<f32>,
}

impl Cache<'_> {
    fn open(&mut self, path: &std::path::Path) -> Result<(), String> {
        use std::io::BufRead;
        if let Ok(f) = std::fs::File::open(path) {
            for line in std::io::BufReader::new(f).lines() {
                let line = line.map_err(|e| format!("{}: {e}", path.display()))?;
                // A torn last line from an interrupted run is skipped.
                if let Ok(l) = serde_json::from_str::<CacheLine>(&line) {
                    self.stored.insert(l.key, l.v);
                }
            }
        }
        let f = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)
            .map_err(|e| format!("{}: {e}", path.display()))?;
        self.file = Some(f);
        Ok(())
    }

    fn key(&self, role: &str, text: &str) -> String {
        use sha2::{Digest, Sha256};
        let mut h = Sha256::new();
        for part in [self.contract.as_str(), role, text] {
            h.update((part.len() as u64).to_le_bytes());
            h.update(part.as_bytes());
        }
        h.finalize().iter().map(|b| format!("{b:02x}")).collect()
    }

    fn recall(&self, role: &str, text: &str) -> Option<Vec<f32>> {
        self.stored.get(&self.key(role, text)).cloned()
    }

    fn keep(&mut self, role: &str, text: &str, v: &[f32]) -> Result<(), String> {
        use std::io::Write;
        let key = self.key(role, text);
        if let Some(f) = self.file.as_mut() {
            let line = serde_json::to_string(&CacheLine { key, v: v.to_vec() }).expect("json");
            writeln!(f, "{line}").map_err(|e| format!("writing the cache: {e}"))?;
        }
        Ok(())
    }

    /// `by_length` embeds the missing texts shortest first, so a batch pads
    /// little; the export keeps its own order, as the canary embeds it.
    async fn docs(&mut self, texts: &[String], by_length: bool) -> Result<(), String> {
        let mut todo: Vec<&str> = Vec::new();
        let mut seen = std::collections::HashSet::new();
        for t in texts {
            if self.docs.contains_key(t) || !seen.insert(t.as_str()) {
                continue;
            }
            match self.recall("doc", t) {
                Some(v) => {
                    self.docs.insert(t.clone(), v);
                }
                None => todo.push(t),
            }
        }
        if by_length {
            todo.sort_by_key(|t| t.len());
        }
        let cap = self.embedder.batch_capacity().max(1);
        for chunk in todo.chunks(cap) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
            let answers = self.embedder.embed_batch(chunk, deadline).await;
            for (text, v) in chunk.iter().zip(answers) {
                let v = v.map_err(|e| format!("embedding a document text: {e}"))?;
                self.keep("doc", text, &v)?;
                self.docs.insert(text.to_string(), v);
                self.tick();
            }
        }
        Ok(())
    }

    async fn queries(&mut self, texts: &[String]) -> Result<(), String> {
        for t in texts {
            if self.queries.contains_key(t) {
                continue;
            }
            if let Some(v) = self.recall("query", t) {
                self.queries.insert(t.clone(), v);
                continue;
            }
            {
                let v = self
                    .embedder
                    .embed_query(t)
                    .await
                    .map_err(|e| format!("embedding a query: {e}"))?;
                self.keep("query", t, &v)?;
                self.queries.insert(t.clone(), v);
                self.tick();
            }
        }
        Ok(())
    }

    fn tick(&mut self) {
        self.done += 1;
        if self.done.is_multiple_of(50) {
            eprintln!("  embedded {}", self.done);
        }
    }
}

#[tokio::main]
async fn main() {
    if let Err(message) = run().await {
        eprintln!("eval_measure: {message}");
        std::process::exit(2);
    }
}

/// Embed the export the way the canary does (`embed_all` in
/// `src/embed/canary.rs`): `texts` in order, duplicates included, chunked
/// by `batch_capacity`, timed as one pass. The deadline handed to the
/// embedder is generous so the pass always finishes and the report can say
/// by how much it is over or under `CANARY_EMBED_TIMEOUT`. Returns the
/// unrounded seconds; the vectors go into the cache for the rest of the run.
async fn canary_export_pass(cache: &mut Cache<'_>, texts: &[&str]) -> Result<f64, String> {
    let cap = cache.embedder.batch_capacity().max(1);
    let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
    let t0 = Instant::now();
    let mut vectors = Vec::with_capacity(texts.len());
    for chunk in texts.chunks(cap) {
        let answers = cache.embedder.embed_batch(chunk, deadline).await;
        if answers.len() < chunk.len() {
            return Err(format!(
                "the embedder returned {} vectors for a batch of {}",
                answers.len(),
                chunk.len()
            ));
        }
        for v in answers.into_iter().take(chunk.len()) {
            vectors.push(v.map_err(|e| format!("embedding an export text: {e}"))?);
        }
    }
    let secs = t0.elapsed().as_secs_f64();
    for (text, v) in texts.iter().zip(vectors) {
        if !cache.docs.contains_key(*text) {
            if !cache.stored.contains_key(&cache.key("doc", text)) {
                cache.keep("doc", text, &v)?;
            }
            cache.docs.insert(text.to_string(), v);
        }
    }
    Ok(secs)
}

async fn run() -> Result<(), String> {
    let args = args()?;
    let set = schema::load(&args.dir)?;
    let errors = schema::validate(&set, MAX_CONTENT_BYTES);
    if !errors.is_empty() {
        return Err(format!("the set fails its schema:\n{}", errors.join("\n")));
    }
    if set.corpus.is_empty() && !args.export_only {
        return Err("corpus.json holds no concepts: there is nothing to retrieve over".into());
    }

    let cfg = EmbedderConfig::from_env().map_err(|e| e.to_string())?;
    // Only fields that name the model: no URL, credential or local path, so
    // a run file can be committed as it is.
    let contract = json!({
        "dim": cfg.dim,
        "llama_model": cfg.llama_model,
        "device": cfg.device,
        "repo": cfg.repo,
        "revision": cfg.revision,
        "weights_file": cfg.weights_file,
        "gemini_model": cfg.gemini_model,
    });
    let kind = format!("{:?}", cfg.kind);
    if kind.to_ascii_lowercase().contains("fixture") && !args.allow_fixture {
        return Err(
            "LAMBO_EMBEDDER is the fixture embedder; its cosines mean nothing. \
                    Set LAMBO_EMBEDDER, or pass --allow-fixture for a harness smoke run"
                .into(),
        );
    }
    let embedder = build_embedder(cfg.clone()).map_err(|e| e.to_string())?;
    let started = Instant::now();
    embedder
        .prepare()
        .await
        .map_err(|e| format!("preparing the embedder: {e}"))?;
    let prepare_secs = started.elapsed().as_secs_f64();
    let identity = lambo::embed::candle_identity(embedder.as_ref());
    // #87: the EG2 contract's `model` (weights artifact and prompt profile),
    // which the calibration table keys on. Reported only: the cache key keeps
    // `identity` above, so existing caches stay valid.
    let reported_identity = identity
        .clone()
        .or_else(|| lambo::embed::eg2_identity(embedder.as_ref()));
    let mut cache = Cache {
        embedder: embedder.as_ref(),
        docs: HashMap::new(),
        queries: HashMap::new(),
        done: 0,
        // Everything that picks the vectors (endpoint, local weights, model,
        // project) keys the cache, so a different server or weights never
        // reuses another's vectors; credentials are left out. Only a SHA-256
        // of it reaches the cache file, and none of it the report.
        contract: format!(
            "{kind}|{contract}|{:?}|{:?}|{:?}|{:?}|{identity:?}",
            cfg.llama_url, cfg.weights_dir, cfg.gemini_project, cfg.gemini_location
        ),
        stored: HashMap::new(),
        file: None,
    };
    if let Some(path) = &args.cache {
        cache.open(path)?;
    }

    // 1. The export first, raw and alone, embedded as the canary embeds it:
    // every pair's a then b in file order, duplicates included, in batches
    // of the embedder's capacity, under one clock. Never from the cache, so
    // the timing is always the canary's own workload.
    let export_texts: Vec<&str> = set
        .export
        .iter()
        .flat_map(|p| [p.a.as_str(), p.b.as_str()])
        .collect();
    let export_secs = canary_export_pass(&mut cache, &export_texts).await?;

    // 2. Canary phrases, for the random-pair floor.
    let canaries: Vec<String> = CANARY_PHRASES.iter().map(|s| s.to_string()).collect();
    cache.docs(&canaries, false).await?;
    let mut floor = Vec::new();
    for (i, a) in canaries.iter().enumerate() {
        for b in &canaries[i + 1..] {
            floor.push(cosine64(&cache.docs[a], &cache.docs[b]));
        }
    }
    let floor = pair_quantiles(floor).expect("190 canary pairs");
    let export_cos: Vec<f64> = set
        .export
        .iter()
        .map(|p| cosine64(&cache.docs[&p.a], &cache.docs[&p.b]))
        .collect();
    let derived = pair_quantiles(export_cos).ok_or(
        "related_pairs.json holds no adjudicated related_distinct pairs: nothing to derive",
    )?;
    // The count and floor rules `[embedder] related_pairs` applies to the
    // derived p99. The canary also has to embed the file within its timeout,
    // which `within_canary_timeout` reports.
    let eligible = derived.pairs >= MIN_RELATED_PAIRS && derived.p99 > floor.p99;
    let within_timeout = export_secs <= CANARY_EMBED_TIMEOUT.as_secs_f64();

    let mut report = BTreeMap::new();
    report.insert(
        "embedder",
        json!({
            "kind": kind,
            "config": contract,
            "identity": reported_identity.as_deref().map(public_identity),
            "dimensions": embedder.dimensions(),
        }),
    );
    report.insert(
        "related_pairs_export",
        json!({
            "pairs": derived.pairs,
            "p50": derived.p50, "p90": derived.p90, "p99": derived.p99, "max": derived.max,
            "canary_random_pair_floor": { "p50": floor.p50, "p90": floor.p90, "p99": floor.p99, "max": floor.max },
            "threshold_rules_met": eligible,
            "within_canary_timeout": within_timeout,
            "embed_texts": export_texts.len(),
            "embed_secs": (export_secs * 10.0).round() / 10.0,
            "canary_embed_timeout_secs": CANARY_EMBED_TIMEOUT.as_secs(),
            "prepare_secs": (prepare_secs * 10.0).round() / 10.0,
        }),
    );

    if args.export_only {
        return emit(&report, args.out.as_deref());
    }

    // 3. Every text in every framing.
    let mut docs = Vec::new();
    for f in FRAMINGS {
        for p in &set.pairs {
            docs.push(frame(&p.a, f));
            docs.push(frame(&p.b, f));
        }
        for c in &set.corpus {
            docs.push(frame(&c.text, f));
        }
    }
    cache.docs(&docs, true).await?;
    let qtexts: Vec<String> = set.queries.iter().map(|q| q.query.clone()).collect();
    cache.queries(&qtexts).await?;

    // Per-item cosines and rankings, for reading the labels against the
    // embedder (`--detail`); they quote the set's own texts by id only.
    let mut detail: BTreeMap<&str, Vec<Value>> = BTreeMap::new();
    let mut pairs_out = BTreeMap::new();
    let mut retrieval_out = BTreeMap::new();
    for f in FRAMINGS {
        // Pairs: overall per label, per (label, family), per split.
        let cos =
            |p: &schema::Pair| cosine64(&cache.docs[&frame(&p.a, f)], &cache.docs[&frame(&p.b, f)]);
        let mut by_label: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut by_family: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut by_split: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        for p in &set.pairs {
            detail.entry("pairs").or_default().push(json!({
                "framing": f, "id": p.id, "label": p.label, "family": p.family,
                "cosine": (cos(p) * 10_000.0).round() / 10_000.0,
                "review": p.review.is_some(),
            }));
        }
        for p in set.pairs.iter().filter(|p| p.adjudicated()) {
            let c = cos(p);
            by_label.entry(p.label.clone()).or_default().push(c);
            by_family
                .entry(format!("{}/{}", p.label, p.family))
                .or_default()
                .push(c);
            by_split
                .entry(format!("{}/{}", p.label, p.split))
                .or_default()
                .push(c);
        }
        let para = by_label.get("paraphrase").cloned().unwrap_or_default();
        let rbd = by_label
            .get("related_distinct")
            .cloned()
            .unwrap_or_default();
        // The identifier family is what #193's guard refuses to merge before
        // any threshold applies, so the rest is reported on its own too.
        let rbd_other: Vec<f64> = by_family
            .iter()
            .filter(|(k, _)| {
                k.starts_with("related_distinct/") && *k != "related_distinct/identifier"
            })
            .flat_map(|(_, v)| v.iter().copied())
            .collect();
        let at_085: BTreeMap<&String, Value> = by_family
            .iter()
            .map(|(k, v)| {
                (
                    k,
                    fraction(v.iter().filter(|c| **c >= 0.85).count(), v.len()),
                )
            })
            .collect();
        let sep = json!({
            "all_related_distinct": separation(&rbd, &para),
            "related_distinct_without_identifier": separation(&rbd_other, &para),
            "at_or_above_0.85_by_family": at_085,
        });
        pairs_out.insert(
            f,
            json!({
                "by_label": by_label.into_iter().map(|(k, v)| (k, summary(v))).collect::<BTreeMap<_, _>>(),
                "by_family": by_family.into_iter().map(|(k, v)| (k, summary(v))).collect::<BTreeMap<_, _>>(),
                "by_split": by_split.into_iter().map(|(k, v)| (k, summary(v))).collect::<BTreeMap<_, _>>(),
                "separation": sep,
            }),
        );

        // Retrieval over the corpus in this framing.
        let corpus: Vec<(&str, &Vec<f32>)> = set
            .corpus
            .iter()
            .map(|c| (c.id.as_str(), &cache.docs[&frame(&c.text, f)]))
            .collect();
        let mut true_recall = Vec::new();
        let mut top1_answerable: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut top1_none: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        // Query against every concept it does not name as relevant (all of
        // them, for a no-answer query): the null a vector floor rests on.
        let mut irrelevant: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        #[derive(Default)]
        struct Acc {
            n: usize,
            hit1: usize,
            hit5: usize,
            hit10: usize,
            rr: f64,
            ndcg: f64,
        }
        let mut acc: BTreeMap<String, Acc> = BTreeMap::new();
        for q in set.queries.iter().filter(|q| q.adjudicated()) {
            let qv = &cache.queries[&q.query];
            let mut ranked: Vec<(f64, &str)> = corpus
                .iter()
                .map(|(id, v)| (cosine64(qv, v), *id))
                .collect();
            ranked.sort_by(|x, y| y.0.total_cmp(&x.0).then(x.1.cmp(y.1)));
            let top1 = ranked[0].0;
            detail.entry("queries").or_default().push(json!({
                "framing": f, "id": q.id, "kind": q.kind, "family": q.family,
                "top": ranked.iter().take(3).map(|(c, id)| json!([id, (c * 10_000.0).round() / 10_000.0])).collect::<Vec<_>>(),
                "relevant": q.relevant.iter().map(|r| json!([r.concept, r.grade])).collect::<Vec<_>>(),
            }));
            {
                let grade_of: HashMap<&str, u8> = q
                    .relevant
                    .iter()
                    .map(|r| (r.concept.as_str(), r.grade))
                    .collect();
                for (c, id) in &ranked {
                    let g = grade_of.get(id).copied().unwrap_or(0);
                    if g == 0 {
                        irrelevant.entry(q.split.clone()).or_default().push(*c);
                    }
                    if args.detail.is_some() {
                        detail.entry("query_concepts").or_default().push(json!({
                            "framing": f, "query": q.id, "split": q.split, "concept": id,
                            "grade": g, "cosine": (c * 10_000.0).round() / 10_000.0,
                        }));
                    }
                }
            }
            if q.kind == "no_answer" {
                top1_none.entry(q.split.clone()).or_default().push(top1);
                continue;
            }
            top1_answerable
                .entry(q.split.clone())
                .or_default()
                .push(top1);
            let grade: HashMap<&str, u8> = q
                .relevant
                .iter()
                .map(|r| (r.concept.as_str(), r.grade))
                .collect();
            for (c, id) in &ranked {
                if grade.get(id) == Some(&2) {
                    true_recall.push(*c);
                }
            }
            let first = ranked.iter().position(|(_, id)| grade.get(id) == Some(&2));
            let dcg: f64 = ranked
                .iter()
                .take(10)
                .enumerate()
                .map(|(i, (_, id))| {
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
                "all".to_string(),
                format!("split/{}", q.split),
                format!("family/{}", q.family),
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
        let r4 = |x: f64| (x * 10_000.0).round() / 10_000.0;
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
                        "mrr": r4(a.rr / a.n as f64),
                        "ndcg@10": r4(a.ndcg / a.n as f64),
                    }),
                )
            })
            .collect();
        // Split-conformal top-1 gate: tune on dev answerable, score held_out.
        let mut dev = top1_answerable.get("dev").cloned().unwrap_or_default();
        dev.sort_by(f64::total_cmp);
        let gate = if dev.is_empty() {
            Value::Null
        } else {
            let k = ((ALPHA * (dev.len() + 1) as f64).floor() as usize).clamp(1, dev.len());
            let tau = dev[k - 1];
            let held_a = top1_answerable.get("held_out").cloned().unwrap_or_default();
            let held_n = top1_none.get("held_out").cloned().unwrap_or_default();
            json!({
                "alpha": ALPHA,
                "tau": r4(tau),
                "held_out_answerable_kept": fraction(held_a.iter().filter(|c| **c >= tau).count(), held_a.len()),
                "held_out_no_answer_abstained": fraction(held_n.iter().filter(|c| **c < tau).count(), held_n.len()),
            })
        };
        retrieval_out.insert(
            f,
            json!({
                "true_recall_grade2": summary(true_recall),
                "query_to_irrelevant": irrelevant.into_iter().map(|(k, v)| (k, summary(v))).collect::<BTreeMap<_, _>>(),
                "metrics": metrics,
                "top1_answerable": top1_answerable.into_iter().map(|(k, v)| (k, summary(v))).collect::<BTreeMap<_, _>>(),
                "top1_no_answer": top1_none.into_iter().map(|(k, v)| (k, summary(v))).collect::<BTreeMap<_, _>>(),
                "conformal_top1_gate": gate,
            }),
        );
    }
    report.insert("pairs", json!(pairs_out));
    report.insert("retrieval", json!(retrieval_out));
    report.insert(
        "excluded_for_review",
        json!({
            "pairs": set.pairs.iter().filter(|p| !p.adjudicated()).count(),
            "queries": set.queries.iter().filter(|q| !q.adjudicated()).count(),
        }),
    );
    report.insert(
        "set",
        json!({
            "corpus": set.corpus.len(),
            "pairs": schema::count_by(&set.pairs, |p| p.label.clone()),
            "queries": schema::count_by(&set.queries, |q| q.kind.clone()),
        }),
    );

    if let Some(path) = &args.detail {
        let text = serde_json::to_string_pretty(&detail).expect("json");
        std::fs::write(path, text).map_err(|e| format!("{}: {e}", path.display()))?;
    }
    emit(&report, args.out.as_deref())
}

fn emit(report: &BTreeMap<&str, Value>, out: Option<&std::path::Path>) -> Result<(), String> {
    let text = serde_json::to_string_pretty(report).expect("json");
    println!("{text}");
    if let Some(out) = out {
        std::fs::write(out, format!("{text}\n")).map_err(|e| format!("{}: {e}", out.display()))?;
        eprintln!("wrote {}", out.display());
    }
    Ok(())
}
