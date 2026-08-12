#!/usr/bin/env bash
# fetch-bge-m3.sh — download BGE-M3 GGUF weights for llama.cpp (P7 T7.0).
#
# Weights are large and gitignored (`models/`). Never commit them.
# Record the exact repo + revision you download in the Handoff Log so the demo is
# reproducible (see dev-diary/notes/embeddings-portable.md).
#
# Usage:
#   ./scripts/fetch-bge-m3.sh              # default repo/file below
#   ./scripts/fetch-bge-m3.sh --dry-run    # print what would be downloaded
set -euo pipefail

ROOT="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$ROOT"

if [[ -f .env ]]; then
  set -a
  # shellcheck disable=SC1091
  source .env
  set +a
fi

# Where to put weights (gitignored).
MODEL_DIR="${LAMBO_BGE_M3_DIR:-$ROOT/models/bge-m3}"

# Repo + file on Hugging Face. Override for another build.
REPO="${LAMBO_BGE_M3_HF_REPO:-gpustack/bge-m3-GGUF}"
FILE="${LAMBO_BGE_M3_GGUF:-bge-m3-FP16.gguf}"
# Pinned revision (sha) for reproducibility - see dev-diary notes. Override to track another.
REVISION="${LAMBO_BGE_M3_REVISION:-2d48f1737679ad900d5c26c5aad5410e9c70fdca}"

# Pinned sha256 of $FILE at $REVISION (run `sha256sum` on the downloaded file
# to update after intentionally switching builds). A partial or corrupt
# download must never be trusted: any mismatch is a hard error.
EXPECTED_SHA256="daec91ffb5dd0c27411bd71f29932917c49cf529a641d0168496c3a501e3062c"

# Fail hard unless $1's sha256 matches EXPECTED_SHA256.
verify_checksum() {
  local file="$1" actual
  actual="$(sha256sum "$file")" || { echo "error: cannot read $file" >&2; exit 1; }
  actual="${actual%% *}"
  if [[ "$actual" != "$EXPECTED_SHA256" ]]; then
    echo "error: checksum mismatch for $file" >&2
    echo "  expected: $EXPECTED_SHA256" >&2
    echo "  actual  : $actual" >&2
    echo "  the file is partial or corrupt — delete it and re-run this script to re-download" >&2
    exit 1
  fi
}

TARGET="$MODEL_DIR/$FILE"

echo "== BGE-M3 GGUF download =="
echo "  repo : $REPO"
echo "  file : $FILE @ $REVISION"
echo "  dest : $TARGET"

if [[ -f "$TARGET" ]]; then
  verify_checksum "$TARGET"
  echo "already present and verified ($(du -h "$TARGET" | cut -f1)) — skipping."
  exit 0
fi

mkdir -p "$MODEL_DIR"

if [[ "${1:-}" == "--dry-run" ]]; then
  echo "  (dry run, nothing downloaded)"
  echo "  would run: huggingface-cli download $REPO --include \"*$FILE*\" --local-dir $MODEL_DIR"
  exit 0
fi

# Preferred path: `hf` (new hub CLI). huggingface-cli is deprecated and may be a
# non-functional stub, so try `hf` first, then legacy huggingface-cli.
if command -v hf >/dev/null 2>&1; then
  echo "== using hf (hub CLI) =="
  hf download "$REPO" "$FILE" --revision "$REVISION" --local-dir "$MODEL_DIR"
elif command -v huggingface-cli >/dev/null 2>&1; then
  echo "== using huggingface-cli =="
  huggingface-cli download "$REPO" \
    --revision "$REVISION" \
    --include "*${FILE}*" \
    --local-dir "$MODEL_DIR"
else
  # Fallback: raw resolve URL pinned to the revision (needs `curl`).
  echo "== huggingface-cli not found; falling back to curl =="
  command -v curl >/dev/null 2>&1 || { echo "error: need huggingface-cli or curl" >&2; exit 1; }
  URL="https://huggingface.co/${REPO}/resolve/${REVISION}/${FILE}"
  curl -L --fail --proto '=https' -o "$TARGET.part" "$URL"
  mv "$TARGET.part" "$TARGET"
fi

if [[ ! -f "$TARGET" ]]; then
  echo "error: download did not produce $TARGET" >&2
  exit 1
fi
verify_checksum "$TARGET"

echo "== done =="
echo "  $(du -h "$TARGET" | cut -f1)  $TARGET"
echo "  next: LAMBO_BGE_M3_MODEL=$TARGET  then run ./scripts/run-llama-embed.sh"
