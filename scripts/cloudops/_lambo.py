"""Shared scaffolding for the `lambo-cloudops` agent scripts (plan §3, Phase 4).

Read `docs/plans/multi-agent-cloudops-aws-plan.md` (revision 2) before changing
anything here. The plan is the specification; this module encodes only the parts
of it the three agent scripts have to agree on.

This is the Phase 4 counterpart to `scripts/aws-infra/_common.py`, and it
deliberately duplicates none of it. Tags, tag-based lookup, client construction,
`InfraError`, the output helpers and the argument plumbing are all imported from
there and re-exported below, so the two directories cannot drift on a resource
name or on what `--dry-run` promises. What Phase 4 adds, and all this module
really owns, is driving the `lambo` binary as a subprocess.

Four rules drive the design. The first three carry over from the sibling
directory; the fourth is new here.

1. **Tags are the only inventory.** Discovery is `Project=lambo-cloudops` plus a
   `Name`, exactly as provisioning wrote it. There is no state file and no id
   list, so a graph built by these scripts always describes resources that are
   really in the account.
2. **`--dry-run` never touches AWS.** Not "makes only read calls" but makes *no*
   calls and constructs no client. Phase 4 extends the promise: `--dry-run` also
   runs no `lambo` subprocess, so the plan output works on a machine with
   neither credentials nor a built binary.
3. **Fail with a sentence, not a traceback.** Anything the operator can fix is an
   `InfraError` with a hint.
4. **The graph is only worth anything if it matches the account.** Concept
   contents are the plan's resource names verbatim, and every resource also gets
   its live AWS id derived as a child concept, so a reader can check the graph
   against `describe-*` output rather than trusting it.

Dependencies: stdlib + boto3, and the `lambo` binary at run time. Nothing else.
"""

from __future__ import annotations

import argparse
import os
import pathlib
import re
import shutil
import subprocess
import sys

sys.path.insert(0, str(pathlib.Path(__file__).resolve().parents[1] / "aws-infra"))

from _common import (  # noqa: E402,F401  (re-exported for the agent scripts)
    EC2_NAME,
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
    find_instance,
    find_secret,
    note,
    one_or_none,
    project_filters,
    require_boto3,
    require_sg,
    require_subnet,
    require_vpc,
    run_main,
    say,
    skipped,
    step,
    warn,
    would,
)

REPO_ROOT = pathlib.Path(__file__).resolve().parents[2]

# The two agent identities from plan §3. They are stamped on every interaction
# and concept these scripts write, and they are what makes the conflict line
# ("Agent App-data-agent wrote to it <n> seconds ago") name a real writer.
AGENT_NETWORK = "network-infra-agent"
AGENT_APP_DATA = "app-data-agent"

# The prefix of the spec §13 warning line, matched rather than reproduced. The
# full sentence carries an em dash, which is the one place in this repo where one
# is allowed (it is quoted screen text, pinned byte-for-byte by
# `src/recall/format.rs`). Matching on the prefix keeps the em dash out of this
# file while still recognising the line wherever it is rendered.
PILLAR_WARNING_PREFIX = "⚑ Load-bearing pillar"

# Edge kinds `lambo inspect` renders that mean "this would be stranded if the
# focus went away": an outbound Causal, Dependency or Hierarchical edge. Only
# these may name a dependent. `CoOccurrence` pairs two concepts merely because
# one interaction wrote both (it decays, spec §5) and `Semantic` because they
# are close in embedding space (it decays too) — neither means "one depends on
# the other" — and `Derives`/`Temporal` are interaction provenance. See
# `parse_outbound_neighbours` for why only the structural kinds survive.
#
# This tuple is the Python mirror of the CLI's structural edge set — the Rust
# `is_structural` at `src/cli/serve_web.rs` (used by the inspect page) and
# `DRIFT_EDGE_TYPES` at `src/daemon/drift.rs` both treat exactly
# `{Dependency, Causal, Hierarchical}` as structural. The other four rendered
# kinds (`CoOccurrence`, `Semantic`, `Derives`, `Temporal`) are correctly
# non-structural here. If the Rust side ever adds or renames a structural edge
# type, update this tuple AND the `_self_test_structural_whitelist` synthetic
# banner so the drift fails loudly rather than silently re-widening the
# blast-radius report.
STRUCTURAL_EDGE_LABELS = ("Causal", "Dependency", "Hierarchical")

