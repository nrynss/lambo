#!/usr/bin/env python3
"""Launch `EC2-LamboWebExhibit`, the public judge portal (plan §2, §5, §8).

A `t4g.large` in `Subnet-Public-1a`, in `SG-PublicWeb`, running Caddy on 443 in
front of `lambo serve-web` on loopback:7710, with llama.cpp serving BGE-M3 on
loopback:8080 beside it, all as systemd services.

Four constraints shape everything below.

**The instance type picks the release assets, not the reverse.** The default
`t4g.large` is Graviton (arm64); `--instance-type` also accepts x86_64 families
(e.g. `m7i-flex.large`, the free-tier option). User data fetches the
`lambo-<version>-linux-<arch>` release asset matching the instance type, and verifies it against
the `.sha256` published beside it rather than trusting the download. Same for
Caddy, against the release's `checksums.txt`.

**The DSN never lands anywhere durable.** No user data, no AMI, no repo, no log.
The instance profile grants `GetSecretValue` on exactly the `lambo/cockroach-dsn`
ARN, nothing wider, and the service wrapper resolves it at *start* and `exec`s
lambo with it in the environment. It is never written to a file, so there is
nothing to leak in a snapshot, and rotating the secret only needs a restart.

**TLS is a decision, not a default (plan §8).** `https://<EC2-IP>` cannot get a
trusted certificate: public CAs do not issue for bare IP addresses. So either
pass `--hostname <name you control>` and point an A record at the Elastic IP this
script allocates, or pass `--self-signed` and accept that every judge meets a
browser warning. There is no silent fallback.

**`lambo serve-web` binds loopback only.** It is a reader process that takes no
writer lease, and on a non-loopback bind it refuses to start without a bearer
token. Caddy is the only thing that talks to it, over 127.0.0.1, which keeps the
public portal token-free without weakening anything.

Usage:

    python3 scripts/aws-infra/launch_exhibit_ec2.py \\
        --session <lambo-session-id> --hostname lambo.example.com \\
        --key-name my-keypair --lambo-version 0.1.0

    python3 scripts/aws-infra/launch_exhibit_ec2.py --session <id> --hostname h --dry-run

Prerequisite: provision_network.py has run and the secret holds a DSN.
"""
from __future__ import annotations

import argparse
import base64
import http.client
import json
import pathlib
import ssl
import sys
import time

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from _common import (  # noqa: E402
    EC2_NAME,
    EIP_NAME,
    EXHIBIT_PROFILE_NAME,
    EXHIBIT_ROLE_NAME,
    LAMBO_WEB_PORT,
    PROJECT,
    PROJECT_TAG_KEY,
    SECRET_NAME,
    SG_PUBLIC_WEB_NAME,
    SUBNET_PUBLIC_NAME,
    Aws,
    ClientError,
    InfraError,
    add_common_args,
    created,
    existing,
    find_instance,
    note,
    one_or_none,
    poll,
    project_filters,
    require_boto3,
    require_secret_arn,
    require_sg,
    require_subnet,
    require_vpc,
    run_main,
    say,
    step,
    tag_spec,
    tags,
    warn,
    would,
)

DEFAULT_LAMBO_REPO = "nrynss/lambo"
DEFAULT_LAMBO_VERSION = "0.1.0"
DEFAULT_CADDY_VERSION = "2.10.0"

# BGE-M3 as GGUF, served by llama.cpp on the instance itself.
#
# This has to be the same model that wrote the vectors in the store, not merely
# one of the same dimension. `resolve_backends` only enforces the *width*
# (VECTOR(1024) vs embedder dim), so a mismatched model resolves cleanly and
# then returns confident nonsense: the query lands in a different vector space
# from the stored embeddings, and nothing errors. The live sessions were written
# with bge_m3, so the exhibit reads with bge_m3.
#
# This is the FP16 build, pinned by hash, because it is byte-identical to the
# model the operator's own llama.cpp serves — the one that produced every vector
# currently in the store. Verified: HuggingFace's `x-linked-etag` (the LFS
# sha256) equals the local file's sha256, and both are 1,157,671,200 bytes.
# A quantized build would be *close enough* to be indistinguishable in casual
# use and subtly wrong under comparison, which is the worst failure mode
# available here, so the exact file is used instead.
#
# GGUF is architecture-independent: the same file gives the same vectors on
# Graviton as on the x86 box that wrote them. The default instance is Graviton
# (the better value per unit of throughput); x86_64 is supported for accounts
# that can only launch free-tier families.
DEFAULT_BGE_MODEL_URL = (
    "https://huggingface.co/gpustack/bge-m3-GGUF/resolve/main/bge-m3-FP16.gguf"
)
DEFAULT_BGE_MODEL_SHA256 = (
    "daec91ffb5dd0c27411bd71f29932917c49cf529a641d0168496c3a501e3062c"
)
DEFAULT_LLAMA_CPP_REF = "b10453"

# Prebuilt llama.cpp, per release ref and per architecture, each pinned by a hash
# computed from the downloaded artefact. Upstream publishes no checksum file beside
# these, so the pin is what makes the download verifiable at all. Both the tarball
# name and the hash are keyed by ref (not just arch), so an `--llama-cpp-ref` that
# is absent from this table is refused at parse time - a non-default ref can never
# silently pull the default ref's hash, which would fail sha256sum -c at boot after
# the instance was already reported running.
LLAMA_TARBALLS = {
    "b10453": {
        "x86_64": (
            "llama-b10453-bin-ubuntu-x64.tar.gz",
            "550eb155a09c3051c7add5becf6d0badc3a4c33416807985963036b27b859fb4",
        ),
        "arm64": (
            "llama-b10453-bin-ubuntu-arm64.tar.gz",
            "b164e72dfb69c711275178e0d0fae54748042f039e4fe7386f1c0ea7019c109c",
        ),
    },
}
LLAMA_PORT = 8080

# The FP16 model is ~1.2 GiB resident before llama.cpp's own allocations and the
# KV cache, so these are rejected up front rather than discovered when the OOM
# killer takes llama-server mid-demo. t4g.medium (4 GiB) fits but leaves little
# headroom, so it is warned about rather than blocked. There is no source build
# any more - llama.cpp ships as a prebuilt binary (see DEFAULT_LLAMA_CPP_REF).
TOO_SMALL_FOR_LOCAL_BGE = (
    "t4g.nano", "t4g.micro", "t4g.small",
    "t3.nano", "t3.micro", "t3.small",
    "t3a.nano", "t3a.micro", "t3a.small",
    "t2.nano", "t2.micro", "t2.small",
)
# 4 GiB: fits the FP16 model + the prebuilt llama.cpp with little headroom.
TIGHT_FOR_LOCAL_BGE = ("t4g.medium", "t3.medium", "t3a.medium", "c7i-flex.large")

# Ubuntu 26.04 LTS, resolved through Canonical's public SSM parameter rather
# than a hardcoded AMI id: AMI ids are per-region and go stale every few weeks.
#
# Not Amazon Linux 2023. That choice used to be a glibc workaround: AL2023
# ships glibc 2.34, while the release workflow then built both Linux targets on
# Ubuntu 24.04 runners (glibc 2.39), so the published binary passed its checksum
# and then died with `version GLIBC_2.39 not found` the moment systemd started
# it — true of the arm64 asset as well, so it was not an artefact of the
# instance architecture. Release builds now run inside a `debian:bookworm`
# container (T12), whose older toolchain keeps the shipped binary below the
# AL2023 glibc floor; a repo-side "Assert max required GLIBC <= 2.34" CI gate
# makes that structural (see .github/workflows/release.yml). So the binary runs
# on AL2023 too, and Ubuntu 26.04 is no longer chosen as a glibc workaround —
# it is a newer, well-maintained platform, newer than the build environment, so
# the checksum verification still means what it is supposed to mean.
UBUNTU_SSM = {
    "arm64": "/aws/service/canonical/ubuntu/server/26.04/stable/current/arm64/hvm/ebs-gp3/ami-id",
    "x86_64": "/aws/service/canonical/ubuntu/server/26.04/stable/current/amd64/hvm/ebs-gp3/ami-id",
}
#
# NEW-5 (T6) DISCHARGED by the E2E round-1 recheck: both UBUNTU_SSM parameter
# paths above were confirmed against a live plumbing account to resolve in
# us-east-1 — `aws ssm get-parameter --name
# /aws/service/canonical/ubuntu/server/26.04/stable/current/{arm64,amd64}/hvm/ebs-gp3/ami-id`
# each returned the canonical Ubuntu 26.04 AMI id. No path correction needed.
#
# The `stable/current` path rotates: the AMI id resolved here is logged on launch
# (see resolve_ami / the "AMI ..." note in main), but is deliberately not pinned.
# Pinning a specific AMI id would sacrifice Canonical's own security/notch fixes;
# the resolved id is recorded in the run output and the SSM lookup is re-evaluated
# on every launch. Accept that tradeoff explicitly when you audit a deploy.

