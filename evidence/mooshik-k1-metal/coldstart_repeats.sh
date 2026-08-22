#!/usr/bin/env bash
# Cold start, five process launches per backend.
#
# Each line is one *fresh process*: `coldstart` timestamps at `main()` entry, so
# `process_to_first_vector_ms` is what a stdio client spawning a serve per
# session would actually wait for. Weights are on disk and in the page cache —
# stated rather than hidden, because that is the steady-state case the ~30 s
# client-abandon gate is about; the first-run fetch is measured separately.
#
# Usage: bash evidence/mooshik-k1-metal/coldstart_repeats.sh <repo-root>
set -euo pipefail

ROOT="${1:?repo root}"
BIN="$ROOT/spikes/k1-candle-bgem3/target/release/k1-candle-bgem3"
OUT="$ROOT/evidence/mooshik-k1-metal/coldstart.jsonl"

: > "$OUT"
for spec in "cpu f32" "metal f32" "metal f16"; do
  set -- $spec
  for r in 1 2 3 4 5; do
    echo "--- $1 $2 run $r ---" >&2
    "$BIN" coldstart --device "$1" --dtype "$2" | tee -a "$OUT"
  done
done
