#!/usr/bin/env python3
"""Delete everything tagged `Project=lambo-cloudops` (plan §7).

Deletion order, which is dependency order run backwards:

     1. Lambda-LamboStats-API   (its Function URL goes with it)
     2. /aws/lambda/...         the log group AWS created implicitly
     3. lambo-cloudops-stats-role
     4. Elastic IP              disassociate, then release
     5. EC2-LamboWebExhibit     terminate, then wait for `terminated`
     6. instance profile + role
     7. RDS-Lambo-Demo-DB       SkipFinalSnapshot, then wait for gone
     8. lambo-cloudops-db-subnets
     9. SG-Base-VPC, SG-PublicWeb   revoke rules first, then delete
    10. RouteTable-Public       disassociate, then delete
    11. InternetGateway         detach, then delete
    12. the three subnets
    13. VPC-Enterprise-Prod
    14. lambo/cockroach-dsn     last, because everything above may read it

Three of those steps exist because of an ordering trap rather than a diagram:

* The two security groups reference each other, and AWS refuses to delete a
  group any rule still points at. Revoking every rule on both first breaks the
  cycle; deleting them in "the right order" does not, because there isn't one.
* Terminating an instance returns immediately but the ENI lingers for a minute
  or two, and the security group cannot go until it does. Hence the wait.
* The Lambda log group is created by the Lambda service, not by
  provision_app_data.py, so it carries no tags and tag-filtered discovery will
  never find it. It is deleted by name.

Safety: this script deletes nothing without `--confirm`. Run it bare to get the
discovered inventory and the deletion plan; run `--dry-run` to get the plan with
no AWS calls at all.

    python3 scripts/aws-infra/teardown.py                 # discover and report
    python3 scripts/aws-infra/teardown.py --dry-run       # offline plan
    python3 scripts/aws-infra/teardown.py --confirm       # actually delete
    python3 scripts/aws-infra/teardown.py --verify-only   # post-hoc sweep

Plan §7 asks for this to be proven once in a scratch region before it is ever
needed for real. `--verify-only` is the sweep that proves it: tag-filtered
describes that must all come back empty.
"""

from __future__ import annotations

import argparse
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from _common import (  # noqa: E402
    DB_SUBNET_GROUP,
    EC2_NAME,
    EIP_NAME,
    EXHIBIT_PROFILE_NAME,
    EXHIBIT_ROLE_NAME,
    IGW_NAME,
    LAMBDA_NAME,
    PROJECT,
    PROJECT_TAG_KEY,
    RDS_INSTANCE_ID,
    RDS_NAME,
    ROUTE_TABLE_PUBLIC_NAME,
    SECRET_NAME,
    SG_BASE_NAME,
    SG_PUBLIC_WEB_NAME,
    STATS_ROLE_NAME,
    SUBNET_PRIVATE_B_NAME,
    SUBNET_PRIVATE_NAME,
    SUBNET_PUBLIC_NAME,
    VPC_NAME,
    Aws,
    ClientError,
    InfraError,
    add_common_args,
    deleted,
    note,
    poll,
    project_filters,
    require_boto3,
    run_main,
    say,
    skipped,
    step,
    tag_value,
    warn,
    would,
)

LAMBDA_LOG_GROUP = f"/aws/lambda/{LAMBDA_NAME}"

DELETION_ORDER = [
    ("lambda", LAMBDA_NAME),
    ("log-group", LAMBDA_LOG_GROUP),
    ("iam-role", STATS_ROLE_NAME),
    ("elastic-ip", EIP_NAME),
    ("instance", EC2_NAME),
    ("instance-profile", EXHIBIT_PROFILE_NAME),
    ("iam-role", EXHIBIT_ROLE_NAME),
    ("rds-instance", RDS_INSTANCE_ID),
    ("db-subnet-group", DB_SUBNET_GROUP),
    ("security-group", SG_BASE_NAME),
    ("security-group", SG_PUBLIC_WEB_NAME),
    ("route-table", ROUTE_TABLE_PUBLIC_NAME),
    ("internet-gateway", IGW_NAME),
    ("subnet", SUBNET_PUBLIC_NAME),
    ("subnet", SUBNET_PRIVATE_NAME),
    ("subnet", SUBNET_PRIVATE_B_NAME),
    ("vpc", VPC_NAME),
    ("secret", SECRET_NAME),
]


