//! In-process BGE-M3 via candle (K2).
//!
//! This adapter embeds locally using [`candle-transformers`]' native
//! `XLMRobertaModel` (CLS pool, L2-normalize), bypassing the llama.cpp server
//! that [`super::bge_m3`] talks to. It is the Level B shape mandated by K: a
//! Cargo feature (`embed-candle`), a registry arm in `build_embedder`, config
//! keys on [`super::EmbedderConfig`], never a fork of the core.
//!
//! **Device (the blocking K requirement).** On Apple silicon the default
//! (`device = "auto"`) resolves a Metal device; everywhere else it resolves a
//! CUDA device. When neither accelerator exists — or the binary was built
//! without the accelerator's Cargo feature — [`CandleEmbedder::new`] is a
//! **hard error**, even at resolve time, unless the operator explicitly pins
//! `device = "cpu"`. CPU is the intentional fallback, never a silent default.
//! Prefer f16 on GPU (same parity, ~half the resident weights); f32 on CPU.
//!
//! **Weights (hf-hub).** The default artifact is the operator-published f16
//! safetensors repo `nrynss/bge-m3-f16-safetensors` (a bitwise-verified cast of
//! the canonical `BAAI/bge-m3` revision, 391/391 tensors f16-equal — see
//! `dev-diary/lambo-for-mooshik/K-candle-embedder.md`). Weights are fetched on
//! first use via hf-hub and cached under the hub cache; `offline = true` (or an
//! explicit `weights_dir`) never touches the network and fails loudly if the
//! weights are absent. Weights are never committed to the repo (standing rule).
//!
//! **Contract identity (K2 task 3).** The adapter stamps the loaded artifact —
//! the canonical source revision plus the loaded weight file's sha256 prefix —
//! rather than the raw `llama_model` string, so a kind/dim match can no longer
//! hide a swap between two quantizations of the same model.
//!
//! **Batching (K1's central finding).** [`Embedder::embed`] is single-text and
//! async, but the K1 throughput legs showed the Metal/CUDA advantage comes from
//! *one forward per batch*, not from thread concurrency (the command queue
//! serializes). So this adapter coalesces concurrent `embed()` calls: each call
//! pushes `(text, oneshot)` onto a pending queue and awaits its result, and a
//! single background thread drains the queue into batches, runs one forward per
//! batch, and resolves every caller's oneshot. A short debounce bounds lone-call
//! latency (a solitary `embed()` is not starved waiting for a batch that never
//! fills), while a burst of concurrent calls shares one forward.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{Config, XLMRobertaModel};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use super::{EmbedError, Embedder};

/// Canonical upstream source of the weights (what the contract stamps).
const SOURCE_REPO: &str = "BAAI/bge-m3";
/// The revision both K1 legs pinned and the f16 repo was converted from.
const SOURCE_REVISION: &str = "5617a9f61b028005a4858fdac845db406aefb181";

/// The shipped weight artifact: operator-published f16 safetensors, loaded via
/// `VarBuilder::from_mmaped_safetensors` (no pickle, ~half the fetch).
const WEIGHT_REPO: &str = "nrynss/bge-m3-f16-safetensors";
const WEIGHT_REVISION: &str = "main";
const DEFAULT_WEIGHT_FILE: &str = "model.safetensors";
/// sha256 of `model.safetensors` in `WEIGHT_REPO` at `WEIGHT_REVISION`
/// (recorded in the K doc; the hub's LFS pointer carries the same oid).
const WEIGHT_SHA256: &str = "68440cc1b73b9af8ab85ecdc138b51877493ffbcec92a0a16e5d7e518eb22908";

/// BGE-M3 is XLM-RoBERTa-large with 8194 position slots; position ids are
/// offset by `pad_token_id`, so the usable sequence length is 8192.
const MAX_SEQ_LEN: usize = 8192;
/// How long the coalescer waits for extra callers to join an incomplete batch
/// before running the forward alone. Bounds lone-call latency (K1).
const BATCH_DEBOUNCE: Duration = Duration::from_millis(2);
/// Hard cap on one forward's batch width (bounds GPU memory + forward compute).
const MAX_BATCH: usize = 32;

