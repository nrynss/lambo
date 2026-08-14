# End to end

This page shows how the parts fit together, from serving a session to driving it with agents.

## The model

A session has exactly one writer, which is a `lambo serve` process that holds the session's lease. Readers, such as dashboards or the CockroachDB managed MCP server, query the store directly and never open a writer.

Writes flow through one in-memory graph. They land in a write-behind log and flush to the durable store on an interval, so durability is eventual, not immediate. Call `lambo_stats` to see the current lag.

Canonization runs in the background. It promotes concepts to canonical facts when they earn it from structural evidence, not when an agent declares them important.

## Serve a session and drive it over MCP

1. Provision the durable store once with `scripts/provision.sh`. You can skip this if you use the in-memory store.
2. Start the writer.

   ```bash
   lambo serve --config lambo.toml --session demo --agent agent-a
   ```

   Add `--transport http` to serve over HTTP on localhost instead of stdio.
3. Connect your agents as MCP clients. A typical agent loop calls `lambo_recall` to load prior memory, acts, then calls `lambo_derive` and `lambo_record_action` to write what it learned and did. It calls `lambo_reserve` before editing a shared concept.
4. Stop the server with `Ctrl-C` or a clean disconnect. Lambo flushes the pending tail and releases the lease.

A second `lambo serve` on the same session is refused while the first holds the lease, so it never becomes a silent second writer.

## Run a swarm

Run one `lambo serve` writer for the session, then have many small agents each write with a single command line call, such as `lambo derive`. Canonization collapses the duplicate observations that many agents produce into single canonical facts. `lambo reserve` keeps two agents from editing the same concept at once.

Because each agent call is one deterministic line, a small local model can drive it reliably, and no tool schema takes up the model's context.

See [Installation](installation.md), [MCP tools](mcp.md), [Command line](cli.md), [Library API](api.md), and [Configuration](config.md).
