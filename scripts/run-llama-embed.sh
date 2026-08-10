#!/usr/bin/env bash
# run-llama-embed.sh — start llama.cpp as a local embedding server for BGE-M3 (P7 T7.0).
#
# Lambo then talks to it over HTTP at LAMBO_LLAMA_EMBED_URL (default
# http://127.0.0.1:8080) via the OpenAI-compatible /v1/embeddings endpoint.
#
# Usage:
#   ./scripts/fetch-bge-m3.sh            # once, weights into models/
#   ./scripts/run-llama-embed.sh         # foreground
#   ./scripts/run-llama-embed.sh --check # health-check an already-running server
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

MODEL="${LAMBO_BGE_M3_MODEL:-$ROOT/models/bge-m3/bge-m3-f16.gguf}"
CTX="${LAMBO_LLAMA_EMBED_CTX:-8192}"

check_health() {
  curl -fsS --max-time 2 "${URL%/}/health" >/dev/null 2>&1
}

if [[ "${1:-}" == "--check" ]]; then
  if check_health; then
    echo "llama.cpp embedding server OK at ${URL}"
    exit 0
  fi
  echo "no llama.cpp server responding at ${URL}" >&2
  exit 1
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
  echo "a server is already running at ${URL} — exiting (use --check to verify)."
  exit 0
fi

echo "== starting llama.cpp embedding server =="
echo "  model : $MODEL"
echo "  bind  : $HOST:$PORT"
echo "  url   : ${URL}"
echo "  ctx   : $CTX"

# `--embedding` exposes the /v1/embeddings (and /embedding) endpoints.
exec "$SERVER" \
  -m "$MODEL" \
  --host "$HOST" \
  --port "$PORT" \
  -c "$CTX" \
  --embedding \
  "$@"
