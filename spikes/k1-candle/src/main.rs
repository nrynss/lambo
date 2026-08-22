//!
//! Modes:
//!   coldstart [--device cpu|cuda] [--fresh-cache]   cost: phase timings from process start
//!   parity    [--device ...]                        correctness: cosine self-agreement vs llama-server
//!   bench     [--device ...] [--sizes 32,256,...]   throughput: serial + batched legs, warm-up discarded
//!   llama-bench [--sizes ...]                       same harness against the rig's llama-server (baseline)
//!
//! Read-only measurement: writes nothing outside stdout and the data/ dir.
use anyhow::{bail, Context, Result};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_transformers::models::xlm_roberta::{Config, XLMRobertaModel};
use std::io::Write as _;
use std::path::PathBuf;
use std::time::{Duration, Instant};
use tokenizers::Tokenizer;

const MODEL_ID: &str = "BAAI/bge-m3";
const LLAMA_URL: &str = "http://127.0.0.1:8080/v1/embeddings";
/// Canonical artifact only (same-artifact rule, DOGFOOD-SETUP §1): BAAI ships no
/// safetensors, so weights come from pytorch_model.bin via VarBuilder::from_pth.
const WEIGHTS_FILE_DEFAULT: &str = "pytorch_model.bin";
/// Position embeddings cap minus headroom; BGE-M3 config says 8144.
const MAX_LEN: usize = 8100;

fn main() -> Result<()> {
    let t0 = Instant::now();
    let mut args = std::env::args().skip(1);
    let mode = args.next().unwrap_or_else(|| "help".into());
    let mut device_arg = "cpu".to_string();
    let mut fresh_cache = false;
    let mut weights_file = WEIGHTS_FILE_DEFAULT.to_string();
    let mut dtype_arg = "f32".to_string();
    let mut per_size = 24usize;
    let mut bs_cap = 16usize;
    let mut sizes: Vec<usize> = vec![32, 256, 2048, 8192];
    while let Some(a) = args.next() {
        match a.as_str() {
            "--device" => device_arg = args.next().context("--device needs a value")?,
            "--fresh-cache" => fresh_cache = true,
            "--sizes" => {
                sizes = args
                    .next()
                    .context("--sizes needs a value")?
                    .split(',')
                    .map(|s| s.parse())
                    .collect::<Result<_, _>>()
                    .context("bad --sizes")?;
            }
            "--dtype" => dtype_arg = args.next().context("--dtype needs a value")?,
            "--per-size" => per_size = args.next().context("--per-size needs a value")?.parse()?,
            "--bs-cap" => bs_cap = args.next().context("--bs-cap needs a value")?.parse()?,
            "--weights-file" => {
                weights_file = args.next().context("--weights-file needs a value")?;
            }
            other => bail!("unknown arg {other}"),
        }
    }
    match mode.as_str() {
        "coldstart" => coldstart(&device_arg, fresh_cache, t0, &weights_file, &dtype_arg),
        "parity" => parity(&device_arg, &weights_file, &dtype_arg),
        "bench" => bench(&device_arg, &sizes, &weights_file, &dtype_arg, per_size, bs_cap),
        "llama-bench" => llama_bench(&sizes, per_size),
        "help" | _ => {
            eprintln!("modes: coldstart | parity | bench | llama-bench");
            bail!("unknown mode {mode}")
        }
    }
}

// ---------------------------------------------------------------- device / cache

fn parse_dtype(s: &str) -> Result<DType> {
    match s {
        "f32" => Ok(DType::F32),
        "f16" => Ok(DType::F16),
        other => bail!("unknown dtype {other}"),
    }
}

fn pick_device(name: &str) -> Result<Device> {
    match name {
        "cpu" => Ok(Device::Cpu),
        "cuda" => Ok(Device::new_cuda(0).context("CUDA requested but unavailable")?),
        other => bail!("unknown device {other}"),
    }
}

