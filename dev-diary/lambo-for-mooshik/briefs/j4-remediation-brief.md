# J4 remediation — close J4-R1-1 and J4-R1-2 (both P3, review APPROVE)

Work in the J4 worktree `/home/nryn/work/lambo/.claude/worktrees/j4` on branch `wt/j4` at
`b58ef91` (= origin/wt/j4). Do NOT integrate. The review verdict is APPROVE with two P3
findings; close them, re-gate, commit, push, so the next review (or integration) sees a clean
tree.

## J4-R1-1 (P3) — a proxy-degrading loser records no store refusal, but §J4 says it does

Authority: `adve-review-mooshik-J4-round1.md` (in the worktree) finding 1.
`J-multi-client.md:2603-2604` states a refused loser "still records its store refusal even
when it degrades to a proxy", but `record_refused_loser` runs only at the four terminal
refusal exits (`src/mcp/serve.rs` ~1061,1077,1114,1140); the **Proxy branch**
(`src/mcp/serve.rs:1091-1100`) returns `Ok(Role::Proxy)` without calling it, so no
`lease_refusals` row and no `kind:lease event:refused side:loser` line is produced there —
the incumbent's poller never learns it was contended, and "both sides" has no runner-up side
on the stdio loser-to-proxy path.

**Preferred fix:** wire `record_refused_loser` into the proxy branch so a loser that degrades
to a proxy still records its refusal (store row + `side:loser` line) and the accepted
acquisition is observable from the incumbent side too — this makes the J4 "both sides"
guarantee genuinely hold on the proxy path and matches the as-built doc's claim. Reuse the
same helper the terminal-refusal exits use. If you judge that wrong (e.g. the contended
acquisition should be silent when the loser has a follow-on proxy line), then instead
CORRECT the doc note at `J-multi-client.md:2603-2604` to state exactly what the code does
and why — but prefer wiring it, because "the incumbent learns it was contended" is the point
of J4's metric-6 / why-no-memory story. Deviate only with a written argument.

Add/keep a test that proves the proxy-degrading loser's refusal reaches the ledger/store on
both sides (extend the existing both-sides test to the proxy path if that is the chosen fix).

## J4-R1-2 (P3) — use the real agent, not the literal "proxy", on proxying lines

`src/mcp/proxy.rs:1178` and `:1433` pass the literal `"proxy"` as the `agent` for the
`proxying` and `proxying_stopped` lease lines, because `HubProxy::new` (`proxy.rs:880-910`)
is not given its own agent. **Fix:** thread `opts.agent` into `HubProxy::new` and use the real
agent id for both lines, matching every other J4 line. Add/keep a test asserting the proxying
line carries the real agent.

## Gate and ship

Re-run and report Claimed/Measured for the affected gates: `cargo test --features
store-sqlite,embed-fixture,fixtures`; `cargo test --all --features fixtures`; `cargo test
--no-default-features --features store-cockroach` (compile+unit; report the env-blocked live
part separately — no DSN on this machine); `bash scripts/observability/verify.sh` (must stay
46 ok, sample byte-identical); `cargo fmt --all -- --check`; clippy x4. Update the review
doc's disposition if you change it (mark J4-R1-1/2 closed with the commit that closes them).
Commit conventionally on `wt/j4` as a logical unit and `git push origin wt/j4`. Confirm the
hash. Do NOT integrate into lambo-for-mooshik.

## Report back
Per-finding fix (file:line) + whether you wired the refusal or corrected the doc (and why);
the test evidence (proxy-path both-sides, real-agent line); the gate table (Claimed/Measured);
commit hash + push confirmation; final clean git status in the worktree.
