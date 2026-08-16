#!/usr/bin/env python3
"""The `network-infra-agent` track (plan §3 Track 1), Phase 4.

Discovers the network tier that `scripts/aws-infra/provision_network.py` created
and feeds it into Lambo as graph concepts, so the foundation the second agent
builds on is memory rather than assumption.

Discovered, all by tag (`Project=lambo-cloudops` plus a `Name`), and all
read-only:

    VPC-Enterprise-Prod        and its CIDR
      Subnet-Public-1a
      Subnet-Private-1a
      Subnet-Private-1b
      InternetGateway
      RouteTable-Public
      SG-Base-VPC              and its ingress / egress rules
      SG-PublicWeb             and its ingress / egress rules
    lambo/cockroach-dsn        (Secrets Manager, existence only, never the value)
    EC2-LamboWebExhibit        (only if it has been launched yet)

This script **creates and modifies nothing in AWS**. Provisioning is Phase 3's
job; if a resource is missing, the error says which script creates it.

## What the graph ends up looking like, and why it is shaped that way

Containment is expressed once, as a `Hierarchical` edge from parent to child
(`lambo derive --parent-of`). Everything else is a `record-action` edge pointing
out of the action node.

That split is not cosmetic. Blast radius counts, for a node, the concepts whose
**only** inbound structural edge comes from it. Give a subnet both a hierarchy
parent and a `--produces` edge from the action that created it and the subnet
stops counting toward either, silently. Do that across the tier and
`VPC-Enterprise-Prod` never clears Stage 3, never becomes Canonical, and the
load-bearing-pillar warning the whole demo turns on simply never renders, with
nothing anywhere reporting an error. `check_single_source` refuses the hierarchy
half of that mistake; the actions below avoid the other half by naming the tier
root in `--depends-on` rather than re-producing resources the derive step has
already placed.

Usage:

    python3 scripts/cloudops/01_network_agent.py --session <lambo-session-id>
    python3 scripts/cloudops/01_network_agent.py --session <id> --dry-run

Prerequisites: `provision_network.py` has run, and no other process holds this
session's writer lease. `lambo derive` and `lambo record-action` each take that
lease for the length of one invocation, so stop `lambo serve` on this session
first. Re-running is safe: contents canonicalize to the same concepts and edges
reinforce rather than duplicate.
"""

from __future__ import annotations

import argparse
import pathlib
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parent))