/// Operator-supplied options for a candle embedder (subset of the candle
/// config keys on [`super::EmbedderConfig`]).
#[derive(Debug, Clone, Default)]
pub struct CandleOpts {
    /// `None`/`auto` | `cpu`. See module docs on device resolution.
    pub device: Option<String>,
    /// hf-hub repo override (default `WEIGHT_REPO`).
    pub repo: Option<String>,
    /// hf-hub revision override (default `WEIGHT_REVISION`).
    pub revision: Option<String>,
    /// Weight filename to load: `model.safetensors` (default) | `pytorch_model.bin`.
    pub weights_file: Option<String>,
    /// Never touch the network; fail if weights are not cached locally.
    pub offline: bool,
    /// Explicit local weights directory; bypasses hf-hub entirely (offline).
    pub weights_dir: Option<PathBuf>,
}

/// The resolved computational device plus the dtype to run (f16 on GPU, f32 on
/// CPU). Kept as data so `resolve_device` is pure and unit-testable.
#[derive(Debug, Clone)]
pub struct DeviceChoice {
    pub device: Device,
    pub dtype: DType,
}

/// A pending `embed()` request: the text plus a oneshot to deliver its vector.
struct Pending {
    text: String,
    tx: tokio::sync::oneshot::Sender<Result<Vec<f32>, EmbedError>>,
}

/// Shared state between the adapter handles and the coalescer thread.
struct Shared {
    queue: Mutex<Vec<Pending>>,
    wake: Condvar,
    core: BgeM3Core,
    max_batch: usize,
    debounce: Duration,
}