# `lambo inspect` emits this exact phrase when the focus concept is not in the
# session at all — the `Focus::Missing` arm at `src/cli/inspect.rs` produces
# `no concept matching '{focus}' in session '{session}'` and exits 1 — which is
# the one condition `03_crossover_protect.py` treats as "nothing to protect".
# The sentinel lives here (beside the reader-verb contract) rather than inline
# in 03 so re-using it is a named constant, and `_self_test_empty_session_sentinel`
# pins it to the real error string: a future reword of the CLI fails loudly
# instead of silently reverting the empty-session guard to a hard abort.
EMPTY_SESSION_ERR = "no concept matching"

# `inspect` renders a neighbour as `content [Type{, status}]`, where the
# bracketed tail is render metadata (ConceptType plus optional canonization
# status; see `src/cli/inspect.rs` `render_neighbourhood`/`label`). The content
# itself may legitimately contain a ` [`, so the tail is stripped with this
# anchored pattern rather than a blind `rsplit(" [", 1)` — which would truncate
# bracketed concept text. `Parse outbound neighbour` rows are always concept
# rows (provenance kinds are filtered out), so only ConceptType tags appear.
NEIGHBOUR_METADATA_RE = re.compile(
    r"\s*\[(?:Entity|Logic|Constraint|Resource|Observation)"
    r"(?:,\s*(?:canonical|venerable|candidate))?\]\s*$"
)

# `observation` is never used as a concept kind by these scripts, and that is
# not a stylistic choice. `canonicalize` (`src/graph/canonical.rs`) excludes
# `ConceptType::Observation` from key matching, so an observation can never be
# matched to an existing node: every re-run creates a second copy, and naming
# one as a `--parent-of` end creates a third. That breaks idempotency and
# quietly halves a pillar's blast radius by splitting one child into two nodes
# with one inbound edge each. Facts that would naturally be observations are
# recorded as `entity` when they identify something and `constraint` when they
# state a rule the resource is configured to.
IDENTITY_KIND = "entity"
CONFIG_KIND = "constraint"


# --------------------------------------------------------------------------
# Output
# --------------------------------------------------------------------------
#
# Same shape as `_common`'s helpers so the three directories' output reads as one
# stream. The label is padded to eight characters inside the brackets, which is
# what lines the columns up with `[created ]` and `[exists  ]`.


def derived(kind: str, ident: str, extra: str = "") -> None:
    say(f"  [derived ] {kind:<22} {ident}{(' ' + extra) if extra else ''}")


def recorded(kind: str, ident: str, extra: str = "") -> None:
    say(f"  [recorded] {kind:<22} {ident}{(' ' + extra) if extra else ''}")


def queried(kind: str, ident: str, extra: str = "") -> None:
    say(f"  [queried ] {kind:<22} {ident}{(' ' + extra) if extra else ''}")


def blocked(kind: str, ident: str, extra: str = "") -> None:
    say(f"  [blocked ] {kind:<22} {ident}{(' ' + extra) if extra else ''}")


# --------------------------------------------------------------------------
# Argument plumbing
# --------------------------------------------------------------------------


def add_lambo_args(parser: argparse.ArgumentParser) -> None:
    """`--session`, plus the two knobs that say *which* Lambo to talk to.

    Neither a DSN nor a store kind appears here. `lambo` resolves its own
    backends from `LAMBO_CONFIG`, then `./lambo.toml` (Level B, AGENTS.md), and
    `--lambo-config` only chooses which file that is. Passing a connection
    string through argv would put it in `ps` and in shell history, which is the
    same reason `provision_network.py` refuses to take the DSN.
    """
    parser.add_argument(
        "--session",
        required=True,
        help=(
            "Lambo session id to write to and recall against. The same id the "
            "exhibit's `lambo serve-web` has open."
        ),
    )
    parser.add_argument(
        "--lambo-bin",
        default=None,
        help=(
            "Path to the `lambo` binary. Default: `lambo` on PATH, then "
            "target/release/lambo, then target/debug/lambo in this repo."
        ),
    )
    parser.add_argument(
        "--lambo-config",
        default=None,
        help=(
            "Passed through as `lambo --config PATH`. Omit to let lambo resolve "
            "LAMBO_CONFIG, then ./lambo.toml. The store and its DSN are chosen "
            "there, never here."
        ),
    )