# Which architecture each instance family lands on. The release asset names are
# chosen at script level, so a mismatch downloads a binary the machine cannot
# execute and the failure would only surface in the boot log. Graviton remains
# the better value per unit of throughput; x86_64 is supported because an AWS
# account on the Free plan may only launch free-tier-eligible types, and the
# only such type with enough memory for BGE-M3 FP16 is m7i-flex.large.
# Fail closed: an unlisted family is rejected at parse time (see
# arch_for_instance_type), so a missing entry costs a clear error, never a
# wrong-architecture download.
ARM_FAMILIES = (
    "a1.", "t4g.", "m6g.", "m7g.", "m8g.", "c6g.", "c7g.", "c8g.",
    "r6g.", "r7g.", "r8g.", "x2g.", "x2gd.", "im4g.", "im4gn.", "is4g.",
    "is4gen.", "g5g.", "mac2.", "mac2-m2.",
)
X86_FAMILIES = (
    "t2.", "t3.", "t3a.", "m3.", "m4.", "m5.", "m5a.", "m6i.", "m6a.",
    "m7i.", "m7i-flex.", "c3.", "c4.", "c5.", "c5a.", "c6i.", "c6a.",
    "c7i.", "c7i-flex.", "r3.", "r4.", "r5.", "r6i.", "r7i.", "d2.",
    "h1.", "i3.", "i3en.", "g3.", "g3s.", "g4dn.", "g5.", "p3.", "p3dn.",
    "inf1.", "inf2.", "trn1.", "x1.", "x1e.", "z1d.", "u-", "mac1.",
)

# Release asset naming per architecture. lambo and Caddy spell the same machine
# differently ("x86_64" against "amd64"), which is exactly the kind of detail
# that is silently wrong if it is written out at each use site instead of once.
ASSET_NAMES = {
    "arm64": {"lambo": "linux-arm64", "caddy": "linux_arm64"},
    "x86_64": {"lambo": "linux-x86_64", "caddy": "linux_amd64"},
}


def arch_for_instance_type(value: str) -> str:
    """Map an instance type to the architecture its assets must match."""
    if value.startswith(ARM_FAMILIES):
        return "arm64"
    if value.startswith(X86_FAMILIES):
        return "x86_64"
    raise argparse.ArgumentTypeError(
        f"{value!r} is not a family this script knows the architecture of. User data "
        "picks the lambo and Caddy assets from that architecture, so guessing would "
        "download a binary the machine cannot execute. Use t4g.large (Graviton), or "
        "m7i-flex.large (x86_64, free-tier eligible), or add the family to "
        "ARM_FAMILIES or X86_FAMILIES."
    )


def known_instance_type(value: str) -> str:
    arch_for_instance_type(value)
    return value


def llama_tarball(ref: str, arch: str) -> tuple[str, str]:
    """(tarball filename, sha256) for a pinned llama.cpp release ref + arch.

    Keyed by ref so a custom `--llama-cpp-ref` resolves to *its* hash (and its
    tarball name), never the default ref's. The caller must already have checked
    `known_llama_cpp_ref`; a missing ref here is a programming error.
    """
    return LLAMA_TARBALLS[ref][arch]


def known_llama_cpp_ref(value: str) -> str:
    """Refuse a `--llama-cpp-ref` that has no pinned tarball hash (NEW-1).

    Without this the URL, the extraction dir and the sha256 all drift apart for a
    non-default ref and the bootstrap aborts at `sha256sum -c` after the instance
    is already reported running. Fail here, in Python, before any AWS work.
    """
    if value not in LLAMA_TARBALLS:
        raise argparse.ArgumentTypeError(
            f"{value!r} is not a llama.cpp ref this script pins a tarball hash for. "
            f"Supported: {', '.join(sorted(LLAMA_TARBALLS))}. Add the ref (and its "
            "per-architecture sha256, computed from the downloaded artifact) to "
            "LLAMA_TARBALLS to use it."
        )
    return value


def effective_bge_model_sha256(args: argparse.Namespace) -> str:
    """The hash to verify the model against: the explicit one, or the default.

    `--bge-model-sha256` has no default value (it is `None` unless given), so we
    can tell a supplied hash from an omitted one and enforce that a custom
    `--bge-model-url` demands a custom hash (T2-P2-2).
    """
    return args.bge_model_sha256 or DEFAULT_BGE_MODEL_SHA256


# --------------------------------------------------------------------------
# User data
# --------------------------------------------------------------------------

