#!/usr/bin/env bash
# Add THIS machine's public egress IP to the Cloud SQL instance's authorized networks.
#
# Why this exists. The Mooshik deployment reaches Cloud SQL over a public IP guarded by an
# allowlist, and the allowlist held exactly one /32: the cachyos box's egress address at
# provisioning time. That breaks in two ordinary ways.
#
#   1. Mooshik's autobiography spans TWO machines. The second one cannot reach the store
#      at all until its address is added, and "the store is shared" is the entire point of
#      workstream B.
#   2. A home/ISP address rotates. When it does, every lambo call against the hosted store
#      fails to connect, and the failure looks like an outage rather than an allowlist miss.
#
# So each machine runs this once (and again whenever its address changes). It is idempotent:
# an address already on the list is reported and nothing is patched.
#
# Usage:
#   scripts/cloudsql-allowlist.sh                 # add this host's current egress IP
#   scripts/cloudsql-allowlist.sh --list          # show the current allowlist, change nothing
#   scripts/cloudsql-allowlist.sh --dry-run       # say what would change, change nothing
#   scripts/cloudsql-allowlist.sh --ip 1.2.3.4    # add a specific address (the other machine)
#   scripts/cloudsql-allowlist.sh --remove --ip 1.2.3.4
#
# Environment (all optional; the defaults are this deployment's):
#   LAMBO_CLOUDSQL_INSTANCE   instance name           (default: lambo-pg)
#   LAMBO_GCP_PROJECT         project id              (default: mooshik)
#
# Note on names: `gcloud sql instances patch --authorized-networks` takes CIDRs only, so
# per-entry names are not preserved by this path. The list of addresses is what matters and
# what this script preserves; it never drops an entry it did not add.
set -euo pipefail

INSTANCE="${LAMBO_CLOUDSQL_INSTANCE:-lambo-pg}"
PROJECT="${LAMBO_GCP_PROJECT:-mooshik}"
MODE="add"
DRY_RUN=0
IP=""

# A value-taking flag at the end of the line used to set the variable empty, shift twice,
# and die from the second shift with no message. Say what is missing instead.
need_value() { # $1 = flag, $2 = remaining argument count
    [ "$2" -ge 2 ] || { echo "$1 needs a value" >&2; exit 2; }
}

while [ $# -gt 0 ]; do
    case "$1" in
        --list) MODE="list" ;;
        --remove) MODE="remove" ;;
        --dry-run) DRY_RUN=1 ;;
        --ip) need_value --ip $#; IP="$2"; shift ;;
        --instance) need_value --instance $#; INSTANCE="$2"; shift ;;
        --project) need_value --project $#; PROJECT="$2"; shift ;;
        # The header block, however long it grows: a hardcoded line range goes stale
        # silently and this one had, printing `set -euo pipefail` as help text.
        -h|--help) awk 'NR > 1 && /^#/ { print; next } NR > 1 { exit }' "$0"; exit 0 ;;
        *) echo "unknown argument: $1" >&2; exit 2 ;;
    esac
    shift
done

command -v gcloud >/dev/null || { echo "gcloud is not on PATH" >&2; exit 1; }

current="$(gcloud sql instances describe "$INSTANCE" --project "$PROJECT" \
    --format='value(settings.ipConfiguration.authorizedNetworks[].value)' | tr ';,' '\n\n' | sed '/^$/d')"

if [ "$MODE" = "list" ]; then
    echo "authorized networks on ${PROJECT}/${INSTANCE}:"
    if [ -z "$current" ]; then
        echo "  (none: the allowlist is empty, so no machine can reach the instance)"
    else
        echo "$current" | sed 's/^/  /'
    fi
    exit 0
fi

if [ -z "$IP" ]; then
    # This host's egress address, from two independent resolvers so a single one being
    # down or wrong does not silently allowlist the wrong /32.
    a="$(curl -fsS --max-time 10 https://ifconfig.me 2>/dev/null || true)"
    b="$(curl -fsS --max-time 10 https://api.ipify.org 2>/dev/null || true)"
    if [ -z "$a" ] && [ -z "$b" ]; then
        echo "could not determine this host's egress IP (both resolvers failed)" >&2
        exit 1
    fi
    if [ -n "$a" ] && [ -n "$b" ] && [ "$a" != "$b" ]; then
        echo "resolvers disagree on the egress IP ($a vs $b); pass --ip explicitly" >&2
        exit 1
    fi
    IP="${a:-$b}"
fi

case "$IP" in
    */*) CIDR="$IP" ;;
    *) CIDR="${IP}/32" ;;
esac

echo "instance: ${PROJECT}/${INSTANCE}"
echo "current:  $(echo "$current" | paste -sd, -)"
echo "target:   ${CIDR} (${MODE})"

if [ "$MODE" = "add" ]; then
    if echo "$current" | grep -qxF "$CIDR"; then
        echo "already allowlisted; nothing to do"
        exit 0
    fi
    desired="$(printf '%s\n%s\n' "$current" "$CIDR" | sed '/^$/d' | sort -u | paste -sd, -)"
else
    if ! echo "$current" | grep -qxF "$CIDR"; then
        echo "not on the list; nothing to do"
        exit 0
    fi
    desired="$(echo "$current" | grep -vxF "$CIDR" | sed '/^$/d' | sort -u | paste -sd, - || true)"
    if [ -z "$desired" ]; then
        echo "refusing to empty the allowlist: that locks every machine out of the store" >&2
        exit 1
    fi
fi

echo "desired:  ${desired}"
if [ "$DRY_RUN" = "1" ]; then
    echo "dry run: not patching"
    exit 0
fi

gcloud sql instances patch "$INSTANCE" --project "$PROJECT" \
    --authorized-networks="$desired" --quiet
echo "patched. verify with: $0 --list"
