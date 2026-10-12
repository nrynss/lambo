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

# The edge kinds `lambo inspect` names in its `dependents (N):` section (#36):
# a concept is a dependent of the focus when it has a `Dependency` edge into
# the focus (an action's `--depends-on`) or is one of the focus's
# `--parent-of` children. `Causal` (`--produces`/`--modifies`) records
# provenance and never makes a dependent, nor do the association kinds
# (`CoOccurrence`, `Semantic`) or interaction provenance (`Derives`,
# `Temporal`). This tuple mirrors `EdgeType::dependent_end` in
# `src/types/mod.rs`; `_self_test_dependents_section` pins the reader to the
# section's grammar.
DEPENDENT_EDGE_LABELS = ("Dependency", "Hierarchical")

# `lambo inspect` emits this exact phrase when the focus concept is not in the
# session at all — the CLI's `Focus::Missing` arm renders `missing_refusal` in
# `src/surface/focus.rs`, which produces
# `no concept matching '{focus}' in session '{session}'`, and exits 1 — which is
# the one condition `03_crossover_protect.py` treats as "nothing to protect".
# The sentinel lives here (beside the reader-verb contract) rather than inline
# in 03 so re-using it is a named constant, and `_self_test_empty_session_sentinel`
# pins it to the real error string: a future reword of the CLI fails loudly
# instead of silently reverting the empty-session guard to a hard abort.
EMPTY_SESSION_ERR = "no concept matching"

