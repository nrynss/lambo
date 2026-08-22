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
//! explicit `weights_dir`) NEVER touches the network — resolution is
//! cache-only ([`cache_get`]) and fails loudly naming the missing artifact
//! (K2-R1-3). Weights are never committed to the repo (standing rule).
//!
//! **Contract identity (K2 task 3).** The adapter stamps the loaded artifact —
//! the effective source revision plus the sha256 prefix of the weight file's
//! actual bytes, hashed on every path including explicit overrides (K2-R1-1) —
//! rather than the raw `llama_model` string, so a kind/dim match can no longer
//! hide a swap between two quantizations of the same model.
//!
//! **Batching (K1's central finding).** [`Embedder::embed`] is single-text and
//! async, but the K1 throughput legs showed the Metal/CUDA advantage comes from
//! *one forward per batch*, not from thread concurrency (the command queue
//! serializes). So this adapter coalesces concurrent `embed()` calls: each call
//! pushes `(text, oneshot)` onto a pending queue and awaits its result, and a
//! single supervised background thread drains the queue into batches of at most
//! [`MAX_BATCH`], runs one forward per batch inside `catch_unwind` (K2-R1-5),
//! and resolves every caller's oneshot. A short debounce bounds lone-call
//! latency (a solitary `embed()` is not starved waiting for a batch that never
//! fills), while a burst of concurrent calls shares one forward.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::Duration;

use async_trait::async_trait;
use candle_core::{DType, Device, IndexOp, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::xlm_roberta::{Config, XLMRobertaModel};
use tokenizers::{PaddingParams, PaddingStrategy, Tokenizer, TruncationParams};

use super::{EmbedError, Embedder};

/// Canonical upstream source of the weights (what the contract stamps for the
/// verified default artifact).
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
/// BGE-M3's architectural output width: XLM-RoBERTa-large's hidden size. Not
/// configurable — a configured `dim` that disagrees is refused at construction
/// (K2-R1-4), never silently served under the wrong contract.
const BGE_M3_DIM: usize = 1024;
/// How long the coalescer waits for extra callers to join an incomplete batch
/// before running the forward alone. Bounds lone-call latency (K1).
const BATCH_DEBOUNCE: Duration = Duration::from_millis(2);
/// Hard cap on one forward's batch width (bounds GPU memory + forward compute).
const MAX_BATCH: usize = 32;
/// Upper bound one `embed()` waits on its coalescer oneshot (K2-R1-5). This is
/// a deadlock guard, not a latency bound: with the supervised coalescer it can
/// only fire if nothing scheduled a forward for ten minutes — which is exactly
/// the wedge this bound exists to name instead of hanging forever.
const EMBED_WAIT: Duration = Duration::from_secs(600);

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

/// Queue of pending requests shared between the adapter handles and the
/// coalescer thread, plus the shutdown flag (K2-R1-7).
struct BatchQueue {
    inner: Mutex<Vec<Pending>>,
    wake: Condvar,
    /// Set when the last adapter handle dropped: finish what is queued, then
    /// exit so the model is freed with the handles.
    shutdown: Mutex<bool>,
}

impl BatchQueue {
    fn new() -> Self {
        Self {
            inner: Mutex::new(Vec::new()),
            wake: Condvar::new(),
            shutdown: Mutex::new(false),
        }
    }

    fn push(&self, pending: Pending) {
        let mut q = self.lock_queue();
        q.push(pending);
        self.wake.notify_all();
    }

    #[cfg(test)]
    fn len(&self) -> usize {
        self.lock_queue().len()
    }

    /// K2-R1-7: stop accepting work and wake the coalescer.
    fn request_shutdown(&self) {
        *self.lock_shutdown() = true;
        self.wake.notify_all();
    }

    fn shutdown_requested(&self) -> bool {
        *self.lock_shutdown()
    }

    /// Block until at least one request is queued, then take up to `max_batch`.
    ///
    /// K2-R1-2: the take is CLAMPED to `max_batch` — anything beyond the cap
    /// STAYS queued for the next iteration, it is never destroyed. Returns
    /// `None` only once shutdown was requested with an empty queue.
    fn wait_for_batch(&self, max_batch: usize) -> Option<Vec<Pending>> {
        let mut q = self.lock_queue();
        loop {
            if !q.is_empty() {
                let take = q.len().min(max_batch);
                return Some(q.drain(..take).collect());
            }
            if *self.lock_shutdown() {
                return None;
            }
            q = self.wake.wait(q).unwrap_or_else(unpoison);
        }
    }

    /// Debounce top-up during an open batch: extend by at most the remaining
    /// capacity. Leftovers stay queued (K2-R1-2: there is no `clear()`).
    fn top_up(&self, batch: &mut Vec<Pending>, max_batch: usize) {
        if batch.len() >= max_batch {
            return;
        }
        let mut q = self.lock_queue();
        let additional = q.len().min(max_batch - batch.len());
        batch.extend(q.drain(..additional));
    }

    /// Fail everything still queued at shutdown (their callers are awaiting).
    fn drain_all(&self) -> Vec<Pending> {
        std::mem::take(&mut *self.lock_queue())
    }

    fn lock_queue(&self) -> MutexGuard<'_, Vec<Pending>> {
        // K2-R1-5: never unwrap a poisoned lock — a panicked forward must not
        // wedge every future embed behind poison.
        self.inner.lock().unwrap_or_else(unpoison)
    }

    fn lock_shutdown(&self) -> MutexGuard<'_, bool> {
        self.shutdown.lock().unwrap_or_else(unpoison)
    }
}

