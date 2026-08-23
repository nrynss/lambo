# B0 round-2 review brief

Orchestrator: grok-agent. Review the B0 round-1 remediation against
`adve-review-mooshik-B-B0-round1.md` and this tree on branch
`b0-pg-extraction`.

**Reviews only.** Edit nothing except
`dev-diary/adversarial-review/adve-review-mooshik-B-B0-round2.md`.
No commit, no push. Restore the tree after every mutation.

House style: `dev-diary/adversarial-review/adve-review-mooshik-K-round2.md`.
Mutation-test every claimed regression pin. Re-run every gate yourself.
Hunt for defects the remediation introduced.

## Closures to verify

- B0-R1-1 (P2): composed-SQL byte-identity test landed; M5 and M7 now red;
  CYCLE.md counts 939 / 598 / 1007.
- B0-R1-2 (P3): two rustdoc links fixed; `--document-private-items` on the
  gate list; two new warnings gone.
- B0-R1-3 (P3): write-up names `init_schema` and `connect_options` as known
  over-merged B2/B3 debt. Code not split (correct).
- B0-R1-4 (P3): B0-N2 lists both `PgStore::new` and `connect_options`.
- B0-R1-5 (P3): CYCLE.md fixtures row present (may have closed before
  remediation). Verify, do not re-open if already done.
- B0-R1-6 (P3): 52 corrected to 26 of 938 (26 of 597, 36 of 1006).

A closure holds only if the cited test FAILS under the mutation. Trace
verification is allowed only when a weightless/offline mutation is
impractical, and must be labelled as such.

## Standing

No em dashes. Do not touch `.env` or `models/`. Do not start Postgres.
Do not run live Cockroach tests. Do not re-litigate B-postgres-store.md.

Verdict: APPROVE with zero residue, or REQUEST_CHANGES with P1/P2/P3
findings. agent_id `B0Review2`.
