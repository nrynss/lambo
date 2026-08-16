#!/usr/bin/env python3
"""Provision the `lambo-cloudops` app and data tier (plan §3 Track 2).

Creates:

    RDS-Lambo-Demo-DB       db.t4g.micro PostgreSQL, single-AZ, private subnet,
                            PubliclyAccessible=False, in SG-Base-VPC
    lambo-cloudops-db-subnets   DB subnet group over the two private subnets
    Lambda-LamboStats-API   python3.12 on arm64, OUTSIDE the VPC, Function URL
    lambo-cloudops-stats-role   execution role, scoped to the one secret

Two things in here are easy to get wrong, so they are stated up front.

**RDS is the tracked workload, not a Lambo store.** Plan §6. `migrations/cockroach/
001_init.sql` is CockroachDB-specific by construction - `embedding VECTOR(1024)`
and `CREATE VECTOR INDEX` - and will not apply to stock PostgreSQL. Worse, it
*looks* like it would work: `StoreKind::from_str` accepts "postgres" and "pg" as
aliases for Cockroach (src/store/mod.rs:403), so pointing lambo.toml at this
instance's DSN connects cleanly and only then fails applying the schema. Do not
wire Lambo at this database. Its job is to be the private-tier workload whose
dependency on SG-Base-VPC and Subnet-Private-1a gives those nodes their blast
radius - which is the entire reason the demo's climax has stakes.

**The Lambda is not in the VPC, and that is deliberate.** Plan §2 and §7. It
reads CockroachDB Cloud over the internet; a VPC-attached Lambda has no internet
route without a NAT gateway, and it has no reason to reach RDS. No `VpcConfig` is
passed anywhere below. If you find yourself adding one, you also need a NAT
gateway, and at that point re-read §2 rather than provisioning it.

Usage:

    python3 scripts/aws-infra/provision_app_data.py --session <lambo-session-id>
    python3 scripts/aws-infra/provision_app_data.py --session <id> --dry-run
    python3 scripts/aws-infra/provision_app_data.py --session <id> --skip-rds

Prerequisite: provision_network.py has run. Re-running is safe.
"""

from __future__ import annotations

import argparse
import json
import pathlib
import shutil
import subprocess
import sys
import tempfile
import zipfile

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from _common import (  # noqa: E402
    AZ_PRIMARY_SUFFIX,
    DB_SUBNET_GROUP,
    LAMBDA_NAME,
    PROJECT,
    PROJECT_TAG_KEY,
    RDS_INSTANCE_ID,
    RDS_NAME,
    SECRET_NAME,
    SG_BASE_NAME,
    STATS_ROLE_NAME,
    SUBNET_PRIVATE_B_NAME,
    SUBNET_PRIVATE_NAME,
    Aws,
    ClientError,
    InfraError,
    add_common_args,
    created,
    existing,
    note,
    poll,
    require_boto3,
    require_secret_arn,
    require_sg,
    require_subnet,
    require_vpc,
    run_main,
    say,
    step,
    tags,
    warn,
    would,
)

HERE = pathlib.Path(__file__).resolve().parent
LAMBDA_SRC = HERE / "lambda_src" / "lambo_stats.py"
LAMBDA_HANDLER = "lambo_stats.handler"
LAMBDA_RUNTIME = "python3.12"
# Pure-Python driver, so one zip works on arm64 and x86_64 alike. Pinned to a
# major range rather than an exact version: a floating major could change the
# DBAPI surface the handler depends on.
PG8000_SPEC = "pg8000>=1.30,<2"

MASTER_USERNAME = "lambo_admin"


