#!/usr/bin/env bash
# Regenerate runs-v2/: the re-run on the 368-query set (58 queries added in
# fixtures/eval/authoring/t_more173.py). Same harness and environment as
# run.sh; a shorter dev sweep, then the same three held-out variants once.
# Usage: CACHE_BGE=bge.jsonl CACHE_EG2=eg2.jsonl bash evidence/issue-173-fusion/run-v2.sh
set -euo pipefail
cd "$(dirname "$0")/../.."
# Writes to a new directory by default: the archived runs-v2/ is the evidence
# and is never overwritten (the fixture set may have changed since). Compare
# with: python3 evidence/issue-173-fusion/summarise.py "$OUT"
OUT="${OUT:-evidence/issue-173-fusion/runs-v2-rerun}"
case "$(basename "$OUT")" in
  runs|runs-v2) echo "refusing to overwrite the archived $OUT; set OUT to a new directory" >&2; exit 2 ;;
esac
DEV="max rrf rrf:k=60 rrf:k=2 rrf:k=5 rrf:k=10 rrf:k=3,wd=0"
CONFIRM="max rrf:k=60 rrf"
args() { for v in $1; do printf -- '--variant %s ' "$v"; done; }
cargo build --release --features embed-candle,embed-eg2,store-sqlite --example eval_fusion
BIN="${CARGO_TARGET_DIR:-target}/release/examples/eval_fusion"
bge() {
  env -u LAMBO_EMBED_DIM -u LAMBO_LLAMA_EMBED_URL -u LAMBO_LLAMA_MODEL \
    LAMBO_EMBEDDER=candle LAMBO_EMBED_DEVICE=cpu \
    "$BIN" ${CACHE_BGE:+--cache "$CACHE_BGE"} "$@"
}
eg2() {
  env -u LAMBO_EMBED_DEVICE LAMBO_EMBEDDER=eg2 LAMBO_EMBED_DIM=768 \
    LAMBO_LLAMA_EMBED_URL="${LAMBO_LLAMA_EMBED_URL:-http://127.0.0.1:8080}" \
    LAMBO_LLAMA_MODEL=ggml-org/embeddinggemma-2-GGUF@bfcd2987/Q8_0 \
    "$BIN" ${CACHE_EG2:+--cache "$CACHE_EG2"} "$@"
}
# shellcheck disable=SC2046
bge $(args "$DEV") --out "$OUT/bge-m3-candle-cpu-dev.json"
# shellcheck disable=SC2046
bge --held-out $(args "$CONFIRM") --out "$OUT/bge-m3-candle-cpu-confirm.json"
# shellcheck disable=SC2046
eg2 $(args "$DEV") --out "$OUT/eg2-q8-768-dev.json"
# shellcheck disable=SC2046
eg2 --held-out $(args "$CONFIRM") --out "$OUT/eg2-q8-768-confirm.json"
python3 evidence/issue-173-fusion/summarise.py "$OUT"
