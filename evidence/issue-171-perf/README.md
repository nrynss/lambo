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

## PR 171a: B1 (lane cosine, partial top-k), B2 (no cache or hot-list lock across recall), anchor once, release profile

| measure | main (3d126d4c) | this PR |
|---|---:|---:|
| populate_s (settled) | 162.2 | 147.2 |
| provision_ms (p50) | 159.56 | 138.4 |
| startup_loaded_ms | 661.6 | 664.0 |
| store after populate (concepts / edges) | 9508 / 11669 | 9508 / 11669 |
| keyword_semantic_recall p50 / p90 ms | 18.71 / 23.2 | 7.46 / 9.68 |
| structural_hit p50 / p90 ms | 2.35 / 2.77 | 1.85 / 2.39 |
| structural_miss p50 / p90 ms | 20.45 / 25.96 | 9.33 / 11.73 |
| identifier_recall p50 / p90 ms | 14.82 / 16.11 | 6.71 / 10.74 |
| recall_16_concurrent p50 / p90 ms | 249.8 / 279.04, 63.0 rps | 13.06 / 17.66, 1103.9 rps |
| derive_ack_back_to_back p50 / p90 ms | 27.49 / 31.09 | 26.19 / 29.27 |
| derive_applied_back_to_back p50 / p90 ms | 43.65 / 49.18 | 34.44 / 38.09 |
| derive_ack_isolated p50 / p90 ms | 1.76 / 2.34 | 1.64 / 1.99 |

About `populate_s`: under `--settle` it is almost all waiting. Each of the
142 batches is acknowledged in about 2 ms on both binaries
(`derive_batch_ack_ms`), then the drain poll, which samples every 0.5 s,
sees it drained after 1.0 s at the median on both (`derive_batch_drain_ms`:
p50 1004.9 ms on both; p90 1508 ms on main, 1006 ms here). The first run of
this benchmark (an earlier revision of the PR) recorded 156.4 s on main and
294.3 s on the branch; that branch run overlapped a `cargo` build on the
same box, and the per-batch timings did not exist yet to show it. Both
binaries were re-run back to back with nothing else running for the table
above, with the per-batch timings recorded; the earlier numbers are not
used.

The earlier revision also held the daemon's hot-list write lock from the
cache lookup through assembly, which serialized concurrent recalls (16
concurrent: 40.3 ms p50, 381.5 rps). The lock is now taken only around the
hot-list revalidation, which is the 13 ms / 1104 rps row.

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
