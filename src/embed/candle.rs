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
//! **Identity before load (issue #31).** Construction is two steps.
//! [`CandleEmbedder::deferred`], which `build_embedder` (so every resolve)
//! uses, resolves the device, the artifact, its sha256 (cached on disk by file
//! metadata so an unchanged 1.1 GB file is not re-hashed every start) and the
//! width from `config.json`, which is all the resolve-time checks and the
//! contract need. [`CandleEmbedder::load`] puts the weights on the device; a
//! `lambo serve` runs it through [`Embedder::prepare`] only once it has won
//! the session's lease, so a serve that proxies never holds the model.
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
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
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
    /// `None`/`auto` | `cpu` | `metal` | `cuda`. See module docs on device
    /// resolution; `resolve_device` is the authority on the accepted set.
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

/// Load the tokenizer + model from the parsed `config.json` and the on-disk
/// `tokenizer.json` and weight file. Shared by the hf-hub and explicit-dir
/// fetch paths; run by [`CandleEmbedder::load`] only (issue #31).
///
/// The tokenizer is parsed and configured FIRST (#31 review L3): it is a few
/// MB of JSON against ~1.1 GB of weights, so a corrupt `tokenizer.json` is
/// refused before anything reaches the device.
///
/// Error classes (#31 review L2, M-A): a tokenizer that does not parse, or a
/// weight file of an unsupported kind, is [`EmbedError::Backend`] (permanent:
/// the same artifact fails the same way). A candle error from mapping the
/// weights or building the model is classed by [`candle_load_error`]: I/O and
/// device (CUDA, Metal) failures are [`EmbedError::Unavailable`], since they
/// come from memory pressure or a busy device and can succeed later, so a
/// pinned session's background retry keeps trying; a malformed safetensors
/// file, a missing tensor or a shape or dtype mismatch is the artifact's
/// fault and is [`EmbedError::Backend`], never retried.
fn load_core(
    cfg: &Config,
    tokenizer_path: &Path,
    weights_path: &Path,
    device: &Device,
    dtype: DType,
) -> Result<BgeM3Core, EmbedError> {
    let tokenizer = load_tokenizer(cfg, tokenizer_path)?;
    let vb = load_varbuilder(weights_path, dtype, device)?;
    let model =
        XLMRobertaModel::new(cfg, vb).map_err(|e| candle_load_error("candle model load", &e))?;
    Ok(BgeM3Core {
        model,
        tokenizer,
        device: device.clone(),
    })
}

/// Parse `tokenizer.json` and set BGE-M3's truncation and padding.
fn load_tokenizer(cfg: &Config, tokenizer_path: &Path) -> Result<Tokenizer, EmbedError> {
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
    Ok(tokenizer)
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
                .map_err(|e| candle_load_error("safetensors", &e))?
        },
        "bin" | "pth" | "pt" => VarBuilder::from_pth(weights, dtype, device)
            .map_err(|e| candle_load_error("pytorch_model.bin", &e))?,
        other => {
            return Err(EmbedError::Backend(format!(
                "unsupported weight extension: {other:?}"
            )))
        }
    };
    Ok(vb)
}

/// #31 review M-A: class a candle error from the device load. Transient
/// ([`EmbedError::Unavailable`], retried): I/O (an mmap or read that failed
/// for lack of memory or a busy disk) and the device backends (CUDA, Metal:
/// allocation, a busy or lost device). Everything else is the artifact's
/// fault and permanent ([`EmbedError::Backend`]): a safetensors file whose
/// header or offsets do not parse (truncated, corrupt), a tensor the model
/// needs that is missing, a shape or dtype that does not match the config.
/// Context wrappers are looked through to the cause.
fn candle_load_error(what: &str, err: &candle_core::Error) -> EmbedError {
    if candle_error_is_transient(err) {
        EmbedError::Unavailable(format!("{what}: {err}"))
    } else {
        EmbedError::Backend(format!("{what}: {err}"))
    }
}

