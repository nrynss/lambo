# #22 PR 5: EmbeddingGemma 2 over llama.cpp, live evidence

Captured 2026-10-09 on the Metal Mac, against llama.cpp **b11517** (`8a1a9b512`, the
upstream release binary) serving the Q8_0 GGUF. Lambo at `10e18e2` on
`feat/22-pr5-eg2`, debug build. No weights or images are committed: the weights are
pinned by sha256 in [`versions.txt`](versions.txt), and the live test generates its
images. Local paths read `<HOME>`, scratch paths `<SCRATCH>` (this run) and `<EVAL>`
(the 2026-10-09 eval the PR 5 amendment cites).

| File | What it shows |
|---|---|
| [`live-eg2.txt`](live-eg2.txt) | `tests/live_eg2.rs`, both tests passing: AC2 against the `--mmproj` server, and the no-projector case against a second server. Includes the ranking table below. |
| [`versions.txt`](versions.txt) | llama-server and rustc versions, the GGUF sha256s and sizes, the source revision. |
| [`props-mmproj.json`](props-mmproj.json), [`props-text-only.json`](props-text-only.json), [`v1-models-mmproj.json`](v1-models-mmproj.json) | What b11517 reports about itself: the fields the adapter's startup check reads. |
| [`ubatch-cap.txt`](ubatch-cap.txt) | The finding below: the default ubatch silently caps the 280 image budget to 256, and the adapter refuses those images. |
| [`bad-images.txt`](bad-images.txt) | How b11517 answers a WebP and two undecodable images. |
| [`image-latency.txt`](image-latency.txt) | Server-side time per image at the fixed 280 budget. |
| [`size-invariance.txt`](size-invariance.txt) | 22g, profile `lambo-eg2-v2`: cosine per picture and size (128 to 3000 px) before and after the canonical image form (every image resized to a 768 px longer side), the downscale and upscale filter comparisons, and why WebP is never sent as WebP. |

## How the servers ran

```bash
# 8191: the lambo-eg2-v1 server (text and images), in the background
llama-server --host 127.0.0.1 --port 8191 \
  -m embeddinggemma-2-Q8_0.gguf --mmproj mmproj-embeddinggemma-2-Q8_0.gguf \
  --embeddings --pooling mean --image-min-tokens 280 --image-max-tokens 280 \
  --ctx-size 8192 --batch-size 8192 --ubatch-size 8192 > server-8191.log 2>&1 &
# 8192: the same without --mmproj (text only), in the background
llama-server --host 127.0.0.1 --port 8192 \
  -m embeddinggemma-2-Q8_0.gguf \
  --embeddings --pooling mean \
  --ctx-size 8192 --batch-size 8192 --ubatch-size 8192 > server-8192.log 2>&1 &
# wait until both answer GET /health with 200, then run the live tests
LAMBO_EG2_URL=http://127.0.0.1:8191 LAMBO_EG2_TEXT_ONLY_URL=http://127.0.0.1:8192 \
  cargo test --features embed-eg2 --test live_eg2 -- --ignored --nocapture --test-threads=1
```

## AC2 results

- Text: 768-d, unit norm, both roles. The same sentence as a query and as a document:
  cosine 0.977, so the prefixes move it.
- MRL 256: the 768 vector's first 256 values have norm 0.672 before re-normalizing; the
  256-d result is exactly that head re-normalized.
- Images: 768-d, unit norm. A red square at 64 px and at 512 px: cosine **1.000000**.
- Cross-modal: each generated image ranks its own caption first (0.750, 0.755, 0.794,
  against 0.566 to 0.678 for the other captions); red is closer to "a solid red square"
  than to "a bicycle".
- `/props` check: `Verified { vision: Some(true) }` on 8191, `Some(false)` on 8192.
- No projector (8192): text embeds; an image is refused as a permanent `Backend` error
  naming `--mmproj`, both by the startup check and, with the check off, by
  classifying the server's own `500 ... provide the mmproj`.

## Findings that changed the implementation

1. **The default ubatch silently caps the image budget.** Started with only
   `--image-min-tokens 280 --image-max-tokens 280`, b11517 logs
   `cap image_max_tokens (original=280) to 256 (n_ubatch = 512)` and embeds every image
   as 260 tokens. That server is still perfectly size invariant (64 px against 512 px:
   1.000000), so the size-invariance assertion the amendment planned would pass on
   it, yet its vectors sit off the profile (cosine 0.9989 to the same image on the 280
   server). `/props` does not report the budget. So the adapter reads
   `usage.prompt_tokens` on every image response and refuses a count outside 280 to
   312 (b11517 reports 293 at the fixed budget; superseded, see 4), and the documented
   command line adds
   `--ctx-size 8192 --batch-size 8192 --ubatch-size 8192`. The large ubatch also lets a
   text input of up to 8,192 tokens fit the one ubatch a non-causal model needs
   (verified: the server runs 4 slots with `n_ctx_slot = 8192, kv_unified = 'true'`, a
   7,008-token input embeds and a 9,808-token one gets the "increase the physical
   batch size" 500, which Lambo settles as a content refusal; see
   [long-text.txt](long-text.txt)).
