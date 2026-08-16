#!/usr/bin/env python3
"""Provision the `lambo-cloudops` network foundation (plan §2, §3 Track 1).

Creates, in dependency order:

    VPC-Enterprise-Prod   10.0.0.0/16
      Subnet-Public-1a    10.0.1.0/24   (us-east-1a, auto-assign public IP)
      Subnet-Private-1a   10.0.2.0/24   (us-east-1a)
      Subnet-Private-1b   10.0.3.0/24   (us-east-1b, see the note below)
      InternetGateway     attached to the VPC
      RouteTable-Public   0.0.0.0/0 -> IGW, associated with Subnet-Public-1a
      SG-Base-VPC         internal mesh
      SG-PublicWeb        perimeter ingress
    Secrets Manager       lambo/cockroach-dsn   (created EMPTY, see below)

There is deliberately **no NAT gateway** (plan §2). Nothing in this design needs
one: the stats Lambda runs outside the VPC precisely so it can reach CockroachDB
Cloud without one, and the private subnet holds only RDS, which never initiates
outbound traffic. If a later step appears to need a NAT, something has been put
in the wrong tier — re-check the placement before provisioning one.

Two things this script does NOT do, on purpose:

* **It does not write the DSN.** The secret is created with no value. Putting a
  DSN on this script's command line would put it in shell history, in `ps`, and
  in any log that captures the invocation. The operator sets the value once, out
  of band; the script prints the exact command. Plan §11: "no connection string
  is baked into user data, an AMI, or the repo".
* **It does not create the private-subnet route table.** The private subnet uses
  the VPC's main route table, which has only the local route. That is exactly
  the isolation the private tier is supposed to have.

Usage:

    python3 scripts/aws-infra/provision_network.py --ssh-cidr 203.0.113.4/32
    python3 scripts/aws-infra/provision_network.py --ssh-cidr 203.0.113.4/32 --dry-run

Re-running is safe: everything is looked up by tag first and adopted if present.
"""

from __future__ import annotations

import argparse
import sys

sys.path.insert(0, str(__import__("pathlib").Path(__file__).resolve().parent))

from _common import (  # noqa: E402
    AZ_PRIMARY_SUFFIX,
    AZ_SECONDARY_SUFFIX,
    IGW_NAME,
    PROJECT,
    ROUTE_TABLE_PUBLIC_NAME,
    SECRET_NAME,
    SG_BASE_NAME,
    SG_PUBLIC_WEB_NAME,
    SUBNET_PRIVATE_B_CIDR,
    SUBNET_PRIVATE_B_NAME,
    SUBNET_PRIVATE_CIDR,
    SUBNET_PRIVATE_NAME,
    SUBNET_PUBLIC_CIDR,
    SUBNET_PUBLIC_NAME,
    VPC_CIDR,
    VPC_NAME,
    Aws,
    ClientError,
    InfraError,
    add_common_args,
    cidr_arg,
    created,
    existing,
    find_secret,
    find_sg,
    find_subnet,
    find_vpc,
    note,
    one_or_none,
    project_filters,
    require_boto3,
    run_main,
    say,
    step,
    tag_spec,
    tags,
    warn,
    would,
)

# Plan §8 specifies 443 as the public surface, but Caddy also needs 80 for the
# plain-http -> https redirect and, when a DNS-01/TLS-ALPN path is unavailable,
# the ACME HTTP-01 challenge fallback. Both are world-open by default so a fresh
# provision_(network|app|launch) run just works.
PUBLIC_INGRESS = [
    (80, "HTTP - Caddy http->https redirect and ACME HTTP-01 fallback"),
    (443, "HTTPS - the judge portal, via Caddy"),
]


def _plan(args: argparse.Namespace) -> int:
    step(f"PLAN for region {args.region} (no AWS calls made)")
    would("vpc", VPC_NAME, VPC_CIDR)
    would("subnet", SUBNET_PUBLIC_NAME, f"{SUBNET_PUBLIC_CIDR} in {args.region}{AZ_PRIMARY_SUFFIX}")
    would("subnet", SUBNET_PRIVATE_NAME, f"{SUBNET_PRIVATE_CIDR} in {args.region}{AZ_PRIMARY_SUFFIX}")
    would(
        "subnet",
        SUBNET_PRIVATE_B_NAME,
        f"{SUBNET_PRIVATE_B_CIDR} in {args.region}{AZ_SECONDARY_SUFFIX} (DB subnet group quorum)",
    )
    would("internet-gateway", IGW_NAME, f"attached to {VPC_NAME}")
    would("route-table", ROUTE_TABLE_PUBLIC_NAME, f"0.0.0.0/0 -> {IGW_NAME}, assoc {SUBNET_PUBLIC_NAME}")
    would("security-group", SG_BASE_NAME, "self-referential mesh + 5432 from SG-PublicWeb")
    ports = ", ".join(str(p) for p, _ in PUBLIC_INGRESS)
    would("security-group", SG_PUBLIC_WEB_NAME, f"{ports} from 0.0.0.0/0; 22 from {args.ssh_cidr}")
    would("secret", SECRET_NAME, "created with NO value; operator sets it separately")
    say()
    note("no NAT gateway is created, by design (plan §2)")
    note(f"every resource is tagged Project={PROJECT}")
    return 0