fn candle_error_is_transient(err: &candle_core::Error) -> bool {
    use candle_core::Error as E;
    match err {
        E::Io(_) | E::Cuda(_) | E::Metal(_) => true,
        E::WithPath { inner, .. } | E::WithBacktrace { inner, .. } | E::Context { inner, .. } => {
            candle_error_is_transient(inner)
        }
        _ => false,
    }
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

/// Everything the device load needs, resolved and verified at construction
/// (issue #31): the parsed `config.json`, the artifact paths, and the device.
///
/// Built by [`CandleEmbedder::deferred`] without touching the weights beyond
/// hashing them (or reading the cached digest), so a serve that loses the
/// election never pays the ~1.1 GB device load.
struct LoadPlan {
    config: Config,
    tokenizer_path: PathBuf,
    weights_path: PathBuf,
    device: Device,
    dtype: DType,
    /// Each artifact file as construction read it (#31 review M2): the
    /// weights (fingerprint taken before they were hashed for the identity),
    /// `config.json` and `tokenizer.json`, each with its sha256.
    /// [`CandleEmbedder::load`] checks them before and after the device load,
    /// so the bytes it loads are the bytes the stamped identity describes,
    /// even when the load runs hours after the resolve (a pinned retry, an
    /// on-demand attach). Empty for a test embedder built already loaded.
    artifacts: Vec<PinnedFile>,
    /// Test hook run between the pre-load check and the device load, to
    /// change the artifact in exactly that window (#31 review M-C).
    #[cfg(test)]
    before_map: Mutex<Option<Box<dyn FnOnce() + Send>>>,
}

/// One artifact file as [`CandleEmbedder::deferred`] identified it.
struct PinnedFile {
    path: PathBuf,
    fingerprint: Fingerprint,
    sha256: String,
}

impl PinnedFile {
    /// Read `path`'s fingerprint and digest, through the same digest cache
    /// as the weights (BGE-M3's `tokenizer.json` is ~17 MB, which a proxy
    /// would otherwise hash on every start).
    fn identify(path: &Path, cache_dir: Option<&Path>) -> Result<Self, EmbedError> {
        let (sha256, fingerprint) = weight_sha256(path, cache_dir)?;
        Ok(Self {
            path: path.to_path_buf(),
            sha256,
            fingerprint,
        })
    }

    /// #31 review M2, M-B: the file still holds the bytes construction
    /// identified. The fingerprint is the fast path; when it differs (a new
    /// mtime, but also a chmod, an xattr, an extra hard link or a backup tool
    /// that only moves ctime) the bytes are hashed again and only a different
    /// digest refuses. A changed artifact is permanent for this process (its
    /// identity was stamped at resolve), so that error is
    /// [`EmbedError::Backend`], naming what changed.
    fn check_unchanged(&self) -> Result<(), EmbedError> {
        let now = fingerprint_or_backend(&self.path)?;
        if now == self.fingerprint {
            return Ok(());
        }
        let changed = self.fingerprint.changed_fields(&now).join(", ");
        let digest = sha256_file(&self.path)?;
        if digest == self.sha256 {
            tracing::debug!(
                target: "lambo::embed",
                file = %self.path.display(),
                changed = %changed,
                "candle: artifact metadata changed since resolve but its bytes did not"
            );
            return Ok(());
        }
        Err(EmbedError::Backend(format!(
            "candle artifact {} changed after this process resolved its identity ({changed} \
             changed, and its sha256 is now {} where the resolve read {}); refusing to load \
             bytes the stamped embedding contract does not describe. Restart the process to \
             resolve the new artifact",
            self.path.display(),
            &digest[..12],
            &self.sha256[..12]
        )))
    }
}

impl LoadPlan {
    fn check_unchanged(&self) -> Result<(), EmbedError> {
        self.artifacts
            .iter()
            .try_for_each(PinnedFile::check_unchanged)
    }
}

/// The device-loaded half of a [`CandleEmbedder`], shared by every clone.
///
/// `handle` is set at most once, by [`CandleEmbedder::load`], under `init`;
/// reads after that are lock-free. The model and the coalescer go with the
/// last clone's drop of this slot (`Handle::drop`, K2-R1-7), loaded or not.
struct LoadSlot {
    handle: OnceLock<Handle>,
    init: Mutex<()>,
    /// Device loads this slot has run: 0 until the holder path loads, then 1
    /// for the life of the slot. Read by tests and the #31 measurement.
    loads: AtomicUsize,
}

impl LoadSlot {
    fn empty() -> Self {
        Self {
            handle: OnceLock::new(),
            init: Mutex::new(()),
            loads: AtomicUsize::new(0),
        }
    }
}

/// The in-process BGE-M3 embedder. Cheap to clone; the model + coalescer are
/// shared behind an `Arc`.
///
/// **Identity and load are two steps (issue #31).** [`Self::deferred`] (what
/// `build_embedder` uses, so what every resolve builds) computes everything
/// the resolve-time checks and the [`crate::types::EmbeddingContract`] need:
/// the device, the artifact identity (source + weight sha256) and the width
/// (`config.json`'s `hidden_size`). The weights reach the device only in
/// [`Self::load`], which the holder path runs through
/// [`Embedder::prepare`] once its lease is won. [`Self::new`] is both steps.
#[derive(Clone)]
pub struct CandleEmbedder {
    dim: usize,
    identity: String,
    /// The weights live (or, deferred, will live) in Metal buffers: unified
    /// memory the macOS pager can compress while the process idles (issue
    /// #13). Known before the load because the device is resolved first.
    /// Drives the keep-warm auto default through
    /// [`crate::embed::weights_in_unified_memory`].
    on_metal: bool,
    plan: Arc<LoadPlan>,
    /// Shutdown ownership: `Handle::drop` fires only when the LAST adapter
    /// handle is gone (K2-R1-7), so dropping one clone never kills the
    /// coalescer the other clones still serve.
    slot: Arc<LoadSlot>,
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
    /// device, and spawn the batch-coalescing thread: [`Self::deferred`]
    /// followed by [`Self::load`].
    ///
    /// Fails with [`EmbedError::Unavailable`] when weights are absent/offline
    /// or the configured `dim` disagrees with BGE-M3's architectural width, and
    /// (per the K device rule) when no accelerator is available and the
    /// operator did not pin `device = "cpu"`.
    pub fn new(dim: usize, opts: CandleOpts) -> Result<Self, EmbedError> {
        let embedder = Self::deferred(dim, opts)?;
        embedder.load()?;
        Ok(embedder)
    }

    /// Everything [`Self::new`] does except put the weights on the device
    /// (issue #31).
    ///
    /// Resolves the device (the K hard-error rule still fires here), the
    /// artifact paths, the weight sha256 (served from a metadata-keyed cache
    /// when the file is unchanged, see [`weight_sha256`]) and the width from
    /// `config.json`. Every refusal [`Self::new`] makes before its load is
    /// made here: zero or non-1024 `dim`, a missing artifact, an unverified
    /// default artifact, and (new) a `config.json` whose `hidden_size` is not
    /// the configured width. The tokenizer is parsed by [`Self::load`], before
    /// the weights; every artifact file's fingerprint is recorded here and
    /// checked again by the load.
    pub fn deferred(dim: usize, opts: CandleOpts) -> Result<Self, EmbedError> {
        Self::deferred_in(dim, opts, default_identity_cache_dir())
    }

    /// [`Self::deferred`] with the weight-digest cache in `cache_dir` (`None`:
    /// no cache, hash every time). Crate-internal so [`CandleOpts`] keeps the
    /// public shape it had in 0.4.0 (#31 review L5); tests point it at a
    /// scratch directory.
    pub(crate) fn deferred_in(
        dim: usize,
        opts: CandleOpts,
        cache_dir: Option<PathBuf>,
    ) -> Result<Self, EmbedError> {
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

        // Resolve device + dtype BEFORE anything else (f16 on GPU, f32 on CPU).
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
        // contract stamps, on EVERY path including explicit overrides. #31: an
        // unchanged file's digest comes from the identity cache.
        let (weight_sha256, weights_fingerprint) =
            weight_sha256(&weights_path, cache_dir.as_deref())?;
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

        // #31: the width the resolve checks against the store comes from the
        // artifact's own config, not from a loaded model. `hidden_size` is the
        // CLS vector's width, so it is exactly what the forward will emit.
        let config_file = PinnedFile::identify(&config_path, cache_dir.as_deref())?;
        let config = read_config(&config_path)?;
        check_hidden_size(&config, dim, &config_path)?;
        let tokenizer_file = PinnedFile::identify(&tokenizer_path, cache_dir.as_deref())?;

        // K2-R1-1: the stamp ALWAYS describes the artifact that will be loaded —
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
            on_metal: device.is_metal(),
            plan: Arc::new(LoadPlan {
                artifacts: vec![
                    PinnedFile {
                        path: weights_path.clone(),
                        fingerprint: weights_fingerprint,
                        sha256: weight_sha256.clone(),
                    },
                    config_file,
                    tokenizer_file,
                ],
                #[cfg(test)]
                before_map: Mutex::new(None),
                config,
                tokenizer_path,
                weights_path,
                device,
                dtype,
            }),
            slot: Arc::new(LoadSlot::empty()),
        })
    }

    /// Put the weights on the device and start the coalescer (issue #31).
    ///
    /// Blocking (it reads ~1.1 GB on the shipped artifact) and idempotent:
    /// the first call loads, concurrent callers wait for it, later calls
    /// return at once. Every clone shares the load. A failed load leaves the
    /// embedder unloaded, so a later call retries it.
    ///
    /// The serve holder path does not call this directly: it awaits
    /// [`Embedder::prepare`], which runs this on a blocking thread.
    pub fn load(&self) -> Result<(), EmbedError> {
        if self.is_loaded() {
            return Ok(());
        }
        let _init = self.slot.init.lock().unwrap_or_else(unpoison);
        if self.is_loaded() {
            return Ok(());
        }
        let plan = &self.plan;
        plan.check_unchanged()?;
        #[cfg(test)]
        if let Some(hook) = plan.before_map.lock().unwrap_or_else(unpoison).take() {
            hook();
        }
        let core = load_core(
            &plan.config,
            &plan.tokenizer_path,
            &plan.weights_path,
            &plan.device,
            plan.dtype,
        )?;
        // #31 review M-C: mapping the weights takes seconds, and a file
        // swapped while it ran would put unverified bytes on the device.
        // Check again now that the load has read them; on a mismatch the
        // model is dropped here, before anything can embed with it.
        plan.check_unchanged()?;
        let shared = Arc::new(Shared {
            queue: BatchQueue::new(),
            core,
            dim: self.dim,
        });
        spawn_coalescer(Arc::clone(&shared));
        self.slot.loads.fetch_add(1, Ordering::SeqCst);
        // Under `init` and after the check above, so the slot is empty.
        let _ = self.slot.handle.set(Handle { shared });
        Ok(())
    }

    /// Whether the weights are on the device (issue #31). `false` from
    /// [`Self::deferred`] until [`Self::load`] (or [`Embedder::prepare`])
    /// has run.
    pub fn is_loaded(&self) -> bool {
        self.slot.handle.get().is_some()
    }

    /// How many device loads this embedder (and its clones) has run: 0 or 1.
    /// A proxying serve stays at 0 (issue #31).
    pub fn load_count(&self) -> usize {
        self.slot.loads.load(Ordering::SeqCst)
    }

    /// Whether the weights sit (or, deferred, will sit) on a Metal device
    /// (issue #13).
    pub fn on_metal(&self) -> bool {
        self.on_metal
    }

    /// The served-artifact identity string to stamp into the session's
    /// `EmbeddingContract.model` (K2 task 3): the artifact's source and
    /// sha256 prefix, so a kind/dim match cannot hide a swap between two
    /// quantizations of the same model. Available before the load (#31).
    pub fn model_identity(&self) -> &str {
        &self.identity
    }
}