def _require_executable(path: pathlib.Path) -> pathlib.Path:
    """A resolvable-but-not-executable binary is a broken stage, not a fallback.

    Spawning it would raise a bare `PermissionError` from `subprocess` that no
    call site catches; fail here instead, where the offending path can be named.
    """
    if not os.access(path, os.X_OK):
        raise InfraError(
            f"{path} exists but is not executable.",
            hint="fix the file mode (chmod +x), or pass a real binary via --lambo-bin.",
        )
    return path


def resolve_lambo_binary(explicit: str | None) -> pathlib.Path:
    """Find the binary, or say exactly how to build one.

    Searched in this order because it matches how the operator's machine is
    likely to be set up: an installed `lambo` first (that is what the agent
    skill and the docs assume), then the repo's own build outputs. When both
    `target/release` and `target/debug` builds exist, the **newer** is used:
    a debug build freshly rebuilt with newly enabled features is preferred over
    a stale release build that predates them.
    """
    if explicit is not None:
        path = pathlib.Path(explicit).expanduser().resolve()
        if not path.is_file():
            raise InfraError(f"--lambo-bin {path} does not exist.")
        return _require_executable(path)

    found = shutil.which("lambo")
    if found:
        return _require_executable(pathlib.Path(found))
    candidates = [
        REPO_ROOT / "target" / "release" / "lambo",
        REPO_ROOT / "target" / "debug" / "lambo",
    ]
    existing = [c for c in candidates if c.is_file()]
    if existing:
        newest = max(existing, key=lambda p: p.stat().st_mtime)
        return _require_executable(newest)
    raise InfraError(
        "the `lambo` binary was not found on PATH or in this repo's target/ directory.",
        hint=(
            "cargo build --release, or pass --lambo-bin <path>. `--dry-run` needs "
            "no binary at all."
        ),
    )


# --------------------------------------------------------------------------
# The lambo CLI
# --------------------------------------------------------------------------