def _plan(args: argparse.Namespace) -> int:
    step(f"PLAN for region {args.region} (no AWS calls made)")
    if args.skip_rds:
        note("RDS skipped by --skip-rds")
    else:
        would("db-subnet-group", DB_SUBNET_GROUP, f"{SUBNET_PRIVATE_NAME} + {SUBNET_PRIVATE_B_NAME}")
        would(
            "rds-instance",
            RDS_INSTANCE_ID,
            f"db.t4g.micro postgres, 20GB gp3, single-AZ, {args.region}{AZ_PRIMARY_SUFFIX}, "
            "PubliclyAccessible=False",
        )
        note("master password is managed by RDS in its own secret; this script never sees it")
        note("NOT a Lambo store - plan §6")
    if args.skip_lambda:
        note("Lambda skipped by --skip-lambda")
    else:
        would("iam-role", STATS_ROLE_NAME, f"logs + GetSecretValue on {SECRET_NAME} only")
        would("lambda", LAMBDA_NAME, f"{LAMBDA_RUNTIME} arm64, NO VpcConfig (plan §2)")
        would("function-url", LAMBDA_NAME, "AuthType=NONE, public read-only stats")
        note(f"deployment zip is built locally from {LAMBDA_SRC.relative_to(HERE.parent.parent)}")
        note(f"vendoring {PG8000_SPEC} into the zip requires pip and network access")
    say()
    note("no NAT gateway is created, by design (plan §2)")
    note(f"every resource is tagged {PROJECT_TAG_KEY}={PROJECT}")
    return 0


# --------------------------------------------------------------------------
# RDS
# --------------------------------------------------------------------------


def ensure_db_subnet_group(aws: Aws, subnet_ids: list[str]) -> None:
    try:
        aws.rds.describe_db_subnet_groups(DBSubnetGroupName=DB_SUBNET_GROUP)
        existing("db-subnet-group", DB_SUBNET_GROUP)
        return
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "DBSubnetGroupNotFoundFault":
            raise
    # RDS requires at least two subnets in two different AZs here even for a
    # single-AZ instance. That is why provision_network.py creates
    # Subnet-Private-1b; the instance itself is still pinned to AZ "a" below.
    aws.rds.create_db_subnet_group(
        DBSubnetGroupName=DB_SUBNET_GROUP,
        DBSubnetGroupDescription="Private subnets for the lambo-cloudops tracked workload.",
        SubnetIds=subnet_ids,
        Tags=tags(DB_SUBNET_GROUP),
    )
    created("db-subnet-group", DB_SUBNET_GROUP)


def find_db(aws: Aws) -> dict | None:
    try:
        return aws.rds.describe_db_instances(DBInstanceIdentifier=RDS_INSTANCE_ID)["DBInstances"][0]
    except ClientError as exc:
        if exc.response["Error"]["Code"] == "DBInstanceNotFound":
            return None
        raise


def ensure_rds(aws: Aws, sg_id: str, az: str, wait: bool) -> None:
    db = find_db(aws)
    if db is not None:
        existing("rds-instance", RDS_INSTANCE_ID, db["DBInstanceStatus"])
    else:
        aws.rds.create_db_instance(
            DBInstanceIdentifier=RDS_INSTANCE_ID,
            DBInstanceClass="db.t4g.micro",
            Engine="postgres",
            # EngineVersion is left unset on purpose: RDS picks the current
            # default major version, so this script does not rot the way a
            # pinned version does when AWS deprecates it.
            MasterUsername=MASTER_USERNAME,
            # RDS generates the master password and stores it in a secret it
            # owns. Nothing here ever handles, prints, or logs a password, and
            # the secret is deleted with the instance.
            ManageMasterUserPassword=True,
            AllocatedStorage=20,
            StorageType="gp3",
            StorageEncrypted=True,
            DBSubnetGroupName=DB_SUBNET_GROUP,
            VpcSecurityGroupIds=[sg_id],
            AvailabilityZone=az,
            MultiAZ=False,
            # The plan's hard requirement (§2, §11): the private tier is private.
            PubliclyAccessible=False,
            # Demo workload with no data worth restoring. This also makes
            # teardown's SkipFinalSnapshot honest rather than reckless.
            BackupRetentionPeriod=0,
            AutoMinorVersionUpgrade=True,
            CopyTagsToSnapshot=True,
            DeletionProtection=False,
            Tags=tags(RDS_NAME),
        )
        created("rds-instance", RDS_INSTANCE_ID, "creating")

    if not wait:
        note("not waiting for the instance to become available (--no-wait)")
        return

    poll(
        lambda: aws.rds.describe_db_instances(DBInstanceIdentifier=RDS_INSTANCE_ID)["DBInstances"][0],
        lambda d: d["DBInstanceStatus"] == "available",
        f"{RDS_INSTANCE_ID} to become available",
        timeout=1800,
        interval=30,
    )
    db = find_db(aws) or {}
    endpoint = (db.get("Endpoint") or {}).get("Address", "n/a")
    note(f"{RDS_NAME} endpoint {endpoint} (reachable only from inside the VPC)")
    warn("This is NOT a Lambo store. Do not point lambo.toml at it - plan §6.")