fn unpoison<T>(poisoned: std::sync::PoisonError<T>) -> T {
    poisoned.into_inner()
}

/// State owned jointly by all adapter handles and the coalescer thread. The
/// model goes with the coalescer's last `Arc<Shared>` drop.
struct Shared {
    queue: BatchQueue,
    core: BgeM3Core,
    /// The width the contract pins; forward output is checked against it
    /// before any vector reaches a caller (K2-R1-4 defense in depth).
    dim: usize,
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
                        .map_err(|e| no_accelerator("Metal", e))
                } else {
                    Err(no_accelerator("Metal", "no Metal backend compiled"))
                }
            } else if has_cuda {
                Device::new_cuda(0)
                    .map(|d| DeviceChoice {
                        device: d,
                        dtype: DType::F16,
                    })
                    .map_err(|e| no_accelerator("CUDA", e))
            } else {
                Err(no_accelerator("CUDA", "no CUDA backend compiled"))
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

/// K2-R1-9: name the accelerator that actually failed. The old signature
/// derived the name from `(has_cuda, macos)` flags its callers mis-fed, so a
/// genuine `Device::new_metal`/`new_cuda` failure printed "an accelerator".
fn no_accelerator(accelerator: &str, err: impl std::fmt::Display) -> EmbedError {
    EmbedError::Unavailable(format!(
        "{accelerator} is unavailable ({err}) and device was not pinned to \"cpu\": the candle \
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

/// Compile-time accelerator facts (cfg-gated so the plain build stays clean).
fn has_metal_backend() -> bool {
    cfg!(all(target_os = "macos", feature = "embed-candle-metal"))
}

fn has_cuda_backend() -> bool {
    cfg!(all(not(target_os = "macos"), feature = "embed-candle-cuda"))
}

fn is_macos() -> bool {
    cfg!(target_os = "macos")
}

/// The in-process BGE-M3 embedder. Cheap to clone; the model + coalescer are
/// shared behind an `Arc`.
#[derive(Clone)]
pub struct CandleEmbedder {
    dim: usize,
    identity: String,
    /// Shutdown ownership: `Handle::drop` fires only when the LAST adapter
    /// handle is gone (K2-R1-7), so dropping one clone never kills the
    /// coalescer the other clones still serve.
    handle: Arc<Handle>,
}

struct Handle {
    shared: Arc<Shared>,
}

impl Drop for Handle {
    fn drop(&mut self) {
        self.shared.queue.request_shutdown();
    }
}

impl CandleEmbedder {
    /// Load weights (fetching/caching via hf-hub on first use), resolve the
    /// device, and spawn the batch-coalescing thread.
    ///
    /// Fails with [`EmbedError::Unavailable`] when weights are absent/offline
    /// or the configured `dim` disagrees with BGE-M3's architectural width, and
    /// (per the K device rule) when no accelerator is available and the
    /// operator did not pin `device = "cpu"`.
    pub fn new(dim: usize, opts: CandleOpts) -> Result<Self, EmbedError> {
        if dim == 0 {
            return Err(EmbedError::Unavailable("embedder dim must be > 0".into()));
        }
        // K2-R1-4: BGE-M3 emits a fixed-width dense vector — the width check
        // downstream cannot catch a wrong config because the stored vectors all
        // share it, so a wrong `dim` would resolve and stamp a lying contract.
        // Refuse here like the bge_m3 sibling refuses at response time.
        if dim != BGE_M3_DIM {
            return Err(EmbedError::Unavailable(format!(
                "candle BGE-M3 emits fixed {BGE_M3_DIM}-dim embeddings but dim = {dim} is \
                 configured — refusing to resolve: stored vectors would disagree with the \
                 stamped contract width"
            )));
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

        // Resolve device + dtype BEFORE loading (f16 on GPU, f32 on CPU).
        let DeviceChoice { device, dtype } = resolve_device(
            opts.device.as_deref(),
            has_metal_backend(),
            has_cuda_backend(),
            is_macos(),
        )?;

        let weights_path = if let Some(dir) = &opts.weights_dir {
            // Explicit local dir: use it directly, never the network.
            dir.join(&weight_file)
        } else if opts.offline {
            // K2-R1-3: cache-only lookup — never dials the network.
            cache_get(&repo, &revision, &weight_file)?
        } else {
            hub_get(&repo, &revision, &weight_file)?
        };

        // config.json + tokenizer.json come from the same repo as the weights.
        let config_path = match &opts.weights_dir {
            Some(dir) => dir.join("config.json"),
            None if opts.offline => cache_get(&repo, &revision, "config.json")?,
            None => hub_get(&repo, &revision, "config.json")?,
        };
        let tokenizer_path = match &opts.weights_dir {
            Some(dir) => dir.join("tokenizer.json"),
            None if opts.offline => cache_get(&repo, &revision, "tokenizer.json")?,
            None => hub_get(&repo, &revision, "tokenizer.json")?,
        };

        // K2-R1-8: hash BEFORE any model work — refuse tampered/corrupt bytes
        // before they influence anything. K2-R1-1: this digest is also what the
        // contract stamps, on EVERY path including explicit overrides.
        let weight_sha256 = sha256_file(&weights_path)?;
        // The shipped artifact is machine-checked against the pinned digest;
        // any override (repo/revision/file/dir) is stamped from its own bytes.
        let is_default_artifact = opts.weights_dir.is_none()
            && repo == WEIGHT_REPO
            && revision == WEIGHT_REVISION
            && weight_file == DEFAULT_WEIGHT_FILE;
        if is_default_artifact && weight_sha256 != WEIGHT_SHA256 {
            return Err(EmbedError::Backend(format!(
                "weight file {} sha256 mismatch: expected {WEIGHT_SHA256}, got {weight_sha256} — \
                 refusing to serve unverified weights (same-artifact rule)",
                weights_path.display()
            )));
        }

        let core = load_core(&config_path, &tokenizer_path, &weights_path, &device, dtype)?;

        let shared = Arc::new(Shared {
            queue: BatchQueue::new(),
            core,
            dim,
        });
        spawn_coalescer(Arc::clone(&shared));

        // K2-R1-1: the stamp ALWAYS describes the artifact actually loaded —
        // canonical source for the verified default, effective source plus the
        // computed digest for every override.
        let identity = if is_default_artifact {
            stamp_identity(
                SOURCE_REPO,
                SOURCE_REVISION,
                DEFAULT_WEIGHT_FILE,
                None,
                &weight_sha256,
            )
        } else {
            stamp_identity(
                &repo,
                &revision,
                &weight_file,
                opts.weights_dir.as_deref(),
                &weight_sha256,
            )
        };

        Ok(Self {
            dim,
            identity,
            handle: Arc::new(Handle { shared }),
        })
    }

    /// The served-artifact identity string to stamp into the session's
    /// `EmbeddingContract.model` (K2 task 3): the loaded artifact's source and
    /// sha256 prefix, so a kind/dim match cannot hide a swap between two
    /// quantizations of the same model.
    pub fn model_identity(&self) -> &str {
        &self.identity
    }
}

/// Fetch `filename` via hf-hub (cache hit, or network download on first use).
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

/// Cache-only hf-hub lookup: NEVER dials the network (K2-R1-3). Fails loudly
/// naming the missing artifact, telling the operator how to fix it.
fn cache_get(repo: &str, revision: &str, filename: &str) -> Result<PathBuf, EmbedError> {
    hf_hub::Cache::from_env()
        .repo(hf_hub::Repo::with_revision(
            repo.to_string(),
            hf_hub::RepoType::Model,
            revision.to_string(),
        ))
        .get(filename)
        .ok_or_else(|| {
            EmbedError::Unavailable(format!(
                "offline: {filename} not cached for {repo}@{revision} (fetch once online, then \
                 go offline)"
            ))
        })
}

/// sha256 of a file's bytes, streamed (K2-R1-8: computed BEFORE the model is
/// built, so an unverified artifact never influences anything).
fn sha256_file(path: &Path) -> Result<String, EmbedError> {
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
    Ok(format!("{:x}", hasher.finalize()))
}

use sha2::{Digest, Sha256};

/// Build the contract-stamp identity from the RESOLVED artifact facts.
///
/// K2-R1-1: `sha256_hex` must be the digest of the bytes actually hashed —
/// never a compile-time constant for an artifact whose bytes were not verified.
/// For the shipped default the caller passes the canonical
/// `SOURCE_REPO`/`SOURCE_REVISION` AFTER verifying the digest equals the pinned
/// `WEIGHT_SHA256`; for every override the effective source and the computed
/// digest go in directly.
fn stamp_identity(
    repo: &str,
    revision: &str,
    weight_file: &str,
    weights_dir: Option<&Path>,
    sha256_hex: &str,
) -> String {
    let source = match weights_dir {
        Some(dir) => format!("dir:{}", dir.display()),
        None => format!("{repo}@{revision}"),
    };
    format!("{source} {weight_file} sha256:{}", &sha256_hex[..12])
}

/// Spawn the SUPERVISED batch-coalescing thread (K2-R1-5): the worker's
/// forward runs inside `catch_unwind` and the loop survives poisoned locks, so
/// a worker exit should be impossible — if one ever dies anyway, the supervisor
/// resurrects it instead of wedging every future `embed()` forever.
fn spawn_coalescer(shared: Arc<Shared>) {
    std::thread::Builder::new()
        .name("lambo-candle-coalescer".into())
        .spawn(move || supervise(shared))
        .expect("spawn candle coalescer");
}

fn supervise(shared: Arc<Shared>) {
    while !shared.queue.shutdown_requested() {
        let worker_shared = Arc::clone(&shared);
        let worker = std::thread::Builder::new()
            .name("lambo-candle-coalescer-worker".into())
            .spawn(move || coalesce_loop(worker_shared))
            .expect("spawn candle coalescer worker");
        // A healthy worker returns only on shutdown.
        let _ = worker.join();
        if shared.queue.shutdown_requested() {
            break;
        }
        eprintln!("lambo: candle coalescer worker died unexpectedly; restarting it");
    }
}

fn coalesce_loop(shared: Arc<Shared>) {
    loop {
        let Some(mut batch) = shared.queue.wait_for_batch(MAX_BATCH) else {
            // K2-R1-7: shutdown with the queue drained — exit so the model is
            // freed with the handles. Fail anything that raced in.
            for item in shared.queue.drain_all() {
                let _ = item.tx.send(Err(EmbedError::Unavailable(
                    "candle embedder dropped while the request was queued".into(),
                )));
            }
            return;
        };

        // Debounce: allow a burst of concurrent callers to land in the same
        // batch before we run the forward. A lone caller waits at most one
        // debounce, bounding lone-call latency.
        if batch.len() < MAX_BATCH {
            std::thread::sleep(shared_debounce());
            shared.queue.top_up(&mut batch, MAX_BATCH);
        }

        let texts: Vec<&str> = batch.iter().map(|p| p.text.as_str()).collect();
        let batch_len = batch.len();
        // K2-R1-5: a candle panic fails THIS batch and the loop goes on.
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| shared.core.forward(&texts)));
        match result {
            Ok(Ok(vectors)) => {
                debug_assert_eq!(vectors.len(), batch.len());
                // K2-R1-4 defense in depth: never hand a caller a vector whose
                // width disagrees with the stamped contract.
                if vectors.iter().any(|v| v.len() != shared.dim) {
                    let got = vectors.first().map(Vec::len).unwrap_or(0);
                    fail_batch(
                        batch,
                        &format!(
                            "candle returned {got}-wide vectors but the contract pins {} — \
                             refusing to serve width-mismatched embeddings",
                            shared.dim
                        ),
                    );
                } else {
                    for (item, vector) in batch.into_iter().zip(vectors) {
                        let _ = item.tx.send(Ok(vector));
                    }
                }
            }
            Ok(Err(e)) => fail_batch(
                batch,
                &format!("candle forward failed for a batch of {batch_len} texts: {e}"),
            ),
            Err(panic) => fail_batch(
                batch,
                &format!(
                    "candle coalescer panicked running a batch of {} texts: {}; the worker \
                     keeps serving",
                    batch_len,
                    panic_message(panic.as_ref())
                ),
            ),
        }
    }
}

/// Debounce is a constant today; kept as a fn so the loop reads against the
/// shared state it actually uses.
fn shared_debounce() -> Duration {
    BATCH_DEBOUNCE
}

fn fail_batch(batch: Vec<Pending>, msg: &str) {
    for item in batch {
        let _ = item.tx.send(Err(EmbedError::Backend(msg.to_string())));
    }
}

fn panic_message(panic: &(dyn std::any::Any + Send)) -> String {
    panic
        .downcast_ref::<&str>()
        .map(|s| (*s).to_string())
        .or_else(|| panic.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "unknown panic".to_string())
}

/// Bounded wait on the coalescer oneshot (K2-R1-5): a dead worker produces a
/// named error, never an indefinite hang.
async fn recv_bounded(
    rx: tokio::sync::oneshot::Receiver<Result<Vec<f32>, EmbedError>>,
    limit: Duration,
) -> Result<Vec<f32>, EmbedError> {
    match tokio::time::timeout(limit, rx).await {
        Ok(Ok(result)) => result,
        // Sender dropped without sending: the batch died with a dead worker.
        Ok(Err(_)) => Err(EmbedError::Backend(
            "candle coalescer dropped the request (worker exited mid-batch)".into(),
        )),
        Err(_) => Err(EmbedError::Unavailable(format!(
            "candle embed did not complete within {} s — the coalescer appears wedged; \
             retry or restart",
            limit.as_secs()
        ))),
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
        self.handle.shared.queue.push(Pending {
            text: text.to_string(),
            tx,
        });
        recv_bounded(rx, EMBED_WAIT).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fail_batch_resolves_every_caller_even_after_a_panic() {
        // K2-R1-5: the panic path fails each oneshot with the panic named.
        let mut rxs = Vec::new();
        let batch: Vec<Pending> = (0..3)
            .map(|_| {
                let (tx, rx) = tokio::sync::oneshot::channel();
                rxs.push(rx);
                Pending {
                    text: "x".into(),
                    tx,
                }
            })
            .collect();
        fail_batch(
            batch,
            "candle coalescer panicked running a batch of 3 texts: bad tensor; the worker \
             keeps serving",
        );
        for rx in rxs {
            let err = rx.await.unwrap().unwrap_err().to_string();
            assert!(
                err.contains("panicked") && err.contains("bad tensor"),
                "{err}"
            );
        }
    }

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
    fn auto_names_the_missing_accelerator() {
        // K2-R1-9: the failure names Metal/CUDA, never "an accelerator".
        let e = resolve_device(None, false, false, false)
            .unwrap_err()
            .to_string();
        assert!(e.contains("CUDA is unavailable"), "{e}");

        // The runtime-failure arm (feature compiled, device creation failed)
        // names the accelerator too; exercised directly so the test stays
        // host-independent.
        let direct = no_accelerator("Metal", "stub failure").to_string();
        assert!(
            direct.contains("Metal is unavailable (stub failure)"),
            "{direct}"
        );
    }

    #[test]
    fn empty_string_device_falls_back_to_auto() {
        // "" / whitespace == unset -> auto -> hard error without accelerator.
        assert!(resolve_device(Some("  "), false, false, false).is_err());
    }

    #[test]
    fn identity_stamps_source_revision_and_sha_prefix() {
        // Default artifact: canonical source + VERIFIED pinned digest.
        let identity = stamp_identity(
            SOURCE_REPO,
            SOURCE_REVISION,
            DEFAULT_WEIGHT_FILE,
            None,
            WEIGHT_SHA256,
        );
        assert_eq!(identity, "BAAI/bge-m3@5617a9f61b028005a4858fdac845db406aefb181 model.safetensors sha256:68440cc1b73b");
    }

    #[test]
    fn override_identity_describes_the_loaded_artifact_not_the_default_stamp() {
        // K2-R1-1: a non-default source stamps ITS OWN bytes' digest — never
        // the default f16 sha256 prefix that made a quantization swap invisible.
        let identity = stamp_identity(
            "BAAI/bge-m3",
            "main",
            "pytorch_model.bin",
            None,
            "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff",
        );
        assert!(!identity.contains(&WEIGHT_SHA256[..12]), "{identity}");
        assert!(identity.contains("sha256:001122334455"), "{identity}");
        assert!(
            identity.contains("BAAI/bge-m3@main pytorch_model.bin"),
            "{identity}"
        );

        // An explicit weights_dir names the directory it loaded from.
        let local = stamp_identity(
            WEIGHT_REPO,
            WEIGHT_REVISION,
            DEFAULT_WEIGHT_FILE,
            Some(Path::new("/srv/lambo/bge-m3")),
            WEIGHT_SHA256,
        );
        assert!(local.starts_with("dir:/srv/lambo/bge-m3 "), "{local}");
        assert!(
            local.contains("model.safetensors sha256:68440cc1b73b"),
            "{local}"
        );
    }

    #[test]
    fn sha256_file_hashes_the_bytes_actually_on_disk() {
        // K2-R1-1/R1-8 foundation: the digest comes from the file, not a const.
        let dir =
            std::env::temp_dir().join(format!("lambo-candle-sha-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("artifact.bin");
        std::fs::write(&path, b"abc").unwrap();
        let got = sha256_file(&path).unwrap();
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn wrong_dim_is_refused_at_construction_before_any_resolution() {
        // K2-R1-4: dim=768 must hard-error BEFORE device/weights resolution
        // (no network, no model) — the old code resolved and stamped it.
        let err = match CandleEmbedder::new(
            768,
            CandleOpts {
                device: Some("cpu".into()),
                offline: true,
                ..Default::default()
            },
        ) {
            Err(e) => e,
            Ok(_) => panic!("dim=768 must be refused at construction"),
        };
        let msg = err.to_string();
        assert!(msg.contains("1024"), "{msg}");
        assert!(msg.contains("dim = 768"), "{msg}");
    }

    #[test]
    fn initial_take_never_exceeds_max_batch_and_leftovers_survive() {
        // K2-R1-2: 50 concurrent callers against MAX_BATCH=32 — the old code
        // mem::take'd all 50 onto ONE forward, destroyed the 19 beyond the cap
        // with q.clear(), and every one of them failed. Now the take is clamped
        // and every survivor keeps its sender alive.
        let q = BatchQueue::new();
        let mut rxs = Vec::new();
        for i in 0..50 {
            let (tx, rx) = tokio::sync::oneshot::channel();
            q.push(Pending {
                text: format!("t{i}"),
                tx,
            });
            rxs.push(rx);
        }
        let first = q.wait_for_batch(MAX_BATCH).expect("first batch");
        assert_eq!(first.len(), MAX_BATCH, "initial take must respect the cap");
        assert_eq!(
            q.len(),
            50 - MAX_BATCH,
            "leftovers stay queued, never cleared"
        );

        let second = q.wait_for_batch(MAX_BATCH).expect("second batch");
        assert_eq!(second.len(), 50 - MAX_BATCH);

        // Not one queued request lost its sender (old code: 19 Disconnected).
        for mut rx in rxs {
            assert!(
                !matches!(
                    rx.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed)
                ),
                "a queued request was dropped"
            );
        }
    }

    #[test]
    fn debounce_top_up_takes_only_the_remaining_capacity() {
        // K2-R1-2, second half: during debounce the drain is bounded by the
        // batch's remaining capacity and leftovers stay queued.
        let q = BatchQueue::new();
        let mut rxs = Vec::new();
        for i in 0..10 {
            let (tx, _) = tokio::sync::oneshot::channel();
            q.push(Pending {
                text: format!("a{i}"),
                tx,
            });
        }
        let mut batch = q.wait_for_batch(MAX_BATCH).unwrap();
        assert_eq!(batch.len(), 10);
        for i in 0..30 {
            let (tx, rx) = tokio::sync::oneshot::channel();
            q.push(Pending {
                text: format!("b{i}"),
                tx,
            });
            rxs.push(rx);
        }
        q.top_up(&mut batch, MAX_BATCH);
        assert_eq!(batch.len(), MAX_BATCH, "top-up stops at capacity");
        assert_eq!(q.len(), 8, "beyond-capacity requests remain queued");
        for mut rx in rxs {
            assert!(
                !matches!(
                    rx.try_recv(),
                    Err(tokio::sync::oneshot::error::TryRecvError::Closed)
                ),
                "a queued request was dropped by the top-up"
            );
        }
    }

    #[test]
    fn shutdown_exits_only_after_draining_queued_work() {
        // K2-R1-7: the wait loop returns None (thread exits) once shutdown is
        // requested AND the queue is drained — not while work remains.
        let q = BatchQueue::new();
        q.request_shutdown();
        assert!(
            q.wait_for_batch(MAX_BATCH).is_none(),
            "empty + shutdown exits"
        );

        let q = BatchQueue::new();
        for i in 0..2 {
            let (tx, _) = tokio::sync::oneshot::channel();
            q.push(Pending {
                text: format!("t{i}"),
                tx,
            });
        }
        q.request_shutdown();
        let batch = q
            .wait_for_batch(MAX_BATCH)
            .expect("queued work drains first");
        assert_eq!(batch.len(), 2);
        assert!(q.wait_for_batch(MAX_BATCH).is_none(), "then the loop exits");
    }

    #[tokio::test]
    async fn bounded_wait_reports_a_dropped_sender_as_a_named_error() {
        // K2-R1-5: a dead worker drops the sender; embed() surfaces a named
        // Backend error instead of hanging on rx.await forever.
        let (tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<f32>, EmbedError>>();
        drop(tx);
        let err = recv_bounded(rx, Duration::from_secs(1)).await.unwrap_err();
        assert!(err.to_string().contains("dropped the request"), "{err}");
    }

    #[tokio::test]
    async fn bounded_wait_times_out_instead_of_hanging_forever() {
        let (_tx, rx) = tokio::sync::oneshot::channel::<Result<Vec<f32>, EmbedError>>();
        let err = recv_bounded(rx, Duration::from_millis(20))
            .await
            .unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
        assert!(err.to_string().contains("did not complete within"), "{err}");
    }

    #[test]
    fn offline_weight_resolution_is_cache_only_and_loud() {
        // K2-R1-3: offline resolution consults ONLY the local hub cache. A miss
        // fails instantly naming the artifact — the old code called Api::get,
        // which DOWNLOADS on a miss (silent network on an online rig, a raw
        // fetch error on an airgapped one).
        let err =
            cache_get("lambo-ci/no-such-repo-k2r13", "main", "model.safetensors").unwrap_err();
        assert!(matches!(err, EmbedError::Unavailable(_)), "{err:?}");
        let msg = err.to_string();
        assert!(msg.contains("not cached"), "{msg}");
        assert!(msg.contains("model.safetensors"), "{msg}");
        assert!(msg.contains("fetch once online"), "{msg}");
    }

    /// LIVE test: loads the real published f16 weights via hf-hub (a ~1.1 GB
    /// download on first run, then cached) and embeds two texts on CPU.
    ///
    /// Ignored by default — CI's candle row is deliberately weightless and
    /// offline. Run explicitly with:
    /// `cargo test --features embed-candle -- --ignored live_weights`.
    #[ignore]
    #[tokio::test]
    async fn live_weights_load_and_embed_on_cpu() {
        let embedder = CandleEmbedder::new(
            BGE_M3_DIM,
            CandleOpts {
                device: Some("cpu".into()),
                ..Default::default()
            },
        )
        .expect("live weights must load and verify");
        let near = embedder.embed("user schema").await.expect("embed");
        let far = embedder
            .embed("the mitochondria is the powerhouse of the cell")
            .await
            .expect("embed");
        assert_eq!(near.len(), BGE_M3_DIM);
        assert_eq!(far.len(), BGE_M3_DIM);
        assert!(
            near.iter()
                .zip(&far)
                .filter(|(a, b)| (**a - **b).abs() > 1e-3)
                .count()
                > BGE_M3_DIM / 2,
        );
    }

    // Compile-time sanity (clippy wants const assertions, not runtime tests).
    const _: () = {
        assert!(MAX_BATCH >= 1);
        assert!(MAX_SEQ_LEN >= 8192);
        assert!(BGE_M3_DIM == 1024);
    };
}
