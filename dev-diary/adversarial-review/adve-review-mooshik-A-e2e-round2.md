# Adversarial review - mooshik A (Gemini embedder) end-to-end composition, round 2 (verification of the round-1 remediation)

**Reviewer**: independent E2E round-2 reviewer, agent_id `E2ER2`. STRICT READ-ONLY of source; the
only file written is this one.
**Worktree**: `/home/nryn/work/lambo`, branch `lambo-for-mooshik`, working tree with the round-1
remediation uncommitted (verified `git status --short` before starting: `M
.github/workflows/ci.yml`, `M src/embed/gemini.rs`, `M src/embed/mod.rs`, `M src/resolve.rs`,
plus the untracked round-1 review file; nothing else). No source edits were made; no mutations
were applied to the tree. Any mutation reasoning below is by source trace across the closure,
not by editing files.
**Scope**: confirm the three round-1 E2E findings (P1-A-E2E-1, P2-A-E2E-2, P3-A-E2E-3) are CLOSED
with zero residue, by reading the actual remediation in the working tree and re-running the
gate matrix the round-1 review used.

**Verdict**: **APPROVE** with zero residue. All three round-1 E2E findings are closed by the
working-tree remediation and verified at both the source and the gate level. No new findings.
Every gate the round-1 review ran stays green (all counts below), and the two completions the
remediation adds (the CI row, the resolve-level stamp pin) are exactly where the round-1 review
said they were missing.

---

## Round-1 findings, status

| # | Severity | Status | Verification (this round) |
|---|---|---|---|
| P1-A-E2E-1 | P1 | **closed** | `gemini` row present in the `feature-matrix` job of `.github/workflows/ci.yml`; YAML parses clean (`yaml.safe_load` via python3); row runs `cargo clippy --all-targets --features embed-gemini -- -D warnings` then `cargo test --features embed-gemini`, no network, no service-account key. |
| P2-A-E2E-2 | P2 | **closed** | `src/embed/gemini.rs` gains `gemini_live_embeds_against_vertex`, `#[tokio::test]` + `#[ignore]`, creds resolved `LAMBO_GEMINI_CREDENTIALS` else `GOOGLE_APPLICATION_CREDENTIALS`, clean skip when absent, real OAuth + `embedContent` round-trip, asserts width and unit norm. Compiles (`--no-run`) and is truly ignored. |
| P3-A-E2E-3 | P3 | **closed** | `src/resolve.rs` gains `resolve_gemini_stamps_embedding_model` asserting `EmbeddingContract.model == Some("gemini-embedding-001")`; `pub(crate) mod gemini` in `src/embed/mod.rs` makes the test key reachable. Passes. |

---

## P1-A-E2E-1 (P1): embed-gemini CI row. CLOSED.

`.github/workflows/ci.yml` `feature-matrix` matrix `include` now lists a `gemini` row
(immediately after the `candle` row, mirroring its pattern and rationale comment):

```yaml
- name: gemini
  command: |
    set -o pipefail
    cargo clippy --all-targets --features embed-gemini -- -D warnings
    cargo test --features embed-gemini
```

Verified:
- `python3 -c 'import yaml; d=yaml.safe_load(open(".github/workflows/ci.yml"))'` succeeds; the
  row is present in `jobs.feature-matrix.strategy.matrix.include` (rows listed in order: sqlite,
  sqlite-minimal, sqlite-vectors, minimal, cockroach, postgres, candle, gemini, demo,
  ship-fixtures). YAML is well-formed.
- The row is fully offline: `cargo clippy` + `cargo test` against the local feature set only
  (httpmock-based unit tests, no network, no API key). This matches the `candle` row convention.
- It lints with `--all-targets` and `-D warnings`, so it enforces clippy-cleanliness on the new
  security-sensitive HTTP + OAuth + RS256 surface that neither `check` (default features) nor
  `ship`/`demo` ever compiles.

Mutation check (by trace, no edit): `embed-gemini` is optional and NOT in `default`, `ship`, or
`demo`; `check` lints default features only. Removing this row leaves the Gemini adapter never
compiled, linted, or unit-tested anywhere in CI. The row is the sole catching surface for
gemini compile + test; removing it regresses P1-A-E2E-1 in full. It is correctly placed and
load-bearing.

