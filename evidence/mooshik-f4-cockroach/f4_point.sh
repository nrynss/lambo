#!/usr/bin/env bash
# Lean F4 point: fire N at-cap record_action writes, SIGTERM IMMEDIATELY when
# the burst ends (no store probe in the timing path), measure the close-flush
# window (winding down -> session closed tail durable, or the 8 s grace
# timeout), and the durable-intent tail the close had to carry (approx N minus
# what the write-behind daemon drained during the burst).
set -uo pipefail

REPO="/home/nryn/work/lambo"
BIN="$REPO/target/release/lambo"
N="$1"
SESSION="$2"
OUT="$3"
PORT="${4:-8100}"
PER="${5:-16}"

mkdir -p "$OUT"
TOKEN="scratch-$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')"
export LAMBO_AUTH_TOKEN="$TOKEN"
export RUST_LOG="${RUST_LOG:-lambo=info}"
STDERR="$OUT/stderr-$SESSION-$N.log"
LEDGER="$OUT/ledger-$SESSION-$N.jsonl"
CFG="$REPO/evidence/mooshik-f4-cockroach/lambo.cockroach.toml"
DSN="${LAMBO_COCKROACH_DSN:-}"
: > "$LEDGER"

"$BIN" serve --config "$CFG" --session "$SESSION" --agent f4-capture \
    --transport http --port "$PORT" --bind 127.0.0.1 > "$STDERR" 2>&1 &
SERVE_PID=$!
for _ in $(seq 1 120); do
  grep -q "listening on /mcp" "$STDERR" 2>/dev/null && break
  sleep 0.1
done

python3 "$REPO/evidence/mooshik-f4-cockroach/f4_drive.py" \
    --n "$N" --ledger "$LEDGER" --per "$PER" \
    --endpoint "http://127.0.0.1:${PORT}/mcp" \
    > "$OUT/driver-$SESSION-$N.stdout" 2>> "$STDERR" &
DRIVER_PID=$!
for _ in $(seq 1 2000); do
  grep -q "BURST_DONE" "$OUT/driver-$SESSION-$N.stdout" 2>/dev/null && break
  kill -0 "$DRIVER_PID" 2>/dev/null || break
  sleep 0.01
done
DRIVER_SECS=$(awk '/BURST_DONE/{print $NF}' "$OUT/driver-$SESSION-$N.stdout" | sed 's/s//' 2>/dev/null)
ACCEPTED=$(grep -c '"is_error": false' "$LEDGER" 2>/dev/null); ACCEPTED=${ACCEPTED:-0}

T0="$(date +%s%N)"
kill -TERM "$SERVE_PID" 2>/dev/null || true

EXIT_CODE=""
for _ in $(seq 1 600); do
  if ! kill -0 "$SERVE_PID" 2>/dev/null; then
    wait "$SERVE_PID" 2>/dev/null; EXIT_CODE=$?; break
  fi
  sleep 0.05
done
T1="$(date +%s%N)"
[[ -z "$EXIT_CODE" ]] && EXIT_CODE="TIMEOUT"
SIG_TO_EXIT_MS=$(( (T1 - T0) / 1000000 ))
wait "$DRIVER_PID" 2>/dev/null

W=$(grep "shutdown signal received, winding down" "$STDERR" | head -1 | awk '{print $1" "$2}')
E=$(grep "lambo serve: session closed, tail durable" "$STDERR" | head -1 | awk '{print $1" "$2}')
FLUSH_MS="NA"
if [[ -n "$W" && -n "$E" ]]; then
  WE=$(python3 -c "import datetime;print(int(datetime.datetime.fromisoformat('$W'+'Z').timestamp()*1e6))" 2>/dev/null)
  EE=$(python3 -c "import datetime;print(int(datetime.datetime.fromisoformat('$E'+'Z').timestamp()*1e6))" 2>/dev/null)
  if [[ -n "$WE" && -n "$EE" ]]; then FLUSH_MS=$(( (EE - WE) / 1000 )); fi
fi
VERDICT="unknown"
grep -q "tail durable" "$STDERR" && VERDICT="tail-durable"
grep -q "did not finish within the grace window" "$STDERR" && VERDICT="tail-lost"

K_POST=""; CONCEPTS_POST=""
if [[ -n "$DSN" ]]; then
  K_POST=$(psql "$DSN" -At -c "SELECT count(*) FROM write_intents WHERE session_id='$SESSION';" 2>/dev/null)
  CONCEPTS_POST=$(psql "$DSN" -At -c "SELECT count(*) FROM concepts WHERE session_id='$SESSION';" 2>/dev/null)
fi
echo "RESULT session=$SESSION N=$N per=$PER accepted=$ACCEPTED burst_secs=${DRIVER_SECS:-?} flush_ms=$FLUSH_MS sig_to_exit_ms=$SIG_TO_EXIT_MS exit=$EXIT_CODE verdict=$VERDICT k_post=${K_POST:-NA} concepts=${CONCEPTS_POST:-NA}"
