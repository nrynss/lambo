# remed-T8 — crossover-guard live-run evidence (round-1 remediation)

**Captured:** re-run read-only for the T8 round-1 remediation, against the
worktree at detached HEAD (object `555698e`).

> **Redaction note (repo rule).** This repo keeps no DSN, API key, cluster id,
> or home/public IP out of `dev-diary/evidence/` — any such value on the wire at
> capture time is replaced by a named placeholder (see `dev-diary/evidence/README.md`).
> These transcripts contain **no real identifiers at all**: the security-group /
> VPC ids (`sg-0abc1234`, `vpc-0def5678`), the session ids
> (`t8-demo-session`, `t8-empty-session-probe-9f3`) and the concept content are
> synthetic stand-ins for the real exhibit, so there is nothing to scrub. The
> exact `lambo inspect` empty-session error string (`no concept matching … in
> session …`) is the real CLI text, reproduced verbatim from
> `src/cli/inspect.rs` (`Focus::Missing`).

## Method (read-only)

`03_crossover_protect.py` is driven exactly as the adversarial review did:
only `Lambo`'s two **read** verbs — `recall` / `inspect` — are stubbed with safe
synthetic output, then `run_guard` → `render_blocked` / `render_unprotected` →
the `main` exit-code mapping run for real. No AWS API and no `lambo` binary are
invoked; the destructive action is only ever *rendered* as text (03 has no code
path that issues a mutating AWS call). `recall`/`inspect` never take the writer
lease, so nothing on disk is touched.

---

## PARSE-UNIT CHECK — `parse_outbound_neighbours` (structural whitelist)

Synthetic `inspect` banner mixing all four non-structural rendered kinds
(`CoOccurrence` shown; `Semantic`/`Derives`/`Temporal` exercised by the
`_lambo.py` self-test) with the three structural kinds, plus a hop-2 decoy:

```
focus: RDS-Lambo-Demo-DB [Resource]

hop 1:
  CoOccurrence
    -> other concept in the same interaction [Entity]
  Causal
    -> RDS-Lambo-Demo-DB config block [Constraint]
  Dependency
    -> rds-lambo-demo-db [Resource]
    -> vpc-route entry [Constraint]
  Hierarchical
    -> SG-Base-VPC child [Entity]

hop 2:
  Causal
    -> hop-two-peers-ignored [Resource]
```

Captured output:

```
structural dependents: ['RDS-Lambo-Demo-DB config block', 'rds-lambo-demo-db', 'vpc-route entry', 'SG-Base-VPC child']
assert OK: CoOccurrence decoy excluded; all structural kept; hop 2 ignored.
```

Conclusion: exactly the three structural edge kinds name dependents;
`CoOccurrence` (and by the `_lambo.py` self-test, `Semantic`/`Derives`/`Temporal`)
are excluded; hop-2 rows do not count.

---

## CASE 1 — shared-SG guard (non-empty session, structural dependents)

Captured (stdout):

```
==> pre-flight recall protocol (plan §4.1)
  . query: "tear down Subnet-Private-1a and delete SG-Base-VPC"

    context:
      nothing about this drift query is Canonical yet; no pillar warning

  [queried ] recall                 pillar warning absent; the pillar is not Canonical yet
==> inspecting SG-Base-VPC directly

    focus: RDS-Lambo-Demo-DB [Resource]

    hop 1:
      CoOccurrence
        -> other concept in the same interaction [Entity]
      Causal
        -> RDS-Lambo-Demo-DB config block [Constraint]
      Dependency
        -> rds-lambo-demo-db [Resource]
        -> vpc-route entry [Constraint]
      Hierarchical
        -> SG-Base-VPC child [Entity]

    hop 2:
      Causal
        -> hop-two-peers-ignored [Resource]

  [queried ] inspect                SG-Base-VPC blast radius 0

==> ABORTED. The destructive action was not issued.

  [blocked ] aws-call               delete-security-group on SG-Base-VPC (sg-0abc1234)
  . ec2:DeleteSecurityGroup GroupId=sg-0abc1234
  .   the group is the internal mesh of vpc-0def5678

==> why
  . 4 concept(s) hang off it and would be stranded

==> what would have been stranded
  . RDS-Lambo-Demo-DB config block
  . rds-lambo-demo-db
  . vpc-route entry
  . SG-Base-VPC child

  . network-infra-agent halted, as the lambo-cloudops agent skill requires (plan §4.1)
  . no AWS resource was created, modified or deleted
[exit code] blocked -> 0
```

Conclusion: the guard blocks (exit 0), the abort banner names only the three
structural dependents, `CoOccurrence` is absent, and hop 2 is never counted.

---

## CASE 2 — empty / wrong session (`lambo inspect` raises `Focus::Missing`)

`Lambo.inspect` raises the real empty-session `InfraError`
(`no concept matching 'SG-Base-VPC' in session 't8-empty-session-probe-9f3'`),
which `run_guard` swallows *only* for that exact phrase (`EMPTY_SESSION_ERR`).

Captured (stdout):

```
==> pre-flight recall protocol (plan §4.1)
  . query: "tear down Subnet-Private-1a and delete SG-Base-VPC"

    context:
      nothing about this drift query is Canonical yet; no pillar warning

  [queried ] recall                 pillar warning absent; the pillar is not Canonical yet
==> inspecting SG-Base-VPC directly
  ! SG-Base-VPC is not in session 't8-empty-session-probe-9f3'; nothing to protect



  [queried ] inspect                SG-Base-VPC blast radius 0

==> NOT BLOCKED. The destructive action was still not issued.

  ! Lambo reports no dependents for SG-Base-VPC: no pillar warning, and a blast radius of 0.

==> what that most likely means
  . 01_network_agent.py and 02_app_data_agent.py have not run against this session
  . or they ran against a different session id than the one passed here

  . this script refuses the action regardless of the verdict; nothing was deleted
[exit code] concept_missing -> 0
```

Captured **stderr** — the T8-R1-1 prominent banner, emitted in addition to the
stdout note, so a mistyped/unpopulated session is unmissable even when stdout is
captured to a log (exit code stays 0):

```
!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
!! EMPTY OR WRONG SESSION: nothing to protect                                      !!
!! double-check --session 't8-empty-session-probe-9f3', and that agents 01/02 have !!
!! run against THIS session id, not a different one                                !!
!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!
```

Conclusion: `render_unprotected` runs, the exit code is 0 (concept_missing), and
the operator-facing banner goes to stderr.

---

## Re-verification

- `python3 -m py_compile scripts/cloudops/03_crossover_protect.py scripts/cloudops/_lambo.py scripts/cloudops/02_app_data_agent.py` — PASS
- `python3 scripts/cloudops/_lambo.py` (self-test: IPv6 `--parent-of`; structural whitelist; empty-session sentinel) — PASS
