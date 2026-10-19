# #171 performance evidence

`perf171.py` drives a real `lambo serve --transport http` over MCP against a
scratch SQLite store (fixture embedder, dim 1024), so every number below is
end to end: JSON-RPC, the session's locks, the store and the daemon.

Populate: 142 `lambo_derive` batches of 64 concepts, then 200
`lambo_record_action` calls that each depend on one of 20 `SG-Base-VPC-*`
anchors. Run with `--settle`, which waits for the write queue to drain after
every batch: derives are acknowledged before they apply, so without it the
store's final size depends on write-behind timing (two runs of the same
binary differed), and the two binaries would be measured on different
stores. Settled, both binaries build the identical store.

Both binaries are `cargo build --release --features store-sqlite` on the
same box (x86-64, no other build running): main with cargo's default release
profile (main has none), this branch with the `[profile.release]` it adds.

## PR 171a: B1 (lane cosine, partial top-k), B2 (no cache lock across recall), anchor once, release profile

| measure | main (3d126d4c) | this PR |
|---|---:|---:|
| provision_ms (p50) | 144.63 | 137.07 |
| startup_loaded_ms | 559.3 | 508.5 |
| store after populate (concepts / edges) | 9508 / 11669 | 9508 / 11669 |
| keyword_semantic_recall p50 / p90 ms | 14.21 / 16.36 | 6.77 / 8.0 |
| structural_hit p50 / p90 ms | 2.33 / 2.63 | 1.88 / 2.06 |
| structural_miss p50 / p90 ms | 16.47 / 18.64 | 6.61 / 7.74 |
| identifier_recall p50 / p90 ms | 14.25 / 15.09 | 5.13 / 5.62 |
| recall_16_concurrent p50 / p90 ms | 237.29 / 260.51, 66.5 rps | 40.3 / 52.71, 381.5 rps |
| derive_ack_back_to_back p50 / p90 ms | 27.81 / 32.69 | 25.15 / 30.72 |
| derive_applied_back_to_back p50 / p90 ms | 44.9 / 49.56 | 32.81 / 42.02 |
| derive_ack_isolated p50 / p90 ms | 1.54 / 1.86 | 1.49 / 1.73 |

`populate_s` is left out: under `--settle` it is dominated by the 0.5 s
drain poll and the flush tick, not by either binary.

Raw: `main.json`, `pr-171a.json`. Re-run:

```bash
cargo build --release --features store-sqlite
python3 evidence/issue-171-perf/perf171.py --settle --bin target/release/lambo --label me --out /tmp/me.json
```

### Micro-measurements (release, this branch's profile)

`cargo test --release --features fixtures --lib -- --ignored --nocapture --test-threads=1 rank_by_cosine_timing`,
9,000 candidates at dim 1024, top 10:

| variant | per scan |
|---|---:|
| scalar cosine + full sort (pre-#171) | 8.93 ms |
| lane kernel + full sort | 2.15 ms |
| lane kernel + partial top-k (this PR) | 1.85 ms |

Anchor resolution (`anchor_resolution_timing`, 9,100 concepts) costs about
0.95 ms per resolution. A dispatched structural recall used to resolve it
three times (`fits_structural`, `try_structural`, `resolved_anchor_text`);
it now resolves once and reuses it while the graph epoch is unchanged.
