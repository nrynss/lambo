# B1 round-2 review brief

Orchestrator: grok-agent. Verify B1 round-1 closures. Reviews only.
Write only `dev-diary/adversarial-review/adve-review-mooshik-B-B1-round2.md`.
No commit. No merge to lambo-for-mooshik.

House style: B0 round 2 / K round 2. Mutation-test every claimed pin.
Re-run gates. Hunt defects the remediation introduced.

Closures: R1-1 omitted-db + libpq spelling; R1-2 exhaustive shareable
helper so `_ => true` goes red; R1-3 provision Postgres arm; R1-4 em
dashes gone; R1-5 config.mdx + site postgres as shareable.

A closure holds only if the cited test FAILS under the mutation.

Keep CYCLE merge-once line. Do not reopen B0. No live DB. No em dashes.
agent_id `B1Review2`.