/// Parse `config.json` (#31: at construction, so the width is known before the
/// load and the load does not read the file a second time).
fn read_config(config_path: &Path) -> Result<Config, EmbedError> {
    serde_json::from_slice(
        &std::fs::read(config_path).map_err(|e| EmbedError::Backend(format!("config: {e}")))?,
    )
    .map_err(|e| EmbedError::Backend(format!("parse config.json: {e}")))
}

/// #31: the resolve-time width check that used to need the loaded model. The
/// CLS vector is `hidden_size` wide, so an artifact whose config disagrees
/// with the configured (and contract-stamped) width is refused here, before
/// any store check could accept it and before any forward runs.
fn check_hidden_size(config: &Config, dim: usize, config_path: &Path) -> Result<(), EmbedError> {
    if config.hidden_size != dim {
        return Err(EmbedError::Unavailable(format!(
            "candle artifact {} declares hidden_size = {} but dim = {dim} is configured — \
             refusing to resolve: the model would emit {}-dim vectors under a {dim}-dim \
             contract",
            config_path.display(),
            config.hidden_size,
            config.hidden_size
        )));
    }
    Ok(())
}

/// Where [`CandleEmbedder::deferred`] keeps the weight-digest cache: `$XDG_CACHE_HOME/lambo/candle-identity`, else
/// `$HOME/.cache/lambo/candle-identity` (the same `~/.cache` root hf-hub uses
/// for the weights). `None` (no usable directory) disables the cache: every
/// construction hashes, as before #31.
fn default_identity_cache_dir() -> Option<PathBuf> {
    let non_empty = |key: &str| std::env::var_os(key).filter(|v| !v.is_empty());
    let root = non_empty("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| non_empty("HOME").map(|home| PathBuf::from(home).join(".cache")))?;
    Some(root.join("lambo").join("candle-identity"))
}

/// The weight file's sha256 (K2-R1-1/R1-8), served from `cache_dir` when the
/// file is unchanged since it was last hashed (issue #31).
///
/// Hashing the shipped artifact reads ~1.1 GB, which every `lambo serve`
/// would otherwise pay at resolve, the election losers included. The cache
/// entry is keyed by [`file_fingerprint`]: the canonical path (an hf-hub
/// snapshot entry resolves to its blob), the size, the modification and
/// status-change times in nanoseconds, and the device and inode. Rewriting,
/// replacing, truncating or `touch`-restoring the file changes at least one
/// of them, so the entry misses and the bytes are hashed again. The cache is
/// best effort: an unreadable, corrupt or foreign entry is a miss, a failed
/// write only costs the next start a hash, and a file that changed while it
/// was being hashed is not cached at all.
///
/// Trust: the entry lives under the operator's own cache directory, writable
/// only by the same user who can already rewrite the weights; it does not
/// widen who can change what the contract stamps.
///
/// Returns the digest and the fingerprint taken BEFORE hashing, which the
/// load checks again (#31 review M2): a file that changes during or after
/// the hash fails that check.
///
/// Trade-off (#31 review L4): a cache hit does not read the bytes, so silent
/// on-disk corruption that leaves the metadata alone (bit rot) is no longer
/// caught at start the way a full re-hash would catch it. A safetensors file
/// that rots badly enough still fails to load; one that rots quietly embeds
/// under the old identity, as it would have between two starts before #31.
fn weight_sha256(
    path: &Path,
    cache_dir: Option<&Path>,
) -> Result<(String, Fingerprint), EmbedError> {
    let before = fingerprint_or_backend(path)?;
    // #31 review L-C: the cache is unix-only (it needs ownership and
    // permission bits to be trusted); elsewhere every construction hashes.
    let cache_dir = cache_dir.filter(|_| cfg!(unix));
    let Some(cache_dir) = cache_dir else {
        return Ok((sha256_file(path)?, before));
    };
    let key = before.key();
    let entry = cache_dir.join(format!("{}.json", &sha256_hex(key.as_bytes())[..32]));
    if let Some(digest) = read_identity_entry(cache_dir, &entry, &key) {
        return Ok((digest, before));
    }
    let digest = sha256_file(path)?;
    if file_fingerprint(path).ok().as_ref() == Some(&before) {
        write_identity_entry(cache_dir, &entry, &key, &digest);
    }
    Ok((digest, before))
}

/// [`file_fingerprint`], with a missing or unreadable file as the adapter's
/// usual artifact error.
fn fingerprint_or_backend(path: &Path) -> Result<Fingerprint, EmbedError> {
    file_fingerprint(path)
        .map_err(|e| EmbedError::Backend(format!("candle artifact {}: {e}", path.display())))
}

/// A file's identifying metadata: the key of [`weight_sha256`]'s cache and
/// the fast path of [`PinnedFile::check_unchanged`].
///
/// On unix: canonical path, size, mtime and ctime in nanoseconds, device and
/// inode. Elsewhere (#31 review L-C, not built today) only the canonical
/// path, size and mtime, with the rest zero.
#[derive(Clone, Debug, PartialEq, Eq)]
struct Fingerprint {
    canonical: PathBuf,
    len: u64,
    mtime: (i64, i64),
    ctime: (i64, i64),
    dev: u64,
    ino: u64,
}

impl Fingerprint {
    /// The cache key (unchanged from the first #31 cut, so existing entries
    /// stay valid).
    fn key(&self) -> String {
        format!(
            "v1|{}|len={}|mtime={}.{}|ctime={}.{}|dev={}|ino={}",
            self.canonical.display(),
            self.len,
            self.mtime.0,
            self.mtime.1,
            self.ctime.0,
            self.ctime.1,
            self.dev,
            self.ino
        )
    }

    /// The fields that differ from `other`, for an error message.
    fn changed_fields(&self, other: &Self) -> Vec<&'static str> {
        let mut changed = Vec::new();
        if self.canonical != other.canonical {
            changed.push("resolved path");
        }
        if self.len != other.len {
            changed.push("size");
        }
        if self.mtime != other.mtime {
            changed.push("mtime");
        }
        if self.ctime != other.ctime {
            changed.push("ctime");
        }
        if (self.dev, self.ino) != (other.dev, other.ino) {
            changed.push("inode");
        }
        changed
    }
}