from _lambo import (  # noqa: E402
    AGENT_NETWORK,
    EC2_NAME,
    IDENTITY_KIND,
    IGW_NAME,
    PROJECT,
    PROJECT_TAG_KEY,
    ROUTE_TABLE_PUBLIC_NAME,
    SECRET_NAME,
    SG_BASE_NAME,
    SG_PUBLIC_WEB_NAME,
    SUBNET_PRIVATE_B_NAME,
    SUBNET_PRIVATE_NAME,
    SUBNET_PUBLIC_NAME,
    VPC_NAME,
    Aws,
    InfraError,
    Lambo,
    account_binding,
    add_common_args,
    add_lambo_args,
    check_single_source,
    derived,
    find_instance,
    find_secret,
    note,
    one_or_none,
    project_filters,
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

# The VPC's direct children, in the order the plan's §2 diagram reads. Order is
# fixed rather than derived from a dict walk so two runs produce the same
# command lines and the same plan output.
VPC_CHILDREN = (
    SUBNET_PUBLIC_NAME,
    SUBNET_PRIVATE_NAME,
    SUBNET_PRIVATE_B_NAME,
    IGW_NAME,
    ROUTE_TABLE_PUBLIC_NAME,
    SG_BASE_NAME,
    SG_PUBLIC_WEB_NAME,
)


def _plan(args: argparse.Namespace) -> int:
    step(f"PLAN for region {args.region} (no AWS calls, and no lambo calls, are made)")
    say()
    note(f"agent identity: {AGENT_NETWORK}")
    note(f"session: {args.session}")
    say()
    step("discover, read-only, by tag")
    for kind, name in (
        ("vpc", VPC_NAME),
        ("subnet", SUBNET_PUBLIC_NAME),
        ("subnet", SUBNET_PRIVATE_NAME),
        ("subnet", SUBNET_PRIVATE_B_NAME),
        ("internet-gateway", IGW_NAME),
        ("route-table", ROUTE_TABLE_PUBLIC_NAME),
        ("security-group", SG_BASE_NAME),
        ("security-group", SG_PUBLIC_WEB_NAME),
    ):
        would(kind, name, f"tag:{PROJECT_TAG_KEY}={PROJECT}")
    would("nat-gateway", "none expected", "plan §2 forbids one; a NAT here is a finding")
    would("secret", SECRET_NAME, "existence only; the value is never read")
    would("ec2-instance", EC2_NAME, "optional; skipped if the exhibit is not launched")

    say()
    step("derive into Lambo")
    would("concept", VPC_NAME, "entity, the tier root")
    for child in VPC_CHILDREN:
        would("concept", child, f"entity, parent_of {VPC_NAME}")
    would("concept", f"{VPC_NAME} CIDR <discovered>", f"constraint, parent_of {VPC_NAME}")
    would("concept", f"{VPC_NAME} has no NAT gateway", f"constraint, parent_of {VPC_NAME}")
    would("concept", "<name> = <aws-id>", "entity per resource, parent_of that resource")
    would("concept", "<sg> ingress|egress <port> from|to <source>", "constraint, parent_of that sg")
    would("concept", SECRET_NAME, "resource, no parent; it is not inside the VPC")

    say()
    step("record actions into Lambo")
    would("action", f"{AGENT_NETWORK} provisioned {VPC_NAME}", f"produces {VPC_NAME}")
    would("action", f"{AGENT_NETWORK} laid out the subnets", f"depends-on {VPC_NAME}")
    would("action", f"{AGENT_NETWORK} attached the internet path", f"depends-on {VPC_NAME}")
    would("action", f"{AGENT_NETWORK} provisioned the security groups", f"depends-on {VPC_NAME}")
    would("action", f"{AGENT_NETWORK} provisioned {SECRET_NAME}", f"produces {SECRET_NAME}")
    would("action", f"{AGENT_NETWORK} launched {EC2_NAME}", "only if the exhibit exists")

    say()
    note("nothing in AWS is created, modified or deleted by this script")
    note(
        "the actions name the tier root in --depends-on rather than re-producing "
        "the resources the derive step placed; see the module docstring"
    )
    return 0


# --------------------------------------------------------------------------
# Discovery (read-only)
# --------------------------------------------------------------------------


class Network:
    """Everything the network tier says about itself, read once.

    Held as one object so the write phase below cannot accidentally issue a
    second describe call halfway through and act on a tier that changed under it.
    """

    def __init__(self) -> None:
        self.ids: dict[str, str] = {}
        self.vpc_cidr: str = ""
        self.rules: list[tuple[str, str]] = []
        self.nat_gateways: list[str] = []
        self.secret: bool = False
        self.instance_id: str | None = None


def _find_igw(aws: Aws) -> dict:
    resp = aws.ec2.describe_internet_gateways(Filters=project_filters(IGW_NAME))
    igw = one_or_none(resp["InternetGateways"], "internet gateway", IGW_NAME)
    if igw is None:
        raise InfraError(
            f"no internet gateway tagged Name={IGW_NAME} exists in {aws.region}.",
            hint="run `python3 scripts/aws-infra/provision_network.py --ssh-cidr <yours>` first.",
        )
    return igw


def _find_route_table(aws: Aws) -> dict:
    resp = aws.ec2.describe_route_tables(Filters=project_filters(ROUTE_TABLE_PUBLIC_NAME))
    rt = one_or_none(resp["RouteTables"], "route table", ROUTE_TABLE_PUBLIC_NAME)
    if rt is None:
        raise InfraError(
            f"no route table tagged Name={ROUTE_TABLE_PUBLIC_NAME} exists in {aws.region}.",
            hint="run `python3 scripts/aws-infra/provision_network.py --ssh-cidr <yours>` first.",
        )
    return rt


def _port_label(perm: dict) -> str:
    proto = perm.get("IpProtocol", "-1")
    if proto == "-1":
        return "all protocols"
    lo, hi = perm.get("FromPort"), perm.get("ToPort")
    if lo is None or hi is None:
        return str(proto)
    return f"{proto}/{lo}" if lo == hi else f"{proto}/{lo}-{hi}"

def _peer_label(peer: str) -> str:
    """Generalise a single-host CIDR to a role.

    A /32 or /128 in these groups is the operator's own address: plan §8
    requires SSH ingress be restricted to it rather than 0.0.0.0/0. These
    concepts are rendered on the public judge portal by `lambo serve-web`,
    so the literal would publish a home IP address to every visitor. What
    matters for blast radius is that the rule is scoped to one host, not
    which host, so the label keeps the property and drops the value.

    `scripts/aws-infra/README.md` makes the same argument for the account
    id. This is that rule applied to the graph rather than to the captures.

    Module scope (not a closure in `_rule_texts`) so it is allocated once,
    not re-created for every security group iterated.
    """
    if peer.endswith("/32") or peer.endswith("/128"):
        return "the operator address"
    return peer


def _rule_texts(sg_name: str, group: dict, name_by_id: dict[str, str]) -> list[str]:
    """Render one security group's rules as concept contents.

    One concept per (permission, source) pair rather than per permission, so a
    rule that is widened later shows up as a new concept beside the old one
    instead of quietly changing the meaning of an existing node.
    """

    out: list[str] = []
    for direction, key, preposition in (
        ("ingress", "IpPermissions", "from"),
        ("egress", "IpPermissionsEgress", "to"),
    ):
        for perm in group.get(key) or []:
            port = _port_label(perm)
            peers: list[str] = [r["CidrIp"] for r in perm.get("IpRanges") or []]
            peers += [r["CidrIpv6"] for r in perm.get("Ipv6Ranges") or []]
            peers += [p["PrefixListId"] for p in perm.get("PrefixListIds") or []]
            peers += [
                name_by_id.get(p.get("GroupId", ""), p.get("GroupId", ""))
                for p in perm.get("UserIdGroupPairs") or []
            ]
            for peer in sorted(peers):
                out.append(
                    f"{sg_name} {direction} {port} {preposition} {_peer_label(peer)}"
                )
    # Sorted so a re-run emits the same command lines in the same order, which
    # is what makes the run diffable against the last one.
    return sorted(set(out))


def discover(aws: Aws) -> Network:
    net = Network()
    vpc = require_vpc(aws)
    net.ids[VPC_NAME] = vpc["VpcId"]
    net.vpc_cidr = vpc["CidrBlock"]

    for name in (SUBNET_PUBLIC_NAME, SUBNET_PRIVATE_NAME, SUBNET_PRIVATE_B_NAME):
        net.ids[name] = require_subnet(aws, name)["SubnetId"]
    net.ids[IGW_NAME] = _find_igw(aws)["InternetGatewayId"]
    net.ids[ROUTE_TABLE_PUBLIC_NAME] = _find_route_table(aws)["RouteTableId"]

    groups = {name: require_sg(aws, name) for name in (SG_BASE_NAME, SG_PUBLIC_WEB_NAME)}
    for name, group in groups.items():
        net.ids[name] = group["GroupId"]
    # The two groups reference each other, so rules render with the peer's plan
    # name rather than a raw sg-id wherever the peer is one of ours. A rule that
    # names `SG-PublicWeb` is a graph edge a reader can follow; one that names
    # `sg-0a1b2c3d` is not.
    name_by_id = {group["GroupId"]: name for name, group in groups.items()}
    for name, group in groups.items():
        net.rules.extend((name, text) for text in _rule_texts(name, group, name_by_id))

    # Plan §2 states it twice, in bold: there is deliberately no NAT gateway
    # anywhere in this design. That is a claim about the account, so it is worth
    # checking rather than asserting, and worth putting in memory either way. A
    # NAT that has appeared is both a cost surprise and a sign something was
    # placed in the wrong tier.
    nats = aws.ec2.describe_nat_gateways(
        Filters=[
            {"Name": "vpc-id", "Values": [net.ids[VPC_NAME]]},
            {"Name": "state", "Values": ["pending", "available"]},
        ]
    )
    net.nat_gateways = sorted(n["NatGatewayId"] for n in nats.get("NatGateways") or [])

    net.secret = find_secret(aws) is not None
    instance = find_instance(aws)
    net.instance_id = instance["InstanceId"] if instance else None
    return net


# --------------------------------------------------------------------------
# Derivation
# --------------------------------------------------------------------------


def derive_topology(lam: Lambo, net: Network) -> None:
    """The tier root and its children, in one interaction.

    One `derive` call rather than eight, because the concepts of a single call
    also get pairwise `CoOccurrence` edges. Those are not structural and do not
    touch blast radius, but they are what makes a recall for one subnet surface
    its siblings, which is the association a human operator would expect.
    """
    pairs = [(VPC_NAME, child) for child in VPC_CHILDREN]
    check_single_source(pairs)
    out = lam.derive(
        AGENT_NETWORK,
        VPC_NAME,
        "entity",
        concepts=[f"{child}:entity" for child in VPC_CHILDREN],
        parent_of=pairs,
    )
    derived("topology", VPC_NAME, f"+{len(VPC_CHILDREN)} children")
    note(out)


def derive_account_bindings(lam: Lambo, net: Network) -> None:
    """`<name> = <aws-id>` as a child concept under each resource.

    Two jobs. It records which account object each graph node actually is, so a
    reviewer can check the graph rather than trust it. And each binding is a
    child that nothing else ever points at, which is what gives its parent a
    blast radius that survives the cross-tier dependency edges the app-data
    agent adds in the next script.

    The secret is deliberately absent: its identifier is an ARN, and an ARN's
    colons cannot appear on the CHILD end of `--parent-of` (the first colon is
    the separator).
    """
    names = [VPC_NAME, *VPC_CHILDREN]
    bindings = [(name, account_binding(name, net.ids[name])) for name in names]
    pairs = [(name, binding) for name, binding in bindings]
    check_single_source(pairs)
    first = bindings[0][1]
    out = lam.derive(
        AGENT_NETWORK,
        first,
        IDENTITY_KIND,
        concepts=[f"{binding}:{IDENTITY_KIND}" for _, binding in bindings[1:]],
        parent_of=pairs,
    )
    derived("account bindings", f"{len(bindings)} resources", "graph names mapped to live ids")
    note(out)


def derive_vpc_invariants(lam: Lambo, net: Network) -> None:
    """The two facts about the VPC itself that are worth remembering.

    Its CIDR, and whether it has a NAT gateway. The second is the plan's own
    hard rule (§2) turned into something an agent can recall, so that the next
    agent to reach for a NAT reads why the design does not have one before it
    provisions one.
    """
    cidr = f"{VPC_NAME} CIDR {net.vpc_cidr}"
    if net.nat_gateways:
        warn(
            f"{VPC_NAME} has {len(net.nat_gateways)} NAT gateway(s): "
            f"{', '.join(net.nat_gateways)}. Plan §2 says there should be none."
        )
        nat = f"{VPC_NAME} has {len(net.nat_gateways)} NAT gateway which plan section 2 forbids"
    else:
        nat = f"{VPC_NAME} has no NAT gateway, by design"

    pairs = [(VPC_NAME, cidr), (VPC_NAME, nat)]
    check_single_source(pairs)
    out = lam.derive(
        AGENT_NETWORK,
        cidr,
        "constraint",
        concepts=[f"{nat}:constraint"],
        parent_of=pairs,
    )
    derived("constraint", cidr)
    derived("constraint", nat)
    note(out)


def derive_security_rules(lam: Lambo, net: Network) -> None:
    """Every discovered rule as a constraint under its security group.

    This is what gives `SG-Base-VPC` a blast radius of its own rather than only
    an inbound degree. The rules are the part of the group that would be lost
    with it, so counting them as dependents is the honest reading, not a padded
    one.
    """
    usable: list[tuple[str, str]] = []
    for sg_name, text in net.rules:
        if ":" in text:
            # The rule text becomes the CHILD end of `--parent-of`; an IPv6
            # CIDR renders with colons, so the child end cannot carry it.
            # Skipping one rule is better than mangling it into a concept that
            # claims something slightly different from the account.
            skipped("constraint", text, "contains a colon, so it cannot be a hierarchy end")
            continue
        usable.append((sg_name, text))
    if not usable:
        warn("no security group rules were discovered; SG-Base-VPC will have no dependents")
        return

    pairs = [(sg_name, text) for sg_name, text in usable]
    check_single_source(pairs)
    out = lam.derive(
        AGENT_NETWORK,
        usable[0][1],
        "constraint",
        concepts=[f"{text}:constraint" for _, text in usable[1:]],
        parent_of=pairs,
    )
    for sg_name, text in usable:
        derived("constraint", text, f"parent_of {sg_name}")
    note(out)


def derive_secret(lam: Lambo, net: Network) -> None:
    if not net.secret:
        skipped("resource", SECRET_NAME, "not provisioned yet")
        return
    out = lam.derive(AGENT_NETWORK, SECRET_NAME, "resource")
    derived("resource", SECRET_NAME, "existence only; the value is never read")
    note(out)


def derive_exhibit(lam: Lambo, net: Network) -> None:
    """The exhibit host, if it has been launched (plan §3 Track 1 step 3).

    Optional because Phase 3's `launch_exhibit_ec2.py` runs last and the two
    agent tracks are worth exercising before it does. Its containment parent is
    the public subnet; its dependency on `SG-PublicWeb` and the secret is
    recorded as an action below, which is the plan's own phrasing.
    """
    if net.instance_id is None:
        skipped("entity", EC2_NAME, "the exhibit instance is not running")
        return
    binding = account_binding(EC2_NAME, net.instance_id)
    pairs = [(SUBNET_PUBLIC_NAME, EC2_NAME), (EC2_NAME, binding)]
    check_single_source(pairs)
    out = lam.derive(
        AGENT_NETWORK,
        EC2_NAME,
        "entity",
        concepts=[f"{binding}:{IDENTITY_KIND}"],
        parent_of=pairs,
    )
    derived("entity", EC2_NAME, f"parent_of {SUBNET_PUBLIC_NAME}")
    note(out)


# --------------------------------------------------------------------------
# Actions
# --------------------------------------------------------------------------


def record_actions(lam: Lambo, net: Network) -> None:
    """What the agent did, as `record-action` edges.

    Read the `--produces` lists carefully before changing them. `--produces` and
    `--modifies` write a `Causal` edge from the action into the named concept,
    which makes the action a second structural source of that concept. For a
    resource the derive step already placed under a hierarchy parent, that
    removes it from the parent's blast radius. So the only things produced here
    are the two concepts with no hierarchy parent: the tier root, and the
    secret, which sits in Secrets Manager and not in the VPC. Everything else is
    recorded as a dependency on the root, which points the other way and costs
    nothing.

    Each call is its own interaction, and that matters beyond tidiness: Stage 2
    of canonization wants at least three *distinct origin interactions* among a
    node's inbound structural sources before it will promote anything.
    """
    steps: list[tuple[str, list[str], list[str]]] = [
        (
            f"{AGENT_NETWORK} provisioned {VPC_NAME}",
            [VPC_NAME],
            [],
        ),
        (
            f"{AGENT_NETWORK} laid out the subnets of {VPC_NAME}",
            [],
            [VPC_NAME],
        ),
        (
            f"{AGENT_NETWORK} attached {IGW_NAME} and {ROUTE_TABLE_PUBLIC_NAME}",
            [],
            [VPC_NAME],
        ),
        (
            f"{AGENT_NETWORK} provisioned {SG_BASE_NAME} and {SG_PUBLIC_WEB_NAME}",
            [],
            [VPC_NAME],
        ),
    ]
    if net.secret:
        steps.append(
            (
                f"{AGENT_NETWORK} provisioned {SECRET_NAME}",
                [SECRET_NAME],
                [],
            )
        )
    if net.instance_id is not None:
        # Plan §3 Track 1 step 3, verbatim on the dependency list. The secret is
        # added because the instance profile resolves it at service start, which
        # is a real dependency the plan describes in §2 even though its step 3
        # list predates it.
        steps.append(
            (
                f"{AGENT_NETWORK} launched {EC2_NAME}",
                [],
                [VPC_NAME, SUBNET_PUBLIC_NAME, SG_PUBLIC_WEB_NAME, SECRET_NAME],
            )
        )

    for action, produces, depends_on in steps:
        out = lam.record_action(
            AGENT_NETWORK, action, produces=produces, depends_on=depends_on
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

    step(f"discovering the network tier (read-only, tag:{PROJECT_TAG_KEY}={PROJECT})")
    net = discover(aws)
    for name in (VPC_NAME, *VPC_CHILDREN):
        note(f"{name} = {net.ids[name]}")
    note(f"{len(net.rules)} security group rule(s) discovered")
    if not net.secret:
        warn(f"{SECRET_NAME} does not exist yet; it will not be derived")

    lam = Lambo(binary, args.session, args.lambo_config)

    step(f"deriving the network tier into session {args.session} as {AGENT_NETWORK}")
    derive_topology(lam, net)
    derive_account_bindings(lam, net)
    derive_vpc_invariants(lam, net)
    derive_security_rules(lam, net)
    derive_secret(lam, net)
    derive_exhibit(lam, net)

    step("recording what the agent did")
    record_actions(lam, net)

    say()
    step("network tier is in memory")
    note(lam.stats())
    say()
    note("nothing in AWS was created, modified or deleted")
    note("next: 02_app_data_agent.py --session " + args.session)
    return 0


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    add_common_args(parser)
    add_lambo_args(parser)
    return parser


if __name__ == "__main__":
    raise SystemExit(run_main(main, build_parser()))
