#!/usr/bin/env bash
# Capture a provisioning or agent run as a raw evidence file.
#
# Every claim in this repository is backed by a capture in `evidence/`, and the
# AWS work is no exception: the exhibit is meant to be rebuildable from the
# scripts alone, and a run nobody recorded is a claim nobody can check.
#
#   scripts/cloudops/record-run.sh --slug network -- \
#       python3 scripts/aws-infra/provision_network.py --ssh-cidr "$CIDR"
#
# The command's exit status is preserved, so this wrapper is safe to put in
# front of anything without changing whether a failure is noticed.
#
# Output lands in `evidence/cloudops-run/<UTC>-<slug>.txt`.

set -uo pipefail

SLUG=""
while [ $# -gt 0 ]; do
    case "$1" in
        --slug) SLUG="${2:-}"; shift 2 ;;
        --)     shift; break ;;
        *)      echo "record-run.sh: unexpected argument '$1'" >&2; exit 2 ;;
    esac
done

if [ -z "$SLUG" ] || [ $# -eq 0 ]; then
    echo "usage: record-run.sh --slug <name> -- <command> [args...]" >&2
    exit 2
fi

REPO_ROOT="$(git -C "$(dirname "$0")" rev-parse --show-toplevel)"
OUT_DIR="$REPO_ROOT/evidence/cloudops-run"
mkdir -p "$OUT_DIR"
OUT="$OUT_DIR/$(date -u +%Y%m%d-%H%M%S)-${SLUG}.txt"

# Redaction. These captures are committed, so anything that identifies the
# account or the operator has to be stripped on the way in rather than
# remembered about later. The account id is the important one: `scripts/
# aws-infra/README.md` states that no account id, profile, key or DSN is
# hardcoded anywhere in this directory, and an evidence file carrying one
# would make that false.
#
# OPERATOR_IP is passed in by the caller when a public IP is about to appear
# in a security group rule. It is a literal, so it is escaped for sed.
OPERATOR_IP="${OPERATOR_IP:-}"
redact() {
    sed -E \
        -e 's/[0-9]{12}/<account-id>/g' \
        -e 's#(postgres(ql)?://)[^[:space:]"]*#\1<redacted-dsn>#g' \
        -e 's/AKIA[0-9A-Z]{16}/<access-key-id>/g' \
        -e 's/(aws_secret_access_key|SecretAccessKey)[[:space:]]*[=:][[:space:]]*[^[:space:]"]+/\1=<redacted>/gI' \
    | if [ -n "$OPERATOR_IP" ]; then
          sed -E "s#$(printf '%s' "$OPERATOR_IP" | sed 's/[.[\*^$()+?{}|/]/\\&/g')#<operator-ip>#g"
      else
          cat
      fi
}

GIT_SHA="$(git -C "$REPO_ROOT" rev-parse --short HEAD 2>/dev/null || echo unknown)"
GIT_DIRTY=""
git -C "$REPO_ROOT" diff --quiet 2>/dev/null || GIT_DIRTY=" (working tree dirty)"

# The header goes through `redact` as well. It carries the command line, and
# the command line is exactly where the operator's own address shows up:
# `provision_network.py --ssh-cidr <your ip>/32`. Writing it straight to the
# file put a home IP into a capture meant to be committed.
{
    echo "# lambo cloudops run capture"
    echo "# utc      : $(date -u +%Y-%m-%dT%H:%M:%SZ)"
    echo "# commit   : ${GIT_SHA}${GIT_DIRTY}"
    echo "# region   : ${AWS_REGION:-${AWS_DEFAULT_REGION:-us-east-1}}"
    echo "# command  : $*"
    echo "# redacted : account id, DSN, access keys, operator IP"
    echo
} | redact > "$OUT"

# stdbuf keeps the capture in step with a long run rather than arriving in
# 4 KiB lumps, which matters when the thing being watched is a slow boot.
stdbuf -oL -eL "$@" 2>&1 | redact | tee -a "$OUT"
STATUS="${PIPESTATUS[0]}"

{
    echo
    echo "# exit     : $STATUS"
} >> "$OUT"

echo
echo "captured to ${OUT#"$REPO_ROOT"/}"
exit "$STATUS"
