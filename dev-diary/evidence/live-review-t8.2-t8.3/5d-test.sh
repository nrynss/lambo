#!/usr/bin/env bash
# Result 5d (N3/N4) — unreachable embedder: does recall degrade or hang? Does anything leak?
set -uo pipefail
cd /home/nryn/work/lambo
set -a; source /home/nryn/work/lambo/.env; set +a
export LAMBO_STORE=cockroach LAMBO_EMBEDDER=bge_m3
BIN=/home/nryn/work/lambo/target/debug/lambo
S=live-mcp-5d3

cat > /tmp/lambo-bad-embed.toml <<'EOF'
[store]
kind = "cockroach"

[embedder]
kind = "bge_m3"
dim = 1024
url = "http://127.0.0.1:59999"
EOF

PSQL=/home/nryn/.local/bin/psql
"$PSQL" "$LAMBO_COCKROACH_DSN" -c "DELETE FROM concepts WHERE session_id='$S';" >/dev/null 2>&1
"$PSQL" "$LAMBO_COCKROACH_DSN" -c "DELETE FROM session_leases WHERE session_id='$S';" >/dev/null 2>&1

# 1) seed durable state + real vectors with the GOOD embedder
cat > /tmp/lambo-good-embed.toml <<'EOF'
[store]
kind = "cockroach"

[embedder]
kind = "bge_m3"
dim = 1024
url = "http://127.0.0.1:8080"
EOF
"$BIN" --config /tmp/lambo-good-embed.toml derive --session "$S" --agent seed --content "billing service retries failed charges" --kind entity >/tmp/5d-seed.log 2>&1
echo "seed_exit=$? concepts=$("$PSQL" "$LAMBO_COCKROACH_DSN" -tAc "SELECT count(*) FROM concepts WHERE session_id='$S';")"

# 2) reopen with the BAD embedder, drive initialize + one recall over stdio, 20s hard cap
T0=$(date +%s)
timeout 20 "$BIN" --config /tmp/lambo-bad-embed.toml serve --session "$S" --agent a --transport stdio \
  > /tmp/5d-out.jsonl 2> /tmp/5d-err.log <<'EOF'
{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"5d","version":"1"}}}
{"jsonrpc":"2.0","method":"notifications/initialized"}
{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"lambo_recall","arguments":{"agent_id":"a","query":"what retries failed billing charges","top_k":5}}}
EOF
RC=$?
ELAPSED=$(( $(date +%s) - T0 ))
echo "serve_exit=$RC elapsed=${ELAPSED}s (124=timed out)"

echo "=== wire responses ==="
cat /tmp/5d-out.jsonl
echo "=== stderr (warn/error lines) ==="
grep -iE 'warn|error|degrad|hybrid|embed|fallback' /tmp/5d-err.log | head -20