## P2-A-E2E-2 (P2): live ignored Vertex test. CLOSED.

`src/embed/gemini.rs` (end of `mod tests`) gains:

```rust
#[tokio::test]
#[ignore]
async fn gemini_live_embeds_against_vertex() {
    let creds_path = std::env::var_os("LAMBO_GEMINI_CREDENTIALS")
        .or_else(|| std::env::var_os("GOOGLE_APPLICATION_CREDENTIALS"));
    let Some(creds_path) = creds_path else {
        eprintln!("skipping: set LAMBO_GEMINI_CREDENTIALS or GOOGLE_APPLICATION_CREDENTIALS");
        return;
    };
    ...
    let token_source = Box::new(ServiceAccountTokenSource::new(creds, client.clone()).unwrap());
    let embed_url = GeminiEmbedder::vertex_embed_url(&project, &location, &model);
    let e = GeminiEmbedder::new(model, dim, token_source, embed_url, client).unwrap();
    let v = e.embed("lambo live vertex round-trip").await.unwrap();
    assert_eq!(v.len(), dim, "live Vertex returned the configured width");
    let norm: f32 = v.iter().map(|x| x * x).sum::<f32>().sqrt();
    assert!((norm - 1.0).abs() < 1e-3, "live Vertex vector must be L2-normalized, norm={norm}");
}
```

Verified:
- `#[tokio::test]` and `#[ignore]` are both present on the function.
- Credentials resolve `LAMBO_GEMINI_CREDENTIALS` first, falling back to
  `GOOGLE_APPLICATION_CREDENTIALS` via `Option::or_else`.
- It skips cleanly when absent: the `let Some(...) else { eprintln!; return }` short-circuits
  before any credential load, so running it without creds exits `ok` (demonstrated below),
  not panic.
- It does a real OAuth + `embedContent` round-trip: builds the `ServiceAccountTokenSource`
  (RS256 JWT mint + token exchange) and calls `e.embed(...)` against the real Vertex URL.