# --------------------------------------------------------------------------
# Discovery (read only)
# --------------------------------------------------------------------------


def discover(aws: Aws) -> list[tuple[str, str, str]]:
    """Return `(kind, id, detail)` for everything this stack owns, in delete order.

    Every EC2 lookup filters on the project tag, which is the whole point of
    tagging at creation: there is no hand-maintained id list to drift.
    """
    found: list[tuple[str, str, str]] = []

    try:
        fn = aws.awslambda.get_function(FunctionName=LAMBDA_NAME)
        found.append(("lambda", LAMBDA_NAME, fn["Configuration"]["State"]))
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "ResourceNotFoundException":
            raise

    groups = aws.logs.describe_log_groups(logGroupNamePrefix=LAMBDA_LOG_GROUP)["logGroups"]
    if any(g["logGroupName"] == LAMBDA_LOG_GROUP for g in groups):
        found.append(("log-group", LAMBDA_LOG_GROUP, "implicitly created, untagged"))

    for role in (STATS_ROLE_NAME, EXHIBIT_ROLE_NAME):
        try:
            aws.iam.get_role(RoleName=role)
            found.append(("iam-role", role, ""))
        except ClientError as exc:
            if exc.response["Error"]["Code"] != "NoSuchEntity":
                raise

    try:
        aws.iam.get_instance_profile(InstanceProfileName=EXHIBIT_PROFILE_NAME)
        found.append(("instance-profile", EXHIBIT_PROFILE_NAME, ""))
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "NoSuchEntity":
            raise

    for address in aws.ec2.describe_addresses(Filters=project_filters())["Addresses"]:
        found.append(
            ("elastic-ip", address["AllocationId"], address.get("PublicIp", ""))
        )

    resp = aws.ec2.describe_instances(
        Filters=project_filters()
        + [
            {
                "Name": "instance-state-name",
                "Values": ["pending", "running", "stopping", "stopped"],
            }
        ]
    )
    for reservation in resp["Reservations"]:
        for inst in reservation["Instances"]:
            found.append(
                ("instance", inst["InstanceId"], f"{tag_value(inst, 'Name')} {inst['State']['Name']}")
            )

    try:
        db = aws.rds.describe_db_instances(DBInstanceIdentifier=RDS_INSTANCE_ID)["DBInstances"][0]
        found.append(("rds-instance", RDS_INSTANCE_ID, db["DBInstanceStatus"]))
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "DBInstanceNotFound":
            raise

    try:
        aws.rds.describe_db_subnet_groups(DBSubnetGroupName=DB_SUBNET_GROUP)
        found.append(("db-subnet-group", DB_SUBNET_GROUP, ""))
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "DBSubnetGroupNotFoundFault":
            raise

    for sg in aws.ec2.describe_security_groups(Filters=project_filters())["SecurityGroups"]:
        found.append(("security-group", sg["GroupId"], sg["GroupName"]))
    for rt in aws.ec2.describe_route_tables(Filters=project_filters())["RouteTables"]:
        found.append(("route-table", rt["RouteTableId"], tag_value(rt, "Name") or ""))
    for igw in aws.ec2.describe_internet_gateways(Filters=project_filters())["InternetGateways"]:
        found.append(("internet-gateway", igw["InternetGatewayId"], ""))
    for subnet in aws.ec2.describe_subnets(Filters=project_filters())["Subnets"]:
        found.append(
            ("subnet", subnet["SubnetId"], f"{tag_value(subnet, 'Name')} {subnet['CidrBlock']}")
        )
    for vpc in aws.ec2.describe_vpcs(Filters=project_filters())["Vpcs"]:
        found.append(("vpc", vpc["VpcId"], vpc["CidrBlock"]))

    try:
        desc = aws.secrets.describe_secret(SecretId=SECRET_NAME)
        state = "already scheduled for deletion" if desc.get("DeletedDate") else "active"
        found.append(("secret", SECRET_NAME, state))
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "ResourceNotFoundException":
            raise

    return found


# --------------------------------------------------------------------------
# Deletion
# --------------------------------------------------------------------------


