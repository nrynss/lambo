# B4 implementation brief

Orchestrator: grok-agent. B0-B3 closed on `b0-pg-extraction` (`e219488`).
You implement B4. **Do not commit. Do not merge to lambo-for-mooshik.**

## Read
`B-postgres-store.md` **B4**. CYCLE.md. F-R1-2 pin semantics already
shipped: do not re-decide authority.

## Build
`GraphStore::vector_dimensions()` on Postgres reports the **schema**
width (`vector(n)` DDL), the way Cockroach reports `VECTOR(n)` from
DDL and ignores the pin for reporting.

The pin still fires at `resolve_backends` (kind-agnostic, serving
verbs). Construction of a store whose sessions carry a different
contract must still work (re-embed). `check_vector_compatibility` is
not the pin check.

Add what SQLite cannot have: the *initialized* schema width matches
config, verified against the **live database**, not echoed from the
same config value. Pin a test that goes red if reporting just
echoes construction dim while the live column is a different n.

Preserve B3 conversion, H3, fencing, quarantine. sqlite.rs untouched
unless a shared test helper requires it.

Pinned image for live schema probe:
`pgvector/pgvector:pg17` digest
`sha256:cf134a767f474095eeba57e0117be8e568e011a63f33fbf252f14c9b760f8e6f`.

## Do not
Re-litigate pin precedence. Claim B4 closed H3. Touch `.env`. No em
dashes. No commit.

## Gates
CYCLE + store-postgres + live schema probe if you run the container.

## Deliverable
`b-run/B4-implementation.md`. agent_id `b4-implementor`.
