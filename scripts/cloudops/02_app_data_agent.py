#!/usr/bin/env python3
"""The `app-data-agent` track (plan §3 Track 2), Phase 4.

Discovers the app and data tier that `scripts/aws-infra/provision_app_data.py`
created, asks Lambo what the network agent already recorded, and writes the
cross-tier dependency edges that give the shared network resources their blast
radius.

Discovered, all by tag or by the identifier provisioning used, and all
read-only:

    lambo-cloudops-db-subnets   the DB subnet group over the two private subnets
    rds-lambo-demo-db           the private-tier workload (Name tag RDS-Lambo-Demo-DB)
    Lambda-LamboStats-API       the stats function, outside the VPC by design
    lambo-cloudops-stats-role   the Lambda execution role
    lambo-cloudops-exhibit-role the exhibit instance profile's role, if it exists

This script **creates and modifies nothing in AWS**, and it never reads the
secret's value.

## The edges this script exists to write

Plan §3 Track 2 step 3, verbatim: the RDS instance depends on
`VPC-Enterprise-Prod`, `Subnet-Private-1a` and `SG-Base-VPC`. The Lambda depends
on `lambo/cockroach-dsn` and its execution role, and on nothing inside the VPC,
because it runs outside it (plan §2) and a `VpcConfig` here would need a NAT
gateway the design deliberately does not have.

`RDS-Lambo-Demo-DB` is placed under `SG-Base-VPC` as a hierarchy child, and that
is the load-bearing detail of the whole demo: it is what makes the database show
up as a *dependent* when the next script inspects the shared security group, and
what turns "delete an idle security group" into "the database loses its
network". It also means the RDS action below must not `--produces` the database.
A produce would write a second structural edge into it, and a concept with two
structural sources counts toward neither of them, so the group's blast radius
would quietly drop by one and take the dependent's name off the warning with it.

## RDS is not a Lambo store

Plan §6. It is the workload whose dependencies Lambo tracks. Nothing here reads
or writes it, and `lambo.toml` must never point at it: the Cockroach migration
uses `VECTOR(1024)` and `CREATE VECTOR INDEX`, and `StoreKind::from_str` accepts
"postgres" as an alias for Cockroach, so pointing Lambo at this instance
connects cleanly and only then fails applying the schema.

Usage:

    python3 scripts/cloudops/02_app_data_agent.py --session <lambo-session-id>
    python3 scripts/cloudops/02_app_data_agent.py --session <id> --dry-run

Prerequisites: `01_network_agent.py` has run against this session, and no other
process holds the session's writer lease. Re-running is safe.
"""

from __future__ import annotations

import argparse
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from _lambo import (  # noqa: E402
    AGENT_APP_DATA,
    AGENT_NETWORK,
    CONFIG_KIND,
    IDENTITY_KIND,
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
    VPC_NAME,
    Aws,
    ClientError,
    InfraError,
    Lambo,
    account_binding,
    add_common_args,
    add_lambo_args,
    check_single_source,
    derived,
    find_secret,
    note,
    parse_outbound_neighbours,
    queried,
    recorded,
    require_boto3,
    require_sg,
    require_subnet,
    require_vpc,
    resolve_lambo_binary,
    run_main,
    say,
    skipped,
    step,
    warn,
    would,
)

# Names `_common` does not export, because Phase 3 only ever used them as local
# strings. Kept spelled out here rather than reconstructed, so a rename in the
# sibling scripts shows up as a mismatch a reader can see.
DB_SUBNET_GROUP = "lambo-cloudops-db-subnets"
EXHIBIT_ROLE_NAME = "lambo-cloudops-exhibit-role"

# What 01 must already have put in the graph. Checked before anything is
# written, because `lambo derive --parent-of` happily *creates* a missing parent
# as a bare Entity: run out of order and the graph would grow a second,
# contentless `VPC-Enterprise-Prod` shaped node instead of failing.
REQUIRED_FROM_NETWORK_AGENT = (SG_BASE_NAME, SUBNET_PRIVATE_NAME)


