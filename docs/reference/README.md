# Lambo reference documentation

Per-surface reference for Lambo. Written against the shipped binary, not the
spec. (T8.8 — the README/getting-started onboarding lives separately in T9.1 and
links here.)

| Page | Surface | Status |
|---|---|---|
| [mcp.md](mcp.md) | The seven MCP tools — args, caps, errors, transports | ✅ complete (T8.2) |
| [api.md](api.md) | `Memory` API, `GraphStore`/`Embedder` adapters, the lease | ✅ complete (T8.1/T8.6) |
| [config.md](config.md) | `lambo.toml`, features, provisioning | ✅ complete (T8.7 HTTP hardening pending) |
| [cli.md](cli.md) | CLI read + write verbs | ⏳ pending T8.3 |
| [end-to-end.md](end-to-end.md) | How the pieces compose | ◑ serve+MCP done; CLI/demo pending T8.3/T8.4 |

> Coverage note: this set documents the surfaces frozen so far (T8.1 `Memory`,
> T8.2 MCP, T8.6 lease). The CLI and the full end-to-end walkthrough complete
> once T8.3/T8.4 land; T8.8's verification pass (help-text ↔ reference
> consistency, `cargo doc` warning-free) runs after those tasks.
