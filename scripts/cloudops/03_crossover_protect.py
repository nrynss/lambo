#!/usr/bin/env python3
"""Conflict and blast-radius protection (plan §3 "Climax"), Phase 4.

`network-infra-agent` runs a drift cleanup and decides that `SG-Base-VPC` is an
idle security group worth deleting. It is not idle: `RDS-Lambo-Demo-DB` sits in
it, and deleting it strands the database's network. That is the outage this
script exists to not have.

The sequence, in order:

1. Confirm from AWS, read-only, that the shared group really is shared: read
   `SG-Base-VPC`, and read which security groups `rds-lambo-demo-db` is
   attached to. If the account and the graph disagree, the graph is what is
   wrong, and the run says so.
2. Run the pre-flight recall protocol (plan §4.1) with the plan's own query.
3. Inspect `SG-Base-VPC` directly for its blast radius and dependents.
4. Decide, and render the outcome.

**The destructive AWS call is never issued.** Not "issued only when the guard
clears", not "issued behind a flag": there is no code path in this file that
calls a mutating AWS API at all. `describe_destructive_call` returns lines of
text describing the call, and the only thing ever done with them is print them.
That is deliberate. A demo whose safety depends on a conditional being written
correctly is a demo that eventually deletes a security group.

## Two signals, not one, and why

`recall` renders the spec §13 load-bearing-pillar warning **only** for a concept
the daemon has already promoted to `Canonical` (`src/recall/assemble.rs`).
Canonization is earned structurally over time: at least twenty non-canonical
peers in the session, three surviving GC sweeps, structural edges older than
sixty seconds, three distinct origin interactions, and a blast radius above
five. A short-lived CLI process does not survive long enough to run those
sweeps, so on a session that has only ever been written by scripts 01 and 02 the
warning is usually not there yet. It arrives once `lambo serve` has held the
session for a few minutes.

`inspect` has no such gate. It prints the blast radius and the dependents for
any focus, whatever its canonization status, which makes it the signal that is
always available. So the guard reads both, and blocks on either. The recall line
is the one the plan puts on screen; the inspect line is the one that works from
the first run.

This script uses read verbs only, so it takes no writer lease and is safe to run
beside a live `lambo serve` and beside the judge portal.

Usage:

    python3 scripts/cloudops/03_crossover_protect.py --session <lambo-session-id>
    python3 scripts/cloudops/03_crossover_protect.py --session <id> --dry-run
    python3 scripts/cloudops/03_crossover_protect.py --session <id> --action revoke-ingress

Exit status is 0 when the guard blocked the action, which is the outcome the
demo wants, and 1 when it found nothing to protect.
"""

from __future__ import annotations

import argparse
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from _lambo import (  # noqa: E402
    AGENT_NETWORK,
    PROJECT,
    PROJECT_TAG_KEY,
    RDS_INSTANCE_ID,
    RDS_NAME,
    SG_BASE_NAME,
    Aws,
    ClientError,
    Lambo,
    add_common_args,
    add_lambo_args,
    blocked,
    carries_pillar_warning,
    indent,
    note,
    parse_blast_radius,
    parse_outbound_neighbours,
    queried,
    require_boto3,
    require_sg,
    resolve_lambo_binary,
    run_main,
    say,
    step,
    warn,
    would,
)

# Plan §3's climax query, verbatim. It names both the subnet and the shared
# group, which is the point: a drift routine phrases its intent in prose, and
# the memory has to recognise the pillars inside it.
DEFAULT_QUERY = "tear down Subnet-Private-1a and delete SG-Base-VPC"

ACTIONS = ("delete-security-group", "revoke-ingress")


def _plan(args: argparse.Namespace) -> int:
    step(f"PLAN for region {args.region} (no AWS calls, and no lambo calls, are made)")
    say()
    note(f"acting as: {AGENT_NETWORK}")
    note(f"session: {args.session}")
    note(f"attempted action: {args.action} on {SG_BASE_NAME}")
    say()
    step("confirm the shared resource, read-only")
    would("security-group", SG_BASE_NAME, f"tag:{PROJECT_TAG_KEY}={PROJECT}")
    would("rds-instance", RDS_INSTANCE_ID, "which security groups it is attached to")
    say()
    step("pre-flight recall protocol (plan §4.1)")
    would("recall", args.query, "abort if a load-bearing pillar warning comes back")
    would("inspect", SG_BASE_NAME, "depth 1, abort if anything hangs off it")
    say()
    step("the call that will not be made")
    for line in describe_destructive_call(args.action, "<sg-id>", "<vpc-id>"):
        would("aws", line)
    say()
    note("this script issues no mutating AWS call on any path, guarded or not")
    note("it uses lambo read verbs only, so it takes no writer lease")
    return 0