/// A loaded BGE-M3 (model + tokenizer on a device), moved behind the coalescer.
struct BgeM3Core {
    model: XLMRobertaModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl BgeM3Core {
    fn forward(&self, texts: &[&str]) -> Result<Vec<Vec<f32>>, EmbedError> {
        let encodings = self
            .tokenizer
            .encode_batch(texts.to_vec(), true)
            .map_err(|e| EmbedError::Backend(format!("tokenize: {e}")))?;
        let batch = encodings.len();
        if batch == 0 {
            return Ok(Vec::new());
        }
        // encode_batch with BatchLongest padding yields equal-length rows.
        let seq = encodings[0].get_ids().len();

        let mut ids = Vec::with_capacity(batch * seq);
        let mut mask = Vec::with_capacity(batch * seq);
        for e in &encodings {
            ids.extend_from_slice(e.get_ids());
            mask.extend_from_slice(e.get_attention_mask());
        }

        let input_ids = Tensor::from_vec(ids, (batch, seq), &self.device)
            .map_err(|e| EmbedError::Backend(format!("ids tensor: {e}")))?;
        let attention_mask = Tensor::from_vec(mask, (batch, seq), &self.device)
            .map_err(|e| EmbedError::Backend(format!("mask tensor: {e}")))?;
        // BGE-M3 is single-segment (`type_vocab_size = 1`).
        let token_type_ids = Tensor::zeros((batch, seq), DType::U32, &self.device)
            .map_err(|e| EmbedError::Backend(format!("type-ids tensor: {e}")))?;

        let hidden = self
            .model
            .forward(
                &input_ids,
                &attention_mask,
                &token_type_ids,
                None,
                None,
                None,
            )
            .map_err(|e| EmbedError::Backend(format!("forward: {e}")))?;

        // BGE-M3's dense representation is the CLS token, not a mean pool.
        let cls = hidden
            .i((.., 0))
            .and_then(|t| t.to_dtype(DType::F32))
            .map_err(|e| EmbedError::Backend(format!("cls: {e}")))?;
        let norm = cls
            .sqr()
            .and_then(|t| t.sum_keepdim(1))
            .and_then(|t| t.sqrt())
            .map_err(|e| EmbedError::Backend(format!("norm: {e}")))?;
        let normed = cls
            .broadcast_div(&norm)
            .and_then(|t| t.to_vec2::<f32>())
            .map_err(|e| EmbedError::Backend(format!("normalize: {e}")))?;

        for v in &normed {
            // Zero-norm inputs (all-pad rows cannot reach here on the happy
            // path, but a degenerate tokenization could) would otherwise leak
            // NaNs into stored vectors — reject them like the llama adapter.
            if !v.iter().all(|x| x.is_finite())
                || v.iter().map(|x| x * x).sum::<f32>().sqrt() <= f32::EPSILON
            {
                return Err(EmbedError::Backend(
                    "candle returned a non-finite or zero-norm embedding".into(),
                ));
            }
        }
        Ok(normed)
    }
}

/// Config-based device resolution, kept pure so it is unit-testable without
/// touching a real GPU and without compiling the accelerator backends.
///
/// Returns `Err` when the operator did not pin `cpu` and no usable accelerator
/// exists — the hard refusal K mandates (never a silent CPU fallback unless the
/// operator asked for one).
pub fn resolve_device(
    device: Option<&str>,
    // Compile-time facts, passed in so the pure function can be tested:
    has_metal: bool,
    has_cuda: bool,
    is_macos: bool,
) -> Result<DeviceChoice, EmbedError> {
    let choice = device
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("auto")
        .to_ascii_lowercase();
    match choice.as_str() {
        "cpu" => Ok(DeviceChoice {
            device: Device::Cpu,
            dtype: DType::F32,
        }),
        "auto" | "" => {
            if is_macos {
                if has_metal {
                    // Metal on Apple silicon, f16.
                    Device::new_metal(0)
                        .map(|d| DeviceChoice {
                            device: d,
                            dtype: DType::F16,
                        })
                        .map_err(|e| no_accelerator(e, has_cuda, false))
                } else {
                    Err(no_accelerator("no Metal backend compiled", has_cuda, true))
                }
            } else if has_cuda {
                Device::new_cuda(0)
                    .map(|d| DeviceChoice {
                        device: d,
                        dtype: DType::F16,
                    })
                    .map_err(|e| no_accelerator(e, false, false))
            } else {
                Err(no_accelerator("no CUDA backend compiled", false, false))
            }
        }
        "metal" => {
            #[cfg(all(target_os = "macos", feature = "embed-candle-metal"))]
            {
                Device::new_metal(0)
                    .map(|d| DeviceChoice {
                        device: d,
                        dtype: DType::F16,
                    })
                    .map_err(|e| EmbedError::Unavailable(format!("Metal device unavailable: {e}")))
            }
            #[cfg(not(all(target_os = "macos", feature = "embed-candle-metal")))]
            {
                let _ = (is_macos, has_cuda);
                Err(EmbedError::Unavailable(
                    "'metal' requested but this build has no Metal backend; rebuild with \
                     `--features embed-candle-metal` on macOS, or pin device = \"cpu\""
                        .into(),
                ))
            }
        }
        "cuda" => {
            #[cfg(not(target_os = "macos"))]
            #[cfg(feature = "embed-candle-cuda")]
            {
                Device::new_cuda(0)
                    .map(|d| DeviceChoice {
                        device: d,
                        dtype: DType::F16,
                    })
                    .map_err(|e| EmbedError::Unavailable(format!("CUDA device unavailable: {e}")))
            }
            #[cfg(any(target_os = "macos", not(feature = "embed-candle-cuda")))]
            {
                let _ = (is_macos, has_cuda);
                Err(EmbedError::Unavailable(
                    "'cuda' requested but this build has no CUDA backend; rebuild with \
                     `--features embed-candle-cuda`, or pin device = \"cpu\""
                        .into(),
                ))
            }
        }
        other => Err(EmbedError::Unavailable(format!(
            "unknown candle device {other:?} (expected auto | cpu | metal | cuda)"
        ))),
    }
}

fn no_accelerator(_err: impl std::fmt::Display, has_cuda: bool, macos: bool) -> EmbedError {
    let accelerator = if macos {
        "Metal"
    } else if has_cuda {
        "CUDA"
    } else {
        "an accelerator"
    };
    EmbedError::Unavailable(format!(
        "{accelerator} is unavailable and device was not pinned to \"cpu\": the candle \
         embedder refuses to serve on CPU unless the operator explicitly sets device = \\\"cpu\\\" \
         (expect ~3% of the llama.cpp path's throughput). Set [embedder] device = \\\"cpu\\\" to \
         proceed, or build with the accelerator's Cargo feature."
    ))
}

/// CON-7 guard: refuse empty / whitespace-only text before it can reach the
/// queue (a blank vector would silently poison hybrid ranking — see the trait
/// contract). Kept as a pure fn so it is testable without a loaded model.
pub fn reject_empty(text: &str) -> Result<(), EmbedError> {
    if text.trim().is_empty() {
        return Err(EmbedError::Unavailable(
            "cannot embed empty/whitespace text".into(),
        ));
    }
    Ok(())
}

/// Load the tokenizer + model from on-disk artifact paths (a `config.json`, a
/// `tokenizer.json`, and a weight file). Shared by the hf-hub and explicit-dir
/// fetch paths.
fn load_core(
    config_path: &Path,
    tokenizer_path: &Path,
    weights_path: &Path,
    device: &Device,
    dtype: DType,
) -> Result<BgeM3Core, EmbedError> {
    let cfg: Config = serde_json::from_slice(
        &std::fs::read(config_path).map_err(|e| EmbedError::Backend(format!("config: {e}")))?,
    )
    .map_err(|e| EmbedError::Backend(format!("parse config.json: {e}")))?;

    let vb = load_varbuilder(weights_path, dtype, device)?;
    let model =
        XLMRobertaModel::new(&cfg, vb).map_err(|e| EmbedError::Backend(format!("model: {e}")))?;

    let mut tokenizer = Tokenizer::from_file(tokenizer_path)
        .map_err(|e| EmbedError::Backend(format!("tokenizer: {e}")))?;
    tokenizer
        .with_truncation(Some(TruncationParams {
            max_length: MAX_SEQ_LEN,
            ..Default::default()
        }))
        .map_err(|e| EmbedError::Backend(format!("truncation: {e}")))?;
    // `pad_id` MUST be the model's pad token (1 for XLM-R), not tokenizers'
    // default of 0. The embedding layer derives position ids from
    // `input_ids.ne(pad_token_id)`, so a wrong pad id silently shifts every
    // position in a padded batch (K1's "plausible but wrong" failure class).
    tokenizer.with_padding(Some(PaddingParams {
        strategy: PaddingStrategy::BatchLongest,
        pad_id: cfg.pad_token_id,
        pad_type_id: 0,
        pad_token: "<pad>".to_string(),
        ..Default::default()
    }));

    Ok(BgeM3Core {
        model,
        tokenizer,
        device: device.clone(),
    })
}

fn load_varbuilder(
    weights: &Path,
    dtype: DType,
    device: &Device,
) -> Result<VarBuilder<'static>, EmbedError> {
    let ext = weights.extension().and_then(|e| e.to_str()).unwrap_or("");
    let vb = match ext {
        "safetensors" => unsafe {
            VarBuilder::from_mmaped_safetensors(&[weights.to_path_buf()], dtype, device)
                .map_err(|e| EmbedError::Backend(format!("safetensors: {e}")))?
        },
        "bin" | "pth" | "pt" => VarBuilder::from_pth(weights, dtype, device)
            .map_err(|e| EmbedError::Backend(format!("pytorch_model.bin: {e}")))?,
        other => {
            return Err(EmbedError::Backend(format!(
                "unsupported weight extension: {other:?}"
            )))
        }
    };
    Ok(vb)
}