def _plan(args: argparse.Namespace) -> int:
    step(f"PLAN for region {args.region} (no AWS calls, and no lambo calls, are made)")
    say()
    note(f"agent identity: {AGENT_APP_DATA}")
    note(f"session: {args.session}")
    say()
    step("discover, read-only")
    if args.skip_rds:
        note("RDS skipped by --skip-rds")
    else:
        would("db-subnet-group", DB_SUBNET_GROUP, "spans the two private subnets")
        would("rds-instance", RDS_INSTANCE_ID, f"Name tag {RDS_NAME}")
    if args.skip_lambda:
        note("Lambda skipped by --skip-lambda")
    else:
        would("lambda", LAMBDA_NAME, "runtime, architecture and VpcConfig")
        would("iam-role", STATS_ROLE_NAME, "existence only")
    would("iam-role", EXHIBIT_ROLE_NAME, "optional; skipped if the exhibit is not provisioned")
    would("secret", SECRET_NAME, "existence only; the value is never read")

    say()
    step("query Lambo for what the network agent recorded")
    would("inspect", ", ".join(REQUIRED_FROM_NETWORK_AGENT), "by name, expecting each present")

    say()
    step("derive into Lambo")
    if not args.skip_rds:
        would("concept", DB_SUBNET_GROUP, f"entity, parent_of {VPC_NAME}")
        would("concept", RDS_NAME, f"entity, parent_of {SG_BASE_NAME}")
        would("concept", f"{RDS_NAME} = <endpoint>", f"entity, parent_of {RDS_NAME}")
        would(
            "concept",
            f"{RDS_NAME} is not publicly accessible",
            f"constraint, parent_of {RDS_NAME}",
        )
    if not args.skip_lambda:
        would("concept", LAMBDA_NAME, "entity, no parent; it is outside the VPC")
        would(
            "concept",
            f"{LAMBDA_NAME} runs outside {VPC_NAME}",
            f"constraint, parent_of {LAMBDA_NAME}",
        )
        would("concept", STATS_ROLE_NAME, "entity, no parent")

    say()
    step("record the cross-tier dependency edges")
    if not args.skip_rds:
        would(
            "action",
            f"{AGENT_APP_DATA} provisioned {RDS_NAME}",
            f"depends-on {VPC_NAME}, {SUBNET_PRIVATE_NAME}, {SG_BASE_NAME}",
        )
    if not args.skip_lambda:
        would(
            "action",
            f"{AGENT_APP_DATA} deployed {LAMBDA_NAME}",
            f"produces {LAMBDA_NAME}; depends-on {SECRET_NAME}, {STATS_ROLE_NAME}",
        )
    would(
        "action",
        f"{AGENT_APP_DATA} bound the workloads into the network",
        f"depends-on {VPC_NAME}, {SUBNET_PRIVATE_NAME}, {SG_BASE_NAME}",
    )

    say()
    note("nothing in AWS is created, modified or deleted by this script")
    note(f"{RDS_NAME} is the tracked workload, not a Lambo store (plan §6)")
    note(f"{LAMBDA_NAME} depends on nothing inside the VPC, by design (plan §2)")
    return 0


# --------------------------------------------------------------------------
# Discovery (read-only)
# --------------------------------------------------------------------------


class AppData:
    def __init__(self) -> None:
        self.db_subnet_group: bool = False
        self.db_subnets: list[str] = []
        self.rds_endpoint: str | None = None
        self.rds_public: bool = False
        self.rds_class: str = ""
        self.rds_engine: str = ""
        self.lambda_runtime: str = ""
        self.lambda_arch: str = ""
        self.lambda_in_vpc: bool = False
        self.stats_role: bool = False
        self.exhibit_role: bool = False
        self.secret: bool = False