# Placeholders use @@NAME@@ rather than str.format or %-substitution: the script
# below is dense with `${...}` and `%` and every other templating scheme would
# need most of it escaped.
USER_DATA = r"""#!/bin/bash
# lambo-cloudops exhibit bootstrap. Idempotent enough to re-run by hand.
#
# There is deliberately no `set -x` anywhere in this file. The service wrapper
# installed below resolves the CockroachDB DSN from Secrets Manager, and tracing
# would put it straight into /var/log/lambo-bootstrap.log and the console log.
set -euo pipefail
exec >>/var/log/lambo-bootstrap.log 2>&1
echo "=== lambo-cloudops bootstrap $(date -Is) ==="

REGION="@@REGION@@"
LAMBO_REPO="@@LAMBO_REPO@@"
LAMBO_VERSION="@@LAMBO_VERSION@@"
CADDY_VERSION="@@CADDY_VERSION@@"
SESSION="@@SESSION@@"
SECRET_ID="@@SECRET_ID@@"
WEB_PORT="@@WEB_PORT@@"

export DEBIAN_FRONTEND=noninteractive
apt-get update -y >/dev/null
# awscli is not in the Ubuntu base image the way it is in Amazon Linux, and the
# service wrapper shells out to it to resolve the DSN at start. Without it
# lambo-web restart-loops with `aws: not found` and never binds.
apt-get install -y tar gzip curl ca-certificates awscli >/dev/null


# Static system UIDs for the lambo/caddy/llama service accounts keep their data
# dirs and the systemd units' Protect rules stable across re-boots. They assume
# a fresh image: on a reused/heterogeneous one an unrelated account may already
# hold the UID. Creating a new static-UID user then must fail loudly with the
# conflict named, not abort under a bare useradd error. Existing accounts pass
# straight through, so idempotent re-runs are untouched.
ensure_system_user() {
    local name="$1" uid="$2" home="$3"
    if id -u "$name" >/dev/null 2>&1; then
        return 0
    fi
    if ! getent group "$name" >/dev/null 2>&1; then
        if getent group "$uid" >/dev/null 2>&1; then
            echo "cannot create system group '$name': GID $uid is already taken by '$(getent group "$uid" | cut -d: -f1)'" >&2
            exit 1
        fi
        groupadd --system --gid "$uid" "$name"
    fi
    if getent passwd "$uid" >/dev/null 2>&1; then
        echo "cannot create system user '$name': UID $uid is already taken by '$(getent passwd "$uid" | cut -d: -f1)'" >&2
        exit 1
    fi
    useradd --system --uid "$uid" --gid "$uid" --home-dir "$home" \
        --create-home --shell /sbin/nologin "$name"
}
@@LLAMA_BLOCK@@

# ---------------------------------------------------------------- lambo -----
# The asset name carries the instance's architecture, substituted at render
# time from the instance type. The release publishes `lambo-<version>-<name>`
# alongside a matching `.sha256` (see .github/workflows/release.yml); verify
# rather than trust the download.
cd /tmp
ASSET="lambo-${LAMBO_VERSION}-@@LAMBO_ASSET_ARCH@@"
BASE="https://github.com/${LAMBO_REPO}/releases/download/v${LAMBO_VERSION}"
curl -fsSL --retry 5 --retry-delay 5 -o "${ASSET}"         "${BASE}/${ASSET}"
curl -fsSL --retry 5 --retry-delay 5 -o "${ASSET}.sha256"  "${BASE}/${ASSET}.sha256"
# The .sha256 is `sha256sum` output, so `-c` checks the file by the name inside it.
sha256sum -c "${ASSET}.sha256"
install -m 0755 -o root -g root "${ASSET}" /usr/local/bin/lambo
rm -f "${ASSET}" "${ASSET}.sha256"
/usr/local/bin/lambo --version

# ---------------------------------------------------------------- caddy -----
CADDY_TGZ="caddy_${CADDY_VERSION}_@@CADDY_ASSET_ARCH@@.tar.gz"
CADDY_BASE="https://github.com/caddyserver/caddy/releases/download/v${CADDY_VERSION}"
curl -fsSL --retry 5 --retry-delay 5 -o "${CADDY_TGZ}" "${CADDY_BASE}/${CADDY_TGZ}"
curl -fsSL --retry 5 --retry-delay 5 -o caddy_checksums.txt \
    "${CADDY_BASE}/caddy_${CADDY_VERSION}_checksums.txt"
# Caddy publishes SHA-512, not SHA-256, so this is sha512sum. Getting it wrong
# does not fail loudly the way a bad hash would: sha256sum rejects the 128-char
# digests as "no properly formatted checksum lines found" and exits non-zero,
# which under `set -e` kills the bootstrap after lambo is already installed.
grep " ${CADDY_TGZ}\$" caddy_checksums.txt | sha512sum -c -
tar -xzf "${CADDY_TGZ}" caddy
install -m 0755 -o root -g root caddy /usr/local/bin/caddy
rm -f "${CADDY_TGZ}" caddy_checksums.txt caddy
/usr/local/bin/caddy version

ensure_system_user lambo 901 /var/lib/lambo
ensure_system_user caddy 902 /var/lib/caddy
install -d -m 0755 -o root  -g root  /etc/lambo /etc/caddy
install -d -m 0750 -o lambo -g lambo /var/lib/lambo
install -d -m 0750 -o caddy -g caddy /var/lib/caddy

# --------------------------------------------------------------- config -----
# No DSN here. `[store] dsn` is intentionally absent: LAMBO_COCKROACH_DSN from
# the environment is the primary source (src/store/mod.rs), and the wrapper below
# is what puts it there.
cat >/etc/lambo/lambo.toml <<'LAMBOTOML'
@@LAMBO_TOML@@
LAMBOTOML
chmod 0644 /etc/lambo/lambo.toml

# ------------------------------------------------------- secret resolution ---
# The one place the DSN exists on this host is the environment of the exec'd
# lambo process. Not a file, not an EnvironmentFile, not an AMI layer. `exec`
# replaces this shell, so there is no parent holding a copy either.
cat >/usr/local/bin/lambo-serve-web <<'WRAPPER'
#!/bin/sh
set -eu
LAMBO_COCKROACH_DSN="$(aws secretsmanager get-secret-value \
    --region "$LAMBO_REGION" --secret-id "$LAMBO_SECRET_ID" \
    --query SecretString --output text)"
if [ -z "$LAMBO_COCKROACH_DSN" ] || [ "$LAMBO_COCKROACH_DSN" = "None" ]; then
    echo "lambo/cockroach-dsn holds no value; set it with put-secret-value" >&2
    exit 1
fi
export LAMBO_COCKROACH_DSN
# When the embedder is local, wait for it. lambo builds an embedder at startup
# and health-checks it, so racing llama-server's model load would just crash-loop
# under Restart=always until it happened to win. Bounded, so a genuinely broken
# llama-server still surfaces as a failure rather than hanging forever. It also
# stops polling the moment llama-server is no longer active (T2-P3-4): with
# Restart=always a dead server flips in and out of `active`, so we only give up
# after several consecutive inactive checks rather than on the first transient.
if [ -n "${LAMBO_LLAMA_HEALTH:-}" ]; then
    i=0
    down=0
    until curl -fsS --max-time 3 "$LAMBO_LLAMA_HEALTH" >/dev/null 2>&1; do
        i=$((i + 1))
        if [ "$i" -ge 60 ]; then
            echo "llama-server did not become healthy at $LAMBO_LLAMA_HEALTH" >&2
            exit 1
        fi
        if [ -n "${LAMBO_LLAMA_SERVICE:-}" ] && \
           ! systemctl is-active --quiet "${LAMBO_LLAMA_SERVICE}"; then
            down=$((down + 1))
            if [ "$down" -ge 3 ]; then
                echo "${LAMBO_LLAMA_SERVICE} is not active (${down} consecutive " \
                     "checks); aborting instead of polling $LAMBO_LLAMA_HEALTH for 5 min." >&2
                exit 1
            fi
        else
            down=0
        fi
        sleep 5
    done
fi
exec /usr/local/bin/lambo --config /etc/lambo/lambo.toml serve-web \
    --session "$LAMBO_SESSION" --port "$LAMBO_PORT" --bind 127.0.0.1
WRAPPER
chmod 0755 /usr/local/bin/lambo-serve-web

cat >/etc/systemd/system/lambo-web.service <<UNIT
[Unit]
Description=lambo serve-web (read-only judge portal)
After=network-online.target @@LLAMA_AFTER@@
Wants=network-online.target

[Service]
Type=exec
User=lambo
Group=lambo
Environment=HOME=/var/lib/lambo
Environment=LAMBO_REGION=${REGION}
Environment=LAMBO_SECRET_ID=${SECRET_ID}
Environment=LAMBO_SESSION=${SESSION}
Environment=LAMBO_PORT=${WEB_PORT}
Environment=LAMBO_LLAMA_SERVICE=@@LLAMA_SERVICE@@
Environment=LAMBO_LLAMA_HEALTH=@@LLAMA_HEALTH@@
ExecStart=/usr/local/bin/lambo-serve-web
Restart=always
RestartSec=5
# serve-web is a reader and needs nothing outside its own state directory.
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/lambo

[Install]
WantedBy=multi-user.target
UNIT

# ---------------------------------------------------------------- caddy -----
cat >/etc/caddy/Caddyfile <<'CADDYFILE'
@@CADDYFILE@@
CADDYFILE
chown caddy:caddy /etc/caddy/Caddyfile
chmod 0640 /etc/caddy/Caddyfile

cat >/etc/systemd/system/caddy.service <<'UNIT'
[Unit]
Description=Caddy
Documentation=https://caddyserver.com/docs/
After=network-online.target
Wants=network-online.target

[Service]
Type=notify
User=caddy
Group=caddy
Environment=HOME=/var/lib/caddy
Environment=XDG_DATA_HOME=/var/lib/caddy
Environment=XDG_CONFIG_HOME=/var/lib/caddy
ExecStart=/usr/local/bin/caddy run --config /etc/caddy/Caddyfile
ExecReload=/usr/local/bin/caddy reload --config /etc/caddy/Caddyfile --force
Restart=always
RestartSec=5
TimeoutStopSec=5s
LimitNOFILE=1048576
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=full
# The only reason this process is allowed near a privileged port.
AmbientCapabilities=CAP_NET_BIND_SERVICE

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now lambo-web.service
systemctl enable --now caddy.service

echo "=== bootstrap complete $(date -Is) ==="
"""


