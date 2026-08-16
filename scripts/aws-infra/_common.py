"""Shared scaffolding for the `lambo-cloudops` AWS provisioning scripts.

Read `docs/plans/multi-agent-cloudops-aws-plan.md` (revision 2) before changing
anything here. The plan is the specification; this module only encodes the parts
of it that all four scripts need to agree on.

Three rules drive most of the design decisions below:

1. **Tags are the only inventory.** Every resource is stamped
   `Project=lambo-cloudops` plus a stable `Name` at creation time, and every
   lookup goes through those tags. `teardown.py` therefore never needs a
   hand-maintained list of ids, and a provisioning run that dies halfway leaves
   behind resources the next run can still find and adopt. See plan §7.
2. **`--dry-run` never touches AWS.** Not "makes only read calls" — makes *no*
   calls, and does not even construct a client. That makes the plan output
   runnable with no credentials at all, which is how these scripts are
   validated without going near the real account.
3. **Fail with a sentence, not a traceback.** Anything the operator can fix
   (missing credentials, missing prerequisite stack, an unset hostname) is
   raised as `InfraError` and printed as one line plus a hint. Raw botocore
   exceptions only escape for genuine surprises.

Dependencies: stdlib + boto3. Nothing else.
"""

from __future__ import annotations

import argparse
import sys
import time
from typing import Any, Callable, Sequence

# boto3 is imported lazily so that `--help` and `--dry-run` work in an
# environment that has never installed it. `require_boto3()` is the gate.
try:  # pragma: no cover - trivial import shim
    import boto3
    from botocore.exceptions import ClientError, NoCredentialsError

    _IMPORT_ERROR: Exception | None = None
except ImportError as exc:  # pragma: no cover
    boto3 = None  # type: ignore[assignment]
    ClientError = NoCredentialsError = Exception  # type: ignore[misc,assignment]
    _IMPORT_ERROR = exc


# --------------------------------------------------------------------------
# Project constants
# --------------------------------------------------------------------------

PROJECT = "lambo-cloudops"
PROJECT_TAG_KEY = "Project"
DEFAULT_REGION = "us-east-1"

# Resource names come from the plan's §2 diagram and §3 workflow, verbatim.
# Phase 4's agent scripts derive Lambo graph nodes under these exact names, so
# renaming anything here silently breaks the blast-radius demo: the graph would
# describe resources that no longer match what is in the account.
VPC_NAME = "VPC-Enterprise-Prod"
SUBNET_PUBLIC_NAME = "Subnet-Public-1a"
SUBNET_PRIVATE_NAME = "Subnet-Private-1a"
# Second private subnet: NOT in the plan, and not part of the demo narrative.
# RDS refuses a DB subnet group that does not span at least two availability
# zones, even for a single-AZ instance. This subnet exists solely to satisfy
# that constraint; the instance itself is pinned to the AZ of Subnet-Private-1a.
SUBNET_PRIVATE_B_NAME = "Subnet-Private-1b"
IGW_NAME = "InternetGateway"
ROUTE_TABLE_PUBLIC_NAME = "RouteTable-Public"
SG_BASE_NAME = "SG-Base-VPC"
SG_PUBLIC_WEB_NAME = "SG-PublicWeb"
EC2_NAME = "EC2-LamboWebExhibit"
RDS_NAME = "RDS-Lambo-Demo-DB"
LAMBDA_NAME = "Lambda-LamboStats-API"
EIP_NAME = "EIP-LamboWebExhibit"

# RDS lowercases DBInstanceIdentifier on its own, but sending mixed case makes
# every later `describe` call a guess about what came back. Send the lowercase
# identifier explicitly and carry the plan's name on the Name tag instead.
RDS_INSTANCE_ID = "rds-lambo-demo-db"
DB_SUBNET_GROUP = "lambo-cloudops-db-subnets"

SECRET_NAME = "lambo/cockroach-dsn"

EXHIBIT_ROLE_NAME = "lambo-cloudops-exhibit-role"
EXHIBIT_PROFILE_NAME = "lambo-cloudops-exhibit-profile"
STATS_ROLE_NAME = "lambo-cloudops-stats-role"

VPC_CIDR = "10.0.0.0/16"
SUBNET_PUBLIC_CIDR = "10.0.1.0/24"
SUBNET_PRIVATE_CIDR = "10.0.2.0/24"
SUBNET_PRIVATE_B_CIDR = "10.0.3.0/24"

