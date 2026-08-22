//! K1 spike — in-process BGE-M3 through candle, measured on CPU and Metal.
//!
//! Read-only measurement for `dev-diary/lambo-for-mooshik/K-candle-embedder.md`.
//! Nothing here is wired into the `lambo` crate: no registry arm, no feature
//! flag, no dependency edge. That is K2, and K2 is gated on these numbers.
//!
//! The path under test is deliberately the whole path an adapter would run:
//! tokenize (XLM-RoBERTa, pad id from config), forward `XLMRobertaModel`,
//! take the **CLS** token of the last hidden state (BGE-M3's dense head is
//! CLS, not mean), L2-normalize. Getting any one of those wrong produces
//! vectors that are plausible but wrong, which is the failure mode the parity
//! gate exists to catch.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use anyhow::{bail, Context, Result};
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{Config, XLMRobertaModel};
use clap::{Parser, Subcommand, ValueEnum};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

/// The revision this spike pins. Recorded so a re-run is against the same
/// artifact rather than whatever `main` has drifted to.
const MODEL_REPO: &str = "BAAI/bge-m3";
const MODEL_REVISION: &str = "5617a9f61b028005a4858fdac845db406aefb181";

/// BGE-M3 is XLM-RoBERTa-large with 8194 position slots; position ids are
/// offset by `pad_token_id`, so the usable sequence length is 8192.
const MAX_SEQ_LEN: usize = 8192;

#[derive(Parser)]
#[command(name = "k1-candle-bgem3", about = "K1 spike: BGE-M3 via candle")]
struct Cli {
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Resolve weights + tokenizer through hf-hub, reporting fetch wall time.
    Fetch,
    /// Embed a JSONL corpus (`{"id":..,"text":..}`) to JSONL vectors.
    Embed {
        #[arg(long)]
        input: PathBuf,
        #[arg(long)]
        out: PathBuf,
        #[command(flatten)]
        rt: Runtime,
    },
    /// Throughput by the J3 probe method: warm-up discarded, serial +
    /// concurrent legs at representative input sizes.
    Bench {
        #[command(flatten)]
        rt: Runtime,
        /// Input sizes in bytes. J3 probes 35 (PROBE_TEXT) and 1024
        /// (PROBE_TEXT_BYTES).
        #[arg(long, value_delimiter = ',', default_values_t = vec![35usize, 1024])]
        sizes: Vec<usize>,
        /// J3's PROBE_CONCURRENCY.
        #[arg(long, default_value_t = 4)]
        concurrency: usize,
        #[arg(long, default_value_t = 24)]
        iters: usize,
        #[arg(long, default_value_t = 4)]
        warmup: usize,
    },
    /// Cold start: process entry -> weights resident -> first embed returned.
    Coldstart {
        #[command(flatten)]
        rt: Runtime,
    },
}

#[derive(clap::Args, Clone)]
struct Runtime {
    #[arg(long, value_enum, default_value_t = Backend::Cpu)]
    device: Backend,
    #[arg(long, value_enum, default_value_t = Dtype::F32)]
    dtype: Dtype,
}

#[derive(Copy, Clone, ValueEnum, PartialEq)]
enum Backend {
    Cpu,
    Metal,
}

#[derive(Copy, Clone, ValueEnum, PartialEq)]
enum Dtype {
    F32,
    F16,
}

impl Runtime {
    fn device(&self) -> Result<Device> {
        match self.device {
            Backend::Cpu => Ok(Device::Cpu),
            Backend::Metal => {
                #[cfg(feature = "metal")]
                {
                    Device::new_metal(0).context("opening Metal device 0")
                }
                #[cfg(not(feature = "metal"))]
                {
                    bail!("built without the `metal` feature; rebuild with --features metal")
                }
            }
        }
    }

    fn dtype(&self) -> DType {
        match self.dtype {
            Dtype::F32 => DType::F32,
            Dtype::F16 => DType::F16,
        }
    }