LLAMA_BLOCK = r"""# ----------------------------------------------------------- llama.cpp -----
# BGE-M3 on the instance, so /api/recall embeds the judge's query in the same
# vector space the stored embeddings live in. See DEFAULT_BGE_MODEL_URL for why
# a same-dimension substitute is not good enough.
#
# llama.cpp now publishes prebuilt Linux binaries for both architectures, so
# this installs the pinned release rather than building from source. The build
# was not merely slow (several minutes on two vCPUs): the tag this script used
# to pin predates GCC 15 and no longer compiles on a current toolchain, which
# turned every boot into a coin toss against the distro's compiler version.
#
# The tarball publishes no per-asset checksum, so the hash below was computed
# from the downloaded artefact and is pinned here. That keeps the same property
# the lambo and Caddy fetches have: the bytes are verified before anything runs
# them, rather than trusted because they came from the right URL.
BGE_MODEL_URL="@@BGE_MODEL_URL@@"
LLAMA_CPP_REF="@@LLAMA_CPP_REF@@"
LLAMA_TARBALL="@@LLAMA_TARBALL@@"
LLAMA_TARBALL_SHA256="@@LLAMA_TARBALL_SHA256@@"
LLAMA_PORT="@@LLAMA_PORT@@"

# The prebuilt llama.cpp links against the OpenMP runtime. Building from source
# pulled it in transitively as a g++ dependency; installing a binary does not,
# and libgomp1 is not in the base Ubuntu cloud image, so it is installed
# explicitly here. Without it llama-server fails at exec with
# `libgomp.so.1: cannot open shared object file` and the bootstrap aborts.
apt-get install -y libgomp1 >/dev/null
ensure_system_user llama 903 /var/lib/llama
install -d -m 0755 -o llama -g llama /var/lib/llama/models

if [ ! -x /usr/local/bin/llama-server ]; then
    cd /tmp
    rm -rf "llama-${LLAMA_CPP_REF}" "${LLAMA_TARBALL}"
    curl -fsSL --retry 5 --retry-delay 5 -o "${LLAMA_TARBALL}" \
        "https://github.com/ggml-org/llama.cpp/releases/download/${LLAMA_CPP_REF}/${LLAMA_TARBALL}"
    echo "${LLAMA_TARBALL_SHA256}  ${LLAMA_TARBALL}" | sha256sum -c -
    tar -xzf "${LLAMA_TARBALL}"
    # The archive is a flat directory of executables beside the shared objects
    # they link against, so it is installed whole and the loader is pointed at
    # it. Copying just llama-server out would leave it unable to find libllama.
    # The hash above guarantees the *bytes*, not the layout, so verify the
    # expected tree before relying on it (NEW-3): a wrong top-level dir or a
    # missing libllama would otherwise produce a dangling /usr/local/bin symlink.
    LLAMA_DIR="llama-${LLAMA_CPP_REF}"
    if [ ! -d "${LLAMA_DIR}" ] || [ ! -x "${LLAMA_DIR}/llama-server" ]; then
        echo "llama tarball ${LLAMA_TARBALL} did not extract a usable ${LLAMA_DIR}/llama-server" >&2
        exit 1
    fi
    if ! ls "${LLAMA_DIR}"/libllama.so* >/dev/null 2>&1; then
        echo "llama tarball ${LLAMA_TARBALL} has no libllama.so* in ${LLAMA_DIR}" >&2
        exit 1
    fi
    rm -rf /opt/llama
    install -d -m 0755 -o root -g root /opt/llama
    cp -a "${LLAMA_DIR}/." /opt/llama/
    echo "/opt/llama" > /etc/ld.so.conf.d/llama.conf
    ldconfig
    ln -sf /opt/llama/llama-server /usr/local/bin/llama-server
    cd /tmp && rm -rf "${LLAMA_DIR}" "${LLAMA_TARBALL}"
fi
/usr/local/bin/llama-server --version 2>&1 | head -2 || true

MODEL=/var/lib/llama/models/bge-m3.gguf
BGE_MODEL_SHA256="@@BGE_MODEL_SHA256@@"
if [ ! -s "${MODEL}" ]; then
    # ~1.2 GB. Downloaded to .part and only renamed after the hash matches, so an
    # interrupted boot cannot leave a truncated model that llama-server would
    # happily load and then embed nonsense with.
    curl -fsSL --retry 5 --retry-delay 5 -o "${MODEL}.part" "${BGE_MODEL_URL}"
    echo "${BGE_MODEL_SHA256}  ${MODEL}.part" | sha256sum -c -
    mv "${MODEL}.part" "${MODEL}"
    chown llama:llama "${MODEL}"
fi
# Re-check on every boot: this hash is what ties the exhibit's query embeddings
# to the vectors already in the store. If it ever stops matching, the portal
# must not come up pretending otherwise.
echo "${BGE_MODEL_SHA256}  ${MODEL}" | sha256sum -c -

cat >/etc/systemd/system/llama-server.service <<UNIT
[Unit]
Description=llama.cpp embeddings server (BGE-M3)
After=network-online.target
Wants=network-online.target

[Service]
Type=exec
User=llama
Group=llama
# --embeddings puts the server in embedding mode; loopback only, because Caddy
# fronts the portal and nothing outside this host has any business calling it.
ExecStart=/usr/local/bin/llama-server \
    --model /var/lib/llama/models/bge-m3.gguf \
    --embeddings --host 127.0.0.1 --port ${LLAMA_PORT} \
    --ctx-size 8192 --threads $(nproc)
Restart=always
RestartSec=5
NoNewPrivileges=yes
PrivateTmp=yes
ProtectSystem=strict
ProtectHome=yes
ReadWritePaths=/var/lib/llama

[Install]
WantedBy=multi-user.target
UNIT

systemctl daemon-reload
systemctl enable --now llama-server.service
"""


def warn_fixture_embedder(embedder_kind: str) -> None:
    """Say out loud what `--embedder fixture` costs.

    The plan does not settle this and the code forces the question: `serve-web`
    goes through `resolve_backends`, so it constructs an embedder whether or not
    the exhibit needs one, and `/api/recall` embeds the judge's query with it. A
    The live sessions were written with `bge_m3`, so that is the default and the
    instance hosts its own llama.cpp. `fixture` remains available and starts and
    answers fine — its vectors are simply unrelated to the ones in the session,
    so the vector arm of recall returns noise while the lexical and structural
    arms (which carry the blast-radius warning) still work.

    Nothing errors in that case, which is the danger: `resolve_backends` enforces
    the vector *width* (VECTOR(1024) vs embedder dim) and not the model, so a
    mismatch resolves cleanly and then ranks confidently against a vector space
    the stored embeddings do not share. Worth stating rather than discovering
    during a demo.
    """
    if embedder_kind != "fixture":
        note(f"embedder {embedder_kind}: query embeddings share the session's vector space")
        return
    warn("embedder = fixture. /api/recall's VECTOR similarity will be meaningless:")
    warn("fixture vectors have no relationship to the session's stored embeddings,")
    warn("and nothing will error — only the width is checked, not the model.")
    warn("Lexical and structural recall still work, so the blast-radius and")
    warn("canonical markers the demo turns on are unaffected.")
    warn("Drop --embedder fixture to get real vectors (bge_m3 is the default).")


def local_llama(args: argparse.Namespace) -> bool:
    """True when this instance hosts its own llama.cpp.

    `--llama-url` means one is already running somewhere reachable, so we point
    at it and build nothing.
    """
    return args.embedder == "bge_m3" and not args.llama_url


def effective_llama_url(args: argparse.Namespace) -> str | None:
    if args.embedder != "bge_m3":
        return None
    return args.llama_url or f"http://127.0.0.1:{LLAMA_PORT}"