# --------------------------------------------------------------------------
# The destructive action, described but never issued
# --------------------------------------------------------------------------


def describe_destructive_call(action: str, sg_id: str, vpc_id: str) -> list[str]:
    """The AWS call `network-infra-agent`'s drift cleanup would have made.

    Returned as text, not as a callable and not as `(client, method, kwargs)`.
    A tuple like that invites a `getattr(client, method)(**kwargs)` somewhere
    downstream, and then the only thing standing between a demo and a deleted
    security group is a boolean. There is nothing here to invoke.
    """
    if action == "delete-security-group":
        return [
            f"ec2:DeleteSecurityGroup GroupId={sg_id}",
            f"  the group is the internal mesh of {vpc_id}",
        ]
    return [
        f"ec2:RevokeSecurityGroupIngress GroupId={sg_id} tcp/5432",
        "  removes the only path from the public tier to the database",
    ]


# --------------------------------------------------------------------------
# Step 1: confirm the resource really is shared (read-only)
# --------------------------------------------------------------------------


class Target:
    def __init__(self) -> None:
        self.sg_id: str = ""
        self.vpc_id: str = ""
        self.rds_attached: bool = False
        self.rds_status: str = ""


def confirm_shared(aws: Aws) -> Target:
    target = Target()
    sg = require_sg(aws, SG_BASE_NAME)
    target.sg_id = sg["GroupId"]
    target.vpc_id = sg["VpcId"]
    queried("security-group", target.sg_id, SG_BASE_NAME)

    try:
        db = aws.rds.describe_db_instances(DBInstanceIdentifier=RDS_INSTANCE_ID)["DBInstances"][0]
    except ClientError as exc:
        if exc.response["Error"]["Code"] != "DBInstanceNotFound":
            raise
        # Not fatal. The graph can still carry the dependency from an earlier
        # run, and the guard should still block on it. But say so plainly:
        # without the workload, the climax is a rehearsal.
        warn(
            f"{RDS_INSTANCE_ID} does not exist in this account, so nothing in AWS "
            f"currently depends on {SG_BASE_NAME}"
        )
        return target

    target.rds_status = db.get("DBInstanceStatus", "unknown")
    target.rds_attached = any(
        g.get("VpcSecurityGroupId") == target.sg_id for g in db.get("VpcSecurityGroups") or []
    )
    queried("rds-instance", RDS_INSTANCE_ID, target.rds_status)
    if target.rds_attached:
        note(f"{RDS_NAME} is attached to {SG_BASE_NAME} in the account, not only in the graph")
    else:
        warn(f"{RDS_NAME} is not attached to {SG_BASE_NAME}; the account and the graph disagree")
    return target


# --------------------------------------------------------------------------
# Step 2 and 3: ask Lambo
# --------------------------------------------------------------------------


class Verdict:
    """What the guard concluded, and on what evidence."""

    def __init__(self) -> None:
        self.pillar_warning: bool = False
        self.blast_radius: int = 0
        self.dependents: list[str] = []
        self.recall_text: str = ""
        self.inspect_text: str = ""

    @property
    def blocked(self) -> bool:
        return self.pillar_warning or self.blast_radius > 0 or bool(self.dependents)

    def reasons(self) -> list[str]:
        out: list[str] = []
        if self.pillar_warning:
            out.append("recall returned a load-bearing pillar warning")
        if self.blast_radius > 0:
            out.append(f"{SG_BASE_NAME} has a blast radius of {self.blast_radius}")
        if self.dependents:
            out.append(f"{len(self.dependents)} concept(s) hang off it and would be stranded")
        return out