#[cfg(unix)]
fn file_fingerprint(path: &Path) -> std::io::Result<Fingerprint> {
    use std::os::unix::fs::MetadataExt;
    let canonical = std::fs::canonicalize(path)?;
    let meta = std::fs::metadata(&canonical)?;
    Ok(Fingerprint {
        canonical,
        len: meta.len(),
        mtime: (meta.mtime(), meta.mtime_nsec()),
        ctime: (meta.ctime(), meta.ctime_nsec()),
        dev: meta.dev(),
        ino: meta.ino(),
    })
}

#[cfg(not(unix))]
fn file_fingerprint(path: &Path) -> std::io::Result<Fingerprint> {
    let canonical = std::fs::canonicalize(path)?;
    let meta = std::fs::metadata(&canonical)?;
    let mtime = meta
        .modified()?
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| (d.as_secs() as i64, i64::from(d.subsec_nanos())))
        .unwrap_or((0, 0));
    Ok(Fingerprint {
        canonical,
        len: meta.len(),
        mtime,
        ctime: (0, 0),
        dev: 0,
        ino: 0,
    })
}

#[derive(serde::Serialize, serde::Deserialize)]
struct IdentityEntry {
    fingerprint: String,
    sha256: String,
}

/// #31 review L-A: say once per process that the cache was refused, so an
/// operator whose cache never hits (a shared or group-writable directory)
/// learns why instead of paying a silent hash on every start.
#[cfg(unix)]
fn warn_cache_refused(path: &Path, why: &str) {
    static WARNED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !WARNED.swap(true, Ordering::Relaxed) {
        tracing::warn!(
            target: "lambo::embed",
            path = %path.display(),
            "candle: not using the weight-digest cache ({why}); every start hashes the \
             weights until it is owned by this user and not group- or world-writable"
        );
    }
}

/// #31 review L4, L-A: the cache directory is trusted only when this user
/// owns it and nobody else can write it. One this user owns but left group-
/// or world-writable (a umask of 002) is tightened to 0700 first; anything
/// else is refused, with a warning once per process.
#[cfg(unix)]
fn cache_dir_is_private(dir: &Path) -> bool {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    let Ok(meta) = std::fs::symlink_metadata(dir) else {
        return false;
    };
    // SAFETY: geteuid has no preconditions and cannot fail.
    let ours = meta.uid() == unsafe { libc::geteuid() } && meta.file_type().is_dir();
    if !ours {
        warn_cache_refused(dir, "the directory is not this user's, or is a link");
        return false;
    }
    if meta.mode() & 0o022 == 0 {
        return true;
    }
    if std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).is_ok() {
        return true;
    }
    warn_cache_refused(dir, "the directory is group- or world-writable");
    false
}

/// Read an entry, trusting it only if it is a regular file (not a link:
/// opened `O_NOFOLLOW`, #31 review L-B) owned by this user and not group- or
/// world-writable, checked on the open descriptor.
#[cfg(unix)]
fn read_identity_entry(cache_dir: &Path, entry: &Path, fingerprint: &str) -> Option<String> {
    use std::io::Read as _;
    use std::os::unix::fs::{MetadataExt, OpenOptionsExt};
    if !cache_dir_is_private(cache_dir) {
        return None;
    }
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(entry)
        .ok()?;
    let meta = file.metadata().ok()?;
    // SAFETY: geteuid has no preconditions and cannot fail.
    if !meta.is_file() || meta.uid() != unsafe { libc::geteuid() } || meta.mode() & 0o022 != 0 {
        warn_cache_refused(entry, "an entry is not this user's private file");
        return None;
    }
    let mut bytes = Vec::new();
    file.take(64 * 1024).read_to_end(&mut bytes).ok()?;
    let parsed: IdentityEntry = serde_json::from_slice(&bytes).ok()?;
    let well_formed = parsed.sha256.len() == 64
        && parsed
            .sha256
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    (parsed.fingerprint == fingerprint && well_formed).then_some(parsed.sha256)
}

#[cfg(not(unix))]
fn read_identity_entry(_cache_dir: &Path, _entry: &Path, _fingerprint: &str) -> Option<String> {
    None
}

#[cfg(unix)]
fn write_identity_entry(cache_dir: &Path, entry: &Path, fingerprint: &str, digest: &str) {
    let write = || -> std::io::Result<()> {
        use std::io::Write as _;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        // 0700 (#31 review L4): created private, and an existing directory
        // that is not is tightened or refused by `cache_dir_is_private`.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(cache_dir)?;
        let body = serde_json::to_vec(&IdentityEntry {
            fingerprint: fingerprint.to_string(),
            sha256: digest.to_string(),
        })
        .map_err(std::io::Error::other)?;
        // Write-then-rename, so a reader never sees a torn entry; 0600.
        let tmp = entry.with_extension(format!("tmp.{}", std::process::id()));
        std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(0o600)
            .open(&tmp)?
            .write_all(&body)?;
        std::fs::rename(&tmp, entry)
    };
    if let Err(e) = write() {
        tracing::debug!(
            target: "lambo::embed",
            error = %e,
            dir = %cache_dir.display(),
            "candle: could not cache the weight digest; the next start hashes again"
        );
    }
}

#[cfg(not(unix))]
fn write_identity_entry(_cache_dir: &Path, _entry: &Path, _fingerprint: &str, _digest: &str) {}

