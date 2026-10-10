# Spec deviation: §2.2, one process may hold many sessions (#32)

The spec of record, `lambo-hackathon-spec-v0.1.md`, is frozen. This note
records where shipped code departs from §2.2, as the dev-diary README asks
for any disagreement with the spec. It does not edit the spec.

Recorded 2026-10-10. The deviation shipped with #32 PRs 4 to 7 and first
reached main in PR 4 (#72, `22441345`).

## What §2.2 says

§2.2, "Single writer, many readers — the deployment model":

- one `lambo serve` process owns a session;
- any number of readers may query the store directly;
- readers never write.

Read literally, the first bullet pairs one process with one session. Every
serve before #32 behaved that way: `lambo serve --session X` held exactly
one session for its whole life.

## What ships

One HTTP `lambo serve` can hold **many per-session leases at once**. There
are two ways a session gets one:

- **pinned**, by a repeated `--session` or `[serve] sessions`;
- **on demand**, when a credential reaches past the pinned sessions.

Each attached session has its own `Memory`, its own lease row, its own
monotonic fencing token, its own lease heartbeat and its own fence. Many
leases share one process, but each lease is held exactly as before.

## What does not change: one writer per session

The property §2.2 protects still holds: **two processes never write one
session.** Two RAM copies of one session would diverge.

- The lease model is unchanged. Each session's lease is acquired, renewed
  and released by itself. The store fence still applies to each session
  separately: the `flush_batch` token check and Postgres `FOR SHARE`, with
  lease rows locked in sorted session order (refactor-26).
- A second process that asks for a session this process holds is refused,
  as before:
  - a CLI writer exits 1;
  - a second multi-session hub marks that session `HeldElsewhere` and
    answers 503 for it;
  - a stdio serve proxies into the holder's socket for that session (J2,
    unchanged).
- Inside the process, each session is a separate `Memory`. The isolation
  tests check that recall, inspect, saints, stats and GC never cross
  sessions (PR 4).
- A detach (idle, eviction, lost lease, erase) flushes the session and
  releases its lease, or, for a lost lease or an erase, closes without
  releasing. A reattach mints `token + 1`, so a straggler from the earlier
  attach is fenced (#23 made tokens monotonic across a release).
- Readers are unaffected. `serve-web` and the read verbs still take no
  lease.

So the restated rule is: **each session has exactly one owning process, and
a process may own many sessions.** This is the wording the #32 design
recommended (Q18), and the repo owner accepted it on 2026-10-09.

## Consequences an operator should know

- **A lost lease.** A process that holds several sessions, or that attaches
  sessions on demand, does not exit when one session loses its lease. Only
  that session is detached (`LeaseLossPolicy::DetachSession`). A
  one-session serve with nothing on demand still exits (`ExitProcess`), so
  its client respawns into a proxy, as before. That includes the rig's
  single `--session lambo-dev` with or without `LAMBO_AUTH_TOKEN`.
- **Startup.** A pinned session held by another process at startup no
  longer blocks the start. The process serves its other sessions, answers
  503 for that one, and tries again 5 to 6 s after each failed attempt.
- **SQLite.** On SQLite, every session's flushes, heartbeats and attach
  loads share one pooled connection (design R1). `attach_concurrency` is
  forced to 1 there. For user scope at scale, use Postgres.

## Where it is documented

- [feature-32-multi-session.md](feature-32-multi-session.md): the
  consolidated #32 record.
- `docs/reference/cli.mdx`, "Several sessions in one serve", and its site
  mirror.
- `dev-diary/lambo-for-mooshik/DOGFOOD-SETUP.md` § 7, for the rig.
