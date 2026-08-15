#!/usr/bin/env bash
# Result 4c (retry): fencing-token / expiry latency + takeover replayed. live-fencing2 session.
set -uo pipefail
cd /home/nryn/work/lambo
set -a; source /home/nryn/work/lambo/.env; set +a
export LAMBO_STORE=cockroach LAMBO_EMBEDDER=bge_m3 LAMBO_LLAMA_EMBED_URL=http://127.0.0.1:8080
BIN=/home/nryn/work/lambo/target/debug/lambo
CFG=/tmp/lambo-live.toml
PSQL=/home/nryn/.local/bin/psql
q() { "$PSQL" "$LAMBO_COCKROACH_DSN" -tAc "$1" 2>/dev/null; }

S=live-fencing2
q "DELETE FROM session_leases WHERE session_id='$S';" >/dev/null 2>&1 || true
q "DELETE FROM concepts WHERE session_id='$S';" >/dev/null 2>&1 || true

echo "### seed durable state via CLI derive (correct kind case)"
"$BIN" --config "$CFG" derive --session "$S" --agent fence-a --content "fencing marker concept" --kind entity >/tmp/f2-derive.log 2>&1
echo "derive_exit=$? concepts=$(q "SELECT count(*) FROM concepts WHERE session_id='$S';")"

echo "### start A, kill -9 (crash); measure expiry latency"
tail -f /dev/null | "$BIN" --config "$CFG" serve --session "$S" --agent agent-a --transport stdio >/tmp/f2-a.log 2>&1 &
A_PID=$!
for i in $(seq 1 30); do ROW=$(q "SELECT holder FROM session_leases WHERE session_id='$S';"); [ -n "$ROW" ] && break; sleep 0.5; done
echo "A_holder='$ROW'"
T0=$(date +%s)
kill -9 "$A_PID" 2>/dev/null
ELAPSED=-1
for i in $(seq 1 60); do
  ACTIVE=$(q "SELECT count(*) FROM session_leases WHERE session_id='$S' AND expires_at > now();")
  if [ "$ACTIVE" = "0" ]; then ELAPSED=$(( $(date +%s) - T0 )); break; fi
  sleep 1
done
echo "expiry_elapsed_seconds=$ELAPSED (TTL=45s)"

echo "### B acquires after expiry (stdin held open so it holds the lease)"
tail -f /dev/null | "$BIN" --config "$CFG" serve --session "$S" --agent agent-b --transport stdio >/tmp/f2-b.log 2>&1 &
B_PID=$!
for i in $(seq 1 30); do ROW=$(q "SELECT holder FROM session_leases WHERE session_id='$S';"); echo "$ROW" | grep -q 'agent-b' && break; sleep 0.5; done
echo "B_holder='$ROW' (expect agent-b)"

echo "### B replays durable state at startup (existing=true, concepts>0)"
grep -oE 'existing=(true|false)' /tmp/f2-b.log | head -1
grep -oE 'materialised [0-9]+ (node|concept)[^ ]*|loaded [0-9]+' /tmp/f2-b.log | head -3

echo "### B sees the seeded concept via recall (replay check, reader-safe? B is writer: use saints/stats via fresh reader)"
"$BIN" --config "$CFG" saints --session "$S" 2>&1 | head -8

echo "### cleanup"
kill -INT "$B_PID" 2>/dev/null; sleep 2
q "DELETE FROM session_leases WHERE session_id='$S';" >/dev/null 2>&1 || true
kill -TERM "$A_PID" "$B_PID" 2>/dev/null || true
echo DONE