class Lambo:
    """One session's `lambo` CLI, driven as a subprocess.

    Every method here maps onto a verb that really exists in `src/cli/`; the
    flags are taken from `src/main.rs` rather than from the plan's prose, which
    narrates the workflow and is not a flag reference.

    The writer verbs (`derive`, `record-action`) each open a `Memory` and so
    take the single-writer lease for the length of one invocation. The reader
    verbs (`recall`, `inspect`, `stats`) never touch the lease. That split is
    the reason `03_crossover_protect.py` can run beside a live `lambo serve`
    and `01`/`02` cannot; see `_conflict_hint`.
    """

    def __init__(
        self,
        binary: pathlib.Path,
        session: str,
        config: str | None = None,
        timeout: int = 180,
    ) -> None:
        self.binary = binary
        self.session = session
        self.config = config
        self.timeout = timeout

    # -- plumbing ----------------------------------------------------------

    def _argv(self, verb: str, flags: list[str]) -> list[str]:
        argv = [str(self.binary)]
        # `--config` is a global on the root command, so it goes before the
        # subcommand. Clap accepts it either side, but printing it in the
        # documented position keeps the plan output copy-pasteable.
        if self.config:
            argv += ["--config", self.config]
        return argv + [verb, "--session", self.session] + flags

    def _run(self, verb: str, flags: list[str]) -> str:
        argv = self._argv(verb, flags)
        try:
            proc = subprocess.run(argv, capture_output=True, text=True, timeout=self.timeout)
        except FileNotFoundError as exc:
            raise InfraError(
                f"could not execute {self.binary}: {exc}",
                hint="cargo build --release, or pass --lambo-bin <path>.",
            ) from exc
        except subprocess.TimeoutExpired as exc:
            raise InfraError(
                f"`lambo {verb}` did not finish within {self.timeout}s.",
                hint=(
                    "a Cockroach store on a cold connection is slow on the first "
                    "call; re-run, and raise the timeout if it persists."
                ),
            ) from exc
        if proc.returncode != 0:
            detail = (proc.stderr or proc.stdout or "").strip().splitlines()
            # Clap closes its message with a usage block and the generic
            # "For more information, try '--help'." line, so the LAST line is
            # the least informative one. Surface the actual error instead.
            message = next(
                (line.strip() for line in detail
                 if line.strip() and "For more information" not in line),
                None,
            )
            if message is None:
                message = f"exit {proc.returncode}"
            raise InfraError(
                f"`lambo {verb}` failed: {message}",
                hint=_conflict_hint(proc.stderr),
            )
        return proc.stdout.strip()

    # -- writers -----------------------------------------------------------

    def derive(
        self,
        agent: str,
        content: str,
        kind: str,
        concepts: list[str] | None = None,
        parent_of: list[tuple[str, str]] | None = None,
    ) -> str:
        """`lambo derive`. `parent_of` is a list of `(parent, child)`.

        The CLI's own flag is `--parent-of CHILD:PARENT`, child left of the
        colon. That reads backwards next to every other API in this repo, so the
        argument here is `(parent, child)` and the flip happens in one place,
        below, where it can be checked against `src/cli/derive.rs` once instead
        of at every call site.
        """
        flags = ["--agent", agent, "--content", content, "--kind", kind]
        for spec in concepts or []:
            flags += ["--concept", spec]
        flags += _parent_of_flags(parent_of or [])
        return self._run("derive", flags)

    def record_action(
        self,
        agent: str,
        action: str,
        produces: list[str] | None = None,
        modifies: list[str] | None = None,
        depends_on: list[str] | None = None,
    ) -> str:
        """`lambo record-action`.

        Edge direction is fixed by `src/graph/action.rs`: the action node is
        always the source, `Causal` to each produce/modify and `Dependency` to
        each depends-on. Nothing here can change that, and the blast-radius
        arithmetic in `check_single_source` depends on knowing it.
        """
        flags = ["--agent", agent, "--action", action]
        for item in produces or []:
            flags += ["--produces", item]
        for item in modifies or []:
            flags += ["--modifies", item]
        for item in depends_on or []:
            flags += ["--depends-on", item]
        return self._run("record-action", flags)

    # -- readers -----------------------------------------------------------

    def recall(
        self,
        query: str,
        top_k: int | None = None,
        max_tokens: int | None = None,
        traversal_depth: int | None = None,
    ) -> str:
        flags = ["--query", query]
        if top_k is not None:
            flags += ["--top-k", str(top_k)]
        if max_tokens is not None:
            flags += ["--max-tokens", str(max_tokens)]
        if traversal_depth is not None:
            flags += ["--traversal-depth", str(traversal_depth)]
        return self._run("recall", flags)

    def inspect(self, focus: str, depth: int = 2) -> str:
        return self._run("inspect", ["--focus", focus, "--depth", str(depth)])

    def stats(self) -> str:
        return self._run("stats", [])


def _refuse_colon(flag: str, value: str) -> None:
    """Refuse a colon on the CHILD end of `--parent-of`.

    The CLI flag is `CHILD:PARENT` and `src/cli/derive.rs` splits on the first
    colon, so a colon in the child would be read as the delimiter and both sides
    are free text besides. A resource name that grew a colon in the child would
    therefore fail at the CLI with a message about ambiguity rather than about
    the name, so catch it here where the offending value can be named.

    The parent end may legitimately carry a colon — an IPv6 CIDR — because the
    first-colon split keeps the remainder as the parent (T1 part 2 #4), so this
    check guards the child only.
    """
    if ":" in value:
        raise InfraError(
            f"{value!r} contains a colon, which --{flag} cannot express on the child end.",
            hint=(
                "a hierarchy child must be colon-free: rename the child, or express "
                "the relationship with record-action. (A colon-bearing *parent*, "
                "e.g. an IPv6 CIDR, is fine.)"
            ),
        )