    fn label(&self) -> String {
        let d = match self.device {
            Backend::Cpu => "cpu",
            Backend::Metal => "metal",
        };
        let t = match self.dtype {
            Dtype::F32 => "f32",
            Dtype::F16 => "f16",
        };
        format!("{d}-{t}")
    }
}

/// Resolved artifact paths plus how long resolving them took.
struct Artifacts {
    config: PathBuf,
    tokenizer: PathBuf,
    weights: PathBuf,
    fetch_ms: f64,
}

fn resolve_artifacts() -> Result<Artifacts> {
    use hf_hub::{api::sync::Api, Repo, RepoType};
    let started = Instant::now();
    let api = Api::new().context("building hf-hub api")?;
    let repo = api.repo(Repo::with_revision(
        MODEL_REPO.to_string(),
        RepoType::Model,
        MODEL_REVISION.to_string(),
    ));
    let config = repo.get("config.json").context("fetching config.json")?;
    let tokenizer = repo.get("tokenizer.json").context("fetching tokenizer.json")?;
    // The canonical BAAI/bge-m3 repo ships **no** `model.safetensors`; the only
    // full-precision weight file is a PyTorch pickle. candle reads it directly
    // via `VarBuilder::from_pth`, so the spike loads the canonical artifact
    // rather than a third-party safetensors mirror.
    let weights = repo
        .get("pytorch_model.bin")
        .context("fetching pytorch_model.bin")?;
    Ok(Artifacts {
        config,
        tokenizer,
        weights,
        fetch_ms: started.elapsed().as_secs_f64() * 1000.0,
    })
}

struct BgeM3 {
    model: XLMRobertaModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl BgeM3 {
    fn load(a: &Artifacts, device: Device, dtype: DType) -> Result<Self> {
        let cfg: Config = serde_json::from_slice(&std::fs::read(&a.config)?)
            .context("parsing config.json into xlm_roberta::Config")?;

        let vb = load_varbuilder(&a.weights, dtype, &device)?;
        let model = XLMRobertaModel::new(&cfg, vb).context("building XLMRobertaModel")?;

        let mut tokenizer =
            Tokenizer::from_file(&a.tokenizer).map_err(|e| anyhow::anyhow!("tokenizer: {e}"))?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: MAX_SEQ_LEN,
                ..Default::default()
            }))
            .map_err(|e| anyhow::anyhow!("truncation: {e}"))?;
        // `pad_id` MUST be the model's pad token (1 for XLM-R), not tokenizers'
        // default of 0. The embedding layer derives position ids from
        // `input_ids.ne(pad_token_id)`, so a wrong pad id silently shifts every
        // position in a padded batch — a "plausible but wrong" vector of
        // exactly the kind the parity gate is here to catch.
        tokenizer.with_padding(Some(PaddingParams {
            strategy: PaddingStrategy::BatchLongest,
            pad_id: cfg.pad_token_id,
            pad_type_id: 0,
            pad_token: "<pad>".to_string(),
            ..Default::default()
        }));

        Ok(Self {
            model,
            tokenizer,
            device,
        })
    }

    /// Tokenize -> forward -> CLS -> L2-normalize. Returns one unit vector per
    /// input, in input order.
    fn embed(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| anyhow::anyhow!("encode_batch: {e}"))?;
        let batch = encodings.len();
        let seq = encodings[0].get_ids().len();

        let mut ids = Vec::with_capacity(batch * seq);
        let mut mask = Vec::with_capacity(batch * seq);
        for e in &encodings {
            ids.extend_from_slice(e.get_ids());
            mask.extend_from_slice(e.get_attention_mask());
        }

        let input_ids = Tensor::from_vec(ids, (batch, seq), &self.device)?;
        let attention_mask = Tensor::from_vec(mask, (batch, seq), &self.device)?;
        // BGE-M3 is single-segment (`type_vocab_size = 1`).
        let token_type_ids = Tensor::zeros((batch, seq), DType::U32, &self.device)?;

        let hidden = self
            .model
            .forward(&input_ids, &attention_mask, &token_type_ids, None, None, None)?;

        // BGE-M3's dense representation is the CLS token, not a mean pool.
        let cls = hidden.i((.., 0))?.to_dtype(DType::F32)?;
        let norm = cls.sqr()?.sum_keepdim(1)?.sqrt()?;
        let normed = cls.broadcast_div(&norm)?;
        Ok(normed.to_vec2::<f32>()?)
    }

    fn token_count(&self, text: &str) -> Result<usize> {
        Ok(self
            .tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("encode: {e}"))?
            .get_ids()
            .len())
    }
}