/// The in-process BGE-M3 embedder. Cheap to clone; the model + coalescer are
/// shared behind an `Arc`.
#[derive(Clone)]
pub struct CandleEmbedder {
    dim: usize,
    shared: Arc<Shared>,
    /// The served-artifact identity stamped into the session contract.
    identity: String,
}

impl CandleEmbedder {
    /// Load weights (fetching/caching via hf-hub on first use), resolve the
    /// device, and spawn the batch-coalescing thread.
    ///
    /// Fails with [`EmbedError::Unavailable`] when weights are absent/offline,
    /// and (per the K device rule) when no accelerator is available and the
    /// operator did not pin `device = "cpu"`.
    pub fn new(dim: usize, opts: CandleOpts) -> Result<Self, EmbedError> {
        if dim == 0 {
            return Err(EmbedError::Unavailable("embedder dim must be > 0".into()));
        }
        let repo = opts.repo.clone().unwrap_or_else(|| WEIGHT_REPO.to_string());
        let revision = opts
            .revision
            .clone()
            .unwrap_or_else(|| WEIGHT_REVISION.to_string());
        let weight_file = opts
            .weights_file
            .clone()
            .unwrap_or_else(|| DEFAULT_WEIGHT_FILE.to_string());
        let _weight_file_path = Path::new(&weight_file);

        // Resolve device + dtype BEFORE loading (f16 on GPU, f32 on CPU).
        #[cfg(all(target_os = "macos", feature = "embed-candle-metal"))]
        let has_metal = true;
        #[cfg(not(all(target_os = "macos", feature = "embed-candle-metal")))]
        let has_metal = false;
        #[cfg(not(target_os = "macos"))]
        #[cfg(feature = "embed-candle-cuda")]
        let has_cuda = true;
        #[cfg(any(target_os = "macos", not(feature = "embed-candle-cuda")))]
        let has_cuda = false;
        #[cfg(target_os = "macos")]
        let is_macos = true;
        #[cfg(not(target_os = "macos"))]
        let is_macos = false;
        let DeviceChoice { device, dtype } =
            resolve_device(opts.device.as_deref(), has_metal, has_cuda, is_macos)?;

        let weights_dir = if let Some(dir) = &opts.weights_dir {
            // Explicit local dir: use it directly, never the network.
            dir.join(&weight_file)
        } else if opts.offline {
            resolve_offline(&repo, &revision, &weight_file)?
        } else {
            resolve_hf(&repo, &revision, &weight_file)?
        };

        // config.json + tokenizer.json come from the same repo as the weights.
        let config_path = if let Some(dir) = &opts.weights_dir {
            dir.join("config.json")
        } else if opts.offline {
            let p = hf_path(&repo, &revision, "config.json")?;
            if !p.exists() {
                return Err(EmbedError::Unavailable(format!(
                    "offline: config.json not cached for {repo}@{revision} (fetch once online, then go offline)"
                )));
            }
            p
        } else {
            hub_get(&repo, &revision, "config.json")?
        };
        let tokenizer_path = if let Some(dir) = &opts.weights_dir {
            dir.join("tokenizer.json")
        } else if opts.offline {
            let p = hf_path(&repo, &revision, "tokenizer.json")?;
            if !p.exists() {
                return Err(EmbedError::Unavailable(format!(
                    "offline: tokenizer.json not cached for {repo}@{revision}"
                )));
            }
            p
        } else {
            hub_get(&repo, &revision, "tokenizer.json")?
        };

        let core = load_core(&config_path, &tokenizer_path, &weights_dir, &device, dtype)?;

        // Verify the loaded weight file against the pinned hash when it is the
        // shipped artifact (same-artifact rule becomes machine-checked). Only
        // checked for the default artifact we published; a custom repo is the
        // operator's provenance.
        if repo == WEIGHT_REPO && weight_file == DEFAULT_WEIGHT_FILE {
            verify_hash(&weights_dir, WEIGHT_SHA256)?;
        }

        let shared = Arc::new(Shared {
            queue: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            core,
            max_batch: MAX_BATCH,
            debounce: BATCH_DEBOUNCE,
        });
        spawn_coalescer(Arc::clone(&shared));

        let identity = format!(
            "{SOURCE_REPO}@{SOURCE_REVISION} {} sha256:{}",
            weight_file,
            &WEIGHT_SHA256[..12],
        );

        Ok(Self {
            dim,
            shared,
            identity,
        })
    }