# --------------------------------------------------------------------------
# Lambda
# --------------------------------------------------------------------------


def build_lambda_zip(dest: pathlib.Path) -> pathlib.Path:
    """Vendor the driver and the handler into a deployment zip.

    Kept out of the repo: a committed zip would go stale silently and would put a
    third-party wheel in version control. Built here instead, from the handler
    source next door plus one pip install.
    """
    if not LAMBDA_SRC.is_file():
        raise InfraError(
            f"the Lambda handler source is missing: {LAMBDA_SRC}",
            hint="it ships in this repo at scripts/aws-infra/lambda_src/lambo_stats.py.",
        )
    build = dest / "build"
    build.mkdir(parents=True, exist_ok=True)
    note(f"vendoring {PG8000_SPEC} into the deployment package")
    try:
        subprocess.run(
            [
                sys.executable,
                "-m",
                "pip",
                "install",
                "--quiet",
                "--no-compile",
                "--target",
                str(build),
                PG8000_SPEC,
            ],
            check=True,
            capture_output=True,
            text=True,
        )
    except FileNotFoundError as exc:
        raise InfraError(
            "pip is not available, so the Lambda package cannot be built.",
            hint=(
                "install pip, or build the zip elsewhere and pass it with "
                "--lambda-zip <path>."
            ),
        ) from exc
    except subprocess.CalledProcessError as exc:
        raise InfraError(
            f"pip failed to vendor {PG8000_SPEC}: {(exc.stderr or '').strip().splitlines()[-1:]}",
            hint="check network access to PyPI, or pass a prebuilt zip with --lambda-zip.",
        ) from exc

    shutil.copy2(LAMBDA_SRC, build / LAMBDA_SRC.name)

    archive = dest / "lambo-stats.zip"
    # Sorted walk and a fixed timestamp: the zip is then byte-identical for
    # identical inputs, so `update_function_code` is a genuine no-op on a re-run
    # rather than a new version every time.
    with zipfile.ZipFile(archive, "w", zipfile.ZIP_DEFLATED) as zf:
        for path in sorted(build.rglob("*")):
            if path.is_file() and "__pycache__" not in path.parts:
                info = zipfile.ZipInfo(str(path.relative_to(build)), date_time=(1980, 1, 1, 0, 0, 0))
                info.external_attr = 0o644 << 16
                info.compress_type = zipfile.ZIP_DEFLATED
                zf.writestr(info, path.read_bytes())
    return archive