/// hf-hub 1.0: client.model(owner, name); cache dir from HF_HOME, XDG, then ~/.cache.
fn hf_model_repo(id: &str) -> Result<hf_hub::HFRepositorySync<hf_hub::RepoTypeModel>> {
    let client = hf_hub::HFClientSync::new().context("hf client")?;
    let (owner, name) = id.split_once('/').context("repo id must be owner/name")?;
    Ok(client.model(owner, name))
}

fn hf_file(repo: &hf_hub::HFRepositorySync<hf_hub::RepoTypeModel>, name: &str) -> Result<PathBuf> {
    repo.download_file()
        .filename(name.to_string())
        .send()
        .with_context(|| format!("fetch {name}"))
}

struct CandleBge {
    model: XLMRobertaModel,
    tokenizer: Tokenizer,
    device: Device,
    dtype: DType,
}

impl CandleBge {
    /// The ~150-line load path K2 would ship: resolve -> mmap safetensors -> XLM-R -> ready.
    fn load(
        device: Device,
        weights_file: &str,
        dtype_s: &str,
        phases: Option<&mut Vec<(&'static str, Duration)>>,
    ) -> Result<Self> {
        let mut log: Vec<(&'static str, Duration)> = Vec::new();
        let now = Instant::now();
        let cfg_file = hf_file(&hf_model_repo(MODEL_ID)?, "config.json")?;
        let tok_file = hf_file(&hf_model_repo(MODEL_ID)?, "tokenizer.json")?;
        let weights_path = hf_file(&hf_model_repo(MODEL_ID)?, &weights_file)?;
        log.push(("hub-resolve+fetch", now.elapsed()));

        let now = Instant::now();
        let tokenizer = Tokenizer::from_file(&tok_file).map_err(|e| anyhow::anyhow!("{e}"))?;
        log.push(("tokenizer-load", now.elapsed()));

        let now = Instant::now();
        let config: Config =
            serde_json::from_str(&std::fs::read_to_string(cfg_file)?).context("parse config.json")?;
        // Canonical fp32 pytorch_model.bin (same-artifact rule); f16 is a post-load cast,
        // which is also exactly what K2 would do before shipping half-width weights.
        let vb = candle_nn::VarBuilder::from_pth(&weights_path, DType::F32, &device)
            .context("reading pytorch_model.bin")?;
        let vb = if dtype_s == "f16" { vb.to_dtype(DType::F16) } else { vb };
        log.push(("weights-mmap", now.elapsed()));

        let now = Instant::now();
        let model = XLMRobertaModel::new(&config, vb).context("build XLMRobertaModel")?;
        log.push(("model-build", now.elapsed()));

        if let Some(p) = phases {
            *p = log;
        }
        Ok(Self { model, tokenizer, device, dtype: parse_dtype(dtype_s)? })
    }

    /// One forward per text, CLS row, L2-normalized. bs=1 keeps parity pure.
    fn embed_one(&self, text: &str) -> Result<Vec<f32>> {
        let enc = self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let ids: Vec<u32> = enc.get_ids().iter().copied().take(MAX_LEN).collect();
        let l = ids.len();
        let ids = Tensor::from_vec(ids, (1, l), &self.device)?;
        let ones = Tensor::ones((1, l), self.dtype, &self.device)?;
        let zeros = Tensor::zeros((1, l), DType::U32, &self.device)?;
        let out = self
            .model
            .forward(&ids, &ones, &zeros, None, None, None)?;
        let cls = out.i((0, 0))?.squeeze(0)?;
        Ok(l2_normalize(cls.to_dtype(DType::F32)?.to_vec1::<f32>()?))
    }

    /// Padded batch forward. Returns one normalized vector per input.
    fn embed_batch(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>> {
        let encs: Vec<_> = texts
            .iter()
            .map(|t| self.tokenizer.encode(*t, true))
            .collect::<std::result::Result<_, _>>()
            .map_err(|e| anyhow::anyhow!("tokenize: {e}"))?;
        let id_rows: Vec<Vec<u32>> = encs
            .iter()
            .map(|e| e.get_ids().iter().copied().take(MAX_LEN).collect())
            .collect();
        let max_l = id_rows.iter().map(|r| r.len()).max().unwrap_or(1);
        let pad = 1u32; // XLM-R <pad>
        let mut ids_mat = vec![0u32; id_rows.len() * max_l];
        let mut mask_mat = vec![0.0f32; id_rows.len() * max_l];
        for (row, ids) in id_rows.iter().enumerate() {
            for (col, id) in ids.iter().enumerate() {
                ids_mat[row * max_l + col] = *id;
                mask_mat[row * max_l + col] = 1.0;
            }
            for col in ids.len()..max_l {
                ids_mat[row * max_l + col] = pad;
            }
        }
        let b = id_rows.len();
        anyhow::ensure!(
            b > 0 && max_l > 0 && ids_mat.len() == b * max_l,
            "embed_batch degenerate: b={b} max_l={max_l} mat={} texts={:?}",
            ids_mat.len(),
            texts.first().map(|t| t.chars().take(60).collect::<String>())
        );
        let ids = Tensor::from_vec(ids_mat, (b, max_l), &self.device)?;
        let mask = Tensor::from_vec(mask_mat, (b, max_l), &self.device)?.to_dtype(self.dtype)?;
        let zeros = Tensor::zeros((b, max_l), DType::U32, &self.device)?;
        let out = self.model.forward(&ids, &mask, &zeros, None, None, None)?;
        let mut vecs = Vec::with_capacity(b);
        for i in 0..b {
            let cls = out.i((i, 0))?.squeeze(0)?;
            vecs.push(l2_normalize(cls.to_dtype(DType::F32)?.to_vec1::<f32>()?));
        }
        Ok(vecs)
    }
}


fn l2_normalize(mut v: Vec<f32>) -> Vec<f32> {
    let n: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!(n.is_finite() && n > 1e-6, "zero or non-finite norm");
    for x in &mut v {
        *x /= n;
    }
    v
}

// ---------------------------------------------------------------- corpus

fn load_dogfood_corpus() -> Result<Vec<String>> {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("data/dogfood-corpus.jsonl");
    let raw = std::fs::read_to_string(manifest).context("data/dogfood-corpus.jsonl missing")?;
    #[derive(serde::Deserialize)]
    struct Row {
        text: String,
    }
    Ok(raw.lines().filter_map(|l| serde_json::from_str::<Row>(l).ok().map(|r| r.text)).collect())
}

/// Deterministic spread sized for batch legs: `per_size` items per bucket,
/// variant-marked so batches are not six copies of one string.
fn synthetic_spread_n(sizes: &[usize], per_size: usize) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for &size in sizes {
        for k in 0..per_size {
            let base = format!("{} variant {}.", BASE_TEXTS[k % BASE_TEXTS.len()], k);
            let mut s = String::new();
            while s.len() < size {
                s.push_str(&base);
                s.push(' ');
            }
            let mut cut = size.min(s.len());
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push((size, s[..cut].to_string()));
        }
    }
    out
}

/// Deterministic multilingual spread across the size ladder (32 B .. 8 KiB).
const BASE_TEXTS: [&str; 6] = [
        "The canonization fair test compares revision rates across sessions.",
        "Kanonisierungstests vergleichen die Revisionsraten über Sitzungen.",
        "正典化の公正なテストは、セッション間の改訂率を比較します。",
        "规范化的公平测试比较各个会话之间的修订率。",
        "اختبار التقييس العادل يقارن معدلات المراجعة عبر الجلسات.",
        "Справедливый тест канонизации сравнивает показатели изменений между сессиями.",
];

fn synthetic_spread(sizes: &[usize]) -> Vec<(usize, String)> {
    let mut out = Vec::new();
    for &size in sizes {
        for base in BASE_TEXTS {
            let mut s = String::new();
            while s.len() < size {
                s.push_str(base);
                s.push(' ');
            }
            // cut at a char boundary
            let mut cut = size.min(s.len());
            while !s.is_char_boundary(cut) {
                cut -= 1;
            }
            out.push((size, s[..cut].to_string()));
        }
    }
    out
}

// ---------------------------------------------------------------- llama leg

fn llama_embed(text: &str) -> Result<Vec<f32>> {
    #[derive(serde::Deserialize)]
    struct Resp {
        data: Vec<Datum>,
    }
    #[derive(serde::Deserialize)]
    struct Datum {
        embedding: Vec<f32>,
    }
    let mut last: Option<anyhow::Error> = None;
    for attempt in 0..4 {
        if attempt > 0 {
            std::thread::sleep(Duration::from_millis(250 * 2u64.pow(attempt)));
        }
        let r: std::result::Result<Resp, _> = ureq::post(LLAMA_URL)
            .timeout(Duration::from_secs(120))
            .send_json(ureq::json!({ "input": text }))
            .map_err(|e| anyhow::anyhow!("llama http: {e}"))
            .and_then(|r| r.into_json().context("llama json"));
        match r {
            Ok(resp) => {
                return Ok(resp
                    .data
                    .into_iter()
                    .next()
                    .context("no embedding in response")?
                    .embedding)
            }
            Err(e) => last = Some(e),
        }
    }
    Err(last.unwrap())
}

// ---------------------------------------------------------------- stats

fn cosine(a: &[f32], b: &[f32]) -> f32 {
    let dot: f32 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    dot / (na * nb)
}

fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    let n = v.len();
    if n % 2 == 1 {
        v[n / 2]
    } else {
        (v[n / 2 - 1] + v[n / 2]) / 2.0
    }
}

/// Spearman rank correlation between two same-length samples (average ranks on ties).
fn spearman(a: &[f64], b: &[f64]) -> f64 {
    fn ranks(v: &[f64]) -> Vec<f64> {
        let mut idx: Vec<usize> = (0..v.len()).collect();
        idx.sort_by(|&i, &j| v[i].partial_cmp(&v[j]).unwrap());
        let mut r = vec![0.0; v.len()];
        let mut i = 0;
        while i < idx.len() {
            let mut j = i;
            while j + 1 < idx.len() && v[idx[j + 1]] == v[idx[i]] {
                j += 1;
            }
            let avg = (i + j) as f64 / 2.0 + 1.0;
            for k in i..=j {
                r[idx[k]] = avg;
            }
            i = j + 1;
        }
        r
    }
    let (ra, rb) = (ranks(a), ranks(b));
    let n = ra.len() as f64;
    let ma = ra.iter().sum::<f64>() / n;
    let mb = rb.iter().sum::<f64>() / n;
    let num: f64 = ra.iter().zip(&rb).map(|(x, y)| (x - ma) * (y - mb)).sum();
    let da: f64 = ra.iter().map(|x| (x - ma) * (x - ma)).sum::<f64>().sqrt();
    let db: f64 = rb.iter().map(|x| (x - mb) * (x - mb)).sum::<f64>().sqrt();
    num / (da * db)
}

fn coldstart(device_arg: &str, fresh_cache: bool, t0: Instant, weights_repo: &str, dtype_s: &str) -> Result<()> {
    let device = pick_device(device_arg)?;
    let cache_dir = fresh_cache.then(|| {
        let d = std::env::temp_dir().join(format!("k1-fresh-hf-{}", std::process::id()));
        std::fs::create_dir_all(&d).unwrap();
        d
    });
    // Override the default cache location when asked to measure a first-run fetch.
    if let Some(d) = &cache_dir {
        std::env::set_var("HF_HOME", d);
    }
    let mut phases = Vec::new();
    let backend = CandleBge::load(device, weights_repo, dtype_s, Some(&mut phases))?;
    let loaded_at = t0.elapsed();
    let now = Instant::now();
    backend.embed_one("cold start probe: first embed after weight load.")?;
    let first = now.elapsed();
    let now = Instant::now();
    backend.embed_one("second embed, steady state.")?;
    let second = now.elapsed();

    println!("{{\"mode\":\"coldstart\",\"device\":\"{device_arg}\",\"fresh_fetch\":{fresh_cache}}}");
    for (name, d) in &phases {
        println!("phase {name}: {:.2?}", d);
    }
    println!(
        "TOTAL process->ready: {:.2?} (first embed +{:.2?}, second embed {:.2?})",
        loaded_at, first, second
    );
    println!("gate: client spawn budget ~30 s (J2 measured opencode giving up at ~32 s)");
    if let Some(d) = cache_dir {
        println!("fresh cache dir (left for inspection): {}", d.display());
    }
    Ok(())
}

fn parity(device_arg: &str, weights_repo: &str, dtype_s: &str) -> Result<()> {
    let device = pick_device(device_arg)?;
    let backend = CandleBge::load(device, weights_repo, dtype_s, None)?;

    let mut items: Vec<(String, String)> = Vec::new(); // (label, text)
    for (i, t) in load_dogfood_corpus()?.into_iter().enumerate() {
        items.push((format!("dogfood#{i}"), t));
    }
    for (size, t) in synthetic_spread(&[32, 128, 512, 2048, 8192]) {
        items.push((format!("synth-{size}B"), t));
    }

    let cache = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("data/candle-vecs-{device_arg}-{dtype_s}.json"));
    let candle_vecs: Vec<Vec<f32>> = match std::fs::read_to_string(&cache)
        .ok()
        .and_then(|raw| serde_json::from_str(&raw).ok())
    {
        Some(v) => {
            println!("candle leg: reusing cached vectors from {}", cache.display());
            v
        }
        None => {
            print!("embedding {} items through candle ({device_arg})...", items.len());
            std::io::stdout().flush().ok();
            let v: Vec<Vec<f32>> = items
                .iter()
                .enumerate()
                .map(|(i, (_, t))| {
                    if i % 20 == 0 {
                        print!(" {i}");
                        std::io::stdout().flush().ok();
                    }
                    backend.embed_one(t)
                })
                .collect::<Result<_>>()?;
            std::fs::write(&cache, serde_json::to_string(&v)?)?;
            println!(" done (cached to {})", cache.display());
            v
        }
    };