# `inspect` renders a neighbour as `content [Type{, status}]`, where the
# bracketed tail is render metadata (ConceptType plus optional canonization
# status; see `src/surface/neighbourhood.rs` `render_neighbourhood`/`label`). The content
# itself may legitimately contain a ` [`, so the tail is stripped with this
# anchored pattern rather than a blind `rsplit(" [", 1)` — which would truncate
# bracketed concept text. Dependents are always concepts, so only ConceptType
# tags appear.
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
        each depends-on. A `--depends-on` entry makes the action a dependent
        of that concept, so it counts toward the concept's blast radius;
        `--produces`/`--modifies` record provenance and do not (#36).
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


def account_binding(name: str, aws_id: str) -> str:
    """The concept content that ties a plan name to the live AWS id.

    Derived as an [`IDENTITY_KIND`] child of the resource, never an observation
    (see that constant for why). It is a real fact worth recording on its own,
    because it is what lets a reader check the graph against `describe-*` rather
    than trust it, and as a `--parent-of` child it is one of the parent's
    dependents, so it counts toward the parent's blast radius. No colon, because it has to be usable as a
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


def parse_dependents(inspect_text: str) -> list[str]:
    """The focus's dependents, by concept content, from `lambo inspect`.

    `render_neighbourhood` (`src/surface/neighbourhood.rs`) prints the
    dependents its blast radius counts in their own section, right after the
    `blast radius:` line and the warning:

        dependents (2):
          RDS-Lambo-Demo-DB [Resource] (Hierarchical)
          app-data-agent provisioned ... [Resource] (Dependency)

    A dependent is a concept with a `Dependency` edge into the focus or one of
    its `--parent-of` children (#36, [`DEPENDENT_EDGE_LABELS`]), which is
    exactly the set the `blast radius:` count is made of. The hop listing
    below the section names neighbours in both directions and is not read.

    Parsing text is a knowing risk: the CLI has no JSON mode for `inspect`
    (the MCP tool's structured result carries the same list as `dependents`).
    The grammar is the section header (carrying the true total), then
    two-space rows of `content [Type{, status}] (EdgeKind)`, then an optional
    `(truncated at N dependents)` row when the rows stop at inspect's cap.
    Anything else ends the section. A truncated list returns the rows that
    were printed; [`parse_dependents_total`] gives the full count.
    `_self_test_dependents_section` runs this against the renderer's own
    output, pinned in `fixtures/inspect-dependents-golden.txt`.
    """
    names: list[str] = []
    in_section = False
    for raw in inspect_text.splitlines():
        stripped = raw.strip()
        if not in_section:
            in_section = stripped.startswith("dependents (") and stripped.endswith("):")
            continue
        if not raw.startswith("  ") or raw.startswith("   ") or not stripped:
            break
        if stripped.startswith("(truncated at "):
            break
        kind_at = stripped.rfind(" (")
        if kind_at < 0 or stripped[kind_at + 2 : -1] not in DEPENDENT_EDGE_LABELS:
            break
        names.append(NEIGHBOUR_METADATA_RE.sub("", stripped[:kind_at]).strip())
    return names


def parse_dependents_total(inspect_text: str) -> int:
    """The true dependent total from the `dependents (N):` header, or 0.

    Past inspect's row cap the section lists fewer rows than this; the header
    always carries the full count, which equals the `blast radius:` line.
    """
    for line in inspect_text.splitlines():
        stripped = line.strip()
        if stripped.startswith("dependents (") and stripped.endswith("):"):
            try:
                return int(stripped[len("dependents (") : -2])
            except ValueError:
                return 0
    return 0


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


def _self_test_dependents_section() -> None:
    """Pin `parse_dependents` to real renderer output (#36).

    `fixtures/inspect-dependents-golden.txt` is `render_neighbourhood`'s text
    for the shared dependents case, pinned byte for byte by the Rust test
    `inspect_text_matches_the_golden_the_cloudops_guard_parses`
    (`src/surface/neighbourhood.rs`). Its hop listing names a parent, a
    `Causal` producer, an association neighbour and a concept the focus
    depends on, none of which may be read as a dependent.

    The truncation row cannot appear in that small case, so its text is
    pinned to the renderer's source instead, and a truncated banner is built
    from the golden with it: the parser must return the printed rows and the
    header's true total.
    """
    golden_path = REPO_ROOT / "fixtures" / "inspect-dependents-golden.txt"
    try:
        golden = golden_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise SystemExit(f"self-test FAILED: cannot read {golden_path}: {exc}") from exc
    expected = [f"concept {n}" for n in ("A1", "A2", "C1", "C2", "E", "V", "W", "Z")]
    names = parse_dependents(golden)
    if names != expected:
        raise SystemExit(
            f"self-test FAILED: parse_dependents returned {names!r}, expected {expected!r}"
        )
    if parse_dependents_total(golden) != len(expected) or parse_blast_radius(golden) != len(expected):
        raise SystemExit("self-test FAILED: the dependents header must equal the blast radius")

    source_path = REPO_ROOT / "src" / "surface" / "neighbourhood.rs"
    try:
        source = source_path.read_text(encoding="utf-8")
    except OSError as exc:
        raise SystemExit(f"self-test FAILED: cannot read {source_path}: {exc}") from exc
    header_fmt = '"dependents ({dependents_total}):\\n"'
    row_fmt = '"  {} ({:?})\\n"'
    trunc_fmt = '"  (truncated at {MAX_INSPECT_NODES} dependents)\\n"'
    for needle in (header_fmt, row_fmt, trunc_fmt):
        if needle not in source:
            raise SystemExit(
                f"self-test FAILED: {source_path} no longer renders {needle}; "
                "update parse_dependents and this self-test together"
            )
    truncated = golden.replace("dependents (8):", "dependents (250):", 1).replace(
        "  concept Z [Resource] (Dependency)\n",
        "  concept Z [Resource] (Dependency)\n  (truncated at 200 dependents)\n",
        1,
    )
    if parse_dependents(truncated) != expected or parse_dependents_total(truncated) != 250:
        raise SystemExit("self-test FAILED: a truncated dependents list must parse")
    if parse_dependents("focus: lonely [Entity]\nblast radius: 0\n") != []:
        raise SystemExit("self-test FAILED: no dependents section must read as no dependents")


def _source_focus_missing_text() -> str:
    """Return the live `Focus::Missing` error-formatting source, so
    `EMPTY_SESSION_ERR` is pinned to the real string rather than a duplicated
    copy. The CLI's `Focus::Missing` arm (`src/cli/inspect.rs`) renders its
    error with `missing_refusal`, which lives in `src/surface/focus.rs`
    (resolved against REPO_ROOT) beside `resolve_focus`; this returns the text
    of that function. Fails loudly — raising SystemExit — if the source can't
    be found or read, or if the function is no longer present, so this
    self-test never silently vacates.
    """
    focus_rs = REPO_ROOT / "src" / "surface" / "focus.rs"
    try:
        text = focus_rs.read_text(encoding="utf-8")
    except OSError as exc:
        raise SystemExit(
            "self-test FAILED: could not read the live inspect error source "
            f"{focus_rs} to pin EMPTY_SESSION_ERR: {exc}"
        ) from exc
    lines = text.splitlines()
    for i, line in enumerate(lines):
        # Target the *error-formatting* function — not the `Focus::Missing`
        # values `resolve_focus` returns — so we pin against the actual runtime
        # message source.
        if "fn missing_refusal(" in line:
            # Collect a bounded window over the function body so a reword that
            # drops or renames the phrase still fails the assertion below.
            return "\n".join(lines[i : i + 12])
    raise SystemExit(
        "self-test FAILED: `missing_refusal` not found in the live inspect "
        f"source {focus_rs}; EMPTY_SESSION_ERR can no longer be pinned"
    )


def _check_cli_missing_arm() -> None:
    """Fail unless the CLI's `Focus::Missing` arm (`src/cli/inspect.rs`) still
    renders its error with `missing_refusal(` and maps it to
    `CliError::Runtime`. Pinning `missing_refusal`'s text alone would keep
    passing if `lambo inspect` stopped calling it, or changed how the error
    leaves the process, and the sentinel guards what `lambo inspect` prints.
    """
    inspect_rs = REPO_ROOT / "src" / "cli" / "inspect.rs"
    try:
        lines = inspect_rs.read_text(encoding="utf-8").splitlines()
    except OSError as exc:
        raise SystemExit(
            "self-test FAILED: could not read the CLI inspect source "
            f"{inspect_rs}: {exc}"
        ) from exc
    for i, line in enumerate(lines):
        if "Focus::Missing" in line:
            # The arm may be wrapped by rustfmt; look a few lines ahead.
            arm = "\n".join(lines[i : i + 4])
            if "missing_refusal(" in arm and "CliError::Runtime" in arm:
                return
    raise SystemExit(
        "self-test FAILED: no `Focus::Missing` arm in "
        f"{inspect_rs} maps `missing_refusal(` to `CliError::Runtime`; "
        "EMPTY_SESSION_ERR no longer pins what `lambo inspect` prints"
    )


def _self_test_empty_session_sentinel() -> None:
    """Regression for T8-R1-2: `EMPTY_SESSION_ERR` must still match the live
    `lambo inspect` empty-session error — the `Focus::Missing` arm at
    `src/cli/inspect.rs` emits `missing_refusal`'s `no concept matching
    '{focus}' in session '{session}'` (`src/surface/focus.rs`). This sources
    the actual Rust source at self-test time (see
    `_source_focus_missing_text` and `_check_cli_missing_arm`), so a reword of
    the CLI, or a CLI arm that stops using `missing_refusal`, fails loudly
    instead of a stale sentinel silently reverting 03_crossover_protect.py's
    guard to a hard abort on empty sessions (the original T3-2-P2-1 bug).
    """
    _check_cli_missing_arm()
    live = _source_focus_missing_text()
    if EMPTY_SESSION_ERR not in live:
        raise SystemExit(
            "self-test FAILED: EMPTY_SESSION_ERR no longer matches the live "
            f"lambo Focus::Missing error source; got {live!r}"
        )


if __name__ == "__main__":
    _self_test_ipv6_parent()
    _self_test_dependents_section()
    _self_test_empty_session_sentinel()
    print("self-test: IPv6 --parent-of; dependents section; empty-session sentinel — all pass")