def ensure_stats_role(aws: Aws, secret_arn: str) -> str:
    trust = {
        "Version": "2012-10-17",
        "Statement": [
            {
                "Effect": "Allow",
                "Principal": {"Service": "lambda.amazonaws.com"},
                "Action": "sts:AssumeRole",
            }
        ],
    }
    try:
        arn = aws.iam.get_role(RoleName=STATS_ROLE_NAME)["Role"]["Arn"]
        existing("iam-role", STATS_ROLE_NAME)
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "NoSuchEntity":
            raise
        arn = aws.iam.create_role(
            RoleName=STATS_ROLE_NAME,
            AssumeRolePolicyDocument=json.dumps(trust),
            Description="Execution role for Lambda-LamboStats-API.",
            Tags=tags(STATS_ROLE_NAME),
        )["Role"]["Arn"]
        created("iam-role", STATS_ROLE_NAME)

    aws.iam.attach_role_policy(
        RoleName=STATS_ROLE_NAME,
        PolicyArn="arn:aws:iam::aws:policy/service-role/AWSLambdaBasicExecutionRole",
    )
    # Exactly one secret, by full ARN. Not secretsmanager:*, not a resource
    # wildcard - plan §8 and the Phase 3 checklist.
    aws.iam.put_role_policy(
        RoleName=STATS_ROLE_NAME,
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
    note(f"{STATS_ROLE_NAME} may read exactly {SECRET_NAME}, nothing wider")
    return arn


def ensure_lambda(aws: Aws, role_arn: str, zip_path: pathlib.Path, session: str) -> str:
    payload = zip_path.read_bytes()
    env = {"LAMBO_DSN_SECRET_ID": SECRET_NAME, "LAMBO_SESSION": session}

    try:
        fn = aws.awslambda.get_function(FunctionName=LAMBDA_NAME)
        existing("lambda", LAMBDA_NAME, fn["Configuration"]["State"])
        aws.awslambda.update_function_code(FunctionName=LAMBDA_NAME, ZipFile=payload, Publish=False)
        aws.awslambda.get_waiter("function_updated_v2").wait(FunctionName=LAMBDA_NAME)
        aws.awslambda.update_function_configuration(
            FunctionName=LAMBDA_NAME,
            Role=role_arn,
            Handler=LAMBDA_HANDLER,
            Runtime=LAMBDA_RUNTIME,
            Timeout=15,
            MemorySize=256,
            Environment={"Variables": env},
        )
        aws.awslambda.get_waiter("function_updated_v2").wait(FunctionName=LAMBDA_NAME)
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "ResourceNotFoundException":
            raise
        # IAM role propagation to Lambda is eventually consistent; the first
        # CreateFunction after CreateRole routinely fails with
        # InvalidParameterValueException. Retry rather than make the operator
        # re-run.
        for attempt in range(10):
            try:
                aws.awslambda.create_function(
                    FunctionName=LAMBDA_NAME,
                    Runtime=LAMBDA_RUNTIME,
                    Role=role_arn,
                    Handler=LAMBDA_HANDLER,
                    Code={"ZipFile": payload},
                    Description="Public read-only stats over the live CockroachDB session.",
                    Timeout=15,
                    MemorySize=256,
                    # arm64 to match the rest of the stack, and cheaper per ms.
                    # pg8000 is pure Python so the package is architecture-free.
                    Architectures=["arm64"],
                    Environment={"Variables": env},
                    # NO VpcConfig. See the module docstring and plan §2.
                    Tags={t["Key"]: t["Value"] for t in tags(LAMBDA_NAME)},
                )
                break
            except ClientError as create_exc:
                code = create_exc.response["Error"]["Code"]
                message = create_exc.response["Error"].get("Message", "")
                retryable = code == "InvalidParameterValueException" and "assume" in message.lower()
                if not retryable or attempt == 9:
                    raise
                import time

                time.sleep(5)
        aws.awslambda.get_waiter("function_active_v2").wait(FunctionName=LAMBDA_NAME)
        created("lambda", LAMBDA_NAME)

    # Function URL: public and unauthenticated, matching the plan's "public
    # read-only stats endpoint". The handler only ever SELECTs, and its error
    # path returns a generic message so a driver error cannot leak the DSN host.
    try:
        url = aws.awslambda.get_function_url_config(FunctionName=LAMBDA_NAME)["FunctionUrl"]
        existing("function-url", url)
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "ResourceNotFoundException":
            raise
        url = aws.awslambda.create_function_url_config(
            FunctionName=LAMBDA_NAME,
            AuthType="NONE",
            Cors={"AllowOrigins": ["*"], "AllowMethods": ["GET"]},
        )["FunctionUrl"]
        created("function-url", url)

    # AuthType=NONE still needs an explicit resource policy statement, or every
    # request returns 403. This is the single most commonly missed step.
    try:
        aws.awslambda.add_permission(
            FunctionName=LAMBDA_NAME,
            StatementId="AllowPublicFunctionUrl",
            Action="lambda:InvokeFunctionUrl",
            Principal="*",
            FunctionUrlAuthType="NONE",
        )
        note("public invoke permission added to the function URL")
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "ResourceConflictException":
            raise
        note("public invoke permission already present")
    return url


# --------------------------------------------------------------------------


def main(args: argparse.Namespace) -> int:
    if args.dry_run:
        return _plan(args)

    require_boto3()
    aws = Aws(args.region, args.profile)
    ident = aws.whoami()
    step(f"region {args.region}, identity {ident['Arn']}")

    # Fail on the prerequisite, not on a NoneType attribute error deeper in.
    require_vpc(aws)
    secret_arn = require_secret_arn(aws)

    if not args.skip_rds:
        step("data tier (the tracked workload - NOT a Lambo store, plan §6)")
        private_a = require_subnet(aws, SUBNET_PRIVATE_NAME)
        private_b = require_subnet(aws, SUBNET_PRIVATE_B_NAME)
        base_sg = require_sg(aws, SG_BASE_NAME)
        ensure_db_subnet_group(aws, [private_a["SubnetId"], private_b["SubnetId"]])
        ensure_rds(aws, base_sg["GroupId"], private_a["AvailabilityZone"], wait=not args.no_wait)

    url = None
    if not args.skip_lambda:
        step("app tier (outside the VPC, plan §2)")
        role_arn = ensure_stats_role(aws, secret_arn)
        if args.lambda_zip:
            zip_path = pathlib.Path(args.lambda_zip).expanduser().resolve()
            if not zip_path.is_file():
                raise InfraError(f"--lambda-zip {zip_path} does not exist.")
            url = ensure_lambda(aws, role_arn, zip_path, args.session)
        else:
            with tempfile.TemporaryDirectory(prefix="lambo-lambda-") as tmp:
                zip_path = build_lambda_zip(pathlib.Path(tmp))
                url = ensure_lambda(aws, role_arn, zip_path, args.session)

    say()
    step("app and data tier ready")
    if url:
        note(f"stats endpoint: {url}")
        note(f"it returns 503 until {SECRET_NAME} holds a working DSN")
    note("no NAT gateway was created (plan §2)")
    say()
    note("next: launch_exhibit_ec2.py --hostname <yours> --session <id>")
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    add_common_args(parser)
    parser.add_argument(
        "--session",
        required=True,
        help="Lambo session id the stats endpoint reports on (the same one serve-web opens).",
    )
    parser.add_argument(
        "--skip-rds", action="store_true", help="Provision only the Lambda tier."
    )
    parser.add_argument(
        "--skip-lambda", action="store_true", help="Provision only the RDS tier."
    )
    parser.add_argument(
        "--lambda-zip",
        default=None,
        help=(
            "Use a prebuilt deployment zip instead of building one here. Useful on a "
            "machine with no PyPI access."
        ),
    )
    parser.add_argument(
        "--no-wait",
        action="store_true",
        help=(
            "Return without waiting for RDS to reach `available` (it takes 5 to 10 "
            "minutes). Re-run later to pick up where this left off."
        ),
    )
    return parser


if __name__ == "__main__":
    raise SystemExit(run_main(main, build_parser()))