def render_llama_block(args: argparse.Namespace) -> str:
    if not local_llama(args):
        return ""
    out = LLAMA_BLOCK
    tarball, tarball_sha = llama_tarball(
        args.llama_cpp_ref, arch_for_instance_type(args.instance_type)
    )
    for key, value in {
        "@@BGE_MODEL_URL@@": args.bge_model_url,
        "@@LLAMA_CPP_REF@@": args.llama_cpp_ref,
        "@@LLAMA_TARBALL@@": tarball,
        "@@LLAMA_TARBALL_SHA256@@": tarball_sha,
        "@@BGE_MODEL_SHA256@@": effective_bge_model_sha256(args),
        "@@LLAMA_PORT@@": str(LLAMA_PORT),
    }.items():
        out = out.replace(key, value)
    return out


def render_lambo_toml(embedder_kind: str, llama_url: str | None) -> str:
    lines = ["# Written by scripts/aws-infra/launch_exhibit_ec2.py. Secrets stay out of here.", ""]
    lines += ["[store]", '# The DSN arrives via LAMBO_COCKROACH_DSN at service start.', 'kind = "cockroach"', ""]
    lines += ["[embedder]", f'kind = "{embedder_kind}"', "dim = 1024"]
    if embedder_kind == "bge_m3":
        lines.append(f'url = "{llama_url}"')
    return "\n".join(lines) + "\n"


def render_caddyfile(hostname: str | None, acme_email: str | None) -> str:
    proxy = f"""    encode zstd gzip
    reverse_proxy 127.0.0.1:{LAMBO_WEB_PORT}"""
    if hostname:
        head = f"{{\n    email {acme_email}\n}}\n\n" if acme_email else ""
        return (
            f"{head}{hostname} {{\n"
            f"{proxy}\n"
            "}\n"
        )
    # Self-signed path. `tls internal` issues from Caddy's own local CA, which no
    # browser trusts. Deliberately explicit so nobody can reach this state by
    # accident; the script prints the same warning.
    return (
        ":443 {\n"
        "    tls internal\n"
        f"{proxy}\n"
        "}\n"
    )


def render_user_data(args: argparse.Namespace, caddyfile: str, lambo_toml: str) -> str:
    replacements = {
        "@@REGION@@": args.region,
        "@@LAMBO_REPO@@": args.lambo_repo,
        "@@LAMBO_VERSION@@": args.lambo_version,
        "@@CADDY_VERSION@@": args.caddy_version,
        # Both asset names are derived from the instance type's architecture, in
        # one place, so the two spellings cannot drift apart.
        "@@LAMBO_ASSET_ARCH@@": ASSET_NAMES[arch_for_instance_type(args.instance_type)]["lambo"],
        "@@CADDY_ASSET_ARCH@@": ASSET_NAMES[arch_for_instance_type(args.instance_type)]["caddy"],
        "@@SESSION@@": args.session,
        "@@SECRET_ID@@": SECRET_NAME,
        "@@WEB_PORT@@": str(LAMBO_WEB_PORT),
        "@@CADDYFILE@@": caddyfile.rstrip("\n"),
        "@@LAMBO_TOML@@": lambo_toml.rstrip("\n"),
        # Empty when the embedder is the fixture, or when --llama-url points at
        # something already running elsewhere: in both cases this instance has
        # no llama.cpp to build.
        "@@LLAMA_BLOCK@@": render_llama_block(args),
        # `After=` only orders start-up, it does not wait for the model to load.
        # The wrapper's health poll is what actually gates lambo; this just stops
        # systemd starting them in the wrong order in the first place.
        "@@LLAMA_AFTER@@": "llama-server.service" if local_llama(args) else "",
        # Empty disables the wrapper's poll entirely, which is what we want when
        # the embedder is the fixture (nothing to wait for) or when the URL is
        # off-instance (not ours to wait on).
        "@@LLAMA_HEALTH@@": (
            f"{effective_llama_url(args)}/health" if local_llama(args) else ""
        ),
        # The local service the wrapper watches; empty when there is none.
        "@@LLAMA_SERVICE@@": "llama-server.service" if local_llama(args) else "",
    }
    out = USER_DATA
    for key, value in replacements.items():
        out = out.replace(key, value)
    if "@@" in out:
        leftover = out[out.index("@@") : out.index("@@") + 40]
        raise InfraError(f"user data template has an unsubstituted placeholder near {leftover!r}")
    return out


# --------------------------------------------------------------------------
# IAM
# --------------------------------------------------------------------------


def ensure_instance_profile(aws: Aws, secret_arn: str) -> str:
    trust = {
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Principal": {"Service": "ec2.amazonaws.com"},
                "Action": "sts:AssumeRole",
            }
        ],
    }
    try:
        aws.iam.get_role(RoleName=EXHIBIT_ROLE_NAME)
        existing("iam-role", EXHIBIT_ROLE_NAME)
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "NoSuchEntity":
            raise
        aws.iam.create_role(
            RoleName=EXHIBIT_ROLE_NAME,
            AssumeRolePolicyDocument=json.dumps(trust),
            Description="Instance role for EC2-LamboWebExhibit. Reads one secret, nothing else.",
            Tags=tags(EXHIBIT_ROLE_NAME),
        )
        created("iam-role", EXHIBIT_ROLE_NAME)

    # Exactly one secret, by full ARN. Plan §8: "scoped to reading exactly the
    # one secret, nothing wider". Not secretsmanager:*, not `Resource: "*"`, and
    # not the `lambo/cockroach-dsn-*` prefix form either - the exact ARN, which
    # DescribeSecret hands us, is tighter and just as stable.
    aws.iam.put_role_policy(
        RoleName=EXHIBIT_ROLE_NAME,
        PolicyName="read-cockroach-dsn",
        PolicyDocument=json.dumps(
            {
                "Version": "2012-10-17",
                "Statement": [
                    {
                        "Effect": "Allow",
                        "Action": ["secretsmanager:GetSecretValue", "secretsmanager:DescribeSecret"],
                        "Resource": secret_arn,
                    }
                ],
            }
        ),
    )
    note(f"{EXHIBIT_ROLE_NAME} may read exactly {SECRET_NAME}, nothing wider")

    try:
        aws.iam.get_instance_profile(InstanceProfileName=EXHIBIT_PROFILE_NAME)
        existing("instance-profile", EXHIBIT_PROFILE_NAME)
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "NoSuchEntity":
            raise
        aws.iam.create_instance_profile(
            InstanceProfileName=EXHIBIT_PROFILE_NAME, Tags=tags(EXHIBIT_PROFILE_NAME)
        )
        created("instance-profile", EXHIBIT_PROFILE_NAME)

    profile = aws.iam.get_instance_profile(InstanceProfileName=EXHIBIT_PROFILE_NAME)[
        "InstanceProfile"
    ]
    if not any(r["RoleName"] == EXHIBIT_ROLE_NAME for r in profile["Roles"]):
        aws.iam.add_role_to_instance_profile(
            InstanceProfileName=EXHIBIT_PROFILE_NAME, RoleName=EXHIBIT_ROLE_NAME
        )
        note("role added to instance profile")
    return profile["Arn"]


# --------------------------------------------------------------------------
# EC2
# --------------------------------------------------------------------------


def resolve_ami(aws: Aws, arch: str) -> tuple[str, str]:
    ami_id = aws.ssm.get_parameter(Name=UBUNTU_SSM[arch])["Parameter"]["Value"]
    image = aws.ec2.describe_images(ImageIds=[ami_id])["Images"][0]
    if image["Architecture"] != arch:
        raise InfraError(
            f"{ami_id} reports architecture {image['Architecture']}, expected {arch}.",
            hint="the SSM parameter path may have changed; check UBUNTU_SSM.",
        )
    return ami_id, image.get("RootDeviceName", "/dev/xvda")
