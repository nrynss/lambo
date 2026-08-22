#!/usr/bin/env bash
# Compile time and binary size for the spike, per feature configuration.
#
# The headline compile number is the **from-clean** build of the
# K2-representative configuration (metal + accelerate), because that is what a
# contributor or a CI row would actually pay. The other three configurations are
# built afterwards with the dependency graph already warm, so their times are
# *incremental* and are labelled as such rather than quoted as clean builds.
#
# Usage: bash evidence/mooshik-k1-metal/cost_build.sh <repo-root>
set -euo pipefail

ROOT="${1:?repo root}"
CRATE="$ROOT/spikes/k1-candle-bgem3"
OUT="$ROOT/evidence/mooshik-k1-metal/cost-build.jsonl"
BINDIR="$ROOT/evidence/mooshik-k1-metal"

cd "$CRATE"
: > "$OUT"

emit() { # name features seconds kind
  local bin="$CRATE/target/release/k1-candle-bgem3"
  local size stripped
  size=$(stat -f%z "$bin")
  cp "$bin" /tmp/k1-strip-probe
  strip /tmp/k1-strip-probe 2>/dev/null || true
  stripped=$(stat -f%z /tmp/k1-strip-probe)
  rm -f /tmp/k1-strip-probe
  printf '{"config":"%s","features":"%s","compile_s":%s,"compile_kind":"%s","binary_bytes":%s,"binary_stripped_bytes":%s}\n' \
    "$1" "$2" "$3" "$4" "$size" "$stripped" | tee -a "$OUT"
}

echo "=== clean build: metal + accelerate (K2-representative) ===" >&2
cargo clean
S=$(date +%s.%N)
cargo build --release --features metal,accelerate >&2
E=$(date +%s.%N)
emit "metal+accelerate" "metal,accelerate" "$(echo "$E - $S" | bc)" "from-clean"

for cfg in "cpu-plain:" "cpu-accelerate:accelerate" "metal:metal"; do
  name="${cfg%%:*}"; feats="${cfg#*:}"
  echo "=== warm build: $name ===" >&2
  S=$(date +%s.%N)
  if [ -z "$feats" ]; then
    cargo build --release >&2
  else
    cargo build --release --features "$feats" >&2
  fi
  E=$(date +%s.%N)
  emit "$name" "$feats" "$(echo "$E - $S" | bc)" "warm-deps"
done

# Restore the configuration the measurement legs used.
cargo build --release --features metal,accelerate >&2

cargo tree --features metal,accelerate 2>/dev/null \
  | grep -E 'candle|tokenizers|hf-hub' | sort -u > "$BINDIR/cargo-tree-versions.txt"
echo "wrote $BINDIR/cargo-tree-versions.txt" >&2
