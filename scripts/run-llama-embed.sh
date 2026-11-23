#!/usr/bin/env bash
# run-llama-embed.sh — start llama.cpp as a local embedding server for BGE-M3 (P7 T7.0).
#
# Lambo then talks to it over HTTP at LAMBO_LLAMA_EMBED_URL (default
# http://127.0.0.1:8080) via the OpenAI-compatible /v1/embeddings endpoint.
#
# Usage:
#   ./scripts/fetch-bge-m3.sh            # once, weights into models/
#   ./scripts/run-llama-embed.sh         # foreground
#   ./scripts/run-llama-embed.sh --check # check an already-running server, long input included
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

# --- resolve settings ---------------------------------------------------------
URL="${LAMBO_LLAMA_EMBED_URL:-http://127.0.0.1:8080}"
HOST="${LAMBO_LLAMA_EMBED_HOST:-127.0.0.1}"
PORT="${LAMBO_LLAMA_EMBED_PORT:-8080}"
# If URL carries a port different from LAMBO_LLAMA_EMBED_PORT, take it from the URL.
if [[ "$URL" == *":"* ]]; then
  URL_PORT="${URL##*:}"
  URL_PORT="${URL_PORT%%/*}"
  if [[ "$URL_PORT" =~ ^[0-9]+$ ]]; then
    PORT="$URL_PORT"
  fi
fi

MODEL="${LAMBO_BGE_M3_MODEL:-$ROOT/models/bge-m3/bge-m3-FP16.gguf}"
CTX="${LAMBO_LLAMA_EMBED_CTX:-8192}"
# An embedding is computed in one physical batch, so the batch sizes must cover
# the longest input. llama-server defaults to a 512-token ubatch and answers
# HTTP 500 for longer input while /health still passes; match both to the context.
BATCH="${LAMBO_LLAMA_EMBED_BATCH:-$CTX}"

check_health() {
  curl -fsS --max-time 2 "${URL%/}/health" >/dev/null 2>&1
}

# /health passes even on a server whose batch is too small for real input, so
# also embed one text of more than 512 tokens: a server started without
# --batch-size/--ubatch-size answers it with HTTP 500.
check_long_input() {
  local text body code
  text="$(printf 'lambo%.0s ' $(seq 1 700))"
  body="$(printf '{"input": "%s"}' "$text")"
  code="$(curl -sS --max-time 60 -o /dev/null -w '%{http_code}' \
    -H 'Content-Type: application/json' -d "$body" "${URL%/}/v1/embeddings" 2>/dev/null || true)"
  [[ "$code" == "200" ]]
}

LONG_INPUT_HINT="it answers /health but cannot embed a 700-word input: restart it with \
--batch-size and --ubatch-size at least as large as your longest input (this script does)"

if [[ "${1:-}" == "--check" ]]; then
  if ! check_health; then
    echo "no llama.cpp server responding at ${URL}" >&2
    exit 1
  fi
  if ! check_long_input; then
    echo "llama.cpp server at ${URL}: $LONG_INPUT_HINT" >&2
    exit 1
  fi
  echo "llama.cpp embedding server OK at ${URL} (long input embedded)"
  exit 0
fi

if [[ ! -f "$MODEL" ]]; then
  echo "error: model not found at $MODEL" >&2
  echo "  run ./scripts/fetch-bge-m3.sh first, or set LAMBO_BGE_M3_MODEL" >&2
  exit 1
fi

SERVER="${LAMBO_LLAMA_CPP_SERVER:-llama-server}"
command -v "$SERVER" >/dev/null 2>&1 || {
  echo "error: '$SERVER' not found on PATH (install llama.cpp)" >&2
  exit 1
}

if check_health; then
  if check_long_input; then
    echo "a server is already running at ${URL} and embeds long input — exiting."
    exit 0
  fi
  echo "a server is already running at ${URL}: $LONG_INPUT_HINT" >&2
  exit 1
fi

echo "== starting llama.cpp embedding server =="
echo "  model : $MODEL"
echo "  bind  : $HOST:$PORT"
echo "  url   : ${URL}"
echo "  ctx   : $CTX"
echo "  batch : $BATCH"

# `--embedding` exposes the /v1/embeddings (and /embedding) endpoints.
exec "$SERVER" \
  -m "$MODEL" \
  --host "$HOST" \
  --port "$PORT" \
  -c "$CTX" \
  --batch-size "$BATCH" \
  --ubatch-size "$BATCH" \
  --embedding \
  "$@"