AZ_PRIMARY_SUFFIX = "a"
AZ_SECONDARY_SUFFIX = "b"

# `lambo serve-web` listens here; Caddy is the only thing that talks to it, over
# loopback. Keeping the bind on 127.0.0.1 also keeps serve-web in its
# unauthenticated loopback mode (a non-loopback bind refuses to start without a
# bearer token, and the public portal is meant to be readable without one).
LAMBO_WEB_PORT = 7710


class InfraError(Exception):
    """An operator-fixable problem. Printed as one line, plus an optional hint."""

    def __init__(self, message: str, hint: str | None = None) -> None:
        super().__init__(message)
        self.hint = hint


# --------------------------------------------------------------------------
# Output
# --------------------------------------------------------------------------


def say(message: str = "") -> None:
    print(message, flush=True)


def step(message: str) -> None:
    say(f"==> {message}")


def created(kind: str, ident: str, extra: str = "") -> None:
    say(f"  [created ] {kind:<22} {ident}{(' ' + extra) if extra else ''}")


def existing(kind: str, ident: str, extra: str = "") -> None:
    say(f"  [exists  ] {kind:<22} {ident}{(' ' + extra) if extra else ''}")


def deleted(kind: str, ident: str) -> None:
    say(f"  [deleted ] {kind:<22} {ident}")


def would(kind: str, ident: str, extra: str = "") -> None:
    say(f"  [plan    ] {kind:<22} {ident}{(' ' + extra) if extra else ''}")


def skipped(kind: str, ident: str, why: str) -> None:
    say(f"  [skipped ] {kind:<22} {ident} ({why})")


def warn(message: str) -> None:
    say(f"  ! {message}")


def note(message: str) -> None:
    say(f"  . {message}")


# --------------------------------------------------------------------------
# Argument plumbing
# --------------------------------------------------------------------------


def add_common_args(parser: argparse.ArgumentParser) -> None:
    parser.add_argument(
        "--region",
        default=DEFAULT_REGION,
        help=f"AWS region (default: {DEFAULT_REGION}).",
    )
    parser.add_argument(
        "--profile",
        default=None,
        help=(
            "AWS named profile. Omit to use the ordinary boto3 credential chain "
            "(environment, then default profile, then instance role)."
        ),
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print the plan and exit. Makes no AWS API calls whatsoever.",
    )


def cidr_arg(value: str) -> str:
    """Validate an SSH source CIDR and refuse the whole internet.

    Plan §8: SSH ingress is restricted to the operator's own address. There is
    no default for this on purpose — a default would eventually be somebody
    else's address, or worse, nobody's.
    """
    import ipaddress

    try:
        net = ipaddress.ip_network(value, strict=False)
    except ValueError as exc:
        raise argparse.ArgumentTypeError(f"{value!r} is not a valid CIDR: {exc}") from exc
    if net.prefixlen == 0:
        raise argparse.ArgumentTypeError(
            f"{value!r} opens SSH to the entire internet. Pass your own address, "
            "e.g. --ssh-cidr $(curl -s https://checkip.amazonaws.com)/32"
        )
    return str(net)


def run_main(entry: Callable[[argparse.Namespace], int], parser: argparse.ArgumentParser) -> int:
    """Parse, run, and turn known failure modes into one readable line."""
    args = parser.parse_args()
    try:
        return entry(args)
    except InfraError as exc:
        print(f"\nerror: {exc}", file=sys.stderr)
        if exc.hint:
            print(f"hint:  {exc.hint}", file=sys.stderr)
        return 2
    except NoCredentialsError:
        print(
            "\nerror: no AWS credentials found.\n"
            "hint:  export AWS_PROFILE=<profile>, or set AWS_ACCESS_KEY_ID / "
            "AWS_SECRET_ACCESS_KEY, then re-run.",
            file=sys.stderr,
        )
        return 2
    except ClientError as exc:
        code = exc.response.get("Error", {}).get("Code", "Unknown")
        msg = exc.response.get("Error", {}).get("Message", str(exc))
        print(f"\nerror: AWS rejected the call ({code}): {msg}", file=sys.stderr)
        if code in ("UnauthorizedOperation", "AccessDenied", "AccessDeniedException"):
            print(
                "hint:  the calling identity is missing a permission. See the "
                "IAM section of scripts/aws-infra/README.md.",
                file=sys.stderr,
            )
        return 2
    except KeyboardInterrupt:
        print("\ninterrupted; re-run to reconcile (every script is idempotent).", file=sys.stderr)
        return 130