def delete_lambda(aws: Aws) -> None:
    try:
        # Deleting the function removes its Function URL config and its resource
        # policy with it; there is nothing to unwind first.
        aws.awslambda.delete_function(FunctionName=LAMBDA_NAME)
        deleted("lambda", LAMBDA_NAME)
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "ResourceNotFoundException":
            raise
    try:
        aws.logs.delete_log_group(logGroupName=LAMBDA_LOG_GROUP)
        deleted("log-group", LAMBDA_LOG_GROUP)
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "ResourceNotFoundException":
            raise


def delete_role(aws: Aws, role_name: str) -> None:
    """A role cannot go while it still has policies or profile memberships."""
    try:
        aws.iam.get_role(RoleName=role_name)
    except ClientError as exc:
        if exc.response["Error"]["Code"] == "NoSuchEntity":
            return
        raise
    for name in aws.iam.list_role_policies(RoleName=role_name)["PolicyNames"]:
        aws.iam.delete_role_policy(RoleName=role_name, PolicyName=name)
    for policy in aws.iam.list_attached_role_policies(RoleName=role_name)["AttachedPolicies"]:
        aws.iam.detach_role_policy(RoleName=role_name, PolicyArn=policy["PolicyArn"])
    for profile in aws.iam.list_instance_profiles_for_role(RoleName=role_name)["InstanceProfiles"]:
        aws.iam.remove_role_from_instance_profile(
            InstanceProfileName=profile["InstanceProfileName"], RoleName=role_name
        )
    aws.iam.delete_role(RoleName=role_name)
    deleted("iam-role", role_name)


def delete_instance_profile(aws: Aws) -> None:
    try:
        profile = aws.iam.get_instance_profile(InstanceProfileName=EXHIBIT_PROFILE_NAME)[
            "InstanceProfile"
        ]
    except ClientError as exc:
        if exc.response["Error"]["Code"] == "NoSuchEntity":
            return
        raise
    for role in profile["Roles"]:
        aws.iam.remove_role_from_instance_profile(
            InstanceProfileName=EXHIBIT_PROFILE_NAME, RoleName=role["RoleName"]
        )
    aws.iam.delete_instance_profile(InstanceProfileName=EXHIBIT_PROFILE_NAME)
    deleted("instance-profile", EXHIBIT_PROFILE_NAME)


def delete_addresses(aws: Aws) -> None:
    for address in aws.ec2.describe_addresses(Filters=project_filters())["Addresses"]:
        if address.get("AssociationId"):
            aws.ec2.disassociate_address(AssociationId=address["AssociationId"])
        # Release, not just disassociate. An unassociated Elastic IP is billed by
        # the hour, which is the single most common thing left behind by hand.
        aws.ec2.release_address(AllocationId=address["AllocationId"])
        deleted("elastic-ip", address.get("PublicIp", address["AllocationId"]))


def delete_instances(aws: Aws) -> None:
    resp = aws.ec2.describe_instances(
        Filters=project_filters()
        + [
            {
                "Name": "instance-state-name",
                "Values": ["pending", "running", "stopping", "stopped"],
            }
        ]
    )
    ids = [i["InstanceId"] for r in resp["Reservations"] for i in r["Instances"]]
    if not ids:
        return
    aws.ec2.terminate_instances(InstanceIds=ids)
    for iid in ids:
        deleted("instance", iid)
    # Not politeness: the instance's ENI holds SG-PublicWeb, and the security
    # group delete below fails with DependencyViolation until it is released.
    note("waiting for termination so the ENIs release the security groups")
    aws.ec2.get_waiter("instance_terminated").wait(InstanceIds=ids)


def delete_rds(aws: Aws) -> None:
    try:
        aws.rds.describe_db_instances(DBInstanceIdentifier=RDS_INSTANCE_ID)
    except ClientError as exc:
        if exc.response["Error"]["Code"] == "DBInstanceNotFound":
            return
        raise
    aws.rds.delete_db_instance(
        DBInstanceIdentifier=RDS_INSTANCE_ID,
        # Plan §7 says skip the final snapshot. It is a demo workload with
        # BackupRetentionPeriod=0 and no data worth keeping, and a retained
        # snapshot would quietly keep billing after teardown "finished".
        SkipFinalSnapshot=True,
        DeleteAutomatedBackups=True,
    )
    deleted("rds-instance", RDS_INSTANCE_ID)
    note("the RDS-managed master password secret is deleted with the instance")
    poll(
        lambda: aws.rds.describe_db_instances(DBInstanceIdentifier=RDS_INSTANCE_ID),
        lambda _: False,
        f"{RDS_NAME} to finish deleting",
        timeout=1800,
        interval=30,
        gone_is_done=True,
    )


