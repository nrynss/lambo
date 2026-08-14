# CLI reference

> **Status: pending T8.3.** The CLI read verbs and the write verbs (at MCP
> parity, lease-held) are being built under T8.3. This page is a placeholder so
> the reference set is complete; it will be filled against the shipped binary
> when T8.3 lands. Until then, `lambo <cmd> --help` is authoritative.

Planned surface (spec §6.2 + the T8.3 write-parity decision):

**Read verbs** (reader processes — never open a writer on an owned session):
- `lambo recall --session --query --top-k`
- `lambo saints --session`
- `lambo inspect --session --focus --depth`
- `lambo stats --session`
- `lambo provision` (wraps `scripts/provision.sh`)

**Write verbs** (acquire the T8.6 lease; fail closed naming the holder if a
`serve` owns the session) — argument semantics mirror the MCP tools 1:1
([mcp.md](mcp.md)), same caps and validation:
- `lambo derive --session --agent --content --kind [--parent-of CHILD:PARENT ...]`
- `lambo record-action --session --agent --action [--produces N ...] [--modifies N ...] [--depends-on N ...]`
- `lambo reserve --session --agent --node` / `lambo release --session --agent --node`

The CLI is intended as the **primary agent surface for the swarm case**
(deterministic, zero tool-schema tokens); MCP is the compatibility surface. For
verb semantics, caps, and error behavior today, see [mcp.md](mcp.md) — the two
surfaces are thin adapters over the same `Memory` ([api.md](api.md)).
