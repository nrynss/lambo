# J5 — proportional review (round 1; REVIEW-ONLY)

J5 is small and docs/CI-only (no Rust changed, per the implementer; confirm). Review the
three commits on `wt/j5` in the worktree `/home/nryn/work/lambo/.claude/worktrees/j5` (base
`0dd15b3`, pushed: `7b8b9d9` CI gate, `6879633` four-mirror prose, `a778235` docs/decision
note). REVIEW-ONLY: verify + verdict; the only file you write is your review doc.

## The one thing that most needs independent adjudication

The implementer DEVIATED from the brief's "byte-identical pairs + raw diff gate" framing:
it cites `dev-diary/lambo-for-mooshik/J-multi-client.md:838-848` (J2 round-1 review) that the
four copies are DELIBERATELY not raw-byte-identical — the site copies add Astro imports, a
`/lambo/` link prefix, and mcp.mdx's site copy has a site-only "Verified clients" /
managed-CockroachDB section. So it built a **canonical shared-prose** drift gate
(`scripts/docs/check-mirror-drift.sh`) that strips those site-only deltas and diffs the
shared prose. Verify:
1. The design record at `J-multi-client.md:838-848` genuinely says the copies are NOT meant
   to be byte-identical (so the deviation is faithful to the design, not a dodge).
2. The gate actually enforces a meaningful invariant: it FAILS if the shared prose diverges
   between a pair (mutate one pair's shared line and show it fails / would fail), and does
   not false-pass (strips exactly the Astro/link/site-only deltas and nothing load-bearing).
3. The ordering held: the gate commit (`7b8b9d9`) is an ancestor of / earlier than the prose
   commit (`6879633`), and the gate was green before the edits (the implementer says it
   reconciled one genuine drift — site cli.mdx "v0.2" → "v0.1", matching `src/cli/demo.rs`).
4. The CI wiring is right: the `docs-mirror` job, the path filters on BOTH push and
   pull_request so a mirror-only push still triggers CI, and the job fails loudly on drift.

## The prose (all four mirrors)

Verify HTTP-as-default-for-a-machine-with-multiple-independent-clients is documented with
the correct framing (single-writer reason, explicitly NOT "subagents need HTTP" — one
orchestrator + subagents is one connection fine on stdio), plus the config-layering
migration gotcha (transport touches every layer; a stale `command` beside a new `url` is
rejected). Confirm the four files carry matching canonical shared prose (the gate is green).

## The decision note
`--print-client-config` documented as decided-not-built — verify the note in
`dev-diary/lambo-for-mooshik/J-multi-client.md` (commit `a778235`) states why (a paste-ready
emitter needs DOGFOOD-rig operator paths + the binary lacks the resolved config path; a
placeholder template would be a half-verb), and that this is a reasonable, honest disposition
for a "consider" item.

## Gates (spot-check, Claimed/Measured)
The new mirror-drift check (green before + after); fixtures 902; sqlite 973; cockroach 559;
verify.sh 46 ok; fmt; clippy x4. Confirm no Rust source was touched (the diff is
docs/CI/script only).

## Output + verdict
Append a disposition to `dev-diary/adversarial-review/adve-review-mooshik-J5-round1.md`
(worktree). Verdict APPROVE (clean, ready to integrate) or REQUEST_CHANGES with new graded
findings. Report per-check verdicts, the deviation adjudication (faithful to the design
record? gate meaningful and false-pass-free?), the gate table, HEAD == origin/wt/j5, tree
clean. Leave the worktree clean apart from your review doc; do NOT commit.