# --------------------------------------------------------------------------
# VPC
# --------------------------------------------------------------------------


def ensure_vpc(aws: Aws) -> str:
    vpc = find_vpc(aws)
    if vpc is None:
        vpc = aws.ec2.create_vpc(
            CidrBlock=VPC_CIDR,
            TagSpecifications=tag_spec("vpc", VPC_NAME),
        )["Vpc"]
        aws.ec2.get_waiter("vpc_available").wait(VpcIds=[vpc["VpcId"]])
        created("vpc", vpc["VpcId"], VPC_CIDR)
    else:
        existing("vpc", vpc["VpcId"], VPC_CIDR)

    vpc_id = vpc["VpcId"]
    # Both are off by default on a hand-made VPC and both are needed: RDS hands
    # out a DNS name rather than an address, and the exhibit instance needs a
    # public DNS name for anything that resolves it by hostname. Setting them on
    # every run is the reconcile path for a half-finished earlier run.
    aws.ec2.modify_vpc_attribute(VpcId=vpc_id, EnableDnsSupport={"Value": True})
    aws.ec2.modify_vpc_attribute(VpcId=vpc_id, EnableDnsHostnames={"Value": True})
    return vpc_id


def ensure_subnet(aws: Aws, vpc_id: str, name: str, cidr: str, az: str, public: bool) -> str:
    subnet = find_subnet(aws, name)
    if subnet is None:
        subnet = aws.ec2.create_subnet(
            VpcId=vpc_id,
            CidrBlock=cidr,
            AvailabilityZone=az,
            TagSpecifications=tag_spec("subnet", name),
        )["Subnet"]
        aws.ec2.get_waiter("subnet_available").wait(SubnetIds=[subnet["SubnetId"]])
        created("subnet", subnet["SubnetId"], f"{name} {cidr} {az}")
    else:
        existing("subnet", subnet["SubnetId"], f"{name} {cidr} {subnet['AvailabilityZone']}")

    if public and not subnet.get("MapPublicIpOnLaunch"):
        aws.ec2.modify_subnet_attribute(
            SubnetId=subnet["SubnetId"], MapPublicIpOnLaunch={"Value": True}
        )
        note(f"{name}: auto-assign public IPv4 enabled")
    return subnet["SubnetId"]


def ensure_igw(aws: Aws, vpc_id: str) -> str:
    resp = aws.ec2.describe_internet_gateways(Filters=project_filters(IGW_NAME))
    igw = one_or_none(resp["InternetGateways"], "internet gateway", IGW_NAME)
    if igw is None:
        igw = aws.ec2.create_internet_gateway(
            TagSpecifications=tag_spec("internet-gateway", IGW_NAME)
        )["InternetGateway"]
        created("internet-gateway", igw["InternetGatewayId"])
    else:
        existing("internet-gateway", igw["InternetGatewayId"])

    igw_id = igw["InternetGatewayId"]
    attached = {a["VpcId"] for a in igw.get("Attachments", [])}
    if vpc_id not in attached:
        # An IGW created but not attached is the classic half-finished state.
        # Attaching separately here is what makes the re-run reconcile it.
        aws.ec2.attach_internet_gateway(InternetGatewayId=igw_id, VpcId=vpc_id)
        note(f"internet gateway attached to {vpc_id}")
    return igw_id