def _missing(what: str, ident: str) -> InfraError:
    return InfraError(
        f"no {what} named {ident} exists in this account and region.",
        hint=(
            "run `python3 scripts/aws-infra/provision_app_data.py --session <id>` "
            "first. This script only reads; it never creates anything."
        ),
    )


def _role_exists(aws: Aws, name: str) -> bool:
    try:
        aws.iam.get_role(RoleName=name)
        return True
    except ClientError as exc:
        if exc.response["Error"]["Code"] == "NoSuchEntity":
            return False
        raise


def discover(aws: Aws, skip_rds: bool, skip_lambda: bool) -> AppData:
    app = AppData()
    # Fail on the prerequisite tier, not on a KeyError deeper in. The network
    # tier has to exist for any of the dependency edges below to mean anything.
    require_vpc(aws)
    require_subnet(aws, SUBNET_PRIVATE_NAME)
    require_sg(aws, SG_BASE_NAME)
    app.secret = find_secret(aws) is not None

    if not skip_rds:
        try:
            groups = aws.rds.describe_db_subnet_groups(DBSubnetGroupName=DB_SUBNET_GROUP)
        except ClientError as exc:
            if exc.response["Error"]["Code"] != "DBSubnetGroupNotFoundFault":
                raise
            raise _missing("DB subnet group", DB_SUBNET_GROUP) from exc
        app.db_subnet_group = True
        app.db_subnets = sorted(
            s["SubnetIdentifier"] for s in groups["DBSubnetGroups"][0].get("Subnets") or []
        )

        try:
            found = aws.rds.describe_db_instances(DBInstanceIdentifier=RDS_INSTANCE_ID)
            db = found["DBInstances"][0]
        except ClientError as exc:
            if exc.response["Error"]["Code"] != "DBInstanceNotFound":
                raise
            raise _missing("RDS instance", RDS_INSTANCE_ID) from exc
        # The endpoint is absent until the instance leaves `creating`. That is a
        # normal intermediate state, not an error: the topology is still worth
        # deriving, so record what is known and say what is not.
        app.rds_endpoint = (db.get("Endpoint") or {}).get("Address")
        app.rds_public = bool(db.get("PubliclyAccessible"))
        app.rds_class = db.get("DBInstanceClass", "")
        app.rds_engine = db.get("Engine", "")

    if not skip_lambda:
        try:
            fn = aws.awslambda.get_function(FunctionName=LAMBDA_NAME)
        except ClientError as exc:
            if exc.response["Error"]["Code"] != "ResourceNotFoundException":
                raise
            raise _missing("Lambda function", LAMBDA_NAME) from exc
        cfg = fn["Configuration"]
        app.lambda_runtime = cfg.get("Runtime", "")
        app.lambda_arch = (cfg.get("Architectures") or ["unknown"])[0]
        # Plan §2 and §7: no VpcConfig, ever. `get_function` returns an empty
        # VpcConfig rather than omitting it on some API versions, so test the
        # subnet list rather than the key's presence.
        app.lambda_in_vpc = bool((cfg.get("VpcConfig") or {}).get("SubnetIds"))
        app.stats_role = _role_exists(aws, STATS_ROLE_NAME)
        if not app.stats_role:
            raise _missing("IAM role", STATS_ROLE_NAME)

    app.exhibit_role = _role_exists(aws, EXHIBIT_ROLE_NAME)
    return app


# --------------------------------------------------------------------------
# Asking Lambo what the network agent already knows
# --------------------------------------------------------------------------


