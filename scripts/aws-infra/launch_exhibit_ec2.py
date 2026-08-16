#!/usr/bin/env python3
"""Launch `EC2-LamboWebExhibit`, the public judge portal (plan §2, §5, §8).

A `t4g.micro` in `Subnet-Public-1a`, in `SG-PublicWeb`, running Caddy on 443 in
front of `lambo serve-web` on loopback:7710, both as systemd services.

Four constraints shape everything below.

**t4g is Graviton, so the machine is ARM64.** User data fetches the
`lambo-<version>-linux-arm64` release asset, not x86_64, and verifies it against
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
import json
import pathlib
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
# Q8_0 is near-lossless, which matters here — quantization noise keeps the query
# in the same space, where a different model would not be.
DEFAULT_BGE_MODEL_URL = (
    "https://huggingface.co/gpustack/bge-m3-GGUF/resolve/main/bge-m3-Q8_0.gguf"
)
DEFAULT_LLAMA_CPP_REF = "b4585"
LLAMA_PORT = 8080

# Local BGE-M3 needs roughly 1 GiB resident plus headroom for llama.cpp's build
# and the OS. Instance types known to be too small are rejected up front rather
# than discovered when the OOM killer takes llama-server mid-demo.
TOO_SMALL_FOR_LOCAL_BGE = ("t4g.nano", "t4g.micro", "t4g.small")

# Amazon Linux 2023, arm64, resolved through the public SSM parameter rather
# than a hardcoded AMI id: AMI ids are per-region and go stale every few weeks.
AL2023_ARM64_SSM = "/aws/service/ami-amazon-linux-latest/al2023-ami-kernel-default-arm64"

# Graviton families. The release asset name is chosen at script level, so a
# non-ARM instance type would download a binary the machine cannot execute and
# the failure would only surface in the boot log.
ARM_FAMILIES = (
    "t4g.", "m6g.", "m7g.", "m8g.", "c6g.", "c7g.", "c8g.",
    "r6g.", "r7g.", "r8g.", "x2g", "im4g", "is4g", "g5g.",
)


def arm_instance_type(value: str) -> str:
    if not value.startswith(ARM_FAMILIES):
        raise argparse.ArgumentTypeError(
            f"{value!r} is not a Graviton (ARM64) instance type. User data fetches the "
            "lambo linux-arm64 asset, so an x86_64 type would boot and then fail. Use "
            "t4g.micro, or add the family to ARM_FAMILIES and provide an x86_64 path."
        )
    return value


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

dnf -y install tar gzip >/dev/null

@@LLAMA_BLOCK@@

# ---------------------------------------------------------------- lambo -----
# t4g.micro is Graviton, so this is the linux-arm64 asset. The release publishes
# `lambo-<version>-<name>` alongside a matching `.sha256` (see
# .github/workflows/release.yml); verify rather than trust the download.
cd /tmp
ASSET="lambo-${LAMBO_VERSION}-linux-arm64"
BASE="https://github.com/${LAMBO_REPO}/releases/download/v${LAMBO_VERSION}"
curl -fsSL --retry 5 --retry-delay 5 -o "${ASSET}"         "${BASE}/${ASSET}"
curl -fsSL --retry 5 --retry-delay 5 -o "${ASSET}.sha256"  "${BASE}/${ASSET}.sha256"
# The .sha256 is `sha256sum` output, so `-c` checks the file by the name inside it.
sha256sum -c "${ASSET}.sha256"
install -m 0755 -o root -g root "${ASSET}" /usr/local/bin/lambo
rm -f "${ASSET}" "${ASSET}.sha256"
/usr/local/bin/lambo --version

# ---------------------------------------------------------------- caddy -----
CADDY_TGZ="caddy_${CADDY_VERSION}_linux_arm64.tar.gz"
CADDY_BASE="https://github.com/caddyserver/caddy/releases/download/v${CADDY_VERSION}"
curl -fsSL --retry 5 --retry-delay 5 -o "${CADDY_TGZ}" "${CADDY_BASE}/${CADDY_TGZ}"
curl -fsSL --retry 5 --retry-delay 5 -o caddy_checksums.txt \
    "${CADDY_BASE}/caddy_${CADDY_VERSION}_checksums.txt"
grep " ${CADDY_TGZ}\$" caddy_checksums.txt | sha256sum -c -
tar -xzf "${CADDY_TGZ}" caddy
install -m 0755 -o root -g root caddy /usr/local/bin/caddy
rm -f "${CADDY_TGZ}" caddy_checksums.txt caddy
/usr/local/bin/caddy version

# ---------------------------------------------------------------- users -----
id -u lambo >/dev/null 2>&1 || \
    useradd --system --home-dir /var/lib/lambo --create-home --shell /sbin/nologin lambo
id -u caddy >/dev/null 2>&1 || \
    useradd --system --home-dir /var/lib/caddy --create-home --shell /sbin/nologin caddy
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
# llama-server still surfaces as a failure rather than hanging forever.
if [ -n "${LAMBO_LLAMA_HEALTH:-}" ]; then
    i=0
    until curl -fsS --max-time 3 "$LAMBO_LLAMA_HEALTH" >/dev/null 2>&1; do
        i=$((i + 1))
        if [ "$i" -ge 60 ]; then
            echo "llama-server did not become healthy at $LAMBO_LLAMA_HEALTH" >&2
            exit 1
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
Restart=on-failure
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
# llama.cpp publishes no linux-arm64 release binary, so this builds from a
# pinned tag. On two vCPUs that is several minutes of boot; the service comes up
# when it is done, and lambo-web waits for it rather than starting broken.
BGE_MODEL_URL="@@BGE_MODEL_URL@@"
LLAMA_CPP_REF="@@LLAMA_CPP_REF@@"
LLAMA_PORT="@@LLAMA_PORT@@"

dnf -y install git cmake gcc-c++ make >/dev/null

id -u llama >/dev/null 2>&1 || \
    useradd --system --home-dir /var/lib/llama --create-home --shell /sbin/nologin llama
install -d -m 0755 -o llama -g llama /var/lib/llama/models

if [ ! -x /usr/local/bin/llama-server ]; then
    cd /tmp
    rm -rf llama.cpp
    git clone --depth 1 --branch "${LLAMA_CPP_REF}" https://github.com/ggerganov/llama.cpp
    cd llama.cpp
    cmake -B build -DCMAKE_BUILD_TYPE=Release -DLLAMA_CURL=OFF -DGGML_NATIVE=ON
    cmake --build build --config Release --target llama-server -j "$(nproc)"
    install -m 0755 -o root -g root build/bin/llama-server /usr/local/bin/llama-server
    cd /tmp && rm -rf llama.cpp
fi

MODEL=/var/lib/llama/models/bge-m3.gguf
if [ ! -s "${MODEL}" ]; then
    curl -fsSL --retry 5 --retry-delay 5 -o "${MODEL}.part" "${BGE_MODEL_URL}"
    mv "${MODEL}.part" "${MODEL}"
    chown llama:llama "${MODEL}"
fi

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
    for key, value in {
        "@@BGE_MODEL_URL@@": args.bge_model_url,
        "@@LLAMA_CPP_REF@@": args.llama_cpp_ref,
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


def resolve_ami(aws: Aws) -> tuple[str, str]:
    ami_id = aws.ssm.get_parameter(Name=AL2023_ARM64_SSM)["Parameter"]["Value"]
    image = aws.ec2.describe_images(ImageIds=[ami_id])["Images"][0]
    if image["Architecture"] != "arm64":
        raise InfraError(
            f"{ami_id} reports architecture {image['Architecture']}, expected arm64.",
            hint="the SSM parameter path may have changed; check AL2023_ARM64_SSM.",
        )
    return ami_id, image.get("RootDeviceName", "/dev/xvda")


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
    # immediately after creating one routinely fails for a few seconds.
    last: ClientError | None = None
    for attempt in range(12):
        try:
            return aws.ec2.run_instances(**kwargs)["Instances"][0]["InstanceId"]
        except ClientError as exc:
            code = exc.response["Error"]["Code"]
            if code not in ("InvalidParameterValue", "InvalidIamInstanceProfileArn.Malformed"):
                raise
            last = exc
            if attempt == 0:
                note("waiting for the instance profile to propagate to EC2")
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


# --------------------------------------------------------------------------


def _plan(args: argparse.Namespace, caddyfile: str, lambo_toml: str) -> int:
    step(f"PLAN for region {args.region} (no AWS calls made)")
    would("iam-role", EXHIBIT_ROLE_NAME, f"GetSecretValue on {SECRET_NAME} only")
    would("instance-profile", EXHIBIT_PROFILE_NAME, f"holding {EXHIBIT_ROLE_NAME}")
    would(
        "instance",
        EC2_NAME,
        f"{args.instance_type} arm64 AL2023 in {SUBNET_PUBLIC_NAME}/{SG_PUBLIC_WEB_NAME}",
    )
    would("volume", EC2_NAME, f"{args.volume_size}GB gp3, encrypted, delete on termination")
    if args.eip:
        would("elastic-ip", EIP_NAME, "allocated and associated")
    note(f"lambo asset: lambo-{args.lambo_version}-linux-arm64 (+ .sha256, verified in user data)")
    note(f"caddy asset: caddy_{args.caddy_version}_linux_arm64.tar.gz (+ checksums, verified)")
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
    if args.llama_url and args.embedder != "bge_m3":
        raise InfraError(
            "--llama-url only applies to --embedder bge_m3.",
            hint=f"the embedder is {args.embedder!r}, which reaches no llama.cpp at all.",
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
    else:
        ami_id, root_device = resolve_ami(aws)
        note(f"AMI {ami_id} (Amazon Linux 2023, arm64) root {root_device}")
        user_data = render_user_data(args, caddyfile, lambo_toml)
        instance_id = launch_instance(
            aws, ami_id, root_device, subnet["SubnetId"], sg["GroupId"], profile_arn, user_data, args
        )
        created("instance", instance_id)
        aws.ec2.get_waiter("instance_running").wait(InstanceIds=[instance_id])

    public_ip = None
    if args.eip:
        step("address")
        public_ip = ensure_eip(aws, instance_id)
    else:
        described = aws.ec2.describe_instances(InstanceIds=[instance_id])
        public_ip = described["Reservations"][0]["Instances"][0].get("PublicIpAddress")
        note(f"using the ephemeral public IP {public_ip}; it changes on stop/start")

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
    note("port 443 is the only public ingress unless provision_network.py ran with --open-http")
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
            "fetched is lambo-<version>-linux-arm64, verified against its .sha256."
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
        "--llama-cpp-ref",
        default=DEFAULT_LLAMA_CPP_REF,
        help=f"llama.cpp git tag to build (default {DEFAULT_LLAMA_CPP_REF}).",
    )
    parser.add_argument(
        "--instance-type",
        default="t4g.medium",
        type=arm_instance_type,
        help=(
            "Graviton instance type (default t4g.medium). Must be ARM64. The "
            "default is sized for local BGE-M3; t4g.micro is enough only with "
            "--embedder fixture or an off-instance --llama-url."
        ),
    )
    parser.add_argument(
        "--volume-size",
        type=int,
        default=24,
        help="Root gp3 volume size in GB (default 24: the model is ~640 MB and llama.cpp builds from source).",
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
