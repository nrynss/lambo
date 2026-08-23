# J5 — transport defaults + config layering docs; mirror-drift CI gate; client-config emitter

Small workstream. Work in the worktree `/home/nryn/work/lambo/.claude/worktrees/j5` on branch
`wt/j5` at `0dd15b3` (= origin/lambo-for-mooshik, which now includes J4). Do NOT integrate
into lambo-for-mooshik. Commit conventionally on `wt/j5` and push.

## Authority
`dev-diary/lambo-for-mooshik/J-multi-client.md` §J5 (lines ~2545-2566):
* document HTTP as the default for any machine running MORE THAN ONE CLIENT;
* a transport migration touches every config layer — document the gotcha;
* consider `lambo serve --print-client-config <client>` so migration is a copy, not a hand-edit;
* **catch:** the `--ledger`/transport prose lives in FOUR hand-maintained mirrors —
  `docs/reference/cli.mdx`, `docs/reference/mcp.mdx`, `site/src/content/docs/cli.mdx`,
  `site/src/content/docs/mcp.mdx` — kept as **byte-identical pairs** with NO drift gate.
  **Add a one-line `diff` of each pair to CI BEFORE editing them**, so the edit lands
  against a gate instead of installing the first drift.

## Scope (in this order — the order is load-bearing)

1. **CI drift gate FIRST.** Add a check to `.github/workflows/ci.yml` that fails if either
   mirror pair diverges (byte compare `docs/reference/cli.mdx` vs
   `site/src/content/docs/cli.mdx`, and the same for `mcp.mdx`). Land this gate, confirm it
   is GREEN on the current (byte-identical) mirrors, and keep it green after you edit the
   mirrored content (the pairs must stay byte-identical). This gate is the point of J5's
   catch. Note in the commit/CI comment that it guards the two pairs (added before the
   edits, per §J5).

2. **Document, across all four mirrors (byte-identically):** HTTP as the default transport
   **for a machine running more than one independent client/session** — with the precise
   framing (the reason is single-writer, not subagents): a single orchestrator + its
   subagents is ONE connection and fine on stdio; the moment a machine runs multiple
   distinct clients (e.g. OMP + Claude Code + Pi), point them all at one HTTP serve so they
   do not spawn competing stdio writers. Also document the **config-layering gotcha**: a
   transport migration touches EVERY config layer (project `.mcp.json`, user/global scope,
   per-client files like Pi's `~/.pi/agent/mcp.json`, Cursor's `~/.cursor/mcp.json`, …), and
   a stale `command` entry beside a new `url` is rejected by clients — so migration is a
   copy across layers, not one file. Mirror both mdx pairs byte-for-byte.

3. **`lambo serve --print-client-config <client>`** — the §J5 "consider" verb. Implement it
   only if it is clean and small (emit the MCP client registration block for the named
   client — OMP / Claude Code / Cursor / Pi / Codex — matching the registration shapes in
   `dev-diary/lambo-for-mooshik/DOGFOOD-SETUP.md` §4 so migration is a copy). If it needs a
   config/registry the branch does not have, document it as a decided-but-not-built
   consideration instead of scaffolding a half verb. Do not invent a GUI; a plain manifest
   emit is enough.

## Gates (Claimed/Measured, at minimum)
- The NEW mirror-drift check (both pairs byte-identical, pre- and post-edit) — this is the
  J5-specific gate.
- `cargo test --all --features fixtures`; `cargo test --features
  store-sqlite,embed-fixture,fixtures`; `cargo test --no-default-features --features
  store-cockroach`; `bash scripts/observability/verify.sh` (46 ok); `cargo fmt --all -- --check`;
  clippy x4. Confirm the mdx/doc edits changed no Rust behaviour.

## Report back
The CI gate line(s) + that it was green before the edits and stayed green after; the four
mirrors byte-identical (diff output = none); whether you implemented `--print-client-config`
or documented it (and why); the gate table; commit hashes; push confirmation; final clean
git status in the worktree.