def _parent_of_flags(parent_of: list[tuple[str, str]]) -> list[str]:
    """Render `--parent-of CHILD:PARENT` flags from `(parent, child)` pairs.

    The child must be colon-free ([`_refuse_colon`]); the parent may carry a
    colon, so an IPv6 CIDR survives the client and round-trips to the CLI, which
    splits on the first colon (T1 part 2 #4). Kept as its own pure function so
    the IPv6 round-trip can be regression-tested without spawning a subprocess.
    """
    flags: list[str] = []
    for parent, child in parent_of:
        _refuse_colon("parent-of", child)
        flags += ["--parent-of", f"{child}:{parent}"]
    return flags


def _conflict_hint(stderr: str | None) -> str | None:
    if stderr and "single-writer lease" in stderr:
        return (
            "another process holds this session's writer lease, almost certainly "
            "`lambo serve`. Stop it, run this script, then start it again. Only "
            "03_crossover_protect.py is safe to run alongside a live writer, "
            "because it uses read verbs only."
        )
    return None


# --------------------------------------------------------------------------
# Graph shape
# --------------------------------------------------------------------------


def check_single_source(parent_of: list[tuple[str, str]]) -> None:
    """Refuse a hierarchy that hands one concept two parents.

    This looks like a stylistic constraint and is not. Blast radius counts, for
    a node, the concepts whose **only** inbound structural edge comes from it
    (`blast_radii` in `src/recall/format.rs`). A second parent therefore does
    not split the credit between the two parents; it removes the child from
    both of their counts. Do that to enough children and the pillar's blast
    radius reaches zero, `stage3_passes` never admits it, no concept is ever
    promoted to Canonical, and the load-bearing-pillar warning the entire demo
    turns on silently never renders. Nothing errors at any point.

    So the invariant is: containment is expressed exactly once, with
    `--parent-of`, and every other relationship is a `record-action` edge. This
    catches a violation at build time, where it can be read, rather than as a
    blast radius of zero at the climax.
    """
    seen: dict[str, str] = {}
    for parent, child in parent_of:
        if child in seen and seen[child] != parent:
            raise InfraError(
                f"'{child}' is given two hierarchy parents: '{seen[child]}' and '{parent}'.",
                hint=(
                    "a concept with two structural sources counts toward neither "
                    "parent's blast radius. Keep one parent and express the other "
                    "relationship as a record-action --depends-on edge."
                ),
            )
        seen[child] = parent


def account_binding(name: str, aws_id: str) -> str:
    """The concept content that ties a plan name to the live AWS id.

    Derived as an [`IDENTITY_KIND`] child of the resource, never an observation
    (see that constant for why). It is a real fact worth recording on its own,
    because it is what lets a reader check the graph against `describe-*` rather
    than trust it, and it is also one of the exclusive children that gives the
    parent a blast radius at all. No colon, because it has to be usable as a
    `--parent-of` end.
    """
    return f"{name} = {aws_id}"


# --------------------------------------------------------------------------
# Reading `lambo inspect` back
# --------------------------------------------------------------------------


def parse_blast_radius(inspect_text: str) -> int:
    """The `blast radius: N` line from `lambo inspect`, or 0 if absent.

    `inspect` prints this for any focus, whatever its canonization status. That
    is the difference that matters for the climax: `recall` renders the
    load-bearing-pillar warning **only** for a Canonical concept
    (`src/recall/assemble.rs`), and canonization is earned over time by the
    daemon in a long-running writer. `inspect` reports the structure as it
    stands right now. The guard in 03 therefore reads both.
    """
    for line in inspect_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("blast radius:"):
            try:
                return int(stripped.split(":", 1)[1].strip())
            except ValueError:
                return 0
    return 0


