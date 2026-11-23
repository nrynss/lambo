# Claims audits

Each file in this directory audits one documentation page against the Lean
models in `formal/LamboProofs/`, the Rust in `src/`, and the tests. An audit
lists every normative claim the page makes, says what backs each one, and
records what was done about the claims that turned out to be wrong.

| Audit | Page | Issue drafts |
|---|---|---|
| [`mcp.md`](mcp.md) | `docs/reference/mcp.mdx` and its mirror `site/src/content/docs/mcp.mdx` | [`mcp-issue-drafts.md`](mcp-issue-drafts.md) |

## What a claim is

A claim is a sentence that promises behaviour an agent or operator could rely
on: an order, a limit, a default or a count; when something is refused or
accepted; what an error means; what a field means; anything timed ("after",
"before", "once", "within"); and anything with always, never, must, only, at
most, exactly or guarantees. Navigation is not a claim. An example is a claim
only when it asserts behaviour, such as the text of an error.

## How to read the classes

Each row has exactly one class. A sentence whose parts have different backing
is split into rows (`D-64a`, `D-64b`).

| Class | Meaning | How much to trust it |
|---|---|---|
| PROVEN | A theorem's statement entails the claim. The row names the theorem and its model file. | As far as the model matches the Rust. The notes say what the model leaves out (exact arithmetic in place of `f64`, no concurrency, no clock). `scripts/formal/check-model-drift.py` says whether the Rust has changed since the model was last read. |
| TESTED | A Rust test asserts the claim. The row names the test. | For the cases the test covers. |
| CODE-ONLY | The code does what the claim says, but no theorem or test pins it. The row cites `file:item`. | Until the code changes. Nothing fails if it drifts. |
| COMMENT-ONLY | Only a doc or module comment says so, and the check against the code was not finished. The row cites the comment. | Not at all: a comment is not code. The row needs checking against the code before anyone relies on it. |
| CONTRADICTED | A theorem or the code says otherwise. | The audit lists the resolution: the page was fixed, or an issue was drafted. |
| NOT CHECKABLE | The claim is about quality, statistics, history, or another program's behaviour. | Not at all, from the code. |

A related theorem is not enough for PROVEN. If a theorem proves something
close to the claim but not the claim itself, the row is classed by its next
best backing, and the notes say what theorem would close the gap.

## When a claim is contradicted

- If the code (and the model, where one exists) agree and the page is wrong,
  the page is fixed.
- If the page states the intended behaviour (per a model, or an owner
  decision recorded in an issue) and the code differs, the code has a bug. The page is left as it is and an issue is drafted.
- If it is not clear which is intended, an issue is drafted that says so.

Issue drafts are not filed by the audit. They sit beside the audit for the
owner to file.

## Anchors keep the page true

An audit is a record of one reading. Once it is done, the page carries each
checked claim's backing as invisible MDX anchors (`lean:`, `test:`, `code:`,
`const:`) beside the sentence it backs, and
`scripts/docs/check-doc-anchors.py` resolves them in CI: a renamed test, a
removed theorem or a deleted item fails the docs job. The audit file records
what was found and fixed; the anchors keep it true afterwards. `mcp.mdx`, in
both copies, is anchored this way.

## Line numbers

Each audit names the commit it read. Line numbers refer to the page at that
commit, not to the page after the audit's fixes. A `section` column names the
heading each claim sits under in the current page, so a writer can find it
there.
