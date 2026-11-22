# Issue drafts from the `mcp.mdx` claims audit

Draft 1 is filed as #338. Draft 2 was posted as a comment on #332, the rmcp
bump. Draft 3 was fixed on the branch instead. Line
numbers in `docs/reference/mcp.mdx` are at commit `8cede2ae`. See
[`mcp.md`](mcp.md) for the full audit.

## 1. An `agent_id` with a control character gets the generic refusal, which says tab and newline are allowed (filed: #338)

**Kind:** code bug, or intent unclear (see below).

**Doc sentence** (Errors table, line 134, unchanged by the audit):

> An `agent_id` that is not a single line | That the id must be one line with
> no tabs, control or line-separator characters, and the codepoint found.

**What the code does.** `LamboServer::check_agent_id`
(`src/mcp/server/params.rs`) runs `check_size("agent_id", ...)` before its own
one-line check (`breaks_one_line`). `check_size` refuses every control
character except tab and newline, with the generic text that says tab and
newline are the only control characters allowed. So for `\r`, NUL, ESC and
the other C0 and C1 controls, the caller sees a message that is false for
`agent_id`: tab and newline are refused there too. Only tab, newline and
`U+2028`/`U+2029` reach the one-line message. An invisible formatting
character gets `check_size`'s invisible-character message, which is true for
`agent_id` (it is not a line break, and the Errors table's row for a string
carrying an invisible character covers it), so only the control characters are
affected. An id over 16,384 bytes gets the byte-cap message, not the
256-character one. The comment in `check_agent_id` says the other controls are
"already refused upstream by `check_size`", so the order looks deliberate for
the refusal, but not for the message.

**Reproduction.**

```json
{"name": "lambo_recall", "arguments": {"agent_id": "agent\ra", "query": "x"}}
```

The error names `U+000D` and says tab and newline are allowed. Test
`every_tool_refuses_an_unusable_agent_id` (`src/mcp/server/tests/errors.rs`)
asserts only that the error mentions `agent_id`.

**Suggested fix.** In `check_agent_id`, run the one-line check and the
256-character cap before `check_size`, so every id the one-line rule refuses
gets the one-line message. Extend `every_tool_refuses_an_unusable_agent_id` to
assert the message for `\r`, NUL and `U+2028`. If the generic message is
intended, change the Errors row instead to "the offending codepoint, with the
generic or the one-line message".

## 2. A wrongly typed argument's error names no field and can quote the value back (posted on #332)

**Fold into #332** (the rmcp bump) rather than filing on its own.

**Kind:** intent unclear. The page now describes the current behaviour.

**Doc sentences** (before the audit):

> A missing, unknown, or malformed argument | The parameter error naming the
> field, before the tool runs. (line 129)

> Neither the base64, the bytes nor a vector value appears in an error, a
> receipt or the `--ledger` file. (line 552)

**What the code does.** Argument extraction happens in rmcp 3.1.2, through a
plain `serde_json::from_value`, before any Lambo code runs. A missing or
unknown field is named by serde. A value of the wrong type is not: the error is
"invalid type: string ..., expected u32" with no field path. For a wrongly
typed slot that holds caller data, such as `image` sent as a string or a string
inside `vector.values`, serde's message quotes the value. `params.rs` (around
lines 483-492) records this as an accepted residual until rmcp offers an
extraction-error hook. The echo goes only to the caller, never to a receipt or
the ledger. rmcp returns the error as an `isError` tool result, not a JSON-RPC
error (`into_tool_argument_error`). This finding rests on reading rmcp's
source; the audit did not run it.

**Suggested fix.** Decide whether the residual is acceptable. If not, wrap
argument deserialization with `serde_path_to_error` (or an rmcp hook when one
exists) so the error names the field path, and replace serde's message with one
that names the expected type without quoting the value. Add a test that sends
`image` as a string and asserts the field is named and the value is absent.

## 3. Rust doc comments repeat claims the page has dropped

**Not filed: fixed on branch `docs/mcp-claims-audit`.** Three doc comments
stated what the old page stated, and the code does otherwise:
`ReceiptAnswer::Dropped` (`src/writeq/receipts.rs`), `companion_of` on the
recall detail (`src/recall/detail.rs`) and `LamboError::EmbedUnavailable`
(`src/types/mod.rs`). They now match the code and the page. The same commit
fixes a fourth, the `check_agent_id` comment (`src/mcp/server/params.rs`),
which said one bearer token authenticates the server (T-57). Comments only;
`scripts/formal/check-model-drift.py` reports no drift.
