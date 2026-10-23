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
//! The fixture embedder (the default when `LAMBO_EMBEDDER` is unset) gives
//! meaningless numbers, so it is refused unless `--allow-fixture` is passed
//! (a smoke run of the harness itself).

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
    allow_fixture: bool,
}

fn args() -> Result<Args, String> {
    let usage = "usage: eval_measure [--dir fixtures/eval] [--out run.json] [--allow-fixture]";
    let mut a = Args {
        dir: PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("fixtures/eval"),
        out: None,
        allow_fixture: false,
    };
    let mut it = std::env::args().skip(1);
    while let Some(flag) = it.next() {
        match flag.as_str() {
            "--dir" => a.dir = it.next().ok_or(usage)?.into(),
            "--out" => a.out = Some(it.next().ok_or(usage)?.into()),
            "--allow-fixture" => a.allow_fixture = true,
            "-h" | "--help" => {
                println!("{usage}");
                std::process::exit(0);
            }
            other => return Err(format!("unknown argument {other:?}\n{usage}")),
        }
    }
    Ok(a)
}

/// Embeds texts once each, in the document or query role, with progress on
/// stderr.
struct Cache<'e> {
    embedder: &'e dyn Embedder,
    docs: HashMap<String, Vec<f32>>,
    queries: HashMap<String, Vec<f32>>,
    done: usize,
}

impl Cache<'_> {
    async fn docs(&mut self, texts: &[String]) -> Result<(), String> {
        let todo: Vec<&str> = {
            let mut seen = std::collections::HashSet::new();
            texts
                .iter()
                .filter(|t| !self.docs.contains_key(*t) && seen.insert(t.as_str()))
                .map(String::as_str)
                .collect()
        };
        let cap = self.embedder.batch_capacity().max(1);
        for chunk in todo.chunks(cap) {
            let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
            let answers = self.embedder.embed_batch(chunk, deadline).await;
            for (text, v) in chunk.iter().zip(answers) {
                let v = v.map_err(|e| format!("embedding a document text: {e}"))?;
                self.docs.insert(text.to_string(), v);
                self.tick();
            }
        }
        Ok(())
    }

    async fn queries(&mut self, texts: &[String]) -> Result<(), String> {
        for t in texts {
            if !self.queries.contains_key(t) {
                let v = self
                    .embedder
                    .embed_query(t)
                    .await
                    .map_err(|e| format!("embedding a query: {e}"))?;
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

async fn run() -> Result<(), String> {
    let args = args()?;
    let set = schema::load(&args.dir)?;
    let errors = schema::validate(&set, MAX_CONTENT_BYTES);
    if !errors.is_empty() {
        return Err(format!("the set fails its schema:\n{}", errors.join("\n")));
    }

    let cfg = EmbedderConfig::from_env().map_err(|e| e.to_string())?;
    let contract = format!("{cfg:?}");
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
    let mut cache = Cache {
        embedder: embedder.as_ref(),
        docs: HashMap::new(),
        queries: HashMap::new(),
        done: 0,
    };

    // 1. The export first, raw and alone, timed as the canary would embed it.
    let export_texts: Vec<String> = set
        .export
        .iter()
        .flat_map(|p| [p.a.clone(), p.b.clone()])
        .collect();
    let t0 = Instant::now();
    cache.docs(&export_texts).await?;
    let export_secs = t0.elapsed().as_secs_f64();

    // 2. Canary phrases, for the random-pair floor.
    let canaries: Vec<String> = CANARY_PHRASES.iter().map(|s| s.to_string()).collect();
    cache.docs(&canaries).await?;
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
    let derived = pair_quantiles(export_cos).expect("non-empty export");
    let applied = derived.pairs >= MIN_RELATED_PAIRS && derived.p99 > floor.p99;

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
    cache.docs(&docs).await?;
    let qtexts: Vec<String> = set.queries.iter().map(|q| q.query.clone()).collect();
    cache.queries(&qtexts).await?;

    let mut report = BTreeMap::new();
    report.insert(
        "embedder",
        json!({ "kind": kind, "config": contract, "dimensions": embedder.dimensions() }),
    );
    report.insert(
        "related_pairs_export",
        json!({
            "pairs": derived.pairs,
            "p50": derived.p50, "p90": derived.p90, "p99": derived.p99, "max": derived.max,
            "canary_random_pair_floor": { "p50": floor.p50, "p90": floor.p90, "p99": floor.p99, "max": floor.max },
            "would_apply": applied,
            "embed_secs": (export_secs * 10.0).round() / 10.0,
            "canary_embed_timeout_secs": CANARY_EMBED_TIMEOUT.as_secs(),
            "prepare_secs": (prepare_secs * 10.0).round() / 10.0,
        }),
    );

    let mut pairs_out = BTreeMap::new();
    let mut retrieval_out = BTreeMap::new();
    for f in FRAMINGS {
        // Pairs: overall per label, per (label, family), per split.
        let cos =
            |p: &schema::Pair| cosine64(&cache.docs[&frame(&p.a, f)], &cache.docs[&frame(&p.b, f)]);
        let mut by_label: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut by_family: BTreeMap<String, Vec<f64>> = BTreeMap::new();
        let mut by_split: BTreeMap<String, Vec<f64>> = BTreeMap::new();
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
        let rbd = by_label
            .get("related_distinct")
            .cloned()
            .unwrap_or_default();
        let para = by_label.get("paraphrase").cloned().unwrap_or_default();
        let mut sorted_rbd = rbd.clone();
        sorted_rbd.sort_by(f64::total_cmp);
        let rbd_p99 = quantile(&sorted_rbd, 0.99);
        let sep = json!({
            "rbd_p99": (rbd_p99 * 10_000.0).round() / 10_000.0,
            "paraphrase_above_rbd_p99": fraction(para.iter().filter(|c| **c > rbd_p99).count(), para.len()),
            "rbd_at_or_above_0.85": fraction(rbd.iter().filter(|c| **c >= 0.85).count(), rbd.len()),
            "paraphrase_at_or_above_0.85": fraction(para.iter().filter(|c| **c >= 0.85).count(), para.len()),
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

    let text = serde_json::to_string_pretty(&report).expect("json");
    println!("{text}");
    if let Some(out) = args.out {
        std::fs::write(&out, format!("{text}\n")).map_err(|e| format!("{}: {e}", out.display()))?;
        eprintln!("wrote {}", out.display());
    }
    Ok(())
}