fn sha256_hex(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
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

    /// K2 task 3 / K2-R1-1: expose the concrete type so `resolve.rs` can
    /// downcast through [`crate::embed::candle_identity`] and stamp the loaded
    /// artifact's identity into the session's `EmbeddingContract`. Without this
    /// override the trait default returns `None`, the downcast always fails,
    /// and every candle session silently stamps `model = None`.
    fn as_any(&self) -> Option<&dyn std::any::Any> {
        Some(self)
    }

    /// Issue #31: load the weights deferred at resolve, on a blocking thread
    /// so the runtime keeps polling (a holder's startup races this against
    /// its shutdown signal). Idempotent; every clone shares the one load.
    async fn prepare(&self) -> Result<(), EmbedError> {
        if self.is_loaded() {
            return Ok(());
        }
        let this = self.clone();
        tokio::task::spawn_blocking(move || this.load())
            .await
            .map_err(|e| EmbedError::Backend(format!("candle load task failed: {e}")))?
    }

    fn is_prepared(&self) -> bool {
        self.is_loaded()
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbedError> {
        reject_empty(text)?;
        // #31: the holder path has loaded by now (`prepare` in the attach).
        // A caller that embeds straight from `ResolvedBackends.embedder`
        // without a `Memory` (the one-shot CLI recall, a library user) loads
        // here, on its first call, which is when it would have needed the
        // weights anyway.
        if !self.is_loaded() {
            self.prepare().await?;
        }
        let handle = self.slot.handle.get().ok_or_else(|| {
            EmbedError::Unavailable("candle embedder weights are not loaded".into())
        })?;
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle.shared.queue.push(Pending {
            text: text.to_string(),
            tx,
        });
        recv_bounded(rx, EMBED_WAIT).await
    }
}

/// A tiny, real BGE-M3-shaped artifact on disk for tests (issue #31): a
/// `config.json` with BGE-M3's 1024 hidden size but one layer, one head and a
/// four-word vocabulary, a matching `tokenizer.json`, and randomly initialised
/// f32 `model.safetensors` (~17 MB). It loads and embeds on CPU like the real
/// artifact, with no network and no cached weights, so the deferred-load path
/// runs in CI's candle row. Every call draws new weights, so two calls produce
/// two artifacts with different digests.
#[cfg(test)]
pub(crate) mod test_artifact {
    use super::*;

    /// The tiny config, as `config.json` would carry it.
    pub(crate) fn config_json(hidden_size: usize) -> String {
        serde_json::json!({
            "hidden_size": hidden_size,
            "layer_norm_eps": 1e-5,
            "attention_probs_dropout_prob": 0.0,
            "hidden_dropout_prob": 0.0,
            "num_attention_heads": 1,
            "position_embedding_type": "absolute",
            "intermediate_size": 8,
            "hidden_act": "gelu",
            "num_hidden_layers": 1,
            "vocab_size": 8,
            "max_position_embeddings": 16,
            "type_vocab_size": 1,
            "pad_token_id": 1
        })
        .to_string()
    }

    /// Write the three files into `dir` (created if missing).
    pub(crate) fn write(dir: &Path) {
        std::fs::create_dir_all(dir).expect("artifact dir");
        let config = config_json(BGE_M3_DIM);
        std::fs::write(dir.join("config.json"), &config).expect("config.json");
        std::fs::write(
            dir.join("tokenizer.json"),
            serde_json::json!({
                "version": "1.0",
                "truncation": null,
                "padding": null,
                "added_tokens": [],
                "normalizer": null,
                "pre_tokenizer": null,
                "post_processor": null,
                "decoder": null,
                "model": {
                    "type": "WordPiece",
                    "unk_token": "[UNK]",
                    "continuing_subword_prefix": "##",
                    "max_input_chars_per_word": 100,
                    "vocab": {"[UNK]": 0, "<pad>": 1, "x": 2, "lambo": 3}
                }
            })
            .to_string(),
        )
        .expect("tokenizer.json");
        write_weights(dir);
    }

    /// (Re)write only `model.safetensors`, with fresh random weights.
    pub(crate) fn write_weights(dir: &Path) {
        let cfg: Config = serde_json::from_str(&config_json(BGE_M3_DIM)).expect("tiny config");
        let varmap = candle_nn::VarMap::new();
        let vb = VarBuilder::from_varmap(&varmap, DType::F32, &Device::Cpu);
        XLMRobertaModel::new(&cfg, vb).expect("tiny model initialises");
        varmap
            .save(dir.join(DEFAULT_WEIGHT_FILE))
            .expect("model.safetensors");
    }

    /// Options that load this artifact on CPU, offline.
    pub(crate) fn opts(dir: &Path) -> CandleOpts {
        CandleOpts {
            device: Some("cpu".into()),
            offline: true,
            weights_dir: Some(dir.to_path_buf()),
            ..Default::default()
        }
    }

    /// A deferred embedder over this artifact, with the digest cache in
    /// `cache_dir` (never the operator's).
    pub(crate) fn deferred(dir: &Path, cache_dir: &Path) -> Result<CandleEmbedder, EmbedError> {
        CandleEmbedder::deferred_in(BGE_M3_DIM, opts(dir), Some(cache_dir.to_path_buf()))
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

    /// A `CandleEmbedder` whose model is a tiny zero-weighted XLM-RoBERTa and
    /// whose tokenizer is an empty WordPiece — never touching the network or
    /// the real artifact. `embed()` is never called on it; it exists solely so
    /// the identity plumbing can be exercised through a real trait object.
    #[cfg(test)]
    fn weightless_embedder() -> CandleEmbedder {
        let device = Device::Cpu;
        let cfg = Config {
            hidden_size: 8,
            layer_norm_eps: 1e-12,
            attention_probs_dropout_prob: 0.0,
            hidden_dropout_prob: 0.0,
            num_attention_heads: 1,
            position_embedding_type: "absolute".to_string(),
            intermediate_size: 8,
            hidden_act: candle_nn::Activation::Gelu,
            num_hidden_layers: 1,
            vocab_size: 16,
            max_position_embeddings: 32,
            type_vocab_size: 1,
            pad_token_id: 1,
        };
        let model =
            XLMRobertaModel::new(&cfg, VarBuilder::zeros(DType::F32, &device)).expect("tiny model");
        let tokenizer = Tokenizer::new(
            tokenizers::models::wordpiece::WordPiece::builder()
                .build()
                .unwrap(),
        );
        let core = BgeM3Core {
            model,
            tokenizer,
            device: device.clone(),
        };
        let shared = Arc::new(Shared {
            queue: BatchQueue::new(),
            core,
            dim: BGE_M3_DIM,
        });
        let slot = LoadSlot::empty();
        let _ = slot.handle.set(Handle { shared });
        CandleEmbedder {
            dim: BGE_M3_DIM,
            identity: stamp_identity(
                SOURCE_REPO,
                SOURCE_REVISION,
                DEFAULT_WEIGHT_FILE,
                None,
                WEIGHT_SHA256,
            ),
            on_metal: false,
            plan: Arc::new(LoadPlan {
                config: cfg,
                tokenizer_path: PathBuf::new(),
                weights_path: PathBuf::new(),
                device,
                dtype: DType::F32,
                artifacts: Vec::new(),
                before_map: Mutex::new(None),
            }),
            slot: Arc::new(slot),
        }
    }

    #[test]
    fn identity_reaches_resolve_through_the_trait_object() {
        // K2 task 3 / K2-R1-1 dogfood regression: `resolve.rs` only ever sees
        // a `&dyn Embedder`, and `candle_identity` recovers the stamp via the
        // `as_any` downcast. The existing identity tests call the concrete
        // `model_identity()` and could not catch a missing override (the trait
        // default returns `None`, so every session stamped `model = None`
        // silently). Pin the exact property resolve.rs relies on, through the
        // trait object.
        let e = weightless_embedder();
        let expected = e.model_identity().to_string();
        let dyn_ref: &dyn Embedder = &e;
        assert_eq!(
            crate::embed::candle_identity(dyn_ref),
            Some(expected),
            "candle_identity must recover the artifact identity through the trait object"
        );
    }

    /// Issue #13: the keep-warm auto default reads memory placement through the
    /// same trait-object downcast. A CPU-device candle embedder is not on
    /// unified Metal memory, so auto keep-warm stays off for it; an
    /// accelerator-specific positive case needs a real Metal device and the
    /// weights, so it is covered by the measurement, not this unit test.
    #[test]
    fn cpu_candle_is_not_reported_as_unified_memory() {
        let e = weightless_embedder();
        assert!(!e.on_metal());
        let dyn_ref: &dyn Embedder = &e;
        assert!(!crate::embed::weights_in_unified_memory(dyn_ref));
    }

    #[test]
    fn sha256_file_hashes_the_bytes_actually_on_disk() {
        // K2-R1-1/R1-8 foundation: the digest comes from the file, not a const.
        let dir = crate::test_util::ScratchDir::new("lambo-candle-sha-test");
        let path = dir.join("artifact.bin");
        std::fs::write(&path, b"abc").unwrap();
        let got = sha256_file(&path).unwrap();
        assert_eq!(
            got,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
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

    /// LIVE Metal test (K2 Done-when: "cosine parity against the K1 gate holds
    /// in the shipped adapter, not just the spike"). Runs the shipped
    /// `CandleEmbedder` on **Metal** against the rig's llama.cpp q8_0 reference
    /// and applies K1's own 0.99 median gate.
    ///
    /// Ignored by default: needs real weights AND a llama-server on :8080
    /// started with the DOGFOOD-SETUP capacity flags. Run with:
    /// `cargo test --features embed-candle-metal,embed-bge -- --ignored metal_parity`
    #[ignore]
    #[tokio::test]
    async fn live_metal_parity_against_the_llama_reference() {
        let texts = [
            "user schema",
            "the mitochondria is the powerhouse of the cell",
            "lambo serve binds a session endpoint so other clients can attach",
            "会話の記憶をグラフとして保存する",
            "Der schnelle braune Fuchs springt über den faulen Hund am Flussufer",
            "fn resolve_device(opts: &CandleOpts) -> Result<DeviceChoice, EmbedError>",
            &"canonization promotes a concept once independent sessions agree. ".repeat(40),
        ];

        // Prove the leg under test is really Metal: a silent CPU fallback would
        // let every assertion below pass while testing nothing about Metal.
        let choice = resolve_device(Some("metal"), has_metal_backend(), false, true)
            .expect("metal must resolve on this rig");
        assert!(
            choice.device.is_metal(),
            "expected a Metal device, got {:?} — this test would otherwise pass on CPU",
            choice.device
        );
        assert_eq!(choice.dtype, DType::F16, "Metal leg runs f16 (K1)");

        let metal = CandleEmbedder::new(
            BGE_M3_DIM,
            CandleOpts {
                device: Some("metal".into()),
                ..Default::default()
            },
        )
        .expect("metal device + weights must resolve");

        let reference = crate::embed::bge_m3::BgeM3LlamaCppEmbedder::new(
            "http://127.0.0.1:8080",
            "bge-m3",
            BGE_M3_DIM,
        )
        .expect("reference embedder");

        let mut cosines = Vec::new();
        for t in &texts {
            let a = metal.embed(t).await.expect("metal embed");
            let b = reference.embed(t).await.expect("reference embed");
            assert_eq!(a.len(), BGE_M3_DIM);
            assert_eq!(b.len(), BGE_M3_DIM);
            let dot: f32 = a.iter().zip(&b).map(|(x, y)| x * y).sum();
            let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
            let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
            let cos = dot / (na * nb);
            eprintln!("cos={cos:.6}  len={}  {:.50}", t.len(), t);
            cosines.push(cos);
        }
        cosines.sort_by(|x, y| x.partial_cmp(y).unwrap());
        let median = cosines[cosines.len() / 2];
        let min = cosines[0];
        eprintln!(
            "METAL PARITY: median={median:.6} min={min:.6} n={}",
            cosines.len()
        );
        assert!(
            median >= 0.99,
            "K1 gate: median {median} < 0.99 — pooling wrong on Metal"
        );
        assert!(min >= 0.99, "worst item {min} < 0.99 on Metal");
    }

    // ---- Issue #31: identity before load ----

    #[tokio::test]
    async fn deferred_stamps_identity_and_width_without_loading_the_model() {
        let dir = crate::test_util::ScratchDir::new("lambo-31-deferred");
        let artifact = dir.join("artifact");
        test_artifact::write(&artifact);
        let e = test_artifact::deferred(&artifact, &dir.join("cache"))
            .expect("the tiny artifact resolves");

        // Everything the resolve needs, and no weights on the device.
        assert!(!e.is_loaded(), "deferred must not load the model");
        assert_eq!(e.load_count(), 0);
        assert_eq!(e.dimensions(), BGE_M3_DIM);
        let digest = sha256_file(&artifact.join(DEFAULT_WEIGHT_FILE)).unwrap();
        let expected = stamp_identity(
            WEIGHT_REPO,
            WEIGHT_REVISION,
            DEFAULT_WEIGHT_FILE,
            Some(&artifact),
            &digest,
        );
        assert_eq!(e.model_identity(), expected);
        let dyn_ref: &dyn Embedder = &e;
        assert_eq!(crate::embed::candle_identity(dyn_ref), Some(expected));

        // The holder's explicit load: once, shared by every clone.
        let clone = e.clone();
        dyn_ref.prepare().await.expect("prepare loads");
        assert!(e.is_loaded() && clone.is_loaded());
        clone.prepare().await.expect("prepare is idempotent");
        e.load().expect("load is idempotent");
        assert_eq!(
            e.load_count(),
            1,
            "one device load per embedder, not per call"
        );

        let v = clone
            .embed("lambo")
            .await
            .expect("a loaded embedder embeds");
        assert_eq!(v.len(), BGE_M3_DIM);
        assert_eq!(e.load_count(), 1);
    }

    /// A direct user of `ResolvedBackends.embedder` (the one-shot CLI recall)
    /// never calls `prepare`; its first embed loads.
    #[tokio::test]
    async fn embed_without_prepare_loads_once_then_embeds() {
        let dir = crate::test_util::ScratchDir::new("lambo-31-fallback");
        let artifact = dir.join("artifact");
        test_artifact::write(&artifact);
        let e = test_artifact::deferred(&artifact, &dir.join("cache")).unwrap();
        let (a, b) = tokio::join!(e.embed("x"), e.embed("lambo"));
        assert_eq!(a.unwrap().len(), BGE_M3_DIM);
        assert_eq!(b.unwrap().len(), BGE_M3_DIM);
        assert_eq!(e.load_count(), 1, "concurrent first calls share one load");
    }

    /// The width check that used to need the loaded model now reads
    /// `config.json` at construction (#31), so an artifact that would emit
    /// another width is refused before any load.
    #[test]
    fn a_config_hidden_size_other_than_dim_is_refused_at_construction() {
        let dir = crate::test_util::ScratchDir::new("lambo-31-hidden");
        let artifact = dir.join("artifact");
        test_artifact::write(&artifact);
        std::fs::write(
            artifact.join("config.json"),
            test_artifact::config_json(768),
        )
        .unwrap();
        let err = match test_artifact::deferred(&artifact, &dir.join("cache")) {
            Err(e) => e.to_string(),
            Ok(_) => panic!("a 768-wide artifact must not resolve under dim = 1024"),
        };
        assert!(err.contains("hidden_size = 768"), "{err}");
        assert!(err.contains("dim = 1024"), "{err}");
    }

    #[test]
    fn the_identity_cache_serves_an_unchanged_file_and_misses_a_changed_one() {
        let dir = crate::test_util::ScratchDir::new("lambo-31-idcache");
        let cache = dir.join("cache");
        let weights = dir.join("weights.bin");
        std::fs::write(&weights, b"first artifact bytes").unwrap();
        let first = sha256_file(&weights).unwrap();

        // A miss hashes the bytes and records them.
        assert_eq!(weight_sha256(&weights, Some(&cache)).unwrap().0, first);
        let entries: Vec<_> = std::fs::read_dir(&cache).unwrap().collect();
        assert_eq!(entries.len(), 1, "one entry per fingerprint");
        let entry = entries[0].as_ref().unwrap().path();

        // A hit is served from the entry, not the bytes: plant a different
        // (well-formed) digest under the same fingerprint and it comes back.
        let fingerprint = file_fingerprint(&weights).unwrap();
        let planted = "0".repeat(64);
        std::fs::write(
            &entry,
            serde_json::to_vec(&IdentityEntry {
                fingerprint: fingerprint.key(),
                sha256: planted.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(weight_sha256(&weights, Some(&cache)).unwrap().0, planted);

        // Rewriting the file (same length, new bytes) changes the
        // fingerprint, so the entry misses and the new bytes are hashed.
        std::thread::sleep(Duration::from_millis(5));
        std::fs::write(&weights, b"other artifact bytes").unwrap();
        assert_ne!(file_fingerprint(&weights).unwrap(), fingerprint);
        let second = sha256_file(&weights).unwrap();
        assert_ne!(second, first);
        assert_eq!(weight_sha256(&weights, Some(&cache)).unwrap().0, second);

        // A corrupt entry is a miss, never an error.
        for entry in std::fs::read_dir(&cache).unwrap() {
            std::fs::write(entry.unwrap().path(), b"{not json").unwrap();
        }
        assert_eq!(weight_sha256(&weights, Some(&cache)).unwrap().0, second);
        // No cache directory: hash every time, as before #31.
        assert_eq!(weight_sha256(&weights, None).unwrap().0, second);
    }

    /// The contract identity follows the bytes through the cache: a changed
    /// weight file stamps a new identity on the next construction.
    #[test]
    fn a_changed_weight_file_changes_the_stamped_identity() {
        let dir = crate::test_util::ScratchDir::new("lambo-31-reweigh");
        let artifact = dir.join("artifact");
        let cache = dir.join("cache");
        test_artifact::write(&artifact);
        let before = test_artifact::deferred(&artifact, &cache)
            .unwrap()
            .model_identity()
            .to_string();
        let again = test_artifact::deferred(&artifact, &cache)
            .unwrap()
            .model_identity()
            .to_string();
        assert_eq!(before, again, "an unchanged file keeps its identity");
        std::thread::sleep(Duration::from_millis(5));
        test_artifact::write_weights(&artifact);
        let after = test_artifact::deferred(&artifact, &cache)
            .unwrap()
            .model_identity()
            .to_string();
        assert_ne!(before, after, "new weights must stamp a new identity");
        let digest = sha256_file(&artifact.join(DEFAULT_WEIGHT_FILE)).unwrap();
        assert!(after.contains(&digest[..12]), "{after}");
    }

    /// #31 review M2: the load re-checks the artifact against what the
    /// resolve identified. New weights written between the two are refused,
    /// permanently, and nothing reaches the device.
    #[tokio::test]
    async fn a_weight_file_changed_after_resolve_is_refused_at_load() {
        let dir = crate::test_util::ScratchDir::new("lambo-31-m2");
        let artifact = dir.join("artifact");
        test_artifact::write(&artifact);
        let e = test_artifact::deferred(&artifact, &dir.join("cache")).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        test_artifact::write_weights(&artifact);
        let err = e
            .prepare()
            .await
            .expect_err("changed weights must not load");
        let msg = err.to_string();
        assert!(msg.contains("changed after this process resolved"), "{msg}");
        assert!(msg.contains(DEFAULT_WEIGHT_FILE), "{msg}");
        assert!(
            !err.is_transient(),
            "a changed artifact needs a restart: {err:?}"
        );
        assert!(!e.is_loaded());
        assert_eq!(e.load_count(), 0);

        // The same for config.json and tokenizer.json (new bytes: a change
        // of metadata alone is re-hashed and accepted, review M-B).
        for file in ["config.json", "tokenizer.json"] {
            let e = test_artifact::deferred(&artifact, &dir.join("cache")).unwrap();
            std::thread::sleep(Duration::from_millis(5));
            let path = artifact.join(file);
            let mut body = std::fs::read(&path).unwrap();
            body.extend_from_slice(b"\n");
            std::fs::write(&path, body).unwrap();
            let msg = e.prepare().await.expect_err(file).to_string();
            assert!(msg.contains(file), "{msg}");
            assert_eq!(e.load_count(), 0);
        }
    }

    /// #31 review L3: the tokenizer is validated before the weights are
    /// touched. With a corrupt tokenizer AND weights that would fail to map,
    /// the load fails on the tokenizer: the weights were never read.
    #[tokio::test]
    async fn a_corrupt_tokenizer_fails_the_load_before_the_weights() {
        let dir = crate::test_util::ScratchDir::new("lambo-31-l3");
        let artifact = dir.join("artifact");
        test_artifact::write(&artifact);
        std::fs::write(artifact.join("tokenizer.json"), b"{ not a tokenizer").unwrap();
        std::fs::write(artifact.join(DEFAULT_WEIGHT_FILE), b"not safetensors").unwrap();
        let e = test_artifact::deferred(&artifact, &dir.join("cache"))
            .expect("neither file is parsed at resolve");
        let err = e
            .prepare()
            .await
            .expect_err("a corrupt tokenizer must not load");
        let msg = err.to_string();
        assert!(msg.contains("tokenizer"), "{msg}");
        assert!(
            !msg.contains("safetensors"),
            "the weights were read first: {msg}"
        );
        assert!(!err.is_transient(), "{err:?}");
        assert_eq!(e.load_count(), 0);
    }

    /// #31 review L4: the cache is created private, and an entry or a
    /// directory that another user could write is never trusted.
    #[test]
    fn the_identity_cache_trusts_only_private_entries() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_util::ScratchDir::new("lambo-31-l4");
        let cache = dir.join("cache");
        let weights = dir.join("weights.bin");
        std::fs::write(&weights, b"artifact bytes").unwrap();
        let real = sha256_file(&weights).unwrap();
        assert_eq!(weight_sha256(&weights, Some(&cache)).unwrap().0, real);
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&cache), 0o700, "the cache directory is created 0700");
        let entry = std::fs::read_dir(&cache)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        assert_eq!(mode(&entry), 0o600, "entries are written 0600");

        // Plant a digest under the right fingerprint: served while private...
        let planted = "0".repeat(64);
        std::fs::write(
            &entry,
            serde_json::to_vec(&IdentityEntry {
                fingerprint: file_fingerprint(&weights).unwrap().key(),
                sha256: planted.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(weight_sha256(&weights, Some(&cache)).unwrap().0, planted);
        // ...a miss once the entry is group/world-writable...
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o666)).unwrap();
        assert_eq!(weight_sha256(&weights, Some(&cache)).unwrap().0, real);
        // ...and once the directory is.
        std::fs::set_permissions(&entry, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::write(
            &entry,
            serde_json::to_vec(&IdentityEntry {
                fingerprint: file_fingerprint(&weights).unwrap().key(),
                sha256: planted.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        assert_eq!(weight_sha256(&weights, Some(&cache)).unwrap().0, planted);
        // A symlinked entry is never followed (review L-B).
        let target = dir.join("elsewhere.json");
        std::fs::rename(&entry, &target).unwrap();
        std::os::unix::fs::symlink(&target, &entry).unwrap();
        assert_eq!(weight_sha256(&weights, Some(&cache)).unwrap().0, real);
    }

    /// #31 review L-A: a cache directory this user owns but left group- or
    /// world-writable (a umask of 002) is tightened to 0700 and used, not
    /// silently missed on every start.
    #[test]
    fn a_loose_cache_directory_of_ours_is_tightened_and_used() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_util::ScratchDir::new("lambo-31-la");
        let cache = dir.join("cache");
        let weights = dir.join("weights.bin");
        std::fs::write(&weights, b"artifact bytes").unwrap();
        weight_sha256(&weights, Some(&cache)).unwrap();
        let entry = std::fs::read_dir(&cache)
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let planted = "0".repeat(64);
        std::fs::write(
            &entry,
            serde_json::to_vec(&IdentityEntry {
                fingerprint: file_fingerprint(&weights).unwrap().key(),
                sha256: planted.clone(),
            })
            .unwrap(),
        )
        .unwrap();
        std::fs::set_permissions(&cache, std::fs::Permissions::from_mode(0o775)).unwrap();
        assert_eq!(
            weight_sha256(&weights, Some(&cache)).unwrap().0,
            planted,
            "our own loose directory is tightened, then trusted"
        );
        let mode = std::fs::metadata(&cache).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o700);
    }

    /// #31 review M-A: a weight file whose safetensors header does not parse
    /// (truncated) is the artifact's fault: permanent, never retried. The
    /// classifier looks through context wrappers to the cause.
    #[tokio::test]
    async fn a_truncated_weight_file_is_a_permanent_load_failure() {
        let dir = crate::test_util::ScratchDir::new("lambo-31-ma");
        let artifact = dir.join("artifact");
        test_artifact::write(&artifact);
        let weights = artifact.join(DEFAULT_WEIGHT_FILE);
        let bytes = std::fs::read(&weights).unwrap();
        std::fs::write(&weights, &bytes[..bytes.len() / 2]).unwrap();
        let e = test_artifact::deferred(&artifact, &dir.join("cache")).unwrap();
        let err = e
            .prepare()
            .await
            .expect_err("a truncated file must not load");
        assert!(!err.is_transient(), "never retried: {err:?}");
        assert!(err.to_string().contains("safetensors"), "{err}");
        assert_eq!(e.load_count(), 0);

        use candle_core::Error as E;
        let io = || E::Io(std::io::Error::other("cannot allocate memory"));
        assert!(candle_error_is_transient(&io()));
        assert!(candle_error_is_transient(&io().with_path("/w")));
        assert!(!candle_error_is_transient(&E::CannotFindTensor {
            path: "embeddings.word_embeddings.weight".into()
        }));
        assert!(!candle_error_is_transient(&E::Msg("shape mismatch".into())));
    }

    /// #31 review M-B: a chmod (which moves ctime only) after the resolve is
    /// not a changed artifact: the bytes are re-hashed and the load proceeds.
    #[tokio::test]
    async fn a_metadata_only_change_after_resolve_still_loads() {
        use std::os::unix::fs::PermissionsExt;
        let dir = crate::test_util::ScratchDir::new("lambo-31-mb");
        let artifact = dir.join("artifact");
        test_artifact::write(&artifact);
        let e = test_artifact::deferred(&artifact, &dir.join("cache")).unwrap();
        let weights = artifact.join(DEFAULT_WEIGHT_FILE);
        let before = file_fingerprint(&weights).unwrap();
        std::thread::sleep(Duration::from_millis(5));
        std::fs::set_permissions(&weights, std::fs::Permissions::from_mode(0o600)).unwrap();
        assert_ne!(
            file_fingerprint(&weights).unwrap(),
            before,
            "the chmod moved ctime"
        );
        e.prepare().await.expect("identical bytes load");
        assert!(e.is_loaded());
    }

    /// #31 review M-C: a file swapped while the weights were being mapped is
    /// caught by the check after the load, and the model is dropped.
    #[tokio::test]
    async fn a_swap_during_the_load_is_refused_after_it() {
        let dir = crate::test_util::ScratchDir::new("lambo-31-mc");
        let artifact = dir.join("artifact");
        test_artifact::write(&artifact);
        let e = test_artifact::deferred(&artifact, &dir.join("cache")).unwrap();
        let swap = artifact.clone();
        *e.plan.before_map.lock().unwrap() = Some(Box::new(move || {
            std::thread::sleep(Duration::from_millis(5));
            test_artifact::write_weights(&swap);
        }));
        let err = e
            .prepare()
            .await
            .expect_err("a swapped file must not be served");
        assert!(err.to_string().contains("changed after"), "{err}");
        assert!(
            !e.is_loaded(),
            "the model loaded from the swapped file is dropped"
        );
        assert_eq!(e.load_count(), 0);
    }

    /// Issue #31 measurement, recorded as a test: the process footprint of a
    /// deferred candle embedder on Metal (what a proxying serve now holds)
    /// against the same embedder once loaded (what a holder holds). Needs the
    /// cached published f16 weights (offline, never downloads) and a Metal
    /// build. Run with:
    /// `cargo test --features embed-candle-metal --lib -- --ignored --nocapture metal_proxy_footprint`
    #[cfg(target_os = "macos")]
    #[ignore]
    #[test]
    fn metal_proxy_footprint() {
        fn footprint_mb() -> f64 {
            let mut info = std::mem::MaybeUninit::<libc::rusage_info_v2>::zeroed();
            // SAFETY: proc_pid_rusage writes a rusage_info_v2 into `info`.
            let rc = unsafe {
                libc::proc_pid_rusage(
                    std::process::id() as libc::c_int,
                    libc::RUSAGE_INFO_V2,
                    info.as_mut_ptr().cast(),
                )
            };
            assert_eq!(rc, 0, "proc_pid_rusage");
            // SAFETY: initialised by the successful call above.
            unsafe { info.assume_init() }.ri_phys_footprint as f64 / 1_048_576.0
        }
        let opts = || CandleOpts {
            device: Some("metal".into()),
            offline: true,
            ..Default::default()
        };
        let base = footprint_mb();
        let t = std::time::Instant::now();
        let e = CandleEmbedder::deferred(BGE_M3_DIM, opts()).expect("cached weights, Metal");
        let first = t.elapsed();
        let deferred = footprint_mb();
        let t = std::time::Instant::now();
        drop(CandleEmbedder::deferred(BGE_M3_DIM, opts()).unwrap());
        let cached = t.elapsed();
        let t = std::time::Instant::now();
        e.load().expect("load on Metal");
        let load = t.elapsed();
        let loaded = footprint_mb();
        eprintln!(
            "#31 METAL FOOTPRINT: baseline {base:.1} MB, deferred (proxy) {deferred:.1} MB, \
             loaded (holder) {loaded:.1} MB; deferred construct {first:?} (first), \
             {cached:?} (identity cached); load {load:?}"
        );
        assert!(e.on_metal() && e.is_loaded());
        assert!(
            loaded - deferred > 500.0,
            "the load is where the weights arrive"
        );
        assert!(
            deferred - base < 100.0,
            "a deferred embedder must not hold the weights"
        );
    }

    // Compile-time sanity (clippy wants const assertions, not runtime tests).
    const _: () = {
        assert!(MAX_BATCH >= 1);
        assert!(MAX_SEQ_LEN >= 8192);
        assert!(BGE_M3_DIM == 1024);
    };
}
