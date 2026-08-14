# End-to-end: how the pieces compose

> Partial: the `serve` + MCP path below is shippable today. The CLI walkthrough
> and the two-agent demo depend on T8.3 / T8.4 and are marked pending.

## The model

- A session has **exactly one writer** — a `lambo serve` process holding the
  T8.6 lease. Readers (CLI read verbs, the CockroachDB MCP server) go straight to
  the store and never open a writer.
- Writes flow through one `Memory`: RAM graph → write-behind log → periodic flush
  to the durable store. Durability is bounded by `backend_flush_interval`, not
  immediate; `lambo_stats` reports the current lag.
- Canonization runs in the background, promoting eligible concepts to Canonical
  ("saints") through the audited transition path.

## Serve a session and drive it over MCP (shippable today)

1. **Provision** the durable store (once): `scripts/provision.sh` (creates the
   graph schema + the `session_leases` table).
2. **Serve** as the single writer:
   ```bash
   lambo serve --config lambo.toml --session demo --agent agent-a --transport stdio
   ```
   (or `--transport http` on loopback — see the T8.7 caveat in [config.md](config.md)).
3. **Drive it** from any MCP client. The agent's loop:
   `lambo_recall` (load prior memory) → act → `lambo_derive` /
   `lambo_record_action` (write what it learned/did) → `lambo_reserve` before
   editing a shared concept. See [mcp.md](mcp.md).
4. **Shutdown** (SIGINT/SIGTERM or clean disconnect) flushes the tail durably and
   releases the lease.

A second `lambo serve` on the same session is **refused** (lease held) — it does
not become a silent second writer.

## The swarm topology (why the CLI matters — pending T8.3)

One `serve` writer per session; many small agents each shell out a single
deterministic `lambo derive …` / `lambo record-action …` line (zero tool-schema
tokens in their context). Canonization collapses the duplicate observations many
agents produce into single canonical nodes; `lambo reserve` coordinates edits so
two agents don't clobber the same concept. This is documented fully once T8.3
lands the write verbs.

## Two-agent demo (pending T8.4)

The scripted spec §13 scenario — Agent A derives, canonization fills, Agent B
recalls with the blast-radius ⚑ warning and does *not* make the breaking change —
will be documented here when T8.4 lands, with the demo app view (T8.5).

See also: [mcp.md](mcp.md), [cli.md](cli.md), [api.md](api.md), [config.md](config.md).
