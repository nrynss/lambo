# Command line

Lambo has command line verbs that mirror the MCP tools, so you can read and write session memory from the terminal without an MCP client. The command line is the primary surface for large swarms of small agents, because each call is one deterministic line with no tool schema overhead.

The command line is in active development. Run `lambo <command> --help` for the current, authoritative flags on your build. The verbs and their arguments are described below.

## Read commands

Read commands query the store directly and never open a writer, so they are safe to run against a session another process owns.

| Command | What it does |
|---|---|
| `lambo recall --session <id> --query <text> --top-k <n>` | Recall the context block for a query. |
| `lambo saints --session <id>` | List the session's canonical memories. |
| `lambo inspect --session <id> --focus <text> --depth <n>` | Inspect the neighbourhood around a concept. |
| `lambo stats --session <id>` | Report session health. |
| `lambo provision` | Apply the durable store schema. |

## Write commands

Write commands acquire the session's writer lease. If a `lambo serve` process already owns the session, the write is refused and the command tells you who holds it, so a command line write never becomes a silent second writer. The arguments match the MCP tools, including the same limits.

| Command | What it does |
|---|---|
| `lambo derive --session <id> --agent <id> --content <text> --kind <type>` | Store a concept. Repeat `--parent-of child:parent` for hierarchy links. |
| `lambo record-action --session <id> --agent <id> --action <text>` | Record an action. Repeat `--produces`, `--modifies`, and `--depends-on` for targets. |
| `lambo reserve --session <id> --agent <id> --node <uuid>` | Take a soft lock on a node. |
| `lambo release --session <id> --agent <id> --node <uuid>` | Release a soft lock. |

For the meaning of each argument, the value limits, and the concept types, see [MCP tools](mcp.md). Both surfaces run the same underlying operations. See [Library API](api.md) for the type they share.
