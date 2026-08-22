#!/usr/bin/env python3
"""Throughput of the llama.cpp reference, measured by the *same* harness shape
as the candle spike so the two numbers are comparable.

The recorded branch baseline (110-141 items/s, `src/writeq.rs`
`MEASURED_LOCAL_EMBEDDER_RPS`) was taken through the release binary over stdio,
which carries lambo's own per-call overhead. Re-measuring the reference here,
with the same warm-up discard and the same probe texts the spike uses, gives an
apples-to-apples figure alongside the recorded one rather than instead of it.

Method mirrors `src/writeq.rs::probe_embedder` and the spike's `bench`:
warm-up embeds discarded, then a serial leg and a `--concurrency`-wide leg, at
J3's PROBE_TEXT (35 B) and PROBE_TEXT_BYTES (1024 B).

Usage:
    python3 evidence/mooshik-k1-metal/bench_llama.py --url http://127.0.0.1:8099
"""

import argparse
import json
import time
import urllib.request
from concurrent.futures import ThreadPoolExecutor

SEED = "lambo write queue calibration probe"


def probe_text_at(n: int) -> str:
    s = ""
    while len(s) < n:
        s += SEED + " "
    return s[:n]


def embed(url: str, text: str) -> None:
    body = json.dumps({"input": text, "model": "bge-m3"}).encode()
    req = urllib.request.Request(
        url.rstrip("/") + "/v1/embeddings",
        data=body,
        headers={"Content-Type": "application/json"},
    )
    with urllib.request.urlopen(req, timeout=300) as r:
        json.load(r)


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--url", default="http://127.0.0.1:8099")
    ap.add_argument("--sizes", default="35,1024")
    ap.add_argument("--concurrency", type=int, default=4)
    ap.add_argument("--iters", type=int, default=32)
    ap.add_argument("--warmup", type=int, default=4)
    args = ap.parse_args()

    for size in [int(s) for s in args.sizes.split(",")]:
        text = probe_text_at(size)

        for _ in range(args.warmup):
            embed(args.url, text)

        t0 = time.perf_counter()
        for _ in range(args.iters):
            embed(args.url, text)
        serial_s = time.perf_counter() - t0

        per = args.iters // args.concurrency
        total = per * args.concurrency
        t0 = time.perf_counter()
        with ThreadPoolExecutor(max_workers=args.concurrency) as ex:
            list(ex.map(lambda _: embed(args.url, text), range(total)))
        conc_s = time.perf_counter() - t0

        print(
            json.dumps(
                {
                    "op": "bench",
                    "backend": "llama.cpp-q8_0",
                    "url": args.url,
                    "input_bytes": size,
                    "iters": args.iters,
                    "warmup_discarded": args.warmup,
                    "concurrency": args.concurrency,
                    "serial_items_per_s": args.iters / serial_s,
                    "serial_ms_per_item": serial_s * 1000.0 / args.iters,
                    "concurrent_items_per_s": total / conc_s,
                }
            )
        )


if __name__ == "__main__":
    main()
