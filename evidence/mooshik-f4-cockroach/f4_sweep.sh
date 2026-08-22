#!/usr/bin/env bash
# F4 sweep: burst N at-cap record_action writes at one scratch server, let the
# write-behind daemon settle one cycle, measure the durable intents that the
# close flush must carry (K), SIGTERM, and time the close-flush.
#
# K (durable-intent tail the close must flush) = N accepted - intents already
# durable in the store when we pull the SIGTERM (the daemon flushes on a 1 s
# cadence / 500-mutations, so some of N land before close).
#
# flush_ms = signal -> "session closed, tail durable" when the close succeeds;
# for an abandoned close we report the 8 s grace timeout (tail lost, exit 1).
# Meticulous but plain bash; no set -e so RESULT always prints.
set -uo pipefail

REPO="/home/nryn/work/lambo"
BIN="$REPO/target/release/lambo"
N="$1"
SESSION="$2"
OUT="$3"
PORT="${4:-7980}"
PER="${5:-64}"
SETTLE="${6:-1.0}"

mkdir -p "$OUT"
TOKEN="scratch-$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')"
export LAMBO_AUTH_TOKEN="$TOKEN"
export RUST_LOG="${RUST_LOG:-lambo=info}"

STDERR="$OUT/stderr-$SESSION-$N.log"
LEDGER="$OUT/ledger-$SESSION-$N.jsonl"
CFG="$REPO/evidence/mooshik-f4-cockroach/lambo.cockroach.toml"
DSN="${LAMBO_COCKROACH_DSN:-}"
: > "$LEDGER"

cleanup() {
  if [[ -n "${SERVE_PID:-}" ]] && kill -0 "$SERVE_PID" 2>/dev/null; then
    kill -TERM "$SERVE_PID" 2>/dev/null || true
  fi
}
trap cleanup EXIT

"$BIN" serve --config "$CFG" --session "$SESSION" --agent f4-capture \
    --transport http --port "$PORT" --bind 127.0.0.1 > "$STDERR" 2>&1 &
SERVE_PID=$!
for _ in $(seq 1 120); do
  grep -q "listening on /mcp" "$STDERR" 2>/dev/null && break
  kill -0 "$SERVE_PID" 2>/dev/null || { echo "serve died early"; exit 1; }
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
wait "$DRIVER_PID" 2>/dev/null

ACCEPTED=$(grep -c '"is_error": false' "$LEDGER" 2>/dev/null); ACCEPTED=${ACCEPTED:-0}

# Let the write-behind daemon do a settle cycle, then measure its durable count.
sleep "$SETTLE"
PRE=""
if [[ -n "$DSN" ]]; then
  PRE=$(psql "$DSN" -At -c "SELECT count(*) FROM write_intents WHERE session_id='$SESSION';" 2>/dev/null)
fi
PRE="${PRE:-0}"
K_CLOSE=$(( ACCEPTED - PRE ))

T0="$(date +%s%N)"
kill -TERM "$SERVE_PID" 2>/dev/null || true

EXIT_CODE=""
for _ in $(seq 1 600); do
  if ! kill -0 "$SERVE_PID" 2>/dev/null; then
    wait "$SERVE_PID" 2>/dev/null; EXIT_CODE=$?
    break
  fi
  sleep 0.05
done
T1="$(date +%s%N)"
[[ -z "$EXIT_CODE" ]] && EXIT_CODE="TIMEOUT"
SIG_TO_EXIT_MS=$(( (T1 - T0) / 1000000 ))

FLUSH_MS="NA"
WIND_TS=$(grep "shutdown signal received, winding down" "$STDERR" | head -1 | awk '{print $1" "$2}')
DUR_TS=$(grep "lambo serve: session closed, tail durable" "$STDERR" | head -1 | awk '{print $1" "$2}')
if [[ -n "$WIND_TS" && -n "$DUR_TS" ]]; then
  W_E=$(python3 -c "import datetime,sys;print(int(datetime.datetime.fromisoformat('$WIND_TS'+'Z').timestamp()*1e6))" 2>/dev/null)
  D_E=$(python3 -c "import datetime,sys;print(int(datetime.datetime.fromisoformat('$DUR_TS'+'Z').timestamp()*1e6))" 2>/dev/null)
  if [[ -n "$W_E" && -n "$D_E" ]]; then FLUSH_MS=$(( (D_E - W_E) / 1000 )); fi
fi

VERDICT="unknown"
grep -q "lambo serve: session closed, tail durable" "$STDERR" && VERDICT="tail-durable"
grep -q "did not finish within the grace window" "$STDERR" && VERDICT="tail-lost"
TAIL_MUT=$(grep "Memory session closed (tail flushed)" "$STDERR" | tail -1 | sed -nE 's/.*mutations=([0-9]+).*/\1/p')
TAIL_MUT="${TAIL_MUT:-0}"

K_POST=""; CONCEPTS_POST=""
if [[ -n "$DSN" ]]; then
  K_POST=$(psql "$DSN" -At -c "SELECT count(*) FROM write_intents WHERE session_id='$SESSION';" 2>/dev/null)
  CONCEPTS_POST=$(psql "$DSN" -At -c "SELECT count(*) FROM concepts WHERE session_id='$SESSION';" 2>/dev/null)
fi

echo "RESULT session=$SESSION N=$N per=$PER accepted=$ACCEPTED pre=$PRE k_close=$K_CLOSE sig_to_exit_ms=$SIG_TO_EXIT_MS flush_ms=$FLUSH_MS tail_mutations=$TAIL_MUT exit=$EXIT_CODE verdict=$VERDICT k_post=$K_POST concepts=$CONCEPTS_POST"
