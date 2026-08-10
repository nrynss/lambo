# Lambo

Agentic memory for multi-agent coding: a bipartite interaction/concept graph with
write-behind durability (CockroachDB), background scoring/GC, and **canonization**
(importance earned from structural evidence, not declared at write time).

## Deployment model (single writer)

**One `lambo serve` process owns a session.** Agents connect over MCP (stdio or HTTP)
and share that process's in-memory graph. Mutations flush asynchronously to the durable
store. Any number of **readers** may query the store directly (dashboards, CockroachDB
managed MCP) and see eventually consistent state. Readers never write. Multi-writer
coordination is out of scope for v0.1.

## Status

Hackathon build (CockroachDB × AWS). Spec: [`lambo-hackathon-spec-v0.1.md`](lambo-hackathon-spec-v0.1.md).
Phase handoffs: [`dev-diary/`](dev-diary/).

```bash
# prerequisites: Rust stable, .env with LAMBO_COCKROACH_DSN (see .env.example)
cargo run -- --help
cargo test
```

## License

MIT — see [LICENSE](LICENSE).

Derived from design work credited as Lambo v0.6.0 (prior design, not incorporated code).