def ensure_public_route_table(aws: Aws, vpc_id: str, igw_id: str, public_subnet_id: str) -> str:
    resp = aws.ec2.describe_route_tables(Filters=project_filters(ROUTE_TABLE_PUBLIC_NAME))
    rt = one_or_none(resp["RouteTables"], "route table", ROUTE_TABLE_PUBLIC_NAME)
    if rt is None:
        rt = aws.ec2.create_route_table(
            VpcId=vpc_id,
            TagSpecifications=tag_spec("route-table", ROUTE_TABLE_PUBLIC_NAME),
        )["RouteTable"]
        created("route-table", rt["RouteTableId"], ROUTE_TABLE_PUBLIC_NAME)
    else:
        existing("route-table", rt["RouteTableId"], ROUTE_TABLE_PUBLIC_NAME)

    rt_id = rt["RouteTableId"]
    has_default = any(r.get("DestinationCidrBlock") == "0.0.0.0/0" for r in rt.get("Routes", []))
    if not has_default:
        aws.ec2.create_route(
            RouteTableId=rt_id, DestinationCidrBlock="0.0.0.0/0", GatewayId=igw_id
        )
        note("default route 0.0.0.0/0 -> internet gateway added")

    associated = any(a.get("SubnetId") == public_subnet_id for a in rt.get("Associations", []))
    if not associated:
        aws.ec2.associate_route_table(RouteTableId=rt_id, SubnetId=public_subnet_id)
        note(f"{ROUTE_TABLE_PUBLIC_NAME} associated with {SUBNET_PUBLIC_NAME}")
    return rt_id


# --------------------------------------------------------------------------
# Security groups
# --------------------------------------------------------------------------


def ensure_sg(aws: Aws, vpc_id: str, name: str, description: str) -> str:
    sg = find_sg(aws, name)
    if sg is not None:
        existing("security-group", sg["GroupId"], name)
        return sg["GroupId"]
    # GroupName is scoped to the VPC, so a same-named group in the default VPC
    # does not collide. Description is required and must not contain an em dash
    # (AGENTS.md), which is why these read as plain hyphenated prose.
    gid = aws.ec2.create_security_group(
        GroupName=name,
        Description=description,
        VpcId=vpc_id,
        TagSpecifications=tag_spec("security-group", name),
    )["GroupId"]
    created("security-group", gid, name)
    return gid


def authorize(aws: Aws, group_id: str, permissions: list[dict], label: str) -> None:
    """Add ingress, treating "already there" as success.

    Re-running must not error, and AuthorizeSecurityGroupIngress has no
    idempotent mode, so InvalidPermission.Duplicate is the signal that the
    desired state already holds.
    """
    try:
        aws.ec2.authorize_security_group_ingress(GroupId=group_id, IpPermissions=permissions)
        note(f"ingress added: {label}")
    except ClientError as exc:
        if exc.response["Error"]["Code"] == "InvalidPermission.Duplicate":
            note(f"ingress already present: {label}")
        else:
            raise

def ensure_security_groups(aws: Aws, vpc_id: str, ssh_cidr: str) -> tuple[str, str]:
    base_id = ensure_sg(
        aws,
        vpc_id,
        SG_BASE_NAME,
        "Internal mesh for the lambo-cloudops private tier. Shared, load-bearing: "
        "deleting it strands RDS-Lambo-Demo-DB.",
    )
    web_id = ensure_sg(
        aws,
        vpc_id,
        SG_PUBLIC_WEB_NAME,
        "Perimeter ingress for the public judge portal on EC2-LamboWebExhibit.",
    )

    # SG-PublicWeb: the only world-facing surface in the stack.
    public_perms = [
        {
            "IpProtocol": "tcp",
            "FromPort": port,
            "ToPort": port,
            "IpRanges": [{"CidrIp": "0.0.0.0/0", "Description": desc}],
        }
        for port, desc in PUBLIC_INGRESS
    ]
    authorize(aws, web_id, public_perms, f"{SG_PUBLIC_WEB_NAME} world ingress")

    authorize(
        aws,
        web_id,
        [
            {
                "IpProtocol": "tcp",
                "FromPort": 22,
                "ToPort": 22,
                "IpRanges": [{"CidrIp": ssh_cidr, "Description": "operator SSH"}],
            }
        ],
        f"{SG_PUBLIC_WEB_NAME} SSH from {ssh_cidr}",
    )

    # SG-Base-VPC: the internal mesh. The self-reference is what makes it
    # "shared" in the plan's sense - anything placed in this group can talk to
    # anything else in it, which is why it accumulates incoming degree in the
    # Lambo graph and eventually earns Canonical status.
    authorize(
        aws,
        base_id,
        [
            {
                "IpProtocol": "-1",
                "UserIdGroupPairs": [
                    {"GroupId": base_id, "Description": "internal mesh, self-referential"}
                ],
            }
        ],
        f"{SG_BASE_NAME} self-referential mesh",
    )
    # The dependency edge with real stakes (plan §6): the exhibit host can reach
    # the database only through this rule. Delete SG-Base-VPC and RDS loses its
    # network. That is the outage the blast-radius warning prevents.
    authorize(
        aws,
        base_id,
        [
            {
                "IpProtocol": "tcp",
                "FromPort": 5432,
                "ToPort": 5432,
                "UserIdGroupPairs": [
                    {"GroupId": web_id, "Description": "PostgreSQL from the public tier"}
                ],
            }
        ],
        f"{SG_BASE_NAME} 5432 from {SG_PUBLIC_WEB_NAME}",
    )
    return base_id, web_id