# --------------------------------------------------------------------------
# Clients
# --------------------------------------------------------------------------


def require_boto3() -> None:
    if boto3 is None:
        raise InfraError(
            f"boto3 is not installed ({_IMPORT_ERROR}).",
            hint="python3 -m pip install boto3   (stdlib + boto3 is the whole dependency set)",
        )


class Aws:
    """A lazily-built bundle of clients for one region.

    Constructed only outside `--dry-run`. `whoami()` is the credential preflight
    so that a missing profile surfaces as a sentence before anything is created.
    """

    def __init__(self, region: str, profile: str | None = None) -> None:
        require_boto3()
        self.region = region
        self._session = boto3.Session(profile_name=profile, region_name=region)
        self._clients: dict[str, Any] = {}

    def client(self, name: str) -> Any:
        if name not in self._clients:
            self._clients[name] = self._session.client(name, region_name=self.region)
        return self._clients[name]

    @property
    def ec2(self) -> Any:
        return self.client("ec2")

    @property
    def iam(self) -> Any:
        return self.client("iam")

    @property
    def rds(self) -> Any:
        return self.client("rds")

    @property
    def awslambda(self) -> Any:
        return self.client("lambda")

    @property
    def secrets(self) -> Any:
        return self.client("secretsmanager")

    @property
    def ssm(self) -> Any:
        return self.client("ssm")

    @property
    def logs(self) -> Any:
        return self.client("logs")

    def whoami(self) -> dict[str, str]:
        try:
            return self.client("sts").get_caller_identity()
        except NoCredentialsError as exc:
            raise InfraError(
                "no AWS credentials found for this run.",
                hint=(
                    "export AWS_PROFILE=<profile>, or set AWS_ACCESS_KEY_ID / "
                    "AWS_SECRET_ACCESS_KEY. These scripts never embed a profile, "
                    "an account id, or a key."
                ),
            ) from exc
        except ClientError as exc:
            raise InfraError(
                f"credentials were found but rejected: {exc.response['Error'].get('Message', exc)}",
                hint="the access key may be disabled or the profile may point at a stale session.",
            ) from exc


# --------------------------------------------------------------------------
# Tagging and tag-based lookup
# --------------------------------------------------------------------------


def tags(name: str, **extra: str) -> list[dict[str, str]]:
    """The tag set every resource in this stack carries.

    `Project=lambo-cloudops` is what teardown filters on; `Name` is what makes
    the console and the plan output legible.
    """
    out = [{"Key": PROJECT_TAG_KEY, "Value": PROJECT}, {"Key": "Name", "Value": name}]
    out.extend({"Key": k, "Value": v} for k, v in extra.items())
    return out


def tag_spec(resource_type: str, name: str, **extra: str) -> list[dict[str, Any]]:
    """EC2 `TagSpecifications` so the tag lands *with* creation, not after it.

    Tagging in a second call leaves a window where a crash produces an untagged
    resource that teardown cannot see. Every EC2 create in these scripts uses
    this.
    """
    return [{"ResourceType": resource_type, "Tags": tags(name, **extra)}]


def project_filters(name: str | None = None) -> list[dict[str, Any]]:
    out = [{"Name": f"tag:{PROJECT_TAG_KEY}", "Values": [PROJECT]}]
    if name is not None:
        out.append({"Name": "tag:Name", "Values": [name]})
    return out


def tag_value(resource: dict[str, Any], key: str) -> str | None:
    for tag in resource.get("Tags") or []:
        if tag.get("Key") == key:
            return tag.get("Value")
    return None


def one_or_none(items: Sequence[dict[str, Any]], kind: str, name: str) -> dict[str, Any] | None:
    """Idempotency's sharp edge: two matches means a previous run duplicated.

    Refuse rather than pick, because picking would hide the duplicate and the
    second copy would survive teardown's dependency ordering in unpredictable
    ways.
    """
    if not items:
        return None
    if len(items) > 1:
        raise InfraError(
            f"found {len(items)} {kind} resources tagged Name={name}; expected at most one.",
            hint=(
                "a previous run duplicated them. Run teardown.py --confirm, or delete "
                "the extras by hand, then re-run."
            ),
        )
    return items[0]


# --- EC2 lookups -----------------------------------------------------------


