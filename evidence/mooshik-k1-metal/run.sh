#!/usr/bin/env bash
# K1 — the whole capture, start to finish. Committed so the run is reproducible
# rather than described.
#
#   bash evidence/mooshik-k1-metal/run.sh <repo-root>
#
# Prerequisites:
#   * a llama-server on $REF_URL serving the DOGFOOD-SETUP GGUF
#     (bge-m3-q8_0.gguf, sha256 aa473d51...a173). Start one that is NOT the
#     rig's own :8080 server, with room for an 8 KiB input:
#       llama-server --embedding -m ~/models/bge-m3-q8_0.gguf \
#         --port 8099 --host 127.0.0.1 -c 8192 -b 8192 -ub 8192
#   * network on first run only: hf-hub fetches BAAI/bge-m3 into the default
#     HF cache OUTSIDE the repo. Weights are never committed.
set -euo pipefail

ROOT="${1:?repo root}"
REF_URL="${2:-http://127.0.0.1:8099}"
EV="$ROOT/evidence/mooshik-k1-metal"
CRATE="$ROOT/spikes/k1-candle-bgem3"
BIN="$CRATE/target/release/k1-candle-bgem3"

echo "### 0. build (metal + accelerate)"
cargo build --release --manifest-path "$CRATE/Cargo.toml" --features metal,accelerate

echo "### 1. corpus"
python3 "$EV/corpus.py" > "$EV/corpus.jsonl"
wc -l < "$EV/corpus.jsonl"

echo "### 2. first-run weight fetch (timed; no-op once cached)"
"$BIN" fetch

echo "### 3. embed both sides"
# The reference is llama.cpp q8_0 — a different tokenizer, graph and
# quantization. It is written out exactly as returned: no re-normalization, no
# truncation, no pooling change. A fix-up here would land on both sides of the
# comparison and void the gate.
python3 "$EV/reference_llama.py" --corpus "$EV/corpus.jsonl" --url "$REF_URL" \
  --out "$EV/vectors-llama-q8.jsonl"

for spec in "cpu f32" "metal f32" "metal f16"; do
  set -- $spec
  "$BIN" embed --input "$EV/corpus.jsonl" \
    --out "$EV/vectors-candle-$1-$2.jsonl" --device "$1" --dtype "$2"
done

echo "### 3b. reference cross-check against the rig's own :8080 server"
# Confirms the -c/-b/-ub capacity flags above changed capacity only, not the
# vectors: same GGUF, default flags, must agree at cosine 1.000000.
grep '"group": "evidence"' "$EV/corpus.jsonl" > /tmp/k1-subset.jsonl
python3 "$EV/reference_llama.py" --corpus /tmp/k1-subset.jsonl \
  --url http://127.0.0.1:8080 \
  --out "$EV/vectors-llama-q8-port8080-crosscheck.jsonl" || true

echo "### 4. parity (number 1)"
for spec in "cpu-f32" "metal-f32" "metal-f16"; do
  python3 "$EV/compare.py" \
    --a "$EV/vectors-candle-$spec.jsonl" \
    --b "$EV/vectors-llama-q8.jsonl" \
    --corpus "$EV/corpus.jsonl" \
    --label "candle-$spec vs llama.cpp-q8_0" \
    --out-json "$EV/parity-candle-$spec.json" \
    --out-csv "$EV/parity-candle-$spec.csv"
done
python3 "$EV/compare.py" \
  --a "$EV/vectors-llama-q8-port8080-crosscheck.jsonl" \
  --b "$EV/vectors-llama-q8.jsonl" \
  --corpus /tmp/k1-subset.jsonl \
  --label "llama :8080 (baseline flags) vs llama :8099 (K1 flags)" \
  --out-json "$EV/parity-reference-crosscheck.json" \
  --out-csv "$EV/parity-reference-crosscheck.csv"

echo "### 5. throughput (number 2)"
bash "$EV/bench_repeats.sh" "$ROOT" "$REF_URL"

echo "### 6. cost (number 3)"
bash "$EV/coldstart_repeats.sh" "$ROOT"
bash "$EV/cost_build.sh" "$ROOT"

echo "### 7. summary"
python3 "$EV/summarize.py" "$EV"

echo "### 8. shrink the raw vector captures for commit"
gzip -9 -f "$EV"/vectors-*.jsonl