def read_network_topology(lam: Lambo) -> list[str]:
    """Plan §3 Track 2 step 3: discover the existing topology from memory.

    `inspect` rather than `recall`, because this is a structural question with a
    right answer, and recall is a ranked semantic search that could plausibly
    return the network tier's neighbours in a different order or miss one under
    a token budget. The point of the check is to be certain, not to be relevant.

    Each required node is asked for **by name**. Asking once for the VPC and
    reading its hop-1 neighbour list is not enough: that list is bounded by the
    CLI's neighbour budget (`MAX_INSPECT_NODES`), and once the session crosses
    the bound the list truncates and a required node can soundly vanish, making
    a large-but-fine session look like the network agent never ran. Asking for a
    node by name is exact regardless of how many children it has — the focus
    itself is always reported, and an absent one is an error.
    """
    present: list[str] = []
    for name in REQUIRED_FROM_NETWORK_AGENT:
        try:
            lam.inspect(name, depth=1)
        except InfraError as exc:
            raise InfraError(
                f"{name} is not in this Lambo session.",
                hint=(
                    "01_network_agent.py has not finished against this session, or it "
                    "ran against a different one. Re-run it, then re-run this script."
                ),
            ) from exc
        queried("inspect", name, "present in the session")
        present.append(name)
    return present


# --------------------------------------------------------------------------
# Derivation
# --------------------------------------------------------------------------


def derive_topology(lam: Lambo, app: AppData, skip_rds: bool, skip_lambda: bool) -> None:
    """Where each app and data resource sits.

    Containment parents, and the reasoning behind each:

    * `lambo-cloudops-db-subnets` under `VPC-Enterprise-Prod`. It spans both
      private subnets, so neither one of them can own it without the other
      becoming a second structural source.
    * `RDS-Lambo-Demo-DB` under `SG-Base-VPC`. Membership of the shared group is
      the relationship the demo is about; the subnet placement is recorded as an
      action dependency instead.
    * `Lambda-LamboStats-API` and the IAM roles have no parent. Nothing in the
      VPC contains them, and inventing a parent for the Lambda would be the same
      mistake as giving it a `VpcConfig`.

    The RDS tier and the Lambda tier are derived in **separate** calls. Lambo
    links everything written by one interaction with a `CoOccurrence` edge, and
    the architecture deliberately isolates the two tiers: co-deriving them would
    assert a relationship that does not exist. The DB subnet group stays with
    RDS (same tier) and the roles stay with the Lambda (same tier); only the
    cross-tier pairing is split apart.
    """


    if not skip_rds:
        rds_pairs = [(VPC_NAME, DB_SUBNET_GROUP), (SG_BASE_NAME, RDS_NAME)]
        check_single_source(rds_pairs)
        out = lam.derive(
            AGENT_APP_DATA,
            DB_SUBNET_GROUP,
            "entity",
            concepts=[f"{RDS_NAME}:entity"],
            parent_of=rds_pairs,
        )
        derived("topology", DB_SUBNET_GROUP, "+1 concept(s), 2 hierarchy edge(s)")
        note(out)

    if not skip_lambda:
        lam_concepts = [f"{STATS_ROLE_NAME}:entity"]
        if app.exhibit_role:
            lam_concepts.append(f"{EXHIBIT_ROLE_NAME}:entity")
        out = lam.derive(AGENT_APP_DATA, LAMBDA_NAME, "entity", concepts=lam_concepts)
        derived("topology", LAMBDA_NAME, f"+{len(lam_concepts)} concept(s)")
        note(out)
    elif app.exhibit_role:
        # Both tiers skipped by flag, but the exhibit role still exists on the
        # account; record it rather than silently dropping it.
        out = lam.derive(AGENT_APP_DATA, EXHIBIT_ROLE_NAME, "entity")
        derived("topology", EXHIBIT_ROLE_NAME, "+0 concept(s)")
        note(out)
    elif skip_rds:
        warn("both tiers were skipped; nothing to derive")