- It asserts both width (`v.len() == dim`) and L2 unit norm (`|norm - 1| < 1e-3`), defaulting
  dim to 1536 (within the adapter's supported set) for the live probe.
- It honors the established `#[ignore]`d live-test convention (parallel to `bge_m3` and `candle`
  live tests).

Compiles and is truly ignored (live evidence):
- `cargo test --features embed-gemini --lib --no-run` succeeds (unit-test binary builds; the live
  test compiles).
- In the normal `--features embed-gemini,embed-bge,embed-fixture --lib` run the suite reports
  `2 ignored` (round-1 baseline was `1`), i.e. the new live test is counted and NOT executed in
  the default pass.
- Explicit `cargo test --features embed-gemini --lib embed::gemini::tests::gemini_live_embeds_against_vertex -- --ignored`
  with both env vars unset: `test ... ok`, `1 passed`, message "skipping: set
  LAMBO_GEMINI_CREDENTIALS or GOOGLE_APPLICATION_CREDENTIALS", exit 0. The skip path works.

Mutation check (by trace, no edit): removing this test deletes the only operator-runnable proof
of the real OAuth exchange and `embedContent` round-trip; that regresses the completeness half
of "Done when" item (e). Ungating (stripping `#[ignore]`) would make it run on every CI push
where, absent a key, it now returns early (harmless but noise) rather than failing; keeping it
ignored is the designed behavior. Breaking the skip path (removing the `let Some(...) else
{ return }`, or `.unwrap()`ing the raw Option) turns an absent-credentials run into a panic,
which is exactly the regression the clean skip is there to prevent. The skip is intact and the
whole closure is correct.

## P3-A-E2E-3 (P3): resolve-level model stamping pin. CLOSED.

`src/embed/mod.rs` changes `mod gemini;` to `pub(crate) mod gemini;` (under
`#[cfg(feature = "embed-gemini")]`), making the test-only `pub(crate) const
TEST_RSA_PRIVATE_KEY_PEM` in `src/embed/gemini.rs` reachable from the resolve test module.

`src/resolve.rs` `mod tests` gains:

```rust
#[cfg(all(feature = "store-memory", feature = "embed-gemini"))]
#[test]
fn resolve_gemini_stamps_embedding_model() {
    use crate::embed::gemini::TEST_RSA_PRIVATE_KEY_PEM;
    ...
    let r = resolve_backends(file).unwrap();
    assert_eq!(r.embedding.kind, "gemini");
    assert_eq!(r.embedding.model.as_deref(), Some("gemini-embedding-001"),
        "the Gemini contract must carry the real model id, not NULL");
    ...
}
```

Verified:
- The test drives `resolve_backends` with a real gemini `EmbedderConfig` (memory store, dim
  1536, project/location/credentials set, a real SA JSON written to temp using the RSA test key)
  and asserts `embedding.kind == "gemini"` and `embedding.model == Some("gemini-embedding-001")`.
  It builds a real embedder through `resolve_backends`, not a hand-constructed contract, so it
  locks the whole seam.
- The key `TEST_RSA_PRIVATE_KEY_PEM` is `#[cfg(test)] pub(crate) const` in `gemini.rs`; the
  `pub(crate) mod gemini` visibility is what makes it importable, and the import is present
  (`use crate::embed::gemini::TEST_RSA_PRIVATE_KEY_PEM;`).
- Result: `cargo test --features store-memory,embed-gemini --lib resolve_gemini_stamps_embedding_model`
  → `1 passed; 0 failed`.

Mutation check (by trace, no edit): removing this test un-pins the gemini `resolve_backends`
stamping; nothing else at the resolve boundary would then enforce
`Some("gemini-embedding-001")` (the A3-side `gemini_feature_on_builds_adapter_from_credentials`
only asserts the downcast identity on the adapter, not the resulting `EmbeddingContract`). Candle
and fixture already have analogous resolve-level locks; removing the gemini one leaves gemini the
only adapter without it. The closure is load-bearing.

---

## Gate matrix re-run by me at the working tree (exact results)

| Gate | Result |
| --- | --- |
| `cargo fmt --all -- --check` | pass |
| `cargo clippy --all-targets --features embed-gemini -- -D warnings` | pass |
| `cargo test --features embed-gemini --lib --no-run` | pass (live test compiles) |
| `cargo test --features embed-gemini,embed-bge,embed-fixture --lib` | 934 passed / 0 failed / 2 ignored, exit 0 |
| `cargo test --features store-memory,embed-gemini --lib resolve_gemini_stamps_embedding_model` | 1 passed / 0 failed |
| live test, no creds, `-- --ignored` | 1 passed / 0 failed (clean skip, exit 0) |
| CI YAML parse (`python3 -c 'import yaml; yaml.safe_load(...)'`) | pass; `gemini` row present and well-formed |

The default lib suite reports `2 ignored` (up from round-1's `1`): the new `#[ignore]`d gemini
live test joins the pre-existing candle live test. No new failures anywhere. The four gates the
round-1 review required to stay green (fmt, clippy on embed-gemini, the embed-gemini lib suite,
and the resolve pin) are all green, and the remediation's own CI row is exactly the clippy+test
pair those gates measure.

## Conclusion

The round-1 remediation is genuine and complete. P1-A-E2E-1 (no embed-gemini CI row) is closed
by a real `gemini` feature-matrix row running clippy + test offline on `embed-gemini`. P2-A-E2E-2
(no `#[ignore]`d live Vertex test) is closed by `gemini_live_embeds_against_vertex`, which is
`#[tokio::test] #[ignore]`, resolves `LAMBO_GEMINI_CREDENTIALS` else
`GOOGLE_APPLICATION_CREDENTIALS`, skips cleanly when absent (verified live), performs a real
OAuth + `embedContent` round-trip, and asserts width and unit norm. P3-A-E2E-3 (no resolve-level
gemini stamp test) is closed by `resolve_gemini_stamps_embedding_model`, reachable via the new
`pub(crate) mod gemini`, and passes. Each would-be regression (removing the CI row, removing or
ungating the live test or breaking its skip path, removing the resolve pin) is caught by the
correct remaining surface. **APPROVE, zero residue, no new findings.**

- E2ER2, 2026-08-24