def parse_outbound_neighbours(inspect_text: str) -> list[str]:
    """Hop-1 neighbours the focus points at, by concept content.

    `render_neighbourhood` writes one indented `EdgeType` heading per hop and
    prefixes each neighbour with `->` when the focus is the edge's source and
    `<-` when it is the target. Only the `->` direction is a dependent; a `<-`
    is something that points *at* the focus, and listing those would name the
    **Keep only the structural headings, and only their `->` rows.** A
    dependent is a concept with an outbound `Causal`, `Dependency` or
    `Hierarchical` edge from the focus — the shapes that mean "this would be
    stranded if the focus went away". `CoOccurrence` (two concepts written by
    the same interaction) and `Semantic` (near in embedding space) are
    co-occurrence, not dependence, and naming them would show a judge a false
    "stranded dependent"; `Derives`/`Temporal` are interaction provenance.
    With T7 removing the cross-tier edge at its source, the hierarchy children
    render under `Hierarchical`, so restricting to structural headings no
    longer loses them. The authoritative *count* is `parse_blast_radius`; this
    is the list of names to show beside it.

    Parsing text is not free of risk, and the risk is taken knowingly: neither
    `recall` nor `inspect` has a JSON mode on the CLI today (`inspect` builds a
    structured value internally and discards it), and the alternative is to
    print a warning with no names in it.

    Indentation is the grammar: `render_neighbourhood` writes the hop header
    flush left, each edge-type heading at two spaces, and each neighbour at
    four. Anything else at hop level is the truncation notice, which ends
    collection because what follows it is not a neighbour row.
    """
    names: list[str] = []
    in_hop_one = False
    counts = False
    for raw in inspect_text.splitlines():
        stripped = raw.strip()
        if not stripped:
            continue
        if stripped.startswith("hop "):
            in_hop_one = stripped == "hop 1:"
            counts = False
        elif not in_hop_one:
            continue
        elif raw.startswith("    "):
            if counts and stripped.startswith("-> "):
                # Labels render as `content [Type, canonical]`; keep only the
                # content. The trailing ` [Type{, status}]` is render metadata;
                # the content itself may contain a ` [`, so strip only a real
                # trailing metadata bracket rather than truncating at the last
                # ` [` ([`NEIGHBOUR_METADATA_RE`]).
                names.append(NEIGHBOUR_METADATA_RE.sub("", stripped[3:]).strip())
        elif raw.startswith("  "):
            counts = stripped in STRUCTURAL_EDGE_LABELS
        else:
            counts = False
    return names


def carries_pillar_warning(text: str) -> bool:
    """Does this block carry the spec §13 load-bearing-pillar line?

    This is the machine-readable half of the agent skill's pre-flight recall
    protocol (plan §4.1): if the warning comes back, the destructive action is
    halted.
    """
    return PILLAR_WARNING_PREFIX in text


def indent(text: str, prefix: str = "    ") -> str:
    """Quote CLI output inside a script's own output without losing blank lines."""
    return "\n".join(prefix + line if line else prefix.rstrip() for line in text.splitlines())


def _self_test_ipv6_parent() -> None:
    """Regression for the T7 IPv6 deferral (T1b-R1-3): an IPv6 CIDR as a
    `--parent-of` PARENT must survive the client's colon check and round-trip
    into `CHILD:PARENT`, while a colon on the CHILD end is still refused.

    These agent scripts have no formal test framework, so this runs as a
    standalone self-check (`python3 scripts/cloudops/_lambo.py`).
    """
    flags = _parent_of_flags([("2001:db8::/64", "SG-PublicWeb")])
    expected = ["--parent-of", "SG-PublicWeb:2001:db8::/64"]
    if flags != expected:
        raise SystemExit(
            f"self-test FAILED: _parent_of_flags returned {flags!r}, expected {expected!r}"
        )
    try:
        _parent_of_flags([("2001:db8::/64", "SG:Public")])  # colon in the child
    except InfraError:
        pass
    else:
        raise SystemExit("self-test FAILED: a colon-bearing child must still be refused")