def delete_db_subnet_group(aws: Aws) -> None:
    try:
        aws.rds.delete_db_subnet_group(DBSubnetGroupName=DB_SUBNET_GROUP)
        deleted("db-subnet-group", DB_SUBNET_GROUP)
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "DBSubnetGroupNotFoundFault":
            raise


def delete_security_groups(aws: Aws) -> None:
    sgs = aws.ec2.describe_security_groups(Filters=project_filters())["SecurityGroups"]
    if not sgs:
        return
    # Two passes. SG-Base-VPC references SG-PublicWeb and itself, and AWS refuses
    # to delete a group while any rule still points at it, so no ordering of
    # deletes alone can work. Strip every rule first, then delete.
    for sg in sgs:
        if sg.get("IpPermissions"):
            aws.ec2.revoke_security_group_ingress(
                GroupId=sg["GroupId"], IpPermissions=sg["IpPermissions"]
            )
        if sg.get("IpPermissionsEgress"):
            aws.ec2.revoke_security_group_egress(
                GroupId=sg["GroupId"], IpPermissions=sg["IpPermissionsEgress"]
            )
        note(f"rules revoked on {sg['GroupName']} ({sg['GroupId']})")
    for sg in sgs:
        if sg["GroupName"] == "default":
            # The VPC's default group is not deletable and goes with the VPC.
            skipped("security-group", sg["GroupId"], "VPC default, deleted with the VPC")
            continue
        aws.ec2.delete_security_group(GroupId=sg["GroupId"])
        deleted("security-group", f"{sg['GroupId']} {sg['GroupName']}")


def delete_route_tables(aws: Aws) -> None:
    for rt in aws.ec2.describe_route_tables(Filters=project_filters())["RouteTables"]:
        for assoc in rt.get("Associations", []):
            if assoc.get("Main"):
                continue
            aws.ec2.disassociate_route_table(AssociationId=assoc["RouteTableAssociationId"])
        aws.ec2.delete_route_table(RouteTableId=rt["RouteTableId"])
        deleted("route-table", rt["RouteTableId"])


def delete_igws(aws: Aws) -> None:
    for igw in aws.ec2.describe_internet_gateways(Filters=project_filters())["InternetGateways"]:
        for attachment in igw.get("Attachments", []):
            aws.ec2.detach_internet_gateway(
                InternetGatewayId=igw["InternetGatewayId"], VpcId=attachment["VpcId"]
            )
        aws.ec2.delete_internet_gateway(InternetGatewayId=igw["InternetGatewayId"])
        deleted("internet-gateway", igw["InternetGatewayId"])


def delete_subnets(aws: Aws) -> None:
    for subnet in aws.ec2.describe_subnets(Filters=project_filters())["Subnets"]:
        aws.ec2.delete_subnet(SubnetId=subnet["SubnetId"])
        deleted("subnet", f"{subnet['SubnetId']} {tag_value(subnet, 'Name')}")


def delete_vpcs(aws: Aws) -> None:
    for vpc in aws.ec2.describe_vpcs(Filters=project_filters())["Vpcs"]:
        aws.ec2.delete_vpc(VpcId=vpc["VpcId"])
        deleted("vpc", vpc["VpcId"])


def delete_secret(aws: Aws, force: bool) -> None:
    try:
        if force:
            # Irreversible. Only on request, because re-provisioning inside the
            # recovery window is otherwise a restore rather than a re-create,
            # and provision_network.py already handles that case.
            aws.secrets.delete_secret(SecretId=SECRET_NAME, ForceDeleteWithoutRecovery=True)
            deleted("secret", f"{SECRET_NAME} (purged, unrecoverable)")
        else:
            aws.secrets.delete_secret(SecretId=SECRET_NAME, RecoveryWindowInDays=7)
            deleted("secret", f"{SECRET_NAME} (7 day recovery window)")
            note("provision_network.py restores it if you rebuild inside that window")
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "ResourceNotFoundException":
            raise


