# B4 round-1 remediation brief

Orchestrator: grok-agent. Close B4-R1-1. **Do not commit. Do not merge
to lambo-for-mooshik.** You edit; the next review is a different agent.

## B4-R1-1 (P2)

`.github/workflows/ci.yml` `postgres-live` first step passes two cargo
`TESTNAME` args. Cargo 1.97.1 takes one. The step never starts; greps
are dead; the two-width step is broken with it.

The B3 step in the same job has the same two-name shape. Fix both.

**Closure:** pass test names as harness filters after `--`, keep
`--ignored --nocapture --exact` and both greps.

Do not reopen the live-check code. No em dashes. No `.env`.

Write `b-run/B4-remediation.md` and a closures appendix on
`adve-review-mooshik-B-B4-round1.md`. agent_id `b4-remediator`.