    println!("embedding through llama-server q8_0...");
    let llama_vecs: Vec<Vec<f32>> = items
        .iter()
        .enumerate()
        .map(|(i, (_, t))| {
            if i % 20 == 0 {
                print!(" {i}");
                std::io::stdout().flush().ok();
            }
            llama_embed(t)
        })
        .collect::<Result<_>>()?;
    println!(" done");

    let dims_ok = candle_vecs.iter().chain(llama_vecs.iter()).all(|v| v.len() == 1024);
    println!("dim check (both sides 1024): {dims_ok}");

    let mut self_cos = Vec::new();
    println!("\nper-item cosine(candle, llama):");
    for (i, ((label, _), (c, l))) in items.iter().zip(candle_vecs.iter().zip(&llama_vecs)).enumerate() {
        let cos = cosine(c, l);
        self_cos.push(cos);
        if cos < 0.99 || i < 8 || label.starts_with("synth") {
            println!("  {label:<16} cos={cos:.5}");
        }
    }

    // Operational consequence of subtly wrong pooling: do rankings survive the swap?
    let n = self_cos.len();
    let mut sim_candle = Vec::new();
    let mut sim_llama = Vec::new();
    for i in 0..n {
        for j in (i + 1)..n {
            sim_candle.push(cosine(&candle_vecs[i], &candle_vecs[j]) as f64);
            sim_llama.push(cosine(&llama_vecs[i], &llama_vecs[j]) as f64);
        }
    }
    let rho = spearman(&sim_candle, &sim_llama);