def _self_test_structural_whitelist() -> None:
    """Regression for T8 (T3-2-P2-2; review T8-R1-4): the outbound neighbour
    list must name exactly the *structural* dependents — Causal, Dependency,
    Hierarchical — and none of the decaying/provenance kinds, mirroring the Rust
    set `is_structural` (`src/cli/serve_web.rs`) / `DRIFT_EDGE_TYPES`
    (`src/daemon/drift.rs`). The synthetic banner below mixes all seven rendered
    edge kinds in hop 1 plus a hop-2 row; if a structural heading were renamed,
    dropped, or an excluded kind started counting, the assertion fails.
    """
    banner = (
        "focus: RDS-Lambo-Demo-DB [Resource]\n"
        "\n"
        "hop 1:\n"
        "  CoOccurrence\n"
        "    -> coincident-peer [Entity]\n"
        "  Semantic\n"
        "    -> embedding-neighbour [Entity]\n"
        "  Derives\n"
        "    -> <interaction d3659f27-fdfe-4e55-8eaa-f56f0019e0c9>\n"
        "  Temporal\n"
        "    -> ordering-peer [Entity]\n"
        "  Causal\n"
        "    -> causal-dependent [Entity]\n"
        "  Dependency\n"
        "    -> resource-dependent [Resource]\n"
        "  Hierarchical\n"
        "    -> child-concept [Entity]\n"
        "\n"
        "hop 2:\n"
        "  Causal\n"
        "    -> hop-two-ignored [Resource]\n"
    )
    names = parse_outbound_neighbours(banner)
    expected = ["causal-dependent", "resource-dependent", "child-concept"]
    if names != expected:
        raise SystemExit(
            "self-test FAILED: parse_outbound_neighbours returned "
            f"{names!r}, expected only the three structural dependents {expected!r}"
        )


def _source_focus_missing_text() -> str:
    """Return the live `Focus::Missing` error-formatting source from the Rust
    CLI, so `EMPTY_SESSION_ERR` is pinned to the real string rather than a
    duplicated copy. Reads `src/cli/inspect.rs` (resolved against REPO_ROOT)
    and returns the text of the `Focus::Missing` match arm. Fails loudly —
    raising SystemExit — if the source can't be found or read, or if the arm
    is no longer present, so this self-test never silently vacates.
    """
    inspect_rs = REPO_ROOT / "src" / "cli" / "inspect.rs"
    try:
        text = inspect_rs.read_text(encoding="utf-8")
    except OSError as exc:
        raise SystemExit(
            "self-test FAILED: could not read the live CLI error source "
            f"{inspect_rs} to pin EMPTY_SESSION_ERR: {exc}"
        ) from exc
    lines = text.splitlines()
    for i, line in enumerate(lines):
        # Target the *error-formatting* arm (`Focus::Missing => Err(...)`) —
        # not the unrelated `return Focus::Missing;` in `resolve_focus` — so we
        # pin against the actual runtime message source.
        if "Focus::Missing" in line and "=> Err(" in line:
            # This arm spans this line and the following ones; collect a bounded
            # window so a reword that drops or renames the phrase still fails
            # the assertion below.
            return "\n".join(lines[i : i + 12])
    raise SystemExit(
        "self-test FAILED: `Focus::Missing` arm not found in the live CLI "
        f"source {inspect_rs}; EMPTY_SESSION_ERR can no longer be pinned"
    )


def _self_test_empty_session_sentinel() -> None:
    """Regression for T8-R1-2: `EMPTY_SESSION_ERR` must still match the live
    `lambo inspect` empty-session error — the `Focus::Missing` arm at
    `src/cli/inspect.rs` emits `no concept matching '{focus}' in session
    '{session}'`. This sources the actual Rust source at self-test time (see
    `_source_focus_missing_text`), so a reword of the CLI fails loudly instead
    of a stale sentinel silently reverting 03_crossover_protect.py's guard to
    a hard abort on empty sessions (the original T3-2-P2-1 bug).
    """
    live = _source_focus_missing_text()
    if EMPTY_SESSION_ERR not in live:
        raise SystemExit(
            "self-test FAILED: EMPTY_SESSION_ERR no longer matches the live "
            f"lambo Focus::Missing error source; got {live!r}"
        )


if __name__ == "__main__":
    _self_test_ipv6_parent()
    _self_test_structural_whitelist()
    _self_test_empty_session_sentinel()
    print("self-test: IPv6 --parent-of; structural whitelist; empty-session sentinel — all pass")