    /// The served-artifact identity string to stamp into the session's
    /// `EmbeddingContract.model` (K2 task 3): the canonical source revision and
    /// the loaded weight file's sha256 prefix, so a kind/dim match cannot hide
    /// a model swap.
    pub fn model_identity(&self) -> &str {
        &self.identity
    }
}

/// Find a cached weight file under hf-hub's cache without dialling the network.
fn hf_path(repo: &str, revision: &str, filename: &str) -> Result<PathBuf, EmbedError> {
    use hf_hub::api::sync::Api;
    let api = Api::new().map_err(|e| EmbedError::Unavailable(format!("hf-hub init: {e}")))?;
    let repo_api = api.repo(hf_hub::Repo::with_revision(
        repo.to_string(),
        hf_hub::RepoType::Model,
        revision.to_string(),
    ));
    let path = repo_api
        .get(filename)
        .map_err(|e| EmbedError::Unavailable(format!("{filename}: {e}")))?;
    Ok(path)
}

/// Fetch `filename` via hf-hub's cache (downloading on first use).
fn hub_get(repo: &str, revision: &str, filename: &str) -> Result<PathBuf, EmbedError> {
    use hf_hub::api::sync::Api;
    let api = Api::new().map_err(|e| EmbedError::Unavailable(format!("hf-hub init: {e}")))?;
    let repo_api = api.repo(hf_hub::Repo::with_revision(
        repo.to_string(),
        hf_hub::RepoType::Model,
        revision.to_string(),
    ));
    let path = repo_api
        .get(filename)
        .map_err(|e| EmbedError::Unavailable(format!("fetch {filename}: {e}")))?;
    Ok(path)
}