def find_vpc(aws: Aws) -> dict[str, Any] | None:
    resp = aws.ec2.describe_vpcs(Filters=project_filters(VPC_NAME))
    return one_or_none(resp["Vpcs"], "VPC", VPC_NAME)


def require_vpc(aws: Aws) -> dict[str, Any]:
    vpc = find_vpc(aws)
    if vpc is None:
        raise InfraError(
            f"no VPC tagged Name={VPC_NAME}, {PROJECT_TAG_KEY}={PROJECT} exists in "
            f"{aws.region}.",
            hint="run `python3 scripts/aws-infra/provision_network.py --ssh-cidr <yours>` first.",
        )
    return vpc


def find_subnet(aws: Aws, name: str) -> dict[str, Any] | None:
    resp = aws.ec2.describe_subnets(Filters=project_filters(name))
    return one_or_none(resp["Subnets"], "subnet", name)


def require_subnet(aws: Aws, name: str) -> dict[str, Any]:
    subnet = find_subnet(aws, name)
    if subnet is None:
        raise InfraError(
            f"no subnet tagged Name={name} exists in {aws.region}.",
            hint="provision_network.py creates it; run that first, or re-run it to reconcile.",
        )
    return subnet


def find_sg(aws: Aws, name: str) -> dict[str, Any] | None:
    resp = aws.ec2.describe_security_groups(Filters=project_filters(name))
    return one_or_none(resp["SecurityGroups"], "security group", name)


def require_sg(aws: Aws, name: str) -> dict[str, Any]:
    sg = find_sg(aws, name)
    if sg is None:
        raise InfraError(
            f"no security group tagged Name={name} exists in {aws.region}.",
            hint="provision_network.py creates it; run that first, or re-run it to reconcile.",
        )
    return sg


def find_instance(aws: Aws, name: str = EC2_NAME) -> dict[str, Any] | None:
    """Live instances only. A terminated instance lingers in the API for about
    an hour and would otherwise make a re-run think the exhibit already exists.
    """
    resp = aws.ec2.describe_instances(
        Filters=project_filters(name)
        + [
            {
                "Name": "instance-state-name",
                "Values": ["pending", "running", "stopping", "stopped"],
            }
        ]
    )
    found = [i for r in resp["Reservations"] for i in r["Instances"]]
    return one_or_none(found, "EC2 instance", name)


def find_secret(aws: Aws) -> dict[str, Any] | None:
    """Return the secret's describe output, or None if it does not exist.

    Never reads the secret *value*. Nothing in this repo's provisioning path
    needs the DSN; only the exhibit instance does, and it resolves it itself at
    service start.
    """
    try:
        return aws.secrets.describe_secret(SecretId=SECRET_NAME)
    except ClientError as exc:
        if exc.response["Error"]["Code"] == "ResourceNotFoundException":
            return None
        raise


def require_secret_arn(aws: Aws) -> str:
    desc = find_secret(aws)
    if desc is None:
        raise InfraError(
            f"the secret {SECRET_NAME} does not exist in {aws.region}.",
            hint="provision_network.py creates the (empty) secret; run that first.",
        )
    if desc.get("DeletedDate"):
        raise InfraError(
            f"the secret {SECRET_NAME} is scheduled for deletion.",
            hint="re-run provision_network.py, which restores it, or wait out the recovery window.",
        )
    return desc["ARN"]


# --------------------------------------------------------------------------
# Waiting
# --------------------------------------------------------------------------


def poll(
    describe: Callable[[], Any],
    done: Callable[[Any], bool],
    what: str,
    timeout: int = 900,
    interval: int = 15,
    gone_is_done: bool = False,
) -> None:
    """A progress-printing poll loop.

    boto3's own waiters are used where they fit; this covers the cases where the
    terminal state is "the API 404s" (deletion) or where a visible heartbeat
    matters because the wait is measured in minutes (RDS).
    """
    say(f"  . waiting for {what} (up to {timeout}s)")
    deadline = time.monotonic() + timeout
    while True:
        try:
            state = describe()
        except ClientError as exc:
            code = exc.response["Error"]["Code"]
            if gone_is_done and code in (
                "ResourceNotFoundException",
                "DBInstanceNotFound",
                "InvalidInstanceID.NotFound",
            ):
                return
            raise
        if done(state):
            return
        if time.monotonic() > deadline:
            raise InfraError(
                f"timed out after {timeout}s waiting for {what}.",
                hint="the resource is probably still converging; re-run to reconcile.",
            )
        time.sleep(interval)