2. **Two image faults arrive as HTTP 500.** "provide the mmproj" (no projector) and
   "Failed to load image or audio file" (undecodable bytes). The J3 table reads a 500
   as transient, which would retry those writes forever. Image calls use a refined
   rule: the first is a permanent configuration error, the second a content refusal;
   any other 500 stays transient. WebP decodes on b11517 only when an `ffmpeg`/`ffprobe` is on the server's `PATH` (22g, [size-invariance.txt](size-invariance.txt)); `lambo-eg2-v2` sends every image, WebP included, as its canonical PNG.
3. **Image latency at the 280 budget is about 370 ms per image** server-side on this
   Mac, in this run and in the eval's own 280-budget server log. The amendment's 95 to
   160 ms was measured at llama.cpp's dynamic budget (85 to 125 tokens). Text stays
   at about 7 ms warm.

4. **An image's token count does not show the budget** (review remediation, same
   build, [image-token-counts.txt](image-token-counts.txt)). The 293 above holds only
   for small, squarish images. At the 280 budget b11517 reports 236 to 540 prompt
   tokens across 100 sizes and shapes: 260 for a square of 800 px or more, 268 for
   1100x600, 250 for 2000x330, 540 for 16x4096. The 280 to 312 window therefore
   refused ordinary photos on a correctly started server (the live test's 2000x330
   image failed with "as 250 tokens"), and the counts of a capped or default budget
   overlap the range, so no window can be right. The adapter now checks the budget
   with a fixed reference image (1x1 PNG): 293 at the profile's budget, 260 when a 512
   ubatch caps it, 85 at the dynamic default, 328 at a budget of 300. It embeds it
   before the first image for a server `/props` verified, and again every 60 s, and
   refuses image embeds unless the count is 293 within 8. An image's own count is not
   judged. The same file shows that renders of one picture at different sizes are not
   one vector: a solid square is identical up to 768 px and at 0.9989 from 896 px
   (exactly the capped server's offset), and a checkerboard drifts to 0.97 to 0.99.

## Design 7.3: ranking parity (measured, not adjusted)

EG2 Q8_0, profile `lambo-eg2-v1`, dim 768. Query role against document role (text) or
image; document role against document role for the concept-name pairs.

| pair | n | min | mean | max |
|---|---|---|---|---|
| text query to image, relevant | 3 | 0.7500 | 0.7664 | 0.7943 |
| text query to image, irrelevant | 9 | 0.5663 | 0.6077 | 0.6776 |
| text query to text, relevant | 4 | 0.8228 | 0.8447 | 0.8665 |
| text query to text, irrelevant | 12 | 0.5458 | 0.6078 | 0.6656 |
| concept names, paraphrase | 4 | 0.7461 | 0.8277 | 0.8659 |
| concept names, distinct | 4 | 0.6554 | 0.7247 | 0.7554 |

What it says about the two BGE-M3-calibrated constants and the PR 3 question:

- **Modality gap: about 0.08.** Relevant images score 0.75 to 0.79 where relevant text
  scores 0.82 to 0.87. Both clear their irrelevant set (whose maximum is 0.68 for
  images) by at least 0.07, so within the vector leg a relevant image still outranks an
  irrelevant one.
- **`RECENT_SCORE = 0.35` is below everything EG2 returns**, irrelevant hits
  included (the lowest cosine measured is 0.546). The design's worry, a relevant image
  hit under 0.35 losing to a recent-interaction member, does not occur. The reverse
  does: in phase 1 any EG2 vector hit, relevant or not, outranks a recent member.
  That is a calibration question for Q15, not a defect of this adapter.
- **The 0.85 merge threshold** no longer applies to images (design 4.4). For text,
  EG2 paraphrases score 0.75 to 0.87 and distinct names 0.66 to 0.76: no distinct pair
  reaches 0.85, so the threshold is safe, but most paraphrases fall under it too (mean
  0.83, maximum 0.866): EG2 text under-merges at 0.85, the safe direction.
- **Cold start (PR 3's open question).** Recall's final score is
  `0.5 x daemon + 0.5 x query`, and a concept derived since the daemon's last cycle has
  no daemon score. A fresh relevant image therefore scores about 0.38 and a fresh
  relevant text about 0.42, and both lose to the older, daemon-scored noise concept
  at 0.533 from PR 3's run. It is modality-independent (spec 8 behaviour for any fresh
  concept), and EG2's high cosine floor makes it slightly worse: an older irrelevant
  concept already gets about 0.30 from its query leg at EG2's 0.61 mean, so a daemon
  score above about 0.16 lets it outrank a fresh, perfectly captioned image. After the
  daemon's next cycle the image competes on its daemon score like any concept. No
  change in this PR (design 7.3: measured, not adjusted); this is evidence for Q15.

The reference set is small (3 images, 4 documents, 8 pairs) and synthetic (solid
colours and a checkerboard). It shows the shape of the distribution, not a
calibration.

## Not verified here

- Numeric agreement with the reference model (transformers or sentence-transformers,
  BF16) and the Q8_0 quantization drift: the optional fidelity check of the amendment's
  section 3, blocking for Dresscode's browser vectors.
- Ollama or a hosted endpoint behind this kind (the adapter treats a missing `/props` as
  "unchecked" and logs it once; httpmock covers that path).
