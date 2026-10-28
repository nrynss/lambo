#!/usr/bin/env bash
# Regenerate the #181 evidence. Needs the candle BGE-M3 weights in the HF
# cache (BAAI/bge-m3@5617a9f6; no network with HF_HUB_OFFLINE=1). Optional:
# CACHE_BGE, an embedding cache in eval_measure's format, and PAPER, a
# checkout of nrynss/lambo-paper for the 2026-09-07 replay.
# Usage: bash evidence/issue-181/run.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
# Writes to a new directory by default: runs/ is the evidence and is never
# overwritten. Compare with: python3 evidence/issue-181/summarise.py "$OUT/bge-m3-candle-cpu.json"
OUT="${OUT:-evidence/issue-181/runs-rerun}"
real() { python3 -c 'import os, sys; print(os.path.realpath(sys.argv[1]))' "$1"; }
if [ "$(real "$OUT")" = "$(real evidence/issue-181/runs)" ]; then
  echo "refusing to overwrite the archived runs/; set OUT to a new directory" >&2
  exit 2
fi
cargo build --release --features embed-candle,store-sqlite --example eval_redundancy
env -u LAMBO_EMBED_DIM -u LAMBO_LLAMA_EMBED_URL -u LAMBO_LLAMA_MODEL \
  HF_HUB_OFFLINE=1 LAMBO_EMBEDDER=candle LAMBO_EMBED_DEVICE=cpu \
  "${CARGO_TARGET_DIR:-target}/release/examples/eval_redundancy" \
  ${CACHE_BGE:+--cache "$CACHE_BGE"} --out "$OUT/bge-m3-candle-cpu.json"
python3 evidence/issue-181/summarise.py "$OUT/bge-m3-candle-cpu.json"
cargo run --release --example bench_redundancy
if [ -n "${PAPER:-}" ]; then
  python3 evidence/issue-181/hitk_replay.py "$PAPER/evaluation/results/2026-09-07"
fi