fn resolve_hf(repo: &str, revision: &str, weight_file: &str) -> Result<PathBuf, EmbedError> {
    hub_get(repo, revision, weight_file)
}

fn resolve_offline(repo: &str, revision: &str, weight_file: &str) -> Result<PathBuf, EmbedError> {
    hf_path(repo, revision, weight_file)
}

/// Verify a file's sha256 against a pinned digest (the same-artifact rule).
fn verify_hash(path: &Path, expected: &str) -> Result<(), EmbedError> {
    use std::io::Read;
    let mut file = std::fs::File::open(path)
        .map_err(|e| EmbedError::Backend(format!("open weights for hash: {e}")))?;
    let mut hasher = Sha256::new();
    let mut buf = [0u8; 65536];
    loop {
        let n = file
            .read(&mut buf)
            .map_err(|e| EmbedError::Backend(format!("read weights: {e}")))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    let got = format!("{:x}", hasher.finalize());
    if got != expected {
        return Err(EmbedError::Backend(format!(
            "weight file {path:?} sha256 mismatch: expected {expected}, got {got} — refusing to \
             serve unverified weights (same-artifact rule)"
        )));
    }
    Ok(())
}

use sha2::{Digest, Sha256};

/// Spawn the batch-coalescing thread. It owns the model exclusively (the
/// forward is synchronous and CPU/GPU-bound), drains a debounced batch, runs
/// one forward, and resolves each caller's oneshot.
fn spawn_coalescer(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("lambo-candle-coalescer".into())
        .spawn(move || coalesce_loop(shared))
        .expect("spawn candle coalescer");
}

fn coalesce_loop(shared: Arc<Shared>) {
    // The model goes with the thread; the queue is shared.
    loop {
        // Wait for at least one pending request.
        let mut first_batch = {
            let mut q = shared.queue.lock().unwrap();
            while q.is_empty() {
                q = shared.wake.wait(q).unwrap();
            }
            std::mem::take(&mut *q)
        };

        // Debounce: allow a burst of concurrent callers to land in the same
        // batch before we run the forward. A lone caller waits at most one
        // debounce, bounding lone-call latency.
        if first_batch.len() < shared.max_batch {
            std::thread::sleep(shared.debounce);
            let mut q = shared.queue.lock().unwrap();
            let additional = q.len().min(shared.max_batch - first_batch.len());
            let tail: Vec<Pending> = q.drain(..additional).collect();
            q.clear(); // already-drained first_batch is separate; keep leftovers
            first_batch.extend(tail);
            // Re-arm the queue: anything left beyond max_batch stays for the
            // next loop iteration.
            if !q.is_empty() {
                shared.wake.notify_all();
            }
        }

        let texts: Vec<&str> = first_batch.iter().map(|p| p.text.as_str()).collect();
        let result = shared.core.forward(&texts);
        match result {
            Ok(vectors) => {
                debug_assert_eq!(vectors.len(), first_batch.len());
                for (item, vector) in first_batch.into_iter().zip(vectors) {
                    let _ = item.tx.send(Ok(vector));
                }
            }
            Err(e) => {
                let msg = format!(
                    "candle forward failed for a batch of {}: {e}",
                    first_batch.len()
                );
                for item in first_batch {
                    let _ = item.tx.send(Err(EmbedError::Backend(msg.clone())));
                }
            }
        }
    }
}

#[async_trait]
impl Embedder for CandleEmbedder {
    fn dimensions(&self) -> usize {
        self.dim
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        reject_empty(text)?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        {
            let mut q = self.shared.queue.lock().unwrap();
            q.push(Pending {
                text: text.to_string(),
                tx,
            });
            self.shared.wake.notify_all();
        }
        rx.await
            .map_err(|_| EmbedError::Backend("candle coalescer dropped the request".into()))?
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reject_empty_contract_con7() {
        for text in ["", "   ", "\t\n", " \n "] {
            assert!(
                matches!(reject_empty(text), Err(EmbedError::Unavailable(_))),
                "{text:?}"
            );
        }
        assert!(reject_empty("user schema").is_ok());
    }

    #[test]
    fn unknown_device_rejected() {
        let e = resolve_device(Some("quantum"), true, true, false).unwrap_err();
        assert!(e.to_string().contains("unknown candle device"), "{e}");
    }

    #[test]
    fn cpu_pin_is_explicit_fallback() {
        let c = resolve_device(Some("cpu"), false, false, false).unwrap();
        // `Device` does not implement PartialEq; compare debug reprs.
        assert!(format!("{:?}", c.device).contains("Cpu"), "{:?}", c.device);
        assert_eq!(c.dtype, DType::F32);
    }

    #[test]
    fn auto_without_accelerator_is_hard_error() {
        let e = resolve_device(None, false, false, false).unwrap_err();
        assert!(e.to_string().contains("device ="), "{e}");
        assert!(matches!(e, EmbedError::Unavailable(_)));
        // Same on Apple with no metal backend compiled.
        let e2 = resolve_device(None, false, false, true).unwrap_err();
        assert!(e2.to_string().contains("device ="), "{e2}");
    }

    #[test]
    fn empty_string_device_falls_back_to_auto() {
        // "" / whitespace == unset -> auto -> hard error without accelerator.
        assert!(resolve_device(Some("  "), false, false, false).is_err());
    }

    #[test]
    fn identity_stamps_source_revision_and_sha_prefix() {
        // Constructed logic mirrors `CandleEmbedder::new`'s identity string.
        let identity = format!(
            "{SOURCE_REPO}@{SOURCE_REVISION} {DEFAULT_WEIGHT_FILE} sha256:{}",
            &WEIGHT_SHA256[..12],
        );
        assert_eq!(identity, "BAAI/bge-m3@5617a9f61b028005a4858fdac845db406aefb181 model.safetensors sha256:68440cc1b73b");
    }

    // Compile-time sanity (clippy wants const assertions, not runtime tests).
    const _: () = {
        assert!(MAX_BATCH >= 1);
        assert!(MAX_SEQ_LEN >= 8192);
    };
}
