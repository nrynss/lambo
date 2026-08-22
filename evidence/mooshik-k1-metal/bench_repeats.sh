#!/usr/bin/env bash
# Three repeats of each candle throughput leg, plus the llama.cpp reference on
# the same harness. The recorded branch baseline was itself quoted across
# repeats (110, 131, 141), so a single reading is not comparable to it.
#
# Usage: bash evidence/mooshik-k1-metal/bench_repeats.sh <repo-root> <llama-url>
set -euo pipefail

ROOT="${1:?repo root}"
URL="${2:-http://127.0.0.1:8099}"
BIN="$ROOT/spikes/k1-candle-bgem3/target/release/k1-candle-bgem3"
EV="$ROOT/evidence/mooshik-k1-metal"

SIZES=35,1024
ITERS=32
WARMUP=4
CONC=4

run_candle() {
  local dev="$1" dt="$2" out="$3"
  : > "$out"
  for r in 1 2 3; do
    echo "--- repeat $r: candle $dev $dt ---" >&2
    "$BIN" bench --device "$dev" --dtype "$dt" \
      --sizes "$SIZES" --concurrency "$CONC" --iters "$ITERS" --warmup "$WARMUP" \
      | tee -a "$out"
  done
}

run_candle metal f16 "$EV/bench-repeats-candle-metal-f16.jsonl"
run_candle metal f32 "$EV/bench-repeats-candle-metal-f32.jsonl"
run_candle cpu   f32 "$EV/bench-repeats-candle-cpu-accelerate-f32.jsonl"

OUT="$EV/bench-repeats-llama-q8.jsonl"
: > "$OUT"
for r in 1 2 3; do
  echo "--- repeat $r: llama.cpp q8_0 ---" >&2
  python3 "$EV/bench_llama.py" --url "$URL" \
    --sizes "$SIZES" --concurrency "$CONC" --iters "$ITERS" --warmup "$WARMUP" \
    | tee -a "$OUT"
done
