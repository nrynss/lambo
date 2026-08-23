# Agent briefs

The prompts that actually drove the agentic cycles on this branch: what each
implementation, review, remediation and validation agent was told, including the
worktree it was pointed at, the commit it started from, and the section of the
workstream doc it was given as authority.

They are the other half of the record. `dev-diary/adversarial-review/` holds what
the agents concluded; these hold what they were asked. A review verdict is hard to
weigh without knowing what the reviewer was pointed at and what it was told to
ignore, and a closure that looks thin often turns out to be a brief that scoped it
that way.

Recovered 2026-08-24 from a directory literally named `local:` at the repository
root, which was an untracked path typo, not a location anyone chose. The files
themselves are dated 2026-08-23 and cover **F4, J3 round 3, J4 including its round
2 and validation legs, and J5**.

Two honest gaps:

* **Coverage is partial.** Only the workstreams whose briefs happened to be written
  to files are here. Earlier ones (A, C, D, K) were composed inline and are not
  recoverable, and workstream B's were composed inline too, so B is represented by
  `b-run/CYCLE.md` and its reports rather than by briefs.
* **These are the briefs as sent, not as followed.** Where an agent negotiated
  scope mid-run, or was corrected, that exchange is not here. Read them as the
  opening instruction, not as a transcript.