def derive_facts(lam: Lambo, app: AppData, skip_rds: bool, skip_lambda: bool) -> None:
    """The discovered detail, as children of the resource it belongs to.

    Every one of these is read back out of the account rather than asserted from
    the plan, which is the point: a concept that says the database is private is
    only worth having if it went stale when the database stopped being private.
    They are also the children that nothing else ever points at, so they are what
    keeps a parent's blast radius above zero once the cross-tier dependency edges
    below start landing on the network tier.

    None of them is derived as an `observation`, however natural that would read.
    See `IDENTITY_KIND` in `_lambo.py`: `canonicalize` skips observations when
    matching, so one would be duplicated on every run.
    """
    facts: list[tuple[str, str, str]] = []  # (parent, content, kind)

    if not skip_rds:
        if app.rds_endpoint:
            facts.append((RDS_NAME, account_binding(RDS_NAME, app.rds_endpoint), IDENTITY_KIND))
        else:
            skipped("fact", f"{RDS_NAME} endpoint", "the instance is still creating")
        if app.rds_public:
            # Loud, because plan §2 and §11 both require the private tier to be
            # private, and a graph that quietly recorded the opposite would be
            # worse than no graph.
            warn(f"{RDS_NAME} is publicly accessible, which contradicts plan §2")
            facts.append((RDS_NAME, f"{RDS_NAME} is publicly accessible", CONFIG_KIND))
        else:
            facts.append((RDS_NAME, f"{RDS_NAME} is not publicly accessible", CONFIG_KIND))
        if app.rds_class and app.rds_engine:
            facts.append(
                (RDS_NAME, f"{RDS_NAME} runs {app.rds_engine} on {app.rds_class}", CONFIG_KIND)
            )
        if app.db_subnets:
            facts.append(
                (
                    DB_SUBNET_GROUP,
                    f"{DB_SUBNET_GROUP} spans {SUBNET_PRIVATE_NAME} and {SUBNET_PRIVATE_B_NAME}",
                    CONFIG_KIND,
                )
            )

    if not skip_lambda:
        if app.lambda_in_vpc:
            warn(f"{LAMBDA_NAME} is attached to a VPC, which contradicts plan §2 and needs a NAT")
            facts.append((LAMBDA_NAME, f"{LAMBDA_NAME} is attached to a VPC", CONFIG_KIND))
        else:
            facts.append((LAMBDA_NAME, f"{LAMBDA_NAME} runs outside {VPC_NAME}", CONFIG_KIND))
        if app.lambda_runtime and app.lambda_arch:
            facts.append(
                (
                    LAMBDA_NAME,
                    f"{LAMBDA_NAME} runs {app.lambda_runtime} on {app.lambda_arch}",
                    CONFIG_KIND,
                )
            )

    if not facts:
        skipped("facts", "app and data tier", "nothing was discovered to record")
        return

    pairs = [(parent, content) for parent, content, _ in facts]
    check_single_source(pairs)
    out = lam.derive(
        AGENT_APP_DATA,
        facts[0][1],
        facts[0][2],
        concepts=[f"{content}:{kind}" for _, content, kind in facts[1:]],
        parent_of=pairs,
    )
    for parent, content, _ in facts:
        derived("fact", content, f"parent_of {parent}")
    note(out)


# --------------------------------------------------------------------------
# Actions
# --------------------------------------------------------------------------