def run_guard(lam: Lambo, query: str, top_k: int) -> Verdict:
    verdict = Verdict()

    step("pre-flight recall protocol (plan §4.1)")
    note(f'query: "{query}"')
    verdict.recall_text = lam.recall(query, top_k=top_k)
    say()
    say(indent(verdict.recall_text))
    say()
    verdict.pillar_warning = carries_pillar_warning(verdict.recall_text)
    if verdict.pillar_warning:
        queried("recall", "pillar warning", "present")
    else:
        # Not a failure of the guard, and not silence either. Say which of the
        # two signals is missing so the operator knows whether to wait for
        # canonization or to go and run 01 and 02.
        queried("recall", "pillar warning", "absent; the pillar is not Canonical yet")

    step(f"inspecting {SG_BASE_NAME} directly")
    # Depth 1, not 2. Blast radius is a one-hop measure with no recursion
    # (`src/recall/format.rs`), so hop 2 adds nothing the verdict reads and a
    # great deal the operator has to scroll past, including every interaction
    # node in the session. The outcome has to be legible to be worth rendering.
    verdict.inspect_text = lam.inspect(SG_BASE_NAME, depth=1)
    say()
    say(indent(verdict.inspect_text))
    say()
    verdict.blast_radius = parse_blast_radius(verdict.inspect_text)
    verdict.dependents = parse_outbound_neighbours(verdict.inspect_text)
    queried("inspect", SG_BASE_NAME, f"blast radius {verdict.blast_radius}")
    return verdict


# --------------------------------------------------------------------------
# Step 4: render the outcome
# --------------------------------------------------------------------------


def render_blocked(verdict: Verdict, target: Target, action: str) -> None:
    say()
    step("ABORTED. The destructive action was not issued.")
    say()
    blocked("aws-call", action, f"on {SG_BASE_NAME} ({target.sg_id or 'unknown id'})")
    sg_id = target.sg_id or "<sg-id>"
    vpc_id = target.vpc_id or "<vpc-id>"
    for line in describe_destructive_call(action, sg_id, vpc_id):
        note(line)
    say()
    step("why")
    for reason in verdict.reasons():
        note(reason)
    if verdict.dependents:
        say()
        step("what would have been stranded")
        for name in verdict.dependents:
            note(name)
    say()
    note(f"{AGENT_NETWORK} halted, as the lambo-cloudops agent skill requires (plan §4.1)")
    note("no AWS resource was created, modified or deleted")


def render_unprotected(verdict: Verdict, target: Target, action: str) -> None:
    say()
    step("NOT BLOCKED. The destructive action was still not issued.")
    say()
    warn(
        f"Lambo reports no dependents for {SG_BASE_NAME}: no pillar warning, and a "
        f"blast radius of {verdict.blast_radius}."
    )
    say()
    step("what that most likely means")
    note("01_network_agent.py and 02_app_data_agent.py have not run against this session")
    note("or they ran against a different session id than the one passed here")
    if target.rds_attached:
        note(
            f"the account says {RDS_NAME} is attached to {SG_BASE_NAME}, so the "
            "graph is behind the account rather than the other way round"
        )
    say()
    note("this script refuses the action regardless of the verdict; nothing was deleted")


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
    note(f"acting as {AGENT_NETWORK}, attempting {args.action} on {SG_BASE_NAME}")

    step(f"confirming the shared resource (read-only, tag:{PROJECT_TAG_KEY}={PROJECT})")
    target = confirm_shared(aws)

    lam = Lambo(binary, args.session, args.lambo_config)
    verdict = run_guard(lam, args.query, args.top_k)

    if verdict.blocked:
        render_blocked(verdict, target, args.action)
        return 0
    render_unprotected(verdict, target, args.action)
    return 1


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    add_common_args(parser)
    add_lambo_args(parser)
    parser.add_argument(
        "--action",
        choices=ACTIONS,
        default=ACTIONS[0],
        help=(
            "Which destructive action the drift cleanup is attempting. Only ever "
            "described, never issued: delete-security-group removes the shared "
            "mesh outright, revoke-ingress removes its 5432 rule. Default: "
            f"{ACTIONS[0]}."
        ),
    )
    parser.add_argument(
        "--query",
        default=DEFAULT_QUERY,
        help=f"The recall query the pre-flight protocol runs. Default: {DEFAULT_QUERY!r}",
    )
    parser.add_argument(
        "--top-k",
        type=int,
        default=8,
        help=(
            "Hits the recall returns. Wider than lambo's own default so a pillar "
            "that ranks below the obvious lexical matches still shows up. Default: 8."
        ),
    )
    return parser


if __name__ == "__main__":
    raise SystemExit(run_main(main, build_parser()))