# --------------------------------------------------------------------------
# Verification sweep
# --------------------------------------------------------------------------


def verify(aws: Aws) -> int:
    """The tag-filtered sweep plan §7 asks for: everything must come back empty."""
    step("verification sweep (tag-filtered describes, all must be empty)")
    leftovers = discover(aws)
    # A secret in its recovery window is expected after a non-forced teardown, and
    # counting it as a leftover would make a correct teardown look failed.
    leftovers = [
        item for item in leftovers if not (item[0] == "secret" and "scheduled" in item[2])
    ]
    if not leftovers:
        say(f"  clean: nothing tagged {PROJECT_TAG_KEY}={PROJECT} remains in {aws.region}")
        return 0
    warn(f"{len(leftovers)} resource(s) still present:")
    for kind, ident, detail in leftovers:
        say(f"    {kind:<20} {ident} {detail}")
    note("re-run with --confirm; deletion is idempotent and picks up where it stopped")
    return 1


# --------------------------------------------------------------------------


def _plan_offline(args: argparse.Namespace) -> int:
    step(f"PLAN for region {args.region} (no AWS calls made)")
    note(f"discovery filters on tag {PROJECT_TAG_KEY}={PROJECT}; nothing else is touched")
    say()
    for kind, ident in DELETION_ORDER:
        would(kind, ident)
    say()
    note(f"the secret is deleted {'immediately' if args.force_delete_secret else 'with a 7 day recovery window'}")
    warn("this was the offline plan. Run without --dry-run to see what actually exists,")
    warn("and add --confirm to delete it.")
    return 0


def main(args: argparse.Namespace) -> int:
    if args.dry_run:
        if args.confirm:
            raise InfraError("--dry-run and --confirm are contradictory; pick one.")
        return _plan_offline(args)

    require_boto3()
    aws = Aws(args.region, args.profile)
    ident = aws.whoami()
    step(f"region {args.region}, identity {ident['Arn']}")

    if args.verify_only:
        return verify(aws)

    step(f"discovering resources tagged {PROJECT_TAG_KEY}={PROJECT}")
    found = discover(aws)
    if not found:
        say(f"  nothing tagged {PROJECT_TAG_KEY}={PROJECT} found in {aws.region}")
        return 0
    for kind, resource_id, detail in found:
        would(kind, resource_id, detail)

    if not args.confirm:
        say()
        warn(f"{len(found)} resource(s) would be deleted. NOTHING HAS BEEN DELETED.")
        say("    Re-run with --confirm to proceed:")
        say(f"        python3 scripts/aws-infra/teardown.py --region {args.region} --confirm")
        return 0

    say()
    step("deleting, in dependency order")
    delete_lambda(aws)
    delete_role(aws, STATS_ROLE_NAME)
    delete_addresses(aws)
    delete_instances(aws)
    delete_instance_profile(aws)
    delete_role(aws, EXHIBIT_ROLE_NAME)
    delete_rds(aws)
    delete_db_subnet_group(aws)
    delete_security_groups(aws)
    delete_route_tables(aws)
    delete_igws(aws)
    delete_subnets(aws)
    delete_vpcs(aws)
    delete_secret(aws, args.force_delete_secret)

    say()
    return verify(aws)


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    add_common_args(parser)
    parser.add_argument(
        "--confirm",
        action="store_true",
        help=(
            "Actually delete. Without it this script only discovers and reports; "
            "there is no way to delete anything by accident."
        ),
    )
    parser.add_argument(
        "--verify-only",
        action="store_true",
        help="Run only the tag-filtered verification sweep and report leftovers.",
    )
    parser.add_argument(
        "--force-delete-secret",
        action="store_true",
        help=(
            "Purge lambo/cockroach-dsn immediately instead of leaving the 7 day "
            "recovery window. Irreversible."
        ),
    )
    return parser


if __name__ == "__main__":
    raise SystemExit(run_main(main, build_parser()))