def record_actions(lam: Lambo, app: AppData, skip_rds: bool, skip_lambda: bool) -> None:
    """The cross-over binding (plan §3 Track 2 step 3).

    `--produces` is used only for concepts with no hierarchy parent, which here
    means the Lambda and the IAM roles. `RDS-Lambo-Demo-DB` is deliberately not
    produced: it already has a structural source in `SG-Base-VPC`, and a second
    one would take it out of that group's blast radius, which is exactly the
    number the next script reads to decide whether to abort.
    """
    steps: list[tuple[str, list[str], list[str]]] = []

    if not skip_rds:
        steps.append(
            (
                f"{AGENT_APP_DATA} provisioned {RDS_NAME} in {SUBNET_PRIVATE_NAME}",
                [],
                [VPC_NAME, SUBNET_PRIVATE_NAME, SG_BASE_NAME],
            )
        )
    if not skip_lambda:
        produced = [LAMBDA_NAME, STATS_ROLE_NAME]
        if app.exhibit_role:
            produced.append(EXHIBIT_ROLE_NAME)
        steps.append(
            (
                f"{AGENT_APP_DATA} deployed {LAMBDA_NAME} outside {VPC_NAME}",
                produced,
                # No VPC resource appears here, and that absence is the plan's
                # §2 argument written into the graph: the function reads
                # CockroachDB Cloud over the internet and has no reason to reach
                # anything in the private tier.
                [SECRET_NAME] if app.secret else [],
            )
        )
    if steps:
        # The plan names the cross-over binding as its own step, so it is its own
        # interaction. The edges repeat ones written above; `upsert_edge`
        # reinforces rather than duplicating, and the extra distinct origin
        # interaction is what Stage 2 of canonization counts.
        steps.append(
            (
                f"{AGENT_APP_DATA} bound the workloads into {VPC_NAME}",
                [],
                [VPC_NAME, SUBNET_PRIVATE_NAME, SG_BASE_NAME],
            )
        )

    for action, produces, depends_on in steps:
        out = lam.record_action(
            AGENT_APP_DATA, action, produces=produces, depends_on=depends_on
        )
        detail = []
        if produces:
            detail.append("produces " + ", ".join(produces))
        if depends_on:
            detail.append("depends-on " + ", ".join(depends_on))
        recorded("action", action, "; ".join(detail))
        note(out)


# --------------------------------------------------------------------------


def main(args: argparse.Namespace) -> int:
    if args.dry_run:
        return _plan(args)

    require_boto3()
    binary = resolve_lambo_binary(args.lambo_bin)
    aws = Aws(args.region, args.profile)
    ident = aws.whoami()
    step(f"region {args.region}, identity {ident['Arn']}")
    note(f"lambo binary: {binary}")

    step(f"discovering the app and data tier (read-only, tag:{PROJECT_TAG_KEY}={PROJECT})")
    app = discover(aws, args.skip_rds, args.skip_lambda)
    if not args.skip_rds:
        note(f"{RDS_INSTANCE_ID} endpoint {app.rds_endpoint or 'not assigned yet'}")
    if not args.skip_lambda:
        note(f"{LAMBDA_NAME} {app.lambda_runtime} on {app.lambda_arch}")
    if not app.exhibit_role:
        skipped("iam-role", EXHIBIT_ROLE_NAME, "the exhibit is not provisioned yet")

    lam = Lambo(binary, args.session, args.lambo_config)

    step(f"asking Lambo what {AGENT_NETWORK} recorded in session {args.session}")
    read_network_topology(lam)

    step(f"deriving the app and data tier as {AGENT_APP_DATA}")
    derive_topology(lam, app, args.skip_rds, args.skip_lambda)
    derive_facts(lam, app, args.skip_rds, args.skip_lambda)

    step("recording the cross-tier dependency edges")
    record_actions(lam, app, args.skip_rds, args.skip_lambda)

    say()
    step("app and data tier is in memory")
    note(lam.stats())
    say()
    note("nothing in AWS was created, modified or deleted")
    note(f"{RDS_NAME} is the tracked workload, not a Lambo store (plan §6)")
    note("next: 03_crossover_protect.py --session " + args.session)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    add_common_args(parser)
    add_lambo_args(parser)
    parser.add_argument(
        "--skip-rds",
        action="store_true",
        help=(
            "Record only the Lambda tier. Mirrors provision_app_data.py's flag, so "
            "a stack provisioned with --skip-rds can still be recorded."
        ),
    )
    parser.add_argument(
        "--skip-lambda",
        action="store_true",
        help="Record only the RDS tier. Mirrors provision_app_data.py's flag.",
    )
    return parser


if __name__ == "__main__":
    raise SystemExit(run_main(main, build_parser()))