fn load_varbuilder<'a>(weights: &Path, dtype: DType, device: &Device) -> Result<VarBuilder<'a>> {
    let ext = weights.extension().and_then(|e| e.to_str()).unwrap_or("");
    match ext {
        "safetensors" => {
            let vb = unsafe {
                VarBuilder::from_mmaped_safetensors(&[weights.to_path_buf()], dtype, device)?
            };
            Ok(vb)
        }
        "bin" | "pth" | "pt" => {
            VarBuilder::from_pth(weights, dtype, device).context("reading pytorch_model.bin")
        }
        other => bail!("unsupported weight extension: {other:?}"),
    }
}

#[derive(serde::Deserialize)]
struct CorpusItem {
    id: String,
    text: String,
}

/// J3's probe text and its size-parameterised form, reproduced so the
/// throughput legs are measured on the same shapes the baseline was.
fn probe_text_at(bytes: usize) -> String {
    const SEED: &str = "lambo write queue calibration probe";
    let mut s = String::with_capacity(bytes + SEED.len());
    while s.len() < bytes {
        s.push_str(SEED);
        s.push(' ');
    }
    s.truncate(bytes);
    s
}

fn main() -> Result<()> {
    let process_started = Instant::now();
    let cli = Cli::parse();

    match cli.cmd {
        Cmd::Fetch => {
            let a = resolve_artifacts()?;
            let size = std::fs::metadata(&a.weights)?.len();
            println!(
                "{}",
                serde_json::json!({
                    "op": "fetch",
                    "repo": MODEL_REPO,
                    "revision": MODEL_REVISION,
                    "weights": a.weights,
                    "weights_bytes": size,
                    "tokenizer": a.tokenizer,
                    "config": a.config,
                    "fetch_ms": a.fetch_ms,
                })
            );
        }

        Cmd::Embed { input, out, rt } => {
            let a = resolve_artifacts()?;
            let load_started = Instant::now();
            let bge = BgeM3::load(&a, rt.device()?, rt.dtype())?;
            let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;

            let raw = std::fs::read_to_string(&input)?;
            let items: Vec<CorpusItem> = raw
                .lines()
                .filter(|l| !l.trim().is_empty())
                .map(serde_json::from_str)
                .collect::<Result<_, _>>()
                .context("parsing corpus JSONL")?;

            let mut w = String::new();
            for item in &items {
                let started = Instant::now();
                // One text per forward: the parity leg must not let batching
                // padding change a vector.
                let v = bge.embed(std::slice::from_ref(&item.text))?;
                let ms = started.elapsed().as_secs_f64() * 1000.0;
                let tokens = bge.token_count(&item.text)?;
                w.push_str(&serde_json::json!({
                    "id": item.id,
                    "backend": rt.label(),
                    "tokens": tokens,
                    "bytes": item.text.len(),
                    "embed_ms": ms,
                    "vector": v[0],
                })
                .to_string());
                w.push('\n');
            }
            std::fs::write(&out, w)?;
            eprintln!(
                "embedded {} items on {} (load {:.0} ms) -> {}",
                items.len(),
                rt.label(),
                load_ms,
                out.display()
            );
        }

        Cmd::Bench {
            rt,
            sizes,
            concurrency,
            iters,
            warmup,
        } => {
            let a = resolve_artifacts()?;
            let bge = Arc::new(BgeM3::load(&a, rt.device()?, rt.dtype())?);

            for bytes in sizes {
                let text = probe_text_at(bytes);
                let tokens = bge.token_count(&text)?;

                // Warm-up, discarded — J3's PROBE_WARMUP_EMBEDS, widened.
                for _ in 0..warmup {
                    bge.embed(std::slice::from_ref(&text))?;
                }

                // Serial leg: one text at a time.
                let started = Instant::now();
                for _ in 0..iters {
                    bge.embed(std::slice::from_ref(&text))?;
                }
                let serial_s = started.elapsed().as_secs_f64();
                let serial_rps = iters as f64 / serial_s;

                // Concurrent leg: `concurrency` threads sharing one model, the
                // in-process analogue of J3's N-wide probe against llama-server.
                let per = iters / concurrency;
                let started = Instant::now();
                std::thread::scope(|s| -> Result<()> {
                    let mut handles = Vec::new();
                    for _ in 0..concurrency {
                        let bge = Arc::clone(&bge);
                        let text = text.clone();
                        handles.push(s.spawn(move || -> Result<()> {
                            for _ in 0..per {
                                bge.embed(std::slice::from_ref(&text))?;
                            }
                            Ok(())
                        }));
                    }
                    for h in handles {
                        h.join().map_err(|_| anyhow::anyhow!("worker panicked"))??;
                    }
                    Ok(())
                })?;
                let conc_s = started.elapsed().as_secs_f64();
                let conc_rps = (per * concurrency) as f64 / conc_s;

                // Batched leg: what an in-process adapter would actually do
                // with N outstanding requests — one forward, batch dim N.
                let batch: Vec<String> = (0..concurrency).map(|_| text.clone()).collect();
                let rounds = (iters / concurrency).max(1);
                let started = Instant::now();
                for _ in 0..rounds {
                    bge.embed(&batch)?;
                }
                let batch_s = started.elapsed().as_secs_f64();
                let batch_rps = (rounds * concurrency) as f64 / batch_s;

                println!(
                    "{}",
                    serde_json::json!({
                        "op": "bench",
                        "backend": rt.label(),
                        "input_bytes": bytes,
                        "input_tokens": tokens,
                        "iters": iters,
                        "warmup_discarded": warmup,
                        "concurrency": concurrency,
                        "serial_items_per_s": serial_rps,
                        "serial_ms_per_item": serial_s * 1000.0 / iters as f64,
                        "concurrent_items_per_s": conc_rps,
                        "batched_items_per_s": batch_rps,
                    })
                );
            }
        }

        Cmd::Coldstart { rt } => {
            let a = resolve_artifacts()?;
            let load_started = Instant::now();
            let bge = BgeM3::load(&a, rt.device()?, rt.dtype())?;
            let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;

            let probe = probe_text_at(35);
            let first_started = Instant::now();
            bge.embed(std::slice::from_ref(&probe))?;
            let first_embed_ms = first_started.elapsed().as_secs_f64() * 1000.0;

            let second_started = Instant::now();
            bge.embed(std::slice::from_ref(&probe))?;
            let second_embed_ms = second_started.elapsed().as_secs_f64() * 1000.0;

            println!(
                "{}",
                serde_json::json!({
                    "op": "coldstart",
                    "backend": rt.label(),
                    "hub_resolve_ms": a.fetch_ms,
                    "weight_load_ms": load_ms,
                    "first_embed_ms": first_embed_ms,
                    "second_embed_ms": second_embed_ms,
                    "process_to_ready_ms": process_started.elapsed().as_secs_f64() * 1000.0
                        - first_embed_ms
                        - second_embed_ms,
                    "process_to_first_vector_ms":
                        process_started.elapsed().as_secs_f64() * 1000.0 - second_embed_ms,
                })
            );
        }
    }

    Ok(())
}