# --------------------------------------------------------------------------
# Secret
# --------------------------------------------------------------------------


def ensure_secret(aws: Aws) -> str:
    desc = find_secret(aws)
    if desc is not None and desc.get("DeletedDate"):
        # teardown.py leaves a 7-day recovery window by default, and CreateSecret
        # refuses while a same-named secret is pending deletion. Restoring is the
        # reconcile path for a rebuild inside that window.
        aws.secrets.restore_secret(SecretId=SECRET_NAME)
        note(f"{SECRET_NAME} was pending deletion; restored")
        desc = find_secret(aws)
    if desc is not None:
        existing("secret", desc["ARN"], SECRET_NAME)
        if not desc.get("VersionIdsToStages"):
            warn(f"{SECRET_NAME} exists but holds no value yet")
            _print_secret_instructions(aws)
        return desc["ARN"]

    # Created with neither SecretString nor SecretBinary. The secret exists so
    # IAM can be scoped to its exact ARN; the value arrives separately.
    arn = aws.secrets.create_secret(
        Name=SECRET_NAME,
        Description="CockroachDB Cloud DSN for the lambo-cloudops exhibit. "
        "Resolved at service start by EC2-LamboWebExhibit and Lambda-LamboStats-API.",
        Tags=tags(SECRET_NAME),
    )["ARN"]
    created("secret", arn, SECRET_NAME)
    _print_secret_instructions(aws)
    return arn


def _print_secret_instructions(aws: Aws) -> None:
    say()
    warn("The DSN is NOT set by this script, and must never be passed to it.")
    say(
        f"""
    Set it once, from a shell whose history you control:

        read -rs LAMBO_DSN            # paste the DSN; it is not echoed
        aws secretsmanager put-secret-value \\
            --region {aws.region} --secret-id {SECRET_NAME} \\
            --secret-string "$LAMBO_DSN"
        unset LAMBO_DSN

    The DSN must carry sslmode=verify-full&sslrootcert=system (AGENTS.md).
    Nothing in this repo reads the value back: the exhibit instance and the
    stats Lambda each resolve it themselves at runtime through an IAM policy
    scoped to this one secret ARN.
"""
    )


# --------------------------------------------------------------------------


def main(args: argparse.Namespace) -> int:
    if args.dry_run:
        return _plan(args)

    require_boto3()
    aws = Aws(args.region, args.profile)
    ident = aws.whoami()
    step(f"region {args.region}, identity {ident['Arn']}")

    step("network foundation")
    vpc_id = ensure_vpc(aws)
    az_a = f"{args.region}{AZ_PRIMARY_SUFFIX}"
    az_b = f"{args.region}{AZ_SECONDARY_SUFFIX}"
    public_id = ensure_subnet(aws, vpc_id, SUBNET_PUBLIC_NAME, SUBNET_PUBLIC_CIDR, az_a, public=True)
    ensure_subnet(aws, vpc_id, SUBNET_PRIVATE_NAME, SUBNET_PRIVATE_CIDR, az_a, public=False)
    ensure_subnet(aws, vpc_id, SUBNET_PRIVATE_B_NAME, SUBNET_PRIVATE_B_CIDR, az_b, public=False)
    igw_id = ensure_igw(aws, vpc_id)
    ensure_public_route_table(aws, vpc_id, igw_id, public_id)

    step("security groups")
    base_id, web_id = ensure_security_groups(aws, vpc_id, args.ssh_cidr)

    step("secrets")
    ensure_secret(aws)

    say()
    step("network foundation ready")
    note(f"{VPC_NAME} = {vpc_id}")
    note(f"{SG_BASE_NAME} = {base_id}   {SG_PUBLIC_WEB_NAME} = {web_id}")
    note("no NAT gateway was created (plan §2)")
    note("port 80 and 443 are open from 0.0.0.0/0: 80 for the HTTP->HTTPS redirect and ACME HTTP-01")
    say()
    note("next: provision_app_data.py, then launch_exhibit_ec2.py")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    add_common_args(parser)
    parser.add_argument(
        "--ssh-cidr",
        required=True,
        type=cidr_arg,
        help=(
            "Source CIDR allowed to SSH to the exhibit instance. REQUIRED, with no "
            "default, and 0.0.0.0/0 is rejected. Typically "
            "$(curl -s https://checkip.amazonaws.com)/32."
        ),
    )
    return parser


if __name__ == "__main__":
    raise SystemExit(run_main(main, build_parser()))
