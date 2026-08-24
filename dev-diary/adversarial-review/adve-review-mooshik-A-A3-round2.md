# Adversarial review: mooshik A (Gemini/Vertex embedder adapter), round 2

**Reviewer**: independent adversarial reviewer, agent_id `A3Review2`. Wrote nothing under
review except this file.
**Scope**: round-2 re-review of the uncommitted A3 implementation in worktree
`/home/nryn/work/lambo-wt-a3` (branch `a3-gemini-adapter`), focused on remediation of the
two round-1 findings (from `adve-review-mooshik-A-A3-round1.md`): A3-R1-1 (P1,
`outputDimensionality` omitted from the embed body) and A3-R1-3 (P3, no body-pinning test).
Reviewed strictly read-only against the current spec sections A3/A4 of
`lambo-for-mooshik/A-gemini-embedder.md`, the round-1 review, and the updated implementation
record (`a-run/A3-implementation.md`).
**Verdict**: **APPROVE** with zero residue. Both round-1 findings are CLOSED and mutation-
checked; no new findings.

## A3-R1-1 (P1) CLOSED

The embed request body now carries the configured width. `request_embedding`
(`src/embed/gemini.rs:303-309`) sends:

```json
{ "content": { "content": text }, "outputDimensionality": self.dim }
```

`outputDimensionality` is set from `self.dim` (a `usize`, serialized as an exact JSON
number), satisfying A3's `outputDimensionality from cfg.dim` requirement and A4's
"truncates to 768, 1536 or 3072" model. The misleading module doc-comment was corrected:
`gemini.rs:12-16` now states "**outputDimensionality IS sent from the configured `dim`**",
that `gemini-embedding-001` truncates to 768/1536/3072 via that parameter, and that the
adapter sends it as-is while failing `width == dim` with `Backend` if Vertex disagrees. The
in-code comment at `gemini.rs:305-308` and the module doc both record that A4 owns the
construction guard rejecting a `dim` outside {768, 1536, 3072} before an unsupported value
is sent.

The implementation record was updated to match: `A3-implementation.md` now reads
"`outputDimensionality` IS sent from the configured `dim` (A3-R1-1, corrects the initial
omission)" and explicitly notes "A4 owns the construction guard that rejects any other `dim`
before an unsupported value is sent". Source, module doc, and record are all consistent.

**A4 construction guard adjudication**: the A4 scope (the construction-time validation of
`dim` against {768, 1536, 3072}) is correctly left to the next phase, A4. It is not an A3
requirement; deferring it is proper since A3's ask is that the adapter honour the configured
`dim` at request time. Not demanding it in A3.

## A3-R1-3 (P3) CLOSED

`embeds_and_normalizes` (`src/embed/gemini.rs:553-575`) now pins the request body. The mock
adds `.body_contains("\"outputDimensionality\":768")` (`gemini.rs:559-561`), and the
embedder is constructed with `dim = 768` (`test_embedder(&server, 768)`, `gemini.rs:566`), so
the mock only matches when the body actually contains `"outputDimensionality":768`.

**Mutation check**: removing `outputDimensionality` from the sent body yields
`{"content":{"content":"user schema"}}`, which no longer contains the required substring. The
mock then does not match the POST to `/embedContent`; httpmock answers 404 for an unmatched
request, so `embed` takes the non-2xx -> `Backend` path and `e.embed(...).unwrap()` panics.
The test fails on the omission. Confirmed mutation-sensitive.

**Robustness of the assertion**: byte-level substring match is key-order agnostic, so the
serde field-ordering of the JSON cannot mask a wrong body, and it simultaneously pins
presence AND value (a wrong dimension such as 1536 would not contain `"outputDimensionality":768`
and would fail too). The value is an exact JSON number, not a quoted string, matching how
`usize` serializes. Solid.

## No new findings

Round-1's adversarial verification of everything else was re-confirmed unchanged and clean:
the JWT mint/verify, OAuth exchange and TTL cache, `embeds_and_normalizes` URL/auth header,
serde response parse, `width == dim` check, L2 normalization with non-finite/zero-norm
rejection, CON-7 empty/whitespace before network, CON-2 no-retry, error classification,
`as_any`/`model_identity`, resolve.rs stamping, `is_ready` feature gating, credentials
resolution, Cargo/feature gating, and the A1-A1-1 supersession are all sound. No em dashes
appear in `gemini.rs` or the implementation record (both score 0).

## Gates (my own runs, worktree `/home/nryn/work/lambo-wt-a3`)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | pass |
| `cargo test --features embed-gemini,embed-bge,embed-fixture --lib gemini` | 21 passed / 0 failed |

All three gates pass. The gemini-scoped test suite (the 13 gemini adapter tests plus the 8
gemini registry/overlay/toml tests) runs green, including the remediated
`embeds_and_normalizes`.

## Conclusion

A3-R1-1 (P1) and A3-R1-3 (P3) are both CLOSED with zero residue. The body-send fix is real
and documented, the body-pinning test is mutation-sensitive and robust, and the A4
construction guard is correctly left to its own phase. No new findings.

- A3Review2, 2026-08-24