def _iam_propagation_error(exc: ClientError) -> bool:
    """True only when a RunInstances failure looks like IAM->EC2 eventual
    consistency, so the launch retry loop does not mask real config errors
    (T2-P2-3).

    EC2 has no dedicated code for "profile not yet propagated": it surfaces as
    `InvalidParameterValue` whose message mentions the instance profile. A
    malformed ARN is unambiguous. Anything else (wrong instance type, bad
    subnet, tenancy) raises immediately.
    """
    code = exc.response["Error"]["Code"]
    if code == "InvalidIamInstanceProfileArn.Malformed":
        return True
    if code != "InvalidParameterValue":
        return False
    msg = (exc.response.get("Error", {}).get("Message") or "").lower()
    return any(tok in msg for tok in ("instance profile", "iam instance profile", "iaminstanceprofile"))


def launch_instance(
    aws: Aws,
    ami_id: str,
    root_device: str,
    subnet_id: str,
    sg_id: str,
    profile_arn: str,
    user_data: str,
    args: argparse.Namespace,
) -> str:
    kwargs: dict = {
        "ImageId": ami_id,
        "InstanceType": args.instance_type,
        "MinCount": 1,
        "MaxCount": 1,
        "SubnetId": subnet_id,
        "SecurityGroupIds": [sg_id],
        "IamInstanceProfile": {"Arn": profile_arn},
        "UserData": user_data,
        "BlockDeviceMappings": [
            {
                "DeviceName": root_device,
                "Ebs": {
                    "VolumeSize": args.volume_size,
                    "VolumeType": "gp3",
                    "Encrypted": True,
                    "DeleteOnTermination": True,
                },
            }
        ],
        # IMDSv2 only. The instance profile is the credential path for the secret,
        # so an SSRF that could read IMDSv1 would read the DSN.
        "MetadataOptions": {
            "HttpTokens": "required",
            "HttpEndpoint": "enabled",
            "HttpPutResponseHopLimit": 1,
        },
        # Tag at creation, both the instance and its volume, so a crash between
        # RunInstances and any follow-up still leaves teardown able to find them.
        "TagSpecifications": tag_spec("instance", EC2_NAME) + tag_spec("volume", EC2_NAME),
    }
    if args.key_name:
        kwargs["KeyName"] = args.key_name

    # Instance profiles are eventually consistent from IAM to EC2; RunInstances
    # immediately after creating one routinely fails for a few seconds. Narrow the
    # retry to errors that are actually IAM-propagation-shaped (T2-P2-3): a broad
    # `InvalidParameterValue` can also be a real config mistake (bad instance
    # type, bad subnet) and must surface immediately, not after 12 silent retries
    # wearing an IAM hint.
    last: ClientError | None = None
    for attempt in range(12):
        try:
            return aws.ec2.run_instances(**kwargs)["Instances"][0]["InstanceId"]
        except ClientError as exc:
            if not _iam_propagation_error(exc):
                raise
            last = exc
            if attempt == 0:
                note("instance profile not yet visible to EC2; waiting for IAM propagation")
            time.sleep(5)
    raise InfraError(
        f"EC2 kept rejecting the instance profile: {last.response['Error']['Message'] if last else ''}",
        hint="re-run in a minute; IAM propagation is usually under 30 seconds.",
    )


def ensure_eip(aws: Aws, instance_id: str) -> str:
    """Allocate and associate a stable address.

    The A record in §8 has to point somewhere that survives a stop/start, and a
    default public IPv4 does not. Tagged like everything else so teardown
    releases it - a stranded Elastic IP is billed by the hour.
    """
    resp = aws.ec2.describe_addresses(Filters=project_filters(EIP_NAME))
    address = one_or_none(resp["Addresses"], "elastic IP", EIP_NAME)
    if address is None:
        alloc = aws.ec2.allocate_address(
            Domain="vpc", TagSpecifications=tag_spec("elastic-ip", EIP_NAME)
        )
        address = {"AllocationId": alloc["AllocationId"], "PublicIp": alloc["PublicIp"]}
        created("elastic-ip", alloc["PublicIp"])
    else:
        existing("elastic-ip", address["PublicIp"])

    if address.get("InstanceId") != instance_id:
        aws.ec2.associate_address(
            AllocationId=address["AllocationId"], InstanceId=instance_id, AllowReassociation=True
        )
        note(f"{address['PublicIp']} associated with {instance_id}")
    return address["PublicIp"]

def _check_port_80_open(sg: dict) -> None:
    """Warn (not fail) if the public web SG lacks tcp/80.

    Port 80 is open by default in provision_network.py; a missing rule means that
    script ran before this change (or the SG was hand-edited). The HTTP->HTTPS
    redirect and ACME HTTP-01 fallback silently stop working without it, so say so
    rather than let the operator discover the redirect is dead later (T2-P2-1).
    """
    for perm in sg.get("IpPermissions", []):
        if (
            perm.get("IpProtocol") == "tcp"
            and perm.get("FromPort") == 80
            and perm.get("ToPort") == 80
        ):
            return
    warn(
        f"{SG_PUBLIC_WEB_NAME} has no tcp/80 ingress. The http->https redirect and "
        "ACME HTTP-01 fallback will not work. Re-run provision_network.py, which "
        "opens port 80 by default."
    )


def _instance_state(aws: Aws, instance_id: str) -> str:
    return aws.ec2.describe_instances(InstanceIds=[instance_id])["Reservations"][0][
        "Instances"
    ][0]["State"]["Name"]


def _ephemeral_ip(aws: Aws, instance_id: str, timeout: int = 120) -> str:
    """Wait for a public IPv4 on a non-EIP instance (T2-P3-1).

    The address is only assigned once the instance is running, so a re-adopted
    `pending` instance has no PublicIpAddress yet; reading it immediately yields
    None and the launcher prints "at None". Poll until it appears.
    """
    deadline = time.monotonic() + timeout
    while True:
        ip = _instance_state_public_ip(aws, instance_id)
        if ip:
            return ip
        if time.monotonic() > deadline:
            raise InfraError(
                f"no public IPv4 appeared on {instance_id}.",
                hint="check the instance is in a public subnet with auto-assign public IPv4 enabled.",
            )
        time.sleep(10)


def _instance_state_public_ip(aws: Aws, instance_id: str) -> str | None:
    inst = aws.ec2.describe_instances(InstanceIds=[instance_id])["Reservations"][0][
        "Instances"
    ][0]
    return inst.get("PublicIpAddress")


def _status_checks_ok(aws: Aws, instance_id: str) -> bool:
    """True once EC2 reports both status checks 2/2 passed."""
    resp = aws.ec2.describe_instance_status(InstanceIds=[instance_id])
    statuses = resp.get("InstanceStatuses", [])
    return bool(statuses) and all(
        s.get("SystemStatus", {}).get("Status") == "ok"
        and s.get("InstanceStatus", {}).get("Status") == "ok"
        for s in statuses
    )


def _caddy_up(public_ip: str) -> bool:
    """Probe :443 with a TLS handshake; any server answering counts as up.

    We deliberately treat a completed TLS handshake (even mid-ACME with a
    temporary/self-signed cert) as Caddy being alive - the point of NEW-2 is to
    detect a bootstrap that never installed Caddy, which shows up as a refused
    connection, not a certificate warning.
    """
    ctx = ssl._create_unverified_context()
    try:
        conn = http.client.HTTPSConnection(public_ip, 443, timeout=10, context=ctx)
        try:
            conn.request("HEAD", "/")
            conn.getresponse().read()
        finally:
            conn.close()
        return True
    except ssl.SSLError:
        return True  # a mid-ACME temporary cert can upset the probe; still a live Caddy
    except (ConnectionRefusedError, TimeoutError, OSError):
        return False