    let self_f64: Vec<f64> = self_cos.iter().map(|&x| x as f64).collect();
    let med = median(self_f64.clone());
    let min = self_f64.iter().cloned().fold(f64::INFINITY, f64::min);
    let below = self_cos.iter().filter(|&&c| c < 0.99).count();
    println!("\nSELF-AGREEMENT: median={med:.5} min={min:.5} items_below_0.99={below}/{n}");
    println!("RANK AGREEMENT (Spearman over all pairwise sims): rho={rho:.4}");
    println!("GATE (median >= 0.99): {}", if med >= 0.99 { "PASS" } else { "FAIL" });
    Ok(())
}

fn bench(device_arg: &str, sizes: &[usize], weights_repo: &str, dtype_s: &str, per_size: usize, bs_cap: usize) -> Result<()> {
    let device = pick_device(device_arg)?;
    let backend = CandleBge::load(device, weights_repo, dtype_s, None)?;
    let spread = synthetic_spread_n(sizes, per_size);

    // Group by size bucket.
    let mut buckets: Vec<(usize, Vec<String>)> = Vec::new();
    for (size, text) in &spread {
        match buckets.iter_mut().find(|(s, _)| s == size) {
            Some((_, v)) => v.push(text.clone()),
            None => buckets.push((*size, vec![text.clone()])),
        }
    }
    let warmup = 3;
    println!(
        "{{\"mode\":\"bench\",\"backend\":\"candle\",\"device\":\"{device_arg}\"}}"
    );
    for (size, texts) in &buckets {
        // warm-up, discarded
        for t in texts.iter().take(warmup) {
            backend.embed_one(t)?;
        }
        // serial leg
        let now = Instant::now();
        for t in texts {
            backend.embed_one(t)?;
        }
        let serial_s = now.elapsed().as_secs_f64();
        let serial_ips = texts.len() as f64 / serial_s;

        // batched legs (pad-to-longest within batch)
        let mut batch_line = String::new();
        for bs in [8usize, 16].iter().copied().filter(|&b| b <= bs_cap) {
            if texts.len() < bs {
                continue;
            }
            let refs: Vec<&str> = texts.iter().map(|s| s.as_str()).collect();
            // one warm-up batch
            backend.embed_batch(&refs[..bs])?;
            let batches = refs.chunks(bs);
            let count = batches.len() * bs;
            let now = Instant::now();
            for chunk in refs.chunks(bs) {
                backend.embed_batch(chunk)?;
            }
            let ips = count as f64 / now.elapsed().as_secs_f64();
            batch_line.push_str(&format!("  batch{bs}: {:>9.1} items/s", ips));
        }
        println!(
            "input~{:>5}B n={:>3}  serial: {:>9.1} items/s{}",
            size,
            texts.len(),
            serial_ips,
            batch_line
        );
    }
    println!("baseline: llama.cpp q8_0 measured 110-141 items/s on this rig (J3 probe)");
    Ok(())
}

fn llama_bench(sizes: &[usize], per_size: usize) -> Result<()> {
    let spread = synthetic_spread_n(sizes, per_size);
    let mut buckets: Vec<(usize, Vec<String>)> = Vec::new();
    for (size, text) in &spread {
        match buckets.iter_mut().find(|(s, _)| s == size) {
            Some((_, v)) => v.push(text.clone()),
            None => buckets.push((*size, vec![text.clone()])),
        }
    }
    let warmup = 3;
    println!("{{\"mode\":\"bench\",\"backend\":\"llama.cpp-q8_0\",\"device\":\"cpu\"}}");
    for (size, texts) in &buckets {
        for t in texts.iter().take(warmup) {
            llama_embed(t)?;
        }
        let now = Instant::now();
        for t in texts {
            llama_embed(t)?;
        }
        let ips = texts.len() as f64 / now.elapsed().as_secs_f64();
        println!("input~{:>5}B n={:>3}  serial: {:>9.1} items/s", size, texts.len(), ips);
    }
    Ok(())
}