def wait_for_bootstrap(
    aws: Aws, instance_id: str, public_ip: str, timeout: int = 600, interval: int = 15
) -> None:
    """Wait until the bootstrap demonstrably finished (NEW-2).

    `get_waiter("instance_running")` only reports the EC2 state machine; user-data
    can still abort on a checksum and leave a running, portal-less instance. Here
    we require both status checks 2/2 AND a live Caddy endpoint before returning.
    On timeout we print the tail of the EC2 console output and fail, so a green
    "exhibit launched" is never printed for a dead bootstrap. The bootstrap
    script redirects its stdout/stderr to /var/log/lambo-bootstrap.log, so the
    console tail is kernel/systemd/cloud-init meta, not the failing step. That
    real log lives on the instance; this launcher does not install the SSM agent
    nor grant ssm:SendCommand, so it cannot be fetched from here - pull it from
    the host directly (recovery console / SSM shell) to see the failing line.
    """
    deadline = time.monotonic() + timeout
    saw_status = False
    while time.monotonic() < deadline:
        if public_ip and _status_checks_ok(aws, instance_id):
            saw_status = True
        if public_ip and saw_status and _caddy_up(public_ip):
            note(f"bootstrap ready: status checks 2/2, {public_ip} answers on :443")
            return
        time.sleep(interval)

    tail = _console_tail(aws, instance_id)
    raise InfraError(
        f"the exhibit did not become healthy on {public_ip} within {timeout}s "
        "(status checks and/or the :443 probe never passed).",
        hint="console output (the bootstrap script redirects to "
        "/var/log/lambo-bootstrap.log, so the failing step may not appear "
        "here; pull the real log from the host directly):\n"
        + "\n".join(tail),
    )


def _console_tail(aws: Aws, instance_id: str, lines: int = 30) -> list[str]:
    """Tail of the EC2 console output (best-effort; not the bootstrap log).

    USER_DATA redirects stdout/stderr to /var/log/lambo-bootstrap.log, so this
    shows kernel/systemd/cloud-init meta, not the failing step. Kept as a final
    diagnostic signal only; it never errors the launch path.
    """
    try:
        out = aws.ec2.get_console_output(InstanceId=instance_id).get("Output", "")
        if not out:
            return ["<console output is empty>"]
        text = base64.b64decode(out).decode("utf-8", errors="replace")
        return text.strip().splitlines()[-lines:] or ["<console output empty>"]
    except ClientError as exc:
        return [
            f"<could not read console output: {exc.response['Error'].get('Code', 'error')}>"
        ]


# --------------------------------------------------------------------------


def _plan(args: argparse.Namespace, caddyfile: str, lambo_toml: str) -> int:
    step(f"PLAN for region {args.region} (no AWS calls made)")
    would("iam-role", EXHIBIT_ROLE_NAME, f"GetSecretValue on {SECRET_NAME} only")
    would("instance-profile", EXHIBIT_PROFILE_NAME, f"holding {EXHIBIT_ROLE_NAME}")
    would(
        "instance",
        EC2_NAME,
        f"{args.instance_type} {arch_for_instance_type(args.instance_type)} Ubuntu 26.04 in {SUBNET_PUBLIC_NAME}/{SG_PUBLIC_WEB_NAME}",
    )
    would("volume", EC2_NAME, f"{args.volume_size}GB gp3, encrypted, delete on termination")
    if args.eip:
        would("elastic-ip", EIP_NAME, "allocated and associated")
    _a = ASSET_NAMES[arch_for_instance_type(args.instance_type)]
    note(f"lambo asset: lambo-{args.lambo_version}-{_a['lambo']} (+ .sha256, verified in user data)")
    note(f"caddy asset: caddy_{args.caddy_version}_{_a['caddy']}.tar.gz (+ checksums, verified)")
    note(f"key pair: {args.key_name or 'none (no SSH access will be possible)'}")
    say()
    step("embedder")
    warn_fixture_embedder(args.embedder)
    say()
    step("TLS")
    if args.hostname:
        note(f"Caddy will request a public certificate for {args.hostname}")
        note(f"create an A record {args.hostname} -> the Elastic IP this script allocates")
    else:
        warn("SELF-SIGNED: Caddy's internal CA will issue the certificate.")
        warn("Every browser will show a security warning. Judges will see it.")
    say()
    step("/etc/caddy/Caddyfile")
    for line in caddyfile.rstrip("\n").split("\n"):
        say(f"    {line}")
    say()
    step("/etc/lambo/lambo.toml")
    for line in lambo_toml.rstrip("\n").split("\n"):
        say(f"    {line}")
    say()
    note("the DSN appears nowhere in user data; the service wrapper resolves it at start")
    note(f"every resource is tagged {PROJECT_TAG_KEY}={PROJECT}")
    return 0


def main(args: argparse.Namespace) -> int:
    if args.hostname and args.self_signed:
        raise InfraError(
            "--hostname and --self-signed are mutually exclusive.",
            hint="pick one: a real certificate for a name you control, or a browser warning.",
        )
    if not args.hostname and not args.self_signed:
        raise InfraError(
            "no TLS strategy chosen (plan §8).",
            hint=(
                "public CAs do not issue certificates for bare IP addresses, so "
                "https://<EC2-IP> cannot work. Either pass --hostname <name you "
                "control> and point an A record at the Elastic IP this script "
                "allocates, or pass --self-signed and accept that every judge sees a "
                "browser warning. Cloudflare Tunnel is the third option in §8 and is "
                "not automated here."
            ),
        )
    if local_llama(args) and args.instance_type in TOO_SMALL_FOR_LOCAL_BGE:
        raise InfraError(
            f"{args.instance_type} is too small to host BGE-M3 locally.",
            hint=(
                "use --instance-type t4g.medium or larger, or point --llama-url at a "
                "llama.cpp running elsewhere, or accept --embedder fixture. Sizing it "
                "too small does not fail at launch: llama-server loads the model and "
                "is then killed by the OOM killer, usually mid-demo."
            ),
        )
    if local_llama(args) and args.instance_type in TIGHT_FOR_LOCAL_BGE:
        warn(f"{args.instance_type} fits the FP16 model but leaves little headroom.")
        warn("llama.cpp ships as a prebuilt binary; t4g.large is the tested size.")
    if args.llama_url and args.embedder != "bge_m3":
        raise InfraError(
            "--llama-url only applies to --embedder bge_m3.",
            hint=f"the embedder is {args.embedder!r}, which reaches no llama.cpp at all.",
        )
    if args.bge_model_url != DEFAULT_BGE_MODEL_URL and not args.bge_model_sha256:
        raise InfraError(
            "--bge-model-sha256 is required when --bge-model-url is customized.",
            hint=(
                "changing the URL changes the bytes, so the pinned default hash no "
                "longer matches and the boot would fail sha256sum -c on the model. "
                "Pass --bge-model-sha256 of the new file (and re-embed the store "
                "with the new model, since the vectors must match)."
            ),
        )

    caddyfile = render_caddyfile(args.hostname, args.acme_email)
    lambo_toml = render_lambo_toml(args.embedder, effective_llama_url(args))

    if args.dry_run:
        return _plan(args, caddyfile, lambo_toml)

    require_boto3()
    aws = Aws(args.region, args.profile)
    ident = aws.whoami()
    step(f"region {args.region}, identity {ident['Arn']}")

    require_vpc(aws)
    subnet = require_subnet(aws, SUBNET_PUBLIC_NAME)
    sg = require_sg(aws, SG_PUBLIC_WEB_NAME)
    _check_port_80_open(sg)
    secret_arn = require_secret_arn(aws)
    secret_desc = aws.secrets.describe_secret(SecretId=SECRET_NAME)
    if not secret_desc.get("VersionIdsToStages"):
        # Not fatal: the instance boots, lambo-web fails, systemd retries, and it
        # comes up on its own once the value lands. Say so rather than let the
        # operator debug a restart loop.
        warn(f"{SECRET_NAME} holds no value yet. lambo-web will restart-loop until it does.")

    step("embedder")
    warn_fixture_embedder(args.embedder)

    step("identity")
    profile_arn = ensure_instance_profile(aws, secret_arn)

    step("instance")
    instance = find_instance(aws)
    if instance is not None:
        existing("instance", instance["InstanceId"], instance["State"]["Name"])
        note("user data is only read at first boot; terminate and re-run to change it")
        instance_id = instance["InstanceId"]
        state = instance["State"]["Name"]
        if state in ("stopping", "stopped"):
            raise InfraError(
                f"{EC2_NAME} is {state}; a stopped exhibit serves nothing.",
                hint=f"start it (aws ec2 start-instances --instance-ids {instance_id}) then re-run this script.",
            )
    else:
        ami_id, root_device = resolve_ami(aws, arch_for_instance_type(args.instance_type))
        note(f"AMI {ami_id} (Ubuntu 26.04 LTS, {arch_for_instance_type(args.instance_type)}) root {root_device}")
        user_data = render_user_data(args, caddyfile, lambo_toml)
        instance_id = launch_instance(
            aws, ami_id, root_device, subnet["SubnetId"], sg["GroupId"], profile_arn, user_data, args
        )
        created("instance", instance_id)

    # A freshly launched instance, or a re-adopted `pending` one, needs to reach
    # running before a public IP exists or anything can be probed (T2-P3-1).
    poll(
        lambda: _instance_state(aws, instance_id),
        lambda s: s == "running",
        "instance to reach running",
        timeout=600,
        interval=10,
    )

    public_ip = None
    if args.eip:
        step("address")
        public_ip = ensure_eip(aws, instance_id)
    else:
        note("waiting for the ephemeral public IP (it is only assigned once the instance is running)")
        public_ip = _ephemeral_ip(aws, instance_id)
        note(f"using the ephemeral public IP {public_ip}; it changes on stop/start")

    # NEW-2: a green `instance_running` does not mean the bootstrap finished. All
    # the new download-and-verify steps (llama tarball, BGE model) can abort
    # user-data with set -e, leaving a running instance with no portal. Wait for
    # both status checks 2/2 AND the Caddy endpoint to answer before claiming
    # "exhibit launched"; on failure, print the console tail (console meta, NOT
    # the boot log — the bootstrap redirects to /var/log/lambo-bootstrap.log).
    step("bootstrap")
    wait_for_bootstrap(aws, instance_id, public_ip)
    say()
    step("exhibit launched")
    note(f"instance {instance_id} at {public_ip}")
    note("boot log: sudo tail -f /var/log/lambo-bootstrap.log  (takes 2 to 4 minutes)")
    say()
    if args.hostname:
        step("finish the TLS setup")
        say(f"    Create an A record:   {args.hostname}  ->  {public_ip}")
        say("    Caddy retries the ACME order until the record resolves, so the")
        say(f"    portal comes up on its own at https://{args.hostname}/ once it does.")
    else:
        warn("SELF-SIGNED CERTIFICATE IN USE.")
        say(f"    https://{public_ip}/ will show a browser security warning to every")
        say("    visitor, judges included. Plan §8: public CAs do not issue for bare")
        say("    IP addresses. Re-run with --hostname once you have a name.")
    say()
    note("port 80 and 443 are open from 0.0.0.0/0: 80 exists for the http->https redirect and ACME HTTP-01")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    add_common_args(parser)
    parser.add_argument(
        "--session", required=True, help="Lambo session id that serve-web opens a window onto."
    )
    tls = parser.add_argument_group("TLS (plan §8 - one of these is required)")
    tls.add_argument(
        "--hostname",
        default=None,
        help=(
            "Public hostname you control. Caddy issues and renews a real certificate "
            "for it. Point an A record at the Elastic IP this script allocates."
        ),
    )
    tls.add_argument(
        "--self-signed",
        action="store_true",
        help=(
            "Serve on the bare IP with Caddy's internal CA instead. Every browser "
            "shows a security warning; the script says so, loudly, rather than "
            "falling back to this silently."
        ),
    )
    tls.add_argument(
        "--acme-email",
        default=None,
        help="Contact address for the ACME account (expiry notices). Optional.",
    )
    parser.add_argument(
        "--lambo-version",
        default=DEFAULT_LAMBO_VERSION,
        help=(
            f"Release version to install (default {DEFAULT_LAMBO_VERSION}). The asset "
            "fetched is lambo-<version>-linux-<arch>, verified against its .sha256."
        ),
    )
    parser.add_argument(
        "--lambo-repo", default=DEFAULT_LAMBO_REPO, help=f"GitHub owner/repo (default {DEFAULT_LAMBO_REPO})."
    )
    parser.add_argument(
        "--caddy-version", default=DEFAULT_CADDY_VERSION, help=f"Caddy release (default {DEFAULT_CADDY_VERSION})."
    )
    parser.add_argument(
        "--embedder",
        choices=("fixture", "bge_m3"),
        default="bge_m3",
        help=(
            "Embedder kind written into /etc/lambo/lambo.toml (default bge_m3). "
            "`bge_m3` is the default because the live sessions were written with "
            "it, and /api/recall embeds the judge's query with whatever is "
            "configured here: a different model resolves fine and then ranks by a "
            "vector space the stored embeddings do not share. With no --llama-url, "
            "llama.cpp and the model are installed on the instance. `fixture` needs "
            "no service but makes the vector arm of recall meaningless."
        ),
    )
    parser.add_argument(
        "--llama-url",
        default=None,
        help=(
            "Use an already-running llama.cpp at this base URL instead of "
            "installing one. Only meaningful with --embedder bge_m3."
        ),
    )
    parser.add_argument(
        "--bge-model-url",
        default=DEFAULT_BGE_MODEL_URL,
        help="GGUF to serve when llama.cpp is installed locally. Must be BGE-M3.",
    )
    parser.add_argument(
        "--bge-model-sha256",
        default=None,
        help=(
            "Expected sha256 of the GGUF, checked on download and on every boot. "
            "Defaults to the hash of the DEFAULT_BGE_MODEL_URL file. Required "
            "(T2-P2-2) once you change --bge-model-url: the new file's bytes will "
            "not match the default hash, so the check is enforced rather than "
            "hinted, and you must also re-embed the store with the new model."
        ),
    )
    parser.add_argument(
        "--llama-cpp-ref",
        default=DEFAULT_LLAMA_CPP_REF,
        type=known_llama_cpp_ref,
        help=(
            f"llama.cpp release tag to install (default {DEFAULT_LLAMA_CPP_REF}). "
            "Must be a ref this script pins a tarball sha256 for (see "
            "LLAMA_TARBALLS); anything else is refused at parse time so a "
            "bootstrap never fails a checksum it could not have passed."
        ),
    )
    parser.add_argument(
        "--instance-type",
        default="t4g.large",
        type=known_instance_type,
        help=(
            "Instance type (default t4g.large, Graviton/arm64). Must be a family "
            "this script maps to an architecture; m7i-flex.large is the x86_64 "
            "free-tier option. The default is sized for the FP16 BGE-M3 with "
            "headroom; t4g.micro is enough only with --embedder fixture or an "
            "off-instance --llama-url."
        ),
    )
    parser.add_argument(
        "--volume-size",
        type=int,
        default=32,
        help=(
            "Root gp3 volume size in GB (default 32: the FP16 model is ~1.2 GB, "
            "plus room for the prebuilt llama.cpp, Caddy and lambo)."
        ),
    )
    parser.add_argument(
        "--key-name",
        default=None,
        help=(
            "Existing EC2 key pair for SSH. Omit for no SSH access at all, which is "
            "fine if you never need to look at the boot log."
        ),
    )
    parser.add_argument(
        "--no-eip",
        dest="eip",
        action="store_false",
        help="Do not allocate an Elastic IP. The address then changes on stop/start.",
    )
    parser.set_defaults(eip=True)
    return parser


if __name__ == "__main__":
    raise SystemExit(run_main(main, build_parser()))
